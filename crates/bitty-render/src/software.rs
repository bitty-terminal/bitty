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
        self.blend_coverage_mask_clipped(mask, mask_width, mask_height, x, y, color, None);
    }

    /// [`Self::blend_coverage_mask`] with an optional rounded content clip
    /// (CTX-0311): each texel's coverage is scaled by the clip's analytic
    /// coverage at the destination pixel center, so glyph overhang never
    /// paints outside a decorated frame's inner corner curve. `None` is
    /// byte-identical to [`Self::blend_coverage_mask`].
    #[allow(clippy::too_many_arguments)]
    pub fn blend_coverage_mask_clipped(
        &mut self,
        mask: &[u8],
        mask_width: u32,
        mask_height: u32,
        x: i32,
        y: i32,
        color: Rgba8,
        clip: Option<crate::grid::RoundedClip>,
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
                let sx = (i64::from(x) + gx) as usize;
                let sy = (i64::from(y) + gy) as usize;
                let mut coverage = u32::from(mask[gy as usize * mask_width + gx as usize]);
                if coverage == 0 {
                    continue;
                }
                if let Some(clip) = clip {
                    if clip.may_clip_pixel(sx as i64, sy as i64) {
                        let clip_coverage = clip.coverage_at(sx as f32 + 0.5, sy as f32 + 0.5);
                        if clip_coverage <= 0.0 {
                            continue;
                        }
                        // Round to the nearest byte; `clip_coverage == 1.0`
                        // keeps the exact pre-CTX-0311 integer value.
                        coverage = (coverage as f32 * clip_coverage + 0.5) as u32;
                        if coverage == 0 {
                            continue;
                        }
                    }
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

    /// Fills a rounded rectangle or border ring with analytic pixel-center
    /// coverage (CTX-0311) — the CPU twin of the wgpu rounded-box SDF
    /// fragment stage. Partial coverage blends premultiplied src-over like
    /// [`Self::blend_coverage_mask`]; fully-covered opaque pixels are exact.
    /// Empty or off-surface frames are no-ops.
    pub fn fill_rounded_rect(&mut self, fill: &crate::grid::RoundedFill) {
        let left = i64::from(fill.frame.x).max(0);
        let top = i64::from(fill.frame.y).max(0);
        let right = (i64::from(fill.frame.x) + i64::from(fill.frame.width))
            .max(0)
            .min(i64::from(self.width));
        let bottom = (i64::from(fill.frame.y) + i64::from(fill.frame.height))
            .max(0)
            .min(i64::from(self.height));
        if right <= left || bottom <= top {
            return;
        }
        if fill.border == 0 {
            // Solid rounded fill: the whole frame carries coverage (not used
            // by the decoration ring path).
            for y in top..bottom {
                for x in left..right {
                    self.blend_rounded_pixel(fill, x, y);
                }
            }
            return;
        }
        // Ring hot path (CTX-0311): only the border band and the four corner
        // squares can carry coverage. Iterate just those runs so interior
        // rows/columns never touch `coverage_at` (the soak/latency budgets
        // depend on this).
        let fx0 = i64::from(fill.frame.x);
        let fy0 = i64::from(fill.frame.y);
        let fx1 = fx0 + i64::from(fill.frame.width);
        let fy1 = fy0 + i64::from(fill.frame.height);
        let band = i64::from(
            u32::from(fill.border)
                .min(fill.frame.width)
                .min(fill.frame.height),
        ) + 1;
        let cs = (fill.resolved_radius().ceil() as i64) + 1;
        let mid_left_end = (fx0 + band).min(right);
        let mid_right_start = (fx1 - band).max(left).max(mid_left_end);
        let cor_left_end = (fx0 + cs).min(right);
        let cor_right_start = (fx1 - cs).max(left).max(cor_left_end);
        for y in top..bottom {
            if y < fy0 + band || y >= fy1 - band {
                self.paint_rounded_run(fill, left, right, y);
            } else if y < fy0 + cs || y >= fy1 - cs {
                self.paint_rounded_run(fill, left, cor_left_end, y);
                self.paint_rounded_run(fill, cor_right_start, right, y);
            } else {
                self.paint_rounded_run(fill, left, mid_left_end, y);
                self.paint_rounded_run(fill, mid_right_start, right, y);
            }
        }
    }

    /// Blends one rounded-fill pixel with analytic coverage; zero-coverage
    /// pixels are skipped (CTX-0311).
    fn blend_rounded_pixel(&mut self, fill: &crate::grid::RoundedFill, x: i64, y: i64) {
        let coverage = fill.coverage_at(x as f32 + 0.5, y as f32 + 0.5);
        if coverage <= 0.0 {
            return;
        }
        let [cr, cg, cb, ca] = fill.color;
        let coverage = (coverage * 255.0 + 0.5).min(255.0) as u32;
        let sa = (coverage * u32::from(ca)) / 255;
        let src = [
            ((u32::from(cr) * coverage * u32::from(ca)) / 65025) as u8,
            ((u32::from(cg) * coverage * u32::from(ca)) / 65025) as u8,
            ((u32::from(cb) * coverage * u32::from(ca)) / 65025) as u8,
            sa.min(255) as u8,
        ];
        let d = (y as usize * self.width as usize + x as usize) * 4;
        let inv = 255 - u32::from(src[3]);
        self.data[d] = saturating_add_u8(src[0], (u32::from(self.data[d]) * inv / 255) as u8);
        self.data[d + 1] =
            saturating_add_u8(src[1], (u32::from(self.data[d + 1]) * inv / 255) as u8);
        self.data[d + 2] =
            saturating_add_u8(src[2], (u32::from(self.data[d + 2]) * inv / 255) as u8);
        self.data[d + 3] =
            saturating_add_u8(src[3], (u32::from(self.data[d + 3]) * inv / 255) as u8);
    }

    /// Runs [`Self::blend_rounded_pixel`] over `x0..x1` (empty when
    /// `x0 >= x1`).
    fn paint_rounded_run(&mut self, fill: &crate::grid::RoundedFill, x0: i64, x1: i64, y: i64) {
        for x in x0..x1 {
            self.blend_rounded_pixel(fill, x, y);
        }
    }

    /// Composites a straight-alpha RGBA blit onto the surface with src-over
    /// (CTX-0248 Kitty present layer). Out-of-surface regions are clipped.
    ///
    /// `rgba` must be exactly `w * h * 4` bytes; mismatched or zero spans
    /// are skipped (fail closed).
    pub fn blend_rgba_image(&mut self, rgba: &[u8], w: u32, h: u32, x: i32, y: i32) {
        if w == 0 || h == 0 {
            return;
        }
        let expected = (u64::from(w) * u64::from(h)).checked_mul(4);
        if expected.is_none_or(|n| n as usize != rgba.len()) {
            return;
        }
        let dst_left = i64::from(-x).max(0);
        let dst_top = i64::from(-y).max(0);
        let dst_right = (i64::from(self.width) - i64::from(x))
            .max(0)
            .min(i64::from(w));
        let dst_bottom = (i64::from(self.height) - i64::from(y))
            .max(0)
            .min(i64::from(h));
        if dst_right <= dst_left || dst_bottom <= dst_top {
            return;
        }
        let stride = w as usize;
        for gy in dst_top..dst_bottom {
            for gx in dst_left..dst_right {
                let s = (gy as usize * stride + gx as usize) * 4;
                let (sr, sg, sb, sa) = (rgba[s], rgba[s + 1], rgba[s + 2], rgba[s + 3]);
                if sa == 0 {
                    continue;
                }
                let ps_r = (u32::from(sr) * u32::from(sa) / 255) as u8;
                let ps_g = (u32::from(sg) * u32::from(sa) / 255) as u8;
                let ps_b = (u32::from(sb) * u32::from(sa) / 255) as u8;
                let sx = (i64::from(x) + gx) as usize;
                let sy = (i64::from(y) + gy) as usize;
                let d = (sy * self.width as usize + sx) * 4;
                let inv = 255 - u32::from(sa);
                self.data[d] = saturating_add_u8(ps_r, (u32::from(self.data[d]) * inv / 255) as u8);
                self.data[d + 1] =
                    saturating_add_u8(ps_g, (u32::from(self.data[d + 1]) * inv / 255) as u8);
                self.data[d + 2] =
                    saturating_add_u8(ps_b, (u32::from(self.data[d + 2]) * inv / 255) as u8);
                self.data[d + 3] =
                    saturating_add_u8(sa, (u32::from(self.data[d + 3]) * inv / 255) as u8);
            }
        }
    }
}

/// Composites a grid-pipeline [`DrawList`] onto a surface: fills, then
/// rounded decoration fills/rings (CTX-0311), then glyphs (clipped when the
/// instance carries a rounded clip), then RGBA image blits (CTX-0248,
/// topmost), preserving vector order. Atlas instances sample
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
    for fill in &list.rounded_fills {
        surface.fill_rounded_rect(fill);
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
                surface.blend_coverage_mask_clipped(
                    &mask,
                    slot.width.into(),
                    slot.height.into(),
                    glyph.dest[0],
                    glyph.dest[1],
                    glyph.color,
                    glyph.clip,
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
                surface.blend_coverage_mask_clipped(
                    mask,
                    *width,
                    *height,
                    glyph.dest[0],
                    glyph.dest[1],
                    glyph.color,
                    glyph.clip,
                );
            }
        }
    }
    for blit in &list.images {
        surface.blend_rgba_image(
            &blit.rgba,
            blit.dest.width,
            blit.dest.height,
            blit.dest.x,
            blit.dest.y,
        );
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
    fn rgba_image_blits_topmost_clipped_and_validated() {
        // Opaque green 2x2 at (1, 0) on a 3x2 surface.
        let mut surface = SurfaceRgba::try_new(3, 2).unwrap();
        surface.clear([0, 0, 0, 255]);
        surface.blend_rgba_image(&[0, 0xFF, 0, 0xFF].repeat(4), 2, 2, 1, 0);
        for (x, y) in [(1, 0), (2, 0), (1, 1), (2, 1)] {
            let o = (y * 3 + x) * 4;
            assert_eq!(&surface.as_bytes()[o..o + 4], &[0, 255, 0, 255]);
        }
        assert_eq!(&surface.as_bytes()[0..4], &[0, 0, 0, 255]);
        // Mismatched bytes and zero spans are safe no-ops.
        surface.blend_rgba_image(&[1; 15], 2, 2, 0, 0);
        surface.blend_rgba_image(&[], 0, 2, 0, 0);
        assert_eq!(&surface.as_bytes()[0..4], &[0, 0, 0, 255]);
        // Fully off-surface is a safe no-op (painted pixels survive).
        surface.blend_rgba_image(&[9; 4], 1, 1, 9, 9);
        let o = 20;
        assert_eq!(&surface.as_bytes()[o..o + 4], &[0, 255, 0, 255]);
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

    #[test]
    fn rounded_fill_ring_paints_arc_interior_and_aa() {
        let mut surface = SurfaceRgba::try_new(20, 10).unwrap();
        surface.clear([0, 0, 0, 255]);
        let fill = crate::grid::RoundedFill {
            frame: crate::geometry::RectPx::new(0, 0, 20, 10),
            border: 2,
            radius: 4,
            color: [0x58, 0x5B, 0x70, 0xFF],
        };
        surface.fill_rounded_rect(&fill);
        let px = |x: usize, y: usize| -> [u8; 4] {
            let o = (y * 20 + x) * 4;
            [
                surface.as_bytes()[o],
                surface.as_bytes()[o + 1],
                surface.as_bytes()[o + 2],
                surface.as_bytes()[o + 3],
            ]
        };
        // Straight border edge is the opaque ring color.
        assert_eq!(px(0, 5), [0x58, 0x5B, 0x70, 0xFF]);
        // Outer corner is cut; the content interior stays background.
        assert_eq!(px(0, 0), [0, 0, 0, 255]);
        assert_eq!(px(10, 5), [0, 0, 0, 255]);
        // An arc pixel carries partial coverage, strictly between ring and
        // background (anti-aliased, never a hard step).
        let aa = px(1, 0);
        assert!(aa[3] == 255 && aa[0] > 0 && aa[0] < 0x58, "aa={aa:?}");
    }

    #[test]
    fn clipped_glyph_coverage_is_scaled_by_the_inner_arc() {
        let mut surface = SurfaceRgba::try_new(8, 8).unwrap();
        surface.clear([0, 0, 0, 0]);
        let clip = crate::grid::RoundedClip {
            rect: crate::geometry::RectPx::new(0, 0, 8, 8),
            radius: 3,
        };
        surface.blend_coverage_mask_clipped(
            &[255; 64],
            8,
            8,
            0,
            0,
            [255, 255, 255, 255],
            Some(clip),
        );
        // Fully outside the corner arc: untouched.
        assert_eq!(&surface.as_bytes()[0..4], &[0, 0, 0, 0]);
        // Fully inside: opaque white.
        let inside = (4 * 8 + 4) * 4;
        assert_eq!(&surface.as_bytes()[inside..inside + 4], &[255; 4]);
        // On the arc: partial coverage strictly between.
        let arc = 4;
        let v = surface.as_bytes()[arc];
        assert!(v > 0 && v < 255, "arc coverage must be partial: {v}");

        // `None` is byte-identical to the unclipped path.
        let mut plain = SurfaceRgba::try_new(8, 8).unwrap();
        plain.clear([1, 2, 3, 4]);
        let mut delegated = SurfaceRgba::try_new(8, 8).unwrap();
        delegated.clear([1, 2, 3, 4]);
        plain.blend_coverage_mask(&[128; 64], 8, 8, 0, 0, [200, 100, 50, 255]);
        delegated.blend_coverage_mask_clipped(&[128; 64], 8, 8, 0, 0, [200, 100, 50, 255], None);
        assert_eq!(plain.as_bytes(), delegated.as_bytes());
    }

    #[test]
    fn draw_list_paints_rounded_fills_after_plain_fills() {
        use crate::grid::{DrawList, FillRect, GlyphSource, RoundedFill};
        let mut surface = SurfaceRgba::try_new(8, 8).unwrap();
        surface.clear([0, 0, 0, 255]);
        let list = DrawList {
            generation: 1,
            plan: crate::frame::FramePlan {
                extent: crate::geometry::ExtentPx::new(8, 8),
                mode: crate::frame::FrameMode::Full,
                dirty_rects: vec![crate::geometry::RectPx::new(0, 0, 8, 8)],
            },
            // A plain fill covers the whole surface, then the ring paints on
            // top: paint order is fills, rounded fills, glyphs, images.
            fills: vec![FillRect {
                rect: crate::geometry::RectPx::new(0, 0, 8, 8),
                color: [0, 0, 255, 255],
            }],
            rounded_fills: vec![RoundedFill {
                frame: crate::geometry::RectPx::new(0, 0, 8, 8),
                border: 2,
                radius: 3,
                color: [0, 255, 0, 255],
            }],
            glyphs: vec![],
            images: vec![],
        };
        draw_list_onto(&list, None, &mut surface).unwrap();
        let px = |x: usize, y: usize| -> [u8; 4] {
            let o = (y * 8 + x) * 4;
            [
                surface.as_bytes()[o],
                surface.as_bytes()[o + 1],
                surface.as_bytes()[o + 2],
                surface.as_bytes()[o + 3],
            ]
        };
        assert_eq!(px(4, 0), [0, 255, 0, 255], "ring over plain fill");
        assert_eq!(px(4, 4), [0, 0, 255, 255], "interior keeps plain fill");
        assert_eq!(px(0, 0), [0, 0, 255, 255], "corner cut shows plain fill");
        // Inline glyphs without a clip still composite (regression guard).
        let mut with_glyph = list;
        with_glyph.glyphs.push(crate::grid::GlyphInstance {
            dest: [2, 2],
            size: [2, 2],
            uv: [0.0; 4],
            color: [255, 255, 255, 255],
            clip: None,
            source: GlyphSource::Inline {
                mask: vec![255; 4],
                width: 2,
                height: 2,
            },
        });
        let mut glyph_surface = SurfaceRgba::try_new(8, 8).unwrap();
        glyph_surface.clear([0, 0, 0, 255]);
        draw_list_onto(&with_glyph, None, &mut glyph_surface).unwrap();
        let o = (2 * 8 + 2) * 4;
        assert_eq!(&glyph_surface.as_bytes()[o..o + 4], &[255; 4]);
    }
}
