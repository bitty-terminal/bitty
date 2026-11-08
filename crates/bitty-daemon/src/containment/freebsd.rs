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
//!   and its reap was confirmed before the recorded group was drained, because
//!   a group-wide wait takes the leader too and would free the group id
//!   unnoticed,
//! - every recorded descendant is absent from the reaper's pid list or marked
//!   exiting/dead,
//! - the reaper lists no live descendant this session cannot attribute to
//!   another session — which is what covers a descendant created after the
//!   pre-signal scan, including one that left the group with `setsid`,
//! - and `PROC_REAP_STATUS` still reports this daemon as the owner.
//!
//! An unreadable pid list, a group that still has a member, or a live
//! descendant the daemon cannot attribute to another session stays
//! `unverified`. No pid outside the recorded set is signalled.
//!
//! This plan is selected by [`probe`] and is on the served spawn path: a
//! `bittyd` on this cell opens the PTY, spawns through [`spawn_with`], and
//! reports the `swept` class a verified stop can prove. Its session records
//! carry no durable journal identity, so the orphan verifier stays unserved
//! here.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};

use nix::sys::signal::Signal;
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::{Pid, getpgid};

use crate::orphan::identity::{ProcessGroupId, ProcessIdentity};
use crate::platform::PlatformCell;
use crate::registry::CleanupClass;

use super::freebsd_sys;
use super::posix::{
    GroupIdentity, drain_adopted, drain_group, forget_child, group_has_members, live_children,
    probe_leader, remember_child, signal_group,
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
    Ok(ContainedChild {
        child,
        group,
        identity: GroupIdentity::fresh(),
    })
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
    /// Whether this handle confirmed that it reaped the leader.
    ///
    /// A reap frees the leader's pid, and with it the group id minted from
    /// that pid, so nothing about the recorded group is provable afterwards.
    /// Every signal, probe, and drain below refuses once this is set.
    identity: GroupIdentity,
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
        let status = self.child.wait()?;
        self.identity.note_reap();
        Ok(status)
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
        self.try_wait().map(|status| status.is_some())
    }

    /// Non-blocking wait: the exit status once the direct child has exited.
    ///
    /// # Errors
    ///
    /// Propagates a wait failure.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.identity.note_reap();
        }
        Ok(status)
    }

    /// The journal identity this plan can re-verify after a restart.
    ///
    /// The FreeBSD reaper record has no observable start time in this slice,
    /// so there is nothing the orphan verifier could re-check. The plan
    /// returns `None`, and the journal is not served on this cell.
    #[must_use]
    pub const fn durable_identity(&self) -> Option<ProcessIdentity> {
        None
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
    /// Returns `1` when the kernel accepted the signal, `0` otherwise. A
    /// confirmed reap frees the recorded group id, so nothing is signalled
    /// after one.
    pub fn signal(&self, signal: Signal) -> usize {
        if !self
            .identity
            .may_signal(self.group.get(), probe_leader(self.child.id()))
        {
            return 0;
        }
        usize::from(signal_group(self.group.get(), Some(signal)))
    }

    /// Drain the recorded group and adopted members after the leader was
    /// reaped.
    ///
    /// A no-op until this handle confirmed the reap: [`drain_group`] waits on
    /// the whole recorded group, the leader included, so draining first would
    /// reap the leader behind this handle's back and free the group id while
    /// the handle still reports it unreaped.
    pub fn drain_recorded(&self) {
        if !self.identity.reaped() {
            return;
        }
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
    /// the group empty, every recorded descendant gone, and no live
    /// descendant this session cannot attribute to another session. `swept`
    /// requires all of that and a reaper status that still names this daemon.
    ///
    /// Nothing is signalled once this handle has reaped the leader: the reap
    /// frees the pid, and the recorded group id with it, so `killpg` could
    /// then reach a group the daemon never minted. The recorded group is
    /// drained only after the reap is confirmed, for the same reason: the
    /// group-wide wait also reaps the leader. A stop that finds the leader
    /// already reaped reports `unverified` and signals nothing.
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
        // here, so its pid is captured; a descendant created later is not,
        // and is covered by the post-kill `subtree_settled` check instead.
        // If the list cannot be read the stop cannot prove a sweep.
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
            self.leader_exited()
                && descendants.as_deref().is_some_and(descendants_gone)
                && subtree_settled(leader)
        });
        let leader_exited = self.leader_exited();
        // Reap the leader only after the kill landed and the exit was peeked;
        // until then the zombie keeps the recorded group id reserved. The
        // recorded group is drained only after that reap is confirmed, because
        // the group-wide wait also reaps the leader.
        let reaped = matches!(self.child.try_wait(), Ok(Some(_)));
        self.identity.reap_and_drain(reaped, self.group.get());
        let group_gone = !group_has_members(self.group.get());
        let reaper_owned = freebsd_sys::status().is_ok_and(|status| status.owned_by_current());
        let class = if proven
            && leader_exited
            && self.identity.settled(group_gone)
            && reaper_owned
            && descendants.as_deref().is_some_and(descendants_gone)
            && subtree_settled(leader)
        {
            CleanupClass::Swept
        } else {
            CleanupClass::Unverified
        };
        CleanupOutcome { class, signalled }
    }

    /// Whether the recorded leader may still be signalled exactly.
    ///
    /// The group id is minted from the leader's pid, so it stays exact only
    /// while the kernel reserves that pid: a live leader and an unreaped
    /// zombie both do. A leader this handle already reaped fails closed, and
    /// so does `ESRCH` — a pid that is gone entirely was reaped somewhere, so
    /// the recorded group id is free and may already name another group.
    fn group_is_ours(&self) -> bool {
        self.identity
            .may_signal(self.group.get(), probe_leader(self.child.id()))
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

/// Whether the reaper lists no live descendant this session cannot rule out.
///
/// This is the check the recorded-pid snapshot cannot make: a descendant
/// created after the snapshot can leave the recorded group with
/// `setsid`/`setpgid`, so neither the group probe nor the recorded list names
/// it, yet the daemon's reaper still lists it. Re-enumerating after the kill
/// closes that window.
///
/// An entry is attributed to another session only when its `subtree` lane —
/// the direct child of this daemon the entry descends from, or the entry's own
/// pid once it was reparented to the daemon — names a live
/// [`ContainedChild`] of another session. Everything else, including anything
/// still under `leader`, is treated as this session's, so a live stranger
/// keeps the stop `unverified` rather than proving a sweep it cannot back. A
/// failed enumeration is `false`.
fn subtree_settled(leader: u32) -> bool {
    let Ok(entries) = freebsd_sys::descendants(MAX_REAPER_DESCENDANTS) else {
        return false;
    };
    let mut others = live_children();
    // The leader is still registered while this stop runs; its own subtree is
    // exactly what this session has to prove gone.
    others.remove(&leader);
    entries.iter().all(|entry| {
        entry.not_running() || {
            let subtree = u32::try_from(entry.subtree).unwrap_or(0);
            subtree != leader && others.contains(&subtree)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::sync::MutexGuard;
    use std::time::Duration;

    use super::*;
    use crate::containment::posix::serial_spawn;

    /// Serialize the process-spawning tests: the reaper role is process-wide,
    /// so a descendant spawned by a parallel test would appear in this
    /// session's enumeration and correctly make the stop unverified.
    fn serial() -> MutexGuard<'static, ()> {
        serial_spawn()
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

    /// The leader reaps first, so the recorded group id it minted is free.
    ///
    /// Signalling after that reap can reach a group the daemon never created.
    /// The plan must refuse: nothing signalled, `unverified`.
    #[test]
    fn a_stop_after_this_handle_reaped_the_leader_signals_nothing() {
        let _serial = serial();
        let report = report();
        let mut command = Command::new("/bin/sh");
        // The leader exits and leaves a live member in the recorded group, so
        // the group still answers `killpg` after the leader was reaped.
        command
            .args([
                "-c",
                "(trap '' TERM; while :; do sleep 0.05; done) & exit 0",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-reaped-leader",
            &mut command,
            limits(),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        let group = child.process_group();
        let _ = child.wait().expect("reap the leader");
        let outcome = child.terminate(limits());
        // End the descendant through the recorded group before asserting, so a
        // failing assertion cannot leave it running.
        signal_group(group.get(), Some(Signal::SIGKILL));
        assert_eq!(
            outcome.signalled(),
            0,
            "the recorded group must not be signalled once its leader was reaped"
        );
        assert_eq!(outcome.class(), CleanupClass::Unverified, "{report}");
    }

    /// A descendant created after the pre-signal snapshot leaves the group.
    ///
    /// Neither the group probe nor the recorded pid list names it, so only the
    /// reaper's post-kill enumeration can prove the tree empty. The stop must
    /// stay `unverified` instead of reporting a sweep over a live process.
    #[test]
    fn a_descendant_that_setsids_after_the_snapshot_keeps_the_stop_unverified() {
        use nix::sys::signal::kill;

        let _serial = serial();
        let report = report();
        let escapee =
            std::env::temp_dir().join(format!("bittyd-freebsd-escapee-{}", std::process::id()));
        // The helper starts inside the recorded group and, after the snapshot
        // is taken, moves its own child into a new session with `setsid`.
        let script = format!(
            "trap '' TERM; (sleep 0.2; setsid sh -c 'echo $$ > {}; exec sleep 30') & while :; do sleep 0.05; done",
            escapee.display()
        );
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_with(
            &report,
            "bittyd-test-escapee",
            &mut command,
            // A grace window wider than the helper's delay, so the escape
            // happens before the kill lands.
            ContainmentLimits::new(Duration::from_secs(1), Duration::from_millis(300)),
            SessionMode::ProcessGroup,
        )
        .expect("a contained spawn");
        let outcome = child.terminate(ContainmentLimits::new(
            Duration::from_secs(1),
            Duration::from_millis(300),
        ));
        let escaped = std::fs::read_to_string(&escapee)
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        let _ = std::fs::remove_file(&escapee);
        if let Some(pid) = escaped {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        assert_eq!(
            outcome.class(),
            CleanupClass::Unverified,
            "a descendant that left the group after the snapshot cannot be proven gone"
        );
    }
}
