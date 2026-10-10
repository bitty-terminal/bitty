//! Kitty raster mechanics (Core-owned, issue #1802).
//!
//! Nearest-neighbor scaling of stored RGBA bitmaps to placement rects.
//! The `bitty-graphics` extension crate owns its own copy plus a raster
//! cache (W-141); Core keeps this uncached copy so the present layer can
//! composite without a Core-to-extension call shape. The per-frame budget
//! caps stay Core-owned policy ([`super::kitty_place`]).
//!
//! Bounds: every length is validated with checked arithmetic **before**
//! any buffer is allocated, including the nearest-neighbor output
//! (`rect_w * rect_h * 4`, itself bounded because the caller clamps the
//! rect to the viewport first).

use super::kitty_place::{KITTY_DECODE_MAX_BYTES, KittyPlacedImage};
use crate::geometry::RectPx;

/// Scales stored RGBA to the exact `rect` pixel extent (nearest neighbor).
///
/// Returns `None` (paints nothing) when `rect` is empty, when the output
/// byte size fails checked validation against the 64 MiB cap, or when the
/// source bitmap fails validation. No allocation occurs before validation.
///
/// The output is straight-alpha RGBA8, row-major, exactly
/// `rect.width * rect.height * 4` bytes — the shape the present layer
/// composites.
#[must_use]
pub fn rasterize_kitty(image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>> {
    rasterize_kitty_clipped(image, rect, rect)
}

/// Scales the visible window of a placement (nearest neighbor).
///
/// `full` is the unclamped placement extent (the image scales into this,
/// exactly like [`rasterize_kitty`] would); `visible` is the
/// viewport-clipped sub-rectangle to emit (`visible` must lie inside
/// `full`). The output is bit-identical to scaling the whole image into
/// `full` and then cropping `visible` — but only the visible bytes are
/// ever allocated, so a placement overflowing the viewport paints its
/// visible part at true scale instead of squeezing the whole image into it.
///
/// Returns `None` (paints nothing) when `visible` is empty or outside
/// `full`, when the visible byte size fails checked validation against
/// the 64 MiB cap, or when the source bitmap fails validation. No
/// allocation occurs before validation.
///
/// The output is straight-alpha RGBA8, row-major, exactly
/// `visible.width * visible.height * 4` bytes.
#[must_use]
pub fn rasterize_kitty_clipped(
    image: &KittyPlacedImage,
    full: RectPx,
    visible: RectPx,
) -> Option<Vec<u8>> {
    if visible.width == 0 || visible.height == 0 || full.width == 0 || full.height == 0 {
        return None;
    }
    // `visible` must lie inside `full` (same origin space); anything else
    // is a caller bug and fails closed. `i64` differences of `i32`
    // coordinates never overflow; non-negative after the origin guard,
    // so the `as u64` casts are exact.
    if visible.x < full.x || visible.y < full.y {
        return None;
    }
    let offset_x = (i64::from(visible.x) - i64::from(full.x)) as u64;
    let offset_y = (i64::from(visible.y) - i64::from(full.y)) as u64;
    if offset_x + u64::from(visible.width) > u64::from(full.width)
        || offset_y + u64::from(visible.height) > u64::from(full.height)
    {
        return None;
    }
    let out_len = (u64::from(visible.width) * u64::from(visible.height))
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES as u64)?;
    // `out_len` fits `usize` on every supported target: it is at most
    // 64 MiB while `usize` is at least 32 bits.
    let out_len = out_len as usize;
    if image.width == 0 || image.height == 0 {
        return None;
    }
    let expected_src = (u64::from(image.width) * u64::from(image.height))
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES as u64)?;
    if image.rgba.len() as u64 != expected_src {
        return None;
    }
    let mut out = vec![0_u8; out_len];
    let (sw, sh) = (u64::from(image.width), u64::from(image.height));
    let (fw, fh) = (u64::from(full.width), u64::from(full.height));
    let (vw, vh) = (u64::from(visible.width), u64::from(visible.height));
    for dy in 0..vh {
        // Nearest neighbor into the full extent, then the visible window:
        // `sy = (offset_y + dy) * sh / fh` — division in u64, exact for
        // the bounded ranges here. Bit-identical to scaling into `full`
        // and cropping `visible`.
        let sy = (offset_y + dy) * sh / fh;
        for dx in 0..vw {
            let sx = (offset_x + dx) * sw / fw;
            let src = ((sy * sw + sx) * 4) as usize;
            let dst = ((dy * vw + dx) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&image.rgba[src..src + 4]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_place::{KittyImageId, KittyPlacedImage};

    fn solid_red_2x2() -> KittyPlacedImage {
        KittyPlacedImage {
            id: KittyImageId(1),
            origin: None,
            width: 2,
            height: 2,
            rgba: [0xFF, 0x00, 0x00, 0xFF].repeat(4),
            compressed_len: 16,
        }
    }

    fn two_color_2x1() -> KittyPlacedImage {
        KittyPlacedImage {
            id: KittyImageId(2),
            origin: None,
            width: 2,
            height: 1,
            rgba: vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF],
            compressed_len: 8,
        }
    }

    #[test]
    fn identity_scale_is_byte_identical() {
        let image = solid_red_2x2();
        let rect = RectPx::new(0, 0, 2, 2);
        assert_eq!(rasterize_kitty(&image, rect), Some(image.rgba.clone()));
    }

    #[test]
    fn upscale_replicates_nearest() {
        // 2x1 red|blue scaled to 4x1 doubles each source column.
        let image = two_color_2x1();
        let out = rasterize_kitty(&image, RectPx::new(0, 0, 4, 1)).expect("raster");
        assert_eq!(
            out,
            vec![
                0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00,
                0xFF, 0xFF,
            ]
        );
    }

    #[test]
    fn downscale_samples_nearest() {
        // 2x2 solid red down to 1x1 stays red and opaque.
        let image = solid_red_2x2();
        let out = rasterize_kitty(&image, RectPx::new(5, 5, 1, 1)).expect("raster");
        assert_eq!(out, vec![0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn clipped_matches_scale_then_crop() {
        // Full 4x1 scale of red|blue, then the right-half crop, must equal
        // the clipped raster of the right-half window.
        let image = two_color_2x1();
        let full = RectPx::new(0, 0, 4, 1);
        let visible = RectPx::new(2, 0, 2, 1);
        let out = rasterize_kitty_clipped(&image, full, visible).expect("raster");
        assert_eq!(out, vec![0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn empty_or_outside_rects_paint_nothing() {
        let image = solid_red_2x2();
        assert_eq!(rasterize_kitty(&image, RectPx::new(0, 0, 0, 2)), None);
        let full = RectPx::new(0, 0, 2, 2);
        // `visible` outside `full` fails closed.
        assert_eq!(
            rasterize_kitty_clipped(&image, full, RectPx::new(4, 4, 2, 2)),
            None
        );
        // `visible` larger than `full` fails closed.
        assert_eq!(
            rasterize_kitty_clipped(&image, full, RectPx::new(0, 0, 4, 4)),
            None
        );
    }

    #[test]
    fn corrupt_source_paints_nothing() {
        let bad = KittyPlacedImage {
            id: KittyImageId(9),
            origin: None,
            width: 2,
            height: 2,
            rgba: vec![0; 5],
            compressed_len: 5,
        };
        assert_eq!(rasterize_kitty(&bad, RectPx::new(0, 0, 2, 2)), None);
    }
}
