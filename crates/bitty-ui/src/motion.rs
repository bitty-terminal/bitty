//! Core-owned motion hierarchy (UX-23, CTX-0671).
//!
//! Candidate implementation of U-6 (`motion` third). Nothing here is
//! normative, accepted, or verified: the hierarchy shape, the scopes,
//! every duration, and every curve below is a candidate spelling that
//! the UI Runtime RFC accepts or rejects, never this module. The module
//! is English-only.
//!
//! What this module provides:
//!
//! - Ownership split: Lua sets target state only
//!   ([`MotionValue::set_target`]); Rust owns interpolation
//!   ([`MotionValue::step`], [`MotionCurve::sample`]). No Lua callback
//!   runs per frame.
//! - Hierarchy ([`MotionConfig::resolve`]): `motion.default` is the root,
//!   `panel` overrides it, and `panel.open` / `panel.move` /
//!   `panel.close` override `panel`. Missing levels fall through to the
//!   nearest present ancestor, so a single `motion.default` configures
//!   the whole surface.
//! - Mandatory reduced motion: [`MotionConfig::reduced_motion`] forces
//!   every scope to resolve to the instant spec (zero duration, no
//!   wakeups), regardless of configured curves.
//! - Zero wakeups: the crate owns no timers and reads no clock.
//!   [`MotionValue::needs_wakeup`] is `false` for settled and instant
//!   values, so the runtime schedules wakeups only while an animation
//!   is actually in flight.
//!
//! All interpolation is a pure function of its inputs (deterministic,
//! headless testable). All types are bounded and
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::fmt;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on animation duration in milliseconds (inclusive).
///
/// Rejected with [`MotionError::DurationTooLong`]: unbounded durations
/// are an unbounded wakeup budget on the runtime.
pub const MAX_MOTION_DURATION_MS: u16 = 2000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to configure motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MotionError {
    /// A duration exceeds [`MAX_MOTION_DURATION_MS`].
    DurationTooLong {
        /// Rejected duration in milliseconds.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
}

impl fmt::Display for MotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurationTooLong { found, cap } => {
                write!(f, "motion duration too long: {found}ms exceeds cap {cap}ms")
            }
        }
    }
}

impl std::error::Error for MotionError {}

// ---------------------------------------------------------------------------
// Curves and specs
// ---------------------------------------------------------------------------

/// Interpolation curve (Rust-owned; Lua names a scope, never a curve).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MotionCurve {
    /// No animation: values jump to target with no wakeups.
    Instant,
    /// Constant velocity.
    Linear,
    /// Fast start, gentle landing. Default for animated scopes.
    #[default]
    EaseOut,
    /// Gentle start and landing.
    EaseInOut,
}

impl MotionCurve {
    /// Candidate vocabulary spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instant => "instant",
            Self::Linear => "linear",
            Self::EaseOut => "ease-out",
            Self::EaseInOut => "ease-in-out",
        }
    }

    /// Samples the curve at `t` (clamped to `[0, 1]`).
    ///
    /// Endpoints are exact: `sample(0) == 0` and `sample(1) == 1` for
    /// every curve, so settled values always land exactly on target.
    /// Non-finite `t` settles (`1` for non-negative quiet NaN handling
    /// is avoided: NaN clamps to `0`, infinities to their end).
    #[must_use]
    pub fn sample(self, t: f32) -> f32 {
        let clamped = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match self {
            Self::Instant => {
                if clamped >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Linear => clamped,
            Self::EaseOut => 1.0 - (1.0 - clamped) * (1.0 - clamped),
            Self::EaseInOut => {
                if clamped < 0.5 {
                    2.0 * clamped * clamped
                } else {
                    1.0 - (-2.0 * clamped + 2.0).powi(2) / 2.0
                }
            }
        }
    }
}

impl fmt::Display for MotionCurve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One animation recipe: how long, and along which curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MotionSpec {
    duration_ms: u16,
    curve: MotionCurve,
}

impl MotionSpec {
    /// The instant spec: zero duration, no wakeups.
    pub const INSTANT: Self = Self {
        duration_ms: 0,
        curve: MotionCurve::Instant,
    };

    /// Builds a spec.
    ///
    /// # Errors
    ///
    /// Returns [`MotionError::DurationTooLong`] when `duration_ms`
    /// exceeds [`MAX_MOTION_DURATION_MS`].
    pub fn new(duration_ms: u16, curve: MotionCurve) -> Result<Self, MotionError> {
        if duration_ms > MAX_MOTION_DURATION_MS {
            return Err(MotionError::DurationTooLong {
                found: duration_ms,
                cap: MAX_MOTION_DURATION_MS,
            });
        }
        Ok(Self { duration_ms, curve })
    }

    /// Duration in milliseconds.
    #[must_use]
    pub const fn duration_ms(self) -> u16 {
        self.duration_ms
    }

    /// Interpolation curve.
    #[must_use]
    pub const fn curve(self) -> MotionCurve {
        self.curve
    }

    /// Whether this spec animates nothing (zero duration or instant
    /// curve): stepping settles immediately with no wakeups.
    #[must_use]
    pub const fn is_instant(self) -> bool {
        self.duration_ms == 0 || matches!(self.curve, MotionCurve::Instant)
    }
}

impl Default for MotionSpec {
    /// Default: 150ms ease-out (candidate timing for the RFC to accept).
    fn default() -> Self {
        Self {
            duration_ms: 150,
            curve: MotionCurve::EaseOut,
        }
    }
}

// ---------------------------------------------------------------------------
// Hierarchy
// ---------------------------------------------------------------------------

/// Addressable scope in the motion hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MotionScope {
    /// Whole-surface fallback (`motion.default` also covers this).
    Default,
    /// Any panel transition without a more specific override.
    Panel,
    /// Panel open transition (`panel.open`).
    Open,
    /// Panel move/resize transition (`panel.move`).
    Move,
    /// Panel close transition (`panel.close`).
    Close,
}

impl MotionScope {
    /// Candidate vocabulary spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "motion.default",
            Self::Panel => "panel",
            Self::Open => "panel.open",
            Self::Move => "panel.move",
            Self::Close => "panel.close",
        }
    }
}

impl fmt::Display for MotionScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Core-owned motion hierarchy.
///
/// Resolution ([`MotionConfig::resolve`]) walks
/// `panel.open/move/close` -> `panel` -> `motion.default`: the nearest
/// present ancestor wins. When [`MotionConfig::reduced_motion`] is set,
/// every scope resolves to [`MotionSpec::INSTANT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MotionConfig {
    default: MotionSpec,
    panel: Option<MotionSpec>,
    open: Option<MotionSpec>,
    move_spec: Option<MotionSpec>,
    close: Option<MotionSpec>,
    reduced_motion: bool,
}

impl MotionConfig {
    /// Builds a hierarchy with only `motion.default` set.
    #[must_use]
    pub const fn new(default: MotionSpec) -> Self {
        Self {
            default,
            panel: None,
            open: None,
            move_spec: None,
            close: None,
            reduced_motion: false,
        }
    }

    /// Sets the `panel` override (builder).
    #[must_use]
    pub const fn with_panel(mut self, spec: MotionSpec) -> Self {
        self.panel = Some(spec);
        self
    }

    /// Sets the `panel.open` override (builder).
    #[must_use]
    pub const fn with_open(mut self, spec: MotionSpec) -> Self {
        self.open = Some(spec);
        self
    }

    /// Sets the `panel.move` override (builder).
    #[must_use]
    pub const fn with_move(mut self, spec: MotionSpec) -> Self {
        self.move_spec = Some(spec);
        self
    }

    /// Sets the `panel.close` override (builder).
    #[must_use]
    pub const fn with_close(mut self, spec: MotionSpec) -> Self {
        self.close = Some(spec);
        self
    }

    /// Enables mandatory reduced motion: every scope resolves instant.
    #[must_use]
    pub const fn with_reduced_motion(mut self, reduced: bool) -> Self {
        self.reduced_motion = reduced;
        self
    }

    /// Whether reduced motion is enabled.
    #[must_use]
    pub const fn reduced_motion(self) -> bool {
        self.reduced_motion
    }

    /// Resolves the spec for `scope` through the hierarchy.
    ///
    /// Reduced motion short-circuits everything to
    /// [`MotionSpec::INSTANT`]; otherwise the nearest present ancestor
    /// wins (`open`/`move`/`close` -> `panel` -> `default`).
    #[must_use]
    pub const fn resolve(self, scope: MotionScope) -> MotionSpec {
        if self.reduced_motion {
            return MotionSpec::INSTANT;
        }
        match scope {
            MotionScope::Default => self.default,
            MotionScope::Panel => match self.panel {
                Some(spec) => spec,
                None => self.default,
            },
            MotionScope::Open => match self.open {
                Some(spec) => spec,
                None => match self.panel {
                    Some(spec) => spec,
                    None => self.default,
                },
            },
            MotionScope::Move => match self.move_spec {
                Some(spec) => spec,
                None => match self.panel {
                    Some(spec) => spec,
                    None => self.default,
                },
            },
            MotionScope::Close => match self.close {
                Some(spec) => spec,
                None => match self.panel {
                    Some(spec) => spec,
                    None => self.default,
                },
            },
        }
    }
}

impl Default for MotionConfig {
    /// Single `motion.default` (150ms ease-out), no overrides.
    fn default() -> Self {
        Self::new(MotionSpec::default())
    }
}

// ---------------------------------------------------------------------------
// Interpolated values (Lua target, Rust motion)
// ---------------------------------------------------------------------------

/// One animated scalar: Lua sets the target, Rust steps toward it.
///
/// `current` is presentation state only: it never writes back into the
/// retained tree, the grid, or any Lua-visible state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionValue {
    current: f32,
    target: f32,
    spec: MotionSpec,
}

impl MotionValue {
    /// Builds a settled value (`current == target`).
    #[must_use]
    pub const fn new(resting: f32, spec: MotionSpec) -> Self {
        Self {
            current: resting,
            target: resting,
            spec,
        }
    }

    /// Current interpolated position.
    #[must_use]
    pub const fn current(self) -> f32 {
        self.current
    }

    /// Lua-set target position.
    #[must_use]
    pub const fn target(self) -> f32 {
        self.target
    }

    /// Active spec.
    #[must_use]
    pub const fn spec(self) -> MotionSpec {
        self.spec
    }

    /// Retargets (the only Lua-reachable mutation).
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
        if self.spec.is_instant() {
            self.current = target;
        }
    }

    /// Replaces the spec (scope resolution changed); keeps positions.
    pub fn set_spec(&mut self, spec: MotionSpec) {
        self.spec = spec;
        if self.spec.is_instant() {
            self.current = self.target;
        }
    }

    /// Whether `current` reached `target`.
    #[must_use]
    pub fn is_settled(self) -> bool {
        self.current == self.target
    }

    /// Whether the runtime must schedule wakeups for this value:
    /// unsettled and animated. Settled and instant values are silent
    /// (zero wakeups).
    #[must_use]
    pub fn needs_wakeup(self) -> bool {
        !self.is_settled() && !self.spec.is_instant()
    }

    /// Advances toward target by normalized progress `t` in `[0, 1]`
    /// (elapsed / duration, supplied by the runtime clock the crate
    /// never reads) and returns the new position.
    ///
    /// `t <= 0` holds `current`; `t >= 1` (or an instant spec) lands
    /// exactly on `target`.
    pub fn step(&mut self, t: f32) -> f32 {
        if self.spec.is_instant() {
            self.current = self.target;
            return self.current;
        }
        if t.is_nan() || t <= 0.0 {
            return self.current;
        }
        if t >= 1.0 {
            self.current = self.target;
            return self.current;
        }
        let amount = self.spec.curve.sample(t);
        self.current = self.current + (self.target - self.current) * amount;
        if amount >= 1.0 {
            self.current = self.target;
        }
        self.current
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(ms: u16) -> MotionSpec {
        MotionSpec::new(ms, MotionCurve::Linear).expect("valid spec")
    }

    #[test]
    fn overlong_duration_fails_closed() {
        let err = MotionSpec::new(MAX_MOTION_DURATION_MS + 1, MotionCurve::Linear)
            .expect_err("overlong duration must fail");
        assert_eq!(
            err,
            MotionError::DurationTooLong {
                found: MAX_MOTION_DURATION_MS + 1,
                cap: MAX_MOTION_DURATION_MS,
            }
        );
        assert_eq!(
            err.to_string(),
            "motion duration too long: 2001ms exceeds cap 2000ms"
        );
    }

    #[test]
    fn resolution_falls_through_to_nearest_ancestor() {
        let default = spec(100);
        let panel = spec(200);
        let open = spec(300);
        let config = MotionConfig::new(default).with_panel(panel).with_open(open);
        assert_eq!(config.resolve(MotionScope::Default), default);
        assert_eq!(config.resolve(MotionScope::Panel), panel);
        assert_eq!(config.resolve(MotionScope::Open), open);
        // Move and close fall through to `panel`, not to each other.
        assert_eq!(config.resolve(MotionScope::Move), panel);
        assert_eq!(config.resolve(MotionScope::Close), panel);
        // Without overrides everything is `motion.default`.
        let bare = MotionConfig::new(default);
        for scope in [
            MotionScope::Default,
            MotionScope::Panel,
            MotionScope::Open,
            MotionScope::Move,
            MotionScope::Close,
        ] {
            assert_eq!(bare.resolve(scope), default, "{scope}");
        }
    }

    #[test]
    fn reduced_motion_forces_every_scope_instant() {
        let config = MotionConfig::new(spec(400))
            .with_panel(spec(300))
            .with_open(spec(200))
            .with_move(spec(200))
            .with_close(spec(100))
            .with_reduced_motion(true);
        assert!(config.reduced_motion());
        for scope in [
            MotionScope::Default,
            MotionScope::Panel,
            MotionScope::Open,
            MotionScope::Move,
            MotionScope::Close,
        ] {
            let resolved = config.resolve(scope);
            assert_eq!(resolved, MotionSpec::INSTANT, "{scope}");
            assert!(resolved.is_instant());
        }
    }

    #[test]
    fn curve_endpoints_are_exact_and_monotone() {
        for curve in [
            MotionCurve::Instant,
            MotionCurve::Linear,
            MotionCurve::EaseOut,
            MotionCurve::EaseInOut,
        ] {
            assert_eq!(curve.sample(0.0), 0.0, "{curve}");
            assert_eq!(curve.sample(1.0), 1.0, "{curve}");
            assert_eq!(curve.sample(f32::NAN), 0.0, "{curve}");
            let mut prev = 0.0_f32;
            let mut t = 0.0_f32;
            while t <= 1.0 {
                let next = curve.sample(t);
                assert!(next >= prev, "{curve} must not go backwards at t={t}");
                prev = next;
                t += 0.05;
            }
        }
        assert_eq!(MotionCurve::Linear.sample(0.5), 0.5);
    }

    #[test]
    fn retarget_steps_to_settled_with_zero_terminal_wakeup() {
        let mut value = MotionValue::new(0.0, spec(150));
        assert!(!value.needs_wakeup());
        value.set_target(100.0);
        assert!(!value.is_settled());
        assert!(value.needs_wakeup());
        assert_eq!(value.step(0.0), 0.0);
        assert!(value.needs_wakeup());
        let landed = value.step(1.0);
        assert_eq!(landed, 100.0);
        assert!(value.is_settled());
        assert!(!value.needs_wakeup());
    }

    #[test]
    fn instant_values_never_need_wakeups() {
        let mut value = MotionValue::new(0.0, MotionSpec::INSTANT);
        value.set_target(50.0);
        assert!(value.is_settled());
        assert!(!value.needs_wakeup());
        assert_eq!(value.current(), 50.0);
        // A scope change to instant settles in place.
        let mut animated = MotionValue::new(0.0, spec(150));
        animated.set_target(80.0);
        assert!(animated.needs_wakeup());
        animated.set_spec(MotionSpec::INSTANT);
        assert!(animated.is_settled());
        assert!(!animated.needs_wakeup());
    }

    #[test]
    fn scope_spellings_cover_the_hierarchy() {
        assert_eq!(MotionScope::Default.as_str(), "motion.default");
        assert_eq!(MotionScope::Panel.as_str(), "panel");
        assert_eq!(MotionScope::Open.as_str(), "panel.open");
        assert_eq!(MotionScope::Move.as_str(), "panel.move");
        assert_eq!(MotionScope::Close.as_str(), "panel.close");
    }
}
