//! Windows owned-tree backend: Job Objects (CTX-0903, DEC-0083).
//!
//! Adoption creates an anonymous kill-on-close Job Object through the
//! reviewed `bitty-winjob` adapter and assigns the leader to it. Every
//! process the leader creates afterwards joins the same job, and the job
//! never sets `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so `CREATE_BREAKAWAY_FROM_JOB`
//! cannot take a descendant out: the job, not a process group, is the
//! owned tree.
//!
//! - **Prepared children** ([`super::OwnedTree::prepare_command`]) start
//!   with `CREATE_SUSPENDED`; [`Observer::arm_prepared`] assigns them before
//!   their first instruction and then resumes them on every path, so no
//!   descendant can escape and no child is ever left suspended.
//! - **Already-running children** (the ConPTY child) are assigned after the
//!   spawn. Residual race: a descendant the child creates before the
//!   assignment lands is not in the job and survives a tree kill.
//!   `portable-pty` spawns ConPTY children itself with fixed creation
//!   flags, so `CREATE_SUSPENDED` cannot be requested there today.
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

use super::{LeaderExit, TreeBackend, TreeSignal};

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
/// Dropping it closes the job's only handle, which kills whatever is still
/// in the job (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
///
/// # Lifetime divergence from Unix (documented, not changed here)
///
/// The job handle is owned by this process. When this process exits or
/// crashes, the kernel closes the handle and kill-on-close terminates every
/// live job tree, **including jobs declared detached or service lifetime**.
/// Unix process groups outlive their parent, so the same jobs keep running
/// there. A Windows job therefore never outlives the process that adopted
/// it; whether detached jobs need a different mechanism is an open
/// decision tracked separately.
pub(super) struct Observer {
    job: JobObject,
    leader: JobMember,
}

impl Observer {
    /// Assigns an already-running `leader` to a new job. Never pass a
    /// `CREATE_SUSPENDED` child here: nothing would resume it.
    pub(super) fn arm(leader: u32) -> io::Result<Self> {
        let job = JobObject::new()?;
        let leader = job.assign_pid(leader)?;
        Ok(Self { job, leader })
    }

    /// Assigns a `CREATE_SUSPENDED` `leader` to a new job, then resumes it.
    ///
    /// The resume runs on every path. When the assignment fails the child is
    /// still resumed and the assignment error returned, so the caller keeps
    /// direct-child semantics over a running child. When the resume fails
    /// the child can never run: it is terminated (through the job when one
    /// exists, else directly) and the resume error returned.
    pub(super) fn arm_prepared(leader: u32) -> io::Result<Self> {
        let armed = Self::arm(leader);
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
