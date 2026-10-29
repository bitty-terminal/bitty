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
//! PTY slave only, the master is close-on-exec, and the listener, audit,
//! journal, and capability descriptors are all opened close-on-exec by their
//! owners.
//!
//! The worker path is served on Linux only, and only when the startup
//! containment probe selected an enforced plan. Every other platform or a
//! `Refused` plan returns the typed [`WorkerSpawnError::Unsupported`] and
//! advertises `worker_sessions: false`.

#![allow(unsafe_code)]

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd as _, AsRawFd as _};
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
        if set_nonblocking(master.as_raw_fd()).is_err() {
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
        resize_fd(worker.master.as_raw_fd(), cols, rows).map_err(|_| ResizeError::Pty)
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
    /// A handle whose leader may still run is terminated before it is dropped,
    /// so retiring a session can never leak a live worker. Returns the cleanup
    /// class when this call had to terminate a live handle, so the caller can
    /// settle its journal record; `None` when the handle was already settled or
    /// absent.
    #[cfg(target_os = "linux")]
    pub fn forget(&mut self, session_id: SessionId) -> Option<CleanupClass> {
        let mut worker = self.workers.remove(&session_id)?;
        if worker.settled || worker.child.leader_exited() {
            worker.child.drain_recorded();
            return None;
        }
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
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    self.drained_bytes = self.drained_bytes.saturating_add(read as u64);
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
        let slave = pty.slave;
        set_cloexec(master.as_raw_fd()).map_err(|_| WorkerSpawnError::NotStarted)?;
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
fn dup_file(slave: &std::os::fd::OwnedFd) -> Result<File, WorkerSpawnError> {
    use std::os::fd::BorrowedFd;
    // `openpty` descriptors are not close-on-exec; the duplicates handed to
    // stdio are dup2'ed onto 0/1/2 in the child (which clears CLOEXEC) and
    // dropped in the parent.
    let borrowed: BorrowedFd<'_> = slave.as_fd();
    nix::unistd::dup(borrowed)
        .map(File::from)
        .map_err(|_| WorkerSpawnError::NotStarted)
}

#[cfg(target_os = "linux")]
fn set_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use std::os::fd::BorrowedFd;
    // SAFETY: `fd` is an owned descriptor of this process for the duration of
    // the call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    fcntl(borrowed, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(|_| ())
        .map_err(io::Error::from)
}

#[cfg(target_os = "linux")]
fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::os::fd::BorrowedFd;
    // SAFETY: `fd` is an owned descriptor of this process for the duration of
    // the call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let flags = fcntl(borrowed, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(borrowed, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(io::Error::from)
}

#[cfg(target_os = "linux")]
fn resize_fd(fd: std::os::fd::RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let winsize = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is the owned PTY master of the worker; `winsize` is a valid
    // `struct winsize` for the duration of the call.
    let result = unsafe {
        nix::libc::ioctl(
            fd,
            nix::libc::TIOCSWINSZ as _,
            &winsize as *const nix::pty::Winsize,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
