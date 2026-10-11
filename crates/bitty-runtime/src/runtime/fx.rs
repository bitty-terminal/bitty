//! Plugin-requested chrome-ring pulse (CTX-1139, DEC-0110).
//!
//! Design-first slice of the effects runtime API: a single verb,
//! `bitty.fx.pulse`, lets a granted plugin ask Core to fade one live leaf's
//! Core-owned decoration ring from transparent to its committed color. The
//! plugin supplies only the target view id; Core owns the timing, the easing,
//! the budget, and the paint, so the accepted RFC-0002 rule that no plugin
//! animates Core-owned chrome holds by construction.
//!
//! Consistency with accepted RFC-0002 (deliberately unchanged):
//! only the Core-owned ring alpha moves (geometry, grid, cursor, and
//! scrollback are never interpolated); the closed easing enum is reused
//! (`EaseInOut`, no extension — new easings need an RFC review); the closed
//! transition set and the `appearance.animations` config grammar are
//! untouched (no new `AnimationKind`, so OQ-043 stays reserved); the fixed
//! 150 ms duration mirrors the panel-open default and sits under the existing
//! `enabled`/reduced-motion/safe-mode gates and the 500 ms ceiling; the
//! single shared 8-surface cap (`MAX_CONCURRENT_ANIMATIONS`) is enforced
//! bidirectionally at both the core and the fx trigger sites; frame-on-demand
//! is preserved (a deadline only while a pulse is active, zero wakeups after).
//!
//! Delivery follows the accepted intent pattern (`workspace_request`,
//! `set_pinned`): the Lua verb only enqueues a bounded [`FxPulseQueue`]
//! intent carrying the raw view id, and the application drains it once per
//! tick, revalidating each intent against the live layout. A vanished target,
//! an instant policy, or a full budget drops the intent with state untouched.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bitty_ui::ViewId;

use super::animations::{AnimationCurve, AnimationPolicy, MAX_CONCURRENT_ANIMATIONS};

/// Fixed pulse duration in ms (CTX-1139 slice).
///
/// Mirrors the accepted panel-open default. Fixed, not configured: per-view
/// animation fields stay reserved under OQ-043, so the slice adds no config
/// grammar. Trivially inside the RFC-0002 500 ms ceiling.
pub const FX_PULSE_DURATION_MS: u32 = 150;

/// Bound on queued `bitty.fx.pulse` intents across all plugins between two
/// app ticks (CTX-1139).
///
/// Same governance as the pin/workspace queues: small enough that a hostile
/// loop cannot queue unbounded work, large enough for a legitimate burst.
/// Overflow drops the newest intent, counts it, and the Lua call returns
/// `false`.
pub const FX_PULSE_QUEUE_CAPACITY: usize = 16;

/// One queued ring-pulse intent (CTX-1139).
///
/// Carries the raw Lua-validated view id only — no timing, easing, or
/// geometry. The drain revalidates the id against the live layout, so a view
/// that vanished between enqueue and drain fails closed with state untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedFxPulse {
    /// Lua-validated view id (a `ViewId` raw value).
    pub view: u64,
}

/// Bounded runtime-shared fx intent queue (CTX-1139).
///
/// Same governance as `PanelPinQueue`: overflow drops the newest request and
/// counts it, and the application drains it once per tick. The drain never
/// requeues: a refused, instant, or vanished intent is dropped with the end
/// state (the already-committed ring) holding.
#[derive(Debug)]
pub struct FxPulseQueue {
    items: VecDeque<QueuedFxPulse>,
    capacity: usize,
    dropped: u64,
}

impl FxPulseQueue {
    /// Creates a bounded queue (capacity at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Pushes an intent; returns whether it was accepted.
    pub fn push(&mut self, intent: QueuedFxPulse) -> bool {
        if self.items.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.items.push_back(intent);
        true
    }

    /// Drains all queued intents in FIFO order.
    pub fn drain(&mut self) -> Vec<QueuedFxPulse> {
        self.items.drain(..).collect()
    }

    /// Number of queued intents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number dropped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// One active ring pulse on a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveFxPulse {
    surface: ViewId,
    started: Instant,
    duration: Duration,
}

impl ActiveFxPulse {
    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.started);
        let raw = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        raw.clamp(0.0, 1.0)
    }

    fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration
    }
}

/// Tracks active plugin-requested ring pulses and drives frame-on-demand.
///
/// At most one pulse per surface: a repeat trigger on the same surface
/// restarts the bounded pulse (never accumulates work). The shared
/// 8-surface budget spans core transitions and pulses together: `trigger`
/// takes the live core count and refuses past the shared cap, so the RFC-0002
/// concurrency bound holds no matter which side fills it.
#[derive(Debug, Clone, Default)]
pub struct FxPulseTracker {
    active: Vec<ActiveFxPulse>,
}

impl FxPulseTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self { active: Vec::new() }
    }

    /// Effective pulse duration under `policy`.
    ///
    /// Mirrors the `AnimationPolicy::duration` gates (`safe_mode`,
    /// `enabled`, reduced motion resolve to instant) without adding a config
    /// leaf: the fixed duration applies only when the policy animates at all.
    #[must_use]
    pub fn duration_for(policy: &AnimationPolicy) -> Duration {
        let reduced = match policy.reduced_motion {
            super::animations::ReducedMotionMode::Always => true,
            super::animations::ReducedMotionMode::Never => false,
            super::animations::ReducedMotionMode::Auto => policy.platform_reduced,
        };
        if policy.safe_mode || !policy.enabled || reduced {
            return Duration::ZERO;
        }
        Duration::from_millis(u64::from(FX_PULSE_DURATION_MS.min(500)))
    }

    /// Arms a pulse for `surface` at `now`.
    ///
    /// `core_active` is the live core-transition count the caller measured at
    /// the same instant, so the shared cap is enforced across both sides.
    /// Returns `true` when a pulse is now active, or `false` when it was
    /// instant (0 ms under the policy gates) or refused at the shared
    /// capacity, in which case the committed ring already holds the end
    /// state and nothing further is needed.
    pub fn trigger(
        &mut self,
        surface: ViewId,
        now: Instant,
        policy: &AnimationPolicy,
        core_active: usize,
    ) -> bool {
        let duration = Self::duration_for(policy);
        if duration.is_zero() {
            self.active.retain(|pulse| pulse.surface != surface);
            return false;
        }
        // Restart (bounded) any existing pulse on the same surface.
        self.active.retain(|pulse| pulse.surface != surface);
        if self.active_count(now) + core_active >= MAX_CONCURRENT_ANIMATIONS {
            // Shared capacity exceeded: drop finished pulses first so a stale
            // entry cannot starve a live trigger, then refuse (end state
            // holds) rather than queue unbounded work.
            self.active.retain(|pulse| !pulse.done(now));
            if self.active_count(now) + core_active >= MAX_CONCURRENT_ANIMATIONS {
                return false;
            }
        }
        self.active.push(ActiveFxPulse {
            surface,
            started: now,
            duration,
        });
        true
    }

    /// Eased progress `0..=1` of the active pulse on `surface`, if any.
    ///
    /// Returns `None` once the pulse has completed, so callers fall back to
    /// the final committed state (never a lingering partial value). The
    /// closed `EaseInOut` curve is reused verbatim: no new easing.
    #[must_use]
    pub fn progress(&self, surface: ViewId, now: Instant) -> Option<f32> {
        self.active
            .iter()
            .find(|pulse| pulse.surface == surface && !pulse.done(now))
            .map(|pulse| AnimationCurve::EaseInOut.eval(pulse.progress(now)))
    }

    /// Whether any pulse is active (frame-on-demand gate).
    #[must_use]
    pub fn is_active(&self, now: Instant) -> bool {
        self.active.iter().any(|pulse| !pulse.done(now))
    }

    /// The next instant at which an in-flight pulse needs a frame, or `None`
    /// when idle. Same contract as the core tracker deadline.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.is_active(now) {
            return None;
        }
        let earliest_end = self
            .active
            .iter()
            .filter(|pulse| !pulse.done(now))
            .map(|pulse| pulse.started + pulse.duration)
            .min();
        let next_frame = now + super::animations::ANIMATION_FRAME_INTERVAL;
        Some(match earliest_end {
            Some(end) => next_frame.min(end),
            None => next_frame,
        })
    }

    /// Drops finished pulses. Returns `true` when any was removed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.active.len();
        self.active.retain(|pulse| !pulse.done(now));
        before != self.active.len()
    }

    /// Active pulse count.
    #[must_use]
    pub fn active_count(&self, now: Instant) -> usize {
        self.active.iter().filter(|pulse| !pulse.done(now)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::super::animations::ReducedMotionMode;
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn live_policy() -> AnimationPolicy {
        AnimationPolicy::default()
    }

    #[test]
    fn pulse_progresses_monotone_and_completes() {
        let now = t0();
        let policy = live_policy();
        let mut tracker = FxPulseTracker::new();
        let view = ViewId::new(3);
        assert!(tracker.trigger(view, now, &policy, 0));
        assert_eq!(tracker.progress(view, now), Some(0.0));
        let mid = now + Duration::from_millis(75);
        let value = tracker.progress(view, mid).expect("mid pulse");
        assert!((0.0..1.0).contains(&value), "mid progress {value}");
        // EaseInOut at half time is exactly half.
        assert!((value - 0.5).abs() < 1e-6, "ease_in_out midpoint {value}");
        let end = now + Duration::from_millis(u64::from(FX_PULSE_DURATION_MS));
        assert_eq!(tracker.progress(view, end), None);
        assert!(!tracker.is_active(end));
        assert_eq!(tracker.next_deadline(end), None);
        assert!(tracker.tick(end));
    }

    #[test]
    fn repeat_trigger_restarts_instead_of_accumulating() {
        let now = t0();
        let policy = live_policy();
        let mut tracker = FxPulseTracker::new();
        let view = ViewId::new(5);
        assert!(tracker.trigger(view, now, &policy, 0));
        assert!(tracker.trigger(view, now, &policy, 0));
        assert_eq!(tracker.active_count(now), 1);
        // A different surface is a distinct slot.
        assert!(tracker.trigger(ViewId::new(6), now, &policy, 0));
        assert_eq!(tracker.active_count(now), 2);
    }

    #[test]
    fn instant_policies_never_arm() {
        let now = t0();
        let view = ViewId::new(1);
        let policies = [
            AnimationPolicy {
                enabled: false,
                ..live_policy()
            },
            AnimationPolicy {
                reduced_motion: ReducedMotionMode::Always,
                ..live_policy()
            },
            AnimationPolicy {
                safe_mode: true,
                ..live_policy()
            },
            AnimationPolicy {
                platform_reduced: true,
                ..live_policy()
            },
        ];
        for policy in policies {
            let mut tracker = FxPulseTracker::new();
            assert_eq!(FxPulseTracker::duration_for(&policy), Duration::ZERO);
            assert!(!tracker.trigger(view, now, &policy, 0));
            assert!(!tracker.is_active(now));
            assert_eq!(tracker.next_deadline(now), None);
        }
    }

    #[test]
    fn shared_capacity_counts_core_transitions() {
        let now = t0();
        let policy = live_policy();
        let mut tracker = FxPulseTracker::new();
        // Seven live core transitions leave room for exactly one pulse.
        assert!(tracker.trigger(ViewId::new(1), now, &policy, 7));
        assert_eq!(tracker.active_count(now), 1);
        // Eight live core transitions refuse the next pulse (end state holds).
        let mut full = FxPulseTracker::new();
        assert!(!full.trigger(ViewId::new(9), now, &policy, 8));
        assert_eq!(full.active_count(now), 0);
        assert_eq!(full.progress(ViewId::new(9), now), None);
    }

    #[test]
    fn queue_bounds_and_drains_fifo() {
        let mut queue = FxPulseQueue::new(2);
        assert!(queue.is_empty());
        assert!(queue.push(QueuedFxPulse { view: 1 }));
        assert!(queue.push(QueuedFxPulse { view: 2 }));
        assert_eq!(queue.len(), 2);
        assert!(!queue.push(QueuedFxPulse { view: 3 }));
        assert_eq!(queue.dropped(), 1);
        let drained = queue.drain();
        assert_eq!(
            drained,
            vec![QueuedFxPulse { view: 1 }, QueuedFxPulse { view: 2 }]
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn next_deadline_collapses_onto_pulse_end() {
        let now = t0();
        let policy = live_policy();
        let mut tracker = FxPulseTracker::new();
        assert!(tracker.trigger(ViewId::new(2), now, &policy, 0));
        assert_eq!(
            tracker.next_deadline(now),
            Some(now + crate::runtime::animations::ANIMATION_FRAME_INTERVAL)
        );
        let near_end = now + Duration::from_millis(u64::from(FX_PULSE_DURATION_MS) - 10);
        assert_eq!(
            tracker.next_deadline(near_end),
            Some(now + Duration::from_millis(u64::from(FX_PULSE_DURATION_MS)))
        );
    }
}
