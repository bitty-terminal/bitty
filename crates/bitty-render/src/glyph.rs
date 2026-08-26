//! The Bitty-owned glyph rasterization contract.
//!
//! [`GlyphRasterizer`] is the seam behind which upstream font engines live
//! (ADR-0004 "Wrap" row). Everything crossing it is owned by this crate:
//! font requests as [`FontQuery`], loaded faces as opaque [`FontId`] handles,
//! rasterization requests as [`RasterKey`], and results as [`GlyphBitmap`]
//! values with enforced length invariants. No upstream type appears here.
//!
//! Shaping, kerning, and fallback-chain policy are **out of scope** by design
//! (deferred to the text RFC named in ADR-0004); this trait rasterizes single
//! characters only.

use std::fmt;

use crate::error::RenderError;

/// Opaque handle to a font face loaded inside one rasterizer session.
///
/// Handles are only meaningful to the rasterizer instance that issued them;
/// using one against a different instance yields
/// [`RenderError::UnknownFontHandle`]. Handles stay stable for the lifetime
/// of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontId(u64);

impl FontId {
    /// Issues the next sequential handle. Public so external
    /// [`GlyphRasterizer`] implementations can mint their own session
    /// handles; the built-in crossfont backend uses the same entry point.
    pub fn next(counter: &mut u64) -> Self {
        let id = *counter;
        *counter = counter.wrapping_add(1);
        FontId(id)
    }

    /// Raw numeric identity (used as a compact cache key component).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Style requested for a font family.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FontStyle {
    /// Regular weight, normal slant.
    Normal,
    /// Bold weight, normal slant.
    Bold,
    /// Regular weight, italic slant.
    Italic,
    /// Bold weight, italic slant.
    BoldItalic,
    /// A specific style name resolved by the platform font system (for
    /// example `"Semibold"` or `"Oblique"`).
    Name(String),
}

/// Description of a font face to load.
///
/// `point_size` is an `f32`, so equality is exact-bit `PartialEq`; `Eq` is
/// deliberately not implemented (see [`RasterKey`] for the bit-exact key
/// alternative).
#[derive(Debug, Clone, PartialEq)]
pub struct FontQuery {
    /// Family name as understood by the platform (for example
    /// `"JetBrains Mono"`).
    pub family: String,
    /// Requested style within the family.
    pub style: FontStyle,
    /// Requested size in points. Values outside `(0, 3999]` are rejected by
    /// [`FontQuery::validate`], mirroring the upstream clamp range.
    pub point_size: f32,
}

impl FontQuery {
    /// Validates the query before it reaches any upstream call.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when the family is blank or the point
    /// size is not finite within `(0, 3999]`.
    pub fn validate(&self) -> Result<(), RenderError> {
        if self.family.trim().is_empty() {
            return Err(RenderError::InvalidInput {
                reason: "font family must not be empty",
            });
        }
        if !(self.point_size.is_finite() && self.point_size > 0.0 && self.point_size <= 3999.0) {
            return Err(RenderError::InvalidInput {
                reason: "point size must be finite within (0, 3999]",
            });
        }
        Ok(())
    }
}

/// Identifies exactly what to rasterize: one character of one face at one
/// size.
#[derive(Debug, Clone, Copy)]
pub struct RasterKey {
    /// Character to rasterize (already resolved by the text layer; combining
    /// marks are separate keys).
    pub character: char,
    /// Face handle from [`GlyphRasterizer::load_font`].
    pub font: FontId,
    /// Size in points. Compared and hashed by bit pattern so every distinct
    /// finite value is a distinct key; constructors reject non-finite sizes.
    pub point_size: f32,
}

impl RasterKey {
    /// Builds a key, rejecting non-finite or non-positive sizes.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when `point_size` is NaN, infinite, or
    /// non-positive.
    pub fn new(character: char, font: FontId, point_size: f32) -> Result<Self, RenderError> {
        if !(point_size.is_finite() && point_size > 0.0) {
            return Err(RenderError::InvalidInput {
                reason: "raster size must be finite and positive",
            });
        }
        Ok(Self {
            character,
            font,
            point_size,
        })
    }
}

impl PartialEq for RasterKey {
    fn eq(&self, other: &Self) -> bool {
        // Bit-pattern equality keeps the total-order/hash contract intact for
        // `-0.0` vs `0.0` while `new` has already excluded non-finite sizes.
        self.character == other.character
            && self.font == other.font
            && self.point_size.to_bits() == other.point_size.to_bits()
    }
}

impl Eq for RasterKey {}

impl std::hash::Hash for RasterKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.character.hash(state);
        self.font.hash(state);
        self.point_size.to_bits().hash(state);
    }
}

/// Pixel layout of an owned glyph bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphMetrics {
    /// Horizontal offset from the pen position to the bitmap's left edge;
    /// may be negative.
    pub left: i32,
    /// Vertical offset from the baseline up to the bitmap's top edge; may be
    /// negative (descenders).
    pub top: i32,
    /// Bitmap width in pixels (non-negative; zero for blank glyphs that
    /// still carry metrics).
    pub width: i32,
    /// Bitmap height in pixels.
    pub height: i32,
    /// Advance as `[x, y]` in pixels.
    pub advance: [i32; 2],
}

/// Channel layout of [`GlyphBitmap::data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitmapFormat {
    /// Three bytes per pixel carrying coverage (grayscale antialiasing) or
    /// subpixel triplets, as produced by the upstream engine.
    Rgb,
    /// Four bytes per pixel, premultiplied alpha.
    Rgba,
}

impl BitmapFormat {
    /// Bytes per pixel for this format.
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            BitmapFormat::Rgb => 3,
            BitmapFormat::Rgba => 4,
        }
    }
}

/// Owned rasterized glyph bitmap with an enforced length invariant:
/// `data.len() == width * height * channels` (with overflow checked), or the
/// empty bitmap for zero-sized glyphs.
///
/// Construction goes through [`GlyphBitmap::try_new`]; the invariant holds by
/// construction everywhere else in the crate, which lets downstream consumers
/// (atlas upload, software blit) trust lengths without re-validating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphBitmap {
    /// Placement metrics relative to the pen/baseline.
    pub metrics: GlyphMetrics,
    /// Channel layout of `data`.
    pub format: BitmapFormat,
    /// Pixel data, row-major from the top-left, length-validated.
    pub data: Vec<u8>,
}

impl GlyphBitmap {
    /// Validates dimensions and buffer length, then takes ownership.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when either dimension is negative, when
    /// the pixel count overflows `usize`, or when `data.len()` does not equal
    /// `width * height * channels`. This is the bounded-memory gate for all
    /// bitmap data entering the crate.
    pub fn try_new(
        metrics: GlyphMetrics,
        format: BitmapFormat,
        data: Vec<u8>,
    ) -> Result<Self, RenderError> {
        let GlyphMetrics {
            left: _,
            top: _,
            width,
            height,
            advance: _,
        } = metrics;
        if width < 0 || height < 0 {
            return Err(RenderError::InvalidInput {
                reason: "glyph dimensions must be non-negative",
            });
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(usize::try_from(height).ok()?));
        let expected = match expected {
            Some(e) => e.checked_mul(format.channels()),
            None => None,
        };
        match expected {
            Some(len) if len == data.len() => Ok(Self {
                metrics,
                format,
                data,
            }),
            // The mismatch is reported without echoing sizes back: reasons
            // stay 'static so hostile inputs cannot grow error allocations.
            Some(_) => Err(RenderError::InvalidInput {
                reason: "glyph bitmap length does not match its dimensions",
            }),
            None => Err(RenderError::InvalidInput {
                reason: "glyph pixel count overflows the address space bound",
            }),
        }
    }

    /// True when the bitmap carries no pixels (metrics-only blank glyph).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.data.is_empty()
    }
}

impl fmt::Display for GlyphBitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} {:?}",
            self.metrics.width, self.metrics.height, self.format
        )
    }
}

/// The Bitty-owned rasterization contract (see module docs).
///
/// Implementors must be deterministic per session: identical keys yield
/// bit-identical results. Errors mean "this request failed"; callers may
/// retry with different inputs but implementations should not hold unbounded
/// internal state.
pub trait GlyphRasterizer {
    /// Loads (or matches) a font face, returning its session handle.
    ///
    /// Loading the same query twice may return the same or a fresh handle;
    /// both are valid, but each returned handle must rasterize identically.
    ///
    /// # Errors
    ///
    /// [`RenderError::FontNotFound`] when no face matches, or
    /// [`RenderError::UpstreamRasterizer`] on platform font-stack failure.
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError>;

    /// Rasterizes one character. Returns `Ok(None)` when the character has
    /// no drawable representation (whitespace, missing glyph mapped to blank)
    /// — a cacheable negative result, not a failure.
    ///
    /// # Errors
    ///
    /// [`RenderError::UnknownFontHandle`] for stale handles,
    /// [`RenderError::UpstreamRasterizer`] for engine failures.
    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(width: i32, height: i32) -> GlyphMetrics {
        GlyphMetrics {
            left: 0,
            top: 8,
            width,
            height,
            advance: [width, 0],
        }
    }

    #[test]
    fn try_new_enforces_length_invariant() {
        let m = metrics(2, 2);
        assert!(GlyphBitmap::try_new(m, BitmapFormat::Rgb, vec![0; 12]).is_ok());
        assert!(matches!(
            GlyphBitmap::try_new(m, BitmapFormat::Rgb, vec![0; 11]),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            GlyphBitmap::try_new(m, BitmapFormat::Rgba, vec![0; 12]),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn try_new_rejects_negative_dimensions() {
        assert!(matches!(
            GlyphBitmap::try_new(metrics(-1, 2), BitmapFormat::Rgb, Vec::new()),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn try_new_accepts_empty_bitmap_and_flags_it_blank() {
        let bm = GlyphBitmap::try_new(metrics(0, 0), BitmapFormat::Rgb, Vec::new()).unwrap();
        assert!(bm.is_blank());
        assert_eq!(bm.to_string(), "0x0 Rgb");
    }

    #[test]
    fn huge_dimension_overflow_is_rejected_not_panicked() {
        let m = metrics(i32::MAX, i32::MAX);
        assert!(matches!(
            GlyphBitmap::try_new(m, BitmapFormat::Rgba, vec![0; 4]),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn font_query_validation() {
        let ok = FontQuery {
            family: "mono".into(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        assert!(ok.validate().is_ok());

        let blank = FontQuery {
            family: "   ".into(),
            style: FontStyle::Name("Bold".into()),
            point_size: 10.0,
        };
        assert!(blank.validate().is_err());

        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, 4000.0] {
            let q = FontQuery {
                family: "mono".into(),
                style: FontStyle::Normal,
                point_size: bad,
            };
            assert!(q.validate().is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn raster_key_equality_is_bit_exact() {
        let id = FontId::next(&mut 1);
        let a = RasterKey::new('a', id, 12.5).unwrap();
        let b = RasterKey::new('a', id, 12.5).unwrap();
        let c = RasterKey::new('a', id, 12.500_001).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);

        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));

        assert!(RasterKey::new('x', id, f32::NAN).is_err());
        assert!(RasterKey::new('x', id, 0.0).is_err());
    }

    #[test]
    fn font_ids_are_unique_per_counter() {
        let mut counter = 0u64;
        let a = FontId::next(&mut counter);
        let b = FontId::next(&mut counter);
        assert_ne!(a, b);
        assert_eq!(b.as_u64(), 1);
    }
}
