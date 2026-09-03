//! Bounded memoization in front of any [`GlyphRasterizer`].
//!
//! The cache turns the rasterizer contract into an efficient hot path:
//! repeated keys (the common case — a terminal redraws the same glyphs every
//! frame) never re-enter the upstream engine, and blank results
//! (`Ok(None)`) are cached negatively so whitespace costs nothing.
//!
//! # Bounding policy
//!
//! Memory is bounded by construction: at most [`GlyphCache::capacity`]
//! entries are retained. When the cache is full and a *new* key arrives, the
//! whole map is cleared before inserting (wholesale eviction). This is
//! deliberately simple and deterministic; per-entry LRU would add bookkeeping
//! for marginal gain at terminal working-set sizes. Errors are **not**
//! cached: a transient upstream failure must not poison a key forever.
//!
//! The default capacity of 2048 covers several full screens of distinct
//! glyphs across styles; worst case memory is bounded by capacity times the
//! largest accepted bitmap.
//!
//! # Counters
//!
//! [`GlyphCache`] tracks cumulative hit/miss lookups ([`GlyphCache::hits`],
//! [`GlyphCache::misses`]) so callers can report rasterization cost against
//! the performance-budget-rfc counters without their own instrumentation.

use std::collections::HashMap;

use crate::error::RenderError;
use crate::glyph::{FontId, FontQuery, GlyphBitmap, GlyphRasterizer, RasterKey};

/// Default entry cap used by [`GlyphCache::new`] callers that do not need a
/// custom bound.
pub const DEFAULT_GLYPH_CACHE_CAPACITY: usize = 2048;

/// A cached glyph lookup result: either a bitmap reference or a cached blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedGlyph<'a> {
    /// A rasterized bitmap owned by the cache; valid until the cache is
    /// mutated.
    Bitmap(&'a GlyphBitmap),
    /// The key has no drawable representation (cached negative).
    Blank,
}

/// Memoizing decorator around a [`GlyphRasterizer`].
#[derive(Debug)]
pub struct GlyphCache<R: GlyphRasterizer> {
    rasterizer: R,
    entries: HashMap<RasterKey, Option<GlyphBitmap>>,
    capacity: usize,
    lookups_hit: u64,
    lookups_missed: u64,
}

impl<R: GlyphRasterizer> GlyphCache<R> {
    /// Wraps `rasterizer` with a cache bounded to `capacity` entries.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when `capacity` is zero (an unbounded or
    /// useless cache is never constructed silently).
    pub fn new(rasterizer: R, capacity: usize) -> Result<Self, RenderError> {
        if capacity == 0 {
            return Err(RenderError::InvalidInput {
                reason: "glyph cache capacity must be non-zero",
            });
        }
        Ok(Self {
            rasterizer,
            entries: HashMap::new(),
            capacity,
            lookups_hit: 0,
            lookups_missed: 0,
        })
    }

    /// Configured maximum number of entries.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of currently cached keys (bitmaps plus blanks).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Cumulative lookups served from the cache without touching the
    /// upstream rasterizer. Saturates at `u64::MAX`; never resets.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.lookups_hit
    }

    /// Cumulative lookups that had to rasterize (including cached-negative
    /// blanks). Saturates at `u64::MAX`; never resets. Errors are counted as
    /// misses but never cached.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.lookups_missed
    }

    /// Loads a font through to the wrapped rasterizer. New faces get fresh
    /// handles, so no cached entry can go stale after this call.
    ///
    /// # Errors
    ///
    /// Whatever the wrapped rasterizer reports.
    pub fn load_font(&mut self, query: &FontQuery) -> Result<FontId, RenderError> {
        self.rasterizer.load_font(query)
    }

    /// Drops all cached entries (bitmaps and blanks) without resetting the
    /// cumulative hit/miss counters.
    ///
    /// Call after the rasterized size changes (for example a DPI rescale):
    /// keys embed the point size, so stale entries would otherwise pin
    /// previous-scale bitmaps while new-scale keys rasterize alongside them.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns the glyph for `key`, rasterizing on first use.
    ///
    /// # Errors
    ///
    /// Propagates rasterizer failures without caching them (see module docs).
    pub fn glyph(&mut self, key: RasterKey) -> Result<CachedGlyph<'_>, RenderError> {
        if !self.entries.contains_key(&key) {
            self.lookups_missed = self.lookups_missed.saturating_add(1);
            if self.entries.len() >= self.capacity {
                // Wholesale eviction keeps the bound obvious and the policy
                // deterministic; see module docs.
                self.entries.clear();
            }
            let entry = self.rasterizer.rasterize(key)?;
            self.entries.insert(key, entry);
        } else {
            self.lookups_hit = self.lookups_hit.saturating_add(1);
        }
        Ok(match &self.entries[&key] {
            Some(bitmap) => CachedGlyph::Bitmap(bitmap),
            None => CachedGlyph::Blank,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{GlyphMetrics, GlyphRasterizer};
    use std::cell::Cell;

    /// Deterministic fake implementing the full trait contract; counts
    /// upstream calls and can be told to fail or report blanks.
    #[derive(Debug, Default)]
    struct FakeRasterizer {
        next_id: u64,
        rasterize_calls: Cell<u32>,
        fail_next: Cell<bool>,
        blank_keys: Vec<char>,
    }

    impl FakeRasterizer {
        fn bitmap_for(key: RasterKey) -> GlyphBitmap {
            let side = usize::from(u8::try_from(key.character as u32 % 16).unwrap_or(1)) + 1;
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 8,
                    width: i32::try_from(side).unwrap(),
                    height: i32::try_from(side).unwrap(),
                    advance: [i32::try_from(side).unwrap(), 0],
                },
                crate::glyph::BitmapFormat::Rgb,
                vec![0xAA; side * side * 3],
            )
            .unwrap()
        }
    }

    impl GlyphRasterizer for FakeRasterizer {
        fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, RenderError> {
            Ok(FontId::next(&mut self.next_id))
        }

        fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
            self.rasterize_calls.set(self.rasterize_calls.get() + 1);
            if self.fail_next.replace(false) {
                return Err(RenderError::UpstreamRasterizer("synthetic".into()));
            }
            if self.blank_keys.contains(&key.character) {
                return Ok(None);
            }
            Ok(Some(Self::bitmap_for(key)))
        }
    }

    fn key(cache: &mut GlyphCache<FakeRasterizer>, ch: char) -> RasterKey {
        let font = cache.load_font(&font_query()).unwrap();
        RasterKey::new(ch, font, 12.0).unwrap()
    }

    fn font_query() -> FontQuery {
        FontQuery {
            family: "Fake Mono".into(),
            style: crate::glyph::FontStyle::Normal,
            point_size: 12.0,
        }
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            GlyphCache::new(FakeRasterizer::default(), 0),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn repeated_keys_hit_upstream_once() {
        let mut cache =
            GlyphCache::new(FakeRasterizer::default(), DEFAULT_GLYPH_CACHE_CAPACITY).unwrap();
        let k = key(&mut cache, 'A');

        for _ in 0..5 {
            let CachedGlyph::Bitmap(bm) = cache.glyph(k).unwrap() else {
                panic!("expected bitmap");
            };
            let side = i32::try_from(u32::from('A') % 16 + 1).unwrap();
            assert_eq!(bm.metrics.width + bm.metrics.height, 2 * side);
        }
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
        assert_eq!(cache.hits(), 4);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn counters_cover_blanks_and_errors() {
        let mut fake = FakeRasterizer::default();
        fake.blank_keys.push(' ');
        let mut cache = GlyphCache::new(fake, DEFAULT_GLYPH_CACHE_CAPACITY).unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        let space = RasterKey::new(' ', font, 12.0).unwrap();

        assert!(matches!(cache.glyph(space), Ok(CachedGlyph::Blank)));
        assert!(matches!(cache.glyph(space), Ok(CachedGlyph::Blank)));
        // A cached blank costs one rasterization (a miss); the second
        // lookup is a hit served from the cache.
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn different_sizes_are_different_entries() {
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 16).unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        let small = RasterKey::new('x', font, 10.0).unwrap();
        let large = RasterKey::new('x', font, 20.0).unwrap();
        let _ = cache.glyph(small).unwrap();
        let _ = cache.glyph(large).unwrap();
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 2);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn blanks_are_cached_negatively() {
        let mut fake = FakeRasterizer::default();
        fake.blank_keys.push(' ');
        let mut cache = GlyphCache::new(fake, 16).unwrap();
        let k = key(&mut cache, ' ');

        assert_eq!(cache.glyph(k).unwrap(), CachedGlyph::Blank);
        assert_eq!(cache.glyph(k).unwrap(), CachedGlyph::Blank);
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn errors_propagate_without_caching() {
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 16).unwrap();
        cache.rasterizer.fail_next.set(true);
        let k = key(&mut cache, 'E');
        // First call fails (after load_font), nothing cached...
        assert!(matches!(
            cache.glyph(k),
            Err(RenderError::UpstreamRasterizer(_))
        ));
        assert_eq!(cache.len(), 0);
        // ...and retry succeeds then caches.
        assert!(matches!(cache.glyph(k).unwrap(), CachedGlyph::Bitmap(_)));
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 2);
    }

    #[test]
    fn wholesale_eviction_keeps_the_bound() {
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 4).unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        for ch in 'a'..'e' {
            let _ = cache.glyph(RasterKey::new(ch, font, 12.0).unwrap());
        }
        assert_eq!(cache.len(), 4);

        // Overflowing insert clears, then inserts exactly one entry.
        let _ = cache.glyph(RasterKey::new('z', font, 12.0).unwrap());
        assert_eq!(cache.len(), 1);
        // The evicted entries rasterize again on demand.
        let _ = cache.glyph(RasterKey::new('a', font, 12.0).unwrap());
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 6);
    }

    #[test]
    fn load_font_issues_fresh_handles_per_call() {
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 16).unwrap();
        let a = cache.load_font(&font_query()).unwrap();
        let b = cache.load_font(&font_query()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn works_as_dyn_trait_object_target() {
        // Renderer backends will hold Box<dyn GlyphRasterizer>; make sure the
        // trait stays object-safe by exercising it through a dyn pointer.
        let fake = FakeRasterizer::default();
        let mut cache = GlyphCache::new(fake, 16).unwrap();
        let dyn_ref: &mut dyn GlyphRasterizer = &mut cache.rasterizer;
        let id = dyn_ref.load_font(&font_query()).unwrap();
        let k = RasterKey::new('q', id, 9.0).unwrap();
        assert!(dyn_ref.rasterize(k).unwrap().is_some());
    }
}
