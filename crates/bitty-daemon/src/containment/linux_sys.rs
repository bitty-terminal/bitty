//! The single documented Linux syscall seam for containment (bitty#2791).
//!
//! The process-spawn, pidfd, descriptor-flag, and PTY calls that safe
//! `std`/`nix` does not wrap live here:
//!
//! - `Command::pre_exec` plus one `write(2)` of the child's own pid into a
//!   pre-opened `cgroup.procs` descriptor, so the child enters its session
//!   subtree before `exec` and before it can fork;
//! - `Command::pre_exec` plus `setsid(2)` and `ioctl(TIOCSCTTY)`, so a PTY
//!   worker is a session and process-group leader whose controlling terminal
//!   is the PTY slave on fd 0;
//! - `pidfd_open(2)` and `pidfd_send_signal(2)`, so an orphan reap pins the
//!   exact process and cannot signal a reused pid;
//! - `close_range(2)` in the child, so the worker's descriptor table is
//!   exactly its stdio no matter what the parent already held;
//! - `ioctl(TIOCSWINSZ)` plus the `fcntl(2)` descriptor flags the PTY owner
//!   needs (close-on-exec and non-blocking).
//!
//! All calls are async-signal-safe or descriptor-local. The `pre_exec`
//! closures allocate nothing and touch no lock: one formats the pid into a
//! stack buffer and calls `write`, another calls `setsid` and `ioctl`, and the
//! descriptor sweep calls one syscall or a bounded `fcntl` loop. A failure
//! aborts the exec and surfaces as a spawn error.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd as _, FromRawFd as _, OwnedFd};
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
/// `fs.nr_open` (1048576 by default), so the fixed ceiling covers every
/// descriptor a process can hold.
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
/// Descriptors are marked, not closed. `std` reports a failed `exec` through a
/// close-on-exec pipe that must stay usable until `exec` itself, and the
/// cgroup plan still writes the child's pid through its `cgroup.procs`
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
    // `close_range(2)` arrived in Linux 5.9 and `CLOSE_RANGE_CLOEXEC` in 5.11;
    // the raw syscall keeps the binary runnable on a libc whose `close_range`
    // wrapper is newer than the deployed glibc.
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
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // `ENOSYS`: no `close_range` at all. `EINVAL`: the kernel predates the
        // `CLOSE_RANGE_CLOEXEC` flag. `EPERM`: a sandbox filter denies a
        // syscall it does not know; the `fcntl` scan still proves the sweep.
        Some(code)
            if code == nix::libc::ENOSYS
                || code == nix::libc::EINVAL
                || code == nix::libc::EPERM =>
        {
            mark_inherited_cloexec_by_fcntl()
        }
        // Any other failure means the sweep cannot be proven: abort the exec
        // rather than start a worker with an unknown descriptor table.
        _ => Err(error),
    }
}

/// The fallback sweep for kernels without `CLOSE_RANGE_CLOEXEC`.
///
/// The scan is bounded by [`FALLBACK_SCAN_MAX`] rather than the process soft
/// limit. It runs once per spawn and only on kernels that predate the
/// `close_range` flag, where the full scan costs a bounded sweep of `fcntl`
/// calls and never more.
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
        let _pty = crate::containment::test_lock::lock();
        let pty = nix::pty::openpty(None, None).expect("pty");
        let mut master = File::from(pty.master);
        set_cloexec(&master).expect("master close-on-exec");
        let slave = File::from(pty.slave);
        let stdin = Stdio::from(dup_cloexec(&slave).expect("slave dup"));
        let stdout = Stdio::from(dup_cloexec(&slave).expect("slave dup"));
        set_cloexec(&slave).expect("slave close-on-exec");
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

    #[test]
    fn descriptors_above_stdio_do_not_survive_exec() {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

        use nix::fcntl::{FcntlArg, fcntl};

        let _pty = crate::containment::test_lock::lock();
        // A pipe the parent holds without close-on-exec, the same class of
        // descriptor a test harness hands its child on CI (bitty#2824). The
        // high duplicate keeps the probe clear of the descriptors `sh` opens
        // for itself.
        let (read, _write) = nix::unistd::pipe().expect("pipe");
        let duplicated = fcntl(&read, FcntlArg::F_DUPFD(64)).expect("dup the read end");
        // SAFETY: `F_DUPFD` returned a fresh descriptor owned by this test.
        let inherited = unsafe { OwnedFd::from_raw_fd(duplicated) };
        let probe = format!("test -e /proc/self/fd/{}", inherited.as_raw_fd());

        // The probe has teeth: without the hook the parent's descriptor is
        // visible in the child after `exec`.
        let leaked = Command::new("/bin/sh")
            .arg("-c")
            .arg(&probe)
            .status()
            .expect("the control child");
        assert!(
            leaked.success(),
            "the parent's descriptor must be visible without the hook"
        );

        let mut hardened = Command::new("/bin/sh");
        hardened.arg("-c").arg(&probe);
        close_inherited_descriptors(&mut hardened);
        let swept = hardened.status().expect("the hardened child");
        assert!(
            !swept.success(),
            "a descriptor the parent held survived the child's exec"
        );
    }
}
