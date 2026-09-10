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

/// Default Core-owned workspace decoration gaps (CTX-0292, accepted spec
/// CTX-0118): `gaps_in` 4 logical px, `gaps_out` 6 logical px.
pub const DEFAULT_DECORATION_GAPS_IN_PX: u32 = 4;

/// Default outer workspace decoration gap (CTX-0292): 6 logical px.
pub const DEFAULT_DECORATION_GAPS_OUT_PX: u32 = 6;

/// Default View frame border thickness (CTX-0292): 2 logical px.
pub const DEFAULT_DECORATION_BORDER_PX: u32 = 2;

/// Default View frame corner radius (CTX-0292): 6 logical px.
pub const DEFAULT_DECORATION_RADIUS_PX: u32 = 6;

/// Maximum decoration gap in logical px (either axis), accepted CTX-0118.
pub const MAX_DECORATION_GAP_PX: u32 = 32;

/// Maximum View frame border thickness in logical px, accepted CTX-0118.
pub const MAX_DECORATION_BORDER_PX: u32 = 8;

/// Maximum View frame corner radius in logical px, accepted CTX-0118.
pub const MAX_DECORATION_RADIUS_PX: u32 = 16;

/// Safe-mode decoration gaps (CTX-0292 rule 5: `bitty --safe` = `0/0/1/0`).
pub const SAFE_DECORATION_GAPS_IN_PX: u32 = 0;

/// Safe-mode decoration outer gap; see [`SAFE_DECORATION_GAPS_IN_PX`].
pub const SAFE_DECORATION_GAPS_OUT_PX: u32 = 0;

/// Safe-mode View frame border thickness (`1`, not the `2` default).
pub const SAFE_DECORATION_BORDER_PX: u32 = 1;

/// Safe-mode View frame corner radius (`0`, not the `6` default).
pub const SAFE_DECORATION_RADIUS_PX: u32 = 0;

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

/// Default scrollbar mode (CTX-0181): `hidden`.
///
/// Hidden-by-default keeps the grid geometry-neutral for existing users:
/// no track, no thumb, zero fills, zero layout delta. `always` paints the
/// overlay thumb whenever scrollback exists; `auto` reveals it on mouse
/// proximity/hover/drag like modern terminals.
pub const DEFAULT_SCROLLBAR_MODE: &str = "hidden";

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

/// Braille/symbols fallback family (CTX-0163, issue #263).
///
/// `fc-query` evidence on the reference host: `DejaVu Sans Mono` covers
/// `2500-262f` (box drawing + block elements `U+2580-U+259F`) but has no
/// `28xx` row, so braille patterns `U+2800-U+28FF` (btop CPU graphs) fall
/// through; `Noto Sans Symbols 2` covers `2800-28ff` and resolves via
/// fontconfig (`fc-match "Noto Sans Symbols 2"`). Ships in `noto-fonts`,
/// already an `optdepend` in `packaging/PKGBUILD` — no new dependency.
pub const SYMBOLS_FALLBACK_FAMILY: &str = "Noto Sans Symbols 2";

/// Documented monospace/Nerd fallback stack.
///
/// Order: configured primary (Nerd-patched by default) -> unpatched
/// `JetBrains Mono` -> system `monospace` (fontconfig/WC) ->
/// `DejaVu Sans Mono` (widely available, covers box drawing + block
/// elements `U+2580-U+259F`) -> [`SYMBOLS_FALLBACK_FAMILY`] (covers braille
/// patterns `U+2800-U+28FF` for TUI graphs such as btop). Mirrors ghostty
/// (embedded JetBrains Mono variable + symbols-only Nerd fallback, always
/// present) and kitty (`font_family = "monospace"` + builtin Nerd font,
/// `set_font_family(..., add_builtin_nerd_font=True)`).
///
/// Per-glyph coverage fallback is implemented by
/// `bitty-render::fallback::FallbackRasterizer`, which walks this chain on
/// a missing glyph (reference: ghostty
/// `src/font/CodepointResolver.zig`, per-codepoint fallback via discovery).
/// Full shaping stays deferred to the text RFC (ADR-0004 "Wrap" row); this
/// chain is family-level attempt order for embedders: try each in order
/// until `load_font` succeeds, ending in headless.
/// [`FontConfig::fallback_chain`] builds the configured-first variant.
pub const FONT_FALLBACK_CHAIN: [&str; 5] = [
    DEFAULT_FONT_FAMILY,
    "JetBrains Mono",
    "monospace",
    "DejaVu Sans Mono",
    SYMBOLS_FALLBACK_FAMILY,
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
/// Implements the accepted workspace-compositor contract
/// (`bitty-docs/docs/specifications/workspace-compositor.md`, section
/// "Core-owned gaps, border, and radius", accepted via CTX-0118):
///
/// | Property   | Default | Range      |
/// | ---------- | ------- | ---------- |
/// | `gaps_in`  | 4 px    | 0..=32 px  |
/// | `gaps_out` | 6 px    | 0..=32 px  |
/// | `border`   | 2 px    | 0..=8 px   |
/// | `radius`   | 6 px    | 0..=16 px  |
///
/// Values are integers in logical pixels, scaled by the `Window` DPI factor
/// only at render time. Unknown keys or out-of-range values fail validation
/// with a source-attributed diagnostic; Core never falls back to a silent
/// default when validation fails. Decoration is Core-owned: it is never part
/// of a `LayoutTree`, a `View`, or a `LayoutProvider` proposal.
///
/// This is distinct from the CTX-0177 `layout.gaps_in`/`gaps_out` panel gaps,
/// which remain integer **cells** and keep their existing behavior.
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
}

impl Default for DecorationConfig {
    fn default() -> Self {
        Self {
            gaps_in: DEFAULT_DECORATION_GAPS_IN_PX,
            gaps_out: DEFAULT_DECORATION_GAPS_OUT_PX,
            border: DEFAULT_DECORATION_BORDER_PX,
            radius: DEFAULT_DECORATION_RADIUS_PX,
        }
    }
}

impl DecorationConfig {
    /// Safe-mode decoration (`bitty --safe`, spec rule 5): `0/0/1/0`
    /// regardless of user configuration.
    #[must_use]
    pub const fn safe() -> Self {
        Self {
            gaps_in: SAFE_DECORATION_GAPS_IN_PX,
            gaps_out: SAFE_DECORATION_GAPS_OUT_PX,
            border: SAFE_DECORATION_BORDER_PX,
            radius: SAFE_DECORATION_RADIUS_PX,
        }
    }

    /// True when every decoration is zero (undecorated fast path).
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.gaps_in == 0 && self.gaps_out == 0 && self.border == 0 && self.radius == 0
    }

    /// Validate decoration config (fail-closed on out-of-range values).
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
        Ok(())
    }
}

/// Scrollbar display mode (CTX-0181).
///
/// Mirrors `bitty-ui`'s mode by value (`bitty-config` owns no workspace
/// dependencies, so the pairing is by string, pinned by a `bitty-app`
/// cross-crate test): `hidden` (default, geometry-neutral), `always`
/// (overlay thumb whenever scrollback exists), `auto` (revealed on mouse
/// proximity/hover/drag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    /// Never painted (default; zero pixels, zero geometry delta).
    #[default]
    Hidden,
    /// Painted whenever scrollback exists.
    Always,
    /// Painted only while engaged (hover/proximity/drag).
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
/// defaulting to hidden/`8` when the table is present but omits them, so
/// `scrollbar = {}` keeps the geometry-neutral default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarConfig {
    /// Display mode; default [`ScrollbarMode::Hidden`].
    pub mode: ScrollbarMode,
    /// Thumb width in logical pixels, `1..=MAX_SCROLLBAR_WIDTH_PX`.
    pub width: u32,
}

impl Default for ScrollbarConfig {
    fn default() -> Self {
        Self {
            mode: ScrollbarMode::Hidden,
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
/// `focus_follows_mouse` controls Hyprland-like hover focus: `false`
/// (default) preserves click-to-focus (hover never moves keyboard focus);
/// `true` moves keyboard focus to the hovered pane. Set via `init.lua`
/// `mouse = { focus_follows_mouse = true }` (key optional, defaulting to
/// `false` when the table is present but omits it, so `mouse = {}` keeps
/// click-to-focus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseConfig {
    /// Whether hover moves keyboard focus to the hovered pane.
    pub focus_follows_mouse: bool,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            focus_follows_mouse: DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE,
        }
    }
}

impl MouseConfig {
    /// Validate mouse config (booleans are total; always succeeds).
    pub fn validate(&self) -> Result<(), ConfigError> {
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
    /// Layout config (CTX-0177 panel gaps in cells; default edge-to-edge).
    pub layout: LayoutConfig,
    /// Core-owned workspace decoration in logical px (CTX-0292; accepted
    /// spec CTX-0118 defaults 4/6/2/6).
    pub decoration: DecorationConfig,
    /// Scrollbar config (CTX-0181 overlay scrollbar; default hidden).
    pub scrollbar: ScrollbarConfig,
    /// Mouse config (CTX-0260 focus-follows-mouse; default off).
    pub mouse: MouseConfig,
    /// Appearance config (theme defaults to `None` if unset).
    pub appearance: AppearanceConfig,
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
            layout: LayoutConfig::default(),
            decoration: DecorationConfig::default(),
            scrollbar: ScrollbarConfig::default(),
            mouse: MouseConfig::default(),
            appearance: AppearanceConfig::default(),
            mod_key: ModKey::default(),
            keymaps: Vec::new(),
            plugins: Vec::new(),
            profile: None,
            schema_version: crate::migration::CURRENT_SCHEMA_VERSION,
        }
    }
}

impl EffectiveConfig {
    /// Validate all fields of the effective config.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.font.validate()?;
        self.window.validate()?;
        self.terminal.validate()?;
        self.selection.validate()?;
        self.layout.validate()?;
        self.decoration.validate()?;
        self.scrollbar.validate()?;
        self.mouse.validate()?;
        self.appearance.validate()?;
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
        assert_eq!(
            chain,
            vec![
                "JetBrainsMono Nerd Font".to_string(),
                "JetBrains Mono".to_string(),
                "monospace".to_string(),
                "DejaVu Sans Mono".to_string(),
                "Noto Sans Symbols 2".to_string(),
            ]
        );
        // Custom primary stays first, chain dedups case-insensitively.
        let custom = FontConfig {
            family: "monospace".into(),
            ..Default::default()
        };
        let chain = custom.fallback_chain();
        assert_eq!(chain[0], "monospace");
        assert_eq!(chain.len(), 5);
        // No duplicates when primary already in chain.
        let nerd = FontConfig {
            family: "  jetbrainsmono nerd font  ".into(),
            ..Default::default()
        };
        let chain = nerd.fallback_chain();
        assert_eq!(chain.len(), 5);
    }

    #[test]
    fn font_fallback_chain_covers_tui_graph_slices() {
        // CTX-0163 (issue #263): btop CPU graphs draw braille patterns
        // (`U+2800-U+28FF`); block graphs use `U+2580-U+259F`. The chain
        // must end in a braille-capable symbols face and keep the
        // block-capable `DejaVu Sans Mono` entry ahead of it, so a
        // per-glyph fallback walk (see `bitty-render::fallback`) can
        // resolve both slices on bare installs without the Nerd font.
        assert_eq!(
            FONT_FALLBACK_CHAIN[FONT_FALLBACK_CHAIN.len() - 1],
            SYMBOLS_FALLBACK_FAMILY
        );
        assert_eq!(SYMBOLS_FALLBACK_FAMILY, "Noto Sans Symbols 2");
        assert!(FONT_FALLBACK_CHAIN.contains(&"DejaVu Sans Mono"));
        // The symbols tail survives a custom primary (dedup only removes
        // the primary itself, never the tail).
        let custom = FontConfig {
            family: "My Mono".into(),
            ..Default::default()
        };
        let chain = custom.fallback_chain();
        assert_eq!(chain[chain.len() - 1], "Noto Sans Symbols 2");
        assert!(chain.contains(&"DejaVu Sans Mono".to_string()));
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
    fn scrollbar_defaults_hidden_and_validates_bounds() {
        // CTX-0181: hidden-by-default keeps grid geometry-neutral; width
        // bounds fail closed.
        const { assert!(DEFAULT_SCROLLBAR_WIDTH == 8) }
        const { assert!(MIN_SCROLLBAR_WIDTH_PX == 1) }
        const { assert!(MAX_SCROLLBAR_WIDTH_PX == 32) }
        assert_eq!(DEFAULT_SCROLLBAR_MODE, "hidden");
        let d = ScrollbarConfig::default();
        assert_eq!(d.mode, ScrollbarMode::Hidden);
        assert_eq!(d.width, DEFAULT_SCROLLBAR_WIDTH);
        d.validate().expect("default valid");
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
        assert!(!MouseConfig::default().focus_follows_mouse);
        assert!(!EffectiveConfig::default().mouse.focus_follows_mouse);
        MouseConfig::default().validate().expect("default valid");
        MouseConfig {
            focus_follows_mouse: true,
        }
        .validate()
        .expect("opt-in valid");
        EffectiveConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn effective_validate_calls_every_section_validator() {
        // CTX-0303: commit 117381b (CTX-0292) silently dropped
        // `self.mouse.validate()?;` from EffectiveConfig::validate. The call is
        // currently behavior-neutral (MouseConfig::validate is total and
        // ConfigPlan::validate still validates mouse), so pin it structurally
        // by scanning production source only. The `#[cfg(test)]` region is
        // excluded so this test cannot satisfy itself.
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
    fn decoration_defaults_match_accepted_spec_and_validate() {
        // CTX-0292 / accepted spec CTX-0118: defaults 4/6/2/6 logical px and
        // ranges gaps 0..=32, border 0..=8, radius 0..=16, fail closed.
        const { assert!(DEFAULT_DECORATION_GAPS_IN_PX == 4) }
        const { assert!(DEFAULT_DECORATION_GAPS_OUT_PX == 6) }
        const { assert!(DEFAULT_DECORATION_BORDER_PX == 2) }
        const { assert!(DEFAULT_DECORATION_RADIUS_PX == 6) }
        const { assert!(MAX_DECORATION_GAP_PX == 32) }
        const { assert!(MAX_DECORATION_BORDER_PX == 8) }
        const { assert!(MAX_DECORATION_RADIUS_PX == 16) }
        let d = DecorationConfig::default();
        assert_eq!((d.gaps_in, d.gaps_out, d.border, d.radius), (4, 6, 2, 6));
        d.validate().expect("default valid");
        assert!(!d.is_zero());
        // Safe-mode inversion: 0/0/1/0 regardless of the defaults.
        let safe = DecorationConfig::safe();
        assert_eq!(
            (safe.gaps_in, safe.gaps_out, safe.border, safe.radius),
            (0, 0, 1, 0)
        );
        safe.validate().expect("safe valid");
        assert!(!safe.is_zero());
        for good in [
            DecorationConfig {
                gaps_in: 0,
                gaps_out: 0,
                border: 0,
                radius: 0,
            },
            DecorationConfig {
                gaps_in: 32,
                gaps_out: 32,
                border: 8,
                radius: 16,
            },
        ] {
            good.validate().expect("boundary decoration must be valid");
        }
        for (field, bad) in [
            ("decoration.gaps_in", 33),
            ("decoration.gaps_out", 33),
            ("decoration.border", 9),
            ("decoration.radius", 17),
        ] {
            let mut c = DecorationConfig::default();
            match field {
                "decoration.gaps_in" => c.gaps_in = bad,
                "decoration.gaps_out" => c.gaps_out = bad,
                "decoration.border" => c.border = bad,
                _ => c.radius = bad,
            }
            let err = c.validate().expect_err("out-of-range must fail closed");
            assert_eq!(err.field(), Some(field), "wrong field for {field}");
        }
        // Effective-level validation covers decoration too.
        let mut eff = EffectiveConfig::default();
        eff.decoration.radius = MAX_DECORATION_RADIUS_PX + 1;
        eff.validate()
            .expect_err("effective must reject oversized decoration");
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
}
