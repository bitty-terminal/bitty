//! The single documented Linux-only syscall seam for containment
//! (bitty#2791).
//!
//! The calls that safe `std`/`nix` does not wrap and that only the Linux
//! plans need live here:
//!
//! - `Command::pre_exec` plus one `write(2)` of the child's own pid into a
//!   pre-opened `cgroup.procs` descriptor, so the child enters its session
//!   subtree before `exec` and before it can fork;
//! - `pidfd_open(2)` and `pidfd_send_signal(2)`, so an orphan reap pins the
//!   exact process and cannot signal a reused pid.
//!
//! The calls every POSIX cell shares (the session/controlling-terminal hook,
//! the signal-state reset, the descriptor sweep, the `fcntl(2)` flags, and
//! `ioctl(TIOCSWINSZ)`) live in [`super::posix_sys`].
//!
//! All calls are async-signal-safe or descriptor-local. The `pre_exec`
//! closures allocate nothing and touch no lock: one formats the pid into a
//! stack buffer and calls `write`. A failure aborts the exec and surfaces as a
//! spawn error.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command};

use nix::sys::signal::Signal;

#[cfg(test)]
use super::posix_sys::{
    close_inherited_descriptors, dup_cloexec, establish_controlling_terminal, set_cloexec,
};

/// Test-only fixtures for the child signal-state reset (bitty#2920).
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_signal_state {
    use nix::sys::signal::{
        SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal, pthread_sigmask, sigaction,
    };

    /// The daemon's own blocked mask plus the ignored dispositions a service
    /// manager installs, applied to the calling thread.
    ///
    /// `pthread_sigmask` is per thread and `fork` inherits the forking
    /// thread's mask and dispositions, so this is exactly the state a hosted
    /// shell would inherit without the child-side reset. The guard restores
    /// both on drop.
    pub(crate) struct InheritedSignalState {
        previous_mask: SigSet,
        previous_ignored: [(Signal, SigAction); 2],
    }

    impl InheritedSignalState {
        /// Install the inherited state, or fail the test.
        pub(crate) fn install() -> Self {
            let mut mask = SigSet::empty();
            mask.add(Signal::SIGTERM);
            mask.add(Signal::SIGINT);
            mask.add(Signal::SIGHUP);
            let mut previous_mask = SigSet::empty();
            pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), Some(&mut previous_mask))
                .expect("block the daemon's signals");
            // systemd sets `SIGPIPE`/`SIGXFSZ` to `SIG_IGN` for every service.
            // `SIGXFSZ` is the one a Rust child does not re-ignore itself, so
            // both are installed and both are restored.
            let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
            let mut previous_ignored = [(Signal::SIGPIPE, ignore), (Signal::SIGXFSZ, ignore)];
            for (signal, action) in &mut previous_ignored {
                // SAFETY: `sigaction(2)` takes a fully initialized action; the
                // previous action is restored on drop.
                *action = unsafe { sigaction(*signal, &ignore) }.expect("ignore the signal");
            }
            Self {
                previous_mask,
                previous_ignored,
            }
        }
    }

    impl Drop for InheritedSignalState {
        fn drop(&mut self) {
            pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&self.previous_mask), None)
                .expect("restore the test thread's mask");
            for (signal, action) in &self.previous_ignored {
                // SAFETY: the action is the one this guard replaced.
                let _ = unsafe { sigaction(*signal, action) };
            }
        }
    }
}

/// Pin one inheritable descriptor at or above `floor` for the EV-03
/// negative-control test.
///
/// The returned descriptor has its close-on-exec bit cleared, so it survives
/// `exec` exactly like a parent handle whose `CLOEXEC` was dropped. `floor`
/// keeps it clear of stdio and of the descriptors `openpty` and `Command`
/// open for themselves.
///
/// Test-only: production never hands a child an inheritable descriptor.
///
/// # Errors
///
/// Returns the `pipe(2)` or `fcntl(2)` error.
#[cfg(test)]
pub(crate) fn inheritable_probe_descriptor(floor: i32) -> io::Result<OwnedFd> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let (read, write) = nix::unistd::pipe().map_err(io::Error::from)?;
    // Only the pinned descriptor is the probe; the pipe ends themselves must
    // not add to the child's inherited table.
    let pinned = fcntl(&read, FcntlArg::F_DUPFD(floor)).map_err(io::Error::from)?;
    drop(read);
    drop(write);
    // SAFETY: `F_DUPFD` returned a fresh descriptor owned by this call.
    let probe = unsafe { OwnedFd::from_raw_fd(pinned) };
    // `F_DUPFD` already clears the flag on the copy; setting it explicitly
    // makes the dropped flag the test's stated precondition.
    fcntl(&probe, FcntlArg::F_SETFD(FdFlag::empty())).map_err(io::Error::from)?;
    Ok(probe)
}

/// Clear `fd`'s close-on-exec bit in the forked child, after the production
/// descriptor sweep.
///
/// This is the negative-control mutation for the EV-03 test: the registered
/// hook runs after [`close_inherited_descriptors`] and before `exec`, so the
/// descriptor reaches `exec` with a dropped `CLOEXEC`. Production never
/// registers it.
#[cfg(test)]
pub(crate) fn drop_cloexec_before_exec(command: &mut Command, fd: i32) {
    // SAFETY: `pre_exec` runs in the forked child between fork and exec. The
    // closure calls one descriptor-local `fcntl(2)`, allocates nothing, and
    // touches no Rust data structure or lock. Returning an error aborts the
    // exec.
    unsafe {
        command.pre_exec(move || {
            if nix::libc::fcntl(fd, nix::libc::F_SETFD, 0) < 0 {
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

    /// Whether the pinned process has exited.
    ///
    /// A pidfd becomes readable when the process exits; `poll(2)` with a zero
    /// timeout peeks at that state without blocking or reaping.
    ///
    /// # Errors
    ///
    /// Returns the OS error from `poll(2)`.
    pub(super) fn has_exited(&self) -> io::Result<bool> {
        let mut fds = [nix::poll::PollFd::new(
            self.0.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        match nix::poll::poll(&mut fds, 0u16) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(fds[0]
                .revents()
                .is_some_and(|events| events.contains(nix::poll::PollFlags::POLLIN))),
            Err(error) => Err(io::Error::from(error)),
        }
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
