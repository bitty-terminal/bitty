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

/// Initial atlas side length for lazy allocation (512x512 = 256 KiB).
/// The atlas grows on demand up to DEFAULT_ATLAS_DIMENSION.
pub const INITIAL_ATLAS_DIMENSION: u16 = 512;

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

// ---------------------------------------------------------------------------
// Kitty image layer composition (CTX-0950, issue #1668).
//
// The shelf packer above places glyph bitmaps; image bitmaps share it
// through the same [`AtlasLayout::allocate`] (a decoded frame is just a
// rectangle). What is image-specific is *composition*: many placements
// overlap the viewport at different z-indexes with different blend modes,
// and the pipeline needs them in paint order, clipped, with empty
// projections dropped. Everything here is pure integer geometry over
// caller-supplied rects — no pixels, no GPU, no allocation beyond the
// returned order list — so headless CI verifies it exhaustively.
//
// Reference: kitty `graphics-protocol.rst` (z-index layering, compose
// modes), kitty `graphics.c` layer sort (`z`, then image id), ghostty
// `graphics_storage.zig` paint ordering.
//
// CTX-1087 (F11, #1891) normative note: the Kitty z-order threshold is
// single-sourced in `bitty-term-state::KITTY_Z_BELOW_BACKGROUND` (the
// terminal-truth placement store owns the value). This module consumes it;
// [`IMAGE_Z_BELOW_BACKGROUND`] below is a compatibility alias, not an
// independent definition, and must never drift (pinned by
// `z_threshold_single_sourced`).
// ---------------------------------------------------------------------------

/// `z-index` below which images draw under non-default cell backgrounds
/// (`INT32_MIN/2`, kitty layering rule).
///
/// CTX-1087 compatibility alias of the normative
/// `bitty-term-state::KITTY_Z_BELOW_BACKGROUND`: the value is defined once
/// in term-state and consumed here, so render never invents its own
/// threshold. New code should prefer the term-state constant directly.
pub const IMAGE_Z_BELOW_BACKGROUND: i32 = bitty_term_state::KITTY_Z_BELOW_BACKGROUND;

/// Whether `z` draws under text (any negative z-index).
#[must_use]
pub const fn image_is_below_text(z: i32) -> bool {
    z < 0
}

/// Whether `z` draws under non-default cell backgrounds.
#[must_use]
pub const fn image_is_below_background(z: i32) -> bool {
    z < IMAGE_Z_BELOW_BACKGROUND
}

/// Pixel rectangle: a placement's viewport-space footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageRectPx {
    /// Left edge in pixels.
    pub x: u32,
    /// Top edge in pixels.
    pub y: u32,
    /// Width in pixels (`0` = empty).
    pub w: u32,
    /// Height in pixels (`0` = empty).
    pub h: u32,
}

impl ImageRectPx {
    /// Whether the rectangle covers no pixels.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Intersection with `viewport` (`None` when disjoint or empty).
    /// Saturating: coordinates near `u32::MAX` clip instead of wrapping.
    #[must_use]
    pub fn intersect(self, viewport: ImageRectPx) -> Option<ImageRectPx> {
        if self.is_empty() || viewport.is_empty() {
            return None;
        }
        let x0 = self.x.max(viewport.x);
        let y0 = self.y.max(viewport.y);
        let x1 = self
            .x
            .saturating_add(self.w)
            .min(viewport.x.saturating_add(viewport.w));
        let y1 = self
            .y
            .saturating_add(self.h)
            .min(viewport.y.saturating_add(viewport.h));
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(ImageRectPx {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

/// How a layer blends onto the layers beneath it (wire `C=` compose key:
/// `0` alpha-blends, `1` replaces; any other value alpha-blends, since
/// only `1` is defined).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImageBlend {
    /// Full alpha blend over the layers beneath (default).
    #[default]
    Alpha,
    /// Simple pixel replacement of the layers beneath.
    Replace,
}

impl ImageBlend {
    /// Maps the wire `C=` value to a blend mode.
    #[must_use]
    pub const fn from_c(value: u32) -> Self {
        match value {
            1 => Self::Replace,
            _ => Self::Alpha,
        }
    }
}

/// One placement awaiting composition: identity plus geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageLayer {
    /// Wire `i=` image id (frame pixels come from the image store).
    pub image_id: u32,
    /// Wire `p=` placement id.
    pub placement_id: u32,
    /// Viewport-space footprint in pixels.
    pub rect: ImageRectPx,
    /// Wire `z=` stacking order.
    pub z_index: i32,
    /// Wire `C=` blend mode.
    pub blend: ImageBlend,
}

/// One layer after composition: paint-order position plus the clipped
/// rectangle the pipeline uploads/blits. `order` is the paint index
/// (`0` paints first, i.e. bottom-most).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComposedImage {
    /// Paint index (bottom-most is `0`).
    pub order: usize,
    /// Image id.
    pub image_id: u32,
    /// Placement id.
    pub placement_id: u32,
    /// Clipped footprint in pixels (never empty).
    pub rect: ImageRectPx,
    /// Stacking order (informational for the pipeline).
    pub z_index: i32,
    /// Blend mode (informational for the pipeline).
    pub blend: ImageBlend,
}

/// Composes `layers` for `viewport` in paint order.
///
/// Stable sort by `(z-index, image id, placement id)` — the kitty rule
/// (same `z` orders the lower id beneath; same id keeps submission
/// order via sort stability) — then clips every footprint to the
/// viewport, dropping empty and disjoint projections. Cost is
/// `O(n log n)` in the placement count with no retained state; the
/// caller bounds `n` (the placement store caps it).
#[must_use]
pub fn compose_image_layers(layers: &[ImageLayer], viewport: ImageRectPx) -> Vec<ComposedImage> {
    let mut order: Vec<usize> = (0..layers.len()).collect();
    order.sort_by(|&a, &b| {
        (
            layers[a].z_index,
            layers[a].image_id,
            layers[a].placement_id,
        )
            .cmp(&(
                layers[b].z_index,
                layers[b].image_id,
                layers[b].placement_id,
            ))
    });
    let mut composed = Vec::new();
    // `order` counts returned images only: dropped (empty/disjoint)
    // layers must not shift the paint indices of the survivors, so the
    // bottom-most returned image is always index `0`.
    for index in order.iter() {
        let layer = layers[*index];
        if let Some(rect) = layer.rect.intersect(viewport) {
            composed.push(ComposedImage {
                order: composed.len(),
                image_id: layer.image_id,
                placement_id: layer.placement_id,
                rect,
                z_index: layer.z_index,
                blend: layer.blend,
            });
        }
    }
    composed
}

/// Converts a grid placement span to a pixel rectangle: `span` cells of
/// `cell` pixels at grid `origin`, plus the wire `X=`/`Y=` pixel `offset`
/// inside the origin cell. Saturating: oversized spans clip at `u32::MAX`
/// instead of wrapping.
#[must_use]
pub fn placement_rect_px(
    origin: (u32, u32),
    span: (u32, u32),
    cell: (u32, u32),
    offset: (u32, u32),
) -> ImageRectPx {
    ImageRectPx {
        x: origin.0.saturating_mul(cell.0).saturating_add(offset.0),
        y: origin.1.saturating_mul(cell.1).saturating_add(offset.1),
        w: span.0.saturating_mul(cell.0),
        h: span.1.saturating_mul(cell.1),
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

    // -- CTX-0950: image layer composition.

    fn layer(id: u32, placement: u32, z: i32, x: u32, y: u32, w: u32, h: u32) -> ImageLayer {
        ImageLayer {
            image_id: id,
            placement_id: placement,
            rect: ImageRectPx { x, y, w, h },
            z_index: z,
            blend: ImageBlend::Alpha,
        }
    }

    fn viewport() -> ImageRectPx {
        ImageRectPx {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        }
    }

    #[test]
    fn compose_sorts_by_z_then_id() {
        let layers = [
            layer(2, 1, 5, 0, 0, 10, 10),
            layer(1, 1, -1, 0, 0, 10, 10),
            layer(1, 2, 0, 0, 0, 10, 10),
            layer(1, 1, 0, 0, 0, 10, 10),
        ];
        let composed = compose_image_layers(&layers, viewport());
        let keys: Vec<(i32, u32, u32)> = composed
            .iter()
            .map(|c| (c.z_index, c.image_id, c.placement_id))
            .collect();
        assert_eq!(keys, [(-1, 1, 1), (0, 1, 1), (0, 1, 2), (5, 2, 1)]);
        assert!(composed.iter().enumerate().all(|(i, c)| c.order == i));
    }

    #[test]
    fn compose_clips_and_drops_disjoint_or_empty() {
        let layers = [
            // Half outside the viewport: clipped, not dropped.
            layer(1, 1, 0, 700, 500, 200, 200),
            // Fully outside: dropped.
            layer(2, 1, 0, 900, 0, 10, 10),
            // Empty: dropped.
            layer(3, 1, 0, 0, 0, 0, 10),
        ];
        let composed = compose_image_layers(&layers, viewport());
        assert_eq!(composed.len(), 1);
        assert_eq!(composed[0].image_id, 1);
        assert_eq!(composed[0].order, 0);
        assert_eq!(
            composed[0].rect,
            ImageRectPx {
                x: 700,
                y: 500,
                w: 100,
                h: 100
            }
        );
    }

    #[test]
    fn compose_dropped_layer_before_visible_keeps_dense_order() {
        // The dropped layer sorts first (lowest z) but contributes no
        // paint index: survivors are numbered densely from `0`.
        let layers = [
            // Fully outside the viewport: dropped, sorts first.
            layer(1, 1, -1, 900, 0, 10, 10),
            layer(2, 1, 0, 0, 0, 10, 10),
            layer(3, 1, 5, 20, 20, 10, 10),
        ];
        let composed = compose_image_layers(&layers, viewport());
        let keys: Vec<(u32, usize)> = composed.iter().map(|c| (c.image_id, c.order)).collect();
        assert_eq!(keys, [(2, 0), (3, 1)]);
    }

    #[test]
    fn compose_empty_viewport_or_layers_is_empty() {
        let layers = [layer(1, 1, 0, 0, 0, 10, 10)];
        let empty_view = ImageRectPx {
            x: 0,
            y: 0,
            w: 0,
            h: 600,
        };
        assert!(compose_image_layers(&layers, empty_view).is_empty());
        assert!(compose_image_layers(&[], viewport()).is_empty());
    }

    #[test]
    fn compose_is_deterministic_and_stable() {
        // Same id and z twice: submission order survives (stable sort).
        let layers = [
            layer(7, 0, 0, 10, 10, 20, 20),
            layer(7, 0, 0, 30, 30, 20, 20),
        ];
        let first = compose_image_layers(&layers, viewport());
        let second = compose_image_layers(&layers, viewport());
        assert_eq!(first, second);
        assert_eq!(first[0].rect.x, 10);
        assert_eq!(first[1].rect.x, 30);
    }

    #[test]
    fn blend_mode_maps_wire_c() {
        assert_eq!(ImageBlend::from_c(0), ImageBlend::Alpha);
        assert_eq!(ImageBlend::from_c(1), ImageBlend::Replace);
        assert_eq!(ImageBlend::from_c(99), ImageBlend::Alpha);
        let mut replace = layer(1, 1, 0, 0, 0, 10, 10);
        replace.blend = ImageBlend::Replace;
        let composed = compose_image_layers(&[replace], viewport());
        assert_eq!(composed[0].blend, ImageBlend::Replace);
    }

    #[test]
    fn z_partition_predicates_match_spec() {
        assert!(image_is_below_text(-1));
        assert!(!image_is_below_text(0));
        assert!(!image_is_below_background(-1));
        assert!(image_is_below_background(IMAGE_Z_BELOW_BACKGROUND - 1));
        assert_eq!(IMAGE_Z_BELOW_BACKGROUND, -1_073_741_824);
    }

    #[test]
    fn z_threshold_single_sourced() {
        // CTX-1087 (F11, #1891): the render alias must equal the normative
        // term-state threshold; the value is defined once in term-state.
        assert_eq!(
            IMAGE_Z_BELOW_BACKGROUND,
            bitty_term_state::KITTY_Z_BELOW_BACKGROUND
        );
        assert!(image_is_below_background(
            bitty_term_state::KITTY_Z_BELOW_BACKGROUND - 1
        ));
        assert!(!image_is_below_background(
            bitty_term_state::KITTY_Z_BELOW_BACKGROUND
        ));
    }

    #[test]
    fn placement_rect_px_scales_cells_and_offsets() {
        // 3x2 cells of 10x20px at grid (4, 5) with a (2, 3)px origin shift.
        let rect = placement_rect_px((4, 5), (3, 2), (10, 20), (2, 3));
        assert_eq!(
            rect,
            ImageRectPx {
                x: 42,
                y: 103,
                w: 30,
                h: 40
            }
        );
        // Zero cells: empty footprint (dropped by composition).
        assert!(placement_rect_px((0, 0), (0, 2), (10, 20), (0, 0)).is_empty());
        // Saturation, never wrap.
        let huge = placement_rect_px((u32::MAX, u32::MAX), (u32::MAX, 1), (100, 100), (99, 99));
        assert_eq!(huge.x, u32::MAX);
        assert_eq!(huge.y, u32::MAX);
    }

    #[test]
    fn intersect_is_total_over_extremes() {
        let full = ImageRectPx {
            x: 0,
            y: 0,
            w: u32::MAX,
            h: u32::MAX,
        };
        // Viewport inside the huge rect clips without overflow.
        let clipped = full.intersect(viewport()).unwrap();
        assert_eq!(clipped, viewport());
        // Disjoint and touching edges are empty (no zero-area layers).
        let right = ImageRectPx {
            x: 800,
            y: 0,
            w: 10,
            h: 10,
        };
        assert_eq!(right.intersect(viewport()), None);
    }
}
