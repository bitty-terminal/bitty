//! Protocol shell: HTTP/WS markers (no wire code yet).
//!
//! Sealed: framing and wire behavior arrive in a follow-up task behind the
//! `http`/`websocket` features. This module performs no I/O; the markers
//! below reserve the protocols follow-ups will implement.

/// Marker reserving the HTTP protocol shape (no wire code yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Http {
    _private: (),
}

/// Marker reserving the WebSocket protocol shape (no wire code yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WebSocket {
    _private: (),
}
