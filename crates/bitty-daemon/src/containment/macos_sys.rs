//! Raw macOS process-status binding for the process-group plan.
//!
//! macOS has no `waitid(2)` in the safe surface (`nix` 0.31 wraps it only on
//! Linux, FreeBSD, and Haiku), so the process-group plan cannot peek at a
//! child's exit the way the FreeBSD `waitid(WNOWAIT)` path does.
//! `proc_pidinfo(PROC_PIDTBSDINFO)` reports the process status without
//! reaping, which is the same observation: a `SZOMB` status means the leader
//! exited and stays a zombie, so its pid - and the process-group id minted
//! from it - is still reserved.
//!
//! The `arg` argument is XNU's zombie selector, not padding. For
//! `PROC_PIDTBSDINFO`, `proc_info` sets `findzomb` only when `arg` is nonzero
//! and otherwise calls `proc_find(pid)`, which does not return a zombie. The
//! production probe passes `1`, so the kernel falls back to
//! `proc_find_zombref(pid)` and reports `SZOMB`; [`observe_leader_with_arg`]
//! lets the macOS test compare both selectors on a real unreaped child so the
//! selector's effect is visible in the failure message.
//!
//! Following the `bitty-sandbox`, `bitty-winjob`, and `freebsd_sys`
//! precedent, the crate root keeps `#![deny(unsafe_code)]` and only this
//! module carries `#[allow(unsafe_code)]`; the safe predicate above the call
//! is the module's whole surface.

#![allow(unsafe_code)]

use std::fmt;
use std::mem::size_of;

use nix::libc;

/// The `arg` value that makes XNU resolve an unreaped zombie.
///
/// XNU's `proc_info` sets `findzomb` for `PROC_PIDTBSDINFO` only when `arg` is
/// nonzero; `arg = 0` calls `proc_find(pid)`, which does not return a zombie.
/// `arg = 1` falls back to `proc_find_zombref(pid)` and reports `SZOMB`.
const ZOMBIE_INCLUSIVE_ARG: u64 = 1;

/// One `proc_pidinfo(PROC_PIDTBSDINFO)` observation.
///
/// Every field is kept so a failed probe is self-describing: `read` and
/// `expected` distinguish a short/failed call, `errno` names the failure,
/// and `pid`/`ppid`/`status` are the record the kernel wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaderObservation {
    /// Byte count returned by `proc_pidinfo` (`<= 0` on failure).
    pub(crate) read: i32,
    /// `size_of::<proc_bsdinfo>()` passed as the buffer size.
    pub(crate) expected: i32,
    /// `errno` after a failed call, or `0` when a record was returned.
    pub(crate) errno: i32,
    /// `pbi_pid` from the record, or `0` when no record was written.
    pub(crate) pid: u32,
    /// `pbi_ppid` from the record, or `0` when no record was written.
    pub(crate) ppid: u32,
    /// `pbi_status` (`p_stat`) from the record, or `0` when no record was
    /// written.
    pub(crate) status: u32,
}

impl LeaderObservation {
    /// Whether this observation proves `requested` exited and is unreaped.
    ///
    /// The record must be the full expected size, name the requested pid, and
    /// carry `SZOMB`. A short record, a failed call, or a different pid proves
    /// nothing.
    #[must_use]
    pub(crate) const fn proved_exit(&self, requested: u32) -> bool {
        self.read == self.expected && self.pid == requested && self.status == libc::SZOMB
    }
}

impl fmt::Display for LeaderObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "read={} expected={} errno={} ({}) pid={} ppid={} status={}",
            self.read,
            self.expected,
            self.errno,
            std::io::Error::from_raw_os_error(self.errno),
            self.pid,
            self.ppid,
            self.status
        )
    }
}

/// Observe `pid` through `proc_pidinfo(PROC_PIDTBSDINFO)` with an explicit
/// `arg`.
///
/// `arg` is XNU's zombie selector: `0` uses `proc_find`, while a nonzero value
/// also tries `proc_find_zombref`. The production wrapper and the macOS test
/// share this primitive so a runner failure names the exact call.
#[must_use]
pub(crate) fn observe_leader_with_arg(pid: u32, arg: u64) -> LeaderObservation {
    let expected = i32::try_from(size_of::<libc::proc_bsdinfo>()).unwrap_or(i32::MAX);
    let Ok(raw) = i32::try_from(pid) else {
        return LeaderObservation {
            read: -1,
            expected,
            errno: libc::EINVAL,
            pid: 0,
            ppid: 0,
            status: 0,
        };
    };
    // SAFETY: `proc_bsdinfo` is a plain-old-data C struct of integers and byte
    // arrays, so an all-zero value is valid, and `proc_pidinfo` overwrites it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, correctly sized out-parameter for the
    // requested flavor; the kernel writes at most `expected` bytes into it.
    let read = unsafe {
        libc::proc_pidinfo(
            raw,
            libc::PROC_PIDTBSDINFO,
            arg,
            std::ptr::addr_of_mut!(info).cast(),
            expected,
        )
    };
    if read != expected {
        return LeaderObservation {
            read,
            expected,
            errno: if read <= 0 {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
            } else {
                0
            },
            pid: 0,
            ppid: 0,
            status: 0,
        };
    }
    LeaderObservation {
        read,
        expected,
        errno: 0,
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        status: info.pbi_status,
    }
}

/// Observe `pid` with the production selector.
#[must_use]
pub(crate) fn observe_leader(pid: u32) -> LeaderObservation {
    observe_leader_with_arg(pid, ZOMBIE_INCLUSIVE_ARG)
}

/// Whether `pid` has exited and waits to be reaped.
///
/// The production selector is zombie-inclusive, so a child that exited and was
/// not reaped is observed with `pbi_status == SZOMB` instead of `ESRCH`.
/// Returns `false` when the status cannot be read: an unreadable status never
/// proves an exit, so the caller keeps its existing state instead of acting on
/// an observation it does not have.
#[must_use]
pub(crate) fn leader_exited(pid: u32) -> bool {
    observe_leader(pid).proved_exit(pid)
}
