//! Session process spawn, PTY ownership, and non-blocking supervision
//! (contract `D-01`, `D-03`, `D-04`, `D-06`; bitty#2790).
//!
//! One session process is spawned exactly once, under the containment plan the
//! daemon probed at startup, with a single-owner PTY. A terminal kind owns the
//! content plane; a worker kind is a service process whose PTY is used for
//! resize only:
//!
//! 1. the caller reserves the orphan-journal record with the pre-spawn
//!    containment identity ([`planned_record`]);
//! 2. [`WorkerPool::spawn`] opens the PTY, builds the child from the bounded
//!    spawn specification, and spawns it through
//!    [`crate::containment::spawn_with`] with a new session on the PTY;
//! 3. the caller commits the observed identity, binds generation `1`, and only
//!    then exposes the session as `running`.
//!
//! The spawn path is bounded before step 3: the configured
//! `spawn_bind_timeout_ms` covers the PTY allocation and the spawn handoff, and
//! the child must still be usable when the single bind probe runs. A deadline
//! that expires, a leader that already exited, or a terminal whose PTY slave is
//! already closed refuses the bind, stops the recorded tree through the
//! bounded `SIGTERM` -> `SIGKILL` escalation, and leaves generation `0` with
//! the truthful cleanup class. The probe is the linearization point: an exit
//! observed after it is a post-bind exit and supervision records
//! `worker_exit`, while an exit observed before it can never produce a bound
//! generation a caller could attach to (bitty#2790).
//!
//! The poll loop never waits for the worker. [`WorkerPool::supervise`] drains
//! the PTY with non-blocking reads, polls `try_wait`, sends the bounded stop
//! signals when an accepted stop reaches its deadline, and records the
//! terminal transition once the containment identity is settled.
//!
//! The child never inherits the daemon environment or a daemon descriptor:
//! the environment is exactly the explicit spawn specification, stdio is the
//! PTY slave only, and the child's descriptor table is reduced to exactly
//! 0/1/2 before `exec` - the slave handles are close-on-exec duplicates and a
//! `close_range(2)` sweep in the child marks everything the parent already
//! held close-on-exec, so a harness pipe or a sibling's transient handle cannot
//! survive the exec. The master is close-on-exec, and the listener, audit,
//! journal, and capability descriptors are all opened close-on-exec by their
//! owners.
//!
//! No build serves the worker kind today. Contract `D-01` reserves the
//! vocabulary but requires `worker_sessions: false` in every v0.2.0 build until
//! an accepted ADR 0019 amendment names worker supervision in the slice and an
//! independent security review accepts the spawn, environment, credential,
//! cleanup, and audit behavior. The served wire therefore refuses every worker
//! create with `Denied/UnsupportedCapability` before any spawn work; the pool
//! below stays compiled and directly testable for the amendment that enables
//! it. The terminal spawn path is the separately accepted 1.3 slice and keeps
//! the containment-plan gate ([`spawn_path_served`]).

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::io::{Read as _, Write as _};
#[cfg(target_os = "linux")]
use std::process::{Command, ExitStatus, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use nix::sys::signal::Signal;

#[cfg(all(test, target_os = "linux"))]
use crate::containment::ExecRefusalKind;
#[cfg(target_os = "linux")]
use crate::containment::{ContainmentError, ContainmentLimits, SessionMode, spawn_with};
use crate::containment::{
    ContainmentKind, ContainmentRecord, ContainmentReport, ExecRefusal, SESSION_CGROUP_PREFIX,
};
#[cfg(target_os = "linux")]
use crate::content::ContentBuffer;
#[cfg(target_os = "linux")]
use crate::lease::{ExitObservation, SessionHost, Termination};
#[cfg(target_os = "linux")]
use crate::limits::LimitId;
#[cfg(target_os = "linux")]
use crate::orphan::OrphanSupervisor;
use crate::registry::{CleanupClass, DaemonInstanceId, SessionId};
#[cfg(target_os = "linux")]
use crate::registry::{LifecycleState, MonotonicMs, SessionKind};
#[cfg(target_os = "linux")]
use crate::spawn::SpawnSpec;

/// Whether the two `D-01` enablement conditions exist in this build.
///
/// Worker hosting may be advertised only after (a) an accepted ADR 0019
/// amendment names worker supervision in the slice and (b) an independent
/// security review accepts the spawn, environment, credential, cleanup, and
/// audit behavior. Neither exists, so the marker is `false` in every shipped
/// build. Flipping it is a contract change, not a configuration choice.
pub const WORKER_HOSTING_AMENDMENT_ACCEPTED: bool = false;

/// Whether this build advertises and serves the worker kind.
///
/// Derived only from [`WORKER_HOSTING_AMENDMENT_ACCEPTED`], so no cell,
/// feature, probe, or configuration can turn the capability on while the
/// amendment is absent. The served harness narrows the value further with
/// [`sessions_served`]; today both are `false` everywhere.
pub const WORKER_SESSIONS_SERVED: bool = WORKER_HOSTING_AMENDMENT_ACCEPTED;

/// Whether this build and the probed containment plan serve worker sessions.
///
/// Always `false` until [`WORKER_HOSTING_AMENDMENT_ACCEPTED`] is set, because
/// the contract reserves the kind in every v0.2.0 build. The containment-plan
/// term is kept so an amended build still cannot advertise a worker tree on a
/// cell whose probe selected no mechanism.
#[must_use]
pub fn sessions_served(report: &ContainmentReport) -> bool {
    WORKER_SESSIONS_SERVED && report.kind() != ContainmentKind::Refused
}

/// Whether this build and plan serve the contained spawn path for terminals.
///
/// This is the separately accepted 1.3 terminal-spawn slice: it depends only on
/// the platform cell and the plan the startup probe selected, never on the
/// reserved worker capability. A `Refused` plan returns the typed
/// `Denied/UnsupportedCapability` before the spawn specification is parsed.
#[must_use]
pub fn spawn_path_served(report: &ContainmentReport) -> bool {
    cfg!(target_os = "linux") && report.kind() != ContainmentKind::Refused
}

/// Bytes one worker's PTY may hand the poll loop in a single tick.
///
/// The bound is one transport `max_chunk_bytes`, the largest content unit the
/// service forwards in a frame. The per-tick budget is divided across the live
/// workers, so one continuously writing session cannot starve the others or
/// hold the single-threaded service loop inside one session's `read`. The
/// remaining bytes stay queued in the PTY buffer as backpressure and are
/// drained on later ticks.
pub const DRAIN_BUDGET_BYTES: usize =
    crate::limits::LimitId::MaxChunkBytes.default_value() as usize;

/// The share of [`DRAIN_BUDGET_BYTES`] one worker may drain in a tick.
///
/// An empty pool still reports the full bound so the caller never divides by
/// zero; the clamp keeps a pool larger than the budget at one byte per worker
/// instead of a zero-byte tick that would never make progress.
#[cfg(target_os = "linux")]
fn per_worker_drain_budget(worker_count: usize) -> usize {
    DRAIN_BUDGET_BYTES
        .checked_div(worker_count.max(1))
        .unwrap_or(DRAIN_BUDGET_BYTES)
        .clamp(1, DRAIN_BUDGET_BYTES)
}

/// Why a worker spawn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSpawnError {
    /// This cell does not serve the worker path.
    Unsupported,
    /// The PTY could not be created or its descriptors could not be prepared.
    /// Nothing was started, and the refusal is distinct from an `exec`
    /// refusal so a client can tell "the daemon could not allocate a terminal"
    /// from "your program could not be executed" (bitty#2790).
    Pty,
    /// The spawn syscall, or the containment plan behind it, refused before a
    /// child existed, with an errno the exec classes do not name.
    Spawn,
    /// `exec` refused the program. Nothing was started; the carried refusal
    /// names the errno class (`EACCES`, `ENOEXEC`, `ENOENT`).
    Exec(ExecRefusal),
    /// The PTY handoff crossed the configured `spawn_bind_timeout_ms` before
    /// any child existed, so nothing needs cleanup.
    BindDeadline,
    /// The child existed, but it was not usable when the bind probe ran: it
    /// had already exited, or its PTY slave was already closed. The child was
    /// terminated and reaped before this error returned; the carried class is
    /// what the cleanup could prove.
    NotBindable(CleanupClass),
    /// The PTY handoff and spawn did not finish within the configured
    /// `spawn_bind_timeout_ms`. The child existed, so it was stopped through
    /// the bounded escalation and reaped before this error returned; the
    /// carried class is what the cleanup could prove.
    BindTimeout(CleanupClass),
    /// A child existed and its cleanup was observed (`swept` or `unverified`).
    Cleanup(CleanupClass),
}

impl std::fmt::Display for WorkerSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                formatter.write_str("worker sessions are not served by this build")
            }
            Self::Pty => formatter.write_str("the worker PTY could not be allocated"),
            Self::Spawn => formatter.write_str("worker spawn failed before any child existed"),
            Self::Exec(refusal) => write!(formatter, "worker exec was refused: {refusal}"),
            Self::BindDeadline => formatter.write_str(
                "the worker handoff crossed spawn_bind_timeout_ms before a child existed",
            ),
            Self::NotBindable(cleanup) => {
                write!(
                    formatter,
                    "worker was not bindable before generation 1; cleanup {}",
                    cleanup.as_str()
                )
            }
            Self::BindTimeout(cleanup) => {
                write!(
                    formatter,
                    "worker did not bind within spawn_bind_timeout_ms; cleanup {}",
                    cleanup.as_str()
                )
            }
            Self::Cleanup(cleanup) => {
                write!(
                    formatter,
                    "worker spawn failed; cleanup {}",
                    cleanup.as_str()
                )
            }
        }
    }
}

impl std::error::Error for WorkerSpawnError {}

/// Why a resize could not be applied to the content plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeError {
    /// No worker is bound to this session.
    NotBound,
    /// The PTY rejected the geometry.
    Pty,
}

/// One bounded pull of terminal output.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentRead {
    /// Raw bytes taken from the session buffer.
    pub data: Vec<u8>,
    /// Bytes still buffered after the pull.
    pub buffered_bytes: usize,
}

impl std::fmt::Debug for ContentRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContentRead")
            .field("data_bytes", &self.data.len())
            .field("buffered_bytes", &self.buffered_bytes)
            .finish()
    }
}

/// The result of writing terminal input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentWrite {
    /// The PTY accepted `accepted` bytes; `remaining` bytes were not written
    /// (`remaining > 0` means the caller should retry the tail).
    Accepted {
        /// Bytes accepted by the PTY.
        accepted: usize,
        /// Bytes the caller attempted.
        attempted: usize,
    },
    /// The PTY input buffer is full and accepted nothing.
    Backpressure {
        /// Bytes the caller attempted.
        attempted: usize,
    },
    /// The worker has exited (or the PTY is gone); nothing was written.
    Closed,
}

/// The pre-spawn containment identity, derived from ids only.
///
/// The name is never caller text: it is the compiled prefix plus the instance
/// and session ids, so it is unique per live session and bounded well below
/// `SESSION_CGROUP_NAME_MAX`.
#[must_use]
pub fn planned_record(
    report: &ContainmentReport,
    instance: DaemonInstanceId,
    session: SessionId,
) -> ContainmentRecord {
    match report.kind() {
        ContainmentKind::CgroupV2 => match report.cgroup_root() {
            Some(root) => ContainmentRecord::cgroup(root.join(cgroup_name(instance, session))),
            None => ContainmentRecord::Subreaper,
        },
        _ => ContainmentRecord::Subreaper,
    }
}

/// The session cgroup name: prefix plus the instance and session ids.
#[must_use]
pub fn cgroup_name(instance: DaemonInstanceId, session: SessionId) -> String {
    format!(
        "{SESSION_CGROUP_PREFIX}{}-{}",
        instance.as_hex(),
        session.as_hex()
    )
}

/// One spawned worker and its owned PTY.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct Worker {
    child: crate::containment::ContainedChild,
    master: File,
    exit: Option<ExitStatus>,
    /// Monotonic time the exit was first observed, for the cleanup bound.
    exit_seen_ms: Option<u64>,
    /// Whether `SIGTERM` was already sent for an accepted stop.
    term_sent: bool,
    /// Whether `SIGKILL` was already sent (deadline or natural-exit cleanup).
    kill_sent: bool,
    /// Whether a bounded terminate already settled this handle.
    settled: bool,
    /// Whether this kind owns a terminal content plane (`kind: terminal`).
    content_enabled: bool,
    /// Bounded PTY output retained for `session.read` / `session.subscribe`.
    content: ContentBuffer,
    /// Bytes drained from the PTY over the handle's lifetime.
    drained_bytes: u64,
}

/// Every live worker bound to this daemon instance.
///
/// The pool is metadata plus the owned child and PTY handles. It never exposes
/// a pid, a descriptor, or caller bytes.
pub struct WorkerPool {
    #[cfg(target_os = "linux")]
    workers: BTreeMap<SessionId, Worker>,
    #[cfg(target_os = "linux")]
    limits: ContainmentLimits,
    /// Committed `spawn_bind_timeout_ms` for the spawn+PTY handoff.
    ///
    /// Enforced by [`WorkerPool::spawn`] before the caller commits the
    /// orphan-journal identity or binds generation `1` (bitty#2790).
    #[cfg(target_os = "linux")]
    bind_timeout_ms: u64,
    /// One-shot pre-bind pause used by the failure-injection tests.
    ///
    /// The pause runs after the child exists and before the bind probe, so a
    /// test can place an exit, a PTY hangup, or an elapsed deadline in the
    /// window the production deadline covers without racing the scheduler.
    #[cfg(all(test, target_os = "linux"))]
    pre_bind_delay: Option<Duration>,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerPool")
            .field("served", &WORKER_SESSIONS_SERVED)
            .finish_non_exhaustive()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // A pool that is dropped while a handle is still live (an abnormal
        // service teardown, a test fixture) must not leak a child. The bounded
        // terminate path reaps the leader and drains the recorded identity.
        #[cfg(target_os = "linux")]
        {
            let session_ids: Vec<SessionId> = self.workers.keys().copied().collect();
            for session_id in session_ids {
                if let Some(mut worker) = self.workers.remove(&session_id) {
                    worker.settled = true;
                    let _ = worker.child.terminate(self.limits);
                }
            }
        }
    }
}

impl WorkerPool {
    /// An empty pool with the compiled escalation bounds.
    #[must_use]
    pub fn new(limits: &crate::limits::Limits) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                workers: BTreeMap::new(),
                limits: ContainmentLimits::for_session_stop(limits),
                bind_timeout_ms: limits.effective(LimitId::SpawnBindTimeoutMs),
                #[cfg(test)]
                pre_bind_delay: None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = limits;
            Self {}
        }
    }

    /// One-shot pre-bind pause for the failure-injection tests.
    ///
    /// The pause runs after the child exists and before the bind probe, so a
    /// test can place an exit, a PTY hangup, or an expired deadline in the
    /// production window without racing the scheduler.
    #[cfg(all(test, target_os = "linux"))]
    pub fn delay_bind_probe_for_test(&mut self, delay: Duration) {
        self.pre_bind_delay = Some(delay);
    }

    /// Number of live bound workers.
    #[must_use]
    pub fn len(&self) -> usize {
        #[cfg(target_os = "linux")]
        {
            self.workers.len()
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    /// Whether no worker is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a worker is bound to `session_id`.
    #[must_use]
    pub fn contains(&self, session_id: crate::registry::SessionId) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.workers.contains_key(&session_id)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = session_id;
            false
        }
    }

    /// Whether `session_id` owns a live terminal content plane right now.
    ///
    /// A registry-only terminal (no worker), a worker-kind session (a service
    /// process), a settled handle, and a worker whose leader exited before the
    /// poll tick recorded the terminal transition all answer `false`. The
    /// content handlers use this as the liveness precondition, so
    /// `session.subscribe` cannot succeed on a plane that can never deliver a
    /// frame.
    #[must_use]
    pub fn content_ready(&self, session_id: crate::registry::SessionId) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.workers
                .get(&session_id)
                .is_some_and(Worker::content_ready)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = session_id;
            false
        }
    }

    /// The sessions this pool still holds a live worker handle for.
    ///
    /// This is the only set the on-demand orphan verifier may skip: a session
    /// with no worker handle has nothing to protect, even while its terminal
    /// record is retained for reads.
    #[must_use]
    pub fn session_ids(&self) -> Vec<crate::registry::SessionId> {
        #[cfg(target_os = "linux")]
        {
            self.workers.keys().copied().collect()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    /// Spawn one worker and retain its handle. Returns the observed identity.
    ///
    /// The configured `spawn_bind_timeout_ms` bounds the whole PTY and spawn
    /// handoff, and the child must still be usable when the bind probe runs:
    ///
    /// - a deadline that expires after the PTY was opened but before any
    ///   child existed is [`WorkerSpawnError::BindDeadline`];
    /// - a deadline that expires after the child existed stops the child
    ///   through the bounded escalation and is
    ///   [`WorkerSpawnError::BindTimeout`];
    /// - a child that already exited, or a terminal whose PTY slave was
    ///   already closed, is [`WorkerSpawnError::NotBindable`].
    ///
    /// The bind is linearized at the probe. An exit observed after the probe
    /// is a post-bind exit: the caller binds generation `1` and supervision
    /// records `worker_exit` on its next tick. An exit observed before it
    /// never yields a bound generation, so no caller can attach to a worker
    /// that was already gone (bitty#2790).
    ///
    /// The bound is observed at the two points this synchronous handoff can
    /// inspect: after the PTY is opened and after the child exists. A kernel
    /// call that never returns cannot be interrupted from the same thread, but
    /// a handoff that merely completed late is still refused before the commit.
    ///
    /// A failed spawn leaves no child running and no pool entry.
    ///
    /// # Errors
    ///
    /// [`WorkerSpawnError::Unsupported`] on a non-served cell,
    /// [`WorkerSpawnError::Pty`] when the PTY could not be allocated,
    /// [`WorkerSpawnError::Exec`] when `exec` refused the program (a typed
    /// errno class, never the PTY refusal), [`WorkerSpawnError::Spawn`] when
    /// the spawn syscall or the containment plan refused before a child
    /// existed, [`WorkerSpawnError::BindDeadline`] when the handoff crossed the
    /// bound before a child existed, [`WorkerSpawnError::NotBindable`] or
    /// [`WorkerSpawnError::BindTimeout`] when the child existed but could not
    /// bind, or [`WorkerSpawnError::Cleanup`] with the class the plan could
    /// prove after terminating a child that did exist.
    #[cfg(target_os = "linux")]
    pub fn spawn(
        &mut self,
        report: &ContainmentReport,
        instance: DaemonInstanceId,
        session_id: SessionId,
        spec: &SpawnSpec,
        kind: SessionKind,
    ) -> Result<crate::orphan::identity::ProcessIdentity, WorkerSpawnError> {
        let started = Instant::now();
        let name = cgroup_name(instance, session_id);
        let mut pty = PtyCommand::build(spec)?;
        let master = pty.master.take().ok_or(WorkerSpawnError::Pty)?;
        if started.elapsed() >= self.bind_timeout() {
            // No child exists yet, so the refusal needs no cleanup. Dropping
            // the PTY handles closes both sides.
            return Err(WorkerSpawnError::BindDeadline);
        }
        let mut child = match spawn_with(
            report,
            &name,
            &mut pty.command,
            self.limits,
            SessionMode::NewSession,
        ) {
            Ok(child) => child,
            // `exec` refused the program: no child ran, and the refusal keeps
            // its errno class so a client can tell it from a PTY allocation
            // failure (bitty#2790).
            Err(ContainmentError::Exec(refusal)) => {
                return Err(WorkerSpawnError::Exec(refusal));
            }
            Err(error) => {
                return Err(match error.cleanup() {
                    CleanupClass::NotStarted => WorkerSpawnError::Spawn,
                    cleanup => WorkerSpawnError::Cleanup(cleanup),
                });
            }
        };
        let identity = child.identity();
        // The command owned the parent-side copies of the child's stdio. Those
        // copies must be gone before the PTY hangup probe runs, or the master
        // would still have a slave reference and a dead child would look
        // usable.
        drop(pty);
        if crate::containment::set_nonblocking(&master).is_err() {
            // The child exists and is already contained: terminate it before
            // the handle is dropped, and report what the cleanup could prove.
            let cleanup = child.terminate(self.limits).class();
            return Err(WorkerSpawnError::Cleanup(cleanup));
        }
        #[cfg(test)]
        if let Some(delay) = self.pre_bind_delay.take() {
            std::thread::sleep(delay);
        }
        let content_enabled = kind == SessionKind::Terminal;
        self.probe_bindable(&mut child, &master, content_enabled, started)?;
        self.workers.insert(
            session_id,
            Worker {
                child,
                master,
                exit: None,
                exit_seen_ms: None,
                term_sent: false,
                kill_sent: false,
                settled: false,
                content_enabled,
                content: ContentBuffer::new(),
                drained_bytes: 0,
            },
        );
        Ok(identity)
    }

    /// The configured bind deadline as a duration.
    #[cfg(target_os = "linux")]
    fn bind_timeout(&self) -> Duration {
        Duration::from_millis(self.bind_timeout_ms)
    }

    /// Decide whether the spawned child may bind generation `1`.
    ///
    /// The liveness probe is non-blocking and runs once, so the bind has a
    /// single linearization point instead of a poll window that a child could
    /// exit inside. A terminal additionally requires its PTY slave to still be
    /// open: a child that closed its stdio has no content plane to serve, even
    /// if its leader has not been reaped yet. A worker kind keeps its PTY for
    /// resize only, so a service process may close its stdio and still bind.
    ///
    /// On refusal the child is terminated through the same bounded
    /// `SIGTERM` -> `SIGKILL` escalation supervision uses, and the returned
    /// class is what the containment plan could prove.
    #[cfg(target_os = "linux")]
    fn probe_bindable(
        &self,
        child: &mut crate::containment::ContainedChild,
        master: &File,
        content_enabled: bool,
        started: Instant,
    ) -> Result<(), WorkerSpawnError> {
        if child.leader_exited() {
            let cleanup = child.terminate(self.limits).class();
            return Err(WorkerSpawnError::NotBindable(cleanup));
        }
        if content_enabled && !pty_slave_open(master) {
            let cleanup = child.terminate(self.limits).class();
            return Err(WorkerSpawnError::NotBindable(cleanup));
        }
        if started.elapsed() >= self.bind_timeout() {
            let cleanup = child.terminate(self.limits).class();
            return Err(WorkerSpawnError::BindTimeout(cleanup));
        }
        Ok(())
    }

    /// Non-served cells refuse every spawn.
    ///
    /// # Errors
    ///
    /// Always [`WorkerSpawnError::Unsupported`].
    #[cfg(not(target_os = "linux"))]
    pub fn spawn(
        &mut self,
        _report: &crate::containment::ContainmentReport,
        _instance: crate::registry::DaemonInstanceId,
        _session_id: crate::registry::SessionId,
        _spec: &crate::spawn::SpawnSpec,
        _kind: crate::registry::SessionKind,
    ) -> Result<crate::orphan::identity::ProcessIdentity, WorkerSpawnError> {
        Err(WorkerSpawnError::Unsupported)
    }

    /// Resize one worker's PTY (`session.resize`).
    ///
    /// # Errors
    ///
    /// [`ResizeError::NotBound`] when no worker is bound, or
    /// [`ResizeError::Pty`] when the ioctl fails.
    #[cfg(target_os = "linux")]
    pub fn resize(&self, session_id: SessionId, cols: u16, rows: u16) -> Result<(), ResizeError> {
        let worker = self.workers.get(&session_id).ok_or(ResizeError::NotBound)?;
        crate::containment::resize_pty(&worker.master, cols, rows).map_err(|_| ResizeError::Pty)
    }

    /// Non-served cells have no content plane.
    ///
    /// # Errors
    ///
    /// Always [`ResizeError::NotBound`].
    #[cfg(not(target_os = "linux"))]
    pub fn resize(
        &self,
        _session_id: crate::registry::SessionId,
        _cols: u16,
        _rows: u16,
    ) -> Result<(), ResizeError> {
        Err(ResizeError::NotBound)
    }

    /// Take buffered terminal output for `session.read` / `session.subscribe`.
    ///
    /// Returns `None` when this session owns no live terminal content plane: an
    /// unbound session, a worker-kind session (a service process, not a
    /// terminal), or a handle removed after exit. A live terminal with an empty
    /// buffer is `Some` with zero bytes, so the caller can distinguish "no data
    /// yet" from "not running".
    #[cfg(target_os = "linux")]
    pub fn read(&mut self, session_id: SessionId, max_bytes: usize) -> Option<ContentRead> {
        let worker = self.workers.get_mut(&session_id)?;
        if !worker.content_enabled || worker.exit.is_some() {
            return None;
        }
        let data = worker.content.pop(max_bytes);
        Some(ContentRead {
            data,
            buffered_bytes: worker.content.len(),
        })
    }

    /// Non-served cells have no terminal content plane.
    #[cfg(not(target_os = "linux"))]
    pub fn read(
        &mut self,
        _session_id: crate::registry::SessionId,
        _max_bytes: usize,
    ) -> Option<ContentRead> {
        None
    }

    /// Write terminal input to the owned PTY, never blocking.
    ///
    /// A full PTY input buffer is [`ContentWrite::Backpressure`] with nothing
    /// written; a partial write is [`ContentWrite::Accepted`] with the caller's
    /// remaining tail. The daemon keeps no write queue, so a slow child cannot
    /// grow memory or stall the poll loop.
    #[cfg(target_os = "linux")]
    pub fn write(&mut self, session_id: SessionId, data: &[u8]) -> ContentWrite {
        let Some(worker) = self.workers.get_mut(&session_id) else {
            return ContentWrite::Closed;
        };
        if !worker.content_enabled || worker.exit.is_some() {
            return ContentWrite::Closed;
        }
        classify_write(worker.master.write(data), data.len())
    }

    /// Non-served cells have no writable content plane.
    #[cfg(not(target_os = "linux"))]
    pub fn write(&mut self, _session_id: crate::registry::SessionId, _data: &[u8]) -> ContentWrite {
        ContentWrite::Closed
    }

    /// Drain every bound worker with one shared per-tick byte budget.
    ///
    /// Dividing the budget evenly keeps a continuously writing session from
    /// starving the rest; an unused share is not redistributed, so the total
    /// never exceeds one `max_chunk_bytes` per pass.
    #[cfg(target_os = "linux")]
    fn drain_tick(&mut self) {
        let budget = per_worker_drain_budget(self.workers.len());
        for worker in self.workers.values_mut() {
            worker.drain(budget);
        }
    }

    /// Drain every worker's PTY and supervise exits and stop deadlines.
    ///
    /// This is the poll-loop entry point: no call waits for a running worker.
    /// The terminal transition is recorded once the containment identity is
    /// settled (`swept`) or the bounded kill window expires (`unverified`).
    ///
    /// Returns the sessions whose terminal transition this pass recorded, so
    /// the service can retire the per-session stores the same tick. A worker
    /// handle is never dropped while its child may still run: a session
    /// collected before its worker settled is terminated and its journal
    /// record settled first.
    #[cfg(target_os = "linux")]
    pub fn supervise(
        &mut self,
        host: &mut SessionHost,
        orphans: &mut OrphanSupervisor,
        now: MonotonicMs,
    ) -> Vec<SessionId> {
        let mut finished = Vec::new();
        let kill_wait_ms = u64::try_from(self.limits.kill_wait().as_millis()).unwrap_or(u64::MAX);
        // A descendant killed by the natural-exit group signal can become a
        // zombie only after the tick that recorded the exit. The subreaper plan
        // adopts it, so reap adopted children on every pass; the cgroup plan
        // never adopts one and this finds nothing.
        crate::containment::drain_adopted_children();
        self.drain_tick();
        let session_ids: Vec<SessionId> = self.workers.keys().copied().collect();
        for session_id in session_ids {
            let mut collected: Option<CleanupClass> = None;
            let mut terminal: Option<(ExitObservation, CleanupClass)> = None;
            {
                let Some(worker) = self.workers.get_mut(&session_id) else {
                    continue;
                };
                if worker.exit.is_none() && worker.child.leader_exited() {
                    // The leader is an unreaped zombie: its pid, and therefore
                    // the recorded process-group id, cannot be reused. Signal
                    // the recorded identity now, before the reap, so a
                    // descendant that ignored the leader's exit is still in
                    // the recorded group when the kill lands.
                    worker.child.signal(Signal::SIGKILL);
                    worker.kill_sent = true;
                    if let Ok(Some(status)) = worker.child.try_wait() {
                        worker.exit = Some(status);
                        worker.exit_seen_ms = Some(now.as_millis());
                        worker.child.drain_recorded();
                    }
                }
                if worker.exit.is_none() {
                    // This pass observed the worker alive, which is the
                    // contract's worker liveness signal. It is not a client
                    // heartbeat and never bumps `state_revision`.
                    host.observe_worker(session_id, now);
                }
                if let Some(snapshot) = host.snapshot(session_id, now) {
                    if worker.exit.is_none() && snapshot.state == LifecycleState::Stopping {
                        let Some(stop) = host.stop_view(session_id, now) else {
                            continue;
                        };
                        if !worker.term_sent {
                            worker.child.signal(Signal::SIGTERM);
                            worker.term_sent = true;
                        }
                        if !worker.kill_sent && now.as_millis() >= stop.deadline.as_millis() {
                            worker.child.signal(Signal::SIGKILL);
                            worker.kill_sent = true;
                        }
                    }
                    if let Some(status) = worker.exit {
                        if let Some(cleanup) = worker.sweep(now, kill_wait_ms) {
                            terminal = Some((observation(status), cleanup));
                        }
                    }
                } else {
                    // Collected before its worker settled: terminate the tree
                    // and settle the record; nothing is left to record.
                    collected = Some(worker.child.terminate(self.limits).class());
                }
            }
            if terminal.is_none() && collected.is_none() {
                // The worker is still running (or its killed subtree is still
                // draining): keep the handle for the next tick.
                continue;
            }
            if let Some((observation, cleanup)) = terminal {
                if let Ok(Termination::Completed(_)) =
                    host.record_termination(session_id, observation, cleanup, now)
                {
                    let _ = orphans.settle(session_id, cleanup, now);
                    finished.push(session_id);
                }
            }
            if let Some(cleanup) = collected {
                let _ = orphans.settle(session_id, cleanup, now);
            }
            self.workers.remove(&session_id);
        }
        finished
    }

    /// Non-served cells have no workers to supervise.
    #[cfg(not(target_os = "linux"))]
    pub fn supervise(
        &mut self,
        _host: &mut crate::lease::SessionHost,
        _orphans: &mut crate::orphan::OrphanSupervisor,
        _now: crate::registry::MonotonicMs,
    ) -> Vec<crate::registry::SessionId> {
        Vec::new()
    }

    /// Terminate one worker immediately with a bounded escalation.
    ///
    /// Used when the daemon is already stopping and cannot wait for a later
    /// tick. The terminal transition is recorded by the caller.
    #[cfg(target_os = "linux")]
    pub fn terminate(&mut self, session_id: SessionId) -> Option<CleanupClass> {
        self.workers.get_mut(&session_id).map(|worker| {
            worker.settled = true;
            worker.child.terminate(self.limits).class()
        })
    }

    /// Non-served cells have no worker to terminate.
    #[cfg(not(target_os = "linux"))]
    pub fn terminate(&mut self, _session_id: crate::registry::SessionId) -> Option<CleanupClass> {
        None
    }

    /// Drop a worker handle that is being retired.
    ///
    /// A handle that is not already settled is terminated with the bounded
    /// escalation and reaped before it is dropped, so retiring a session can
    /// never leak a live worker or leave an exited leader as an unreaped
    /// zombie. Returns the cleanup class when this call had to settle the
    /// handle, so the caller can settle its journal record; `None` when the
    /// handle was already settled or absent.
    #[cfg(target_os = "linux")]
    pub fn forget(&mut self, session_id: SessionId) -> Option<CleanupClass> {
        let mut worker = self.workers.remove(&session_id)?;
        if worker.settled {
            return None;
        }
        // `terminate` reaps the leader (`try_wait`) and drains the recorded
        // identity. That is also the required path for a leader that already
        // exited: a cgroup-scope zombie has no `drain_recorded` work, and
        // dropping the handle without a wait would leak it.
        Some(worker.child.terminate(self.limits).class())
    }

    /// Drop the handle of a session the registry already terminalized.
    #[cfg(not(target_os = "linux"))]
    pub fn forget(&mut self, _session_id: crate::registry::SessionId) -> Option<CleanupClass> {
        None
    }
}

/// Classify one non-blocking PTY write result.
///
/// `Ok(0)`, `WouldBlock`, and `Interrupted` are the backpressure paths: nothing
/// was accepted, so the caller gets the typed `ChannelFull` refusal and retries
/// later. A partial `Ok(n)` is a success with the caller's tail; any other
/// error means the PTY is gone.
#[cfg(target_os = "linux")]
fn classify_write(result: io::Result<usize>, attempted: usize) -> ContentWrite {
    match result {
        Ok(0) => ContentWrite::Backpressure { attempted },
        Ok(accepted) => ContentWrite::Accepted {
            accepted,
            attempted,
        },
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            ContentWrite::Backpressure { attempted }
        }
        Err(_) => ContentWrite::Closed,
    }
}

#[cfg(target_os = "linux")]
impl Worker {
    /// Whether this handle still owns a live, writable content plane.
    ///
    /// `leader_exited` is a non-blocking `waitid(..., WNOWAIT)` probe, so a
    /// worker that died since the last supervision tick is already refused
    /// even though the registry still reports `running`.
    fn content_ready(&self) -> bool {
        self.content_enabled && self.exit.is_none() && !self.settled && !self.child.leader_exited()
    }

    fn drain(&mut self, mut budget: usize) {
        let mut buffer = [0u8; 4096];
        while budget > 0 {
            if self.content_enabled && self.content.is_full() {
                // The reader is behind: leave the remainder in the PTY so the
                // child sees terminal backpressure instead of unbounded memory
                // growth inside the daemon.
                break;
            }
            let want = budget
                .min(buffer.len())
                .min(self.content.remaining_capacity().max(1));
            match self.master.read(&mut buffer[..want]) {
                Ok(0) => break,
                Ok(read) => {
                    self.drained_bytes = self.drained_bytes.saturating_add(read as u64);
                    if self.content_enabled {
                        let accepted = self.content.push(&buffer[..read]);
                        debug_assert_eq!(accepted, read, "the drain bound respects the buffer cap");
                    }
                    budget -= read;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // EIO is the Linux PTY hangup once the slave side is gone.
                Err(_) => break,
            }
        }
    }

    /// Settle the containment identity after the exit was observed.
    ///
    /// Returns `None` while a killed subtree is still draining, so the poll
    /// loop keeps making progress instead of waiting. After the kill window the
    /// identity is reported `unverified` rather than claimed swept.
    fn sweep(&mut self, now: MonotonicMs, kill_wait_ms: u64) -> Option<CleanupClass> {
        if let Some(cleanup) = self.child.sweep_if_empty() {
            return Some(cleanup.class());
        }
        let seen = self.exit_seen_ms.unwrap_or(now.as_millis());
        if now.as_millis().saturating_sub(seen) >= kill_wait_ms {
            return Some(CleanupClass::Unverified);
        }
        None
    }
}

/// The command plus the owned PTY master.
#[cfg(target_os = "linux")]
struct PtyCommand {
    command: Command,
    master: Option<File>,
}

#[cfg(target_os = "linux")]
impl PtyCommand {
    fn build(spec: &SpawnSpec) -> Result<Self, WorkerSpawnError> {
        #[cfg(test)]
        if openpty_failure::take() {
            return Err(WorkerSpawnError::Pty);
        }
        let winsize = nix::pty::Winsize {
            ws_row: spec.rows(),
            ws_col: spec.cols(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = nix::pty::openpty(Some(&winsize), None).map_err(|_| WorkerSpawnError::Pty)?;
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        crate::containment::set_cloexec(&master).map_err(|_| WorkerSpawnError::Pty)?;
        // The slave is dropped after the duplicates, and it must never be
        // inherited either: keep the parent handle close-on-exec while the
        // child's stdio is built from close-on-exec copies.
        crate::containment::set_cloexec(&slave).map_err(|_| WorkerSpawnError::Pty)?;
        let stdin = Stdio::from(dup_file(&slave)?);
        let stdout = Stdio::from(dup_file(&slave)?);
        let stderr = Stdio::from(dup_file(&slave)?);
        drop(slave);

        let argv = spec.argv();
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        // The child environment is exactly the explicit specification: no
        // daemon variable, credential, socket path, or token is inherited.
        command.env_clear();
        for (name, value) in spec.env() {
            command.env(name, value);
        }
        command.env("TERM", spec.term());
        command.current_dir(spec.cwd());
        command.stdin(stdin);
        command.stdout(stdout);
        command.stderr(stderr);
        // The daemon blocks `SIGTERM`/`SIGINT`/`SIGHUP` for its own signal fd
        // and a service manager leaves `SIGPIPE`/`SIGXFSZ` ignored; neither
        // state is reset by `execve`, so the child must clear both before it
        // becomes the user's shell (bitty#2920).
        crate::containment::reset_child_signal_state(&mut command);
        // The child establishes the session and takes the PTY slave as its
        // controlling terminal inside the one documented syscall seam.
        crate::containment::establish_controlling_terminal(&mut command);
        // Whatever the parent already held - a concurrently forked sibling's
        // PTY duplicates, a test harness IPC pipe on a CI runner - is marked
        // close-on-exec in the child, so the worker's descriptor table is
        // exactly its stdio (bitty#2824).
        crate::containment::close_inherited_descriptors(&mut command);
        Ok(Self {
            command,
            master: Some(master),
        })
    }
}

/// Whether the child still holds the PTY slave open.
///
/// The parent dropped its slave handle after building stdio, so a hangup on
/// the master means every child-side slave descriptor is already closed. The
/// probe never reads content: it only asks whether the terminal can still
/// deliver a frame. `POLLIN` with buffered output is fine; `POLLHUP`, an
/// error, or an invalid descriptor is not.
#[cfg(target_os = "linux")]
fn pty_slave_open(master: &File) -> bool {
    use std::os::fd::AsFd as _;

    let mut fds = [nix::poll::PollFd::new(
        master.as_fd(),
        nix::poll::PollFlags::POLLIN,
    )];
    match nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO) {
        Ok(_) => !fds[0]
            .revents()
            .unwrap_or(nix::poll::PollFlags::empty())
            .intersects(
                nix::poll::PollFlags::POLLHUP
                    | nix::poll::PollFlags::POLLERR
                    | nix::poll::PollFlags::POLLNVAL,
            ),
        // An interrupted probe observed nothing, so it must not turn a live
        // child into a refusal.
        Err(nix::errno::Errno::EINTR) => true,
        Err(_) => false,
    }
}

/// Test-only `openpty` failure injection.
///
/// The seam is thread-local because the unit tests run concurrently and a
/// process-wide flag would fail an unrelated PTY test. Production compiles it
/// out entirely. The module is `pub(crate)` so the service-layer tests can
/// prove the PTY refusal and the exec refusal are different wire shapes
/// (bitty#2790).
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod openpty_failure {
    use std::cell::Cell;

    thread_local! {
        static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
    }

    /// Fail the next `PtyCommand::build` on this thread.
    pub fn fail_next() {
        FAIL_NEXT.with(|flag| flag.set(true));
    }

    /// Consume one injected failure.
    pub fn take() -> bool {
        FAIL_NEXT.with(|flag| flag.replace(false))
    }
}

#[cfg(target_os = "linux")]
fn dup_file(slave: &File) -> Result<File, WorkerSpawnError> {
    // `openpty` descriptors are not close-on-exec. The child `dup2`s these
    // copies onto 0/1/2 (which clears CLOEXEC on the target), so the copies
    // must not survive `exec` themselves: duplicate them already
    // close-on-exec so a concurrently forked child of the daemon cannot
    // inherit a stray slave handle either. The `close_range(2)` sweep on the
    // command is the guarantee that nothing above stdio survives this worker's
    // own `exec`.
    crate::containment::dup_cloexec(slave).map_err(|_| WorkerSpawnError::Pty)
}

#[cfg(target_os = "linux")]
fn signal_number(status: ExitStatus) -> Option<u32> {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .signal()
        .and_then(|signal| u32::try_from(signal).ok())
}

/// Reduce one exit status to the observation the lease layer records.
#[cfg(target_os = "linux")]
fn observation(status: ExitStatus) -> ExitObservation {
    match status.code() {
        Some(code) => ExitObservation::Exited {
            code: u8::try_from(code.clamp(0, 255)).unwrap_or(255),
        },
        None => ExitObservation::Signalled {
            signal: signal_number(status).unwrap_or(1),
        },
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::{Seek as _, SeekFrom};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;
    use crate::containment::ContainmentReport;
    use crate::content::CONTENT_FRAME_BYTES;
    use crate::limits::{LimitId, LimitOverride, Limits};
    use crate::registry::{DaemonInstanceId, SessionId, SessionKind};

    /// Pause between content-plane readiness polls.
    const READY_POLL: Duration = Duration::from_millis(10);

    /// Spawn one live terminal worker through the production path.
    fn spawn_test_worker(limits: &Limits, argv: &[&str]) -> (WorkerPool, SessionId) {
        spawn_test_worker_for(limits, argv, SessionKind::Terminal)
    }

    /// Spawn one live terminal worker with an explicit child environment.
    fn spawn_test_worker_with_env(
        limits: &Limits,
        argv: &[&str],
        env: &[&str],
    ) -> (WorkerPool, SessionId) {
        spawn_test_worker_env_for(limits, argv, env, SessionKind::Terminal)
    }

    /// Spawn one live worker of `kind` through the production path.
    fn spawn_test_worker_for(
        limits: &Limits,
        argv: &[&str],
        kind: SessionKind,
    ) -> (WorkerPool, SessionId) {
        spawn_test_worker_env_for(limits, argv, &[], kind)
    }

    /// Spawn one live worker of `kind` with an explicit child environment.
    fn spawn_test_worker_env_for(
        limits: &Limits,
        argv: &[&str],
        env: &[&str],
        kind: SessionKind,
    ) -> (WorkerPool, SessionId) {
        let session = SessionId::parse("0000000000000000000000000000c4d1").expect("a session id");
        let mut pool = WorkerPool::new(limits);
        spawn_test_worker_into(&mut pool, limits, argv, env, kind, session);
        (pool, session)
    }

    /// Spawn one live worker into an existing pool.
    fn spawn_test_worker_into(
        pool: &mut WorkerPool,
        limits: &Limits,
        argv: &[&str],
        env: &[&str],
        kind: SessionKind,
        session: SessionId,
    ) {
        let report = ContainmentReport::probe();
        let spec = test_spec(limits, argv, env);
        // Serialized with the other PTY tests so no concurrent `openpty`
        // descriptor can be inherited by this worker's fork.
        let _pty = crate::containment::test_lock::lock();
        pool.spawn(
            &report,
            DaemonInstanceId::generate().expect("an instance id"),
            session,
            &spec,
            kind,
        )
        .expect("the test worker must spawn");
    }

    /// Build one valid spawn specification for the worker tests.
    fn test_spec(limits: &Limits, argv: &[&str], env: &[&str]) -> SpawnSpec {
        let fields = json!({ "argv": argv, "env": env, "cwd": "/" })
            .as_object()
            .expect("a JSON object")
            .clone();
        SpawnSpec::parse(Some(&fields), limits).expect("a valid spawn spec")
    }

    /// Spawn into `pool` and return the typed refusal without panicking.
    fn spawn_test_worker_refusal(
        pool: &mut WorkerPool,
        limits: &Limits,
        argv: &[&str],
        kind: SessionKind,
        session: SessionId,
    ) -> WorkerSpawnError {
        let report = ContainmentReport::probe();
        let spec = test_spec(limits, argv, &[]);
        // Serialized with the other PTY tests for the same `openpty` reason.
        let _pty = crate::containment::test_lock::lock();
        pool.spawn(
            &report,
            DaemonInstanceId::generate().expect("an instance id"),
            session,
            &spec,
            kind,
        )
        .expect_err("the test worker must be refused")
    }

    /// A session id the spawn-refusal tests can use.
    fn refusal_session() -> SessionId {
        SessionId::parse("0000000000000000000000000000c4d2").expect("a session id")
    }

    #[test]
    fn an_openpty_failure_is_a_typed_pty_refusal_and_creates_no_handle() {
        let limits = Limits::default();
        let mut pool = WorkerPool::new(&limits);
        let session = refusal_session();
        super::openpty_failure::fail_next();
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &["/bin/sleep", "60"],
            SessionKind::Terminal,
            session,
        );
        assert_eq!(
            error,
            WorkerSpawnError::Pty,
            "a PTY allocation failure has its own typed refusal"
        );
        assert!(pool.is_empty(), "no handle may survive an openpty failure");
    }

    #[test]
    fn a_non_executable_program_is_a_typed_exec_refusal_and_creates_no_handle() {
        use std::os::unix::fs::PermissionsExt as _;

        let limits = Limits::default();
        let mut pool = WorkerPool::new(&limits);
        let dir = tempfile::tempdir().expect("a temporary directory");
        let program = dir.path().join("bitty-not-executable");
        std::fs::write(&program, b"#!/bin/sh\nexit 0\n").expect("write the fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        let program = program.to_str().expect("a UTF-8 fixture path");
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &[program],
            SessionKind::Worker,
            refusal_session(),
        );
        // `Command::spawn` reports the child's `exec` failure synchronously
        // through its error pipe, so `EACCES` is not "missing file": no child
        // ever ran and the cleanup class is not_started.
        assert_eq!(
            error,
            WorkerSpawnError::Exec(ExecRefusal::new(
                ExecRefusalKind::NotPermitted,
                nix::libc::EACCES
            )),
            "{error}"
        );
        assert!(
            !matches!(error, WorkerSpawnError::Pty),
            "an exec refusal is never the PTY refusal"
        );
        assert!(pool.is_empty(), "no handle may survive a failed exec");
    }

    #[test]
    fn a_missing_shebang_interpreter_is_a_typed_exec_refusal_and_creates_no_handle() {
        use std::os::unix::fs::PermissionsExt as _;

        let limits = Limits::default();
        let mut pool = WorkerPool::new(&limits);
        let dir = tempfile::tempdir().expect("a temporary directory");
        // Executable, correct `#!` shape, but the interpreter path does not
        // exist. The kernel resolves the line inside `execve`, so the failure
        // is `ENOENT` and not a daemon-side parse refusal.
        let program = dir.path().join("bitty-bad-shebang");
        std::fs::write(&program, b"#!/nonexistent/bitty-interpreter\n").expect("write the fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let program = program.to_str().expect("a UTF-8 fixture path");
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &[program],
            SessionKind::Worker,
            refusal_session(),
        );
        assert_eq!(
            error,
            WorkerSpawnError::Exec(ExecRefusal::new(
                ExecRefusalKind::NotFound,
                nix::libc::ENOENT
            )),
            "{error}"
        );
        assert!(pool.is_empty(), "no handle may survive a failed exec");
    }

    #[test]
    fn a_child_that_exits_before_bind_is_never_bound() {
        let limits = Limits::default();
        let mut pool = WorkerPool::new(&limits);
        let session = refusal_session();
        pool.delay_bind_probe_for_test(Duration::from_millis(500));
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &["/bin/sh", "-c", "exit 0"],
            SessionKind::Worker,
            session,
        );
        assert!(
            matches!(error, WorkerSpawnError::NotBindable(cleanup) if cleanup != CleanupClass::NotStarted),
            "an exited leader must refuse the bind after terminating the tree: {error}"
        );
        assert!(
            pool.is_empty(),
            "an exited child may not stay in the pool as a bound handle"
        );
    }

    #[test]
    fn a_terminal_that_closed_its_pty_before_bind_is_never_bound() {
        let limits = Limits::default();
        let mut pool = WorkerPool::new(&limits);
        let session = refusal_session();
        pool.delay_bind_probe_for_test(Duration::from_millis(500));
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &["/bin/sh", "-c", "exec 0<&- 1>&- 2>&-; sleep 60"],
            SessionKind::Terminal,
            session,
        );
        assert!(
            matches!(error, WorkerSpawnError::NotBindable(_)),
            "a closed PTY slave cannot serve a terminal content plane: {error}"
        );
        assert!(pool.is_empty());
    }

    #[test]
    fn a_spawn_that_crosses_the_bind_deadline_is_stopped_and_reaped() {
        let limits = Limits::new(&[LimitOverride::new(LimitId::SpawnBindTimeoutMs, 100)])
            .expect("a hundred-millisecond bind deadline");
        let mut pool = WorkerPool::new(&limits);
        let session = refusal_session();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pid_file = dir.path().join("leader.pid");
        let script = format!(
            "echo $$ > '{pid_file}'; exec sleep 60",
            pid_file = pid_file.display()
        );
        // The pause runs after the child exists and before the probe, so the
        // half-second handoff deterministically crosses the configured bound
        // while leaving the PTY and spawn time comfortably inside it.
        pool.delay_bind_probe_for_test(Duration::from_millis(500));
        let error = spawn_test_worker_refusal(
            &mut pool,
            &limits,
            &["/bin/sh", "-c", script.as_str()],
            SessionKind::Worker,
            session,
        );
        assert!(
            matches!(error, WorkerSpawnError::BindTimeout(cleanup) if cleanup != CleanupClass::NotStarted),
            "a crossed bind deadline must stop the spawned tree: {error}"
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the child wrote its pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            std::fs::metadata(format!("/proc/{pid}")).is_err(),
            "the timed-out worker leader {pid} must be reaped, not left behind"
        );
        assert!(pool.is_empty(), "the timed-out handle may not stay bound");
    }

    /// Wait, bounded, until `session` owns a live terminal content plane.
    ///
    /// Binding is synchronous with the spawn call, but the leader is scheduled
    /// independently: a test that asserts immediately can observe the handle
    /// before the child has done anything, and a leader that is already gone
    /// (an exec failure, a hangup) must surface as a bounded timeout instead
    /// of racing the assertion that follows. The bound is the committed
    /// `spawn_bind_timeout_ms`.
    fn wait_for_content_ready(pool: &WorkerPool, limits: &Limits, session: SessionId) {
        let deadline =
            Instant::now() + Duration::from_millis(limits.effective(LimitId::SpawnBindTimeoutMs));
        while !pool.content_ready(session) {
            assert!(
                Instant::now() < deadline,
                "the worker never owned a live content plane within spawn_bind_timeout_ms"
            );
            std::thread::sleep(READY_POLL);
        }
    }

    /// Replace one worker's PTY master with a regular-file producer that
    /// already holds `bytes` unread bytes, and return the real master.
    ///
    /// The caller must keep the returned master alive for as long as the
    /// handle drains the file: dropping it hangs the session up, and a leader
    /// that has not installed its signal handling yet dies from the pending
    /// `SIGHUP`, which turns a liveness assertion into a load-dependent flake.
    /// A regular file is used instead of a PTY or pipe so the queued volume is
    /// deterministic.
    fn use_file_producer(pool: &mut WorkerPool, session: SessionId, bytes: usize) -> File {
        let mut producer = tempfile::tempfile().expect("a temporary producer");
        producer
            .write_all(&vec![b'x'; bytes])
            .expect("fill the over-budget producer");
        producer
            .seek(SeekFrom::Start(0))
            .expect("rewind the producer");
        let worker = pool.workers.get_mut(&session).expect("the bound worker");
        std::mem::replace(&mut worker.master, producer)
    }

    /// Lowest descriptor the dropped-`CLOEXEC` probe may take.
    ///
    /// Above stdio and above the descriptors `openpty` and `Command` open for
    /// themselves, so the probe never aliases a handle the worker path also
    /// uses.
    const PROBE_FD_FLOOR: i32 = 64;

    /// Marker the probe worker prints once it runs its own code.
    const FD_PROBE_READY: &str = "BITTY_FD_PROBE_READY";

    /// Bound on observing the probe worker's own code after its `exec`.
    const FD_PROBE_WAIT: Duration = Duration::from_secs(10);

    /// Pin one inheritable descriptor at [`PROBE_FD_FLOOR`] with its
    /// close-on-exec bit dropped.
    ///
    /// The descriptor is exactly a parent handle that survives `exec` unless
    /// the child-side sweep marks it again; the syscall seam owns the raw
    /// descriptor work because this crate denies `unsafe_code`.
    fn inheritable_probe_descriptor() -> std::os::fd::OwnedFd {
        crate::containment::inheritable_probe_descriptor(PROBE_FD_FLOOR)
            .expect("pin the probe descriptor")
    }

    /// Spawn one worker through the production command builder and containment
    /// path, plus one test-only child hook that clears `descriptor`'s
    /// close-on-exec bit after the production sweep set it.
    ///
    /// This is the negative control arm: the descriptor reaches `exec` exactly
    /// as a parent handle with a dropped `CLOEXEC` does. The production path
    /// installs no such hook.
    fn spawn_control_worker(
        limits: &Limits,
        argv: &[&str],
        descriptor: i32,
    ) -> (crate::containment::ContainedChild, File) {
        // Serialized with the other PTY tests: no concurrent `openpty` handle
        // may land in this worker's descriptor table.
        let _pty = crate::containment::test_lock::lock();
        let report = ContainmentReport::probe();
        let instance = DaemonInstanceId::generate().expect("an instance id");
        let session = SessionId::parse("0000000000000000000000000000c4d5").expect("a session id");
        let name = cgroup_name(instance, session);
        let fields = json!({ "argv": argv, "env": [], "cwd": "/" })
            .as_object()
            .expect("a JSON object")
            .clone();
        let spec = SpawnSpec::parse(Some(&fields), limits).expect("a valid spawn spec");
        let mut pty = PtyCommand::build(&spec).expect("the production worker command");
        let master = pty.master.take().expect("the PTY master");
        // The mutation is owned by the syscall seam, which is the only module
        // allowed `unsafe_code`: it clears the bit the sweep set, so the
        // descriptor reaches `exec` with a dropped `CLOEXEC`.
        crate::containment::drop_cloexec_before_exec(&mut pty.command, descriptor);
        let child = spawn_with(
            &report,
            &name,
            &mut pty.command,
            ContainmentLimits::for_session_stop(limits),
            SessionMode::NewSession,
        )
        .expect("the control worker must spawn");
        crate::containment::set_nonblocking(&master).expect("a non-blocking probe master");
        (child, master)
    }

    /// Drain the PTY until the probe worker reports that it runs its own code.
    ///
    /// The marker is the post-`exec` boundary: the worker can only print it
    /// after its `exec` completed, so the `/proc/<pid>/fd` read that follows
    /// observes the executed image rather than the forked child.
    fn wait_for_fd_probe_ready(master: &mut File) {
        let deadline = Instant::now() + FD_PROBE_WAIT;
        let mut output = String::new();
        let mut chunk = [0u8; 256];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    output.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    if output.contains(FD_PROBE_READY) {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "the fd probe never reported after exec: {output}"
                    );
                    std::thread::sleep(READY_POLL);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // Linux reports EIO on the master once the last slave closed.
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::EIO as i32) => break,
                Err(error) => panic!("read the worker PTY: {error}"),
            }
        }
        panic!("the fd probe exited before reporting: {output}");
    }

    /// The descriptor numbers `/proc/<pid>/fd` reports for `pid`.
    fn child_fd_numbers(pid: u32) -> Vec<u32> {
        let table = format!("/proc/{pid}/fd");
        let mut fds: Vec<u32> = std::fs::read_dir(&table)
            .unwrap_or_else(|error| panic!("read {table}: {error}"))
            .map(|entry| {
                entry
                    .expect("a child fd entry")
                    .file_name()
                    .to_str()
                    .expect("an fd name")
                    .parse()
                    .expect("a numeric descriptor")
            })
            .collect();
        fds.sort_unstable();
        fds
    }

    /// The target of one descriptor in `/proc/<pid>/fd`.
    fn child_fd_target(pid: u32, fd: u32) -> std::path::PathBuf {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .unwrap_or_else(|error| panic!("read fd {fd} of {pid}: {error}"))
    }

    #[test]
    fn forget_reaps_a_leader_that_already_exited() {
        let limits = Limits::default();
        // The child has to survive the pre-bind liveness probe (bitty#2790),
        // then exit while the handle is retained. An immediate `exit 0` would
        // be refused as NotBindable instead of producing a bound handle.
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/sleep", "1"]);
        let pid = pool.workers[&session].child.identity().pid().get();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !pool.workers[&session].child.leader_exited() {
            assert!(Instant::now() < deadline, "the worker leader did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }

        // The leader is an unreaped zombie at this point. Forgetting it must
        // settle the handle through the bounded terminate path, which reaps
        // it, rather than dropping the handle without a wait.
        let cleanup = pool.forget(session);
        assert!(
            cleanup.is_some(),
            "an exited but unreaped leader still needs settlement"
        );
        assert!(
            std::fs::metadata(format!("/proc/{pid}")).is_err(),
            "the forgotten leader {pid} must be reaped, not left as a zombie"
        );
    }

    #[test]
    fn one_tick_drains_at_most_the_budget_and_resumes_after_a_read() {
        let limits = Limits::default();
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/sleep", "60"]);
        wait_for_content_ready(&pool, &limits, session);

        // A producer that is already over budget when the tick starts: the
        // test proves the drain loop stops at the budget instead of relying on
        // a kernel buffer size. The real PTY master stays open in this test.
        let _pty_master = use_file_producer(&mut pool, session, DRAIN_BUDGET_BYTES * 4);

        let worker = pool.workers.get_mut(&session).expect("the bound worker");
        worker.drain(DRAIN_BUDGET_BYTES);
        assert_eq!(
            worker.drained_bytes, DRAIN_BUDGET_BYTES as u64,
            "one tick must stop at the per-worker byte budget"
        );
        assert_eq!(
            worker.content.len(),
            crate::content::CONTENT_BUFFER_BYTES,
            "the tick fills the bounded content buffer"
        );
        // The buffer is full, so a second tick makes no progress: that is the
        // terminal backpressure path, not unbounded buffering.
        worker.drain(DRAIN_BUDGET_BYTES);
        assert_eq!(worker.drained_bytes, DRAIN_BUDGET_BYTES as u64);

        // A reader that takes half the buffer re-opens exactly that much for
        // the next tick; the tick still stops at the budget.
        let read = worker.content.pop(CONTENT_FRAME_BYTES);
        assert_eq!(read.len(), CONTENT_FRAME_BYTES);
        worker.drain(DRAIN_BUDGET_BYTES);
        assert_eq!(
            worker.drained_bytes,
            (DRAIN_BUDGET_BYTES + CONTENT_FRAME_BYTES) as u64,
            "the next tick must resume the queued producer after a read"
        );

        let _ = pool.terminate(session);
    }

    /// The child-side half of [`the_child_fd_table_is_exactly_the_pty_stdio`].
    ///
    /// The fd-table test spawns this same test binary as a worker with
    /// `BITTY_FD_TABLE_PROBE=1` and `--ignored`; the child then reports the
    /// descriptors that survived `exec` on its PTY stdout. The `read_dir`
    /// handle is the probe's own transient descriptor and is skipped; every
    /// other entry is inherited state.
    #[test]
    #[ignore = "child probe spawned by the_child_fd_table_is_exactly_the_pty_stdio"]
    fn fd_table_probe() {
        if std::env::var_os("BITTY_FD_TABLE_PROBE").is_none() {
            return;
        }

        let own_table = format!("/proc/{}/fd", std::process::id());
        let mut fds = Vec::new();
        for entry in std::fs::read_dir(&own_table).expect("the probe fd table") {
            let entry = entry.expect("a probe fd entry");
            let fd: u32 = entry
                .file_name()
                .to_str()
                .expect("an fd name")
                .parse()
                .expect("a numeric fd");
            let target = std::fs::read_link(entry.path()).expect("an fd target");
            if target == std::path::Path::new(&own_table) {
                continue;
            }
            fds.push((fd, target));
        }
        fds.sort();

        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "FD_TABLE_BEGIN").expect("the probe marker");
        for (fd, target) in fds {
            writeln!(stdout, "FD_TABLE {fd} {}", target.display()).expect("a probe fd line");
        }
        writeln!(stdout, "FD_TABLE_END").expect("the probe end marker");
        stdout.flush().expect("flush the probe table");
    }

    #[test]
    fn the_child_fd_table_is_exactly_the_pty_stdio() {
        let limits = Limits::default();
        let exe = std::env::current_exe().expect("the test binary");
        let argv = [
            exe.to_str().expect("a UTF-8 test binary path"),
            "--exact",
            "worker::tests::fd_table_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ];
        // Hold a non-close-on-exec pipe across the spawn: the same class of
        // descriptor a CI test harness exposes at fd 142/145 (bitty#2824). A
        // worker whose child-side sweep is missing reports it in the table
        // below, so the assertion does not depend on how the local harness
        // happens to open its own descriptors.
        let _inherited = nix::unistd::pipe().expect("an inherited descriptor pair");
        let (mut pool, session) =
            spawn_test_worker_with_env(&limits, &argv, &["BITTY_FD_TABLE_PROBE=1"]);

        // The probe runs after `exec` in the worker itself, so its table is
        // the inherited state and not the parent's or the dynamic loader's
        // transient handles. No helper process and no new wire surface is
        // involved.
        let master = &mut pool
            .workers
            .get_mut(&session)
            .expect("the bound worker")
            .master;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    output.extend_from_slice(&chunk[..read]);
                    if String::from_utf8_lossy(&output).contains("FD_TABLE_END") {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "the fd-table probe did not finish: {}",
                        String::from_utf8_lossy(&output)
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // Linux reports EIO on the master once the last slave closes.
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::EIO as i32) => break,
                Err(error) => panic!("read the worker PTY: {error}"),
            }
        }
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("FD_TABLE_END"),
            "the fd-table probe did not report a complete table: {text}"
        );

        let mut fds: Vec<(u32, String)> = text
            .lines()
            .filter_map(|line| line.strip_prefix("FD_TABLE "))
            .map(|rest| {
                let (fd, target) = rest.split_once(' ').expect("a probe fd line");
                (fd.parse().expect("a numeric fd"), target.to_owned())
            })
            .collect();
        fds.sort();

        let numbers: Vec<u32> = fds.iter().map(|(fd, _)| *fd).collect();
        assert_eq!(
            numbers,
            vec![0, 1, 2],
            "the worker inherited descriptors beyond stdio: {fds:?}\n{text}"
        );
        let slave = &fds[0].1;
        assert!(
            slave.starts_with("/dev/pts/"),
            "worker stdio must be a PTY slave: {fds:?}\n{text}"
        );
        for (fd, target) in &fds {
            assert_eq!(
                target, slave,
                "fd {fd} does not point at the stdio PTY slave: {fds:?}\n{text}"
            );
        }

        let _ = pool.terminate(session);
    }

    /// `P0-AC-023` / EV-03 negative test: a dropped `CLOEXEC` is observable.
    ///
    /// The control arm spawns the production command through the same
    /// containment path, but a test-only hook clears the close-on-exec bit the
    /// child-side sweep just set. The pinned descriptor is then exactly a
    /// parent handle whose `CLOEXEC` was dropped, and the `/proc/<pid>/fd`
    /// probe must report it: an absent descriptor in the hardened arm is the
    /// sweep's result, not a vacuous read.
    ///
    /// The hardened arm is the real worker spawn path. Its `/proc/<pid>/fd`
    /// holds no injected descriptor and keeps stdio on the PTY slave, so the
    /// `close_range(3, u32::MAX, CLOSE_RANGE_CLOEXEC)` sweep runs before
    /// `exec` and does not touch 0/1/2.
    ///
    /// EV-03 stays `partial`: this is the ordering and dropped-`CLOEXEC`
    /// artifact, not the full evidence package.
    #[test]
    fn close_range_runs_before_exec_and_a_dropped_cloexec_fails_the_probe() {
        use std::os::fd::AsRawFd as _;

        let limits = Limits::default();
        let script = format!("echo {FD_PROBE_READY}; exec /bin/sleep 30");
        let argv = ["/bin/sh", "-c", script.as_str()];
        let probe = inheritable_probe_descriptor();
        let probe_fd = u32::try_from(probe.as_raw_fd()).expect("a positive descriptor");

        // Control arm: the production command, with one test-only hook that
        // drops the flag the sweep set, so the probe must see the descriptor.
        let (mut control, mut control_master) =
            spawn_control_worker(&limits, &argv, probe.as_raw_fd());
        let control_pid = control.identity().pid().get();
        wait_for_fd_probe_ready(&mut control_master);
        let control_fds = child_fd_numbers(control_pid);
        assert!(
            control_fds.contains(&probe_fd),
            "the fd probe cannot see a dropped close-on-exec descriptor, so the \
             hardened assertion would be vacuous: {control_fds:?}"
        );
        let _ = control.terminate(ContainmentLimits::for_session_stop(&limits));
        drop(control_master);

        // Hardened arm: the real worker spawn path, no test hook.
        let (mut pool, session) = spawn_test_worker(&limits, &argv);
        let worker_pid = pool.workers[&session].child.identity().pid().get();
        wait_for_fd_probe_ready(
            &mut pool
                .workers
                .get_mut(&session)
                .expect("the bound worker")
                .master,
        );
        let worker_fds = child_fd_numbers(worker_pid);
        assert!(
            !worker_fds.contains(&probe_fd),
            "fd {probe_fd} survived the worker's exec as a daemon descriptor: {worker_fds:?}"
        );
        let stdio: Vec<u32> = worker_fds.iter().copied().filter(|fd| *fd < 3).collect();
        assert_eq!(
            stdio,
            vec![0, 1, 2],
            "the sweep must not close stdio: {worker_fds:?}"
        );
        let slave = child_fd_target(worker_pid, 0);
        assert!(
            slave.starts_with("/dev/pts/"),
            "worker stdio must be a PTY slave: {slave:?}"
        );
        for fd in 0..3 {
            assert_eq!(
                child_fd_target(worker_pid, fd),
                slave,
                "fd {fd} does not point at the stdio PTY slave"
            );
        }

        let _ = pool.terminate(session);
    }

    #[test]
    fn a_multi_worker_tick_splits_one_shared_budget() {
        let limits = Limits::default();
        let first = SessionId::parse("0000000000000000000000000000c4d1").expect("session id");
        let second = SessionId::parse("0000000000000000000000000000c4d2").expect("session id");
        let mut pool = WorkerPool::new(&limits);
        spawn_test_worker_into(
            &mut pool,
            &limits,
            &["/bin/sleep", "60"],
            &[],
            SessionKind::Terminal,
            first,
        );
        spawn_test_worker_into(
            &mut pool,
            &limits,
            &["/bin/sleep", "60"],
            &[],
            SessionKind::Terminal,
            second,
        );

        // Deterministic producers: a regular file already holds more than the
        // whole shared budget, so each worker can only stop at its own share.
        // Both real PTY masters stay open for the duration of the test.
        let mut pty_masters = Vec::new();
        for session in [first, second] {
            wait_for_content_ready(&pool, &limits, session);
            pty_masters.push(use_file_producer(
                &mut pool,
                session,
                DRAIN_BUDGET_BYTES * 2,
            ));
        }

        let share = per_worker_drain_budget(2);
        assert_eq!(share * 2, DRAIN_BUDGET_BYTES);
        pool.drain_tick();

        for session in [first, second] {
            let worker = &pool.workers[&session];
            assert_eq!(
                worker.drained_bytes, share as u64,
                "each worker gets exactly its share"
            );
            assert_eq!(
                worker.content.len(),
                share,
                "the share lands in the bounded content buffer"
            );
            assert!(
                !worker.content.is_full(),
                "the tick never overfills the per-session buffer"
            );
        }
        let total: u64 = pool
            .workers
            .values()
            .map(|worker| worker.drained_bytes)
            .sum();
        assert_eq!(
            total, DRAIN_BUDGET_BYTES as u64,
            "one tick never exceeds the shared transport budget"
        );

        let _ = pool.terminate(first);
        let _ = pool.terminate(second);
    }

    #[test]
    fn a_slow_reader_stops_the_drain_at_the_buffer_cap() {
        let limits = Limits::default();
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/sleep", "60"]);
        wait_for_content_ready(&pool, &limits, session);

        // A deterministic producer that is already far past the cap. The real
        // PTY master is held open by the test: replacing it with the file and
        // dropping the master hangs the session up, and a leader that has not
        // installed its signal handling yet dies from the pending `SIGHUP`
        // before the cap is reached (a loaded runner loses that race).
        let _pty_master = use_file_producer(&mut pool, session, DRAIN_BUDGET_BYTES * 8);

        // No reader: every tick stops when the bounded buffer is full, and the
        // drained total never grows past the cap or the shared tick budget.
        for _ in 0..8 {
            pool.drain_tick();
        }
        let worker = &pool.workers[&session];
        assert_eq!(
            worker.content.len(),
            crate::content::CONTENT_BUFFER_BYTES,
            "a slow reader never grows the buffer past its cap"
        );
        assert_eq!(
            worker.drained_bytes,
            crate::content::CONTENT_BUFFER_BYTES as u64,
            "a full buffer stops further drain instead of dropping bytes"
        );
        assert!(pool.content_ready(session));

        // One bounded read re-opens exactly that much room for the next tick.
        let read = pool.read(session, CONTENT_FRAME_BYTES).expect("live plane");
        assert_eq!(read.data.len(), CONTENT_FRAME_BYTES);
        pool.drain_tick();
        assert_eq!(
            pool.workers[&session].drained_bytes,
            (crate::content::CONTENT_BUFFER_BYTES + CONTENT_FRAME_BYTES) as u64,
            "the stopped producer resumes after the reader catches up"
        );
        let _ = pool.terminate(session);
    }

    #[test]
    fn a_zero_or_blocked_write_is_the_typed_backpressure_path() {
        assert_eq!(
            classify_write(Ok(0), 8),
            ContentWrite::Backpressure { attempted: 8 }
        );
        assert_eq!(
            classify_write(Err(io::Error::from(io::ErrorKind::WouldBlock)), 8),
            ContentWrite::Backpressure { attempted: 8 }
        );
        assert_eq!(
            classify_write(Err(io::Error::from(io::ErrorKind::Interrupted)), 8),
            ContentWrite::Backpressure { attempted: 8 }
        );
        assert_eq!(
            classify_write(Ok(3), 8),
            ContentWrite::Accepted {
                accepted: 3,
                attempted: 8
            }
        );
        assert_eq!(
            classify_write(Err(io::Error::from(io::ErrorKind::BrokenPipe)), 8),
            ContentWrite::Closed
        );
    }

    #[test]
    fn terminal_content_round_trips_through_read_and_write() {
        let limits = Limits::default();
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/cat"]);
        wait_for_content_ready(&pool, &limits, session);

        match pool.write(session, b"hello\n") {
            ContentWrite::Accepted { accepted, .. } => assert_eq!(accepted, 6),
            other => panic!("the PTY must accept the small write: {other:?}"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while !seen.windows(7).any(|window| window == b"hello\r\n") {
            assert!(
                Instant::now() < deadline,
                "the PTY did not echo the input; seen {seen:?}"
            );
            pool.workers
                .get_mut(&session)
                .expect("the live worker")
                .drain(4096);
            if let Some(read) = pool.read(session, 64) {
                seen.extend_from_slice(&read.data);
            } else {
                panic!("the live terminal must have a content plane");
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let _ = pool.terminate(session);
    }

    #[test]
    fn a_worker_kind_has_no_terminal_content_plane() {
        let limits = Limits::default();
        let (mut pool, session) =
            spawn_test_worker_for(&limits, &["/bin/cat"], SessionKind::Worker);
        assert!(pool.read(session, 16).is_none());
        assert_eq!(
            pool.write(session, b"x"),
            ContentWrite::Closed,
            "a service worker is not a terminal content owner"
        );
        let _ = pool.terminate(session);
    }

    /// A hosted shell must start with the daemon's signal state cleared.
    ///
    /// The daemon blocks `SIGTERM`/`SIGINT`/`SIGHUP` behind its signal fd and
    /// a service manager leaves `SIGPIPE`/`SIGXFSZ` ignored; neither is reset
    /// by `execve`, so without the child-side hook every hosted process keeps
    /// them and a hangup or a termination request is never delivered. The
    /// probe is this same binary re-executed through the production spawn
    /// path, so nothing (a shell in particular, which sets its own mask)
    /// stands between the daemon's fork and the observation (bitty#2920).
    #[test]
    fn a_spawned_session_clears_the_inherited_signal_mask_and_dispositions() {
        /// The end marker the probe writes before it signals itself.
        const END: &str = "SIGNAL_STATE_END";
        /// The `SigIgn` bit of `SIGXFSZ` (signal 25): the ignored disposition
        /// systemd sets for services that a Rust child does not re-set.
        const SIGXFSZ_BIT: u64 = 1 << 24;

        let _inherited = crate::containment::test_signal_state::InheritedSignalState::install();
        let limits = Limits::default();
        let dir = tempfile::tempdir().expect("a temp dir");
        let report = dir.path().join("signal-state");
        let exe = std::env::current_exe().expect("the test binary");
        let argv = [
            exe.to_str().expect("a UTF-8 test binary path"),
            "--exact",
            "worker::tests::signal_state_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ];
        let env = [
            BITTY_SIGNAL_STATE_PROBE.to_owned(),
            format!("BITTY_SIGNAL_STATE_OUT={}", report.display()),
        ];
        let env: Vec<&str> = env.iter().map(String::as_str).collect();
        let (mut pool, session) = spawn_test_worker_with_env(&limits, &argv, &env);

        // The probe writes its report and then terminates itself. Waiting for
        // the exit instead of reading the PTY keeps the observation off the
        // terminal drain: by the time the child is gone, every write it made
        // is in the report.
        let deadline = Instant::now() + Duration::from_secs(10);
        let exited = loop {
            let child = &mut pool
                .workers
                .get_mut(&session)
                .expect("the bound worker")
                .child;
            if child.try_wait().expect("wait for the probe").is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let text = std::fs::read_to_string(&report).expect("the probe report");
        assert!(exited, "the probe did not exit: {text}");

        let field = |name: &str| -> u64 {
            let line = text
                .lines()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} line in the probe report: {text}"));
            let (_, value) = line.split_once(' ').expect("a probe field");
            u64::from_str_radix(value.trim(), 16).expect("a hexadecimal signal mask")
        };

        assert!(
            text.contains(END),
            "the probe did not report a complete signal state: {text}"
        );
        assert_eq!(
            field("SIGBLK"),
            0,
            "the hosted process inherited the daemon's blocked mask: {text}"
        );
        assert_eq!(
            field("SIGIGN") & SIGXFSZ_BIT,
            0,
            "the hosted process inherited an ignored `SIGXFSZ`: {text}"
        );
        assert!(
            !text.contains("SURVIVED"),
            "a termination request did not end the hosted process: {text}"
        );
        let _ = pool.terminate(session);
    }

    /// The environment marker that turns this test binary into the child probe
    /// for [`a_spawned_session_clears_the_inherited_signal_mask_and_dispositions`].
    const BITTY_SIGNAL_STATE_PROBE: &str = "BITTY_SIGNAL_STATE_PROBE=1";

    /// The child-side half of
    /// [`a_spawned_session_clears_the_inherited_signal_mask_and_dispositions`].
    ///
    /// It reports the mask and the ignored set that survived `exec`, then
    /// raises `SIGTERM` on itself. A process that inherited the daemon's
    /// blocked mask is still running when `raise` returns and records
    /// `SURVIVED`; one with a cleared mask dies there.
    #[test]
    #[ignore = "child probe spawned by a_spawned_session_clears_the_inherited_signal_mask"]
    fn signal_state_probe() {
        if std::env::var_os("BITTY_SIGNAL_STATE_PROBE").is_none() {
            return;
        }
        let report_path = std::env::var("BITTY_SIGNAL_STATE_OUT").expect("the probe report path");
        let status = std::fs::read_to_string(format!("/proc/{}/status", std::process::id()))
            .expect("the probe's own status");
        let mut report = String::new();
        for (label, prefix) in [("SIGBLK", "SigBlk:"), ("SIGIGN", "SigIgn:")] {
            let line = status
                .lines()
                .find(|line| line.starts_with(prefix))
                .expect("a signal-state field");
            let (_, value) = line.split_once(':').expect("a signal-state value");
            report.push_str(&format!("{label} {}\n", value.trim()));
        }
        report.push_str("SIGNAL_STATE_END\n");
        std::fs::write(&report_path, &report).expect("the probe report");

        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).expect("raise SIGTERM");
        std::fs::write(&report_path, format!("{report}SURVIVED\n")).expect("the probe report");
    }
}
