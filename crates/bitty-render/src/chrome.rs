//! Shared Core chrome tokens and the one paint path for floating surfaces
//! (issue #2468, CTX-1477).
//!
//! The help/which-key panel, command-palette/presentation overlays, plugin
//! overlay surfaces, panel rings, and band chrome all resolve their border
//! weight, border colour, radius, padding, and surface elevation here. A
//! widget never picks a raw pixel width or an ad-hoc accent again: it builds
//! a [`ChromeFrame`] from [`ChromeTokens`] and pushes the resulting
//! [`ChromePaint`] into the frame layers.
//!
//! # Visual contract
//!
//! | Token | Default | Unit |
//! |-------|---------|------|
//! | border width | 1 | logical px |
//! | radius | 6 | logical px |
//! | padding | 8 | logical px |
//! | normal border | theme idle outline (`border.idle`) | sRGB |
//! | focused border | theme focused outline (`border.focused`) | sRGB |
//! | surface | theme selection ground | sRGB |
//! | elevated surface | surface blended 8% toward foreground | sRGB |
//!
//! The border pair is the accepted theme outline contract: the focused border
//! clears AC-1 (3:1 against the window ground) and AC-2 (3:1 against the
//! composite idle border), and the idle border stays the designed muted
//! tone. Using the theme pair keeps focus distinguishable on every shipped
//! preset without inventing a second accent. A preset switch or a live OSC 4
//! remap moves the whole chrome language with the terminal.
//!
//! # Grid-safe corners
//!
//! Two paint tiers share one token set:
//!
//! - **Pixel surfaces** (help, palette, presentation/plugin overlays, panels)
//!   paint a [`RoundedFill`] surface plus a 1-px ring and use the SDF inner
//!   arc as the corner mask. Glyphs that cross the arc carry the
//!   [`ChromePaint::clip`] rounded clip, so no cell ink leaks into the
//!   corner notch. This is exact at any DPI and needs no corner glyph.
//! - **Cell-grid chrome** (one-row bands) cannot express a sub-cell radius.
//!   [`ChromeCornerGlyphs`] names the rounded box-drawing corner set and two
//!   documented fallbacks: light box-drawing corners when the font covers
//!   those, then square corners (no glyph) when even those are unavailable.
//!   The fallback never leaves a hole: the band surface stays rectangular and
//!   only the decorative corner glyph is dropped.
//!
//! All values are deterministic, bounded, and English-only. The module is a
//! mechanism, not a style preference: plugins still choose their own content
//! colours inside a Core-owned surface.

use crate::geometry::RectPx;
use crate::grid::{Rgba8, RoundedClip, RoundedFill, ThemePalette, lerp_rgba, rounded_frame_clip};
use crate::hidpi::sanitize_dpi_scale;

/// Thin chrome rule width in logical pixels (issue #2468).
///
/// One logical pixel is the accepted thin-border weight; [`ChromeTokens::scaled`]
/// turns it into one physical pixel at 1x and keeps the same physical weight on
/// HiDPI displays.
pub const CHROME_BORDER_PX: u16 = 1;

/// Default chrome corner radius in logical pixels.
///
/// Matches the accepted Core panel default
/// (`bitty_config::types::DEFAULT_DECORATION_RADIUS_PX`) so overlays and
/// panels read as one visual system.
pub const CHROME_RADIUS_PX: u16 = 6;

/// Default chrome content padding in logical pixels.
///
/// Eight logical pixels is one text cell on the reference font stack and
/// leaves the radius arc outside the text box at every supported DPI.
pub const CHROME_PADDING_PX: u16 = 8;

/// Minimum composited visibility of the muted border against the surface.
///
/// Some shipped presets intentionally alias `border.idle` to the selection
/// ground. Chrome must still show a boundary, so a token at or below this
/// ratio is nudged toward the window ground by
/// [`CHROME_MUTED_FALLBACK_MIX`]. The nudge is bounded and theme-owned.
pub const MIN_CHROME_MUTED_VISIBILITY: f64 = 1.05;

/// Bounded mix toward the window ground for an otherwise invisible muted
/// border.
pub const CHROME_MUTED_FALLBACK_MIX: f32 = 0.35;
/// Reserved surface tone alpha for [`ChromeElevation::Base`].
const TRANSPARENT: Rgba8 = [0x00, 0x00, 0x00, 0x00];

/// Fraction of the foreground mixed into the selection ground for the raised
/// surface. A fixed, bounded step keeps elevation deterministic per theme.
pub const CHROME_ELEVATE_MIX: f32 = 0.08;

/// Advisory visibility floor for the normal chrome border (AC-3, 1.5:1).
///
/// The idle outline is a designed subtle tone; AC-3 is advisory and canonical
/// presets may sit below it, exactly like the accepted panel outline pair.
pub const MIN_CHROME_MUTED_CONTRAST: f64 = 1.5;

/// Non-text UI floor for the focused chrome border (AC-1/AC-2, 3:1).
pub const MIN_CHROME_FOCUS_CONTRAST: f64 = 3.0;

/// Corner order used by every glyph/mask table: top-left, top-right,
/// bottom-left, bottom-right.
pub const CORNER_ORDER: [&str; 4] = ["top-left", "top-right", "bottom-left", "bottom-right"];

/// Rounded box-drawing corners (U+256D..U+2570): the preferred grid corners.
pub const CHROME_CORNERS_ROUNDED: [char; 4] = ['╭', '╮', '╰', '╯'];

/// Light box-drawing corners (U+250C..U+2510): the first fallback when the
/// rounded set is unavailable but the font still covers box drawing.
pub const CHROME_CORNERS_BOX: [char; 4] = ['┌', '┐', '└', '┘'];

/// Quadrant blocks that mask the outer corner of a cell-grid surface when a
/// host supplies a surface fill below the glyphs.
///
/// The mask uses block elements (U+2598..U+2597), the same coverage family the
/// renderer's fallback chain already guarantees for TUI graph cells. A host
/// that cannot draw them must use [`ChromeGlyphCoverage::None`] and square
/// corners instead.
pub const CHROME_CORNER_MASKS: [char; 4] = ['▘', '▝', '▖', '▗'];

/// Advertised glyph coverage for cell-grid corner drawing.
///
/// This is a host capability input, not a font probe: Core's default font
/// chain lists box drawing and block elements as covered families, so the
/// production host advertises [`Self::Full`]. A constrained host that strips
/// those families advertises [`Self::Basic`] or [`Self::None`] and gets the
/// documented square fallback instead of a tofu glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeGlyphCoverage {
    /// Rounded box drawing plus quadrant blocks are available.
    Full,
    /// Only light box drawing is available; corners are square (no arc) but
    /// the frame stays visually closed.
    Basic,
    /// No box-drawing coverage; corners are square and glyph-free.
    None,
}

/// The resolved corner glyphs for one host capability.
///
/// `None` entries mean "paint no corner glyph"; the surface keeps its square
/// rectangle. This is the documented fallback, never a silent tofu glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeCornerGlyphs {
    /// One optional glyph per [`CORNER_ORDER`] entry.
    pub corners: [Option<char>; 4],
}

impl ChromeCornerGlyphs {
    /// Resolves the corner table for a host coverage level.
    #[must_use]
    pub const fn resolve(coverage: ChromeGlyphCoverage) -> Self {
        match coverage {
            ChromeGlyphCoverage::Full => Self {
                corners: [
                    Some(CHROME_CORNERS_ROUNDED[0]),
                    Some(CHROME_CORNERS_ROUNDED[1]),
                    Some(CHROME_CORNERS_ROUNDED[2]),
                    Some(CHROME_CORNERS_ROUNDED[3]),
                ],
            },
            ChromeGlyphCoverage::Basic => Self {
                corners: [
                    Some(CHROME_CORNERS_BOX[0]),
                    Some(CHROME_CORNERS_BOX[1]),
                    Some(CHROME_CORNERS_BOX[2]),
                    Some(CHROME_CORNERS_BOX[3]),
                ],
            },
            ChromeGlyphCoverage::None => Self { corners: [None; 4] },
        }
    }

    /// Glyph for one [`CORNER_ORDER`] index.
    #[must_use]
    pub const fn glyph(&self, corner: usize) -> Option<char> {
        self.corners[corner]
    }

    /// Quadrant mask for one [`CORNER_ORDER`] index, or `None` when the host
    /// cannot draw block elements.
    #[must_use]
    pub const fn mask(&self, corner: usize, coverage: ChromeGlyphCoverage) -> Option<char> {
        if matches!(coverage, ChromeGlyphCoverage::Full) {
            Some(CHROME_CORNER_MASKS[corner])
        } else {
            None
        }
    }
}

/// Whether a chrome surface is idle or focused.
///
/// The focused state selects [`ChromeTokens::border_focus`]; the normal state
/// selects [`ChromeTokens::border_muted`]. Both states share geometry, so a
/// focus change never moves content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeState {
    /// Idle/unfocused surface: the muted border.
    Normal,
    /// Focused/active surface: the accent border.
    Focused,
}

/// Surface elevation level.
///
/// Elevation changes only the surface tone; border, radius, and padding stay
/// identical, so nesting two surfaces cannot change their geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromeElevation {
    /// Base window ground (no lifted card).
    Base,
    /// Raised card: the theme selection ground.
    Raised,
    /// Floating overlay: one step above [`Self::Raised`].
    Overlay,
}

/// Resolved chrome tokens for one theme.
///
/// `border_px`, `radius_px`, and `padding_px` are logical pixels; call
/// [`Self::scaled`] once per present with the live DPI factor before building
/// a [`ChromeFrame`]. Colours are straight-alpha sRGB bytes and already clear
/// their documented contrast floors against [`Self::surface`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeTokens {
    /// Thin border width in logical pixels.
    pub border_px: u16,
    /// Corner radius in logical pixels.
    pub radius_px: u16,
    /// Content padding in logical pixels.
    pub padding_px: u16,
    /// Raised surface tone.
    pub surface: Rgba8,
    /// Floating-overlay surface tone (one elevation step above `surface`).
    pub surface_elevated: Rgba8,
    /// Normal/idle border tone.
    pub border_muted: Rgba8,
    /// Focused/active border tone.
    pub border_focus: Rgba8,
}

impl Default for ChromeTokens {
    fn default() -> Self {
        Self::resolve(&ThemePalette::default())
    }
}

impl ChromeTokens {
    /// Resolves the token set from a live terminal palette.
    ///
    /// The border pair is the theme's accepted outline pair
    /// (`border.idle` / `border.focused`), so focus stays distinguishable on
    /// every shipped preset and a theme switch recolours both states. The
    /// surface is the theme selection ground; the elevated surface is a fixed
    /// theme-owned blend toward the foreground, so elevation never invents a
    /// raw literal.
    #[must_use]
    pub fn resolve(palette: &ThemePalette) -> Self {
        let surface = palette.selection;
        let elevated = lerp_rgba(surface, palette.foreground, CHROME_ELEVATE_MIX);
        Self {
            border_px: CHROME_BORDER_PX,
            radius_px: CHROME_RADIUS_PX,
            padding_px: CHROME_PADDING_PX,
            surface,
            surface_elevated: elevated,
            border_muted: muted_border(palette),
            border_focus: palette.border_focused,
        }
    }

    /// Scales logical geometry to physical pixels with the sanitized live DPI
    /// factor. Colours are unchanged.
    #[must_use]
    pub fn scaled(self, scale: f64) -> Self {
        let scale = sanitize_dpi_scale(scale);
        Self {
            border_px: scale_px(self.border_px, scale),
            radius_px: scale_px(self.radius_px, scale),
            padding_px: scale_px(self.padding_px, scale),
            ..self
        }
    }

    /// Surface tone for an elevation level.
    #[must_use]
    pub const fn surface_for(&self, elevation: ChromeElevation) -> Rgba8 {
        match elevation {
            ChromeElevation::Base => TRANSPARENT,
            ChromeElevation::Raised => self.surface,
            ChromeElevation::Overlay => self.surface_elevated,
        }
    }

    /// Border tone for a state.
    #[must_use]
    pub const fn border_for(&self, state: ChromeState) -> Rgba8 {
        match state {
            ChromeState::Normal => self.border_muted,
            ChromeState::Focused => self.border_focus,
        }
    }
}

/// One resolved chrome frame: geometry plus the state/elevation it paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeFrame {
    /// Outer frame in physical pixels.
    pub frame: RectPx,
    /// Resolved token set (already DPI-scaled by the caller).
    pub tokens: ChromeTokens,
    /// Idle or focused.
    pub state: ChromeState,
    /// Surface elevation.
    pub elevation: ChromeElevation,
}

/// The one paint record produced by [`ChromeFrame::paint`].
///
/// `surface` and `border` are rounded fills (surface first, ring second);
/// `clip` is the SDF inner arc that glyph instances should carry so their ink
/// is masked at the corners. A zero radius yields a square `border` ring and
/// `None` for the clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromePaint {
    /// Rounded surface fill (`border == 0`).
    pub surface: RoundedFill,
    /// Rounded border ring, or `None` when the border width resolves to zero.
    pub border: Option<RoundedFill>,
    /// Inner rounded clip for text/glyph instances.
    pub clip: Option<RoundedClip>,
}

impl ChromePaint {
    /// True when this paint has any draw work.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.surface.frame.width == 0 || self.surface.frame.height == 0
    }
}

impl ChromeFrame {
    /// Builds a frame from already-scaled tokens.
    #[must_use]
    pub const fn new(
        frame: RectPx,
        tokens: ChromeTokens,
        state: ChromeState,
        elevation: ChromeElevation,
    ) -> Self {
        Self {
            frame,
            tokens,
            state,
            elevation,
        }
    }

    /// Surface fill colour (alpha 0 for [`ChromeElevation::Base`]).
    #[must_use]
    pub const fn surface_color(&self) -> Rgba8 {
        self.tokens.surface_for(self.elevation)
    }

    /// Border ring colour for the current state.
    #[must_use]
    pub const fn border_color(&self) -> Rgba8 {
        self.tokens.border_for(self.state)
    }

    /// Content rectangle: the frame inset by border plus padding.
    #[must_use]
    pub fn content_rect(&self) -> RectPx {
        inset(
            self.frame,
            u32::from(self.tokens.border_px) + u32::from(self.tokens.padding_px),
        )
    }

    /// Resolves the surface, border ring, and inner glyph clip.
    ///
    /// The surface uses the same frame and radius as the ring, so the ring
    /// covers the surface edge exactly. Degenerate frames produce zero-area
    /// fills; callers already gate on non-zero geometry.
    #[must_use]
    pub fn paint(&self) -> ChromePaint {
        let surface = RoundedFill {
            frame: self.frame,
            border: 0,
            radius: self.tokens.radius_px,
            color: self.surface_color(),
        };
        let border = (self.tokens.border_px > 0).then(|| RoundedFill {
            frame: self.frame,
            border: self.tokens.border_px,
            radius: self.tokens.radius_px,
            color: self.border_color(),
        });
        let clip = if self.tokens.radius_px == 0 {
            None
        } else {
            rounded_frame_clip(self.frame, self.tokens.border_px, self.tokens.radius_px)
        };
        ChromePaint {
            surface,
            border,
            clip,
        }
    }

    /// Pushes the paint into one rounded layer, surface before ring.
    ///
    /// Returns the paint record so the caller can attach `clip` to the glyph
    /// instances it emits for this surface.
    pub fn push(&self, out: &mut Vec<RoundedFill>) -> ChromePaint {
        let paint = self.paint();
        if paint.surface.frame.width > 0 && paint.surface.frame.height > 0 {
            out.push(paint.surface.clone());
            if let Some(border) = &paint.border {
                out.push(border.clone());
            }
        }
        paint
    }
}

/// Builds a rounded border ring for a caller that already resolved its own
/// width, radius, and colour (per-`View` panel outlines).
///
/// This is the single ring construction used by panels and chrome frames, so
/// the SDF path never diverges. Callers that use the shared tokens should
/// build a [`ChromeFrame`] instead.
#[must_use]
pub fn outline_ring(frame: RectPx, border: u16, radius: u16, color: Rgba8) -> RoundedFill {
    RoundedFill {
        frame,
        border,
        radius,
        color,
    }
}

/// Insets a rectangle by `amount` pixels per side, saturating at zero area.
#[must_use]
pub fn inset(frame: RectPx, amount: u32) -> RectPx {
    RectPx::new(
        frame
            .x
            .saturating_add(i32::try_from(amount).unwrap_or(i32::MAX)),
        frame
            .y
            .saturating_add(i32::try_from(amount).unwrap_or(i32::MAX)),
        frame.width.saturating_sub(amount.saturating_mul(2)),
        frame.height.saturating_sub(amount.saturating_mul(2)),
    )
}

/// Scales one logical pixel length to physical pixels, rounding half away
/// from zero. Zero stays zero (a caller explicitly disabled the field).
fn scale_px(value: u16, scale: f64) -> u16 {
    if value == 0 {
        return 0;
    }
    let scaled = f64::from(value) * scale;
    let rounded = scaled.round();
    if rounded >= f64::from(u16::MAX) {
        u16::MAX
    } else if rounded < 1.0 {
        1
    } else {
        rounded as u16
    }
}

/// Resolves the muted border, nudging an invisible idle token toward the
/// window ground.
///
/// A handful of shipped presets alias `border.idle` to the selection ground;
/// painting that token would draw no boundary at all. The fallback keeps the
/// alpha and hue of the theme token and moves it a bounded step toward the
/// theme's own background, so it remains theme-owned and deterministic.
fn muted_border(palette: &ThemePalette) -> Rgba8 {
    let idle = palette.border_idle;
    let surface = palette.selection;
    let visible =
        contrast_ratio(composite_over(idle, surface), surface) > MIN_CHROME_MUTED_VISIBILITY;
    if visible {
        return idle;
    }
    let background = palette.background;
    let mixed = lerp_rgba(idle, background, CHROME_MUTED_FALLBACK_MIX);
    // `lerp_rgba` interpolates alpha too; keep the theme-owned alpha so a
    // translucent idle token stays translucent.
    [mixed[0], mixed[1], mixed[2], idle[3]]
}

/// Straight-alpha src-over composition of `fg` onto opaque `bg`.
fn composite_over(fg: Rgba8, bg: Rgba8) -> Rgba8 {
    let alpha = u16::from(fg[3]);
    let mix = |src: u8, dst: u8| -> u8 {
        (((u16::from(src) * alpha) + (u16::from(dst) * (255 - alpha)) + 127) / 255) as u8
    };
    [
        mix(fg[0], bg[0]),
        mix(fg[1], bg[1]),
        mix(fg[2], bg[2]),
        0xFF,
    ]
}

/// WCAG contrast ratio between two opaque sRGB colours.
fn contrast_ratio(a: Rgba8, b: Rgba8) -> f64 {
    let la = crate::grid::relative_luminance([a[0], a[1], a[2]]);
    let lb = crate::grid::relative_luminance([b[0], b[1], b[2]]);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (f64::from(hi) + 0.05) / (f64::from(lo) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::theme::ALL_PRESETS;

    fn ratio(a: Rgba8, b: Rgba8) -> f64 {
        let la = crate::grid::relative_luminance([a[0], a[1], a[2]]);
        let lb = crate::grid::relative_luminance([b[0], b[1], b[2]]);
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (f64::from(hi) + 0.05) / (f64::from(lo) + 0.05)
    }

    /// Straight-alpha src-over of `fg` onto opaque `bg`.
    fn composite(fg: Rgba8, bg: Rgba8) -> Rgba8 {
        let alpha = u16::from(fg[3]);
        let mix = |src: u8, dst: u8| -> u8 {
            (((u16::from(src) * alpha) + (u16::from(dst) * (255 - alpha)) + 127) / 255) as u8
        };
        [
            mix(fg[0], bg[0]),
            mix(fg[1], bg[1]),
            mix(fg[2], bg[2]),
            0xFF,
        ]
    }

    fn presets() -> Vec<ThemePalette> {
        ALL_PRESETS
            .iter()
            .map(|theme| ThemePalette::from_theme(theme))
            .collect()
    }

    #[test]
    fn bitty_dark_tokens_are_pinned() {
        // Golden: the reference preset must produce the documented token set.
        let tokens = ChromeTokens::resolve(&ThemePalette::bitty_dark());
        assert_eq!(tokens.border_px, 1, "thin one-pixel rule");
        assert_eq!(tokens.radius_px, 6, "accepted panel radius");
        assert_eq!(tokens.padding_px, 8, "one-cell padding");
        assert_eq!(
            u32::from(tokens.border_px),
            bitty_config::types::DEFAULT_DECORATION_BORDER_PX,
            "panel and overlay borders share one width default"
        );
        assert_eq!(
            u32::from(tokens.radius_px),
            bitty_config::types::DEFAULT_DECORATION_RADIUS_PX,
            "panel and overlay corners share one radius default"
        );
        assert_eq!(tokens.surface, ThemePalette::bitty_dark().selection);
        assert_eq!(
            tokens.border_muted,
            [0x59, 0x59, 0x59, 0xAA],
            "Bitty Dark muted border is the accepted idle outline"
        );
        assert_eq!(
            tokens.border_focus,
            [0x33, 0xCC, 0xFF, 0xFF],
            "Bitty Dark focus border is the accepted focused outline"
        );
        assert_ne!(tokens.border_muted, tokens.border_focus);
    }

    #[test]
    fn every_preset_preserves_contrast_floors() {
        for palette in presets() {
            let tokens = ChromeTokens::resolve(&palette);
            let muted = composite(tokens.border_muted, tokens.surface);
            assert!(
                ratio(tokens.border_focus, palette.background) >= MIN_CHROME_FOCUS_CONTRAST,
                "focus border must clear AC-1 against the window ground {tokens:?}"
            );
            assert!(
                ratio(tokens.border_focus, muted) >= MIN_CHROME_FOCUS_CONTRAST,
                "focus border must stay distinguishable from the composite idle border \
                 (AC-2) {tokens:?} ratio={}",
                ratio(tokens.border_focus, muted)
            );
            assert_ne!(
                tokens.border_muted, tokens.surface,
                "the normal border must not disappear into the surface"
            );
            assert!(
                ratio(muted, tokens.surface) >= 1.0,
                "muted border is never darker than its ground"
            );
            assert!(
                ratio(tokens.surface_elevated, tokens.surface) >= 1.0,
                "elevated surface is never darker than its base"
            );
        }
    }

    #[test]
    fn render_golden_frame_emits_surface_then_thin_ring() {
        // Golden paint record: fixed frame, fixed tokens, byte-stable values.
        let tokens = ChromeTokens {
            border_px: 1,
            radius_px: 6,
            padding_px: 8,
            surface: [0x20, 0x21, 0x30, 0xFF],
            surface_elevated: [0x28, 0x29, 0x3A, 0xFF],
            border_muted: [0x58, 0x5B, 0x70, 0xFF],
            border_focus: [0x89, 0xB4, 0xFA, 0xFF],
        };
        let frame = ChromeFrame::new(
            RectPx::new(8, 8, 120, 60),
            tokens,
            ChromeState::Normal,
            ChromeElevation::Overlay,
        );
        let paint = frame.paint();
        assert_eq!(
            paint.surface,
            RoundedFill {
                frame: RectPx::new(8, 8, 120, 60),
                border: 0,
                radius: 6,
                color: tokens.surface_elevated,
            }
        );
        assert_eq!(
            paint.border,
            Some(RoundedFill {
                frame: RectPx::new(8, 8, 120, 60),
                border: 1,
                radius: 6,
                color: tokens.border_muted,
            })
        );
        let clip = paint.clip.expect("radius 6 leaves an inner arc");
        assert_eq!(clip.rect, RectPx::new(9, 9, 118, 58));
        assert_eq!(clip.radius, 5);
        assert_eq!(frame.content_rect(), RectPx::new(17, 17, 102, 42));
    }

    #[test]
    fn focus_and_normal_borders_select_distinct_tokens() {
        let tokens = ChromeTokens::resolve(&ThemePalette::bitty_dark());
        let normal = ChromeFrame::new(
            RectPx::new(0, 0, 40, 20),
            tokens,
            ChromeState::Normal,
            ChromeElevation::Raised,
        );
        let focused = ChromeFrame::new(
            RectPx::new(0, 0, 40, 20),
            tokens,
            ChromeState::Focused,
            ChromeElevation::Raised,
        );
        assert_eq!(normal.border_color(), tokens.border_muted);
        assert_eq!(focused.border_color(), tokens.border_focus);
        assert_ne!(normal.border_color(), focused.border_color());
        assert_eq!(
            normal.paint().border.expect("ring").border,
            1,
            "focus must not change ring geometry"
        );
        assert_eq!(
            focused.paint().border.expect("ring").border,
            1,
            "focus must not change ring geometry"
        );
    }

    #[test]
    fn radius_zero_is_the_square_fallback() {
        let tokens = ChromeTokens {
            border_px: 1,
            radius_px: 0,
            ..ChromeTokens::resolve(&ThemePalette::bitty_dark())
        };
        let frame = ChromeFrame::new(
            RectPx::new(0, 0, 40, 20),
            tokens,
            ChromeState::Normal,
            ChromeElevation::Raised,
        );
        let paint = frame.paint();
        assert_eq!(paint.surface.radius, 0, "square surface");
        assert_eq!(
            paint.border.expect("ring").radius,
            0,
            "square ring fallback"
        );
        assert!(paint.clip.is_none(), "square frames never clip glyphs");
    }

    #[test]
    fn corner_glyph_fallback_is_documented_and_total() {
        let full = ChromeCornerGlyphs::resolve(ChromeGlyphCoverage::Full);
        assert_eq!(full.corners, [Some('╭'), Some('╮'), Some('╰'), Some('╯')]);
        assert_eq!(full.mask(0, ChromeGlyphCoverage::Full), Some('▘'));
        let basic = ChromeCornerGlyphs::resolve(ChromeGlyphCoverage::Basic);
        assert_eq!(basic.corners, [Some('┌'), Some('┐'), Some('└'), Some('┘')]);
        assert_eq!(basic.mask(0, ChromeGlyphCoverage::Basic), None);
        let none = ChromeCornerGlyphs::resolve(ChromeGlyphCoverage::None);
        assert_eq!(none.corners, [None; 4], "square, glyph-free fallback");
        assert_eq!(none.mask(0, ChromeGlyphCoverage::None), None);
    }

    #[test]
    fn dpi_scaling_keeps_one_physical_pixel_per_logical_pixel() {
        let tokens = ChromeTokens::resolve(&ThemePalette::bitty_dark());
        assert_eq!(tokens.scaled(1.0).border_px, 1);
        assert_eq!(tokens.scaled(2.0).border_px, 2);
        assert_eq!(tokens.scaled(2.0).radius_px, 12);
        assert_eq!(tokens.scaled(2.0).padding_px, 16);
        assert_eq!(
            tokens.scaled(2.0).border_muted,
            tokens.border_muted,
            "DPI never invents a colour"
        );
        assert_eq!(tokens.scaled(f64::NAN).border_px, 1, "sanitized scale");
    }

    #[test]
    fn elevation_surfaces_are_ordered_and_deterministic() {
        let tokens = ChromeTokens::resolve(&ThemePalette::bitty_dark());
        let base = tokens.surface_for(ChromeElevation::Base);
        let raised = tokens.surface_for(ChromeElevation::Raised);
        let overlay = tokens.surface_for(ChromeElevation::Overlay);
        assert_eq!(base[3], 0, "base leaves the underlying frame visible");
        assert_eq!(raised, tokens.surface);
        assert_eq!(overlay, tokens.surface_elevated);
        assert_ne!(raised, overlay);
    }

    #[test]
    fn degenerate_frame_paints_nothing_but_stays_total() {
        let tokens = ChromeTokens::resolve(&ThemePalette::bitty_dark());
        let frame = ChromeFrame::new(
            RectPx::new(0, 0, 0, 0),
            tokens,
            ChromeState::Normal,
            ChromeElevation::Raised,
        );
        let mut out = Vec::new();
        let paint = frame.push(&mut out);
        assert!(out.is_empty(), "no zero-area fills are pushed");
        assert!(paint.is_empty());
        assert_eq!(frame.content_rect(), RectPx::new(9, 9, 0, 0));
    }
}
