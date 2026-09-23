//! TLS shell: handshake/policy marker (no crypto yet).
//!
//! Sealed: TLS implementation arrives in a follow-up task. This module
//! performs no I/O, holds no certificates, and takes no crypto dependency;
//! the marker below reserves the shape follow-ups will fill.

/// Marker reserving the TLS shape (no crypto yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Tls {
    _private: (),
}
