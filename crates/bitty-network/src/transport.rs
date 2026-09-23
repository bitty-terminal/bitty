//! Transport shell: TCP/UDP/QUIC markers (no sockets yet).
//!
//! Sealed: real transports arrive in a follow-up task. This module performs
//! no I/O and opens no sockets; the markers below reserve the variants
//! follow-ups will implement behind the `client`/`server`/`quic` features.

/// Marker reserving the TCP transport shape (no sockets yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Tcp {
    _private: (),
}

/// Marker reserving the UDP transport shape (no sockets yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Udp {
    _private: (),
}

/// Marker reserving the QUIC transport shape (no sockets yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Quic {
    _private: (),
}
