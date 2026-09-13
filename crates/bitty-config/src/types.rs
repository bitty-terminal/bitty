//! Typed configuration structs for the draft `ConfigPlan` pipeline.
//!
//! These types are **candidate** shapes derived from
//! `bitty-docs/docs/configuration/lua-and-xdg.md` and the proposed
//! configuration-model RFC. They are pure data, cloneable, comparable, and
//! validated without I/O.
//!
//! # Draft status
//!
//! The RFC is still `Proposed` and may change. Field sets, defaults, and
//! validation thresholds are **not normative** and will track the accepted
//! contract once an RFC is accepted. See crate-level docs for the mapping
//! table.

use crate::error::ConfigError;
use crate::keymap::ModKey;

/// Upper bounds that keep every structure bounded against untrusted input
/// (threat T-01).
pub const MAX_FONT_FAMILY_LEN: usize = 128;
pub const MAX_THEME_LEN: usize = 64;
pub const MAX_PLUGIN_ID_LEN: usize = 128;
pub const MAX_KEYMAPS: usize = 1024;
pub const MAX_PLUGINS: usize = 1024;
pub const MAX_SHELL_LEN: usize = 1024;

/// Default lines scrolled per wheel notch (LineDelta unit 1.0).
/// Matches alacritty/ghostty-class `3` lines per tick.
pub const DEFAULT_SCROLL_LINES_PER_NOTCH: u32 = 3;

/// Maximum lines per wheel notch (per-frame SGR/viewport cap is 32).
pub const MAX_SCROLL_LINES_PER_NOTCH: u32 = 32;

/// Default smooth-scroll (PixelDelta) pixels per wheel notch.
/// Matches the default 8x16 cell height (one cell per notch before the
/// lines multiplier).
pub const DEFAULT_SCROLL_PIXELS_PER_NOTCH: u32 = 16;

/// Maximum smooth-scroll pixels per wheel notch.
pub const MAX_SCROLL_PIXELS_PER_NOTCH: u32 = 256;

/// Default inner panel gap (`layout.gaps_in`), in cells: edge-to-edge.
pub const DEFAULT_LAYOUT_GAPS_IN: u32 = 0;

/// Default outer panel gap (`layout.gaps_out`), in cells: edge-to-edge.
pub const DEFAULT_LAYOUT_GAPS_OUT: u32 = 0;

/// Maximum panel gap in cells (either axis).
///
/// Cells are coarse (one cell is ~10px wide at the default 10x22 cell), so 16
/// cells (~144px) is already far past tasteful; the bound exists to keep
/// untrusted input bounded (threat T-01), not to bless huge gaps. Larger
/// values fail closed like every other config bound.
pub const MAX_LAYOUT_GAP_CELLS: u32 = 16;

/// Default Core-owned workspace decoration sibling gap (CTX-0292; unified
/// CTX-0333): 6 logical px. CTX-0333 raised this from the earlier `4` so the
/// default sibling (panel-to-panel / panel-to-terminal) gap equals the
/// container (`gaps_out`) gap and reads as one spacing.
pub const DEFAULT_DECORATION_GAPS_IN_PX: u32 = 6;

/// Default outer workspace decoration gap (CTX-0292): 6 logical px.
pub const DEFAULT_DECORATION_GAPS_OUT_PX: u32 = 6;

/// Default View frame border thickness (CTX-0292): 2 logical px.
pub const DEFAULT_DECORATION_BORDER_PX: u32 = 2;

/// Default View frame corner radius (CTX-0292): 6 logical px.
pub const DEFAULT_DECORATION_RADIUS_PX: u32 = 6;

/// Default content inset in logical px (CTX-0333): 6.
///
/// Inner padding between the View frame's border and its painted content on
/// every side, so text never sits flush against the panel margin line.
pub const DEFAULT_DECORATION_CONTENT_INSET_PX: u32 = 6;

/// Maximum decoration gap in logical px (either axis), accepted CTX-0118.
pub const MAX_DECORATION_GAP_PX: u32 = 32;

/// Maximum content inset in logical px (CTX-0333), same bound as the gaps.
pub const MAX_DECORATION_CONTENT_INSET_PX: u32 = 32;

/// Maximum View frame border thickness in logical px, accepted CTX-0118.
pub const MAX_DECORATION_BORDER_PX: u32 = 8;

/// Maximum focused/idle **outline width** in logical px (CTX-0344,
/// RFC-0001/OQ-045): `0..=16`.
///
/// Deliberately wider than [`MAX_DECORATION_BORDER_PX`] (the content-inset
/// geometry bound) because a focused outline may need to stand out from a
/// thick idle one. This is the paint-only ring thickness; it never moves the
/// content grid. Out-of-range values fail closed, never clamp.
pub const MAX_DECORATION_BORDER_WIDTH_PX: u32 = 16;

/// Safe-mode outline width (CTX-0344, RFC-0001/OQ-045): focused `1`, idle `1`.
///
/// Equal widths supply no non-color focus cue, so safe mode satisfies AC-2
/// through the accepted safe color pair (`#FFFFFF` / `#808080`) instead.
pub const SAFE_DECORATION_BORDER_WIDTH_PX: u32 = 1;

/// Maximum View frame corner radius in logical px, accepted CTX-0118.
pub const MAX_DECORATION_RADIUS_PX: u32 = 16;

/// Safe-mode decoration gaps (CTX-0292 rule 5: `bitty --safe` = `0/0/1/0/0`).
pub const SAFE_DECORATION_GAPS_IN_PX: u32 = 0;

/// Safe-mode decoration outer gap; see [`SAFE_DECORATION_GAPS_IN_PX`].
pub const SAFE_DECORATION_GAPS_OUT_PX: u32 = 0;

/// Safe-mode View frame border thickness (`1`, not the `2` default).
pub const SAFE_DECORATION_BORDER_PX: u32 = 1;

/// Safe-mode View frame corner radius (`0`, not the `6` default).
pub const SAFE_DECORATION_RADIUS_PX: u32 = 0;

/// Safe-mode content inset (`0`, not the `6` default), so safe mode keeps
/// the legacy border-only content geometry (CTX-0333).
pub const SAFE_DECORATION_CONTENT_INSET_PX: u32 = 0;

/// Ratified default focused outline color (CTX-0340, RFC-0001/OQ-039):
/// `#33CCFF` (alpha `FF`). This is also the Bitty Dark `border.focused`
/// theme token.
pub const DEFAULT_DECORATION_BORDER_FOCUSED: OutlineColor = OutlineColor([0x33, 0xCC, 0xFF, 0xFF]);

/// Ratified default idle outline color (CTX-0340, RFC-0001/OQ-039):
/// `#595959AA`. This is also the Bitty Dark `border.idle` theme token.
pub const DEFAULT_DECORATION_BORDER_IDLE: OutlineColor = OutlineColor([0x59, 0x59, 0x59, 0xAA]);

/// Safe-mode focused outline color (CTX-0340): opaque `#FFFFFF`, which
/// satisfies AC-1/AC-2/AC-3 against the Bitty Dark workspace background.
pub const SAFE_DECORATION_BORDER_FOCUSED: OutlineColor = OutlineColor([0xFF, 0xFF, 0xFF, 0xFF]);

/// Safe-mode idle outline color (CTX-0340): opaque `#808080`, distinct from
/// the focused color and passing the advisory AC-3 floor.
pub const SAFE_DECORATION_BORDER_IDLE: OutlineColor = OutlineColor([0x80, 0x80, 0x80, 0xFF]);

/// Minimum focused-outline contrast against the workspace background
/// (CTX-0340 AC-1; WCAG 2.1 SC 1.4.11 non-text contrast).
pub const MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST: f64 = 3.0;

/// Minimum focused-outline contrast against the idle outline when no
/// non-color focus cue is implemented (CTX-0340 AC-2).
pub const MIN_OUTLINE_FOCUSED_IDLE_CONTRAST: f64 = 3.0;

/// Advisory minimum idle-outline contrast against the workspace background
/// (CTX-0340 AC-3); never enforced, reported by `bitty config check`.
pub const MIN_OUTLINE_IDLE_BACKGROUND_CONTRAST: f64 = 1.5;

/// Maximum accepted outline color spelling length: `#RRGGBBAA` (9 bytes).
pub const MAX_DECORATION_COLOR_LEN: usize = 9;

// ── Panel animations (RFC-0002, CTX-0341) ────────────────────────────────
//
// Accepted contract: a closed transition set (panel open/close, focus change,
// workspace switch) with per-transition integer durations in `0..=500` ms and
// a closed easing enum (`linear | ease_in | ease_out | ease_in_out | spring`).
// `spring` is a reserved leaf whose parameters are deferred, so it resolves to
// `ease_in_out` until a follow-up RFC defines them. Durations and easings fail
// closed; `enabled = false` and `reduced_motion = "always"` are equivalent to
// `0` ms. Renderer-side by default; never interpolates terminal truth.

/// Default panel-open duration in ms (RFC-0002).
pub const DEFAULT_ANIMATION_OPEN_MS: u32 = 150;

/// Default panel-close duration in ms (RFC-0002).
pub const DEFAULT_ANIMATION_CLOSE_MS: u32 = 120;

/// Default focus-change duration in ms (RFC-0002).
pub const DEFAULT_ANIMATION_FOCUS_MS: u32 = 100;

/// Default workspace-switch duration in ms (RFC-0002).
pub const DEFAULT_ANIMATION_WORKSPACE_MS: u32 = 200;

/// Hard upper bound for every animation duration in ms (RFC-0002: `0..=500`).
pub const MAX_ANIMATION_DURATION_MS: u32 = 500;

/// Default `appearance.animations.enabled` (RFC-0002).
pub const DEFAULT_ANIMATIONS_ENABLED: bool = true;

/// Default `appearance.animations.reduced_motion` (RFC-0002).
pub const DEFAULT_REDUCED_MOTION: ReducedMotion = ReducedMotion::Auto;

/// One animatable panel transition (RFC-0002 transition set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationTransition {
    /// A `View` becomes occupied or a Panel is shown.
    Open,
    /// A `View` becomes empty/hidden or a Panel is hidden.
    Close,
    /// The focused `View` changes.
    Focus,
    /// The active `Workspace` changes.
    Workspace,
}

impl AnimationTransition {
    /// Canonical leaf name used in `appearance.animations.duration_ms` /
    /// `easing`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::Focus => "focus",
            Self::Workspace => "workspace",
        }
    }
}

/// Closed easing enum accepted by `appearance.animations.easing.*`.
///
/// `spring` is a reserved leaf: its parameters (stiffness, damping, rest
/// threshold) are deferred, so [`Self::resolved`] maps it to
/// [`Self::EaseInOut`] until a follow-up RFC defines them (RFC-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationEasing {
    /// Constant velocity.
    Linear,
    /// Slow start, fast end.
    EaseIn,
    /// Fast start, slow end.
    EaseOut,
    /// Slow start and end (S-curve).
    EaseInOut,
    /// Reserved name; parameters deferred, resolved as `ease_in_out`.
    Spring,
}

impl AnimationEasing {
    /// Parses the exact lowercase spelling; unknown spellings return `None`
    /// so callers fail closed with a source-attributed diagnostic.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "linear" => Some(Self::Linear),
            "ease_in" => Some(Self::EaseIn),
            "ease_out" => Some(Self::EaseOut),
            "ease_in_out" => Some(Self::EaseInOut),
            "spring" => Some(Self::Spring),
            _ => None,
        }
    }

    /// Canonical spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::EaseIn => "ease_in",
            Self::EaseOut => "ease_out",
            Self::EaseInOut => "ease_in_out",
            Self::Spring => "spring",
        }
    }

    /// The curve actually applied for this easing (RFC-0002 `spring` mapping).
    #[must_use]
    pub fn resolved(self) -> Self {
        match self {
            Self::Spring => Self::EaseInOut,
            other => other,
        }
    }
}

/// Bounded `appearance.animations.reduced_motion` enum (RFC-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReducedMotion {
    /// Follow the platform reduced-motion signal when one exists; otherwise
    /// animate.
    Auto,
    /// Force `0` ms durations.
    Always,
    /// Ignore the platform signal but still respect the duration bounds.
    Never,
}

impl ReducedMotion {
    /// Parses the exact lowercase spelling; unknown spellings return `None`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// Canonical spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// Per-transition durations in milliseconds, each `0..=500` (RFC-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationDurations {
    /// Panel-open duration.
    pub open: u32,
    /// Panel-close duration.
    pub close: u32,
    /// Focus-change duration.
    pub focus: u32,
    /// Workspace-switch duration.
    pub workspace: u32,
}

impl Default for AnimationDurations {
    fn default() -> Self {
        Self {
            open: DEFAULT_ANIMATION_OPEN_MS,
            close: DEFAULT_ANIMATION_CLOSE_MS,
            focus: DEFAULT_ANIMATION_FOCUS_MS,
            workspace: DEFAULT_ANIMATION_WORKSPACE_MS,
        }
    }
}

impl AnimationDurations {
    /// Reads the duration for one transition.
    #[must_use]
    pub fn get(self, transition: AnimationTransition) -> u32 {
        match transition {
            AnimationTransition::Open => self.open,
            AnimationTransition::Close => self.close,
            AnimationTransition::Focus => self.focus,
            AnimationTransition::Workspace => self.workspace,
        }
    }
}

/// Per-transition easings (RFC-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationEasings {
    /// Panel-open easing.
    pub open: AnimationEasing,
    /// Panel-close easing.
    pub close: AnimationEasing,
    /// Focus-change easing.
    pub focus: AnimationEasing,
    /// Workspace-switch easing.
    pub workspace: AnimationEasing,
}

impl Default for AnimationEasings {
    fn default() -> Self {
        Self {
            open: AnimationEasing::EaseOut,
            close: AnimationEasing::EaseIn,
            focus: AnimationEasing::EaseInOut,
            workspace: AnimationEasing::EaseInOut,
        }
    }
}

impl AnimationEasings {
    /// Reads the easing for one transition.
    #[must_use]
    pub fn get(self, transition: AnimationTransition) -> AnimationEasing {
        match transition {
            AnimationTransition::Open => self.open,
            AnimationTransition::Close => self.close,
            AnimationTransition::Focus => self.focus,
            AnimationTransition::Workspace => self.workspace,
        }
    }
}

/// `appearance.animations` — the accepted RFC-0002 panel animation contract.
///
/// This table is owned by the animation contract and does not migrate any
/// existing key. It is renderer-side by default and compositor-gated: no
/// platform surface is required, and an unsupported platform simply renders
/// the final committed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationsConfig {
    /// Master switch; `false` is equivalent to `0` ms durations while the
    /// final-state contract is kept.
    pub enabled: bool,
    /// Per-transition durations.
    pub duration_ms: AnimationDurations,
    /// Per-transition easings.
    pub easing: AnimationEasings,
    /// Reduced-motion mode.
    pub reduced_motion: ReducedMotion,
}

impl Default for AnimationsConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ANIMATIONS_ENABLED,
            duration_ms: AnimationDurations::default(),
            easing: AnimationEasings::default(),
            reduced_motion: DEFAULT_REDUCED_MOTION,
        }
    }
}

impl AnimationsConfig {
    /// Resolves the duration actually applied for `transition`.
    ///
    /// Fail-closed at the value level: any disabled path yields `0` (instant)
    /// rather than an unbounded or degraded state. `safe_mode` forces `0`
    /// regardless of configuration and the platform signal (RFC-0002);
    /// `platform_reduced` is the platform reduced-motion signal consulted
    /// only by [`ReducedMotion::Auto`].
    #[must_use]
    pub fn effective_duration_ms(
        &self,
        transition: AnimationTransition,
        platform_reduced: bool,
        safe_mode: bool,
    ) -> u32 {
        if safe_mode || !self.enabled {
            return 0;
        }
        let reduced = match self.reduced_motion {
            ReducedMotion::Always => true,
            ReducedMotion::Never => false,
            ReducedMotion::Auto => platform_reduced,
        };
        if reduced {
            return 0;
        }
        self.duration_ms.get(transition)
    }

    /// The applied easing for `transition` with `spring` resolved.
    #[must_use]
    pub fn effective_easing(&self, transition: AnimationTransition) -> AnimationEasing {
        self.easing.get(transition).resolved()
    }

    /// Applies one layer's per-field overrides onto this value (RFC-0002
    /// "deep-merges as a table while each field is scalar-replace").
    ///
    /// Every `Some` leaf replaces the corresponding field; `None` inherits
    /// the receiver's value, so a layer that sets only one duration or easing
    /// never resets the others.
    pub fn apply_overrides(&mut self, over: &AnimationsOverride) {
        if let Some(v) = over.enabled {
            self.enabled = v;
        }
        if let Some(v) = over.reduced_motion {
            self.reduced_motion = v;
        }
        if let Some(v) = over.duration_open {
            self.duration_ms.open = v;
        }
        if let Some(v) = over.duration_close {
            self.duration_ms.close = v;
        }
        if let Some(v) = over.duration_focus {
            self.duration_ms.focus = v;
        }
        if let Some(v) = over.duration_workspace {
            self.duration_ms.workspace = v;
        }
        if let Some(v) = over.easing_open {
            self.easing.open = v;
        }
        if let Some(v) = over.easing_close {
            self.easing.close = v;
        }
        if let Some(v) = over.easing_focus {
            self.easing.focus = v;
        }
        if let Some(v) = over.easing_workspace {
            self.easing.workspace = v;
        }
    }

    /// Validates the duration bounds fail-closed (each field `0..=500`).
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            (
                "appearance.animations.duration_ms.open",
                self.duration_ms.open,
            ),
            (
                "appearance.animations.duration_ms.close",
                self.duration_ms.close,
            ),
            (
                "appearance.animations.duration_ms.focus",
                self.duration_ms.focus,
            ),
            (
                "appearance.animations.duration_ms.workspace",
                self.duration_ms.workspace,
            ),
        ] {
            if value > MAX_ANIMATION_DURATION_MS {
                return Err(ConfigError::validation(
                    field,
                    format!("must be within [0, {MAX_ANIMATION_DURATION_MS}]"),
                ));
            }
        }
        Ok(())
    }
}

/// One layer's optional `appearance.animations` leaves (RFC-0002, CTX-0341).
///
/// Every leaf is `Option` so "this layer says nothing" is distinguishable
/// from an explicit value; merge applies each `Some` leaf by scalar replace
/// and inherits the rest. Exactly one of the four duration leaves (and one of
/// the four easing leaves) is expected per declared table; omitted leaves
/// inherit the lower-precedence value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnimationsOverride {
    /// `appearance.animations.enabled`.
    pub enabled: Option<bool>,
    /// `appearance.animations.reduced_motion`.
    pub reduced_motion: Option<ReducedMotion>,
    /// `appearance.animations.duration_ms.open`.
    pub duration_open: Option<u32>,
    /// `appearance.animations.duration_ms.close`.
    pub duration_close: Option<u32>,
    /// `appearance.animations.duration_ms.focus`.
    pub duration_focus: Option<u32>,
    /// `appearance.animations.duration_ms.workspace`.
    pub duration_workspace: Option<u32>,
    /// `appearance.animations.easing.open`.
    pub easing_open: Option<AnimationEasing>,
    /// `appearance.animations.easing.close`.
    pub easing_close: Option<AnimationEasing>,
    /// `appearance.animations.easing.focus`.
    pub easing_focus: Option<AnimationEasing>,
    /// `appearance.animations.easing.workspace`.
    pub easing_workspace: Option<AnimationEasing>,
}

impl AnimationsOverride {
    /// Validates present duration leaves fail-closed (each `0..=500`).
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("appearance.animations.duration_ms.open", self.duration_open),
            (
                "appearance.animations.duration_ms.close",
                self.duration_close,
            ),
            (
                "appearance.animations.duration_ms.focus",
                self.duration_focus,
            ),
            (
                "appearance.animations.duration_ms.workspace",
                self.duration_workspace,
            ),
        ] {
            if let Some(v) = value {
                if v > MAX_ANIMATION_DURATION_MS {
                    return Err(ConfigError::validation(
                        field,
                        format!("must be within [0, {MAX_ANIMATION_DURATION_MS}]"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Canonical Core-owned outline color (CTX-0340, RFC-0001/OQ-039).
///
/// Accepted spelling is exactly `#RRGGBB` or `#RRGGBBAA` (8-digit form is
/// RGBA byte order); alpha defaults to `FF` when omitted. Named colors,
/// `#RGB` shorthand, `rgb()`/`rgba()` syntax, gradients, and images are
/// rejected fail-closed by [`Self::parse`]. All channels are unpremultiplied
/// `sRGB` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutlineColor(pub [u8; 4]);

impl OutlineColor {
    /// Builds a color from RGBA bytes.
    #[must_use]
    pub const fn from_rgba(rgba: [u8; 4]) -> Self {
        Self(rgba)
    }

    /// Parses a canonical `#RRGGBB` / `#RRGGBBAA` spelling.
    ///
    /// Returns `None` for any other grammar (including `#RGB`, missing `#`,
    /// wrong digit count, or non-hex bytes). Fail-closed: the caller reports
    /// the offending config key.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.len() > MAX_DECORATION_COLOR_LEN {
            return None;
        }
        let body = trimmed.strip_prefix('#')?;
        let (r, g, b, a) = match body.len() {
            6 => (
                hex_byte(body, 0)?,
                hex_byte(body, 2)?,
                hex_byte(body, 4)?,
                0xFF,
            ),
            8 => (
                hex_byte(body, 0)?,
                hex_byte(body, 2)?,
                hex_byte(body, 4)?,
                hex_byte(body, 6)?,
            ),
            _ => return None,
        };
        Some(Self([r, g, b, a]))
    }

    /// Canonical spelling: `#RRGGBB` when opaque, `#RRGGBBAA` otherwise.
    #[must_use]
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 0xFF {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// True when alpha is fully opaque.
    #[must_use]
    pub const fn is_opaque(self) -> bool {
        self.0[3] == 0xFF
    }

    /// This color composited with straight-alpha src-over onto opaque `bg`.
    #[must_use]
    pub fn composited_over(self, bg: [u8; 3]) -> [u8; 3] {
        let [r, g, b, a] = self.0;
        let a16 = u16::from(a);
        let mix = |src: u8, dst: u8| -> u8 {
            let src = u16::from(src);
            let dst = u16::from(dst);
            (((src * a16) + (dst * (255 - a16)) + 127) / 255) as u8
        };
        [mix(r, bg[0]), mix(g, bg[1]), mix(b, bg[2])]
    }

    /// WCAG 2.1 contrast ratio of this color (composited over `bg`) against
    /// `bg`. `bg` is assumed opaque.
    #[must_use]
    pub fn contrast_over(self, bg: [u8; 3]) -> f64 {
        contrast_ratio(self.composited_over(bg), bg)
    }

    /// WCAG 2.1 contrast ratio between this color and `other`, both
    /// composited over the same opaque `bg`.
    #[must_use]
    pub fn contrast_with(self, other: Self, bg: [u8; 3]) -> f64 {
        contrast_ratio(self.composited_over(bg), other.composited_over(bg))
    }
}

impl std::fmt::Display for OutlineColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Parses one 2-hex-digit byte at `start` in `body`.
fn hex_byte(body: &str, start: usize) -> Option<u8> {
    let bytes = body.as_bytes().get(start..start + 2)?;
    let hi = (bytes[0] as char).to_digit(16)?;
    let lo = (bytes[1] as char).to_digit(16)?;
    Some(((hi << 4) | lo) as u8)
}

/// WCAG 2.1 relative luminance of an opaque sRGB byte triple.
fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let channel = |c: u8| -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

/// WCAG 2.1 contrast ratio between two opaque colors.
fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// One resolved focused/idle outline pair (CTX-0340).
///
/// Produced by [`DecorationConfig::resolve_outline`] after applying the
/// accepted resolution order; this is what the render path consumes per
/// `View`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOutlineColors {
    /// Outline for the focused `View`.
    pub focused: OutlineColor,
    /// Outline for every idle (unfocused) `View`.
    pub idle: OutlineColor,
}

/// One resolved focused/idle outline-width pair in logical pixels (CTX-0344,
/// RFC-0001/OQ-045).
///
/// Produced by [`DecorationConfig::resolve_outline_width`] after applying the
/// accepted resolution order (`decoration.border` then
/// `decoration.border_width` then the explicit `_focused` / `_idle` pair).
/// The render path scales these at the live DPI factor exactly like
/// `decoration.border`; a focused/idle delta of `1` logical px stays at
/// least `1` physical px at any DPI. Values are paint-only: the content
/// rectangle stays inset by `border + content_inset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOutlineWidths {
    /// Focused `View` ring thickness in logical px.
    pub focused: u32,
    /// Idle (unfocused) `View` ring thickness in logical px.
    pub idle: u32,
}

/// Default selection auto-copy behavior (CTX-0191).
/// `true` preserves the ghostty-class copy-on-select: a committed mouse
/// selection auto-copies to the clipboard (which best-effort syncs primary).
/// `false` leaves the highlight in place and copies only via the explicit
/// `copy_to_clipboard` chord (Ctrl+Shift+C).
pub const DEFAULT_SELECTION_AUTO_COPY: bool = true;

/// Default focus-follows-mouse behavior (CTX-0260).
/// `false` preserves click-to-focus: hovering never moves keyboard focus.
/// `true` opts into hover moving keyboard focus to the hovered pane.
pub const DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE: bool = false;

/// Default hover-activation delay in milliseconds (CTX-0334).
/// `0` activates immediately on pointer entry, matching the CTX-0260
/// behavior; a positive value makes the pointer dwell in the hovered pane
/// for that long before keyboard focus moves.
pub const DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS: u32 = 0;

/// Maximum accepted hover-activation delay in milliseconds (CTX-0334).
///
/// Bounds the dwell timer so a hostile or mistaken configuration cannot
/// stall activation indefinitely (KDE-style focus delay); `validate()`
/// rejects values above it fail-closed and `bitty-runtime` mirrors the bound.
pub const MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS: u32 = 2_000;

/// Default scrollbar mode (CTX-0362): `auto`.
///
/// The overlay is transparent at rest and reveals on mouse proximity/hover
/// or an active drag, so it stays geometry-neutral for existing users (no
/// track, no thumb, zero fills, zero layout delta until engaged) while
/// making the scrollback thumb discoverable without configuration.
/// `always` pins it visible whenever scrollback exists; `hidden` disables
/// it entirely.
pub const DEFAULT_SCROLLBAR_MODE: &str = "auto";

/// Default scrollbar thumb width in logical pixels (CTX-0181).
///
/// Scaled by the live DPI factor exactly like `window.padding`, so the
/// overlay keeps its physical size across displays.
pub const DEFAULT_SCROLLBAR_WIDTH: u32 = 8;

/// Minimum scrollbar thumb width in logical pixels (CTX-0181).
pub const MIN_SCROLLBAR_WIDTH_PX: u32 = 1;

/// Maximum scrollbar thumb width in logical pixels (CTX-0181).
///
/// An overlay wider than a cell would swallow grid readability; 32px stays
/// well under one default cell row while keeping untrusted input bounded
/// (threat T-01). Larger values fail closed like every other config bound.
pub const MAX_SCROLLBAR_WIDTH_PX: u32 = 32;

/// Default window corner radius in physical px (CTX-0241 S0).
///
/// Zero means square corners (zero-cost integer fast path everywhere);
/// rounding itself is a later stage. The value is parsed, validated,
/// merged, and reported, with no render effect in S0.
pub const DEFAULT_WINDOW_RADIUS_PX: u32 = 0;

/// Maximum window corner radius in physical px (CTX-0241 S0: `0..=24`).
///
/// 24px covers tasteful rounding at terminal window sizes while keeping
/// untrusted input bounded (threat T-01). Larger values fail closed.
pub const MAX_WINDOW_RADIUS_PX: u32 = 24;

/// Default window padding in logical pixels (CTX-0223).
///
/// 8px keeps ghostty/alacritty-class breathing room between the grid and the
/// window edge; the padding band keeps the theme background.
pub const DEFAULT_WINDOW_PADDING: u32 = 8;

/// Maximum window padding in logical pixels (`0..=64`).
///
/// Bounds untrusted input (threat T-01); larger values fail closed. Mirrored
/// by value in `bitty-runtime::config::MAX_WINDOW_PADDING` (runtime cannot
/// depend on `bitty-config`); the pairing is pinned by the app-level test.
pub const MAX_WINDOW_PADDING: u32 = 64;

/// Default font family: Nerd-Font-patched JetBrains Mono.
///
/// Matches the CTX-0157 acceptance probe (`JetBrainsMono Nerd Font 12pt`
/// side-by-side vs ghostty must show no material difference) and renders
/// starship/opencode Nerd glyphs out of the box. System `monospace` remains
/// in the chain via [`FONT_FALLBACK_CHAIN`], so bare installs
/// without the Nerd font still start (headless fallback path in
/// `bitty-runtime`).
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Default point size: 12pt on Linux.
///
/// Matches ghostty `font-size = 12` (Linux; 13 on macOS) and keeps the
/// "smaller than normal" complaint closed: 12.0 was already the prior
/// default and stays. Kitty defaults to 11.0; bitty stays at ghostty parity.
pub const DEFAULT_FONT_SIZE: f32 = 12.0;

/// Default line-height multiplier: 1.375x.
///
/// Legacy design cell was `8x16` with no breathing room. Measured
/// `JetBrainsMonoNerdFont-Regular.ttf` (`fontTools`, UPM 1000,
/// ascent 1020 / descent -300): at 12pt (16px em) the true advance is
/// `0.6 * 16 = 9.6px` and the true line is `1320/1000 * 16 = 21.1px`.
/// Ghostty defaults to `adjust-cell-height = null` (pure font metrics) and
/// kitty to no `modify_font` adjustment; bitty's legacy `8x16` is ~20%
/// too narrow and ~32% too short vs those metrics.
///
/// CTX-0237 raster truth (headless crossfont probe on the live seat,
/// FreeType-rounded): average advance `10px`, line `22px`, descent `-5px`;
/// block/powerline glyphs rasterize `top=17, h=22`. The CTX-0157 compromise
/// (`1.2` -> `round(16 * 1.2) = 19px`, kitty-11pt-like) left tall glyphs
/// overflowing the cell, and neighbor-row repaints erased the overhang
/// ("tops cut off"). `1.375` gives `round(16 * 1.375) = 22px` — the full
/// measured line box, so in-font tall glyphs fit with zero overhang while
/// outliers (box drawing) keep the permitted overdraw path.
pub const DEFAULT_LINE_HEIGHT: f32 = 1.375;

/// Default letter-spacing: 2.0px.
///
/// Legacy advance 8px vs measured 10px at 12pt (CTX-0237 probe): `+2px`
/// gives effective width 10, matching the rounded raster advance so glyphs
/// sit on their natural grid instead of crowding one pixel per cell.
/// Ghostty `adjust-cell-width = null`; the `+2` closes the cramped legacy
/// base exactly instead of stopping at the CTX-0157 compromise (`+1`).
pub const DEFAULT_LETTER_SPACING: f32 = 2.0;

/// Legacy design cell (pre-CTX-0157): the compiled base that spacing
/// applies to. Kept explicit so [`FontConfig::effective_cell`] stays
/// deterministic and testable.
pub const BASE_CELL_WIDTH: u32 = 8;
/// Legacy design cell height (see [`BASE_CELL_WIDTH`]).
pub const BASE_CELL_HEIGHT: u32 = 16;

/// Maximum number of platform fallback families appended after the primary.
///
/// Bound from the text-rendering RFC ("Fallback chain construction",
/// `MAX_FALLBACK_FAMILIES = 8`): enumeration stays deterministic and the
/// per-scalar walk stays cheap.
pub const MAX_FALLBACK_FAMILIES: usize = 8;

/// Maximum total chain depth (primary plus tails).
///
/// RFC `MAX_FALLBACK_DEPTH = 12`; the pinned chains stay well below it, and
/// `bitty-render::fallback` never walks more faces than the chain holds.
pub const MAX_FALLBACK_DEPTH: usize = 12;

/// Braille/symbols fallback family (CTX-0163, issue #263).
///
/// `fc-query` evidence on the reference host: `DejaVu Sans Mono` covers
/// `2500-262f` (box drawing + block elements `U+2580-U+259F`) but has no
/// `28xx` row, so braille patterns `U+2800-U+28FF` (btop CPU graphs) fall
/// through; `Noto Sans Symbols 2` covers `2800-28ff` and resolves via
/// fontconfig (`fc-match "Noto Sans Symbols 2"`). Ships in `noto-fonts`,
/// already an `optdepend` in `packaging/PKGBUILD` — no new dependency.
pub const SYMBOLS_FALLBACK_FAMILY: &str = "Noto Sans Symbols 2";

/// Emoji-capable fallback family for the running platform (CTX-0368).
///
/// The render path flattens any color bitmap to its alpha coverage in the
/// atlas (monochrome tint by the cell foreground); the face is still the
/// coverage authority for emoji-presentation scalars (`U+1F300+`, `U+2705`,
/// `U+2699`-class) that the monospace and symbols tails miss. Color emoji
/// *color* rendering remains an open follow-up in the text-rendering RFC.
#[cfg(target_os = "linux")]
pub const EMOJI_FALLBACK_FAMILY: &str = "Noto Color Emoji";
/// Emoji fallback family on macOS (see [`EMOJI_FALLBACK_FAMILY`]).
#[cfg(target_os = "macos")]
pub const EMOJI_FALLBACK_FAMILY: &str = "Apple Color Emoji";
/// Emoji fallback family on Windows (see [`EMOJI_FALLBACK_FAMILY`]).
#[cfg(windows)]
pub const EMOJI_FALLBACK_FAMILY: &str = "Segoe UI Emoji";
/// Emoji fallback family on other targets (see [`EMOJI_FALLBACK_FAMILY`]).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub const EMOJI_FALLBACK_FAMILY: &str = "Noto Color Emoji";

/// Documented monospace/Nerd fallback stack for the running platform.
///
/// Order on Linux: configured primary (Nerd-patched by default) -> unpatched
/// `JetBrains Mono` -> system `monospace` (fontconfig/WC) ->
/// `DejaVu Sans Mono` (widely available, covers box drawing + block
/// elements `U+2580-U+259F`) -> [`SYMBOLS_FALLBACK_FAMILY`] (covers braille
/// patterns `U+2800-U+28FF` for TUI graphs such as btop) ->
/// [`EMOJI_FALLBACK_FAMILY`] (emoji-presentation scalars the mono/symbols
/// faces miss, for example `U+2699 GEAR`). Mirrors ghostty (embedded
/// JetBrains Mono variable + symbols-only Nerd fallback, always present)
/// and kitty (`font_family = "monospace"` + builtin Nerd font,
/// `set_font_family(..., add_builtin_nerd_font=True)`).
///
/// macOS and Windows substitute their system equivalents (Menlo/Monaco,
/// Apple Braille, Apple Symbols, Apple Color Emoji; Consolas/Cascadia Mono,
/// Segoe UI Symbol, Segoe UI Emoji); every chain is a fixed, deterministic,
/// bounded list (`<= MAX_FALLBACK_FAMILIES` tails, `<= MAX_FALLBACK_DEPTH`
/// total), never a runtime enumeration. Platform fontconfig/CoreText/
/// DirectWrite enumeration policy remains an open question under ADR-0004.
///
/// Per-glyph coverage fallback is implemented by
/// `bitty-render::fallback::FallbackRasterizer`, which walks this chain on
/// a missing glyph (reference: ghostty
/// `src/font/CodepointResolver.zig`, per-codepoint fallback via discovery).
/// Full shaping stays deferred to the text RFC (ADR-0004 "Wrap" row); this
/// chain is family-level attempt order for embedders: try each in order
/// until `load_font` succeeds, ending in headless.
/// [`FontConfig::fallback_chain`] builds the configured-first variant.
#[cfg(target_os = "linux")]
pub const FONT_FALLBACK_CHAIN: &[&str] = &[
    DEFAULT_FONT_FAMILY,
    "JetBrains Mono",
    "monospace",
    "DejaVu Sans Mono",
    SYMBOLS_FALLBACK_FAMILY,
    EMOJI_FALLBACK_FAMILY,
];

/// Documented fallback stack on macOS (see [`FONT_FALLBACK_CHAIN`]).
#[cfg(target_os = "macos")]
pub const FONT_FALLBACK_CHAIN: &[&str] = &[
    DEFAULT_FONT_FAMILY,
    "Menlo",
    "Monaco",
    "Apple Braille",
    "Apple Symbols",
    EMOJI_FALLBACK_FAMILY,
    "Arial Unicode MS",
];

/// Documented fallback stack on Windows (see [`FONT_FALLBACK_CHAIN`]).
#[cfg(windows)]
pub const FONT_FALLBACK_CHAIN: &[&str] = &[
    DEFAULT_FONT_FAMILY,
    "Consolas",
    "Cascadia Mono",
    "Segoe UI Symbol",
    EMOJI_FALLBACK_FAMILY,
    "Arial Unicode MS",
];

/// Documented fallback stack on other targets (see [`FONT_FALLBACK_CHAIN`]).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub const FONT_FALLBACK_CHAIN: &[&str] = &[
    DEFAULT_FONT_FAMILY,
    "DejaVu Sans Mono",
    SYMBOLS_FALLBACK_FAMILY,
    EMOJI_FALLBACK_FAMILY,
];

/// Font configuration.
///
/// `family`/`size` match ghostty Linux defaults (`JetBrainsMono Nerd Font`
/// 12pt for the acceptance probe); `line_height`/`letter_spacing` give the
/// slight breathing room the legacy `8x16` cell lacked. All four are
/// tunable via `init.lua` `font = { family, size, line_height,
/// letter_spacing }` (new keys optional, defaulted — existing
/// `{ family, size }` tables keep working).
#[derive(Debug, Clone, PartialEq)]
pub struct FontConfig {
    /// Font family, trimmed, non-empty.
    pub family: String,
    /// Point size, finite, `> 0` and `<= 128`.
    pub size: f32,
    /// Line-height multiplier, finite within `[1.0, 2.0]`.
    pub line_height: f32,
    /// Extra advance in px, finite within `[0.0, 8.0]`.
    pub letter_spacing: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: DEFAULT_FONT_FAMILY.to_string(),
            size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            letter_spacing: DEFAULT_LETTER_SPACING,
        }
    }
}

impl FontConfig {
    /// Validate this font config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let fam = self.family.trim();
        if fam.is_empty() {
            return Err(ConfigError::validation(
                "font.family",
                "must not be empty or whitespace",
            ));
        }
        if fam.len() > MAX_FONT_FAMILY_LEN {
            return Err(ConfigError::validation(
                "font.family",
                format!("must be <= {MAX_FONT_FAMILY_LEN} bytes"),
            ));
        }
        if !(self.size.is_finite() && self.size > 0.0 && self.size <= 128.0) {
            return Err(ConfigError::validation(
                "font.size",
                "must be finite within (0, 128]",
            ));
        }
        if !(self.line_height.is_finite() && (1.0..=2.0).contains(&self.line_height)) {
            return Err(ConfigError::validation(
                "font.line_height",
                "must be finite within [1.0, 2.0]",
            ));
        }
        if !(self.letter_spacing.is_finite() && (0.0..=8.0).contains(&self.letter_spacing)) {
            return Err(ConfigError::validation(
                "font.letter_spacing",
                "must be finite within [0.0, 8.0]",
            ));
        }
        Ok(())
    }

    /// Family-level fallback attempt order, configured family first.
    ///
    /// Starts with `self.family` (trimmed), then the documented
    /// [`FONT_FALLBACK_CHAIN`] entries not already covered (case-insensitive
    /// dedup), preserving order. Bounded: at most `1 + CHAIN.len()` entries,
    /// each `<= MAX_FONT_FAMILY_LEN`.
    #[must_use]
    pub fn fallback_chain(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(1 + FONT_FALLBACK_CHAIN.len());
        let primary = self.family.trim().to_string();
        out.push(primary.clone());
        let lower = primary.to_lowercase();
        for cand in FONT_FALLBACK_CHAIN {
            if cand.to_lowercase() != lower
                && !out.iter().any(|e| e.to_lowercase() == cand.to_lowercase())
            {
                out.push(cand.to_string());
            }
        }
        out
    }

    /// Effective cell `(width, height)` after breathing room.
    ///
    /// `width = base_width + round(letter_spacing)`,
    /// `height = round(base_height * line_height)`, each saturated to
    /// `>= 1`. Defaults give `(10, 22)` from the legacy `(8, 16)` base
    /// (CTX-0237 measured raster truth at 12pt).
    #[must_use]
    pub fn effective_cell(&self, base_width: u32, base_height: u32) -> (u32, u32) {
        let extra_w = self.letter_spacing.round().clamp(0.0, 8.0) as u32;
        let width = base_width.saturating_add(extra_w).max(1);
        let scaled_h = (f64::from(base_height) * f64::from(self.line_height)).round();
        let height = u32::try_from(scaled_h as i64).unwrap_or(u32::MAX).max(1);
        (width, height)
    }

    /// Effective cell from the legacy [`BASE_CELL_WIDTH`]/[`BASE_CELL_HEIGHT`].
    #[must_use]
    pub fn default_effective_cell(&self) -> (u32, u32) {
        self.effective_cell(BASE_CELL_WIDTH, BASE_CELL_HEIGHT)
    }
}

/// Window presentation configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowConfig {
    /// Opacity `0.0..=1.0`, finite.
    pub opacity: f32,
    /// Padding in logical pixels `0..=64`.
    pub padding: u32,
    /// Corner radius in physical px `0..=24` (CTX-0241 S0: parsed no-op,
    /// default 0 = square, zero render effect; later stages add rounding).
    pub radius_px: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            padding: DEFAULT_WINDOW_PADDING,
            radius_px: DEFAULT_WINDOW_RADIUS_PX,
        }
    }
}

impl WindowConfig {
    /// Validate window config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(self.opacity.is_finite() && (0.0..=1.0).contains(&self.opacity)) {
            return Err(ConfigError::validation(
                "window.opacity",
                "must be finite within [0.0, 1.0]",
            ));
        }
        if self.padding > MAX_WINDOW_PADDING {
            return Err(ConfigError::validation(
                "window.padding",
                format!("must be <= {MAX_WINDOW_PADDING}"),
            ));
        }
        if self.radius_px > MAX_WINDOW_RADIUS_PX {
            return Err(ConfigError::validation(
                "window.radius_px",
                format!("must be <= {MAX_WINDOW_RADIUS_PX}"),
            ));
        }
        Ok(())
    }
}

/// Terminal behavior configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    /// Scrollback lines, `0..=100000` (bounded memory).
    pub scrollback: u32,
    /// Preferred shell argv[0], if overridden; trimmed non-empty when present.
    pub shell: Option<String>,
    /// Lines scrolled per wheel notch, `1..=32` (CTX-0185; default 3,
    /// ghostty/alacritty-class throughput).
    pub scroll_lines_per_notch: u32,
    /// Smooth-scroll pixels per wheel notch, `1..=256` (CTX-0185; default 16,
    /// one default cell height per notch before the lines multiplier).
    pub scroll_pixels_per_notch: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            shell: None,
            scroll_lines_per_notch: DEFAULT_SCROLL_LINES_PER_NOTCH,
            scroll_pixels_per_notch: DEFAULT_SCROLL_PIXELS_PER_NOTCH,
        }
    }
}

impl TerminalConfig {
    /// Validate terminal config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.scrollback > 100_000 {
            return Err(ConfigError::validation(
                "terminal.scrollback",
                "must be <= 100000",
            ));
        }
        if !(1..=MAX_SCROLL_LINES_PER_NOTCH).contains(&self.scroll_lines_per_notch) {
            return Err(ConfigError::validation(
                "terminal.scroll_lines_per_notch",
                format!("must be within [1, {MAX_SCROLL_LINES_PER_NOTCH}]"),
            ));
        }
        if !(1..=MAX_SCROLL_PIXELS_PER_NOTCH).contains(&self.scroll_pixels_per_notch) {
            return Err(ConfigError::validation(
                "terminal.scroll_pixels_per_notch",
                format!("must be within [1, {MAX_SCROLL_PIXELS_PER_NOTCH}]"),
            ));
        }
        if let Some(s) = &self.shell {
            let t = s.trim();
            if t.is_empty() {
                return Err(ConfigError::validation(
                    "terminal.shell",
                    "when present must be non-empty after trimming",
                ));
            }
            if t.len() > MAX_SHELL_LEN {
                return Err(ConfigError::validation(
                    "terminal.shell",
                    format!("must be <= {MAX_SHELL_LEN} bytes"),
                ));
            }
            // CTX-0298: process authority must not carry control characters
            // (`init_clean_shell` already rejects them before writing the
            // wizard config; spawn re-checks defensively and fails closed).
            if t.chars().any(char::is_control) {
                return Err(ConfigError::validation(
                    "terminal.shell",
                    "must not contain control characters",
                ));
            }
        }
        Ok(())
    }
}

/// Selection behavior configuration (CTX-0191).
///
/// `auto_copy` controls ghostty-class copy-on-select. `true` (default)
/// preserves current behavior: a committed mouse selection auto-copies to
/// the clipboard (which best-effort syncs the primary selection on Linux).
/// `false` leaves the highlight in place and copies only via the explicit
/// `copy_to_clipboard` chord (Ctrl+Shift+C).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionConfig {
    /// Whether a committed mouse selection auto-copies to the clipboard.
    pub auto_copy: bool,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            auto_copy: DEFAULT_SELECTION_AUTO_COPY,
        }
    }
}

impl SelectionConfig {
    /// Validate selection config (booleans are total; always succeeds).
    pub fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

/// Default close-confirmation mode (CTX-0370): `when_busy`.
pub const DEFAULT_CLOSE_CONFIRM: CloseConfirm = CloseConfirm::WhenBusy;

/// Close-confirmation mode (CTX-0370 top-level `close_confirm` key).
///
/// Kitty/ghostty-class close safety for views and windows, applied to the
/// view/window close gestures:
/// - `always`: confirm every close, even when every pane is an idle shell.
/// - `when_busy` (default): confirm only while some pane's PTY has a running
///   foreground job beyond the idle shell (kernel foreground process group
///   differs from the spawned shell pid; undetectable states count as not
///   busy).
/// - `never`: never confirm.
///
/// The workspace kill-confirm gate (CTX-0257) is a separate accepted control
/// and is not governed by this key. Project layers must not declare it: a
/// repository-local file must not disable a data-loss guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseConfirm {
    /// Confirm every view/window close.
    Always,
    /// Confirm only when a foreground job is running. Default.
    #[default]
    WhenBusy,
    /// Never confirm.
    Never,
}

impl CloseConfirm {
    /// Parses a config string (exact lowercase; fail-closed).
    ///
    /// Returns `None` for anything but `"always"`, `"when_busy"`, `"never"`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "always" => Some(Self::Always),
            "when_busy" => Some(Self::WhenBusy),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// Canonical config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::WhenBusy => "when_busy",
            Self::Never => "never",
        }
    }
}

impl std::fmt::Display for CloseConfirm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Panel-gap layout configuration (CTX-0177).
///
/// Hyprland-like spacing between terminal panes, in **cells** (not pixels:
/// the layout algebra is integer cell-space and glyphs must sit on the cell
/// grid, so a pixel gap would quantize to cells anyway; hit-testing converts
/// via the live cell metrics):
///
/// - `gaps_in`: background-colored band between sibling panes at every
///   split (nested splits each insert their own band, matching Hyprland).
/// - `gaps_out`: background-colored inset between the container edge and all
///   panes.
///
/// Set via `init.lua` `layout = { gaps_in = 1, gaps_out = 2 }` (both keys
/// optional, defaulting to `0` when the table is present but omits them, so
/// `layout = {}` keeps edge-to-edge tiling). Both default to `0`, preserving
/// edge-to-edge tiling for existing users.
///
/// One coherent model with [`DecorationConfig`]: the px decoration is scaled
/// by the Window DPI factor and the cell gaps convert through the live cell
/// metrics, so `effective gap = decoration.gap * DPI_scale +
/// layout.gap_cells * cell_axis`. Because `layout` cell gaps default to `0`,
/// the default effective sibling and container gaps are both the `6` logical
/// px decoration default (CTX-0333).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutConfig {
    /// Spacing between sibling panes, in cells, `0..=MAX_LAYOUT_GAP_CELLS`.
    pub gaps_in: u32,
    /// Inset around the container edge, in cells, `0..=MAX_LAYOUT_GAP_CELLS`.
    pub gaps_out: u32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            gaps_in: DEFAULT_LAYOUT_GAPS_IN,
            gaps_out: DEFAULT_LAYOUT_GAPS_OUT,
        }
    }
}

impl LayoutConfig {
    /// Validate layout config (fail-closed on oversized gaps).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.gaps_in > MAX_LAYOUT_GAP_CELLS {
            return Err(ConfigError::validation(
                "layout.gaps_in",
                format!("must be within [0, {MAX_LAYOUT_GAP_CELLS}]"),
            ));
        }
        if self.gaps_out > MAX_LAYOUT_GAP_CELLS {
            return Err(ConfigError::validation(
                "layout.gaps_out",
                format!("must be within [0, {MAX_LAYOUT_GAP_CELLS}]"),
            ));
        }
        Ok(())
    }
}

/// Core-owned workspace decoration in logical pixels (CTX-0292).
///
/// Implements the workspace-compositor contract
/// (`bitty-docs/docs/specifications/workspace-compositor.md`, section
/// "Core-owned gaps, border, and radius", accepted via CTX-0118; unified and
/// extended by CTX-0333):
///
/// | Property        | Default | Range      |
/// | --------------- | ------- | ---------- |
/// | `gaps_in`       | 6 px    | 0..=32 px  |
/// | `gaps_out`      | 6 px    | 0..=32 px  |
/// | `border`        | 2 px    | 0..=8 px   |
/// | `radius`        | 6 px    | 0..=16 px  |
/// | `content_inset` | 6 px    | 0..=32 px  |
///
/// CTX-0340 extends this surface with the accepted focused/idle outline
/// pair (RFC-0001 `OQ-039`): `border_color` is the base, and
/// `border_color_focused` / `border_color_idle` override it explicitly when
/// the user sets them. Resolution order (later wins) is theme token then
/// base then explicit pair; see [`Self::resolve_outline`]. Values are
/// canonical `#RRGGBB` / `#RRGGBBAA` ([`OutlineColor`]).
///
/// CTX-0344 adds the accepted focus/idle **outline width** triple
/// (RFC-0001 `OQ-045`): `border_width` is the base (inheriting
/// `decoration.border` when unset), and `border_width_focused` /
/// `border_width_idle` override it explicitly; see
/// [`Self::resolve_outline_width`]. Bounds are `0..=16` logical px,
/// fail-closed, live reload, and the ring is painted inside the frame so the
/// content grid never moves.
///
/// CTX-0333 raised the sibling gap default from the earlier `4` so
/// `gaps_in == gaps_out` out of the box (panel-to-panel matches
/// panel-to-terminal/container spacing) and added `content_inset`, the inner
/// padding between the frame border and the painted content.
///
/// Values are integers in logical pixels, scaled by the `Window` DPI factor
/// only at render time. Unknown keys or out-of-range values fail validation
/// with a source-attributed diagnostic; Core never falls back to a silent
/// default when validation fails. Decoration is Core-owned: it is never part
/// of a `LayoutTree`, a `View`, or a `LayoutProvider` proposal.
///
/// This is distinct from the CTX-0177 `layout.gaps_in`/`gaps_out` panel gaps,
/// which remain integer **cells** and keep their existing behavior. The one
/// coherent model is: effective gap = `decoration.gap * DPI_scale +
/// layout.gap_cells * cell_axis`, so with the default `layout` cell gaps of
/// `0` the effective sibling and container gaps are both the `6` logical px
/// decoration default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecorationConfig {
    /// Gap between adjacent views inside one workspace, logical px.
    pub gaps_in: u32,
    /// Gap between the workspace tiling area and the window edge, logical px.
    pub gaps_out: u32,
    /// Border thickness drawn inside each View frame, logical px.
    pub border: u32,
    /// Corner radius for View frames, logical px.
    pub radius: u32,
    /// Inner padding between the frame border and the painted content,
    /// logical px (CTX-0333; `0` reproduces the legacy border-only content).
    pub content_inset: u32,
    /// Base outline color for both focus states (CTX-0340
    /// `decoration.border_color`). `None` means "unset: use the theme token"
    /// (and the ratified default pair lives on the theme).
    pub border_color: Option<OutlineColor>,
    /// Explicit focused outline override (CTX-0340
    /// `decoration.border_color_focused`). `None` inherits the resolved
    /// base and never silently shadows it.
    pub border_color_focused: Option<OutlineColor>,
    /// Explicit idle outline override (CTX-0340
    /// `decoration.border_color_idle`). `None` inherits the resolved base.
    pub border_color_idle: Option<OutlineColor>,
    /// Base outline width for both focus states (CTX-0344
    /// `decoration.border_width`), logical px. `None` inherits
    /// [`Self::border`] (the accepted OQ-045 order: `border` then
    /// `border_width` then the explicit pair).
    pub border_width: Option<u32>,
    /// Explicit focused outline-width override (CTX-0344
    /// `decoration.border_width_focused`), logical px. `None` inherits the
    /// resolved base and never silently shadows it.
    pub border_width_focused: Option<u32>,
    /// Explicit idle outline-width override (CTX-0344
    /// `decoration.border_width_idle`), logical px. `None` inherits the
    /// resolved base.
    pub border_width_idle: Option<u32>,
}

impl Default for DecorationConfig {
    fn default() -> Self {
        Self {
            gaps_in: DEFAULT_DECORATION_GAPS_IN_PX,
            gaps_out: DEFAULT_DECORATION_GAPS_OUT_PX,
            border: DEFAULT_DECORATION_BORDER_PX,
            radius: DEFAULT_DECORATION_RADIUS_PX,
            content_inset: DEFAULT_DECORATION_CONTENT_INSET_PX,
            border_color: None,
            border_color_focused: None,
            border_color_idle: None,
            border_width: None,
            border_width_focused: None,
            border_width_idle: None,
        }
    }
}

impl DecorationConfig {
    /// Safe-mode decoration (`bitty --safe`, spec rule 5): `0/0/1/0/0`
    /// regardless of user configuration. Content inset stays zero so safe
    /// mode reproduces the legacy border-only geometry. CTX-0340: the
    /// outline pair is forced to the opaque built-in pair and the explicit
    /// color knobs are cleared, so user colors can never leak into safe
    /// mode.
    #[must_use]
    pub const fn safe() -> Self {
        Self {
            gaps_in: SAFE_DECORATION_GAPS_IN_PX,
            gaps_out: SAFE_DECORATION_GAPS_OUT_PX,
            border: SAFE_DECORATION_BORDER_PX,
            radius: SAFE_DECORATION_RADIUS_PX,
            content_inset: SAFE_DECORATION_CONTENT_INSET_PX,
            border_color: None,
            border_color_focused: Some(SAFE_DECORATION_BORDER_FOCUSED),
            border_color_idle: Some(SAFE_DECORATION_BORDER_IDLE),
            border_width: Some(SAFE_DECORATION_BORDER_WIDTH_PX),
            border_width_focused: Some(SAFE_DECORATION_BORDER_WIDTH_PX),
            border_width_idle: Some(SAFE_DECORATION_BORDER_WIDTH_PX),
        }
    }

    /// True when every decoration is zero (undecorated fast path).
    ///
    /// The CTX-0344 outline-width knobs participate: an explicit non-zero
    /// width paints a ring even when the geometry `border` is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.gaps_in == 0
            && self.gaps_out == 0
            && self.border == 0
            && self.radius == 0
            && self.content_inset == 0
            && matches!(self.border_width, None | Some(0))
            && matches!(self.border_width_focused, None | Some(0))
            && matches!(self.border_width_idle, None | Some(0))
    }

    /// Resolves the focused/idle outline pair from the theme tokens and the
    /// explicit `decoration.border_color*` values (CTX-0340 accepted order).
    ///
    /// A color is available from, in increasing precedence:
    /// 1. the theme tokens ([`crate::theme::Theme::border_focused`] /
    ///    [`crate::theme::Theme::border_idle`]);
    /// 2. `decoration.border_color` (base, both states);
    /// 3. the explicit `decoration.border_color_focused` /
    ///    `decoration.border_color_idle` pair.
    ///
    /// Only an explicit member overrides the resolved base; an unset member
    /// inherits it and never shadows it. Safe mode ([`Self::safe`], which
    /// stores an opaque built-in pair) short-circuits to those values.
    #[must_use]
    pub fn resolve_outline(&self, theme: &crate::theme::Theme) -> ResolvedOutlineColors {
        let focused = self
            .border_color_focused
            .or(self.border_color)
            .unwrap_or(theme.border_focused);
        let idle = self
            .border_color_idle
            .or(self.border_color)
            .unwrap_or(theme.border_idle);
        ResolvedOutlineColors { focused, idle }
    }

    /// Resolves the focused/idle outline-width pair in logical px from the
    /// accepted CTX-0344 (RFC-0001 `OQ-045`) order.
    ///
    /// A width is available from, in increasing precedence:
    /// 1. `decoration.border` (the base paint thickness, default `2`);
    /// 2. `decoration.border_width` (base, both states);
    /// 3. the explicit `decoration.border_width_focused` /
    ///    `decoration.border_width_idle` pair.
    ///
    /// Only an explicit member overrides the resolved base; an unset member
    /// inherits it and never shadows it. Safe mode ([`Self::safe`], which
    /// stores the equal `1`/`1` pair) short-circuits to that pair, so safe
    /// mode never relies on a width cue.
    #[must_use]
    pub fn resolve_outline_width(&self) -> ResolvedOutlineWidths {
        let base = self.border_width.unwrap_or(self.border);
        let focused = self.border_width_focused.unwrap_or(base);
        let idle = self.border_width_idle.unwrap_or(base);
        ResolvedOutlineWidths { focused, idle }
    }

    /// Whether the resolved width pair supplies the AC-2 non-color focus cue
    /// (`border_width_focused >= border_width_idle + 1`, CTX-0344/RFC-0001).
    ///
    /// Integer logical px, so the delta survives DPI scaling. Used by the
    /// contrast contract to allow a focused/idle color pair below the `3:1`
    /// threshold when the thickness delta distinguishes focus instead.
    #[must_use]
    pub fn has_non_color_focus_cue(&self) -> bool {
        let width = self.resolve_outline_width();
        width.focused >= width.idle.saturating_add(1)
    }

    /// Validates the geometry ranges only (fail-closed on out-of-range
    /// values).
    ///
    /// The CTX-0340 outline contrast contract is deliberately **not** checked
    /// here: a single layer may legitimately set only the base color while a
    /// higher-precedence layer sets the focused member, so the pair can only
    /// be judged after merge. [`EffectiveConfig::validate`] runs
    /// [`Self::validate_outline_contract`] on the resolved effective pair.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.gaps_in > MAX_DECORATION_GAP_PX {
            return Err(ConfigError::validation(
                "decoration.gaps_in",
                format!("must be within [0, {MAX_DECORATION_GAP_PX}]"),
            ));
        }
        if self.gaps_out > MAX_DECORATION_GAP_PX {
            return Err(ConfigError::validation(
                "decoration.gaps_out",
                format!("must be within [0, {MAX_DECORATION_GAP_PX}]"),
            ));
        }
        if self.border > MAX_DECORATION_BORDER_PX {
            return Err(ConfigError::validation(
                "decoration.border",
                format!("must be within [0, {MAX_DECORATION_BORDER_PX}]"),
            ));
        }
        if self.radius > MAX_DECORATION_RADIUS_PX {
            return Err(ConfigError::validation(
                "decoration.radius",
                format!("must be within [0, {MAX_DECORATION_RADIUS_PX}]"),
            ));
        }
        if self.content_inset > MAX_DECORATION_CONTENT_INSET_PX {
            return Err(ConfigError::validation(
                "decoration.content_inset",
                format!("must be within [0, {MAX_DECORATION_CONTENT_INSET_PX}]"),
            ));
        }
        // CTX-0344 (RFC-0001/OQ-045): the outline-width triple is bounded
        // `0..=16` logical px fail-closed; never clamped.
        for (field, value) in [
            ("decoration.border_width", self.border_width),
            ("decoration.border_width_focused", self.border_width_focused),
            ("decoration.border_width_idle", self.border_width_idle),
        ] {
            if value.is_some_and(|v| v > MAX_DECORATION_BORDER_WIDTH_PX) {
                return Err(ConfigError::validation(
                    field,
                    format!("must be within [0, {MAX_DECORATION_BORDER_WIDTH_PX}]"),
                ));
            }
        }
        Ok(())
    }

    /// Enforces the CTX-0340 minimum-contrast contract on the resolved pair
    /// over the theme background.
    ///
    /// - AC-1: focused outline >= 3:1 versus the background; fail-closed.
    /// - AC-2: focused >= 3:1 versus idle, **or** the CTX-0344 non-color cue
    ///   (`border_width_focused >= border_width_idle + 1`) is present, in
    ///   which case the thickness delta supplies the focus distinction.
    ///   Reviewer clarification (a) of the RFC supports a base-only config
    ///   where both states share one color; that case claims no color-only
    ///   focus distinction, so AC-2 applies only when the two resolved
    ///   colors differ.
    /// - AC-3: idle >= 1.5:1 versus the background; advisory only, never a
    ///   failure (see [`Self::idle_contrast_warning`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the focused key when a pair
    /// violates AC-1 or AC-2.
    pub fn validate_outline_contract(
        &self,
        theme: &crate::theme::Theme,
    ) -> Result<(), ConfigError> {
        let resolved = self.resolve_outline(theme);
        let bg = theme.background;
        let ac1 = resolved.focused.contrast_over(bg);
        if ac1 < MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST {
            return Err(ConfigError::validation(
                "decoration.border_color_focused",
                format!(
                    "focused outline {} has contrast {ac1:.2}:1 against the background; \
                     AC-1 requires >= {MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST:.1}:1",
                    resolved.focused
                ),
            ));
        }
        if resolved.focused != resolved.idle && !self.has_non_color_focus_cue() {
            let ac2 = resolved.focused.contrast_with(resolved.idle, bg);
            if ac2 < MIN_OUTLINE_FOCUSED_IDLE_CONTRAST {
                return Err(ConfigError::validation(
                    "decoration.border_color_focused",
                    format!(
                        "focused outline {} has contrast {ac2:.2}:1 against idle {}; \
                         AC-2 requires >= {MIN_OUTLINE_FOCUSED_IDLE_CONTRAST:.1}:1 or a \
                         focused outline width >= idle + 1 logical px",
                        resolved.focused, resolved.idle
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Advisory idle-outline contrast (CTX-0340 AC-3), if it falls below the
    /// 1.5:1 floor. Never a validation failure; surfaced by `config check`.
    #[must_use]
    pub fn idle_contrast_warning(&self, theme: &crate::theme::Theme) -> Option<String> {
        let resolved = self.resolve_outline(theme);
        let bg = theme.background;
        let ac3 = resolved.idle.contrast_over(bg);
        if ac3 < MIN_OUTLINE_IDLE_BACKGROUND_CONTRAST {
            Some(format!(
                "idle outline {} has contrast {ac3:.2}:1 against the background \
                 (advisory AC-3 floor {MIN_OUTLINE_IDLE_BACKGROUND_CONTRAST:.1}:1)",
                resolved.idle
            ))
        } else {
            None
        }
    }
}

/// Scrollbar display mode (CTX-0181).
///
/// Mirrors `bitty-ui`'s mode by value (`bitty-config` owns no workspace
/// dependencies, so the pairing is by string, pinned by a `bitty-app`
/// cross-crate test): `auto` (default since CTX-0362, revealed on mouse
/// proximity/hover/drag), `hidden` (opt-out, geometry-neutral), `always`
/// (overlay thumb whenever scrollback exists).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    /// Never painted (opt-out; zero pixels, zero geometry delta).
    Hidden,
    /// Painted whenever scrollback exists.
    Always,
    /// Painted only while engaged (hover/proximity/drag). Default.
    #[default]
    Auto,
}

impl ScrollbarMode {
    /// Parses a config `mode` string (exact lowercase; fail-closed).
    ///
    /// Returns `None` for anything but `"hidden"`, `"always"`, `"auto"`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "hidden" => Some(Self::Hidden),
            "always" => Some(Self::Always),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Canonical config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Always => "always",
            Self::Auto => "auto",
        }
    }
}

/// Overlay scrollbar configuration (CTX-0181).
///
/// The scrollbar is a presentation-only overlay for the scrollback viewport:
/// the thumb is painted in the present layer (like the selection highlight),
/// never grid truth, and the track lives inside the leaf allocation so grid
/// geometry is untouched. Set via `init.lua`
/// `scrollbar = { mode = "auto", width = 8 }` (both keys optional,
/// defaulting to auto/`8` when the table is present but omits them, so
/// `scrollbar = {}` keeps the geometry-neutral auto default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarConfig {
    /// Display mode; default [`ScrollbarMode::Auto`].
    pub mode: ScrollbarMode,
    /// Thumb width in logical pixels, `1..=MAX_SCROLLBAR_WIDTH_PX`.
    pub width: u32,
}

impl Default for ScrollbarConfig {
    fn default() -> Self {
        Self {
            mode: ScrollbarMode::Auto,
            width: DEFAULT_SCROLLBAR_WIDTH,
        }
    }
}

impl ScrollbarConfig {
    /// Validate scrollbar config (fail-closed on bad mode/width).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(MIN_SCROLLBAR_WIDTH_PX..=MAX_SCROLLBAR_WIDTH_PX).contains(&self.width) {
            return Err(ConfigError::validation(
                "scrollbar.width",
                format!("must be within [{MIN_SCROLLBAR_WIDTH_PX}, {MAX_SCROLLBAR_WIDTH_PX}]"),
            ));
        }
        Ok(())
    }
}

/// Mouse behavior configuration (CTX-0260).
///
/// `focus_follows_mouse` controls Hyprland-like hover activation: `false`
/// (default) preserves click-to-focus (hover never moves keyboard focus);
/// `true` moves keyboard focus to the hovered pane. Set via `init.lua`
/// `mouse = { focus_follows_mouse = true }` (key optional, defaulting to
/// `false` when the table is present but omits it, so `mouse = {}` keeps
/// click-to-focus).
///
/// `focus_follows_mouse_delay_ms` (CTX-0334) optionally makes activation
/// wait for the pointer to dwell in the hovered pane: `0` (default)
/// activates on pointer entry, a positive value delays focus by that many
/// milliseconds so a transient pass-through never steals focus. Values are
/// bounded by [`MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS`] and validated
/// fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseConfig {
    /// Whether hover moves keyboard focus to the hovered pane.
    pub focus_follows_mouse: bool,
    /// Dwell time in milliseconds before hover activation moves focus.
    pub focus_follows_mouse_delay_ms: u32,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            focus_follows_mouse: DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE,
            focus_follows_mouse_delay_ms: DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
        }
    }
}

impl MouseConfig {
    /// Validate mouse config.
    ///
    /// # Errors
    ///
    /// [`ConfigError::validation`] when
    /// `focus_follows_mouse_delay_ms` exceeds
    /// [`MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS`] (fail-closed).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.focus_follows_mouse_delay_ms > MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS {
            return Err(ConfigError::validation(
                "mouse.focus_follows_mouse_delay_ms",
                format!(
                    "must be within [0, {MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS}] milliseconds"
                ),
            ));
        }
        Ok(())
    }
}

/// Appearance configuration.
///
/// The optional theme identifier resolves through the built-in preset
/// registry ([`crate::theme`]): `None`/empty means the designed default
/// preset ([`crate::theme::DEFAULT_THEME_NAME`]), a known name resolves to
/// its exact values, and an unknown name falls back to the default (logged).
/// No config-file I/O happens here; the identifier is already-parsed data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppearanceConfig {
    /// Optional theme identifier.
    pub theme: Option<String>,
    /// Panel animation overrides (RFC-0002, CTX-0341). `None` means "this
    /// layer says nothing"; a present value deep-merges per leaf, and the
    /// effective concrete [`AnimationsConfig`] lives on [`EffectiveConfig`].
    pub animations: Option<AnimationsOverride>,
}

impl AppearanceConfig {
    /// Validate appearance config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(t) = &self.theme {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                return Err(ConfigError::validation(
                    "appearance.theme",
                    "when present must be non-empty after trimming",
                ));
            }
            if trimmed.len() > MAX_THEME_LEN {
                return Err(ConfigError::validation(
                    "appearance.theme",
                    format!("must be <= {MAX_THEME_LEN} bytes"),
                ));
            }
        }
        if let Some(a) = &self.animations {
            a.validate()?;
        }
        Ok(())
    }
}

/// A single key mapping — data describing a chord, action, and context.
///
/// RFC merge rule: key mappings merge by `context + chord` identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeymapEntry {
    /// Chord string, e.g. `"ctrl+shift+p"`.
    pub chord: String,
    /// Action/command identifier.
    pub action: String,
    /// Context in which the mapping applies (e.g. `"global"`).
    pub context: String,
}

impl KeymapEntry {
    /// Validate this entry: shape bounds plus semantic chord/action/context
    /// checks ([`crate::keymap`]); unknown actions or keys fail closed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.chord.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].chord",
                "must not be empty",
            ));
        }
        if self.action.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].action",
                "must not be empty",
            ));
        }
        if self.context.trim().is_empty() {
            return Err(ConfigError::validation(
                "keymaps[].context",
                "must not be empty",
            ));
        }
        if self.chord.len() > 128 || self.action.len() > 256 || self.context.len() > 128 {
            return Err(ConfigError::validation(
                "keymaps[]",
                "chord/action/context exceed length bounds",
            ));
        }
        crate::keymap::validate_entry(self)
    }

    /// Identity key for set-by-identifier merging.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}::{}", self.context, self.chord)
    }
}

/// A plugin declaration.
///
/// RFC merge rule: plugin set merges by globally unique plugin `id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginSpec {
    /// Globally unique plugin identifier, e.g. `"xuepoo/bitty-markdown"`.
    pub id: String,
    /// Whether the plugin is enabled.
    pub enabled: bool,
}

impl PluginSpec {
    /// Validate this spec.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let id = self.id.trim();
        if id.is_empty() {
            return Err(ConfigError::validation("plugins[].id", "must not be empty"));
        }
        if id.len() > MAX_PLUGIN_ID_LEN {
            return Err(ConfigError::validation(
                "plugins[].id",
                format!("must be <= {MAX_PLUGIN_ID_LEN} bytes"),
            ));
        }
        Ok(())
    }
}

/// Fully resolved effective configuration after merge — every field has a
/// concrete value, never `Option`, derived from core defaults plus layers.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    /// Font config.
    pub font: FontConfig,
    /// Window config.
    pub window: WindowConfig,
    /// Terminal config.
    pub terminal: TerminalConfig,
    /// Selection config (CTX-0191; default auto-copies on select).
    pub selection: SelectionConfig,
    /// Close-confirmation mode (CTX-0370 top-level `close_confirm`; default
    /// `when_busy`).
    pub close_confirm: CloseConfirm,
    /// Layout config (CTX-0177 panel gaps in cells; default edge-to-edge).
    pub layout: LayoutConfig,
    /// Core-owned workspace decoration in logical px (CTX-0292; accepted
    /// spec CTX-0118 defaults 4/6/2/6).
    pub decoration: DecorationConfig,
    /// Scrollbar config (CTX-0181 overlay scrollbar; default auto, CTX-0362).
    pub scrollbar: ScrollbarConfig,
    /// Mouse config (CTX-0260 focus-follows-mouse; default off).
    pub mouse: MouseConfig,
    /// Appearance config (theme defaults to `None` if unset).
    pub appearance: AppearanceConfig,
    /// Resolved panel animation contract (RFC-0002, CTX-0341). Always a
    /// concrete value: the accepted defaults plus every declared layer's
    /// per-field overrides. Lives beside `appearance` because it is the
    /// effective form of `appearance.animations`.
    pub animations: AnimationsConfig,
    /// Leader/Mod key the shipped chrome map is expressed against (CTX-0236;
    /// default Alt). Honored by [`crate::keymap::resolve_keymaps`].
    pub mod_key: ModKey,
    /// Keymaps, possibly empty.
    pub keymaps: Vec<KeymapEntry>,
    /// Plugins, possibly empty.
    pub plugins: Vec<PluginSpec>,
    /// Profile name that produced this config, if any.
    pub profile: Option<String>,
    /// Schema version of the source plan that produced this config.
    pub schema_version: u32,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            window: WindowConfig::default(),
            terminal: TerminalConfig::default(),
            selection: SelectionConfig::default(),
            close_confirm: DEFAULT_CLOSE_CONFIRM,
            layout: LayoutConfig::default(),
            decoration: DecorationConfig::default(),
            scrollbar: ScrollbarConfig::default(),
            mouse: MouseConfig::default(),
            appearance: AppearanceConfig::default(),
            animations: AnimationsConfig::default(),
            mod_key: ModKey::default(),
            keymaps: Vec::new(),
            plugins: Vec::new(),
            profile: None,
            schema_version: crate::migration::CURRENT_SCHEMA_VERSION,
        }
    }
}

impl EffectiveConfig {
    /// Returns the built-in safe configuration: every field at its core
    /// default except the Core-owned decoration forced to the safe-mode values
    /// (`0/0/1/0/0` geometry and the opaque `#FFFFFF`/`#808080` outline pair)
    /// and every panel-animation duration forced to `0` ms (RFC-0002: `--safe`
    /// forces instant final-state application) regardless of external
    /// configuration (`bitty --safe`, spec rule 5, R-009/P0-AC-019). The
    /// result is always valid; construction itself performs no I/O.
    #[must_use]
    pub fn with_safe_decoration(mut self) -> Self {
        self.decoration = DecorationConfig::safe();
        self.animations.duration_ms = AnimationDurations {
            open: 0,
            close: 0,
            focus: 0,
            workspace: 0,
        };
        self
    }

    /// Validate all fields of the effective config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.font.validate()?;
        self.window.validate()?;
        self.terminal.validate()?;
        self.selection.validate()?;
        self.layout.validate()?;
        self.decoration.validate()?;
        // CTX-0340: the outline pair is resolvable only after merge, so the
        // AC-1/AC-2 contrast contract is enforced on the effective resolved
        // pair against the selected theme's background (AC-3 stays advisory).
        let theme = crate::theme::resolve_theme(self.appearance.theme.as_deref());
        self.decoration.validate_outline_contract(theme)?;
        self.scrollbar.validate()?;
        self.mouse.validate()?;
        self.appearance.validate()?;
        self.animations.validate()?;
        if self.keymaps.len() > MAX_KEYMAPS {
            return Err(ConfigError::validation(
                "keymaps",
                format!("must contain <= {MAX_KEYMAPS} entries"),
            ));
        }
        if self.plugins.len() > MAX_PLUGINS {
            return Err(ConfigError::validation(
                "plugins",
                format!("must contain <= {MAX_PLUGINS} entries"),
            ));
        }
        for km in &self.keymaps {
            km.validate()?;
        }
        for p in &self.plugins {
            p.validate()?;
        }
        // Keymap and plugin IDs must be unique per merge contract.
        let mut seen_km = std::collections::HashSet::new();
        for km in &self.keymaps {
            let id = km.id();
            if !seen_km.insert(id.clone()) {
                return Err(ConfigError::validation(
                    "keymaps",
                    format!("duplicate keymap id '{id}'"),
                ));
            }
        }
        let mut seen_pl = std::collections::HashSet::new();
        for p in &self.plugins {
            let id = p.id.trim().to_string();
            if !seen_pl.insert(id.clone()) {
                return Err(ConfigError::validation(
                    "plugins",
                    format!("duplicate plugin id '{id}'"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_validation() {
        FontConfig {
            family: String::new(),
            size: 12.0,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "JetBrains Mono".into(),
            size: f32::NAN,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 0.0,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 12.0,
            line_height: 0.9,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig {
            family: "Mono".into(),
            size: 12.0,
            letter_spacing: 9.0,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        FontConfig::default().validate().expect("default valid");
    }

    #[test]
    fn font_defaults_match_ghostty_linux_acceptance() {
        let d = FontConfig::default();
        assert_eq!(d.family, DEFAULT_FONT_FAMILY);
        assert_eq!(d.family, "JetBrainsMono Nerd Font");
        assert!((d.size - 12.0).abs() < f32::EPSILON);
        assert!((d.line_height - 1.375).abs() < f32::EPSILON);
        assert!((d.letter_spacing - 2.0).abs() < f32::EPSILON);
        // Effective cell from legacy 8x16 base covers the measured 12pt
        // raster truth (CTX-0237: advance 10, line 22).
        assert_eq!(d.default_effective_cell(), (10, 22));
        assert_eq!(d.effective_cell(8, 16), (10, 22));
    }

    #[test]
    fn font_fallback_chain_is_documented_order() {
        let d = FontConfig::default();
        let chain = d.fallback_chain();
        // Configured primary first, then the pinned platform tails in order.
        assert_eq!(chain[0], DEFAULT_FONT_FAMILY);
        let expected_tails: Vec<String> = FONT_FALLBACK_CHAIN
            .iter()
            .skip(1)
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(chain[1..], expected_tails[..]);
        // Custom primary stays first, chain dedups case-insensitively.
        let custom = FontConfig {
            family: "monospace".into(),
            ..Default::default()
        };
        let chain = custom.fallback_chain();
        assert_eq!(chain[0], "monospace");
        assert_eq!(chain.iter().filter(|f| *f == "monospace").count(), 1);
        // No duplicates when the primary is already a tail (case-insensitive).
        let nerd = FontConfig {
            family: "  jetbrainsmono nerd font  ".into(),
            ..Default::default()
        };
        let chain = nerd.fallback_chain();
        assert_eq!(chain.len(), FONT_FALLBACK_CHAIN.len());
    }

    #[test]
    fn font_fallback_chain_covers_tui_graph_slices() {
        // CTX-0163 (issue #263) + CTX-0368: btop CPU graphs draw braille
        // patterns (`U+2800-U+28FF`); block graphs use `U+2580-U+259F`;
        // symbols/emoji (`U+2714`, `U+2611`, `U+2699`) need a symbols and an
        // emoji tail. The chain is a fixed, deterministic, bounded list that
        // a per-glyph fallback walk (`bitty-render::fallback`) can traverse
        // on bare installs without the Nerd font.
        let tails = FONT_FALLBACK_CHAIN.len() - 1;
        assert!(
            tails <= MAX_FALLBACK_FAMILIES,
            "tails {tails} exceed MAX_FALLBACK_FAMILIES"
        );
        assert!(FONT_FALLBACK_CHAIN.len() <= MAX_FALLBACK_DEPTH);
        assert_eq!(SYMBOLS_FALLBACK_FAMILY, "Noto Sans Symbols 2");
        // No duplicate families in the pinned order.
        for (i, a) in FONT_FALLBACK_CHAIN.iter().enumerate() {
            for b in &FONT_FALLBACK_CHAIN[i + 1..] {
                assert_ne!(a, b, "duplicate fallback family {a}");
            }
        }
        // The symbols/emoji tails survive a custom primary (dedup only
        // removes the primary itself, never a tail).
        let custom = FontConfig {
            family: "My Mono".into(),
            ..Default::default()
        };
        let chain = custom.fallback_chain();
        assert_eq!(chain.len(), FONT_FALLBACK_CHAIN.len() + 1);
        assert!(chain.contains(&EMOJI_FALLBACK_FAMILY.to_string()));
        // The platform symbols/braille tail survives a custom primary.
        #[cfg(target_os = "linux")]
        assert!(chain.contains(&SYMBOLS_FALLBACK_FAMILY.to_string()));
        #[cfg(target_os = "macos")]
        assert!(chain.contains(&"Apple Braille".to_string()));
        #[cfg(windows)]
        assert!(chain.contains(&"Segoe UI Symbol".to_string()));
        #[cfg(target_os = "linux")]
        {
            assert!(FONT_FALLBACK_CHAIN.contains(&"DejaVu Sans Mono"));
            assert_eq!(EMOJI_FALLBACK_FAMILY, "Noto Color Emoji");
        }
        #[cfg(target_os = "macos")]
        {
            assert!(FONT_FALLBACK_CHAIN.contains(&"Apple Braille"));
            assert_eq!(EMOJI_FALLBACK_FAMILY, "Apple Color Emoji");
        }
        #[cfg(windows)]
        {
            assert!(FONT_FALLBACK_CHAIN.contains(&"Segoe UI Symbol"));
            assert_eq!(EMOJI_FALLBACK_FAMILY, "Segoe UI Emoji");
        }
    }

    #[test]
    fn font_effective_cell_math() {
        let base = FontConfig {
            line_height: 1.0,
            letter_spacing: 0.0,
            ..Default::default()
        };
        assert_eq!(base.effective_cell(8, 16), (8, 16));
        let roomy = FontConfig::default();
        assert_eq!(roomy.effective_cell(8, 16), (10, 22));
        // Zero base still saturates to >= 1 (width keeps the +2px
        // letter-spacing floor, height saturates from zero).
        assert_eq!(roomy.effective_cell(0, 0), (2, 1));
    }

    #[test]
    fn window_validation() {
        WindowConfig {
            opacity: 2.0,
            padding: 8,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        WindowConfig {
            opacity: 0.5,
            padding: 100,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        WindowConfig::default().validate().expect("default valid");
    }

    #[test]
    fn window_padding_validation() {
        // CTX-0301: padding literals are named; pin the default, the bound,
        // and the fail-closed edge (`> MAX_WINDOW_PADDING`).
        assert_eq!(WindowConfig::default().padding, DEFAULT_WINDOW_PADDING);
        assert_eq!(DEFAULT_WINDOW_PADDING, 8);
        assert_eq!(MAX_WINDOW_PADDING, 64);
        WindowConfig {
            padding: 0,
            ..Default::default()
        }
        .validate()
        .expect("zero valid");
        WindowConfig {
            padding: MAX_WINDOW_PADDING,
            ..Default::default()
        }
        .validate()
        .expect("max valid");
        WindowConfig {
            padding: MAX_WINDOW_PADDING + 1,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        WindowConfig {
            padding: u32::MAX,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
    }

    #[test]
    fn window_radius_validation() {
        // CTX-0241 S0: physical px `0..=24`, default 0 = square no-op.
        assert_eq!(WindowConfig::default().radius_px, DEFAULT_WINDOW_RADIUS_PX);
        assert_eq!(DEFAULT_WINDOW_RADIUS_PX, 0);
        assert_eq!(MAX_WINDOW_RADIUS_PX, 24);
        WindowConfig {
            radius_px: 0,
            ..Default::default()
        }
        .validate()
        .expect("zero valid");
        WindowConfig {
            radius_px: MAX_WINDOW_RADIUS_PX,
            ..Default::default()
        }
        .validate()
        .expect("max valid");
        WindowConfig {
            radius_px: MAX_WINDOW_RADIUS_PX + 1,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        WindowConfig {
            radius_px: u32::MAX,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
    }

    #[test]
    fn terminal_validation() {
        TerminalConfig {
            scrollback: 200_000,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        TerminalConfig {
            scrollback: 10,
            shell: Some("   ".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        // CTX-0298: process authority rejects control characters and
        // overlong paths fail closed. Trimmed-away edge whitespace is
        // normalized by the spawn resolver, so only embedded controls are
        // rejected here.
        for bad in ["/bin/zsh\u{7}", "/bin/z\nsh", "/bin/z\tsh"] {
            TerminalConfig {
                shell: Some(bad.into()),
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        TerminalConfig {
            shell: Some(format!("/{}", "x".repeat(MAX_SHELL_LEN))),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        TerminalConfig {
            shell: Some("/bin/fish".into()),
            ..Default::default()
        }
        .validate()
        .expect("clean absolute shell is valid");
        TerminalConfig::default().validate().expect("default valid");
    }

    #[test]
    fn terminal_scroll_speed_validation() {
        // CTX-0185: lines/pixels per notch are validated fail-closed.
        for bad_lines in [0, MAX_SCROLL_LINES_PER_NOTCH + 1] {
            TerminalConfig {
                scroll_lines_per_notch: bad_lines,
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        for bad_pixels in [0, MAX_SCROLL_PIXELS_PER_NOTCH + 1] {
            TerminalConfig {
                scroll_pixels_per_notch: bad_pixels,
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        for (lines, pixels) in [(1, 1), (3, 16), (32, 256)] {
            TerminalConfig {
                scroll_lines_per_notch: lines,
                scroll_pixels_per_notch: pixels,
                ..Default::default()
            }
            .validate()
            .expect("boundary scroll speed must be valid");
        }
    }

    #[test]
    fn selection_auto_copy_defaults_on_and_validates() {
        // CTX-0191: default-on preserves copy-on-select for existing users.
        const { assert!(DEFAULT_SELECTION_AUTO_COPY) }
        assert!(SelectionConfig::default().auto_copy);
        assert!(EffectiveConfig::default().selection.auto_copy);
        SelectionConfig::default()
            .validate()
            .expect("default valid");
        SelectionConfig { auto_copy: false }
            .validate()
            .expect("opt-out valid");
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn close_confirm_defaults_when_busy_and_parses_exact_values() {
        // CTX-0370: `close_confirm` accepts only the three exact lowercase
        // spellings; everything else fails closed (`parse` -> None). Default
        // is `when_busy` in both the enum and the effective config.
        assert_eq!(DEFAULT_CLOSE_CONFIRM, CloseConfirm::WhenBusy);
        assert_eq!(CloseConfirm::default(), CloseConfirm::WhenBusy);
        assert_eq!(
            EffectiveConfig::default().close_confirm,
            CloseConfirm::WhenBusy
        );
        for (raw, expected) in [
            ("always", CloseConfirm::Always),
            ("when_busy", CloseConfirm::WhenBusy),
            ("never", CloseConfirm::Never),
        ] {
            assert_eq!(CloseConfirm::parse(raw), Some(expected), "{raw}");
            assert_eq!(expected.as_str(), raw);
            assert_eq!(expected.to_string(), raw);
        }
        for raw in [
            "",
            " ",
            "ALWAYS",
            "Always",
            "when busy",
            "when-busy",
            "busy",
            "ask",
            "auto",
            "off",
            "0",
            "true",
        ] {
            assert_eq!(CloseConfirm::parse(raw), None, "{raw:?} must fail closed");
        }
    }

    #[test]
    fn scrollbar_defaults_auto_and_validates_bounds() {
        // CTX-0362: the default is the geometry-neutral `auto` overlay
        // (transparent at rest, revealed on engagement); width bounds fail
        // closed.
        const { assert!(DEFAULT_SCROLLBAR_WIDTH == 8) }
        const { assert!(MIN_SCROLLBAR_WIDTH_PX == 1) }
        const { assert!(MAX_SCROLLBAR_WIDTH_PX == 32) }
        assert_eq!(DEFAULT_SCROLLBAR_MODE, "auto");
        let d = ScrollbarConfig::default();
        assert_eq!(d.mode, ScrollbarMode::Auto);
        assert_eq!(d.width, DEFAULT_SCROLLBAR_WIDTH);
        d.validate().expect("default valid");
        assert_eq!(ScrollbarMode::default(), ScrollbarMode::Auto);
        assert_eq!(ScrollbarMode::parse("hidden"), Some(ScrollbarMode::Hidden));
        assert_eq!(ScrollbarMode::parse("always"), Some(ScrollbarMode::Always));
        assert_eq!(ScrollbarMode::parse("auto"), Some(ScrollbarMode::Auto));
        assert_eq!(ScrollbarMode::parse("overlay"), None);
        assert_eq!(ScrollbarMode::parse("AUTO"), None);
        for bad in [0, MAX_SCROLLBAR_WIDTH_PX + 1] {
            ScrollbarConfig {
                width: bad,
                ..Default::default()
            }
            .validate()
            .unwrap_err();
        }
        for good in [MIN_SCROLLBAR_WIDTH_PX, 8, MAX_SCROLLBAR_WIDTH_PX] {
            ScrollbarConfig {
                mode: ScrollbarMode::Auto,
                width: good,
            }
            .validate()
            .expect("boundary width must be valid");
        }
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn mouse_focus_follows_mouse_defaults_off_and_validates() {
        // CTX-0260: default-off preserves click-to-focus for existing users.
        const { assert!(!DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE) }
        const { assert!(DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS == 0) }
        assert!(!MouseConfig::default().focus_follows_mouse);
        assert_eq!(
            MouseConfig::default().focus_follows_mouse_delay_ms,
            DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS
        );
        assert!(!EffectiveConfig::default().mouse.focus_follows_mouse);
        MouseConfig::default().validate().expect("default valid");
        MouseConfig {
            focus_follows_mouse: true,
            ..MouseConfig::default()
        }
        .validate()
        .expect("opt-in valid");
        // CTX-0334: the dwell delay is bounded fail-closed.
        for good in [
            0,
            1,
            DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
            MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
        ] {
            MouseConfig {
                focus_follows_mouse: true,
                focus_follows_mouse_delay_ms: good,
            }
            .validate()
            .expect("boundary delay must be valid");
        }
        MouseConfig {
            focus_follows_mouse: true,
            focus_follows_mouse_delay_ms: MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS + 1,
        }
        .validate()
        .expect_err("over-max delay must fail closed");
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn effective_validate_calls_every_section_validator() {
        // CTX-0303: commit 117381b (CTX-0292) silently dropped
        // `self.mouse.validate()?;` from EffectiveConfig::validate. The call
        // became behavior-bearing again in CTX-0334 (MouseConfig::validate now
        // rejects an over-max hover delay), so pin it structurally by scanning
        // production source only. The `#[cfg(test)]` region is excluded so this
        // test cannot satisfy itself.
        let src = include_str!("types.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        for call in [
            "self.font.validate()?;",
            "self.window.validate()?;",
            "self.terminal.validate()?;",
            "self.selection.validate()?;",
            "self.layout.validate()?;",
            "self.decoration.validate()?;",
            "self.scrollbar.validate()?;",
            "self.mouse.validate()?;",
            "self.appearance.validate()?;",
        ] {
            assert!(
                prod.contains(call),
                "EffectiveConfig::validate must call {call}"
            );
        }
    }

    #[test]
    fn layout_gaps_default_zero_and_validate_bounds() {
        // CTX-0177: defaults preserve edge-to-edge tiling; bounds fail closed.
        const { assert!(DEFAULT_LAYOUT_GAPS_IN == 0) }
        const { assert!(DEFAULT_LAYOUT_GAPS_OUT == 0) }
        let d = LayoutConfig::default();
        assert_eq!(d.gaps_in, 0);
        assert_eq!(d.gaps_out, 0);
        d.validate().expect("default valid");
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
        for (gin, gout) in [(0, 0), (1, 2), (MAX_LAYOUT_GAP_CELLS, MAX_LAYOUT_GAP_CELLS)] {
            LayoutConfig {
                gaps_in: gin,
                gaps_out: gout,
            }
            .validate()
            .expect("boundary gaps must be valid");
        }
        for (gin, gout) in [
            (MAX_LAYOUT_GAP_CELLS + 1, 0),
            (0, MAX_LAYOUT_GAP_CELLS + 1),
            (u32::MAX, u32::MAX),
        ] {
            LayoutConfig {
                gaps_in: gin,
                gaps_out: gout,
            }
            .validate()
            .expect_err("oversized gaps must fail closed");
        }
        // Effective-level validation covers layout too.
        let mut eff = EffectiveConfig::default();
        eff.layout.gaps_in = MAX_LAYOUT_GAP_CELLS + 1;
        eff.validate()
            .expect_err("effective must reject oversized gaps");
    }

    #[test]
    fn decoration_defaults_match_unified_spec_and_validate() {
        // CTX-0333: unified 6/6 sibling/container gaps, 2px border, 6px
        // radius, 6px content inset; ranges 0..=32 / 0..=32 / 0..=8 /
        // 0..=16 / 0..=32, fail closed.
        const { assert!(DEFAULT_DECORATION_GAPS_IN_PX == 6) }
        const { assert!(DEFAULT_DECORATION_GAPS_OUT_PX == 6) }
        const { assert!(DEFAULT_DECORATION_BORDER_PX == 2) }
        const { assert!(DEFAULT_DECORATION_RADIUS_PX == 6) }
        const { assert!(DEFAULT_DECORATION_CONTENT_INSET_PX == 6) }
        const { assert!(MAX_DECORATION_GAP_PX == 32) }
        const { assert!(MAX_DECORATION_BORDER_PX == 8) }
        const { assert!(MAX_DECORATION_RADIUS_PX == 16) }
        const { assert!(MAX_DECORATION_CONTENT_INSET_PX == 32) }
        let d = DecorationConfig::default();
        assert_eq!(
            (d.gaps_in, d.gaps_out, d.border, d.radius, d.content_inset),
            (6, 6, 2, 6, 6)
        );
        d.validate().expect("default valid");
        assert!(!d.is_zero());
        // CTX-0333: the sibling and container defaults match out of the box.
        assert_eq!(d.gaps_in, d.gaps_out);
        // Safe-mode inversion: 0/0/1/0/0 regardless of the defaults.
        let safe = DecorationConfig::safe();
        assert_eq!(
            (
                safe.gaps_in,
                safe.gaps_out,
                safe.border,
                safe.radius,
                safe.content_inset
            ),
            (0, 0, 1, 0, 0)
        );
        safe.validate().expect("safe valid");
        assert!(!safe.is_zero());
        for good in [
            DecorationConfig {
                gaps_in: 0,
                gaps_out: 0,
                border: 0,
                radius: 0,
                content_inset: 0,
                ..Default::default()
            },
            DecorationConfig {
                gaps_in: 32,
                gaps_out: 32,
                border: 8,
                radius: 16,
                content_inset: 32,
                ..Default::default()
            },
        ] {
            good.validate().expect("boundary decoration must be valid");
        }
        for (field, bad) in [
            ("decoration.gaps_in", 33),
            ("decoration.gaps_out", 33),
            ("decoration.border", 9),
            ("decoration.radius", 17),
            ("decoration.content_inset", 33),
        ] {
            let mut c = DecorationConfig::default();
            match field {
                "decoration.gaps_in" => c.gaps_in = bad,
                "decoration.gaps_out" => c.gaps_out = bad,
                "decoration.border" => c.border = bad,
                "decoration.radius" => c.radius = bad,
                _ => c.content_inset = bad,
            }
            let err = c.validate().expect_err("out-of-range must fail closed");
            assert_eq!(err.field(), Some(field), "wrong field for {field}");
        }
        // Effective-level validation covers decoration too.
        let mut eff = EffectiveConfig::default();
        eff.decoration.radius = MAX_DECORATION_RADIUS_PX + 1;
        eff.validate()
            .expect_err("effective must reject oversized decoration");
        let mut eff = EffectiveConfig::default();
        eff.decoration.content_inset = MAX_DECORATION_CONTENT_INSET_PX + 1;
        eff.validate()
            .expect_err("effective must reject oversized content inset");
    }

    #[test]
    fn keymap_id_uniqueness_enforced() {
        let cfg = EffectiveConfig {
            keymaps: vec![
                KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_next".into(),
                    context: "global".into(),
                },
                KeymapEntry {
                    chord: "ctrl+p".into(),
                    action: "focus_prev".into(),
                    context: "global".into(),
                },
            ],
            ..Default::default()
        };
        cfg.validate().unwrap_err();
    }

    #[test]
    fn plugin_id_uniqueness_enforced() {
        let cfg = EffectiveConfig {
            plugins: vec![
                PluginSpec {
                    id: "a/b".into(),
                    enabled: true,
                },
                PluginSpec {
                    id: "a/b".into(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        cfg.validate().unwrap_err();
    }

    #[test]
    fn effective_default_valid() {
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn outline_color_parses_canonical_grammar_only() {
        // CTX-0340: exactly `#RRGGBB` / `#RRGGBBAA`; alpha defaults to FF.
        assert_eq!(
            OutlineColor::parse("#33CCFF"),
            Some(OutlineColor([0x33, 0xCC, 0xFF, 0xFF]))
        );
        assert_eq!(
            OutlineColor::parse("#595959AA"),
            Some(OutlineColor([0x59, 0x59, 0x59, 0xAA]))
        );
        assert_eq!(
            OutlineColor::parse("  #000000FF  "),
            Some(OutlineColor([0, 0, 0, 0xFF]))
        );
        // Fail-closed: #RGB shorthand, missing '#', wrong length, non-hex,
        // named colors, function syntax, and overlong input are all rejected.
        for bad in [
            "#FFF",
            "33CCFF",
            "#33CCF",
            "#33CCFFF",
            "#GGGGGG",
            "red",
            "rgb(1,2,3)",
            "rgba(1,2,3,0.5)",
            "",
            "   ",
            "#1234567890",
        ] {
            assert!(
                OutlineColor::parse(bad).is_none(),
                "{bad:?} must be rejected"
            );
        }
        // Alpha `00` is a valid 8-digit spelling (fully transparent).
        assert_eq!(
            OutlineColor::parse("#33CCFF00"),
            Some(OutlineColor([0x33, 0xCC, 0xFF, 0x00]))
        );
    }

    #[test]
    fn outline_color_hex_round_trips() {
        for raw in ["#33CCFF", "#595959AA", "#000000", "#12345678"] {
            let parsed = OutlineColor::parse(raw).expect("canonical spelling parses");
            assert_eq!(parsed.to_hex(), raw, "round trip for {raw}");
        }
        // Opaque 8-digit input canonicalizes to the 6-digit form.
        assert_eq!(
            OutlineColor::parse("#FFFFFFFF").unwrap().to_hex(),
            "#FFFFFF"
        );
        assert!(OutlineColor::parse("#33CCFF").unwrap().is_opaque());
        assert!(!OutlineColor::parse("#595959AA").unwrap().is_opaque());
    }

    #[test]
    fn outline_contrast_matches_wcag_reference() {
        // CTX-0340: the ratified pair clears AC-1/AC-2 and the advisory AC-3
        // against the Bitty Dark workspace background.
        let bg = crate::theme::BITTY_DARK.background;
        let focused = DEFAULT_DECORATION_BORDER_FOCUSED;
        let idle = DEFAULT_DECORATION_BORDER_IDLE;
        assert!(focused.contrast_over(bg) >= MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST);
        assert!(focused.contrast_with(idle, bg) >= MIN_OUTLINE_FOCUSED_IDLE_CONTRAST);
        assert!(idle.contrast_over(bg) >= MIN_OUTLINE_IDLE_BACKGROUND_CONTRAST);
        // Translucent idle composites onto the background before comparison.
        assert_eq!(idle.composited_over(bg), [0x45, 0x45, 0x4B]);
        // A hard black focused color is below AC-1 and must fail closed.
        let dark = OutlineColor([0x00, 0x00, 0x00, 0xFF]);
        assert!(dark.contrast_over(bg) < MIN_OUTLINE_FOCUSED_BACKGROUND_CONTRAST);
    }

    #[test]
    fn outline_resolution_order_theme_base_then_pair() {
        let theme = crate::theme::default_theme();
        // Unset everything -> theme token pair.
        let d = DecorationConfig::default();
        let r = d.resolve_outline(theme);
        assert_eq!(r.focused, DEFAULT_DECORATION_BORDER_FOCUSED);
        assert_eq!(r.idle, DEFAULT_DECORATION_BORDER_IDLE);
        // Base set -> both states use the base.
        let d = DecorationConfig {
            border_color: Some(OutlineColor([0x11, 0x22, 0x33, 0xFF])),
            ..Default::default()
        };
        let r = d.resolve_outline(theme);
        assert_eq!(r.focused, OutlineColor([0x11, 0x22, 0x33, 0xFF]));
        assert_eq!(r.idle, OutlineColor([0x11, 0x22, 0x33, 0xFF]));
        // Explicit pair members override the base; an unset member inherits
        // it and never silently shadows it.
        let base = OutlineColor([0x11, 0x22, 0x33, 0xFF]);
        let focused = OutlineColor([0xAA, 0xBB, 0xCC, 0xFF]);
        let d = DecorationConfig {
            border_color: Some(base),
            border_color_focused: Some(focused),
            border_color_idle: None,
            ..Default::default()
        };
        let r = d.resolve_outline(theme);
        assert_eq!(r.focused, focused);
        assert_eq!(r.idle, base);
        // With no base, an unset member falls through to the theme token.
        let d = DecorationConfig {
            border_color_focused: Some(focused),
            ..Default::default()
        };
        let r = d.resolve_outline(theme);
        assert_eq!(r.focused, focused);
        assert_eq!(r.idle, DEFAULT_DECORATION_BORDER_IDLE);
    }

    #[test]
    fn outline_contrast_contract_is_fail_closed_for_ac1_and_ac2() {
        let theme = crate::theme::default_theme();
        // AC-1: focused black over the dark background fails closed naming
        // the focused key.
        let bad_ac1 = DecorationConfig {
            border_color_focused: Some(OutlineColor([0x00, 0x00, 0x00, 0xFF])),
            ..Default::default()
        };
        let err = bad_ac1
            .validate_outline_contract(theme)
            .expect_err("AC-1 violation must fail");
        assert_eq!(err.field(), Some("decoration.border_color_focused"));
        assert!(err.to_string().contains("AC-1"), "{err}");
        // AC-2: two near-identical bright colors both pass AC-1 but fail the
        // focused-vs-idle 3:1 floor.
        let bad_ac2 = DecorationConfig {
            border_color_focused: Some(OutlineColor([0xFF, 0xFF, 0xFF, 0xFF])),
            border_color_idle: Some(OutlineColor([0xDD, 0xDD, 0xDD, 0xFF])),
            ..Default::default()
        };
        let err = bad_ac2
            .validate_outline_contract(theme)
            .expect_err("AC-2 violation must fail");
        assert!(err.to_string().contains("AC-2"), "{err}");
        // A base-only config (both states share one color) is supported and
        // does not claim a color-only focus distinction, so AC-2 is skipped.
        let base_only = DecorationConfig {
            border_color: Some(OutlineColor([0xFF, 0xFF, 0xFF, 0xFF])),
            ..Default::default()
        };
        base_only
            .validate_outline_contract(theme)
            .expect("base-only config valid");
        // The default (theme-token) pair passes.
        DecorationConfig::default()
            .validate_outline_contract(theme)
            .expect("default valid");
        // Safe pair passes AC-1/AC-2.
        DecorationConfig::safe()
            .validate_outline_contract(theme)
            .expect("safe valid");
        // A single layer may hold only the base; per-layer geometry
        // validation must not run the merged-pair contract.
        bad_ac1
            .validate()
            .expect("layer-level geometry validation is pair-agnostic");
        let eff = EffectiveConfig {
            decoration: bad_ac1,
            ..Default::default()
        };
        eff.validate().expect_err("effective must enforce AC-1");
    }

    #[test]
    fn outline_idle_contrast_is_advisory_only() {
        // AC-3: a low-contrast idle is reported, never a validation failure.
        let theme = crate::theme::default_theme();
        let d = DecorationConfig {
            border_color_idle: Some(OutlineColor([0x22, 0x22, 0x30, 0xFF])),
            ..Default::default()
        };
        assert!(d.idle_contrast_warning(theme).is_some());
        d.validate().expect("AC-3 is advisory, not a failure");
        d.validate_outline_contract(theme)
            .expect("AC-3 is advisory, not a failure");
        // The ratified idle clears the advisory floor: no warning.
        assert!(
            DecorationConfig::default()
                .idle_contrast_warning(theme)
                .is_none()
        );
    }

    #[test]
    fn safe_decoration_forces_opaque_builtin_pair_regardless_of_user() {
        // CTX-0340: `--safe` ignores user/preset colors and forces the
        // opaque built-in pair; explicit user knobs are cleared.
        let safe = DecorationConfig::safe();
        assert_eq!(safe.border_color, None);
        assert_eq!(
            safe.border_color_focused,
            Some(SAFE_DECORATION_BORDER_FOCUSED)
        );
        assert_eq!(safe.border_color_idle, Some(SAFE_DECORATION_BORDER_IDLE));
        assert!(SAFE_DECORATION_BORDER_FOCUSED.is_opaque());
        assert!(SAFE_DECORATION_BORDER_IDLE.is_opaque());
        let theme = crate::theme::default_theme();
        let r = safe.resolve_outline(theme);
        assert_eq!(r.focused, SAFE_DECORATION_BORDER_FOCUSED);
        assert_eq!(r.idle, SAFE_DECORATION_BORDER_IDLE);
        // No user color can survive safe mode: the resolver never reads a
        // theme token when an explicit pair is present.
        assert_ne!(r.focused, DEFAULT_DECORATION_BORDER_FOCUSED);
    }

    #[test]
    fn outline_width_defaults_bounds_and_inheritance() {
        // CTX-0344 (RFC-0001/OQ-045): `0..=16` logical px; the base inherits
        // `decoration.border`; the pair inherits the resolved base; safe mode
        // forces the equal `1`/`1` pair.
        const { assert!(MAX_DECORATION_BORDER_WIDTH_PX == 16) }
        const { assert!(SAFE_DECORATION_BORDER_WIDTH_PX == 1) }
        // Default: everything unset -> both states inherit border 2.
        let d = DecorationConfig::default();
        let w = d.resolve_outline_width();
        assert_eq!((w.focused, w.idle), (2, 2));
        assert!(!d.has_non_color_focus_cue());
        // Base set -> both states use the base.
        let d = DecorationConfig {
            border_width: Some(5),
            ..Default::default()
        };
        let w = d.resolve_outline_width();
        assert_eq!((w.focused, w.idle), (5, 5));
        // Explicit members override the base; an unset member inherits it.
        let d = DecorationConfig {
            border_width: Some(4),
            border_width_focused: Some(7),
            border_width_idle: None,
            ..Default::default()
        };
        let w = d.resolve_outline_width();
        assert_eq!((w.focused, w.idle), (7, 4));
        assert!(d.has_non_color_focus_cue());
        // No base: an unset member falls through to `decoration.border`.
        let d = DecorationConfig {
            border: 3,
            border_width_idle: Some(1),
            border_width_focused: Some(2),
            ..Default::default()
        };
        let w = d.resolve_outline_width();
        assert_eq!((w.focused, w.idle), (2, 1));
        assert!(d.has_non_color_focus_cue());
        // Focused == idle supplies no non-color cue.
        let d = DecorationConfig {
            border_width: Some(6),
            ..Default::default()
        };
        assert!(!d.has_non_color_focus_cue());
        // Safe mode: equal 1/1 regardless of user values.
        let safe = DecorationConfig::safe();
        let w = safe.resolve_outline_width();
        assert_eq!((w.focused, w.idle), (1, 1));
        assert!(!safe.has_non_color_focus_cue());
        assert_eq!(safe.border_width, Some(SAFE_DECORATION_BORDER_WIDTH_PX));
        // Boundaries are accepted.
        for raw in [0u32, MAX_DECORATION_BORDER_WIDTH_PX] {
            DecorationConfig {
                border_width: Some(raw),
                border_width_focused: Some(raw),
                border_width_idle: Some(raw),
                ..Default::default()
            }
            .validate()
            .expect("boundary width valid");
        }
        // Out-of-range fails closed naming the offending key, never clamps.
        for (field, value) in [
            (
                "decoration.border_width",
                DecorationConfig {
                    border_width: Some(MAX_DECORATION_BORDER_WIDTH_PX + 1),
                    ..Default::default()
                },
            ),
            (
                "decoration.border_width_focused",
                DecorationConfig {
                    border_width_focused: Some(MAX_DECORATION_BORDER_WIDTH_PX + 1),
                    ..Default::default()
                },
            ),
            (
                "decoration.border_width_idle",
                DecorationConfig {
                    border_width_idle: Some(MAX_DECORATION_BORDER_WIDTH_PX + 1),
                    ..Default::default()
                },
            ),
        ] {
            let err = value
                .validate()
                .expect_err("out-of-range width must fail closed");
            assert_eq!(err.field(), Some(field), "wrong field for {field}");
        }
        // Effective-level validation covers the widths too.
        let mut eff = EffectiveConfig::default();
        eff.decoration.border_width_focused = Some(MAX_DECORATION_BORDER_WIDTH_PX + 1);
        eff.validate()
            .expect_err("effective must reject oversized width");
        // is_zero accounts for a non-zero explicit width.
        assert!(
            !DecorationConfig {
                border: 0,
                gaps_in: 0,
                gaps_out: 0,
                radius: 0,
                content_inset: 0,
                border_width_focused: Some(3),
                ..Default::default()
            }
            .is_zero()
        );
    }

    #[test]
    fn outline_width_non_color_cue_satisfies_ac2() {
        // CTX-0344: a focused/idle color pair below the AC-2 3:1 threshold is
        // accepted when the width cue (`focused >= idle + 1`) is present.
        let theme = crate::theme::default_theme();
        let no_cue = DecorationConfig {
            border_color_focused: Some(OutlineColor([0xFF, 0xFF, 0xFF, 0xFF])),
            border_color_idle: Some(OutlineColor([0xDD, 0xDD, 0xDD, 0xFF])),
            ..Default::default()
        };
        no_cue
            .validate_outline_contract(theme)
            .expect_err("without a cue the pair must clear 3:1");
        let with_cue = DecorationConfig {
            border_width_focused: Some(3),
            border_width_idle: Some(1),
            ..no_cue
        };
        with_cue
            .validate_outline_contract(theme)
            .expect("the width cue satisfies AC-2");
    }

    #[test]
    fn animations_defaults_match_rfc0002_and_validate() {
        // RFC-0002: open 150 / close 120 / focus 100 / workspace 200 ms;
        // enabled = true; reduced_motion = "auto"; ratified easings.
        const { assert!(DEFAULT_ANIMATION_OPEN_MS == 150) }
        const { assert!(DEFAULT_ANIMATION_CLOSE_MS == 120) }
        const { assert!(DEFAULT_ANIMATION_FOCUS_MS == 100) }
        const { assert!(DEFAULT_ANIMATION_WORKSPACE_MS == 200) }
        const { assert!(MAX_ANIMATION_DURATION_MS == 500) }
        let a = AnimationsConfig::default();
        assert!(a.enabled);
        assert_eq!(a.reduced_motion, ReducedMotion::Auto);
        assert_eq!(
            a.duration_ms,
            AnimationDurations {
                open: 150,
                close: 120,
                focus: 100,
                workspace: 200,
            }
        );
        assert_eq!(
            a.easing,
            AnimationEasings {
                open: AnimationEasing::EaseOut,
                close: AnimationEasing::EaseIn,
                focus: AnimationEasing::EaseInOut,
                workspace: AnimationEasing::EaseInOut,
            }
        );
        a.validate().expect("accepted defaults valid");
        // Effective duration follows the contract defaults when nothing
        // reduces motion.
        assert_eq!(
            a.effective_duration_ms(AnimationTransition::Open, false, false),
            150
        );
        assert_eq!(
            a.effective_duration_ms(AnimationTransition::Workspace, false, false),
            200
        );
        // Bound boundaries are accepted: 0 and 500 inclusive.
        for raw in [0u32, 500] {
            let b = AnimationsConfig {
                duration_ms: AnimationDurations {
                    open: raw,
                    close: raw,
                    focus: raw,
                    workspace: raw,
                },
                ..Default::default()
            };
            b.validate().expect("0 and 500 are inside the hard bound");
        }
        // Out-of-range durations fail closed naming the field, never clamp.
        for (field, bad) in [
            ("appearance.animations.duration_ms.open", 501u32),
            ("appearance.animations.duration_ms.close", 999),
            ("appearance.animations.duration_ms.focus", 501),
            ("appearance.animations.duration_ms.workspace", 501),
        ] {
            let mut b = AnimationsConfig::default();
            match field {
                "appearance.animations.duration_ms.open" => b.duration_ms.open = bad,
                "appearance.animations.duration_ms.close" => b.duration_ms.close = bad,
                "appearance.animations.duration_ms.focus" => b.duration_ms.focus = bad,
                _ => b.duration_ms.workspace = bad,
            }
            let err = b.validate().expect_err("out-of-range must fail closed");
            assert_eq!(err.field(), Some(field), "wrong field for {field}");
        }
        // Layer-level override validation shares the same bound and paths.
        for (field, over) in [
            (
                "appearance.animations.duration_ms.open",
                AnimationsOverride {
                    duration_open: Some(501),
                    ..Default::default()
                },
            ),
            (
                "appearance.animations.duration_ms.close",
                AnimationsOverride {
                    duration_close: Some(u32::MAX),
                    ..Default::default()
                },
            ),
            (
                "appearance.animations.duration_ms.focus",
                AnimationsOverride {
                    duration_focus: Some(501),
                    ..Default::default()
                },
            ),
            (
                "appearance.animations.duration_ms.workspace",
                AnimationsOverride {
                    duration_workspace: Some(501),
                    ..Default::default()
                },
            ),
        ] {
            let err = over.validate().expect_err("override must fail closed");
            assert_eq!(err.field(), Some(field), "wrong field for {field}");
        }
        // Effective-level validation covers appearance.animations too.
        let mut eff = EffectiveConfig::default();
        eff.animations.duration_ms.open = MAX_ANIMATION_DURATION_MS + 1;
        eff.validate()
            .expect_err("effective must reject oversized animation duration");
    }

    #[test]
    fn animation_overrides_apply_per_field_and_inherit_the_rest() {
        // RFC-0002: the table deep-merges while each field is scalar-replace;
        // a layer that sets one duration/easing leaves the others intact.
        let mut a = AnimationsConfig::default();
        a.apply_overrides(&AnimationsOverride {
            duration_open: Some(500),
            easing_open: Some(AnimationEasing::Linear),
            ..Default::default()
        });
        assert_eq!(a.duration_ms.open, 500);
        assert_eq!(a.easing.open, AnimationEasing::Linear);
        // Untouched leaves inherit the accepted defaults.
        assert_eq!(a.duration_ms.close, DEFAULT_ANIMATION_CLOSE_MS);
        assert_eq!(a.duration_ms.focus, DEFAULT_ANIMATION_FOCUS_MS);
        assert_eq!(a.duration_ms.workspace, DEFAULT_ANIMATION_WORKSPACE_MS);
        assert_eq!(a.easing.close, AnimationEasing::EaseIn);
        // A second layer overrides a disjoint leaf and keeps the first.
        a.apply_overrides(&AnimationsOverride {
            enabled: Some(false),
            reduced_motion: Some(ReducedMotion::Always),
            ..Default::default()
        });
        assert!(!a.enabled);
        assert_eq!(a.reduced_motion, ReducedMotion::Always);
        assert_eq!(a.duration_ms.open, 500, "earlier override survives");
        assert_eq!(a.easing.open, AnimationEasing::Linear);
        // Empty override is a pure no-op (this layer said nothing).
        let before = a;
        a.apply_overrides(&AnimationsOverride::default());
        assert_eq!(a, before);
    }

    #[test]
    fn animation_easing_enum_round_trips_and_spring_maps_to_ease_in_out() {
        for (raw, value) in [
            ("linear", AnimationEasing::Linear),
            ("ease_in", AnimationEasing::EaseIn),
            ("ease_out", AnimationEasing::EaseOut),
            ("ease_in_out", AnimationEasing::EaseInOut),
            ("spring", AnimationEasing::Spring),
        ] {
            assert_eq!(AnimationEasing::parse(raw), Some(value));
            assert_eq!(value.as_str(), raw);
        }
        // Unknown spellings fail closed (the caller names the field).
        for bad in ["ease", "ease-in", "Spring", "cubic_bezier", ""] {
            assert_eq!(AnimationEasing::parse(bad), None, "{bad} must be unknown");
        }
        // RFC-0002: spring parameters are deferred, so `spring` resolves to
        // the `ease_in_out` curve.
        assert_eq!(
            AnimationEasing::Spring.resolved(),
            AnimationEasing::EaseInOut
        );
        assert_eq!(AnimationEasing::Linear.resolved(), AnimationEasing::Linear);
        let a = AnimationsConfig {
            easing: AnimationEasings {
                open: AnimationEasing::Spring,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            a.effective_easing(AnimationTransition::Open),
            AnimationEasing::EaseInOut
        );
    }

    #[test]
    fn reduced_motion_enum_parses_exactly() {
        for (raw, value) in [
            ("auto", ReducedMotion::Auto),
            ("always", ReducedMotion::Always),
            ("never", ReducedMotion::Never),
        ] {
            assert_eq!(ReducedMotion::parse(raw), Some(value));
            assert_eq!(value.as_str(), raw);
        }
        for bad in ["Auto", "system", "on", ""] {
            assert_eq!(ReducedMotion::parse(bad), None, "{bad} must be unknown");
        }
    }

    #[test]
    fn animation_effective_duration_honors_disabled_reduced_and_safe() {
        let base = AnimationsConfig::default();
        // Disabled is equivalent to 0 ms durations but keeps the contract.
        let disabled = AnimationsConfig {
            enabled: false,
            ..Default::default()
        };
        for t in [
            AnimationTransition::Open,
            AnimationTransition::Close,
            AnimationTransition::Focus,
            AnimationTransition::Workspace,
        ] {
            assert_eq!(disabled.effective_duration_ms(t, false, false), 0);
        }
        // `reduced_motion = "always"` forces 0 even when enabled.
        let always = AnimationsConfig {
            reduced_motion: ReducedMotion::Always,
            ..Default::default()
        };
        assert_eq!(
            always.effective_duration_ms(AnimationTransition::Open, false, false),
            0
        );
        // `reduced_motion = "never"` ignores the platform signal.
        let never = AnimationsConfig {
            reduced_motion: ReducedMotion::Never,
            ..Default::default()
        };
        assert_eq!(
            never.effective_duration_ms(AnimationTransition::Open, true, false),
            150
        );
        // `auto` follows the platform signal.
        assert_eq!(
            base.effective_duration_ms(AnimationTransition::Open, true, false),
            0
        );
        assert_eq!(
            base.effective_duration_ms(AnimationTransition::Focus, false, false),
            100
        );
        // `--safe` forces 0 regardless of config and the platform signal.
        assert_eq!(
            base.effective_duration_ms(AnimationTransition::Open, false, true),
            0
        );
        assert_eq!(
            never.effective_duration_ms(AnimationTransition::Workspace, false, true),
            0
        );
    }
}
