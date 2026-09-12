//! Per-glyph font fallback for TUI graphs (CTX-0163, issue #263).
//!
//! The family-level chain in `bitty-config` ([`FONT_FALLBACK_CHAIN`]) only
//! helped when the *primary* face failed to load: the crossfont backend maps
//! a missing glyph to `Ok(None)` (blank cell, not a failure), so btop CPU
//! graphs drawn with braille patterns (`U+2800-U+28FF`) vanished while text
//! survived. [`FallbackRasterizer`] closes that gap by walking the chain on
//! a missing glyph — the same per-codepoint strategy as ghostty's
//! `src/font/CodepointResolver.zig` (fallback via discovery), reduced here
//! to a bounded static chain with no new dependency.
//!
//! [`FONT_FALLBACK_CHAIN`]: bitty_config::types::FONT_FALLBACK_CHAIN

#[cfg(test)]
use bitty_config::types::EMOJI_FALLBACK_FAMILY;
#[cfg(all(test, target_os = "linux"))]
use bitty_config::types::SYMBOLS_FALLBACK_FAMILY;
use bitty_config::types::{FONT_FALLBACK_CHAIN, FontConfig};

use crate::error::RenderError;
use crate::glyph::{FontId, FontQuery, GlyphBitmap, GlyphRasterizer, RasterKey};

/// First braille-pattern scalar (`BRAILLE PATTERN BLANK`, used by btop for
/// the "off" dots of its graphs — blank-looking but must still resolve to a
/// real glyph so the graph lattice stays aligned).
pub const BRAILLE_FIRST: char = '\u{2800}';
/// Last braille-pattern scalar (`BRAILLE PATTERN DOTS-12345678`).
pub const BRAILLE_LAST: char = '\u{28FF}';
/// First block-element scalar (`UPPER HALF BLOCK`).
pub const BLOCK_FIRST: char = '\u{2580}';
/// Last block-element scalar (`FULL BLOCK` and friends).
pub const BLOCK_LAST: char = '\u{259F}';

/// True for braille patterns `U+2800-U+28FF` (btop CPU/graph rendering).
#[must_use]
pub const fn is_braille_pattern(c: char) -> bool {
    (c as u32) >= (BRAILLE_FIRST as u32) && (c as u32) <= (BRAILLE_LAST as u32)
}

/// True for block elements `U+2580-U+259F` (block graphs, sparklines).
#[must_use]
pub const fn is_block_element(c: char) -> bool {
    (c as u32) >= (BLOCK_FIRST as u32) && (c as u32) <= (BLOCK_LAST as u32)
}

/// True for either TUI-graph slice covered by this task.
#[must_use]
pub const fn is_tui_graph_scalar(c: char) -> bool {
    is_braille_pattern(c) || is_block_element(c)
}

/// Per-glyph fallback decorator over any [`GlyphRasterizer`].
///
/// On [`load_font`](GlyphRasterizer::load_font) the primary face loads (its
/// error propagates, preserving today's startup contract) plus every chain
/// tail at the same style/size; tails that fail with
/// [`RenderError::FontNotFound`] are skipped best-effort so bare installs
/// without the symbols face still start. On
/// [`rasterize`](GlyphRasterizer::rasterize) each loaded face is tried in
/// order until one yields a bitmap: the first `Some` wins, an all-blank walk
/// returns `Ok(None)` (cacheable negative, as before), and a walk that
/// produced only errors returns the last error so engine failures stay
/// observable instead of silently blanking.
///
/// [`resolve`](FallbackRasterizer::resolve) exposes the same walk with
/// **coverage reporting** (CTX-0368, text-rendering RFC "Fallback chain
/// construction"): `covered = true` means some loaded face produced a
/// drawable bitmap for the scalar; `covered = false` means every face
/// reported a missing glyph, which is the caller's cue to paint the RFC
/// tofu box. `Ok(None)` from `rasterize` is exactly `covered = false`.
///
/// Bounded: at most `1 + chain.len()` faces; each rasterization performs at
/// most that many upstream calls, and the [`GlyphCache`](crate::cache::GlyphCache)
/// in front memoizes the outcome per key, so the walk runs once per distinct
/// glyph — never per frame.
#[derive(Debug)]
pub struct FallbackRasterizer<R: GlyphRasterizer> {
    inner: R,
    fallback_families: Vec<String>,
    fonts: Vec<FontId>,
    point_size: f32,
}

/// Outcome of one coverage-driven fallback resolution.
///
/// `covered == true` carries the winning face and its bitmap; `covered ==
/// false` means no loaded face had a drawable glyph for the scalar (the
/// caller paints tofu and counts it, per the text-rendering RFC
/// "Missing-glyph behavior").
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedGlyph {
    /// The face that produced `bitmap`, or the face the walk started from
    /// when nothing covered.
    pub font: FontId,
    /// True when a drawable bitmap was produced.
    pub covered: bool,
    /// The drawable bitmap; `None` exactly when `covered == false`.
    pub bitmap: Option<GlyphBitmap>,
}

impl<R: GlyphRasterizer> FallbackRasterizer<R> {
    /// Wraps `inner` with an explicit fallback family list (tails only; the
    /// primary comes from each [`load_font`](GlyphRasterizer::load_font)
    /// query and is deduplicated case-insensitively).
    pub fn new(inner: R, fallback_families: Vec<String>) -> Self {
        Self {
            inner,
            fallback_families,
            fonts: Vec::new(),
            point_size: 0.0,
        }
    }

    /// Wraps `inner` with the documented [`FONT_FALLBACK_CHAIN`] tails, so
    /// production wiring needs no list of its own (and gains the braille
    /// symbols tail automatically when the config chain grows).
    pub fn with_default_chain(inner: R) -> Self {
        let tails = FONT_FALLBACK_CHAIN
            .iter()
            .skip(1)
            .map(|s| (*s).to_string())
            .collect();
        Self::new(inner, tails)
    }

    /// Builds the configured-first chain for `family` (primary first, then
    /// the documented tails deduplicated) — the list form of
    /// [`FontConfig::fallback_chain`] for callers that need names, not faces.
    #[must_use]
    pub fn chain_for(family: &str) -> Vec<String> {
        FontConfig {
            family: family.to_string(),
            ..Default::default()
        }
        .fallback_chain()
    }

    /// Borrowed access to the wrapped rasterizer (production uses this to
    /// observe backend kind without breaking the wrapper boundary).
    #[must_use]
    pub const fn inner(&self) -> &R {
        &self.inner
    }

    /// Mutable access to the wrapped rasterizer.
    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Faces loaded by the last [`load_font`](GlyphRasterizer::load_font)
    /// call, in attempt order (primary first). Empty before the first load.
    #[must_use]
    pub fn fonts(&self) -> &[FontId] {
        &self.fonts
    }

    /// Configured tail families (primary excluded).
    #[must_use]
    pub fn fallback_families(&self) -> &[String] {
        &self.fallback_families
    }

    /// Point size of the last loaded chain (0.0 before the first load).
    #[must_use]
    pub const fn point_size(&self) -> f32 {
        self.point_size
    }

    /// Resolves `key` with coverage reporting (CTX-0368).
    ///
    /// Walks the requested face first (normally the primary) and then every
    /// loaded chain face in load order, deduplicated. The first face that
    /// yields a bitmap wins and is reported with `covered = true`. When every
    /// face reports a missing glyph (`Ok(None)`), the result is
    /// `covered = false` with no bitmap; the caller paints tofu. Engine
    /// errors are skipped during the walk; a walk that produced only errors
    /// returns the last one so failures stay observable.
    ///
    /// Deterministic: the order is fixed by `load_font` and the result is a
    /// pure function of face coverage. Bounded: at most `1 + fonts().len()`
    /// upstream calls per resolution.
    ///
    /// # Errors
    ///
    /// [`RenderError::UnknownFontHandle`] before any face was loaded, and the
    /// last upstream error when no face produced coverage and at least one
    /// errored.
    pub fn resolve(&mut self, key: RasterKey) -> Result<ResolvedGlyph, RenderError> {
        if self.fonts.is_empty() {
            return Err(RenderError::UnknownFontHandle);
        }
        // Attempt order: the requested face first (normally the primary the
        // grid pipeline cached under), then the stored chain deduplicated.
        let mut order: Vec<FontId> = Vec::with_capacity(self.fonts.len() + 1);
        order.push(key.font);
        for font in &self.fonts {
            if !order.contains(font) {
                order.push(*font);
            }
        }
        let mut last_err: Option<RenderError> = None;
        for font in order {
            let attempt = RasterKey::new(key.character, font, key.point_size)
                .map_err(|_| RenderError::UnknownFontHandle)?;
            match self.inner.rasterize(attempt) {
                Ok(Some(bitmap)) => {
                    return Ok(ResolvedGlyph {
                        font,
                        covered: true,
                        bitmap: Some(bitmap),
                    });
                }
                Ok(None) => continue,
                Err(err) => {
                    last_err = Some(err);
                    continue;
                }
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(ResolvedGlyph {
            font: key.font,
            covered: false,
            bitmap: None,
        })
    }
}

impl<R: GlyphRasterizer> GlyphRasterizer for FallbackRasterizer<R> {
    fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        query.validate()?;
        self.fonts.clear();
        let primary = self.inner.load_font(query)?;
        self.point_size = query.point_size;
        self.fonts.push(primary);
        let lower_primary = query.family.trim().to_lowercase();
        for family in &self.fallback_families {
            if family.trim().is_empty() || family.trim().to_lowercase() == lower_primary {
                continue;
            }
            let tail = FontQuery {
                family: family.clone(),
                style: query.style.clone(),
                point_size: query.point_size,
            };
            match self.inner.load_font(&tail) {
                Ok(id) => {
                    if !self.fonts.contains(&id) {
                        self.fonts.push(id);
                    }
                }
                // Best-effort tails: a missing symbols face must not strand
                // the primary on bare installs. Any other engine failure is
                // skipped the same way — the primary still renders, and the
                // miss stays observable through cache-miss counters.
                Err(_) => continue,
            }
        }
        Ok(primary)
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        self.resolve(key).map(|resolved| resolved.bitmap)
    }

    fn font_metrics(
        &self,
        font: FontId,
        point_size: f32,
    ) -> Result<Option<crate::glyph::FontMetrics>, RenderError> {
        // Outer handles are inner handles (see `load_font`), so the primary
        // face measurement forwards directly; the chain tails share the
        // primary's line box by terminal-monospace construction.
        self.inner.font_metrics(font, point_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{BitmapFormat, FontStyle, GlyphMetrics};
    use std::cell::Cell;
    use std::collections::HashMap;

    /// Fake with per-family coverage: each loaded family gets a handle, and
    /// `blank` lists `(family, char)` pairs that rasterize to `None`
    /// (upstream `MissingGlyph`). Families in `fail_load` fail with
    /// `FontNotFound`; families in `fail_raster` fail with an upstream
    /// error on every rasterize.
    #[derive(Debug, Default)]
    struct Fake {
        next_id: u64,
        families: HashMap<FontId, String>,
        blank: Vec<(String, char)>,
        fail_load: Vec<String>,
        fail_raster: Vec<String>,
        rasterize_calls: Cell<u32>,
    }

    impl Fake {
        fn bitmap_for(character: char) -> GlyphBitmap {
            let side = i32::try_from(u32::from(character) % 3 + 6).unwrap();
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side,
                    height: side,
                    advance: [side, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap()
        }

        fn family_of(&self, id: FontId) -> String {
            self.families.get(&id).cloned().unwrap_or_default()
        }
    }

    impl GlyphRasterizer for Fake {
        fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
            if self.fail_load.contains(&query.family) {
                return Err(RenderError::FontNotFound(query.family.clone()));
            }
            let id = FontId::next(&mut self.next_id);
            self.families.insert(id, query.family.clone());
            Ok(id)
        }

        fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
            self.rasterize_calls.set(self.rasterize_calls.get() + 1);
            let family = self.family_of(key.font);
            if self.fail_raster.contains(&family) {
                return Err(RenderError::UpstreamRasterizer("synthetic".into()));
            }
            if self.blank.contains(&(family, key.character)) {
                return Ok(None);
            }
            Ok(Some(Self::bitmap_for(key.character)))
        }
    }

    fn query(family: &str, size: f32) -> FontQuery {
        FontQuery {
            family: family.into(),
            style: FontStyle::Normal,
            point_size: size,
        }
    }

    fn tails() -> Vec<String> {
        vec![
            "JetBrains Mono".to_string(),
            "monospace".to_string(),
            "DejaVu Sans Mono".to_string(),
            "Noto Sans Symbols 2".to_string(),
        ]
    }

    #[test]
    fn tui_graph_scalar_classification() {
        assert!(is_braille_pattern('\u{2800}'));
        assert!(is_braille_pattern('\u{28FF}'));
        assert!(is_braille_pattern('\u{283F}'));
        assert!(!is_braille_pattern('\u{27FF}'));
        assert!(!is_braille_pattern('\u{2900}'));
        assert!(!is_braille_pattern('A'));
        assert!(is_block_element('\u{2580}'));
        assert!(is_block_element('\u{259F}'));
        assert!(is_block_element('\u{2588}'));
        assert!(!is_block_element('\u{257F}'));
        assert!(!is_block_element('\u{25A0}'));
        assert!(is_tui_graph_scalar('\u{2800}'));
        assert!(is_tui_graph_scalar('\u{259F}'));
        // Box drawing stays out of scope: DejaVu already covers it, so the
        // symbols tail exists for braille/blocks, not for boxes.
        assert!(!is_tui_graph_scalar('\u{2500}'));
        assert!(!is_tui_graph_scalar('x'));
    }

    #[test]
    fn default_chain_matches_config_tails() {
        let wrapped = FallbackRasterizer::with_default_chain(Fake::default());
        let expected: Vec<String> = FONT_FALLBACK_CHAIN
            .iter()
            .skip(1)
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(wrapped.fallback_families(), expected.as_slice());
        assert!(
            wrapped
                .fallback_families()
                .iter()
                .any(|f| f == EMOJI_FALLBACK_FAMILY)
        );
        // The braille/symbols face is platform-pinned: Linux resolves it to
        // Noto Sans Symbols 2, macOS to Apple Braille, Windows to Segoe UI
        // Symbol (asserted together in `bitty-config`).
        #[cfg(target_os = "linux")]
        assert!(
            wrapped
                .fallback_families()
                .iter()
                .any(|f| f == SYMBOLS_FALLBACK_FAMILY)
        );
        #[cfg(target_os = "macos")]
        assert!(
            wrapped
                .fallback_families()
                .iter()
                .any(|f| f == "Apple Braille")
        );
        #[cfg(windows)]
        assert!(
            wrapped
                .fallback_families()
                .iter()
                .any(|f| f == "Segoe UI Symbol")
        );
    }

    #[test]
    fn chain_for_puts_primary_first_without_dupes() {
        let chain = FallbackRasterizer::<Fake>::chain_for("My Mono");
        assert_eq!(chain[0], "My Mono");
        assert!(chain.iter().any(|f| f == EMOJI_FALLBACK_FAMILY));
        #[cfg(target_os = "linux")]
        assert!(chain.iter().any(|f| f == SYMBOLS_FALLBACK_FAMILY));
        let chain = FallbackRasterizer::<Fake>::chain_for("monospace");
        assert_eq!(chain.iter().filter(|f| *f == "monospace").count(), 1);
    }

    #[test]
    fn primary_hit_never_walks_fallbacks() {
        let mut wrapped = FallbackRasterizer::new(Fake::default(), tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        assert_eq!(wrapped.fonts().len(), 5);
        let key = RasterKey::new('A', primary, 12.0).unwrap();
        assert!(wrapped.rasterize(key).unwrap().is_some());
        assert_eq!(wrapped.inner.rasterize_calls.get(), 1);
    }

    #[test]
    fn braille_falls_through_to_symbols_tail() {
        // Every face except the symbols tail reports braille as missing —
        // the btop-graph shape on a bare install without the Nerd font.
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
        ] {
            inner.blank.push((family.to_string(), '\u{28FF}'));
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('\u{28FF}', primary, 12.0).unwrap();
        let bitmap = wrapped
            .rasterize(key)
            .unwrap()
            .expect("symbols tail covers braille");
        assert!(!bitmap.is_blank());
        assert_eq!(wrapped.inner.rasterize_calls.get(), 5);
    }

    #[test]
    fn block_elements_fall_through_to_dejavu() {
        // Primary + unpatched fallbacks miss blocks; DejaVu covers them.
        let mut inner = Fake::default();
        for family in ["Primary Mono", "JetBrains Mono", "monospace"] {
            inner.blank.push((family.to_string(), '\u{2588}'));
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('\u{2588}', primary, 12.0).unwrap();
        assert!(wrapped.rasterize(key).unwrap().is_some());
        assert_eq!(wrapped.inner.rasterize_calls.get(), 4);
    }

    #[test]
    fn all_faces_missing_returns_blank_not_error() {
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
            "Noto Sans Symbols 2",
        ] {
            inner.blank.push((family.to_string(), '�'));
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('�', primary, 12.0).unwrap();
        assert!(wrapped.rasterize(key).unwrap().is_none());
    }

    #[test]
    fn resolve_reports_fallback_coverage() {
        // CTX-0368: a symbol the primary lacks resolves through the chain and
        // must report `covered = true`, naming the winning face.
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
        ] {
            inner.blank.push((family.to_string(), '✔'));
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let resolved = wrapped
            .resolve(RasterKey::new('✔', primary, 12.0).unwrap())
            .unwrap();
        assert!(resolved.covered, "symbols tail covers U+2714");
        assert_eq!(
            resolved.font,
            wrapped.fonts()[4],
            "the Noto Sans Symbols 2 tail must win"
        );
        assert!(resolved.bitmap.is_some());
        // Walk bounded by the loaded face count (primary + four tails).
        assert_eq!(wrapped.inner.rasterize_calls.get(), 5);
        // The legacy rasterize projection stays Some for the same key and
        // performs exactly one more bounded walk.
        assert!(
            wrapped
                .rasterize(RasterKey::new('✔', primary, 12.0).unwrap())
                .unwrap()
                .is_some()
        );
        assert_eq!(wrapped.inner.rasterize_calls.get(), 10);
    }

    #[test]
    fn resolve_reports_uncovered_scalar_as_not_covered() {
        // CTX-0368: when no available face covers the scalar, resolution must
        // report `covered = false` with no bitmap so the caller paints tofu.
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
            "Noto Sans Symbols 2",
        ] {
            inner.blank.push((family.to_string(), '\u{10FFFF}'));
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('\u{10FFFF}', primary, 12.0).unwrap();
        let resolved = wrapped.resolve(key).unwrap();
        assert!(!resolved.covered);
        assert_eq!(resolved.bitmap, None);
        assert_eq!(resolved.font, primary, "uncovered reports the start face");
        // The legacy contract still yields the cacheable negative.
        assert!(wrapped.rasterize(key).unwrap().is_none());
        // Two resolutions of the same uncovered scalar: 5 faces each.
        assert_eq!(
            wrapped.inner.rasterize_calls.get(),
            u32::try_from(2 * wrapped.fonts().len()).unwrap()
        );
    }

    #[test]
    fn resolve_is_deterministic_across_instances() {
        // CTX-0368: selection is a pure function of coverage and pinned order.
        let build = || {
            let mut inner = Fake::default();
            inner.blank.push(("Primary Mono".to_string(), '✔'));
            inner.blank.push(("JetBrains Mono".to_string(), '☑'));
            inner.blank.push(("monospace".to_string(), '⚙'));
            FallbackRasterizer::new(inner, tails())
        };
        let mut a = build();
        let mut b = build();
        let primary_a = a.load_font(&query("Primary Mono", 12.0)).unwrap();
        let primary_b = b.load_font(&query("Primary Mono", 12.0)).unwrap();
        let scalars = ['✔', '☑', '⚙', '→', '⣿', 'A', '\u{10FFFF}'];
        let mut sequence_a = Vec::new();
        let mut sequence_b = Vec::new();
        for c in scalars {
            let ra = a
                .resolve(RasterKey::new(c, primary_a, 12.0).unwrap())
                .unwrap();
            let rb = b
                .resolve(RasterKey::new(c, primary_b, 12.0).unwrap())
                .unwrap();
            let face_a = a.fonts().iter().position(|f| *f == ra.font);
            let face_b = b.fonts().iter().position(|f| *f == rb.font);
            assert_eq!(ra.covered, rb.covered, "{c:?} coverage differs");
            assert_eq!(ra.bitmap.is_some(), rb.bitmap.is_some());
            sequence_a.push((ra.covered, face_a));
            sequence_b.push((rb.covered, face_b));
        }
        assert_eq!(sequence_a, sequence_b);
        assert!(
            a.inner.rasterize_calls.get()
                <= u32::try_from(scalars.len() * a.fonts().len()).unwrap(),
            "walk stays bounded per resolution"
        );
    }

    #[test]
    fn fallback_walk_and_cache_stay_bounded() {
        // CTX-0368: the fallback decorator sits behind the bounded GlyphCache;
        // a tiny cache must evict wholesale instead of growing, and the
        // per-scalar walk must never exceed the loaded face count.
        use crate::cache::GlyphCache;
        let capacity = 4;
        let mut cache =
            GlyphCache::new(FallbackRasterizer::new(Fake::default(), tails()), capacity).unwrap();
        let font = cache.load_font(&query("Primary Mono", 12.0)).unwrap();
        let scalars = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j'];
        for c in scalars {
            assert!(
                matches!(
                    cache.glyph(RasterKey::new(c, font, 12.0).unwrap()).unwrap(),
                    crate::cache::CachedGlyph::Bitmap(_)
                ),
                "{c:?} must rasterize through the fallback chain"
            );
        }
        assert!(cache.len() <= capacity, "cache bound must hold");
        let faces = cache.rasterizer().fonts().len();
        assert!(
            cache.rasterizer().inner.rasterize_calls.get()
                <= u32::try_from(scalars.len() * faces).unwrap()
        );
    }

    #[test]
    fn missing_tail_faces_are_skipped_on_load() {
        let mut inner = Fake::default();
        inner.fail_load.push("monospace".to_string());
        inner.fail_load.push("Noto Sans Symbols 2".to_string());
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        assert_eq!(wrapped.fonts()[0], primary);
        // Two tails missing: primary + 2 surviving tails.
        assert_eq!(wrapped.fonts().len(), 3);
        let key = RasterKey::new('z', primary, 12.0).unwrap();
        assert!(wrapped.rasterize(key).unwrap().is_some());
    }

    #[test]
    fn missing_primary_still_fails_load() {
        let mut inner = Fake::default();
        inner.fail_load.push("Nope Mono".to_string());
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        assert!(matches!(
            wrapped.load_font(&query("Nope Mono", 12.0)),
            Err(RenderError::FontNotFound(_))
        ));
        assert!(wrapped.fonts().is_empty());
    }

    #[test]
    fn rasterize_before_load_is_unknown_handle() {
        let mut wrapped = FallbackRasterizer::new(Fake::default(), tails());
        let key = RasterKey::new('a', FontId::next(&mut 99), 12.0).unwrap();
        assert!(matches!(
            wrapped.rasterize(key),
            Err(RenderError::UnknownFontHandle)
        ));
    }

    #[test]
    fn engine_error_falls_through_then_surfaces() {
        // Broken primary engine still yields the fallback glyph …
        let mut inner = Fake::default();
        inner.fail_raster.push("Primary Mono".to_string());
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('q', primary, 12.0).unwrap();
        assert!(wrapped.rasterize(key).unwrap().is_some());
        // … while a totally broken engine stays an observable error.
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
            "Noto Sans Symbols 2",
        ] {
            inner.fail_raster.push(family.to_string());
        }
        let mut wrapped = FallbackRasterizer::new(inner, tails());
        let primary = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        let key = RasterKey::new('q', primary, 12.0).unwrap();
        assert!(matches!(
            wrapped.rasterize(key),
            Err(RenderError::UpstreamRasterizer(_))
        ));
    }

    #[test]
    fn reload_refreshes_chain_at_new_size() {
        let mut wrapped = FallbackRasterizer::new(Fake::default(), tails());
        let first = wrapped.load_font(&query("Primary Mono", 12.0)).unwrap();
        assert!((wrapped.point_size() - 12.0).abs() < f32::EPSILON);
        let second = wrapped.load_font(&query("Primary Mono", 24.0)).unwrap();
        assert_ne!(first, second);
        assert!((wrapped.point_size() - 24.0).abs() < f32::EPSILON);
        assert_eq!(wrapped.fonts().len(), 5);
        assert_eq!(wrapped.fonts()[0], second);
    }

    #[test]
    fn invalid_query_fails_before_touching_upstream() {
        let mut wrapped = FallbackRasterizer::new(Fake::default(), tails());
        let bad = FontQuery {
            family: "   ".into(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        assert!(matches!(
            wrapped.load_font(&bad),
            Err(RenderError::InvalidInput { .. })
        ));
        assert!(wrapped.fonts().is_empty());
    }

    #[test]
    fn braille_line_renders_glyphs_through_grid_pipeline() {
        use crate::grid::{CellMetrics, GridRenderer};
        use bitty_term_state::{Damage, DamageRect, DamagedRegion, State, TerminalAction};
        use bitty_vt::GraphemeCell;

        // btop-graph shape: a row of braille cells the primary face lacks.
        let mut inner = Fake::default();
        for family in [
            "Primary Mono",
            "JetBrains Mono",
            "monospace",
            "DejaVu Sans Mono",
        ] {
            for c in ['\u{2800}', '\u{283F}', '\u{28FF}', '\u{2588}'] {
                inner.blank.push((family.to_string(), c));
            }
        }
        let wrapped = FallbackRasterizer::new(inner, tails());
        let cell = CellMetrics::new(8, 16).unwrap();
        let mut renderer = GridRenderer::new(wrapped, &query("Primary Mono", 12.0), cell).unwrap();
        let mut state = State::new();
        for c in ['\u{2800}', '\u{283F}', '\u{28FF}', '\u{2588}'] {
            state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
        }
        let damage = Damage {
            generation: state.generation(),
            regions: Box::new([DamagedRegion::Grid(DamageRect::full(
                u16::try_from(state.height()).unwrap(),
                u16::try_from(state.width()).unwrap(),
            ))]),
        };
        let snapshot = state.snapshot();
        let list = renderer.render(&snapshot, &damage).unwrap();
        // Four graph cells must emit four glyph instances (never blanks);
        // the untouched grid cells stay blank (whitespace negatives).
        assert_eq!(list.glyphs.len(), 4);
        assert_eq!(renderer.counters().glyphs_emitted, 4);
        assert_eq!(
            renderer.counters().blank_cells_skipped,
            (state.width() * state.height() - 4) as u64
        );
    }
}
