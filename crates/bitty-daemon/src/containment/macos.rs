//! macOS containment: the recorded process group only.
//!
//! macOS has no cgroup, no subreaper, and no `procctl(2)` reaper, so a process
//! group is the strongest identity the daemon can mint for one session. A
//! child can leave the group with `setsid` or `setpgid`, so this plan cannot
//! prove a whole tree. It signals the recorded group, observes it gone, and
//! reports the dedicated `D-05` cleanup class `group-signalled`; it never
//! claims `swept` and never signals a pid it did not record.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};

use nix::sys::signal::Signal;
use nix::unistd::{Pid, getpgid};

use crate::orphan::identity::ProcessGroupId;
use crate::platform::PlatformCell;
use crate::registry::CleanupClass;

use super::posix::{drain_group, group_has_members, signal_group};
use super::{
    CleanupOutcome, ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRecord,
    ContainmentRefusal, ContainmentReport, SessionMode, classify_spawn_error, wait_bounded,
};

/// Probe the macOS process-group plan.
///
/// Minting a process group needs no privilege or setup, so the plan is always
/// available on this cell. It is not a whole-tree mechanism and says so.
pub fn probe() -> ContainmentReport {
    let cell = PlatformCell::current();
    ContainmentReport {
        cell,
        kind: ContainmentKind::ProcessGroup,
        refusal: None,
        fallback_reason: None,
        cgroup_root: None,
    }
}

/// Spawn a child in a fresh process group and record that group.
pub fn spawn_with(
    report: &ContainmentReport,
    _name: &str,
    command: &mut Command,
    _limits: ContainmentLimits,
    mode: SessionMode,
) -> Result<ContainedChild, ContainmentError> {
    if report.kind() != ContainmentKind::ProcessGroup {
        return Err(ContainmentError::Refused(
            report
                .refusal()
                .cloned()
                .unwrap_or(ContainmentRefusal::UnsupportedPlatform {
                    cell: report.cell(),
                }),
        ));
    }
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
        // The child is not the leader of the fresh group this plan records;
        // kill it through the group it actually has and refuse to record a
        // group the daemon did not mint.
        let _ = signal_group(group.get(), Some(Signal::SIGKILL));
        let _ = child.kill();
        let _ = child.wait();
        return Err(ContainmentError::Uncontained(CleanupClass::Unverified));
    }
    Ok(ContainedChild { child, group })
}

fn observed_group(pid: u32) -> Option<ProcessGroupId> {
    let raw = i32::try_from(pid).ok()?;
    let pgid = getpgid(Some(Pid::from_raw(raw))).ok()?;
    let group = u32::try_from(pgid.as_raw()).ok()?;
    ProcessGroupId::new(group)
}

/// One child contained by a process group for its whole lifetime.
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
        ContainmentRecord::process_group(Some(self.group))
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

    /// Nothing to drain beyond the recorded group.
    pub fn drain_recorded(&self) {
        drain_group(self.group.get());
    }

    /// Send a signal to the recorded process group.
    ///
    /// Returns `1` when the kernel accepted the signal, `0` otherwise. A group
    /// that no longer exists is not an error and delivers nothing.
    pub fn signal(&self, signal: Signal) -> usize {
        usize::from(signal_group(self.group.get(), Some(signal)))
    }

    /// Settle the process group after the direct child was reaped.
    ///
    /// A natural exit sends no signal, so the `group-signalled` class would be
    /// a lie. When the group is observed gone this returns `unverified`; while
    /// a member remains it returns `None` and the caller keeps polling.
    #[must_use]
    pub fn sweep_if_empty(&self) -> Option<CleanupOutcome> {
        if group_has_members(self.group.get()) {
            None
        } else {
            Some(CleanupOutcome {
                class: CleanupClass::Unverified,
                signalled: 0,
            })
        }
    }

    /// Terminate the recorded process group with a bounded escalation.
    ///
    /// Sends `SIGTERM`, waits [`ContainmentLimits::term_grace`], sends
    /// `SIGKILL`, reaps the leader, and drains the group. It reports
    /// `group-signalled` only when the group is observed gone after the
    /// signals; a group that still has a member is `unverified`. The leader is
    /// kept unreaped through both signals so its pid, and therefore the
    /// recorded group id, cannot be reused before the kill lands.
    pub fn terminate(&mut self, limits: ContainmentLimits) -> CleanupOutcome {
        if !self.group_is_ours() {
            // The recorded pid now belongs to a different process (or is
            // gone); the group may have been reused. Signal nothing and let
            // the caller keep the conservative class.
            let _ = self.child.try_wait();
            return CleanupOutcome {
                class: CleanupClass::Unverified,
                signalled: 0,
            };
        }
        let mut signalled = 0;
        if signal_group(self.group.get(), Some(Signal::SIGTERM)) {
            signalled += 1;
        }
        // macOS has no non-reaping exit probe in the safe surface, so the
        // grace window is spent before the kill. The leader stays unreaped,
        // which keeps the recorded group id reserved.
        std::thread::sleep(limits.term_grace());
        if signal_group(self.group.get(), Some(Signal::SIGKILL)) {
            signalled += 1;
        }
        // The kill is already on its way to the recorded group; reap the
        // leader inside the bounded window so the group can be observed empty.
        let _ = wait_bounded(limits.kill_wait(), limits, || {
            let _ = self.child.try_wait();
            drain_group(self.group.get());
            !group_has_members(self.group.get())
        });
        let reaped = matches!(self.child.try_wait(), Ok(Some(_)));
        let gone = !group_has_members(self.group.get());
        CleanupOutcome {
            class: if gone && reaped && signalled > 0 {
                CleanupClass::GroupSignalled
            } else {
                CleanupClass::Unverified
            },
            signalled,
        }
    }

    /// Whether the recorded leader still belongs to the recorded group.
    ///
    /// A reused pid usually lands in another group; the check refuses to
    /// signal then. A leader that already exited answers `false` (ESRCH), but
    /// its group may still hold recorded members, so callers treat that as a
    /// reason to stay conservative rather than as proof of a sweep.
    fn group_is_ours(&self) -> bool {
        let Ok(raw) = i32::try_from(self.child.id()) else {
            return false;
        };
        match getpgid(Some(Pid::from_raw(raw))) {
            Ok(pgid) => u32::try_from(pgid.as_raw()).ok() == Some(self.group.get()),
            // The leader already exited; while the recorded group still has a
            // member its id cannot be reused, so signalling it stays exact.
            Err(nix::errno::Errno::ESRCH) => true,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use super::*;
    use crate::containment::CLEANUP_GROUP_SIGNALLED;

    fn report() -> ContainmentReport {
        ContainmentReport::process_group(PlatformCell::current())
    }

    fn limits() -> ContainmentLimits {
        ContainmentLimits::new(Duration::from_millis(100), Duration::from_secs(3))
    }

    #[test]
    fn the_live_probe_selects_the_process_group_plan() {
        let report = probe();
        assert_eq!(report.kind(), ContainmentKind::ProcessGroup);
        assert_eq!(report.cleanup_claim(), CLEANUP_GROUP_SIGNALLED);
        assert!(!report.is_enforced());
    }

    #[test]
    fn a_foreign_plan_is_refused_before_a_child_exists() {
        let foreign = ContainmentReport::windows_job(PlatformCell::MacosArm64);
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0");
        let error = spawn_with(
            &foreign,
            "bittyd-test-foreign",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect_err("a job plan is not the macOS plan");
        assert!(matches!(error, ContainmentError::Refused(_)));
    }

    #[test]
    fn a_contained_child_is_its_own_group_and_a_stop_reports_group_signalled() {
        let report = report();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-group",
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
        assert_eq!(outcome.class(), CleanupClass::GroupSignalled, "{report}");
        assert!(outcome.signalled() >= 1, "the group was signalled");
        assert!(
            !group_has_members(child.process_group().get()),
            "the recorded group must be observed gone"
        );
    }

    #[test]
    fn a_natural_exit_never_claims_group_signalled() {
        let report = report();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-group-exit",
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
            "no signal was sent, so `group-signalled` would be a lie"
        );
    }
}
