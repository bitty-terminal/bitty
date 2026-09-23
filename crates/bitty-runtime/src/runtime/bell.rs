//! Terminal bell and notification policy (CTX-0577, M1-16 / issue #1142).
//!
//! Untrusted PTY output can emit `BEL`, `OSC 9`, and `OSC 777` at
//! child-output rate. Without a policy those bytes would drive an unbounded,
//! user-visible surface (or an unbounded queue). This module holds the
//! policy, the bounds, and the rate limiter; the owning runtime applies them
//! on the PTY path and the present path.
//!
//! Policy summary (owner-pending: `OQ-076`, see
//! `specifications/bell-notification-policy.md`):
//!
//! - **Bell (`BEL`)**: visual flash by default ([`BellMode::Visual`]), never
//!   audible by default. An audible request rings the installed
//!   [`bitty_platform::BellSink`] (the app installs the best-effort OS
//!   primitive for real runs); with no sink installed the request is only
//!   counted, so headless runs stay silent.
//! - **`OSC 9` / `OSC 777` notifications**: default **deny**; an embedder
//!   must opt in with `Runtime::set_osc_notification_allowed`.
//! - **Rate**: both surfaces are governed by the accepted `RC-8`
//!   notification/title/metadata ceiling (10 events/s, coalesced); excess
//!   events are dropped and counted, never queued without bound.
//! - **Presentation**: a single bounded banner (one notification at a time)
//!   and a single bounded flash; no per-event surface accumulates.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bitty_vt::Notification;

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

    const NOTIFICATION_MAX_PROBE: usize = 400;
}
