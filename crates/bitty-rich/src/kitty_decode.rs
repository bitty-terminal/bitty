//! Bounded Kitty payload decode: assembled bytes become RGBA8 bitmaps.
//!
//! [`crate::kitty`] performs intake only (chunked `m=` assembly under the
//! 320 MiB ledger, payloads held inert). This module is the next stage:
//! it decodes an assembled payload into an RGBA8 bitmap for the formats
//! terminals actually transmit, and fails closed on everything else.
//! Parser/APC wiring is unchanged; the caller passes the transmission
//! parameters (`f`, `s`, `v`) alongside the stored payload bytes.
//!
//! # Formats (`f=`)
//!
//! | `f` | Meaning | Dimensions from |
//! |---|---|---|
//! | `100` | PNG (DEFLATE) | PNG `IHDR` (supplied `s`/`v` ignored) |
//! | `24` | Raw 24-bit RGB, 3 bytes per pixel | Supplied `s`/`v` (required) |
//! | `32` | Raw 32-bit RGBA, 4 bytes per pixel | Supplied `s`/`v` (required) |
//!
//! Any other `f` value is rejected by
//! [`KittyTransmitFormat::from_f`] (`None`, never guessed). Raw payloads
//! must be exactly `width * height * channels` bytes; PNG payloads decode
//! through the `png` crate with `normalize_to_color8 | ALPHA` so every PNG
//! color type (palette, gray, gray-alpha, RGB, RGBA, 16-bit, interlaced)
//! normalizes to 8-bit channels before the local expansion to RGBA8.
//! Animated PNG encodes only its first frame here; animation stays deferred
//! with placement (CTX-0248).
//!
//! # Bounds (threat T-01/T-02)
//!
//! Every length is validated with checked arithmetic **before** any pixel
//! buffer is allocated:
//!
//! | Cap | Value | Rationale |
//! |---|---|---|
//! | [`KITTY_DECODE_MAX_DIMENSION`] | 8192 px/side | Wide panoramas allowed per side |
//! | [`KITTY_DECODE_MAX_PIXELS`] | 4096 x 4096 px | Area of the accepted RFC frame (IMG-2 as area) |
//! | [`KITTY_DECODE_MAX_BYTES`] | 64 MiB RGBA | Mirrors `IMG-3`; 4x+ headroom under the ledger |
//!
//! A decoded bitmap can therefore never rival stored-plus-in-flight ledger
//! pressure, and oversize input is rejected without allocating. Placement
//! (CTX-0248) re-enforces these same decode ceilings (8192 px/side,
//! 4096 x 4096 px area, 64 MiB RGBA) before any bitmap reaches the store;
//! the wire `compressed_len` is carried as diagnostics only, never as an
//! admission bound (see [`crate::kitty_place`]). The generic RFC store
//! ([`crate::image`]: IMG-1 4 MiB compressed, IMG-2 4096 px/side) is
//! stricter, but the Kitty path intentionally admits wide panoramas up to
//! 8192 px/side within the same 4096²-pixel / 64 MiB area budget, so the
//! memory ceiling is identical either way: 8192 x 2048 x 4 and 4096 x 4096
//! x 4 are both exactly 64 MiB. Tightening this path to the RFC side cap
//! would reject legitimate panoramas without lowering the memory ceiling,
//! and a wire-length cap would misfire on PNG (compressed size is unrelated
//! to decoded size; the IHDR + output-size checks above are the bomb
//! defense). The decoder attack surface gets its own review (CTX-0249).
//!
//! # Fail-closed behavior
//!
//! Every rejection returns [`KittyDecodeError`]; no path panics on
//! untrusted bytes and no partial bitmap is ever surfaced as `Ok`.
//! Truncated and garbage inputs (including every prefix of a valid PNG)
//! deterministically decode to `Err`.
//!
//! # Determinism
//!
//! Decoding is a pure function of `(format, dimensions, payload)`: the same
//! bytes always yield the same bitmap.

use std::io::Cursor;

/// Kitty `f=` value for PNG payloads.
pub const KITTY_FORMAT_PNG: u32 = 100;
/// Kitty `f=` value for raw 24-bit RGB payloads (3 bytes per pixel).
pub const KITTY_FORMAT_RGB: u32 = 24;
/// Kitty `f=` value for raw 32-bit RGBA payloads (4 bytes per pixel).
pub const KITTY_FORMAT_RGBA: u32 = 32;

/// Maximum decoded image width or height in pixels.
pub const KITTY_DECODE_MAX_DIMENSION: u32 = 8192;
/// Maximum decoded pixels (4096 x 4096 area).
///
/// Couples the two per-side ceilings into one area bound so a decoded RGBA8
/// bitmap never exceeds [`KITTY_DECODE_MAX_BYTES`]. Wide aspect ratios up to
/// [`KITTY_DECODE_MAX_DIMENSION`] per side still pass (for example
/// 8192 x 2048); anything denser than the accepted RFC frame is rejected
/// before allocation.
pub const KITTY_DECODE_MAX_PIXELS: u64 = 4096 * 4096;
/// Maximum decoded RGBA8 bytes (mirrors `IMG-3`, 5x under the ledger cap).
pub const KITTY_DECODE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Kitty transmission format (`f=` control parameter) supported for decode.
///
/// Only the formats terminals actually transmit are represented; anything
/// else fails closed at [`KittyTransmitFormat::from_f`] instead of being
/// guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyTransmitFormat {
    /// `f=100`: PNG data, dimensions read from `IHDR`.
    Png,
    /// `f=24`: raw RGB, 3 bytes per pixel, dimensions from `s`/`v`.
    Rgb,
    /// `f=32`: raw RGBA, 4 bytes per pixel, dimensions from `s`/`v`.
    Rgba,
}

impl KittyTransmitFormat {
    /// Maps a wire `f=` value to a supported format.
    ///
    /// Returns `None` for every value outside `{100, 24, 32}`; callers must
    /// reject rather than guess.
    #[must_use]
    pub const fn from_f(value: u32) -> Option<Self> {
        match value {
            KITTY_FORMAT_PNG => Some(Self::Png),
            KITTY_FORMAT_RGB => Some(Self::Rgb),
            KITTY_FORMAT_RGBA => Some(Self::Rgba),
            _ => None,
        }
    }

    /// Wire `f=` value for this format.
    #[must_use]
    pub const fn f_value(self) -> u32 {
        match self {
            Self::Png => KITTY_FORMAT_PNG,
            Self::Rgb => KITTY_FORMAT_RGB,
            Self::Rgba => KITTY_FORMAT_RGBA,
        }
    }

    /// Bytes per pixel for raw formats; `None` for PNG (IHDR governs).
    #[must_use]
    pub const fn raw_channels(self) -> Option<usize> {
        match self {
            Self::Png => None,
            Self::Rgb => Some(3),
            Self::Rgba => Some(4),
        }
    }
}

/// Decoded Kitty bitmap: owned RGBA8 pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyDecodedImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl KittyDecodedImage {
    /// Pixel width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Pixel height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// `(width, height)`.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Total pixels (`width * height`, always within
    /// [`KITTY_DECODE_MAX_PIXELS`]).
    #[must_use]
    pub fn pixel_count(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// RGBA8 bytes, row-major, exactly `width * height * 4` long.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Moves the RGBA8 bytes out (`width * height * 4` long).
    #[must_use]
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

/// Typed decode rejection.
///
/// Every variant fails closed: the caller gets `Err` and no bitmap, partial
/// or otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyDecodeError {
    /// Empty payload carries no image.
    EmptyPayload,
    /// Raw RGB/RGBA arrived without both `s` (width) and `v` (height).
    MissingDimensions,
    /// A dimension is zero (raw params or PNG `IHDR`).
    ZeroDimension,
    /// A dimension exceeds [`KITTY_DECODE_MAX_DIMENSION`]; rejected before
    /// any pixel buffer exists.
    DimensionsTooLarge {
        /// Requested (or `IHDR`) width.
        width: u32,
        /// Requested (or `IHDR`) height.
        height: u32,
        /// Side cap that refused them.
        cap: u32,
    },
    /// `width * height` exceeds [`KITTY_DECODE_MAX_PIXELS`]; rejected before
    /// any pixel buffer exists.
    TooManyPixels {
        /// Requested (or `IHDR`) pixel count.
        pixels: u64,
        /// Area cap that refused it.
        cap: u64,
    },
    /// Decoded bytes exceed [`KITTY_DECODE_MAX_BYTES`]; rejected before the
    /// pixel buffer is allocated (or grown by expansion).
    DecodedTooLarge {
        /// Bytes the bitmap would have needed (`usize::MAX` when the size
        /// computation itself overflowed).
        bytes: usize,
        /// Byte cap that refused them.
        cap: usize,
    },
    /// Raw payload length is not exactly `width * height * channels`.
    LengthMismatch {
        /// `width * height * channels`.
        expected: usize,
        /// Actual payload length.
        actual: usize,
    },
    /// The PNG stream is malformed, truncated, or uses output the decoder
    /// cannot normalize; the message is the underlying decoder diagnostic.
    /// Same bytes always produce the same message.
    MalformedPng(String),
}

impl std::fmt::Display for KittyDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPayload => write!(f, "kitty payload is empty"),
            Self::MissingDimensions => {
                write!(f, "kitty raw payload needs width and height (s/v)")
            }
            Self::ZeroDimension => write!(f, "kitty image dimension is zero"),
            Self::DimensionsTooLarge { width, height, cap } => write!(
                f,
                "kitty image {width}x{height} exceeds max dimension of {cap}px"
            ),
            Self::TooManyPixels { pixels, cap } => write!(
                f,
                "kitty image of {pixels} pixels exceeds max of {cap} pixels"
            ),
            Self::DecodedTooLarge { bytes, cap } => write!(
                f,
                "kitty decoded bitmap of {bytes} bytes exceeds max of {cap} bytes"
            ),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "kitty raw payload of {actual} bytes does not match {expected} expected bytes"
            ),
            Self::MalformedPng(detail) => write!(f, "kitty PNG is malformed: {detail}"),
        }
    }
}

impl std::error::Error for KittyDecodeError {}

/// Validates dimensions with checked arithmetic before any allocation.
///
/// Returns the pixel count. Zero, over-side, and over-area inputs are
/// rejected here so no caller can allocate from untrusted dimensions.
fn checked_dimensions(width: u32, height: u32) -> Result<u64, KittyDecodeError> {
    if width == 0 || height == 0 {
        return Err(KittyDecodeError::ZeroDimension);
    }
    if width > KITTY_DECODE_MAX_DIMENSION || height > KITTY_DECODE_MAX_DIMENSION {
        return Err(KittyDecodeError::DimensionsTooLarge {
            width,
            height,
            cap: KITTY_DECODE_MAX_DIMENSION,
        });
    }
    // No overflow is possible: both sides are at most 8192.
    let pixels = u64::from(width) * u64::from(height);
    if pixels > KITTY_DECODE_MAX_PIXELS {
        return Err(KittyDecodeError::TooManyPixels {
            pixels,
            cap: KITTY_DECODE_MAX_PIXELS,
        });
    }
    Ok(pixels)
}

/// Decodes an assembled Kitty payload into an RGBA8 bitmap.
///
/// `format` is the wire `f=` value mapped through
/// [`KittyTransmitFormat::from_f`]; `width`/`height` are the wire `s`/`v`
/// values. PNG ignores supplied dimensions (`IHDR` governs); raw formats
/// require both. Bounds run before allocation; malformed input returns
/// `Err` without panicking.
///
/// # Errors
///
/// - [`KittyDecodeError::EmptyPayload`] for empty input.
/// - [`KittyDecodeError::MissingDimensions`] for raw formats without both
///   dimensions.
/// - [`KittyDecodeError::ZeroDimension`], `DimensionsTooLarge`,
///   `TooManyPixels`, or `DecodedTooLarge` when bounds refuse the image,
///   always before allocating the refused bytes.
/// - [`KittyDecodeError::LengthMismatch`] when a raw payload is not exactly
///   `width * height * channels`.
/// - [`KittyDecodeError::MalformedPng`] for truncated or corrupt PNG data.
pub fn decode_kitty_payload(
    format: KittyTransmitFormat,
    width: Option<u32>,
    height: Option<u32>,
    payload: &[u8],
) -> Result<KittyDecodedImage, KittyDecodeError> {
    if payload.is_empty() {
        return Err(KittyDecodeError::EmptyPayload);
    }
    match format {
        KittyTransmitFormat::Png => decode_png(payload),
        KittyTransmitFormat::Rgb | KittyTransmitFormat::Rgba => {
            let (Some(w), Some(h)) = (width, height) else {
                return Err(KittyDecodeError::MissingDimensions);
            };
            // `raw_channels` is `Some` for both arms by construction.
            let channels = format.raw_channels().unwrap_or(3);
            decode_raw(w, h, channels, payload)
        }
    }
}

/// Decodes raw RGB/RGBA bytes into an RGBA8 bitmap.
///
/// All bounds (including the exact-length check) run before the RGBA
/// buffer is allocated, so the allocation size is always a validated
/// `pixels * 4 <= KITTY_DECODE_MAX_BYTES`.
fn decode_raw(
    width: u32,
    height: u32,
    channels: usize,
    payload: &[u8],
) -> Result<KittyDecodedImage, KittyDecodeError> {
    let pixels = checked_dimensions(width, height)?;
    let expected = (pixels as usize)
        .checked_mul(channels)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyDecodeError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    if payload.len() != expected {
        return Err(KittyDecodeError::LengthMismatch {
            expected,
            actual: payload.len(),
        });
    }
    // Post-validation: `pixels * 4 <= KITTY_DECODE_MAX_BYTES` holds because
    // `pixels <= KITTY_DECODE_MAX_PIXELS` (4096^2) and channels >= 3 covers
    // the RGBA expansion size.
    let rgba_len = (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyDecodeError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    let rgba = if channels == 4 {
        payload.to_vec()
    } else {
        let mut out = Vec::with_capacity(rgba_len);
        for px in payload.chunks_exact(3) {
            out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
        }
        out
    };
    debug_assert_eq!(rgba.len(), rgba_len);
    Ok(KittyDecodedImage {
        width,
        height,
        rgba,
    })
}

/// Decodes a PNG payload into an RGBA8 bitmap.
///
/// `IHDR` is parsed first and validated through [`checked_dimensions`];
/// the frame buffer is then sized from the decoder-reported output size
/// (re-checked against [`KITTY_DECODE_MAX_BYTES`]) and only then
/// allocated. Animated PNG yields its first frame; the rest is dropped
/// with the reader.
fn decode_png(payload: &[u8]) -> Result<KittyDecodedImage, KittyDecodeError> {
    let malformed = |e: png::DecodingError| KittyDecodeError::MalformedPng(e.to_string());
    let mut decoder = png::Decoder::new(Cursor::new(payload));
    // Text chunks are metadata, never pixels; skipping them keeps hostile
    // zTXt/iTXt payloads out of decoder-internal buffers.
    decoder.set_ignore_text_chunk(true);
    // Defense in depth: the decoder's own accounting refuses internal
    // buffers past the same byte ceiling our explicit checks enforce.
    decoder.set_limits(png::Limits {
        bytes: KITTY_DECODE_MAX_BYTES,
    });
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().map_err(malformed)?;
    // Header-only so far: no pixel buffer exists. Validate IHDR dims now.
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    let pixels = checked_dimensions(width, height)?;
    let buf_len = reader
        .output_buffer_size()
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyDecodeError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    let mut buf = vec![0_u8; buf_len];
    let output = reader.next_frame(&mut buf).map_err(malformed)?;
    if output.width != width || output.height != height {
        return Err(KittyDecodeError::MalformedPng(
            "frame dimensions changed mid-stream".to_owned(),
        ));
    }
    buf.truncate(output.buffer_size().min(buf.len()));
    expand_png_output(&buf, width, height, pixels, reader.output_color_type())
}

/// Expands one normalized PNG frame to RGBA8.
///
/// After `normalize_to_color8 | ALPHA` the decoder only emits 8-bit
/// grayscale, gray-alpha, RGB, or RGBA. Anything else (including a
/// surviving palette) fails closed instead of being guessed.
fn expand_png_output(
    buf: &[u8],
    width: u32,
    height: u32,
    pixels: u64,
    color_type: (png::ColorType, png::BitDepth),
) -> Result<KittyDecodedImage, KittyDecodeError> {
    use png::{BitDepth::Eight, ColorType as CT};
    let (color, depth) = color_type;
    if depth != Eight {
        return Err(KittyDecodeError::MalformedPng(format!(
            "unexpected PNG output bit depth: {depth:?}"
        )));
    }
    let channels = match color {
        CT::Grayscale => 1,
        CT::GrayscaleAlpha => 2,
        CT::Rgb => 3,
        CT::Rgba => 4,
        CT::Indexed => {
            return Err(KittyDecodeError::MalformedPng(
                "unexpected PNG palette output after expansion".to_owned(),
            ));
        }
    };
    let expected = (pixels as usize)
        .checked_mul(channels)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES && n == buf.len())
        .ok_or_else(|| {
            KittyDecodeError::MalformedPng(format!(
                "PNG frame of {} bytes does not match {pixels} {color:?} pixels",
                buf.len()
            ))
        })?;
    debug_assert_eq!(expected, buf.len());
    let rgba = match color {
        CT::Rgba => buf.to_vec(),
        CT::Rgb => {
            let mut out = Vec::with_capacity(pixels as usize * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        CT::Grayscale => {
            let mut out = Vec::with_capacity(pixels as usize * 4);
            for &g in buf {
                out.extend_from_slice(&[g, g, g, 0xFF]);
            }
            out
        }
        CT::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixels as usize * 4);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        CT::Indexed => {
            return Err(KittyDecodeError::MalformedPng(
                "unexpected PNG palette output after expansion".to_owned(),
            ));
        }
    };
    // `width`/`height` are the IHDR values `checked_dimensions` already
    // validated and `buf` was sized from; the struct carries them through.
    Ok(KittyDecodedImage {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 RGBA PNG, single red opaque pixel. Generated with Python stdlib
    /// `zlib` (no encoder dependency); `IHDR` CRC verified at generation.
    const PNG_1X1_RGBA_RED: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 2x1 RGB PNG: red then green. Exercises the opaque-alpha fill.
    const PNG_2X1_RGB: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 1, 8, 2,
        0, 0, 0, 123, 64, 232, 221, 0, 0, 0, 15, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 192,
        240, 159, 1, 0, 7, 255, 1, 255, 184, 4, 53, 224, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
        130,
    ];

    /// 1x1 grayscale PNG (`0x7F`). Exercises gray expansion.
    const PNG_1X1_GRAY: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 0,
        0, 0, 0, 58, 126, 155, 85, 0, 0, 0, 10, 73, 68, 65, 84, 120, 218, 99, 168, 7, 0, 0, 129, 0,
        128, 126, 28, 41, 199, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 gray-alpha PNG (`0x7F` at `0x80` alpha). Exercises gray+alpha.
    const PNG_1X1_GRAY_ALPHA: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4,
        0, 0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 168, 111, 0, 0, 1, 129,
        1, 0, 64, 19, 200, 179, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 palette PNG (index 0 = red). Exercises `EXPAND` to RGB.
    const PNG_1X1_PALETTE: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 3,
        0, 0, 0, 40, 203, 52, 187, 0, 0, 0, 6, 80, 76, 84, 69, 255, 0, 0, 0, 255, 0, 210, 135, 239,
        113, 0, 0, 0, 10, 73, 68, 65, 84, 120, 218, 99, 96, 0, 0, 0, 2, 0, 1, 229, 39, 222, 252, 0,
        0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 1x1 16-bit RGB PNG (red). Exercises `STRIP_16` to 8-bit RGB.
    const PNG_1X1_RGB16: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 16,
        2, 0, 0, 0, 192, 231, 143, 157, 0, 0, 0, 12, 73, 68, 65, 84, 120, 218, 99, 248, 207, 0, 2,
        0, 6, 1, 1, 0, 65, 8, 143, 241, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// Valid 1x1 PNG bytes with `IHDR` width patched to 9000 (CRC fixed).
    /// Rejected by the side cap after header parse, before pixel alloc.
    const PNG_W9000_IHDR: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 35, 40, 0, 0, 0, 1, 8,
        6, 0, 0, 0, 179, 35, 5, 69, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// Valid header with `IHDR` patched to 5000x5000 (25M px, CRC fixed).
    /// Rejected by the area cap after header parse, before pixel alloc.
    const PNG_5000X5000_IHDR: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 19, 136, 0, 0, 19, 136,
        8, 6, 0, 0, 0, 93, 152, 135, 203, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192,
        240, 31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn decode_png_default(payload: &[u8]) -> Result<KittyDecodedImage, KittyDecodeError> {
        decode_kitty_payload(KittyTransmitFormat::Png, None, None, payload)
    }

    #[test]
    fn format_mapping_is_exact() {
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
    fn png_1x1_rgba_decodes() {
        let img = decode_png_default(PNG_1X1_RGBA_RED).unwrap();
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.pixel_count(), 1);
        assert_eq!(img.rgba(), &[0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn png_rgb_gains_opaque_alpha() {
        let img = decode_png_default(PNG_2X1_RGB).unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(
            img.rgba(),
            &[0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]
        );
    }

    #[test]
    fn png_gray_expands_to_rgb() {
        let img = decode_png_default(PNG_1X1_GRAY).unwrap();
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.rgba(), &[0x7F, 0x7F, 0x7F, 0xFF]);
    }

    #[test]
    fn png_gray_alpha_keeps_alpha() {
        let img = decode_png_default(PNG_1X1_GRAY_ALPHA).unwrap();
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.rgba(), &[0x7F, 0x7F, 0x7F, 0x80]);
    }

    #[test]
    fn png_palette_and_16bit_normalize() {
        let pal = decode_png_default(PNG_1X1_PALETTE).unwrap();
        assert_eq!(pal.dimensions(), (1, 1));
        assert_eq!(pal.rgba(), &[0xFF, 0x00, 0x00, 0xFF]);
        let deep = decode_png_default(PNG_1X1_RGB16).unwrap();
        assert_eq!(deep.dimensions(), (1, 1));
        assert_eq!(deep.rgba(), &[0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn png_ignores_supplied_dimensions() {
        // IHDR governs for PNG; stale s/v must not reshape the bitmap.
        let img = decode_kitty_payload(
            KittyTransmitFormat::Png,
            Some(99),
            Some(99),
            PNG_1X1_RGBA_RED,
        )
        .unwrap();
        assert_eq!(img.dimensions(), (1, 1));
    }

    #[test]
    fn decode_is_deterministic() {
        let a = decode_png_default(PNG_2X1_RGB).unwrap();
        let b = decode_png_default(PNG_2X1_RGB).unwrap();
        assert_eq!(a, b);
        let raw = [1_u8, 2, 3, 4, 5, 6];
        let c = decode_kitty_payload(KittyTransmitFormat::Rgb, Some(2), Some(1), &raw).unwrap();
        let d = decode_kitty_payload(KittyTransmitFormat::Rgb, Some(2), Some(1), &raw).unwrap();
        assert_eq!(c, d);
    }

    #[test]
    fn raw_rgb_expands_with_opaque_alpha() {
        let img = decode_kitty_payload(
            KittyTransmitFormat::Rgb,
            Some(2),
            Some(1),
            &[10, 20, 30, 40, 50, 60],
        )
        .unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.rgba(), &[10, 20, 30, 0xFF, 40, 50, 60, 0xFF]);
        assert_eq!(img.into_rgba().len(), 8);
    }

    #[test]
    fn raw_rgba_passes_through() {
        let payload = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let img =
            decode_kitty_payload(KittyTransmitFormat::Rgba, Some(2), Some(1), &payload).unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.rgba(), &payload);
    }

    #[test]
    fn raw_requires_both_dimensions() {
        let payload = [0_u8; 12];
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgb, None, None, &payload),
            Err(KittyDecodeError::MissingDimensions)
        );
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgb, Some(2), None, &payload),
            Err(KittyDecodeError::MissingDimensions)
        );
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgba, None, Some(2), &payload),
            Err(KittyDecodeError::MissingDimensions)
        );
    }

    #[test]
    fn zero_dimension_rejected() {
        let payload = [0_u8; 4];
        for (w, h) in [(0, 1), (1, 0), (0, 0)] {
            assert_eq!(
                decode_kitty_payload(KittyTransmitFormat::Rgba, Some(w), Some(h), &payload),
                Err(KittyDecodeError::ZeroDimension),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn empty_payload_rejected_first() {
        assert_eq!(decode_png_default(&[]), Err(KittyDecodeError::EmptyPayload));
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgb, Some(1), Some(1), &[]),
            Err(KittyDecodeError::EmptyPayload)
        );
    }

    #[test]
    fn raw_length_must_match_exactly() {
        // 2x1 RGB needs exactly 6 bytes.
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgb, Some(2), Some(1), &[0; 5]),
            Err(KittyDecodeError::LengthMismatch {
                expected: 6,
                actual: 5
            })
        );
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgb, Some(2), Some(1), &[0; 7]),
            Err(KittyDecodeError::LengthMismatch {
                expected: 6,
                actual: 7
            })
        );
    }

    #[test]
    fn oversize_dimension_rejected_without_allocating() {
        // 100_000 x 100_000 would need 30 GB; the side cap fires first on a
        // 4-byte payload, proving bounds run before allocation.
        assert_eq!(
            decode_kitty_payload(
                KittyTransmitFormat::Rgb,
                Some(100_000),
                Some(100_000),
                &[0; 4]
            ),
            Err(KittyDecodeError::DimensionsTooLarge {
                width: 100_000,
                height: 100_000,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        assert_eq!(
            decode_kitty_payload(
                KittyTransmitFormat::Rgba,
                Some(KITTY_DECODE_MAX_DIMENSION + 1),
                Some(1),
                &[0; 4]
            ),
            Err(KittyDecodeError::DimensionsTooLarge {
                width: KITTY_DECODE_MAX_DIMENSION + 1,
                height: 1,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
    }

    #[test]
    fn over_area_rejected_without_allocating() {
        // 5000x5000 = 25M px > 16.7M cap; would-be 100 MB RGBA never allocs.
        assert_eq!(
            decode_kitty_payload(KittyTransmitFormat::Rgba, Some(5000), Some(5000), &[0; 8]),
            Err(KittyDecodeError::TooManyPixels {
                pixels: 25_000_000,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
    }

    #[test]
    fn png_oversize_ihdr_rejected_before_pixel_alloc() {
        assert_eq!(
            decode_png_default(PNG_W9000_IHDR),
            Err(KittyDecodeError::DimensionsTooLarge {
                width: 9000,
                height: 1,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        assert_eq!(
            decode_png_default(PNG_5000X5000_IHDR),
            Err(KittyDecodeError::TooManyPixels {
                pixels: 25_000_000,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
    }

    #[test]
    fn truncated_png_always_fails_closed() {
        // No prefix may surface a partial or wrong bitmap: every prefix
        // decodes to Err, unless the pixel stream is already complete and
        // integrity-checked (all consumed chunk CRCs verified), in which
        // case it must equal the full decode. Prefixes that cut into the
        // signature or IHDR (8 + 25 bytes) can never satisfy `read_info`.
        let full = decode_png_default(PNG_1X1_RGBA_RED).unwrap();
        for len in 0..PNG_1X1_RGBA_RED.len() {
            match decode_png_default(&PNG_1X1_RGBA_RED[..len]) {
                Err(_) => {}
                Ok(img) => {
                    assert!(
                        len >= 8 + 25,
                        "prefix of {len} bytes decoded without complete headers"
                    );
                    assert_eq!(img, full, "prefix of {len} bytes gave a partial bitmap");
                }
            }
        }
        assert!(decode_png_default(PNG_1X1_RGBA_RED).is_ok());
    }

    #[test]
    fn garbage_bytes_always_fail_closed() {
        let garbage: [&[u8]; 8] = [
            b"not a png at all, just terminal output",
            &[0_u8; 64],
            &[0xFF_u8; 64],
            &PNG_1X1_RGBA_RED[..8],
            &PNG_1X1_RGBA_RED[..33],
            b"\x89PNG\r\n\x1a\ntrailing junk, no IHDR",
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0xFF],
            b"\x1b_Gf=100,s=1,v=1;",
        ];
        for (i, input) in garbage.iter().enumerate() {
            assert!(
                decode_png_default(input).is_err(),
                "garbage[{i}] decoded as Ok"
            );
        }
        // Deterministic pseudo-random fuzz shape: same input, same error.
        let mut pseudo = [0_u8; 256];
        let mut state = 0x9E37_79B9_u32;
        for slot in pseudo.iter_mut() {
            state = state.wrapping_mul(2_654_435_761).wrapping_add(1);
            *slot = (state >> 24) as u8;
        }
        let first = decode_png_default(&pseudo);
        assert!(first.is_err());
        assert_eq!(first, decode_png_default(&pseudo));
        for len in [1_usize, 7, 8, 33, 70, 128, 255] {
            assert!(
                decode_png_default(&pseudo[..len]).is_err(),
                "pseudo[{len}] decoded as Ok"
            );
        }
    }

    #[test]
    fn malformed_png_error_is_stable() {
        let err = decode_png_default(b"garbage").unwrap_err();
        assert_eq!(err, decode_png_default(b"garbage").unwrap_err());
        assert!(err.to_string().starts_with("kitty PNG is malformed: "));
    }

    #[test]
    fn error_display() {
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
    }

    #[test]
    fn caps_document_ledger_relationship() {
        // Compile-time: decoded ceiling keeps 4x+ headroom under the ledger
        // cap (threat T-01/T-02); no bitmap rivals stored+in-flight pressure.
        const _: () = assert!(KITTY_DECODE_MAX_BYTES * 4 < crate::kitty::KITTY_LEDGER_MAX_BYTES);
        const _: () = assert!(KITTY_DECODE_MAX_PIXELS == 4096 * 4096);
        const _: () = assert!(KITTY_DECODE_MAX_PIXELS * 4 == KITTY_DECODE_MAX_BYTES as u64);
        assert_eq!(KITTY_DECODE_MAX_DIMENSION, 8192);
        assert_eq!(KITTY_DECODE_MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(KITTY_FORMAT_PNG, 100);
        assert_eq!(KITTY_FORMAT_RGB, 24);
        assert_eq!(KITTY_FORMAT_RGBA, 32);
    }
}
