//! Renderer-side panel animations (RFC-0002, CTX-0341).
//!
//! Accepted contract: a closed transition set (panel open/close, focus change,
//! workspace switch) with per-transition integer durations in `0..=500` ms and
//! a closed easing enum. `spring` resolves to `ease_in_out` (its parameters are
//! deferred). Animations are presentation-only chrome: never grid, cursor,
//! scrollback, or Terminal Truth. Frame-on-demand is preserved: a frame is
//! scheduled only while an animation is active, and a completed animation
//! schedules no further wakeups (PB-7).
//!
//! Hostile-config safety: every duration is bounded, at most one animation per
//! surface and a bounded number of concurrently animating surfaces, and any
//! excess trigger commits its end state immediately rather than queueing
//! unbounded work.
use std::time::{Duration, Instant};

use bitty_ui::ViewId;

/// Maximum concurrently animating surfaces (RFC-0002 present-path budget,
/// candidate `8`). A trigger beyond this capacity commits its end state
/// immediately (no queue growth).
pub const MAX_CONCURRENT_ANIMATIONS: usize = 8;

/// Frame cadence while an animation is active (~60 fps).
///
/// RFC-0002 leaves the exact per-frame microsecond ceiling to the owning
/// repository. This cadence is the renderer-side wake interval: it bounds the
/// number of frames a transition produces (a 500 ms transition is at most
/// ~32 frames) while keeping motion smooth. It never schedules work after the
/// last animation completes.
pub const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The last presented frame of a `View` removed during a close transition.
///
/// A closed `View` has no allocation on the next present, so its retained
/// frame paints as a fading chrome ring until the close duration elapses
/// (RFC-0002: "the final removed state is committed at animation end or on
/// cancel"). Terminal content is never interpolated: only the Core-owned ring
/// fades, and the retained shape is bounded by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosingFrame {
    /// Removed View this frame belonged to.
    pub view: ViewId,
    /// Decoration-inclusive frame rectangle (physical px), pre-padding.
    pub frame: bitty_render::geometry::RectPx,
    /// Border thickness (physical px).
    pub border: u16,
    /// Corner radius (physical px).
    pub radius: u16,
    /// Outline color at the moment of removal (straight alpha).
    pub color: bitty_render::grid::Rgba8,
}

/// One animatable panel transition (RFC-0002 transition set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationKind {
    /// A `View` becomes occupied or a Panel is shown.
    Open,
    /// A `View` becomes empty/hidden or a Panel is hidden.
    Close,
    /// The focused `View` changes.
    Focus,
    /// The active `Workspace` changes.
    Workspace,
}

impl AnimationKind {
    /// Index into a [`AnimationPolicy`] duration/curve table.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Open => 0,
            Self::Close => 1,
            Self::Focus => 2,
            Self::Workspace => 3,
        }
    }
}

/// Closed easing enum; `spring` is already mapped to `EaseInOut` by the config
/// layer, but the variant is kept so the curve table can carry it verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationCurve {
    /// Constant velocity.
    Linear,
    /// Slow start, fast end.
    EaseIn,
    /// Fast start, slow end.
    EaseOut,
    /// Slow start and end (S-curve); also the resolved `spring` curve.
    EaseInOut,
}

impl AnimationCurve {
    /// Evaluates the curve at normalized time `t` (clamped to `0..=1`).
    ///
    /// All four curves are polynomial and total: `linear` is identity,
    /// `ease_in` is quadratic, `ease_out` is its mirror, and `ease_in_out` is
    /// the smoothstep S-curve. Values stay in `0..=1` for `t` in `0..=1`.
    #[must_use]
    pub fn eval(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
            Self::EaseInOut => t * t * (3.0 - 2.0 * t),
        }
    }
}

/// How the platform reduced-motion signal is honored (RFC-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReducedMotionMode {
    /// Follow the platform signal when one exists; otherwise animate.
    #[default]
    Auto,
    /// Force `0` ms durations.
    Always,
    /// Ignore the platform signal but still respect the duration bounds.
    Never,
}

/// Resolved animation policy carried on the runtime config (RFC-0002).
///
/// Durations are indexed by [`AnimationKind::index`]; `0` means instant and is
/// never an error. `safe_mode` forces every duration to `0` regardless of the
/// configuration and the platform signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationPolicy {
    /// Master switch; `false` is equivalent to `0` ms durations.
    pub enabled: bool,
    /// Per-transition durations in milliseconds.
    pub duration_ms: [u32; 4],
    /// Per-transition easing curves (already `spring`-resolved by config).
    pub curves: [AnimationCurve; 4],
    /// Reduced-motion mode.
    pub reduced_motion: ReducedMotionMode,
    /// Safe mode (`bitty --safe`); forces `0` ms.
    pub safe_mode: bool,
    /// Platform reduced-motion signal (`true` = user asked for less motion).
    pub platform_reduced: bool,
}

impl Default for AnimationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: [150, 120, 100, 200],
            curves: [
                AnimationCurve::EaseOut,
                AnimationCurve::EaseIn,
                AnimationCurve::EaseInOut,
                AnimationCurve::EaseInOut,
            ],
            reduced_motion: ReducedMotionMode::Auto,
            safe_mode: false,
            platform_reduced: false,
        }
    }
}

impl AnimationPolicy {
    /// The effective duration for `kind`, already reduced/safe/resolved.
    ///
    /// `0` whenever animations are disabled, safe mode is on, or the reduced
    /// motion mode resolves to reduced. Values are already bounded by config
    /// validation; the runtime additionally clamps defensively.
    #[must_use]
    pub fn duration(&self, kind: AnimationKind) -> Duration {
        let reduced = match self.reduced_motion {
            ReducedMotionMode::Always => true,
            ReducedMotionMode::Never => false,
            ReducedMotionMode::Auto => self.platform_reduced,
        };
        if self.safe_mode || !self.enabled || reduced {
            return Duration::ZERO;
        }
        let ms = self.duration_ms[kind.index()].min(500);
        Duration::from_millis(u64::from(ms))
    }

    /// The effective easing curve for `kind`.
    #[must_use]
    pub fn curve(&self, kind: AnimationKind) -> AnimationCurve {
        self.curves[kind.index()]
    }

    /// Whether `kind` animates at all under this policy.
    #[must_use]
    pub fn animates(&self, kind: AnimationKind) -> bool {
        !self.duration(kind).is_zero()
    }
}

/// One active animation on a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveAnimation {
    kind: AnimationKind,
    /// `None` for the workspace transition (a whole-surface transition).
    surface: Option<ViewId>,
    started: Instant,
    duration: Duration,
}

impl ActiveAnimation {
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

/// Tracks active panel animations and drives frame-on-demand.
///
/// At most one animation per surface: a repeat trigger on the same surface
/// restarts the bounded animation (never accumulates work). When the bounded
/// concurrent-surface capacity is reached a new trigger commits its end state
/// immediately (it is not admitted), so a hostile trigger storm cannot grow
/// the tracker without bound.
#[derive(Debug, Clone)]
pub struct PanelAnimator {
    policy: AnimationPolicy,
    active: Vec<ActiveAnimation>,
}

impl Default for PanelAnimator {
    fn default() -> Self {
        Self::new(AnimationPolicy::default())
    }
}

impl PanelAnimator {
    /// Creates a tracker for `policy`.
    #[must_use]
    pub fn new(policy: AnimationPolicy) -> Self {
        Self {
            policy,
            active: Vec::new(),
        }
    }

    /// Replaces the policy; adopts it without disturbing in-flight animation
    /// timing (the in-flight duration is retained until it finishes).
    pub fn set_policy(&mut self, policy: AnimationPolicy) {
        self.policy = policy;
    }

    /// Current policy.
    #[must_use]
    pub fn policy(&self) -> AnimationPolicy {
        self.policy
    }

    /// Arms a transition for `surface` at `now`.
    ///
    /// Returns `true` when an animation is now active for the surface, or
    /// `false` when it was instant (0 ms) or refused at capacity, in which
    /// case the caller applies the end state immediately.
    pub fn trigger(&mut self, kind: AnimationKind, surface: Option<ViewId>, now: Instant) -> bool {
        let duration = self.policy.duration(kind);
        if duration.is_zero() {
            self.clear_surface(surface);
            return false;
        }
        // Restart (bounded) any existing animation on the same surface.
        self.active
            .retain(|a| !(a.kind == kind && a.surface == surface));
        if self.active.len() >= MAX_CONCURRENT_ANIMATIONS {
            // Capacity exceeded: commit the end state immediately. Drop an
            // existing finished animation first so a stale entry cannot
            // starve a live trigger.
            self.active.retain(|a| !a.done(now));
            if self.active.len() >= MAX_CONCURRENT_ANIMATIONS {
                return false;
            }
        }
        self.active.push(ActiveAnimation {
            kind,
            surface,
            started: now,
            duration,
        });
        true
    }

    /// Drops any animation for `surface` of any kind (e.g. the surface was
    /// removed for good).
    fn clear_surface(&mut self, surface: Option<ViewId>) {
        self.active.retain(|a| a.surface != surface);
    }

    /// Progress `0..=1` of the active animation of `kind` on `surface`, if any.
    ///
    /// Returns `None` once the animation has completed, so callers fall back
    /// to the final committed state (never a lingering partial value).
    #[must_use]
    pub fn progress(
        &self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: Instant,
    ) -> Option<f32> {
        self.active
            .iter()
            .find(|a| a.kind == kind && a.surface == surface && !a.done(now))
            .map(|a| self.policy.curve(kind).eval(a.progress(now)))
    }

    /// Whether `surface` currently has any active animation.
    #[must_use]
    pub fn surface_active(&self, surface: Option<ViewId>, now: Instant) -> bool {
        self.active
            .iter()
            .any(|a| a.surface == surface && !a.done(now))
    }

    /// Whether any animation is active (frame-on-demand gate).
    #[must_use]
    pub fn is_active(&self, now: Instant) -> bool {
        self.active.iter().any(|a| !a.done(now))
    }

    /// The next instant at which an in-flight animation needs a frame, or
    /// `None` when idle.
    ///
    /// Returns the sooner of the next animation frame interval and the
    /// earliest animation end, so the app can sleep in `ControlFlow::Wait`
    /// until then. `None` means zero periodic wakeups attributable to
    /// animations after completion (PB-7).
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.is_active(now) {
            return None;
        }
        let earliest_end = self
            .active
            .iter()
            .filter(|a| !a.done(now))
            .map(|a| a.started + a.duration)
            .min();
        let next_frame = now + ANIMATION_FRAME_INTERVAL;
        Some(match earliest_end {
            Some(end) => next_frame.min(end),
            None => next_frame,
        })
    }

    /// Drops finished animations. Returns `true` when any was removed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.active.len();
        self.active.retain(|a| !a.done(now));
        before != self.active.len()
    }

    /// Active animation count (bounded by [`MAX_CONCURRENT_ANIMATIONS`]).
    #[must_use]
    pub fn active_count(&self, now: Instant) -> usize {
        self.active.iter().filter(|a| !a.done(now)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn curves_are_total_and_clamped() {
        for curve in [
            AnimationCurve::Linear,
            AnimationCurve::EaseIn,
            AnimationCurve::EaseOut,
            AnimationCurve::EaseInOut,
        ] {
            assert_eq!(curve.eval(0.0), 0.0, "{curve:?}");
            assert_eq!(curve.eval(1.0), 1.0, "{curve:?}");
            for i in 0..=20 {
                let v = curve.eval(i as f32 / 20.0);
                assert!((0.0..=1.0).contains(&v), "{curve:?} out of range: {v}");
            }
            // Out-of-range input clamps instead of extrapolating.
            assert_eq!(curve.eval(-5.0), 0.0);
            assert_eq!(curve.eval(5.0), 1.0);
        }
        assert_eq!(AnimationCurve::Linear.eval(0.5), 0.5);
        assert_eq!(AnimationCurve::EaseIn.eval(0.5), 0.25);
        assert_eq!(AnimationCurve::EaseInOut.eval(0.5), 0.5);
    }

    #[test]
    fn default_policy_matches_rfc0002() {
        let p = AnimationPolicy::default();
        assert!(p.enabled);
        assert!(!p.safe_mode);
        assert_eq!(p.duration(AnimationKind::Open), Duration::from_millis(150));
        assert_eq!(p.duration(AnimationKind::Close), Duration::from_millis(120));
        assert_eq!(p.duration(AnimationKind::Focus), Duration::from_millis(100));
        assert_eq!(
            p.duration(AnimationKind::Workspace),
            Duration::from_millis(200)
        );
        assert_eq!(p.curve(AnimationKind::Open), AnimationCurve::EaseOut);
        assert_eq!(p.curve(AnimationKind::Close), AnimationCurve::EaseIn);
    }

    #[test]
    fn policy_honors_disabled_reduced_and_safe() {
        let disabled = AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        };
        assert_eq!(disabled.duration(AnimationKind::Open), Duration::ZERO);
        let always = AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        };
        assert_eq!(always.duration(AnimationKind::Open), Duration::ZERO);
        let never = AnimationPolicy {
            reduced_motion: ReducedMotionMode::Never,
            platform_reduced: true,
            ..AnimationPolicy::default()
        };
        assert_eq!(
            never.duration(AnimationKind::Open),
            Duration::from_millis(150)
        );
        let safe = AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        };
        assert_eq!(safe.duration(AnimationKind::Open), Duration::ZERO);
        // `auto` follows the platform signal, and zero means instant/not
        // animating (never an error).
        let reduced = AnimationPolicy {
            platform_reduced: true,
            ..AnimationPolicy::default()
        };
        assert_eq!(reduced.duration(AnimationKind::Open), Duration::ZERO);
        assert!(!reduced.animates(AnimationKind::Open));
        let instant = AnimationPolicy {
            duration_ms: [0, 0, 0, 0],
            ..AnimationPolicy::default()
        };
        assert!(!instant.animates(AnimationKind::Open));
        // Hostile duration is clamped defensively to the RFC-0002 ceiling.
        let hostile = AnimationPolicy {
            duration_ms: [u32::MAX, u32::MAX, u32::MAX, u32::MAX],
            ..AnimationPolicy::default()
        };
        assert_eq!(
            hostile.duration(AnimationKind::Open),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn tracker_progresses_and_completes() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(7);
        assert!(!anim.is_active(now));
        assert!(anim.trigger(AnimationKind::Open, Some(view), now));
        assert!(anim.is_active(now));
        assert_eq!(anim.active_count(now), 1);
        // At start the eased progress is 0; halfway it advances.
        let p0 = anim.progress(AnimationKind::Open, Some(view), now).unwrap();
        assert_eq!(p0, 0.0);
        let mid = now + Duration::from_millis(75);
        let p1 = anim.progress(AnimationKind::Open, Some(view), mid).unwrap();
        assert!((0.0..1.0).contains(&p1), "mid progress {p1}");
        assert!(p1 > p0);
        // After the duration the animation is done and the frame-on-demand
        // gate is closed (zero wakeups).
        let end = now + Duration::from_millis(150);
        assert!(!anim.is_active(end));
        assert_eq!(anim.progress(AnimationKind::Open, Some(view), end), None);
        assert_eq!(anim.next_deadline(end), None);
        assert!(anim.tick(end));
    }

    #[test]
    fn instant_policy_never_arms_and_reports_inactive() {
        let now = t0();
        let p = AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        };
        let mut anim = PanelAnimator::new(p);
        assert!(!anim.trigger(AnimationKind::Open, Some(ViewId::new(1)), now));
        assert!(!anim.is_active(now));
        assert_eq!(anim.next_deadline(now), None);
    }

    #[test]
    fn one_animation_per_surface_restarts_not_accumulates() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(3);
        assert!(anim.trigger(AnimationKind::Open, Some(view), now));
        assert!(anim.trigger(AnimationKind::Open, Some(view), now));
        assert_eq!(anim.active_count(now), 1, "restart must not accumulate");
        // A different kind on the same surface is a distinct slot but the
        // close kind replaces the open on repeat.
        assert!(anim.trigger(AnimationKind::Focus, Some(view), now));
        assert_eq!(anim.active_count(now), 2);
    }

    #[test]
    fn concurrent_surface_capacity_is_bounded_and_excess_commits_immediately() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        for i in 0..MAX_CONCURRENT_ANIMATIONS {
            assert!(anim.trigger(AnimationKind::Open, Some(ViewId::new(i as u64 + 1)), now));
        }
        assert_eq!(anim.active_count(now), MAX_CONCURRENT_ANIMATIONS);
        // One past the cap is refused (end state commits immediately).
        assert!(!anim.trigger(AnimationKind::Open, Some(ViewId::new(999)), now));
        assert_eq!(anim.active_count(now), MAX_CONCURRENT_ANIMATIONS);
        // Once animations finish, capacity is available again.
        let end = now + Duration::from_millis(200);
        assert!(!anim.is_active(end));
        assert!(anim.trigger(AnimationKind::Open, Some(ViewId::new(999)), end));
    }

    #[test]
    fn next_deadline_is_bounded_by_earliest_finish_and_frame_cadence() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        // Focus is 100 ms, open is 150 ms: the next wake is the frame cadence
        // (16 ms), which is sooner than either finish.
        anim.trigger(AnimationKind::Open, Some(ViewId::new(1)), now);
        anim.trigger(AnimationKind::Focus, Some(ViewId::new(2)), now);
        assert_eq!(
            anim.next_deadline(now),
            Some(now + ANIMATION_FRAME_INTERVAL)
        );
        // Near the earliest finish the deadline collapses onto the end so the
        // final frame lands exactly at completion (no overshoot past it).
        let near_end = now + Duration::from_millis(90);
        assert_eq!(
            anim.next_deadline(near_end),
            Some(now + Duration::from_millis(100))
        );
        // Idle has no deadline (frame-on-demand PB-7).
        let end = now + Duration::from_millis(200);
        anim.tick(end);
        assert_eq!(anim.next_deadline(end), None);
    }

    #[test]
    fn workspace_transition_uses_none_surface() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        assert!(anim.trigger(AnimationKind::Workspace, None, now));
        assert!(anim.next_deadline(now).is_some());
        let p = anim.progress(AnimationKind::Workspace, None, now);
        assert_eq!(p, Some(0.0));
    }
}
