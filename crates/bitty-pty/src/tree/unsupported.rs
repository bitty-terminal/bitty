//! No owned-tree backend on this platform (BSDs other than macOS, and any
//! other target without a backend module).
//!
//! Adoption fails with [`io::ErrorKind::Unsupported`], so callers keep
//! direct-child semantics and report [`TreeBackend::Unsupported`] instead of
//! claiming tree cleanup.

use std::io;

use super::{LeaderExit, TreeBackend, TreeLifetime, TreeSignal};

pub(super) const BACKEND: TreeBackend = TreeBackend::Unsupported;

/// Never constructed: [`Observer::arm`] always fails.
pub(super) struct Observer;

impl Observer {
    pub(super) fn arm(leader: u32, lifetime: TreeLifetime) -> io::Result<Self> {
        let _ = (leader, lifetime);
        Err(unsupported())
    }

    pub(super) fn leader_exit(&self, leader: u32) -> io::Result<Option<LeaderExit>> {
        let _ = leader;
        Err(unsupported())
    }

    pub(super) fn signal_tree(&self, leader: u32, signal: TreeSignal) -> io::Result<()> {
        let _ = (leader, signal);
        Err(unsupported())
    }
}

pub(super) fn signal_group(pgid: u32, signal: TreeSignal) -> io::Result<()> {
    let _ = (pgid, signal);
    Err(unsupported())
}

pub(super) fn is_own_group(pgid: u32) -> bool {
    let _ = pgid;
    false
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "no owned-process-tree backend on this platform",
    )
}
