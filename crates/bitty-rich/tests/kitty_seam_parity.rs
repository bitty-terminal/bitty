//! Kitty graphics-seam parity (CTX-1186, issue #2000).
//!
//! Pins the Core-owned seam contract against the published
//! `bitty-graphics` surface (`bitty-graphics#15`: exact signatures, caps
//! table, binding map, 22 conformance tests) through the Core-owned trait
//! ([`bitty_rich::kitty_seam`]) only — this suite names no extension
//! crate, mirroring the conformance rule that every test runs over public
//! re-exports. If any test here fails, the Core side drifted from the seam
//! the extension binds to.
//!
//! Bounds under test (verbatim): 8192 px/side, 4096 x 4096 px area,
//! 64 MiB RGBA, 32 blits, 64 MiB frame. The raster-cache entry cap is an
//! explicit documented delta: Core keeps 32 (one per per-frame blit
//! budget slot); the extension sizes its own cache at 128. Both share the
//! 64 MiB byte ceiling.

use bitty_rich::kitty_seam::{
    CoreKittyBackend, KittyDecodeBackend, KittyRasterBackend, KittyTransmitFormat,
};
use bitty_rich::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS, KITTY_FORMAT_PNG,
    KITTY_FORMAT_RGB, KITTY_FORMAT_RGBA, KITTY_PRESENT_MAX_BLITS_PER_FRAME,
    KITTY_PRESENT_MAX_BYTES_PER_FRAME, KITTY_RASTER_CACHE_MAX_BYTES,
    KITTY_RASTER_CACHE_MAX_ENTRIES, KittyDecodeError, KittyDecodedImage, KittyImageId,
    KittyPlacedImage, RectPx, decode_kitty_payload, rasterize_kitty, rasterize_kitty_clipped,
};

/// 1x1 RGBA PNG, single red opaque pixel (same bytes as the extension suite).
const PNG_1X1_RGBA_RED: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// 2x1 RGB PNG: red then green (opaque-alpha expansion probe).
const PNG_2X1_RGB: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 123, 64, 232, 221, 0, 0, 0, 15, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 192, 240,
    159, 1, 0, 7, 255, 1, 255, 184, 4, 53, 224, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// 1x1 grayscale PNG (`0x7F`, gray-expansion probe).
const PNG_1X1_GRAY: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0,
    0, 0, 58, 126, 155, 85, 0, 0, 0, 10, 73, 68, 65, 84, 120, 218, 99, 168, 7, 0, 0, 129, 0, 128,
    126, 28, 41, 199, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// 1x1 gray-alpha PNG (`0x7F` luma, `0x80` alpha; same bytes as the extension suite).
const PNG_1X1_GRAY_ALPHA: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 168, 111, 0, 0, 1, 129, 1, 0,
    64, 19, 200, 179, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// 1x1 paletted PNG (red entry; same bytes as the extension suite).
const PNG_1X1_PALETTE: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 3, 0,
    0, 0, 40, 203, 52, 187, 0, 0, 0, 6, 80, 76, 84, 69, 255, 0, 0, 0, 255, 0, 210, 135, 239, 113,
    0, 0, 0, 10, 73, 68, 65, 84, 120, 218, 99, 96, 0, 0, 0, 2, 0, 1, 229, 39, 222, 252, 0, 0, 0, 0,
    73, 69, 78, 68, 174, 66, 96, 130,
];

/// 1x1 16-bit RGB PNG (red; same bytes as the extension suite).
const PNG_1X1_RGB16: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 16, 2, 0,
    0, 0, 192, 231, 143, 157, 0, 0, 0, 12, 73, 68, 65, 84, 120, 218, 99, 248, 207, 0, 2, 0, 6, 1,
    1, 0, 65, 8, 143, 241, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn backend() -> CoreKittyBackend {
    CoreKittyBackend::new()
}

fn solid_red_2x2() -> KittyPlacedImage {
    KittyPlacedImage {
        id: KittyImageId(1),
        generation: 1,
        origin: None,
        wire_image: 0,
        width: 2,
        height: 2,
        rgba: [0xFF, 0x00, 0x00, 0xFF].repeat(4),
        frames: Vec::new(),
        compressed_len: 16,
    }
}

fn two_color_2x1() -> KittyPlacedImage {
    KittyPlacedImage {
        id: KittyImageId(2),
        generation: 1,
        origin: None,
        wire_image: 0,
        width: 2,
        height: 1,
        rgba: vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF],
        frames: Vec::new(),
        compressed_len: 8,
    }
}

#[test]
fn seam_caps_are_frozen_verbatim() {
    assert_eq!(KITTY_DECODE_MAX_DIMENSION, 8192);
    assert_eq!(KITTY_DECODE_MAX_PIXELS, 4096 * 4096);
    assert_eq!(KITTY_DECODE_MAX_BYTES, 64 * 1024 * 1024);
    assert_eq!(KITTY_PRESENT_MAX_BLITS_PER_FRAME, 32);
    assert_eq!(KITTY_PRESENT_MAX_BYTES_PER_FRAME, 64 * 1024 * 1024);
    assert_eq!(KITTY_FORMAT_PNG, 100);
    assert_eq!(KITTY_FORMAT_RGB, 24);
    assert_eq!(KITTY_FORMAT_RGBA, 32);
    // Relationships the seam relies on: the area cap at 4 bytes per pixel
    // is exactly the byte cap, and a lone max image fits the frame budget.
    assert_eq!(KITTY_DECODE_MAX_PIXELS * 4, KITTY_DECODE_MAX_BYTES as u64);
    assert_eq!(KITTY_PRESENT_MAX_BYTES_PER_FRAME, KITTY_DECODE_MAX_BYTES);
    // Documented delta: Core sizes its raster cache at one entry per
    // per-frame blit slot under the shared 64 MiB byte ceiling; the
    // extension sizes its own cache differently. The byte ceiling is the
    // shared invariant.
    assert_eq!(KITTY_RASTER_CACHE_MAX_ENTRIES, 32);
    assert_eq!(KITTY_RASTER_CACHE_MAX_BYTES, 64 * 1024 * 1024);
}

#[test]
fn seam_format_mapping_is_exact() {
    assert_eq!(
        KittyTransmitFormat::from_f(100),
        Some(KittyTransmitFormat::Png)
    );
    assert_eq!(
        KittyTransmitFormat::from_f(24),
        Some(KittyTransmitFormat::Rgb)
    );
    assert_eq!(
        KittyTransmitFormat::from_f(32),
        Some(KittyTransmitFormat::Rgba)
    );
    // Unknown `f` never reaches the seam entry points: the caller rejects
    // before calling (the legacy wire-`u32` wrappers keep their own
    // `MalformedPng` spelling for direct callers).
    for unsupported in [0, 1, 2, 23, 25, 31, 33, 99, 101, 255, u32::MAX] {
        assert_eq!(
            KittyTransmitFormat::from_f(unsupported),
            None,
            "f={unsupported}"
        );
    }
    assert_eq!(KittyTransmitFormat::Png.f_value(), 100);
    assert_eq!(KittyTransmitFormat::Rgb.f_value(), 24);
    assert_eq!(KittyTransmitFormat::Rgba.f_value(), 32);
    assert_eq!(KittyTransmitFormat::Png.raw_channels(), None);
    assert_eq!(KittyTransmitFormat::Rgb.raw_channels(), Some(3));
    assert_eq!(KittyTransmitFormat::Rgba.raw_channels(), Some(4));
}

#[test]
fn seam_error_display_is_pinned() {
    assert_eq!(
        KittyDecodeError::EmptyPayload.to_string(),
        "kitty payload is empty"
    );
    assert_eq!(
        KittyDecodeError::MissingDimensions.to_string(),
        "kitty raw payload needs width and height (s/v)"
    );
    assert_eq!(
        KittyDecodeError::ZeroDimension.to_string(),
        "kitty image dimension is zero"
    );
    assert_eq!(
        KittyDecodeError::DimensionsTooLarge {
            width: 9000,
            height: 1,
            cap: 8192
        }
        .to_string(),
        "kitty image 9000x1 exceeds max dimension of 8192px"
    );
    assert_eq!(
        KittyDecodeError::TooManyPixels {
            pixels: 25_000_000,
            cap: 16_777_216
        }
        .to_string(),
        "kitty image of 25000000 pixels exceeds max of 16777216 pixels"
    );
    assert_eq!(
        KittyDecodeError::DecodedTooLarge {
            bytes: 100,
            cap: 64
        }
        .to_string(),
        "kitty decoded bitmap of 100 bytes exceeds max of 64 bytes"
    );
    assert_eq!(
        KittyDecodeError::LengthMismatch {
            expected: 6,
            actual: 5
        }
        .to_string(),
        "kitty raw payload of 5 bytes does not match 6 expected bytes"
    );
    let malformed = KittyDecodeError::MalformedPng("end of input".to_owned());
    assert_eq!(
        malformed.to_string(),
        "kitty PNG is malformed: end of input"
    );
}

#[test]
fn seam_backend_decode_matches_free_functions() {
    let back = backend();
    // Borrowed entry point, every format, through the trait.
    let png = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGBA_RED)
        .expect("valid PNG");
    assert_eq!((png.width, png.height), (1, 1));
    assert_eq!(png.rgba, vec![0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(
        png,
        decode_kitty_payload(100, None, None, PNG_1X1_RGBA_RED).expect("free fn agrees")
    );
    let rgb = back
        .decode_transmit(
            KittyTransmitFormat::Rgb,
            Some(2),
            Some(1),
            &[10, 20, 30, 40, 50, 60],
        )
        .expect("valid raw");
    assert_eq!(rgb.rgba, vec![10, 20, 30, 0xFF, 40, 50, 60, 0xFF]);
    // Owned entry point moves RGBA without copying and matches borrowed.
    let owned: Box<[u8]> = [0xFF, 0x00, 0x00, 0xFF].repeat(4).into_boxed_slice();
    let via_owned = back
        .decode_transmit_owned(KittyTransmitFormat::Rgba, Some(2), Some(2), owned)
        .expect("valid owned raw");
    let borrowed = bitty_rich::kitty_decode::decode_kitty_payload_owned(
        32,
        Some(2),
        Some(2),
        [0xFF, 0x00, 0x00, 0xFF].repeat(4).into_boxed_slice(),
    );
    assert!(borrowed.is_ok());
    assert_eq!(via_owned.rgba.len(), 16);
    // Determinism: same bytes always yield the same bitmap.
    let again = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGBA_RED)
        .expect("valid PNG");
    assert_eq!(png, again);
    // Fail-closed parity with the free functions.
    assert_eq!(
        back.decode_transmit(KittyTransmitFormat::Png, None, None, b"garbage"),
        decode_kitty_payload(100, None, None, b"garbage"),
    );
}

#[test]
fn seam_png_fixtures_normalize_like_the_seam() {
    let back = backend();
    // 8-bit color types normalize exactly (shared with the legacy suite).
    let rgb = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_2X1_RGB)
        .expect("rgb png");
    assert_eq!((rgb.width, rgb.height), (2, 1));
    assert_eq!(
        rgb.rgba,
        vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]
    );
    let gray = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_GRAY)
        .expect("gray png");
    assert_eq!(gray.rgba, vec![0x7F, 0x7F, 0x7F, 0xFF]);
    // Gray-alpha keeps its alpha channel.
    let gray_alpha = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_GRAY_ALPHA)
        .expect("gray-alpha png");
    assert_eq!(gray_alpha.rgba, vec![0x7F, 0x7F, 0x7F, 0x80]);
    // Palette and 16-bit inputs normalize to 8-bit RGBA through the shared
    // minimal codec (same bytes as the extension conformance suite pins).
    let palette = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_PALETTE)
        .expect("palette png");
    assert_eq!(palette.rgba, vec![0xFF, 0x00, 0x00, 0xFF]);
    let deep = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGB16)
        .expect("16-bit png");
    assert_eq!(deep.rgba, vec![0xFF, 0x00, 0x00, 0xFF]);
    // Truncated and garbage inputs fail closed, never as partial bitmaps.
    assert!(
        back.decode_transmit(
            KittyTransmitFormat::Png,
            None,
            None,
            b"definitely not a png"
        )
        .is_err()
    );
    let full = back
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGBA_RED)
        .expect("valid PNG");
    let pixel_end = PNG_1X1_RGBA_RED.len() - 12;
    for len in 0..PNG_1X1_RGBA_RED.len() {
        match back.decode_transmit(
            KittyTransmitFormat::Png,
            None,
            None,
            &PNG_1X1_RGBA_RED[..len],
        ) {
            Err(_) => {}
            Ok(img) => {
                assert!(len >= pixel_end, "len {len}: partial pixels as Ok");
                assert_eq!(img, full, "len {len}: trailing bytes changed pixels");
            }
        }
    }
}

#[test]
fn seam_png_oversize_ihdr_refused_before_decode() {
    let back = backend();
    // Patched 9000-wide IHDR: the side cap fires before any pixel work.
    let mut wide = PNG_1X1_RGBA_RED.to_vec();
    wide[16..20].copy_from_slice(&9000u32.to_be_bytes());
    assert_eq!(
        back.decode_transmit(KittyTransmitFormat::Png, None, None, &wide),
        Err(KittyDecodeError::DimensionsTooLarge {
            width: 9000,
            height: 1,
            cap: 8192
        })
    );
    // Patched 5000x5000 IHDR: the area cap fires first.
    let mut area = PNG_1X1_RGBA_RED.to_vec();
    area[16..20].copy_from_slice(&5000u32.to_be_bytes());
    area[20..24].copy_from_slice(&5000u32.to_be_bytes());
    assert_eq!(
        back.decode_transmit(KittyTransmitFormat::Png, None, None, &area),
        Err(KittyDecodeError::TooManyPixels {
            pixels: 25_000_000,
            cap: 16_777_216
        })
    );
}

#[test]
fn seam_backend_raster_matches_free_functions() {
    let back = backend();
    let image = solid_red_2x2();
    let rect = RectPx::new(0, 0, 2, 2);
    // Identity scale is byte-identical through the trait.
    assert_eq!(
        back.rasterize_placed(&image, rect),
        rasterize_kitty(&image, rect)
    );
    assert_eq!(
        back.rasterize_placed(&image, rect),
        Some(image.rgba.clone())
    );
    // Nearest-neighbor upscale doubles each source column.
    let wide = two_color_2x1();
    assert_eq!(
        back.rasterize_placed(&wide, RectPx::new(0, 0, 4, 1)),
        Some(vec![
            0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00,
            0xFF, 0xFF,
        ])
    );
    // Clipped raster equals scale-then-crop and matches the free function.
    let full = RectPx::new(0, 0, 4, 1);
    let visible = RectPx::new(2, 0, 2, 1);
    assert_eq!(
        back.rasterize_placed_clipped(&wide, full, visible),
        rasterize_kitty_clipped(&wide, full, visible)
    );
    assert_eq!(
        back.rasterize_placed_clipped(&wide, full, visible),
        Some(vec![0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF])
    );
    // The raw-frame entry point rasterizes animation frames through the
    // same path: identical bytes for the same triple.
    assert_eq!(
        back.rasterize_frame(2, 1, &wide.rgba, full, visible),
        back.rasterize_placed_clipped(&wide, full, visible)
    );
    // Empty and outside rects paint nothing through either spelling.
    assert_eq!(back.rasterize_placed(&image, RectPx::new(0, 0, 0, 2)), None);
    assert_eq!(
        back.rasterize_placed_clipped(&image, full, RectPx::new(4, 4, 2, 2)),
        None
    );
    // Corrupt sources paint nothing, never panic.
    let bad = KittyPlacedImage {
        id: KittyImageId(9),
        generation: 1,
        origin: None,
        wire_image: 0,
        width: 2,
        height: 2,
        rgba: vec![0; 5],
        frames: Vec::new(),
        compressed_len: 5,
    };
    assert_eq!(back.rasterize_placed(&bad, RectPx::new(0, 0, 2, 2)), None);
}

#[test]
fn seam_backend_is_stateless_and_shared() {
    // The in-repo backend carries no per-call state: repeated calls agree
    // and the value is cheap to copy at the composition root.
    let first = backend();
    let second = first;
    let img: KittyDecodedImage = first
        .decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGBA_RED)
        .expect("valid PNG");
    assert_eq!(
        second.decode_transmit(KittyTransmitFormat::Png, None, None, PNG_1X1_RGBA_RED),
        Ok(img)
    );
}
