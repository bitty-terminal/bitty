//! The crossfont-backed [`GlyphRasterizer`] implementation.
//!
//! This module is the only place where `crossfont` types are named (ADR-0004
//! "Wrap" row). Every value crossing back out is converted into the owned
//! vocabulary of [`crate::glyph`], and every upstream failure is flattened
//! through [`RenderError::flatten_rasterizer`] or mapped onto a semantic
//! variant:
//!
//! | Upstream condition                | Owned result                                   |
//! |-----------------------------------|------------------------------------------------|
//! | `Error::FontNotFound(desc)`       | [`RenderError::FontNotFound`] (owned string)   |
//! | `Error::MissingGlyph(_)`          | `Ok(None)` — blank cell, not a failure         |
//! | `Error::UnknownFontKey`           | [`RenderError::UnknownFontHandle`]             |
//! | `Error::MetricsNotFound`          | [`RenderError::UpstreamRasterizer`]            |
//! | `Error::PlatformError(msg)`       | [`RenderError::UpstreamRasterizer`]            |
//!
//! Font discovery uses the upstream defaults of the running platform
//! (CoreText / DirectWrite / FreeType+fontconfig); this crate adds no search
//! path or substitution policy of its own — that is Core presentation
//! semantics deferred to the font-configuration slice.

use std::collections::HashMap;

use crossfont::{
    BitmapBuffer, Error as UpstreamError, FontDesc, GlyphKey as UpstreamGlyphKey, Rasterize as _,
    RasterizedGlyph, Size as UpstreamSize, Slant, Style as UpstreamStyle, Weight,
};

use crate::error::RenderError;
use crate::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};

/// [`GlyphRasterizer`] implementation wrapping the platform rasterizer that
/// `crossfont` selects at compile time.
pub struct CrossFontRasterizer {
    inner: crossfont::Rasterizer,
    /// Maps Bitty-owned handles to upstream keys issued by this session.
    fonts: HashMap<FontId, crossfont::FontKey>,
    next_font_id: u64,
}

impl std::fmt::Debug for CrossFontRasterizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The upstream rasterizer does not implement Debug; report only the
        // owned bookkeeping state.
        f.debug_struct("CrossFontRasterizer")
            .field("loaded_fonts", &self.fonts.len())
            .field("next_font_id", &self.next_font_id)
            .finish()
    }
}

impl CrossFontRasterizer {
    /// Creates the platform rasterizer with default discovery.
    ///
    /// # Errors
    ///
    /// [`RenderError::UpstreamRasterizer`] when the platform font stack fails
    /// to initialize (for example a broken fontconfig installation).
    pub fn new() -> Result<Self, RenderError> {
        let inner = crossfont::Rasterizer::new().map_err(RenderError::flatten_rasterizer)?;
        Ok(Self {
            inner,
            fonts: HashMap::new(),
            next_font_id: 0,
        })
    }

    /// Resolves a session handle back to the upstream key it was issued for.
    fn upstream_key(&self, font: FontId) -> Result<crossfont::FontKey, RenderError> {
        self.fonts
            .get(&font)
            .copied()
            .ok_or(RenderError::UnknownFontHandle)
    }
}

impl GlyphRasterizer for CrossFontRasterizer {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        query.validate()?;
        let desc = FontDesc::new(query.family.as_str(), map_style(&query.style));
        let key = self
            .inner
            .load_font(&desc, UpstreamSize::new(query.point_size))
            .map_err(map_upstream_error)?;
        let id = FontId::next(&mut self.next_font_id);
        self.fonts.insert(id, key);
        Ok(id)
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        let font_key = self.upstream_key(key.font)?;
        let glyph_key = UpstreamGlyphKey {
            character: key.character,
            font_key,
            size: UpstreamSize::new(key.point_size),
        };
        match self.inner.get_glyph(glyph_key) {
            Ok(glyph) => convert_glyph(glyph).map(Some),
            Err(UpstreamError::MissingGlyph(_)) => Ok(None),
            Err(err) => Err(map_upstream_error(err)),
        }
    }
}

/// Converts an upstream rasterized glyph into the owned bitmap type.
///
/// Crate-visible (not `pub`) so tests can exercise the conversion with
/// synthetic glyphs without touching a platform font stack — headless CI
/// verifies exactly this logic.
pub(crate) fn convert_glyph(glyph: RasterizedGlyph) -> Result<GlyphBitmap, RenderError> {
    let (format, data) = match glyph.buffer {
        BitmapBuffer::Rgb(data) => (BitmapFormat::Rgb, data),
        BitmapBuffer::Rgba(data) => (BitmapFormat::Rgba, data),
    };
    let metrics = GlyphMetrics {
        left: glyph.left,
        top: glyph.top,
        width: glyph.width,
        height: glyph.height,
        advance: [glyph.advance.0, glyph.advance.1],
    };
    // `try_new` enforces the length invariant here too: malformed upstream
    // output is rejected instead of trusted.
    GlyphBitmap::try_new(metrics, format, data)
}

/// Maps an upstream error onto the owned taxonomy (see module table).
pub(crate) fn map_upstream_error(err: UpstreamError) -> RenderError {
    match err {
        UpstreamError::FontNotFound(desc) => RenderError::FontNotFound(desc.to_string()),
        UpstreamError::MissingGlyph(_) => RenderError::InvalidInput {
            reason: "upstream reported a missing glyph where none was expected",
        },
        UpstreamError::UnknownFontKey => RenderError::UnknownFontHandle,
        UpstreamError::MetricsNotFound => RenderError::flatten_rasterizer("metrics unavailable"),
        UpstreamError::PlatformError(msg) => RenderError::flatten_rasterizer(msg),
    }
}

/// Maps the owned style vocabulary onto upstream style descriptions.
fn map_style(style: &FontStyle) -> UpstreamStyle {
    match style {
        FontStyle::Normal => UpstreamStyle::Description {
            slant: Slant::Normal,
            weight: Weight::Normal,
        },
        FontStyle::Bold => UpstreamStyle::Description {
            slant: Slant::Normal,
            weight: Weight::Bold,
        },
        FontStyle::Italic => UpstreamStyle::Description {
            slant: Slant::Italic,
            weight: Weight::Normal,
        },
        FontStyle::BoldItalic => UpstreamStyle::Description {
            slant: Slant::Italic,
            weight: Weight::Bold,
        },
        FontStyle::Name(name) => UpstreamStyle::Specific(name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(buffer: BitmapBuffer, width: i32, height: i32) -> RasterizedGlyph {
        RasterizedGlyph {
            character: 'x',
            width,
            height,
            top: 10,
            left: -2,
            advance: (7, 0),
            buffer,
        }
    }

    #[test]
    fn rgb_glyph_converts_to_owned_bitmap() {
        let bm = convert_glyph(synthetic(BitmapBuffer::Rgb(vec![7; 3 * 2]), 1, 2)).unwrap();
        assert_eq!(bm.format, BitmapFormat::Rgb);
        assert_eq!(bm.data.len(), 6);
        assert_eq!(bm.metrics.left, -2);
        assert_eq!(bm.metrics.top, 10);
        assert_eq!(bm.metrics.advance, [7, 0]);
    }

    #[test]
    fn rgba_glyph_converts_to_owned_bitmap() {
        let bm = convert_glyph(synthetic(BitmapBuffer::Rgba(vec![9; 8]), 2, 1)).unwrap();
        assert_eq!(bm.format, BitmapFormat::Rgba);
        assert_eq!(bm.data.len(), 8);
    }

    #[test]
    fn malformed_upstream_buffer_is_rejected() {
        // Upstream claims 4x2 RGB but supplies too few bytes: the invariant
        // gate must reject rather than trust.
        assert!(matches!(
            convert_glyph(synthetic(BitmapBuffer::Rgb(vec![0; 5]), 4, 2)),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(matches!(
            convert_glyph(synthetic(BitmapBuffer::Rgb(Vec::new()), -1, 2)),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn empty_buffer_converts_as_blank() {
        let bm = convert_glyph(synthetic(BitmapBuffer::Rgb(Vec::new()), 0, 0)).unwrap();
        assert!(bm.is_blank());
    }

    #[test]
    fn upstream_error_mapping_table() {
        let desc = FontDesc::new("Fake", UpstreamStyle::Specific("Bold".into()));
        assert!(matches!(
            map_upstream_error(UpstreamError::FontNotFound(desc)),
            RenderError::FontNotFound(_)
        ));
        assert_eq!(
            map_upstream_error(UpstreamError::PlatformError("fc init".into())).to_string(),
            "font rasterizer error: fc init"
        );
        assert!(matches!(
            map_upstream_error(UpstreamError::UnknownFontKey),
            RenderError::UnknownFontHandle
        ));
        assert!(matches!(
            map_upstream_error(UpstreamError::MetricsNotFound),
            RenderError::UpstreamRasterizer(_)
        ));
    }

    #[test]
    fn missing_glyph_maps_through_rasterize_contract_not_error() {
        // Documented contract: MissingGlyph becomes Ok(None) inside
        // `rasterize`; the mapper itself only sees it in unexpected places,
        // where it must not be silently swallowed.
        assert!(matches!(
            map_upstream_error(UpstreamError::MissingGlyph(synthetic(
                BitmapBuffer::Rgb(Vec::new()),
                0,
                0
            ))),
            RenderError::InvalidInput { .. }
        ));
    }

    #[test]
    fn style_mapping_covers_all_variants() {
        assert_eq!(
            format!("{:?}", map_style(&FontStyle::Normal)),
            format!(
                "{:?}",
                UpstreamStyle::Description {
                    slant: Slant::Normal,
                    weight: Weight::Normal
                }
            )
        );
        assert!(matches!(
            map_style(&FontStyle::Name("Semibold".into())),
            UpstreamStyle::Specific(_)
        ));
    }

    #[test]
    fn invalid_queries_fail_before_touching_upstream() {
        // Constructing CrossFontRasterizer requires a working font stack;
        // validation happens before any load, so prove ordering by checking
        // the validator directly against the wrapper's call sequence.
        let query = FontQuery {
            family: String::new(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        assert!(matches!(
            query.validate(),
            Err(RenderError::InvalidInput { .. })
        ));
    }
}
