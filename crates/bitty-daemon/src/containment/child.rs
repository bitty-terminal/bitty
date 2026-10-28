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
    /// The cgroup plan failed.
    Cgroup(CgroupError),
    /// The subreaper fallback is unavailable.
    Subreaper(SubreaperRefusal),
    /// `spawn` itself failed.
    Spawn(io::Error),
    /// The child's `/proc` identity could not be observed.
    IdentityUnavailable,
}

impl fmt::Display for ContainmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(formatter, "containment refused: {refusal}"),
            Self::Cgroup(error) => write!(formatter, "cgroup containment failed: {error}"),
            Self::Subreaper(error) => write!(formatter, "subreaper unavailable: {error}"),
            Self::Spawn(error) => write!(formatter, "spawn failed: {error}"),
            Self::IdentityUnavailable => {
                formatter.write_str("spawned child identity could not be observed")
            }
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
/// from the daemon instance and session ids, never from user text.
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
) -> Result<ContainedChild, ContainmentError> {
    match report.kind() {
        ContainmentKind::CgroupV2 => spawn_cgroup(report, name, command),
        ContainmentKind::Subreaper => spawn_subreaper(name, command),
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
    command.process_group(0);
    let child = match linux_sys::spawn_into_cgroup(command, procs) {
        Ok(child) => child,
        Err(error) => {
            let _ = cgroup.remove();
            return Err(error.into());
        }
    };
    let pid = child.id();
    let Some(identity) = observe_identity(pid) else {
        abandon_cgroup_child(child, &cgroup);
        return Err(ContainmentError::IdentityUnavailable);
    };
    match cgroup.verify_member(pid) {
        Ok(()) | Err(CgroupError::Missing) => {}
        Err(_) => {
            abandon_cgroup_child(child, &cgroup);
            return Err(ContainmentError::Cgroup(CgroupError::MembershipMismatch));
        }
    }
    remember_child(pid);
    Ok(ContainedChild {
        child,
        identity,
        scope: Scope::Cgroup(cgroup),
    })
}

fn spawn_subreaper(_name: &str, command: &mut Command) -> Result<ContainedChild, ContainmentError> {
    SubreaperGuard::enable().map_err(ContainmentError::Subreaper)?;
    command.process_group(0);
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
        return Err(ContainmentError::IdentityUnavailable);
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
fn abandon_cgroup_child(mut child: Child, cgroup: &SessionCgroup) {
    let _ = cgroup.kill_all();
    let _ = child.kill();
    let _ = child.wait();
    let limits = ContainmentLimits::default();
    let _ = wait_bounded(limits.kill_wait(), limits, || {
        cgroup.verify_empty().unwrap_or(false)
    });
    let _ = cgroup.remove();
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
    if empty {
        let _ = cgroup.remove();
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
    // The process may already be gone; only an exact identity earns a signal.
    if !identity_matches(identity) {
        let _ = child.try_wait();
        return CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled: 0,
        };
    }
    let Ok(pidfd) = PidFd::open(pid.get()) else {
        return CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled: 0,
        };
    };
    let mut signalled = 0;
    let term_sent = pidfd.send_signal(Signal::SIGTERM).is_ok();
    if term_sent {
        signalled += 1;
        if let Ok(raw) = i32::try_from(pgid.get()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), Signal::SIGTERM);
        }
    }
    let reaped = wait_bounded(limits.term_grace(), limits, || {
        matches!(child.try_wait(), Ok(Some(_)) | Err(_))
    });
    // The pidfd pins the exact process even after exit. The process group is
    // only signalled while the leader is unreaped, so its id cannot have been
    // reused by an unrelated group.
    let _ = pidfd.send_signal(Signal::SIGKILL);
    if !reaped {
        if let Ok(raw) = i32::try_from(pgid.get()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), Signal::SIGKILL);
        }
    }
    let _ = wait_bounded(limits.kill_wait(), limits, || {
        matches!(child.try_wait(), Ok(Some(_)) | Err(_))
    });
    drain_adopted(pid.get());
    // A process group is never a whole-tree proof.
    CleanupOutcome {
        class: CleanupClass::Unverified,
        signalled,
    }
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
