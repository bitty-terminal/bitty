//! The single documented Linux syscall seam for containment (bitty#2791).
//!
//! Everything this module needs that safe `std`/`nix` does not wrap lives here
//! and nowhere else:
//!
//! - `Command::pre_exec` plus one `write(2)` of the child's own pid into a
//!   pre-opened `cgroup.procs` descriptor, so the child enters its session
//!   subtree before `exec` and before it can fork;
//! - `pidfd_open(2)` and `pidfd_send_signal(2)`, so an orphan reap pins the
//!   exact process and cannot signal a reused pid.
//!
//! Both are async-signal-safe syscalls. The `pre_exec` closure allocates
//! nothing and touches no lock: it formats the pid into a stack buffer and
//! calls `write`. A failure aborts the exec and surfaces as a spawn error.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};

use nix::sys::signal::Signal;

/// Spawn `command` with its pid already written into `procs`.
///
/// `procs` is an open, writable `cgroup.procs` descriptor for the session
/// subtree. The child writes its own pid before exec, so the cgroup identity
/// exists before any user code runs.
///
/// # Errors
///
/// Returns the spawn error, including a failed cgroup write.
pub(super) fn spawn_into_cgroup(command: &mut Command, procs: File) -> io::Result<Child> {
    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure only calls `getpid(2)` and `write(2)` (through `nix`), formats
    // the pid into a fixed stack buffer, and allocates nothing. Returning an
    // error aborts the exec and is reported to the parent by `std`.
    unsafe {
        command.pre_exec(move || write_pid(&procs));
    }
    command.spawn()
}

fn write_pid(procs: &File) -> io::Result<()> {
    let pid = nix::unistd::getpid().as_raw();
    let mut digits = [0u8; 21];
    let mut index = digits.len();
    let mut value = u32::try_from(pid).unwrap_or(0);
    loop {
        index -= 1;
        digits[index] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        if value == 0 {
            break;
        }
    }
    index -= 1;
    digits[index] = b'\n';
    nix::unistd::write(procs, &digits[index..])
        .map(|_written| ())
        .map_err(io::Error::from)
}

/// A kernel pidfd pinning one process for the life of the value.
#[derive(Debug)]
pub(super) struct PidFd(OwnedFd);

impl PidFd {
    /// Open a pidfd for `pid`.
    ///
    /// # Errors
    ///
    /// Returns the OS error (`ESRCH` for a vanished pid, `ENOSYS` before Linux
    /// 5.3).
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        let raw = i32::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `pidfd_open` takes two integer arguments and returns a new
        // file descriptor or -1. No pointer crosses the boundary.
        let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, raw, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh, owned descriptor returned by the kernel and
        // not used anywhere else.
        Ok(Self(unsafe {
            OwnedFd::from_raw_fd(
                i32::try_from(fd).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?,
            )
        }))
    }

    /// Send a signal through the pinned process handle.
    ///
    /// # Errors
    ///
    /// Returns the OS error; `ESRCH` means the pinned process already exited.
    pub(super) fn send_signal(&self, signal: Signal) -> io::Result<()> {
        let raw = self.0.as_raw_fd();
        // SAFETY: the descriptor is owned by `self` and open for the call;
        // `siginfo` and `flags` are the documented null/zero values.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                raw,
                signal as i32,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0u32,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
