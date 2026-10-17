//! Bounded memoization in front of any [`GlyphRasterizer`].
//!
//! The cache turns the rasterizer contract into an efficient hot path:
//! repeated keys (the common case — a terminal redraws the same glyphs every
//! frame) never re-enter the upstream engine, and blank results
//! (`Ok(None)`) are cached negatively so whitespace costs nothing.
//!
//! # Bounding policy
//!
//! The retained entry count is bounded by construction: at most
//! [`GlyphCache::capacity`] entries are retained. When the cache is full and a
//! *new* key arrives,
//! exactly the least recently used entry is evicted (CTX-0471). Recency is
//! a monotone lookup counter stamped on every hit and insert; eviction
//! takes the smallest stamp, so a workload with more distinct glyphs than
//! the capacity (for example a CJK screen) keeps the hot working set
//! instead of dropping every cached glyph each time the table fills.
//! Errors are **not** cached: a transient upstream failure must not poison
//! a key forever, and a failed rasterization never evicts a live entry.
//!
//! The default capacity of 2048 covers several full screens of distinct
//! glyphs across styles. Retained pixel bytes are also capped by
//! [`DEFAULT_GLYPH_CACHE_BYTE_CAPACITY`]. Malformed bitmaps are rejected
//! before they can evict or enter the cache; an accepted bitmap larger than
//! the retained quota uses one bounded transient entry instead, preserving
//! rendering without growing the retained cache.
//!
//! # Counters
//!
//! [`GlyphCache`] tracks cumulative hit/miss lookups ([`GlyphCache::hits`],
//! [`GlyphCache::misses`]) so callers can report rasterization cost against
//! the performance-budget-rfc counters without their own instrumentation.

use std::collections::HashMap;

use crate::atlas::CoveragePolicy;
use crate::error::RenderError;
use crate::glyph::{CachedBitmap, FontId, FontQuery, GlyphBitmap, GlyphRasterizer, RasterKey};

/// Default entry cap used by [`GlyphCache::new`] callers that do not need a
/// custom bound.
///
/// CTX-1206 cap review (issue #2076): retained at 2048. One full screen of
/// all-distinct glyphs (80x24 = 1920 cells) plus margin fits without churn;
/// the renderer-managed grayscale cache's 8 MiB retained-pixel quota is
/// sufficient for 2048 representative 64x64 strikes. Steady-state pressure
/// is relieved by deterministic LRU eviction rather than accumulation.
pub const DEFAULT_GLYPH_CACHE_CAPACITY: usize = 2048;

/// Maximum pixel bytes retained by one character-keyed glyph cache.
pub const DEFAULT_GLYPH_CACHE_BYTE_CAPACITY: usize = 8 * 1024 * 1024;

/// A cached glyph lookup result: either a bitmap reference or a cached blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedGlyph<'a> {
    /// A rasterized bitmap owned by the cache; valid until the cache is
    /// mutated.
    Bitmap(&'a GlyphBitmap),
    /// The key has no drawable representation (cached negative).
    Blank,
}

/// One retained lookup: the bitmap (or cached blank) plus its recency
/// stamp for LRU eviction.
#[derive(Debug)]
struct CacheEntry {
    bitmap: Option<CachedBitmap>,
    last_used: u64,
}

/// Memoizing decorator around a [`GlyphRasterizer`].
#[derive(Debug)]
pub struct GlyphCache<R: GlyphRasterizer> {
    rasterizer: R,
    entries: HashMap<RasterKey, CacheEntry>,
    capacity: usize,
    byte_capacity: usize,
    retained_bytes: usize,
    transient_key: Option<RasterKey>,
    transient_bitmap: Option<CachedBitmap>,
    coverage_policy: CoveragePolicy,
    lookups_hit: u64,
    lookups_missed: u64,
    clock: u64,
}

impl<R: GlyphRasterizer> GlyphCache<R> {
    /// Wraps `rasterizer` with a grayscale cache bounded to `capacity` entries
    /// and [`DEFAULT_GLYPH_CACHE_BYTE_CAPACITY`] retained pixel bytes.
    ///
    /// # Errors
    ///
    /// [`RenderError::InvalidInput`] when `capacity` is zero (an unbounded or
    /// useless cache is never constructed silently).
    pub fn new(rasterizer: R, capacity: usize) -> Result<Self, RenderError> {
        Self::with_coverage_policy(rasterizer, capacity, CoveragePolicy::Grayscale)
    }

    /// Wraps `rasterizer` with an explicit coverage policy.
    ///
    /// Grayscale caches normalize retained bitmaps in place to public RGB
    /// coverage, avoiding a second allocation while preserving the public
    /// bitmap contract. The explicit subpixel policy preserves upstream
    /// formats for a future triplet-serving path.
    pub fn with_coverage_policy(
        rasterizer: R,
        capacity: usize,
        coverage_policy: CoveragePolicy,
    ) -> Result<Self, RenderError> {
        Self::with_coverage_policy_and_byte_capacity(
            rasterizer,
            capacity,
            DEFAULT_GLYPH_CACHE_BYTE_CAPACITY,
            coverage_policy,
        )
    }

    /// Wraps `rasterizer` with explicit entry, byte, and coverage limits.
    pub fn with_coverage_policy_and_byte_capacity(
        rasterizer: R,
        capacity: usize,
        byte_capacity: usize,
        coverage_policy: CoveragePolicy,
    ) -> Result<Self, RenderError> {
        if capacity == 0 {
            return Err(RenderError::InvalidInput {
                reason: "glyph cache capacity must be non-zero",
            });
        }
        if byte_capacity == 0 {
            return Err(RenderError::InvalidInput {
                reason: "glyph cache byte capacity must be non-zero",
            });
        }
        Ok(Self {
            rasterizer,
            entries: HashMap::new(),
            capacity,
            byte_capacity,
            retained_bytes: 0,
            transient_key: None,
            transient_bitmap: None,
            coverage_policy,
            lookups_hit: 0,
            lookups_missed: 0,
            clock: 0,
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

    /// Number of retained pixel bytes, excluding map and key overhead.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Maximum retained pixel bytes.
    #[must_use]
    pub const fn byte_capacity(&self) -> usize {
        self.byte_capacity
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

    /// Borrowed access to the wrapped rasterizer (the grid pipeline uses
    /// this to read font-wide metrics for baseline placement without
    /// disturbing cached entries or counters).
    #[must_use]
    pub const fn rasterizer(&self) -> &R {
        &self.rasterizer
    }

    /// Mutable access to the wrapped rasterizer (the shaped grid path
    /// uses this to reach `SwashSingle::shape_run` and its caches; the
    /// char-keyed glyph cache itself is untouched by shaping).
    pub fn rasterizer_mut(&mut self) -> &mut R {
        &mut self.rasterizer
    }

    /// Drops all cached entries (bitmaps and blanks) without resetting the
    /// cumulative hit/miss counters.
    ///
    /// Call after the rasterized size changes (for example a DPI rescale):
    /// keys embed the point size, so stale entries would otherwise pin
    /// previous-scale bitmaps while new-scale keys rasterize alongside them.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
        self.transient_key = None;
        self.transient_bitmap = None;
    }

    /// Returns the glyph for `key`, rasterizing on first use.
    ///
    /// # Errors
    ///
    /// Propagates rasterizer failures without caching them (see module docs).
    pub fn glyph(&mut self, key: RasterKey) -> Result<CachedGlyph<'_>, RenderError> {
        let _ = self.cached_glyph(key)?;
        if self.transient_key == Some(key) {
            return Ok(match self.transient_bitmap.as_ref() {
                Some(bitmap) => CachedGlyph::Bitmap(bitmap.as_glyph_bitmap()),
                None => CachedGlyph::Blank,
            });
        }
        Ok(match self.entries.get(&key) {
            Some(CacheEntry {
                bitmap: Some(bitmap),
                ..
            }) => CachedGlyph::Bitmap(bitmap.as_glyph_bitmap()),
            Some(CacheEntry { bitmap: None, .. }) | None => CachedGlyph::Blank,
        })
    }

    /// Returns the private cache representation for renderer hot paths.
    pub(crate) fn cached_glyph(
        &mut self,
        key: RasterKey,
    ) -> Result<CachedGlyphRef<'_>, RenderError> {
        let now = self.tick();
        if self.transient_key == Some(key) {
            self.lookups_hit = self.lookups_hit.saturating_add(1);
            return Ok(match self.transient_bitmap.as_ref() {
                Some(bitmap) => CachedGlyphRef::Bitmap(bitmap),
                None => CachedGlyphRef::Blank,
            });
        }
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = now;
            self.lookups_hit = self.lookups_hit.saturating_add(1);
        } else {
            self.lookups_missed = self.lookups_missed.saturating_add(1);
            // Rasterize first: a failed rasterization must not evict a live
            // entry (errors are never cached).
            let bitmap = self
                .rasterizer
                .rasterize(key)?
                .map(|bitmap| match self.coverage_policy {
                    CoveragePolicy::Grayscale => CachedBitmap::grayscale(bitmap),
                    CoveragePolicy::Subpixel => CachedBitmap::source(bitmap),
                })
                .transpose()?;
            let bitmap_bytes = bitmap.as_ref().map_or(0, CachedBitmap::retained_bytes);
            if bitmap_bytes > self.byte_capacity {
                self.transient_key = Some(key);
                self.transient_bitmap = bitmap;
                return Ok(match self.transient_bitmap.as_ref() {
                    Some(bitmap) => CachedGlyphRef::Bitmap(bitmap),
                    None => CachedGlyphRef::Blank,
                });
            }
            while self.entries.len() >= self.capacity
                || self.retained_bytes.saturating_add(bitmap_bytes) > self.byte_capacity
            {
                self.evict_lru();
            }
            self.retained_bytes = self.retained_bytes.saturating_add(bitmap_bytes);
            self.entries.insert(
                key,
                CacheEntry {
                    bitmap,
                    last_used: now,
                },
            );
        }
        Ok(match &self.entries[&key] {
            CacheEntry {
                bitmap: Some(bitmap),
                ..
            } => CachedGlyphRef::Bitmap(bitmap),
            CacheEntry { bitmap: None, .. } => CachedGlyphRef::Blank,
        })
    }

    /// Advances the recency clock for one lookup.
    ///
    /// The counter wraps (one tick per lookup, so the wrap horizon is 2^64
    /// lookups); stamps are unique up to that horizon, which keeps eviction
    /// independent of `HashMap` iteration order.
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Evicts exactly the least recently used entry. Called only when the
    /// cache is at capacity, so a victim always exists.
    fn evict_lru(&mut self) {
        self.evict_lru_matching(|_| true);
    }

    fn evict_lru_matching(&mut self, mut retain: impl FnMut(&RasterKey) -> bool) {
        if let Some(victim) = self
            .entries
            .iter()
            .filter(|(key, _)| retain(key))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| *key)
        {
            if let Some(entry) = self.entries.remove(&victim) {
                let bytes = entry
                    .bitmap
                    .as_ref()
                    .map_or(0, CachedBitmap::retained_bytes);
                self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CachedGlyphRef<'a> {
    Bitmap(&'a CachedBitmap),
    Blank,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{GlyphBitmap, GlyphMetrics, GlyphRasterizer};
    use std::cell::Cell;

    /// Deterministic fake implementing the full trait contract; counts
    /// upstream calls and can be told to fail or report blanks.
    #[derive(Debug, Default)]
    struct FakeRasterizer {
        next_id: u64,
        rasterize_calls: Cell<u32>,
        fail_next: Cell<bool>,
        blank_keys: Vec<char>,
        malformed_keys: Vec<char>,
        oversized_keys: Vec<char>,
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
            if self.malformed_keys.contains(&key.character) {
                return Ok(Some(GlyphBitmap {
                    metrics: GlyphMetrics {
                        left: 0,
                        top: 8,
                        width: 2,
                        height: 2,
                        advance: [2, 0],
                    },
                    format: crate::glyph::BitmapFormat::Rgb,
                    data: vec![0; 3],
                }));
            }
            if self.oversized_keys.contains(&key.character) {
                let side = 3000usize;
                return Ok(Some(
                    GlyphBitmap::try_new(
                        GlyphMetrics {
                            left: 0,
                            top: 3000,
                            width: i32::try_from(side).unwrap(),
                            height: i32::try_from(side).unwrap(),
                            advance: [i32::try_from(side).unwrap(), 0],
                        },
                        crate::glyph::BitmapFormat::Rgb,
                        vec![0xAA; side * side * 3],
                    )
                    .unwrap(),
                ));
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
    fn zero_byte_capacity_is_rejected() {
        assert!(matches!(
            GlyphCache::with_coverage_policy_and_byte_capacity(
                FakeRasterizer::default(),
                1,
                0,
                CoveragePolicy::Grayscale,
            ),
            Err(RenderError::InvalidInput { .. })
        ));
    }

    #[test]
    fn malformed_bitmaps_are_rejected_before_caching_under_both_policies() {
        for policy in [CoveragePolicy::Grayscale, CoveragePolicy::Subpixel] {
            let mut fake = FakeRasterizer::default();
            fake.malformed_keys.push('!');
            let mut cache = GlyphCache::with_coverage_policy(fake, 4, policy).unwrap();
            let font = cache.load_font(&font_query()).unwrap();
            let key = RasterKey::new('!', font, 12.0).unwrap();
            assert!(matches!(
                cache.glyph(key),
                Err(RenderError::InvalidInput { .. })
            ));
            assert!(cache.is_empty());
        }
    }

    #[test]
    fn retained_byte_quota_evicts_before_insertion() {
        let mut cache = GlyphCache::with_coverage_policy_and_byte_capacity(
            FakeRasterizer::default(),
            8,
            12,
            CoveragePolicy::Grayscale,
        )
        .unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        let first = RasterKey::new('A', font, 12.0).unwrap();
        let second = RasterKey::new('A', font, 13.0).unwrap();
        let _ = cache.glyph(first).unwrap();
        let _ = cache.glyph(second).unwrap();
        assert!(cache.retained_bytes() <= cache.byte_capacity());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn repeated_public_glyph_lookups_keep_source_storage_within_quota() {
        let mut cache = GlyphCache::with_coverage_policy_and_byte_capacity(
            FakeRasterizer::default(),
            8,
            12,
            CoveragePolicy::Grayscale,
        )
        .unwrap();
        let key = key(&mut cache, 'A');

        let mut first_address = None;
        for _ in 0..32 {
            let CachedGlyph::Bitmap(bitmap) = cache.glyph(key).unwrap() else {
                panic!("expected bitmap");
            };
            let address = std::ptr::from_ref(bitmap);
            if let Some(first_address) = first_address {
                assert_eq!(
                    address, first_address,
                    "public lookup must borrow one bitmap"
                );
            } else {
                first_address = Some(address);
            }
            assert!(cache.retained_bytes() <= cache.byte_capacity());
        }

        // 'A' is a 2x2 RGB source. The public view borrows the same twelve
        // source bytes retained by the cache instead of creating a duplicate.
        assert_eq!(cache.retained_bytes(), 12);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
    }

    #[test]
    fn repeated_public_lookups_keep_oversized_source_in_one_transient_slot() {
        let mut cache = GlyphCache::with_coverage_policy_and_byte_capacity(
            FakeRasterizer::default(),
            8,
            11,
            CoveragePolicy::Grayscale,
        )
        .unwrap();
        let key = key(&mut cache, 'A');

        let mut first_address = None;
        for _ in 0..32 {
            let CachedGlyph::Bitmap(bitmap) = cache.glyph(key).unwrap() else {
                panic!("expected bitmap");
            };
            let address = std::ptr::from_ref(bitmap);
            if let Some(first_address) = first_address {
                assert_eq!(
                    address, first_address,
                    "transient lookup must borrow one bitmap"
                );
            } else {
                first_address = Some(address);
            }
            assert_eq!(cache.retained_bytes(), 0);
        }

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
    }

    #[test]
    fn oversized_bitmap_uses_bounded_transient_cache_without_rerasterizing() {
        let mut fake = FakeRasterizer::default();
        fake.oversized_keys.push('!');
        let mut cache = GlyphCache::new(fake, 1).unwrap();
        let first = key(&mut cache, '!');
        assert!(matches!(
            cache.cached_glyph(first),
            Ok(CachedGlyphRef::Bitmap(_))
        ));
        assert_eq!(cache.len(), 0, "oversized entry is not retained in LRU map");
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
        assert!(matches!(
            cache.cached_glyph(first),
            Ok(CachedGlyphRef::Bitmap(_))
        ));
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 1);
        assert!(matches!(cache.glyph(first), Ok(CachedGlyph::Bitmap(_))));
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
    fn default_cache_materializes_cached_grayscale_as_public_rgb() {
        let mut cache =
            GlyphCache::new(FakeRasterizer::default(), DEFAULT_GLYPH_CACHE_CAPACITY).unwrap();
        let k = key(&mut cache, 'A');
        let CachedGlyph::Bitmap(bitmap) = cache.glyph(k).unwrap() else {
            panic!("expected bitmap");
        };
        let pixels = usize::try_from(bitmap.metrics.width * bitmap.metrics.height).unwrap();
        assert_eq!(bitmap.format, crate::glyph::BitmapFormat::Rgb);
        assert_eq!(bitmap.data.len(), pixels * 3);
        assert!(
            bitmap
                .data
                .chunks_exact(3)
                .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
        );
    }

    #[test]
    fn explicit_subpixel_cache_preserves_upstream_format() {
        let mut cache = GlyphCache::with_coverage_policy(
            FakeRasterizer::default(),
            DEFAULT_GLYPH_CACHE_CAPACITY,
            CoveragePolicy::Subpixel,
        )
        .unwrap();
        let k = key(&mut cache, 'A');
        let CachedGlyph::Bitmap(bitmap) = cache.glyph(k).unwrap() else {
            panic!("expected bitmap");
        };
        assert_eq!(
            bitmap.data.len(),
            3 * bitmap.metrics.width as usize * bitmap.metrics.height as usize
        );
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
    fn lru_eviction_keeps_the_bound_and_retains_hot_entries() {
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 4).unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        for ch in 'a'..'e' {
            let _ = cache.glyph(RasterKey::new(ch, font, 12.0).unwrap());
        }
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 4);

        // Touch 'a' so 'b' becomes the least recently used entry.
        let _ = cache.glyph(RasterKey::new('a', font, 12.0).unwrap());
        let _ = cache.glyph(RasterKey::new('z', font, 12.0).unwrap());
        // One entry evicted, bound held, no wholesale clear.
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.rasterizer.rasterize_calls.get(), 5);

        // The hot entry survived; the cold one rasterizes again on demand.
        let before = cache.rasterizer.rasterize_calls.get();
        let _ = cache.glyph(RasterKey::new('a', font, 12.0).unwrap());
        assert_eq!(
            cache.rasterizer.rasterize_calls.get(),
            before,
            "most recently used entry must stay cached"
        );
        let _ = cache.glyph(RasterKey::new('b', font, 12.0).unwrap());
        assert_eq!(
            cache.rasterizer.rasterize_calls.get(),
            before + 1,
            "least recently used entry must have been evicted"
        );
    }

    #[test]
    fn eviction_ignores_failed_rasterizations() {
        // A transient upstream failure must not evict a live entry.
        let mut cache = GlyphCache::new(FakeRasterizer::default(), 2).unwrap();
        let font = cache.load_font(&font_query()).unwrap();
        let a = RasterKey::new('a', font, 12.0).unwrap();
        let b = RasterKey::new('b', font, 12.0).unwrap();
        let _ = cache.glyph(a).unwrap();
        let _ = cache.glyph(b).unwrap();
        assert_eq!(cache.len(), 2);

        cache.rasterizer.fail_next.set(true);
        let z = RasterKey::new('z', font, 12.0).unwrap();
        assert!(matches!(
            cache.glyph(z),
            Err(RenderError::UpstreamRasterizer(_))
        ));
        // Both resident entries are still cached.
        let before = cache.rasterizer.rasterize_calls.get();
        let _ = cache.glyph(a).unwrap();
        let _ = cache.glyph(b).unwrap();
        assert_eq!(cache.rasterizer.rasterize_calls.get(), before);
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
