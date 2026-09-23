//! DNS shell: resolution marker (no lookups yet).
//!
//! Sealed: name resolution arrives in a follow-up task. This module performs
//! no I/O and issues no DNS queries; the marker below reserves the shape
//! follow-ups will fill.

/// Marker reserving the DNS-resolver shape (no lookups yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Dns {
    _private: (),
}
