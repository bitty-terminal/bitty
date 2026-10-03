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
use std::time::Instant;

use bitty_lua::{
    BridgeError, E_UI_ALREADY_CAPTURED, E_UI_NOT_OWNER, OVERLAY_CAPTURE_QUEUE_MAX,
    OVERLAY_CAPTURE_TEXT_MAX_BYTES, OverlayInput,
};

/// Core-side capture session timeout in milliseconds (CTX-0941).
///
/// A capture that outlives this bound is revoked by Core on the next tick, so
/// a blocked or silent plugin cannot pin terminal input.
pub const OVERLAY_CAPTURE_TIMEOUT_MS: u64 = 30_000;

/// The active capture owner for one runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureOwner {
    plugin_id: String,
    handle: i64,
    /// Absolute monotonic deadline; Core revokes at or after this instant.
    expires_at: Instant,
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
            self.clear();
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
        Ok(())
    }

    /// Release the capture owned by (`plugin_id`, `handle`).
    ///
    /// Idempotent: returns `true` only when it actually dropped an owned
    /// capture; a foreign or repeated release returns `false` and never errors.
    pub fn release(&mut self, plugin_id: &str, handle: i64) -> bool {
        let owned = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.plugin_id == plugin_id && owner.handle == handle);
        if owned {
            self.clear();
        }
        owned
    }

    /// Drain up to `max` queued events for the capture owned by
    /// (`plugin_id`, `handle`), oldest first.
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
        let take = max.min(self.queue.len());
        Ok(self.queue.drain(..take).collect())
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
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.queue.push_back(OverlayInput {
            sequence: self.next_sequence,
            kind: bounded_kind(kind),
            text: bounded_text(text),
        });
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
    /// disable, crash). Returns whether a capture was dropped.
    pub fn revoke_plugin(&mut self, plugin_id: &str) -> bool {
        let owned = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.plugin_id == plugin_id);
        if owned {
            self.clear();
        }
        owned
    }

    /// Revoke the capture if it has reached its deadline at `now` (Core-side
    /// timeout). Returns whether a capture was dropped.
    pub fn revoke_expired_at(&mut self, now: Instant) -> bool {
        let expired = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.expires_at <= now);
        if expired {
            self.clear();
        }
        expired
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

    /// Drop the owner and every queued event (transient session end).
    fn clear(&mut self) {
        self.owner = None;
        self.queue.clear();
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
}
