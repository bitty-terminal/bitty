//! FreeBSD containment: the `procctl(2)` reaper plus the recorded group.
//!
//! The daemon acquires `PROC_REAP_ACQUIRE`, so a descendant that outlives its
//! parent — including one that left the process group with `setsid` — is
//! reparented to the daemon instead of `init`. A session records the child's
//! process group and, before the first signal, the pids the reaper reports
//! under that session's subtree. A stop signals the recorded group and claims
//! `swept` only when:
//!
//! - the recorded group has no member left,
//! - the leader was observed exited without being reaped (`waitid(WNOWAIT)`),
//! - every recorded descendant is absent from the reaper's pid list or marked
//!   exiting/dead,
//! - and `PROC_REAP_STATUS` still reports this daemon as the owner.
//!
//! A setsid descendant created after the pre-signal scan, an unreadable pid
//! list, or a group that still has a member stays `unverified`. No pid outside
//! the recorded set is signalled.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};

use nix::sys::signal::Signal;
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::{Pid, getpgid};

use crate::orphan::identity::ProcessGroupId;
use crate::platform::PlatformCell;
use crate::registry::CleanupClass;

use super::freebsd_sys;
use super::posix::{
    drain_adopted, drain_group, forget_child, group_has_members, remember_child, signal_group,
};
use super::{
    CleanupOutcome, ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRecord,
    ContainmentRefusal, ContainmentReport, SessionMode, classify_spawn_error, wait_bounded,
};

/// Upper bound on one reaper pid-list enumeration.
///
/// The daemon bounds its session count; this is the largest descendant list
/// the containment plan will allocate rather than truncate a proof.
const MAX_REAPER_DESCENDANTS: usize = 4096;

/// Acquire the `procctl(2)` reaper role, idempotently.
pub(crate) struct ReaperGuard(());

impl ReaperGuard {
    /// Acquire and confirm the reaper role for this process.
    ///
    /// Idempotent: a probe or a second session on an already-acquired daemon
    /// reads the existing role back instead of re-acquiring it.
    pub(crate) fn acquire() -> io::Result<Self> {
        if freebsd_sys::is_reaper() {
            return Ok(Self(()));
        }
        freebsd_sys::acquire()?;
        let status = freebsd_sys::status()?;
        if status.owned_by_current() {
            Ok(Self(()))
        } else {
            Err(io::Error::other(
                "procctl(PROC_REAP_ACQUIRE) did not report the reaper role",
            ))
        }
    }
}

/// Probe the FreeBSD reaper plan by acquiring the reaper role.
pub fn probe() -> ContainmentReport {
    let cell = PlatformCell::current();
    match ReaperGuard::acquire() {
        Ok(_) => ContainmentReport {
            cell,
            kind: ContainmentKind::FreebsdReaper,
            refusal: None,
            fallback_reason: None,
            cgroup_root: None,
        },
        Err(_) => ContainmentReport {
            cell,
            kind: ContainmentKind::Refused,
            refusal: Some(ContainmentRefusal::UnsupportedPlatform { cell }),
            fallback_reason: None,
            cgroup_root: None,
        },
    }
}

/// Spawn a child in a fresh process group under the reaper.
pub fn spawn_with(
    report: &ContainmentReport,
    _name: &str,
    command: &mut Command,
    _limits: ContainmentLimits,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    if report.kind() != ContainmentKind::FreebsdReaper {
        return Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        ));
    }
    ReaperGuard::acquire().map_err(ContainmentError::Spawn)?;
    if mode == SessionMode::ProcessGroup {
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(classify_spawn_error)?;
    let pid = child.id();
    let Some(group) = observed_group(pid) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ContainmentError::IdentityUnavailable(
            CleanupClass::Unverified,
        ));
    };
    if group.get() != pid {
        let _ = signal_group(group.get(), Some(Signal::SIGKILL));
        let _ = child.kill();
        let _ = child.wait();
        return Err(ContainmentError::Uncontained(CleanupClass::Unverified));
    }
    remember_child(pid);
    Ok(ContainedChild { child, group })
}

fn observed_group(pid: u32) -> Option<ProcessGroupId> {
    let raw = i32::try_from(pid).ok()?;
    let pgid = getpgid(Some(Pid::from_raw(raw))).ok()?;
    ProcessGroupId::new(u32::try_from(pgid.as_raw()).ok()?)
}

/// One child contained by the reaper plus its recorded process group.
#[derive(Debug)]
pub struct ContainedChild {
    child: Child,
    group: ProcessGroupId,
}

impl ContainedChild {
    /// The direct child's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The recorded process group.
    #[must_use]
    pub const fn process_group(&self) -> ProcessGroupId {
        self.group
    }

    /// The recorded containment identity, suitable for the orphan journal.
    #[must_use]
    pub fn containment(&self) -> ContainmentRecord {
        ContainmentRecord::freebsd_reaper(Some(self.group))
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
    /// exit without racing a reused id.
    #[must_use]
    pub fn leader_exited(&self) -> bool {
        let Ok(pid) = i32::try_from(self.child.id()) else {
            return false;
        };
        matches!(
            waitid(
                Id::Pid(Pid::from_raw(pid)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ),
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        )
    }

    /// Send a signal to the recorded process group.
    ///
    /// Returns `1` when the kernel accepted the signal, `0` otherwise.
    pub fn signal(&self, signal: Signal) -> usize {
        usize::from(signal_group(self.group.get(), Some(signal)))
    }

    /// Drain the recorded group and adopted members after the leader was
    /// reaped.
    pub fn drain_recorded(&self) {
        drain_group(self.group.get());
        drain_adopted(self.child.id());
    }

    /// Settle the reaper after the direct child was reaped.
    ///
    /// A natural exit detaches the descendants to the reaper before the poll
    /// tick observes it, and no pre-signal descendant snapshot exists, so this
    /// path cannot prove the tree empty. It returns `unverified` once the
    /// group is gone and `None` while a member remains. A stop records the
    /// pre-signal descendants and can prove a sweep.
    #[must_use]
    pub fn sweep_if_empty(&self) -> Option<CleanupOutcome> {
        if group_has_members(self.group.get()) {
            None
        } else {
            drain_adopted(self.child.id());
            Some(CleanupOutcome {
                class: CleanupClass::Unverified,
                signalled: 0,
            })
        }
    }

    /// Terminate the recorded group with a bounded escalation.
    ///
    /// Records the session subtree from the reaper before the first signal,
    /// sends `SIGTERM`, waits the grace window, sends `SIGKILL`, and verifies
    /// the group empty plus every recorded descendant gone. `swept` requires
    /// all of that and a reaper status that still names this daemon.
    pub fn terminate(&mut self, limits: ContainmentLimits) -> CleanupOutcome {
        let leader = self.child.id();
        if !self.group_is_ours() {
            let _ = self.child.try_wait();
            return CleanupOutcome {
                class: CleanupClass::Unverified,
                signalled: 0,
            };
        }
        // Record the whole session subtree before the first signal. A
        // descendant that already left the group is still under the leader
        // here, so its pid is captured; if the list cannot be read the stop
        // cannot prove a sweep.
        let descendants = recorded_descendants(leader);
        let mut signalled = 0;
        if signal_group(self.group.get(), Some(Signal::SIGTERM)) {
            signalled += 1;
        }
        let _ = wait_bounded(limits.term_grace(), limits, || self.leader_exited());
        if signal_group(self.group.get(), Some(Signal::SIGKILL)) {
            signalled += 1;
        }
        let proven = wait_bounded(limits.kill_wait(), limits, || {
            drain_adopted(leader);
            self.leader_exited() && descendants.as_deref().is_some_and(descendants_gone)
        });
        let leader_exited = self.leader_exited();
        // Reap the leader only after the kill landed and the exit was peeked;
        // until then the zombie keeps the recorded group id reserved.
        let reaped = matches!(self.child.try_wait(), Ok(Some(_)));
        drain_group(self.group.get());
        let group_gone = !group_has_members(self.group.get());
        let reaper_owned = freebsd_sys::status().is_ok_and(|status| status.owned_by_current());
        let class = if proven
            && leader_exited
            && reaped
            && group_gone
            && reaper_owned
            && descendants.as_deref().is_some_and(descendants_gone)
        {
            CleanupClass::Swept
        } else {
            CleanupClass::Unverified
        };
        CleanupOutcome { class, signalled }
    }

    /// Whether the recorded leader still belongs to the recorded group.
    ///
    /// A leader that already exited answers `true` while the group still has
    /// a member: the group id cannot be reused until the last member is gone,
    /// so signalling it stays exact.
    fn group_is_ours(&self) -> bool {
        let Ok(raw) = i32::try_from(self.child.id()) else {
            return false;
        };
        match getpgid(Some(Pid::from_raw(raw))) {
            Ok(pgid) => u32::try_from(pgid.as_raw()).ok() == Some(self.group.get()),
            Err(nix::errno::Errno::ESRCH) => true,
            Err(_) => false,
        }
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        forget_child(self.child.id());
    }
}

/// The pids the reaper lists under `leader`'s subtree, before a signal.
///
/// `None` means the list could not be read, so the stop cannot prove a sweep.
fn recorded_descendants(leader: u32) -> Option<Vec<u32>> {
    let entries = freebsd_sys::descendants(MAX_REAPER_DESCENDANTS).ok()?;
    let Ok(leader) = i32::try_from(leader) else {
        return None;
    };
    let mut pids = Vec::new();
    for entry in entries {
        if entry.valid() && entry.subtree == leader {
            if let Ok(pid) = u32::try_from(entry.pid) {
                pids.push(pid);
            }
        }
    }
    Some(pids)
}

/// Whether every recorded descendant is absent from the reaper list or marked
/// exiting/dead.
///
/// A failed enumeration is `false`: an unreadable list never proves a sweep.
fn descendants_gone(pids: &[u32]) -> bool {
    if pids.is_empty() {
        return true;
    }
    let Ok(entries) = freebsd_sys::descendants(MAX_REAPER_DESCENDANTS) else {
        return false;
    };
    pids.iter().all(|recorded| {
        let Ok(recorded) = i32::try_from(*recorded) else {
            return false;
        };
        entries
            .iter()
            .filter(|entry| entry.pid == recorded)
            .all(|entry| entry.not_running())
    })
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use super::*;

    /// Serialize the process-spawning tests: the reaper role is process-wide,
    /// so a descendant spawned by a parallel test would appear in this
    /// session's enumeration and correctly make the stop unverified.
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SPAWN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn report() -> ContainmentReport {
        ContainmentReport::freebsd_reaper(PlatformCell::current())
    }

    fn limits() -> ContainmentLimits {
        ContainmentLimits::new(Duration::from_millis(100), Duration::from_secs(3))
    }

    #[test]
    fn the_live_probe_acquires_the_reaper_role() {
        let report = probe();
        assert_eq!(report.cell(), PlatformCell::current());
        assert_eq!(report.kind(), ContainmentKind::FreebsdReaper);
        assert_eq!(report.cleanup_claim(), "swept");
        assert!(report.is_enforced());
        assert!(freebsd_sys::is_reaper(), "the probe must confirm the role");
    }

    #[test]
    fn a_foreign_plan_is_refused_before_a_child_exists() {
        let foreign = ContainmentReport::process_group(PlatformCell::FreeBsdX86_64);
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0");
        let error = spawn_with(
            &foreign,
            "bittyd-test-foreign",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect_err("a process-group plan is not the FreeBSD plan");
        assert!(matches!(error, ContainmentError::Refused(_)));
    }

    #[test]
    fn a_contained_child_is_its_own_group_and_a_stop_verifies_a_sweep() {
        let _serial = serial();
        let report = report();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-reaper",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        assert_eq!(child.process_group().get(), child.pid());
        assert_eq!(
            child.containment().recorded_process_group(),
            Some(child.process_group())
        );
        let outcome = child.terminate(limits());
        assert_eq!(outcome.class(), CleanupClass::Swept, "{report}");
        assert!(outcome.signalled() >= 1, "the group was signalled");
        assert!(
            !group_has_members(child.process_group().get()),
            "the recorded group must be observed gone"
        );
    }

    #[test]
    fn a_natural_exit_stays_unverified() {
        let _serial = serial();
        let report = report();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-reaper-exit",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        let _ = child.wait().expect("reap the natural exit");
        let outcome = wait_bounded(limits().kill_wait(), limits(), || {
            child.sweep_if_empty().is_some()
        });
        assert!(outcome, "the empty group settles");
        assert_eq!(
            child.sweep_if_empty().map(CleanupOutcome::class),
            Some(CleanupClass::Unverified),
            "a natural exit has no pre-signal descendant snapshot"
        );
    }
}
