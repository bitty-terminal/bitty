//! The POSIX syscall seam shared by every supported POSIX cell.
//!
//! These are the process-spawn, descriptor-flag, and PTY calls that safe
//! `std`/`nix` does not wrap and that Linux, macOS, and FreeBSD all share:
//!
//! - `Command::pre_exec` plus `setsid(2)` and `ioctl(TIOCSCTTY)`, so a PTY
//!   worker is a session and process-group leader whose controlling terminal
//!   is the PTY slave on fd 0;
//! - `Command::pre_exec` plus `pthread_sigmask(3)` and `sigaction(2)`, so the
//!   worker starts with an empty signal mask and the default disposition for
//!   every resettable signal, instead of the daemon's blocked mask and the
//!   ignored dispositions a service manager hands it;
//! - the descriptor sweep in the child, so the worker's descriptor table is
//!   exactly its stdio no matter what the parent already held;
//! - the `fcntl(2)` flags the PTY owner needs (close-on-exec and
//!   non-blocking) and `ioctl(TIOCSWINSZ)`.
//!
//! Only the descriptor sweep differs per cell: Linux and FreeBSD have
//! `close_range(2)` with `CLOSE_RANGE_CLOEXEC`, macOS has no such call and
//! uses the bounded `fcntl(2)` scan directly. Marking, never closing, keeps
//! `std`'s close-on-exec `exec` error pipe usable until the exec itself.
//!
//! The Linux-only pieces stay in `super::linux_sys`: the cgroup membership
//! write that runs before `exec` and the pidfd that pins an orphan. The
//! FreeBSD reaper FFI stays in `super::freebsd_sys` and the macOS exit peek in
//! `super::macos_sys`.
//!
//! All calls are async-signal-safe or descriptor-local. The `pre_exec`
//! closures allocate nothing and touch no lock: one calls `setsid` and
//! `ioctl`, another calls `pthread_sigmask`/`sigaction`, and the descriptor
//! sweep calls one syscall or a bounded `fcntl` loop. A failure aborts the
//! exec and surfaces as a spawn error.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::Command;

use nix::sys::signal::Signal;

/// Install the PTY session-leader hook on `command`.
///
/// The hook runs between fork and exec: `setsid(2)` makes the child a session
/// and process-group leader, and `ioctl(TIOCSCTTY)` on the already duplicated
/// PTY slave (fd 0) makes it the controlling terminal. Job control and
/// `SIGWINCH` therefore reach the worker's group rather than the daemon's.
///
/// The caller must have set the child's stdin/stdout/stderr to the PTY slave
/// before spawning.
pub(crate) fn establish_controlling_terminal(command: &mut Command) {
    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure only calls `setsid(2)` and `ioctl(2)` on fd 0, allocates
    // nothing, and touches no Rust data structure or lock. Returning an error
    // aborts the exec and is reported to the parent by `std`.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(io::Error::from)?;
            if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The highest signal number `reset_child_signal_state` considers.
///
/// The kernel's `_NSIG` is 64 on Linux, 32 on macOS, and 128 on FreeBSD, but
/// every signal FreeBSD defines is at or below 33. One bound covers every
/// supported cell; `Signal::try_from` drops the numbers a cell does not
/// define, and `sigaction(2)` errors are ignored below.
const LAST_RESETTABLE_SIGNAL: i32 = 64;

/// Restore the child's signal mask and dispositions to the POSIX defaults.
///
/// No signal property other than the disposition survives `execve(2)`, and
/// neither a blocked mask nor `SIG_IGN` is reset by it. Both are inherited
/// from the daemon, so a hosted shell would start with
/// `SIGTERM`/`SIGINT`/`SIGHUP` blocked (the daemon's own mask, bitty#2920)
/// and with `SIGPIPE`/`SIGXFSZ` ignored (the service manager's SysV
/// compatibility default). A shell that keeps that state never sees the
/// hangup that should end it and dies only on `SIGKILL`.
///
/// The hook runs between fork and exec and only calls async-signal-safe
/// functions: `pthread_sigmask(3)` and `sigaction(2)`, with no allocation and
/// no lock.
pub(crate) fn reset_child_signal_state(command: &mut Command) {
    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure calls `pthread_sigmask(3)` and `sigaction(2)` only: both are
    // async-signal-safe, and it allocates nothing and touches no Rust data
    // structure or lock. Returning an error aborts the exec and is reported to
    // the parent by `std`.
    unsafe {
        command.pre_exec(clear_inherited_signal_state);
    }
}

/// The child-side reset registered by [`reset_child_signal_state`].
fn clear_inherited_signal_state() -> io::Result<()> {
    use nix::sys::signal::{
        SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, pthread_sigmask, sigaction,
    };

    // An empty mask unblocks every signal the daemon blocked for its own
    // signal fd. `pthread_sigmask` is used instead of `sigprocmask` because the
    // child is single-threaded after `fork` anyway and this is the call POSIX
    // names async-signal-safe in every implementation.
    let empty = SigSet::empty();
    pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&empty), None).map_err(io::Error::from)?;

    // `execve(2)` keeps `SIG_IGN` but resets a handler to the default, so only
    // an ignored disposition can survive into the shell. Resetting every
    // signal whose disposition may change covers the service manager's
    // `SIGPIPE`/`SIGXFSZ` ignore and anything else the daemon inherited,
    // instead of enumerating the ones known today.
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for signo in 1..=LAST_RESETTABLE_SIGNAL {
        // `SIGKILL` and `SIGSTOP` have no settable disposition, and the
        // numbers the C library reserves are not in `Signal`; the fallible
        // conversion drops both without a second list to keep in sync.
        let Ok(signal) = Signal::try_from(signo) else {
            continue;
        };
        // SAFETY: `sigaction(2)` takes a fully initialized action and returns
        // -1 on error; no pointer outlives the call. A refusal (`EINVAL` for a
        // signal with no settable disposition) leaves the inherited state,
        // which is the default for that signal anyway.
        let _ = unsafe { sigaction(signal, &default) };
    }
    Ok(())
}

/// Set close-on-exec on `file`.
///
/// # Errors
///
/// Returns the `fcntl(2)` error.
pub(crate) fn set_cloexec(file: &File) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    fcntl(file, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map(|_| ())
        .map_err(io::Error::from)
}

/// Duplicate `fd` with close-on-exec set atomically.
///
/// `dup(2)` clears the flag on the copy and `dup2(2)` clears it on the
/// target, so a `dup` followed by `F_SETFD` leaves a window in which a
/// concurrently forked child can inherit the copy. `F_DUPFD_CLOEXEC` closes
/// that window: the returned descriptor never exists without the flag.
///
/// # Errors
///
/// Returns the `fcntl(2)` error.
pub(crate) fn dup_cloexec<Fd: AsFd>(fd: Fd) -> io::Result<File> {
    use nix::fcntl::{FcntlArg, fcntl};

    let raw = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(0)).map_err(io::Error::from)?;
    // SAFETY: `F_DUPFD_CLOEXEC` returned a fresh descriptor owned by this
    // call and not referenced anywhere else.
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(raw) }))
}

/// The lowest descriptor a worker child may not inherit: stdio is 0/1/2.
const FIRST_INHERITED_FD: u32 = 3;

/// Highest descriptor the `fcntl` fallback scans: the loop covers
/// `3..FALLBACK_SCAN_MAX`.
///
/// The process soft `RLIMIT_NOFILE` is not a valid bound: a descriptor opened
/// before the limit was lowered keeps its number above the new soft limit and
/// would survive `exec`. The kernel refuses descriptors at or above
/// `fs.nr_open` on Linux (1048576 by default) and at or above
/// `kern.maxfilesperproc` on the BSDs (below 1048576 at their defaults), so
/// the fixed ceiling covers every descriptor a process can hold.
const FALLBACK_SCAN_MAX: u32 = 1 << 20;

/// Mark every descriptor above stdio close-on-exec in the forked child.
///
/// `Command`'s stdio setup `dup2`s the PTY slave onto 0/1/2 before any
/// `pre_exec` closure runs, so this hook can mark the rest of the table
/// close-on-exec without disturbing the worker's stdio. The kernel walks the
/// child's own table, so descriptors the parent already held - a test harness
/// IPC pipe, an inherited socket - are covered even though this crate never
/// opened them (bitty#2824).
///
/// # Ordering
///
/// The hook is registered on the `Command`, so `std` runs it in the
/// `pre_exec` stage: after the stdio `dup2` and before `execve(2)`. On Linux
/// and FreeBSD the sweep is `close_range(3, u32::MAX, CLOSE_RANGE_CLOEXEC)`,
/// so it starts above the stdio descriptors, leaving the PTY slave on 0/1/2
/// intact, and marks every higher descriptor for the kernel to drop at the
/// exec that follows. macOS has no `close_range(2)`, so it runs the same
/// bounded `fcntl(2)` scan the other cells use as a fallback. This is the
/// ordering artifact `P0-AC-023` / EV-03 asks for, and its negative test
/// lives in `worker::tests`; EV-03 stays `partial` until the evidence package
/// is reviewed.
///
/// Descriptors are marked, not closed. `std` reports a failed `exec` through a
/// close-on-exec pipe that must stay usable until `exec` itself, and the
/// Linux cgroup plan still writes the child's pid through its `cgroup.procs`
/// descriptor in a later hook; both are already close-on-exec or are marked by
/// this sweep, so neither survives the exec. Closing them here would turn a
/// failed `exec` into a silent exit instead of a spawn error.
pub(crate) fn close_inherited_descriptors(command: &mut Command) {
    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure calls one syscall, or a bounded `fcntl` loop, and allocates
    // nothing and touches no Rust data structure or lock. Returning an error
    // aborts the exec and is reported to the parent by `std`.
    unsafe {
        command.pre_exec(mark_inherited_cloexec);
    }
}

/// The child-side descriptor sweep registered by
/// [`close_inherited_descriptors`].
fn mark_inherited_cloexec() -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        match close_range_cloexec() {
            Ok(()) => return Ok(()),
            Err(error) => {
                // `ENOSYS`: no `close_range` at all. `EINVAL`: the kernel
                // predates the `CLOSE_RANGE_CLOEXEC` flag. `EPERM`: a sandbox
                // filter denies a syscall it does not know; the `fcntl` scan
                // still proves the sweep. Any other failure means the sweep
                // cannot be proven, so the exec is aborted rather than started
                // with an unknown descriptor table.
                let fall_back = matches!(
                    error.raw_os_error(),
                    Some(code)
                        if code == nix::libc::ENOSYS
                            || code == nix::libc::EINVAL
                            || code == nix::libc::EPERM
                );
                if !fall_back {
                    return Err(error);
                }
            }
        }
    }
    mark_inherited_cloexec_by_fcntl()
}

/// Mark `3..u32::MAX` close-on-exec with the kernel's `close_range(2)`.
///
/// Linux reached `close_range(2)` in 5.9 and `CLOSE_RANGE_CLOEXEC` in 5.11;
/// the raw syscall keeps the binary runnable on a libc whose wrapper is newer
/// than the deployed glibc. FreeBSD exposes the call through libc.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn close_range_cloexec() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    // SAFETY: `close_range(2)` takes three scalar arguments and returns -1 on
    // error; no pointer crosses the boundary.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_close_range,
            FIRST_INHERITED_FD,
            u32::MAX,
            nix::libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    #[cfg(target_os = "freebsd")]
    // SAFETY: `close_range(2)` takes three scalar arguments and returns -1 on
    // error; no pointer crosses the boundary.
    let result = i64::from(unsafe {
        nix::libc::close_range(
            FIRST_INHERITED_FD,
            u32::MAX,
            nix::libc::CLOSE_RANGE_CLOEXEC as std::os::raw::c_int,
        )
    });
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The fallback sweep for kernels and cells without `CLOSE_RANGE_CLOEXEC`.
///
/// The scan is bounded by [`FALLBACK_SCAN_MAX`] rather than the process soft
/// limit. It runs once per spawn. On macOS, which has no `close_range(2)`, it
/// is the only sweep; elsewhere it covers a kernel that predates the flag.
fn mark_inherited_cloexec_by_fcntl() -> io::Result<()> {
    for fd in FIRST_INHERITED_FD..FALLBACK_SCAN_MAX {
        // SAFETY: `fcntl(2)` with `F_SETFD` takes one descriptor-local flag and
        // no pointer.
        let result = unsafe {
            nix::libc::fcntl(
                fd as nix::libc::c_int,
                nix::libc::F_SETFD,
                nix::libc::FD_CLOEXEC,
            )
        };
        if result == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        // A closed descriptor is `EBADF` and cannot survive `exec`; any other
        // failure means the sweep cannot be proven.
        if error.raw_os_error() != Some(nix::libc::EBADF) {
            return Err(error);
        }
    }
    Ok(())
}

/// Set non-blocking I/O on `file`.
///
/// # Errors
///
/// Returns the `fcntl(2)` error.
pub(crate) fn set_nonblocking(file: &File) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    let flags = fcntl(file, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(file, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(io::Error::from)
}

/// Apply a terminal geometry to the PTY master.
///
/// # Errors
///
/// Returns the `ioctl(2)` error (`ENOTTY` for a non-terminal descriptor).
pub(crate) fn resize_pty(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    let winsize = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `master` is an open descriptor owned by the caller for the
    // duration of the call, and `winsize` is a valid `struct winsize`.
    let result = unsafe {
        nix::libc::ioctl(
            master.as_raw_fd(),
            nix::libc::TIOCSWINSZ as _,
            &winsize as *const nix::pty::Winsize,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
