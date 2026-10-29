//! Worker spawn, PTY ownership, and non-blocking supervision (contract
//! `D-01`, `D-03`, `D-04`, `D-06`; bitty#2790).
//!
//! A worker is spawned exactly once, under the containment plan the daemon
//! probed at startup, with a single-owner PTY:
//!
//! 1. the caller reserves the orphan-journal record with the pre-spawn
//!    containment identity ([`planned_record`]);
//! 2. [`WorkerPool::spawn`] opens the PTY, builds the child from the bounded
//!    spawn specification, and spawns it through
//!    [`crate::containment::spawn_with`] with a new session on the PTY;
//! 3. the caller commits the observed identity, binds generation `1`, and only
//!    then exposes the session as `running`.
//!
//! The poll loop never waits for the worker. [`WorkerPool::supervise`] drains
//! the PTY with non-blocking reads, polls `try_wait`, sends the bounded stop
//! signals when an accepted stop reaches its deadline, and records the
//! terminal transition once the containment identity is settled.
//!
//! The child never inherits the daemon environment or a daemon descriptor:
//! the environment is exactly the explicit spawn specification, stdio is the
//! PTY slave only with close-on-exec duplicates, the master is close-on-exec,
//! and the listener, audit, journal, and capability descriptors are all
//! opened close-on-exec by their owners.
//!
//! The worker path is served on Linux only, and only when the startup
//! containment probe selected an enforced plan. Every other platform or a
//! `Refused` plan returns the typed [`WorkerSpawnError::Unsupported`] and
//! advertises `worker_sessions: false`.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::process::{Command, ExitStatus, Stdio};

#[cfg(target_os = "linux")]
use nix::sys::signal::Signal;

use crate::containment::{
    ContainmentKind, ContainmentRecord, ContainmentReport, SESSION_CGROUP_PREFIX,
};
#[cfg(target_os = "linux")]
use crate::containment::{ContainmentLimits, SessionMode, spawn_with};
#[cfg(target_os = "linux")]
use crate::lease::{ExitObservation, SessionHost, Termination};
#[cfg(target_os = "linux")]
use crate::orphan::OrphanSupervisor;
use crate::registry::{CleanupClass, DaemonInstanceId, SessionId};
#[cfg(target_os = "linux")]
use crate::registry::{LifecycleState, MonotonicMs};
#[cfg(target_os = "linux")]
use crate::spawn::SpawnSpec;

/// Whether this build serves the worker spawn path at all.
///
/// The value is compiled from the platform cell, exactly like the containment
/// plan: Linux enforces a whole-tree identity (cgroup v2 or the subreaper
/// fallback), while every other cell refuses at startup. The served harness is
/// additionally narrowed by [`sessions_served`] from the plan this process
/// actually probed, so a Linux cell whose probe selected no mechanism neither
/// advertises nor serves a worker.
pub const WORKER_SESSIONS_SERVED: bool = cfg!(target_os = "linux");

/// Bytes one worker's PTY may hand the poll loop in a single tick.
///
/// The bound is one transport `max_chunk_bytes`, the largest content unit the
/// service forwards in a frame. A worker that writes continuously is drained
/// for at most one chunk per pass, so it cannot hold the single-threaded
/// service loop inside one session's `read`; the remaining bytes stay queued
/// in the PTY buffer as backpressure and are drained on later ticks.
pub const DRAIN_BUDGET_BYTES: usize =
    crate::limits::LimitId::MaxChunkBytes.default_value() as usize;

/// Whether this compiled cell and the probed containment plan serve workers.
///
/// A [`ContainmentKind::Refused`] plan has no whole-tree identity, so the
/// worker path is the typed `Denied/UnsupportedCapability` and the handshake
/// must not advertise `worker_sessions`. The startup probe selects the plan
/// once; every worker decision reads this same truth.
#[must_use]
pub fn sessions_served(report: &ContainmentReport) -> bool {
    WORKER_SESSIONS_SERVED && report.kind() != ContainmentKind::Refused
}

/// Why a worker spawn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSpawnError {
    /// This cell does not serve the worker path.
    Unsupported,
    /// The spawn failed before any child existed; nothing needs cleanup.
    NotStarted,
    /// A child existed and its cleanup was observed (`swept` or `unverified`).
    Cleanup(CleanupClass),
}

impl std::fmt::Display for WorkerSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("worker sessions are not served on this cell"),
            Self::NotStarted => formatter.write_str("worker spawn failed before any child existed"),
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
    /// Bytes drained from the PTY. Content is discarded in this slice because
    /// no content read method is served; the counter proves the plane drains.
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
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerPool")
            .field("served", &WORKER_SESSIONS_SERVED)
            .finish_non_exhaustive()
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
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = limits;
            Self {}
        }
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
    /// # Errors
    ///
    /// [`WorkerSpawnError::Unsupported`] on a non-served cell,
    /// [`WorkerSpawnError::NotStarted`] when the PTY or the fork failed before
    /// any child existed, or [`WorkerSpawnError::Cleanup`] with the class the
    /// plan could prove after terminating a child that did exist. A failed
    /// spawn leaves no child running and no pool entry.
    #[cfg(target_os = "linux")]
    pub fn spawn(
        &mut self,
        report: &ContainmentReport,
        instance: DaemonInstanceId,
        session_id: SessionId,
        spec: &SpawnSpec,
    ) -> Result<crate::orphan::identity::ProcessIdentity, WorkerSpawnError> {
        let name = cgroup_name(instance, session_id);
        let mut pty = PtyCommand::build(spec)?;
        let master = pty.master.take().ok_or(WorkerSpawnError::NotStarted)?;
        let mut child = match spawn_with(
            report,
            &name,
            &mut pty.command,
            self.limits,
            SessionMode::NewSession,
        ) {
            Ok(child) => child,
            Err(error) => {
                return Err(match error.cleanup() {
                    CleanupClass::NotStarted => WorkerSpawnError::NotStarted,
                    cleanup => WorkerSpawnError::Cleanup(cleanup),
                });
            }
        };
        let identity = child.identity();
        if crate::containment::set_nonblocking(&master).is_err() {
            // The child exists and is already contained: terminate it before
            // the handle is dropped, and report what the cleanup could prove.
            let cleanup = child.terminate(self.limits).class();
            return Err(WorkerSpawnError::Cleanup(cleanup));
        }
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
                drained_bytes: 0,
            },
        );
        Ok(identity)
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
        let session_ids: Vec<SessionId> = self.workers.keys().copied().collect();
        for session_id in session_ids {
            let mut collected: Option<CleanupClass> = None;
            let mut terminal: Option<(ExitObservation, CleanupClass)> = None;
            {
                let Some(worker) = self.workers.get_mut(&session_id) else {
                    continue;
                };
                worker.drain();
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

#[cfg(target_os = "linux")]
impl Worker {
    fn drain(&mut self) {
        let mut buffer = [0u8; 4096];
        let mut budget = DRAIN_BUDGET_BYTES;
        while budget > 0 {
            let want = budget.min(buffer.len());
            match self.master.read(&mut buffer[..want]) {
                Ok(0) => break,
                Ok(read) => {
                    self.drained_bytes = self.drained_bytes.saturating_add(read as u64);
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
        let winsize = nix::pty::Winsize {
            ws_row: spec.rows(),
            ws_col: spec.cols(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty =
            nix::pty::openpty(Some(&winsize), None).map_err(|_| WorkerSpawnError::NotStarted)?;
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        crate::containment::set_cloexec(&master).map_err(|_| WorkerSpawnError::NotStarted)?;
        // The slave is dropped after the duplicates, and it must never be
        // inherited either: keep the parent handle close-on-exec while the
        // child's stdio is built from close-on-exec copies.
        crate::containment::set_cloexec(&slave).map_err(|_| WorkerSpawnError::NotStarted)?;
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
        // The child establishes the session and takes the PTY slave as its
        // controlling terminal inside the one documented syscall seam.
        crate::containment::establish_controlling_terminal(&mut command);
        Ok(Self {
            command,
            master: Some(master),
        })
    }
}

#[cfg(target_os = "linux")]
fn dup_file(slave: &File) -> Result<File, WorkerSpawnError> {
    // `openpty` descriptors are not close-on-exec. The child `dup2`s these
    // copies onto 0/1/2 (which clears CLOEXEC on the target), but the copies
    // themselves must not survive `exec`: duplicate them already
    // close-on-exec so the worker inherits only stdio and never an extra
    // slave handle.
    crate::containment::dup_cloexec(slave).map_err(|_| WorkerSpawnError::NotStarted)
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
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;
    use crate::containment::ContainmentReport;
    use crate::limits::Limits;
    use crate::registry::{DaemonInstanceId, SessionId};

    /// Spawn one live worker through the production path.
    fn spawn_test_worker(limits: &Limits, argv: &[&str]) -> (WorkerPool, SessionId) {
        spawn_test_worker_with_env(limits, argv, &[])
    }

    /// Spawn one live worker with an explicit child environment.
    fn spawn_test_worker_with_env(
        limits: &Limits,
        argv: &[&str],
        env: &[&str],
    ) -> (WorkerPool, SessionId) {
        let report = ContainmentReport::probe();
        let fields = json!({ "argv": argv, "env": env, "cwd": "/" })
            .as_object()
            .expect("a JSON object")
            .clone();
        let spec = SpawnSpec::parse(Some(&fields), limits).expect("a valid spawn spec");
        let session = SessionId::parse("0000000000000000000000000000c4d1").expect("a session id");
        let mut pool = WorkerPool::new(limits);
        // Serialized with the other PTY tests so no concurrent `openpty`
        // descriptor can be inherited by this worker's fork.
        let _pty = crate::containment::test_lock::lock();
        pool.spawn(
            &report,
            DaemonInstanceId::generate().expect("an instance id"),
            session,
            &spec,
        )
        .expect("the test worker must spawn");
        (pool, session)
    }

    #[test]
    fn forget_reaps_a_leader_that_already_exited() {
        let limits = Limits::default();
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/sh", "-c", "exit 0"]);
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
    fn one_tick_drains_at_most_the_budget_and_resumes_next_tick() {
        let limits = Limits::default();
        let (mut pool, session) = spawn_test_worker(&limits, &["/bin/sleep", "60"]);

        // A producer that is already over budget when the tick starts. A
        // regular file is used instead of a PTY or pipe so the queued volume is
        // deterministic: the test proves the drain loop stops at the budget
        // instead of relying on a kernel buffer size.
        let mut master = tempfile::tempfile().expect("a temporary producer");
        master
            .write_all(&vec![b'x'; DRAIN_BUDGET_BYTES * 4])
            .expect("fill the over-budget producer");
        master
            .seek(SeekFrom::Start(0))
            .expect("rewind the producer");

        let worker = pool.workers.get_mut(&session).expect("the bound worker");
        worker.master = master;
        worker.drain();
        assert_eq!(
            worker.drained_bytes, DRAIN_BUDGET_BYTES as u64,
            "one tick must stop at the per-worker byte budget"
        );
        worker.drain();
        assert_eq!(
            worker.drained_bytes,
            (DRAIN_BUDGET_BYTES * 2) as u64,
            "the next tick must resume the queued producer"
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
}
