//! Unix launch-handle adapter for the provider-publication launch seam.
//!
//! Production unsafe code is confined to the `Command::pre_exec`
//! registration that places the inherited child-side descriptors at their
//! fixed launch numbers after `fork(2)` and before `execve(2)`. The hook
//! performs raw descriptor arithmetic only: no allocation, no locks, no std
//! I/O, and no access to parent memory beyond the copied descriptor numbers.
//! A test-only `close_test_descriptor` helper is also `#[allow(unsafe_code)]`
//! under `cfg(test, target_os = "linux")`; it never ships.
//!
//! The bootstrap channel is an anonymous pipe; the control channel is a
//! `socketpair(2)` (via `UnixStream::pair`) so both directions of the
//! dedicated control stream are independent of stdio.
//!
//! # Spawn-time descriptor reservation
//!
//! `std` allocates its close-on-exec exec-error channel inside
//! `Command::spawn`, at the lowest free descriptors, and only then forks and
//! runs the registered `pre_exec` hook. If any free descriptor at or below
//! the placement range is available, the error channel can occupy it, and a
//! sibling error-channel descriptor can land on
//! [`CHILD_BOOTSTRAP_DESCRIPTOR`] or [`CHILD_CONTROL_DESCRIPTOR`]; placement
//! would then `dup2` over it and a failed `exec` could be reported to the
//! parent as a successful spawn.
//!
//! [`Handles::install_child_descriptors`] therefore opens close-on-exec
//! placeholder pipes until every descriptor from `0` through the highest
//! placement target is occupied. A `pipe(2)` always returns the two lowest
//! free descriptors, so a pair that still contains a descriptor at or below
//! the placement range proves a free descriptor remained; the first pair
//! entirely above the range proves the whole range was occupied when it was
//! allocated. std's error channel is then allocated above the placement
//! range while the descriptors occupied here or by another owner stay open.
//! The placeholders are held across `Command::spawn`, are never read or
//! written, carry `FD_CLOEXEC` so they cannot survive `exec`, and are closed
//! by [`Handles::close_child_ends`], by [`Handles`] drop, or by
//! [`super::PreparedLaunch::abort`]. See `SpawnReservation`.
//!
//! Residual race: a placement descriptor already occupied by another owner
//! at reservation time cannot be claimed without closing that owner's
//! descriptor. std cannot allocate its error channel there while it stays
//! open, but a concurrent close of that descriptor between reservation and
//! spawn would reopen the window. The launch seam assumes its caller does not
//! close low descriptors concurrently, which single-threaded launch setup
//! satisfies.

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

/// Highest descriptor that placement overwrites. The whole `0..=` range must
/// be occupied while std allocates its exec-error channel.
const HIGHEST_PLACEMENT_DESCRIPTOR: RawFd = CHILD_CONTROL_DESCRIPTOR as RawFd;

// The range logic assumes the bootstrap target is below the control target.
const _: () = assert!(CHILD_BOOTSTRAP_DESCRIPTOR <= CHILD_CONTROL_DESCRIPTOR);

/// Parent-side close-on-exec placeholders held across `Command::spawn`.
///
/// The pipes are never read or written. Their only purpose is to occupy every
/// descriptor at or below the highest placement target so std's exec-error
/// channel is allocated above the placement range while those descriptors
/// stay open. Every end carries `FD_CLOEXEC`, so none survive `exec`; dropping
/// the value closes them.
struct SpawnReservation {
    // Leading underscore keeps the unused-descriptor lint quiet: the field
    // exists for its drop side effect, not for I/O.
    _placeholders: Vec<(OwnedFd, OwnedFd)>,
}

impl SpawnReservation {
    /// Fill every descriptor at or below the placement range.
    ///
    /// `pipe(2)` allocates the two lowest free descriptors. Each pair that
    /// contains a descriptor at or below [`HIGHEST_PLACEMENT_DESCRIPTOR`]
    /// proves the range was not yet fully occupied, so another pair is
    /// allocated. The first pair entirely above the range proves no free
    /// descriptor remained at or below it; keeping that pair too means the
    /// reservation always owns at least one pipe. std's exec-error channel is
    /// then allocated above every placement target, provided descriptors
    /// occupied by another owner stay open through the spawn.
    fn reserve() -> io::Result<Self> {
        let mut placeholders = Vec::new();
        loop {
            let pair = cloexec_pipe()?;
            let covers_range = pair.0.as_raw_fd() <= HIGHEST_PLACEMENT_DESCRIPTOR
                || pair.1.as_raw_fd() <= HIGHEST_PLACEMENT_DESCRIPTOR;
            placeholders.push(pair);
            if !covers_range {
                return Ok(Self {
                    _placeholders: placeholders,
                });
            }
        }
    }

    #[cfg(test)]
    fn descriptors(&self) -> Vec<RawFd> {
        self._placeholders
            .iter()
            .flat_map(|(read, write)| [read.as_raw_fd(), write.as_raw_fd()])
            .collect()
    }
}

/// Parent- and child-side handles for one prepared launch.
pub(super) struct Handles {
    bootstrap_writer: Option<File>,
    bootstrap_reader: Option<OwnedFd>,
    control_parent: Option<UnixStream>,
    control_child: Option<UnixStream>,
    spawn_reservation: Option<SpawnReservation>,
}

impl Handles {
    pub(super) fn create() -> Result<Self, LaunchError> {
        let (bootstrap_reader, bootstrap_writer) = cloexec_pipe().map_err(LaunchError::Channel)?;
        let (control_parent, control_child) = UnixStream::pair().map_err(LaunchError::Channel)?;
        Ok(Self {
            bootstrap_writer: Some(File::from(bootstrap_writer)),
            bootstrap_reader: Some(bootstrap_reader),
            control_parent: Some(control_parent),
            control_child: Some(control_child),
            spawn_reservation: None,
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

    /// Close the parent's child-side copies and release the spawn-time
    /// descriptor reservation. Idempotent; also runs on drop.
    pub(super) fn close_child_ends(&mut self) {
        self.bootstrap_reader = None;
        self.control_child = None;
        self.spawn_reservation = None;
    }

    /// Test-only view of the live spawn reservation descriptors.
    #[cfg(test)]
    pub(super) fn spawn_reservation_descriptors(&self) -> Vec<RawFd> {
        self.spawn_reservation
            .as_ref()
            .map(SpawnReservation::descriptors)
            .unwrap_or_default()
    }

    /// Test-only raw number of the parent bootstrap read end.
    #[cfg(test)]
    pub(super) fn bootstrap_reader_descriptor(&self) -> Option<RawFd> {
        self.bootstrap_reader.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Test-only raw number of the child-side control stream.
    #[cfg(test)]
    pub(super) fn control_child_descriptor(&self) -> Option<RawFd> {
        self.control_child.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Reserve the placement targets and register the post-fork hook.
    ///
    /// The reservation must exist before `Command::spawn` allocates its
    /// exec-error channel; it stays held until [`Self::close_child_ends`] or
    /// drop. Registering twice keeps the existing reservation and appends a
    /// second placement hook.
    ///
    /// # Safety
    ///
    /// `Command::pre_exec` is unsafe because the closure runs in a forked
    /// child of a possibly multithreaded process. The closure created here
    /// captures only two `RawFd` integers and calls
    /// [`place_child_descriptors`], which touches only async-signal-safe
    /// descriptor syscalls (`fcntl`, `dup2`, `close`) and performs no
    /// allocation, locking, or std I/O. A returned error aborts the spawn, so
    /// descriptor placement can never silently fail open. The parent-side
    /// reservation fills the placement range across the spawn, so std's
    /// close-on-exec exec-error channel is allocated above it unless another
    /// owner closes a range descriptor in the window (module-level residual
    /// race).
    #[allow(unsafe_code)]
    pub(super) fn install_child_descriptors(
        &mut self,
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
        if self.spawn_reservation.is_none() {
            self.spawn_reservation =
                Some(SpawnReservation::reserve().map_err(LaunchError::SpawnReservation)?);
        }
        // SAFETY: see the method contract above. Both descriptors stay open
        // in the parent until `close_child_ends` runs after the spawn attempt,
        // so the captured numbers remain valid through `fork`.
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

/// Create a close-on-exec pipe at the lowest free descriptors.
fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(io::Error::from)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (read, write) = nix::unistd::pipe().map_err(errno_to_io)?;
        set_cloexec(&read)?;
        set_cloexec(&write)?;
        Ok((read, write))
    }
}

/// Test-only: close one descriptor owned by the test process.
///
/// The low-stdio regression needs to free descriptors after launch
/// preparation, which safe std APIs cannot express. This is the only added
/// unsafe operation and exists solely in the audited Unix adapter under
/// `cfg(test)`.
#[cfg(all(test, target_os = "linux"))]
#[allow(unsafe_code)]
pub(super) fn close_test_descriptor(fd: RawFd) -> io::Result<()> {
    // SAFETY: the caller passes a descriptor the test process owns and no
    // other owner remains; `close` neither allocates nor touches other fds.
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
