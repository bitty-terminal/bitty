//! Bounded per-`View` background images (CTX-0347).
//!
//! Implements the accepted RFC-0001/OQ-042 Core-owned user-configuration
//! contract for `decoration.background_image` / `background_fit` /
//! `background_image_roots` and the `views.*` image fields:
//!
//! - **Path trust**: deny-by-default [`ResourcePolicy`] roots; a configured
//!   path must be absolute or `~`-anchored, canonicalize, resolve to a
//!   regular file, and stay under an approved root (symlink escapes are
//!   re-checked after canonicalization). `/proc`, `/sys`, and `/dev` are
//!   forbidden.
//! - **Formats**: PNG, JPEG (baseline and progressive), and static WebP.
//!   Animated or multi-frame containers (APNG, animated WebP, GIF) and
//!   unsupported formats are rejected by header sniff *before* any decode.
//! - **Limits**: BG-1..BG-7 alias the accepted image-store ceilings and the
//!   OQ-042 design/present bounds. BG-3 is enforced before decode as the
//!   overflow-checked charge `width * height * peak_bytes_per_pixel`, where
//!   the per-pixel charge covers the RGBA8 output plus any full-size codec
//!   scratch; bounded fixed overhead (encoded input, row/upsampler scratch)
//!   sits outside the formula.
//! - **Fit modes**: [`BackgroundFit`] `fill`/`fit`/`center`/`tile`/`stretch`
//!   geometry is pure and pixel-exact ([`fit_plan`], [`rasterize_background`]).
//! - **Caching**: [`BackgroundStore`] keys decoded images by canonical path
//!   plus content identity (length + mtime) and holds BG-4/BG-5; the
//!   [`BackgroundRasterCache`] holds scaled blits under the BG-7 byte cap.
//!   Pinned (currently displayed) images are never evicted for an
//!   unpinned admission.
//!
//! The module performs no decode at module scope and no I/O except inside
//! [`BackgroundStore::load`], which the runtime calls off the present path.

use crate::geometry::RectPx;
use crate::loader::{ResourceError, ResourcePolicy, validate_resource_path};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// BG-1: max encoded file bytes per image (aliases IMG-1).
pub const BG_MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;

/// BG-2: max decoded dimension per axis (aliases IMG-2).
pub const BG_MAX_DIMENSION: u32 = 4096;

/// BG-3: max charged decoded peak per image (aliases IMG-3). The charge is the
/// overflow-checked formula `width * height * peak_bytes_per_pixel`; see
/// [`ImageHeader`] for the per-format value.
pub const BG_MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// BG-4: max aggregate decoded background bytes (aliases IMG-4).
pub const BG_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// BG-5: max decoded background images resident (aliases IMG-5).
pub const BG_CACHE_MAX_IMAGES: usize = 256;

/// BG-6: max resident background images per `View` (design bound: one).
pub const BG_MAX_IMAGES_PER_VIEW: usize = 1;

/// BG-7: max background blits composited in one present frame.
pub const BG_PRESENT_MAX_BLITS_PER_FRAME: usize = 32;

/// BG-7: max padded staging bytes uploaded in one present frame.
pub const BG_PRESENT_MAX_BYTES_PER_FRAME: usize = 64 * 1024 * 1024;

/// Max scaled-blit bytes retained by [`BackgroundRasterCache`] (one BG-7
/// frame's worth; mirrors the Kitty raster-cache policy).
pub const BG_RASTER_CACHE_MAX_BYTES: usize = BG_PRESENT_MAX_BYTES_PER_FRAME;

/// Max `background_image` path length in bytes (RFC-0001 table: `<= 4096`).
pub const BG_MAX_PATH_BYTES: usize = 4096;

/// Max `decoration.background_image_roots` entries (RFC-0001: `at most 32`).
pub const BG_MAX_ROOTS: usize = 32;

/// Accepted background formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundFormat {
    /// PNG (APNG rejected by the header sniff).
    Png,
    /// JPEG baseline, extended sequential, or progressive.
    Jpeg,
    /// Static WebP (`VP8 `, `VP8L`, or non-animated `VP8X`).
    WebP,
}

/// Parsed image header: format plus pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    /// Sniffed container format.
    pub format: BackgroundFormat,
    /// Header width in pixels (`> 0`).
    pub width: u32,
    /// Header height in pixels (`> 0`).
    pub height: u32,
    /// Peak decoder bytes per pixel charged against BG-3: the single RGBA8
    /// output plus any full-size decoder-internal buffer the bitstream forces.
    ///
    /// `4` is the RGBA8 output alone (PNG, baseline JPEG, and WebP lossless
    /// with alpha, whose decoder writes straight into the output). `8` is
    /// charged when the decoder materializes a full-size scratch buffer on top
    /// of the output (lossy `VP8`, lossless `VP8L` without the alpha bit, and
    /// the conservative `VP8X` container). Progressive JPEG is charged its
    /// full-image coefficient store (at least `8`, higher for 4:4:4-style
    /// sampling), and a lossy `VP8 ` with an `ALPH` chunk is charged `11` for
    /// the alpha scratch. Bounded fixed overhead (the encoded input, row and
    /// upsampler scratch) sits outside this charge, so BG-3 is enforced as a
    /// formula, not as a measured literal.
    pub peak_bytes_per_pixel: u32,
}

/// Typed background-image rejection. Every variant names the failed bound or
/// trust check; no variant carries image content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundError {
    /// Path-policy rejection (empty, relative, traversal, forbidden prefix,
    /// non-regular file, outside approved roots, symlink escape).
    Resource(ResourceError),
    /// `~`-anchored path with no available home directory.
    HomeUnavailable,
    /// Filesystem error while reading the approved regular file.
    Io {
        /// Configured path (display form).
        path: String,
        /// Underlying error detail.
        detail: String,
    },
    /// Encoded length over BG-1.
    EncodedTooLarge {
        /// Actual encoded byte length.
        actual: usize,
        /// Accepted cap ([`BG_MAX_ENCODED_BYTES`]).
        cap: usize,
    },
    /// Header dimensions are zero or over BG-2.
    Dimensions {
        /// Header width.
        width: u32,
        /// Header height.
        height: u32,
    },
    /// Overflow-checked decode peak estimate over BG-3.
    DecodedTooLarge {
        /// Checked `width * height * peak_bytes_per_pixel` estimate (or
        /// `u64::MAX` on overflow).
        bytes: u64,
        /// Accepted cap ([`BG_MAX_DECODED_BYTES`]).
        cap: u64,
    },
    /// Animated or multi-frame container (APNG, animated WebP, MPO).
    Animated {
        /// Detected container spelling.
        format: &'static str,
    },
    /// Unsupported container or codec profile (SVG, GIF, AVIF, BMP, TIFF,
    /// lossless-JPEG profiles, 12-bit JPEG, ...).
    UnsupportedFormat {
        /// Human-readable refusal reason.
        detail: String,
    },
    /// Truncated or malformed header/container.
    Malformed {
        /// Human-readable refusal reason.
        detail: String,
    },
    /// Cache admission refused: even after evicting every unpinned entry the
    /// image cannot fit BG-4/BG-5 (a single over-budget image is `BG-3`).
    CacheFull,
}

impl std::fmt::Display for BackgroundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resource(err) => write!(f, "background image path denied: {err}"),
            Self::HomeUnavailable => {
                write!(
                    f,
                    "background image path uses '~' but no home directory is set"
                )
            }
            Self::Io { path, detail } => {
                write!(f, "background image read failed for {path}: {detail}")
            }
            Self::EncodedTooLarge { actual, cap } => write!(
                f,
                "background image is {actual} encoded bytes; BG-1 allows at most {cap}"
            ),
            Self::Dimensions { width, height } => write!(
                f,
                "background image is {width}x{height}; BG-2 allows at most \
                 {BG_MAX_DIMENSION}x{BG_MAX_DIMENSION}"
            ),
            Self::DecodedTooLarge { bytes, cap } => write!(
                f,
                "background image decodes to {bytes} bytes; BG-3 allows at most {cap}"
            ),
            Self::Animated { format } => {
                write!(
                    f,
                    "animated or multi-frame {format} background images are rejected"
                )
            }
            Self::UnsupportedFormat { detail } => {
                write!(f, "unsupported background image format: {detail}")
            }
            Self::Malformed { detail } => write!(f, "malformed background image: {detail}"),
            Self::CacheFull => write!(
                f,
                "background image cache cannot admit the image within BG-4/BG-5"
            ),
        }
    }
}

impl std::error::Error for BackgroundError {}

impl From<ResourceError> for BackgroundError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be16(bytes: &[u8]) -> u32 {
    u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn le16(bytes: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn le24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Sniffs `bytes` as one of the accepted static formats.
///
/// The sniff is strict and bounded: it never reads outside the buffer, never
/// allocates, and rejects malformed, truncated, unsupported, and animated
/// containers before any decoder runs. Dimensions come from the container
/// header and are checked against BG-2 by [`check_header_bounds`] (kept
/// separate so a caller can report limit failures distinctly from format
/// failures).
///
/// # Errors
///
/// [`BackgroundError::Animated`] for APNG / animated WebP / multi-frame
/// containers, [`BackgroundError::UnsupportedFormat`] for a recognized but
/// out-of-contract container (GIF, SVG, BMP, TIFF, AVIF, lossless JPEG,
/// 12-bit JPEG), and [`BackgroundError::Malformed`] for truncated or
/// inconsistent headers.
pub fn sniff_image(bytes: &[u8]) -> Result<ImageHeader, BackgroundError> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return sniff_png(bytes);
    }
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return sniff_jpeg(bytes);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return sniff_webp(bytes);
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Err(BackgroundError::Animated { format: "GIF" });
    }
    if bytes.starts_with(b"BM") {
        return Err(BackgroundError::UnsupportedFormat {
            detail: "BMP is not an accepted background format".to_string(),
        });
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Err(BackgroundError::UnsupportedFormat {
            detail: "TIFF is not an accepted background format".to_string(),
        });
    }
    let head = &bytes[..bytes.len().min(256)];
    if head.starts_with(b"<svg")
        || head.starts_with(b"<?xml")
        || head.windows(4).any(|w| w == b"<svg")
    {
        return Err(BackgroundError::UnsupportedFormat {
            detail: "SVG is not an accepted background format".to_string(),
        });
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif" {
        return Err(BackgroundError::UnsupportedFormat {
            detail: "AVIF is not an accepted background format".to_string(),
        });
    }
    Err(BackgroundError::Malformed {
        detail: "unrecognized image header".to_string(),
    })
}

fn sniff_png(bytes: &[u8]) -> Result<ImageHeader, BackgroundError> {
    let mut offset = 8usize;
    let mut header: Option<(u32, u32)> = None;
    loop {
        if offset + 8 > bytes.len() {
            break;
        }
        let len = be32(&bytes[offset..offset + 4]) as usize;
        let ctype = &bytes[offset + 4..offset + 8];
        let Some(end) = offset.checked_add(12).and_then(|v| v.checked_add(len)) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        match ctype {
            b"IHDR" => {
                if offset != 8 || len != 13 {
                    return Err(BackgroundError::Malformed {
                        detail: "PNG IHDR must be the first 13-byte chunk".to_string(),
                    });
                }
                let width = be32(&bytes[offset + 8..offset + 12]);
                let height = be32(&bytes[offset + 12..offset + 16]);
                if width == 0 || height == 0 {
                    return Err(BackgroundError::Dimensions { width, height });
                }
                let bit_depth = bytes[offset + 16];
                if !matches!(bit_depth, 1 | 2 | 4 | 8) {
                    return Err(BackgroundError::UnsupportedFormat {
                        detail: format!(
                            "PNG bit depth {bit_depth} is unsupported; only 1, 2, 4, or 8 bits per sample are accepted"
                        ),
                    });
                }
                header = Some((width, height));
            }
            b"acTL" => {
                return Err(BackgroundError::Animated { format: "APNG" });
            }
            b"IDAT" | b"IEND" => {}
            _ => {}
        }
        offset = end;
        if ctype == b"IEND" {
            break;
        }
    }
    let (width, height) = header.ok_or(BackgroundError::Malformed {
        detail: "PNG is missing a complete IHDR chunk".to_string(),
    })?;
    Ok(ImageHeader {
        format: BackgroundFormat::Png,
        width,
        height,
        peak_bytes_per_pixel: 4,
    })
}

fn sniff_jpeg(bytes: &[u8]) -> Result<ImageHeader, BackgroundError> {
    let mut offset = 2usize;
    loop {
        while offset + 1 < bytes.len() && bytes[offset] == 0xFF && bytes[offset + 1] == 0xFF {
            offset += 1;
        }
        if offset + 1 >= bytes.len() {
            return Err(BackgroundError::Malformed {
                detail: "truncated JPEG marker stream".to_string(),
            });
        }
        if bytes[offset] != 0xFF {
            return Err(BackgroundError::Malformed {
                detail: "JPEG marker expected".to_string(),
            });
        }
        let marker = bytes[offset + 1];
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => {
                offset += 2;
                continue;
            }
            0xD9 => {
                return Err(BackgroundError::Malformed {
                    detail: "JPEG ended before a frame header".to_string(),
                });
            }
            _ => {}
        }
        if offset + 4 > bytes.len() {
            return Err(BackgroundError::Malformed {
                detail: "truncated JPEG segment header".to_string(),
            });
        }
        let seg_len = be16(&bytes[offset + 2..offset + 4]) as usize;
        if seg_len < 2 {
            return Err(BackgroundError::Malformed {
                detail: "JPEG segment length underflow".to_string(),
            });
        }
        let seg_end = offset
            .checked_add(2)
            .and_then(|v| v.checked_add(seg_len))
            .ok_or(BackgroundError::Malformed {
                detail: "JPEG segment length overflow".to_string(),
            })?;
        if seg_end > bytes.len() {
            return Err(BackgroundError::Malformed {
                detail: "truncated JPEG segment".to_string(),
            });
        }
        let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            if seg_len < 8 {
                return Err(BackgroundError::Malformed {
                    detail: "JPEG frame header too short".to_string(),
                });
            }
            if !matches!(marker, 0xC0..=0xC2) {
                return Err(BackgroundError::UnsupportedFormat {
                    detail: format!(
                        "JPEG frame type 0x{marker:02X} is not baseline, extended sequential, \
                         or progressive"
                    ),
                });
            }
            let precision = bytes[offset + 4];
            if precision != 8 {
                return Err(BackgroundError::UnsupportedFormat {
                    detail: format!("{precision}-bit JPEG samples are not accepted"),
                });
            }
            let height = be16(&bytes[offset + 5..offset + 7]);
            let width = be16(&bytes[offset + 7..offset + 9]);
            if width == 0 || height == 0 {
                return Err(BackgroundError::Dimensions { width, height });
            }
            let peak_bytes_per_pixel = if marker == 0xC2 {
                progressive_jpeg_charge(bytes, offset, seg_len)?
            } else {
                4
            };
            return Ok(ImageHeader {
                format: BackgroundFormat::Jpeg,
                width,
                height,
                peak_bytes_per_pixel,
            });
        }
        if marker == 0xDA {
            return Err(BackgroundError::Malformed {
                detail: "JPEG scan started before a frame header".to_string(),
            });
        }
        if marker == 0xE2 && seg_len >= 2 && bytes[offset + 4..].starts_with(b"MPF\0") {
            return Err(BackgroundError::Animated {
                format: "MPO (JPEG)",
            });
        }
        offset = seg_end;
    }
}

/// BG-3 charge for one progressive JPEG (SOF2) frame.
///
/// zune-jpeg, the decoder behind `image`'s JPEG path, keeps a full-image
/// `i16` coefficient buffer for every component of a progressive frame until
/// the final scans run (`mcu_prog.rs`, `block[i] = vec![0; ...]`). Its size is
/// `2 * sum(h_i * v_i) / (h_max * v_max)` bytes per pixel, charged on top of
/// the `4`-byte RGBA8 output. A floor of 8 covers the measured 4:2:0 profile
/// (7.04 B/px including row/upsampler scratch) and the grayscale profile
/// (6.01 B/px); 4:4:4 profiles are charged their full 10.
fn progressive_jpeg_charge(
    bytes: &[u8],
    offset: usize,
    seg_len: usize,
) -> Result<u32, BackgroundError> {
    let components = usize::from(bytes[offset + 9]);
    if components == 0 || components > 4 || seg_len < 8 + 3 * components {
        return Err(BackgroundError::Malformed {
            detail: "progressive JPEG frame header has an invalid component table".to_string(),
        });
    }
    let mut sampling_area = 0u32;
    let mut max_horizontal = 0u32;
    let mut max_vertical = 0u32;
    for index in 0..components {
        let sampling = bytes[offset + 10 + 3 * index + 1];
        let horizontal = u32::from(sampling >> 4);
        let vertical = u32::from(sampling & 0x0F);
        if !(1..=4).contains(&horizontal) || !(1..=4).contains(&vertical) {
            return Err(BackgroundError::Malformed {
                detail: "progressive JPEG component sampling factor out of range".to_string(),
            });
        }
        sampling_area += horizontal * vertical;
        max_horizontal = max_horizontal.max(horizontal);
        max_vertical = max_vertical.max(vertical);
    }
    let coefficient_bytes = 2 * sampling_area.div_ceil(max_horizontal * max_vertical);
    Ok((4 + coefficient_bytes).max(8))
}

fn sniff_webp(bytes: &[u8]) -> Result<ImageHeader, BackgroundError> {
    let declared = le32(&bytes[4..8]) as usize;
    if declared
        .checked_add(8)
        .is_none_or(|total| total > bytes.len())
    {
        return Err(BackgroundError::Malformed {
            detail: "truncated WebP RIFF container".to_string(),
        });
    }
    if bytes.len() < 20 {
        return Err(BackgroundError::Malformed {
            detail: "truncated WebP chunk header".to_string(),
        });
    }
    let chunk_len = le32(&bytes[16..20]) as usize;
    let chunk_end = 20usize
        .checked_add(chunk_len)
        .ok_or(BackgroundError::Malformed {
            detail: "WebP chunk length overflow".to_string(),
        })?;
    if chunk_end > bytes.len() {
        return Err(BackgroundError::Malformed {
            detail: "truncated WebP chunk data".to_string(),
        });
    }
    let data = &bytes[20..chunk_end];
    match &bytes[12..16] {
        b"VP8X" => {
            if chunk_len < 10 {
                return Err(BackgroundError::Malformed {
                    detail: "WebP VP8X chunk too short".to_string(),
                });
            }
            let flags = data[0];
            if flags & 0x02 != 0 {
                return Err(BackgroundError::Animated { format: "WebP" });
            }
            let width = 1 + le24(&data[4..7]);
            let height = 1 + le24(&data[7..10]);
            if width == 0 || height == 0 {
                return Err(BackgroundError::Dimensions { width, height });
            }
            let peak_bytes_per_pixel =
                vp8x_peak_bytes_per_pixel(bytes, declared, chunk_end, chunk_len)?;
            Ok(ImageHeader {
                format: BackgroundFormat::WebP,
                width,
                height,
                peak_bytes_per_pixel,
            })
        }
        b"VP8 " => {
            if chunk_len < 10 {
                return Err(BackgroundError::Malformed {
                    detail: "WebP VP8 frame header too short".to_string(),
                });
            }
            if data[3..6] != [0x9D, 0x01, 0x2A] {
                return Err(BackgroundError::Malformed {
                    detail: "WebP VP8 sync code mismatch".to_string(),
                });
            }
            let width = le16(&data[6..8]) & 0x3FFF;
            let height = le16(&data[8..10]) & 0x3FFF;
            if width == 0 || height == 0 {
                return Err(BackgroundError::Dimensions { width, height });
            }
            Ok(ImageHeader {
                format: BackgroundFormat::WebP,
                width,
                height,
                peak_bytes_per_pixel: 8,
            })
        }
        b"VP8L" => {
            if chunk_len < 5 || data[0] != 0x2F {
                return Err(BackgroundError::Malformed {
                    detail: "WebP VP8L signature mismatch".to_string(),
                });
            }
            let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            if width == 0 || height == 0 {
                return Err(BackgroundError::Dimensions { width, height });
            }
            // The VP8L alpha bit (bit 28) means the decoder writes RGBA8
            // straight into our bounded buffer; without it `image-webp`
            // decodes through an internal full-size RGBA scratch buffer.
            let has_alpha = (bits >> 28) & 1 != 0;
            Ok(ImageHeader {
                format: BackgroundFormat::WebP,
                width,
                height,
                peak_bytes_per_pixel: if has_alpha { 4 } else { 8 },
            })
        }
        b"ANIM" => Err(BackgroundError::Animated { format: "WebP" }),
        _ => Err(BackgroundError::UnsupportedFormat {
            detail: "unrecognized WebP chunk".to_string(),
        }),
    }
}

/// BG-3 charge for a static `VP8X` extended WebP.
///
/// `VP8X` is a container: its payload is a RIFF chunk sequence (`ICCP`,
/// `ALPH`, `VP8 `/`VP8L`, metadata). The measured worst case is a lossy
/// `VP8 ` image with an `ALPH` chunk: `image-webp`'s `read_alpha_chunk`
/// allocates a full-size `w * h * 4` RGBA scratch plus a `w * h` green buffer
/// for the alpha plane on top of the VP8 YUV frame and our RGBA8 output
/// (measured 10.50-10.76 B/px), so that shape is charged 11. Every other
/// `VP8X` shape keeps the previous conservative 8.
///
/// The walk scans the whole physical buffer, not just the declared container:
/// `image-webp` reads chunk headers up to
/// `position + riff_size.saturating_sub(12)` (ten bytes past `riff_end` for a
/// minimum-size `VP8X` chunk), so a declared size shorter than the chunk
/// sequence would let the decoder find a trailing `VP8 ` that a
/// `riff_end`-bounded walk never sees. A chunk that crosses `riff_end` is
/// therefore rejected as malformed instead of treated as absent. The only
/// remaining gap is a sub-8-byte tail, which cannot hold a chunk header.
///
/// # Errors
///
/// [`BackgroundError::Animated`] for an `ANIM`/`ANMF` chunk and
/// [`BackgroundError::Malformed`] for a chunk that overruns the declared
/// RIFF container, so an unparseable container never silently falls back to
/// the cheaper charge.
fn vp8x_peak_bytes_per_pixel(
    bytes: &[u8],
    declared: usize,
    chunk_end: usize,
    chunk_len: usize,
) -> Result<u32, BackgroundError> {
    let riff_end = declared + 8;
    let position_after_vp8x = chunk_end + (chunk_len & 1);
    if position_after_vp8x > riff_end {
        return Err(BackgroundError::Malformed {
            detail: "WebP VP8X chunk overruns the declared RIFF container".to_string(),
        });
    }
    let mut position = position_after_vp8x;
    let mut has_alpha = false;
    let mut has_lossy = false;
    while position + 8 <= bytes.len() {
        let tag = &bytes[position..position + 4];
        let len = le32(&bytes[position + 4..position + 8]) as usize;
        let data_end = position
            .checked_add(8)
            .and_then(|start| start.checked_add(len))
            .ok_or_else(|| BackgroundError::Malformed {
                detail: "WebP chunk length overflow".to_string(),
            })?;
        if data_end > bytes.len() {
            return Err(BackgroundError::Malformed {
                detail: "truncated WebP chunk in VP8X container".to_string(),
            });
        }
        if data_end > riff_end {
            return Err(BackgroundError::Malformed {
                detail: "WebP chunk overruns the declared RIFF container".to_string(),
            });
        }
        match tag {
            b"ALPH" => has_alpha = true,
            b"VP8 " => has_lossy = true,
            b"ANIM" | b"ANMF" => return Err(BackgroundError::Animated { format: "WebP" }),
            _ => {}
        }
        position = data_end + (len & 1);
    }
    Ok(if has_alpha && has_lossy { 11 } else { 8 })
}

/// Enforces BG-2 and the checked BG-3 peak estimate for one parsed header.
///
/// # Errors
///
/// [`BackgroundError::Dimensions`] when an axis is zero or over
/// [`BG_MAX_DIMENSION`], [`BackgroundError::DecodedTooLarge`] when the
/// overflow-checked `width * height * peak_bytes_per_pixel` charge exceeds
/// [`BG_MAX_DECODED_BYTES`]. The charge follows
/// [`ImageHeader::peak_bytes_per_pixel`]: `4` bytes per pixel for a direct
/// RGBA8 write, more where the format forces a full-size decoder-internal
/// scratch buffer. It bounds image-sized allocations; bounded fixed overhead
/// (encoded input, row and upsampler scratch) sits outside the formula.
pub fn check_header_bounds(header: &ImageHeader) -> Result<(), BackgroundError> {
    if header.width == 0
        || header.height == 0
        || header.width > BG_MAX_DIMENSION
        || header.height > BG_MAX_DIMENSION
    {
        return Err(BackgroundError::Dimensions {
            width: header.width,
            height: header.height,
        });
    }
    let bytes = u64::from(header.width)
        .checked_mul(u64::from(header.height))
        .and_then(|pixels| pixels.checked_mul(u64::from(header.peak_bytes_per_pixel)))
        .unwrap_or(u64::MAX);
    if bytes > BG_MAX_DECODED_BYTES as u64 {
        return Err(BackgroundError::DecodedTooLarge {
            bytes,
            cap: BG_MAX_DECODED_BYTES as u64,
        });
    }
    Ok(())
}

/// Expands a configured `background_image` path to an absolute path.
///
/// Accepts absolute paths and `~` / `~/...` (home injected so tests stay
/// hermetic). `~user` forms and relative paths fail closed; a `~` path with
/// no home directory fails closed.
///
/// # Errors
///
/// [`BackgroundError::Malformed`] for empty/NUL/over-long spellings,
/// [`BackgroundError::HomeUnavailable`] for a `~` path without home, and
/// [`BackgroundError::Resource`] with
/// [`ResourceError::OutsideApprovedRoot`](crate::loader::ResourceError) for
/// relative paths.
pub fn expand_background_path(raw: &str, home: Option<&Path>) -> Result<PathBuf, BackgroundError> {
    if raw.is_empty() {
        return Err(BackgroundError::Malformed {
            detail: "background image path must not be empty".to_string(),
        });
    }
    if raw.len() > BG_MAX_PATH_BYTES {
        return Err(BackgroundError::Malformed {
            detail: format!("background image path exceeds {BG_MAX_PATH_BYTES} bytes"),
        });
    }
    if raw.contains('\0') {
        return Err(BackgroundError::Malformed {
            detail: "background image path must not contain NUL".to_string(),
        });
    }
    if raw == "~" {
        let home = home.ok_or(BackgroundError::HomeUnavailable)?;
        return Ok(home.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = home.ok_or(BackgroundError::HomeUnavailable)?;
        return Ok(home.join(rest));
    }
    if raw.starts_with('~') {
        return Err(BackgroundError::UnsupportedFormat {
            detail: "'~user' background image paths are not accepted".to_string(),
        });
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        let shown = path.display().to_string();
        return Err(BackgroundError::Resource(
            ResourceError::OutsideApprovedRoot {
                path: shown.clone(),
                canonical: shown,
            },
        ));
    }
    Ok(path)
}

/// One decoded background image (straight-alpha RGBA8, row-major).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl BackgroundImage {
    /// Dimensions in pixels.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Decoded bytes (`width * height * 4`).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }

    /// Straight-alpha RGBA8 pixels.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Builds an image, validating the byte length against the dims.
    ///
    /// # Errors
    ///
    /// [`BackgroundError::Malformed`] when the byte length does not equal
    /// `width * height * 4` (checked arithmetic).
    pub fn try_new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, BackgroundError> {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&n| n <= usize::MAX as u64);
        if width == 0 || height == 0 || expected.is_none_or(|n| n as usize != rgba.len()) {
            return Err(BackgroundError::Malformed {
                detail: "background image bytes do not match its declared dimensions".to_string(),
            });
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }
}

/// Cache identity for one approved background file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackgroundKey {
    /// Canonical absolute path (symlinks resolved).
    pub canonical: PathBuf,
    /// File length in bytes at resolution time.
    pub len: u64,
    /// File modification time at resolution time (`None` when unavailable).
    pub modified: Option<SystemTime>,
}

impl BackgroundKey {
    fn from_metadata(canonical: PathBuf, meta: &std::fs::Metadata) -> Self {
        Self {
            canonical,
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// Decoded-image store: deny-by-default roots, BG-1..BG-3 enforced at load,
/// BG-4/BG-5 at admission, and identity-keyed reuse (`loads()` counts decodes
/// so tests can prove an unchanged file is reused and a changed one
/// re-decodes).
#[derive(Debug)]
pub struct BackgroundStore {
    policy: ResourcePolicy,
    entries: Vec<(BackgroundKey, Arc<BackgroundImage>)>,
    pinned: Vec<BackgroundKey>,
    bytes: usize,
    loads: u64,
}

impl BackgroundStore {
    /// Creates a store over an approved-root policy.
    #[must_use]
    pub fn new(policy: ResourcePolicy) -> Self {
        Self {
            policy,
            entries: Vec::new(),
            pinned: Vec::new(),
            bytes: 0,
            loads: 0,
        }
    }

    /// Deny-by-default store (no approved roots; every load denied).
    #[must_use]
    pub fn deny_all() -> Self {
        Self::new(ResourcePolicy::deny_all())
    }

    /// Approved-root policy.
    #[must_use]
    pub fn policy(&self) -> &ResourcePolicy {
        &self.policy
    }

    /// Decoded images currently resident.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no decoded image is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Aggregate decoded bytes resident.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.bytes
    }

    /// Successful decode count (identity-mismatched reloads included).
    #[must_use]
    pub fn loads(&self) -> u64 {
        self.loads
    }

    /// Marks `key` as currently displayed; pinned images are never evicted
    /// for an unpinned admission.
    pub fn pin(&mut self, key: &BackgroundKey) {
        if !self.pinned.contains(key) {
            self.pinned.push(key.clone());
        }
    }

    /// Clears every pin (called when the displayed set is recomputed).
    pub fn unpin_all(&mut self) {
        self.pinned.clear();
    }

    /// Resident image for `key`, if present and identity-current.
    #[must_use]
    pub fn get(&self, key: &BackgroundKey) -> Option<Arc<BackgroundImage>> {
        self.entries
            .iter()
            .find(|(resident, _)| resident == key)
            .map(|(_, image)| Arc::clone(image))
    }

    /// Resolves, validates, decodes, and admits `raw` (or reuses the resident
    /// identity), returning the cache key to pass to [`Self::get`].
    ///
    /// The full accepted pipeline runs in order: path syntax, canonicalize +
    /// approved-root + regular-file trust, encoded length BG-1, header sniff,
    /// BG-2 dimensions, checked BG-3 estimate, format/animation check, then
    /// decode and BG-4/BG-5/BG-6 admission. Any failure leaves the store
    /// unchanged.
    ///
    /// # Errors
    ///
    /// [`BackgroundError`] naming the failed trust check, bound, or format.
    pub fn load(
        &mut self,
        raw: &str,
        home: Option<&Path>,
    ) -> Result<BackgroundKey, BackgroundError> {
        let expanded = expand_background_path(raw, home)?;
        let canonical = validate_resource_path(&expanded, &self.policy)?;
        let meta = std::fs::metadata(&canonical).map_err(|err| BackgroundError::Io {
            path: canonical.display().to_string(),
            detail: err.to_string(),
        })?;
        if meta.len() > BG_MAX_ENCODED_BYTES as u64 {
            return Err(BackgroundError::EncodedTooLarge {
                actual: usize::try_from(meta.len()).unwrap_or(usize::MAX),
                cap: BG_MAX_ENCODED_BYTES,
            });
        }
        let key = BackgroundKey::from_metadata(canonical, &meta);
        if self.get(&key).is_some() {
            return Ok(key);
        }
        let bytes = self.read_file(&key.canonical)?;
        let image = decode_background(&bytes)?;
        self.admit(key.clone(), image)?;
        self.loads = self.loads.saturating_add(1);
        Ok(key)
    }

    fn read_file(&self, canonical: &Path) -> Result<Vec<u8>, BackgroundError> {
        use std::io::Read;
        let file = std::fs::File::open(canonical).map_err(|err| BackgroundError::Io {
            path: canonical.display().to_string(),
            detail: err.to_string(),
        })?;
        let mut buf = Vec::new();
        file.take(BG_MAX_ENCODED_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(|err| BackgroundError::Io {
                path: canonical.display().to_string(),
                detail: err.to_string(),
            })?;
        if buf.len() > BG_MAX_ENCODED_BYTES {
            return Err(BackgroundError::EncodedTooLarge {
                actual: buf.len(),
                cap: BG_MAX_ENCODED_BYTES,
            });
        }
        Ok(buf)
    }

    fn admit(&mut self, key: BackgroundKey, image: BackgroundImage) -> Result<(), BackgroundError> {
        let bytes = image.byte_len();
        if bytes > BG_CACHE_MAX_BYTES {
            return Err(BackgroundError::CacheFull);
        }
        while self.entries.len() >= BG_CACHE_MAX_IMAGES
            || self.bytes.saturating_add(bytes) > BG_CACHE_MAX_BYTES
        {
            let Some(index) = self
                .entries
                .iter()
                .position(|(resident, _)| !self.pinned.contains(resident))
            else {
                return Err(BackgroundError::CacheFull);
            };
            let (_, evicted) = self.entries.remove(index);
            self.bytes = self.bytes.saturating_sub(evicted.byte_len());
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push((key, Arc::new(image)));
        Ok(())
    }
}

/// Decodes a fully read and previously sniffed buffer into RGBA8.
///
/// The header sniff runs first, so this function never sees an animated or
/// unsupported container. The resident decode is a single `width * height * 4`
/// allocation: the per-format [`image::ImageDecoder`] writes its native 8-bit
/// channels into that buffer's prefix and [`expand_in_place`] grows them to
/// straight-alpha RGBA8. Full-size codec scratch (progressive JPEG
/// coefficients, WebP alpha planes) is charged separately by
/// [`ImageHeader::peak_bytes_per_pixel`] and bounded by [`check_header_bounds`]
/// before this runs, so no image-sized allocation is uncharged. The previous
/// `DynamicImage::to_rgba8` path cloned even an already-RGBA8 image, peaking
/// at twice the resident buffer.
///
/// Dimensions and the decoder's native color type are re-checked before any
/// allocation so a decoder that disagrees with its header fails closed, and
/// every native type other than 8-bit `L8`/`La8`/`Rgb8`/`Rgba8` is refused.
///
/// # Errors
///
/// [`BackgroundError::`] format/bound/malformed variants from
/// [`sniff_image`], [`check_header_bounds`], or a refused decode.
pub fn decode_background(bytes: &[u8]) -> Result<BackgroundImage, BackgroundError> {
    let header = sniff_image(bytes)?;
    check_header_bounds(&header)?;
    let rgba = decode_rgba8_bounded(bytes, &header)?;
    BackgroundImage::try_new(header.width, header.height, rgba)
}

/// Native channel width for the accepted 8-bit decoder color types.
fn native_bytes_per_pixel(color: image::ColorType) -> Option<usize> {
    match color {
        image::ColorType::L8 => Some(1),
        image::ColorType::La8 => Some(2),
        image::ColorType::Rgb8 => Some(3),
        image::ColorType::Rgba8 => Some(4),
        _ => None,
    }
}

/// Decodes one sniffed buffer into a single bounded RGBA8 allocation.
///
/// The caller has already validated BG-2 dimensions and the BG-3 estimate, so
/// `width * height * 4` is at most [`BG_MAX_DECODED_BYTES`]. The output buffer
/// is allocated once at that size; the decoder writes its native channels into
/// the prefix and the conversion runs in place.
fn decode_rgba8_bounded(bytes: &[u8], header: &ImageHeader) -> Result<Vec<u8>, BackgroundError> {
    use image::ImageDecoder;

    let malformed = |err: image::ImageError| BackgroundError::Malformed {
        detail: format!("decode refused: {err}"),
    };
    let reader = std::io::Cursor::new(bytes);
    let decoder: Box<dyn ImageDecoder> = match header.format {
        BackgroundFormat::Png => {
            Box::new(image::codecs::png::PngDecoder::new(reader).map_err(malformed)?)
        }
        BackgroundFormat::Jpeg => {
            Box::new(image::codecs::jpeg::JpegDecoder::new(reader).map_err(malformed)?)
        }
        BackgroundFormat::WebP => {
            Box::new(image::codecs::webp::WebPDecoder::new(reader).map_err(malformed)?)
        }
    };

    let (width, height) = decoder.dimensions();
    if width != header.width || height != header.height {
        return Err(BackgroundError::Malformed {
            detail: "decoded dimensions disagree with the header".to_string(),
        });
    }
    let color = decoder.color_type();
    let Some(native_bpp) = native_bytes_per_pixel(color) else {
        return Err(BackgroundError::UnsupportedFormat {
            detail: format!("decoder produced unsupported color type {color:?}"),
        });
    };
    let pixels =
        (width as usize)
            .checked_mul(height as usize)
            .ok_or(BackgroundError::DecodedTooLarge {
                bytes: u64::MAX,
                cap: BG_MAX_DECODED_BYTES as u64,
            })?;
    let rgba_len = pixels
        .checked_mul(4)
        .ok_or(BackgroundError::DecodedTooLarge {
            bytes: u64::MAX,
            cap: BG_MAX_DECODED_BYTES as u64,
        })?;
    let native_len = pixels * native_bpp;
    if decoder.total_bytes() != native_len as u64 {
        return Err(BackgroundError::Malformed {
            detail: "decoder reported an inconsistent decoded size".to_string(),
        });
    }

    let mut buf = vec![0u8; rgba_len];
    decoder
        .read_image(&mut buf[..native_len])
        .map_err(malformed)?;
    expand_in_place(&mut buf, pixels, native_bpp);
    Ok(buf)
}

/// Expands interleaved 8-bit native channels to straight-alpha RGBA8 in place.
///
/// Iterating back to front keeps every write at or after the source byte it
/// replaces, so no second buffer is needed. `rgba` must hold `pixels * 4` bytes
/// with the `pixels * native_bpp` source bytes at the front.
fn expand_in_place(rgba: &mut [u8], pixels: usize, native_bpp: usize) {
    match native_bpp {
        4 => {}
        3 => {
            for i in (0..pixels).rev() {
                let (r, g, b) = (rgba[i * 3], rgba[i * 3 + 1], rgba[i * 3 + 2]);
                rgba[i * 4] = r;
                rgba[i * 4 + 1] = g;
                rgba[i * 4 + 2] = b;
                rgba[i * 4 + 3] = 0xFF;
            }
        }
        2 => {
            for i in (0..pixels).rev() {
                let (l, a) = (rgba[i * 2], rgba[i * 2 + 1]);
                rgba[i * 4] = l;
                rgba[i * 4 + 1] = l;
                rgba[i * 4 + 2] = l;
                rgba[i * 4 + 3] = a;
            }
        }
        1 => {
            for i in (0..pixels).rev() {
                let l = rgba[i];
                rgba[i * 4] = l;
                rgba[i * 4 + 1] = l;
                rgba[i * 4 + 2] = l;
                rgba[i * 4 + 3] = 0xFF;
            }
        }
        _ => {}
    }
}

/// Accepted fit mode (RFC-0001/OQ-042).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BackgroundFit {
    /// Cover: uniform scale to cover the content rect, crop overflow.
    #[default]
    Fill,
    /// Contain: uniform scale to fit inside the content rect, letterbox.
    Fit,
    /// Native (DPI-scaled) size, centered, cropped or letterboxed.
    Center,
    /// Native (DPI-scaled) size repeated from the top-left, no scaling.
    Tile,
    /// Non-uniform scale to exactly fill the content rect.
    Stretch,
}

impl BackgroundFit {
    /// Parses a canonical fit name (exact, case-sensitive).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fill" => Some(Self::Fill),
            "fit" => Some(Self::Fit),
            "center" => Some(Self::Center),
            "tile" => Some(Self::Tile),
            "stretch" => Some(Self::Stretch),
            _ => None,
        }
    }

    /// Canonical spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Fit => "fit",
            Self::Center => "center",
            Self::Tile => "tile",
            Self::Stretch => "stretch",
        }
    }
}

/// One background paint plan: destination rectangle (device px) plus the
/// source window in image pixels. `tile` re-samples the full source with
/// wrap-around at `tile` size (also image pixels, already DPI-scaled).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitPlan {
    /// Destination rectangle in device pixels (inside the content rect).
    pub dest: RectPx,
    /// Source window `[x, y, width, height]` in image pixels.
    pub src: [f64; 4],
    /// Whether the source repeats from the top-left of `dest`.
    pub tile: bool,
    /// Tile width in image pixels (native size scaled by DPI); `0` when
    /// `tile` is false.
    pub tile_w: u32,
    /// Tile height in image pixels; `0` when `tile` is false.
    pub tile_h: u32,
}

/// Rounds a centering offset to the nearest device pixel (saturating cast).
fn centered_offset(outer: f64, inner: f64) -> i32 {
    let offset = ((outer - inner) * 0.5).round();
    if offset.is_finite() { offset as i32 } else { 0 }
}

/// Computes the accepted fit-mode geometry for one content rect.
///
/// `dpi` scales the image's native pixel size to device pixels for the
/// `center` and `tile` modes (the contract's "scaled by the `Window` DPI
/// factor"); `fill`, `fit`, and `stretch` absorb any factor. `None` for a
/// degenerate content rect or image; non-finite `dpi` degrades to `1.0`.
#[must_use]
pub fn fit_plan(fit: BackgroundFit, image: (u32, u32), dest: RectPx, dpi: f64) -> Option<FitPlan> {
    let (iw, ih) = image;
    if iw == 0 || ih == 0 || dest.width == 0 || dest.height == 0 {
        return None;
    }
    let dpi = if dpi.is_finite() && dpi > 0.0 {
        dpi
    } else {
        1.0
    };
    let iw_f = f64::from(iw);
    let ih_f = f64::from(ih);
    let dw = f64::from(dest.width);
    let dh = f64::from(dest.height);
    let native_w = iw_f * dpi;
    let native_h = ih_f * dpi;
    match fit {
        BackgroundFit::Fill => {
            let factor = (dw / native_w).max(dh / native_h);
            let visible_w = dw / factor;
            let visible_h = dh / factor;
            Some(FitPlan {
                dest,
                src: [
                    (iw_f - visible_w) * 0.5,
                    (ih_f - visible_h) * 0.5,
                    visible_w,
                    visible_h,
                ],
                tile: false,
                tile_w: 0,
                tile_h: 0,
            })
        }
        BackgroundFit::Fit => {
            let factor = (dw / native_w).min(dh / native_h);
            let out_w = (native_w * factor).round().clamp(1.0, dw);
            let out_h = (native_h * factor).round().clamp(1.0, dh);
            let x = dest.x.saturating_add(centered_offset(dw, out_w));
            let y = dest.y.saturating_add(centered_offset(dh, out_h));
            Some(FitPlan {
                dest: RectPx::new(x, y, out_w as u32, out_h as u32),
                src: [0.0, 0.0, iw_f, ih_f],
                tile: false,
                tile_w: 0,
                tile_h: 0,
            })
        }
        BackgroundFit::Center => {
            let out_w = native_w.round().clamp(1.0, dw);
            let out_h = native_h.round().clamp(1.0, dh);
            let x = dest.x.saturating_add(centered_offset(dw, out_w));
            let y = dest.y.saturating_add(centered_offset(dh, out_h));
            let src_w = (out_w / dpi).min(iw_f);
            let src_h = (out_h / dpi).min(ih_f);
            Some(FitPlan {
                dest: RectPx::new(x, y, out_w as u32, out_h as u32),
                src: [
                    (iw_f - src_w) * 0.5,
                    (ih_f - src_h) * 0.5,
                    src_w.max(1.0),
                    src_h.max(1.0),
                ],
                tile: false,
                tile_w: 0,
                tile_h: 0,
            })
        }
        BackgroundFit::Tile => Some(FitPlan {
            dest,
            src: [0.0, 0.0, iw_f, ih_f],
            tile: true,
            tile_w: (native_w.round() as u32).max(1),
            tile_h: (native_h.round() as u32).max(1),
        }),
        BackgroundFit::Stretch => Some(FitPlan {
            dest,
            src: [0.0, 0.0, iw_f, ih_f],
            tile: false,
            tile_w: 0,
            tile_h: 0,
        }),
    }
}

/// One rasterized background blit: exact destination plus straight-alpha
/// RGBA8 bytes (`dest.area() * 4` long).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundBlit {
    /// Destination rectangle in device pixels.
    pub dest: RectPx,
    /// Straight-alpha RGBA8 bytes, row-major.
    pub rgba: Vec<u8>,
}

/// Rasterizes `image` for `fit` into the `dest` content rect (nearest
/// neighbor, deterministic).
///
/// Returns `None` when the rect or image is degenerate or when the padded
/// blit would exceed [`BG_PRESENT_MAX_BYTES_PER_FRAME`] (never allocates
/// over the bound). `fill`/`fit`/`center`/`stretch` emit one blit possibly
/// smaller than `dest` (the uncovered area keeps the pane background);
/// `tile` emits one content-sized blit with the pattern repeated from the
/// top-left.
#[must_use]
pub fn rasterize_background(
    image: &BackgroundImage,
    fit: BackgroundFit,
    dest: RectPx,
    dpi: f64,
) -> Option<BackgroundBlit> {
    let plan = fit_plan(fit, image.dimensions(), dest, dpi)?;
    let bytes = u64::from(plan.dest.width)
        .checked_mul(u64::from(plan.dest.height))
        .and_then(|pixels| pixels.checked_mul(4))?;
    if bytes > BG_PRESENT_MAX_BYTES_PER_FRAME as u64 {
        return None;
    }
    let (iw, ih) = image.dimensions();
    let out_w = plan.dest.width as usize;
    let out_h = plan.dest.height as usize;
    let mut rgba = vec![0u8; out_w.checked_mul(out_h)?.checked_mul(4)?];
    let src = image.rgba();
    let sample = |sx: usize, sy: usize, out: &mut [u8]| {
        let index = (sy * iw as usize + sx) * 4;
        out.copy_from_slice(&src[index..index + 4]);
    };
    if plan.tile {
        let tw = plan.tile_w as usize;
        let th = plan.tile_h as usize;
        for py in 0..out_h {
            let sy = ((py * ih as usize) / th) % ih as usize;
            for px in 0..out_w {
                let sx = ((px * iw as usize) / tw) % iw as usize;
                let out = &mut rgba[(py * out_w + px) * 4..(py * out_w + px) * 4 + 4];
                sample(sx, sy, out);
            }
        }
    } else {
        let [sx0, sy0, sw, sh] = plan.src;
        for py in 0..out_h {
            let ty = sy0 + (py as f64 + 0.5) * sh / out_h as f64;
            let sy = (ty.floor() as i64).clamp(0, ih as i64 - 1) as usize;
            for px in 0..out_w {
                let tx = sx0 + (px as f64 + 0.5) * sw / out_w as f64;
                let sx = (tx.floor() as i64).clamp(0, iw as i64 - 1) as usize;
                let out = &mut rgba[(py * out_w + px) * 4..(py * out_w + px) * 4 + 4];
                sample(sx, sy, out);
            }
        }
    }
    Some(BackgroundBlit {
        dest: plan.dest,
        rgba,
    })
}

/// Cache key for one scaled background blit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackgroundRasterKey {
    /// Source image identity.
    pub source: BackgroundRasterKeySource,
    /// Fit mode.
    pub fit: BackgroundFit,
    /// Destination content rect.
    pub dest: RectPx,
    /// DPI factor bits (`f64::to_bits`; `Eq`-comparable).
    pub dpi_bits: u64,
}

/// Identity-only projection of [`BackgroundKey`] (path, length, mtime) for
/// use as a hashable raster-cache source key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackgroundRasterKeySource {
    /// Canonical absolute path.
    pub canonical: PathBuf,
    /// File length at resolution time.
    pub len: u64,
    /// Modification time at resolution time.
    pub modified: Option<SystemTime>,
}

impl From<&BackgroundKey> for BackgroundRasterKeySource {
    fn from(value: &BackgroundKey) -> Self {
        Self {
            canonical: value.canonical.clone(),
            len: value.len,
            modified: value.modified,
        }
    }
}

/// Bounded scaled-blit cache (oldest-first eviction, BG-7 byte cap).
///
/// Static frames must not re-scale a decoded image every present tick; the
/// cache retains the scaled bytes for an unchanged `(source, fit, dest,
/// dpi)` key. `hits`/`misses` are exposed so tests can prove reuse and
/// invalidation.
#[derive(Debug, Default)]
pub struct BackgroundRasterCache {
    entries: Vec<(BackgroundRasterKey, Arc<BackgroundBlit>)>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

impl BackgroundRasterCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resident scaled bytes.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.bytes
    }

    /// Resident blit count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no blit is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Cache hits (no rasterization).
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Cache misses (rasterized).
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Drops every entry whose source identity no longer matches any key in
    /// `live` (called when the displayed set is recomputed).
    pub fn retain_sources(&mut self, live: &[BackgroundRasterKeySource]) {
        self.entries.retain(|(key, blit)| {
            let keep = live.contains(&key.source);
            if !keep {
                self.bytes = self.bytes.saturating_sub(blit.rgba.len());
            }
            keep
        });
    }

    /// Returns the cached blit for `key`, rasterizing and admitting it on a
    /// miss. A blit larger than the cache cap is returned uncached.
    pub fn get_or_rasterize(
        &mut self,
        key: BackgroundRasterKey,
        image: &BackgroundImage,
    ) -> Option<Arc<BackgroundBlit>> {
        if let Some((_, blit)) = self.entries.iter().find(|(resident, _)| *resident == key) {
            self.hits = self.hits.saturating_add(1);
            return Some(Arc::clone(blit));
        }
        self.misses = self.misses.saturating_add(1);
        let blit = Arc::new(rasterize_background(
            image,
            key.fit,
            key.dest,
            f64::from_bits(key.dpi_bits),
        )?);
        let bytes = blit.rgba.len();
        if bytes <= BG_RASTER_CACHE_MAX_BYTES {
            while self.bytes.saturating_add(bytes) > BG_RASTER_CACHE_MAX_BYTES
                || self.entries.len() >= BG_CACHE_MAX_IMAGES
            {
                if self.entries.is_empty() {
                    break;
                }
                let (_, evicted) = self.entries.remove(0);
                self.bytes = self.bytes.saturating_sub(evicted.rgba.len());
            }
            self.bytes = self.bytes.saturating_add(bytes);
            self.entries.push((key, Arc::clone(&blit)));
        }
        Some(blit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(suffix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bitty-ctx0347-bg-{}-{}",
            suffix,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn encode_png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&color);
        }
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &pixels,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("png encode");
        out
    }

    fn encode_png_16(width: u32, height: u32, color: [u16; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4 * 2) as usize);
        for _ in 0..(width * height) {
            for channel in color {
                pixels.extend_from_slice(&channel.to_ne_bytes());
            }
        }
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &pixels,
            width,
            height,
            image::ExtendedColorType::Rgba16,
        )
        .expect("png16 encode");
        out
    }

    fn encode_jpeg(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&color);
        }
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &pixels,
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .expect("jpeg encode");
        out
    }

    fn solid(width: u32, height: u32, bpp: usize, color: &[u8]) -> Vec<u8> {
        let mut pixels = vec![0u8; (width as usize) * (height as usize) * bpp];
        for chunk in pixels.chunks_exact_mut(bpp) {
            chunk.copy_from_slice(color);
        }
        pixels
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

    fn encode_webp_lossless(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let pixels = solid(width, height, 4, &color);
        let mut out = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::webp::WebPEncoder::new_lossless(&mut out),
            &pixels,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("webp encode");
        out
    }

    fn write_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    fn policy_for(root: &Path) -> ResourcePolicy {
        ResourcePolicy::with_max_roots(vec![root.to_path_buf()], BG_MAX_ROOTS).expect("policy")
    }

    #[test]
    fn accepted_formats_sniff_and_decode() {
        let png = encode_png(3, 2, [0x10, 0x20, 0x30, 0xFF]);
        let header = sniff_image(&png).expect("png sniff");
        assert_eq!(header.format, BackgroundFormat::Png);
        assert_eq!((header.width, header.height), (3, 2));
        let image = decode_background(&png).expect("png decode");
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.byte_len(), 3 * 2 * 4);

        let jpeg = encode_jpeg(4, 5, [0xAA, 0xBB, 0xCC]);
        let header = sniff_image(&jpeg).expect("jpeg sniff");
        assert_eq!(header.format, BackgroundFormat::Jpeg);
        assert_eq!((header.width, header.height), (4, 5));
        assert!(decode_background(&jpeg).is_ok());

        let webp = encode_webp_lossless(6, 7, [0x01, 0x02, 0x03, 0x80]);
        let header = sniff_image(&webp).expect("webp sniff");
        assert_eq!(header.format, BackgroundFormat::WebP);
        assert_eq!((header.width, header.height), (6, 7));
        assert!(decode_background(&webp).is_ok());
    }

    #[test]
    fn native_channel_expansion_matches_rgba8() {
        // Every accepted 8-bit native decoder layout must expand to
        // straight-alpha RGBA8: L8, La8, Rgb8, and Rgba8.
        let gray = encode_png_gray(2, 1, 0x40);
        let image = decode_background(&gray).expect("gray decode");
        assert_eq!(
            image.rgba(),
            &[0x40, 0x40, 0x40, 0xFF, 0x40, 0x40, 0x40, 0xFF]
        );

        let gray_alpha = encode_png_gray_alpha(1, 1, [0x20, 0x80]);
        let image = decode_background(&gray_alpha).expect("gray+alpha decode");
        assert_eq!(image.rgba(), &[0x20, 0x20, 0x20, 0x80]);

        let rgb = encode_png_rgb(2, 1, [0x11, 0x22, 0x33]);
        let image = decode_background(&rgb).expect("rgb decode");
        assert_eq!(
            image.rgba(),
            &[0x11, 0x22, 0x33, 0xFF, 0x11, 0x22, 0x33, 0xFF]
        );

        let rgba = encode_png(1, 1, [0x01, 0x02, 0x03, 0x04]);
        let image = decode_background(&rgba).expect("rgba decode");
        assert_eq!(image.rgba(), &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn sixteen_bit_png_rejected_before_decode() {
        // BG-3 budgets 4 bytes per pixel (RGBA8); a 16-bit PNG decodes at
        // 12 bytes per pixel, so it must fail closed from the header.
        let png = encode_png_16(256, 256, [0x10, 0x20, 0x30, 0xFFFF]);
        assert!(matches!(
            sniff_image(&png),
            Err(BackgroundError::UnsupportedFormat { detail }) if detail.contains("bit depth")
        ));
        assert!(matches!(
            decode_background(&png),
            Err(BackgroundError::UnsupportedFormat { .. })
        ));
    }

    fn png_header_with_bit_depth(bit_depth: u8) -> Vec<u8> {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.push(bit_depth);
        ihdr.push(3); // palette color type
        ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        png.extend_from_slice(&(ihdr.len() as u32).to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&[0, 0, 0, 0]); // CRC, not verified while sniffing
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0, 0, 0, 0]);
        png
    }

    #[test]
    fn sub_eight_bit_png_still_sniffs() {
        // Only sample widths above 8 bits exceed the RGBA8 budget; 1/2/4-bit
        // palette and grayscale PNGs stay accepted from the header.
        for depth in [1u8, 2, 4] {
            let png = png_header_with_bit_depth(depth);
            let header = sniff_image(&png).expect("sub-8-bit sniff");
            assert_eq!(header.format, BackgroundFormat::Png);
            assert_eq!((header.width, header.height), (2, 1));
        }
    }

    #[test]
    fn invalid_png_bit_depth_rejected_at_sniff() {
        // PNG permits only sample depths 1, 2, 4, 8, and 16; 16 is already
        // rejected as unsupported, and an out-of-set depth such as 3 must fail
        // closed at sniff instead of reaching the decoder.
        for depth in [3u8, 5, 6, 7, 9, 0] {
            let png = png_header_with_bit_depth(depth);
            assert!(
                matches!(
                    sniff_image(&png),
                    Err(BackgroundError::UnsupportedFormat { detail }) if detail.contains("bit depth")
                ),
                "depth {depth} must be rejected at sniff"
            );
        }
    }

    #[test]
    fn unsupported_and_malformed_formats_rejected() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
        assert!(matches!(
            sniff_image(gif),
            Err(BackgroundError::Animated { .. })
        ));
        let bmp = b"BM\x00\x00\x00\x00";
        assert!(matches!(
            sniff_image(bmp),
            Err(BackgroundError::UnsupportedFormat { .. })
        ));
        let tiff = b"II*\0payload";
        assert!(matches!(
            sniff_image(tiff),
            Err(BackgroundError::UnsupportedFormat { .. })
        ));
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert!(matches!(
            sniff_image(svg),
            Err(BackgroundError::UnsupportedFormat { .. })
        ));
        let avif = b"\0\0\0\x18ftypavif";
        assert!(matches!(
            sniff_image(avif),
            Err(BackgroundError::UnsupportedFormat { .. })
        ));
        assert!(matches!(
            sniff_image(b"not an image"),
            Err(BackgroundError::Malformed { .. })
        ));
        assert!(matches!(
            sniff_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Err(BackgroundError::Malformed { .. })
        ));
    }

    #[test]
    fn apng_and_animated_webp_rejected_by_sniff_before_decode() {
        let png = encode_png(1, 1, [0, 0, 0, 0xFF]);
        let mut apng = Vec::with_capacity(png.len() + 12);
        apng.extend_from_slice(&png[..33]);
        apng.extend_from_slice(&13u32.to_be_bytes());
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&[0; 13]);
        apng.extend_from_slice(&[0; 4]);
        apng.extend_from_slice(&png[33..]);
        assert!(matches!(
            decode_background(&apng),
            Err(BackgroundError::Animated { format: "APNG" })
        ));

        let mut animated = Vec::new();
        animated.extend_from_slice(b"RIFF");
        animated.extend_from_slice(&22u32.to_le_bytes());
        animated.extend_from_slice(b"WEBP");
        animated.extend_from_slice(b"VP8X");
        animated.extend_from_slice(&10u32.to_le_bytes());
        animated.push(0x02);
        animated.extend_from_slice(&[0; 3]);
        animated.extend_from_slice(&[0x0F, 0x00, 0x00]);
        animated.extend_from_slice(&[0x0F, 0x00, 0x00]);
        assert!(matches!(
            sniff_image(&animated),
            Err(BackgroundError::Animated { format: "WebP" })
        ));
    }

    /// Builds a JPEG marker stream with one SOF segment and the given
    /// `(horizontal, vertical)` sampling factors.
    fn jpeg_with_sof(marker: u8, components: &[(u8, u8)]) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, marker];
        let seg_len = 2 + 6 + 3 * components.len();
        bytes.extend_from_slice(&(seg_len as u16).to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.push(components.len() as u8);
        for (index, (horizontal, vertical)) in components.iter().enumerate() {
            bytes.push(index as u8 + 1);
            bytes.push((horizontal << 4) | vertical);
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn progressive_jpeg_charge_follows_coefficient_store() {
        // 4:2:0: sum(h*v)=6 over a 2x2 MCU -> 2*ceil(6/4)=4 extra -> 8.
        let h420 = jpeg_with_sof(0xC2, &[(2, 2), (1, 1), (1, 1)]);
        assert_eq!(sniff_image(&h420).expect("420").peak_bytes_per_pixel, 8);
        // 4:4:4: sum=3 -> 2*3=6 extra -> 10.
        let h444 = jpeg_with_sof(0xC2, &[(1, 1), (1, 1), (1, 1)]);
        assert_eq!(sniff_image(&h444).expect("444").peak_bytes_per_pixel, 10);
        // 4:2:2: sum=4 over a 2x1 MCU -> 4 extra -> 8.
        let h422 = jpeg_with_sof(0xC2, &[(2, 1), (1, 1), (1, 1)]);
        assert_eq!(sniff_image(&h422).expect("422").peak_bytes_per_pixel, 8);
        // Grayscale: sum=1 -> 2 extra -> 6, floored to 8 for the unchanged
        // 4:2:0/4:4:4-and-below acceptance rule.
        let gray = jpeg_with_sof(0xC2, &[(1, 1)]);
        assert_eq!(sniff_image(&gray).expect("gray").peak_bytes_per_pixel, 8);
        // Four full-resolution components -> 8 extra -> 12.
        let cmyk = jpeg_with_sof(0xC2, &[(1, 1), (1, 1), (1, 1), (1, 1)]);
        assert_eq!(sniff_image(&cmyk).expect("cmyk").peak_bytes_per_pixel, 12);
        // Baseline and extended sequential keep the single-buffer charge.
        for marker in [0xC0u8, 0xC1] {
            let sequential = jpeg_with_sof(marker, &[(2, 2), (1, 1), (1, 1)]);
            assert_eq!(
                sniff_image(&sequential)
                    .expect("sequential")
                    .peak_bytes_per_pixel,
                4
            );
        }
    }

    #[test]
    fn progressive_jpeg_rejects_malformed_component_table() {
        // No components at all.
        let zero = jpeg_with_sof(0xC2, &[]);
        assert!(matches!(
            sniff_image(&zero),
            Err(BackgroundError::Malformed { .. })
        ));
        // More components than the decoder can hold.
        let five = jpeg_with_sof(0xC2, &[(1, 1); 5]);
        assert!(matches!(
            sniff_image(&five),
            Err(BackgroundError::Malformed { .. })
        ));
        // Zero sampling factor.
        let zero_sampling = jpeg_with_sof(0xC2, &[(0, 1), (1, 1), (1, 1)]);
        assert!(matches!(
            sniff_image(&zero_sampling),
            Err(BackgroundError::Malformed { .. })
        ));
        // Sampling factor above the JPEG maximum of four.
        let over_sampling = jpeg_with_sof(0xC2, &[(1, 5), (1, 1), (1, 1)]);
        assert!(matches!(
            sniff_image(&over_sampling),
            Err(BackgroundError::Malformed { .. })
        ));
        // A declared component count that does not fit the segment length.
        let mut truncated = jpeg_with_sof(0xC2, &[(1, 1), (1, 1), (1, 1)]);
        truncated[4..6].copy_from_slice(&8u16.to_be_bytes());
        assert!(matches!(
            sniff_image(&truncated),
            Err(BackgroundError::Malformed { .. })
        ));
    }

    /// Builds a static `VP8X` container with the given flags and payload
    /// chunks.
    fn webp_with_vp8x(flags: u8, chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WEBP");

        let mut vp8x = vec![flags];
        vp8x.extend_from_slice(&[0; 3]);
        vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
        vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&(vp8x.len() as u32).to_le_bytes());
        body.extend_from_slice(&vp8x);

        for (tag, payload) in chunks {
            body.extend_from_slice(*tag);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn vp8x_lossy_alpha_charges_alpha_scratch() {
        let alpha = [0x01u8];
        let vp8 = [0x00u8; 10];
        let vp8l = [0x2Fu8; 5];
        // Lossy VP8 plus an ALPH chunk: image-webp allocates a full-size RGBA
        // scratch and a green buffer for the alpha plane.
        let lossy_alpha = webp_with_vp8x(0x10, &[(b"ALPH", &alpha), (b"VP8 ", &vp8)]);
        assert_eq!(
            sniff_image(&lossy_alpha)
                .expect("lossy alpha")
                .peak_bytes_per_pixel,
            11
        );
        // Lossy VP8 without alpha keeps the conservative 8.
        let lossy = webp_with_vp8x(0x00, &[(b"VP8 ", &vp8)]);
        assert_eq!(sniff_image(&lossy).expect("lossy").peak_bytes_per_pixel, 8);
        // Lossless VP8L keeps the conservative 8 even with an ALPH chunk.
        let lossless_alpha = webp_with_vp8x(0x10, &[(b"ALPH", &alpha), (b"VP8L", &vp8l)]);
        assert_eq!(
            sniff_image(&lossless_alpha)
                .expect("lossless alpha")
                .peak_bytes_per_pixel,
            8
        );
        // Metadata chunks before the image data do not disturb the walk.
        let with_iccp =
            webp_with_vp8x(0x10, &[(b"ICCP", &vp8), (b"ALPH", &alpha), (b"VP8 ", &vp8)]);
        assert_eq!(
            sniff_image(&with_iccp)
                .expect("iccp lossy alpha")
                .peak_bytes_per_pixel,
            11
        );
    }

    #[test]
    fn vp8x_malformed_chunk_walk_fails_closed() {
        let mut overrun = webp_with_vp8x(0x00, &[(b"ALPH", &[0x01u8])]);
        // The ALPH chunk starts at 30; an impossible length must be refused
        // instead of silently charging the cheaper profile.
        overrun[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            sniff_image(&overrun),
            Err(BackgroundError::Malformed { .. })
        ));

        let anim = webp_with_vp8x(0x00, &[(b"ANIM", &[0; 6])]);
        assert!(matches!(
            sniff_image(&anim),
            Err(BackgroundError::Animated { format: "WebP" })
        ));

        // A declared RIFF size that ends before the trailing `VP8 ` chunk used
        // to stop the walk early (charge 8) while `image-webp`'s ten-byte
        // wider `max_position` still decoded it as lossy-alpha. The declared
        // size must cover the chunk sequence: ALPH data ends at 39, its pad
        // ends the next position, and `VP8 ` spans 40..58, so every declared
        // size in 31..=39 hides the chunk from a bounded walk and must fail
        // closed instead.
        let lossy_alpha = webp_with_vp8x(0x10, &[(b"ALPH", &[0x01u8]), (b"VP8 ", &[0u8; 10])]);
        assert_eq!(lossy_alpha.len(), 58);
        for declared in [31u32, 32, 39] {
            let mut patched = lossy_alpha.clone();
            patched[4..8].copy_from_slice(&declared.to_le_bytes());
            assert!(
                matches!(
                    sniff_image(&patched),
                    Err(BackgroundError::Malformed { .. })
                ),
                "declared {declared}: chunk crossing the declared end must fail closed"
            );
        }
        // A declared size shorter than the VP8X header chunk itself is
        // rejected instead of being trusted.
        let mut shrunken = webp_with_vp8x(0x00, &[]);
        shrunken[4..8].copy_from_slice(&10u32.to_le_bytes());
        assert!(matches!(
            sniff_image(&shrunken),
            Err(BackgroundError::Malformed { .. })
        ));
        // Trailing bytes past the declared container that parse as a chunk
        // are rejected too, not silently ignored.
        let mut trailing = lossy_alpha.clone();
        trailing.extend_from_slice(&[0u8; 8]);
        assert!(matches!(
            sniff_image(&trailing),
            Err(BackgroundError::Malformed { .. })
        ));
        // The unmodified container still charges the lossy-alpha profile.
        assert_eq!(
            sniff_image(&lossy_alpha)
                .expect("lossy alpha")
                .peak_bytes_per_pixel,
            11
        );
    }

    #[test]
    fn over_bg2_dimensions_rejected_from_header_only() {
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&(BG_MAX_DIMENSION + 1).to_be_bytes());
        png.extend_from_slice(&(BG_MAX_DIMENSION + 1).to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&[0; 4]);
        assert!(matches!(
            decode_background(&png),
            Err(BackgroundError::Dimensions { .. })
        ));
    }

    #[test]
    fn over_bg1_file_rejected_without_decode() {
        let root = temp_root("bg1");
        let path = write_file(&root, "big.png", &vec![0u8; BG_MAX_ENCODED_BYTES + 1]);
        let mut store = BackgroundStore::new(policy_for(&root));
        let err = store
            .load(path.to_str().expect("utf8 path"), None)
            .expect_err("over BG-1");
        assert!(matches!(err, BackgroundError::EncodedTooLarge { .. }));
        assert_eq!(store.loads(), 0);
    }

    #[test]
    fn root_trust_denies_outside_symlink_escape_and_non_regular() {
        let approved = temp_root("trust-approved");
        let outside = temp_root("trust-outside");
        let approved_png = write_file(&approved, "ok.png", &encode_png(2, 2, [1, 2, 3, 4]));
        let outside_png = write_file(&outside, "no.png", &encode_png(2, 2, [1, 2, 3, 4]));
        let link = approved.join("escape.png");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_png, &link).expect("symlink");

        let mut store = BackgroundStore::new(policy_for(&approved));
        assert!(
            store
                .load(approved_png.to_str().expect("utf8"), None)
                .is_ok()
        );
        let denied = store
            .load(outside_png.to_str().expect("utf8"), None)
            .expect_err("outside root");
        assert!(matches!(denied, BackgroundError::Resource(_)));
        #[cfg(unix)]
        {
            let escaped = store
                .load(link.to_str().expect("utf8"), None)
                .expect_err("symlink escape");
            assert!(matches!(escaped, BackgroundError::Resource(_)));
        }
        let dir = write_file(&approved, "dir.png", b"");
        let _ = std::fs::remove_file(&dir);
        std::fs::create_dir(&dir).expect("dir");
        let denied = store
            .load(dir.to_str().expect("utf8"), None)
            .expect_err("directory");
        assert!(matches!(denied, BackgroundError::Resource(_)));

        let relative = store.load("relative/one.png", None).expect_err("relative");
        assert!(matches!(relative, BackgroundError::Resource(_)));
        let tilde = store.load("~someone/one.png", None).expect_err("~user");
        assert!(matches!(tilde, BackgroundError::UnsupportedFormat { .. }));
        let no_home = store.load("~/one.png", None).expect_err("no home");
        assert!(matches!(no_home, BackgroundError::HomeUnavailable));
    }

    #[test]
    fn home_expansion_uses_injected_home() {
        let home = temp_root("home");
        let path = expand_background_path("~/wall/one.png", Some(&home)).expect("expand");
        assert_eq!(path, home.join("wall/one.png"));
        assert_eq!(
            expand_background_path("~", Some(&home)).expect("bare tilde"),
            home
        );
    }

    #[test]
    fn cache_reuses_identity_and_redecodes_on_change() {
        let root = temp_root("identity");
        let path = write_file(&root, "one.png", &encode_png(2, 2, [9, 8, 7, 0xFF]));
        let mut store = BackgroundStore::new(policy_for(&root));
        let raw = path.to_str().expect("utf8");
        let key = store.load(raw, None).expect("first load");
        assert_eq!(store.loads(), 1);
        assert_eq!(store.len(), 1);
        let reuse = store.load(raw, None).expect("reuse");
        assert_eq!(store.loads(), 1);
        assert_eq!(reuse, key);
        assert!(store.get(&key).is_some());

        std::fs::write(&path, encode_png(3, 3, [1, 1, 1, 0xFF])).expect("rewrite");
        let changed = store.load(raw, None).expect("changed identity");
        assert_eq!(store.loads(), 2);
        assert_ne!(changed, key);
        let image = store.get(&changed).expect("new image");
        assert_eq!(image.dimensions(), (3, 3));
    }

    #[test]
    fn store_denies_all_without_roots() {
        let root = temp_root("deny-all");
        let path = write_file(&root, "one.png", &encode_png(1, 1, [0, 0, 0, 0xFF]));
        let mut store = BackgroundStore::deny_all();
        let err = store
            .load(path.to_str().expect("utf8"), None)
            .expect_err("deny all");
        assert!(matches!(err, BackgroundError::Resource(_)));
        assert_eq!(store.loads(), 0);
    }

    #[test]
    fn fit_geometry_matches_contract() {
        let dest = RectPx::new(100, 50, 40, 20);
        let image = (20, 20);
        let fill = fit_plan(BackgroundFit::Fill, image, dest, 1.0).expect("fill plan");
        assert_eq!(fill.dest, dest);
        assert_eq!(fill.src, [0.0, 5.0, 20.0, 10.0]);

        let fit = fit_plan(BackgroundFit::Fit, image, dest, 1.0).expect("fit plan");
        assert_eq!(fit.dest, RectPx::new(110, 50, 20, 20));
        assert_eq!(fit.src, [0.0, 0.0, 20.0, 20.0]);

        let center = fit_plan(BackgroundFit::Center, (10, 4), dest, 2.0).expect("center plan");
        assert_eq!(center.dest, RectPx::new(110, 56, 20, 8));
        assert_eq!(center.src, [0.0, 0.0, 10.0, 4.0]);

        let center_crop = fit_plan(BackgroundFit::Center, (40, 40), dest, 2.0).expect("crop");
        assert_eq!(center_crop.dest, RectPx::new(100, 50, 40, 20));
        assert_eq!(center_crop.src, [10.0, 15.0, 20.0, 10.0]);

        let tile = fit_plan(BackgroundFit::Tile, (10, 5), dest, 2.0).expect("tile plan");
        assert_eq!(tile.dest, dest);
        assert!(tile.tile);
        assert_eq!((tile.tile_w, tile.tile_h), (20, 10));

        let stretch = fit_plan(BackgroundFit::Stretch, image, dest, 3.0).expect("stretch");
        assert_eq!(stretch.dest, dest);
        assert_eq!(stretch.src, [0.0, 0.0, 20.0, 20.0]);
    }

    #[test]
    fn rasterize_fill_covers_and_tile_repeats() {
        let image = BackgroundImage::try_new(
            2,
            2,
            vec![
                1, 0, 0, 0xFF, 2, 0, 0, 0xFF, // row 0
                3, 0, 0, 0xFF, 4, 0, 0, 0xFF, // row 1
            ],
        )
        .expect("image");
        let dest = RectPx::new(0, 0, 4, 4);
        let fill = rasterize_background(&image, BackgroundFit::Fill, dest, 1.0).expect("fill");
        assert_eq!(fill.dest, dest);
        assert_eq!(fill.rgba.len(), 4 * 4 * 4);
        assert_eq!(&fill.rgba[0..4], &[1, 0, 0, 0xFF]);
        assert_eq!(&fill.rgba[12..16], &[2, 0, 0, 0xFF]);
        assert_eq!(&fill.rgba[60..64], &[4, 0, 0, 0xFF]);

        let tile = rasterize_background(&image, BackgroundFit::Tile, dest, 1.0).expect("tile");
        assert_eq!(&tile.rgba[0..4], &[1, 0, 0, 0xFF]);
        assert_eq!(&tile.rgba[8..12], &[1, 0, 0, 0xFF]);
        assert_eq!(&tile.rgba[4 * 4..4 * 4 + 4], &[3, 0, 0, 0xFF]);
    }

    #[test]
    fn rasterize_fit_leaves_letterbox_and_center_native() {
        let image = BackgroundImage::try_new(1, 1, vec![9, 9, 9, 0xFF]).expect("image");
        let dest = RectPx::new(10, 20, 8, 8);
        let fit = rasterize_background(&image, BackgroundFit::Fit, dest, 1.0).expect("fit");
        assert_eq!(fit.dest, RectPx::new(10, 20, 8, 8));
        assert_eq!(fit.rgba.len(), 8 * 8 * 4);

        let center =
            rasterize_background(&image, BackgroundFit::Center, dest, 2.0).expect("center");
        assert_eq!(center.dest, RectPx::new(13, 23, 2, 2));
        assert_eq!(center.rgba.len(), 2 * 2 * 4);
    }

    #[test]
    fn raster_cache_reuses_and_bounds() {
        let image = BackgroundImage::try_new(2, 2, vec![7; 2 * 2 * 4]).expect("image");
        let dest = RectPx::new(0, 0, 4, 4);
        let key = BackgroundRasterKey {
            source: BackgroundRasterKeySource {
                canonical: PathBuf::from("/approved/bg.png"),
                len: 10,
                modified: None,
            },
            fit: BackgroundFit::Fill,
            dest,
            dpi_bits: 1.0f64.to_bits(),
        };
        let mut cache = BackgroundRasterCache::new();
        let first = cache.get_or_rasterize(key.clone(), &image).expect("miss");
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);
        let second = cache.get_or_rasterize(key, &image).expect("hit");
        assert_eq!(cache.hits(), 1);
        assert_eq!(first, second);
        assert!(cache.total_bytes() <= BG_RASTER_CACHE_MAX_BYTES);
    }

    #[test]
    fn eviction_never_removes_pinned_displayed_image() {
        let root = temp_root("pinned");
        let a = write_file(&root, "a.png", &encode_png(2, 2, [1, 1, 1, 1]));
        let b = write_file(&root, "b.png", &encode_png(2, 2, [2, 2, 2, 2]));
        let mut store = BackgroundStore::new(policy_for(&root));
        let key_a = store.load(a.to_str().expect("utf8"), None).expect("a");
        store.pin(&key_a);
        let key_b = store.load(b.to_str().expect("utf8"), None).expect("b");
        assert!(store.get(&key_a).is_some(), "pinned image stays resident");
        assert!(store.get(&key_b).is_some());
    }
}
