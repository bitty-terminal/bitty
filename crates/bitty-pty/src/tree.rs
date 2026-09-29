//! Owned process trees: signal and kill every member of a job, never a
//! single pid (CTX-0512).
//!
//! A job's **owned tree** is the process group its leader heads. The leader
//! is a child of this process that has not been reaped yet: either a
//! `std::process::Command` child prepared with
//! [`OwnedTree::prepare_command`] (it leads a new process group), or a PTY
//! child (a session leader, so it already leads its own group). The
//! platform split lives here so callers never branch on `target_os`:
//!
//! | Platform      | Backend                         | Leader exit observed by              |
//! | ------------- | ------------------------------- | ------------------------------------ |
//! | Linux/Android | [`TreeBackend::ProcessGroupPidfd`] | `waitid(P_PIDFD, WNOWAIT)` (pid-scoped `waitid` when the kernel has no pidfd) |
//! | macOS/iOS     | [`TreeBackend::ProcessGroupKqueue`] | `kqueue` `EVFILT_PROC` / `NOTE_EXIT` |
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
//! # Gaps (documented, not hidden)
//!
//! - A member that moves itself into another group or session (`setsid`,
//!   `setpgid`) leaves the tree; only cgroups (Linux) or Job Objects
//!   (Windows) would keep it, and neither is implemented here.
//! - Windows has no backend yet: the Job Object mechanism needs a reviewed
//!   FFI boundary this `unsafe`-free crate does not carry.
//!
//! Nothing here uses `unsafe`: `rustix` (Linux) and `nix` (macOS) own the
//! system-call wrappers.

#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "tree/linux.rs"]
mod imp;

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "tree/macos.rs"]
mod imp;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
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
    /// Polite stop request (`SIGINT`).
    Interrupt,
    /// Graceful termination request (`SIGTERM`).
    Terminate,
    /// Unconditional kill (`SIGKILL`).
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

/// How a tree's leader ended, observed without reaping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderExit {
    /// The leader exited with this status code.
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
/// Signals go to the leader's whole process group. After
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
    /// Makes the child of `command` lead a new process group, so its
    /// descendants form an owned tree [`OwnedTree::adopt`] can take over.
    /// A no-op where no Unix process groups exist.
    pub fn prepare_command(command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    /// Takes over the tree led by `leader`.
    ///
    /// `leader` must be a child of this process that is not reaped yet and
    /// leads its own process group (a [`OwnedTree::prepare_command`] child
    /// or a PTY child). Call this right after the spawn, before anything
    /// else may reap the child.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Unsupported`] where no backend exists (see
    /// [`TreeBackend::Unsupported`]), [`io::ErrorKind::InvalidInput`] for
    /// pid 0 or a pid outside the platform range, and the observer's
    /// arming error otherwise.
    pub fn adopt(leader: u32) -> io::Result<Self> {
        if leader == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a tree leader pid must be non-zero",
            ));
        }
        Ok(Self {
            leader,
            retired: Mutex::new(false),
            observer: imp::Observer::arm(leader)?,
        })
    }

    /// Pid of the tree leader (also the process group id).
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

    /// Delivers `signal` to every member of the leader's process group.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] when the group has no member
    /// left or the tree is retired, [`io::ErrorKind::PermissionDenied`] when
    /// the kernel refuses the signal, and the system error otherwise.
    pub fn signal(&self, signal: TreeSignal) -> io::Result<()> {
        self.signal_group(self.leader, signal)
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
    /// for a refused group id.
    pub fn signal_group(&self, pgid: u32, signal: TreeSignal) -> io::Result<()> {
        let retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        if *retired {
            return Err(retired_error());
        }
        if pgid <= 1 || imp::is_own_group(pgid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to signal process group 0, 1, or this process's own group",
            ));
        }
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
        assert_eq!(TreeBackend::Unsupported.as_str(), "unsupported");
        assert_eq!(TreeSignal::Interrupt.as_str(), "interrupt");
        assert_eq!(TreeSignal::Terminate.as_str(), "terminate");
        assert_eq!(TreeSignal::Kill.as_str(), "kill");
    }

    #[test]
    fn detect_matches_the_compiling_platform() {
        let backend = TreeBackend::detect();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(backend, TreeBackend::ProcessGroupPidfd);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        assert_eq!(backend, TreeBackend::ProcessGroupKqueue);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
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
    }
}
