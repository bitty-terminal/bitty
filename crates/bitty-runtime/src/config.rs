//! Owned configuration for the `bitty-runtime` orchestration.
//!
//! Configuration is declarative, total, and validated eagerly. No file I/O,
//! Lua, or style inference occurs here; the values are already resolved
//! Rust primitives. The grid dimensions remain logical (columns x rows);
//! pixel dimensions are derived from [`cell_width`]/[`cell_height`] when
//! mapping to [`bitty_platform::PhysicalSize`] and surface extents.
//!
//! The initial grid size defaults to [`bitty_term_state::GRID_COLUMNS`] x
//! [`bitty_term_state::GRID_ROWS`] (80x24) because terminal state has not
//! yet implemented resize reflow (the singular reflow algorithm deferred
//! under the terminal-state-rfc open items). Resizes before that lands only
//! reconfigure the GPU/software surface and the PTY window size, not the
//! grid memory itself — documented honestly in [`crate::Runtime`].

use crate::error::RuntimeError;

/// Default lines scrolled per wheel notch (CTX-0185).
/// Mirrors `bitty-config` `DEFAULT_SCROLL_LINES_PER_NOTCH` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SCROLL_LINES_PER_NOTCH: u32 = 3;

/// Maximum lines per wheel notch (matches the per-frame scroll cap).
pub const MAX_SCROLL_LINES_PER_NOTCH: u32 = 32;

/// Default smooth-scroll pixels per wheel notch (CTX-0185).
/// Mirrors `bitty-config` `DEFAULT_SCROLL_PIXELS_PER_NOTCH` (see above).
pub const DEFAULT_SCROLL_PIXELS_PER_NOTCH: u32 = 16;

/// Maximum smooth-scroll pixels per wheel notch.
pub const MAX_SCROLL_PIXELS_PER_NOTCH: u32 = 256;

/// Default selection auto-copy behavior (CTX-0191).
/// Mirrors `bitty-config` `DEFAULT_SELECTION_AUTO_COPY` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SELECTION_AUTO_COPY: bool = true;

/// Default inner panel gap in cells (CTX-0177).
/// Mirrors `bitty-config` `DEFAULT_LAYOUT_GAPS_IN` (kept local for the same
/// no-dependency reason; paired by value and pinned by a `bitty-app` test).
pub const DEFAULT_LAYOUT_GAPS_IN: u16 = 0;

/// Default outer panel gap in cells (CTX-0177).
/// Mirrors `bitty-config` `DEFAULT_LAYOUT_GAPS_OUT` (see above).
pub const DEFAULT_LAYOUT_GAPS_OUT: u16 = 0;

/// Maximum panel gap in cells (either axis, CTX-0177).
/// Mirrors `bitty-config` `MAX_LAYOUT_GAP_CELLS` (see above).
pub const MAX_LAYOUT_GAP_CELLS: u16 = 16;

/// Default window padding in logical pixels (CTX-0223).
/// Mirrors `bitty-config` `WindowConfig` default (`padding: 8`; kept as a
/// local constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_WINDOW_PADDING: u32 = 8;

/// Maximum window padding in logical pixels (CTX-0223).
/// Mirrors `bitty-config` `WindowConfig::validate` (`must be <= 64`;
/// see above).
pub const MAX_WINDOW_PADDING: u32 = 64;

/// Default scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `DEFAULT_SCROLLBAR_WIDTH` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SCROLLBAR_WIDTH: u32 = 8;

/// Minimum scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `MIN_SCROLLBAR_WIDTH_PX` (see above).
pub const MIN_SCROLLBAR_WIDTH_PX: u32 = 1;

/// Maximum scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `MAX_SCROLLBAR_WIDTH_PX` (see above).
pub const MAX_SCROLLBAR_WIDTH_PX: u32 = 32;

/// Owned runtime configuration, validated on construction.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    /// Terminal width in columns.
    pub cols: usize,
    /// Terminal height in rows.
    pub rows: usize,
    /// Cell width in physical pixels.
    ///
    /// Default `9` (CTX-0157): legacy `8` plus `1px` letter-spacing breathing
    /// room. Measured `JetBrainsMono Nerd Font` at 12pt advances `9.6px`;
    /// ghostty uses pure font metrics, kitty 11pt advances ~`8.8px` — `9`
    /// is the conservative readable default (see `bitty-config` `FontConfig`
    /// docs for the full ghostty/kitty table).
    pub cell_width: u32,
    /// Cell height in physical pixels.
    ///
    /// Default `19` (CTX-0157): `round(16 * 1.2)` line-height breathing room.
    /// True JetBrains Mono line at 12pt is `21.1px`; `19` sits between legacy
    /// `16` and true `21`, matching kitty 11pt line (~`19.4px`).
    pub cell_height: u32,
    /// Capacity of the bounded cold-path event queue.
    pub cold_queue_capacity: usize,
    /// Font family used by the renderer; whitespace-trimmed on validation.
    ///
    /// Default [`crate::font_default_family`] (`JetBrainsMono Nerd Font`):
    /// Nerd-patched for starship/opencode glyphs, matching the CTX-0157
    /// ghostty side-by-side acceptance at 12pt. System `monospace` remains
    /// the ultimate fallback (see `bitty-config` `FONT_FALLBACK_CHAIN`).
    pub font_family: String,
    /// Font point size; must be finite and within `(0, 3999]`.
    pub font_size: f32,
    /// Lines scrolled per wheel notch, `1..=32` (CTX-0185; default 3).
    /// Applied to `Lines` deltas directly and to `Pixels` deltas via the
    /// notch equivalence (`scroll_pixels_per_notch` px = one notch).
    /// Direction semantics are unchanged (positive = up into history).
    pub scroll_lines_per_notch: u32,
    /// Smooth-scroll pixels per wheel notch, `1..=256` (CTX-0185; default 16).
    pub scroll_pixels_per_notch: u32,
    /// Whether a committed mouse selection auto-copies to the clipboard
    /// (CTX-0191; default `true` = ghostty-class copy-on-select).
    /// `false` leaves the highlight in place; the explicit
    /// `copy_to_clipboard` chord (Ctrl+Shift+C) still copies.
    pub selection_auto_copy: bool,
    /// Spacing between sibling panes in cells (CTX-0177 `layout.gaps_in`).
    /// `0..=MAX_LAYOUT_GAP_CELLS`; default `0` = edge-to-edge tiling.
    /// The gap band shows the window background; per-leaf rendering and
    /// hit-testing exclude it.
    pub gaps_in: u16,
    /// Inset around the container edge in cells (CTX-0177
    /// `layout.gaps_out`). Same bounds and default as `gaps_in`.
    pub gaps_out: u16,
    /// Window padding in logical pixels on every side (CTX-0223
    /// `window.padding`). `0..=MAX_WINDOW_PADDING`; default
    /// `DEFAULT_WINDOW_PADDING` (`8`, ghostty/alacritty-class breathing
    /// room). The padding band shows the window background; the grid is
    /// translated by the inset origin and grid derivation subtracts twice
    /// the padding before dividing by the cell metrics, so the window —
    /// not the grid — absorbs the inset.
    pub window_padding: u32,
    /// Overlay scrollbar display mode (CTX-0181 `scrollbar.mode`).
    /// Default `Hidden` = geometry-neutral (zero pixels, zero layout delta).
    /// `Always` paints the thumb whenever scrollback exists; `Auto` reveals
    /// it on mouse proximity/hover/drag.
    pub scrollbar_mode: bitty_ui::ScrollbarMode,
    /// Overlay scrollbar thumb width in logical pixels (CTX-0181
    /// `scrollbar.width`). `MIN_SCROLLBAR_WIDTH_PX..=MAX_SCROLLBAR_WIDTH_PX`;
    /// default `DEFAULT_SCROLLBAR_WIDTH` (`8`). Scaled by the live DPI
    /// factor exactly like `window_padding`; the track lives inside the
    /// leaf allocation so the grid never absorbs it.
    pub scrollbar_width: u32,
}

/// Default font family (CTX-0157 acceptance probe).
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Returns the default font family.
#[must_use]
pub fn font_default_family() -> String {
    DEFAULT_FONT_FAMILY.to_string()
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            cols: bitty_term_state::GRID_COLUMNS,
            rows: bitty_term_state::GRID_ROWS,
            cell_width: 9,
            cell_height: 19,
            cold_queue_capacity: 256,
            font_family: font_default_family(),
            font_size: 12.0,
            scroll_lines_per_notch: DEFAULT_SCROLL_LINES_PER_NOTCH,
            scroll_pixels_per_notch: DEFAULT_SCROLL_PIXELS_PER_NOTCH,
            selection_auto_copy: DEFAULT_SELECTION_AUTO_COPY,
            gaps_in: DEFAULT_LAYOUT_GAPS_IN,
            gaps_out: DEFAULT_LAYOUT_GAPS_OUT,
            window_padding: DEFAULT_WINDOW_PADDING,
            scrollbar_mode: bitty_ui::ScrollbarMode::Hidden,
            scrollbar_width: DEFAULT_SCROLLBAR_WIDTH,
        }
    }
}

impl RuntimeConfig {
    /// Validates and builds a config. All fields are checked for
    /// total, deterministic construction.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when any field is outside its
    /// documented range.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cols: usize,
        rows: usize,
        cell_width: u32,
        cell_height: u32,
        cold_queue_capacity: usize,
        font_family: impl Into<String>,
        font_size: f32,
        scroll_lines_per_notch: u32,
        scroll_pixels_per_notch: u32,
        selection_auto_copy: bool,
        gaps_in: u16,
        gaps_out: u16,
        window_padding: u32,
        scrollbar_mode: bitty_ui::ScrollbarMode,
        scrollbar_width: u32,
    ) -> Result<Self, RuntimeError> {
        let font_family = font_family.into();
        let cfg = Self {
            cols,
            rows,
            cell_width,
            cell_height,
            cold_queue_capacity,
            font_family,
            font_size,
            scroll_lines_per_notch,
            scroll_pixels_per_notch,
            selection_auto_copy,
            gaps_in,
            gaps_out,
            window_padding,
            scrollbar_mode,
            scrollbar_width,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validates an already constructed config.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when any field violates its contract.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.cols == 0 || self.rows == 0 {
            return Err(RuntimeError::InvalidConfig("cols and rows must be >= 1"));
        }
        if self.cell_width == 0 || self.cell_height == 0 {
            return Err(RuntimeError::InvalidConfig(
                "cell_width and cell_height must be >= 1",
            ));
        }
        if self.cold_queue_capacity == 0 {
            return Err(RuntimeError::InvalidQueueCapacity);
        }
        if self.font_family.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("font_family must not be empty"));
        }
        if !(self.font_size.is_finite() && self.font_size > 0.0 && self.font_size <= 3999.0) {
            return Err(RuntimeError::InvalidConfig(
                "font_size must be finite within (0, 3999]",
            ));
        }
        if !(1..=MAX_SCROLL_LINES_PER_NOTCH).contains(&self.scroll_lines_per_notch) {
            return Err(RuntimeError::InvalidConfig(
                "scroll_lines_per_notch must be within [1, 32]",
            ));
        }
        if !(1..=MAX_SCROLL_PIXELS_PER_NOTCH).contains(&self.scroll_pixels_per_notch) {
            return Err(RuntimeError::InvalidConfig(
                "scroll_pixels_per_notch must be within [1, 256]",
            ));
        }
        if self.cols > 1000 || self.rows > 1000 {
            return Err(RuntimeError::InvalidConfig(
                "grid dimensions must be <= 1000",
            ));
        }
        if self.gaps_in > MAX_LAYOUT_GAP_CELLS || self.gaps_out > MAX_LAYOUT_GAP_CELLS {
            return Err(RuntimeError::InvalidConfig(
                "layout gaps must be within [0, 16] cells",
            ));
        }
        if self.window_padding > MAX_WINDOW_PADDING {
            return Err(RuntimeError::InvalidConfig(
                "window_padding must be within [0, 64] logical pixels",
            ));
        }
        if !(MIN_SCROLLBAR_WIDTH_PX..=MAX_SCROLLBAR_WIDTH_PX).contains(&self.scrollbar_width) {
            return Err(RuntimeError::InvalidConfig(
                "scrollbar_width must be within [1, 32] logical pixels",
            ));
        }
        Ok(())
    }

    /// Pixel extent for the current grid geometry, excluding window padding.
    ///
    /// Unchanged by CTX-0223: this stays the grid-space extent so existing
    /// geometry (grid derivation round-trips, resize math) keeps its
    /// meaning. Use [`Self::window_extent`] for the surface/window size.
    #[must_use]
    pub fn pixel_extent(&self) -> bitty_platform::PhysicalSize {
        let w = u64::from(self.cell_width) * self.cols as u64;
        let h = u64::from(self.cell_height) * self.rows as u64;
        let w = if w > u32::MAX as u64 {
            u32::MAX
        } else {
            w as u32
        };
        let h = if h > u32::MAX as u64 {
            u32::MAX
        } else {
            h as u32
        };
        bitty_platform::PhysicalSize::new(w, h)
    }

    /// Derives `cols`/`rows` from a physical size using the configured cell
    /// metrics, saturating to at least 1x1.
    #[must_use]
    pub fn grid_from_pixels(&self, size: bitty_platform::PhysicalSize) -> (usize, usize) {
        let cols = (size.width() / self.cell_width).max(1) as usize;
        let rows = (size.height() / self.cell_height).max(1) as usize;
        (cols.min(1000), rows.min(1000))
    }

    /// Window (surface) extent: grid pixels plus the padding inset on every
    /// side (CTX-0223). Saturates instead of overflowing on hostile inputs.
    /// At scale 1.0 logical pixels equal physical pixels, so this is the
    /// extent [`crate::Runtime`] configures its surface with; HiDPI callers
    /// scale the padding via the live scale factor first (see
    /// [`crate::Runtime::window_padding_physical`]).
    #[must_use]
    pub fn window_extent(&self) -> bitty_platform::PhysicalSize {
        let grid = self.pixel_extent();
        let inset = u64::from(self.window_padding.min(MAX_WINDOW_PADDING)).saturating_mul(2);
        let w = u64::from(grid.width()).saturating_add(inset);
        let h = u64::from(grid.height()).saturating_add(inset);
        bitty_platform::PhysicalSize::new(
            w.min(u32::MAX as u64) as u32,
            h.min(u32::MAX as u64) as u32,
        )
    }

    /// Derives `cols`/`rows` from a window (surface) size by first removing
    /// the padding inset on every side, then dividing by the cell metrics
    /// (CTX-0223). Saturates to at least 1x1; a padding that covers the
    /// window still addresses one cell (the caller keeps the previous
    /// geometry when the inset leaves no drawable content).
    #[must_use]
    pub fn grid_from_window_pixels(&self, size: bitty_platform::PhysicalSize) -> (usize, usize) {
        let inset = self
            .window_padding
            .min(MAX_WINDOW_PADDING)
            .saturating_mul(2);
        let content = bitty_platform::PhysicalSize::new(
            size.width().saturating_sub(inset),
            size.height().saturating_sub(inset),
        );
        self.grid_from_pixels(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        RuntimeConfig::default()
            .validate()
            .expect("default must be valid");
    }

    #[test]
    fn pixel_extent_derivation() {
        let cfg = RuntimeConfig::default();
        let extent = cfg.pixel_extent();
        // 80x24 at breathing-room 9x19 (CTX-0157).
        assert_eq!(extent.width(), 720);
        assert_eq!(extent.height(), 456);
    }

    #[test]
    fn grid_from_pixels_rounds_down_and_clamps() {
        let cfg = RuntimeConfig::default();
        let size = bitty_platform::PhysicalSize::new(100, 100);
        let (cols, rows) = cfg.grid_from_pixels(size);
        assert_eq!(cols, 11);
        assert_eq!(rows, 5);
        let zero = bitty_platform::PhysicalSize::new(0, 0);
        let (c, r) = cfg.grid_from_pixels(zero);
        assert_eq!(c, 1);
        assert_eq!(r, 1);
    }

    #[test]
    fn invalid_fields_are_rejected() {
        assert!(
            RuntimeConfig::new(
                0,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                0,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                0,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "   ",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                0.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn scroll_speed_fields_are_rejected_out_of_range() {
        // CTX-0185: scroll speed is validated fail-closed like other config.
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                0,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                33,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                3,
                0,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                3,
                257,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        RuntimeConfig::new(
            80,
            24,
            8,
            16,
            256,
            "mono",
            12.0,
            1,
            1,
            true,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("scroll speed boundaries must be valid");
        RuntimeConfig::new(
            80,
            24,
            8,
            16,
            256,
            "mono",
            12.0,
            32,
            256,
            false,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("scroll speed boundaries must be valid");
    }

    #[test]
    fn selection_auto_copy_defaults_on_and_accepts_both() {
        // CTX-0191: default-on preserves copy-on-select; both values build.
        const { assert!(DEFAULT_SELECTION_AUTO_COPY) }
        assert!(RuntimeConfig::default().selection_auto_copy);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("auto-copy on builds");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            false,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("auto-copy off builds");
    }

    #[test]
    fn scrollbar_defaults_hidden_and_validates_bounds() {
        // CTX-0181: hidden-by-default keeps geometry neutral; width bounds
        // fail closed (mirrors `bitty-config` bounds, pinned in `bitty-app`).
        const { assert!(DEFAULT_SCROLLBAR_WIDTH == 8) }
        const { assert!(MIN_SCROLLBAR_WIDTH_PX == 1) }
        const { assert!(MAX_SCROLLBAR_WIDTH_PX == 32) }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.scrollbar_mode, bitty_ui::ScrollbarMode::Hidden);
        assert_eq!(cfg.scrollbar_width, DEFAULT_SCROLLBAR_WIDTH);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Auto,
            12,
        )
        .expect("auto scrollbar builds");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                0,
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                33,
            )
            .is_err()
        );
    }

    #[test]
    fn default_matches_readable_cell_and_nerd_font() {
        let cfg = RuntimeConfig::default();
        assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
        assert_eq!(cfg.font_family, "JetBrainsMono Nerd Font");
        assert!((cfg.font_size - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn layout_gaps_default_zero_and_validate_bounds() {
        // CTX-0177: defaults preserve edge-to-edge tiling; bounds fail closed.
        const { assert!(DEFAULT_LAYOUT_GAPS_IN == 0) }
        const { assert!(DEFAULT_LAYOUT_GAPS_OUT == 0) }
        const { assert!(MAX_LAYOUT_GAP_CELLS == 16) }
        let cfg = RuntimeConfig::default();
        assert_eq!((cfg.gaps_in, cfg.gaps_out), (0, 0));
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero gaps build");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            16,
            16,
            8,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("max gaps build");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                17,
                0,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                17,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                u16::MAX,
                u16::MAX,
                8,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn window_padding_default_and_bounds() {
        // CTX-0223: default 8px mirrors `bitty-config` `WindowConfig`
        // (pinned by value in `bitty-app`); bounds fail closed.
        const { assert!(DEFAULT_WINDOW_PADDING == 8) }
        const { assert!(MAX_WINDOW_PADDING == 64) }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.window_padding, DEFAULT_WINDOW_PADDING);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            0,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero padding builds");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            64,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("max padding builds");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                65,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                u32::MAX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn window_extent_adds_padding_around_grid() {
        // 80x24 at 9x19 = 720x456 grid; default 8px padding => 736x472 window.
        let cfg = RuntimeConfig::default();
        assert_eq!(
            cfg.pixel_extent(),
            bitty_platform::PhysicalSize::new(720, 456)
        );
        assert_eq!(
            cfg.window_extent(),
            bitty_platform::PhysicalSize::new(736, 472)
        );
        let bare = RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            0,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero padding builds");
        assert_eq!(bare.window_extent(), bare.pixel_extent());
    }

    #[test]
    fn grid_from_window_pixels_removes_padding_first() {
        let cfg = RuntimeConfig::default();
        // 736x472 window minus 2x8px padding = 720x456 grid = 80x24 cells.
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(736, 472)),
            (80, 24)
        );
        // Resize math: 800x600 window => (800-16)/9 x (600-16)/19 = 87x30.
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(800, 600)),
            (87, 30)
        );
        // Padding-covered windows still address one cell (fail-soft floor).
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(10, 10)),
            (1, 1)
        );
    }
}
