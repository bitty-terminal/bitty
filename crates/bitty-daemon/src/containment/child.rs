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
use std::fs::{File, read_dir, read_to_string};
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};

use nix::sys::signal::Signal;

use crate::containment::cgroup::{CgroupError, CgroupRoot, SessionCgroup};
use crate::containment::linux_sys::{self, PidFd};
use crate::containment::posix::{
    drain_adopted, drain_group, forget_child, group_has_members, remember_child,
};
use crate::containment::subreaper::SubreaperGuard;
use crate::containment::{
    CleanupOutcome, ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRecord,
    ContainmentRefusal, ContainmentReport, SessionMode, classify_spawn_error, wait_bounded,
};
use crate::orphan::identity::{ProcessGroupId, ProcessIdentity, ProcessObservation, observe};
use crate::registry::CleanupClass;

/// Spawn with an explicit [`SessionMode`] inside one of the two Linux plans.
///
/// # Errors
///
/// The same refusals as [`crate::containment::spawn`].
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
        _ => Err(ContainmentError::Refused(
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
            return Err(classify_spawn_error(error));
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
    let child = command.spawn().map_err(classify_spawn_error)?;
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
        scope: Scope::Subreaper {
            pgid,
            pidfd: PidFd::open(pid).ok(),
        },
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
    Subreaper {
        pgid: ProcessGroupId,
        /// The recorded leader handle. `None` means it could not be opened;
        /// the plan then cannot claim a verified sweep for this child.
        pidfd: Option<PidFd>,
    },
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

    /// The journal identity this plan can re-verify after a restart.
    ///
    /// The cgroup and subreaper plans record the `/proc` start time, so the
    /// orphan verifier can re-check the exact process after a daemon restart.
    #[must_use]
    pub const fn durable_identity(&self) -> Option<ProcessIdentity> {
        Some(self.identity)
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
        if let Scope::Subreaper { pgid, .. } = &self.scope {
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
            Scope::Subreaper { pgid, pidfd, .. } => {
                if !identity_matches(self.identity) {
                    return 0;
                }
                let mut signalled = 0;
                if pidfd
                    .as_ref()
                    .is_some_and(|pidfd| pidfd.send_signal(signal).is_ok())
                {
                    signalled += 1;
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
    /// `swept` when the cgroup subtree is empty and removed, or when the
    /// subreaper recorded group is empty, the leader pidfd reports exit, and
    /// no descendant was adopted after the spawn; `unverified` when any of
    /// those checks fails; and `None` while a killed cgroup subtree still has
    /// members. Never waits.
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
            Scope::Subreaper { .. } => {
                // A natural exit has already detached the descendants to this
                // subreaper before the poll tick observes it. Without a
                // pre-signal descendant scan this path cannot prove the tree
                // empty, so it stays conservative. A stop scans descendants
                // before it signals; see `terminate`.
                drain_adopted(self.identity.pid().get());
                Some(CleanupOutcome {
                    class: CleanupClass::Unverified,
                    signalled: 0,
                })
            }
        }
    }

    /// Terminate the whole recorded identity with a bounded escalation.
    ///
    /// Sends `SIGTERM`, waits [`ContainmentLimits::term_grace`], sends
    /// `SIGKILL` through the cgroup or the pidfd plus recorded process group,
    /// waits [`ContainmentLimits::kill_wait`], and reaps the child. The cgroup
    /// plan and the subreaper plan both claim [`CleanupClass::Swept`] only
    /// when their respective whole-tree check verifies empty.
    pub fn terminate(&mut self, limits: ContainmentLimits) -> CleanupOutcome {
        match &mut self.scope {
            Scope::Cgroup(cgroup) => term_cgroup(cgroup, &mut self.child, limits),
            Scope::Subreaper { pgid, pidfd } => term_subreaper(
                self.identity,
                *pgid,
                pidfd.as_ref(),
                &mut self.child,
                limits,
            ),
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
    pidfd: Option<&PidFd>,
    child: &mut Child,
    limits: ContainmentLimits,
) -> CleanupOutcome {
    let pid = identity.pid();
    // The stored pidfd pins the recorded process. If it could not be opened,
    // the plan can still signal the recorded group but cannot verify the
    // leader's exit, so the cleanup stays `unverified`.
    // The process may already be gone; only an exact identity earns a signal.
    if !identity_matches(identity) {
        let _ = child.try_wait();
        return CleanupOutcome {
            class: CleanupClass::Unverified,
            signalled: 0,
        };
    }
    // Record every live descendant before the first signal. A `setsid` child
    // leaves the process group but is still a descendant at this point; its
    // pidfd later proves whether the stop reached it. If the leader is no
    // longer running or the scan cannot read `/proc`, the sweep is unproved.
    let descendants = if leader_running(pid.get()) {
        descendant_pidfds(pid.get())
    } else {
        None
    };
    let mut signalled = 0;
    let term_sent = pidfd.is_some_and(|pidfd| pidfd.send_signal(Signal::SIGTERM).is_ok());
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
    if let Some(pidfd) = pidfd {
        let _ = pidfd.send_signal(Signal::SIGKILL);
    }
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
    CleanupOutcome {
        class: if subreaper_swept(pgid, pidfd, descendants.as_deref()) {
            CleanupClass::Swept
        } else {
            CleanupClass::Unverified
        },
        signalled,
    }
}

/// Verify the subreaper whole-tree claim for one stop.
///
/// The recorded group must be empty, the leader pidfd must report exit, and
/// every descendant pidfd recorded before the first signal must report exit.
/// `None` means the pre-signal scan failed (or the leader was already gone), so
/// the tree cannot be proved and the cleanup is never a sweep.
fn subreaper_swept(
    pgid: ProcessGroupId,
    pidfd: Option<&PidFd>,
    descendants: Option<&[PidFd]>,
) -> bool {
    let (Some(pidfd), Some(descendants)) = (pidfd, descendants) else {
        return false;
    };
    if !pidfd.has_exited().unwrap_or(false) {
        return false;
    }
    if group_has_members(pgid.get()) {
        return false;
    }
    descendants
        .iter()
        .all(|descendant| descendant.has_exited().unwrap_or(false))
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

/// Every direct child of this process across its threads, from `/proc`.
///
/// The daemon is single-threaded; iterating the task directory keeps the read
/// correct for a test harness that spawns from several threads. An unreadable
/// task or children file returns `None` so the caller never treats a failed
/// read as an empty set.
fn children_of(pid: u32) -> Option<BTreeSet<u32>> {
    let tasks = read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut children = BTreeSet::new();
    for task in tasks {
        let task = task.ok()?;
        let text = read_to_string(task.path().join("children")).ok()?;
        for field in text.split_whitespace() {
            if let Ok(child) = field.parse::<u32>() {
                children.insert(child);
            }
        }
    }
    Some(children)
}

/// Pin every live descendant of `root` before a stop signals the tree.
///
/// The scan follows `/proc/<pid>/task/*/children`, so a descendant that has
/// already left the recorded process group with `setsid` is still found while
/// its parent is alive. A pid that vanished between the listing and
/// `pidfd_open` is already gone; any other open failure makes the scan
/// unproved and returns `None`.
fn descendant_pidfds(root: u32) -> Option<Vec<PidFd>> {
    let mut queue = vec![root];
    let mut seen = BTreeSet::new();
    let mut descendants = Vec::new();
    while let Some(pid) = queue.pop() {
        for child in children_of(pid)? {
            if !seen.insert(child) {
                continue;
            }
            match PidFd::open(child) {
                Ok(pidfd) => descendants.push(pidfd),
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => {}
                Err(_) => return None,
            }
            queue.push(child);
        }
    }
    Some(descendants)
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        forget_child(self.identity.pid().get());
    }
}

/// Reap adopted descendants that no live handle owns.
///
/// The subreaper plan adopts a worker's orphaned descendants. One that the
/// natural-exit group signal killed can become a zombie only after the tick
/// that recorded the exit, so the service loop drains adopted children on
/// every pass: the plan's whole-tree proof is "recorded pids gone, no adopted
/// child". A leader owned by a live [`ContainedChild`] is left for its own
/// handle.
pub(crate) fn drain_adopted_children() {
    drain_adopted(0);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::containment::ExecRefusal;

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
    fn the_spawn_seam_routes_exec_errnos_to_the_typed_variant() {
        // The errno classes themselves are pinned in `containment::exec`; this
        // proves the spawn seam routes them into the typed variant with the
        // pre-child cleanup class (bitty#2790).
        for errno in [nix::libc::EACCES, nix::libc::ENOEXEC, nix::libc::ENOENT] {
            let refusal = ExecRefusal::from_spawn_error(&io::Error::from_raw_os_error(errno))
                .expect("an exec errno is classified");
            let classified = classify_spawn_error(io::Error::from_raw_os_error(errno));
            assert!(
                matches!(classified, ContainmentError::Exec(carried) if carried == refusal),
                "the errno keeps its typed variant"
            );
            assert_eq!(
                classified.cleanup(),
                CleanupClass::NotStarted,
                "an exec refusal never started a child"
            );
        }
        // A fork refusal and a `pre_exec` failure are not exec refusals, so
        // they must not borrow the typed case.
        for errno in [nix::libc::EAGAIN, nix::libc::EIO, nix::libc::EPIPE] {
            assert!(matches!(
                classify_spawn_error(io::Error::from_raw_os_error(errno)),
                ContainmentError::Spawn(_)
            ));
        }
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
