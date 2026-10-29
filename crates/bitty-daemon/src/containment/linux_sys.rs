//! The single documented Linux syscall seam for containment (bitty#2791).
//!
//! The process-spawn and pidfd calls that safe `std`/`nix` does not wrap live
//! here; the remaining raw calls (`TIOCSWINSZ` resize, `BorrowedFd` wrappers) are
//! documented in `worker.rs`:
//!
//! - `Command::pre_exec` plus one `write(2)` of the child's own pid into a
//!   pre-opened `cgroup.procs` descriptor, so the child enters its session
//!   subtree before `exec` and before it can fork;
//! - `Command::pre_exec` plus `setsid(2)` and `ioctl(TIOCSCTTY)`, so a PTY
//!   worker is a session and process-group leader whose controlling terminal
//!   is the PTY slave on fd 0;
//! - `pidfd_open(2)` and `pidfd_send_signal(2)`, so an orphan reap pins the
//!   exact process and cannot signal a reused pid.
//!
//! All three are async-signal-safe syscalls. The `pre_exec` closures allocate
//! nothing and touch no lock: one formats the pid into a stack buffer and calls
//! `write`, the other calls `setsid` and `ioctl`. A failure aborts the exec and
//! surfaces as a spawn error.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};

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

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::process::Stdio;

    use super::*;

    #[test]
    fn the_pty_child_leads_its_session_and_owns_the_controlling_terminal() {
        let pty = nix::pty::openpty(None, None).expect("pty");
        let mut master = File::from(pty.master);
        let slave = pty.slave;
        let stdin = Stdio::from(slave.try_clone().expect("slave clone"));
        let stdout = Stdio::from(slave.try_clone().expect("slave clone"));
        let stderr = Stdio::from(slave);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("cat /proc/$$/stat")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        establish_controlling_terminal(&mut command);
        let mut child = command.spawn().expect("child");
        // The `Command` keeps its own copies of the slave stdio until it is
        // dropped; without this the master never sees EOF.
        drop(command);

        let mut output = String::new();
        let mut chunk = [0u8; 256];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => output.push_str(&String::from_utf8_lossy(&chunk[..read])),
                // Linux reports EIO on the master once the last slave closed.
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::EIO as i32) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("read the pty: {error}"),
            }
        }
        let status = child.wait().expect("wait");
        assert!(status.success(), "the session-leader child failed");

        // Fields after the parenthesized comm: state, ppid, pgrp, session,
        // tty_nr. A session leader has pgrp == sid == pid, and a controlling
        // terminal reports a non-zero tty_nr.
        let (_, rest) = output.split_once(')').expect("a /proc stat line");
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let pgrp: u32 = fields[2].parse().expect("pgrp");
        let session: u32 = fields[3].parse().expect("session");
        let tty: i64 = fields[4].parse().expect("tty_nr");
        assert_eq!(pgrp, session, "the process group must be the session");
        assert_eq!(
            child.id(),
            session,
            "the child must lead the session it created"
        );
        assert_ne!(tty, 0, "the PTY slave must be the controlling terminal");
    }
}
