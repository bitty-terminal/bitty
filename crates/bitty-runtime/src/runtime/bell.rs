//! Terminal bell and notification policy (CTX-0577, M1-16 / issue #1142;
//! CTX-1008 adds Kitty `OSC 99` for issue #1763).
//!
//! Untrusted PTY output can emit `BEL`, `OSC 9`, `OSC 777`, and Kitty
//! `OSC 99` at child-output rate. Without a policy those bytes would drive an
//! unbounded, user-visible surface (or an unbounded queue). This module holds
//! the policy, the bounds, and the rate limiter; the owning runtime applies them
//! on the PTY path and the present path.
//!
//! Policy summary (owner-pending: `OQ-076`; canonical notification sections in
//! the [platform-services contract](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/platform-services-contract.md)):
//!
//! - **Bell (`BEL`)**: visual flash by default ([`BellMode::Visual`]), never
//!   audible by default. An audible request rings the installed
//!   [`bitty_platform::BellSink`] (the app installs the best-effort OS
//!   primitive for real runs); with no sink installed the request is only
//!   counted, so headless runs stay silent.
//! - **`OSC 9` / `OSC 777` / Kitty `OSC 99` notifications**: default **deny**;
//!   an embedder must opt in with `Runtime::set_osc_notification_allowed`.
//! - **Rate**: all surfaces are governed by the accepted `RC-8`
//!   notification/title/metadata ceiling (10 events/s, coalesced); excess
//!   events are dropped and counted, never queued without bound.
//! - **Presentation**: a single bounded banner (one notification at a time)
//!   and a single bounded flash; no per-event surface accumulates.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use bitty_vt::{KittyNotificationChunk, KittyPayloadType, Notification, NotificationSource};

/// Accepted `RC-8` ceiling: admitted events per fixed window.
pub const RC8_EVENTS_PER_WINDOW: u32 = 10;

/// Accepted `RC-8` fixed window (one second).
pub const RC8_WINDOW: Duration = Duration::from_secs(1);

/// How long the visual bell flash stays on screen.
///
/// Short and self-expiring: the flash is a bounded signal, not a persistent
/// surface, and it never stacks (an admitted bell refreshes the deadline).
pub const BELL_FLASH_DURATION: Duration = Duration::from_millis(120);

/// Capacity of the bounded terminal-notification queue.
///
/// Admitted notifications queue here until the present path shows them; the
/// oldest is shown first and overflow drops the newest (counted), so hostile
/// output can never grow memory without limit.
pub const NOTIFICATION_QUEUE_CAPACITY: usize = 8;

/// Maximum characters retained per notification field for display.
///
/// The parser already length-bounds payloads; this is the display bound so a
/// single banner stays one bounded line.
pub const NOTIFICATION_TEXT_MAX_CHARS: usize = 256;

/// How long one notification banner is shown before the next is presented.
pub const NOTIFICATION_BANNER_DURATION: Duration = Duration::from_secs(4);

/// Maximum Kitty `OSC 99` partial groups buffered while `d=0` chunks arrive.
///
/// Chunked notifications assemble by `i=` identifier; each open group pins
/// its title/body text. Eight groups mirror the notification queue bound so
/// a hostile writer opening many groups can never grow memory without limit;
/// the oldest group is evicted (counted) when a ninth identifier arrives.
pub const KITTY_PARTIALS_CAPACITY: usize = 8;

/// Maximum characters retained per Kitty `OSC 99` assembled side.
///
/// Individual chunks are already bounded by the parser; concatenation across
/// chunks is re-bounded here so a writer sending many `d=0` chunks cannot
/// grow one notification without limit. The display sanitizer re-bounds to
/// [`NOTIFICATION_TEXT_MAX_CHARS`] before painting.
pub const KITTY_ASSEMBLED_MAX_CHARS: usize = 1024;

/// User-visible bell behavior (CTX-0577 `OQ-076` policy input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BellMode {
    /// No bell surface at all.
    Off,
    /// Visual flash only (the bounded default).
    #[default]
    Visual,
    /// Audible request only (rings the installed sink; counted-only when no
    /// sink is installed, so the default headless runtime stays silent).
    Audible,
    /// Visual flash plus audible request.
    Both,
}

impl BellMode {
    /// Whether this mode paints the visual flash.
    #[must_use]
    pub const fn visual(self) -> bool {
        matches!(self, Self::Visual | Self::Both)
    }

    /// Whether this mode records an audible request.
    #[must_use]
    pub const fn audible(self) -> bool {
        matches!(self, Self::Audible | Self::Both)
    }
}

/// Fixed-window `RC-8` rate limiter: at most `limit` admissions per `window`.
///
/// Deterministic via the caller-supplied [`Instant`] so tests never depend on
/// wall-clock timing. A backwards clock can never re-open a window early
/// (`saturating_duration_since`). The caller counts its own drops.
#[derive(Debug, Clone)]
pub(crate) struct Rc8Limiter {
    window: Duration,
    limit: u32,
    window_start: Option<Instant>,
    admitted: u32,
}

impl Rc8Limiter {
    /// New limiter admitting `limit` events per `window` (a zero limit admits
    /// nothing, so the surface stays fail-closed).
    pub(crate) const fn new(window: Duration, limit: u32) -> Self {
        Self {
            window,
            limit,
            window_start: None,
            admitted: 0,
        }
    }

    /// Whether one more event is admitted at `now`.
    pub(crate) fn admit_at(&mut self, now: Instant) -> bool {
        let expired = match self.window_start {
            None => true,
            Some(start) => now.saturating_duration_since(start) >= self.window,
        };
        if expired {
            self.window_start = Some(now);
            self.admitted = 0;
        }
        if self.admitted < self.limit {
            self.admitted += 1;
            true
        } else {
            false
        }
    }
}

/// Bounded queue of admitted terminal notifications (oldest shown first).
#[derive(Debug)]
pub(crate) struct TerminalNotificationQueue {
    items: VecDeque<Notification>,
    capacity: usize,
    dropped: u64,
}

impl TerminalNotificationQueue {
    /// Queue with `capacity` slots (`capacity` is raised to 1).
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Push the newest notification; returns `false` (and counts a drop) when
    /// the queue is full. Overflow drops the newest so the bounded surface
    /// never loses the notification the user is most likely reading.
    pub(crate) fn push(&mut self, notification: Notification) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(notification);
        true
    }

    /// Remove and return the oldest queued notification.
    pub(crate) fn pop_front(&mut self) -> Option<Notification> {
        self.items.pop_front()
    }

    /// Number of queued notifications.
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Notifications dropped to overflow since creation.
    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Origin PTY stream of one Kitty `OSC 99` chunk (CTX-1011, issue #1775).
///
/// The assembler is shared by the primary and every pane PTY stream, but a
/// pending `d=0` group from one stream must never be completed by a chunk
/// from another stream. Streams are therefore part of the assembly key:
/// `Primary` is the runtime-global primary grid drain (`kitty_origin`
/// `None`); `Pane(raw)` is the split-pane drain tagged with that pane's
/// origin token (`kitty_origin` `Some(raw)`, the `ViewId.0` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum KittyStreamId {
    /// Primary PTY stream (no pane origin).
    #[default]
    Primary,
    /// Split-pane PTY stream, tagged by origin token (`ViewId.0`).
    Pane(u64),
}

impl KittyStreamId {
    /// Derives the stream from the drain origin tag (`None` primary).
    #[must_use]
    pub(crate) const fn from_origin(origin: Option<u64>) -> Self {
        match origin {
            None => Self::Primary,
            Some(raw) => Self::Pane(raw),
        }
    }
}

/// One open Kitty `OSC 99` group: concatenated title/body text for one
/// `(stream, session epoch, i=)` key with more chunks expected (`d=0` seen,
/// completion pending).
#[derive(Debug, Clone, Default)]
struct KittyPartial {
    /// Origin stream that opened the group.
    stream: KittyStreamId,
    /// Session epoch of `stream` when the group opened (bumped on every
    /// [`KittyNotificationAssembler::discard_stream`] / [`KittyNotificationAssembler::clear`],
    /// so a later session reusing the same stream token can never match a
    /// stale group even if a discard were missed).
    epoch: u64,
    /// Group identifier (empty when the wire omitted `i=`).
    id: String,
    /// Concatenated title chunks in arrival order, bounded.
    title: String,
    /// Concatenated body chunks in arrival order, bounded.
    body: String,
}

impl KittyPartial {
    /// Appends chunk text to the matching side, truncating the side to
    /// [`KITTY_ASSEMBLED_MAX_CHARS`] characters so chunked reassembly stays
    /// bounded. Time O(chunk), space O(bound).
    fn append(&mut self, chunk: &KittyNotificationChunk) {
        let side = match chunk.payload_type {
            KittyPayloadType::Title => &mut self.title,
            KittyPayloadType::Body => &mut self.body,
        };
        side.push_str(chunk.payload.as_str());
        if side.chars().count() > KITTY_ASSEMBLED_MAX_CHARS {
            let truncated: String = side.chars().take(KITTY_ASSEMBLED_MAX_CHARS).collect();
            *side = truncated;
        }
    }
}

/// Bounded assembler for chunked Kitty `OSC 99` notifications (CTX-1008;
/// CTX-1011 session-binds groups for issue #1775).
///
/// Chunks assemble by `(stream, session epoch, i=)`: `d=0` buffers, `d=1`
/// completes and emits. The stream is the originating PTY drain (primary vs.
/// pane origin token); the epoch is bumped every time a stream's groups are
/// discarded (pane close/replacement, primary respawn, consent revocation),
/// so a later session reusing the same stream token starts empty and can
/// never complete stale text. At most [`KITTY_PARTIALS_CAPACITY`] groups stay
/// open across all streams; a ninth group evicts the oldest (counted).
/// Completion with no text yields no notification (ignored, not queued).
/// Consent revocation discards all buffered groups (counted in
/// [`Self::discarded`]): re-enabling starts empty. All text remains
/// untrusted display data: the runtime sanitizes at banner time and never
/// executes or expands it.
#[derive(Debug, Default)]
pub(crate) struct KittyNotificationAssembler {
    partials: VecDeque<KittyPartial>,
    evicted: u64,
    discarded: u64,
    epochs: HashMap<KittyStreamId, u64>,
}

impl KittyNotificationAssembler {
    /// Empty assembler with no open groups.
    pub(crate) fn new() -> Self {
        Self {
            partials: VecDeque::with_capacity(KITTY_PARTIALS_CAPACITY),
            evicted: 0,
            discarded: 0,
            epochs: HashMap::new(),
        }
    }

    /// Current session epoch for `stream` (`0` before any discard).
    fn epoch_for(&self, stream: KittyStreamId) -> u64 {
        self.epochs.get(&stream).copied().unwrap_or(0)
    }

    /// Pushes one parsed chunk from `stream`; returns a completed
    /// [`Notification`] when this chunk closes its group (`is_done`) and the
    /// group is non-empty.
    ///
    /// Intermediate (`!is_done`) chunks buffer and return `None`. A
    /// single-chunk (`is_done`) notification with no prior partial emits
    /// directly without buffering. Only the same `(stream, epoch, i=)` group
    /// is matched: a completing chunk from another stream or a later session
    /// emits from its own payload alone and leaves other streams' groups
    /// untouched. Groups evicted to capacity are counted in
    /// [`Self::evicted`].
    pub(crate) fn push_chunk(
        &mut self,
        stream: KittyStreamId,
        chunk: &KittyNotificationChunk,
    ) -> Option<Notification> {
        use bitty_vt::BoundedString;
        let epoch = self.epoch_for(stream);
        let key = chunk.id.as_str().to_owned();
        let position = self.partials.iter().position(|partial| {
            partial.stream == stream && partial.epoch == epoch && partial.id == key
        });
        if !chunk.is_done {
            match position {
                Some(index) => {
                    if let Some(partial) = self.partials.get_mut(index) {
                        partial.append(chunk);
                    }
                }
                None => {
                    if self.partials.len() >= KITTY_PARTIALS_CAPACITY {
                        self.partials.pop_front();
                        self.evicted = self.evicted.saturating_add(1);
                    }
                    let mut partial = KittyPartial {
                        stream,
                        epoch,
                        id: key,
                        title: String::new(),
                        body: String::new(),
                    };
                    partial.append(chunk);
                    self.partials.push_back(partial);
                }
            }
            return None;
        }
        // Completing chunk: combine any buffered text with this chunk.
        let mut title = String::new();
        let mut body = String::new();
        if let Some(index) = position {
            if let Some(partial) = self.partials.remove(index) {
                title = partial.title;
                body = partial.body;
            }
        }
        match chunk.payload_type {
            KittyPayloadType::Title => {
                title.push_str(chunk.payload.as_str());
            }
            KittyPayloadType::Body => {
                body.push_str(chunk.payload.as_str());
            }
        }
        // Re-bound the assembled sides (append already bounds partials; the
        // final chunk can still push over).
        if title.chars().count() > KITTY_ASSEMBLED_MAX_CHARS {
            title = title.chars().take(KITTY_ASSEMBLED_MAX_CHARS).collect();
        }
        if body.chars().count() > KITTY_ASSEMBLED_MAX_CHARS {
            body = body.chars().take(KITTY_ASSEMBLED_MAX_CHARS).collect();
        }
        if title.is_empty() && body.is_empty() {
            return None;
        }
        let title_bounded = if title.is_empty() {
            None
        } else {
            Some(BoundedString::new(title))
        };
        Some(Notification {
            source: NotificationSource::Osc99,
            title: title_bounded,
            body: BoundedString::new(body),
        })
    }

    /// Open groups currently buffered.
    pub(crate) fn len(&self) -> usize {
        self.partials.len()
    }

    /// Open groups buffered for `stream` in its current epoch.
    #[cfg(test)]
    pub(crate) fn len_for(&self, stream: KittyStreamId) -> usize {
        let epoch = self.epoch_for(stream);
        self.partials
            .iter()
            .filter(|partial| partial.stream == stream && partial.epoch == epoch)
            .count()
    }

    /// Groups evicted to capacity since creation.
    pub(crate) const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Groups discarded to session close/replacement or consent revocation
    /// since creation (CTX-1011).
    pub(crate) const fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Discards every buffered group for `stream` and advances its session
    /// epoch (CTX-1011, issue #1775).
    ///
    /// Called on pane close/replacement and on primary respawn: the session
    /// that opened the groups is gone, so a later session reusing the same
    /// stream token starts empty and can never complete stale text. Returns
    /// the number of groups dropped (counted in [`Self::discarded`]).
    /// Time O(groups), space O(1) besides the removal.
    pub(crate) fn discard_stream(&mut self, stream: KittyStreamId) -> usize {
        let before = self.partials.len();
        self.partials.retain(|partial| partial.stream != stream);
        let removed = before - self.partials.len();
        self.discarded = self.discarded.saturating_add(removed as u64);
        let next = self.epoch_for(stream).wrapping_add(1);
        self.epochs.insert(stream, next);
        removed
    }

    /// Discards every buffered group across all streams (CTX-1011).
    ///
    /// Consent-revocation semantics: revoking `osc_notification_allowed`
    /// invalidates already-buffered groups. Re-enabling starts empty; a
    /// later chunk can never complete text buffered before the revocation.
    /// Every stream that held a group (plus every previously seen stream)
    /// advances its epoch so stale epochs never match again. Returns the
    /// number of groups dropped (counted in [`Self::discarded`]).
    pub(crate) fn clear(&mut self) -> usize {
        let mut streams: Vec<KittyStreamId> = self.partials.iter().map(|p| p.stream).collect();
        streams.extend(self.epochs.keys().copied());
        streams.sort_by_key(|s| match *s {
            KittyStreamId::Primary => (0, 0),
            KittyStreamId::Pane(raw) => (1, raw),
        });
        streams.dedup();
        let removed = self.partials.len();
        self.partials.clear();
        self.discarded = self.discarded.saturating_add(removed as u64);
        for stream in streams {
            let next = self.epoch_for(stream).wrapping_add(1);
            self.epochs.insert(stream, next);
        }
        removed
    }
}

/// Strips control characters and bounds the length of untrusted display text.
///
/// Notification payloads are terminal-provided observation data. They are
/// never expanded or executed, and this keeps a single banner to one bounded,
/// single-line string (mirroring `sanitize_window_title`).
#[must_use]
pub fn sanitize_notification_text(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|ch| !ch.is_control())
        .take(NOTIFICATION_TEXT_MAX_CHARS)
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One-line banner text for a notification, sanitized and bounded.
///
/// `OSC 777` carries a title; `OSC 9` does not. An empty title degrades to
/// the body alone.
#[must_use]
pub fn notification_banner_text(notification: &Notification) -> String {
    let body = sanitize_notification_text(notification.body.as_str());
    let title = notification
        .title
        .as_ref()
        .map(|t| sanitize_notification_text(t.as_str()))
        .unwrap_or_default();
    if title.is_empty() {
        body
    } else if body.is_empty() {
        title
    } else {
        format!("{title}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_vt::{BoundedString, NotificationSource};

    fn osc9(body: &str) -> Notification {
        Notification {
            source: NotificationSource::Osc9,
            title: None,
            body: BoundedString::new(body),
        }
    }

    fn kitty_chunk(
        id: &str,
        payload_type: KittyPayloadType,
        payload: &str,
        is_done: bool,
    ) -> KittyNotificationChunk {
        KittyNotificationChunk {
            id: BoundedString::new(id),
            payload_type,
            payload: BoundedString::new(payload),
            is_done,
        }
    }

    #[test]
    fn limiter_admits_burst_then_drops_within_window() {
        let mut limiter = Rc8Limiter::new(RC8_WINDOW, RC8_EVENTS_PER_WINDOW);
        let t0 = Instant::now();
        for _ in 0..RC8_EVENTS_PER_WINDOW {
            assert!(limiter.admit_at(t0));
        }
        assert!(!limiter.admit_at(t0), "over-ceiling event must be dropped");
        // The next window admits again.
        assert!(limiter.admit_at(t0 + RC8_WINDOW));
    }

    #[test]
    fn limiter_backwards_clock_never_reopens_window() {
        let mut limiter = Rc8Limiter::new(RC8_WINDOW, 1);
        let t0 = Instant::now();
        assert!(limiter.admit_at(t0));
        assert!(!limiter.admit_at(t0.checked_sub(RC8_WINDOW).unwrap_or(t0)));
    }

    #[test]
    fn limiter_zero_limit_is_fail_closed() {
        let mut limiter = Rc8Limiter::new(RC8_WINDOW, 0);
        assert!(!limiter.admit_at(Instant::now()));
    }

    #[test]
    fn queue_is_bounded_and_drops_newest() {
        let mut queue = TerminalNotificationQueue::new(2);
        assert!(queue.push(osc9("one")));
        assert!(queue.push(osc9("two")));
        assert!(!queue.push(osc9("three")), "overflow drops newest");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop_front().unwrap().body.as_str(), "one");
    }

    #[test]
    fn sanitizer_strips_controls_and_bounds_length() {
        let raw = format!("a\u{1}b\nc\u{7}d {}", "x".repeat(NOTIFICATION_MAX_PROBE));
        let cleaned = sanitize_notification_text(&raw);
        assert!(!cleaned.contains('\u{1}'));
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\u{7}'));
        assert!(cleaned.chars().count() <= NOTIFICATION_TEXT_MAX_CHARS);
    }

    #[test]
    fn banner_text_uses_title_when_present() {
        let titled = Notification {
            source: NotificationSource::Osc777,
            title: Some(BoundedString::new("Build")),
            body: BoundedString::new("finished"),
        };
        assert_eq!(notification_banner_text(&titled), "Build: finished");
        assert_eq!(notification_banner_text(&osc9("plain")), "plain");
    }

    #[test]
    fn kitty_assembler_emits_single_chunk_directly() {
        let mut assembler = KittyNotificationAssembler::new();
        let notification = assembler
            .push_chunk(
                KittyStreamId::Primary,
                &kitty_chunk("", KittyPayloadType::Title, "Hello", true),
            )
            .expect("single done chunk must emit");
        assert_eq!(notification.source, NotificationSource::Osc99);
        assert_eq!(
            notification.title.as_ref().map(|t| t.as_str()),
            Some("Hello")
        );
        assert!(notification.body.is_empty());
        assert_eq!(assembler.len(), 0);
    }

    #[test]
    fn kitty_assembler_joins_title_and_body_by_id() {
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("7", KittyPayloadType::Title, "Hello", false)
                )
                .is_none(),
            "intermediate chunk buffers"
        );
        assert_eq!(assembler.len(), 1);
        let notification = assembler
            .push_chunk(
                KittyStreamId::Primary,
                &kitty_chunk("7", KittyPayloadType::Body, "World", true),
            )
            .expect("completing chunk must emit");
        assert_eq!(notification.source, NotificationSource::Osc99);
        assert_eq!(notification_banner_text(&notification), "Hello: World");
        assert_eq!(assembler.len(), 0);
    }

    #[test]
    fn kitty_assembler_empty_completion_is_ignored() {
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("x", KittyPayloadType::Title, "", true)
                )
                .is_none(),
            "empty notification must not emit"
        );
    }

    #[test]
    fn kitty_assembler_evicts_oldest_past_capacity() {
        let mut assembler = KittyNotificationAssembler::new();
        for index in 0..KITTY_PARTIALS_CAPACITY {
            assert!(
                assembler
                    .push_chunk(
                        KittyStreamId::Primary,
                        &kitty_chunk(
                            &format!("id{index}"),
                            KittyPayloadType::Title,
                            "part",
                            false
                        )
                    )
                    .is_none()
            );
        }
        assert_eq!(assembler.len(), KITTY_PARTIALS_CAPACITY);
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("overflow", KittyPayloadType::Title, "part", false)
                )
                .is_none()
        );
        assert_eq!(assembler.len(), KITTY_PARTIALS_CAPACITY);
        assert_eq!(assembler.evicted(), 1);
    }

    #[test]
    fn kitty_assembler_bounds_concatenated_text() {
        let mut assembler = KittyNotificationAssembler::new();
        let long = "y".repeat(KITTY_ASSEMBLED_MAX_CHARS + 64);
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("b", KittyPayloadType::Body, &long, false)
                )
                .is_none()
        );
        let notification = assembler
            .push_chunk(
                KittyStreamId::Primary,
                &kitty_chunk("b", KittyPayloadType::Body, "tail", true),
            )
            .expect("must emit bounded text");
        assert!(
            notification.body.as_str().chars().count() <= KITTY_ASSEMBLED_MAX_CHARS,
            "assembled body must stay bounded"
        );
    }

    #[test]
    fn kitty_assembler_isolates_streams_with_same_id() {
        // CTX-1011: a pending `d=0` group on one stream and a `d=1` chunk
        // with the same `i=` on another stream must not combine.
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("7", KittyPayloadType::Title, "PRIMARY-", false)
                )
                .is_none()
        );
        let pane_done = assembler
            .push_chunk(
                KittyStreamId::Pane(2),
                &kitty_chunk("7", KittyPayloadType::Body, "PANE", true),
            )
            .expect("other stream completes from its own payload alone");
        assert_eq!(notification_banner_text(&pane_done), "PANE");
        assert_eq!(assembler.len_for(KittyStreamId::Primary), 1);
        assert_eq!(assembler.len_for(KittyStreamId::Pane(2)), 0);
        // The primary group still completes from its own later chunk.
        let primary_done = assembler
            .push_chunk(
                KittyStreamId::Primary,
                &kitty_chunk("7", KittyPayloadType::Body, "PRIMARY-END", true),
            )
            .expect("primary group survives the cross-stream completion");
        assert_eq!(
            notification_banner_text(&primary_done),
            "PRIMARY-: PRIMARY-END"
        );
        assert_eq!(assembler.len(), 0);
    }

    #[test]
    fn kitty_assembler_isolates_empty_default_id_across_streams() {
        // The empty default `i=` is the most collision-prone key: it must
        // also stay stream-bound.
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("", KittyPayloadType::Title, "PRIMARY-", false)
                )
                .is_none()
        );
        let pane_done = assembler
            .push_chunk(
                KittyStreamId::Pane(9),
                &kitty_chunk("", KittyPayloadType::Body, "PANE", true),
            )
            .expect("empty id must stay stream-bound");
        assert_eq!(notification_banner_text(&pane_done), "PANE");
        assert_eq!(assembler.len(), 1);
    }

    #[test]
    fn kitty_assembler_discard_stream_drops_only_that_stream() {
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("7", KittyPayloadType::Title, "PRIMARY-", false)
                )
                .is_none()
        );
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Pane(2),
                    &kitty_chunk("7", KittyPayloadType::Title, "PANE-", false)
                )
                .is_none()
        );
        assert_eq!(assembler.len(), 2);
        assert_eq!(assembler.discard_stream(KittyStreamId::Pane(2)), 1);
        assert_eq!(assembler.discarded(), 1);
        assert_eq!(assembler.len_for(KittyStreamId::Primary), 1);
        assert_eq!(assembler.len_for(KittyStreamId::Pane(2)), 0);
        // A later pane chunk with the same `i=` starts fresh (no stale text).
        let fresh = assembler
            .push_chunk(
                KittyStreamId::Pane(2),
                &kitty_chunk("7", KittyPayloadType::Body, "FRESH", true),
            )
            .expect("post-discard completion emits alone");
        assert_eq!(notification_banner_text(&fresh), "FRESH");
    }

    #[test]
    fn kitty_assembler_replace_epoch_rejects_stale_completion() {
        // Pane close/replacement bumps the session epoch: the same
        // `(stream, i=)` after replacement can never complete pre-replace
        // text, even when the `ViewId` token is reused.
        let mut assembler = KittyNotificationAssembler::new();
        let pane = KittyStreamId::Pane(2);
        assert!(
            assembler
                .push_chunk(
                    pane,
                    &kitty_chunk("7", KittyPayloadType::Title, "STALE-", false)
                )
                .is_none()
        );
        assert_eq!(assembler.discard_stream(pane), 1);
        let fresh = assembler
            .push_chunk(
                pane,
                &kitty_chunk("7", KittyPayloadType::Body, "FRESH", true),
            )
            .expect("replacement session emits from its own payload");
        assert_eq!(notification_banner_text(&fresh), "FRESH");
        assert_eq!(assembler.len(), 0);
    }

    #[test]
    fn kitty_assembler_revocation_clear_invalidates_buffered() {
        // Consent-revocation semantics: `clear` drops buffered groups so a
        // later chunk starts empty.
        let mut assembler = KittyNotificationAssembler::new();
        assert!(
            assembler
                .push_chunk(
                    KittyStreamId::Primary,
                    &kitty_chunk("7", KittyPayloadType::Title, "BUFFERED-", false)
                )
                .is_none()
        );
        assert_eq!(assembler.clear(), 1);
        assert_eq!(assembler.discarded(), 1);
        assert_eq!(assembler.len(), 0);
        let fresh = assembler
            .push_chunk(
                KittyStreamId::Primary,
                &kitty_chunk("7", KittyPayloadType::Body, "FRESH", true),
            )
            .expect("post-revocation completion emits alone");
        assert_eq!(notification_banner_text(&fresh), "FRESH");
    }

    const NOTIFICATION_MAX_PROBE: usize = 400;
}
