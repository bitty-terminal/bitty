//! OSC 52 clipboard presentation (bounded, gated, headless-testable).
//!
//! Compatibility status per `compatibility-milestone-rfc.md`:
//! - OSC 52 **write** is "gated opt-in" (requires user permission).
//! - OSC 52 **read/query** is "out of M1": denied even when configured.
//!
//! This module never touches the platform clipboard. It captures bounded
//! `OSC 52` write requests as inert events and denies reads by default, so
//! the same PTY byte stream always yields the same observable clipboard
//! outcome. The eventual permission/policy channel (RFC replay guarantee 6:
//! policy decisions enter via explicit environment inputs) is intentionally
//! not implemented here — this is the headless bookkeeping seam that policy
//! gates.
//!
//! # One-time read grants (CTX-0213)
//!
//! A read can be authorized exactly once via a one-time-password grant
//! (Ghostty OTP pattern): the embedder mints a token with
//! [`ClipboardState::grant_read`] after out-of-band user consent, hands it
//! to the requesting context, and the context redeems it with
//! [`ClipboardState::handle_action_with_token`]. Redemption consumes the
//! grant atomically — replaying the same token is denied — and a grant is
//! only redeemable from the [`ClipboardGrantScope`] it was minted for.
//! Without a live, in-scope token every read is [`ClipboardOutcome::ReadDenied`]: default-deny is
//! preserved and outstanding grants never weaken the token-less path.
//!
//! A granted read returns the most recently captured write payload (the
//! headless model of clipboard contents), or an empty payload when no write
//! has been captured yet. Grants never expose the platform clipboard.

use bitty_vt::{BoundedBytes, ClipboardOp, TerminalAction};

/// Maximum clipboard payload bytes retained per request (bounded per T-01).
///
/// Matches [`BoundedBytes::MAX_LEN`] so parser truncation and store
/// truncation are consistent: deterministic prefix on overflow.
pub const CLIPBOARD_MAX_PAYLOAD_BYTES: usize = BoundedBytes::MAX_LEN;

/// Maximum clipboard requests retained in history (bounded FIFO).
pub const CLIPBOARD_MAX_HISTORY: usize = 16;

/// Raw entropy bytes per one-time read token (256 bits).
pub const CLIPBOARD_READ_TOKEN_LEN: usize = 32;

/// Maximum outstanding (unredeemed) read grants (bounded FIFO per T-01).
///
/// Minting beyond this evicts the oldest grant, which becomes unredeemable:
/// a later presentation of an evicted token is
/// [`ClipboardOutcome::ReadDenied`].
pub const CLIPBOARD_MAX_OUTSTANDING_GRANTS: usize = 16;

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
    /// A read/query request was denied: no live, in-scope one-time token
    /// was presented (M1 default-deny, preserved by CTX-0213).
    ReadDenied,
    /// A read/query request was authorized by consuming a single-use token.
    ReadGranted {
        /// Most recently captured write payload (empty when no write has
        /// been captured yet). Never platform clipboard contents.
        data: BoundedBytes,
    },
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

/// Granting context a one-time read token is scoped to.
///
/// The value is opaque to this crate: the embedder assigns one identity per
/// requesting context (for example a pane or client id) at grant time and
/// must present the same identity at redemption time. A token presented
/// from any other scope is denied and left unconsumed, so a leaked token
/// cannot be replayed across contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClipboardGrantScope(pub u64);

/// Single-use token authorizing exactly one clipboard read.
///
/// Minted by [`ClipboardState::grant_read`] from 256 bits of OS entropy and
/// redeemed by [`ClipboardState::handle_action_with_token`]. The token is
/// opaque: equality with a stored grant is checked in constant time and the
/// raw bytes are never exposed via [`Debug`] (redacted) or any accessor.
#[derive(Clone)]
pub struct ClipboardReadToken {
    bytes: [u8; CLIPBOARD_READ_TOKEN_LEN],
    scope: ClipboardGrantScope,
}

impl std::fmt::Debug for ClipboardReadToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardReadToken")
            .field("scope", &self.scope)
            .field("bytes", &"[redacted]")
            .finish()
    }
}

impl PartialEq for ClipboardReadToken {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope && constant_time_eq(&self.bytes, &other.bytes)
    }
}

impl Eq for ClipboardReadToken {}

/// Constant-time byte equality so token comparison does not leak the stored
/// grant prefix length via timing.
fn constant_time_eq(
    a: &[u8; CLIPBOARD_READ_TOKEN_LEN],
    b: &[u8; CLIPBOARD_READ_TOKEN_LEN],
) -> bool {
    let mut diff = 0u8;
    for i in 0..CLIPBOARD_READ_TOKEN_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// One outstanding (unredeemed) read grant.
///
/// Raw token bytes are redacted from [`Debug`] so grant entropy never leaks
/// through state dumps or logs.
#[derive(Clone)]
struct OutstandingGrant {
    token: [u8; CLIPBOARD_READ_TOKEN_LEN],
    scope: ClipboardGrantScope,
}

impl std::fmt::Debug for OutstandingGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutstandingGrant")
            .field("scope", &self.scope)
            .field("token", &"[redacted]")
            .finish()
    }
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
///
/// [`Debug`] redacts outstanding grant entropy: it reports only the grant
/// count, never token bytes, so `format!("{:?}", state)` cannot bypass the
/// [`ClipboardReadToken`] redaction.
#[derive(Clone)]
pub struct ClipboardState {
    history: Vec<ClipboardRequest>,
    grants: Vec<OutstandingGrant>,
    next_ordinal: u64,
    pub(crate) denied_reads: u64,
    pub(crate) captured_writes: u64,
}

impl std::fmt::Debug for ClipboardState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardState")
            .field("history", &self.history)
            .field("outstanding_grants", &self.grants.len())
            .field("next_ordinal", &self.next_ordinal)
            .field("denied_reads", &self.denied_reads)
            .field("captured_writes", &self.captured_writes)
            .finish()
    }
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
            grants: Vec::new(),
            next_ordinal: 1,
            denied_reads: 0,
            captured_writes: 0,
        }
    }

    /// Mints a single-use read token for `scope` from OS entropy.
    ///
    /// The embedder calls this only after out-of-band user consent (the
    /// consent UI lives outside this headless seam). The token is stored
    /// until redeemed or evicted; minting beyond
    /// [`CLIPBOARD_MAX_OUTSTANDING_GRANTS`] evicts the oldest grant.
    ///
    /// Fail-closed: returns `None` when the OS RNG fails, so reads stay
    /// denied instead of falling back to predictable tokens.
    pub fn grant_read(&mut self, scope: ClipboardGrantScope) -> Option<ClipboardReadToken> {
        let mut bytes = [0u8; CLIPBOARD_READ_TOKEN_LEN];
        getrandom::fill(&mut bytes).ok()?;
        if self.grants.len() >= CLIPBOARD_MAX_OUTSTANDING_GRANTS {
            self.grants.remove(0);
        }
        self.grants.push(OutstandingGrant {
            token: bytes,
            scope,
        });
        Some(ClipboardReadToken { bytes, scope })
    }

    /// Handles one [`TerminalAction`] as a clipboard event; returns the
    /// outcome and records bounded history for writes.
    ///
    /// Token-less path: writes are captured, reads are always
    /// [`ClipboardOutcome::ReadDenied`], and outstanding grants are neither
    /// consulted nor consumed. Deterministic: same action sequence yields
    /// same history and same counters on every platform.
    pub fn handle_action(&mut self, action: &TerminalAction) -> ClipboardOutcome {
        // Scope is unchecked on the token-less path: without a token every
        // read is denied regardless of scope.
        self.handle_action_with_token(action, None, ClipboardGrantScope(0))
    }

    /// Handles one [`TerminalAction`] with an optional one-time read token.
    ///
    /// - Writes ignore the token and are captured as in [`handle_action`].
    /// - Reads with `Some(token)` whose bytes match a live grant **and**
    ///   whose `scope` equals the grant's scope consume that grant
    ///   atomically (removed before the outcome is built, so no replay)
    ///   and return [`ClipboardOutcome::ReadGranted`].
    /// - Reads with no token, an unknown token, an evicted token, or a
    ///   scope mismatch return [`ClipboardOutcome::ReadDenied`] and leave
    ///   all grants unconsumed.
    /// - Non-clipboard actions return [`ClipboardOutcome::Ignored`] without
    ///   touching grants or counters.
    pub fn handle_action_with_token(
        &mut self,
        action: &TerminalAction,
        token: Option<&ClipboardReadToken>,
        scope: ClipboardGrantScope,
    ) -> ClipboardOutcome {
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
                ClipboardOp::Read => match token {
                    Some(candidate)
                        if candidate.scope == scope && self.redeem(candidate, scope).is_some() =>
                    {
                        let data = self
                            .last_write()
                            .map(|req| req.data.clone())
                            .unwrap_or_else(|| BoundedBytes::new(Vec::new()));
                        ClipboardOutcome::ReadGranted { data }
                    }
                    _ => {
                        self.denied_reads = self.denied_reads.wrapping_add(1);
                        // Reads are denied in M1 regardless of payload: no data
                        // enters the history queue and no platform query occurs.
                        ClipboardOutcome::ReadDenied
                    }
                },
            },
            _ => ClipboardOutcome::Ignored,
        }
    }

    /// Removes the live grant matching `candidate` (constant-time bytes,
    /// exact scope) and returns it. Failed matches leave grants untouched.
    fn redeem(
        &mut self,
        candidate: &ClipboardReadToken,
        scope: ClipboardGrantScope,
    ) -> Option<OutstandingGrant> {
        let pos = self.grants.iter().position(|grant| {
            grant.scope == scope
                && grant.scope == candidate.scope
                && constant_time_eq(&grant.token, &candidate.bytes)
        })?;
        Some(self.grants.remove(pos))
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

    /// Number of live (unredeemed, unevicted) read grants.
    #[must_use]
    pub fn outstanding_grants(&self) -> usize {
        self.grants.len()
    }

    /// Clears history and resets counters (test helper; not triggered by
    /// terminal actions in this slice).
    ///
    /// Outstanding grants are revoked as well: tokens minted before the
    /// clear are denied afterwards.
    pub fn clear(&mut self) {
        self.history.clear();
        self.grants.clear();
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

    fn grant(state: &mut ClipboardState, scope: u64) -> ClipboardReadToken {
        state
            .grant_read(ClipboardGrantScope(scope))
            .expect("OS entropy must be available in tests")
    }

    #[test]
    fn grant_allows_single_read_then_replay_is_denied() {
        let mut state = ClipboardState::new();
        state.handle_action(&write(b"secret"));
        let token = grant(&mut state, 7);
        assert_eq!(state.outstanding_grants(), 1);

        let outcome = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(7));
        assert_eq!(
            outcome,
            ClipboardOutcome::ReadGranted {
                data: BoundedBytes::new(b"secret".to_vec())
            }
        );
        // Grant was consumed atomically: nothing outstanding remains.
        assert_eq!(state.outstanding_grants(), 0);
        assert_eq!(state.denied_reads(), 0);

        // Replay of the same token is denied and counted.
        let replay = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(7));
        assert_eq!(replay, ClipboardOutcome::ReadDenied);
        assert_eq!(state.denied_reads(), 1);
        // A denied read stores nothing.
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn granted_read_without_prior_write_returns_empty() {
        let mut state = ClipboardState::new();
        let token = grant(&mut state, 1);
        let outcome = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(1));
        assert_eq!(
            outcome,
            ClipboardOutcome::ReadGranted {
                data: BoundedBytes::new(Vec::new())
            }
        );
    }

    #[test]
    fn forged_token_is_denied_and_keeps_live_grant() {
        let mut state = ClipboardState::new();
        state.handle_action(&write(b"data"));
        let token = grant(&mut state, 3);
        // A token minted by another state (different entropy) is unknown here.
        let mut other = ClipboardState::new();
        let forged = grant(&mut other, 3);

        let denied = state.handle_action_with_token(&read(), Some(&forged), ClipboardGrantScope(3));
        assert_eq!(denied, ClipboardOutcome::ReadDenied);
        assert_eq!(state.denied_reads(), 1);
        // Failed redemption leaves the live grant unconsumed.
        assert_eq!(state.outstanding_grants(), 1);
        let ok = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(3));
        assert!(matches!(ok, ClipboardOutcome::ReadGranted { .. }));
    }

    #[test]
    fn cross_scope_reuse_is_denied() {
        let mut state = ClipboardState::new();
        state.handle_action(&write(b"scoped"));
        let token = grant(&mut state, 11);

        // Same token bytes, wrong presenting scope.
        let denied = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(12));
        assert_eq!(denied, ClipboardOutcome::ReadDenied);
        assert_eq!(state.outstanding_grants(), 1);

        // Original scope still redeems afterwards.
        let ok = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(11));
        assert!(matches!(ok, ClipboardOutcome::ReadGranted { .. }));
        assert_eq!(state.outstanding_grants(), 0);
    }

    #[test]
    fn tokenless_path_stays_denied_with_outstanding_grant() {
        let mut state = ClipboardState::new();
        state.handle_action(&write(b"data"));
        let token = grant(&mut state, 5);

        // Default-deny is preserved: the plain path never consults grants.
        let denied = state.handle_action(&read());
        assert_eq!(denied, ClipboardOutcome::ReadDenied);
        assert_eq!(state.denied_reads(), 1);
        assert_eq!(state.outstanding_grants(), 1);

        // Explicit `None` token behaves identically.
        let denied_none = state.handle_action_with_token(&read(), None, ClipboardGrantScope(5));
        assert_eq!(denied_none, ClipboardOutcome::ReadDenied);
        assert_eq!(state.outstanding_grants(), 1);

        // The grant itself is still redeemable.
        let ok = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(5));
        assert!(matches!(ok, ClipboardOutcome::ReadGranted { .. }));
    }

    #[test]
    fn non_clipboard_with_token_is_ignored_without_consuming() {
        let mut state = ClipboardState::new();
        let token = grant(&mut state, 9);
        let outcome = state.handle_action_with_token(
            &TerminalAction::FullReset,
            Some(&token),
            ClipboardGrantScope(9),
        );
        assert_eq!(outcome, ClipboardOutcome::Ignored);
        assert_eq!(state.outstanding_grants(), 1);
        assert_eq!(state.denied_reads(), 0);
    }

    #[test]
    fn minted_tokens_are_unique() {
        let mut state = ClipboardState::new();
        let a = grant(&mut state, 1);
        let b = grant(&mut state, 1);
        assert_ne!(a, b);
        assert_eq!(state.outstanding_grants(), 2);
    }

    #[test]
    fn token_debug_redacts_bytes() {
        let mut state = ClipboardState::new();
        let token = grant(&mut state, 42);
        let rendered = format!("{token:?}");
        assert!(
            rendered.contains("[redacted]"),
            "token bytes leaked: {rendered}"
        );
        assert!(rendered.contains("42"));
    }

    #[test]
    fn state_debug_redacts_live_grant_entropy() {
        let mut state = ClipboardState::new();
        let token = grant(&mut state, 99);
        let rendered = format!("{state:?}");
        // Grant count stays observable; raw entropy must not.
        assert!(
            rendered.contains("outstanding_grants"),
            "grant count missing: {rendered}"
        );
        let raw_decimal = format!("{:?}", token.bytes);
        assert!(
            !rendered.contains(&raw_decimal),
            "grant bytes leaked as decimal array: {rendered}"
        );
        let hex_lower: String = token.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let hex_upper: String = token.bytes.iter().map(|b| format!("{b:02X}")).collect();
        assert!(
            !rendered.contains(&hex_lower),
            "grant bytes leaked as hex: {rendered}"
        );
        assert!(
            !rendered.contains(&hex_upper),
            "grant bytes leaked as hex: {rendered}"
        );
    }

    #[test]
    fn clear_revokes_outstanding_grants() {
        let mut state = ClipboardState::new();
        let token = grant(&mut state, 2);
        state.clear();
        assert_eq!(state.outstanding_grants(), 0);
        let denied = state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(2));
        assert_eq!(denied, ClipboardOutcome::ReadDenied);
    }

    #[test]
    fn grants_are_bounded_fifo() {
        let mut state = ClipboardState::new();
        let mut tokens = Vec::new();
        for _ in 0..CLIPBOARD_MAX_OUTSTANDING_GRANTS + 3 {
            tokens.push(grant(&mut state, 8));
        }
        assert_eq!(state.outstanding_grants(), CLIPBOARD_MAX_OUTSTANDING_GRANTS);
        // Oldest 3 were evicted and are now denied.
        for evicted in tokens.iter().take(3) {
            let denied =
                state.handle_action_with_token(&read(), Some(evicted), ClipboardGrantScope(8));
            assert_eq!(denied, ClipboardOutcome::ReadDenied);
        }
        // Newest still redeems.
        let newest = tokens.last().unwrap();
        let ok = state.handle_action_with_token(&read(), Some(newest), ClipboardGrantScope(8));
        assert!(matches!(ok, ClipboardOutcome::ReadGranted { .. }));
    }
}
