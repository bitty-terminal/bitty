//! Event envelope, event kinds, and the attach idempotency key (contract
//! `D-10`, `D-18`).
//!
//! The envelope is internal to this crate for v0.2.0: it fixes the fields the
//! contract orders, routes, and deduplicates on, and leaves JSON framing, the
//! wire `v: 1` marker, and the server-generated per-connection correlation id
//! to the IPC slice (bitty#2607). No variant here carries PTY bytes, terminal
//! text, prompts, tool output, or provider payloads (`D-10`).
//!
//! Every field is fixed size, so [`Event::accounted_bytes`] is exact and the
//! backlog and lease-queue byte bounds are deterministic.

use std::fmt;

use crate::registry::{
    AttachId, CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs,
    SessionId, SessionSnapshot, TerminalReason,
};

/// Maximum bytes in a client-supplied attach idempotency key (`D-18`: a keyed
/// method carries a client-generated 128-bit key encoded in at most 128 bytes).
pub const ATTACH_KEY_MAX_BYTES: usize = 128;

/// A validated attach idempotency key.
///
/// The value is opaque to this crate: it is compared, stored, and dropped, and
/// never interpreted, echoed, or written to a log. [`fmt::Debug`] prints the
/// length only, so a key cannot leak through a tracing field.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachKey(Box<[u8]>);

impl AttachKey {
    /// Validate a client-supplied attach key.
    ///
    /// # Errors
    ///
    /// Returns [`AttachKeyError`] for an empty key or one above
    /// [`ATTACH_KEY_MAX_BYTES`]. The refusal never echoes the bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, AttachKeyError> {
        if bytes.is_empty() {
            return Err(AttachKeyError::Empty);
        }
        if bytes.len() > ATTACH_KEY_MAX_BYTES {
            return Err(AttachKeyError::TooLong { bytes: bytes.len() });
        }
        Ok(Self(bytes.into()))
    }

    /// The stored bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Stored length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the key is empty. Construction rejects empty keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Accounted store bytes: the key bytes plus the fixed owned-record cost.
    pub(crate) fn accounted_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.0.len()
    }
}

impl fmt::Debug for AttachKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachKey")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// A refused attach key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKeyError {
    /// Zero bytes.
    Empty,
    /// More than [`ATTACH_KEY_MAX_BYTES`] bytes.
    TooLong {
        /// Observed length; the value itself is not echoed.
        bytes: usize,
    },
}

impl fmt::Display for AttachKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("attach key is empty"),
            Self::TooLong { bytes } => {
                write!(formatter, "attach key is {bytes} bytes, above the bound")
            }
        }
    }
}

impl std::error::Error for AttachKeyError {}

/// The closed event-kind set (`D-10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    /// Lifecycle, attach, detach, claim, transfer, or revision change.
    SessionState,
    /// Terminal transition with state, reason, exit information, and cleanup.
    SessionTerminal,
    /// Controller moved from one lease to another.
    ControlTransfer,
    /// A lease was revoked by the controller or the operator.
    LeaseRevoked,
    /// Drain started; carries the deadline.
    DaemonStopping,
}

impl EventKind {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionState => "session_state",
            Self::SessionTerminal => "session_terminal",
            Self::ControlTransfer => "control_transfer",
            Self::LeaseRevoked => "lease_revoked",
            Self::DaemonStopping => "daemon_stopping",
        }
    }

    /// Whether the frame is a control frame (`D-10`).
    ///
    /// v0.2.0 defines no shed-able observability class, so every kind is a
    /// control frame and none is ever silently shed. The method exists so a
    /// future class must answer this question instead of assuming it.
    #[must_use]
    pub const fn is_control(self) -> bool {
        true
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Per-kind payload that is not part of the frozen envelope fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventDetail {
    /// No extra payload.
    #[default]
    None,
    /// The two leases a `control_transfer` moved control between (`D-09`).
    ControlTransfer {
        /// The lease that held control.
        from: AttachId,
        /// The lease that received control.
        to: AttachId,
    },
    /// The drain deadline carried by `daemon_stopping` (`D-05`).
    DaemonStopping {
        /// Server monotonic milliseconds after which the daemon stops.
        deadline_ms: u64,
    },
}

/// The terminal marker (`D-10`, `D-04`).
///
/// Present exactly on [`EventKind::SessionTerminal`], carrying the immutable
/// classification the terminal snapshot also reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalMarker {
    /// The immutable terminal state.
    pub state: LifecycleState,
    /// The immutable terminal reason.
    pub reason: TerminalReason,
    /// Exit information specific to the reason.
    pub exit: ExitInformation,
    /// Cleanup classification.
    pub cleanup: CleanupClass,
}

impl TerminalMarker {
    /// Read the marker from a terminal snapshot.
    ///
    /// Returns `None` for a non-terminal snapshot, so a caller cannot fabricate
    /// a terminal event for a live session.
    #[must_use]
    pub fn from_snapshot(snapshot: &SessionSnapshot) -> Option<Self> {
        if !snapshot.state.is_terminal() {
            return None;
        }
        Some(Self {
            state: snapshot.state,
            reason: snapshot.terminal_reason?,
            exit: snapshot.exit,
            cleanup: snapshot.cleanup?,
        })
    }
}

/// One event in a session's stream (`D-10`).
///
/// `attach_id` names the lease a copy is routed to. Backlog entries are
/// canonical and carry `None`; the stream materializes one copy per recipient
/// through [`Event::routed_to`]. Routing is per lease: an event for `attach_id`
/// is never broadcast and never queued on a detached lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The daemon instance that emitted the event (`D-02`).
    pub instance_id: DaemonInstanceId,
    /// The session the event belongs to.
    pub session_id: SessionId,
    /// The lease this copy is routed to, or `None` for a canonical entry.
    pub attach_id: Option<AttachId>,
    /// The session state revision the event describes (`D-03`).
    pub revision: u64,
    /// The event kind.
    pub kind: EventKind,
    /// The terminal marker, present exactly on [`EventKind::SessionTerminal`].
    pub terminal: Option<TerminalMarker>,
    /// Kind-specific payload.
    pub detail: EventDetail,
    /// Server monotonic emission time.
    pub emitted_at: MonotonicMs,
}

impl Event {
    /// Accounted bytes for one retained or queued copy.
    ///
    /// Every field is fixed size and the envelope owns no heap, so this is the
    /// exact cost and the unit of the byte bounds in `D-14`.
    #[must_use]
    pub const fn accounted_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    /// A copy routed to one lease.
    ///
    /// The envelope is shared and unchanged; only the routing field differs, so
    /// every per-lease copy of one revision carries identical content.
    #[must_use]
    pub fn routed_to(&self, attach_id: AttachId) -> Self {
        let mut copy = self.clone();
        copy.attach_id = Some(attach_id);
        copy
    }

    /// Whether this is the terminal transition event.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.kind, EventKind::SessionTerminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> TerminalMarker {
        TerminalMarker {
            state: LifecycleState::Exited,
            reason: TerminalReason::WorkerExit,
            exit: ExitInformation::ExitCode(0),
            cleanup: CleanupClass::Swept,
        }
    }

    #[test]
    fn event_kinds_use_the_contract_spelling_and_are_control_frames() {
        for (kind, spelling) in [
            (EventKind::SessionState, "session_state"),
            (EventKind::SessionTerminal, "session_terminal"),
            (EventKind::ControlTransfer, "control_transfer"),
            (EventKind::LeaseRevoked, "lease_revoked"),
            (EventKind::DaemonStopping, "daemon_stopping"),
        ] {
            assert_eq!(kind.as_str(), spelling);
            assert_eq!(kind.to_string(), spelling);
            assert!(kind.is_control());
        }
    }

    #[test]
    fn attach_keys_are_bounded_opaque_and_never_debugged() {
        assert_eq!(AttachKey::new(b""), Err(AttachKeyError::Empty));
        let key = AttachKey::new(b"caller-key-1").expect("bounded key");
        assert_eq!(key.as_bytes(), b"caller-key-1");
        assert_eq!(key.len(), 12);
        assert!(!key.is_empty());
        assert_eq!(format!("{key:?}"), "AttachKey { bytes: 12 }");
        assert!(!format!("{key:?}").contains("caller"));

        let oversized = vec![7u8; ATTACH_KEY_MAX_BYTES + 1];
        assert_eq!(
            AttachKey::new(&oversized),
            Err(AttachKeyError::TooLong {
                bytes: ATTACH_KEY_MAX_BYTES + 1
            })
        );
        assert!(AttachKey::new(&[7u8; ATTACH_KEY_MAX_BYTES]).is_ok());
    }

    #[test]
    fn terminal_marker_requires_a_terminal_snapshot() {
        let mut snapshot = SessionSnapshot {
            session_id: SessionId::parse("00000000000000000000000000000000").expect("canonical"),
            kind: crate::registry::SessionKind::Terminal,
            terminal_id: crate::registry::TerminalId::supplied("term").expect("valid"),
            state: LifecycleState::Running,
            generation: 1,
            state_revision: 1,
            attach_count: 0,
            attached: false,
            owner_peer: None,
            terminal_reason: None,
            exit: ExitInformation::NotApplicable,
            cleanup: None,
            created_at_ms: MonotonicMs::ZERO,
            last_seen_ms: MonotonicMs::ZERO,
        };
        assert_eq!(TerminalMarker::from_snapshot(&snapshot), None);

        snapshot.state = LifecycleState::Exited;
        assert_eq!(TerminalMarker::from_snapshot(&snapshot), None);
        snapshot.terminal_reason = Some(TerminalReason::WorkerExit);
        snapshot.exit = ExitInformation::ExitCode(0);
        snapshot.cleanup = Some(CleanupClass::Swept);
        assert_eq!(
            TerminalMarker::from_snapshot(&snapshot),
            Some(TerminalMarker {
                state: LifecycleState::Exited,
                reason: TerminalReason::WorkerExit,
                exit: ExitInformation::ExitCode(0),
                cleanup: CleanupClass::Swept,
            })
        );
        assert_eq!(
            marker(),
            TerminalMarker::from_snapshot(&snapshot).expect("terminal")
        );
    }
}
