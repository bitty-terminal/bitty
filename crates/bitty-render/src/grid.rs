//! The grid pipeline: terminal-state snapshots in, owned draw records out.
//!
//! [`GridRenderer`] consumes the public render surface of
//! `bitty-term-state` — a versioned [`Snapshot`] plus its [`Damage`] — and
//! produces an owned [`DrawList`] through three stages:
//!
//! 1. **Plan**: grid-coordinate damage is converted to pixel rectangles
//!    against the configured [`CellMetrics`] and fed to
//!    [`frame::plan_frame`] through [`SnapshotDamage`], preserving the
//!    damage-driven partial-redraw semantics of the terminal-state-rfc
//!    damage model.
//! 2. **Place**: every cell covered by a planned dirty rectangle is
//!    examined. Backgrounds merge into maximal horizontal same-color runs
//!    per row (CTX-0471) and, with text decorations, become [`FillRect`]s;
//!    printable characters become [`GlyphInstance`]s.
//!    Underline paint is style-faithful (CTX-0583): `Single`/`Double` are
//!    full-width bars, while `Curly`/`Dotted`/`Dashed` decompose into
//!    bounded, deterministic rectangle patterns anchored to absolute pixel
//!    columns (see [`MAX_DECORATION_RECTS_PER_CELL`]).
//!    Trailing halves of wide characters (`spacer` cells) are skipped — the
//!    leading half already paints across both columns. Invisible cells
//!    suppress their glyph but keep their background.
//! 3. **Cache**: glyph lookups go through [`GlyphCache`] keyed by
//!    [`RasterKey`]; bitmaps are placed into [`GlyphAtlas`], which pairs
//!    the shelf-packed [`AtlasLayout`] with a CPU-side coverage texture and
//!    a drainable upload queue. A backend drains
//!    [`GridRenderer::take_atlas_uploads`] and copies the queued bytes into
//!    its texture; nothing here touches GPU objects (the GpuContext seam
//!    stays behind `gpu`). Actual vertex buffers are intentionally **not**
//!    part of this slice: the [`DrawList`] describes instances in owned
//!    types only, and the platform-seam slice decides the upload format.
//!
//! # Reading rule (ADR-0003 dependency rule 3)
//!
//! This module depends on `bitty-term-state` exclusively through its public
//! `Snapshot`/`Damage`/`Cell` types. No private structure is reached into,
//! and nothing here ever mutates terminal state: the renderer only reads.
//!
//! # Damage semantics
//!
//! Every visited cell repaints its full background (resolved, including the
//! default background; adjacent same-color cells merge into one horizontal
//! rectangle, which leaves coverage unchanged), so drawing the union of
//! incremental frames equals a full redraw of the same final state —
//! over-damage stays safe and under-damage is impossible, mirroring the
//! state layer's contract.
//! Scrollback damage ranges concern lines above the visible grid; they add
//! no pixels on this surface (the active screen) and contribute no regions.
//! Stale or oversized damage cannot under-damage either: planning clips
//! regions to the snapshot-derived extent.
//!
//! # Deferred deliberately
//!
//! - **Cursor visuals**: the cursor flows through the snapshot, but cursor
//!   shape/color policy belongs to the configuration/theme layer and is not
//!   invented here.
//! - **Scrollback viewport rendering**: this surface is the active screen.
//! - **Shaped clusters, color fonts, synthetic bold**: await the text RFC
//!   named by ADR-0004. One glyph per cell (the leading Unicode scalar),
//!   placed on a metric-aware baseline with a fixed-rule fallback (see
//!   [`BASELINE_NUMERATOR`] and [`resolve_baseline_offset`]).
//! - **Subpixel RGB antialiasing policy**: upstream coverage is averaged to
//!   luminance exactly like [`crate::software::SurfaceRgba::blend_glyph`].
//!
//! # Performance instrumentation vs budgets
//!
//! [`GridRenderer::counters`], [`GridRenderer::cache_stats`], and
//! [`GridRenderer::atlas_stats`] expose monotone counters (frames planned,
//! cells examined/drawn, glyphs emitted/rasterized, cache and atlas
//! hits/misses, evictions, inline fallbacks). They exist to make the cost
//! drivers of performance-budget-rfc **measurable**: PB-4 input latency
//! (≤ 8 ms p50 key-to-screen) and PB-6 throughput floor (≥ 40 MB/s
//! parse-and-render) are dominated by exactly these work items, and PB-7
//! idle usage depends on this pipeline staying frame-on-demand (a clean
//! plan emits no work at all). These counters do **not** measure wall-clock
//! time, and this slice claims **no budget compliance**: per the RFC's
//! cross-cutting rules, budgets become acceptance criteria only after the
//! measurement harness, corpora, and reference machines are defined by an
//! implementing task.

use std::collections::HashMap;

use bitty_term_state::{
    Color, Damage, DamageRect, DamagedRegion, Rgb, Snapshot, Style, char_cell_width,
};

use crate::atlas::{AtlasDims, AtlasLayout, AtlasSlot, DEFAULT_ATLAS_DIMENSION};
use crate::cache::{CachedGlyph, GlyphCache};
use crate::error::RenderError;
use crate::frame::{DamageDescriptor, FramePlan, plan_frame};
use crate::geometry::{ExtentPx, RectPx};
use crate::glyph::{
    BitmapFormat, FontId, FontQuery, GlyphBitmap, GlyphMetrics, GlyphRasterizer, RasterKey,
};

/// Straight-alpha RGBA color, `[r, g, b, a]` bytes.
pub type Rgba8 = [u8; 4];

/// Alpha-scales a straight-alpha color by `factor` (`0.0..=1.0`).
///
/// Used by the RFC-0002 (CTX-0341) renderer-side panel transitions to fade
/// Core-owned chrome in/out without touching terminal truth. RGB is left
/// untouched (straight alpha); `factor` is clamped so a hostile value can
/// never invert or wrap the channel. At `factor >= 1.0` the color is
/// returned byte-identical.
#[must_use]
pub fn scale_alpha(color: Rgba8, factor: f32) -> Rgba8 {
    let a = (f32::from(color[3]) * factor.clamp(0.0, 1.0)).round();
    [color[0], color[1], color[2], a.clamp(0.0, 255.0) as u8]
}

/// Linear interpolation from `from` to `to` at `t` (`0.0..=1.0`), per channel
/// in straight alpha. `t` is clamped; endpoints are returned byte-exact.
#[must_use]
pub fn lerp_rgba(from: Rgba8, to: Rgba8, t: f32) -> Rgba8 {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| -> u8 {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [
        mix(from[0], to[0]),
        mix(from[1], to[1]),
        mix(from[2], to[2]),
        mix(from[3], to[3]),
    ]
}

/// Default foreground: Bitty Dark `#cdd6f4`, fully opaque.
///
/// Mirrors [`bitty_config::theme::BITTY_DARK`]`foreground`: a soft
/// lavender-white that avoids pure-white glare on the dark background.
/// This is the glyph color for unstyled cells and prompt text.
pub const DEFAULT_FG: Rgba8 = [0xCD, 0xD6, 0xF4, 0xFF];
/// Default background: Bitty Dark `#1e1e2e`, fully opaque.
///
/// Mirrors [`bitty_config::theme::BITTY_DARK`]`background` and drives every
/// clear path (GPU clear color, headless composite, software surface): the
/// 0.06 hardcoded gray is gone. Dark indigo-gray, not pure black.
pub const DEFAULT_BG: Rgba8 = [0x1E, 0x1E, 0x2E, 0xFF];
/// Block cursor fill: Bitty Dark `#f5e0dc`, fully opaque.
///
/// Mirrors [`bitty_config::theme::BITTY_DARK`]`cursor`: warm rosewater,
/// distinct from foreground and selection so the cursor stays findable.
/// Embedders composite it (e.g. with reduced alpha for a block cursor);
/// the hue itself is owned by the theme.
pub const DEFAULT_CURSOR: Rgba8 = [0xF5, 0xE0, 0xDC, 0xFF];
/// Selection background fill: Bitty Dark `#313244`, fully opaque.
///
/// Mirrors [`bitty_config::theme::BITTY_DARK`]`selection`: one step above
/// the background so selected cells read clearly while foreground-colored
/// text stays legible on top.
pub const DEFAULT_SELECTION: Rgba8 = [0x31, 0x32, 0x44, 0xFF];

/// Resolved terminal palette carried by the renderer.
///
/// A preset is a fixed set of window background, foreground, cursor,
/// selection, and the 16 ANSI colors (CTX-0355). The [`Default`] value is the
/// designed Bitty Dark preset, so every existing construction keeps the
/// previously hardcoded fallback; a selected preset is resolved once in the
/// app layer and carried here so the clear color, default cell colors, and
/// ANSI 0–15 all follow `appearance.theme`.
///
/// CTX-0392 adds the bounded dynamic layer: `dynamic` is the fixed
/// 256-entry OSC 4 override table (`None` = no override, `Some(rgb)` =
/// live override). Fixed shape, no growth: an index outside `0..=255` is
/// unrepresentable, and malformed OSC 4 never writes here (fail-closed).
/// Queries and `palette_rgb_in` read through this layer first, so 0–15
/// custom/preset ANSI plus 16–255 cube/ramp all honor live overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    /// Window clear color and default cell background.
    pub background: Rgba8,
    /// Default glyph color.
    pub foreground: Rgba8,
    /// Block cursor fill (embedders may override alpha).
    pub cursor: Rgba8,
    /// Selection background fill.
    pub selection: Rgba8,
    /// The 16 ANSI colors, indices 0–15 (8 normal + 8 bright).
    pub ansi: [[u8; 3]; 16],
    /// Live OSC 4 overrides, index-addressed `0..=255` (CTX-0392).
    /// Fixed 256-entry shape, no growth; `None` inherits the base below.
    pub dynamic: [Option<[u8; 3]>; 256],
}

impl ThemePalette {
    /// Resolves a preset from the `bitty-config` theme registry.
    ///
    /// This is the single mapping from the config-side [`Theme`] to the
    /// render-side palette; RGB channels are copied verbatim and alpha is
    /// forced opaque for the four chrome colors (the preset has no alpha).
    /// The dynamic layer starts empty (`[None; 256]`).
    ///
    /// [`Theme`]: bitty_config::theme::Theme
    #[must_use]
    pub fn from_theme(theme: &bitty_config::theme::Theme) -> Self {
        Self {
            background: [
                theme.background[0],
                theme.background[1],
                theme.background[2],
                0xFF,
            ],
            foreground: [
                theme.foreground[0],
                theme.foreground[1],
                theme.foreground[2],
                0xFF,
            ],
            cursor: [theme.cursor[0], theme.cursor[1], theme.cursor[2], 0xFF],
            selection: [
                theme.selection[0],
                theme.selection[1],
                theme.selection[2],
                0xFF,
            ],
            ansi: theme.ansi,
            dynamic: [None; 256],
        }
    }

    /// Resolves an inline custom palette (CTX-0392) to the render side.
    ///
    /// Same mapping as [`Self::from_theme`] but from the validated
    /// [`CustomPalette`](bitty_config::theme::CustomPalette): chrome colors
    /// become opaque `Rgba8`, ANSI copies verbatim, dynamic starts empty.
    #[must_use]
    pub fn from_custom(custom: &bitty_config::theme::CustomPalette) -> Self {
        Self {
            background: [
                custom.background[0],
                custom.background[1],
                custom.background[2],
                0xFF,
            ],
            foreground: [
                custom.foreground[0],
                custom.foreground[1],
                custom.foreground[2],
                0xFF,
            ],
            cursor: [custom.cursor[0], custom.cursor[1], custom.cursor[2], 0xFF],
            selection: [
                custom.selection[0],
                custom.selection[1],
                custom.selection[2],
                0xFF,
            ],
            ansi: custom.ansi,
            dynamic: [None; 256],
        }
    }

    /// Active RGB for palette `index` (CTX-0392): the live OSC 4 override
    /// when present, else the base 16 ANSI / cube / ramp.
    #[must_use]
    pub fn active_index(self, index: u8) -> [u8; 3] {
        palette_rgb_in(&self, index)
    }

    /// The designed default preset (Bitty Dark), byte-identical to the
    /// historical hardcoded [`DEFAULT_BG`]/[`DEFAULT_FG`] fallback.
    #[must_use]
    pub fn bitty_dark() -> Self {
        Self::from_theme(&bitty_config::theme::BITTY_DARK)
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::bitty_dark()
    }
}
/// Pending-paste banner background: opaque dark amber, distinct from the
/// grid background, selection, and cursor so the confirmation prompt reads
/// as a warning overlay (CTX-0186 presentation-only dialog).
pub const PENDING_PASTE_BANNER_BG: Rgba8 = [0x5A, 0x4A, 0x00, 0xFF];
/// Pending-paste banner text: opaque light amber on [`PENDING_PASTE_BANNER_BG`].
pub const PENDING_PASTE_BANNER_FG: Rgba8 = [0xFF, 0xE2, 0x8B, 0xFF];
/// Help popup panel background (CTX-0265): opaque dark indigo, one step
/// above [`DEFAULT_BG`] so the floating panel reads above the grid while
/// staying inside the Bitty Dark family.
pub const HELP_PANEL_BG: Rgba8 = [0x18, 0x18, 0x28, 0xFF];
/// Help popup panel border (CTX-0265): opaque lavender, mirroring the
/// Bitty Dark accent so the 1-cell outline reads as chrome, not content.
pub const HELP_PANEL_BORDER: Rgba8 = [0xB4, 0xBE, 0xFE, 0xFF];
/// Help popup panel text (CTX-0265): opaque Bitty Dark foreground on
/// [`HELP_PANEL_BG`].
pub const HELP_PANEL_FG: Rgba8 = [0xCD, 0xD6, 0xF4, 0xFF];
/// Foreground alpha substituted for faint (`SGR 2`) text.
pub const FAINT_ALPHA: u8 = 0x7F;

/// Numerator of the fixed baseline rule: the pen baseline sits at
/// `row_top + cell_height * BASELINE_NUMERATOR / 4`. Metric-aware baselines
/// take precedence whenever the rasterizer measures the face (see
/// [`resolve_baseline_offset`]); this constant rule stays as the
/// deterministic fallback for backends without measurements.
pub const BASELINE_NUMERATOR: u32 = 3;

/// Resolves the pen-baseline offset below the row top for `cell_height`.
///
/// With usable face measurements the baseline sits one ascent below the row
/// top (`line_height_px + descent_px`, upstream descent-negative sign —
/// the alacritty reference pattern), clamped into the cell so hostile
/// metrics can never push text off-grid; without measurements the legacy
/// `cell_height * BASELINE_NUMERATOR / 4` rule keeps placement
/// deterministic. The resolved offset is total over all inputs.
#[must_use]
pub fn resolve_baseline_offset(
    cell_height: u32,
    metrics: Option<crate::glyph::FontMetrics>,
) -> i64 {
    let cell = i64::from(cell_height);
    let measured = metrics.filter(|m| m.is_usable()).map(|m| {
        // `as` rounds toward zero; ascent is non-negative by construction
        // (`is_usable` guarantees a finite positive line and finite
        // descent), so the cast truncates at most one sub-pixel row.
        (f64::from(m.line_height_px) + f64::from(m.descent_px)) as i64
    });
    match measured {
        Some(ascent) => ascent.clamp(0, cell),
        None => cell * i64::from(BASELINE_NUMERATOR) / 4,
    }
}

/// Pixel size of one grid cell.
///
/// Both dimensions must be non-zero. A monospace face at a given point size
/// yields one fixed cell size, supplied by the embedder; the rasterizer
/// contract deliberately does not aggregate font-wide metrics yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub width: u32,
    /// Cell height in pixels.
    pub height: u32,
}

impl CellMetrics {
    /// Validates and builds cell metrics.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero.
    pub fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidInput {
                reason: "cell metrics must be non-zero",
            });
        }
        Ok(Self { width, height })
    }

    /// Pixel extent of a `cols x rows` grid (saturating arithmetic: every
    /// intermediate multiply saturates, so hostile dimensions cannot panic
    /// or wrap under any build profile).
    #[must_use]
    pub fn extent_for(&self, cols: usize, rows: usize) -> ExtentPx {
        ExtentPx::new(
            saturating_u32(u64::from(self.width).saturating_mul(cols as u64)),
            saturating_u32(u64::from(self.height).saturating_mul(rows as u64)),
        )
    }
}

/// Clamps a `u64` product back into `u32` range (value-preserving for every
/// realistic grid; saturation keeps hostile dimensions overflow-free).
const fn saturating_u32(value: u64) -> u32 {
    const MAX: u64 = u32::MAX as u64;
    if value > MAX { u32::MAX } else { value as u32 }
}

/// Clamps a `u64` product back into positive-`i32` range.
const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

/// Resolves one palette/indexed/direct color to straight-alpha RGBA.
///
/// `Color::Default` maps to `fallback`; indexed entries 0–15 go through the
/// designed default preset ([`bitty_config::theme::BITTY_DARK`]`ansi`, so the
/// synthetic demo pump's `\x1b[32m` green renders as the theme's `#a6e3a1`
/// instead of a hardcoded green), while 16–231 (6x6x6 cube) and 232–255
/// (grayscale ramp) stay xterm-compatible. Fully deterministic on every
/// platform.
///
/// This is the default-preset wrapper over [`resolve_color_in`]; renderers
/// carrying a resolved [`ThemePalette`] call that function instead.
#[must_use]
pub fn resolve_color(color: Option<&Color>, fallback: Rgba8) -> Rgba8 {
    resolve_color_in(&ThemePalette::bitty_dark(), color, fallback)
}

/// Theme-aware [`resolve_color`]: ANSI 0–15 come from `palette`.
#[must_use]
pub fn resolve_color_in(palette: &ThemePalette, color: Option<&Color>, fallback: Rgba8) -> Rgba8 {
    let rgb = match color {
        None | Some(Color::Default) => [fallback[0], fallback[1], fallback[2]],
        Some(Color::Rgb(Rgb { r, g, b })) => [*r, *g, *b],
        Some(Color::Indexed(i)) => palette_rgb_in(palette, *i),
    };
    [rgb[0], rgb[1], rgb[2], fallback[3]]
}

/// Built-in 256-color palette entry.
///
/// Indices 0–15 are the designed default preset's ANSI colors
/// ([`bitty_config::theme::BITTY_DARK`]`ansi`, the single source of truth —
/// read from the preset, never duplicated here); 16–231 are the
/// xterm-compatible 6x6x6 cube and 232–255 the grayscale ramp.
///
/// This is the default-preset wrapper over [`palette_rgb_in`].
#[must_use]
pub fn palette_rgb(index: u8) -> [u8; 3] {
    palette_rgb_in(&ThemePalette::bitty_dark(), index)
}

/// Theme-aware [`palette_rgb`]: live OSC 4 overrides win; else indices
/// 0–15 come from `palette.ansi`; the 6x6x6 cube and grayscale ramp stay
/// xterm-compatible.
#[must_use]
pub fn palette_rgb_in(palette: &ThemePalette, index: u8) -> [u8; 3] {
    if let Some(rgb) = palette.dynamic[index as usize] {
        return rgb;
    }
    if index < 16 {
        return palette.ansi[index as usize];
    }
    const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if index <= 231 {
        let n = u32::from(index) - 16;
        return [
            CUBE_LEVELS[(n / 36) as usize],
            CUBE_LEVELS[((n / 6) % 6) as usize],
            CUBE_LEVELS[(n % 6) as usize],
        ];
    }
    let gray = 8 + 10 * (u32::from(index) - 232);
    [gray as u8; 3]
}

/// Effective (foreground, background) pair for a styled cell.
///
/// Inverse video swaps the pair; faint reduces foreground alpha. The pair
/// is resolved eagerly so downstream code never sees symbolic colors.
///
/// This is the default-preset wrapper over [`resolved_colors_in`]; renderers
/// carrying a resolved [`ThemePalette`] call that function instead.
#[must_use]
pub fn resolved_colors(style: &Style) -> (Rgba8, Rgba8) {
    resolved_colors_in(&ThemePalette::bitty_dark(), style)
}

/// Theme-aware [`resolved_colors`]: default fg/bg come from `palette`.
#[must_use]
pub fn resolved_colors_in(palette: &ThemePalette, style: &Style) -> (Rgba8, Rgba8) {
    let fg = resolve_color_in(palette, style.foreground.as_ref(), palette.foreground);
    let bg = resolve_color_in(palette, style.background.as_ref(), palette.background);
    let (fg, bg) = if style.attributes.inverse {
        (bg, fg)
    } else {
        (fg, bg)
    };
    let fg = if style.attributes.faint {
        [fg[0], fg[1], fg[2], FAINT_ALPHA]
    } else {
        fg
    };
    (fg, bg)
}

/// Resolves an `appearance.theme` identifier to the preset render consumes.
///
/// Thin wrapper over [`bitty_config::theme::resolve_theme`]: `None`/empty
/// and unknown names yield the designed default preset (unknown names are
/// logged to stderr by the config layer). Render entry points that take a
/// theme name — rather than assuming defaults — go through here so there is
/// exactly one preset registry.
#[must_use]
pub fn active_theme(appearance_theme: Option<&str>) -> &'static bitty_config::theme::Theme {
    bitty_config::theme::resolve_theme(appearance_theme)
}

/// Cursor fill for a live cursor, or `None` when the cursor is hidden
/// or outside the `cols` x `rows` grid.
///
/// Honors `DECSCUSR` (`CSI Ps SP q`) via the snapshot [`Cursor::cursor_style`]:
/// `Default`/block paints a full cell, bar paints a left vertical strip,
/// underline paints a bottom horizontal strip. Blink variants (`Blinking*`)
/// share their steady shape here; blink timing stays with the embedder's
/// existing visibility/focus gate. Thickness is 15% of the cell width
/// (rounded, min 1px, clamped to the cell), matching alacritty's
/// `cursor.thickness = 0.15` beam/underline geometry and ghostty's
/// `cursor_bar`/`cursor_underline` thin-rect shapes (`DEC-0017`: ghostty
/// `src/terminal/cursor.zig` + `src/font/sprite/draw/special.zig`).
///
/// The fill color is the theme cursor ([`DEFAULT_CURSOR`]), distinct from
/// selection ([`DEFAULT_SELECTION`]) and the pending-paste banner, so the
/// cursor stays findable. This is the render-side cursor primitive —
/// embedders that paint their own overlay (e.g. the runtime tick path) must
/// reuse this geometry so the cursor never depends on which layer drew it.
#[must_use]
pub fn cursor_fill(
    cursor: &bitty_term_state::Cursor,
    cell: CellMetrics,
    cols: usize,
    rows: usize,
) -> Option<FillRect> {
    cursor_fill_in(&ThemePalette::bitty_dark(), cursor, cell, cols, rows)
}

/// Theme-aware [`cursor_fill`]: the emitted fill carries `palette.cursor`.
#[must_use]
pub fn cursor_fill_in(
    palette: &ThemePalette,
    cursor: &bitty_term_state::Cursor,
    cell: CellMetrics,
    cols: usize,
    rows: usize,
) -> Option<FillRect> {
    use bitty_term_state::CursorStyle;

    if !cursor.visible {
        return None;
    }
    let row = usize::from(cursor.position.row);
    let col = usize::from(cursor.position.col);
    if row >= rows || col >= cols {
        return None;
    }
    let base_x = saturating_i32(u64::from(col as u32).saturating_mul(u64::from(cell.width)));
    let base_y = saturating_i32(u64::from(row as u32).saturating_mul(u64::from(cell.height)));
    let thickness = cursor_thickness(cell);
    let rect = match cursor.cursor_style {
        CursorStyle::Default | CursorStyle::BlinkingBlock | CursorStyle::SteadyBlock => {
            RectPx::new(base_x, base_y, cell.width, cell.height)
        }
        CursorStyle::BlinkingBar | CursorStyle::SteadyBar => {
            RectPx::new(base_x, base_y, thickness, cell.height)
        }
        CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => RectPx::new(
            base_x,
            base_y.saturating_add(cell.height.saturating_sub(thickness) as i32),
            cell.width,
            thickness,
        ),
    };
    Some(FillRect {
        rect,
        color: palette.cursor,
    })
}

/// Thin-cursor thickness in pixels: 15% of the cell width, rounded, min 1px.
///
/// Matches alacritty `cursor.thickness = 0.15` (`thickness * cell_width`,
/// min 1px) for beam/underline rects; ghostty scales its `cursor_thickness`
/// metric the same way for `cursor_bar`/`cursor_underline`. Clamped to the
/// cell so hostile or tiny metrics cannot produce an empty or overflowing
/// rect. Integer arithmetic keeps the pipeline deterministic.
fn cursor_thickness(cell: CellMetrics) -> u32 {
    let rounded = cell
        .width
        .saturating_mul(15)
        .saturating_add(50)
        .checked_div(100)
        .unwrap_or(1)
        .max(1);
    rounded.min(cell.width).min(cell.height)
}

/// Cell-height divisor that scales underline/strikethrough bar thickness.
///
/// Mirrored by `bitty-rich::hyperlink` for headless overlay geometry:
/// `bitty-rich` deliberately avoids a `bitty-render` dependency, so the
/// formula is duplicated by value and pinned across crates by
/// `bitty-runtime/tests/underline_thickness_mirror.rs`.
const UNDERLINE_THICKNESS_DIVISOR: u32 = 8;

/// Minimum underline/strikethrough bar thickness in pixels.
const MIN_UNDERLINE_THICKNESS_PX: u32 = 1;

/// Maximum underline/strikethrough bar thickness in pixels.
const MAX_UNDERLINE_THICKNESS_PX: u32 = 2;

/// Underline/strikethrough bar thickness in pixels for a cell of
/// `cell_height`: `cell_height / 8` clamped to `1..=2`.
///
/// Public so `bitty-runtime`, the one consumer that sees both this crate and
/// `bitty-rich`, can pin the duplicated formula against
/// `bitty_rich::hyperlink::underline_thickness`.
#[must_use]
pub fn underline_thickness(cell_height: u32) -> u32 {
    (cell_height / UNDERLINE_THICKNESS_DIVISOR)
        .clamp(MIN_UNDERLINE_THICKNESS_PX, MAX_UNDERLINE_THICKNESS_PX)
}

/// Maximum decoration rectangles one cell may emit for a patterned
/// underline (`CTX-0583`).
///
/// Patterns (`Curly`, `Dotted`, `Dashed`) are emitted as deterministic
/// [`FillRect`] runs because the backends paint only rectangles. A hostile
/// cell width must not grow the fill list without bound, so when the
/// natural pattern step would exceed this many rectangles the step widens
/// to cover the whole span with at most this many rectangles (coarser, but
/// never a gap and never unbounded).
pub const MAX_DECORATION_RECTS_PER_CELL: u32 = 64;

/// Where one cell's underline paint goes: absolute pixel origin, full
/// pixel span (covered columns included), cell height, thickness, and the
/// resolved underline color.
#[derive(Debug, Clone, Copy)]
struct UnderlinePlacement {
    /// Cell left edge in absolute pixels.
    left: i32,
    /// Cell top edge in absolute pixels.
    top: i32,
    /// Full pixel width to cover (wide cells span both columns).
    span_w: u32,
    /// Cell height in pixels.
    cell_height: u32,
    /// Bar thickness in pixels ([`underline_thickness`] of `cell_height`).
    thickness: u32,
    /// Resolved underline color.
    color: Rgba8,
}

impl UnderlinePlacement {
    /// Baseline strip row: the pre-CTX-0583 single/double underline
    /// position (`cell_height - 2 * thickness`), the highest row that still
    /// fits a thickness-sized bar with one bar of clearance below the glyph
    /// body.
    fn strip(self) -> u32 {
        self.cell_height
            .saturating_sub(self.thickness.saturating_mul(2))
    }

    /// Absolute pixel row for a cell-relative row offset.
    fn row(self, cell_row: u32) -> i32 {
        self.top.saturating_add(saturating_i32(u64::from(cell_row)))
    }
}

/// Emits the underline shape for one cell and returns the rectangle count.
///
/// All arithmetic saturates and every emitted rectangle satisfies `0 <= x`,
/// `x + width <= left + span_w`, and `0 <= y`, `y + height <= top +
/// cell_height`, so no style can leak paint outside its cell under any cell
/// geometry.
///
/// `Curly`/`Dotted`/`Dashed` patterns are anchored to absolute pixel
/// coordinates, so adjacent underlined cells continue one pattern run
/// across the cell boundary instead of restarting per cell.
fn push_underline_pattern(
    fills: &mut Vec<FillRect>,
    style: bitty_term_state::UnderlineStyle,
    cell: UnderlinePlacement,
) -> u64 {
    use bitty_term_state::UnderlineStyle;

    match style {
        UnderlineStyle::None => 0,
        UnderlineStyle::Single => push_bar(
            fills,
            cell.left,
            cell.row(cell.strip()),
            cell.span_w,
            cell.thickness,
            cell.color,
        ),
        UnderlineStyle::Double => {
            let lower = push_bar(
                fills,
                cell.left,
                cell.row(cell.strip()),
                cell.span_w,
                cell.thickness,
                cell.color,
            );
            let upper_row = cell
                .cell_height
                .saturating_sub(cell.thickness.saturating_mul(4));
            lower
                + push_bar(
                    fills,
                    cell.left,
                    cell.row(upper_row),
                    cell.span_w,
                    cell.thickness,
                    cell.color,
                )
        }
        UnderlineStyle::Dotted => push_striped_pattern(
            fills,
            cell,
            cell.thickness.saturating_mul(2),
            cell.thickness,
        ),
        UnderlineStyle::Dashed => {
            let stripe = cell.thickness.saturating_add(1);
            push_striped_pattern(fills, cell, stripe.saturating_mul(2), stripe)
        }
        UnderlineStyle::Curly => push_curly_wave(fills, cell),
    }
}

/// Appends one solid rectangle and returns 1 (keeps call sites uniform).
fn push_bar(
    fills: &mut Vec<FillRect>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    color: Rgba8,
) -> u64 {
    if width == 0 || height == 0 {
        return 0;
    }
    fills.push(FillRect {
        rect: RectPx::new(x, y, width, height),
        color,
    });
    1
}

/// Solid/void stripe pattern on the baseline strip (`Dotted`, `Dashed`).
///
/// Each cycle is one `stripe`-pixel bar followed by a gap of
/// `step - stripe` pixels, so `Dotted` (stripe = thickness, step =
/// 2 * thickness) reads as separated square dots and `Dashed`
/// (stripe = thickness + 1, step = 2 * stripe) as short dashes; `Dashed`
/// matches the ghostty sprite's `width / 3 + 1` dash width at the
/// representative 8x16 cell. The pattern is anchored at
/// absolute pixel `left`, so a run of adjacent underlined cells reads as
/// one uninterrupted stripe sequence. A pattern step narrower than two
/// pixels would overlap itself; one wider than the detail budget is widened
/// until the span fits [`MAX_DECORATION_RECTS_PER_CELL`] rectangles (each
/// cell stays fully covered; only the detail coarsens).
fn push_striped_pattern(
    fills: &mut Vec<FillRect>,
    cell: UnderlinePlacement,
    step: u32,
    stripe: u32,
) -> u64 {
    if cell.span_w == 0 || cell.thickness == 0 || stripe == 0 {
        return 0;
    }
    let mut step = step.max(stripe.saturating_add(1));
    // Bound the emitted runs: `ceil(span_w / step) <= cap`.
    let budget = MAX_DECORATION_RECTS_PER_CELL.max(1);
    if cell.span_w > step.saturating_mul(budget) {
        step = cell.span_w.div_ceil(budget);
    }
    let right = cell
        .left
        .saturating_add(saturating_i32(u64::from(cell.span_w)));
    let y = cell.row(cell.strip());
    let mut x = cell.left;
    let mut pushed = 0u64;
    while x < right {
        let remaining = saturating_u32(u64::try_from(right - x).unwrap_or(u64::MAX));
        pushed += push_bar(
            fills,
            x,
            y,
            stripe.min(remaining),
            cell.thickness,
            cell.color,
        );
        // Integer phase: always advance at least one pixel so a zero or
        // tiny step can never loop forever.
        x = x.saturating_add(saturating_i32(u64::from(step)).max(1));
    }
    pushed
}

/// Stepped wave approximating the undercurl sprite (`Curly`).
///
/// One cycle is four flat segments of `step` pixels: baseline rise,
/// mid-rise, crest at `2 * thickness` above the baseline strip, mid-fall
/// (a stair-step of the smooth single-cycle wave the sprite reference
/// strokes). The wave is anchored at absolute pixel `left`, so the cycle
/// continues across adjacent cells. `step` starts at the underline
/// thickness (2 px at the representative 8x16 cell) and widens until the
/// span fits [`MAX_DECORATION_RECTS_PER_CELL`] rectangles; the crest row is
/// clamped so it can never leave the cell.
fn push_curly_wave(fills: &mut Vec<FillRect>, cell: UnderlinePlacement) -> u64 {
    if cell.span_w == 0 || cell.thickness == 0 {
        return 0;
    }
    // Four flat segments per cycle; the detail budget caps the cycle count.
    const SEGMENTS_PER_CYCLE: u32 = 4;
    let budget = MAX_DECORATION_RECTS_PER_CELL.max(1);
    let max_segments = budget
        .saturating_sub(budget % SEGMENTS_PER_CYCLE)
        .max(SEGMENTS_PER_CYCLE);
    let step = cell
        .thickness
        .max(1)
        .max(cell.span_w.div_ceil(max_segments))
        .max(1);

    // Wave amplitude: two thicknesses, clamped so the crest stays inside
    // the cell even for tiny cells with a low baseline strip.
    let amplitude = cell
        .thickness
        .saturating_mul(2)
        .min(cell.cell_height.saturating_sub(cell.thickness));
    // Row offsets, one per segment: low, mid, crest, mid.
    let offsets = [0, amplitude / 2, amplitude, amplitude / 2];

    let strip = cell.strip();
    let right = cell
        .left
        .saturating_add(saturating_i32(u64::from(cell.span_w)));
    let mut x = cell.left;
    let mut pushed = 0u64;
    while x < right {
        // Phase derives from the absolute pixel column, so the wave is
        // continuous across cell boundaries.
        let segment = (x.unsigned_abs() / step) % SEGMENTS_PER_CYCLE;
        let offset = offsets[usize::try_from(segment).unwrap_or(0)];
        let remaining = saturating_u32(u64::try_from(right - x).unwrap_or(u64::MAX));
        let width = step.min(remaining);
        let y = cell.row(strip.saturating_sub(offset));
        pushed += push_bar(fills, x, y, width, cell.thickness, cell.color);
        x = x.saturating_add(saturating_i32(u64::from(step)).max(1));
    }
    pushed
}

/// Selection background fill color: the theme selection ([`DEFAULT_SELECTION`]).
///
/// Selection highlighting itself lives with the embedder (which owns the
/// selection range), but the hue is owned here so every layer paints the
/// same `#313244`.
#[must_use]
pub const fn selection_fill() -> Rgba8 {
    DEFAULT_SELECTION
}

/// Theme-aware selection color: `palette.selection` (CTX-0355).
#[must_use]
pub const fn selection_fill_in(palette: &ThemePalette) -> Rgba8 {
    palette.selection
}

/// Selection highlight rectangles for a normalized inclusive cell range.
///
/// Pure embedder overlay primitive (CTX-0158): the caller owns the selection
/// range (ghostty `setSelection` semantics) and this function only maps it to
/// pixel [`FillRect`]s in the theme selection color ([`selection_fill`]).
/// One opaque rect per spanned row, row-major order, so the caller pushes at
/// most `grid_height` rects (bounded by the grid, never by input).
///
/// Endpoints are sorted and clamped to the `grid_width` x `grid_height` grid;
/// empty grids, zero cells, or collapsed ranges yield no rects. Total over
/// all inputs: no panics, no allocation beyond the bounded row count, no I/O.
#[must_use]
pub fn selection_fill_rects(
    anchor: (u16, u16),
    focus: (u16, u16),
    grid_width: usize,
    grid_height: usize,
    cell: CellMetrics,
) -> Vec<FillRect> {
    selection_fill_rects_in(
        &ThemePalette::bitty_dark(),
        anchor,
        focus,
        grid_width,
        grid_height,
        cell,
    )
}

/// Theme-aware [`selection_fill_rects`]: the emitted rects carry
/// `palette.selection`.
#[must_use]
pub fn selection_fill_rects_in(
    palette: &ThemePalette,
    anchor: (u16, u16),
    focus: (u16, u16),
    grid_width: usize,
    grid_height: usize,
    cell: CellMetrics,
) -> Vec<FillRect> {
    if grid_width == 0 || grid_height == 0 {
        return Vec::new();
    }
    if cell.width == 0 || cell.height == 0 {
        return Vec::new();
    }
    let (mut start_row, mut start_col) = (anchor.0 as usize, anchor.1 as usize);
    let (mut end_row, mut end_col) = (focus.0 as usize, focus.1 as usize);
    if (start_row, start_col) > (end_row, end_col) {
        core::mem::swap(&mut start_row, &mut end_row);
        core::mem::swap(&mut start_col, &mut end_col);
    }
    if start_row == end_row && start_col == end_col {
        return Vec::new();
    }
    let max_row = grid_height.saturating_sub(1);
    let max_col = grid_width.saturating_sub(1);
    start_row = start_row.min(max_row);
    start_col = start_col.min(max_col);
    end_row = end_row.min(max_row);
    end_col = end_col.min(max_col);
    if (start_row, start_col) == (end_row, end_col) {
        return Vec::new();
    }
    let mut rects = Vec::new();
    let mut row = start_row;
    while row <= end_row {
        let col_start = if row == start_row { start_col } else { 0 };
        let col_end = if row == end_row { end_col } else { max_col };
        if col_end >= col_start {
            let span = col_end - col_start + 1;
            let x = saturating_i32(
                u64::try_from(col_start)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(u64::from(cell.width)),
            );
            let y = saturating_i32(
                u64::try_from(row)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(u64::from(cell.height)),
            );
            let width = saturating_u32(
                u64::try_from(span)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(u64::from(cell.width)),
            );
            rects.push(FillRect {
                rect: RectPx::new(x, y, width, cell.height),
                color: selection_fill_in(palette),
            });
        }
        if row == end_row {
            break;
        }
        row += 1;
    }
    rects
}

/// Converts grid-coordinate damage into the pixel-domain descriptor
/// consumed by [`plan_frame`].
///
/// Construction is infallible: conversion uses saturating `u64`/`i64`
/// intermediates, and out-of-range regions are later clipped by the planner
/// rather than rejected (over-damage is safe). Scrollback regions
/// contribute nothing on this surface (see module docs).
#[derive(Debug, Clone)]
pub struct SnapshotDamage {
    extent: ExtentPx,
    pixel_regions: Vec<RectPx>,
    grid_regions: Vec<DamageRect>,
    full_hint: bool,
}

impl SnapshotDamage {
    /// Builds the descriptor for `snapshot` damaged by `damage`.
    #[must_use]
    pub fn new(snapshot: &Snapshot, damage: &Damage, cell: CellMetrics) -> Self {
        let extent = cell.extent_for(snapshot.width, snapshot.height);
        let mut grid_regions = Vec::new();
        let mut pixel_regions = Vec::new();
        for region in &damage.regions {
            let DamageRect {
                top,
                left,
                bottom,
                right,
            } = match region {
                DamagedRegion::Grid(rect) => *rect,
                DamagedRegion::Scrollback { .. } => continue,
            };
            grid_regions.push(DamageRect {
                top,
                left,
                bottom,
                right,
            });
            pixel_regions.push(grid_rect_to_px(top, left, bottom, right, cell));
        }
        Self {
            extent,
            pixel_regions,
            grid_regions,
            full_hint: false,
        }
    }

    /// Forces a full-redraw hint (first frame after realization, resize,
    /// device-loss recovery).
    #[must_use]
    pub fn with_full_redraw(mut self) -> Self {
        self.full_hint = true;
        self
    }

    /// The grid-coordinate regions behind this descriptor (same order;
    /// scrollback entries dropped).
    #[must_use]
    pub fn grid_regions(&self) -> &[DamageRect] {
        &self.grid_regions
    }
}

impl DamageDescriptor for SnapshotDamage {
    fn extent(&self) -> ExtentPx {
        self.extent
    }

    fn damaged_regions(&self) -> &[RectPx] {
        &self.pixel_regions
    }

    fn full_redraw_hint(&self) -> bool {
        self.full_hint
    }
}

/// Inclusive grid rectangle to inclusive-cell pixel rectangle; saturating.
fn grid_rect_to_px(top: u16, left: u16, bottom: u16, right: u16, cell: CellMetrics) -> RectPx {
    let cols = u64::from(right) - u64::from(left) + 1;
    let rows = u64::from(bottom) - u64::from(top) + 1;
    RectPx::new(
        saturating_i32(u64::from(left) * u64::from(cell.width)),
        saturating_i32(u64::from(top) * u64::from(cell.height)),
        saturating_u32(cols * u64::from(cell.width)),
        saturating_u32(rows * u64::from(cell.height)),
    )
}

/// One opaque background/decoration rectangle to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRect {
    /// Rectangle in logical pixels (cell-aligned by construction).
    pub rect: RectPx,
    /// Straight-alpha fill color.
    pub color: Rgba8,
}

/// Rolling horizontal run of same-colored background fills in one row of
/// one dirty rectangle (CTX-0471).
///
/// A visited cell's background merges into the open run when its color and
/// vertical geometry match and its left edge is not past the run's right
/// edge (adjacent columns, plus the wide-cell/spacer overlap). The run's
/// first rectangle is extended in place; decorations and tofu edges emitted
/// in between stay later in the list, so they keep drawing on top exactly
/// as the unmerged per-cell ordering did.
#[derive(Debug, Default)]
struct BackgroundRun {
    /// Index of the run's rectangle in [`DrawList::fills`].
    fill_index: usize,
    /// Right edge (exclusive) covered by the run so far, in pixels.
    x_end: i32,
    /// Run top edge in pixels.
    y: i32,
    /// Run height in pixels.
    height: u32,
    /// Run color (all merged cells must share it).
    color: Rgba8,
    /// Whether a run is currently open for this row.
    open: bool,
}

impl BackgroundRun {
    /// Merges `fill` into the open run, or starts a new run. Returns `true`
    /// when a new fill was appended to `fills`.
    fn merge(&mut self, fills: &mut Vec<FillRect>, fill: FillRect) -> bool {
        let FillRect { rect, color } = fill;
        if self.open
            && self.color == color
            && self.y == rect.y
            && self.height == rect.height
            && rect.x <= self.x_end
        {
            let right = self
                .x_end
                .max(rect.x.saturating_add(saturating_i32(u64::from(rect.width))));
            if let Some(run) = fills.get_mut(self.fill_index) {
                run.rect.width = saturating_u32(
                    u64::try_from(right.saturating_sub(run.rect.x)).unwrap_or(u64::MAX),
                );
            }
            self.x_end = right;
            return false;
        }
        self.fill_index = fills.len();
        self.x_end = rect.x.saturating_add(saturating_i32(u64::from(rect.width)));
        self.y = rect.y;
        self.height = rect.height;
        self.color = color;
        self.open = true;
        fills.push(FillRect { rect, color });
        true
    }
}

/// Signed distance from `(px, py)` to a rounded box centered at the origin
/// with half extents `(half_w, half_h)` and corner radius `r` (negative
/// inside, positive outside). Shared by the software compositor and the
/// WGSL present shader so both backends evaluate the identical f32 formula
/// (CTX-0311).
#[must_use]
pub(crate) fn sd_rounded_box(px: f32, py: f32, half_w: f32, half_h: f32, r: f32) -> f32 {
    let qx = px.abs() - (half_w - r);
    let qy = py.abs() - (half_h - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    qx.max(qy).min(0.0) + (ox * ox + oy * oy).sqrt() - r
}

/// A rounded-rectangle clip region in physical pixels (CTX-0311).
///
/// Applied per glyph instance as the inner-arc content clip of a decorated
/// View frame: fragments whose pixel center falls outside the rounded
/// rectangle lose their coverage, so text never overdraws the frame curve.
/// `radius == 0` is a plain rectangular clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundedClip {
    /// Clip rectangle (physical px).
    pub rect: RectPx,
    /// Corner radius (physical px), clamped to half the shorter side.
    pub radius: u16,
}

impl RoundedClip {
    /// Analytic coverage of this clip at pixel center `(px, py)` in the same
    /// pixel space as [`Self::rect`]: 1 fully inside, 0 fully outside, with
    /// a one-pixel linear transition at the boundary (identical f32 math to
    /// the WGSL SDF fragment stage). At `radius == 0` this is the exact
    /// rectangular hard edge used by the pre-CTX-0311 compositors.
    #[must_use]
    pub fn coverage_at(&self, px: f32, py: f32) -> f32 {
        let half_w = self.rect.width as f32 * 0.5;
        let half_h = self.rect.height as f32 * 0.5;
        if half_w <= 0.0 || half_h <= 0.0 {
            return 0.0;
        }
        let cx = self.rect.x as f32 + half_w;
        let cy = self.rect.y as f32 + half_h;
        let r = f32::from(self.radius).min(half_w).min(half_h);
        let d = sd_rounded_box(px - cx, py - cy, half_w, half_h, r);
        (0.5 - d).clamp(0.0, 1.0)
    }

    /// Cheap integer test for the software rasterizers: true when the pixel
    /// center at integer pixel `(px, py)` must be evaluated against the SDF.
    /// Pixels outside the rectangle need the zero coverage, and inside
    /// pixels only the four corner squares can be partly clipped; the deep
    /// interior is guaranteed coverage 1 and skips the SDF (CTX-0311 hot
    /// path). `radius == 0` is a plain rectangle: every inside pixel skips.
    #[must_use]
    pub fn may_clip_pixel(&self, px: i64, py: i64) -> bool {
        if self.rect.width == 0 || self.rect.height == 0 {
            return true;
        }
        let x0 = i64::from(self.rect.x);
        let y0 = i64::from(self.rect.y);
        let x1 = x0 + i64::from(self.rect.width);
        let y1 = y0 + i64::from(self.rect.height);
        if px < x0 || px >= x1 || py < y0 || py >= y1 {
            return true;
        }
        let r = i64::from(self.radius)
            .min(i64::from(self.rect.width) / 2)
            .min(i64::from(self.rect.height) / 2)
            + 1;
        if r <= 1 {
            return false;
        }
        let left = px < x0 + r;
        let right = px >= x1 - r;
        let top = py < y0 + r;
        let bottom = py >= y1 - r;
        (left || right) && (top || bottom)
    }
}

/// One rounded-rectangle fill or border ring to paint (CTX-0311).
///
/// `frame` is in physical pixels (the caller applies the DPI scale before
/// constructing it), so HiDPI needs no backend-specific work. The corner
/// radius is clamped to half the shorter side. `border == 0` paints the
/// full solid rounded rectangle; `border > 0` paints only the ring between
/// the outer rounded rectangle and the inner rounded rectangle inset by
/// `border` — the CTX-0294 decoration shape, now evaluated as a fragment
/// SDF instead of per-row scanlines. Rounded fills paint after all plain
/// fills and before glyphs, so the ring covers cell backgrounds in the
/// corner boxes while [`Self::inner_clip`] keeps glyphs out of the curve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundedFill {
    /// Outer frame rectangle (physical px).
    pub frame: RectPx,
    /// Ring thickness in physical px; 0 paints the solid rounded rect.
    pub border: u16,
    /// Outer corner radius in physical px, clamped to half the shorter side.
    pub radius: u16,
    /// Straight-alpha fill color.
    pub color: Rgba8,
}

impl RoundedFill {
    /// Resolved outer radius: `radius` clamped to half the shorter side
    /// (oversized radii saturate exactly like the CTX-0294 scanline ring).
    #[must_use]
    pub fn resolved_radius(&self) -> f32 {
        let half = self.frame.width.min(self.frame.height) as f32 * 0.5;
        f32::from(self.radius).min(half)
    }

    /// Analytic coverage of this shape at pixel center `(px, py)`: 1 inside
    /// the painted region, 0 outside, with a one-pixel linear transition at
    /// the outer boundary and, for a ring, at the inner boundary too.
    #[must_use]
    pub fn coverage_at(&self, px: f32, py: f32) -> f32 {
        if self.frame.width == 0 || self.frame.height == 0 {
            return 0.0;
        }
        let half_w = self.frame.width as f32 * 0.5;
        let half_h = self.frame.height as f32 * 0.5;
        let cx = self.frame.x as f32 + half_w;
        let cy = self.frame.y as f32 + half_h;
        let r_out = self.resolved_radius();
        let outer = (0.5 - sd_rounded_box(px - cx, py - cy, half_w, half_h, r_out)).clamp(0.0, 1.0);
        if self.border == 0 {
            return outer;
        }
        let b = f32::from(self.border).min(half_w).min(half_h);
        let r_in = (r_out - b).max(0.0);
        let inner = sd_rounded_box(px - cx, py - cy, half_w - b, half_h - b, r_in);
        outer * (0.5 + inner).clamp(0.0, 1.0)
    }

    /// The content clip implied by this frame (CTX-0311): the frame inset
    /// by `border` with radius `max(clamped radius - border, 0)`.
    ///
    /// `None` when the radius resolves to zero (square frames keep the
    /// documented glyph-overhang behavior — cells never clip glyphs), when
    /// the border consumes the radius (`radius <= border`, square inner
    /// corner), or when the frame or inner rectangle is degenerate.
    #[must_use]
    pub fn inner_clip(&self) -> Option<RoundedClip> {
        rounded_frame_clip(self.frame, self.border, self.radius)
    }
}

/// The content clip implied by a decorated frame (CTX-0311): the frame
/// inset by `border`, corner radius `max(clamp(radius, min(w,h)/2) - border,
/// 0)` (matching [`RoundedFill`]'s inner arc).
///
/// `None` when the radius resolves to zero (square frames keep the
/// documented glyph-overhang behavior — cells never clip glyphs), when the
/// border consumes the radius, or when the frame or inner rectangle is
/// degenerate.
#[must_use]
pub fn rounded_frame_clip(frame: RectPx, border: u16, radius: u16) -> Option<RoundedClip> {
    let span = frame.width.min(frame.height);
    let r_out = u32::from(radius).min(span / 2);
    let b = u32::from(border).min(span);
    if r_out == 0 || r_out <= b {
        return None;
    }
    let r_in = r_out - b;
    let rect = RectPx::new(
        frame.x.saturating_add(b as i32),
        frame.y.saturating_add(b as i32),
        frame.width.saturating_sub(b.saturating_mul(2)),
        frame.height.saturating_sub(b.saturating_mul(2)),
    );
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    Some(RoundedClip {
        rect,
        radius: u16::try_from(r_in).unwrap_or(u16::MAX),
    })
}

/// Core-owned workspace decoration border color (CTX-0294).
///
/// Bitty Dark ANSI 8 (`#585b70`), the designed "dim decorations" role from
/// the theme table: visible against [`DEFAULT_BG`] without competing with
/// cell content. A focused-view accent variant belongs to the render-owner
/// stage-2 lane (CTX-0238g follow-up); this constant keeps the first live
/// wiring theme-token-controlled in one place.
pub const DECORATION_BORDER: Rgba8 = [0x58, 0x5B, 0x70, 0xFF];

/// Where a glyph instance's texels live.
#[derive(Debug, Clone, PartialEq)]
pub enum GlyphSource {
    /// Coverage texels inside the renderer's atlas texture.
    Atlas {
        /// Placed region; `uv` on the instance carries the normalized form.
        slot: AtlasSlot,
    },
    /// Texels carried inline because the bitmap could not fit the atlas
    /// even after a deterministic wholesale eviction. Coverage bytes,
    /// row-major, length `width * height`.
    Inline {
        /// Coverage bytes (one per texel).
        mask: Vec<u8>,
        /// Mask width in texels.
        width: u32,
        /// Mask height in texels.
        height: u32,
    },
}

/// One glyph to composite, fully described in owned data.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphInstance {
    /// Destination top-left in logical pixels (may be negative; bearings).
    pub dest: [i32; 2],
    /// Source size in texels (equals the bitmap dimensions).
    pub size: [u32; 2],
    /// Normalized atlas coordinates `[u0, v0, u1, v1]`; zeros when inline.
    pub uv: [f32; 4],
    /// Tint color (straight alpha).
    pub color: Rgba8,
    /// Optional rounded content clip (CTX-0311). `None` preserves the
    /// documented cell behavior that glyphs may overhang their cell (and,
    /// for decorated frames, that square or undecorated frames never clip).
    pub clip: Option<RoundedClip>,
    /// Texel source.
    pub source: GlyphSource,
}

/// One RGBA image to composite, fully described in owned data (CTX-0248).
///
/// Produced by the runtime present path from placed Kitty images that the
/// rich layer rasterized to the exact destination extent. `rgba` is
/// straight-alpha RGBA8, row-major, exactly
/// `dest.width * dest.height * 4` bytes. Paint order is after fills and
/// glyphs: images are the topmost present-layer content (above cells,
/// text, and fill overlays such as selection/cursor), and they never
/// mutate grid truth. Both CPU compositors (`headless_present` and the
/// `sw-fallback` `draw_list_onto`) blend every entry, and the real-GPU
/// pipeline uploads each entry into an RGBA texture and blits it last
/// (CTX-0291), bounded by `batch::plan_image_uploads`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlit {
    /// Destination top-left in logical pixels plus span.
    pub dest: RectPx,
    /// Straight-alpha RGBA8 bytes, row-major, `dest.area() * 4` long.
    pub rgba: Vec<u8>,
}

impl ImageBlit {
    /// Builds a blit, validating the byte length with checked arithmetic
    /// before accepting the allocation the caller already made.
    ///
    /// # Errors
    ///
    /// [`crate::error::RenderError::InvalidInput`] when the span is zero
    /// or `rgba.len() != dest.width * dest.height * 4` (including
    /// overflow, which can never satisfy the equality).
    pub fn try_new(dest: RectPx, rgba: Vec<u8>) -> Result<Self, crate::error::RenderError> {
        if dest.width == 0 || dest.height == 0 {
            return Err(crate::error::RenderError::InvalidInput {
                reason: "image blit destination must be non-zero",
            });
        }
        let expected = (u64::from(dest.width) * u64::from(dest.height))
            .checked_mul(4)
            .filter(|&n| n <= usize::MAX as u64);
        let ok = expected.is_some_and(|n| n as usize == rgba.len());
        if !ok {
            return Err(crate::error::RenderError::InvalidInput {
                reason: "image blit bytes do not match destination extent",
            });
        }
        Ok(Self { dest, rgba })
    }
}

/// Owned record of everything one frame needs to draw.
///
/// Produced by [`GridRenderer::render`]; consumed by a GPU backend seam or,
/// under the `sw-fallback` feature, by
/// [`crate::software::draw_list_onto`]. Paint order is fills first, then
/// rounded fills, then background images, then overlay fills, then glyphs,
/// then Kitty images; each vector preserves cell scan order so identical
/// inputs give byte-identical records.
#[derive(Debug, Clone, PartialEq)]
pub struct DrawList {
    /// Snapshot generation this list was built from.
    pub generation: u64,
    /// Atlas epoch at which this list's glyph slots were placed (CTX-0531).
    ///
    /// Every glyph slot in this list must resolve against the atlas at exactly
    /// this epoch. If the atlas epoch changes after the list is built (e.g., by
    /// a subsequent `overlay_text_glyphs` call that exhausts and resets the
    /// atlas), the list becomes stale and must be rebuilt to maintain frame
    /// consistency.
    pub atlas_epoch: u64,
    /// The frame plan that drove cell selection.
    pub plan: FramePlan,
    /// Background and decoration rectangles.
    pub fills: Vec<FillRect>,
    /// Rounded decoration fills/rings (CTX-0311), painted after [`Self::fills`]
    /// and before glyphs so the frame ring covers corner cell backgrounds.
    pub rounded_fills: Vec<RoundedFill>,
    /// Background-image blits (CTX-0347): the per-`View` content rectangle
    /// painted after cell backgrounds and the decoration ring, and before
    /// overlay fills and glyphs, so the image sits behind content and the
    /// selection/cursor overlays stay visible on top of it.
    pub backgrounds: Vec<ImageBlit>,
    /// Overlay fills painted above the background image and below glyphs
    /// (CTX-0347: the selection highlight and cursor fill).
    pub overlay_fills: Vec<FillRect>,
    /// Glyph instances.
    pub glyphs: Vec<GlyphInstance>,
    /// RGBA image blits (CTX-0248 Kitty present layer, topmost).
    pub images: Vec<ImageBlit>,
}

impl DrawList {
    /// True when the frame carries any drawing work.
    #[must_use]
    pub fn needs_draw(&self) -> bool {
        !self.fills.is_empty()
            || !self.rounded_fills.is_empty()
            || !self.backgrounds.is_empty()
            || !self.overlay_fills.is_empty()
            || !self.glyphs.is_empty()
            || !self.images.is_empty()
    }

    /// True when this DrawList's atlas slots are still valid for `current_epoch`.
    ///
    /// Returns false when the atlas has been reset since this list was built,
    /// meaning the glyph slots in this list reference stale atlas texels and
    /// the frame must be rebuilt for consistency.
    #[must_use]
    pub fn is_atlas_epoch_valid(&self, current_epoch: u64) -> bool {
        self.atlas_epoch == current_epoch
    }
}

/// Monotone pipeline counters (see the module-level budget discussion).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderCounters {
    /// Frames planned, including clean ones.
    pub frames_planned: u64,
    /// Cells visited because a dirty rectangle covered them.
    pub cells_examined: u64,
    /// Visited cells that emitted a glyph or decoration.
    pub cells_drawn: u64,
    /// Spacer (trailing wide-half) cells skipped.
    pub spacer_cells_skipped: u64,
    /// Cells skipped without drawing (whitespace, rasterizer failures, and
    /// overlay text whose scalar is uncovered). Grid cells with a missing
    /// glyph no longer land here: they paint the tofu box and count as
    /// [`RenderCounters::missing_glyphs`] (CTX-0368).
    pub blank_cells_skipped: u64,
    /// Cells whose character had no covering face; the RFC tofu box was
    /// painted instead (CTX-0368, text-rendering RFC "Missing-glyph
    /// behavior"). Grid cell path only: overlay text skips uncovered
    /// scalars as `blank_cells_skipped` instead.
    pub missing_glyphs: u64,
    /// Cells whose glyph was suppressed by the invisible attribute.
    pub invisible_cells_skipped: u64,
    /// Background rectangles emitted after horizontal run merging: adjacent
    /// visited cells with identical background color and vertical geometry
    /// share one rectangle (CTX-0471), so this tracks color changes rather
    /// than visited cells.
    pub background_fills: u64,
    /// Decoration rectangles emitted (underline/strikethrough bars).
    pub decorations_emitted: u64,
    /// Glyph instances emitted.
    pub glyphs_emitted: u64,
}

/// One queued texture upload: coverage bytes for a freshly allocated slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasUpload {
    /// Destination region inside the atlas texture.
    pub slot: AtlasSlot,
    /// Coverage bytes, row-major, length `slot.width * slot.height`.
    pub data: Vec<u8>,
}

/// Shelf-packed glyph atlas plus its CPU-side coverage texture and upload
/// queue.
///
/// Eviction mirrors [`GlyphCache`] but is **frame consistent** (CTX-0531):
/// [`GlyphAtlas::ensure`] never resets between two placements of one pass.
/// When an allocation no longer fits it records exhaustion and returns
/// [`GlyphSource::Inline`]; the caller rebuilds the whole pass against a
/// wholesale-reset atlas ([`GlyphAtlas::evict_now`], one counted eviction).
/// That way a returned [`DrawList`] never mixes slots from two
/// [`GlyphAtlas::epoch`]s: every emitted atlas slot resolves to the texels
/// of exactly one generation, and anything that still cannot fit degrades
/// to a clean inline fallback. Bitmaps larger than the whole atlas always
/// fall back inline (a reset cannot make them fit). All outcomes are
/// deterministic functions of the insertion sequence.
#[derive(Debug)]
pub struct GlyphAtlas {
    layout: AtlasLayout,
    slots: HashMap<RasterKey, AtlasSlot>,
    texels: Vec<u8>,
    pending: Vec<AtlasUpload>,
    /// Generation of the current placement set, bumped by every wholesale
    /// reset (eviction or explicit [`GlyphAtlas::clear`]). Every emitted
    /// slot belongs to exactly one generation, so a consumer can tell
    /// placements from different generations apart without trusting slot
    /// coordinates.
    epoch: u64,
    /// Set when a placement pass could not fit a glyph and therefore needs
    /// the caller to reset and rebuild before the frame is returned.
    exhausted: bool,
    hits: u64,
    misses: u64,
    evictions: u64,
    inline_fallbacks: u64,
}

impl GlyphAtlas {
    /// Creates an empty square atlas of side `dimension`.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when `dimension` is zero.
    pub fn new(dimension: u16) -> Result<Self, RenderError> {
        Self::with_dims(dimension, dimension)
    }

    /// Creates an empty atlas with explicit dimensions.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero.
    pub fn with_dims(width: u16, height: u16) -> Result<Self, RenderError> {
        let layout = AtlasLayout::new(width, height)?;
        let len = usize::from(width) * usize::from(height);
        Ok(Self {
            layout,
            slots: HashMap::new(),
            texels: vec![0; len],
            pending: Vec::new(),
            epoch: 0,
            exhausted: false,
            hits: 0,
            misses: 0,
            evictions: 0,
            inline_fallbacks: 0,
        })
    }

    /// Generation of the current placement set.
    ///
    /// Starts at `0` and increments on every wholesale reset, so an emitted
    /// slot and the atlas texture it addresses can be compared by generation
    /// instead of by trusting the slot coordinates alone.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// True when a placement pass could not fit a glyph and needs a reset
    /// plus a full rebuild before the frame is returned.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Clears the exhaustion flag so a fresh placement pass starts clean.
    fn clear_exhausted(&mut self) {
        self.exhausted = false;
    }

    /// Captures the cumulative lookup/fallback counters an abandoned
    /// placement pass must roll back (CTX-0531).
    fn cost_snapshot(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.inline_fallbacks)
    }

    /// Restores [`Self::cost_snapshot`] counters after discarding a pass.
    /// Evictions are deliberately excluded: the wholesale reset did happen.
    fn restore_cost(&mut self, (hits, misses, inline_fallbacks): (u64, u64, u64)) {
        self.hits = hits;
        self.misses = misses;
        self.inline_fallbacks = inline_fallbacks;
    }

    /// Resets the atlas wholesale and bumps the placement generation.
    ///
    /// Call only at a placement boundary: any slot already handed to a
    /// [`DrawList`] belongs to the wiped generation and must be discarded
    /// and rebuilt. Cumulative counters are untouched; the exhaustion reset
    /// adds its own eviction count in [`Self::evict_now`].
    fn reset_placements(&mut self) {
        self.layout.reset();
        self.slots.clear();
        self.texels.fill(0);
        self.pending.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Applies a wholesale exhaustion reset and its counted eviction.
    ///
    /// Called by the frame pipeline between passes (CTX-0531), never inside
    /// one, so no already-emitted slot is invalidated mid-pass.
    fn evict_now(&mut self) {
        self.reset_placements();
        self.exhausted = false;
        self.evictions = self.evictions.saturating_add(1);
    }

    /// True when `width` x `height` can never fit this atlas, regardless of
    /// how full it is (zero span or larger than the atlas itself).
    fn oversized(&self, width: u16, height: u16) -> bool {
        let dims = self.layout.dimensions();
        width == 0 || height == 0 || width > dims.width || height > dims.height
    }

    /// Atlas texture dimensions.
    #[must_use]
    pub const fn dims(&self) -> AtlasDims {
        self.layout.dimensions()
    }

    /// Read-only view of the coverage texture (one byte per texel).
    #[must_use]
    pub fn texels(&self) -> &[u8] {
        &self.texels
    }

    /// Number of placements currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True when no placement is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Fraction of atlas texels covered by issued slots.
    #[must_use]
    pub fn occupancy(&self) -> f64 {
        self.layout.occupancy()
    }

    /// Cumulative lookups served by an existing placement.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// Cumulative lookups that had to place (and queue an upload for) a new
    /// bitmap.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// Cumulative wholesale resets triggered by exhaustion.
    #[must_use]
    pub const fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Cumulative bitmaps that could not fit even after an eviction.
    #[must_use]
    pub const fn inline_fallbacks(&self) -> u64 {
        self.inline_fallbacks
    }

    /// Returns the slot previously issued for `key`, if any.
    #[must_use]
    pub fn slot(&self, key: &RasterKey) -> Option<AtlasSlot> {
        self.slots.get(key).copied()
    }

    /// Drains the pending upload queue (a backend copies these into its own
    /// texture, then drops them).
    pub fn take_pending_uploads(&mut self) -> Vec<AtlasUpload> {
        std::mem::take(&mut self.pending)
    }

    /// Drops all placements and texels while keeping the atlas dimensions.
    ///
    /// Cumulative hits/misses/evictions/inline-fallback counters are
    /// preserved. Call after the rasterized size changes (for example a DPI
    /// rescale): slots hold previous-scale coverage, so they must not survive
    /// alongside new-scale placements. Unlike exhaustion recovery (which
    /// counts an eviction), this explicit invalidation leaves the cumulative
    /// counters untouched.
    pub fn clear(&mut self) {
        self.reset_placements();
    }

    /// Ensures `bitmap` is placed for `key`, queueing an upload when newly
    /// placed. Blank bitmaps are refused by callers before reaching the
    /// atlas.
    ///
    /// Never resets mid-pass (CTX-0531): a bitmap that does not currently
    /// fit degrades to [`GlyphSource::Inline`] and marks the atlas
    /// [`Self::is_exhausted`] so the caller can reset and rebuild. The
    /// glyph is deliberately *not* cached in `slots` on that path, so a
    /// rebuild re-attempts placement against the fresh generation. Bitmaps
    /// larger than the whole atlas fall back inline without marking
    /// exhaustion (a reset cannot make them fit).
    pub fn ensure(&mut self, key: RasterKey, bitmap: &GlyphBitmap) -> GlyphSource {
        if let Some(slot) = self.slots.get(&key) {
            self.hits += 1;
            return GlyphSource::Atlas { slot: *slot };
        }
        self.misses += 1;

        let width = u16::try_from(bitmap.metrics.width.max(0));
        let height = u16::try_from(bitmap.metrics.height.max(0));
        let (width, height) = match (width, height) {
            (Ok(w), Ok(h)) => (w, h),
            _ => return self.fallback_inline(bitmap),
        };
        if self.oversized(width, height) {
            return self.fallback_inline(bitmap);
        }

        let Some(slot) = self.layout.allocate(width, height) else {
            // Exhaustion (not oversize): flag for a caller-driven reset and
            // rebuild; this pass keeps a clean inline fallback meanwhile.
            self.exhausted = true;
            return self.fallback_inline(bitmap);
        };

        let coverage = coverage_mask(bitmap);
        write_slot_texels(&mut self.texels, self.layout.dimensions(), slot, &coverage);
        self.pending.push(AtlasUpload {
            slot,
            data: coverage,
        });
        self.slots.insert(key, slot);
        GlyphSource::Atlas { slot }
    }

    fn fallback_inline(&mut self, bitmap: &GlyphBitmap) -> GlyphSource {
        self.inline_fallbacks += 1;
        GlyphSource::Inline {
            width: saturating_u32(u64::try_from(bitmap.metrics.width.max(0)).unwrap_or(u64::MAX)),
            height: saturating_u32(u64::try_from(bitmap.metrics.height.max(0)).unwrap_or(u64::MAX)),
            mask: coverage_mask(bitmap),
        }
    }
}

/// Flattens any supported bitmap format to one coverage byte per texel.
///
/// RGB coverage is averaged to luminance (identical rule to
/// [`crate::software::SurfaceRgba::blend_glyph`]); RGBA sources contribute
/// their alpha channel (upstream RGBA is premultiplied, so alpha *is*
/// coverage).
fn coverage_mask(bitmap: &GlyphBitmap) -> Vec<u8> {
    let GlyphMetrics { width, height, .. } = bitmap.metrics;
    let mut mask = Vec::with_capacity(width.max(0) as usize * height.max(0) as usize);
    match bitmap.format {
        BitmapFormat::Rgb => {
            for px in bitmap.data.chunks_exact(3) {
                let lum = (u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3;
                mask.push(lum as u8);
            }
        }
        BitmapFormat::Rgba => {
            for px in bitmap.data.chunks_exact(4) {
                mask.push(px[3]);
            }
        }
    }
    mask
}

/// Writes a coverage mask into the atlas texture at `slot`.
fn write_slot_texels(texels: &mut [u8], dims: AtlasDims, slot: AtlasSlot, mask: &[u8]) {
    let stride = usize::from(dims.width);
    let slot_w = usize::from(slot.width);
    for row in 0..usize::from(slot.height) {
        let src = row * slot_w;
        let dst = (usize::from(slot.y) + row) * stride + usize::from(slot.x);
        texels[dst..dst + slot_w].copy_from_slice(&mask[src..src + slot_w]);
    }
}

/// Result of a DPI rescale: the metrics subsequent frames rasterize at.
///
/// Returned by
/// [`GridRenderer::apply_dpi_scale`](GridRenderer::apply_dpi_scale) so the
/// embedder can derive the grid from physical pixels over [`cell`](Self::cell)
/// and reconfigure the surface extent without re-deriving the math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedDpiScale {
    /// Sanitized scale that was applied (see [`crate::hidpi::sanitize_dpi_scale`]).
    pub scale: f64,
    /// Cell metrics subsequent frames place glyphs at.
    pub cell: CellMetrics,
    /// Point size subsequent glyphs rasterize at.
    pub point_size: f32,
}

/// Renders terminal snapshots through the shared pipeline: plan, place,
/// cache, emit.
///
/// Generic over the rasterizer so headless tests drive the identical path
/// the crossfont-backed renderer uses (terminal-state-rfc testing strategy:
/// the software fallback must exercise the production pipeline, not a
/// parallel one).
#[derive(Debug)]
pub struct GridRenderer<R: GlyphRasterizer> {
    cache: GlyphCache<R>,
    atlas: GlyphAtlas,
    font: FontId,
    point_size: f32,
    cell: CellMetrics,
    /// Resolved terminal palette (CTX-0355) used for default cell colors,
    /// ANSI 0–15, and the fill colors the planner emits. Defaults to the
    /// designed Bitty Dark preset; embedders override it with
    /// [`GridRenderer::set_theme_palette`].
    palette: ThemePalette,
    /// Pen-baseline offset below the row top (see [`resolve_baseline_offset`]):
    /// metric-aware when the rasterizer measures the face, legacy 3/4 rule
    /// otherwise. Glyph instances keep their full bitmap size at
    /// `baseline - metrics.top`, so tall glyphs overhang into adjacent
    /// padding instead of being clipped to the cell.
    baseline_offset: i64,
    counters: RenderCounters,
}

impl<R: GlyphRasterizer> GridRenderer<R> {
    /// Builds a renderer around `rasterizer`, loading `query` and using the
    /// default atlas dimension.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] for invalid queries or zero cell
    /// metrics; whatever the rasterizer reports for font loading.
    pub fn new(rasterizer: R, query: &FontQuery, cell: CellMetrics) -> Result<Self, RenderError> {
        Self::with_atlas_dimension(rasterizer, query, cell, DEFAULT_ATLAS_DIMENSION)
    }

    /// As [`GridRenderer::new`] with an explicit atlas side length (tests
    /// use small values to exercise eviction deterministically).
    ///
    /// # Errors
    ///
    /// Same as [`GridRenderer::new`], plus invalid atlas dimensions.
    pub fn with_atlas_dimension(
        rasterizer: R,
        query: &FontQuery,
        cell: CellMetrics,
        dimension: u16,
    ) -> Result<Self, RenderError> {
        query.validate()?;
        let mut cache = GlyphCache::new(rasterizer, crate::cache::DEFAULT_GLYPH_CACHE_CAPACITY)?;
        let font = cache.load_font(query)?;
        // Face measurement is best-effort: backends without metrics (or a
        // failed measurement) resolve to the legacy fallback, never an error.
        let measured = cache
            .rasterizer()
            .font_metrics(font, query.point_size)
            .ok()
            .flatten();
        let baseline_offset = resolve_baseline_offset(cell.height, measured);
        Ok(Self {
            cache,
            atlas: GlyphAtlas::new(dimension)?,
            font,
            point_size: query.point_size,
            cell,
            palette: ThemePalette::default(),
            baseline_offset,
            counters: RenderCounters::default(),
        })
    }

    /// The resolved terminal palette used by [`Self::render`].
    #[must_use]
    pub const fn theme_palette(&self) -> ThemePalette {
        self.palette
    }

    /// Replaces the resolved terminal palette (CTX-0355).
    ///
    /// This is the renderer-side seam for `appearance.theme`: the app layer
    /// resolves the preset once and installs it, so default cell colors,
    /// ANSI 0–15, and emitted fills follow the selected preset instead of
    /// the built-in Bitty Dark default.
    pub fn set_theme_palette(&mut self, palette: ThemePalette) {
        self.palette = palette;
    }

    /// The configured cell metrics.
    #[must_use]
    pub const fn cell_metrics(&self) -> CellMetrics {
        self.cell
    }

    /// Pen-baseline offset below the row top (metric-aware when measured,
    /// legacy 3/4 rule otherwise; see [`resolve_baseline_offset`]).
    #[must_use]
    pub const fn baseline_offset(&self) -> i64 {
        self.baseline_offset
    }

    /// Applies a DPI scale change so atlas rasterization matches the scaled cell.
    ///
    /// Recomputes cell metrics and point size from `base_cell`/`base_query`
    /// at `scale` (see [`crate::hidpi`]), reloads the font at the scaled
    /// size, and invalidates the glyph cache plus atlas so subsequent frames
    /// rasterize at the new size. Cumulative cache/atlas counters are
    /// preserved; only stale entries and previous-scale texels are dropped.
    ///
    /// Call on DPI scale changes after reconfiguring the surface to physical
    /// pixels, then derive the grid from physical pixels over the returned
    /// [`AppliedDpiScale::cell`] so the per-frame NDC factor stays near 1.0
    /// instead of magnifying 1x texels (soft text) or leaving a stale small
    /// surface for the compositor to upscale (blurry text).
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the scaled query is invalid (only
    /// reachable from an invalid `base_query`, since scaling stays total);
    /// whatever the rasterizer reports when the scaled font fails to load.
    /// On error the renderer is unchanged: the font is loaded before any
    /// field is updated or any cache is cleared.
    pub fn apply_dpi_scale(
        &mut self,
        base_cell: CellMetrics,
        base_query: &FontQuery,
        scale: f64,
    ) -> Result<AppliedDpiScale, RenderError> {
        let sanitized = crate::hidpi::sanitize_dpi_scale(scale);
        let cell = crate::hidpi::scaled_cell_metrics(base_cell, sanitized);
        let point_size = crate::hidpi::scaled_point_size(base_query.point_size, sanitized);
        let scaled_query = FontQuery {
            family: base_query.family.clone(),
            style: base_query.style.clone(),
            point_size,
        };
        scaled_query.validate()?;
        let font = self.cache.load_font(&scaled_query)?;
        let measured = self
            .cache
            .rasterizer()
            .font_metrics(font, point_size)
            .ok()
            .flatten();
        self.cell = cell;
        self.point_size = point_size;
        self.font = font;
        self.baseline_offset = resolve_baseline_offset(cell.height, measured);
        self.cache.clear();
        self.atlas.clear();
        Ok(AppliedDpiScale {
            scale: sanitized,
            cell,
            point_size,
        })
    }

    /// Point-in-time copy of the renderer-side counters.
    #[must_use]
    pub const fn counters(&self) -> RenderCounters {
        self.counters
    }

    /// Glyph-cache `(hits, misses)` totals ("glyphs rasterized" is exactly
    /// the miss count).
    #[must_use]
    pub const fn cache_stats(&self) -> (u64, u64) {
        (self.cache.hits(), self.cache.misses())
    }

    /// Atlas `(hits, misses, evictions, inline_fallbacks)` totals.
    #[must_use]
    pub const fn atlas_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.atlas.hits(),
            self.atlas.misses(),
            self.atlas.evictions(),
            self.atlas.inline_fallbacks(),
        )
    }

    /// Current atlas placement epoch.
    ///
    /// The epoch increments on every wholesale atlas reset (eviction or DPI
    /// rescale). Used to detect when a [`DrawList`] has become stale due to
    /// atlas eviction after it was built.
    #[must_use]
    pub const fn atlas_epoch(&self) -> u64 {
        self.atlas.epoch()
    }

    /// Number of glyph placements currently held by the atlas.
    #[must_use]
    pub fn atlas_placements(&self) -> usize {
        self.atlas.len()
    }

    /// Drainable atlas upload queue for the backend seam. Draining does not
    /// invalidate the CPU-side texture ([`Self::atlas_texels`] stays
    /// complete), so software compositing never depends on the drain.
    pub fn take_atlas_uploads(&mut self) -> Vec<AtlasUpload> {
        self.atlas.take_pending_uploads()
    }

    /// Read-only atlas texture (coverage bytes) for software compositing.
    #[must_use]
    pub fn atlas_texels(&self) -> &[u8] {
        self.atlas.texels()
    }

    /// Atlas texture dimensions.
    #[must_use]
    pub const fn atlas_dims(&self) -> AtlasDims {
        self.atlas.dims()
    }

    /// Plans and records one frame from `snapshot` limited by `damage`.
    ///
    /// Deterministic: identical `(snapshot, damage, font, insertion order)`
    /// inputs yield identical [`DrawList`] values — ordering follows the
    /// plan's dirty rectangles then row-major cell scan, background runs
    /// merge only within one dirty-rectangle row, floats come only from
    /// integer slot divisions, and no timing or randomness participates.
    /// Errors mean the frame failed outright; partial frames are never
    /// returned.
    ///
    /// Frame-consistent atlas placement (CTX-0531): a placement pass that
    /// exhausts the atlas abandons its already-emitted slots, the atlas
    /// resets wholesale, and a second bounded pass places the same cells
    /// against the fresh [`GlyphAtlas::epoch`] with further eviction
    /// disabled. Every glyph in the returned list therefore either samples
    /// the current texture generation or carries a clean inline fallback;
    /// none can address a stale slot. At most one eviction reset and two
    /// passes occur per frame.
    ///
    /// # Errors
    ///
    /// Propagates rasterizer failures (never cached by [`GlyphCache`]).
    pub fn render(
        &mut self,
        snapshot: &Snapshot,
        damage: &Damage,
    ) -> Result<DrawList, RenderError> {
        self.counters.frames_planned += 1;

        let descriptor = SnapshotDamage::new(snapshot, damage, self.cell);
        let plan = plan_frame(&descriptor);

        let list = DrawList {
            generation: snapshot.generation,
            atlas_epoch: self.atlas.epoch(),
            fills: Vec::new(),
            // Rounded decoration is composed by the runtime present path
            // (CTX-0311); grid truth carries no rounded geometry.
            rounded_fills: Vec::new(),
            // Background images and overlay fills are composed by the runtime
            // present path (CTX-0347); grid truth carries neither.
            backgrounds: Vec::new(),
            overlay_fills: Vec::new(),
            glyphs: Vec::new(),
            // Grid truth carries no images; the runtime present path pushes
            // placed Kitty blits onto the combined list (CTX-0248).
            images: Vec::new(),
            plan,
        };
        if !list.plan.needs_draw() {
            return Ok(list);
        }

        // Coalesced dirty rectangles are pairwise disjoint (frame.rs
        // invariant), so their cell ranges never overlap and each cell is
        // visited at most once, in row-major order per rectangle. Clone them
        // so cells can be placed while the list is being built (plan output
        // is tiny and bounded).
        let cols = snapshot.width;
        let dirty_rects = list.plan.dirty_rects.clone();

        // Bounded frame-consistent pass (CTX-0531). Pass 1 may observe
        // exhaustion: its partial list is discarded, the atlas is reset
        // wholesale (one counted eviction), and pass 2 re-places every cell
        // against the fresh generation. Pass 2 never resets: whatever still
        // does not fit keeps its inline fallback, so the returned list has
        // slots from exactly one generation.
        let counters_before = self.counters;
        let atlas_costs_before = self.atlas.cost_snapshot();
        let mut pass = self.place_pass(snapshot, cols, &dirty_rects, &list.plan);
        if self.atlas.is_exhausted() {
            // Discard pass 1: roll back its renderer/atlas cost counters so
            // the caller sees only the work that produced the returned list.
            // The eviction counter is intentionally kept (the reset really
            // happened).
            self.counters = counters_before;
            self.atlas.restore_cost(atlas_costs_before);
            self.atlas.evict_now();
            pass = self.place_pass(snapshot, cols, &dirty_rects, &list.plan);
        }
        Ok(pass)
    }

    /// Runs one bounded placement pass over `dirty_rects`.
    ///
    /// Extracted from [`Self::render`] so a frame-consistent rebuild can
    /// discard the partial pass and repeat it against a fresh atlas
    /// generation (CTX-0531). Clears the atlas exhaustion flag on entry so
    /// the pass reports only its own outcome.
    fn place_pass(
        &mut self,
        snapshot: &Snapshot,
        cols: usize,
        dirty_rects: &[RectPx],
        plan: &FramePlan,
    ) -> DrawList {
        self.atlas.clear_exhausted();
        let mut pass = DrawList {
            generation: snapshot.generation,
            atlas_epoch: self.atlas.epoch(),
            fills: Vec::new(),
            rounded_fills: Vec::new(),
            backgrounds: Vec::new(),
            overlay_fills: Vec::new(),
            glyphs: Vec::new(),
            images: Vec::new(),
            plan: plan.clone(),
        };
        for dirty in dirty_rects {
            let col_range = pixel_span_to_cells(dirty.x, dirty.width, self.cell.width);
            let row_range = pixel_span_to_cells(dirty.y, dirty.height, self.cell.height);
            for row in row_range {
                // Background runs restart per dirty-rectangle row: a merged
                // span must never bridge columns the plan did not visit.
                let mut background_run = BackgroundRun::default();
                for col in col_range.clone() {
                    let Some(term_cell) = snapshot.cells.get(row * cols + col) else {
                        // Defensive against malformed snapshots; skipping a
                        // nonexistent cell can only under-*draw* a region
                        // that cannot exist, never corrupt real cells.
                        continue;
                    };
                    self.place_cell(term_cell, row, col, cols, &mut pass, &mut background_run);
                }
            }
        }
        pass
    }

    /// Emits background, decorations, and (unless suppressed) one glyph for
    /// a single visited cell. The cell's background merges into the row's
    /// open same-color run (CTX-0471) instead of emitting a quad per cell.
    fn place_cell(
        &mut self,
        term_cell: &bitty_term_state::Cell,
        row: usize,
        col: usize,
        grid_width: usize,
        list: &mut DrawList,
        background_run: &mut BackgroundRun,
    ) {
        self.counters.cells_examined += 1;

        let (fg, bg) = resolved_colors_in(&self.palette, &term_cell.style);
        let left = u64::try_from(col)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.width));
        let top = u64::try_from(row)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.height));

        // Span in columns: wide cells cover their spacer column too, while
        // spacers paint only their own half (the leading half already
        // covers both). The span clamps at the grid edge so a trailing wide
        // cell cannot paint past the extent (defensive; term-state forbids
        // orphan spacers).
        let span_cols = if term_cell.spacer {
            1
        } else {
            usize::from(term_cell.width).max(1).min(grid_width - col)
        };

        // Every visited cell repaints its background: the union of
        // incremental frames equals a full redraw (module docs). Adjacent
        // same-color cells extend one rectangle so the quad count follows
        // color changes, not cell count (CTX-0471).
        let background = FillRect {
            rect: RectPx::new(
                saturating_i32(left),
                saturating_i32(top),
                saturating_u32(
                    u64::try_from(span_cols)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(u64::from(self.cell.width)),
                ),
                self.cell.height,
            ),
            color: bg,
        };
        if background_run.merge(&mut list.fills, background) {
            self.counters.background_fills += 1;
        }

        if term_cell.spacer {
            self.counters.spacer_cells_skipped += 1;
            return;
        }

        let decorations = self.emit_decorations(term_cell, row, col, span_cols, fg, list);
        if term_cell.style.attributes.invisible {
            self.counters.invisible_cells_skipped += 1;
        } else if term_cell.glyph != ' ' {
            self.emit_glyph(term_cell.glyph, row, col, span_cols, fg, list);
        } else if decorations == 0 {
            self.counters.blank_cells_skipped += 1;
            return;
        }
        self.counters.cells_drawn += 1;
    }

    /// Emits underline/strikethrough bars; returns how many were pushed.
    fn emit_decorations(
        &mut self,
        term_cell: &bitty_term_state::Cell,
        row: usize,
        col: usize,
        span_cols: usize,
        fg: Rgba8,
        list: &mut DrawList,
    ) -> u64 {
        let thickness = underline_thickness(self.cell.height);
        let left = u64::try_from(col)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.width));
        let top = u64::try_from(row)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.height));
        let span_w = saturating_u32(
            u64::try_from(span_cols)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::from(self.cell.width)),
        );
        let underline_color =
            resolve_color_in(&self.palette, term_cell.style.underline_color.as_ref(), fg);

        // CTX-0583 (#1145): each SGR 4:x style paints its own deterministic
        // shape. `Single`/`Double` keep the pre-CTX-0583 bars exactly;
        // `Curly`/`Dotted`/`Dashed` decompose into bounded [`FillRect`] runs
        // because every backend (GPU present, headless RGBA, sw-fallback)
        // paints rectangles only. Geometry is a function of the cell's
        // absolute top-left, span, cell height, and thickness: no state,
        // no randomness, and every rectangle stays inside the cell.
        let mut pushed = push_underline_pattern(
            &mut list.fills,
            term_cell.style.attributes.underline,
            UnderlinePlacement {
                left: saturating_i32(left),
                top: saturating_i32(top),
                span_w,
                cell_height: self.cell.height,
                thickness,
                color: underline_color,
            },
        );

        if term_cell.style.attributes.strikethrough {
            let mid = top + u64::from(self.cell.height) / 2;
            let y = mid.saturating_sub(u64::from(thickness) / 2);
            list.fills.push(FillRect {
                rect: RectPx::new(saturating_i32(left), saturating_i32(y), span_w, thickness),
                color: fg,
            });
            pushed += 1;
        }

        self.counters.decorations_emitted += pushed;
        pushed
    }

    /// Looks up, caches, atlas-uploads, and emits one glyph instance.
    ///
    /// When the rasterizer reports no drawable representation (no loaded face
    /// covers the scalar), the cell paints the RFC missing-glyph tofu box
    /// instead of staying blank (CTX-0368).
    fn emit_glyph(
        &mut self,
        character: char,
        row: usize,
        col: usize,
        span_cols: usize,
        color: Rgba8,
        list: &mut DrawList,
    ) {
        let Ok(key) = RasterKey::new(character, self.font, self.point_size) else {
            // The point size was validated at construction, so this is
            // unreachable; treating it as "nothing to draw" stays safe.
            self.counters.blank_cells_skipped += 1;
            return;
        };
        let bitmap = match self.cache.glyph(key) {
            Ok(CachedGlyph::Bitmap(bitmap)) => bitmap,
            Ok(CachedGlyph::Blank) => {
                // The rasterizer contract answers `Ok(None)` exactly when no
                // loaded face covers the scalar; paint the visible RFC tofu
                // box and count it so missing glyphs never read as blank.
                self.emit_missing_glyph(row, col, span_cols, color, list);
                return;
            }
            Err(_) => {
                // Rasterizer failure: skip this glyph instead of failing
                // the whole frame. The failure stays observable through
                // GlyphCache miss counters and upstream diagnostics.
                self.counters.blank_cells_skipped += 1;
                return;
            }
        };

        let metrics = bitmap.metrics;
        let source = self.atlas.ensure(key, bitmap);
        let dest_x = col as i64 * i64::from(self.cell.width) + i64::from(metrics.left);
        let baseline = row as i64 * i64::from(self.cell.height) + self.baseline_offset;
        let dest_y = baseline - i64::from(metrics.top);

        let instance = match source {
            GlyphSource::Atlas { slot } => GlyphInstance {
                dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                size: [
                    saturating_u32(u64::try_from(metrics.width.max(0)).unwrap_or(u64::MAX)),
                    saturating_u32(u64::try_from(metrics.height.max(0)).unwrap_or(u64::MAX)),
                ],
                uv: slot.uv(self.atlas.dims()),
                color,
                // Cell glyphs overhang by construction; only the runtime's
                // decorated frames set a rounded content clip (CTX-0311).
                clip: None,
                source: GlyphSource::Atlas { slot },
            },
            GlyphSource::Inline {
                mask,
                width,
                height,
            } => GlyphInstance {
                dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                size: [width, height],
                uv: [0.0; 4],
                color,
                clip: None,
                source: GlyphSource::Inline {
                    mask,
                    width,
                    height,
                },
            },
        };
        list.glyphs.push(instance);
        self.counters.glyphs_emitted += 1;
    }

    /// Paints the RFC "Missing-glyph behavior" tofu box (CTX-0368).
    ///
    /// A visible `1 px` outline at the cluster's cell extent in the cell
    /// foreground: four fill rectangles (top, bottom, left, right) sized by
    /// `span_cols` so a wide scalar's tofu spans both grid columns. Never
    /// allocates an atlas slot and never mutates terminal truth — the cell
    /// keeps its scalar, so selection/copy/search are unaffected. The
    /// outline is the v1 missing indicator from the text-rendering RFC; the
    /// `U+XXXX` hex fallback inside the box is explicitly not painted.
    fn emit_missing_glyph(
        &mut self,
        row: usize,
        col: usize,
        span_cols: usize,
        color: Rgba8,
        list: &mut DrawList,
    ) {
        self.counters.missing_glyphs += 1;
        let left = u64::try_from(col)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.width));
        let top = u64::try_from(row)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.cell.height));
        let width = saturating_u32(
            u64::try_from(span_cols)
                .unwrap_or(u64::MAX)
                .saturating_mul(u64::from(self.cell.width)),
        );
        let height = self.cell.height;
        let x = saturating_i32(left);
        let y = saturating_i32(top);
        let right = x.saturating_add(saturating_i32(u64::from(width)).saturating_sub(1));
        let bottom = y.saturating_add(saturating_i32(u64::from(height)).saturating_sub(1));
        for rect in [
            RectPx::new(x, y, width, 1),
            RectPx::new(x, bottom, width, 1),
            RectPx::new(x, y, 1, height),
            RectPx::new(right, y, 1, height),
        ] {
            list.fills.push(FillRect { rect, color });
        }
    }

    /// Rasterizes one line of overlay text at an absolute pixel origin.
    ///
    /// Presentation-only embedder primitive (CTX-0186 pending-paste banner;
    /// CTX-0367 inline IME preedit): the pen advances by each character's
    /// terminal cell width ([`bitty_term_state::char_cell_width`]) — wide
    /// CJK/emoji consume two columns like grid text, zero-width marks stay
    /// at the current column — and stops at `max_cells` columns. Glyph
    /// lookup, caching, and atlas placement mirror [`Self::render`]'s cell
    /// path (same font, same baseline rule), so headless and GPU composites
    /// sample identical texels. Whitespace, rasterizer failures, and
    /// uncovered scalars are skipped as `blank_cells_skipped`; unlike the
    /// cell path, which paints the CTX-0368 tofu box for a missing glyph,
    /// the overlay keeps the skip behavior and never paints tofu. The
    /// caller owns the background fill. Returns the glyph instances to push
    /// (possibly empty). Bounded: at most `max_cells` iterations and
    /// instances, no I/O, no panics.
    #[must_use]
    pub fn overlay_text_glyphs(
        &mut self,
        text: &str,
        origin_px: (i32, i32),
        max_cells: usize,
        color: Rgba8,
    ) -> Vec<GlyphInstance> {
        if max_cells == 0 {
            return Vec::new();
        }
        let (origin_x, origin_y) = (i64::from(origin_px.0), i64::from(origin_px.1));
        let baseline = origin_y + self.baseline_offset;

        // Bounded frame-consistent pass (CTX-0531), mirroring `render`: a
        // pass that exhausts the atlas discards its partial vector, resets
        // the atlas, and repeats against the fresh generation, so the
        // returned instances never mix atlas generations.
        let counters_before = self.counters;
        let atlas_costs_before = self.atlas.cost_snapshot();
        let (mut out, exhausted) = self.overlay_line(text, origin_x, baseline, max_cells, color);
        if exhausted {
            self.counters = counters_before;
            self.atlas.restore_cost(atlas_costs_before);
            self.atlas.evict_now();
            // The final pass may legitimately re-flag exhaustion (a line whose
            // glyphs exceed one fresh generation); its inline fallbacks are
            // the correct terminal outcome, and the next pass clears the flag
            // on entry.
            out = self
                .overlay_line(text, origin_x, baseline, max_cells, color)
                .0;
        }
        out
    }

    /// Runs one bounded overlay-text placement pass (CTX-0531 extraction).
    ///
    /// Returns the instances and whether the atlas reported exhaustion. A
    /// glyph that could not fit keeps its inline fallback in this pass; the
    /// caller resets and rebuilds when `exhausted` is true.
    fn overlay_line(
        &mut self,
        text: &str,
        origin_x: i64,
        baseline: i64,
        max_cells: usize,
        color: Rgba8,
    ) -> (Vec<GlyphInstance>, bool) {
        self.atlas.clear_exhausted();
        let mut out = Vec::new();
        let mut col = 0usize;
        for character in text.chars().take(max_cells) {
            let advance = usize::from(char_cell_width(character));
            if col + advance > max_cells {
                break;
            }
            if character == ' ' {
                col += advance;
                continue;
            }
            let Ok(key) = RasterKey::new(character, self.font, self.point_size) else {
                self.counters.blank_cells_skipped += 1;
                continue;
            };
            let bitmap = match self.cache.glyph(key) {
                Ok(CachedGlyph::Bitmap(bitmap)) => bitmap,
                Ok(CachedGlyph::Blank) => {
                    self.counters.blank_cells_skipped += 1;
                    continue;
                }
                Err(_) => {
                    self.counters.blank_cells_skipped += 1;
                    continue;
                }
            };
            let metrics = bitmap.metrics;
            let source = self.atlas.ensure(key, bitmap);
            let dest_x =
                origin_x + col as i64 * i64::from(self.cell.width) + i64::from(metrics.left);
            let dest_y = baseline - i64::from(metrics.top);
            let instance = match source {
                GlyphSource::Atlas { slot } => GlyphInstance {
                    dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                    size: [
                        saturating_u32(u64::try_from(metrics.width.max(0)).unwrap_or(u64::MAX)),
                        saturating_u32(u64::try_from(metrics.height.max(0)).unwrap_or(u64::MAX)),
                    ],
                    uv: slot.uv(self.atlas.dims()),
                    color,
                    clip: None,
                    source: GlyphSource::Atlas { slot },
                },
                GlyphSource::Inline {
                    mask,
                    width,
                    height,
                } => GlyphInstance {
                    dest: [clamp_i32(dest_x), clamp_i32(dest_y)],
                    size: [width, height],
                    uv: [0.0; 4],
                    color,
                    clip: None,
                    source: GlyphSource::Inline {
                        mask,
                        width,
                        height,
                    },
                },
            };
            out.push(instance);
            self.counters.glyphs_emitted += 1;
            col += advance;
        }
        (out, self.atlas.is_exhausted())
    }
}

/// Converts a pixel span back to the half-open range of cells it covers.
///
/// Dirty rectangles are cell-aligned by construction (grid damage
/// rectangles scale by the same cell metrics that built the extent), so
/// ranges are exact; the dividing ceiling tolerates foreign descriptors.
fn pixel_span_to_cells(offset: i32, span: u32, cell_side: u32) -> std::ops::Range<usize> {
    let side = i64::from(cell_side);
    let start = i64::from(offset).max(0) / side;
    let covered = i64::from(offset).max(0) + i64::from(span);
    let end = (covered + side - 1) / side;
    let to_usize = |v: i64| usize::try_from(v).unwrap_or(usize::MAX);
    to_usize(start)..to_usize(end)
}

#[cfg(feature = "sw-fallback")]
mod sw {
    //! Headless end-to-end entry point: the SAME plan/place/cache pipeline,
    //! composited onto a [`SurfaceRgba`] by the software backend.

    use bitty_term_state::{Damage, Snapshot};

    use super::{CellMetrics, DrawList, GridRenderer, Rgba8};
    use crate::error::RenderError;
    use crate::glyph::GlyphRasterizer;
    use crate::software::{SurfaceRgba, draw_list_onto};

    /// Pixel extent of the surface covering a whole snapshot.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the resulting extent exceeds the
    /// software surface byte cap or a dimension collapses to zero.
    pub fn surface_extent(
        snapshot: &Snapshot,
        cell: CellMetrics,
    ) -> Result<(u32, u32), RenderError> {
        let extent = cell.extent_for(snapshot.width, snapshot.height);
        if extent.is_zero() {
            return Err(RenderError::InvalidInput {
                reason: "snapshot grid collapses to an empty surface",
            });
        }
        let bytes = u64::from(extent.width) * u64::from(extent.height) * 4;
        if bytes > crate::software::MAX_SURFACE_BYTES as u64 {
            return Err(RenderError::InvalidInput {
                reason: "snapshot surface exceeds the configured byte cap",
            });
        }
        Ok((extent.width, extent.height))
    }

    /// Composites one frame's [`DrawList`] onto `surface` using the
    /// renderer's live atlas texture.
    ///
    /// # Errors
    ///
    /// Propagates compositing failures from [`draw_list_onto`].
    pub fn composite_frame<R: GlyphRasterizer>(
        renderer: &GridRenderer<R>,
        list: &DrawList,
        surface: &mut SurfaceRgba,
    ) -> Result<(), RenderError> {
        draw_list_onto(
            list,
            Some((renderer.atlas_texels(), renderer.atlas_dims())),
            surface,
        )
    }

    /// Convenience for tests and tools: renders `snapshot`/`damage` through
    /// the full pipeline and returns a freshly cleared surface holding only
    /// this frame's damage. Incremental consumers should keep one surface
    /// across frames and call [`composite_frame`] per frame instead.
    ///
    /// # Errors
    ///
    /// Propagates rendering and compositing failures.
    pub fn render_snapshot_to_surface<R: GlyphRasterizer>(
        renderer: &mut GridRenderer<R>,
        snapshot: &Snapshot,
        damage: &Damage,
        background: Rgba8,
    ) -> Result<SurfaceRgba, RenderError> {
        let (width, height) = {
            let cell = renderer.cell_metrics();
            // Borrow of renderer ends here; render below needs &mut.
            let (w, h) = surface_extent(snapshot, cell)?;
            (w, h)
        };
        let list = renderer.render(snapshot, damage)?;
        let mut surface = SurfaceRgba::try_new(width, height)?;
        surface.clear(background);
        composite_frame(renderer, &list, &mut surface)?;
        Ok(surface)
    }
}

#[cfg(feature = "sw-fallback")]
pub use sw::{composite_frame, render_snapshot_to_surface, surface_extent};

#[cfg(test)]
mod tests;

/// Clamps an `i64` coordinate back into `i32` range (destinations may be
/// negative from glyph bearings; extremes stay overflow-free).
const fn clamp_i32(value: i64) -> i32 {
    const MAX: i64 = i32::MAX as i64;
    const MIN: i64 = i32::MIN as i64;
    if value > MAX {
        i32::MAX
    } else if value < MIN {
        i32::MIN
    } else {
        value as i32
    }
}
