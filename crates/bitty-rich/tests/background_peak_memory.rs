//! BG-3 peak-memory regression test (RFC-0001/OQ-042, CTX-0395).
//!
//! BG-3 is enforced as an overflow-checked pre-decode charge:
//! `width * height * peak_bytes_per_pixel` must fit 64 MiB, so it is a formula
//! rather than a measured ceiling (a 4096x4096 RGBA8 output alone is exactly
//! 64 MiB, and bounded fixed overhead such as the encoded input and codec row
//! scratch sits on top). `peak_bytes_per_pixel` covers the resident RGBA8
//! buffer plus every full-size codec scratch buffer the format forces;
//! `ImageHeader` documents each value.
//!
//! This test decodes every accepted subformat at the largest size its charge
//! admits while a tracking global allocator measures the real heap peak, so a
//! full-size codec scratch that is not charged (or a second full-size output
//! buffer, like the retired `DynamicImage::to_rgba8` clone) fails here. It
//! also pins each charge constant and the exact rejection boundary.
//!
//! It lives in `tests/` rather than the unit-test module because `bitty-rich`
//! is `#![forbid(unsafe_code)]` and a `GlobalAlloc` implementation requires
//! `unsafe`; the integration test crate does not inherit that lint.
//!
//! The `image` crate can encode PNG, baseline JPEG, and lossless WebP
//! in-process, so those fixtures are built at runtime. Progressive JPEG and
//! lossy WebP have no pure-Rust encoder in the dependency tree; those
//! fixtures are committed and were generated once with libjpeg-turbo and
//! ffmpeg/libwebp from solid-color inputs:
//!
//! ```text
//! # progressive 4:2:0 color, 2896x2896:
//! cjpeg -quality 75 -progressive -sample 2x2,1x1,1x1 -outfile progressive_420_color_2896.jpg IN.ppm
//! # progressive 4:4:4 color, 2590x2590:
//! cjpeg -quality 75 -progressive -sample 1x1,1x1,1x1 -outfile progressive_444_color_2590.jpg IN.ppm
//! # progressive grayscale, 2896x2896:
//! cjpeg -quality 75 -progressive -grayscale -outfile progressive_gray_2896.jpg IN.ppm
//! # lossy WebP without alpha, 2896x2896:
//! ffmpeg -i IN.ppm -c:v libwebp -lossless 0 -q:v 75 lossy_rgb_2896.webp
//! # lossy WebP with lossless-compressed alpha, 2469x2469:
//! ffmpeg -f lavfi -i color=c=0x102030@0.5:s=2469x2469,format=rgba -frames:v 1 alpha.png
//! ffmpeg -i alpha.png -c:v libwebp -lossless 0 -q:v 75 lossy_alpha_2469.webp
//! ```

// The workspace lint level is `deny`; this test needs `unsafe` only for the
// tracking `GlobalAlloc` that measures the decode peak.
#![allow(unsafe_code)]

use bitty_rich::background::{
    BG_MAX_DECODED_BYTES, BG_MAX_DIMENSION, BG_MAX_ENCODED_BYTES, BackgroundError,
    decode_background, sniff_image,
};

/// Per-thread heap accounting. Only allocations on the measuring thread are
/// counted, so the parallel test harness cannot contaminate the peak.
mod peak_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    struct Scope;

    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }

    /// Global allocator that records this thread's live and peak heap bytes
    /// while a measurement scope is active.
    pub struct PeakAllocator;

    unsafe impl GlobalAlloc for PeakAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                record_alloc(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            record_dealloc(layout.size());
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                record_alloc(layout.size());
            }
            ptr
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                record_dealloc(layout.size());
                record_alloc(new_size);
            }
            new_ptr
        }
    }

    fn record_alloc(size: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                LIVE.with(|live| {
                    let next = live.get().saturating_add(size);
                    live.set(next);
                    PEAK.with(|peak| {
                        if next > peak.get() {
                            peak.set(next);
                        }
                    });
                });
            }
        });
    }

    fn record_dealloc(size: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                LIVE.with(|live| live.set(live.get().saturating_sub(size)));
            }
        });
    }

    /// Runs `f` while measuring this thread's heap, returning its result and
    /// the peak live-bytes observed.
    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        ACTIVE.with(|active| active.set(false));
        LIVE.with(|live| live.set(0));
        PEAK.with(|peak| peak.set(0));
        ACTIVE.with(|active| active.set(true));
        let scope = Scope;
        let out = f();
        drop(scope);
        (out, PEAK.with(|peak| peak.get()))
    }
}

#[global_allocator]
static PEAK_ALLOCATOR: peak_alloc::PeakAllocator = peak_alloc::PeakAllocator;

/// Committed fixtures for codecs the in-tree encoders cannot produce.
const PROGRESSIVE_420_COLOR_2896: &[u8] =
    include_bytes!("fixtures/bg3/progressive_420_color_2896.jpg");
const PROGRESSIVE_444_COLOR_2590: &[u8] =
    include_bytes!("fixtures/bg3/progressive_444_color_2590.jpg");
const PROGRESSIVE_GRAY_2896: &[u8] = include_bytes!("fixtures/bg3/progressive_gray_2896.jpg");
const LOSSY_RGB_2896: &[u8] = include_bytes!("fixtures/bg3/lossy_rgb_2896.webp");
const LOSSY_ALPHA_2469: &[u8] = include_bytes!("fixtures/bg3/lossy_alpha_2469.webp");

fn solid(width: u32, height: u32, bpp: usize, color: &[u8]) -> Vec<u8> {
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * bpp];
    for chunk in pixels.chunks_exact_mut(bpp) {
        chunk.copy_from_slice(color);
    }
    pixels
}

fn encode_png_rgba(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let pixels = solid(width, height, 4, &color);
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut out),
        &pixels,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .expect("png rgba encode");
    out
}

fn encode_png_rgb(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let pixels = solid(width, height, 3, &color);
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut out),
        &pixels,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    )
    .expect("png rgb encode");
    out
}

fn encode_png_gray(width: u32, height: u32, luma: u8) -> Vec<u8> {
    let pixels = solid(width, height, 1, &[luma]);
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut out),
        &pixels,
        width,
        height,
        image::ExtendedColorType::L8,
    )
    .expect("png gray encode");
    out
}

fn encode_png_gray_alpha(width: u32, height: u32, color: [u8; 2]) -> Vec<u8> {
    let pixels = solid(width, height, 2, &color);
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut out),
        &pixels,
        width,
        height,
        image::ExtendedColorType::La8,
    )
    .expect("png gray-alpha encode");
    out
}

fn encode_jpeg(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let pixels = solid(width, height, 3, &color);
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::jpeg::JpegEncoder::new(&mut out),
        &pixels,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    )
    .expect("jpeg encode");
    out
}

fn encode_webp_lossless(width: u32, height: u32, color: &[u8]) -> Vec<u8> {
    let bpp = color.len();
    let pixels = solid(width, height, bpp, color);
    let extended = if bpp == 4 {
        image::ExtendedColorType::Rgba8
    } else {
        image::ExtendedColorType::Rgb8
    };
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::webp::WebPEncoder::new_lossless(&mut out),
        &pixels,
        width,
        height,
        extended,
    )
    .expect("webp encode");
    out
}

/// Largest square side whose BG-3 charge (`side^2 * peak_bpp`) fits.
fn largest_side(peak_bytes_per_pixel: u32) -> u32 {
    (1..=BG_MAX_DIMENSION)
        .rev()
        .find(|side| {
            u64::from(*side) * u64::from(*side) * u64::from(peak_bytes_per_pixel)
                <= BG_MAX_DECODED_BYTES as u64
        })
        .expect("at least 1x1 fits")
}

/// Offset of the SOF payload's precision byte in a JPEG marker stream.
fn jpeg_sof_payload_offset(bytes: &[u8]) -> usize {
    let mut offset = 2usize;
    loop {
        while offset + 1 < bytes.len() && bytes[offset] == 0xFF && bytes[offset + 1] == 0xFF {
            offset += 1;
        }
        assert_eq!(bytes[offset], 0xFF, "JPEG marker expected");
        let marker = bytes[offset + 1];
        if marker == 0xD8 {
            offset += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return offset + 4;
        }
        offset += 2 + seg_len;
    }
}

/// Overwrites the SOF frame dimensions so a rejection probe can address a
/// size one pixel past the committed fixture without a second fixture.
fn patch_jpeg_dims(bytes: &mut [u8], width: u32, height: u32) {
    let payload = jpeg_sof_payload_offset(bytes);
    bytes[payload + 1..payload + 3].copy_from_slice(&(height as u16).to_be_bytes());
    bytes[payload + 3..payload + 5].copy_from_slice(&(width as u16).to_be_bytes());
}

/// Overwrites the dimensions of the top-level `VP8X` or `VP8 ` WebP chunk.
fn patch_webp_dims(bytes: &mut [u8], width: u32, height: u32) {
    match &bytes[12..16] {
        b"VP8X" => {
            bytes[24..27].copy_from_slice(&(width - 1).to_le_bytes()[..3]);
            bytes[27..30].copy_from_slice(&(height - 1).to_le_bytes()[..3]);
        }
        b"VP8 " => {
            bytes[26..28].copy_from_slice(&((width & 0x3FFF) as u16).to_le_bytes());
            bytes[28..30].copy_from_slice(&((height & 0x3FFF) as u16).to_le_bytes());
        }
        tag => panic!("unexpected WebP top-level chunk {tag:?}"),
    }
}

#[test]
fn decode_peak_stays_within_bg3_charge() {
    // Fixed overhead outside the charge: the encoded input (at most BG-1)
    // plus bounded codec row/upsampler scratch. 8 MiB is the accepted slack.
    let slack = BG_MAX_ENCODED_BYTES + 4 * 1024 * 1024;
    let bound = BG_MAX_DECODED_BYTES + slack;

    let cases: [(&str, u32, u32, Vec<u8>); 12] = [
        (
            "png-rgba",
            BG_MAX_DIMENSION,
            4,
            encode_png_rgba(BG_MAX_DIMENSION, BG_MAX_DIMENSION, [0x10, 0x20, 0x30, 0xFF]),
        ),
        (
            "png-rgb",
            BG_MAX_DIMENSION,
            4,
            encode_png_rgb(BG_MAX_DIMENSION, BG_MAX_DIMENSION, [0x10, 0x20, 0x30]),
        ),
        (
            "png-gray",
            BG_MAX_DIMENSION,
            4,
            encode_png_gray(BG_MAX_DIMENSION, BG_MAX_DIMENSION, 0x40),
        ),
        (
            "png-gray-alpha",
            BG_MAX_DIMENSION,
            4,
            encode_png_gray_alpha(BG_MAX_DIMENSION, BG_MAX_DIMENSION, [0x40, 0x80]),
        ),
        (
            "jpeg-baseline-rgb",
            BG_MAX_DIMENSION,
            4,
            encode_jpeg(BG_MAX_DIMENSION, BG_MAX_DIMENSION, [0x10, 0x20, 0x30]),
        ),
        (
            "webp-lossless-rgba",
            BG_MAX_DIMENSION,
            4,
            encode_webp_lossless(
                BG_MAX_DIMENSION,
                BG_MAX_DIMENSION,
                &[0x10, 0x20, 0x30, 0xFF],
            ),
        ),
        (
            "webp-lossless-rgb",
            2896,
            8,
            encode_webp_lossless(2896, 2896, &[0x10, 0x20, 0x30]),
        ),
        (
            "jpeg-progressive-420",
            2896,
            8,
            PROGRESSIVE_420_COLOR_2896.to_vec(),
        ),
        (
            "jpeg-progressive-444",
            2590,
            10,
            PROGRESSIVE_444_COLOR_2590.to_vec(),
        ),
        (
            "jpeg-progressive-gray",
            2896,
            8,
            PROGRESSIVE_GRAY_2896.to_vec(),
        ),
        ("webp-lossy-rgb", 2896, 8, LOSSY_RGB_2896.to_vec()),
        ("webp-lossy-alpha", 2469, 11, LOSSY_ALPHA_2469.to_vec()),
    ];

    for (label, side, charge, bytes) in cases {
        assert!(bytes.len() <= BG_MAX_ENCODED_BYTES, "{label}: BG-1 fixture");
        let header = sniff_image(&bytes).unwrap_or_else(|err| panic!("{label}: sniff: {err}"));
        assert_eq!(
            header.peak_bytes_per_pixel, charge,
            "{label}: pinned BG-3 charge"
        );
        assert_eq!(
            largest_side(charge),
            side,
            "{label}: pinned charge must admit exactly this side"
        );
        let (result, peak) = peak_alloc::measure(|| decode_background(&bytes));
        let image = result.unwrap_or_else(|err| panic!("{label}: decode failed: {err}"));
        assert_eq!(image.dimensions(), (side, side), "{label}");
        let resident = (side as usize) * (side as usize) * 4;
        assert_eq!(image.byte_len(), resident, "{label}");
        // Sanity: the tracking allocator must have observed the resident
        // buffer, otherwise a zero peak would pass the bound vacuously.
        assert!(
            peak >= resident,
            "{label}: tracked peak {peak} is below the resident {resident}"
        );
        assert!(
            peak <= bound,
            "{label}: decode peak {peak} exceeds the BG-3 charge plus overhead {bound}"
        );
        std::hint::black_box(&image);
    }
}

#[test]
fn charge_constants_pin_exact_rejection_boundaries() {
    struct Profile {
        label: &'static str,
        bytes: &'static [u8],
        charge: u32,
        largest: u32,
        patch: fn(&mut [u8], u32, u32),
    }

    let profiles = [
        Profile {
            label: "jpeg-progressive-420",
            bytes: PROGRESSIVE_420_COLOR_2896,
            charge: 8,
            largest: 2896,
            patch: patch_jpeg_dims,
        },
        Profile {
            label: "jpeg-progressive-444",
            bytes: PROGRESSIVE_444_COLOR_2590,
            charge: 10,
            largest: 2590,
            patch: patch_jpeg_dims,
        },
        Profile {
            label: "jpeg-progressive-gray",
            bytes: PROGRESSIVE_GRAY_2896,
            charge: 8,
            largest: 2896,
            patch: patch_jpeg_dims,
        },
        Profile {
            label: "webp-lossy-rgb",
            bytes: LOSSY_RGB_2896,
            charge: 8,
            largest: 2896,
            patch: patch_webp_dims,
        },
        Profile {
            label: "webp-lossy-alpha",
            bytes: LOSSY_ALPHA_2469,
            charge: 11,
            largest: 2469,
            patch: patch_webp_dims,
        },
    ];

    for profile in profiles {
        let mut bytes = profile.bytes.to_vec();
        let header = sniff_image(&bytes).expect("fixture sniffs");
        assert_eq!(
            header.peak_bytes_per_pixel, profile.charge,
            "{}: pinned BG-3 charge",
            profile.label
        );
        assert_eq!(
            largest_side(profile.charge),
            profile.largest,
            "{}: pinned rejection boundary",
            profile.label
        );

        // One pixel past the boundary must be refused from the header,
        // before any image-sized allocation happens.
        let over = profile.largest + 1;
        (profile.patch)(&mut bytes, over, over);
        let patched = sniff_image(&bytes).expect("patched header sniffs");
        assert_eq!(
            (patched.width, patched.height),
            (over, over),
            "{}: patched dimensions",
            profile.label
        );
        let (result, peak) = peak_alloc::measure(|| decode_background(&bytes));
        assert!(
            matches!(result, Err(BackgroundError::DecodedTooLarge { .. })),
            "{}: expected DecodedTooLarge at {over}x{over}, got {result:?}",
            profile.label
        );
        assert!(
            peak <= BG_MAX_ENCODED_BYTES,
            "{}: rejection allocated {peak} bytes",
            profile.label
        );
    }
}

#[test]
fn over_budget_webp_rejected_before_allocation() {
    // Lossless non-alpha WebP is charged 8 bytes per pixel (the image-webp
    // internal `w*h*4` scratch plus the RGBA8 output), so a 4096x4096
    // lossless RGB WebP exceeds the BG-3 charge and must be refused before
    // any decode allocation happens.
    let side = BG_MAX_DIMENSION;
    let bytes = encode_webp_lossless(side, side, &[0x10, 0x20, 0x30]);
    let (result, peak) = peak_alloc::measure(|| decode_background(&bytes));
    let err = result.expect_err("over-budget WebP must be rejected");
    assert!(
        matches!(err, BackgroundError::DecodedTooLarge { .. }),
        "expected DecodedTooLarge, got {err:?}"
    );
    // The encoded input (BG-1) is already allocated outside the measurement;
    // a pre-allocation rejection adds no image-sized buffer.
    assert!(
        peak <= 4 * 1024 * 1024,
        "rejection allocated {peak} bytes; expected no decode buffer"
    );
}
