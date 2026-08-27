//! OSC 52 clipboard presentation (bounded, gated, headless-testable).
//!
//! Compatibility status per `compatibility-milestone-rfc.md`:
//! - OSC 52 **write** is "gated opt-in" (requires user permission).
//! - OSC 52 **read/query** is "out of M1": denies even when configured.
//!
//! This module never touches the platform clipboard. It captures bounded
//! `OSC 52` write requests as inert events and denies reads
//! deterministically, so the same PTY byte stream always yields the same
//! observable clipboard outcome. The eventual permission/policy channel
//! (RFC replay guarantee 6: policy decisions enter via explicit environment
//! inputs) is intentionally not implemented here — this is the headless
//! bookkeeping seam that policy will later gate.

use bitty_vt::{BoundedBytes, ClipboardOp, TerminalAction};

/// Maximum clipboard payload bytes retained per request (bounded per T-01).
///
/// Matches [`BoundedBytes::MAX_LEN`] so parser truncation and store
/// truncation are consistent: deterministic prefix on overflow.
pub const CLIPBOARD_MAX_PAYLOAD_BYTES: usize = BoundedBytes::MAX_LEN;

/// Maximum clipboard requests retained in history (bounded FIFO).
pub const CLIPBOARD_MAX_HISTORY: usize = 16;

/// Outcome of handling an `OSC 52` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardOutcome {
    /// A write request was captured (bounded). Delivery to the platform
    /// clipboard is not performed here; a future policy gate will decide
    /// whether to forward it.
    WriteCaptured {
        /// Truncated payload as delivered by the parser (already bounded).
        data: BoundedBytes,
    },
    /// A read/query request was denied (M1: reads are always denied).
    ReadDenied,
    /// The payload was not an OSC 52 clipboard action (no-op).
    Ignored,
}

/// Whether the embedder would allow clipboard writes.
///
/// This draft exposes the type so callers can thread a decision without
/// baking a default-allow path. The crate itself always returns
/// `WriteCaptured` regardless of policy; the caller decides whether to
/// forward to the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClipboardPolicy {
    /// Prompt or pre-granted capability required (M1 expectation). Callers
    /// should require explicit consent before forwarding.
    #[default]
    Gated,
    /// Writes are denied outright.
    Denied,
    /// Writes are allowed without prompt (not recommended; kept for tests
    /// and for a future pre-granted capability token).
    Allow,
}

/// One bounded clipboard request remembered in history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRequest {
    /// Which operation was requested.
    pub op: ClipboardOp,
    /// Payload as bounded by the parser (length ≤ 4096).
    pub data: BoundedBytes,
    /// Monotonic ordinal assigned at capture time (deterministic).
    pub ordinal: u64,
}

/// Bounded, deterministic clipboard state (headless).
///
/// Oldest request is dropped when at capacity (FIFO), mirroring the
/// terminal-state reply and zone policies (bounded memory per T-01).
#[derive(Debug, Clone)]
pub struct ClipboardState {
    history: Vec<ClipboardRequest>,
    next_ordinal: u64,
    pub(crate) denied_reads: u64,
    pub(crate) captured_writes: u64,
}

impl Default for ClipboardState {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardState {
    /// An empty clipboard state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            next_ordinal: 1,
            denied_reads: 0,
            captured_writes: 0,
        }
    }

    /// Handles one [`TerminalAction`] as a clipboard event; returns the
    /// outcome and records bounded history for writes.
    ///
    /// Deterministic: same action sequence yields same history and same
    /// counters on every platform.
    pub fn handle_action(&mut self, action: &TerminalAction) -> ClipboardOutcome {
        match action {
            TerminalAction::OscClipboard { op, data } => match op {
                ClipboardOp::Write => {
                    let req = ClipboardRequest {
                        op: *op,
                        data: data.clone(),
                        ordinal: self.next_ordinal,
                    };
                    self.next_ordinal = self.next_ordinal.wrapping_add(1).max(1);
                    if self.history.len() >= CLIPBOARD_MAX_HISTORY {
                        self.history.remove(0);
                    }
                    self.history.push(req.clone());
                    self.captured_writes = self.captured_writes.wrapping_add(1);
                    ClipboardOutcome::WriteCaptured { data: req.data }
                }
                ClipboardOp::Read => {
                    self.denied_reads = self.denied_reads.wrapping_add(1);
                    // Reads are denied in M1 regardless of payload: no data
                    // enters the history queue and no platform query occurs.
                    ClipboardOutcome::ReadDenied
                }
            },
            _ => ClipboardOutcome::Ignored,
        }
    }

    /// Retained write-request history, oldest first (bounded).
    #[must_use]
    pub fn history(&self) -> &[ClipboardRequest] {
        &self.history
    }

    /// Number of write requests captured since creation or clear.
    #[must_use]
    pub fn captured_writes(&self) -> u64 {
        self.captured_writes
    }

    /// Number of read/query requests denied since creation or clear.
    #[must_use]
    pub fn denied_reads(&self) -> u64 {
        self.denied_reads
    }

    /// Whether any write is currently retained.
    #[must_use]
    pub fn has_pending_write(&self) -> bool {
        !self.history.is_empty()
    }

    /// The most recent write request, if any.
    #[must_use]
    pub fn last_write(&self) -> Option<&ClipboardRequest> {
        self.history.last()
    }

    /// Clears history and resets counters (test helper; not triggered by
    /// terminal actions in this slice).
    pub fn clear(&mut self) {
        self.history.clear();
        self.captured_writes = 0;
        self.denied_reads = 0;
        self.next_ordinal = 1;
    }

    /// Number of retained requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Whether no request is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_vt::{BoundedBytes, TerminalAction};

    fn write(data: &[u8]) -> TerminalAction {
        TerminalAction::OscClipboard {
            op: ClipboardOp::Write,
            data: BoundedBytes::new(data.to_vec()),
        }
    }

    fn read() -> TerminalAction {
        TerminalAction::OscClipboard {
            op: ClipboardOp::Read,
            data: BoundedBytes::new(b"?".to_vec()),
        }
    }

    #[test]
    fn write_is_captured_bounded() {
        let mut state = ClipboardState::new();
        let outcome = state.handle_action(&write(b"hello"));
        assert!(matches!(outcome, ClipboardOutcome::WriteCaptured { .. }));
        assert_eq!(state.len(), 1);
        assert_eq!(state.last_write().unwrap().data.as_bytes(), b"hello");
        assert_eq!(state.captured_writes(), 1);
        assert_eq!(state.denied_reads(), 0);
    }

    #[test]
    fn read_is_denied_not_stored() {
        let mut state = ClipboardState::new();
        let outcome = state.handle_action(&read());
        assert_eq!(outcome, ClipboardOutcome::ReadDenied);
        assert!(state.is_empty());
        assert_eq!(state.denied_reads(), 1);
        assert_eq!(state.captured_writes(), 0);
    }

    #[test]
    fn non_clipboard_is_ignored() {
        let mut state = ClipboardState::new();
        let outcome = state.handle_action(&TerminalAction::FullReset);
        assert_eq!(outcome, ClipboardOutcome::Ignored);
        assert!(state.is_empty());
    }

    #[test]
    fn payload_truncation_is_deterministic() {
        let mut a = ClipboardState::new();
        let mut b = ClipboardState::new();
        let long = vec![0xAB_u8; CLIPBOARD_MAX_PAYLOAD_BYTES + 50];
        let action = write(&long);
        let out_a = a.handle_action(&action);
        let out_b = b.handle_action(&action);
        assert_eq!(out_a, out_b);
        // Stored data must be truncated to cap.
        if let ClipboardOutcome::WriteCaptured { data } = out_a {
            assert_eq!(data.len(), CLIPBOARD_MAX_PAYLOAD_BYTES);
        } else {
            panic!("expected write");
        }
    }

    #[test]
    fn history_is_bounded_fifo() {
        let mut state = ClipboardState::new();
        for i in 0..CLIPBOARD_MAX_HISTORY + 5 {
            state.handle_action(&write(&[i as u8]));
        }
        assert_eq!(state.len(), CLIPBOARD_MAX_HISTORY);
        // Oldest 5 evicted, so first retained ordinal is 6.
        assert_eq!(state.history().first().unwrap().ordinal, 6);
        assert_eq!(
            state.history().last().unwrap().ordinal,
            (CLIPBOARD_MAX_HISTORY + 5) as u64
        );
    }

    #[test]
    fn deterministic_ordinals() {
        let mut a = ClipboardState::new();
        let mut b = ClipboardState::new();
        a.handle_action(&write(b"one"));
        b.handle_action(&write(b"one"));
        a.handle_action(&write(b"two"));
        b.handle_action(&write(b"two"));
        assert_eq!(a.history(), b.history());
        assert_eq!(a.captured_writes(), b.captured_writes());
    }

    #[test]
    fn clear_resets() {
        let mut state = ClipboardState::new();
        state.handle_action(&write(b"x"));
        state.handle_action(&read());
        state.clear();
        assert!(state.is_empty());
        assert_eq!(state.captured_writes(), 0);
        assert_eq!(state.denied_reads(), 0);
    }
}
