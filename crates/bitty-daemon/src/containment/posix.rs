//! Shared POSIX process-group primitives.
//!
//! The process group is the weakest containment identity: it is minted for one
//! session by `setpgid`/`setsid`, but a descendant can leave it with `setsid`
//! or `setpgid`. Every helper here signals only a recorded group and reports
//! what it observed; none of them claims a whole-tree sweep.

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::collections::BTreeSet;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::sync::{LazyLock, Mutex};

use nix::sys::signal::Signal;
use nix::unistd::Pid;

/// Pids owned by a live [`super::ContainedChild`] handle. A reaper drains
/// adopted grandchildren without stealing another session's direct child.
///
/// Only the plans with a reaper (Linux, FreeBSD) need this registry; the
/// macOS process-group plan has no adopted children to drain.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
static LIVE_CHILDREN: LazyLock<Mutex<BTreeSet<u32>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn remember_child(pid: u32) {
    if let Ok(mut live) = LIVE_CHILDREN.lock() {
        live.insert(pid);
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn forget_child(pid: u32) {
    if let Ok(mut live) = LIVE_CHILDREN.lock() {
        live.remove(&pid);
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn live_children() -> BTreeSet<u32> {
    LIVE_CHILDREN
        .lock()
        .map_or_else(|_| BTreeSet::new(), |live| live.clone())
}

/// Send `signal` to a recorded process group, or probe it with signal `0`.
///
/// Returns whether the kernel accepted the call. `None` is a pure existence
/// probe: it never delivers a signal.
pub(crate) fn signal_group(pgid: u32, signal: Option<Signal>) -> bool {
    let Ok(raw) = i32::try_from(pgid) else {
        return false;
    };
    nix::sys::signal::killpg(Pid::from_raw(raw), signal).is_ok()
}

/// Whether the recorded process group still has any member, including a
/// zombie. Signal 0 only checks existence; it never delivers anything.
pub(crate) fn group_has_members(pgid: u32) -> bool {
    signal_group(pgid, None)
}

/// What a `getpgid` probe reported about the recorded leader.
///
/// Only the FreeBSD and macOS plans consult this; Linux compiles it for the
/// unit tests that drive the state machine, because that plan cannot run here.
#[cfg(any(target_os = "freebsd", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaderProbe {
    /// The leader is in this process group.
    Named(u32),
    /// `ESRCH`: no process with that pid exists at all.
    Gone,
    /// Any other failure; nothing is proved either way.
    Unreadable,
}

/// Classify a `getpgid` probe of the recorded leader.
///
/// The answer is the raw observation, not a verdict: [`GroupIdentity`] decides
/// what it may authorise.
#[cfg(any(target_os = "freebsd", target_os = "macos", test))]
pub(crate) fn probe_leader(pid: u32) -> LeaderProbe {
    let Ok(raw) = i32::try_from(pid) else {
        return LeaderProbe::Unreadable;
    };
    match nix::unistd::getpgid(Some(Pid::from_raw(raw))) {
        Ok(pgid) => {
            u32::try_from(pgid.as_raw()).map_or(LeaderProbe::Unreadable, LeaderProbe::Named)
        }
        Err(nix::errno::Errno::ESRCH) => LeaderProbe::Gone,
        Err(_) => LeaderProbe::Unreadable,
    }
}

/// The recorded process-group identity, as a stop may use it.
///
/// The group id is minted from the leader's pid, so it only names this
/// session's group while the kernel keeps that pid reserved — which a live
/// leader and an unreaped zombie both do. A confirmed reap frees the pid, and
/// with it the group id, which may then name a group this daemon never minted.
/// Every signal, probe, and drain of the recorded group goes through this type
/// so that a freed id is never used as if it were still ours.
#[cfg(any(target_os = "freebsd", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupIdentity {
    reaped: bool,
}

#[cfg(any(target_os = "freebsd", target_os = "macos", test))]
impl GroupIdentity {
    /// A freshly spawned child: the leader is unreaped.
    pub(crate) const fn fresh() -> Self {
        Self { reaped: false }
    }

    /// Whether this handle confirmed that it reaped the leader.
    ///
    /// This is also the only state in which the recorded group may be drained:
    /// [`drain_group`] waits on the whole recorded group, the leader included,
    /// so draining earlier would reap the leader behind this handle's back,
    /// free the group id, and leave this flag `false` while the id is gone.
    pub(crate) const fn reaped(&self) -> bool {
        self.reaped
    }

    /// Record a reap this handle performed.
    pub(crate) fn note_reap(&mut self) {
        self.reaped = true;
    }

    /// Record the stop's reap and drain the recorded group, in that order.
    ///
    /// The drain runs only when `reaped` is true: [`drain_group`] waits on the
    /// whole recorded group, the leader included, so an unconfirmed drain
    /// reaps the leader behind this handle's back, frees the group id, and
    /// leaves the identity reporting a group it can no longer prove is ours.
    ///
    /// Returns whether the drain ran. The Linux build compiles this only for
    /// the unit test that drives the FreeBSD path, which cannot run here.
    #[cfg(any(target_os = "freebsd", all(test, target_os = "linux")))]
    pub(crate) fn reap_and_drain(&mut self, reaped: bool, pgid: u32) -> bool {
        if !reaped {
            return false;
        }
        self.note_reap();
        drain_group(pgid);
        true
    }

    /// Whether the recorded group may be signalled, given a leader probe.
    ///
    /// Fails closed: only a probe that names the recorded group authorises a
    /// signal, and only while the leader is unreaped.
    pub(crate) fn may_signal(&self, recorded: u32, probe: LeaderProbe) -> bool {
        if self.reaped {
            return false;
        }
        match probe {
            LeaderProbe::Named(pgid) => pgid == recorded,
            // `ESRCH` means the pid is gone entirely, so it was reaped
            // somewhere; the group id is free and may already be reused.
            // It is not proof that the recorded group is still ours.
            LeaderProbe::Gone | LeaderProbe::Unreadable => false,
        }
    }

    /// Whether a stop may call the recorded group settled.
    ///
    /// An empty group probe alone is not a proof: an unconfirmed drain can
    /// make it read empty by reaping the leader. Only a confirmed reap plus an
    /// empty group backs the claim.
    pub(crate) const fn settled(&self, group_gone: bool) -> bool {
        self.reaped && group_gone
    }
}

/// Reap every child in one recorded process group without blocking.
///
/// The group is the recorded identity, minted for this session; only its
/// members are waited on. `WNOHANG` returns `StillAlive` when a member is
/// still running, so the caller's bounded loop decides how long to wait.
///
/// This waits on the whole group, the leader included, so it may reap the
/// leader. Call it only after [`GroupIdentity::reaped`] confirms the reap;
/// otherwise the group id is freed without the handle noticing.
pub(crate) fn drain_group(pgid: u32) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    let Ok(raw) = i32::try_from(pgid) else {
        return;
    };
    while let Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) =
        waitpid(Pid::from_raw(-raw), Some(WaitPidFlag::WNOHANG))
    {
        // A reaped member: keep draining. Every other status (still alive,
        // stopped, continued, traced, or gone) ends the drain and the caller's
        // bounded loop decides what to do next.
    }
}

/// Reap adopted children without stealing another session's child.
///
/// `waitid(WNOWAIT)` peeks at the next exited child; a pid owned by a live
/// [`super::ContainedChild`] or the child this call is terminating is left for
/// its own handle. Only an adopted grandchild (no handle) is reaped. Nothing
/// is signalled and no name is resolved.
///
/// Linux and FreeBSD expose `waitid(P_ALL)`; macOS does not, so the macOS
/// process-group plan has no reaper and does not drain adopted children.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn drain_adopted(except: u32) {
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
            _ => break,
        };
        let raw = u32::try_from(pid.as_raw()).unwrap_or(0);
        if raw == except || live.contains(&raw) {
            break;
        }
        let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
    }
}

/// Serialize the tests that spawn a child in a recorded process group.
///
/// The FreeBSD sweep verdict enumerates every descendant the process-wide
/// reaper can see, so a child spawned by a parallel test would appear in this
/// session's list and correctly keep the stop `unverified`. The Linux adopted
/// drain has the same scope. Every POSIX leg takes this one lock in its
/// process-group tests, so no test observes another test's child.
#[cfg(all(
    test,
    any(target_os = "linux", target_os = "freebsd", target_os = "macos")
))]
pub(crate) fn serial_spawn() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;

    static SPAWN_LOCK: Mutex<()> = Mutex::new(());
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::containment::{ContainmentLimits, wait_bounded};

    #[test]
    fn a_signalled_group_is_observed_gone() {
        use std::process::{Command, Stdio};

        let _serial = serial_spawn();
        // A short-lived child in its own group: minted by the caller, not
        // resolved by name.
        let mut command = Command::new("sleep");
        command.arg("0.05").stdout(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().expect("spawn a grouped child");
        let pgid = child.id();
        assert!(group_has_members(pgid));
        let _ = child.wait();
        let gone = wait_bounded(Duration::from_secs(2), ContainmentLimits::default(), || {
            !group_has_members(pgid)
        });
        assert!(gone, "the reaped group must report no members");
    }

    /// Whether the leader is an unreaped zombie, without reaping it.
    ///
    /// macOS has no `waitid` in the safe surface, so the zombie peek and the
    /// test that needs it are Linux and FreeBSD only; the macOS build still
    /// compiles the pure state-machine test below.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn leader_is_zombie(pid: u32) -> bool {
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};

        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        matches!(
            waitid(
                Id::Pid(Pid::from_raw(raw)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            ),
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..))
        )
    }

    /// A leader that vanishes between `try_wait` and the drain frees its pid,
    /// and the group id minted from it, behind this handle's back.
    ///
    /// The stop must not drain before it confirmed the reap, and afterwards
    /// the freed id must neither authorise `killpg` nor count as settled.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn a_leader_that_vanishes_before_the_drain_never_authorises_a_group_signal() {
        use std::process::{Command, Stdio};

        let _serial = serial_spawn();
        let mut command = Command::new("sleep");
        command.arg("0.05").stdout(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn().expect("spawn a grouped child");
        let pgid = child.id();
        let exited = wait_bounded(Duration::from_secs(2), ContainmentLimits::default(), || {
            leader_is_zombie(pgid)
        });
        assert!(exited, "the leader must be an unreaped zombie");
        // This handle confirmed nothing: `try_wait` still reports it running.
        let mut identity = GroupIdentity::fresh();
        let drained = identity.reap_and_drain(false, pgid);
        assert!(
            !drained,
            "a stop must not drain the recorded group before the reap is confirmed"
        );
        assert!(!identity.reaped(), "the handle never confirmed the reap");
        // The leader is still an unreaped zombie, so its pid — and the group
        // id minted from it — is still reserved. An unconfirmed drain would
        // have reaped it and freed that id.
        assert_eq!(
            probe_leader(pgid),
            LeaderProbe::Named(pgid),
            "the unconfirmed stop must not reap the leader through the group"
        );

        // Reproduce the silent reap the unguarded drain performed: a
        // group-wide wait takes the leader too.
        drain_group(pgid);
        let probe = probe_leader(pgid);
        assert_eq!(
            probe,
            LeaderProbe::Gone,
            "the group-wide wait reaped the leader, freeing the group id"
        );
        assert!(
            !identity.may_signal(pgid, probe),
            "a freed group id must never authorise killpg"
        );
        assert!(
            !identity.settled(!group_has_members(pgid)),
            "an unconfirmed reap must never report the group settled"
        );
        drop(child);
    }

    /// `ESRCH` means the pid is gone entirely, so it was reaped somewhere and
    /// the recorded group id may already name another group.
    ///
    /// It is not a "group is gone" shortcut: only a probe that names the
    /// recorded group authorises a signal, and never after our own reap.
    #[test]
    fn an_esrch_probe_never_authorises_a_group_signal() {
        let live = GroupIdentity::fresh();
        assert!(
            live.may_signal(7, LeaderProbe::Named(7)),
            "the recorded group is signalled while the leader provably has it"
        );
        assert!(
            !live.may_signal(7, LeaderProbe::Named(8)),
            "another group is never signalled"
        );
        assert!(
            !live.may_signal(7, LeaderProbe::Gone),
            "`ESRCH` fails closed: it never authorises killpg"
        );
        assert!(
            !live.may_signal(7, LeaderProbe::Unreadable),
            "an unreadable probe proves nothing"
        );

        let mut reaped = GroupIdentity::fresh();
        reaped.note_reap();
        assert!(reaped.reaped(), "the confirmed reap is recorded");
        assert!(
            !reaped.may_signal(7, LeaderProbe::Named(7)),
            "a confirmed reap frees the id, so even a matching probe is refused"
        );
        assert!(
            reaped.settled(true),
            "a confirmed reap plus an empty group settles"
        );
        assert!(!reaped.settled(false), "a member keeps the group unsettled");
    }
}
