//! Core-owned focusable-overlay transient input capture (CTX-0941, W-28).
//!
//! A plugin that mounted a block into the focusable `overlay` slot can claim
//! exclusive keyboard/overlay focus for one bounded interaction. Core owns the
//! capture switch, the permission, and the queue; the plugin can never capture
//! input directly and never registers a synchronous callback on the input path
//! (`P0-AC-015`).
//!
//! # Semantics
//!
//! - **Single owner.** At most one capture is active at a time. A second
//!   `acquire` fails closed with [`E_UI_ALREADY_CAPTURED`] instead of stealing
//!   or queueing behind the current owner.
//! - **Transient and bounded.** The queue is capped at
//!   [`OVERLAY_CAPTURE_QUEUE_MAX`]; overflow drops the oldest event and counts
//!   it. A Core-side timeout ([`OVERLAY_CAPTURE_TIMEOUT_MS`]) revokes a capture
//!   that outlives its session.
//! - **Release is guaranteed and idempotent.** [`OverlayCapture::release`]
//!   returns `false` for a foreign or repeated release rather than erroring;
//!   [`OverlayCapture::revoke_plugin`] drops the owner on suspend, dispose,
//!   unload, or crash and [`OverlayCapture::revoke_expired_at`] drops it on
//!   timeout, so a crashed or faulty plugin can never wedge input.
//! - **No hot path.** [`OverlayCapture::enqueue`] only appends to the queue;
//!   it never calls into a VM. The plugin observes events exclusively through
//!   [`OverlayCapture::poll`], a host call it makes on its own cold path.
//!
//! The initial capture timeout constant is a Core-internal implementation bound
//! (the accepted `W-01` host contract will fix the final value); the binding
//! requirement is that a finite limit exists.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bitty_lua::{
    BridgeError, E_UI_ALREADY_CAPTURED, E_UI_NOT_OWNER, OVERLAY_CAPTURE_QUEUE_MAX,
    OVERLAY_CAPTURE_TEXT_MAX_BYTES, OverlayInput,
};

/// Core-side capture session timeout in milliseconds (CTX-0941).
///
/// A capture that outlives this bound is revoked by Core on the next tick, so
/// a blocked or silent plugin cannot pin terminal input.
pub const OVERLAY_CAPTURE_TIMEOUT_MS: u64 = 30_000;

/// Maximum pending `overlay.released` observations buffered between ticks.
///
/// `finish_session` queues one observation per ended session while the
/// application drains them only on its tick. An acquire/release loop inside a
/// single command callback would otherwise grow the buffer without bound
/// (the mechanism path reuses one mounted block, so no block budget stops
/// the loop) and burst every `overlay.released` subscriber on the next tick.
/// Overflow drops the oldest observation and counts it, mirroring the
/// drop-oldest capture-queue idiom.
pub const PENDING_RELEASED_MAX: usize = 64;

/// Full terminal release-reason vocabulary (accepted W-01 contract).
///
/// Only `submitted` and `cancelled` are owner-suppliable; the rest are
/// Core-reported. Involuntary terminal causes overwrite the reason.
pub const RELEASE_REASONS: &[&str] = &[
    "released",
    "submitted",
    "cancelled",
    "focus_switched",
    "unloaded",
    "crashed",
    "timeout",
];

/// Owner-suppliable release dispositions (`release` reason argument).
pub const OWNER_RELEASE_REASONS: &[&str] = &["submitted", "cancelled"];

/// Default release reason when the owner supplies none.
pub const DEFAULT_RELEASE_REASON: &str = "released";

/// Core-reported reason for a capture revoked by plugin suspend, dispose,
/// unload, or disable.
pub const UNLOADED_RELEASE_REASON: &str = "unloaded";

/// Core-reported reason for a capture revoked after a plugin crash or failed
/// activation.
pub const CRASHED_RELEASE_REASON: &str = "crashed";

/// Core-reported reason for a capture revoked by a user focus switch.
pub const FOCUS_SWITCHED_RELEASE_REASON: &str = "focus_switched";

/// Core-reported reason for a capture revoked by the idle timeout.
pub const TIMEOUT_RELEASE_REASON: &str = "timeout";

/// Whether `reason` is a valid terminal release reason.
#[must_use]
pub fn is_release_reason(reason: &str) -> bool {
    RELEASE_REASONS.contains(&reason)
}

/// Whether `reason` may be supplied by the owner in `release`.
#[must_use]
pub fn is_owner_release_reason(reason: &str) -> bool {
    OWNER_RELEASE_REASONS.contains(&reason)
}

/// The active capture owner for one runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureOwner {
    plugin_id: String,
    handle: i64,
    /// Absolute monotonic deadline; Core revokes at or after this instant.
    expires_at: Instant,
}

/// Terminal record of one ended session, remembered for the owner's next
/// poll and for the `overlay.released` bus event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseRecord {
    plugin_id: String,
    handle: i64,
    reason: String,
    /// Sequence of the last event delivered in the ended session.
    seq: u64,
    /// Whether the ended session had overflowed its queue.
    overflowed: bool,
}

/// One pending `overlay.released` bus observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedEvent {
    /// Capture owner at release time.
    pub owner: String,
    /// Terminal release reason.
    pub reason: String,
}

/// Detailed poll result per the accepted W-01 contract.
///
/// `status` is `"active"` while the session holds capture and `"released"`
/// after any terminal cause. `seq` is the monotonic sequence of the last
/// event delivered to this owner in this session. `events` holds drained
/// input in order (empty when there is nothing new). `overflowed` is sticky
/// once queue overflow has dropped an older event. `reason` is present only
/// with `"released"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePoll {
    /// `"active"` or `"released"`.
    pub status: &'static str,
    /// Last delivered sequence in this session.
    pub seq: u64,
    /// Drained events (empty for a released session: the queue is dropped).
    pub events: Vec<OverlayInput>,
    /// Sticky overflow flag for this session.
    pub overflowed: bool,
    /// Terminal reason, present only when `status` is `"released"`.
    pub reason: Option<String>,
}

/// Runtime-shared single-owner overlay capture switch and bounded queue.
///
/// Owned by [`PluginRuntime`](super::PluginRuntime) and shared with every
/// generation's [`PluginServices`](super::services::PluginServices): acquire
/// grants, release clears, suspend/dispose revoke, and a Core tick expires.
#[derive(Debug, Default)]
pub struct OverlayCapture {
    owner: Option<CaptureOwner>,
    queue: VecDeque<OverlayInput>,
    next_sequence: u64,
    dropped: u64,
    /// Sticky overflow flag for the live session (cleared on acquire).
    overflowed: bool,
    /// Sequence of the last event delivered to the live owner (0 when none).
    last_delivered: u64,
    /// Terminal record of the most recent ended session (cleared on acquire).
    last_release: Option<ReleaseRecord>,
    /// Pending `overlay.released` observations for the application tick.
    pending_released: Vec<ReleasedEvent>,
    /// Observations dropped by `pending_released` overflow since creation.
    released_dropped: u64,
}

impl OverlayCapture {
    /// An empty capture manager (no owner, empty queue).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim exclusive capture for `handle` on behalf of `plugin_id`.
    ///
    /// An already-expired owner is revoked first; any other active owner
    /// (including a second acquire by the same plugin) fails closed with
    /// [`E_UI_ALREADY_CAPTURED`].
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with code [`E_UI_ALREADY_CAPTURED`] (class `runtime`).
    pub fn acquire(
        &mut self,
        plugin_id: &str,
        handle: i64,
        expires_at: Instant,
    ) -> Result<(), BridgeError> {
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.expires_at <= Instant::now())
        {
            self.finish_session(TIMEOUT_RELEASE_REASON);
        }
        if self.owner.is_some() {
            return Err(BridgeError::new(
                "runtime",
                E_UI_ALREADY_CAPTURED,
                "a focusable-overlay input capture is already active",
            ));
        }
        self.owner = Some(CaptureOwner {
            plugin_id: plugin_id.to_string(),
            handle,
            expires_at,
        });
        // A new session starts clean: no sticky overflow, no delivered
        // sequence, and no remembered release for the previous handle.
        self.overflowed = false;
        self.last_delivered = 0;
        self.last_release = None;
        Ok(())
    }

    /// Release the capture owned by (`plugin_id`, `handle`).
    ///
    /// Idempotent: returns `true` only when it actually dropped an owned
    /// capture; a foreign or repeated release returns `false` and never errors.
    /// Records the default `released` reason for the owner's next poll.
    pub fn release(&mut self, plugin_id: &str, handle: i64) -> bool {
        self.release_with_reason(plugin_id, handle, DEFAULT_RELEASE_REASON)
    }

    /// Release with an explicit terminal reason.
    ///
    /// Same idempotence as [`OverlayCapture::release`]; the reason is
    /// remembered for the owner's next detailed poll and for the
    /// `overlay.released` bus event. Callers pass one of
    /// [`RELEASE_REASONS`]; unknown reasons are recorded verbatim and never
    /// affect the state transition.
    pub fn release_with_reason(&mut self, plugin_id: &str, handle: i64, reason: &str) -> bool {
        let owned = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.plugin_id == plugin_id && owner.handle == handle);
        if owned {
            self.finish_session(reason);
        }
        owned
    }

    /// Drain up to `max` queued events for the capture owned by
    /// (`plugin_id`, `handle`), oldest first.
    ///
    /// Owner-only low-level drain: refreshes the idle deadline on success.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with code [`E_UI_NOT_OWNER`] (class `runtime`) when this
    /// generation does not hold the capture for `handle`.
    pub fn poll(
        &mut self,
        plugin_id: &str,
        handle: i64,
        max: usize,
    ) -> Result<Vec<OverlayInput>, BridgeError> {
        if !self.is_owner(plugin_id, handle) {
            return Err(BridgeError::new(
                "runtime",
                E_UI_NOT_OWNER,
                "no focusable-overlay input capture is held for this handle",
            ));
        }
        self.refresh_owner(plugin_id, handle);
        let take = max.min(self.queue.len());
        let drained: Vec<OverlayInput> = self.queue.drain(..take).collect();
        if let Some(last) = drained.last() {
            self.last_delivered = last.sequence;
        }
        Ok(drained)
    }

    /// Detailed poll per the accepted W-01 contract.
    ///
    /// While the caller owns the session the result is `active` with drained
    /// events (empty when there is nothing new) and the idle deadline is
    /// refreshed. After any terminal cause the owner's next poll reports
    /// `released` with the exact reason and no events. A non-owner, or a
    /// stale-generation handle that matches no live or remembered session,
    /// fails with [`E_UI_NOT_OWNER`] and changes nothing.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with code [`E_UI_NOT_OWNER`] (class `runtime`) when the
    /// caller owns neither the live session nor the remembered release.
    pub fn poll_detailed(
        &mut self,
        plugin_id: &str,
        handle: i64,
        max: usize,
    ) -> Result<CapturePoll, BridgeError> {
        if self.is_owner(plugin_id, handle) {
            self.refresh_owner(plugin_id, handle);
            let take = max.min(self.queue.len());
            let drained: Vec<OverlayInput> = self.queue.drain(..take).collect();
            if let Some(last) = drained.last() {
                self.last_delivered = last.sequence;
            }
            return Ok(CapturePoll {
                status: "active",
                seq: self.last_delivered,
                events: drained,
                overflowed: self.overflowed,
                reason: None,
            });
        }
        if self.owns_release(plugin_id, handle) {
            let record = self.last_release.as_ref().expect("checked release");
            return Ok(CapturePoll {
                status: "released",
                seq: record.seq,
                events: Vec::new(),
                overflowed: record.overflowed,
                reason: Some(record.reason.clone()),
            });
        }
        Err(BridgeError::new(
            "runtime",
            E_UI_NOT_OWNER,
            "no focusable-overlay input capture is held for this handle",
        ))
    }

    /// Refresh the idle deadline of the owned session.
    ///
    /// The capture uses a sliding idle timeout: any input event, poll, or
    /// update extends the deadline by [`OVERLAY_CAPTURE_TIMEOUT_MS`], so an
    /// actively used session survives while an abandoned one is revoked.
    /// Returns whether the caller owns the live session.
    pub fn refresh_owner(&mut self, plugin_id: &str, handle: i64) -> bool {
        match self.owner.as_mut() {
            Some(owner) if owner.plugin_id == plugin_id && owner.handle == handle => {
                owner.expires_at =
                    Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS);
                true
            }
            _ => false,
        }
    }

    /// Append one captured input event for the active owner.
    ///
    /// This runs on the Core input path and never invokes plugin code
    /// (`P0-AC-015`). Returns `false` when no capture is active; otherwise the
    /// event is queued (dropping the oldest and counting it on overflow).
    pub fn enqueue(&mut self, kind: &str, text: &str) -> bool {
        if self.owner.is_none() {
            return false;
        }
        if self.queue.len() >= OVERLAY_CAPTURE_QUEUE_MAX {
            self.queue.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            self.overflowed = true;
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.queue.push_back(OverlayInput {
            sequence: self.next_sequence,
            kind: bounded_kind(kind),
            text: bounded_text(text),
        });
        // Captured input proves the session is live: extend the idle
        // deadline so fast typing never kills its own session.
        if let Some(owner) = self.owner.as_mut() {
            owner.expires_at = Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS);
        }
        true
    }

    /// Enqueue a pointer-motion event, coalescing with the trailing queued
    /// motion instead of appending.
    ///
    /// A mouse wiggle produces dozens of motion events; appending each one
    /// would evict key/text entries via drop-oldest before the plugin drains
    /// them (CodeRabbit PR #1643). Coalescing keeps the latest position while
    /// consuming exactly one queue slot. Returns `false` when no capture is
    /// active.
    pub fn enqueue_move(&mut self, text: &str) -> bool {
        if self.owner.is_none() {
            return false;
        }
        if let Some(back) = self.queue.back_mut() {
            if back.kind == "pointer" && back.text.starts_with("move:") {
                self.next_sequence = self.next_sequence.saturating_add(1);
                back.sequence = self.next_sequence;
                back.text = bounded_text(text);
                if let Some(owner) = self.owner.as_mut() {
                    owner.expires_at =
                        Instant::now() + Duration::from_millis(OVERLAY_CAPTURE_TIMEOUT_MS);
                }
                return true;
            }
        }
        self.enqueue("pointer", text)
    }

    /// Absolute monotonic deadline of the active capture, if any.
    ///
    /// The application arms its idle wake on this instant so an idle window
    /// still ticks (and expires the capture) at the deadline instead of
    /// holding it until unrelated activity (CodeRabbit PR #1643).
    pub fn expiry_deadline(&self) -> Option<Instant> {
        self.owner.as_ref().map(|owner| owner.expires_at)
    }
    /// Revoke the capture if `plugin_id` owns it (suspend, dispose, unload,
    /// disable, crash). Returns whether a capture was dropped. Records the
    /// `unloaded` reason for the owner's next poll.
    pub fn revoke_plugin(&mut self, plugin_id: &str) -> bool {
        self.revoke_plugin_with_reason(plugin_id, UNLOADED_RELEASE_REASON)
    }

    /// Revoke with an explicit terminal reason (one of [`RELEASE_REASONS`).
    pub fn revoke_plugin_with_reason(&mut self, plugin_id: &str, reason: &str) -> bool {
        let owned = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.plugin_id == plugin_id);
        if owned {
            self.finish_session(reason);
        }
        owned
    }

    /// Revoke the capture if it has reached its deadline at `now` (Core-side
    /// timeout). Returns whether a capture was dropped. Records the `timeout`
    /// reason for the owner's next poll.
    pub fn revoke_expired_at(&mut self, now: Instant) -> bool {
        let expired = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.expires_at <= now);
        if expired {
            self.finish_session(TIMEOUT_RELEASE_REASON);
        }
        expired
    }

    /// Drain pending `overlay.released` bus observations in order.
    pub fn drain_released_events(&mut self) -> Vec<ReleasedEvent> {
        std::mem::take(&mut self.pending_released)
    }

    /// Rewind the owned capture's deadline into the past (test-only).
    ///
    /// Integration tests use this to prove the Core tick revokes a capture
    /// that has reached its deadline without waiting [`OVERLAY_CAPTURE_TIMEOUT_MS`].
    /// Returns whether the owner matched and was rewound.
    #[doc(hidden)]
    pub fn force_expire(&mut self, plugin_id: &str, handle: i64) -> bool {
        match self.owner.as_mut() {
            Some(owner) if owner.plugin_id == plugin_id && owner.handle == handle => {
                owner.expires_at = Instant::now() - std::time::Duration::from_millis(1);
                true
            }
            _ => false,
        }
    }

    /// Whether (`plugin_id`, `handle`) currently owns the capture.
    #[must_use]
    pub fn is_owner(&self, plugin_id: &str, handle: i64) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.plugin_id == plugin_id && owner.handle == handle)
    }

    /// Whether `handle` is the live session's handle, regardless of owner.
    ///
    /// Release uses this to deny ending somebody else's live session while
    /// keeping every other non-owned handle an ok-noop (already-ended,
    /// never-acquired, or stale from before another session started).
    #[must_use]
    pub fn is_live_handle(&self, handle: i64) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.handle == handle)
    }

    /// Whether (`plugin_id`, `handle`) owns the remembered terminal release.
    ///
    /// Used by detailed poll: only the owning generation observes the
    /// terminal record of its ended session. (Release idempotency intentionally
    /// does not consult the remembered release: with no live session a
    /// release is an ok-noop, while a live foreign session denies.)
    #[must_use]
    pub fn owns_release(&self, plugin_id: &str, handle: i64) -> bool {
        self.last_release
            .as_ref()
            .is_some_and(|record| record.plugin_id == plugin_id && record.handle == handle)
    }

    /// Whether any capture is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.owner.is_some()
    }

    /// Plugin id of the active owner, if any (diagnostics and tests).
    #[must_use]
    pub fn owner_plugin(&self) -> Option<&str> {
        self.owner.as_ref().map(|owner| owner.plugin_id.as_str())
    }

    /// Handle of the active capture, if any (block-lifetime wiring).
    ///
    /// Paired with [`OverlayCapture::owner_plugin`]: the runtime uses both
    /// to dispose a spec-acquired surface when its session ends on a
    /// runtime-driven path (expiry, focus-switch/cancel revoke).
    #[must_use]
    pub fn owner_handle(&self) -> Option<i64> {
        self.owner.as_ref().map(|owner| owner.handle)
    }

    /// Number of queued, undrained events.
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Number of events dropped by queue overflow since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether the live session has overflowed its queue (sticky flag).
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Number of pending `overlay.released` bus observations.
    #[must_use]
    pub fn pending_released_len(&self) -> usize {
        self.pending_released.len()
    }

    /// Number of `overlay.released` observations dropped by buffer overflow
    /// since creation (drop-oldest past [`PENDING_RELEASED_MAX`]).
    #[must_use]
    pub fn released_dropped(&self) -> u64 {
        self.released_dropped
    }

    /// Drop the owner and every queued event (transient session end).
    fn clear(&mut self) {
        self.owner = None;
        self.queue.clear();
    }

    /// End the live session with `reason`: remember the terminal record for
    /// the owner's next detailed poll, queue the bus observation (dropping
    /// the oldest past [`PENDING_RELEASED_MAX`]), and drop the owner and
    /// every queued event.
    fn finish_session(&mut self, reason: &str) {
        if let Some(owner) = self.owner.as_ref() {
            let record = ReleaseRecord {
                plugin_id: owner.plugin_id.clone(),
                handle: owner.handle,
                reason: reason.to_string(),
                seq: self.last_delivered,
                overflowed: self.overflowed,
            };
            if self.pending_released.len() >= PENDING_RELEASED_MAX {
                self.pending_released.remove(0);
                self.released_dropped = self.released_dropped.saturating_add(1);
            }
            self.pending_released.push(ReleasedEvent {
                owner: owner.plugin_id.clone(),
                reason: reason.to_string(),
            });
            self.last_release = Some(record);
        }
        self.clear();
    }
}

/// Bound one event class token to a short ASCII-safe label.
fn bounded_kind(kind: &str) -> String {
    kind.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(32)
        .collect()
}

/// Truncate a captured payload at a UTF-8 char boundary to
/// [`OVERLAY_CAPTURE_TEXT_MAX_BYTES`].
fn bounded_text(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > OVERLAY_CAPTURE_TEXT_MAX_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn future(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn acquire_guards_a_single_owner() {
        let mut capture = OverlayCapture::new();
        capture
            .acquire("a", 1, future(1000))
            .expect("first acquire");
        assert!(capture.is_owner("a", 1));
        let error = capture
            .acquire("a", 1, future(1000))
            .expect_err("second acquire must fail");
        assert_eq!(error.code, E_UI_ALREADY_CAPTURED);
        assert_eq!(error.class, "runtime");
        let error = capture
            .acquire("b", 2, future(1000))
            .expect_err("other plugin must fail");
        assert_eq!(error.code, E_UI_ALREADY_CAPTURED);
    }

    #[test]
    fn release_is_idempotent_and_foreign_safe() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        assert!(!capture.release("b", 1), "foreign release is a no-op");
        assert!(capture.release("a", 1), "first release drops the capture");
        assert!(!capture.release("a", 1), "second release is a no-op");
        assert!(!capture.is_active());
    }

    #[test]
    fn poll_requires_ownership_and_drains_oldest_first() {
        let mut capture = OverlayCapture::new();
        assert_eq!(
            capture
                .poll("a", 1, 8)
                .expect_err("poll without capture must fail")
                .code,
            E_UI_NOT_OWNER
        );
        capture.acquire("a", 1, future(1000)).expect("acquire");
        assert!(capture.enqueue("text", "one"));
        assert!(capture.enqueue("key", "two"));
        let drained = capture.poll("a", 1, 8).expect("owned poll");
        assert_eq!(
            drained.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(drained[0].sequence, 1);
        assert_eq!(drained[1].sequence, 2);
        assert_eq!(capture.queued_len(), 0);
    }

    #[test]
    fn enqueue_without_owner_is_a_no_op() {
        let mut capture = OverlayCapture::new();
        assert!(!capture.enqueue("text", "x"));
        assert_eq!(capture.queued_len(), 0);
    }

    #[test]
    fn queue_overflow_drops_the_oldest_and_counts_it() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        for index in 0..=OVERLAY_CAPTURE_QUEUE_MAX {
            assert!(capture.enqueue("text", &format!("{index}")));
        }
        assert_eq!(capture.queued_len(), OVERLAY_CAPTURE_QUEUE_MAX);
        assert_eq!(capture.dropped(), 1);
        let drained = capture
            .poll("a", 1, OVERLAY_CAPTURE_QUEUE_MAX)
            .expect("poll");
        assert_eq!(drained[0].text, "1", "oldest ('0') must be dropped");
    }

    #[test]
    fn text_is_bounded_at_the_event_limit() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        let long = "x".repeat(OVERLAY_CAPTURE_TEXT_MAX_BYTES * 2);
        assert!(capture.enqueue("text", &long));
        let drained = capture.poll("a", 1, 1).expect("poll");
        assert_eq!(drained[0].text.len(), OVERLAY_CAPTURE_TEXT_MAX_BYTES);
    }

    #[test]
    fn revoke_plugin_drops_owner_and_queue() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        assert!(capture.enqueue("text", "x"));
        assert!(!capture.revoke_plugin("b"));
        assert!(capture.revoke_plugin("a"));
        assert!(!capture.is_active());
        assert_eq!(capture.queued_len(), 0);
    }

    #[test]
    fn timeout_revokes_at_the_deadline() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        assert!(!capture.revoke_expired_at(Instant::now()));
        assert!(capture.revoke_expired_at(future(1000)));
        assert!(!capture.is_active());
    }

    #[test]
    fn expired_owner_is_replaced_on_the_next_acquire() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, Instant::now()).expect("acquire");
        capture
            .acquire("b", 2, future(1000))
            .expect("an expired owner must not block a new acquire");
        assert!(capture.is_owner("b", 2));
    }

    #[test]
    fn motion_coalesces_into_one_slot() {
        let mut capture = OverlayCapture::new();
        assert!(!capture.enqueue_move("move:1,1"), "no owner queues nothing");
        capture.acquire("a", 1, future(1000)).expect("acquire");
        assert!(capture.enqueue("key", "x"));
        for (x, y) in [(1, 1), (2, 2), (3, 3)] {
            assert!(capture.enqueue_move(&format!("move:{x},{y}")));
        }
        assert_eq!(capture.queued_len(), 2, "key + one coalesced move");
        // A non-move pointer entry breaks the run: the next move appends.
        assert!(capture.enqueue("pointer", "Left:Pressed"));
        assert!(capture.enqueue_move("move:4,4"));
        assert_eq!(capture.queued_len(), 4);
    }

    #[test]
    fn deadline_tracks_the_active_owner() {
        let mut capture = OverlayCapture::new();
        assert_eq!(capture.expiry_deadline(), None);
        let deadline = future(1000);
        capture.acquire("a", 1, deadline).expect("acquire");
        assert_eq!(capture.expiry_deadline(), Some(deadline));
        assert!(capture.release("a", 1));
        assert_eq!(capture.expiry_deadline(), None);
    }

    #[test]
    fn detailed_poll_reports_active_then_released_with_reason() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(60_000)).expect("acquire");
        assert!(capture.enqueue("key", "k"));
        let live = capture.poll_detailed("a", 1, 8).expect("active poll");
        assert_eq!(live.status, "active");
        assert_eq!(live.seq, 1);
        assert_eq!(live.events.len(), 1);
        assert!(!live.overflowed);
        assert_eq!(live.reason, None);
        assert!(capture.release_with_reason("a", 1, "submitted"));
        let after = capture.poll_detailed("a", 1, 8).expect("released poll");
        assert_eq!(after.status, "released");
        assert_eq!(after.seq, 1);
        assert!(after.events.is_empty());
        assert_eq!(after.reason.as_deref(), Some("submitted"));
        // A non-owner never observes the release: stale handles fail closed.
        assert_eq!(
            capture
                .poll_detailed("b", 1, 8)
                .expect_err("foreign poll must fail")
                .code,
            E_UI_NOT_OWNER
        );
    }

    #[test]
    fn overflow_sets_the_sticky_flag_and_keeps_the_session() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(60_000)).expect("acquire");
        for index in 0..=OVERLAY_CAPTURE_QUEUE_MAX {
            assert!(capture.enqueue("text", &format!("{index}")));
        }
        assert!(capture.overflowed());
        let live = capture
            .poll_detailed("a", 1, OVERLAY_CAPTURE_QUEUE_MAX)
            .expect("poll");
        assert_eq!(live.status, "active");
        assert!(live.overflowed);
        assert_eq!(live.events[0].text, "1");
    }

    #[test]
    fn activity_extends_the_idle_deadline() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(1000)).expect("acquire");
        let before = capture.expiry_deadline().expect("deadline");
        assert!(capture.enqueue("key", "k"), "input is activity");
        assert!(
            capture.expiry_deadline().expect("deadline") >= before,
            "captured input extends the deadline"
        );
        assert!(capture.refresh_owner("a", 1), "poll-equivalent refresh");
        assert!(!capture.refresh_owner("b", 1), "foreign refresh is a no-op");
    }

    #[test]
    fn every_terminal_path_queues_exactly_one_bus_event() {
        let mut capture = OverlayCapture::new();
        capture.acquire("a", 1, future(60_000)).expect("acquire");
        assert!(capture.release_with_reason("a", 1, "cancelled"));
        capture.acquire("b", 2, future(60_000)).expect("re-acquire");
        assert!(capture.revoke_plugin_with_reason("b", "unloaded"));
        capture.acquire("c", 3, future(60_000)).expect("re-acquire");
        assert!(capture.revoke_expired_at(future(120_000)));
        let events = capture.drain_released_events();
        assert_eq!(
            events
                .iter()
                .map(|event| (event.owner.as_str(), event.reason.as_str()))
                .collect::<Vec<_>>(),
            [("a", "cancelled"), ("b", "unloaded"), ("c", "timeout")]
        );
        assert!(capture.drain_released_events().is_empty());
    }

    #[test]
    fn pending_released_is_bounded_with_drop_oldest() {
        // An acquire/release loop inside one tick (no drain between
        // sessions) must stay bounded: the oldest observations drop and are
        // counted, and the newest survive for the next tick's delivery.
        let mut capture = OverlayCapture::new();
        let rounds = PENDING_RELEASED_MAX + 32;
        for _ in 0..rounds {
            capture.acquire("a", 1, future(60_000)).expect("acquire");
            assert!(capture.release("a", 1));
        }
        assert_eq!(capture.pending_released_len(), PENDING_RELEASED_MAX);
        assert_eq!(
            capture.released_dropped(),
            u64::try_from(rounds - PENDING_RELEASED_MAX).expect("count fits")
        );
        let events = capture.drain_released_events();
        assert_eq!(events.len(), PENDING_RELEASED_MAX);
        assert!(
            events.iter().all(|event| event.owner == "a"),
            "newest observations survive the bound"
        );
        assert!(capture.drain_released_events().is_empty());
    }
}
