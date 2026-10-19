//! Unix launch-handle adapter for the provider-publication launch seam.
//!
//! This is the crate's only `#[allow(unsafe_code)]` module. The one unsafe
//! operation is the `Command::pre_exec` registration that places the
//! inherited child-side descriptors at their fixed launch numbers after
//! `fork(2)` and before `execve(2)`. The hook performs raw descriptor
//! arithmetic only: no allocation, no locks, no std I/O, and no access to
//! parent memory beyond the copied descriptor numbers.
//!
//! The bootstrap channel is an anonymous pipe; the control channel is a
//! `socketpair(2)` (via `UnixStream::pair`) so both directions of the
//! dedicated control stream are independent of stdio.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use bitty_network_publication::{ControlClient, ControlError, ControlSession};

use super::{
    CHILD_BOOTSTRAP_DESCRIPTOR, CHILD_CONTROL_DESCRIPTOR, ChildDescriptor, ChildDescriptors,
    EstablishFailure, LaunchError,
};

/// Parent end of the dedicated duplex control channel.
pub(super) type ControlStream = UnixStream;

/// Parent- and child-side handles for one prepared launch.
pub(super) struct Handles {
    bootstrap_writer: Option<File>,
    bootstrap_reader: Option<OwnedFd>,
    control_parent: Option<UnixStream>,
    control_child: Option<UnixStream>,
}

impl Handles {
    pub(super) fn create() -> Result<Self, LaunchError> {
        let (bootstrap_reader, bootstrap_writer) =
            bootstrap_pipe().map_err(LaunchError::Channel)?;
        let (control_parent, control_child) = UnixStream::pair().map_err(LaunchError::Channel)?;
        Ok(Self {
            bootstrap_writer: Some(File::from(bootstrap_writer)),
            bootstrap_reader: Some(bootstrap_reader),
            control_parent: Some(control_parent),
            control_child: Some(control_child),
        })
    }

    pub(super) fn has_child_ends(&self) -> bool {
        self.bootstrap_reader.is_some() && self.control_child.is_some()
    }

    pub(super) fn child_descriptors(&self) -> ChildDescriptors {
        ChildDescriptors::new(
            ChildDescriptor::from_number(CHILD_BOOTSTRAP_DESCRIPTOR),
            ChildDescriptor::from_number(CHILD_CONTROL_DESCRIPTOR),
        )
    }

    pub(super) fn take_bootstrap_writer(&mut self) -> Option<File> {
        self.bootstrap_writer.take()
    }

    /// Test-only access to the child-side bootstrap read end, so a fixture can
    /// impersonate the helper without spawning one.
    #[cfg(test)]
    pub(super) fn take_bootstrap_reader(&mut self) -> Option<OwnedFd> {
        self.bootstrap_reader.take()
    }

    /// Test-only access to the child-side control end, so a fixture can
    /// impersonate the helper without spawning one.
    #[cfg(test)]
    pub(super) fn take_control_child(&mut self) -> Option<UnixStream> {
        self.control_child.take()
    }

    pub(super) fn take_control_parent(&mut self) -> Option<UnixStream> {
        self.control_parent.take()
    }

    pub(super) fn close_child_ends(&mut self) {
        self.bootstrap_reader = None;
        self.control_child = None;
    }

    /// Register the post-fork placement hook.
    ///
    /// # Safety
    ///
    /// `Command::pre_exec` is unsafe because the closure runs in a forked
    /// child of a possibly multithreaded process. The closure created here
    /// captures only two `RawFd` integers and calls
    /// [`place_child_descriptors`], which touches only async-signal-safe
    /// descriptor syscalls (`fcntl`, `dup2`, `close`) and performs no
    /// allocation, locking, or std I/O. A returned error aborts the spawn, so
    /// descriptor placement can never silently fail open.
    #[allow(unsafe_code)]
    pub(super) fn install_child_descriptors(
        &self,
        command: &mut Command,
    ) -> Result<(), LaunchError> {
        let bootstrap = self
            .bootstrap_reader
            .as_ref()
            .ok_or(LaunchError::HandleMissing)?
            .as_raw_fd();
        let control = self
            .control_child
            .as_ref()
            .ok_or(LaunchError::HandleMissing)?
            .as_raw_fd();
        // SAFETY: see the method contract above. Both descriptors stay open
        // in the parent until `close_child_ends` runs after a successful
        // spawn, so the captured numbers remain valid through `fork`.
        unsafe {
            command.pre_exec(move || place_child_descriptors(bootstrap, control));
        }
        Ok(())
    }
}

/// Fail-closed platform check; Unix always supports the launch handles.
pub(super) fn ensure_supported() -> Result<(), LaunchError> {
    Ok(())
}

/// Place both child-side descriptors at their fixed numbers, then close the
/// temporary duplicates.
///
/// Both sources are duplicated above every launch target before either
/// `dup2`, so a source that happens to occupy the other channel's target
/// number cannot be clobbered. The duplicates deliberately use `F_DUPFD`
/// (not `F_DUPFD_CLOEXEC`): `dup2` clears close-on-exec on the placed target,
/// and the temporary duplicates are closed before this function returns.
#[allow(unsafe_code)]
fn place_child_descriptors(bootstrap: RawFd, control: RawFd) -> io::Result<()> {
    // SAFETY: this runs only in the post-fork, single-threaded child created
    // by `Command::spawn`. `fcntl`, `dup2`, and `close` are async-signal-safe
    // and operate on captured descriptor numbers; no memory beyond the stack
    // of this function is shared or mutated.
    unsafe {
        let bootstrap_copy = libc::fcntl(bootstrap, libc::F_DUPFD, 10);
        if bootstrap_copy < 0 {
            return Err(io::Error::last_os_error());
        }
        let control_copy = libc::fcntl(control, libc::F_DUPFD, 10);
        if control_copy < 0 {
            let error = io::Error::last_os_error();
            libc::close(bootstrap_copy);
            return Err(error);
        }

        let mut result = Ok(());
        for (copy, target) in [
            (bootstrap_copy, CHILD_BOOTSTRAP_DESCRIPTOR as RawFd),
            (control_copy, CHILD_CONTROL_DESCRIPTOR as RawFd),
        ] {
            if libc::dup2(copy, target) < 0 {
                result = Err(io::Error::last_os_error());
                break;
            }
        }

        libc::close(bootstrap_copy);
        libc::close(control_copy);
        result
    }
}

/// Drive the pinned TLS client over the parent control stream before
/// `deadline`.
pub(super) fn establish(
    stream: ControlStream,
    client: ControlClient,
    deadline: Instant,
) -> Result<(ControlSession, ControlStream), EstablishFailure> {
    if Instant::now() >= deadline {
        return Err(EstablishFailure::Timeout);
    }
    let mut stream = DeadlineStream::new(stream, deadline);
    match client.establish(&mut stream) {
        Ok(session) => match stream.into_inner() {
            Ok(stream) => Ok((session, stream)),
            Err(error) => Err(EstablishFailure::Tls(ControlError::Io(error))),
        },
        Err(error) => {
            if deadline_expired(&error, deadline) {
                Err(EstablishFailure::Timeout)
            } else {
                Err(EstablishFailure::Tls(error))
            }
        }
    }
}

fn deadline_expired(error: &ControlError, deadline: Instant) -> bool {
    match error {
        ControlError::Io(error) => {
            matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) || Instant::now() >= deadline
        }
        _ => false,
    }
}

/// Blocking stream wrapper whose every read and write is bounded by the
/// absolute establishment deadline.
struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn new(stream: UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    fn remaining(&self) -> io::Result<Duration> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(deadline_error());
        }
        // A sub-microsecond remainder rounds to zero in `timeval` and is
        // rejected by std; one microsecond still fails promptly.
        Ok((self.deadline - now).max(Duration::from_micros(1)))
    }

    fn into_inner(self) -> io::Result<UnixStream> {
        self.stream.set_read_timeout(None)?;
        self.stream.set_write_timeout(None)?;
        Ok(self.stream)
    }
}

fn deadline_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "control establishment deadline expired",
    )
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.stream.set_read_timeout(Some(remaining))?;
        match self.stream.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(deadline_error())
            }
            result => result,
        }
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.stream.set_write_timeout(Some(remaining))?;
        match self.stream.write(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(deadline_error())
            }
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bootstrap_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(io::Error::from)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bootstrap_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = nix::unistd::pipe().map_err(errno_to_io)?;
    set_cloexec(&read)?;
    set_cloexec(&write)?;
    Ok((read, write))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_cloexec(fd: &OwnedFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(errno_to_io)?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn errno_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
