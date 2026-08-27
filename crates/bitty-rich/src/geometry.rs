//! Pixel geometry helpers for the headless presentation seam.
//!
//!mirrors `bitty-render::geometry` without pulling `wgpu` or `crossfont`.
//! All arithmetic is saturating and overflow-free (threat T-01).

/// Cell size in pixels (must be non-zero; validated by presentation code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub width: u32,
    /// Cell height in pixels.
    pub height: u32,
}

impl CellMetrics {
    /// Creates metrics; `None` when either dimension is zero.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            None
        } else {
            Some(Self { width, height })
        }
    }

    /// Pixel extent of a `cols x rows` grid (saturating).
    #[must_use]
    pub fn extent_for(&self, cols: usize, rows: usize) -> ExtentPx {
        ExtentPx::new(
            saturating_u32(u64::from(self.width).saturating_mul(cols as u64)),
            saturating_u32(u64::from(self.height).saturating_mul(rows as u64)),
        )
    }
}

const fn saturating_u32(value: u64) -> u32 {
    if value > u32::MAX as u64 {
        u32::MAX
    } else {
        value as u32
    }
}

/// Pixel extent (unsigned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentPx {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ExtentPx {
    /// Creates an extent.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Zero extent.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Axis-aligned rectangle in pixel space (signed origin, unsigned size).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RectPx {
    /// Left in pixels (signed, may be negative from glyph bearings).
    pub x: i32,
    /// Top in pixels.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl RectPx {
    /// Creates a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}
