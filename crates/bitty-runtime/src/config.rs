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
        Ok(())
    }

    /// Pixel extent for the current grid geometry.
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
        assert!(RuntimeConfig::new(0, 24, 9, 19, 256, "mono", 12.0, 3, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 0, 19, 256, "mono", 12.0, 3, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 9, 19, 0, "mono", 12.0, 3, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 9, 19, 256, "   ", 12.0, 3, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 9, 19, 256, "mono", 0.0, 3, 16).is_err());
    }

    #[test]
    fn scroll_speed_fields_are_rejected_out_of_range() {
        // CTX-0185: scroll speed is validated fail-closed like other config.
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 0, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 33, 16).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 3, 0).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 3, 257).is_err());
        RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 1, 1)
            .expect("scroll speed boundaries must be valid");
        RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 12.0, 32, 256)
            .expect("scroll speed boundaries must be valid");
    }

    #[test]
    fn default_matches_readable_cell_and_nerd_font() {
        let cfg = RuntimeConfig::default();
        assert_eq!((cfg.cell_width, cfg.cell_height), (9, 19));
        assert_eq!(cfg.font_family, "JetBrainsMono Nerd Font");
        assert!((cfg.font_size - 12.0).abs() < f32::EPSILON);
    }
}
