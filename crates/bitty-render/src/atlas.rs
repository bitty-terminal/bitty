//! Glyph atlas layout math (shelf packing).
//!
//! This module owns the pure geometry of packing glyph bitmaps into a fixed
//! rectangular atlas texture. It deliberately does **not** touch the GPU:
//! texture creation and uploads belong to the pipeline slice; everything here
//! is deterministic integer math that headless CI can verify exhaustively.
//!
//! The packer is a classic shelf (row) packer: allocations advance left to
//! right along the current shelf; a bitmap taller than the shelf starts a new
//! shelf beneath it. It favors simplicity and determinism over optimal
//! density — terminal glyphs arrive in a few size classes, where shelves are
//! near-optimal. When no allocation fits, `allocate` returns `None` and the
//! caller decides between eviction ([`AtlasLayout::reset`]) or a larger
//! atlas; this crate never grows an atlas implicitly.

use crate::error::RenderError;

/// Default atlas side length in pixels (a 2048x2048 RGBA texture is 16 MiB,
/// comfortably above worst-case terminal font sets).
pub const DEFAULT_ATLAS_DIMENSION: u16 = 2048;

/// Fixed dimensions of an atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtlasDims {
    /// Atlas width in pixels.
    pub width: u16,
    /// Atlas height in pixels.
    pub height: u16,
}

/// A placed rectangle inside an atlas, in atlas pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtlasSlot {
    /// Left edge of the placed bitmap.
    pub x: u16,
    /// Top edge of the placed bitmap.
    pub y: u16,
    /// Placed width (equal to the requested span).
    pub width: u16,
    /// Placed height (equal to the requested span).
    pub height: u16,
}

impl AtlasSlot {
    /// Normalized UV coordinates of this slot as `[u0, v0, u1, v1]`, with the
    /// top-left of the atlas mapping to `(0, 0)` (the convention wgpu uses
    /// for texture coordinates sampled from `texture_2d` without flips).
    #[must_use]
    pub fn uv(&self, dims: AtlasDims) -> [f32; 4] {
        let w = f32::from(dims.width);
        let h = f32::from(dims.height);
        [
            f32::from(self.x) / w,
            f32::from(self.y) / h,
            f32::from(self.x + self.width) / w,
            f32::from(self.y + self.height) / h,
        ]
    }
}

/// Shelf-packed glyph atlas layout.
///
/// Invariant after every successful [`allocate`](AtlasLayout::allocate): all
/// issued slots are pairwise disjoint and lie fully inside the atlas bounds.
#[derive(Debug, Clone)]
pub struct AtlasLayout {
    dims: AtlasDims,
    /// Top edge of the currently open shelf.
    shelf_top: u16,
    /// Height of the tallest bitmap on the open shelf (0 when closed).
    shelf_height: u16,
    /// Next free x position on the open shelf.
    cursor_x: u16,
    used_area: u64,
}

impl AtlasLayout {
    /// Creates an empty atlas with the given dimensions. Zero dimensions are
    /// rejected up front.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero.
    pub fn new(width: u16, height: u16) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidInput {
                reason: "atlas dimensions must be non-zero",
            });
        }
        Ok(Self {
            dims: AtlasDims { width, height },
            shelf_top: 0,
            shelf_height: 0,
            cursor_x: 0,
            used_area: 0,
        })
    }

    /// The fixed atlas dimensions.
    #[must_use]
    pub const fn dimensions(&self) -> AtlasDims {
        self.dims
    }

    /// Fraction of atlas pixels covered by issued slots, in `[0, 1]`.
    #[must_use]
    pub fn occupancy(&self) -> f64 {
        let total = u64::from(self.dims.width) * u64::from(self.dims.height);
        if total == 0 {
            return 0.0;
        }
        self.used_area as f64 / total as f64
    }

    /// Tries to place a `width x height` bitmap, returning its slot.
    ///
    /// Returns `None` when the request cannot ever fit (zero span or larger
    /// than the atlas) or when no space remains on any shelf. Zero-sized
    /// requests are refused because blank glyphs must never be uploaded;
    /// callers skip them before reaching the atlas.
    pub fn allocate(&mut self, width: u16, height: u16) -> Option<AtlasSlot> {
        if width == 0 || height == 0 || width > self.dims.width || height > self.dims.height {
            return None;
        }

        let fits_current_shelf = u32::from(self.cursor_x) + u32::from(width)
            <= u32::from(self.dims.width)
            && u32::from(self.shelf_top) + u32::from(height) <= u32::from(self.dims.height);

        if !fits_current_shelf {
            // Close the current shelf and open the next one below it.
            let next_top = u32::from(self.shelf_top) + u32::from(self.shelf_height);
            if next_top + u32::from(height) <= u32::from(self.dims.height) {
                self.shelf_top = next_top as u16;
                self.shelf_height = 0;
                self.cursor_x = 0;
            } else {
                return None; // Atlas exhausted.
            }
        }

        let slot = AtlasSlot {
            x: self.cursor_x,
            y: self.shelf_top,
            width,
            height,
        };
        self.cursor_x += width;
        self.shelf_height = self.shelf_height.max(height);
        self.used_area += u64::from(width) * u64::from(height);
        Some(slot)
    }

    /// Evicts every issued slot, returning the atlas to an empty state.
    /// Callers must pair this with invalidating whatever GPU-side copy exists
    /// (the pipeline slice's responsibility).
    pub fn reset(&mut self) {
        self.shelf_top = 0;
        self.shelf_height = 0;
        self.cursor_x = 0;
        self.used_area = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> AtlasLayout {
        AtlasLayout::new(64, 32).unwrap()
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert!(matches!(
            AtlasLayout::new(0, 16),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            AtlasLayout::new(16, 0),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn sequential_allocation_fills_rows() {
        let mut atlas = layout();
        let a = atlas.allocate(8, 8).unwrap();
        let b = atlas.allocate(8, 8).unwrap();
        assert_eq!(
            a,
            AtlasSlot {
                x: 0,
                y: 0,
                width: 8,
                height: 8
            }
        );
        assert_eq!(
            b,
            AtlasSlot {
                x: 8,
                y: 0,
                width: 8,
                height: 8
            }
        );
    }

    #[test]
    fn taller_bitmap_opens_new_shelf() {
        let mut atlas = layout();
        let _a = atlas.allocate(60, 4).unwrap();
        // Does not fit beside the first bitmap: new shelf at y = 4.
        let b = atlas.allocate(8, 8).unwrap();
        assert_eq!(
            b,
            AtlasSlot {
                x: 0,
                y: 4,
                width: 8,
                height: 8
            }
        );
        // Fits next to `b` on that shelf.
        let c = atlas.allocate(8, 2).unwrap();
        assert_eq!(
            c,
            AtlasSlot {
                x: 8,
                y: 4,
                width: 8,
                height: 2
            }
        );
    }

    #[test]
    fn exhaustion_returns_none_deterministically() {
        let mut atlas = layout();
        // Fill the whole atlas with 32 slots of 8x8.
        for i in 0..32 {
            assert!(atlas.allocate(8, 8).is_some(), "slot {i} should fit");
        }
        assert_eq!(atlas.occupancy(), 1.0);
        assert!(atlas.allocate(1, 1).is_none());
        // Repeated refusals are stable.
        assert!(atlas.allocate(1, 1).is_none());
    }

    #[test]
    fn oversized_requests_never_fit() {
        let mut atlas = layout();
        assert!(atlas.allocate(65, 1).is_none());
        assert!(atlas.allocate(1, 33).is_none());
        assert!(atlas.allocate(64, 32).is_some()); // Exactly full-size fits.
        assert!(atlas.allocate(1, 1).is_none());
    }

    #[test]
    fn zero_sized_requests_are_refused() {
        let mut atlas = layout();
        assert!(atlas.allocate(0, 5).is_none());
        assert!(atlas.allocate(5, 0).is_none());
        assert!(atlas.allocate(0, 0).is_none());
    }

    #[test]
    fn reset_restores_empty_state() {
        let mut atlas = layout();
        let first = atlas.allocate(8, 8).unwrap();
        assert!(atlas.occupancy() > 0.0);
        atlas.reset();
        assert_eq!(atlas.occupancy(), 0.0);
        assert_eq!(atlas.allocate(8, 8).unwrap(), first);
    }

    #[test]
    fn uv_coordinates_are_normalized() {
        let dims = AtlasDims {
            width: 128,
            height: 64,
        };
        let slot = AtlasSlot {
            x: 32,
            y: 16,
            width: 32,
            height: 16,
        };
        assert_eq!(slot.uv(dims), [0.25, 0.25, 0.5, 0.5]);
        let corner = AtlasSlot {
            x: 0,
            y: 0,
            width: 128,
            height: 64,
        };
        assert_eq!(corner.uv(dims), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn occupancy_tracks_partial_fill() {
        let mut atlas = layout(); // 64x32 = 2048 pixels
        let _ = atlas.allocate(16, 16); // 256 pixels
        assert!((atlas.occupancy() - 0.125).abs() < 1e-9);
    }

    #[test]
    fn tiny_atlas_still_places_one_exact_fit() {
        let mut atlas = AtlasLayout::new(4, 4).unwrap();
        let only = atlas.allocate(4, 4).unwrap();
        assert_eq!(
            only,
            AtlasSlot {
                x: 0,
                y: 0,
                width: 4,
                height: 4
            }
        );
        assert!(atlas.allocate(1, 1).is_none());
    }
}
