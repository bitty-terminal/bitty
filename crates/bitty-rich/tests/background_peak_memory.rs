//! BG-3 peak-memory regression test (RFC-0001/OQ-042, CTX-0395).
//!
//! RFC-0001/OQ-042 requires "a pre-allocation rejection test proving peak
//! memory stays under BG-3". This integration test decodes the largest accepted
//! size through the public API while a tracking global allocator measures the
//! real heap peak, which catches any decode path that materializes a second
//! full-size buffer (for example the retired `DynamicImage::to_rgba8` clone).
//!
//! It lives in `tests/` rather than the unit-test module because `bitty-rich`
//! is `#![forbid(unsafe_code)]` and a `GlobalAlloc` implementation requires
//! `unsafe`; the integration test crate does not inherit that lint.

// The workspace lint level is `deny`; this test needs `unsafe` only for the
// tracking `GlobalAlloc` that measures the decode peak.
#![allow(unsafe_code)]

use bitty_rich::background::{
    BG_MAX_DECODED_BYTES, BG_MAX_DIMENSION, BG_MAX_ENCODED_BYTES, decode_background,
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

/// Largest square side whose BG-3 peak estimate (`side^2 * peak_bpp`) fits.
fn largest_side(peak_bytes_per_pixel: u32) -> u32 {
    (1..=BG_MAX_DIMENSION)
        .rev()
        .find(|side| {
            u64::from(*side) * u64::from(*side) * u64::from(peak_bytes_per_pixel)
                <= BG_MAX_DECODED_BYTES as u64
        })
        .expect("at least 1x1 fits")
}

#[test]
fn largest_background_decode_peak_stays_within_bg3() {
    // The decode path must hold at most one width*height*4 RGBA8 buffer at a
    // time: the retired DynamicImage::to_rgba8 clone, or a non-in-place
    // native-to-RGBA conversion, would double the peak and fail here. Each
    // accepted format is measured at its largest size that satisfies the BG-3
    // peak estimate (4 or 8 bytes per pixel, per ImageHeader).
    let side = BG_MAX_DIMENSION;
    // Encoded input (BG-1) and bounded codec scratch are alive during decode.
    let slack = BG_MAX_ENCODED_BYTES + 4 * 1024 * 1024;
    let bound = BG_MAX_DECODED_BYTES + slack;

    let webp_side = largest_side(8);
    let cases: [(&str, u32, Vec<u8>); 5] = [
        (
            "png-rgba",
            side,
            encode_png_rgba(side, side, [0x10, 0x20, 0x30, 0xFF]),
        ),
        (
            "png-rgb",
            side,
            encode_png_rgb(side, side, [0x10, 0x20, 0x30]),
        ),
        (
            "jpeg-rgb",
            side,
            encode_jpeg(side, side, [0x10, 0x20, 0x30]),
        ),
        (
            "webp-rgba",
            side,
            encode_webp_lossless(side, side, &[0x10, 0x20, 0x30, 0xFF]),
        ),
        (
            "webp-rgb",
            webp_side,
            encode_webp_lossless(webp_side, webp_side, &[0x10, 0x20, 0x30]),
        ),
    ];

    for (label, expected_side, bytes) in cases {
        assert!(
            bytes.len() <= BG_MAX_ENCODED_BYTES,
            "{label}: fixture exceeds BG-1"
        );
        let (result, peak) = peak_alloc::measure(|| decode_background(&bytes));
        let image = result.unwrap_or_else(|err| panic!("{label}: decode failed: {err}"));
        assert_eq!(
            image.dimensions(),
            (expected_side, expected_side),
            "{label}"
        );
        let resident = (expected_side as usize) * (expected_side as usize) * 4;
        assert_eq!(image.byte_len(), resident, "{label}");
        // Sanity: the tracking allocator must have observed the resident
        // buffer, otherwise a zero peak would pass the bound vacuously.
        assert!(
            peak >= resident,
            "{label}: tracked peak {peak} is below the resident {resident}"
        );
        assert!(
            peak <= bound,
            "{label}: decoded peak {peak} exceeds the BG-3 budget {bound}"
        );
        std::hint::black_box(&image);
    }
}

#[test]
fn over_budget_webp_rejected_before_allocation() {
    // A static WebP without a direct RGBA8 write is charged 8 bytes per pixel
    // (the `image-webp` internal scratch plus the output), so a 4096x4096
    // lossless RGB WebP exceeds BG-3 and must be refused before any decode
    // allocation happens.
    let side = BG_MAX_DIMENSION;
    let bytes = encode_webp_lossless(side, side, &[0x10, 0x20, 0x30]);
    let (result, peak) = peak_alloc::measure(|| decode_background(&bytes));
    let err = result.expect_err("over-budget WebP must be rejected");
    assert!(
        matches!(
            err,
            bitty_rich::background::BackgroundError::DecodedTooLarge { .. }
        ),
        "expected DecodedTooLarge, got {err:?}"
    );
    // The encoded input (BG-1) is already allocated outside the measurement;
    // a pre-allocation rejection adds no image-sized buffer.
    assert!(
        peak <= 4 * 1024 * 1024,
        "rejection allocated {peak} bytes; expected no decode buffer"
    );
}
