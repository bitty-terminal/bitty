//! Bounded Kitty payload decode (Core-owned, issue #1802).
//!
//! The graphics extension crate implements the same decode behind
//! the Core-owned [`crate::kitty_seam`] traits (W-141 extraction); Core
//! never depends on extension internals (one-way only), so this module is
//! the Core-owned decode step the `Runtime` transmit seam calls. Bounds
//! mirror the extension exactly: 8192 px/side, 4096 x 4096 px area,
//! 64 MiB RGBA.
//!
//! # Formats (`f=`)
//!
//! | `f` | Meaning | Dimensions from |
//! |---|---|---|
//! | `100` | PNG (DEFLATE) | PNG `IHDR` (supplied `s`/`v` ignored) |
//! | `24` | Raw 24-bit RGB, 3 bytes per pixel | Supplied `s`/`v` (required) |
//! | `32` | Raw 32-bit RGBA, 4 bytes per pixel | Supplied `s`/`v` (required) |
//!
//! Any other `f` value never reaches this module: the caller maps it to
//! `UnknownFormat` first (never guessed).
//!
//! # PNG path (shared minimal codec)
//!
//! PNG decodes through the minimal `png` codec (CTX-1186): the same crate
//! and pin the graphics extension seam uses, never the full
//! `image` facade (no format autodetect, no codecs beyond PNG, no
//! color-management stack). The `IHDR` dimensions are sniffed and
//! validated **before** the decoder runs, so a hostile header is refused
//! without allocating pixels; the decoded output dimensions are
//! re-validated against the sniffed header before admission. Animated PNG
//! yields its first frame (same as the extension); animation stays
//! deferred with virtual placements.
//!
//! # Raw paths (no codec)
//!
//! Raw payloads must be exactly `width * height * channels` bytes;
//! `f=24` expands to opaque RGBA, `f=32` moves without copying on the
//! owned entry point.
//!
//! # Fail-closed behavior
//!
//! Every rejection returns [`KittyDecodeError`]; no path panics on
//! untrusted bytes and no partial bitmap is ever surfaced as `Ok`.
//! Decoding is a pure function of `(format, dimensions, payload)`.

use super::kitty_place::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS,
};

/// Typed Kitty payload decode rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyDecodeError {
    /// Empty payload carries no image.
    EmptyPayload,
    /// Raw RGB/RGBA arrived without both `s` (width) and `v` (height).
    MissingDimensions,
    /// A declared dimension is zero (raw params or PNG `IHDR`).
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
    /// `width * height` exceeds [`KITTY_DECODE_MAX_PIXELS`]; rejected
    /// before any pixel buffer exists.
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
    /// The PNG stream is malformed, truncated, not a PNG at all, or uses
    /// output the decoder cannot normalize; the message is the underlying
    /// diagnostic. Same bytes always produce the same message.
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

/// Decoded Kitty bitmap: owned RGBA8 pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyDecodedImage {
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// RGBA8 bytes, exactly `width * height * 4` long.
    pub rgba: Vec<u8>,
}

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
/// `format_f` is the wire `f=` value (`100` PNG, `24` RGB, `32` RGBA);
/// `width`/`height` are the wire `s`/`v` values. PNG ignores supplied
/// dimensions (`IHDR` governs); raw formats require both. Bounds run
/// before allocation; malformed input returns `Err` without panicking.
///
/// # Errors
///
/// [`KittyDecodeError`] variants for empty, underspecified, oversize,
/// length-mismatched, or malformed inputs. Failures admit nothing.
pub fn decode_kitty_payload(
    format_f: u32,
    width: Option<u32>,
    height: Option<u32>,
    payload: &[u8],
) -> Result<KittyDecodedImage, KittyDecodeError> {
    if payload.is_empty() {
        return Err(KittyDecodeError::EmptyPayload);
    }
    match format_f {
        super::kitty_place::KITTY_FORMAT_PNG => decode_png(payload),
        super::kitty_place::KITTY_FORMAT_RGB => {
            let (Some(w), Some(h)) = (width, height) else {
                return Err(KittyDecodeError::MissingDimensions);
            };
            decode_raw(w, h, 3, payload)
        }
        super::kitty_place::KITTY_FORMAT_RGBA => {
            let (Some(w), Some(h)) = (width, height) else {
                return Err(KittyDecodeError::MissingDimensions);
            };
            decode_raw(w, h, 4, payload)
        }
        _ => Err(KittyDecodeError::MalformedPng(format!(
            "unsupported kitty format f={format_f}"
        ))),
    }
}

/// Decodes a Kitty graphics payload from an owned byte buffer.
///
/// For `f=32` (RGBA) the payload moves directly into the returned image
/// without copying; `f=24` expands in place. PNG still decodes through
/// the image codec (the decoder needs a borrow).
pub fn decode_kitty_payload_owned(
    format_f: u32,
    width: Option<u32>,
    height: Option<u32>,
    payload: Box<[u8]>,
) -> Result<KittyDecodedImage, KittyDecodeError> {
    if payload.is_empty() {
        return Err(KittyDecodeError::EmptyPayload);
    }
    match format_f {
        super::kitty_place::KITTY_FORMAT_PNG => decode_png(&payload),
        super::kitty_place::KITTY_FORMAT_RGB => {
            let (Some(w), Some(h)) = (width, height) else {
                return Err(KittyDecodeError::MissingDimensions);
            };
            decode_raw_owned(w, h, 3, payload.into_vec())
        }
        super::kitty_place::KITTY_FORMAT_RGBA => {
            let (Some(w), Some(h)) = (width, height) else {
                return Err(KittyDecodeError::MissingDimensions);
            };
            decode_raw_owned(w, h, 4, payload.into_vec())
        }
        _ => Err(KittyDecodeError::MalformedPng(format!(
            "unsupported kitty format f={format_f}"
        ))),
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
    // `pixels <= KITTY_DECODE_MAX_PIXELS` (4096^2) bounds the RGBA
    // expansion identically.
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

/// Decodes raw RGB/RGBA bytes from an owned buffer, avoiding copies.
///
/// For RGBA the payload moves directly into the result; for RGB the
/// buffer expands to RGBA. All bounds checks run before allocation,
/// like [`decode_raw`].
fn decode_raw_owned(
    width: u32,
    height: u32,
    channels: usize,
    payload: Vec<u8>,
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
    let rgba_len = (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyDecodeError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    let rgba = if channels == 4 {
        payload
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

/// Sniffs PNG `IHDR` dimensions without allocating.
///
/// Returns `(width, height)` from the 13-byte `IHDR` that must be the
/// first chunk after the 8-byte signature. Anything else (truncated
/// header, missing `IHDR`, non-PNG bytes) fails closed as
/// [`KittyDecodeError::MalformedPng`]; zero dimensions fail as
/// [`KittyDecodeError::ZeroDimension`] so callers can distinguish them.
fn sniff_png_ihdr(payload: &[u8]) -> Result<(u32, u32), KittyDecodeError> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if payload.len() < 8 || payload[..8] != SIG {
        return Err(KittyDecodeError::MalformedPng(
            "not a PNG stream".to_owned(),
        ));
    }
    // `IHDR` must be the first chunk: length (4) + `IHDR` (4) + 13 data + CRC (4).
    if payload.len() < 8 + 8 + 13 + 4 {
        return Err(KittyDecodeError::MalformedPng(
            "truncated PNG header".to_owned(),
        ));
    }
    let len = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    if len != 13 || &payload[12..16] != b"IHDR" {
        return Err(KittyDecodeError::MalformedPng(
            "PNG IHDR must be the first 13-byte chunk".to_owned(),
        ));
    }
    let width = u32::from_be_bytes([payload[16], payload[17], payload[18], payload[19]]);
    let height = u32::from_be_bytes([payload[20], payload[21], payload[22], payload[23]]);
    if width == 0 || height == 0 {
        return Err(KittyDecodeError::ZeroDimension);
    }
    Ok((width, height))
}

/// Validates a PNG payload's `IHDR` against the decode ceilings without
/// decoding it (issue #2875 scope 4).
///
/// This is the *single* admission gate, and it is single in implementation,
/// not just in intent: the `a=q` support probe calls this wrapper and
/// `decode_png` calls the same `png_admission` body before either touches
/// the codec, so probe and transmit verdicts cannot drift. Without the gate
/// a probe answered `OK` for a PNG whose `IHDR` the decode path then
/// refuses, which tells a client "images work" and paints nothing.
///
/// It checks signature and `IHDR` placement, non-zero dimensions, the
/// per-side and area ceilings, and the RGBA8 byte cap.
///
/// # Scope
///
/// The gate is *header-only*, so it bounds what a refusal proves, not what
/// an acceptance proves: a payload with a valid `IHDR` and a corrupt or
/// truncated stream past it passes here and is refused by the codec
/// (`MalformedPng`). A probe answering `OK` therefore means "the declared
/// geometry is admissible", never "these bytes decode". Decoding inside a
/// probe would allocate the pixels the gate exists to avoid, so the
/// asymmetry is deliberate; `decode_png` stays the authority on stream
/// validity.
///
/// Pure, allocation-free, and bounded by the header bytes it reads.
///
/// # Errors
///
/// [`KittyDecodeError::MalformedPng`] for a non-PNG/truncated header,
/// [`KittyDecodeError::ZeroDimension`] for a zero side, and
/// [`KittyDecodeError::DimensionsTooLarge`]/[`KittyDecodeError::TooManyPixels`]/
/// [`KittyDecodeError::DecodedTooLarge`] past the same caps the decoder
/// enforces.
pub fn precheck_png_payload(payload: &[u8]) -> Result<(), KittyDecodeError> {
    png_admission(payload).map(|_| ())
}

/// Shared body of the PNG admission gate.
///
/// Returns the sniffed `(width, height)` plus the pixel count the RGBA8
/// expansion will need, so [`decode_png`] allocates against the same
/// numbers the gate proved admissible instead of re-deriving them.
fn png_admission(payload: &[u8]) -> Result<(u32, u32, u64), KittyDecodeError> {
    let (width, height) = sniff_png_ihdr(payload)?;
    let pixels = checked_dimensions(width, height)?;
    (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyDecodeError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    Ok((width, height, pixels))
}

/// Decodes a PNG payload into an RGBA8 bitmap.
///
/// Admission runs through `png_admission` — the same body
/// [`precheck_png_payload`] wraps for the `a=q` support probe, so probe and
/// transmit verdicts cannot drift — before the decoder runs (no pixel
/// allocation on refusal). The decoded
/// output dimensions are re-checked against the sniffed header before
/// admission, and the output byte length is checked against
/// [`KITTY_DECODE_MAX_BYTES`] before it is accepted. This function, not
/// the header gate, is the authority on stream validity: a valid `IHDR`
/// with a corrupt stream is refused here.
///
/// PNG decodes through the shared minimal `png` codec (same crate and pin
/// as the graphics extension seam, never the full `image`
/// facade): `normalize_to_color8 | ALPHA` folds every PNG color type
/// (palette, gray, gray-alpha, RGB, RGBA, 16-bit, interlaced) to 8-bit
/// channels before the local expansion to RGBA8, text chunks are skipped,
/// and the decoder's own accounting enforces the same byte ceiling.
/// Animated PNG yields its first frame.
fn decode_png(payload: &[u8]) -> Result<KittyDecodedImage, KittyDecodeError> {
    // One admission gate, shared with the `a=q` support probe: signature
    // and `IHDR` placement, non-zero sides, the per-side/area ceilings, and
    // the RGBA8 byte cap. Duplicating these checks here is exactly how the
    // probe and the transmit path drift apart.
    let (width, height, pixels) = png_admission(payload)?;
    let malformed = |err: png::DecodingError| KittyDecodeError::MalformedPng(err.to_string());
    let mut decoder = png::Decoder::new(std::io::Cursor::new(payload));
    // Text chunks are metadata, never pixels; skipping them keeps hostile
    // zTXt/iTXt payloads out of decoder-internal buffers.
    decoder.set_ignore_text_chunk(true);
    // Defense in depth: the decoder's own accounting refuses internal
    // buffers past the same byte ceiling the explicit checks enforce.
    decoder.set_limits(png::Limits {
        bytes: KITTY_DECODE_MAX_BYTES,
    });
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().map_err(malformed)?;
    let mut buf = vec![
        0_u8;
        reader
            .output_buffer_size()
            .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
            .ok_or(KittyDecodeError::DecodedTooLarge {
                bytes: usize::MAX,
                cap: KITTY_DECODE_MAX_BYTES,
            })?
    ];
    let output = reader
        .next_frame(&mut buf)
        .map_err(|err| KittyDecodeError::MalformedPng(err.to_string()))?;
    if output.width != width || output.height != height {
        return Err(KittyDecodeError::MalformedPng(
            "decoded dimensions disagree with the PNG header".to_owned(),
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
    use png::{BitDepth::Eight, ColorType as Ct};
    let (color, depth) = color_type;
    if depth != Eight {
        return Err(KittyDecodeError::MalformedPng(format!(
            "unexpected PNG output bit depth: {depth:?}"
        )));
    }
    let channels = match color {
        Ct::Grayscale => 1,
        Ct::GrayscaleAlpha => 2,
        Ct::Rgb => 3,
        Ct::Rgba => 4,
        Ct::Indexed => {
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
    // `pixels <= KITTY_DECODE_MAX_PIXELS` bounds every expansion below at
    // the byte cap, so these capacities cannot overflow `usize`.
    let capacity = pixels as usize * 4;
    let rgba = match color {
        Ct::Rgba => buf.to_vec(),
        Ct::Rgb => {
            let mut out = Vec::with_capacity(capacity);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        Ct::Grayscale => {
            let mut out = Vec::with_capacity(capacity);
            for &g in buf {
                out.extend_from_slice(&[g, g, g, 0xFF]);
            }
            out
        }
        Ct::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(capacity);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        Ct::Indexed => {
            return Err(KittyDecodeError::MalformedPng(
                "unexpected PNG palette output after expansion".to_owned(),
            ));
        }
    };
    debug_assert_eq!(rgba.len(), capacity);
    Ok(KittyDecodedImage {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 RGBA PNG, single red opaque pixel. Same fixture shape as the
    /// graphics extension suite (generated with Python stdlib
    /// `zlib`; `IHDR` CRC verified at generation).
    const PNG_1X1_RGBA_RED: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// 2x1 RGB PNG: red then green. Exercises opaque-alpha handling.
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

    #[test]
    fn png_1x1_rgba_decodes_to_red() {
        let img = decode_kitty_payload(100, None, None, PNG_1X1_RGBA_RED).expect("valid PNG");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba, vec![0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn png_ignores_supplied_dimensions() {
        // `s`/`v` are meaningless for PNG: the `IHDR` governs.
        let img =
            decode_kitty_payload(100, Some(9000), Some(9000), PNG_1X1_RGBA_RED).expect("valid PNG");
        assert_eq!((img.width, img.height), (1, 1));
    }

    #[test]
    fn png_2x1_rgb_expands_opaque() {
        let img = decode_kitty_payload(100, None, None, PNG_2X1_RGB).expect("valid PNG");
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(
            img.rgba,
            vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]
        );
    }

    #[test]
    fn png_1x1_gray_expands() {
        let img = decode_kitty_payload(100, None, None, PNG_1X1_GRAY).expect("valid PNG");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba, vec![0x7F, 0x7F, 0x7F, 0xFF]);
    }

    #[test]
    fn png_truncated_prefixes_fail_closed() {
        // Every prefix that cuts into pixel data deterministically decodes
        // to `Err`, never to a partial bitmap. Prefixes missing only
        // trailing `IEND` bytes may still decode (the pixel data is
        // complete); when they do, the bitmap must equal the full image.
        let full = decode_kitty_payload(100, None, None, PNG_1X1_RGBA_RED).expect("valid PNG");
        // `IDAT` (pixel data) ends 12 bytes before the end (`IEND` chunk).
        let pixel_end = PNG_1X1_RGBA_RED.len() - 12;
        for len in 0..PNG_1X1_RGBA_RED.len() {
            match decode_kitty_payload(100, None, None, &PNG_1X1_RGBA_RED[..len]) {
                Err(err) => assert!(
                    !matches!(err, KittyDecodeError::DecodedTooLarge { .. }),
                    "len {len}: wrong variant {err:?}"
                ),
                Ok(img) => {
                    assert!(len >= pixel_end, "len {len}: partial pixels as Ok");
                    assert_eq!(img, full, "len {len}: trailing bytes changed pixels");
                }
            }
        }
    }

    #[test]
    fn png_garbage_fails_closed() {
        let err = decode_kitty_payload(100, None, None, b"definitely not a png")
            .expect_err("garbage must fail");
        assert!(matches!(err, KittyDecodeError::MalformedPng(_)));
    }

    /// PNG signature plus a syntactically valid `IHDR` chunk claiming
    /// `width` x `height`. The CRC is not read by the pre-check or the
    /// sniff, so this needs no real image data.
    fn png_with_ihdr(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![137, 80, 78, 71, 13, 10, 26, 10];
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn precheck_png_payload_accepts_what_the_decoder_accepts() {
        precheck_png_payload(PNG_1X1_RGBA_RED).expect("1x1 RGBA must pass the gate");
        assert!(decode_kitty_payload(100, None, None, PNG_1X1_RGBA_RED).is_ok());
    }

    #[test]
    fn precheck_png_payload_refuses_what_the_decoder_refuses() {
        // Issue #2875 scope 4: the `a=q` probe gate must never answer `OK`
        // for a PNG the transmit path refuses, or a client is told images
        // work and gets a blank.
        for (w, h, want) in [
            (9000_u32, 1_u32, "side cap"),
            (1, 9000, "side cap"),
            (5000, 5000, "area cap"),
        ] {
            let bytes = png_with_ihdr(w, h);
            let precheck = precheck_png_payload(&bytes);
            let decoded = decode_kitty_payload(100, None, None, &bytes);
            assert!(
                precheck.is_err(),
                "{w}x{h} ({want}) must be refused by the gate"
            );
            assert!(
                decoded.is_err(),
                "{w}x{h} ({want}) must be refused by the decoder"
            );
            assert_eq!(
                std::mem::discriminant(&precheck.unwrap_err()),
                std::mem::discriminant(&decoded.unwrap_err()),
                "{w}x{h} ({want}): gate and decoder must agree on the refusal"
            );
        }
    }

    #[test]
    fn precheck_admits_a_header_only_png_that_the_decoder_refuses() {
        // F3 (#2875 follow-up): the gate is header-only by design, so it
        // bounds what a *refusal* proves, not what an acceptance proves. A
        // valid `IHDR` with no `IDAT`/`IEND` behind it passes the gate and
        // is refused by the codec. This is the documented asymmetry made
        // executable: if the gate ever grows a stream check (or the decoder
        // starts trusting the gate for stream validity), this flips.
        let bytes = png_with_ihdr(1, 1);
        precheck_png_payload(&bytes).expect("the gate reads only the header");
        let err = decode_png(&bytes).expect_err("the codec owns stream validity");
        assert!(
            matches!(err, KittyDecodeError::MalformedPng(_)),
            "a truncated stream is malformed, got {err:?}"
        );
    }

    #[test]
    fn precheck_png_payload_refuses_zero_and_non_png() {
        assert!(matches!(
            precheck_png_payload(&png_with_ihdr(0, 1)),
            Err(KittyDecodeError::ZeroDimension)
        ));
        assert!(matches!(
            precheck_png_payload(b"definitely not a png"),
            Err(KittyDecodeError::MalformedPng(_))
        ));
    }

    #[test]
    fn png_oversize_ihdr_refused_before_decode() {
        // Valid 1x1 bytes with `IHDR` width patched to 9000: the side cap
        // fires on the sniffed header before the decoder runs.
        let mut bytes = PNG_1X1_RGBA_RED.to_vec();
        bytes[16..20].copy_from_slice(&9000u32.to_be_bytes());
        let err = decode_kitty_payload(100, None, None, &bytes).expect_err("oversize must fail");
        assert_eq!(
            err,
            KittyDecodeError::DimensionsTooLarge {
                width: 9000,
                height: 1,
                cap: 8192
            }
        );
    }

    #[test]
    fn png_oversize_area_refused_before_decode() {
        // `IHDR` patched to 5000x5000 (25M px): the area cap fires first.
        let mut bytes = PNG_1X1_RGBA_RED.to_vec();
        bytes[16..20].copy_from_slice(&5000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&5000u32.to_be_bytes());
        let err = decode_kitty_payload(100, None, None, &bytes).expect_err("oversize must fail");
        assert_eq!(
            err,
            KittyDecodeError::TooManyPixels {
                pixels: 25_000_000,
                cap: 16_777_216
            }
        );
    }

    #[test]
    fn raw_rgba_moves_exact() {
        let payload = [0xFF, 0x00, 0x00, 0xFF].repeat(4);
        let img = decode_kitty_payload(32, Some(2), Some(2), &payload).expect("valid raw");
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.rgba, payload);
    }

    #[test]
    fn raw_rgb_expands_opaque_alpha() {
        let payload = vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let img = decode_kitty_payload(24, Some(2), Some(1), &payload).expect("valid raw");
        assert_eq!(
            img.rgba,
            vec![0x01, 0x02, 0x03, 0xFF, 0x04, 0x05, 0x06, 0xFF]
        );
    }

    #[test]
    fn raw_owned_rgba_moves_without_copy_shape() {
        let payload: Box<[u8]> = [0xFF, 0x00, 0x00, 0xFF].repeat(4).into_boxed_slice();
        let img = decode_kitty_payload_owned(32, Some(2), Some(2), payload).expect("valid raw");
        assert_eq!(img.rgba.len(), 16);
    }

    #[test]
    fn raw_requires_dimensions() {
        let err = decode_kitty_payload(32, None, Some(2), &[0; 16]).expect_err("needs s/v");
        assert_eq!(err, KittyDecodeError::MissingDimensions);
    }

    #[test]
    fn raw_length_mismatch_fails_closed() {
        let err = decode_kitty_payload(32, Some(2), Some(2), &[0; 5]).expect_err("short");
        assert_eq!(
            err,
            KittyDecodeError::LengthMismatch {
                expected: 16,
                actual: 5
            }
        );
    }

    #[test]
    fn raw_oversize_declaration_refused_before_alloc() {
        let err = decode_kitty_payload(32, Some(9000), Some(1), &[0; 4]).expect_err("oversize");
        assert_eq!(
            err,
            KittyDecodeError::DimensionsTooLarge {
                width: 9000,
                height: 1,
                cap: 8192
            }
        );
    }

    #[test]
    fn empty_payload_rejected_first() {
        for f in [100, 24, 32] {
            assert_eq!(
                decode_kitty_payload(f, Some(1), Some(1), &[]),
                Err(KittyDecodeError::EmptyPayload)
            );
        }
    }

    #[test]
    fn unknown_format_never_guessed() {
        for f in [0, 1, 7, 23, 25, 31, 33, 99, 101] {
            let err = decode_kitty_payload(f, Some(1), Some(1), &[0; 4]).expect_err("unknown f");
            assert!(matches!(err, KittyDecodeError::MalformedPng(_)), "f={f}");
        }
    }
}
