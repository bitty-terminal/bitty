#![forbid(unsafe_code)]
//! Routable provider envelope (CW-23, issue #1001; `OQ-058`).
//!
//! Contract source: the owner ruling of 2026-09-23 adopts `SMO-1..SMO-4`
//! including delivery semantics over the `bitty` owner decision packet
//! (merged `bitty` #1300), with terms from `ADR-0013` (`bitty-docs` #367,
//! `bb96efe`). The candidate envelope fields extend the `SMO-3` routing
//! candidates (`AgentMessage { message_id, sender, recipient, task_id,
//! parent_run_id, kind, payload, deadline, priority }`) with bounded
//! payloads under the accepted framing, `message_id`-based deduplication,
//! deadline and expiry handling, cancellation, and fail-closed routing to
//! a dead or unregistered recipient.
//!
//! What this module provides:
//!
//! - [`MessageKind`] — the seven accepted routable kinds
//!   (`DelegationRequest`, `DelegationResult`, `Observation`,
//!   `ApprovalRequest`, `ToolResult`, `Cancellation`, `StatusUpdate`).
//! - [`AgentMessage`] — the validated routable envelope: non-zero
//!   `message_id` identity, `owner.name` sender/recipient attribution,
//!   optional `task_id`/`parent_run_id` provenance, seven-kind routing,
//!   [`BoundedPayload`](super::panel::BoundedPayload) body (`<= 8 KiB`),
//!   logical-tick `deadline`/`expires_at`, and `u8` priority.
//!   No wall-clock time participates: `deadline`/`expires_at` are
//!   monotonic logical ticks supplied by the caller (`0` means no
//!   deadline/expiry); [`AgentMessage::is_expired`] decides expiry.
//! - [`RoutableLedger`] — the bounded inflight store (`256` envelopes):
//!   `message_id` deduplication, expiry sweep, explicit cancellation,
//!   and priority-ordered drain. Fail-closed and typed: a rejected send
//!   leaves the ledger unchanged.
//! - [`RoutableError`] — typed envelope/ledger failure.
//!
//! Presentation stays non-authoritative (`SMO-2`): routing never grants
//! capability. Capability enforcement stays with the host registry and
//! the v1 capability ledger; this module owns envelope shape and
//! delivery bookkeeping only. Panels never own executions (`ADR-0013`
//! Panel/Execution separation): `task_id`/`parent_run_id` are provenance,
//! never ownership.
//!
//! Composes with [`super::provider`] (provider registration behind the
//! `panel.provider` capability) and [`super::event_bus_v1`] (v1 taxonomy
//! frozen per the `OQ-056` deferral; per-family dimensions are v2 scope).
//! The host validates recipient registration before ledger admission so
//! a missing or stale route fails closed (`SMO-3`) instead of falling
//! back to an ambient recipient.

use std::collections::HashMap;

use super::panel::BoundedPayload;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum inflight routable envelopes in one ledger.
///
/// Bounded so one burst cannot starve the host; a send past the cap
/// fails closed with [`RoutableError::TooManyInflight`].
pub const MAX_ROUTABLE_INFLIGHT: usize = 256;

/// Maximum `owner.name` endpoint length (`16 + 1 + 16`).
pub const MAX_ROUTABLE_ENDPOINT_LEN: usize = 33;

/// Maximum `owner`/`name` segment length (mirrors the topic owner grammar).
pub const MAX_ROUTABLE_ENDPOINT_SEGMENT_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed routable-envelope failure; ledger state is unchanged on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutableError {
    /// `message_id` is zero (envelope identity requires non-zero).
    InvalidMessageId,
    /// Malformed sender endpoint.
    InvalidSender { value: String, reason: String },
    /// Malformed recipient endpoint.
    InvalidRecipient { value: String, reason: String },
    /// Recipient is well-formed but not a registered route (fail-closed).
    UnknownRecipient { recipient: String },
    /// Sender is well-formed but not a registered route (fail-closed).
    UnknownSender { sender: String },
    /// `message_id` already inflight (deduplication).
    DuplicateMessageId { message_id: u64 },
    /// No inflight envelope carries `message_id`.
    UnknownMessageId { message_id: u64 },
    /// Envelope already expired at admission (`now > expires_at`).
    Expired { message_id: u64 },
    /// `expires_at` precedes `deadline` (incoherent delivery window).
    InvalidDeadline { deadline: u64, expires_at: u64 },
    /// Ledger holds [`MAX_ROUTABLE_INFLIGHT`] envelopes already.
    TooManyInflight { max: usize, current: usize },
}

impl std::fmt::Display for RoutableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMessageId => f.write_str("message id must be non-zero"),
            Self::InvalidSender { value, reason } => {
                write!(f, "invalid sender '{value}': {reason}")
            }
            Self::InvalidRecipient { value, reason } => {
                write!(f, "invalid recipient '{value}': {reason}")
            }
            Self::UnknownRecipient { recipient } => {
                write!(f, "unknown recipient '{recipient}': route fails closed")
            }
            Self::UnknownSender { sender } => {
                write!(f, "unknown sender '{sender}': attribution fails closed")
            }
            Self::DuplicateMessageId { message_id } => {
                write!(f, "duplicate message id {message_id}")
            }
            Self::UnknownMessageId { message_id } => {
                write!(f, "unknown message id {message_id}")
            }
            Self::Expired { message_id } => {
                write!(f, "message {message_id} already expired")
            }
            Self::InvalidDeadline {
                deadline,
                expires_at,
            } => {
                write!(
                    f,
                    "invalid delivery window: deadline {deadline} after expiry {expires_at}"
                )
            }
            Self::TooManyInflight { max, current } => {
                write!(
                    f,
                    "too many inflight messages: max {max}, current {current}"
                )
            }
        }
    }
}

impl std::error::Error for RoutableError {}

// ---------------------------------------------------------------------------
// Message kinds (SMO-3 routing candidates, accepted via OQ-058)
// ---------------------------------------------------------------------------

/// Routable message kind: the seven accepted delivery kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    DelegationRequest,
    DelegationResult,
    Observation,
    ApprovalRequest,
    ToolResult,
    Cancellation,
    StatusUpdate,
}

impl MessageKind {
    /// Stable lowercase label used in diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DelegationRequest => "delegation-request",
            Self::DelegationResult => "delegation-result",
            Self::Observation => "observation",
            Self::ApprovalRequest => "approval-request",
            Self::ToolResult => "tool-result",
            Self::Cancellation => "cancellation",
            Self::StatusUpdate => "status-update",
        }
    }

    /// Parses a kind label.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "delegation-request" => Some(Self::DelegationRequest),
            "delegation-result" => Some(Self::DelegationResult),
            "observation" => Some(Self::Observation),
            "approval-request" => Some(Self::ApprovalRequest),
            "tool-result" => Some(Self::ToolResult),
            "cancellation" => Some(Self::Cancellation),
            "status-update" => Some(Self::StatusUpdate),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// Validated routable envelope: who sends what to whom, by when, how urgent.
///
/// Identity is `message_id` (non-zero, deduped by the ledger).
/// Attribution is `sender`/`recipient` (`owner.name` grammar, mirrors the
/// provider and topic owner rules). `task_id`/`parent_run_id` are optional
/// provenance (`None` means no task/run scope); they never confer
/// ownership per the `ADR-0013` Panel/Execution separation. The body is a
/// bounded payload (`<= 8 KiB`). Delivery uses logical ticks: `deadline`
/// is the soft delivery target, `expires_at` the hard drop-after tick
/// (`0` means no deadline/expiry). Priority is an opaque `u8` (higher
/// drains first); it never grants capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMessage {
    message_id: u64,
    sender: String,
    recipient: String,
    task_id: Option<u64>,
    parent_run_id: Option<u64>,
    kind: MessageKind,
    payload: BoundedPayload,
    deadline: u64,
    expires_at: u64,
    priority: u8,
}

impl AgentMessage {
    /// Validates and builds a routable envelope.
    ///
    /// # Errors
    ///
    /// [`RoutableError::InvalidMessageId`], [`RoutableError::InvalidSender`],
    /// [`RoutableError::InvalidRecipient`], or
    /// [`RoutableError::InvalidDeadline`]; nothing is allocated on behalf
    /// of a ledger.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        message_id: u64,
        sender: &str,
        recipient: &str,
        task_id: Option<u64>,
        parent_run_id: Option<u64>,
        kind: MessageKind,
        payload: BoundedPayload,
        deadline: u64,
        expires_at: u64,
        priority: u8,
    ) -> Result<Self, RoutableError> {
        if message_id == 0 {
            return Err(RoutableError::InvalidMessageId);
        }
        validate_endpoint(sender).map_err(|reason| RoutableError::InvalidSender {
            value: sender.to_string(),
            reason,
        })?;
        validate_endpoint(recipient).map_err(|reason| RoutableError::InvalidRecipient {
            value: recipient.to_string(),
            reason,
        })?;
        if expires_at != 0 && deadline != 0 && expires_at < deadline {
            return Err(RoutableError::InvalidDeadline {
                deadline,
                expires_at,
            });
        }
        if deadline != 0 && expires_at == 0 {
            return Err(RoutableError::InvalidDeadline {
                deadline,
                expires_at,
            });
        }
        Ok(Self {
            message_id,
            sender: sender.to_string(),
            recipient: recipient.to_string(),
            task_id,
            parent_run_id,
            kind,
            payload,
            deadline,
            expires_at,
            priority,
        })
    }

    /// Envelope identity (deduplication key).
    #[must_use]
    pub fn message_id(&self) -> u64 {
        self.message_id
    }

    /// Attributed sender (`owner.name`).
    #[must_use]
    pub fn sender(&self) -> &str {
        &self.sender
    }

    /// Attributed recipient (`owner.name`).
    #[must_use]
    pub fn recipient(&self) -> &str {
        &self.recipient
    }

    /// Optional task provenance (`None` means no task scope).
    #[must_use]
    pub fn task_id(&self) -> Option<u64> {
        self.task_id
    }

    /// Optional parent-run provenance (`None` means no run scope).
    #[must_use]
    pub fn parent_run_id(&self) -> Option<u64> {
        self.parent_run_id
    }

    /// Routing kind.
    #[must_use]
    pub fn kind(&self) -> MessageKind {
        self.kind
    }

    /// Bounded body.
    #[must_use]
    pub fn payload(&self) -> &BoundedPayload {
        &self.payload
    }

    /// Soft delivery target tick (`0` means none).
    #[must_use]
    pub fn deadline(&self) -> u64 {
        self.deadline
    }

    /// Hard drop-after tick (`0` means never expires).
    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Urgency (`u8`, higher drains first; never authority).
    #[must_use]
    pub fn priority(&self) -> u8 {
        self.priority
    }

    /// Whether the envelope is expired at `now_ticks`.
    ///
    /// `expires_at == 0` never expires; otherwise expired when
    /// `now_ticks > expires_at`.
    #[must_use]
    pub fn is_expired(&self, now_ticks: u64) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        now_ticks > self.expires_at
    }

    /// Whether the envelope is past its soft deadline at `now_ticks`.
    ///
    /// `deadline == 0` has no deadline; otherwise past when
    /// `now_ticks > deadline`.
    #[must_use]
    pub fn is_past_deadline(&self, now_ticks: u64) -> bool {
        if self.deadline == 0 {
            return false;
        }
        now_ticks > self.deadline
    }
}

fn validate_endpoint(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("endpoint must not be empty".to_string());
    }
    if value.len() > MAX_ROUTABLE_ENDPOINT_LEN {
        return Err("endpoint exceeds bounded length".to_string());
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("endpoint must not contain whitespace".to_string());
    }
    let segments: Vec<&str> = value.split('.').collect();
    if segments.len() != 2 {
        return Err("endpoint must be owner.name".to_string());
    }
    for segment in &segments {
        if segment.is_empty() || segment.len() > MAX_ROUTABLE_ENDPOINT_SEGMENT_LEN {
            return Err("endpoint segments must be 1..=16 bytes".to_string());
        }
        if !segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        {
            return Err("endpoint segments must start with [a-z]".to_string());
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err("endpoint segments must use [a-z0-9_-]".to_string());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ledger: bounded inflight store with dedup, cancel, expiry, priority drain
// ---------------------------------------------------------------------------

/// Bounded inflight routable store.
///
/// Deduplicates on `message_id`, drops expired envelopes on sweep,
/// cancels explicitly by id, and drains highest-priority first
/// (priority descending, `message_id` ascending as the stable tiebreak).
/// Bounded at [`MAX_ROUTABLE_INFLIGHT`]; admission past the cap fails
/// closed. Shape validation stays with [`AgentMessage::new`]; routing
/// attribution (sender/recipient registered) stays with the host.
#[derive(Clone, Debug, Default)]
pub struct RoutableLedger {
    inflight: HashMap<u64, AgentMessage>,
}

impl RoutableLedger {
    /// Creates an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admits a validated envelope at `now_ticks`.
    ///
    /// Rejects already-expired envelopes, duplicate `message_id` values,
    /// and admission past the inflight cap. State is unchanged on error.
    ///
    /// # Errors
    ///
    /// [`RoutableError::Expired`], [`RoutableError::DuplicateMessageId`],
    /// or [`RoutableError::TooManyInflight`].
    pub fn send(&mut self, message: AgentMessage, now_ticks: u64) -> Result<(), RoutableError> {
        if message.is_expired(now_ticks) {
            return Err(RoutableError::Expired {
                message_id: message.message_id(),
            });
        }
        if self.inflight.contains_key(&message.message_id()) {
            return Err(RoutableError::DuplicateMessageId {
                message_id: message.message_id(),
            });
        }
        if self.inflight.len() >= MAX_ROUTABLE_INFLIGHT {
            return Err(RoutableError::TooManyInflight {
                max: MAX_ROUTABLE_INFLIGHT,
                current: self.inflight.len(),
            });
        }
        self.inflight.insert(message.message_id(), message);
        Ok(())
    }

    /// Cancels the inflight envelope with `message_id`, returning it.
    ///
    /// Cancellation is explicit removal: the envelope leaves the inflight
    /// set and is handed back to the caller for audit. A `Cancellation`
    /// [`MessageKind`] envelope is routing data, not this removal.
    ///
    /// # Errors
    ///
    /// [`RoutableError::UnknownMessageId`]; state is unchanged.
    pub fn cancel(&mut self, message_id: u64) -> Result<AgentMessage, RoutableError> {
        self.inflight
            .remove(&message_id)
            .ok_or(RoutableError::UnknownMessageId { message_id })
    }

    /// Drops every envelope expired at `now_ticks`, returning them.
    ///
    /// Returned in `message_id` order for deterministic audit.
    #[must_use]
    pub fn sweep_expired(&mut self, now_ticks: u64) -> Vec<AgentMessage> {
        let mut expired: Vec<u64> = self
            .inflight
            .values()
            .filter(|message| message.is_expired(now_ticks))
            .map(AgentMessage::message_id)
            .collect();
        expired.sort_unstable();
        let mut removed = Vec::with_capacity(expired.len());
        for id in expired {
            if let Some(message) = self.inflight.remove(&id) {
                removed.push(message);
            }
        }
        removed
    }

    /// Drains up to `max` envelopes, highest-priority first.
    ///
    /// Order is priority descending, then `message_id` ascending as the
    /// stable tiebreak. Drained envelopes leave the inflight set.
    #[must_use]
    pub fn drain_by_priority(&mut self, max: usize) -> Vec<AgentMessage> {
        let mut ordered: Vec<&AgentMessage> = self.inflight.values().collect();
        ordered.sort_by(|left, right| {
            right
                .priority()
                .cmp(&left.priority())
                .then_with(|| left.message_id().cmp(&right.message_id()))
        });
        let take = max.min(ordered.len());
        let ids: Vec<u64> = ordered
            .iter()
            .take(take)
            .map(|message| message.message_id())
            .collect();
        let mut drained = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(message) = self.inflight.remove(&id) {
                drained.push(message);
            }
        }
        drained
    }

    /// Returns the inflight envelope with `message_id`, if present.
    #[must_use]
    pub fn get(&self, message_id: u64) -> Option<&AgentMessage> {
        self.inflight.get(&message_id)
    }

    /// Whether `message_id` is inflight (deduplication probe).
    #[must_use]
    pub fn contains(&self, message_id: u64) -> bool {
        self.inflight.contains_key(&message_id)
    }

    /// Number of inflight envelopes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inflight.len()
    }

    /// Whether no envelope is inflight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inflight.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::super::panel::BoundedPayload;
    use super::*;

    fn payload(text: &str) -> BoundedPayload {
        BoundedPayload::try_new(text).unwrap()
    }

    fn message(id: u64, sender: &str, recipient: &str) -> AgentMessage {
        AgentMessage::new(
            id,
            sender,
            recipient,
            Some(7),
            Some(11),
            MessageKind::DelegationRequest,
            payload("work"),
            100,
            200,
            10,
        )
        .unwrap()
    }

    #[test]
    fn kinds_roundtrip_through_labels() {
        let kinds = [
            MessageKind::DelegationRequest,
            MessageKind::DelegationResult,
            MessageKind::Observation,
            MessageKind::ApprovalRequest,
            MessageKind::ToolResult,
            MessageKind::Cancellation,
            MessageKind::StatusUpdate,
        ];
        for kind in kinds {
            assert_eq!(MessageKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MessageKind::parse("nope"), None);
    }

    #[test]
    fn envelope_validates_identity_attribution_and_window() {
        assert_eq!(
            AgentMessage::new(
                0,
                "example.cmd",
                "example.worker",
                None,
                None,
                MessageKind::Observation,
                payload("x"),
                0,
                0,
                0,
            ),
            Err(RoutableError::InvalidMessageId)
        );
        assert!(
            AgentMessage::new(
                1,
                "Example.cmd",
                "example.worker",
                None,
                None,
                MessageKind::Observation,
                payload("x"),
                0,
                0,
                0,
            )
            .is_err()
        );
        assert!(
            AgentMessage::new(
                1,
                "example.cmd",
                "bad recipient",
                None,
                None,
                MessageKind::Observation,
                payload("x"),
                0,
                0,
                0,
            )
            .is_err()
        );
        // Deadline without expiry is incoherent: fail closed.
        assert_eq!(
            AgentMessage::new(
                1,
                "example.cmd",
                "example.worker",
                None,
                None,
                MessageKind::Observation,
                payload("x"),
                100,
                0,
                0,
            ),
            Err(RoutableError::InvalidDeadline {
                deadline: 100,
                expires_at: 0,
            })
        );
        // Expiry before deadline is incoherent.
        assert_eq!(
            AgentMessage::new(
                1,
                "example.cmd",
                "example.worker",
                None,
                None,
                MessageKind::Observation,
                payload("x"),
                200,
                100,
                0,
            ),
            Err(RoutableError::InvalidDeadline {
                deadline: 200,
                expires_at: 100,
            })
        );
        let envelope = message(1, "example.cmd", "example.worker");
        assert_eq!(envelope.message_id(), 1);
        assert_eq!(envelope.sender(), "example.cmd");
        assert_eq!(envelope.recipient(), "example.worker");
        assert_eq!(envelope.task_id(), Some(7));
        assert_eq!(envelope.parent_run_id(), Some(11));
        assert_eq!(envelope.kind(), MessageKind::DelegationRequest);
        assert_eq!(envelope.deadline(), 100);
        assert_eq!(envelope.expires_at(), 200);
        assert_eq!(envelope.priority(), 10);
        assert!(!envelope.is_expired(200));
        assert!(envelope.is_expired(201));
        assert!(!envelope.is_past_deadline(100));
        assert!(envelope.is_past_deadline(101));
    }

    #[test]
    fn ledger_dedups_rejects_expired_and_caps_inflight() {
        let mut ledger = RoutableLedger::new();
        assert!(ledger.is_empty());
        ledger
            .send(message(1, "example.cmd", "example.worker"), 50)
            .unwrap();
        assert!(ledger.contains(1));
        assert_eq!(
            ledger.send(message(1, "example.cmd", "example.worker"), 50),
            Err(RoutableError::DuplicateMessageId { message_id: 1 })
        );
        assert_eq!(ledger.len(), 1);
        // Already expired at admission: fail closed without insertion.
        let expired = AgentMessage::new(
            2,
            "example.cmd",
            "example.worker",
            None,
            None,
            MessageKind::Observation,
            payload("late"),
            10,
            20,
            0,
        )
        .unwrap();
        assert_eq!(
            ledger.send(expired, 21),
            Err(RoutableError::Expired { message_id: 2 })
        );
        assert!(!ledger.contains(2));
    }

    #[test]
    fn ledger_cancel_and_expiry_sweep_are_explicit() {
        let mut ledger = RoutableLedger::new();
        ledger
            .send(message(1, "example.cmd", "example.worker"), 0)
            .unwrap();
        ledger
            .send(message(2, "example.cmd", "example.worker"), 0)
            .unwrap();
        let cancelled = ledger.cancel(1).unwrap();
        assert_eq!(cancelled.message_id(), 1);
        assert_eq!(
            ledger.cancel(1),
            Err(RoutableError::UnknownMessageId { message_id: 1 })
        );
        // Message 2 expires after tick 200.
        let swept = ledger.sweep_expired(201);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].message_id(), 2);
        assert!(ledger.is_empty());
    }

    #[test]
    fn ledger_drains_highest_priority_first() {
        let mut ledger = RoutableLedger::new();
        for (id, priority) in [(1, 1), (2, 9), (3, 5)] {
            let envelope = AgentMessage::new(
                id,
                "example.cmd",
                "example.worker",
                None,
                None,
                MessageKind::StatusUpdate,
                payload("s"),
                0,
                0,
                priority,
            )
            .unwrap();
            ledger.send(envelope, 0).unwrap();
        }
        let drained = ledger.drain_by_priority(2);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].message_id(), 2);
        assert_eq!(drained[1].message_id(), 3);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn ledger_caps_at_bounded_max() {
        let mut ledger = RoutableLedger::new();
        for id in 1..=MAX_ROUTABLE_INFLIGHT as u64 {
            ledger
                .send(message(id, "example.cmd", "example.worker"), 0)
                .unwrap();
        }
        assert_eq!(ledger.len(), MAX_ROUTABLE_INFLIGHT);
        assert_eq!(
            ledger.send(
                message(
                    MAX_ROUTABLE_INFLIGHT as u64 + 1,
                    "example.cmd",
                    "example.worker"
                ),
                0
            ),
            Err(RoutableError::TooManyInflight {
                max: MAX_ROUTABLE_INFLIGHT,
                current: MAX_ROUTABLE_INFLIGHT,
            })
        );
    }
}
