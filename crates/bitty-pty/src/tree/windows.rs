//! Windows owned-tree backend: Job Objects (CTX-0903, DEC-0083; CTX-0978,
//! DEC-0101).
//!
//! Adoption creates an anonymous Job Object through the reviewed
//! `bitty-winjob` adapter and assigns the leader to it: kill-on-close for
//! [`TreeLifetime::Owned`] trees, without the limit for
//! [`TreeLifetime::Detached`] ones (DEC-0102). Every process the leader creates afterwards joins the same job, and the job
//! never sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so `CREATE_BREAKAWAY_FROM_JOB`
//! cannot take a descendant out: the job, not a process group, is the
//! owned tree.
//!
//! - **PTY children** ([`crate::Pty`]) are born inside their job: the
//!   platform spawn assembles the tree at creation through
//!   `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child runs zero instructions
//!   outside it and there is no adopt-after-start window.
//! - **Prepared children** ([`super::OwnedTree::prepare_command`]) start
//!   with `CREATE_SUSPENDED`; [`Observer::arm_prepared`] assigns them before
//!   their first instruction and then resumes them on every path, so no
//!   descendant can escape and no child is ever left suspended.
//! - **Already-running non-PTY children** ([`super::OwnedTree::adopt`]) are
//!   assigned after the spawn: correct only because the caller spawns them
//!   suspended or joins them before they can fork. Never adopt a PTY child
//!   here; its tree travels with the [`crate::Pty`] handle.
//!
//! The leader's exit is observed through the process handle kept from the
//! assignment (`WaitForSingleObject` with a zero timeout plus
//! `GetExitCodeProcess`). That never reaps anything: Windows has no zombie
//! state, and the open handle keeps the process object — and so its pid —
//! from being reused until the tree is dropped.
//!
//! Windows has no signals. [`TreeSignal::Kill`] terminates the whole job;
//! [`TreeSignal::Interrupt`] and [`TreeSignal::Terminate`] fail with
//! [`io::ErrorKind::Unsupported`] (console control events reach a whole
//! console, not a job, and are not a tree-scoped mechanism). Nothing ever
//! falls back to a single-pid kill.

use std::io;

use bitty_winjob::{JobMember, JobObject};

use super::{LeaderExit, TreeBackend, TreeLifetime, TreeSignal};

pub(super) const BACKEND: TreeBackend = TreeBackend::JobObject;

/// Exit code every member gets from a tree kill: the code
/// `std::process::Child::kill` and `portable-pty` use on Windows, so a
/// killed leader reports the same status whichever path ended it.
const TREE_KILL_EXIT_CODE: u32 = 1;

/// Exit code for a prepared child that could not be resumed and therefore
/// is terminated instead of being left suspended.
const UNRESUMABLE_EXIT_CODE: u32 = 1;

/// The job that owns the tree plus the leader's observation handle.
///
/// An [`TreeLifetime::Owned`] job is kill-on-close: dropping it closes the
/// job's only handle, which kills whatever is still in the job
/// (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
///
/// # Lifetime (DEC-0102, #1580)
///
/// Detached-lifetime and service jobs must outlive the bitty process, like
/// Unix process groups do, so they adopt with [`TreeLifetime::Detached`]:
/// the job carries no kill-on-close limit and dropping it leaves its
/// members running as plain OS orphans (Phase 2 reconciliation reports them
/// `Unknown` until reaped or adopted). An explicit [`TreeSignal::Kill`]
/// still ends either flavour through `TerminateJobObject`. No supervisor
/// handoff exists in v0.1 to hold the handle instead, and accepting the
/// divergence would contradict the declared `Detached`
/// continue-under-the-supervisor semantics.
pub(super) struct Observer {
    job: JobObject,
    leader: JobMember,
}

impl Observer {
    /// Wraps a job the spawn already placed the leader in (the ConPTY
    /// at-creation path). No assignment, no window.
    pub(super) fn from_spawned(job: JobObject, leader: JobMember) -> Self {
        Self { job, leader }
    }

    /// Assigns an already-running non-PTY `leader` to a new job with the
    /// given [`TreeLifetime`]. Never pass a `CREATE_SUSPENDED` child here:
    /// nothing would resume it. Never pass a PTY child either: its tree
    /// travels with the `Pty` handle.
    pub(super) fn arm(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        let job = match lifetime {
            TreeLifetime::Owned => JobObject::new()?,
            TreeLifetime::Detached => JobObject::new_detached()?,
        };
        let leader = job.assign_pid(leader)?;
        Ok(Self { job, leader })
    }

    /// Assigns a `CREATE_SUSPENDED` `leader` to a new job with the given
    /// [`TreeLifetime`], then resumes it.
    ///
    /// The resume runs on every path. When the assignment fails the child is
    /// still resumed and the assignment error returned, so the caller keeps
    /// direct-child semantics over a running child. When the resume fails
    /// the child can never run: it is terminated (through the job when one
    /// exists, else directly) and the resume error returned.
    pub(super) fn arm_prepared(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        let armed = Self::arm(leader, lifetime);
        if let Err(resume) = bitty_winjob::resume_suspended_process(leader) {
            match &armed {
                Ok(observer) => {
                    let _ = observer.job.terminate(UNRESUMABLE_EXIT_CODE);
                }
                Err(_) => {
                    // The caller still holds the unreaped `Child`, whose
                    // handle pins `leader` (the `terminate_process`
                    // precondition).
                    let _ = bitty_winjob::terminate_process(leader, UNRESUMABLE_EXIT_CODE);
                }
            }
            return Err(resume);
        }
        armed
    }

    /// Exit codes are `u32` on Windows; they are reinterpreted bit-for-bit as
    /// `i32`, exactly as `std::process::ExitStatus::code` does, so an
    /// `NTSTATUS` such as `0xC000013A` reads as the same negative number on
    /// both paths.
    pub(super) fn leader_exit(&self, _leader: u32) -> io::Result<Option<LeaderExit>> {
        Ok(self
            .leader
            .exit_code()?
            .map(|code| LeaderExit::Exited(i32::from_ne_bytes(code.to_ne_bytes()))))
    }

    pub(super) fn signal_tree(&self, _leader: u32, signal: TreeSignal) -> io::Result<()> {
        match signal {
            TreeSignal::Kill => {
                if self.job.active_processes()? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the job has no live member",
                    ));
                }
                self.job.terminate(TREE_KILL_EXIT_CODE)
            }
            TreeSignal::Interrupt | TreeSignal::Terminate => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Job Objects deliver no graceful stop: only a kill reaches the whole tree",
            )),
        }
    }
}

/// Windows has no process groups: another "group" of the tree cannot be
/// named, so this always refuses rather than guessing a target.
pub(super) fn signal_group(pgid: u32, signal: TreeSignal) -> io::Result<()> {
    let _ = (pgid, signal);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows has no process groups to signal",
    ))
}

pub(super) fn is_own_group(pgid: u32) -> bool {
    let _ = pgid;
    false
}
