//! Owned process trees: signal and kill every member of a job, never a
//! single pid (CTX-0512).
//!
//! A job's **owned tree** is the process group its leader heads (Unix) or
//! the Job Object it was assigned to (Windows). The leader is a child of
//! this process that has not been reaped yet: either a
//! `std::process::Command` child prepared with
//! [`OwnedTree::prepare_command`] and adopted with
//! [`OwnedTree::adopt_prepared`] (it leads a new process group, or starts
//! suspended until it joins its job), or a PTY child travelling with the
//! tree assembled at its spawn (see [`crate::Pty::tree`]: a session leader
//! on Unix, so it already leads its own group; a ConPTY child born inside
//! its job on Windows). The
//! platform split lives here so callers never branch on `target_os`:
//!
//! | Platform      | Backend                         | Leader exit observed by              |
//! | ------------- | ------------------------------- | ------------------------------------ |
//! | Linux/Android | [`TreeBackend::ProcessGroupPidfd`] | `waitid(P_PIDFD, WNOWAIT)` (pid-scoped `waitid` when the kernel has no pidfd) |
//! | macOS/iOS     | [`TreeBackend::ProcessGroupKqueue`] | `kqueue` `EVFILT_PROC` / `NOTE_EXIT` |
//! | Windows       | [`TreeBackend::JobObject`]      | the leader's process handle (`WaitForSingleObject`, zero timeout) |
//! | elsewhere     | [`TreeBackend::Unsupported`]    | nothing: [`OwnedTree::adopt`] fails and callers keep direct-child semantics |
//!
//! # Why exit is observed without reaping
//!
//! A process group id is a pid. While the leader is unreaped (alive or a
//! zombie) its pid — and so the group id — cannot be reused, so signalling
//! the group always reaches this job's members and never an unrelated
//! group. The observers above report the leader's exit **without** reaping
//! it; the caller then kills what is left of the group and only afterwards
//! reaps the leader inside [`OwnedTree::retire`], which also makes every
//! later signal fail with [`std::io::ErrorKind::NotFound`] instead of
//! reaching a recycled id.
//!
//! On Windows the leader's open process handle plays the same role: the
//! process object, and so its pid, outlives the exit until the tree is
//! dropped, and there is no zombie to reap.
//!
//! # Windows signal semantics
//!
//! Windows has no signals. [`TreeSignal::Kill`] terminates the whole Job
//! Object; [`TreeSignal::Interrupt`] and [`TreeSignal::Terminate`] fail with
//! [`io::ErrorKind::Unsupported`], and [`OwnedTree::signal_group`] (there
//! are no process groups) always fails with
//! [`io::ErrorKind::Unsupported`]. None of them falls back to a single pid.
//!
//! # Gaps (documented, not hidden)
//!
//! - Unix: a member that moves itself into another group or session
//!   (`setsid`, `setpgid`) leaves the tree; only cgroups (Linux) would keep
//!   it, and they are not used for the tree here.
//! - Windows: a kill-on-close Job Object's only handle belongs to this
//!   process, so an [`TreeLifetime::Owned`] tree dies with it. Detached and
//!   service jobs instead adopt with [`TreeLifetime::Detached`], whose job
//!   carries no kill-on-close limit and outlives this process exactly like
//!   Unix process groups do (DEC-0102, #1580); an explicit tree kill still
//!   ends either flavour. On Unix the lifetime is a mechanism no-op
//!   (process groups always outlive their parent); the supervisor's
//!   bookkeeping, not the mechanism, differs there.
//!
//! # Pairing rule
//!
//! [`OwnedTree::prepare_command`] pairs with [`OwnedTree::adopt_prepared`]
//! and nothing else. [`OwnedTree::adopt`] is only for children that are
//! already running and were not spawned through [`crate::PtyBuilder`]: PTY
//! children travel with their tree (see [`crate::Pty::tree`]), which on
//! Windows is assembled at creation with no adopt-after-start window. On
//! Windows a prepared child handed to `adopt` is never resumed and stays
//! suspended forever.
//!
//! Nothing here uses `unsafe`: `rustix` (Linux), `nix` (macOS), and the
//! first-party `bitty-winjob` adapter (Windows) own the system-call
//! wrappers.

#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "tree/linux.rs"]
mod imp;

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "tree/macos.rs"]
mod imp;

#[cfg(windows)]
#[path = "tree/windows.rs"]
mod imp;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
#[path = "tree/unsupported.rs"]
mod imp;

use std::fmt;
use std::io;
use std::process::Command;
use std::sync::{Mutex, PoisonError};

/// Mechanism that terminates an owned process tree on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeBackend {
    /// Linux/Android: process groups plus pidfd exit observation.
    ProcessGroupPidfd,
    /// macOS/iOS: process groups plus `kqueue` exit observation.
    ProcessGroupKqueue,
    /// Windows: a kill-on-close Job Object plus process-handle exit
    /// observation. Only [`TreeSignal::Kill`] is deliverable.
    JobObject,
    /// No owned-tree backend: only the direct child can be terminated, and
    /// callers must surface that gap instead of claiming tree cleanup.
    Unsupported,
}

impl TreeBackend {
    /// Backend for the compiling platform.
    #[must_use]
    pub const fn detect() -> Self {
        imp::BACKEND
    }

    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessGroupPidfd => "process_group_pidfd",
            Self::ProcessGroupKqueue => "process_group_kqueue",
            Self::JobObject => "job_object",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether this backend terminates the whole owned tree.
    #[must_use]
    pub const fn kills_owned_tree(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

impl fmt::Display for TreeBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Portable signal intent delivered to every member of an owned tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeSignal {
    /// Polite stop request (`SIGINT`; unsupported on Windows).
    Interrupt,
    /// Graceful termination request (`SIGTERM`; unsupported on Windows).
    Terminate,
    /// Unconditional kill (`SIGKILL`; `TerminateJobObject` on Windows).
    Kill,
}

impl TreeSignal {
    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::Kill => "kill",
        }
    }
}

/// How long a tree lives relative to this process (DEC-0102, #1580).
///
/// Only the Windows backend reads it: [`TreeLifetime::Owned`] selects a
/// kill-on-close Job Object, [`TreeLifetime::Detached`] one without the
/// limit. On Unix it is accepted and ignored — process groups outlive
/// their parent however they were adopted — so callers pass the same value
/// on every platform and never branch on `target_os`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TreeLifetime {
    /// The tree dies with this process (kill-on-close on Windows).
    #[default]
    Owned,
    /// The tree outlives this process (no kill-on-close limit on Windows).
    /// Detached-lifetime and service jobs adopt with this.
    Detached,
}

impl TreeLifetime {
    /// Stable lowercase wire/display name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Detached => "detached",
        }
    }
}

impl fmt::Display for TreeLifetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a tree's leader ended, observed without reaping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderExit {
    /// The leader exited with this status code (on Windows the `u32` exit
    /// code reinterpreted bit-for-bit, as `std::process::ExitStatus::code`
    /// does).
    Exited(i32),
    /// The leader was terminated by this signal number.
    Signaled(i32),
    /// The leader is gone but the observer could not read its status (for
    /// example it had exited before observation was armed); the reap still
    /// reports it.
    StatusUnavailable,
}

/// A job's owned process tree, headed by an unreaped child of this process.
///
/// Signals go to the leader's whole process group (Unix) or Job Object
/// (Windows). After
/// [`OwnedTree::retire`] every signal fails with
/// [`io::ErrorKind::NotFound`]: the leader was reaped, so its id may be
/// recycled and must never be signalled again.
pub struct OwnedTree {
    leader: u32,
    /// `true` once the leader was reaped. Held while signalling so a
    /// concurrent reap can never slip between the check and the signal.
    retired: Mutex<bool>,
    observer: imp::Observer,
}

impl fmt::Debug for OwnedTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedTree")
            .field("leader", &self.leader)
            .field("backend", &TreeBackend::detect())
            .field("retired", &self.is_retired())
            .finish()
    }
}

impl OwnedTree {
    /// Prepares `command` so its child can head an owned tree that
    /// [`OwnedTree::adopt_prepared`] takes over.
    ///
    /// - Unix: the child leads a new process group.
    /// - Windows: the child starts suspended (`CREATE_SUSPENDED`; this
    ///   replaces any creation flags set earlier on `command`, and none are
    ///   set anywhere in this workspace) so it joins its Job Object before
    ///   it runs. A prepared child **must** go through
    ///   [`OwnedTree::adopt_prepared`] right after the spawn, which resumes
    ///   it on every path.
    /// - Elsewhere: a no-op.
    ///
    /// **Pair this only with [`OwnedTree::adopt_prepared`].** Handing the
    /// child to [`OwnedTree::adopt`] instead leaves it suspended forever on
    /// Windows (see the module's pairing rule).
    pub fn prepare_command(command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(bitty_winjob::CREATE_SUSPENDED_FLAG);
        }
        #[cfg(not(any(unix, windows)))]
        let _ = command;
    }

    /// Takes over the tree led by `leader`, a child spawned from a command
    /// prepared with [`OwnedTree::prepare_command`].
    ///
    /// On Unix this is [`OwnedTree::adopt`]. On Windows it assigns the
    /// still-suspended child to a new Job Object and then resumes it. The
    /// resume happens on **every** path: when the assignment fails the
    /// child is resumed anyway and the error returned (callers keep
    /// direct-child semantics over a running child); when the resume itself
    /// fails the child is terminated, since it could never run, and the
    /// resume error returned.
    ///
    /// # Errors
    ///
    /// As [`OwnedTree::adopt`], plus the resume error on Windows.
    pub fn adopt_prepared(leader: u32) -> io::Result<Self> {
        Self::adopt_prepared_with_lifetime(leader, TreeLifetime::Owned)
    }

    /// Takes over the tree led by `leader`, a child spawned from a command
    /// prepared with [`OwnedTree::prepare_command`], with the given
    /// [`TreeLifetime`].
    ///
    /// Detached-lifetime and service jobs adopt with
    /// [`TreeLifetime::Detached`] so the tree outlives this process on
    /// Windows (DEC-0102); every other lifetime uses
    /// [`TreeLifetime::Owned`].
    ///
    /// # Errors
    ///
    /// As [`OwnedTree::adopt_with_lifetime`], plus the resume error on
    /// Windows.
    pub fn adopt_prepared_with_lifetime(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        #[cfg(windows)]
        {
            if leader == 0 {
                return Err(zero_leader());
            }
            Ok(Self {
                leader,
                retired: Mutex::new(false),
                observer: imp::Observer::arm_prepared(leader, lifetime)?,
            })
        }
        #[cfg(not(windows))]
        Self::adopt_with_lifetime(leader, lifetime)
    }

    /// Assembles the tree for a ConPTY child born inside `job` at creation
    /// (Windows only): `member` observes the leader `pid`, which the spawn
    /// placed in the job atomically. There is no adopt-after-start window.
    #[cfg(windows)]
    pub(crate) fn adopt_spawned(
        pid: u32,
        job: bitty_winjob::JobObject,
        member: bitty_winjob::JobMember,
    ) -> Self {
        Self {
            leader: pid,
            retired: Mutex::new(false),
            observer: imp::Observer::from_spawned(job, member),
        }
    }

    /// Takes over the tree led by `leader`.
    ///
    /// `leader` must be a child of this process that is not reaped yet and
    /// leads its own process group and is already running (a non-PTY child
    /// such as a helper spawned outside [`crate::PtyBuilder`]). PTY children
    /// travel with their tree (see [`crate::Pty::tree`]) and must not be
    /// adopted here. Call this right after the spawn, before anything else
    /// may reap the child.
    ///
    /// # Warning: already-running children only
    ///
    /// Never call this for a child spawned from an
    /// [`OwnedTree::prepare_command`] command: on Windows that child was
    /// created suspended and `adopt` never resumes it, so it would hang
    /// forever. Use [`OwnedTree::adopt_prepared`] for those on every
    /// platform.
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Unsupported`] where no backend exists (see
    /// [`TreeBackend::Unsupported`]), [`io::ErrorKind::InvalidInput`] for
    /// pid 0 or a pid outside the platform range, and the observer's
    /// arming error otherwise.
    pub fn adopt(leader: u32) -> io::Result<Self> {
        Self::adopt_with_lifetime(leader, TreeLifetime::Owned)
    }

    /// Takes over the tree led by `leader` with the given [`TreeLifetime`].
    ///
    /// Same contract as [`OwnedTree::adopt`], except a
    /// [`TreeLifetime::Detached`] tree outlives this process on Windows
    /// instead of dying with it (DEC-0102). Detached-lifetime and service
    /// jobs adopt with `Detached`; every other lifetime uses `Owned`.
    ///
    /// # Errors
    ///
    /// As [`OwnedTree::adopt`].
    pub fn adopt_with_lifetime(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        if leader == 0 {
            return Err(zero_leader());
        }
        Ok(Self {
            leader,
            retired: Mutex::new(false),
            observer: imp::Observer::arm(leader, lifetime)?,
        })
    }

    /// Pid of the tree leader (also the process group id on Unix).
    #[must_use]
    pub fn leader(&self) -> u32 {
        self.leader
    }

    /// Backend this tree runs on.
    #[must_use]
    pub fn backend(&self) -> TreeBackend {
        TreeBackend::detect()
    }

    /// Whether the leader was reaped through [`OwnedTree::retire`].
    #[must_use]
    pub fn is_retired(&self) -> bool {
        *self.retired.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reports the leader's exit without reaping it: `None` while it runs.
    ///
    /// # Errors
    ///
    /// Returns the observer's error; after [`OwnedTree::retire`] it returns
    /// [`io::ErrorKind::NotFound`].
    pub fn leader_exit(&self) -> io::Result<Option<LeaderExit>> {
        if self.is_retired() {
            return Err(retired_error());
        }
        self.observer.leader_exit(self.leader)
    }

    /// Delivers `signal` to every member of the tree: the leader's process
    /// group on Unix, the Job Object on Windows.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] when the tree has no member
    /// left or is retired, [`io::ErrorKind::PermissionDenied`] when the
    /// kernel refuses the signal, [`io::ErrorKind::Unsupported`] for a
    /// graceful signal on Windows, and the system error otherwise.
    pub fn signal(&self, signal: TreeSignal) -> io::Result<()> {
        let retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        if *retired {
            return Err(retired_error());
        }
        self.observer.signal_tree(self.leader, signal)
    }

    /// Delivers `signal` to another process group inside this tree's
    /// session, such as a PTY's foreground job.
    ///
    /// The group must belong to the job: callers pass the foreground group
    /// of the job's own PTY. This process's own group, and group ids 0 and
    /// 1, are always refused.
    ///
    /// # Errors
    ///
    /// Same as [`OwnedTree::signal`], plus [`io::ErrorKind::InvalidInput`]
    /// for a refused group id. On Windows (no process groups) every
    /// non-refused id fails with [`io::ErrorKind::Unsupported`].
    pub fn signal_group(&self, pgid: u32, signal: TreeSignal) -> io::Result<()> {
        let retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        if *retired {
            return Err(retired_error());
        }
        refuse_reserved_group(pgid)?;
        imp::signal_group(pgid, signal)
    }

    /// Reaps the leader through `reap` and retires the tree in the same
    /// critical section, so no signal can reach the recycled id.
    ///
    /// Callers kill the remaining members first (the leader's unreaped pid
    /// still pins the group) and then retire.
    pub fn retire<T>(&self, reap: impl FnOnce() -> T) -> T {
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        let reaped = reap();
        *retired = true;
        reaped
    }
}

/// Refuses group ids 0 and 1 and this process's own group.
fn refuse_reserved_group(pgid: u32) -> io::Result<()> {
    if pgid <= 1 || imp::is_own_group(pgid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal process group 0, 1, or this process's own group",
        ));
    }
    Ok(())
}

fn zero_leader() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "a tree leader pid must be non-zero",
    )
}

fn retired_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "the tree leader was reaped; its process group may be recycled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        assert_eq!(
            TreeBackend::ProcessGroupPidfd.as_str(),
            "process_group_pidfd"
        );
        assert_eq!(
            TreeBackend::ProcessGroupKqueue.as_str(),
            "process_group_kqueue"
        );
        assert_eq!(TreeBackend::JobObject.as_str(), "job_object");
        assert_eq!(TreeBackend::Unsupported.as_str(), "unsupported");
        assert_eq!(TreeSignal::Interrupt.as_str(), "interrupt");
        assert_eq!(TreeSignal::Terminate.as_str(), "terminate");
        assert_eq!(TreeSignal::Kill.as_str(), "kill");
        assert_eq!(TreeLifetime::Owned.as_str(), "owned");
        assert_eq!(TreeLifetime::Detached.as_str(), "detached");
    }

    #[test]
    fn owned_is_the_default_lifetime() {
        assert_eq!(TreeLifetime::default(), TreeLifetime::Owned);
    }

    #[test]
    fn detect_matches_the_compiling_platform() {
        let backend = TreeBackend::detect();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(backend, TreeBackend::ProcessGroupPidfd);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        assert_eq!(backend, TreeBackend::ProcessGroupKqueue);
        #[cfg(windows)]
        assert_eq!(backend, TreeBackend::JobObject);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            windows
        )))]
        assert_eq!(backend, TreeBackend::Unsupported);
        assert_eq!(
            backend.kills_owned_tree(),
            backend != TreeBackend::Unsupported
        );
    }

    #[test]
    fn pid_zero_is_never_a_leader() {
        let error = OwnedTree::adopt(0).expect_err("pid 0 is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = OwnedTree::adopt_prepared(0).expect_err("pid 0 is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
