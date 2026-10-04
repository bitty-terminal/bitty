//! The Bitty-owned shaped rasterizer: single-glyph path over
//! `fontdb` + `swash`, plus the run-shaping skeleton Phase B extends.
//!
//! This module is the only place where `fontdb`, `swash`, or `harfrust`
//! types are named (ADR-0004 "Wrap" row). Every value crossing back out is
//! converted into the owned vocabulary of [`crate::glyph`], and every
//! upstream failure is flattened through [`RenderError::flatten_rasterizer`]
//! or mapped onto a semantic variant. No upstream type appears in this
//! crate's public API.
//!
//! Composition (mirroring `cosmic-text`'s `FontSystem` without adopting it):
//! `fontdb::Database` discovers faces for the primary + tails,
//! `harfrust` shapes runs with per-face shape plans, and `swash` rasterizes
//! outlines and bitmap strikes from the same font bytes (shared
//! `skrifa`/`read-fonts` parser underneath — no second font parser beside
//! `fontdb`'s query-time `ttf-parser`).
//!
//! Phase A serves the single-scalar [`GlyphRasterizer`] contract
//! ([`SwashSingle`]); the run-shaping side ([`RunAttrs`],
//! [`ShapedCluster`], [`ShapePlanKey`], [`SwashSingle::shape_run`]) is the
//! skeleton Phase B extends with run caches and grid emission. Grid and
//! overlay paths keep using [`SwashSingle`] through
//! [`FallbackRasterizer`](crate::fallback::FallbackRasterizer), so the
//! existing probe tests answer unchanged.
//!
//! Bounds: faces load as owned bytes capped at [`MAX_FACE_BYTES`] each,
//! at most [`MAX_LOADED_FACES`] faces per session; every bitmap crosses
//! [`GlyphBitmap::try_new`], so malformed upstream output is rejected
//! instead of trusted. Font discovery uses `fontdb` defaults (system font
//! directories plus fontconfig XML on Linux); this crate adds no search
//! path of its own.
//!
//! Backend selection: this module is the explicit opt-in stack, not the
//! production default (CTX-0957 additive landing, DEC-0095 — see the
//! `Backend selection` section in [crate docs](crate)). Production
//! construction sites build `crossfont_backend::CrossFontRasterizer`;
//! opt in here by constructing [`SwashSingle::new`] directly (as
//! `tests/shaped_parity.rs` does). Full removal of the crossfont wrap is
//! deferred to CTX-0961.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use fontdb::{Database, Family, Query, Source, Style, Weight};

use crate::error::RenderError;
use crate::glyph::{
    BitmapFormat, FontId, FontMetrics, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics,
    GlyphRasterizer, RasterKey,
};

/// Maximum bytes retained per loaded face (design 1.4).
///
/// One copy per face load (amortized: loads happen per chain build, never
/// per frame). Faces larger than this fail closed with
/// [`RenderError::InvalidInput`] instead of ballooning the session.
pub const MAX_FACE_BYTES: usize = 64 * 1024 * 1024;

/// Maximum faces retained per [`SwashSingle`] session.
///
/// The per-glyph walk loads at most `1 + chain.len()` faces
/// (`<= MAX_FALLBACK_DEPTH`); 64 leaves wide headroom while keeping a
/// pathological reload loop from growing the session without bound.
pub const MAX_LOADED_FACES: usize = 64;

/// Maximum shape plans cached per face (cosmic-text `NUM_SHAPE_PLANS`
/// sizing, design 5). Phase B enforces this on the retained plan cache;
/// Phase A constructs plans per call (no production path shapes yet), so
/// this is the documented bound the cache will honor, not a live limit.
pub const MAX_SHAPE_PLANS_PER_FACE: usize = 6;

/// Points-to-pixels factor, mirroring `crossfont::Size::as_px` (`pt * 96/72`).
///
/// Keeping the exact factor preserves the raster scale the CTX-0157 probe
/// and the default `10x22` cell were accepted against.
const PT_TO_PX: f32 = 96.0 / 72.0;

/// Owned font bytes for one loaded face.
#[derive(Debug, Clone)]
struct StoredFace {
    /// Face bytes (one copy per face load; shared across calls by `Arc`).
    data: Arc<Vec<u8>>,
    /// Face index inside the collection (`fontdb` source index).
    face_index: u32,
}

/// Kind of font data a drawable glyph comes from (owned vocabulary).
///
/// Mirrors the [`SwashSingle`] source order (outline, then embedded
/// bitmap strike, then color bitmap strike). Phase C atlas policy may treat
/// color bitmaps differently; the parity test gates dimension assertions on
/// this (strike selection legitimately differs from the old stack for color
/// glyphs, while coverage must still match exactly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlyphSource {
    /// Scalable outline (`swash` alpha mask).
    Outline,
    /// Embedded monochrome bitmap strike.
    Bitmap,
    /// Embedded color bitmap strike (flattened to `Rgb`; downstream tints
    /// monochrome by cell foreground — color emoji stays a non-goal).
    ColorBitmap,
}

/// Process-wide system font database, scanned once.
///
/// A full `load_system_fonts` pass costs hundreds of milliseconds on
/// font-rich hosts (453ms release / ~1s+ debug for 1583 faces on the
/// reference seat) — paying it per rasterizer would blow the PB-1 startup
/// budget once and per test runtime after that. Discovery is read-only host
/// state, so every [`SwashSingle`] shares one scan. Caveat: a process that
/// mutates font configuration mid-run (fontconfig env) keeps the first
/// scan; no such use exists (production reads the host stack once, tests
/// share the host stack).
static SYSTEM_DB: OnceLock<Database> = OnceLock::new();

/// Returns the shared system database, scanning on first use.
fn system_db() -> &'static Database {
    SYSTEM_DB.get_or_init(|| {
        let mut db = Database::new();
        db.load_system_fonts();
        db
    })
}

/// [`GlyphRasterizer`] over `fontdb` discovery + `swash` rasterization.
///
/// Loads one family per [`load_font`](GlyphRasterizer::load_font) call;
/// production wiring wraps this in
/// [`FallbackRasterizer`](crate::fallback::FallbackRasterizer) (user tier
/// first, then platform tails) exactly as the crossfont backend was
/// wrapped, so per-glyph fallback needs no second implementation.
///
/// Face discovery reads the process-wide [`system_db`] scan; per-face bytes
/// stay session-owned under [`MAX_FACE_BYTES`].
pub struct SwashSingle {
    db: &'static Database,
    faces: HashMap<FontId, StoredFace>,
    /// `fontdb` face ID to session handle: reloads (for example a font-size
    /// change) re-query the same faces, and without this map every reload
    /// would append duplicate entries until `MAX_LOADED_FACES` is exhausted.
    /// The [`GlyphRasterizer::load_font`](crate::glyph::GlyphRasterizer::load_font)
    /// contract allows returning the same handle for a repeated query.
    by_db_id: HashMap<fontdb::ID, FontId>,
    scale_ctx: swash::scale::ScaleContext,
    next_font_id: u64,
}

impl std::fmt::Debug for SwashSingle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwashSingle")
            .field("system_faces", &self.db.len())
            .field("loaded_faces", &self.faces.len())
            .field("cached_db_ids", &self.by_db_id.len())
            .field("next_font_id", &self.next_font_id)
            .finish()
    }
}

impl SwashSingle {
    /// Builds the face database from the host font stack.
    ///
    /// # Errors
    ///
    /// [`RenderError::FontNotFound`] when no system face was discovered
    /// (bare install with no font stack — the caller falls back to the
    /// deterministic headless rasterizer, same as a failed platform stack
    /// before).
    pub fn new() -> Result<Self, RenderError> {
        let db = system_db();
        if db.is_empty() {
            return Err(RenderError::FontNotFound(
                "no system fonts discovered".to_string(),
            ));
        }
        Ok(Self {
            db,
            faces: HashMap::new(),
            by_db_id: HashMap::new(),
            scale_ctx: swash::scale::ScaleContext::new(),
            next_font_id: 0,
        })
    }

    /// Resolves a session handle back to its stored face.
    fn stored(&self, font: FontId) -> Result<&StoredFace, RenderError> {
        self.faces.get(&font).ok_or(RenderError::UnknownFontHandle)
    }

    /// Raster size in pixels for a query, mirroring the crossfont scale.
    fn px(point_size: f32) -> f32 {
        point_size * PT_TO_PX
    }

    /// Loads one face's bytes, bounded by [`MAX_FACE_BYTES`].
    ///
    /// `Binary` sources are already in memory (cloned once per face load).
    /// `File` sources stream through `take(bound + 1)` so a concurrently
    /// growing file cannot slip past the bound (no metadata/read TOCTOU).
    fn load_bytes(&self, id: fontdb::ID) -> Result<(Arc<Vec<u8>>, u32), RenderError> {
        let (source, index) = self
            .db
            .face_source(id)
            .ok_or_else(|| RenderError::FontNotFound("face vanished".to_string()))?;
        let bytes = match source {
            Source::Binary(arc) => {
                let bytes: &[u8] = arc.as_ref().as_ref();
                if bytes.len() > MAX_FACE_BYTES {
                    return Err(RenderError::InvalidInput {
                        reason: "font face exceeds the per-face byte bound",
                    });
                }
                bytes.to_vec()
            }
            Source::File(path) => {
                use std::io::Read as _;
                let file = std::fs::File::open(&path).map_err(RenderError::flatten_rasterizer)?;
                let mut bounded = file.take(MAX_FACE_BYTES as u64 + 1);
                let mut bytes = Vec::new();
                bounded
                    .read_to_end(&mut bytes)
                    .map_err(RenderError::flatten_rasterizer)?;
                if bytes.len() > MAX_FACE_BYTES {
                    return Err(RenderError::InvalidInput {
                        reason: "font face exceeds the per-face byte bound",
                    });
                }
                bytes
            }
        };
        Ok((Arc::new(bytes), index))
    }

    /// Parses a stored face into a borrowed `swash` font reference.
    fn font_ref(face: &StoredFace) -> Result<swash::FontRef<'_>, RenderError> {
        let index = usize::try_from(face.face_index).map_err(|_| RenderError::InvalidInput {
            reason: "font face index exceeds the address space bound",
        })?;
        swash::FontRef::from_index(&face.data, index).ok_or_else(|| {
            RenderError::flatten_rasterizer("face bytes rejected by the font parser")
        })
    }

    /// Renders one glyph to an owned bitmap at `px` pixels.
    ///
    /// Sources tried in order: scalable outline, then the best-fit embedded
    /// bitmap strike, then the best-fit color bitmap strike. Color bitmaps
    /// flatten to `Rgb` (alpha dropped): the downstream atlas path averages
    /// to luminance and tints by the cell foreground, which is the
    /// documented monochrome-flatten behavior for emoji-presentation scalars
    /// (color emoji stays a non-goal). Returns the winning source alongside
    /// the bitmap so coverage probes share one selection truth.
    fn render_glyph(
        &mut self,
        face: &StoredFace,
        glyph_id: u16,
        px: f32,
    ) -> Result<(GlyphBitmap, GlyphSource), RenderError> {
        use swash::scale::{Render, Source as SwashSource, StrikeWith};
        let font = Self::font_ref(face)?;
        let mut scaler = self.scale_ctx.builder(font).size(px).hint(true).build();
        let image = Render::new(&[
            SwashSource::Outline,
            SwashSource::Bitmap(StrikeWith::BestFit),
            SwashSource::ColorBitmap(StrikeWith::BestFit),
        ])
        .render(&mut scaler, glyph_id)
        .ok_or_else(|| RenderError::flatten_rasterizer("glyph has no renderable source"))?;
        let source = match image.source {
            SwashSource::Outline => GlyphSource::Outline,
            SwashSource::Bitmap(_) => GlyphSource::Bitmap,
            SwashSource::ColorBitmap(_) => GlyphSource::ColorBitmap,
            SwashSource::ColorOutline(_) => {
                return Err(RenderError::flatten_rasterizer(
                    "layered color outlines are not served on this path",
                ));
            }
        };
        let width =
            i32::try_from(image.placement.width).map_err(|_| RenderError::InvalidInput {
                reason: "glyph bitmap width exceeds the placement bound",
            })?;
        let height =
            i32::try_from(image.placement.height).map_err(|_| RenderError::InvalidInput {
                reason: "glyph bitmap height exceeds the placement bound",
            })?;
        let data = match image.content {
            swash::scale::image::Content::Mask => {
                // 1 byte/px alpha coverage -> triplicated `Rgb` coverage,
                // the format the atlas path averages to luminance.
                let mut rgb = Vec::with_capacity(image.data.len() * 3);
                for b in &image.data {
                    rgb.extend_from_slice(&[*b, *b, *b]);
                }
                rgb
            }
            swash::scale::image::Content::SubpixelMask => {
                return Err(RenderError::flatten_rasterizer(
                    "subpixel mask content is not served on this path",
                ));
            }
            swash::scale::image::Content::Color => {
                // 32-bit RGBA -> `Rgb` (alpha dropped; bitmap strikes are
                // opaque, and downstream tints monochrome by cell fg).
                if image.data.len() % 4 != 0 {
                    return Err(RenderError::InvalidInput {
                        reason: "color bitmap length is not a multiple of 4",
                    });
                }
                let mut rgb = Vec::with_capacity(image.data.len() / 4 * 3);
                for rgba in image.data.chunks_exact(4) {
                    rgb.extend_from_slice(&rgba[..3]);
                }
                rgb
            }
        };
        let proxy = swash::proxy::MetricsProxy::from_font(&font);
        let glyph_metrics = proxy.materialize_glyph_metrics(&font, &[]);
        let units = glyph_metrics.units_per_em();
        let advance_units = glyph_metrics.advance_width(glyph_id);
        let advance_px = if units == 0 {
            0.0
        } else {
            advance_units * px / f32::from(units)
        };
        let metrics = GlyphMetrics {
            left: image.placement.left,
            top: image.placement.top,
            width,
            height,
            advance: [advance_px.round() as i32, 0],
        };
        // `try_new` enforces the length invariant here too: malformed
        // upstream output is rejected instead of trusted.
        let bitmap = GlyphBitmap::try_new(metrics, BitmapFormat::Rgb, data)?;
        Ok((bitmap, source))
    }

    /// Probes which source would render `glyph_id` — the cheap coverage
    /// half of [`render_glyph`](Self::render_glyph) without pixel output.
    ///
    /// Tiered in the same source order the renderer tries
    /// (outline, bitmap strike, color bitmap strike), so the probe and the
    /// render agree by construction (verified: space probes `Outline` on
    /// both, matching the empty-mask render).
    fn probe_source(scaler: &mut swash::scale::Scaler<'_>, glyph_id: u16) -> Option<GlyphSource> {
        use swash::scale::StrikeWith;
        if scaler.scale_outline(glyph_id).is_some() {
            return Some(GlyphSource::Outline);
        }
        if scaler.scale_bitmap(glyph_id, StrikeWith::BestFit).is_some() {
            return Some(GlyphSource::Bitmap);
        }
        if scaler
            .scale_color_bitmap(glyph_id, StrikeWith::BestFit)
            .is_some()
        {
            return Some(GlyphSource::ColorBitmap);
        }
        None
    }

    /// Reports which source covers `character` in `font` at `point_size`.
    ///
    /// `Ok(None)` means no drawable representation (unmapped scalar or no
    /// renderable source — the cacheable negative, matching
    /// [`rasterize`](GlyphRasterizer::rasterize)); `Some(kind)` names the
    /// winning source. Phase C atlas policy and the parity test gate on
    /// this; Phase B grid emission probes the same way per cluster.
    ///
    /// # Errors
    ///
    /// [`RenderError::UnknownFontHandle`] for stale handles,
    /// [`RenderError::InvalidInput`] for non-finite/non-positive sizes.
    pub fn glyph_source(
        &mut self,
        font: FontId,
        character: char,
        point_size: f32,
    ) -> Result<Option<GlyphSource>, RenderError> {
        if !(point_size.is_finite() && point_size > 0.0) {
            return Err(RenderError::InvalidInput {
                reason: "probe size must be finite and positive",
            });
        }
        let owned = self.stored(font)?.clone();
        let font_ref = Self::font_ref(&owned)?;
        let glyph_id: u16 = swash::Charmap::from_font(&font_ref).map(character);
        if glyph_id == 0 {
            return Ok(None);
        }
        let mut scaler = self
            .scale_ctx
            .builder(font_ref)
            .size(Self::px(point_size))
            .hint(true)
            .build();
        Ok(Self::probe_source(&mut scaler, glyph_id))
    }
}

/// Maps the owned style vocabulary onto `fontdb` match attributes.
fn style_attrs(style: &FontStyle) -> (Weight, Style) {
    match style {
        FontStyle::Normal => (Weight::NORMAL, Style::Normal),
        FontStyle::Bold => (Weight::BOLD, Style::Normal),
        FontStyle::Italic => (Weight::NORMAL, Style::Italic),
        FontStyle::BoldItalic => (Weight::BOLD, Style::Italic),
        // Style-name matching (`"Semibold"`, `"Oblique"`) has no `fontdb`
        // query form; resolve the family with default attributes. Known gap
        // vs the crossfont `Specific` style, recorded for Phase C
        // per-platform verification.
        FontStyle::Name(_) => (Weight::NORMAL, Style::Normal),
    }
}

impl GlyphRasterizer for SwashSingle {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        query.validate()?;
        let (weight, style_attr) = style_attrs(&query.style);
        let families = [Family::Name(query.family.as_str())];
        let request = Query {
            families: &families,
            weight,
            style: style_attr,
            ..Default::default()
        };
        let id = self
            .db
            .query(&request)
            .ok_or_else(|| RenderError::FontNotFound(query.family.clone()))?;
        // Reloads re-query already-loaded faces: reuse the handle instead
        // of appending a duplicate entry (the bound below guards genuinely
        // new faces only).
        if let Some(existing) = self.by_db_id.get(&id) {
            return Ok(*existing);
        }
        if self.faces.len() >= MAX_LOADED_FACES {
            return Err(RenderError::InvalidInput {
                reason: "rasterizer session face bound exceeded",
            });
        }
        let (data, face_index) = self.load_bytes(id)?;
        let stored = StoredFace { data, face_index };
        // Reject bytes the parser cannot use now (fail-closed at load, not
        // at first rasterize, so a corrupt face never enters the chain).
        Self::font_ref(&stored)?;
        let font = FontId::next(&mut self.next_font_id);
        self.faces.insert(font, stored);
        self.by_db_id.insert(id, font);
        Ok(font)
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if !(key.point_size.is_finite() && key.point_size > 0.0) {
            return Err(RenderError::InvalidInput {
                reason: "raster size must be finite and positive",
            });
        }
        // Clone the Arc (not the bytes) so the scale context borrow ends
        // before the mutable render call.
        let owned = self.stored(key.font)?.clone();
        let font = Self::font_ref(&owned)?;
        let glyph_id: u16 = swash::Charmap::from_font(&font).map(key.character);
        if glyph_id == 0 {
            // Unmapped scalar: blank cell, not a failure (tofu policy lives
            // in the fallback `resolve` layer above).
            return Ok(None);
        }
        let (bitmap, _) = self.render_glyph(&owned, glyph_id, Self::px(key.point_size))?;
        Ok(Some(bitmap))
    }

    fn font_metrics(
        &self,
        font: FontId,
        point_size: f32,
    ) -> Result<Option<FontMetrics>, RenderError> {
        if !(point_size.is_finite() && point_size > 0.0) {
            return Err(RenderError::InvalidInput {
                reason: "raster size must be finite and positive",
            });
        }
        self.stored(font)?;
        // Phase A answers "no measurement" (always valid per the trait).
        // Rationale: the crossfont FreeType backend this replaces reported
        // degenerate metrics on Linux (unrounded `average_advance`,
        // zero `line_height`/`descent` — see the CTX-0957 baseline fixture),
        // so the grid always took the legacy fixed-baseline rule there;
        // answering `None` keeps that path bit-identical. Wiring real
        // swash metrics plus the baseline policy they imply is Phase B work
        // with per-OS evidence (macOS/Windows crossfont backends did report
        // usable metrics, so this is a known Phase C verification point).
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Phase B shaping skeleton
// ---------------------------------------------------------------------------

/// Attributes for one shaped run (grid row segment, LTR only).
///
/// The terminal grid pins logical order (bidi/reordering is an explicit
/// non-goal, parity with the kitty grid model); direction is therefore not
/// a field — every run shapes left-to-right.
#[derive(Debug, Clone, PartialEq)]
pub struct RunAttrs {
    /// OpenType feature overrides for this run (already validated).
    pub features: Vec<bitty_config::types::OpenTypeFeature>,
    /// Point size for advances (`(0, 3999]`, like [`FontQuery`]).
    pub point_size: f32,
}

/// One shaped cluster: the glyph(s) covering a grid cell range.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedCluster {
    /// Grid columns this cluster covers (start, count).
    pub cells: (usize, usize),
    /// Shaped glyph id in `face`.
    pub glyph_id: u16,
    /// Face that covered this cluster (primary or fallback tail).
    pub face: FontId,
    /// Horizontal advance in pixels.
    pub x_advance_px: f32,
    /// Horizontal offset in pixels (GPOS mark positioning).
    pub x_offset_px: f32,
    /// True when no loaded face covered this cluster (caller paints tofu).
    pub uncovered: bool,
    /// Byte offset of this cluster's text start in the outer run (splice
    /// bookkeeping for tail reshapes; grid emission reads `cells` only).
    pub byte_offset: usize,
}

/// Cache key for a per-face shape plan (Phase B retains plans under this).
///
/// Plans are a pure function of (face, direction, script, language,
/// features); hashing the validated feature list keeps the key owned while
/// the plan itself borrows face bytes. Phase A constructs plans per call;
/// Phase B stores at most [`MAX_SHAPE_PLANS_PER_FACE`] plans per face under
/// this key (cosmic-text `NUM_SHAPE_PLANS` sizing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShapePlanKey {
    /// Face the plan was built for.
    pub face: FontId,
    /// Hash of the run feature list.
    pub features_hash: u64,
}

impl ShapePlanKey {
    /// Derives the key for `face` + `features` (FNV-1a over tag bytes and
    /// values; deterministic across processes, no DoS surface — shaping
    /// plans are never keyed by untrusted input lengths).
    #[must_use]
    pub fn new(face: FontId, features: &[bitty_config::types::OpenTypeFeature]) -> Self {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;
        let mut hash = FNV_OFFSET;
        for f in features {
            for b in f.tag {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            for b in f.value.to_le_bytes() {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        Self {
            face,
            features_hash: hash,
        }
    }
}

/// Converts validated config features to `harfrust` global features.
#[must_use]
pub fn harfrust_features(
    features: &[bitty_config::types::OpenTypeFeature],
) -> Vec<harfrust::Feature> {
    features
        .iter()
        .map(|f| harfrust::Feature::new(harfrust::Tag::new(&f.tag), f.value, ..))
        .collect()
}

/// Sums grid cell widths over a text range (zero-width marks contribute 0,
/// CJK/emoji contribute 2 — grid truth stays authoritative).
#[must_use]
pub fn cells_for_range(text: &str) -> usize {
    text.chars()
        .map(|c| usize::from(bitty_term_state::char_cell_width(c)))
        .sum()
}

/// Inputs for one single-face shape call.
///
/// Bundles the single-face shape arguments (keeps the method under the
/// argument-count lint); every field borrows the caller's data, so no
/// ownership crosses the call.
struct ShapeInput<'a> {
    /// Already-resolved face to shape with.
    face: &'a StoredFace,
    /// Handle tagging the produced clusters.
    id: FontId,
    /// Text slice to shape (whole run or one tail gap).
    text: &'a str,
    /// Byte offset of `text` in the outer run.
    base_offset: usize,
    /// Grid column of `text` in the outer run.
    col_base: usize,
    /// Raster size in pixels.
    px: f32,
    /// Validated run features as `harfrust` globals.
    features: &'a [harfrust::Feature],
}

impl SwashSingle {
    /// Shapes one LTR run over the face `chain` (primary first).
    ///
    /// Shapes with the primary face, then re-shapes each maximal uncovered
    /// char range against the tail faces in order (first fully-covering
    /// face wins — the `cosmic-text shape_run` pattern reduced to the grid);
    /// ranges no face covers keep `uncovered = true` for the tofu path.
    /// Combining marks stay attached to their base: cluster byte ranges map
    /// back onto `text`, and cell counts sum grid truth over those ranges.
    ///
    /// Coverage means drawable, not merely mapped: a cluster is covered
    /// only when the face holds a renderable source for it (same probe the
    /// rasterizer serves — a cmap entry with no outline still counts as
    /// uncovered, so shape coverage and raster coverage agree).
    ///
    /// # Errors
    ///
    /// [`RenderError::UnknownFontHandle`] when `chain` is empty or names a
    /// stale face, [`RenderError::UpstreamRasterizer`] when shaping fails.
    pub fn shape_run(
        &mut self,
        text: &str,
        chain: &[FontId],
        attrs: &RunAttrs,
    ) -> Result<Vec<ShapedCluster>, RenderError> {
        if !(attrs.point_size.is_finite() && attrs.point_size > 0.0) {
            return Err(RenderError::InvalidInput {
                reason: "shape size must be finite and positive",
            });
        }
        let (primary, tails) = chain.split_first().ok_or(RenderError::UnknownFontHandle)?;
        let primary_face = self.stored(*primary)?.clone();
        let features = harfrust_features(&attrs.features);
        let px = Self::px(attrs.point_size);
        let primary_input = ShapeInput {
            face: &primary_face,
            id: *primary,
            text,
            base_offset: 0,
            col_base: 0,
            px,
            features: &features,
        };
        let mut clusters = self.shape_with(primary_input)?;
        // Fallback reshape: maximal uncovered char ranges, tails in order.
        let mut index = 0;
        while index < clusters.len() {
            if !clusters[index].uncovered {
                index += 1;
                continue;
            }
            let mut end = index + 1;
            while end < clusters.len() && clusters[end].uncovered {
                end += 1;
            }
            let byte_start = clusters[index].byte_offset;
            let byte_end = if end < clusters.len() {
                clusters[end].byte_offset
            } else {
                text.len()
            };
            let gap = text.get(byte_start..byte_end).ok_or_else(|| {
                RenderError::UpstreamRasterizer("shaper returned an out-of-range gap".to_string())
            })?;
            // Outer-run column where the gap starts (gap clusters are
            // contiguous, so the first uncovered cluster names it).
            let col_base = clusters[index].cells.0;
            for tail in tails {
                let tail_face = self.stored(*tail)?.clone();
                let tail_input = ShapeInput {
                    face: &tail_face,
                    id: *tail,
                    text: gap,
                    base_offset: byte_start,
                    col_base,
                    px,
                    features: &features,
                };
                let reshaped = self.shape_with(tail_input)?;
                if reshaped.iter().all(|c| !c.uncovered) {
                    let inserted = reshaped.len();
                    clusters.splice(index..end, reshaped);
                    end = index + inserted;
                    break;
                }
            }
            index = end;
        }
        Ok(clusters)
    }

    /// Shapes `text` with one face, tagging clusters with `face`.
    ///
    /// `base_offset`/`col_base` locate `text` in the outer run (both 0 for
    /// the primary shape; the gap start for tail reshapes, so spliced
    /// clusters keep outer-run columns). Positions come out
    /// in 26.6 units (`64 * px` scale), so advances divide back to pixels.
    /// Coverage is probed per cluster through the scaler (same source order
    /// the rasterizer serves): a cmap entry with no renderable source stays
    /// uncovered (tofu), so shape coverage agrees with raster coverage.
    fn shape_with(&mut self, input: ShapeInput<'_>) -> Result<Vec<ShapedCluster>, RenderError> {
        let ShapeInput {
            face,
            id,
            text,
            base_offset,
            col_base,
            px,
            features,
        } = input;
        let blob: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> = face.data.clone();
        let font = harfrust::Font::new(blob, face.face_index)
            .ok_or_else(|| RenderError::flatten_rasterizer("face rejected by harfrust"))?;
        // Per-call plan (Phase A): no production path shapes yet, so the
        // retained ≤6-plan LRU of design 5 waits for Phase B traffic.
        let plan = harfrust::ShapePlan::new(
            &font,
            harfrust::Direction::LeftToRight,
            None,
            None,
            features,
        );
        let mut shaper = harfrust::ShaperFont::new(&font);
        const UNITS_PER_PX: f32 = 64.0;
        let scale = (px * UNITS_PER_PX).round();
        if scale > i32::MAX as f32 {
            return Err(RenderError::InvalidInput {
                reason: "shape size exceeds the shaper scale bound",
            });
        }
        shaper.set_scale(scale as i32);
        let mut buffer = harfrust::Buffer::new();
        buffer.push_str(text);
        buffer.set_direction(harfrust::Direction::LeftToRight);
        harfrust::shape(
            &shaper,
            &mut buffer,
            harfrust::ShapeOptions::new().plan(Some(&plan)),
        )
        .map_err(RenderError::flatten_rasterizer)?;
        let infos = buffer.glyph_infos();
        let positions = buffer.glyph_positions();
        if infos.len() != positions.len() {
            return Err(RenderError::UpstreamRasterizer(
                "shaper returned ragged infos/positions".to_string(),
            ));
        }
        let mut clusters = Vec::with_capacity(infos.len());
        let swash_font = Self::font_ref(face)?;
        let mut scaler = self
            .scale_ctx
            .builder(swash_font)
            .size(px)
            .hint(true)
            .build();
        for (i, (info, pos)) in infos.iter().zip(positions.iter()).enumerate() {
            let start = info.cluster as usize;
            let end = if i + 1 < infos.len() {
                (infos[i + 1].cluster as usize).max(start)
            } else {
                text.len()
            };
            // Defensive: a cluster index past the text is corrupt output.
            if start > text.len() || end > text.len() {
                return Err(RenderError::UpstreamRasterizer(
                    "shaper returned an out-of-range cluster".to_string(),
                ));
            }
            let slice = text.get(start..end).unwrap_or("");
            let cells = cells_for_range(slice).max(1);
            // `start` is a shaper-provided cluster: a non-boundary
            // index is corrupt output, rejected (never panics).
            let prefix = text.get(..start).ok_or_else(|| {
                RenderError::UpstreamRasterizer(
                    "shaper returned a non-boundary cluster".to_string(),
                )
            })?;
            let col = col_base + cells_for_range(prefix);
            let glyph_id = info.glyph_id;
            // Drawable, not merely mapped: probe the same source order the
            // rasterizer serves (see `probe_source`).
            let covered =
                glyph_id != 0 && Self::probe_source(&mut scaler, glyph_id as u16).is_some();
            clusters.push(ShapedCluster {
                cells: (col, cells),
                glyph_id: glyph_id as u16,
                face: id,
                x_advance_px: pos.x_advance as f32 / UNITS_PER_PX,
                x_offset_px: pos.x_offset as f32 / UNITS_PER_PX,
                uncovered: !covered,
                byte_offset: base_offset + start,
            });
        }
        Ok(clusters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pt_to_px_mirrors_crossfont_scale() {
        // crossfont Size::as_px = pt * 96/72; the CTX-0157 probe (12pt ->
        // 16px) and the default 10x22 cell rest on it.
        assert!((SwashSingle::px(12.0) - 16.0).abs() < f32::EPSILON);
        assert!((SwashSingle::px(6.0) - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn shape_plan_key_is_deterministic_and_sensitive() {
        use bitty_config::types::OpenTypeFeature;
        let face = FontId::next(&mut 0);
        let a = ShapePlanKey::new(face, &[]);
        let b = ShapePlanKey::new(face, &[]);
        assert_eq!(a, b);
        let feats = vec![OpenTypeFeature::parse("calt=0").unwrap()];
        let c = ShapePlanKey::new(face, &feats);
        assert_ne!(a, c);
        assert_eq!(c, ShapePlanKey::new(face, &feats));
        // Feature order matters (application order is significant).
        let reordered = vec![
            OpenTypeFeature::parse("liga=0").unwrap(),
            OpenTypeFeature::parse("calt=0").unwrap(),
        ];
        let ordered = vec![
            OpenTypeFeature::parse("calt=0").unwrap(),
            OpenTypeFeature::parse("liga=0").unwrap(),
        ];
        assert_ne!(
            ShapePlanKey::new(face, &reordered),
            ShapePlanKey::new(face, &ordered)
        );
        // Different faces never share a key.
        let other = FontId::next(&mut 7);
        assert_ne!(a, ShapePlanKey::new(other, &[]));
    }

    #[test]
    fn harfrust_feature_conversion_is_global() {
        use bitty_config::types::OpenTypeFeature;
        let feats = vec![
            OpenTypeFeature::parse("calt=0").unwrap(),
            OpenTypeFeature::parse("ss01=2").unwrap(),
        ];
        let converted = harfrust_features(&feats);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].tag, harfrust::Tag::new(b"calt"));
        assert_eq!(converted[0].value, 0);
        assert_eq!(converted[1].value, 2);
        // Global range: start 0, end MAX.
        assert_eq!(converted[0].start, harfrust::Feature::GLOBAL_START);
        assert_eq!(converted[0].end, harfrust::Feature::GLOBAL_END);
    }

    #[test]
    fn cells_for_range_follows_grid_truth() {
        assert_eq!(cells_for_range("->"), 2);
        assert_eq!(cells_for_range("漢"), 2);
        // Combining mark attaches without advancing.
        assert_eq!(cells_for_range("é"), 1);
        assert_eq!(cells_for_range(""), 0);
    }

    #[test]
    fn face_bounds_are_named_and_sane() {
        assert_eq!(MAX_FACE_BYTES, 64 * 1024 * 1024);
        const {
            assert!(MAX_LOADED_FACES >= 12, "must hold any legal chain");
        }
        assert_eq!(MAX_SHAPE_PLANS_PER_FACE, 6);
    }
}
