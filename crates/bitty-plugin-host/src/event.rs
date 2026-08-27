//! Event pipeline classes, delivery, batching, and drop handling (OQ-013).
//!
//! Three classes (lifecycle, observation, interception) with the v1 interception
//! set of exactly four actions. Each `(plugin, event-type)` subscription gets
//! one bounded FIFO queue. Coalescing, bounded batches, fail-open timeouts,
//! and the single shared open decision point for queue overflow are modelled
//! here.
//!
//! # Drop policy — open decision point
//!
//! Queue overflow when a queue is full is a **single shared open decision
//! point** owned by `OQ-013` and the plugin-platform RFC section
//! “Delivery, ordering, batching, and coalescing” (point 3). Two candidate
//! policies remain proposed:
//!
//! - **DropOldest:** evict the oldest queued event. Newest signals survive,
//!   consumers converge on current state, but sustained bursts lose early history.
//! - **DropNewest:** refuse arrivals at an already-full queue. Already-queued
//!   events keep FIFO delivery, but newest signals starve.
//!
//! Under either candidate, drops are counted per queue, attributed to the owning
//! plugin, and reported via `bitty plugin doctor` — silent loss is not permitted.
//! This crate exposes both policies via [`DropPolicy`] and requires callers to
//! choose explicitly (no implicit default) so the open point stays honest.
//! Numeric queue depths and timeout milliseconds are OQ-014; this crate uses
//! bounded defaults that are headless-testable (`DEFAULT_QUEUE_CAPACITY`, etc.)
//! and documents them as candidate values.

use std::collections::{BTreeMap, VecDeque};

use crate::error::PluginError;
use crate::manifest::PluginId;

// ── classes and kinds ───────────────────────────────────────────────────

/// Event class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventClass {
    /// Host-internal, delivered to the owning plugin only; never affects outcome.
    Lifecycle,
    /// After terminal/configuration state is updated; never affects outcome.
    Observation,
    /// Before the host performs the user action; may veto only.
    Interception,
}

/// Event kind for v1 (closed set).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    // Lifecycle
    PluginActivated,
    PluginSuspended,
    PluginDisposed,
    HandlerViolation,
    // Observation
    TerminalOpened,
    TerminalClosed,
    TerminalTitleChanged,
    TerminalCwdChanged,
    TerminalBell,
    FocusChanged,
    SelectionChanged,
    ProcessExited,
    ConfigReloaded,
    // Interception (exactly four for v1)
    InterceptCommandDispatch,
    InterceptTerminalSpawn,
    InterceptPaste,
    InterceptOpenUrl,
}

impl EventKind {
    /// Parse a string kind (as appears in manifests / `events.subscribe`).
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        match s {
            "plugin.activated" => Ok(Self::PluginActivated),
            "plugin.suspended" => Ok(Self::PluginSuspended),
            "plugin.disposed" => Ok(Self::PluginDisposed),
            "handler.violation" => Ok(Self::HandlerViolation),
            "terminal.opened" => Ok(Self::TerminalOpened),
            "terminal.closed" => Ok(Self::TerminalClosed),
            "terminal.title-changed" => Ok(Self::TerminalTitleChanged),
            "terminal.cwd-changed" => Ok(Self::TerminalCwdChanged),
            "terminal.bell" => Ok(Self::TerminalBell),
            "focus.changed" => Ok(Self::FocusChanged),
            "selection.changed" => Ok(Self::SelectionChanged),
            "process.exited" => Ok(Self::ProcessExited),
            "config.reloaded" => Ok(Self::ConfigReloaded),
            "intercept.command-dispatch" => Ok(Self::InterceptCommandDispatch),
            "intercept.terminal-spawn" => Ok(Self::InterceptTerminalSpawn),
            "intercept.paste" => Ok(Self::InterceptPaste),
            "intercept.open-url" => Ok(Self::InterceptOpenUrl),
            _ => Err(PluginError::event(format!("unknown event kind '{s}'"))),
        }
    }

    /// Canonical string label.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PluginActivated => "plugin.activated",
            Self::PluginSuspended => "plugin.suspended",
            Self::PluginDisposed => "plugin.disposed",
            Self::HandlerViolation => "handler.violation",
            Self::TerminalOpened => "terminal.opened",
            Self::TerminalClosed => "terminal.closed",
            Self::TerminalTitleChanged => "terminal.title-changed",
            Self::TerminalCwdChanged => "terminal.cwd-changed",
            Self::TerminalBell => "terminal.bell",
            Self::FocusChanged => "focus.changed",
            Self::SelectionChanged => "selection.changed",
            Self::ProcessExited => "process.exited",
            Self::ConfigReloaded => "config.reloaded",
            Self::InterceptCommandDispatch => "intercept.command-dispatch",
            Self::InterceptTerminalSpawn => "intercept.terminal-spawn",
            Self::InterceptPaste => "intercept.paste",
            Self::InterceptOpenUrl => "intercept.open-url",
        }
    }

    /// Class for this kind.
    #[must_use]
    pub fn class(&self) -> EventClass {
        match self {
            Self::PluginActivated
            | Self::PluginSuspended
            | Self::PluginDisposed
            | Self::HandlerViolation => EventClass::Lifecycle,
            Self::TerminalOpened
            | Self::TerminalClosed
            | Self::TerminalTitleChanged
            | Self::TerminalCwdChanged
            | Self::TerminalBell
            | Self::FocusChanged
            | Self::SelectionChanged
            | Self::ProcessExited
            | Self::ConfigReloaded => EventClass::Observation,
            Self::InterceptCommandDispatch
            | Self::InterceptTerminalSpawn
            | Self::InterceptPaste
            | Self::InterceptOpenUrl => EventClass::Interception,
        }
    }

    /// Whether this kind is coalescable (latest value collapses when queue holds undelivered copies).
    ///
    /// Coalescable: title/cwd/focus/selection changes. Non-coalescable: opened/closed/exited/bell
    /// preserve one-by-one delivery up to the queue bound.
    #[must_use]
    pub fn is_coalescable(&self) -> bool {
        matches!(
            self,
            Self::TerminalTitleChanged
                | Self::TerminalCwdChanged
                | Self::FocusChanged
                | Self::SelectionChanged
        )
    }

    /// Whether this kind belongs to the v1 interception set.
    #[must_use]
    pub fn is_interception(&self) -> bool {
        self.class() == EventClass::Interception
    }
}

// ── payload ──────────────────────────────────────────────────────────────

/// Bounded, immutable event payload delivered to handlers.
///
/// Payloads are bounded and redaction-aware; handlers receive values, never
/// live core objects. For paste interception without `clipboard.read`, only
/// length/classification flags are present, not the text itself (preserves
/// the separate clipboard-consent decision per the RFC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPayload {
    /// No payload.
    Empty,
    /// Bounded text (already truncated by producer; max 8 KiB aggregate per batch).
    Text(String),
    /// Title changed: new title (bounded, host-owned rendering).
    TitleChanged(String),
    /// Cwd changed: new cwd (bounded).
    CwdChanged(String),
    /// Interception metadata: action type, origin, sanitized preview.
    Interception {
        /// Action type label.
        action: String,
        /// Origin (e.g. user, api, plugin).
        origin: String,
        /// Sanitized preview (bounded, no credential material).
        preview: String,
    },
}

impl EventPayload {
    /// Approximate byte size of this payload for batch byte-limit checks.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Text(s) => s.len(),
            Self::TitleChanged(s) => s.len(),
            Self::CwdChanged(s) => s.len(),
            Self::Interception {
                action,
                origin,
                preview,
            } => action.len() + origin.len() + preview.len(),
        }
    }
}

/// An immutable event delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Kind.
    pub kind: EventKind,
    /// Bounded payload.
    pub payload: EventPayload,
    /// Monotonic sequence number (for ordering diagnostics).
    pub sequence: u64,
}

impl Event {
    /// Create a new event.
    #[must_use]
    pub fn new(kind: EventKind, payload: EventPayload, sequence: u64) -> Self {
        Self {
            kind,
            payload,
            sequence,
        }
    }

    /// Class derived from kind.
    #[must_use]
    pub fn class(&self) -> EventClass {
        self.kind.class()
    }
}

// ── drop policy (open decision point) ───────────────────────────────────

/// Queue-overflow drop policy.
///
/// This is the single shared open decision point for `OQ-013` and the
/// delivery rules in the plugin-platform RFC (point 3). Two candidates remain
/// proposed; this type exposes both without fixing the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropPolicy {
    /// Evict the oldest queued event; newest signals survive.
    ///
    /// Consumers converge on current state (aligned with coalescing), but a
    /// sustained burst can discard every early event.
    DropOldest,
    /// Refuse arrivals at an already-full queue; already-queued events keep FIFO.
    ///
    /// Sustained floods starve newest signals, leaving the consumer behind.
    DropNewest,
}

impl std::fmt::Display for DropPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DropOldest => f.write_str("drop-oldest"),
            Self::DropNewest => f.write_str("drop-newest"),
        }
    }
}

/// Candidate default (not normative).
///
/// The RFC proposes `<= 32 events or 8 KiB per wakeup` as batch limits and
/// leaves exact queue depths to `OQ-014`. This crate uses a headless-testable
/// default capacity that satisfies the bounded-queue invariant without claiming
/// the accepted number.
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;
/// Proposed default batch event limit.
pub const DEFAULT_BATCH_EVENTS: usize = 32;
/// Proposed default batch byte limit (8 KiB).
pub const DEFAULT_BATCH_BYTES: usize = 8 * 1024;

// ── per-subscriber bounded queue ────────────────────────────────────────

/// Bounded FIFO queue for one `(plugin, event-type)` subscription.
///
/// Host-owned; producers never block on a subscriber (backpressure isolates at
/// the queue boundary, never in the emitting path). Coalescing collapses the
/// latest value when coalescable. Overflow handling is governed by
/// [`DropPolicy`]; drops are counted per queue and attributed to the owning
/// plugin (reported via `bitty plugin doctor`).
#[derive(Debug)]
pub struct EventQueue {
    inner: VecDeque<Event>,
    capacity: usize,
    drop_policy: DropPolicy,
    dropped: u64,
    kind: EventKind,
}

impl EventQueue {
    /// Create a new queue for `kind` with `capacity` and `drop_policy`.
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0`.
    pub fn new(kind: EventKind, capacity: usize, drop_policy: DropPolicy) -> Self {
        assert!(capacity > 0, "queue capacity must be > 0");
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity,
            drop_policy,
            dropped: 0,
            kind,
        }
    }

    /// Capacity of this queue.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of queued events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no event is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of events dropped since creation or last `clear`.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Drop policy for this queue.
    #[must_use]
    pub const fn drop_policy(&self) -> DropPolicy {
        self.drop_policy
    }

    /// Kind this queue subscribes to.
    #[must_use]
    pub fn kind(&self) -> &EventKind {
        &self.kind
    }

    /// Enqueue `event` applying coalescing and drop policy.
    ///
    /// - Coalescable events: if the queue already holds undelivered copies of the same
    ///   coalescable kind, collapse to the latest value (keep only the newest).
    /// - On overflow, apply [`DropPolicy`]: either evict oldest or refuse the arrival.
    pub fn push(&mut self, event: Event) {
        // Coalescing: for coalescable kinds, replace existing queued event(s) of same kind.
        // Simplest policy: if kind is coalescable and queue already contains this kind,
        // remove all existing entries of this kind and push the latest (collapse to newest).
        // For the per-type queue model, coalescing within the single-type queue means:
        // if coalescable and queue non-empty, retain only the latest (pop all, push newest).
        // A more granular model would coalesce only within the same typed queue; since this
        // queue is already per (plugin, event-type), coalescing reduces to keeping 1.
        if event.kind.is_coalescable() && !self.inner.is_empty() {
            // The queue is per-type, so any queued entry is same kind; coalesce to single latest.
            self.inner.clear();
            // Do not count coalesced discards as drops (they are semantic collapse, not loss);
            // overflow accounting tracks actual capacity misses only.
        }

        if self.inner.len() >= self.capacity {
            match self.drop_policy {
                DropPolicy::DropOldest => {
                    self.inner.pop_front();
                    self.dropped = self.dropped.wrapping_add(1);
                    self.inner.push_back(event);
                }
                DropPolicy::DropNewest => {
                    // Refuse arrival; count the drop.
                    self.dropped = self.dropped.wrapping_add(1);
                }
            }
        } else {
            self.inner.push_back(event);
        }
    }

    /// Drain all queued events in FIFO order.
    pub fn drain(&mut self) -> Vec<Event> {
        self.inner.drain(..).collect()
    }

    /// Drain up to `limit` events (bounded batch).
    pub fn drain_bounded(&mut self, limit: usize) -> Vec<Event> {
        let take = limit.min(self.inner.len());
        self.inner.drain(..take).collect()
    }

    /// Drain a bounded batch respecting both event count and byte limits.
    ///
    /// Returns up to `max_events` or `max_bytes` of payload (whichever is smaller),
    /// preserving FIFO order. This implements the RFC's `<= 32 events or 8 KiB`
    /// per-wakeup bound so one slow consumer cannot turn a burst into one
    /// oversized callback.
    pub fn drain_batch(&mut self, max_events: usize, max_bytes: usize) -> Vec<Event> {
        let mut out = Vec::new();
        let mut bytes = 0usize;
        while let Some(front) = self.inner.front() {
            if out.len() >= max_events {
                break;
            }
            let need = front.payload.byte_len();
            if bytes + need > max_bytes && !out.is_empty() {
                break;
            }
            // Allow at least one event even if it exceeds max_bytes (avoid starvation).
            let ev = self.inner.pop_front().unwrap();
            bytes += ev.payload.byte_len();
            out.push(ev);
            if bytes >= max_bytes && !out.is_empty() {
                // If we already exceeded bytes with one event, stop; otherwise continue up to limit.
                if bytes >= max_bytes {
                    break;
                }
            }
        }
        out
    }

    /// Clear queued events and reset the dropped counter.
    pub fn clear(&mut self) {
        self.inner.clear();
        self.dropped = 0;
    }

    /// Iterate queued events in order without consuming.
    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.inner.iter()
    }
}

// ── event pipeline ───────────────────────────────────────────────────────

/// Host-side event pipeline owning all per-subscriber queues.
///
/// Single owner for every `(plugin, event-type)` queue. Producers never block;
/// ordering is FIFO within one queue, no ordering across plugins, and no
/// ordering between observation delivery and unrelated user actions.
#[derive(Debug)]
pub struct EventPipeline {
    queues: BTreeMap<(String, String), EventQueue>,
    default_capacity: usize,
    drop_policy: DropPolicy,
    interception_timeout_ms: Option<u64>,
    observation_soft_limit_ms: Option<u64>,
}

impl EventPipeline {
    /// Create a new pipeline.
    ///
    /// `drop_policy` is the shared policy for queue overflow (open decision point).
    /// `default_capacity` is the per-queue bound (candidate default).
    pub fn new(default_capacity: usize, drop_policy: DropPolicy) -> Self {
        assert!(default_capacity > 0, "pipeline capacity must be > 0");
        Self {
            queues: BTreeMap::new(),
            default_capacity,
            drop_policy,
            interception_timeout_ms: None,
            observation_soft_limit_ms: None,
        }
    }

    /// Drop policy for this pipeline.
    #[must_use]
    pub const fn drop_policy(&self) -> DropPolicy {
        self.drop_policy
    }

    /// Per-queue capacity.
    #[must_use]
    pub const fn default_capacity(&self) -> usize {
        self.default_capacity
    }

    /// Subscribe `plugin_id` to `kind`.
    ///
    /// Each subscription gets one bounded FIFO queue owned by the host executor.
    /// Subscribing to an undeclared event type is a registration error (caller
    /// must have validated against the manifest's declared subscriptions).
    pub fn subscribe(&mut self, plugin_id: &PluginId, kind: EventKind) -> Result<(), PluginError> {
        let key = (plugin_id.as_str().to_string(), kind.as_str().to_string());
        if self.queues.contains_key(&key) {
            return Err(PluginError::Duplicate {
                kind: "subscription".to_string(),
                value: format!("{}:{}", plugin_id.as_str(), kind.as_str()),
            });
        }
        self.queues.insert(
            key,
            EventQueue::new(kind, self.default_capacity, self.drop_policy),
        );
        Ok(())
    }

    /// Whether `plugin_id` is subscribed to `kind`.
    #[must_use]
    pub fn is_subscribed(&self, plugin_id: &PluginId, kind: &EventKind) -> bool {
        self.queues
            .contains_key(&(plugin_id.as_str().to_string(), kind.as_str().to_string()))
    }

    /// Unsubscribe (disposes the queue).
    pub fn unsubscribe(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<(), PluginError> {
        let key = (plugin_id.as_str().to_string(), kind.as_str().to_string());
        self.queues.remove(&key).ok_or_else(|| {
            PluginError::event(format!(
                "not subscribed: {}:{}",
                plugin_id.as_str(),
                kind.as_str()
            ))
        })?;
        Ok(())
    }

    /// Publish `event` to all subscribers of its kind (observation/lifecycle).
    ///
    /// Producers never block; each matching queue receives one copy or drops per policy.
    pub fn publish(&mut self, event: Event) {
        let kind_str = event.kind.as_str().to_string();
        for ((_, _), queue) in self.queues.iter_mut().filter(|((_, k), _)| k == &kind_str) {
            // Clone event per subscriber (payload is bounded, small).
            queue.push(event.clone());
        }
    }

    /// Publish to a specific subscriber (lifecycle, delivered to owning plugin only).
    pub fn publish_to(&mut self, plugin_id: &PluginId, event: Event) -> Result<(), PluginError> {
        let key = (
            plugin_id.as_str().to_string(),
            event.kind.as_str().to_string(),
        );
        let q = self.queues.get_mut(&key).ok_or_else(|| {
            PluginError::event(format!(
                "not subscribed: {}:{}",
                plugin_id.as_str(),
                event.kind.as_str()
            ))
        })?;
        q.push(event);
        Ok(())
    }

    /// Drain a bounded batch for `plugin_id` + `kind` (FIFO, bounded by count/bytes).
    pub fn drain_batch(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<Event>, PluginError> {
        let key = (plugin_id.as_str().to_string(), kind.as_str().to_string());
        let q = self.queues.get_mut(&key).ok_or_else(|| {
            PluginError::event(format!(
                "not subscribed: {}:{}",
                plugin_id.as_str(),
                kind.as_str()
            ))
        })?;
        Ok(q.drain_batch(max_events, max_bytes))
    }

    /// Drain all queued events for `plugin_id` + `kind`.
    pub fn drain(
        &mut self,
        plugin_id: &PluginId,
        kind: &EventKind,
    ) -> Result<Vec<Event>, PluginError> {
        let key = (plugin_id.as_str().to_string(), kind.as_str().to_string());
        let q = self.queues.get_mut(&key).ok_or_else(|| {
            PluginError::event(format!(
                "not subscribed: {}:{}",
                plugin_id.as_str(),
                kind.as_str()
            ))
        })?;
        Ok(q.drain())
    }

    /// Total number of queues.
    #[must_use]
    pub fn queue_count(&self) -> usize {
        self.queues.len()
    }

    /// Total dropped events across all queues (attributed per-queue, aggregated here).
    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.queues.values().map(|q| q.dropped()).sum()
    }

    /// Per-queue dropped counts `(plugin_id, event_kind) -> dropped`.
    #[must_use]
    pub fn dropped_per_queue(&self) -> BTreeMap<(String, String), u64> {
        self.queues
            .iter()
            .map(|(k, q)| (k.clone(), q.dropped()))
            .collect()
    }

    /// Interception timeout (hard limit) in milliseconds (stub, fail-open semantics).
    ///
    /// `None` means the RFC open point has not been assigned a numeric budget yet (OQ-014).
    #[must_use]
    pub fn interception_timeout_ms(&self) -> Option<u64> {
        self.interception_timeout_ms
    }

    /// Observation soft-limit in milliseconds (stub, marked late but delivery continues).
    #[must_use]
    pub fn observation_soft_limit_ms(&self) -> Option<u64> {
        self.observation_soft_limit_ms
    }

    /// Set interception hard timeout (candidate, OQ-014 owns enforceable numbers).
    pub fn set_interception_timeout_ms(&mut self, ms: Option<u64>) {
        self.interception_timeout_ms = ms;
    }

    /// Set observation soft limit (candidate).
    pub fn set_observation_soft_limit_ms(&mut self, ms: Option<u64>) {
        self.observation_soft_limit_ms = ms;
    }

    /// Validate that queue bounds are never exceeded (property test helper).
    #[must_use]
    pub fn invariant_queue_bounds(&self) -> bool {
        self.queues.values().all(|q| q.len() <= q.capacity())
    }
}

// ── interception result (fail-open, veto-wins, reentrancy) ───────────────

/// Outcome of an interception handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InterceptionDecision {
    /// Handler abstains; action proceeds (error treated as abstention).
    Abstain,
    /// Handler explicitly approves.
    Approve,
    /// Handler vetoes the action (single veto wins).
    Veto,
}

/// Accumulate multiple interceptor decisions deterministically: a single veto vetoes
/// regardless of handler order; otherwise the action proceeds.
#[must_use]
pub fn accumulate_interceptions(decisions: &[InterceptionDecision]) -> InterceptionDecision {
    if decisions.contains(&InterceptionDecision::Veto) {
        InterceptionDecision::Veto
    } else {
        InterceptionDecision::Abstain
    }
}

/// Whether an action should proceed under the fail-open policy.
///
/// Timeouts and errors are treated as abstention: the host proceeds with the
/// user action without the plugin, records a violation, and disables the
/// handler after repeated violations in a window.
#[must_use]
pub fn should_proceed(decision: InterceptionDecision, timed_out: bool) -> bool {
    if timed_out {
        return true;
    }
    match decision {
        InterceptionDecision::Veto => false,
        InterceptionDecision::Approve | InterceptionDecision::Abstain => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PluginId {
        PluginId::new(s).unwrap()
    }

    #[test]
    fn queue_drop_oldest() {
        let mut q = EventQueue::new(EventKind::TerminalBell, 2, DropPolicy::DropOldest);
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 1));
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 2));
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 3));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        let drained = q.drain();
        assert_eq!(drained[0].sequence, 2);
        assert_eq!(drained[1].sequence, 3);
    }

    #[test]
    fn queue_drop_newest() {
        let mut q = EventQueue::new(EventKind::TerminalBell, 2, DropPolicy::DropNewest);
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 1));
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 2));
        q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, 3));
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        let drained = q.drain();
        assert_eq!(drained[0].sequence, 1);
        assert_eq!(drained[1].sequence, 2);
    }

    #[test]
    fn coalescable_collapses() {
        let mut q = EventQueue::new(EventKind::TerminalTitleChanged, 8, DropPolicy::DropOldest);
        q.push(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::TitleChanged("a".into()),
            1,
        ));
        q.push(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::TitleChanged("b".into()),
            2,
        ));
        q.push(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::TitleChanged("c".into()),
            3,
        ));
        // Coalescable per-type queue should have collapsed to single latest.
        assert_eq!(q.len(), 1);
        assert_eq!(q.dropped(), 0);
        assert_eq!(q.iter().next().unwrap().sequence, 3);
    }

    #[test]
    fn non_coalescable_preserves_fifo() {
        let mut q = EventQueue::new(EventKind::TerminalBell, 8, DropPolicy::DropOldest);
        for i in 0..4 {
            q.push(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
        }
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn batch_byte_limit() {
        let mut q = EventQueue::new(EventKind::TerminalBell, 8, DropPolicy::DropOldest);
        q.push(Event::new(
            EventKind::TerminalBell,
            EventPayload::Text("a".repeat(100)),
            1,
        ));
        q.push(Event::new(
            EventKind::TerminalBell,
            EventPayload::Text("b".repeat(100)),
            2,
        ));
        let batch = q.drain_batch(32, 120);
        assert_eq!(batch.len(), 1);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn pipeline_publish_fans_out() {
        let mut p = EventPipeline::new(8, DropPolicy::DropOldest);
        p.subscribe(&pid("xuepoo.a"), EventKind::TerminalBell)
            .unwrap();
        p.subscribe(&pid("xuepoo.b"), EventKind::TerminalBell)
            .unwrap();
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, 1));
        assert_eq!(
            p.drain(&pid("xuepoo.a"), &EventKind::TerminalBell)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            p.drain(&pid("xuepoo.b"), &EventKind::TerminalBell)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pipeline_isolated_backpressure() {
        let mut p = EventPipeline::new(2, DropPolicy::DropNewest);
        p.subscribe(&pid("xuepoo.a"), EventKind::TerminalBell)
            .unwrap();
        p.subscribe(&pid("xuepoo.b"), EventKind::TerminalBell)
            .unwrap();
        for i in 0..5 {
            p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
        }
        // Each queue drops independently; one full queue does not block the other.
        assert!(p.invariant_queue_bounds());
        assert_eq!(p.total_dropped(), 6); // 3 per queue after capacity 2 (5 published, 2 kept, 3 dropped each -> 6 total)
    }

    #[test]
    fn interception_veto_wins() {
        assert_eq!(
            accumulate_interceptions(&[
                InterceptionDecision::Approve,
                InterceptionDecision::Veto,
                InterceptionDecision::Abstain
            ]),
            InterceptionDecision::Veto
        );
        assert_eq!(
            accumulate_interceptions(&[
                InterceptionDecision::Approve,
                InterceptionDecision::Abstain
            ]),
            InterceptionDecision::Abstain
        );
    }

    #[test]
    fn fail_open_on_timeout() {
        assert!(should_proceed(InterceptionDecision::Veto, true));
        assert!(!should_proceed(InterceptionDecision::Veto, false));
    }

    #[test]
    fn no_ordering_across_plugins() {
        let mut p = EventPipeline::new(8, DropPolicy::DropOldest);
        p.subscribe(&pid("xuepoo.a"), EventKind::TerminalBell)
            .unwrap();
        p.subscribe(&pid("xuepoo.b"), EventKind::TerminalTitleChanged)
            .unwrap();
        p.publish(Event::new(EventKind::TerminalBell, EventPayload::Empty, 1));
        p.publish(Event::new(
            EventKind::TerminalTitleChanged,
            EventPayload::TitleChanged("hi".into()),
            2,
        ));
        // Each queue has FIFO within one queue, but no ordering guarantee across plugins/kinds is relied upon.
        assert_eq!(
            p.drain(&pid("xuepoo.a"), &EventKind::TerminalBell).unwrap()[0].sequence,
            1
        );
    }
}
