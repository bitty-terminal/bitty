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
//! Following the `bitty-sandbox`, `bitty-winjob`, and `freebsd_sys`
//! precedent, the crate root keeps `#![deny(unsafe_code)]` and only this
//! module carries `#[allow(unsafe_code)]`; the safe predicate above the call
//! is the module's whole surface.

#![allow(unsafe_code)]

use std::mem::size_of;

use nix::libc;

/// Whether `pid` has exited and waits to be reaped.
///
/// Returns `false` when the status cannot be read: an unreadable status never
/// proves an exit, so the caller keeps its existing state instead of acting on
/// an observation it does not have.
pub(crate) fn leader_exited(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let expected = size_of::<libc::proc_bsdinfo>();
    // SAFETY: `proc_bsdinfo` is a plain-old-data C struct of integers and byte
    // arrays, so an all-zero value is valid, and `proc_pidinfo` overwrites it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, correctly sized out-parameter for the
    // requested flavor; the kernel writes at most `expected` bytes into it.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            i32::try_from(expected).unwrap_or(i32::MAX),
        )
    };
    if usize::try_from(read) != Ok(expected) {
        return false;
    }
    info.pbi_status == libc::SZOMB
}
