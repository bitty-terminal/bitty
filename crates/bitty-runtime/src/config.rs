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

/// Owned runtime configuration, validated on construction.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    /// Terminal width in columns.
    pub cols: usize,
    /// Terminal height in rows.
    pub rows: usize,
    /// Cell width in physical pixels.
    pub cell_width: u32,
    /// Cell height in physical pixels.
    pub cell_height: u32,
    /// Capacity of the bounded cold-path event queue.
    pub cold_queue_capacity: usize,
    /// Font family used by the renderer; whitespace-trimmed on validation.
    pub font_family: String,
    /// Font point size; must be finite and within `(0, 3999]`.
    pub font_size: f32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            cols: bitty_term_state::GRID_COLUMNS,
            rows: bitty_term_state::GRID_ROWS,
            cell_width: 8,
            cell_height: 16,
            cold_queue_capacity: 256,
            font_family: String::from("monospace"),
            font_size: 12.0,
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
    pub fn new(
        cols: usize,
        rows: usize,
        cell_width: u32,
        cell_height: u32,
        cold_queue_capacity: usize,
        font_family: impl Into<String>,
        font_size: f32,
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
        assert_eq!(extent.width(), 640);
        assert_eq!(extent.height(), 384);
    }

    #[test]
    fn grid_from_pixels_rounds_down_and_clamps() {
        let cfg = RuntimeConfig::default();
        let size = bitty_platform::PhysicalSize::new(100, 100);
        let (cols, rows) = cfg.grid_from_pixels(size);
        assert_eq!(cols, 12);
        assert_eq!(rows, 6);
        let zero = bitty_platform::PhysicalSize::new(0, 0);
        let (c, r) = cfg.grid_from_pixels(zero);
        assert_eq!(c, 1);
        assert_eq!(r, 1);
    }

    #[test]
    fn invalid_fields_are_rejected() {
        assert!(RuntimeConfig::new(0, 24, 8, 16, 256, "mono", 12.0).is_err());
        assert!(RuntimeConfig::new(80, 24, 0, 16, 256, "mono", 12.0).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 0, "mono", 12.0).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "   ", 12.0).is_err());
        assert!(RuntimeConfig::new(80, 24, 8, 16, 256, "mono", 0.0).is_err());
    }
}
