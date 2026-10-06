//! macOS/iOS owned-tree backend: process groups plus `kqueue`.
//!
//! Adoption registers `EVFILT_PROC` with `NOTE_EXIT | NOTE_EXITSTATUS` for
//! the leader on a private kqueue. The event fires when the leader exits and
//! carries its wait status, but it does not reap: the leader stays a zombie,
//! so its pid — the process group id — stays pinned until the caller reaps it
//! inside `OwnedTree::retire`.

use std::io;
use std::sync::{Mutex, PoisonError};

use nix::errno::Errno;
use nix::libc;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, getpgrp};

use super::{LeaderExit, TreeBackend, TreeLifetime, TreeSignal};

pub(super) const BACKEND: TreeBackend = TreeBackend::ProcessGroupKqueue;

/// Low seven bits of a wait status: the terminating signal, or 0 for a
/// normal exit (`WIFEXITED`/`WTERMSIG`).
const WAIT_SIGNAL_MASK: i32 = 0x7f;

/// Low-bits value marking a stopped (not terminated) child (`WIFSTOPPED`).
const WAIT_STOPPED: i32 = 0x7f;

/// Shift and mask of the exit code in a wait status (`WEXITSTATUS`).
const WAIT_CODE_SHIFT: i32 = 8;
const WAIT_CODE_MASK: i32 = 0xff;

/// Non-reaping exit observer for one leader.
pub(super) struct Observer {
    queue: Kqueue,
    /// The first observed exit; the one-shot event never fires twice.
    exited: Mutex<Option<LeaderExit>>,
}

impl Observer {
    /// Arms the observer. The [`TreeLifetime`] is a mechanism no-op here:
    /// process groups outlive their parent however they were adopted.
    pub(super) fn arm(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        let _ = lifetime;
        let ident = usize::try_from(leader)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
        let queue = Kqueue::new().map_err(io::Error::from)?;
        let exit = KEvent::new(
            ident,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            0,
            0,
        );
        let exited = match queue.kevent(&[exit], &mut [], None) {
            Ok(_) => None,
            // The leader already exited (it is our unreaped zombie): the
            // exit is known, only its status waits for the reap.
            Err(Errno::ESRCH) => Some(LeaderExit::StatusUnavailable),
            Err(errno) => return Err(io::Error::from(errno)),
        };
        Ok(Self {
            queue,
            exited: Mutex::new(exited),
        })
    }

    pub(super) fn leader_exit(&self, _leader: u32) -> io::Result<Option<LeaderExit>> {
        let mut exited = self.exited.lock().unwrap_or_else(PoisonError::into_inner);
        if exited.is_some() {
            return Ok(*exited);
        }
        let mut events = [KEvent::new(
            0,
            EventFilter::EVFILT_PROC,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        let poll_now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ready = self
            .queue
            .kevent(&[], &mut events, Some(poll_now))
            .map_err(io::Error::from)?;
        if ready == 0 {
            return Ok(None);
        }
        let status = i32::try_from(events[0].data()).ok();
        *exited = Some(status.map_or(LeaderExit::StatusUnavailable, decode_wait_status));
        Ok(*exited)
    }

    /// Signals the leader's whole process group (its pid is the group id).
    pub(super) fn signal_tree(&self, leader: u32, signal: TreeSignal) -> io::Result<()> {
        super::refuse_reserved_group(leader)?;
        signal_group(leader, signal)
    }
}

/// Decodes a raw wait status (the BSD `WIFEXITED`/`WTERMSIG` layout).
fn decode_wait_status(status: i32) -> LeaderExit {
    let low = status & WAIT_SIGNAL_MASK;
    if low == 0 {
        LeaderExit::Exited((status >> WAIT_CODE_SHIFT) & WAIT_CODE_MASK)
    } else if low == WAIT_STOPPED {
        LeaderExit::StatusUnavailable
    } else {
        LeaderExit::Signaled(low)
    }
}

pub(super) fn signal_group(pgid: u32, signal: TreeSignal) -> io::Result<()> {
    let signal = match signal {
        TreeSignal::Interrupt => Signal::SIGINT,
        TreeSignal::Terminate => Signal::SIGTERM,
        TreeSignal::Kill => Signal::SIGKILL,
    };
    let pgid = i32::try_from(pgid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    let target = Pid::from_raw(pgid);
    killpg(target, signal).map_err(|errno| {
        // Darwin quirk: `killpg` returns `EPERM` when a process group contains
        // only zombie processes (the leader exited and waits to be reaped).
        // If signal-0 to the leader succeeds, the caller has permission and
        // the group has no live members left, satisfying the `NotFound` contract.
        if errno == Errno::ESRCH
            || (errno == Errno::EPERM && matches!(kill(target, None), Ok(()) | Err(Errno::ESRCH)))
        {
            io::Error::new(io::ErrorKind::NotFound, "process group has no member")
        } else {
            io::Error::from(errno)
        }
    })
}

pub(super) fn is_own_group(pgid: u32) -> bool {
    i32::try_from(pgid).is_ok_and(|pgid| getpgrp().as_raw() == pgid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_statuses_decode_like_the_libc_macros() {
        assert_eq!(decode_wait_status(0), LeaderExit::Exited(0));
        assert_eq!(decode_wait_status(3 << 8), LeaderExit::Exited(3));
        assert_eq!(decode_wait_status(9), LeaderExit::Signaled(9));
        // Core-dump flag (0x80) does not change the signal.
        assert_eq!(decode_wait_status(0x80 | 11), LeaderExit::Signaled(11));
        assert_eq!(
            decode_wait_status((19 << 8) | 0x7f),
            LeaderExit::StatusUnavailable
        );
    }
}
