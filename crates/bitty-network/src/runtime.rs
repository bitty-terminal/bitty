//! Runtime ownership shell: the future executor home (no runtime yet).
//!
//! Sealed: sockets and any executor arrive in a follow-up task. This module
//! performs no I/O, spawns no background tasks, and owns no threads; the
//! marker below reserves the shape follow-ups will fill.

/// Marker reserving the runtime-owner shape (no executor yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Runtime {
    _private: (),
}
