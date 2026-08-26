//! Software fallback: CPU compositing of grid-pipeline draw lists onto an
//! in-memory surface (opt-in via the `sw-fallback` feature).
//!
//! The software/degraded fallback path required by the platform contracts:
//! a bounded premultiplied-alpha RGBA framebuffer, clipped src-over blits
//! of [`GlyphBitmap`]s and of one-byte coverage masks (the software
//! equivalent of sampling an atlas mask texel), and [`draw_list_onto`] —
//! which composites a full [`crate::grid::DrawList`] produced by the SAME
//! plan/place/cache pipeline the GPU backend consumes. Headless tests drive
//! `snapshot -> DrawList -> RGBA bytes` through this module end to end.
//! What it guarantees today:
//!
//! - allocation is capped at [`MAX_SURFACE_BYTES`] (bounded memory);
//! - blits are fully clipped and use saturating arithmetic (no panics on any
//!   bitmap/offset combination);
//! - blending follows premultiplied src-over exactly, verified by unit
//!   tests;
//! - `Rgb` coverage bitmaps are treated as grayscale antialiasing (channels
//!   averaged). Subpixel RGB policy is a later decision and deliberately not
//!   guessed here.
//!
//! CI note: this feature is off by default, so the default CI matrix neither
//! compiles nor tests it. Verify locally with
//! `cargo test -p bitty-render --features sw-fallback`.

use crate::error::RenderError;
use crate::glyph::{BitmapFormat, GlyphBitmap, GlyphMetrics};
use crate::grid::Rgba8;

/// Hard cap on surface bytes (64 MiB): a 4-byte-per-pixel RGBA surface can
/// therefore never exceed 16 Mi pixels. Bounded-memory requirement for any
/// buffer sized from untrusted configuration.
pub const MAX_SURFACE_BYTES: usize = 64 * 1024 * 1024;

/// A bounded, in-memory RGBA8 framebuffer with **premultiplied alpha**.
///
/// Premultiplied storage makes src-over an additive operation with no
/// divisions, which keeps the math exact for the byte range and matches what
/// GPU compositors expect downstream of the eventual present path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRgba {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl SurfaceRgba {
    /// Allocates a zeroed (fully transparent black) surface.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is zero or the
    /// byte size exceeds [`MAX_SURFACE_BYTES`].
    pub fn try_new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::InvalidInput {
                reason: "surface dimensions must be non-zero",
            });
        }
        let bytes = u64::from(width) * u64::from(height) * 4;
        if bytes > MAX_SURFACE_BYTES as u64 {
            return Err(RenderError::InvalidInput {
                reason: "surface exceeds the configured byte cap",
            });
        }
        // The cap check above bounds the allocation; conversion back to
        // usize cannot fail on any supported target.
        let len = usize::try_from(bytes).map_err(|_| RenderError::InvalidInput {
            reason: "surface size does not fit the address space",
        })?;
        Ok(Self {
            data: vec![0; len],
            width,
            height,
        })
    }

    /// Surface width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Surface height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Read-only view of the premultiplied pixel bytes (row-major RGBA).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Fills the surface with a straight-alpha color, converting it to
    /// premultiplied storage.
    pub fn clear(&mut self, rgba: [u8; 4]) {
        let [r, g, b, a] = rgba;
        for px in self.data.chunks_exact_mut(4) {
            px[0] = premultiply_byte(r, a);
            px[1] = premultiply_byte(g, a);
            px[2] = premultiply_byte(b, a);
            px[3] = a;
        }
    }

    /// Composites `bitmap` onto the surface with its top-left at `(x, y)`
    /// using premultiplied src-over. Out-of-surface regions are clipped;
    /// nothing else about `x`/`y` can fail.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] only when the bitmap itself violates
    /// its own documented invariants (which [`GlyphBitmap::try_new`]
    /// normally prevents).
    pub fn blend_glyph(&mut self, bitmap: &GlyphBitmap, x: i32, y: i32) -> Result<(), RenderError> {
        let GlyphMetrics {
            width: bw,
            height: bh,
            ..
        } = bitmap.metrics;
        if bw < 0 || bh < 0 {
            return Err(RenderError::InvalidInput {
                reason: "glyph dimensions must be non-negative",
            });
        }
        if bitmap.is_blank() {
            return Ok(());
        }
        let channels = match bitmap.format {
            BitmapFormat::Rgb | BitmapFormat::Rgba => bitmap.format.channels(),
        };
        if bitmap.data.len() != bw as usize * bh as usize * channels {
            return Err(RenderError::InvalidInput {
                reason: "glyph bitmap length does not match its dimensions",
            });
        }

        // Destination clip window in glyph-local coordinates (i64 throughout
        // so extreme offsets cannot overflow).
        let dst_left = i64::from(-x).max(0);
        let dst_top = i64::from(-y).max(0);
        let dst_right = (i64::from(self.width) - i64::from(x))
            .max(0)
            .min(i64::from(bw));
        let dst_bottom = (i64::from(self.height) - i64::from(y))
            .max(0)
            .min(i64::from(bh));

        for gy in dst_top..dst_bottom {
            for gx in dst_left..dst_right {
                let sx = (i64::from(x) + gx) as usize;
                let sy = (i64::from(y) + gy) as usize;
                let d = (sy * self.width as usize + sx) * 4;
                let s = (gy as usize * bw as usize + gx as usize) * channels;

                let (sr, sg, sb, sa) = match bitmap.format {
                    BitmapFormat::Rgb => {
                        // Coverage alphamap: average channels as luminance and
                        // treat the result as opaque-white coverage.
                        let c = (u16::from(bitmap.data[s])
                            + u16::from(bitmap.data[s + 1])
                            + u16::from(bitmap.data[s + 2]))
                            / 3;
                        (c as u8, c as u8, c as u8, c as u8)
                    }
                    BitmapFormat::Rgba => (
                        bitmap.data[s],
                        bitmap.data[s + 1],
                        bitmap.data[s + 2],
                        bitmap.data[s + 3],
                    ),
                };

                // Premultiplied over: out = src + dst * (1 - src_a), all
                // values are bytes so the multiply fits u32 comfortably.
                let inv = 255 - u32::from(sa);
                self.data[d] = saturating_add_u8(sr, (u32::from(self.data[d]) * inv / 255) as u8);
                self.data[d + 1] =
                    saturating_add_u8(sg, (u32::from(self.data[d + 1]) * inv / 255) as u8);
                self.data[d + 2] =
                    saturating_add_u8(sb, (u32::from(self.data[d + 2]) * inv / 255) as u8);
                self.data[d + 3] =
                    saturating_add_u8(sa, (u32::from(self.data[d + 3]) * inv / 255) as u8);
            }
        }
        Ok(())
    }

    /// Fills `rect` (clipped to the surface) with a straight-alpha color,
    /// converting it to premultiplied storage.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] never occurs today; the signature keeps
    /// compositing callers uniform. Empty or off-surface rects are no-ops.
    pub fn fill_rect(&mut self, rect: crate::geometry::RectPx, color: Rgba8) {
        let [r, g, b, a] = color;
        // Destination clip window in surface coordinates (i64 throughout).
        let left = i64::from(rect.x).max(0);
        let top = i64::from(rect.y).max(0);
        let right = (i64::from(rect.x) + i64::from(rect.width))
            .max(0)
            .min(i64::from(self.width));
        let bottom = (i64::from(rect.y) + i64::from(rect.height))
            .max(0)
            .min(i64::from(self.height));
        if right <= left || bottom <= top {
            return;
        }
        let pr = premultiply_byte(r, a);
        let pg = premultiply_byte(g, a);
        let pb = premultiply_byte(b, a);
        for y in top..bottom {
            let row = y as usize * self.width as usize;
            for x in left..right {
                let d = (row + x as usize) * 4;
                self.data[d] = pr;
                self.data[d + 1] = pg;
                self.data[d + 2] = pb;
                self.data[d + 3] = a;
            }
        }
    }

    /// Composites a one-byte-per-pixel coverage mask tinted with
    /// `color` (straight alpha) using premultiplied src-over — the software
    /// equivalent of sampling an atlas mask texel in the GPU pipeline.
    /// Out-of-surface regions are clipped.
    pub fn blend_coverage_mask(
        &mut self,
        mask: &[u8],
        mask_width: u32,
        mask_height: u32,
        x: i32,
        y: i32,
        color: Rgba8,
    ) {
        let Some(mask_width) = usize::try_from(mask_width).ok().filter(|w| *w > 0) else {
            return;
        };
        let mask_height = usize::try_from(mask_height).unwrap_or(0);

        let dst_left = i64::from(-x).max(0);
        let dst_top = i64::from(-y).max(0);
        let dst_right = (i64::from(self.width) - i64::from(x))
            .max(0)
            .min(i64::try_from(mask_width).unwrap_or(i64::MAX));
        let dst_bottom = (i64::from(self.height) - i64::from(y))
            .max(0)
            .min(i64::try_from(mask_height).unwrap_or(i64::MAX));

        let [cr, cg, cb, ca] = color;
        for gy in dst_top..dst_bottom {
            for gx in dst_left..dst_right {
                let coverage = u32::from(mask[gy as usize * mask_width + gx as usize]);
                if coverage == 0 {
                    continue;
                }
                // Straight -> premultiplied: channel * alpha, then the mask
                // coverage scales both: rgb * c * a / 65025 fits u32.
                let sa = (coverage * u32::from(ca)) / 255;
                let src = [
                    ((u32::from(cr) * coverage * u32::from(ca)) / 65025) as u8,
                    ((u32::from(cg) * coverage * u32::from(ca)) / 65025) as u8,
                    ((u32::from(cb) * coverage * u32::from(ca)) / 65025) as u8,
                    sa.min(255) as u8,
                ];
                let sx = (i64::from(x) + gx) as usize;
                let sy = (i64::from(y) + gy) as usize;
                let d = (sy * self.width as usize + sx) * 4;
                let inv = 255 - u32::from(src[3]);
                self.data[d] =
                    saturating_add_u8(src[0], (u32::from(self.data[d]) * inv / 255) as u8);
                self.data[d + 1] =
                    saturating_add_u8(src[1], (u32::from(self.data[d + 1]) * inv / 255) as u8);
                self.data[d + 2] =
                    saturating_add_u8(src[2], (u32::from(self.data[d + 2]) * inv / 255) as u8);
                self.data[d + 3] =
                    saturating_add_u8(src[3], (u32::from(self.data[d + 3]) * inv / 255) as u8);
            }
        }
    }
}

/// Composites a grid-pipeline [`DrawList`] onto a surface: fills first,
/// then glyphs, preserving vector order. Atlas instances sample
/// `(atlas_texels, atlas_dims)`; inline instances carry their own masks.
///
/// # Errors
///
/// [`RenderError::InvalidInput`] when an atlas instance exists but no atlas
/// was supplied, or when an inline mask violates its own dimensions.
pub fn draw_list_onto(
    list: &crate::grid::DrawList,
    atlas: Option<(&[u8], crate::atlas::AtlasDims)>,
    surface: &mut SurfaceRgba,
) -> Result<(), RenderError> {
    for fill in &list.fills {
        surface.fill_rect(fill.rect, fill.color);
    }
    for glyph in &list.glyphs {
        match &glyph.source {
            crate::grid::GlyphSource::Atlas { slot } => {
                let Some((texels, dims)) = atlas else {
                    return Err(RenderError::InvalidInput {
                        reason: "atlas instance requires atlas texels",
                    });
                };
                let stride = usize::from(dims.width);
                let mut mask =
                    Vec::with_capacity(usize::from(slot.width) * usize::from(slot.height));
                for row in 0..usize::from(slot.height) {
                    let start = (usize::from(slot.y) + row) * stride + usize::from(slot.x);
                    mask.extend_from_slice(&texels[start..start + usize::from(slot.width)]);
                }
                surface.blend_coverage_mask(
                    &mask,
                    slot.width.into(),
                    slot.height.into(),
                    glyph.dest[0],
                    glyph.dest[1],
                    glyph.color,
                );
            }
            crate::grid::GlyphSource::Inline {
                mask,
                width,
                height,
            } => {
                if mask.len() != *width as usize * *height as usize {
                    return Err(RenderError::InvalidInput {
                        reason: "inline mask length does not match its dimensions",
                    });
                }
                surface.blend_coverage_mask(
                    mask,
                    *width,
                    *height,
                    glyph.dest[0],
                    glyph.dest[1],
                    glyph.color,
                );
            }
        }
    }
    Ok(())
}

const fn premultiply_byte(color: u8, alpha: u8) -> u8 {
    // Widening `as` casts are const-stable and lossless (u8 -> u16); the
    // product of two u8 values always divides back into u8 range.
    ((color as u16 * alpha as u16) / 255) as u8
}

const fn saturating_add_u8(a: u8, b: u8) -> u8 {
    a.saturating_add(b)
}

// `as` casts above are all value-preserving by construction:
// - u16 sums divided by 3 fit u8 (765/3 = 255),
// - clip-window coordinates are clamped into [0, bitmap dims] before casting.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::GlyphMetrics;

    fn metrics(width: i32, height: i32) -> GlyphMetrics {
        GlyphMetrics {
            left: 0,
            top: 0,
            width,
            height,
            advance: [width, 0],
        }
    }

    fn rgb_bitmap(width: i32, height: i32, fill: u8) -> GlyphBitmap {
        let n = width as usize * height as usize * 3;
        GlyphBitmap::try_new(metrics(width, height), BitmapFormat::Rgb, vec![fill; n]).unwrap()
    }

    fn rgba_pixel(r: u8, g: u8, b: u8, a: u8) -> GlyphBitmap {
        GlyphBitmap::try_new(metrics(1, 1), BitmapFormat::Rgba, vec![r, g, b, a]).unwrap()
    }

    #[test]
    fn allocation_bounds_are_enforced() {
        assert!(SurfaceRgba::try_new(0, 10).is_err());
        assert!(SurfaceRgba::try_new(10, 0).is_err());
        assert!(matches!(
            SurfaceRgba::try_new(20_000_000, 20_000_000),
            Err(RenderError::InvalidInput { .. })
        ));
        let surface = SurfaceRgba::try_new(4, 2).unwrap();
        assert_eq!(surface.as_bytes().len(), 32);
    }

    #[test]
    fn clear_converts_straight_to_premultiplied() {
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([200, 100, 50, 128]);
        let expected_r = premultiply_byte(200, 128);
        assert_eq!(&surface.as_bytes()[..4], &[expected_r, 50, 25, 128]);
    }

    #[test]
    fn full_coverage_opaque_glyph_replaces_pixels() {
        let mut surface = SurfaceRgba::try_new(2, 1).unwrap();
        surface.clear([0, 0, 0, 0]);
        let glyph = rgb_bitmap(2, 1, 255); // Full white coverage everywhere.
        surface.blend_glyph(&glyph, 0, 0).unwrap();
        assert_eq!(&surface.as_bytes()[..8], &[255; 8]);
    }

    #[test]
    fn zero_coverage_glyph_is_a_noop() {
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([10, 20, 30, 40]);
        let glyph = rgb_bitmap(1, 1, 0);
        surface.blend_glyph(&glyph, 0, 0).unwrap();
        // Premultiplied clear color survives: 10*40/255=1, 20*40/255=3,
        // 30*40/255=4.
        assert_eq!(&surface.as_bytes()[..4], &[1, 3, 4, 40]);
    }

    #[test]
    fn transparent_rgba_source_leaves_destination() {
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([255, 255, 255, 255]);
        let glyph = rgba_pixel(200, 100, 50, 0); // Fully transparent.
        surface.blend_glyph(&glyph, 0, 0).unwrap();
        assert_eq!(&surface.as_bytes()[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn premultiplied_over_math_is_exact() {
        // dst = premul([0,0,0], a=255) = opaque black.
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([0, 0, 0, 255]);
        // src = premul gray 128 with alpha 255 → straight over black stays.
        let glyph = rgba_pixel(128, 128, 128, 255);
        surface.blend_glyph(&glyph, 0, 0).unwrap();
        assert_eq!(&surface.as_bytes()[..4], &[128, 128, 128, 255]);

        // Half-alpha red over opaque blue: r' = 255*0.5? No — premul src
        // (128,0,0,128) + dst*(127/255) = (128, 0, 0*?, ...) exact bytes:
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([0, 0, 255, 255]); // premul opaque blue.
        let half_red = rgba_pixel(128, 0, 0, 128);
        surface.blend_glyph(&half_red, 0, 0).unwrap();
        // out.r = 128 + 0*(127/255) = 128
        // out.b = 0 + 255*(127/255) = 127
        // out.a = 128 + 255*(127/255) = 255
        assert_eq!(&surface.as_bytes()[..4], &[128, 0, 127, 255]);
    }

    #[test]
    fn clipping_handles_negative_and_far_offsets() {
        let mut surface = SurfaceRgba::try_new(2, 2).unwrap();
        surface.clear([0, 0, 0, 0]);
        let glyph = rgb_bitmap(4, 4, 255);

        // Fully off-surface in every direction must be a safe no-op.
        surface.blend_glyph(&glyph, -4, 0).unwrap();
        surface.blend_glyph(&glyph, 2, 0).unwrap();
        surface.blend_glyph(&glyph, 0, -4).unwrap();
        surface.blend_glyph(&glyph, 0, 2).unwrap();
        assert!(surface.as_bytes().iter().all(|&b| b == 0));

        // Partial overlap writes exactly the intersecting pixels.
        surface.blend_glyph(&glyph, -3, -3).unwrap(); // touches (0..1, 0..1)
        assert_eq!(surface.as_bytes()[0], 255);
        assert_eq!(surface.as_bytes()[4], 0);
    }

    #[test]
    fn blank_glyph_is_a_noop() {
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        surface.clear([9, 9, 9, 9]);
        let blank = GlyphBitmap::try_new(metrics(0, 0), BitmapFormat::Rgb, Vec::new()).unwrap();
        surface.blend_glyph(&blank, 0, 0).unwrap();
        assert_eq!(surface.as_bytes()[3], 9);
    }

    #[test]
    fn malformed_bitmap_is_rejected_not_trusted() {
        let mut surface = SurfaceRgba::try_new(1, 1).unwrap();
        let bad = GlyphBitmap {
            metrics: metrics(2, 2),
            format: BitmapFormat::Rgb,
            data: vec![0; 3], // Wrong length for 2x2 RGB.
        };
        assert!(matches!(
            surface.blend_glyph(&bad, 0, 0),
            Err(RenderError::InvalidInput { .. })
        ));
    }
}
