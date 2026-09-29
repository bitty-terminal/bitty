//! Linux/Android owned-tree backend: process groups plus pidfd.
//!
//! The leader's exit is observed with `waitid(WEXITED | WNOHANG | WNOWAIT)`
//! on a pidfd opened at adoption. `WNOWAIT` leaves the leader a zombie, so
//! its pid — the process group id — stays pinned until the caller reaps it
//! inside `OwnedTree::retire`. Kernels without pidfd (before 5.3, or a
//! seccomp policy that denies it) fall back to a pid-scoped `waitid`, which
//! is equally exact here: the leader is an unreaped child of this process,
//! so its pid cannot be recycled before this process reaps it.

use std::io;
use std::os::fd::{AsFd as _, OwnedFd};

use rustix::io::Errno;
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitId, WaitIdOptions, getpgrp, kill_process_group, pidfd_open, waitid,
};

use super::{LeaderExit, TreeBackend, TreeSignal};

pub(super) const BACKEND: TreeBackend = TreeBackend::ProcessGroupPidfd;

/// Non-reaping exit observer for one leader.
pub(super) struct Observer {
    /// `None` when the kernel offers no pidfd (pid-scoped fallback).
    pidfd: Option<OwnedFd>,
}

impl Observer {
    pub(super) fn arm(leader: u32) -> io::Result<Self> {
        let pid = to_pid(leader)?;
        // Any pidfd failure keeps the exact pid-scoped fallback: this process
        // is the leader's parent and alone decides when it is reaped.
        let pidfd = pidfd_open(pid, PidfdFlags::empty()).ok();
        Ok(Self { pidfd })
    }

    pub(super) fn leader_exit(&self, leader: u32) -> io::Result<Option<LeaderExit>> {
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        let status = match &self.pidfd {
            Some(pidfd) => waitid(WaitId::PidFd(pidfd.as_fd()), options),
            None => waitid(WaitId::Pid(to_pid(leader)?), options),
        }
        .map_err(io::Error::from)?;
        Ok(status.map(|status| {
            if let Some(code) = status.exit_status() {
                LeaderExit::Exited(code)
            } else if let Some(signal) = status.terminating_signal() {
                LeaderExit::Signaled(signal)
            } else {
                LeaderExit::StatusUnavailable
            }
        }))
    }
}

pub(super) fn signal_group(pgid: u32, signal: TreeSignal) -> io::Result<()> {
    let signal = match signal {
        TreeSignal::Interrupt => Signal::INT,
        TreeSignal::Terminate => Signal::TERM,
        TreeSignal::Kill => Signal::KILL,
    };
    kill_process_group(to_pid(pgid)?, signal).map_err(|errno| {
        // `ESRCH` has no std kind of its own; the tree contract names it.
        if errno == Errno::SRCH {
            io::Error::new(io::ErrorKind::NotFound, "process group has no member")
        } else {
            io::Error::from(errno)
        }
    })
}

pub(super) fn is_own_group(pgid: u32) -> bool {
    i32::try_from(pgid).is_ok_and(|pgid| getpgrp().as_raw_nonzero().get() == pgid)
}

fn to_pid(raw: u32) -> io::Result<Pid> {
    i32::try_from(raw)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))
}
