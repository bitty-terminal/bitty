//! Renderer-side panel animations (RFC-0002, CTX-0341; CTX-0967 move/resize/drag;
//! CTX-1078 splits/zoom/workspaces).
//!
//! Accepted contract: a closed transition set (panel open/close, focus change,
//! workspace switch, panel move, panel resize, panel drag) with per-transition
//! integer durations in `0..=500` ms and
//! a closed easing enum. `spring` resolves to `ease_in_out` (its parameters are
//! deferred). Animations are presentation-only chrome: never grid, cursor,
//! scrollback, or Terminal Truth. Frame-on-demand is preserved: a frame is
//! scheduled only while an animation is active, and a completed animation
//! schedules no further wakeups (PB-7).
//!
//! Split open/close, single-pane zoom, and workspace switches (CTX-1078,
//! issue #1755) ride this same contract: the layout commits its final state
//! immediately (terminal truth, PTY size, and hit-testing are final on the
//! first frame) and the Core-owned decoration ring fades over the configured
//! duration with a smooth cubic easing (`ease_in_out` smoothstep is degree-3,
//! the `spring` name resolves to it with zero overshoot). They are detected
//! from the `View`-set delta and the active-workspace index on the next
//! present with a virtual-clock seam, so tests advance time deterministically.
//!
//! CTX-1335 (issue #2113) adds paint-only panel-bounds interpolation on the
//! same seam: when a live `View`'s frame/content changes, the committed start
//! and end [`PanelBounds`] are captured and the painting path interpolates
//! between them over the existing duration/easing. The logical layout,
//! terminal grid, cursor, selection, scrollback, input hit-testing, and plugin
//! execution phase never observe the interpolated values. Instant, reduced
//! motion, safe mode, and capacity overflow all paint the committed end state
//! immediately.
//!
//! Move/resize/drag (CTX-0967, issue #1696) are Hyprland-style geometry gestures:
//! the layout commits its final state immediately (terminal content is never
//! interpolated) and the Core-owned decoration ring fades over the configured
//! duration; CTX-1335 also interpolates their painted bounds when the tracked
//! start/end rects are available. They are triggered directly from the gesture
//! paths (Alt-drag float move, border-drag ratio change, keyboard resize, panel
//! reparent) rather than detected from the `View`-set delta, because a move or
//! resize keeps the same `View` set.
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

/// One animatable panel transition (RFC-0002 transition set, extended by
/// CTX-0967 with move/resize/drag).
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
    /// A panel is repositioned (tiled reparent or cross-workspace move).
    ///
    /// Triggered directly from the reparent paths; the layout commits
    /// immediately and only the moved panel's chrome ring fades.
    Move,
    /// A panel boundary moves (border-drag divider or keyboard resize step).
    ///
    /// Triggered directly from the resize paths; ratios commit immediately
    /// and only the resized panels' chrome rings fade.
    Resize,
    /// A floating panel is dragged (Alt+drag float move).
    ///
    /// Triggered on every drag update; repeat updates restart the bounded
    /// transition (never accumulate), so the ring settles after release.
    Drag,
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
            Self::Move => 4,
            Self::Resize => 5,
            Self::Drag => 6,
        }
    }

    /// Number of animatable transitions (table length).
    pub const COUNT: usize = 7;
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
    /// Non-finite `t` settles: NaN clamps to `0`, infinities to their end,
    /// mirroring the `bitty-ui` motion sampler so headless callers never
    /// observe NaN progress.
    #[must_use]
    pub fn eval(self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
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
    /// Per-transition durations in milliseconds, indexed by
    /// [`AnimationKind::index`] (`[open, close, focus, workspace, move,
    /// resize, drag]`).
    pub duration_ms: [u32; AnimationKind::COUNT],
    /// Per-transition easing curves (already `spring`-resolved by config),
    /// same index order as `duration_ms`.
    pub curves: [AnimationCurve; AnimationKind::COUNT],
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
            // Index order: open, close, focus, workspace, move, resize, drag
            // (Hyprland-style: moves slide, resizes track, drags settle).
            duration_ms: [150, 120, 100, 200, 150, 120, 150],
            curves: [
                AnimationCurve::EaseOut,
                AnimationCurve::EaseIn,
                AnimationCurve::EaseInOut,
                AnimationCurve::EaseInOut,
                AnimationCurve::EaseInOut,
                AnimationCurve::EaseInOut,
                AnimationCurve::EaseOut,
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

/// Panel bounds that may be interpolated across a transition (CTX-1335).
///
/// Both rectangles are presentation-only: the layout, PTY size, hit-testing,
/// and terminal truth commit once at the transition start. Only these painted
/// bounds move between the committed start and end snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelBounds {
    /// Decoration-inclusive frame rectangle, physical px.
    pub frame: bitty_render::geometry::RectPx,
    /// Painted content rectangle inside the border, physical px.
    pub content: bitty_render::geometry::RectPx,
}

impl PanelBounds {
    /// Interpolates `from` toward `to` at eased progress `p` (clamped).
    ///
    /// Rounds each edge independently with deterministic integer math, so
    /// tests on a virtual clock observe the exact same rectangles on every
    /// platform. Sizes saturate at zero rather than wrapping when a shrinking
    /// rectangle crosses its end.
    #[must_use]
    pub fn lerp(from: Self, to: Self, p: f32) -> Self {
        let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
        Self {
            frame: lerp_rect(from.frame, to.frame, p),
            content: lerp_rect(from.content, to.content, p),
        }
    }

    /// Translates both rectangles by `(dx, dy)` pixels.
    #[must_use]
    pub fn translated(self, dx: i32, dy: i32) -> Self {
        Self {
            frame: translate_rect(self.frame, dx, dy),
            content: translate_rect(self.content, dx, dy),
        }
    }
}

/// Deterministic integer interpolation of one axis value.
fn lerp_axis(from: i64, to: i64, p: f32) -> i64 {
    let delta = (to as f64) - (from as f64);
    let value = (from as f64) + delta * f64::from(p);
    // `as i64` saturates, so hostile values cannot wrap.
    value.round() as i64
}

/// Interpolates two rectangles without overflow.
fn lerp_rect(
    from: bitty_render::geometry::RectPx,
    to: bitty_render::geometry::RectPx,
    p: f32,
) -> bitty_render::geometry::RectPx {
    let x = lerp_axis(i64::from(from.x), i64::from(to.x), p);
    let y = lerp_axis(i64::from(from.y), i64::from(to.y), p);
    let width = lerp_axis(i64::from(from.width), i64::from(to.width), p).max(0);
    let height = lerp_axis(i64::from(from.height), i64::from(to.height), p).max(0);
    bitty_render::geometry::RectPx::new(
        i32::try_from(x).unwrap_or(if x < 0 { i32::MIN } else { i32::MAX }),
        i32::try_from(y).unwrap_or(if y < 0 { i32::MIN } else { i32::MAX }),
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    )
}

/// Translates one rectangle with saturating arithmetic.
fn translate_rect(
    rect: bitty_render::geometry::RectPx,
    dx: i32,
    dy: i32,
) -> bitty_render::geometry::RectPx {
    bitty_render::geometry::RectPx::new(
        rect.x.saturating_add(dx),
        rect.y.saturating_add(dy),
        rect.width,
        rect.height,
    )
}

/// One active animation on a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveAnimation {
    kind: AnimationKind,
    /// `None` for the workspace transition (a whole-surface transition).
    surface: Option<ViewId>,
    started: Instant,
    duration: Duration,
    /// CTX-1335: painted bounds captured when the transition armed. `None`
    /// for ring-only transitions (open/close/focus), so geometry stays at the
    /// committed final state exactly as before.
    bounds: Option<(PanelBounds, PanelBounds)>,
    /// Whether the entry owns an independent ring fade armed through
    /// [`PanelAnimator::trigger`]. A bounds-only entry (`false`) is removed
    /// outright when its geometry is cancelled; a ring entry keeps fading
    /// with its bounds cleared.
    ring: bool,
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
        // Restart (bounded) any existing animation of the same kind on the
        // surface. Close drops an in-flight bounds tween (the surface is going
        // away); the gesture kinds replace it but carry its painted frame
        // forward, because the gesture re-arms its own geometry on the next
        // present. Focus/Open/Workspace leave bounds interpolation untouched:
        // they share the same surface without conflicting with its painted
        // geometry (CTX-1335).
        let replaces_bounds = matches!(
            kind,
            AnimationKind::Close
                | AnimationKind::Move
                | AnimationKind::Resize
                | AnimationKind::Drag
        );
        // A gesture re-arm replaces its bounds entry, but the painted frame
        // must not jump back to the last committed target before the next
        // present retargets. Carry the current interpolated frame forward as
        // the new start, keeping the previous destination until the detector
        // supplies the freshly committed one (CodeRabbit PR #2231, finding 1).
        let carried_bounds = if matches!(
            kind,
            AnimationKind::Move | AnimationKind::Resize | AnimationKind::Drag
        ) {
            let destination = self
                .active
                .iter()
                .find(|a| a.surface == surface && a.bounds.is_some() && !a.done(now))
                .and_then(|a| a.bounds)
                .map(|(_, to)| to);
            self.interpolated_bounds(surface, now).zip(destination)
        } else {
            None
        };
        self.active.retain(|a| {
            !(a.surface == surface && (a.kind == kind || (replaces_bounds && a.bounds.is_some())))
        });
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
            bounds: carried_bounds,
            ring: true,
        });
        true
    }

    /// Arms a geometry transition for `surface` from `from` to `to` at `now`.
    ///
    /// Returns `true` when the painted bounds are now interpolating, or
    /// `false` when the policy resolves to an instant cut (zero duration) or
    /// the bounded capacity is exhausted; in both cases the caller paints the
    /// committed end state immediately. The transition uses `kind`'s accepted
    /// duration and easing, except when it rides an in-flight drag gesture
    /// (which keeps that gesture's accepted clock), and replaces any other
    /// in-flight geometry transition on the same surface so two bounds
    /// animations never blend.
    pub fn trigger_bounds(
        &mut self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        from: PanelBounds,
        to: PanelBounds,
        now: Instant,
    ) -> bool {
        self.trigger_bounds_at(kind, surface, from, to, now, now)
    }

    /// Arms a geometry transition whose clock started at `started`.
    ///
    /// Gesture paths arm their ring transition before the next present; the
    /// present-time detector reuses that start so the bounds do not restart
    /// half-way through the gesture's accepted duration.
    pub fn trigger_bounds_at(
        &mut self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        from: PanelBounds,
        to: PanelBounds,
        started: Instant,
        now: Instant,
    ) -> bool {
        let duration = self.policy.duration(kind);
        if duration.is_zero() {
            self.cancel_bounds(surface);
            return false;
        }
        // One geometry transition per surface: reuse the same-kind slot, or
        // attach to an in-flight Alt+drag gesture entry (CTX-1335 F3). The
        // drag ring already owns the surface's accepted duration, so its
        // bounds ride that entry instead of consuming a second budget slot
        // and multiplying the ring alpha.
        if let Some(index) = self.bounds_host(kind, surface, now) {
            let mut entry = self.active.remove(index);
            self.active
                .retain(|a| !(a.surface == surface && a.bounds.is_some()));
            if entry.kind == kind {
                entry.started = started;
                entry.duration = duration;
            }
            entry.bounds = Some((from, to));
            self.active.push(entry);
            return true;
        }
        // No reusable slot: replace any stale bounds entry deterministically
        // (a different kind must never keep painting its old end rectangle).
        self.active
            .retain(|a| !(a.surface == surface && a.bounds.is_some()));
        if self.active.len() >= MAX_CONCURRENT_ANIMATIONS {
            self.active.retain(|a| !a.done(now));
            if self.active.len() >= MAX_CONCURRENT_ANIMATIONS {
                return false;
            }
        }
        self.active.push(ActiveAnimation {
            kind,
            surface,
            started,
            duration,
            bounds: Some((from, to)),
            ring: false,
        });
        true
    }

    /// Index of the entry that can host a bounds tween for `kind` on
    /// `surface`: a same-kind slot, or an in-flight Alt+drag gesture entry
    /// (CTX-1335 F3).
    fn bounds_host(
        &self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: Instant,
    ) -> Option<usize> {
        let mut drag_host = None;
        for (index, animation) in self.active.iter().enumerate() {
            if animation.surface != surface || animation.done(now) {
                continue;
            }
            if animation.kind == kind {
                return Some(index);
            }
            if animation.kind == AnimationKind::Drag
                && matches!(kind, AnimationKind::Move | AnimationKind::Resize)
            {
                drag_host = Some(index);
            }
        }
        drag_host
    }

    /// Whether [`Self::trigger_bounds_at`] would reuse an existing entry
    /// instead of consuming a new budget slot.
    #[must_use]
    pub(crate) fn has_bounds_host(
        &self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: Instant,
    ) -> bool {
        self.bounds_host(kind, surface, now).is_some()
    }

    /// Cancels the bounds tween on `surface` without touching an independent
    /// ring fade. Returns whether anything was cancelled.
    ///
    /// A bounds-only entry is removed (nothing else would paint); a ring
    /// entry keeps its accepted fade with `bounds: None`, so the next paint
    /// uses the committed end rectangle immediately (CTX-1335 F1/F2).
    pub(crate) fn cancel_bounds(&mut self, surface: Option<ViewId>) -> bool {
        let mut changed = false;
        let mut index = 0;
        while index < self.active.len() {
            let animation = self.active[index];
            if animation.surface == surface && animation.bounds.is_some() {
                changed = true;
                if animation.ring {
                    self.active[index].bounds = None;
                    index += 1;
                } else {
                    self.active.remove(index);
                }
            } else {
                index += 1;
            }
        }
        changed
    }

    /// Start instant of an active animation of `kind` on `surface`, if any.
    #[must_use]
    pub fn active_started(
        &self,
        kind: AnimationKind,
        surface: Option<ViewId>,
        now: Instant,
    ) -> Option<Instant> {
        self.active
            .iter()
            .find(|a| a.kind == kind && a.surface == surface && !a.done(now))
            .map(|a| a.started)
    }

    /// Earliest start instant among active gesture geometry transitions
    /// (move/resize/drag, any surface), if any.
    ///
    /// A gesture that reparents one panel also changes its siblings' bounds;
    /// those siblings join the gesture's clock instead of starting a fresh
    /// transition on the next present.
    #[must_use]
    pub fn earliest_geometry_started(&self, now: Instant) -> Option<Instant> {
        self.active
            .iter()
            .filter(|a| {
                matches!(
                    a.kind,
                    AnimationKind::Move | AnimationKind::Resize | AnimationKind::Drag
                ) && !a.done(now)
            })
            .map(|a| a.started)
            .min()
    }

    /// The eased, interpolated bounds of the active geometry transition on
    /// `surface`, or `None` when it is not animating (caller paints the
    /// committed end state).
    #[must_use]
    pub fn interpolated_bounds(
        &self,
        surface: Option<ViewId>,
        now: Instant,
    ) -> Option<PanelBounds> {
        self.active
            .iter()
            .find(|a| a.surface == surface && a.bounds.is_some() && !a.done(now))
            .and_then(|a| {
                let (from, to) = a.bounds?;
                let progress = self.policy.curve(a.kind).eval(a.progress(now));
                Some(PanelBounds::lerp(from, to, progress))
            })
    }

    /// Whether at least one geometry transition is currently interpolating.
    #[must_use]
    pub fn bounds_active(&self, now: Instant) -> bool {
        self.active
            .iter()
            .any(|a| a.bounds.is_some() && !a.done(now))
    }

    /// Drops one in-flight bounds transition to make room for a ring
    /// transition at the shared capacity bound.
    ///
    /// Geometry interpolation is additive presentation (CTX-1335); the
    /// accepted open/close/focus/move ring transitions keep priority when a
    /// hostile trigger storm reaches [`MAX_CONCURRENT_ANIMATIONS`]. The
    /// dropped surface paints its committed end state immediately.
    pub fn drop_one_bounds(&mut self, now: Instant) -> bool {
        self.active.retain(|a| !a.done(now));
        // Only bounds-only entries free a budget slot. A reused ring slot
        // (Move or the Alt-drag Drag entry) hosts bounds too; removing it
        // would delete the accepted fade this priority rule protects
        // (CodeRabbit PR #2231, finding 2).
        if let Some(index) = self
            .active
            .iter()
            .position(|a| a.bounds.is_some() && !a.ring)
        {
            self.active.remove(index);
            return true;
        }
        false
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
    fn default_policy_covers_move_resize_drag() {
        // CTX-0967: the Hyprland-style geometry kinds carry bounded defaults
        // with the same reduced-motion contract as the RFC-0002 set.
        assert_eq!(AnimationKind::COUNT, 7);
        assert_eq!(AnimationKind::Move.index(), 4);
        assert_eq!(AnimationKind::Resize.index(), 5);
        assert_eq!(AnimationKind::Drag.index(), 6);
        let p = AnimationPolicy::default();
        assert_eq!(p.duration(AnimationKind::Move), Duration::from_millis(150));
        assert_eq!(
            p.duration(AnimationKind::Resize),
            Duration::from_millis(120)
        );
        assert_eq!(p.duration(AnimationKind::Drag), Duration::from_millis(150));
        assert_eq!(p.curve(AnimationKind::Move), AnimationCurve::EaseInOut);
        assert_eq!(p.curve(AnimationKind::Resize), AnimationCurve::EaseInOut);
        assert_eq!(p.curve(AnimationKind::Drag), AnimationCurve::EaseOut);
        for kind in [
            AnimationKind::Move,
            AnimationKind::Resize,
            AnimationKind::Drag,
        ] {
            assert!(p.animates(kind), "{kind:?} must animate by default");
        }
        // Reduced-motion, safe mode, and the master switch zero the geometry
        // kinds exactly like the RFC-0002 set.
        let reduced = AnimationPolicy {
            reduced_motion: ReducedMotionMode::Always,
            ..AnimationPolicy::default()
        };
        let safe = AnimationPolicy {
            safe_mode: true,
            ..AnimationPolicy::default()
        };
        let disabled = AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        };
        for kind in [
            AnimationKind::Move,
            AnimationKind::Resize,
            AnimationKind::Drag,
        ] {
            assert_eq!(reduced.duration(kind), Duration::ZERO);
            assert_eq!(safe.duration(kind), Duration::ZERO);
            assert_eq!(disabled.duration(kind), Duration::ZERO);
        }
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
            duration_ms: [0; AnimationKind::COUNT],
            ..AnimationPolicy::default()
        };
        assert!(!instant.animates(AnimationKind::Open));
        // Hostile duration is clamped defensively to the RFC-0002 ceiling.
        let hostile = AnimationPolicy {
            duration_ms: [u32::MAX; AnimationKind::COUNT],
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

    fn bounds(x: i32, y: i32, w: u32, h: u32) -> PanelBounds {
        let frame = bitty_render::geometry::RectPx::new(x, y, w, h);
        // Saturating so degenerate fixtures (`0x0`) stay valid under the CI
        // profile's overflow checks instead of panicking in the helper.
        let content = bitty_render::geometry::RectPx::new(
            x.saturating_add(2),
            y.saturating_add(2),
            w.saturating_sub(4),
            h.saturating_sub(4),
        );
        PanelBounds { frame, content }
    }

    #[test]
    fn panel_bounds_interpolation_is_exact_at_endpoints_and_monotone() {
        let from = bounds(10, 20, 100, 60);
        let to = bounds(50, 40, 200, 120);
        assert_eq!(PanelBounds::lerp(from, to, 0.0), from);
        assert_eq!(PanelBounds::lerp(from, to, 1.0), to);
        // Deterministic midpoint: every edge is the rounded average.
        let mid = PanelBounds::lerp(from, to, 0.5);
        assert_eq!(mid.frame.x, 30);
        assert_eq!(mid.frame.y, 30);
        assert_eq!(mid.frame.width, 150);
        assert_eq!(mid.frame.height, 90);
        assert_eq!(mid.content.x, 32);
        assert_eq!(mid.content.width, 146);
        // Out-of-range and non-finite progress clamp instead of extrapolating.
        assert_eq!(PanelBounds::lerp(from, to, -1.0), from);
        assert_eq!(PanelBounds::lerp(from, to, 2.0), to);
        assert_eq!(PanelBounds::lerp(from, to, f32::NAN), from);
        // A shrinking rectangle never reports a wrapped span.
        let empty = PanelBounds::lerp(bounds(0, 0, 4, 4), bounds(0, 0, 0, 0), 0.5);
        assert_eq!(empty.frame.width, 2);
        assert_eq!(empty.frame.height, 2);
    }

    #[test]
    fn bounds_transition_interpolates_and_closes_the_wakeup_gate() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(21);
        let from = bounds(0, 0, 80, 40);
        let to = bounds(80, 40, 160, 80);
        assert!(anim.trigger_bounds(AnimationKind::Move, Some(view), from, to, now));
        assert!(anim.bounds_active(now));
        assert_eq!(anim.interpolated_bounds(Some(view), now), Some(from));
        let mid = now + Duration::from_millis(75);
        let mid_bounds = anim
            .interpolated_bounds(Some(view), mid)
            .expect("mid bounds");
        assert_ne!(mid_bounds, from);
        assert_ne!(mid_bounds, to);
        // The same transition also drives the existing ring fade slot and the
        // frame-on-demand deadline.
        assert!(
            anim.progress(AnimationKind::Move, Some(view), mid)
                .is_some()
        );
        assert!(anim.next_deadline(mid).is_some());
        let end = now + Duration::from_millis(150);
        assert!(!anim.bounds_active(end));
        assert_eq!(anim.interpolated_bounds(Some(view), end), None);
        assert_eq!(anim.next_deadline(end), None);
        assert!(anim.tick(end));
    }

    #[test]
    fn instant_bounds_trigger_cuts_immediately_and_never_accumulates() {
        let now = t0();
        for policy in [
            AnimationPolicy {
                enabled: false,
                ..AnimationPolicy::default()
            },
            AnimationPolicy {
                reduced_motion: ReducedMotionMode::Always,
                ..AnimationPolicy::default()
            },
            AnimationPolicy {
                safe_mode: true,
                ..AnimationPolicy::default()
            },
            AnimationPolicy {
                duration_ms: [0; AnimationKind::COUNT],
                ..AnimationPolicy::default()
            },
        ] {
            let mut anim = PanelAnimator::new(policy);
            let view = ViewId::new(22);
            assert!(
                !anim.trigger_bounds(
                    AnimationKind::Move,
                    Some(view),
                    bounds(0, 0, 10, 10),
                    bounds(20, 20, 30, 30),
                    now,
                ),
                "{policy:?} must cut geometry instantly"
            );
            assert!(!anim.bounds_active(now), "{policy:?}");
            assert_eq!(anim.interpolated_bounds(Some(view), now), None);
            assert_eq!(anim.next_deadline(now), None);
        }
    }

    #[test]
    fn bounds_transitions_share_the_concurrent_surface_cap() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        for i in 0..MAX_CONCURRENT_ANIMATIONS {
            let view = ViewId::new(i as u64 + 1);
            assert!(anim.trigger_bounds(
                AnimationKind::Move,
                Some(view),
                bounds(0, 0, 10, 10),
                bounds(10, 10, 20, 20),
                now,
            ));
        }
        // One past the cap commits the end state immediately (no queue).
        assert!(!anim.trigger_bounds(
            AnimationKind::Move,
            Some(ViewId::new(999)),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        assert_eq!(anim.active_count(now), MAX_CONCURRENT_ANIMATIONS);
    }

    #[test]
    fn bounds_reuse_same_kind_and_drag_slots_without_double_counting() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let moved = ViewId::new(30);
        let dragged = ViewId::new(31);

        // A same-kind ring slot is reused instead of consuming another
        // budget entry (CTX-1335 F4).
        assert!(anim.trigger(AnimationKind::Move, Some(moved), now));
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(moved),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        assert_eq!(anim.active_count(now), 1);
        assert!(anim.has_bounds_host(AnimationKind::Move, Some(moved), now));

        // An in-flight Alt+drag gesture hosts the per-view move bounds: one
        // surface, one budget entry, one ring alpha factor (CTX-1335 F3).
        assert!(anim.trigger(AnimationKind::Drag, Some(dragged), now));
        assert!(anim.has_bounds_host(AnimationKind::Move, Some(dragged), now));
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(dragged),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        assert_eq!(anim.active_count(now), 2);
        assert!(
            anim.progress(AnimationKind::Drag, Some(dragged), now)
                .is_some()
        );
        assert!(
            anim.progress(AnimationKind::Move, Some(dragged), now)
                .is_none(),
            "bounds must ride the drag entry, not add a second Move entry"
        );

        // Cancelling geometry keeps the independent drag ring fade.
        assert!(anim.cancel_bounds(Some(dragged)));
        assert_eq!(anim.interpolated_bounds(Some(dragged), now), None);
        assert!(
            anim.progress(AnimationKind::Drag, Some(dragged), now)
                .is_some()
        );
        assert!(!anim.cancel_bounds(Some(dragged)));
    }

    #[test]
    fn different_kind_bounds_are_replaced_not_blended_and_cancel_cleans_up() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(32);
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(view),
            bounds(0, 0, 10, 10),
            bounds(40, 20, 50, 40),
            now,
        ));
        let mid = now + Duration::from_millis(60);
        let moved = anim
            .interpolated_bounds(Some(view), mid)
            .expect("move tween");
        // A different kind replaces the in-flight tween deterministically.
        assert!(anim.trigger_bounds(
            AnimationKind::Resize,
            Some(view),
            bounds(0, 0, 10, 10),
            bounds(60, 30, 70, 50),
            now,
        ));
        assert_eq!(anim.active_count(now), 1, "one bounds entry per surface");
        assert!(
            anim.progress(AnimationKind::Move, Some(view), mid)
                .is_none()
        );
        assert!(
            anim.progress(AnimationKind::Resize, Some(view), mid)
                .is_some()
        );
        assert_ne!(anim.interpolated_bounds(Some(view), mid), Some(moved));
        // A bounds-only entry disappears on cancel: nothing else paints.
        assert!(anim.cancel_bounds(Some(view)));
        assert_eq!(anim.active_count(now), 0);
        assert!(!anim.bounds_active(now));
        assert!(!anim.cancel_bounds(Some(view)));
    }

    #[test]
    fn overflow_drops_bounds_before_ring_transitions() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(33);
        let focused = ViewId::new(34);
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(view),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        assert!(anim.trigger(AnimationKind::Focus, Some(focused), now));
        assert!(anim.drop_one_bounds(now));
        assert_eq!(anim.active_count(now), 1);
        assert!(anim.interpolated_bounds(Some(view), now).is_none());
        assert!(
            anim.progress(AnimationKind::Focus, Some(focused), now)
                .is_some()
        );
        assert!(
            !anim.drop_one_bounds(now),
            "nothing left to drop: no bounds entry"
        );
    }

    #[test]
    fn overflow_preserves_a_ring_entry_that_hosts_bounds() {
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let dragged = ViewId::new(35);
        let bounds_only = ViewId::new(36);

        // The Alt-drag ring hosts its own bounds (one surface, one entry).
        assert!(anim.trigger(AnimationKind::Drag, Some(dragged), now));
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(dragged),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        // A separate bounds-only tween is the only safe eviction candidate.
        assert!(anim.trigger_bounds(
            AnimationKind::Move,
            Some(bounds_only),
            bounds(0, 0, 10, 10),
            bounds(10, 10, 20, 20),
            now,
        ));
        assert_eq!(anim.active_count(now), 2);

        assert!(anim.drop_one_bounds(now));
        assert_eq!(anim.active_count(now), 1);
        assert!(
            anim.progress(AnimationKind::Drag, Some(dragged), now)
                .is_some(),
            "the accepted drag ring survives"
        );
        assert!(
            anim.interpolated_bounds(Some(dragged), now).is_some(),
            "the ring's hosted bounds survive with it"
        );
        assert_eq!(anim.interpolated_bounds(Some(bounds_only), now), None);

        // Only a ring-hosted tween remains: no slot can be freed without
        // deleting the ring, so the trigger must be refused.
        assert!(!anim.drop_one_bounds(now));
        assert_eq!(anim.active_count(now), 1);
        assert!(
            anim.progress(AnimationKind::Drag, Some(dragged), now)
                .is_some()
        );
        assert!(anim.interpolated_bounds(Some(dragged), now).is_some());
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

    #[test]
    fn move_resize_drag_trigger_progress_and_complete() {
        // CTX-0967: the geometry kinds share the bounded tracker contract —
        // distinct slots per kind on one surface, restart on repeat, eased
        // progress, and zero wakeups after the longest configured duration
        // (move/drag 150 ms outlast resize 120 ms).
        let now = t0();
        let mut anim = PanelAnimator::new(AnimationPolicy::default());
        let view = ViewId::new(9);
        assert!(anim.trigger(AnimationKind::Move, Some(view), now));
        assert!(anim.trigger(AnimationKind::Resize, Some(view), now));
        assert!(anim.trigger(AnimationKind::Drag, Some(view), now));
        assert_eq!(anim.active_count(now), 3);
        // A repeat trigger restarts the same slot instead of accumulating.
        assert!(anim.trigger(AnimationKind::Move, Some(view), now));
        assert_eq!(anim.active_count(now), 3);
        let mid = now + Duration::from_millis(60);
        for kind in [
            AnimationKind::Move,
            AnimationKind::Resize,
            AnimationKind::Drag,
        ] {
            let p = anim
                .progress(kind, Some(view), mid)
                .expect("mid-transition progress");
            assert!((0.0..1.0).contains(&p), "{kind:?} mid progress {p}");
        }
        let end = now + Duration::from_millis(150);
        assert!(!anim.is_active(end));
        assert_eq!(anim.next_deadline(end), None);
        assert!(anim.tick(end));
    }

    #[test]
    fn bezier_easing_is_monotone_endpoints_exact_and_mirror_symmetric() {
        // CTX-1078 (issue #1755): the closed easing set is the smooth
        // cubic-compatible family for splits, zoom, and workspace nav.
        // `ease_in_out` smoothstep is degree-3, `ease_in`/`ease_out` are its
        // quadratic mirrors, and `linear` is identity. Endpoints are exact
        // so settled values land on target, every curve is monotone so
        // motion never runs backwards, and the mirror pairs are symmetric.
        for curve in [
            AnimationCurve::Linear,
            AnimationCurve::EaseIn,
            AnimationCurve::EaseOut,
            AnimationCurve::EaseInOut,
        ] {
            assert_eq!(curve.eval(0.0), 0.0, "{curve:?} start must be exact");
            assert_eq!(curve.eval(1.0), 1.0, "{curve:?} end must be exact");
            let mut prev = 0.0_f32;
            let mut i = 0;
            while i <= 100 {
                let t = (i as f32) / 100.0;
                let next = curve.eval(t);
                assert!(next >= prev, "{curve:?} must not run backwards at t={t}");
                assert!(
                    (0.0..=1.0).contains(&next),
                    "{curve:?} must stay bounded at t={t}: {next}"
                );
                prev = next;
                i += 1;
            }
        }
        // Mirror symmetry: ease-out mirrors ease-in around the midpoint.
        let mut i = 0;
        while i <= 20 {
            let t = (i as f32) / 20.0;
            let out = AnimationCurve::EaseOut.eval(t);
            let mirrored = 1.0 - AnimationCurve::EaseIn.eval(1.0 - t);
            assert!(
                (out - mirrored).abs() < 1e-6,
                "ease_out must mirror ease_in at t={t}: {out} vs {mirrored}"
            );
            // Ease-in-out symmetry around the midpoint.
            let a = AnimationCurve::EaseInOut.eval(t);
            let b = AnimationCurve::EaseInOut.eval(1.0 - t);
            assert!(
                (a + b - 1.0).abs() < 1e-6,
                "ease_in_out must be symmetric at t={t}: {a} + {b} must be 1"
            );
            i += 1;
        }
        assert_eq!(AnimationCurve::EaseInOut.eval(0.5), 0.5);
    }

    #[test]
    fn spring_overshoot_is_bounded_to_zero() {
        // CTX-1078 (issue #1755): RFC-0002 defers `spring` parameters, so the
        // config layer resolves `spring` to `ease_in_out`. The resolved curve
        // never overshoots: every sample stays in `0..=1`, so the overshoot
        // bound is zero and a settled value always lands exactly on target.
        // CTX-1335 interpolates painted bounds with this same resolved curve,
        // so a spring-named transition still has zero overshoot: every edge
        // stays between its committed start and end rectangles.
        let resolved = AnimationCurve::EaseInOut;
        let mut max = 0.0_f32;
        let mut min = 1.0_f32;
        let mut i = 0;
        while i <= 200 {
            let t = (i as f32) / 200.0;
            let v = resolved.eval(t);
            assert!(
                (0.0..=1.0).contains(&v),
                "resolved spring must stay bounded at t={t}: {v}"
            );
            max = max.max(v);
            min = min.min(v);
            i += 1;
        }
        assert_eq!(min, 0.0, "resolved spring must start exactly at 0");
        assert_eq!(max, 1.0, "resolved spring must end exactly at 1");
        let overshoot = max - 1.0;
        assert_eq!(overshoot, 0.0, "resolved spring overshoot must be zero");
        // Non-finite inputs settle instead of producing overshoot.
        assert_eq!(resolved.eval(f32::NAN), 0.0);
        assert_eq!(resolved.eval(f32::INFINITY), 1.0);
        assert_eq!(resolved.eval(f32::NEG_INFINITY), 0.0);
        assert_eq!(resolved.eval(2.0), 1.0);
        assert_eq!(resolved.eval(-1.0), 0.0);
    }

    #[test]
    fn split_zoom_workspace_durations_stay_within_motion_budget() {
        // CTX-1078 (issue #1755): split open/close, zoom (open/close), and
        // workspace switches stay within the RFC-0002 motion budget:
        // every duration in `0..=500` ms, at most one animation per surface,
        // and a bounded number of concurrently animating surfaces. Excess
        // triggers commit their end state immediately (no queue growth).
        let policy = AnimationPolicy::default();
        for kind in [
            AnimationKind::Open,
            AnimationKind::Close,
            AnimationKind::Workspace,
        ] {
            let ms = policy.duration_ms[kind.index()];
            assert!(
                ms <= 500,
                "{kind:?} duration {ms} ms must stay within the 500 ms ceiling"
            );
            assert!(policy.animates(kind), "{kind:?} must animate by default");
        }
        assert_eq!(MAX_CONCURRENT_ANIMATIONS, 8);
        // The budget is enforced by the tracker, not by queueing.
        let now = t0();
        let mut anim = PanelAnimator::new(policy);
        for i in 0..MAX_CONCURRENT_ANIMATIONS {
            assert!(anim.trigger(AnimationKind::Open, Some(ViewId::new(i as u64 + 1)), now));
        }
        assert!(
            !anim.trigger(AnimationKind::Workspace, None, now),
            "past capacity the workspace switch must cut instantly"
        );
    }
}
