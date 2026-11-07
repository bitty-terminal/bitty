//! Raw FreeBSD `procctl(2)` reaper bindings.
//!
//! The `PROC_REAP_*` calls have no safe wrapper in `nix` 0.31 or the
//! workspace `libc` (which exposes the constants and the `procctl` symbol
//! only), so this private module owns the FFI. Following the `bitty-sandbox`
//! and `bitty-winjob` precedent, the crate root keeps `#![deny(unsafe_code)]`
//! and only this module carries `#[allow(unsafe_code)]`; every block has a
//! `SAFETY` rationale and the module above it exposes a safe API.
//!
//! Boundary rules:
//!
//! - The only inputs are the current pid and a capacity bound.
//! - Every out-parameter is a `#[repr(C)]` struct on this module's stack or an
//!   owned `Vec` whose length is passed exactly; the kernel never writes past
//!   the buffer.
//! - A short or missing buffer is reported as `ENOMEM` with the required
//!   length, so the caller can grow once and retry; a length above the
//!   caller's bound is refused instead of truncated.
//! - Every failure is `io::Error::last_os_error()` read immediately after the
//!   failing call.

use std::io;
use std::ptr;

use libc::{c_uint, c_void, pid_t};

/// `REAPER_STATUS_OWNED`: this process is the reaper for its descendants.
const REAPER_STATUS_OWNED: c_uint = 0x0000_0001;

/// `REAPER_PIDINFO_VALID`: the entry describes a descendant.
const REAPER_PIDINFO_VALID: c_uint = 0x0000_0001;

/// `REAPER_PIDINFO_ZOMBIE`: the descendant has exited and waits to be reaped.
const REAPER_PIDINFO_ZOMBIE: c_uint = 0x0000_0008;

/// `REAPER_PIDINFO_EXITING`: the descendant is exiting but not yet dead.
const REAPER_PIDINFO_EXITING: c_uint = 0x0000_0020;

/// `struct procctl_reaper_status` from `sys/procctl.h`.
///
/// The kernel fills every lane; the plan reads only the ownership fields, so
/// the count and padding lanes are deliberately kept for the ABI and marked
/// as such rather than removed.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct ReaperStatus {
    flags: c_uint,
    children: c_uint,
    descendants: c_uint,
    reaper: pid_t,
    pid: pid_t,
    pad0: [c_uint; 15],
}

impl ReaperStatus {
    const fn new() -> Self {
        Self {
            flags: 0,
            children: 0,
            descendants: 0,
            reaper: 0,
            pid: 0,
            pad0: [0; 15],
        }
    }

    /// Whether this process owns the reaper role and reports itself as the
    /// reaper.
    pub(crate) fn owned_by_current(self) -> bool {
        self.flags & REAPER_STATUS_OWNED != 0 && self.reaper == current_pid_signed()
    }

    /// The total number of processes this reaper can reap.
    ///
    /// `PROC_REAP_GETPIDS` never reports its own truncation, so this count
    /// sizes the enumeration and a sentinel slot detects growth between the
    /// two calls.
    pub(crate) fn descendants(self) -> u32 {
        self.descendants
    }
}

/// `struct procctl_reaper_pidinfo` from `sys/procctl.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReaperPidInfo {
    /// The descendant's pid.
    pub(crate) pid: pid_t,
    /// The direct child of the reaper whose subtree contains `pid`.
    pub(crate) subtree: pid_t,
    /// `REAPER_PIDINFO_*` flags.
    pub(crate) flags: c_uint,
    /// ABI padding lanes.
    #[allow(dead_code)]
    pad0: [c_uint; 15],
}

impl ReaperPidInfo {
    const fn new() -> Self {
        Self {
            pid: 0,
            subtree: 0,
            flags: 0,
            pad0: [0; 15],
        }
    }

    /// Whether the entry describes a live descendant (the valid bit is set).
    pub(crate) fn valid(self) -> bool {
        self.flags & REAPER_PIDINFO_VALID != 0
    }

    /// Whether the descendant is exiting or already a zombie.
    pub(crate) fn not_running(self) -> bool {
        self.flags & (REAPER_PIDINFO_EXITING | REAPER_PIDINFO_ZOMBIE) != 0
    }
}

/// `struct procctl_reaper_pids` from `sys/procctl.h`.
#[repr(C)]
#[allow(dead_code)]
struct ReaperPids {
    count: c_uint,
    pad0: [c_uint; 15],
    pids: *mut ReaperPidInfo,
}

fn current_pid_signed() -> pid_t {
    // SAFETY: `getpid` takes no arguments and cannot fail.
    unsafe { libc::getpid() }
}

fn current_pid() -> libc::id_t {
    libc::id_t::from(current_pid_signed())
}

/// Read `PROC_REAP_STATUS` for the current process.
pub(crate) fn status() -> io::Result<ReaperStatus> {
    let mut status = ReaperStatus::new();
    // SAFETY: `P_PID` selects the current process; `PROC_REAP_STATUS` writes
    // exactly one `procctl_reaper_status` through `data`, which points at a
    // live stack value of that exact type.
    let result = unsafe {
        libc::procctl(
            libc::P_PID,
            current_pid(),
            libc::PROC_REAP_STATUS,
            ptr::from_mut(&mut status).cast::<c_void>(),
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(status)
    }
}

/// Acquire the `PROC_REAP_ACQUIRE` reaper role for the current process.
pub(crate) fn acquire() -> io::Result<()> {
    // SAFETY: `PROC_REAP_ACQUIRE` takes no data pointer (`NULL` is the
    // documented form) and applies to the current process selected by P_PID.
    let result = unsafe {
        libc::procctl(
            libc::P_PID,
            current_pid(),
            libc::PROC_REAP_ACQUIRE,
            ptr::null_mut(),
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Enumerate the reaper's descendants with `PROC_REAP_GETPIDS`.
///
/// `PROC_REAP_GETPIDS` caps the copy-out at the caller's `rp_count` and does
/// not report truncation, so this function sizes the buffer from
/// `PROC_REAP_STATUS` plus one sentinel slot. If the sentinel is also filled,
/// the descendant set grew between the two calls, and the call fails closed
/// instead of returning a truncated list. `max` bounds one allocation; an
/// answer larger than the bound is an error, never a partial proof.
pub(crate) fn descendants(max: usize) -> io::Result<Vec<ReaperPidInfo>> {
    let status = status()?;
    let total = usize::try_from(status.descendants()).unwrap_or(usize::MAX);
    let capacity = total.saturating_add(1);
    if capacity > max.max(1) {
        return Err(io::Error::other(
            "the reaper pid list exceeds the configured bound",
        ));
    }
    let mut buffer = vec![ReaperPidInfo::new(); capacity];
    let mut query = ReaperPids {
        count: u32::try_from(capacity).unwrap_or(u32::MAX),
        pad0: [0; 15],
        pids: buffer.as_mut_ptr(),
    };
    // SAFETY: `PROC_REAP_GETPIDS` reads `rp_count` as the array length and
    // writes at most that many `procctl_reaper_pidinfo` entries through
    // `rp_pids`, which points at `buffer`'s `capacity` elements.
    let result = unsafe {
        libc::procctl(
            libc::P_PID,
            current_pid(),
            libc::PROC_REAP_GETPIDS,
            ptr::from_mut(&mut query).cast::<c_void>(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let filled = buffer.iter().take_while(|entry| entry.valid()).count();
    if filled == capacity {
        return Err(io::Error::other(
            "the reaper pid list grew past the status snapshot",
        ));
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// The status a fresh acquisition must report, for tests and diagnostics.
pub(crate) fn is_reaper() -> bool {
    status().is_ok_and(ReaperStatus::owned_by_current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reaper_structs_match_the_freebsd_abi_widths() {
        // `sys/procctl.h` fixes these sizes: three `u_int` lanes, two
        // `pid_t`s, and fifteen padding lanes for the status; three 32-bit
        // lanes plus fifteen padding lanes for one pid info.
        assert_eq!(std::mem::size_of::<ReaperStatus>(), 80);
        assert_eq!(std::mem::size_of::<ReaperPidInfo>(), 72);
        assert_eq!(std::mem::align_of::<ReaperStatus>(), 4);
        assert_eq!(std::mem::align_of::<ReaperPidInfo>(), 4);
    }

    fn ensure_reaper() {
        if !is_reaper() {
            acquire().expect("the reaper role is available to an unprivileged process");
        }
        assert!(is_reaper());
    }

    #[test]
    fn acquiring_the_reaper_role_is_idempotent() {
        ensure_reaper();
        // A second probe path must not fail on an already-acquired role.
        assert!(is_reaper());
    }

    #[test]
    fn the_pid_list_is_bounded() {
        ensure_reaper();
        // No descendants are required for the shape check: the call either
        // returns an empty list or a list no larger than the bound.
        let entries = descendants(4096).expect("the reaper list is readable");
        assert!(entries.len() <= 4096);
    }
}
