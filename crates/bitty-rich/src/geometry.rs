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

    /// True when either span is zero (empty rectangles never intersect).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Intersection by positive-area overlap; `None` when the rectangles
    /// are disjoint or either is empty (`bitty-render` parity).
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Option<Self> {
        let x0 = i64::from(self.x).max(i64::from(other.x));
        let y0 = i64::from(self.y).max(i64::from(other.y));
        let x1 = (i64::from(self.x) + i64::from(self.width))
            .min(i64::from(other.x) + i64::from(other.width));
        let y1 = (i64::from(self.y) + i64::from(self.height))
            .min(i64::from(other.y) + i64::from(other.height));
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let x = i32::try_from(x0).unwrap_or(if x0 < 0 { i32::MIN } else { i32::MAX });
        let y = i32::try_from(y0).unwrap_or(if y0 < 0 { i32::MIN } else { i32::MAX });
        Some(Self {
            x,
            y,
            width: u32::try_from(x1 - x0).unwrap_or(u32::MAX),
            height: u32::try_from(y1 - y0).unwrap_or(u32::MAX),
        })
    }
}
