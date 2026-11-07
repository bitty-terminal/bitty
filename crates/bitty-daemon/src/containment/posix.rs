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

/// Reap every child in one recorded process group without blocking.
///
/// The group is the recorded identity, minted for this session; only its
/// members are waited on. `WNOHANG` returns `StillAlive` when a member is
/// still running, so the caller's bounded loop decides how long to wait.
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
}
