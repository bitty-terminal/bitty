//! Wallpaper-derived terminal colorscheme (issue #1976, CTX-1189).
//!
//! Self-implemented, CPU-only, matugen-class pipeline: a wallpaper image is
//! quantized into a small set of weighted colors, and those colors are mapped
//! onto the Bitty theme-table roles (`background`, `foreground`, `cursor`,
//! `selection`, and the 16 ANSI slots). There is no external daemon, no
//! subprocess, and no network access: the whole derivation is pure integer
//! and `f64` arithmetic over the decoded image.
//!
//! # Pipeline
//!
//! ```text
//! wallpaper bytes -> sniff + bounded decode (existing BG-1..BG-3 path)
//!                 -> budgeted stride sampling (<= MAX_SAMPLE_PIXELS)
//!                 -> deterministic median-cut quantize (<= MAX_CLUSTERS)
//!                 -> role mapping (bg/fg/cursor/selection + ANSI 0..15)
//!                 -> WallpaperThemePreview (theme-table spelling, no write)
//! ```
//!
//! # Contracts
//!
//! - **Deterministic**: the derivation depends only on the decoded pixel
//!   bytes. Ties (equal cluster weight, equal channel range, equal per-bin
//!   score) are broken by a fixed order, and no hash-map iteration order
//!   reaches the result, so the same input always yields the same table.
//! - **Budgeted**: sampling is strided so at most [`MAX_SAMPLE_PIXELS`] pixels
//!   are touched regardless of image dimensions; quantization is bounded by
//!   [`MAX_CLUSTERS`] boxes and a fixed 32768-bin histogram. The default
//!   `image` bounds of the decode path (BG-1..BG-3) still apply upstream.
//! - **Fail-closed**: an unreadable, truncated, unsupported, or non-image
//!   buffer is rejected by the existing [`decode_background`] path and
//!   surfaced as [`WallpaperThemeError::Decode`]. A fully transparent image
//!   ([`WallpaperThemeError::NoOpaquePixels`]) or an image with too few
//!   distinct quantized colors to build a scheme
//!   ([`WallpaperThemeError::InsufficientColorVariety`]) is rejected rather
//!   than filled with invented values.
//! - **Preview, never a silent rewrite**: the public entry points return a
//!   [`WallpaperThemePreview`]. Nothing in this module writes a config file,
//!   mutates live state, or has any I/O beyond the caller-supplied bytes;
//!   the caller must explicitly [`WallpaperThemePreview::confirm`] and
//!   persist [`WallpaperThemePreview::theme_table_lua`]. `bitty --safe`
//!   therefore needs no special case here: with no config change there is no
//!   derived palette to clear.
//!
//! # Output spelling
//!
//! [`WallpaperThemePreview::theme_table_lua`] emits the existing
//! `appearance.colors` Lua table shape (`background`, `foreground`, `cursor`,
//! `selection` plus exactly 16 `ansi` entries, every leaf canonical
//! `#RRGGBB`). That is the Bitty theme table the config pipeline already
//! validates (`bitty-config` `CustomPalette::from_hex`, CTX-0392); no new
//! configuration surface is introduced. Outline tokens (`border.focused` /
//! `border.idle`) stay preset-owned and are not derived here.
//!
//! # Palette behavior
//!
//! The scheme direction is decided by the weighted mean luminance of the
//! sampled image (dark scheme below 0.5). The background is the extreme
//! cluster on the scheme side, shaded toward black (dark) or white (light)
//! until it reaches the scheme's background luminance target or the shade
//! cap, so the wallpaper tint survives. The foreground starts as the
//! maximum-contrast cluster color and is lifted toward pure black/white in
//! 5% steps until it clears [`MIN_FG_CONTRAST`] (WCAG AA, 4.5:1) against the
//! chosen background — a floor the mapping can always reach because the
//! background luminance is bounded by the shade step.
//!
//! ANSI 1..6 are the highest-weight chromatic cluster per hue family
//! (red/yellow/green/cyan/blue/magenta); a family with no chromatic cluster
//! is synthesized by rotating the most chromatic available hue onto that
//! family's center, preserving saturation/lightness. ANSI 9..14 are the
//! bright variants of 1..6 (mixed toward white in a dark scheme, toward
//! black in a light scheme). ANSI 0/8 are Bitty-owned blends of the
//! background toward the foreground (muted surface, bright-black surface),
//! ANSI 7 is the foreground dimmed toward the background, and ANSI 15 is the
//! foreground, mirroring the built-in preset role table.

use std::fmt;

use crate::background::{BackgroundError, BackgroundImage, decode_background};

/// ANSI slots emitted by the mapping (indices 0-15).
pub const WALLPAPER_THEME_ANSI_COUNT: usize = 16;

/// Hard bound on sampled pixels per derivation (budget contract).
///
/// A 4K wallpaper (`3840x2160`) is sampled with a stride of 506, so the
/// derivation stays O([`MAX_SAMPLE_PIXELS`]) regardless of the input size.
pub const MAX_SAMPLE_PIXELS: usize = 16_384;

/// Quantization resolution: 5 bits per channel (32768 histogram bins).
const QUANTIZE_BITS: u32 = 5;

/// Histogram bin count for [`QUANTIZE_BITS`].
const QUANTIZE_BINS: usize = 1 << (QUANTIZE_BITS * 3);

/// Maximum number of quantized clusters kept after median cut.
pub const MAX_CLUSTERS: usize = 12;

/// Distinct quantized clusters required before a scheme is derivable.
pub const MIN_DISTINCT_COLORS: usize = 4;

/// Minimum alpha for a pixel to contribute to the palette.
pub const MIN_OPAQUE_ALPHA: u8 = 128;

/// WCAG AA contrast floor enforced between `foreground` and `background`.
pub const MIN_FG_CONTRAST: f64 = 4.5;

/// Minimum saturation for a cluster to seed an ANSI hue family.
const MIN_CHROMATIC_SATURATION: f64 = 0.15;

/// Hue centers (degrees) of ANSI 1..6: red, yellow, green, cyan, blue, magenta.
const HUE_CENTERS: [f64; 6] = [0.0, 60.0, 120.0, 180.0, 240.0, 300.0];

/// Shade cap for the background (fraction of the way to black/white).
const BACKGROUND_SHADE_CAP: f64 = 0.65;

/// Luminance target for a dark scheme background.
const DARK_BACKGROUND_LUMINANCE: f64 = 0.12;

/// Luminance target for a light scheme background.
const LIGHT_BACKGROUND_LUMINANCE: f64 = 0.88;

/// Rejection of a wallpaper colorscheme derivation. Fail-closed: every
/// variant names the failed condition and no variant carries image content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WallpaperThemeError {
    /// The buffer failed sniff/decode/bounds (unreadable, truncated,
    /// unsupported container, over-budget, or inconsistent header).
    Decode(BackgroundError),
    /// No pixel reached [`MIN_OPAQUE_ALPHA`]; nothing to derive from.
    NoOpaquePixels,
    /// Fewer than [`MIN_DISTINCT_COLORS`] distinct quantized colors survived
    /// (for example a flat, single-color image). Refused rather than padded
    /// with invented colors.
    InsufficientColorVariety {
        /// Distinct quantized colors found.
        distinct: usize,
        /// Distinct quantized colors required.
        required: usize,
    },
}

impl fmt::Display for WallpaperThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(err) => write!(f, "wallpaper image rejected: {err}"),
            Self::NoOpaquePixels => {
                write!(f, "wallpaper has no opaque pixels to derive a theme from")
            }
            Self::InsufficientColorVariety { distinct, required } => write!(
                f,
                "wallpaper has {distinct} distinct quantized colors, {required} required"
            ),
        }
    }
}

impl std::error::Error for WallpaperThemeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(err) => Some(err),
            _ => None,
        }
    }
}

impl From<BackgroundError> for WallpaperThemeError {
    fn from(err: BackgroundError) -> Self {
        Self::Decode(err)
    }
}

/// A derived wallpaper colorscheme in Bitty theme-table shape: the same
/// leaves as `bitty-config` `CustomPalette` (straight `sRGB` bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallpaperPalette {
    /// Window clear color and default cell background.
    pub background: [u8; 3],
    /// Default glyph color; clears [`MIN_FG_CONTRAST`] against `background`.
    pub foreground: [u8; 3],
    /// Block cursor fill.
    pub cursor: [u8; 3],
    /// Selection background fill.
    pub selection: [u8; 3],
    /// The 16 ANSI colors, indices 0-15.
    pub ansi: [[u8; 3]; WALLPAPER_THEME_ANSI_COUNT],
}

impl WallpaperPalette {
    /// ANSI entry `index` (0-15) as RGB bytes.
    #[must_use]
    pub const fn ansi_entry(self, index: u8) -> [u8; 3] {
        self.ansi[index as usize % WALLPAPER_THEME_ANSI_COUNT]
    }

    /// Canonical `#RRGGBB` spelling of one RGB triplet.
    #[must_use]
    pub fn hex(rgb: [u8; 3]) -> String {
        format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
    }

    /// WCAG contrast ratio between `foreground` and `background` (>= 1.0).
    #[must_use]
    pub fn foreground_contrast(&self) -> f64 {
        contrast_ratio(self.foreground, self.background)
    }
}

/// A derived scheme plus the budget evidence for one derivation.
///
/// The preview is inert data: it holds no path, no file handle, and no
/// capability. Persisting requires an explicit
/// [`WallpaperThemePreview::confirm`] followed by the caller writing
/// [`WallpaperThemePreview::theme_table_lua`] under user confirmation, so a
/// derivation can never silently rewrite the active theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperThemePreview {
    palette: WallpaperPalette,
    sampled_pixels: usize,
    opaque_pixels: usize,
    distinct_colors: usize,
}

impl WallpaperThemePreview {
    /// The derived theme table.
    #[must_use]
    pub fn palette(&self) -> &WallpaperPalette {
        &self.palette
    }

    /// Pixels inspected by the stride sampler (<= [`MAX_SAMPLE_PIXELS`]).
    #[must_use]
    pub fn sampled_pixels(&self) -> usize {
        self.sampled_pixels
    }

    /// Sampled pixels that reached [`MIN_OPAQUE_ALPHA`].
    #[must_use]
    pub fn opaque_pixels(&self) -> usize {
        self.opaque_pixels
    }

    /// Distinct quantized colors the histogram found.
    #[must_use]
    pub fn distinct_colors(&self) -> usize {
        self.distinct_colors
    }

    /// The theme table as the `appearance.colors` Lua spelling.
    ///
    /// Emits the exact leaf set the config pipeline validates (CTX-0392):
    /// `background`, `foreground`, `cursor`, `selection`, and 16 `ansi`
    /// entries, every leaf a canonical `#RRGGBB` string.
    #[must_use]
    pub fn theme_table_lua(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("appearance.colors = {\n");
        for (key, rgb) in [
            ("background", self.palette.background),
            ("foreground", self.palette.foreground),
            ("cursor", self.palette.cursor),
            ("selection", self.palette.selection),
        ] {
            out.push_str("  ");
            out.push_str(key);
            out.push_str(" = \"");
            out.push_str(&WallpaperPalette::hex(rgb));
            out.push_str("\",\n");
        }
        out.push_str("  ansi = {\n");
        for rgb in self.palette.ansi {
            out.push_str("    \"");
            out.push_str(&WallpaperPalette::hex(rgb));
            out.push_str("\",\n");
        }
        out.push_str("  },\n}\n");
        out
    }

    /// Explicit confirm-to-persist step: the only way to obtain the table.
    ///
    /// Callers must ask the user before calling this; the derivation itself
    /// never applies anything.
    #[must_use]
    pub fn confirm(self) -> WallpaperPalette {
        self.palette
    }
}

/// Derives a theme table from an already decoded background image.
///
/// # Errors
///
/// [`WallpaperThemeError::NoOpaquePixels`] and
/// [`WallpaperThemeError::InsufficientColorVariety`].
pub fn derive_palette(image: &BackgroundImage) -> Result<WallpaperPalette, WallpaperThemeError> {
    preview_from_image(image).map(WallpaperThemePreview::confirm)
}

/// Derives a preview from an already decoded background image.
///
/// # Errors
///
/// [`WallpaperThemeError::NoOpaquePixels`] and
/// [`WallpaperThemeError::InsufficientColorVariety`].
pub fn preview_from_image(
    image: &BackgroundImage,
) -> Result<WallpaperThemePreview, WallpaperThemeError> {
    let (width, height) = image.dimensions();
    let stride = sample_stride(width, height);
    let histogram = histogram(image.rgba(), stride);
    finish(histogram, planned_sample_count(width, height))
}

/// Derives a preview from wallpaper bytes, fail-closed on decode.
///
/// The buffer runs through the existing background-image path
/// ([`decode_background`]: format sniff, BG-1..BG-3 bounds, bounded RGBA8
/// decode), so an unreadable, truncated, animated, or unsupported file is
/// rejected before any pixel is inspected.
///
/// # Errors
///
/// [`WallpaperThemeError::Decode`] for a rejected buffer, plus the
/// image-level errors above.
pub fn preview_from_bytes(bytes: &[u8]) -> Result<WallpaperThemePreview, WallpaperThemeError> {
    let image = decode_background(bytes)?;
    preview_from_image(&image)
}

/// Derives a theme table from wallpaper bytes (see [`preview_from_bytes`]).
///
/// # Errors
///
/// As [`preview_from_bytes`].
pub fn derive_palette_from_bytes(bytes: &[u8]) -> Result<WallpaperPalette, WallpaperThemeError> {
    preview_from_bytes(bytes).map(WallpaperThemePreview::confirm)
}

/// Pixel stride used for an image of `width x height` (>= 1).
///
/// The stride guarantees at most [`MAX_SAMPLE_PIXELS`] samples, which is the
/// budget contract the CPU-only derivation relies on.
#[must_use]
pub fn sample_stride(width: u32, height: u32) -> usize {
    let pixels = (width as usize).saturating_mul(height as usize);
    pixels.div_ceil(MAX_SAMPLE_PIXELS).max(1)
}

/// Number of pixels the stride sampler visits for `width x height`.
#[must_use]
pub fn planned_sample_count(width: u32, height: u32) -> usize {
    let pixels = (width as usize).saturating_mul(height as usize);
    pixels.div_ceil(sample_stride(width, height))
}

/// WCAG relative luminance of an `sRGB` triplet (0.0 dark .. 1.0 light).
#[must_use]
pub fn luminance(rgb: [u8; 3]) -> f64 {
    fn channel(value: u8) -> f64 {
        let scaled = f64::from(value) / 255.0;
        if scaled <= 0.040_45 {
            scaled / 12.92
        } else {
            ((scaled + 0.055) / 1.055).powf(2.4)
        }
    }
    0.212_6 * channel(rgb[0]) + 0.715_2 * channel(rgb[1]) + 0.072_2 * channel(rgb[2])
}

/// WCAG contrast ratio between two opaque colors (>= 1.0).
#[must_use]
pub fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// One quantized histogram bin with its weighted color sum.
#[derive(Debug, Clone, Copy)]
struct Cluster {
    key: u32,
    count: u32,
    sum: [u32; 3],
}

impl Cluster {
    fn mean(self) -> [u8; 3] {
        let count = u64::from(self.count.max(1));
        let channel = |sum: u32| ((u64::from(sum) + count / 2) / count) as u8;
        [
            channel(self.sum[0]),
            channel(self.sum[1]),
            channel(self.sum[2]),
        ]
    }
}

/// One weighted color with its pixel count, used after median cut.
#[derive(Debug, Clone, Copy)]
struct Weighted {
    rgb: [u8; 3],
    count: u32,
}

/// Builds the quantized histogram, visiting every `stride`-th pixel.
fn histogram(rgba: &[u8], stride: usize) -> Vec<Cluster> {
    let pixels = rgba.len() / 4;
    let mut bins = vec![[0u32; 4]; QUANTIZE_BINS];
    let shift = 8 - QUANTIZE_BITS;
    let mask = (1u32 << QUANTIZE_BITS) - 1;
    let mut index = 0;
    while index < pixels {
        let base = index * 4;
        let alpha = rgba[base + 3];
        if alpha >= MIN_OPAQUE_ALPHA {
            let (r, g, b) = (
                u32::from(rgba[base]),
                u32::from(rgba[base + 1]),
                u32::from(rgba[base + 2]),
            );
            let key = (((r >> shift) & mask) << (2 * QUANTIZE_BITS))
                | (((g >> shift) & mask) << QUANTIZE_BITS)
                | ((b >> shift) & mask);
            let slot = &mut bins[key as usize];
            slot[0] = slot[0].saturating_add(1);
            slot[1] = slot[1].saturating_add(r);
            slot[2] = slot[2].saturating_add(g);
            slot[3] = slot[3].saturating_add(b);
        }
        index += stride;
    }
    let mut clusters = Vec::new();
    for (key, slot) in bins.iter().enumerate() {
        if slot[0] > 0 {
            clusters.push(Cluster {
                key: key as u32,
                count: slot[0],
                sum: [slot[1], slot[2], slot[3]],
            });
        }
    }
    clusters
}

/// Turns a histogram into a preview, or fails closed.
fn finish(
    mut clusters: Vec<Cluster>,
    sampled_pixels: usize,
) -> Result<WallpaperThemePreview, WallpaperThemeError> {
    let entry_count = clusters.len();
    // Bin order is ascending by (r, g, b) quantized key, which is already the
    // deterministic order the tie-breaks below rely on.
    let opaque_pixels = clusters.iter().map(|c| c.count).sum::<u32>() as usize;
    if opaque_pixels == 0 {
        return Err(WallpaperThemeError::NoOpaquePixels);
    }
    if entry_count < MIN_DISTINCT_COLORS {
        return Err(WallpaperThemeError::InsufficientColorVariety {
            distinct: entry_count,
            required: MIN_DISTINCT_COLORS,
        });
    }

    let weighted = median_cut(&mut clusters);
    let (background, foreground) = background_foreground(&weighted);
    let scheme_dark = luminance(background) < 0.5;
    let mut ansi = [[0u8; 3]; WALLPAPER_THEME_ANSI_COUNT];
    ansi[0] = mix(background, foreground, 0.16);
    ansi[7] = mix(foreground, background, 0.28);
    ansi[8] = mix(background, foreground, 0.34);
    ansi[15] = foreground;
    let bases = ansi_hues(&weighted);
    for (offset, base) in bases.iter().enumerate() {
        ansi[offset + 1] = *base;
        ansi[offset + 9] = if scheme_dark {
            mix(*base, [255, 255, 255], 0.30)
        } else {
            mix(*base, [0, 0, 0], 0.25)
        };
    }

    Ok(WallpaperThemePreview {
        palette: WallpaperPalette {
            background,
            foreground,
            cursor: foreground,
            selection: mix(background, foreground, 0.24),
            ansi,
        },
        sampled_pixels,
        opaque_pixels,
        distinct_colors: entry_count,
    })
}

/// Median cut over the quantized histogram (deterministic, bounded).
///
/// Repeatedly splits the heaviest splittable box along its widest channel
/// (ties: red, then green, then blue) at the pixel-weighted median, stopping
/// at [`MAX_CLUSTERS`] boxes. Box selection ties go to the earliest box, and
/// the input slice order is fixed, so the output is a pure function of the
/// histogram.
fn median_cut(clusters: &mut [Cluster]) -> Vec<Weighted> {
    let mut boxes: Vec<(usize, usize)> = vec![(0, clusters.len())];
    while boxes.len() < MAX_CLUSTERS {
        let mut chosen: Option<(usize, u32)> = None;
        for (idx, &(start, end)) in boxes.iter().enumerate() {
            if end - start < 2 {
                continue;
            }
            let count: u32 = clusters[start..end].iter().map(|c| c.count).sum();
            if count == 0 {
                continue;
            }
            match chosen {
                Some((_, best)) if best >= count => {}
                _ => chosen = Some((idx, count)),
            }
        }
        let Some((idx, _)) = chosen else { break };
        let (start, end) = boxes[idx];
        let axis = widest_axis(&clusters[start..end]);
        clusters[start..end].sort_unstable_by_key(|cluster| {
            let mean = cluster.mean();
            (mean[axis], cluster.key)
        });
        let total: u64 = clusters[start..end]
            .iter()
            .map(|c| u64::from(c.count))
            .sum();
        let mut cumulative = 0u64;
        let mut split = start + 1;
        for (offset, cluster) in clusters[start..end].iter().enumerate() {
            cumulative += u64::from(cluster.count);
            let candidate = start + offset + 1;
            if cumulative * 2 >= total {
                split = candidate.clamp(start + 1, end - 1);
                break;
            }
        }
        boxes[idx] = (start, split);
        boxes.push((split, end));
    }

    let mut out = Vec::with_capacity(boxes.len());
    for (start, end) in boxes {
        let count: u32 = clusters[start..end].iter().map(|c| c.count).sum();
        if count == 0 {
            continue;
        }
        let mut sum = [0u64; 3];
        for cluster in &clusters[start..end] {
            for (slot, value) in sum.iter_mut().zip(cluster.sum) {
                *slot += u64::from(value);
            }
        }
        let total = u64::from(count);
        out.push(Weighted {
            rgb: [
                ((sum[0] + total / 2) / total) as u8,
                ((sum[1] + total / 2) / total) as u8,
                ((sum[2] + total / 2) / total) as u8,
            ],
            count,
        });
    }
    out
}

/// Channel index (0=R, 1=G, 2=B) with the largest spread in `clusters`.
fn widest_axis(clusters: &[Cluster]) -> usize {
    let mut lo = [u8::MAX; 3];
    let mut hi = [0u8; 3];
    for cluster in clusters {
        let mean = cluster.mean();
        lo[0] = lo[0].min(mean[0]);
        lo[1] = lo[1].min(mean[1]);
        lo[2] = lo[2].min(mean[2]);
        hi[0] = hi[0].max(mean[0]);
        hi[1] = hi[1].max(mean[1]);
        hi[2] = hi[2].max(mean[2]);
    }
    let ranges = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    if ranges[0] >= ranges[1] && ranges[0] >= ranges[2] {
        0
    } else if ranges[1] >= ranges[2] {
        1
    } else {
        2
    }
}

/// Picks the background (shaded extreme cluster) and the foreground.
///
/// The scheme direction comes from the weighted mean luminance of the
/// clusters; the foreground is lifted toward pure white (dark scheme) or
/// pure black (light scheme) until it clears [`MIN_FG_CONTRAST`].
fn background_foreground(clusters: &[Weighted]) -> ([u8; 3], [u8; 3]) {
    let weight: u64 = clusters.iter().map(|c| u64::from(c.count)).sum();
    let mean_luminance = if weight == 0 {
        0.0
    } else {
        clusters
            .iter()
            .map(|c| luminance(c.rgb) * c.count as f64)
            .sum::<f64>()
            / weight as f64
    };
    let dark_scheme = mean_luminance < 0.5;

    // Extreme cluster: darkest for a dark scheme, lightest for a light one.
    // The scan keeps the earliest cluster on an exact tie (deterministic).
    let mut extreme = clusters[0];
    for cluster in clusters.iter().copied() {
        let (current, best) = (luminance(cluster.rgb), luminance(extreme.rgb));
        let better = if dark_scheme {
            current < best
        } else {
            current > best
        };
        if better {
            extreme = cluster;
        }
    }

    let pole = if dark_scheme {
        [0u8, 0, 0]
    } else {
        [255u8, 255, 255]
    };
    let target = if dark_scheme {
        DARK_BACKGROUND_LUMINANCE
    } else {
        LIGHT_BACKGROUND_LUMINANCE
    };
    let background = shade_to(extreme.rgb, pole, target, dark_scheme);

    let mut foreground = background;
    let mut best_contrast = -1.0;
    for cluster in clusters {
        let ratio = contrast_ratio(cluster.rgb, background);
        if ratio > best_contrast {
            best_contrast = ratio;
            foreground = cluster.rgb;
        }
    }
    let text_pole = if dark_scheme {
        [255u8, 255, 255]
    } else {
        [0u8, 0, 0]
    };
    (
        background,
        lift_to_contrast(foreground, background, text_pole),
    )
}

/// Shades `color` toward `pole` until it reaches `target` luminance or the
/// [`BACKGROUND_SHADE_CAP`], never overshooting the target.
fn shade_to(color: [u8; 3], pole: [u8; 3], target: f64, dark_scheme: bool) -> [u8; 3] {
    let reached = |value: [u8; 3]| {
        if dark_scheme {
            luminance(value) <= target
        } else {
            luminance(value) >= target
        }
    };
    if reached(color) {
        return color;
    }
    let mut result = color;
    for step in 1..=20 {
        let t = BACKGROUND_SHADE_CAP * f64::from(step) / 20.0;
        let candidate = mix(color, pole, t);
        if reached(candidate) {
            return candidate;
        }
        result = candidate;
    }
    result
}

/// Lifts `color` toward `pole` in 5% steps until it clears
/// [`MIN_FG_CONTRAST`] against `background`; `pole` when unreachable.
fn lift_to_contrast(color: [u8; 3], background: [u8; 3], pole: [u8; 3]) -> [u8; 3] {
    if contrast_ratio(color, background) >= MIN_FG_CONTRAST {
        return color;
    }
    for step in 1..=20 {
        let t = f64::from(step) / 20.0;
        let candidate = mix(color, pole, t);
        if contrast_ratio(candidate, background) >= MIN_FG_CONTRAST {
            return candidate;
        }
    }
    pole
}

/// ANSI 1..6 base colors, one per hue family.
///
/// The heaviest chromatic cluster wins each family; ties keep the earliest
/// cluster. A family with no chromatic cluster takes the most chromatic
/// cluster's hue rotated onto its center, or a synthesized center-hue color
/// when the image has no chromatic pixel at all — never an empty slot.
fn ansi_hues(clusters: &[Weighted]) -> [[u8; 3]; 6] {
    let mut best: [Option<(f64, [u8; 3])>; 6] = [None; 6];
    let mut fallback: Option<(f64, [u8; 3])> = None;
    for cluster in clusters {
        let (hue, saturation, lightness) = rgb_to_hsl(cluster.rgb);
        if saturation < MIN_CHROMATIC_SATURATION {
            continue;
        }
        let score = saturation * f64::from(cluster.count);
        if fallback.is_none_or(|(best_score, _)| score > best_score) {
            fallback = Some((score, cluster.rgb));
        }
        if let Some(family) = nearest_family(hue) {
            let better = match best[family] {
                Some((best_score, _)) => score > best_score,
                None => true,
            };
            if better {
                best[family] = Some((score, hsl_to_rgb(hue, saturation, lightness)));
            }
        }
    }

    let mut out = [[0u8; 3]; 6];
    let seed = fallback.map(|(_, rgb)| rgb);
    for (family, slot) in out.iter_mut().enumerate() {
        *slot = match best[family] {
            Some((_, rgb)) => rgb,
            None => match seed {
                Some(seed_rgb) => {
                    let (_, saturation, lightness) = rgb_to_hsl(seed_rgb);
                    hsl_to_rgb(
                        HUE_CENTERS[family],
                        saturation.max(MIN_CHROMATIC_SATURATION),
                        lightness.clamp(0.15, 0.85),
                    )
                }
                None => {
                    let lightness = if luminance(clusters[0].rgb) < 0.5 {
                        0.65
                    } else {
                        0.45
                    };
                    hsl_to_rgb(HUE_CENTERS[family], 0.55, lightness)
                }
            },
        };
    }
    out
}

/// Hue family index (0..5) whose center is nearest `hue` (degrees).
fn nearest_family(hue: f64) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (family, center) in HUE_CENTERS.iter().enumerate() {
        let delta = (hue - center).abs().min(360.0 - (hue - center).abs());
        if best.is_none_or(|(_, best_delta)| delta < best_delta) {
            best = Some((family, delta));
        }
    }
    best.map(|(family, _)| family)
}

/// Linear blend of two colors; `t = 0` yields `from`, `t = 1` yields `to`.
fn mix(from: [u8; 3], to: [u8; 3], t: f64) -> [u8; 3] {
    let blend = |a: u8, b: u8| -> u8 {
        let value = f64::from(a) * (1.0 - t) + f64::from(b) * t;
        value.round().clamp(0.0, 255.0) as u8
    };
    [
        blend(from[0], to[0]),
        blend(from[1], to[1]),
        blend(from[2], to[2]),
    ]
}

/// `sRGB` bytes to `(hue degrees, saturation, lightness)`.
fn rgb_to_hsl(rgb: [u8; 3]) -> (f64, f64, f64) {
    let r = f64::from(rgb[0]) / 255.0;
    let g = f64::from(rgb[1]) / 255.0;
    let b = f64::from(rgb[2]) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f64::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let hue = if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    (hue, saturation.clamp(0.0, 1.0), lightness)
}

/// `(hue degrees, saturation, lightness)` back to `sRGB` bytes.
fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
    let hue = hue.rem_euclid(360.0) / 360.0;
    let saturation = saturation.clamp(0.0, 1.0);
    let lightness = lightness.clamp(0.0, 1.0);
    let channel = |n: f64| -> u8 {
        let k = (n + hue * 12.0) % 12.0;
        let a = saturation * lightness.min(1.0 - lightness);
        let value = lightness - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0);
        (value * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [channel(0.0), channel(8.0), channel(4.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an RGBA image from a per-pixel closure.
    fn image(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> BackgroundImage {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let rgb = pixel(x, y);
                rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0xFF]);
            }
        }
        BackgroundImage::try_new(width, height, rgba).expect("image")
    }

    /// A dark, blue-dominant wallpaper with red/green/cyan/magenta accents.
    fn dusk() -> BackgroundImage {
        image(64, 64, |x, y| {
            let tint = ((x * 3 + y) % 24) as u8;
            match (x / 8, y / 8) {
                (0, _) => [0x18, 0x1C + tint / 3, 0x2E + tint / 4],
                (1, _) => [0xB4, 0x2A, 0x3C],
                (2, _) => [0x3C, 0x9A, 0x50],
                (3, _) => [0xD8, 0xC0, 0x40],
                (4, _) => [0x40, 0xB0, 0xC8],
                (5, _) => [0x4A, 0x58, 0xC0],
                (6, _) => [0xA0, 0x50, 0xB8],
                _ => [0x24, 0x28, 0x38],
            }
        })
    }

    /// A light-dominant wallpaper with sparse saturated accent stripes.
    fn daybreak() -> BackgroundImage {
        image(48, 48, |x, y| {
            if x % 7 == 0 || y % 7 == 0 {
                match (x / 7 + y / 7) % 6 {
                    0 => [0xC0, 0x30, 0x38],
                    1 => [0x30, 0x80, 0x40],
                    2 => [0xC0, 0xA0, 0x30],
                    3 => [0x30, 0x88, 0xA8],
                    4 => [0x38, 0x50, 0xB0],
                    _ => [0x88, 0x40, 0xA0],
                }
            } else {
                [0xE4, 0xEC, 0xF4]
            }
        })
    }

    /// A near-monochrome wallpaper (surfaces only, no saturated pixel).
    fn slate() -> BackgroundImage {
        image(32, 32, |x, y| {
            let level = 0x30 + ((x + y) % 4) as u8 * 6;
            [level, level + 2, level + 4]
        })
    }

    #[test]
    fn derivation_is_deterministic() {
        for source in [dusk(), daybreak(), slate()] {
            let first = derive_palette(&source).expect("scheme");
            let second = derive_palette(&source).expect("scheme");
            let third = preview_from_image(&source).expect("preview").confirm();
            assert_eq!(first, second, "same input yields the same table");
            assert_eq!(first, third, "preview confirm yields the same table");
        }
    }

    #[test]
    fn same_bytes_yield_the_same_table() {
        let bytes = encode_png(&dusk());
        let first = derive_palette_from_bytes(&bytes).expect("scheme");
        let second = derive_palette_from_bytes(&bytes).expect("scheme");
        assert_eq!(first, second);
    }

    #[test]
    fn foreground_clears_wcag_aa_against_background() {
        for source in [dusk(), daybreak(), slate()] {
            let palette = derive_palette(&source).expect("scheme");
            assert!(
                palette.foreground_contrast() >= MIN_FG_CONTRAST,
                "foreground {:?} on background {:?} is {:.2}:1",
                palette.foreground,
                palette.background,
                palette.foreground_contrast()
            );
        }
    }

    #[test]
    fn dark_source_yields_dark_background() {
        let palette = derive_palette(&dusk()).expect("scheme");
        assert!(
            luminance(palette.background) <= DARK_BACKGROUND_LUMINANCE,
            "background {:?} luminance {:.3}",
            palette.background,
            luminance(palette.background)
        );
        assert!(contrast_ratio(palette.foreground, palette.background) >= MIN_FG_CONTRAST);
    }

    #[test]
    fn light_source_yields_light_background() {
        let palette = derive_palette(&daybreak()).expect("scheme");
        assert!(
            luminance(palette.background) >= LIGHT_BACKGROUND_LUMINANCE,
            "background {:?} luminance {:.3}",
            palette.background,
            luminance(palette.background)
        );
    }

    #[test]
    fn ansi_hue_families_are_kept() {
        for source in [dusk(), daybreak()] {
            let palette = derive_palette(&source).expect("scheme");
            for index in 1..=6u8 {
                let rgb = palette.ansi_entry(index);
                let (hue, saturation, _) = rgb_to_hsl(rgb);
                assert!(
                    saturation >= MIN_CHROMATIC_SATURATION,
                    "ansi {index} {rgb:?} is achromatic"
                );
                assert_eq!(
                    nearest_family(hue),
                    Some(index as usize - 1),
                    "ansi {index} hue {hue:.1} left its family"
                );
            }
        }
    }

    #[test]
    fn achromatic_source_still_fills_every_hue_family() {
        let palette = derive_palette(&slate()).expect("scheme");
        let mut families = [false; 6];
        for index in 1..=6u8 {
            let rgb = palette.ansi_entry(index);
            let (hue, _, _) = rgb_to_hsl(rgb);
            families[nearest_family(hue).expect("family")] = true;
        }
        assert!(families.iter().all(|seen| *seen), "one slot per family");
    }

    #[test]
    fn surface_roles_stay_distinct() {
        for source in [dusk(), daybreak()] {
            let palette = derive_palette(&source).expect("scheme");
            let surfaces = [
                palette.ansi_entry(0),
                palette.ansi_entry(7),
                palette.ansi_entry(8),
                palette.selection,
            ];
            for (i, left) in surfaces.iter().enumerate() {
                for (j, right) in surfaces.iter().enumerate() {
                    if i != j {
                        assert_ne!(left, right, "surface roles {i} and {j} collided");
                    }
                }
            }
            assert_eq!(palette.cursor, palette.foreground);
            assert_eq!(palette.ansi_entry(15), palette.foreground);
        }
    }

    #[test]
    fn bright_slots_are_derived_from_their_base_slot() {
        let palette = derive_palette(&dusk()).expect("scheme");
        for base in 1..=6u8 {
            let normal = palette.ansi_entry(base);
            let bright = palette.ansi_entry(base + 8);
            assert_ne!(normal, bright, "bright slot {base} did not move");
            assert!(
                luminance(bright) >= luminance(normal),
                "dark scheme bright slot {base} is darker than its base"
            );
        }
    }

    #[test]
    fn flat_image_fails_closed() {
        let flat = image(16, 16, |_, _| [0x33, 0x44, 0x55]);
        assert_eq!(
            derive_palette(&flat),
            Err(WallpaperThemeError::InsufficientColorVariety {
                distinct: 1,
                required: MIN_DISTINCT_COLORS,
            })
        );
    }

    #[test]
    fn fully_transparent_image_fails_closed() {
        let mut rgba = Vec::new();
        for _ in 0..(8 * 8) {
            rgba.extend_from_slice(&[0x10, 0x20, 0x30, 0x00]);
        }
        let transparent = BackgroundImage::try_new(8, 8, rgba).expect("image");
        assert_eq!(
            derive_palette(&transparent),
            Err(WallpaperThemeError::NoOpaquePixels)
        );
    }

    #[test]
    fn non_image_bytes_fail_closed() {
        let err = preview_from_bytes(b"not an image at all").expect_err("rejected");
        assert!(matches!(err, WallpaperThemeError::Decode(_)), "{err}");
        assert!(err.to_string().contains("wallpaper image rejected"));
    }

    #[test]
    fn unsupported_container_fails_closed() {
        // GIF87a magic: a valid container outside the accepted PNG/JPEG/WebP set.
        let gif = b"GIF87a\x01\x00\x01\x00\x80\x00\x00";
        let err = preview_from_bytes(gif).expect_err("rejected");
        assert!(matches!(err, WallpaperThemeError::Decode(_)), "{err}");
    }

    #[test]
    fn truncated_png_fails_closed() {
        let mut bytes = encode_png(&dusk());
        bytes.truncate(bytes.len() / 2);
        let err = preview_from_bytes(&bytes).expect_err("rejected");
        assert!(matches!(err, WallpaperThemeError::Decode(_)), "{err}");
    }

    #[test]
    fn sampling_budget_is_bounded() {
        for (width, height) in [(1u32, 1u32), (640, 480), (1920, 1080), (3840, 2160)] {
            let planned = planned_sample_count(width, height);
            assert!(
                planned <= MAX_SAMPLE_PIXELS,
                "{width}x{height} planned {planned} samples"
            );
            assert!(planned >= 1);
            assert!(sample_stride(width, height) >= 1);
        }
        assert_eq!(planned_sample_count(1, 1), 1);
        assert_eq!(sample_stride(1, 1), 1);
        assert_eq!(planned_sample_count(4096, 4096), MAX_SAMPLE_PIXELS);
    }

    #[test]
    fn budget_evidence_is_reported() {
        let preview = preview_from_image(&dusk()).expect("preview");
        assert_eq!(preview.sampled_pixels(), 64 * 64);
        assert_eq!(preview.opaque_pixels(), 64 * 64);
        assert!(preview.distinct_colors() >= MIN_DISTINCT_COLORS);
    }

    #[test]
    fn large_image_uses_the_stride_budget() {
        let large = image(256, 256, |x, y| {
            let level = ((x + y) % 32) as u8 * 4;
            [level, 0xFF - level, (level / 2) + 0x20]
        });
        let preview = preview_from_image(&large).expect("preview");
        let stride = sample_stride(256, 256);
        assert_eq!(preview.sampled_pixels(), planned_sample_count(256, 256));
        assert_eq!(preview.opaque_pixels(), 256 * 256 / stride);
    }

    #[test]
    fn theme_table_lua_uses_the_config_leaf_set() {
        let preview = preview_from_image(&dusk()).expect("preview");
        let table = preview.theme_table_lua();
        assert!(table.starts_with("appearance.colors = {\n"));
        for key in ["background", "foreground", "cursor", "selection"] {
            assert!(table.contains(&format!("  {key} = \"#")), "missing {key}");
        }
        assert!(table.contains("  ansi = {\n"));
        let leaves: Vec<&str> = table.matches("\"#").collect();
        assert_eq!(leaves.len(), 4 + WALLPAPER_THEME_ANSI_COUNT);
        for index in 1..=16u8 {
            assert!(table.contains(&format!(
                "\"{}\"",
                WallpaperPalette::hex(preview.palette().ansi_entry(index - 1))
            )));
        }
        // No canonical-#RRGGBB leaf is malformed.
        for chunk in table.split('"') {
            if let Some(hex) = chunk.strip_prefix('#') {
                assert_eq!(hex.len(), 6, "malformed leaf {chunk:?}");
                assert!(
                    hex.chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
                );
            }
        }
        assert_eq!(preview.theme_table_lua(), table, "emit is deterministic");
    }

    #[test]
    fn palette_matches_the_theme_table_shape() {
        let palette = derive_palette(&dusk()).expect("scheme");
        assert_eq!(palette.ansi.len(), WALLPAPER_THEME_ANSI_COUNT);
        assert_eq!(WallpaperPalette::hex([0x1E, 0x1E, 0x2E]), "#1E1E2E");
    }

    #[test]
    fn contrast_and_luminance_helpers_match_wcag() {
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.01);
        assert!(luminance([0, 0, 0]) < luminance([255, 255, 255]));
        // Bitty Dark foreground/background, the documented preset pair.
        assert!((contrast_ratio([0x1E, 0x1E, 0x2E], [0xCD, 0xD6, 0xF4]) - 11.34).abs() < 0.05);
    }

    #[test]
    fn hsl_round_trip_is_stable() {
        for rgb in [[0xB4u8, 0x2A, 0x3C], [0x3C, 0x9A, 0x50], [0x40, 0xB0, 0xC8]] {
            let (hue, saturation, lightness) = rgb_to_hsl(rgb);
            assert_eq!(hsl_to_rgb(hue, saturation, lightness), rgb);
        }
    }

    /// Encodes an image as PNG for the byte-level entry points.
    fn encode_png(source: &BackgroundImage) -> Vec<u8> {
        let (width, height) = source.dimensions();
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(source.rgba()).expect("png encode");
        }
        bytes
    }
}
