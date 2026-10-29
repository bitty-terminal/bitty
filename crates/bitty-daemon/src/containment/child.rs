//! Spawn, bounded signal escalation, and reaping for one contained child
//! (contract `D-04`, `D-06`).
//!
//! A child is contained from before `exec`: the cgroup plan writes the child's
//! own pid into the session subtree in `pre_exec`, and the subreaper fallback
//! records the child's pid, start time, uid, and process group. Termination
//! sends a graceful signal to the recorded identity only, waits a bounded
//! grace, escalates to `SIGKILL`, and verifies the identity gone. A cgroup
//! identity that verifies empty is `swept`; the fallback is always
//! `unverified`, because a process group cannot prove a whole tree.
//!
//! No function here resolves a name, scans for a "newest" process, or signals a
//! pid it did not record.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;

use crate::containment::cgroup::{CgroupError, CgroupRoot, SessionCgroup};
use crate::containment::linux_sys::{self, PidFd};
use crate::containment::subreaper::{SubreaperGuard, SubreaperRefusal};
use crate::containment::{
    ContainmentKind, ContainmentLimits, ContainmentRecord, ContainmentRefusal, ContainmentReport,
};
use crate::orphan::identity::{ProcessGroupId, ProcessIdentity, ProcessObservation, observe};
use crate::registry::CleanupClass;

/// Pids owned by a live [`ContainedChild`] handle. The fallback reaper uses
/// this to avoid reaping another session's child while draining adopted
/// grandchildren.
static LIVE_CHILDREN: LazyLock<Mutex<BTreeSet<u32>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

/// A spawn failure. Every variant refuses the spawn; none leaves an
/// uncontained child running.
#[derive(Debug)]
pub enum ContainmentError {
    /// The probe selected no mechanism.
    Refused(ContainmentRefusal),
    /// The cgroup plan failed before a child existed.
    Cgroup(CgroupError),
    /// The subreaper fallback is unavailable.
    Subreaper(SubreaperRefusal),
    /// `spawn` itself failed; no child was created.
    Spawn(io::Error),
    /// The child's `/proc` identity could not be observed. The child existed
    /// and was terminated; the carried class is what the cleanup could prove
    /// (`unverified` for the process-group plan).
    IdentityUnavailable(CleanupClass),
    /// The child ran but was outside the session's cgroup subtree. The child
    /// existed and was terminated; the carried class is what the cleanup could
    /// prove (`swept` or `unverified`).
    Uncontained(CleanupClass),
}

impl ContainmentError {
    /// The cleanup class observed while the failed spawn was abandoned.
    ///
    /// A refusal, a pre-fork failure, or an unavailable mechanism started
    /// nothing ([`CleanupClass::NotStarted`]); a failure after the fork
    /// reports what the plan could prove about the child it killed.
    #[must_use]
    pub const fn cleanup(&self) -> CleanupClass {
        match self {
            Self::Refused(_) | Self::Cgroup(_) | Self::Subreaper(_) | Self::Spawn(_) => {
                CleanupClass::NotStarted
            }
            Self::IdentityUnavailable(cleanup) | Self::Uncontained(cleanup) => *cleanup,
        }
    }
}

impl fmt::Display for ContainmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(formatter, "containment refused: {refusal}"),
            Self::Cgroup(error) => write!(formatter, "cgroup containment failed: {error}"),
            Self::Subreaper(error) => write!(formatter, "subreaper unavailable: {error}"),
            Self::Spawn(error) => write!(formatter, "spawn failed: {error}"),
            Self::IdentityUnavailable(cleanup) => write!(
                formatter,
                "spawned child identity could not be observed; cleanup {}",
                cleanup.as_str()
            ),
            Self::Uncontained(cleanup) => write!(
                formatter,
                "spawned child escaped the session cgroup; cleanup {}",
                cleanup.as_str()
            ),
        }
    }
}

impl std::error::Error for ContainmentError {}

impl From<CgroupError> for ContainmentError {
    fn from(error: CgroupError) -> Self {
        Self::Cgroup(error)
    }
}

impl From<io::Error> for ContainmentError {
    fn from(error: io::Error) -> Self {
        Self::Spawn(error)
    }
}

/// What a terminal cleanup could prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    class: CleanupClass,
    signalled: usize,
}

impl CleanupOutcome {
    /// The cleanup class (`swept`, `unverified`, `not_started`).
    #[must_use]
    pub const fn class(self) -> CleanupClass {
        self.class
    }

    /// How many recorded members received a signal. Never a pid list.
    #[must_use]
    pub const fn signalled(self) -> usize {
        self.signalled
    }
}

/// Spawn a command inside the selected containment plan.
///
/// `name` must be unique per live session and carry the
/// [`crate::containment::SESSION_CGROUP_PREFIX`] prefix; the caller derives it
/// from the daemon instance and session ids, never from user text. `limits`
/// bounds the escalation used by the failure path that abandons a child whose
/// identity could not be observed.
///
/// # Errors
///
/// Refuses a non-enforced plan, an invalid name, a cgroup creation/membership
/// failure, or a spawn failure. A child that fails the membership check is
/// killed through its recorded identity before the error returns.
pub fn spawn(
    report: &ContainmentReport,
    name: &str,
    command: &mut Command,
    limits: ContainmentLimits,
) -> Result<ContainedChild, ContainmentError> {
    spawn_with(report, name, command, limits, SessionMode::ProcessGroup)
}

/// How the spawned child relates to its process group and session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// The daemon puts the child in a fresh process group
    /// (`setpgid(0, 0)`), which is the fallback containment identity.
    ProcessGroup,
    /// The caller's `pre_exec` establishes a new session (`setsid`), so the
    /// child is already a session and process-group leader. Used by the PTY
    /// spawn path, where `setsid` must run before `setpgid`.
    NewSession,
}

/// Spawn with an explicit [`SessionMode`].
///
/// # Errors
///
/// The same refusals as [`spawn`].
pub fn spawn_with(
    report: &ContainmentReport,
    name: &str,
    command: &mut Command,
    limits: ContainmentLimits,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    match report.kind() {
        ContainmentKind::CgroupV2 => spawn_cgroup(report, name, command, limits, mode),
        ContainmentKind::Subreaper => spawn_subreaper(name, command, mode),
        ContainmentKind::Refused => Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        )),
    }
}

fn spawn_cgroup(
    report: &ContainmentReport,
    name: &str,
    command: &mut Command,
    limits: ContainmentLimits,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    let Some(root_path) = report.cgroup_root() else {
        return Err(ContainmentError::Cgroup(CgroupError::Missing));
    };
    let root = CgroupRoot::from_absolute(root_path.to_path_buf())?;
    let cgroup = root.create_session(name)?;
    // Open before the spawn: the child writes its pid before `exec`, so a
    // descendant can never be born outside the subtree.
    let procs = match File::options()
        .write(true)
        .open(cgroup.path().join("cgroup.procs"))
    {
        Ok(procs) => procs,
        Err(error) => {
            let _ = cgroup.remove();
            return Err(error.into());
        }
    };
    if mode == SessionMode::ProcessGroup {
        command.process_group(0);
    }
    let child = match linux_sys::spawn_into_cgroup(command, procs) {
        Ok(child) => child,
        Err(error) => {
            let _ = cgroup.remove();
            return Err(error.into());
        }
    };
    let pid = child.id();
    let Some(identity) = observe_identity(pid) else {
        let cleanup = abandon_cgroup_child(child, &cgroup, limits);
        return Err(ContainmentError::IdentityUnavailable(cleanup));
    };
    match cgroup.verify_member(pid) {
        Ok(()) | Err(CgroupError::Missing) => {}
        Err(_) => {
            let cleanup = abandon_cgroup_child(child, &cgroup, limits);
            return Err(ContainmentError::Uncontained(cleanup));
        }
    }
    remember_child(pid);
    Ok(ContainedChild {
        child,
        identity,
        scope: Scope::Cgroup(cgroup),
    })
}

fn spawn_subreaper(
    _name: &str,
    command: &mut Command,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    SubreaperGuard::enable().map_err(ContainmentError::Subreaper)?;
    if mode == SessionMode::ProcessGroup {
        command.process_group(0);
    }
    let child = command.spawn()?;
    let pid = child.id();
    let Some(identity) = observe_identity(pid) else {
        let mut child = child;
        // `process_group(0)` makes the child the leader of a fresh group, so
        // its pgid is its pid and the group cannot be reused while the child
        // is unreaped. Kill the recorded group, then reap the leader.
        if let Ok(raw) = i32::try_from(pid) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), Signal::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        drain_adopted(pid);
        // A process group can never prove a whole tree, so the fallback
        // reports `unverified` even after this cleanup.
        return Err(ContainmentError::IdentityUnavailable(
            CleanupClass::Unverified,
        ));
    };
    let pgid = identity.process_group();
    remember_child(pid);
    Ok(ContainedChild {
        child,
        identity,
        scope: Scope::Subreaper { pgid },
    })
}

/// Kill a cgroup child tree and remove its subtree when a post-spawn check
/// fails. The child has already `exec`ed and may have forked, so a pid-only
/// kill is not enough: `cgroup.kill` reaches the whole recorded identity, then
/// a bounded wait lets the empty subtree be removed.
///
/// Returns the cleanup class the attempt could prove: `swept` only when the
/// subtree verified empty and was removed, `unverified` otherwise.
fn abandon_cgroup_child(
    mut child: Child,
    cgroup: &SessionCgroup,
    limits: ContainmentLimits,
) -> CleanupClass {
    let _ = cgroup.kill_all();
    let _ = child.kill();
    let _ = child.wait();
    let empty = wait_bounded(limits.kill_wait(), limits, || {
        cgroup.verify_empty().unwrap_or(false)
    });
    // Only an empty subtree can be removed, and only a successful removal
    // leaves nothing behind. A populated subtree is reported as unverified; it
    // is never removed while members remain.
    if empty && cgroup.remove().is_ok() {
        CleanupClass::Swept
    } else {
        CleanupClass::Unverified
    }
}

fn observe_identity(pid: u32) -> Option<ProcessIdentity> {
    let pid = crate::orphan::identity::Pid::new(pid)?;
    match observe(pid) {
        ProcessObservation::Present {
            start_time,
            process_group,
            uid,
        } => Some(ProcessIdentity::new(pid, start_time, process_group, uid)),
        ProcessObservation::Absent | ProcessObservation::Unreadable => None,
    }
}

#[derive(Debug)]
enum Scope {
    Cgroup(SessionCgroup),
    Subreaper { pgid: ProcessGroupId },
}

/// One child whose whole-tree identity was established before it ran.
#[derive(Debug)]
pub struct ContainedChild {
    child: Child,
    identity: ProcessIdentity,
    scope: Scope,
}

impl ContainedChild {
    /// The exact recorded identity.
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    /// The recorded containment identity, suitable for the orphan journal.
    #[must_use]
    pub fn containment(&self) -> ContainmentRecord {
        match &self.scope {
            Scope::Cgroup(cgroup) => ContainmentRecord::cgroup(cgroup.path()),
            Scope::Subreaper { .. } => ContainmentRecord::Subreaper,
        }
    }

    /// Wait for the child and return its exit status.
    ///
    /// # Errors
    ///
    /// Propagates a wait failure.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Take the child's standard output, when it was piped.
    #[must_use]
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Whether the direct child has been reaped.
    ///
    /// # Errors
    ///
    /// Propagates a `try_wait` failure other than an already-reaped child.
    pub fn has_exited(&mut self) -> io::Result<bool> {
        self.child.try_wait().map(|status| status.is_some())
    }

    /// Non-blocking wait: the exit status once the direct child has exited.
    ///
    /// The recorded containment identity outlives this call; a descendant that
    /// outlives the leader is still terminated by [`Self::terminate`].
    ///
    /// # Errors
    ///
    /// Propagates a wait failure.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Whether the direct child has exited, without reaping it.
    ///
    /// `waitid(WNOWAIT)` peeks at the exit status and leaves the child a
    /// zombie. A zombie keeps its pid, and therefore its process-group id,
    /// reserved, so the recorded group can still be signalled after a natural
    /// exit without racing a reused id. [`Self::try_wait`] then reaps it.
    #[must_use]
    pub fn leader_exited(&self) -> bool {
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};

        let Ok(pid) = i32::try_from(self.child.id()) else {
            return false;
        };
        matches!(
            waitid(
                Id::Pid(nix::unistd::Pid::from_raw(pid)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ),
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        )
    }

    /// Drain the recorded group and adopted members after the leader was
    /// reaped.
    ///
    /// The cgroup plan reaches its members through the subtree, so there is
    /// nothing to drain there. The subreaper fallback waits on the recorded
    /// process group and adopts the leader's reparented descendants; both are
    /// drained without blocking.
    pub fn drain_recorded(&self) {
        if let Scope::Subreaper { pgid } = &self.scope {
            drain_group(pgid.get());
            drain_adopted(self.identity.pid().get());
        }
    }

    /// Send a signal to the whole recorded identity without waiting.
    ///
    /// This is the graceful/kill step of a supervision loop that must not
    /// block. A cgroup plan reaches every member; the subreaper fallback sends
    /// through the pidfd and the recorded process group only while the recorded
    /// leader identity still matches, so a reused pid or group is never
    /// signalled. Returns how many recorded targets received the signal.
    pub fn signal(&self, signal: Signal) -> usize {
        match &self.scope {
            Scope::Cgroup(cgroup) => cgroup.signal_members(signal).map_or(0, |pids| pids.len()),
            Scope::Subreaper { pgid } => {
                if !identity_matches(self.identity) {
                    return 0;
                }
                let mut signalled = 0;
                if let Ok(pidfd) = PidFd::open(self.identity.pid().get()) {
                    if pidfd.send_signal(signal).is_ok() {
                        signalled += 1;
                    }
                }
                if let Ok(raw) = i32::try_from(pgid.get()) {
                    if nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal).is_ok() {
                        signalled += 1;
                    }
                }
                signalled
            }
        }
    }

    /// Settle the containment identity after the direct child was reaped.
    ///
    /// Returns `Some(cleanup)` once the plan can classify the whole tree:
    /// `swept` when the cgroup subtree is empty and removed, `unverified` for
    /// the subreaper fallback (a process group never proves a whole tree), and
    /// `None` while a killed cgroup subtree still has members. Never waits.
    #[must_use]
    pub fn sweep_if_empty(&mut self) -> Option<CleanupOutcome> {
        match &mut self.scope {
            Scope::Cgroup(cgroup) => match cgroup.verify_empty() {
                Ok(true) => Some(if cgroup.remove().is_ok() {
                    CleanupOutcome {
                        class: CleanupClass::Swept,
                        signalled: 0,
                    }
                } else {
                    CleanupOutcome {
                        class: CleanupClass::Unverified,
                        signalled: 0,
                    }
                }),
                Ok(false) | Err(_) => None,
            },
            Scope::Subreaper { .. } => Some(CleanupOutcome {
                class: CleanupClass::Unverified,
                signalled: 0,
            }),
        }
    }

    /// Terminate the whole recorded identity with a bounded escalation.
    ///
    /// Sends `SIGTERM`, waits [`ContainmentLimits::term_grace`], sends
    /// `SIGKILL` through the cgroup or the pidfd plus recorded process group,
    /// waits [`ContainmentLimits::kill_wait`], and reaps the child. Only the
    /// cgroup plan can return [`CleanupClass::Swept`].
    pub fn terminate(&mut self, limits: ContainmentLimits) -> CleanupOutcome {
        match &mut self.scope {
            Scope::Cgroup(cgroup) => term_cgroup(cgroup, &mut self.child, limits),
            Scope::Subreaper { pgid } => {
                term_subreaper(self.identity, *pgid, &mut self.child, limits)
            }
        }
    }
}

fn term_cgroup(
    cgroup: &SessionCgroup,
    child: &mut Child,
    limits: ContainmentLimits,
) -> CleanupOutcome {
    let signalled = cgroup
        .signal_members(Signal::SIGTERM)
        .map_or(0, |pids| pids.len());
    let empty = wait_bounded(limits.term_grace(), limits, || {
        let _ = child.try_wait();
        cgroup.verify_empty().unwrap_or(false)
    });
    if !empty {
        let _ = cgroup.kill_all();
    }
    let empty = wait_bounded(limits.kill_wait(), limits, || {
        let _ = child.try_wait();
        cgroup.verify_empty().unwrap_or(false)
    });
    // `swept` is the whole-tree claim; it needs the subtree removed, not just
    // an empty read. A removal failure leaves the identity in place and is
    // reported as unverified.
    if empty && cgroup.remove().is_ok() {
        CleanupOutcome {
            class: CleanupClass::Swept,
            signalled,
        }
    } else {
        CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled,
        }
    }
}

fn term_subreaper(
    identity: ProcessIdentity,
    pgid: ProcessGroupId,
    child: &mut Child,
    limits: ContainmentLimits,
) -> CleanupOutcome {
    let pid = identity.pid();
    // Open the pidfd before checking the identity: the pidfd pins whatever
    // process currently holds the pid, and the identity check then proves that
    // pinned process is the recorded one. A pid reused between the observation
    // and the open is refused instead of signalled.
    let Ok(pidfd) = PidFd::open(pid.get()) else {
        let _ = child.try_wait();
        return CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled: 0,
        };
    };
    // The process may already be gone; only an exact identity earns a signal.
    if !identity_matches(identity) {
        let _ = child.try_wait();
        return CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled: 0,
        };
    }
    let mut signalled = 0;
    let term_sent = pidfd.send_signal(Signal::SIGTERM).is_ok();
    if term_sent {
        signalled += 1;
        if let Ok(raw) = i32::try_from(pgid.get()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), Signal::SIGTERM);
        }
    }
    // Keep the leader unreaped through the grace window. While it is a zombie
    // its pid, and therefore the process-group id, cannot be reused, so a
    // `SIGTERM`-ignoring descendant is still in the recorded group when the
    // kill below lands. Reaping the leader first would let the group id be
    // recycled and leak the descendant.
    let _ = wait_bounded(limits.term_grace(), limits, || !leader_running(pid.get()));
    let _ = pidfd.send_signal(Signal::SIGKILL);
    if let Ok(raw) = i32::try_from(pgid.get()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), Signal::SIGKILL);
    }
    // Reap the recorded group, including adopted zombies, until the group id
    // is free or the kill window ends. A zombie keeps its process-group id
    // reserved, so an empty group proves the recorded tree was reaped, not
    // merely killed. The group was minted for this session by
    // `process_group(0)`, so no other session's child is waited on here.
    let _ = wait_bounded(limits.kill_wait(), limits, || {
        drain_group(pgid.get());
        drain_adopted(pid.get());
        !group_has_members(pgid.get())
    });
    // A process group is never a whole-tree proof.
    CleanupOutcome {
        class: CleanupClass::Unverified,
        signalled,
    }
}

/// Whether the recorded leader can still run, without reaping it.
///
/// `/proc` reports a zombie as `Z`; neither a zombie nor an absent entry can
/// execute, and both leave the pid unreaped so the process-group id stays
/// reserved.
fn leader_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.trim_start().chars().next(), Some('Z' | 'X' | 'x'))
}

/// Reap every child in one recorded process group without blocking.
///
/// The group is the recorded identity, minted for this session; only its
/// members are waited on. `WNOHANG` returns `StillAlive` when a member is
/// still running, so the caller's bounded loop decides how long to wait.
fn drain_group(pgid: u32) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    let Ok(raw) = i32::try_from(pgid) else {
        return;
    };
    loop {
        match waitpid(nix::unistd::Pid::from_raw(-raw), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive)
            | Ok(WaitStatus::Stopped(..))
            | Ok(WaitStatus::Continued(_))
            | Ok(WaitStatus::PtraceEvent(..))
            | Ok(WaitStatus::PtraceSyscall(_))
            | Err(_) => break,
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {}
        }
    }
}

/// Whether the recorded process group still has any member, including a
/// zombie. Signal 0 only checks existence; it never delivers anything.
fn group_has_members(pgid: u32) -> bool {
    let Ok(raw) = i32::try_from(pgid) else {
        return false;
    };
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), None).is_ok()
}

fn identity_matches(identity: ProcessIdentity) -> bool {
    match observe(identity.pid()) {
        ProcessObservation::Present {
            start_time,
            process_group,
            uid,
        } => {
            start_time == identity.start_time()
                && process_group == identity.process_group()
                && uid == identity.uid()
        }
        ProcessObservation::Absent | ProcessObservation::Unreadable => false,
    }
}

fn remember_child(pid: u32) {
    if let Ok(mut live) = LIVE_CHILDREN.lock() {
        live.insert(pid);
    }
}

fn forget_child(pid: u32) {
    if let Ok(mut live) = LIVE_CHILDREN.lock() {
        live.remove(&pid);
    }
}

fn live_children() -> BTreeSet<u32> {
    LIVE_CHILDREN
        .lock()
        .map_or_else(|_| BTreeSet::new(), |live| live.clone())
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        forget_child(self.identity.pid().get());
    }
}

/// Reap adopted children without stealing another session's child.
///
/// `waitid(WNOWAIT)` peeks at the next exited child; a pid owned by a live
/// [`ContainedChild`] or the child this call is terminating is left for its own
/// handle. Only an adopted grandchild (no handle) is reaped. Nothing is
/// signalled and no name is resolved.
fn drain_adopted(except: u32) {
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};

    let live = live_children();
    loop {
        let flags = WaitPidFlag::WNOHANG | WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT;
        let status = match waitid(Id::All, flags) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(status) => status,
        };
        let pid = match status {
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => pid,
            WaitStatus::Stopped(pid, _) | WaitStatus::Continued(pid) => pid,
            WaitStatus::StillAlive | WaitStatus::PtraceEvent(..) | WaitStatus::PtraceSyscall(_) => {
                break;
            }
        };
        let raw = u32::try_from(pid.as_raw()).unwrap_or(0);
        if raw == except || live.contains(&raw) {
            break;
        }
        let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
    }
}

fn wait_bounded(
    window: Duration,
    limits: ContainmentLimits,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + window;
    loop {
        if done() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(limits.poll.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_spawn_failure_keeps_the_observed_cleanup_class() {
        assert_eq!(
            ContainmentError::Spawn(io::Error::from(io::ErrorKind::NotFound)).cleanup(),
            CleanupClass::NotStarted,
            "a fork failure started nothing"
        );
        assert_eq!(
            ContainmentError::Refused(ContainmentRefusal::NotProbed).cleanup(),
            CleanupClass::NotStarted
        );
        assert_eq!(
            ContainmentError::IdentityUnavailable(CleanupClass::Unverified).cleanup(),
            CleanupClass::Unverified,
            "a child existed and the fallback cannot prove a whole tree"
        );
        assert_eq!(
            ContainmentError::Uncontained(CleanupClass::Swept).cleanup(),
            CleanupClass::Swept,
            "a verified-empty subtree is the observed class"
        );
    }

    #[test]
    fn an_abandoned_child_is_killed_and_its_subtree_removed() {
        let report = ContainmentReport::probe();
        let Some(root_path) = report.cgroup_root() else {
            eprintln!("cgroup unavailable; typed skip");
            return;
        };
        let root = CgroupRoot::from_absolute(root_path.to_path_buf()).expect("delegated root");
        let name = format!("bittyd-test-abandon-{}", std::process::id());
        let cgroup = root.create_session(&name).expect("session cgroup");
        let procs = File::options()
            .write(true)
            .open(cgroup.path().join("cgroup.procs"))
            .expect("cgroup.procs");
        let mut command = Command::new("sleep");
        command.arg("30");
        command.process_group(0);
        let child = linux_sys::spawn_into_cgroup(&mut command, procs).expect("child");
        let pid = child.id();
        assert!(observe_identity(pid).is_some());

        abandon_cgroup_child(
            child,
            &cgroup,
            ContainmentLimits::new(Duration::from_millis(100), Duration::from_secs(2)),
        );

        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the abandoned child was not reaped"
        );
        assert!(
            !cgroup.path().exists(),
            "the abandoned subtree was not removed"
        );
    }
}
