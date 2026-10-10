//! Kitty raster mechanics (Core-owned, issue #1802).
//!
//! Nearest-neighbor scaling of stored RGBA bitmaps to placement rects,
//! plus the bounded scaled-blit cache (S6, #1849). The `bitty-graphics`
//! extension crate owns its own copy plus its own cache (W-141); Core
//! keeps this copy so the present layer can composite without a
//! Core-to-extension call shape. The per-frame budget caps stay Core-owned
//! policy ([`super::kitty_place`]).
//!
//! Bounds: every length is validated with checked arithmetic **before**
//! any buffer is allocated, including the nearest-neighbor output
//! (`rect_w * rect_h * 4`, itself bounded because the caller clamps the
//! rect to the viewport first) and every cache admission (entry count and
//! resident bytes, both mirroring the per-frame ceilings).
//!
//! The cache is a pure optimization: a miss falls back to the uncached
//! [`rasterize_kitty_clipped`] path, so correctness never depends on it.
//! Entries hand out **owned** buffers (cloned on every hit) precisely so
//! the S7 cursor punch can zero blit bytes in place without corrupting
//! resident entries — a future cache must keep this clone-before-write
//! shape and never hand out shared buffers.

use super::kitty_place::{KITTY_DECODE_MAX_BYTES, KittyImageId, KittyPlacedImage};
use crate::geometry::RectPx;

/// Maximum raster-cache entries (S6, #1849): one per-frame blit budget
/// slot ([`super::kitty_place::KITTY_PRESENT_MAX_BLITS_PER_FRAME`]).
pub const KITTY_RASTER_CACHE_MAX_ENTRIES: usize = 32;

/// Maximum raster-cache resident bytes (S6, #1849): one per-frame byte
/// budget ([`super::kitty_place::KITTY_PRESENT_MAX_BYTES_PER_FRAME`],
/// 64 MiB).
pub const KITTY_RASTER_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Scales stored RGBA to the exact `rect` pixel extent (nearest neighbor).
///
/// Returns `None` (paints nothing) when `rect` is empty, when the output
/// byte size fails checked validation against the 64 MiB cap, or when the
/// source bitmap fails validation. No allocation occurs before validation.
///
/// The output is straight-alpha RGBA8, row-major, exactly
/// `rect.width * rect.height * 4` bytes — the shape the present layer
/// composites.
#[must_use]
pub fn rasterize_kitty(image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>> {
    rasterize_kitty_clipped(image, rect, rect)
}

/// Scales the visible window of a placement (nearest neighbor).
///
/// `full` is the unclamped placement extent (the image scales into this,
/// exactly like [`rasterize_kitty`] would); `visible` is the
/// viewport-clipped sub-rectangle to emit (`visible` must lie inside
/// `full`). The output is bit-identical to scaling the whole image into
/// `full` and then cropping `visible` — but only the visible bytes are
/// ever allocated, so a placement overflowing the viewport paints its
/// visible part at true scale instead of squeezing the whole image into it.
///
/// Returns `None` (paints nothing) when `visible` is empty or outside
/// `full`, when the visible byte size fails checked validation against
/// the 64 MiB cap, or when the source bitmap fails validation. No
/// allocation occurs before validation.
///
/// The output is straight-alpha RGBA8, row-major, exactly
/// `visible.width * visible.height * 4` bytes.
#[must_use]
pub fn rasterize_kitty_clipped(
    image: &KittyPlacedImage,
    full: RectPx,
    visible: RectPx,
) -> Option<Vec<u8>> {
    if visible.width == 0 || visible.height == 0 || full.width == 0 || full.height == 0 {
        return None;
    }
    // `visible` must lie inside `full` (same origin space); anything else
    // is a caller bug and fails closed. `i64` differences of `i32`
    // coordinates never overflow; non-negative after the origin guard,
    // so the `as u64` casts are exact.
    if visible.x < full.x || visible.y < full.y {
        return None;
    }
    let offset_x = (i64::from(visible.x) - i64::from(full.x)) as u64;
    let offset_y = (i64::from(visible.y) - i64::from(full.y)) as u64;
    if offset_x + u64::from(visible.width) > u64::from(full.width)
        || offset_y + u64::from(visible.height) > u64::from(full.height)
    {
        return None;
    }
    let out_len = (u64::from(visible.width) * u64::from(visible.height))
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES as u64)?;
    // `out_len` fits `usize` on every supported target: it is at most
    // 64 MiB while `usize` is at least 32 bits.
    let out_len = out_len as usize;
    if image.width == 0 || image.height == 0 {
        return None;
    }
    let expected_src = (u64::from(image.width) * u64::from(image.height))
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES as u64)?;
    if image.rgba.len() as u64 != expected_src {
        return None;
    }
    let mut out = vec![0_u8; out_len];
    let (sw, sh) = (u64::from(image.width), u64::from(image.height));
    let (fw, fh) = (u64::from(full.width), u64::from(full.height));
    let (vw, vh) = (u64::from(visible.width), u64::from(visible.height));
    for dy in 0..vh {
        // Nearest neighbor into the full extent, then the visible window:
        // `sy = (offset_y + dy) * sh / fh` — division in u64, exact for
        // the bounded ranges here. Bit-identical to scaling into `full`
        // and cropping `visible`.
        let sy = (offset_y + dy) * sh / fh;
        for dx in 0..vw {
            let sx = (offset_x + dx) * sw / fw;
            let src = ((sy * sw + sx) * 4) as usize;
            let dst = ((dy * vw + dx) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&image.rgba[src..src + 4]);
        }
    }
    Some(out)
}

/// Cache key for one scaled blit (S6, #1849).
///
/// Binds the image identity **plus** its content generation — never a bare
/// id — so an id reuse (counter wrap, currently only theoretical at 2^64
/// stores; see [`KittyPlacedImage::generation`]) can never serve stale
/// bytes: a reused id carries a fresh generation and misses. `frame` is
/// the animation frame index, always `0` until S1 animation lands; it is
/// part of the key from the start so a future animated image cannot alias
/// frame 0's bytes. `src_width`/`src_height` bind the decoded extent the
/// raster sampled, and `full`/`visible` bind the placement geometry, so
/// the same image at two sizes never aliases.
///
/// Construct only via [`KittyRasterKey::for_image`]: the constructor takes
/// the stored image itself, so callers cannot forget the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyRasterKey {
    /// Stored image handle.
    pub image: KittyImageId,
    /// Content generation (see [`KittyPlacedImage::generation`]).
    pub generation: u64,
    /// Animation frame index (`0` until S1).
    pub frame: u32,
    /// Decoded source width the raster sampled.
    pub src_width: u32,
    /// Decoded source height the raster sampled.
    pub src_height: u32,
    /// Unclamped placement extent the image scales into.
    pub full: RectPx,
    /// Viewport-clipped window emitted.
    pub visible: RectPx,
}

impl KittyRasterKey {
    /// Binds a key to one stored image and placement geometry.
    ///
    /// `frame` is always `0` until S1 animation wires real frames.
    #[must_use]
    pub const fn for_image(
        image: &KittyPlacedImage,
        frame: u32,
        full: RectPx,
        visible: RectPx,
    ) -> Self {
        Self {
            image: image.id,
            generation: image.generation,
            frame,
            src_width: image.width,
            src_height: image.height,
            full,
            visible,
        }
    }
}

/// Bounded scaled-blit cache (S6, #1849): oldest-first (LRU) eviction
/// under the per-frame ceilings.
///
/// Static placements must not re-scale their decoded image every present
/// tick; the cache retains the scaled bytes for an unchanged
/// [`KittyRasterKey`]. At most [`KITTY_RASTER_CACHE_MAX_ENTRIES`]
/// entries and [`KITTY_RASTER_CACHE_MAX_BYTES`] resident bytes; a blit
/// larger than the byte cap is returned uncached (never admitted).
/// `hits`/`misses` are exposed so tests prove reuse, invalidation, and
/// the disable-path fallback.
///
/// Every entry is an **owned** `Vec<u8>` and every hit clones it out, so
/// the S7 cursor punch (which zeroes blit bytes in place) can never
/// corrupt a resident entry. The cache is a pure optimization: a miss —
/// including a disabled cache — falls back to
/// [`rasterize_kitty_clipped`], so correctness never depends on it.
#[derive(Debug, Default)]
pub struct KittyRasterCache {
    entries: Vec<(KittyRasterKey, Vec<u8>)>,
    bytes: usize,
    hits: u64,
    misses: u64,
    enabled: bool,
}

impl KittyRasterCache {
    /// An enabled empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            bytes: 0,
            hits: 0,
            misses: 0,
            enabled: true,
        }
    }

    /// Whether lookups and admissions run (`false` forces every lookup to
    /// miss and every insert to no-op; the caller still paints via the
    /// uncached path).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enables or disables the cache (disable-path test seam: a disabled
    /// cache still paints via the uncached fallback).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
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

    /// Cache hits (owned clone served, no rasterization).
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Cache misses (caller falls back to [`rasterize_kitty_clipped`]).
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Drops every entry rasterized from `image` (called when that image
    /// is removed or evicted; a reused id carries a fresh generation and
    /// would miss anyway, but dropping reclaims the budget at once).
    pub fn invalidate_image(&mut self, image: KittyImageId) {
        self.entries.retain(|(key, blit)| {
            let drop = key.image == image;
            if drop {
                self.bytes = self.bytes.saturating_sub(blit.len());
            }
            !drop
        });
    }

    /// Returns an **owned** copy of the cached blit for `key`, promoting
    /// it to most-recently-used.
    ///
    /// The returned buffer is a fresh clone: the caller (notably the S7
    /// cursor punch, which zeroes blit bytes in place) may mutate it
    /// freely without corrupting the resident entry. Returns `None` on a
    /// miss or when the cache is disabled; the caller then rasterizes via
    /// [`rasterize_kitty_clipped`] and admits with [`Self::insert`].
    #[must_use]
    pub fn get(&mut self, key: &KittyRasterKey) -> Option<Vec<u8>> {
        if !self.enabled {
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        let Some(pos) = self
            .entries
            .iter()
            .position(|(resident, _)| resident == key)
        else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        self.hits = self.hits.saturating_add(1);
        let (hit_key, hit_blit) = self.entries.remove(pos);
        let owned = hit_blit.clone();
        self.entries.push((hit_key, hit_blit));
        Some(owned)
    }

    /// Admits `rgba` under `key` with checked-arithmetic budget
    /// enforcement and deterministic oldest-first (LRU) eviction.
    ///
    /// Returns `true` when admitted. Returns `false` (keeping the
    /// previous state, still paintable via the uncached bytes the caller
    /// holds) when the cache is disabled, when `rgba` alone exceeds
    /// [`KITTY_RASTER_CACHE_MAX_BYTES`], or when the byte accounting
    /// itself overflows (fail closed; unreachable under the caps, but
    /// hostile lengths must never wrap it). An already-resident key is
    /// replaced in place (bytes re-charged) and promoted.
    pub fn insert(&mut self, key: KittyRasterKey, rgba: Vec<u8>) -> bool {
        if !self.enabled {
            return false;
        }
        let bytes = rgba.len();
        if bytes > KITTY_RASTER_CACHE_MAX_BYTES {
            return false;
        }
        if let Some(pos) = self
            .entries
            .iter()
            .position(|(resident, _)| resident == &key)
        {
            let (_, old) = self.entries.remove(pos);
            self.bytes = self.bytes.saturating_sub(old.len());
        }
        // Checked admission: evict oldest-first until the new bytes fit
        // the byte cap and the entry cap holds. `checked_add` fails
        // closed instead of wrapping on hostile lengths.
        while self.entries.len() >= KITTY_RASTER_CACHE_MAX_ENTRIES || {
            let next = self.bytes.checked_add(bytes);
            next.is_none_or(|n| n > KITTY_RASTER_CACHE_MAX_BYTES)
        } {
            if self.entries.is_empty() {
                break;
            }
            let (_, evicted) = self.entries.remove(0);
            self.bytes = self.bytes.saturating_sub(evicted.len());
        }
        let Some(next) = self.bytes.checked_add(bytes) else {
            return false;
        };
        if next > KITTY_RASTER_CACHE_MAX_BYTES
            || self.entries.len() >= KITTY_RASTER_CACHE_MAX_ENTRIES
        {
            return false;
        }
        self.bytes = next;
        self.entries.push((key, rgba));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_place::{KittyImageId, KittyPlacedImage};

    fn solid_red_2x2() -> KittyPlacedImage {
        KittyPlacedImage {
            id: KittyImageId(1),
            generation: 1,
            origin: None,
            width: 2,
            height: 2,
            rgba: [0xFF, 0x00, 0x00, 0xFF].repeat(4),
            compressed_len: 16,
        }
    }

    fn two_color_2x1() -> KittyPlacedImage {
        KittyPlacedImage {
            id: KittyImageId(2),
            generation: 1,
            origin: None,
            width: 2,
            height: 1,
            rgba: vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF],
            compressed_len: 8,
        }
    }

    fn key_for(image: &KittyPlacedImage, full: RectPx, visible: RectPx) -> KittyRasterKey {
        KittyRasterKey::for_image(image, 0, full, visible)
    }

    #[test]
    fn identity_scale_is_byte_identical() {
        let image = solid_red_2x2();
        let rect = RectPx::new(0, 0, 2, 2);
        assert_eq!(rasterize_kitty(&image, rect), Some(image.rgba.clone()));
    }

    #[test]
    fn upscale_replicates_nearest() {
        // 2x1 red|blue scaled to 4x1 doubles each source column.
        let image = two_color_2x1();
        let out = rasterize_kitty(&image, RectPx::new(0, 0, 4, 1)).expect("raster");
        assert_eq!(
            out,
            vec![
                0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00,
                0xFF, 0xFF,
            ]
        );
    }

    #[test]
    fn downscale_samples_nearest() {
        // 2x2 solid red down to 1x1 stays red and opaque.
        let image = solid_red_2x2();
        let out = rasterize_kitty(&image, RectPx::new(5, 5, 1, 1)).expect("raster");
        assert_eq!(out, vec![0xFF, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn clipped_matches_scale_then_crop() {
        // Full 4x1 scale of red|blue, then the right-half crop, must equal
        // the clipped raster of the right-half window.
        let image = two_color_2x1();
        let full = RectPx::new(0, 0, 4, 1);
        let visible = RectPx::new(2, 0, 2, 1);
        let out = rasterize_kitty_clipped(&image, full, visible).expect("raster");
        assert_eq!(out, vec![0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn empty_or_outside_rects_paint_nothing() {
        let image = solid_red_2x2();
        assert_eq!(rasterize_kitty(&image, RectPx::new(0, 0, 0, 2)), None);
        let full = RectPx::new(0, 0, 2, 2);
        // `visible` outside `full` fails closed.
        assert_eq!(
            rasterize_kitty_clipped(&image, full, RectPx::new(4, 4, 2, 2)),
            None
        );
        // `visible` larger than `full` fails closed.
        assert_eq!(
            rasterize_kitty_clipped(&image, full, RectPx::new(0, 0, 4, 4)),
            None
        );
    }

    #[test]
    fn corrupt_source_paints_nothing() {
        let bad = KittyPlacedImage {
            id: KittyImageId(9),
            generation: 1,
            origin: None,
            width: 2,
            height: 2,
            rgba: vec![0; 5],
            compressed_len: 5,
        };
        assert_eq!(rasterize_kitty(&bad, RectPx::new(0, 0, 2, 2)), None);
    }

    #[test]
    fn cache_hit_is_byte_identical_to_uncached() {
        let image = two_color_2x1();
        let full = RectPx::new(0, 0, 4, 1);
        let visible = RectPx::new(0, 0, 4, 1);
        let uncached = rasterize_kitty_clipped(&image, full, visible).expect("raster");
        let mut cache = KittyRasterCache::new();
        let key = key_for(&image, full, visible);
        assert_eq!(cache.get(&key), None, "cold cache must miss");
        assert!(cache.insert(key, uncached.clone()));
        assert_eq!(cache.get(&key), Some(uncached), "hit must equal uncached");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn cache_eviction_is_deterministic_lru() {
        let mut cache = KittyRasterCache::new();
        let image = solid_red_2x2();
        let mut keys = Vec::new();
        for i in 0..KITTY_RASTER_CACHE_MAX_ENTRIES {
            let full = RectPx::new(i as i32 * 4, 0, 2, 2);
            let key = key_for(&image, full, full);
            let rgba = rasterize_kitty_clipped(&image, full, full).expect("raster");
            assert!(cache.insert(key, rgba));
            keys.push(key);
        }
        assert_eq!(cache.len(), KITTY_RASTER_CACHE_MAX_ENTRIES);
        // Promote the oldest entry: it becomes most-recently-used.
        assert!(cache.get(&keys[0]).is_some());
        // One more admission evicts the oldest non-promoted entry
        // (`keys[1]`), never the promoted one.
        let fresh_full = RectPx::new(10_000, 0, 2, 2);
        let fresh_key = key_for(&image, fresh_full, fresh_full);
        let fresh_rgba = rasterize_kitty_clipped(&image, fresh_full, fresh_full).expect("raster");
        assert!(cache.insert(fresh_key, fresh_rgba));
        assert_eq!(cache.len(), KITTY_RASTER_CACHE_MAX_ENTRIES);
        assert!(
            cache.get(&keys[0]).is_some(),
            "promoted entry must survive eviction"
        );
        assert_eq!(
            cache.get(&keys[1]),
            None,
            "oldest non-promoted entry must evict first"
        );
    }

    #[test]
    fn cache_key_never_bare_id_reuse_misses() {
        let mut cache = KittyRasterCache::new();
        let full = RectPx::new(0, 0, 2, 2);
        let first = solid_red_2x2();
        let rgba = rasterize_kitty_clipped(&first, full, full).expect("raster");
        assert!(cache.insert(key_for(&first, full, full), rgba));
        // Same id, fresh generation (id-reuse after wrap): must miss.
        let reused = KittyPlacedImage {
            generation: first.generation.wrapping_add(1).max(1),
            rgba: [0x00, 0x00, 0xFF, 0xFF].repeat(4),
            ..first.clone()
        };
        assert_ne!(reused.generation, first.generation);
        assert_eq!(
            cache.get(&key_for(&reused, full, full)),
            None,
            "reused id with fresh generation must never hit stale bytes"
        );
        // Same id+generation, different geometry: must miss.
        let shifted = RectPx::new(4, 4, 2, 2);
        assert_eq!(
            cache.get(&key_for(&first, shifted, shifted)),
            None,
            "same image at different rects must not alias"
        );
        // Same id+generation, different frame (S1 future): must miss.
        let framed = KittyRasterKey::for_image(&first, 1, full, full);
        assert_eq!(
            cache.get(&framed),
            None,
            "frame index is part of the key from the start"
        );
    }

    #[test]
    fn cache_respects_entry_and_byte_budgets() {
        let mut cache = KittyRasterCache::new();
        let image = solid_red_2x2();
        // Entry cap: 33 tiny admissions keep exactly 32, oldest evicted.
        let mut first_key = None;
        for i in 0..KITTY_RASTER_CACHE_MAX_ENTRIES + 1 {
            let full = RectPx::new(i as i32 * 4, 0, 2, 2);
            let key = key_for(&image, full, full);
            if i == 0 {
                first_key = Some(key);
            }
            let rgba = rasterize_kitty_clipped(&image, full, full).expect("raster");
            assert!(cache.insert(key, rgba));
            assert!(cache.len() <= KITTY_RASTER_CACHE_MAX_ENTRIES);
            assert!(cache.total_bytes() <= KITTY_RASTER_CACHE_MAX_BYTES);
        }
        assert_eq!(cache.len(), KITTY_RASTER_CACHE_MAX_ENTRIES);
        assert_eq!(
            cache.get(&first_key.expect("first key")),
            None,
            "oldest entry must evict at the entry cap"
        );
        // Byte cap: 4 MiB blits accumulate to the 64 MiB ceiling, then
        // evict oldest-first so the total never exceeds it.
        let mut cache = KittyRasterCache::new();
        let wide = KittyPlacedImage {
            id: KittyImageId(70),
            generation: 1,
            origin: None,
            width: 1024,
            height: 1024,
            rgba: vec![0x7F; 1024 * 1024 * 4],
            compressed_len: 1024 * 1024 * 4,
        };
        let mut byte_keys = Vec::new();
        for i in 0..17 {
            let full = RectPx::new(i * 1024, 0, 1024, 1024);
            let key = KittyRasterKey::for_image(&wide, 0, full, full);
            let rgba = rasterize_kitty_clipped(&wide, full, full).expect("raster");
            assert_eq!(rgba.len(), 4 * 1024 * 1024);
            assert!(cache.insert(key, rgba));
            assert!(cache.total_bytes() <= KITTY_RASTER_CACHE_MAX_BYTES);
            byte_keys.push(key);
        }
        assert!(cache.total_bytes() <= KITTY_RASTER_CACHE_MAX_BYTES);
        assert_eq!(
            cache.get(&byte_keys[0]),
            None,
            "oldest bytes must evict at the byte cap"
        );
        // A single blit larger than the whole cache is never admitted
        // (the caller still paints it uncached).
        assert!(!cache.insert(
            KittyRasterKey::for_image(&wide, 0, RectPx::new(0, 0, 2, 2), RectPx::new(0, 0, 2, 2)),
            vec![0u8; KITTY_RASTER_CACHE_MAX_BYTES + 1],
        ));
    }

    #[test]
    fn cache_disabled_still_misses_and_paints_uncached() {
        let image = solid_red_2x2();
        let full = RectPx::new(0, 0, 2, 2);
        let key = key_for(&image, full, full);
        let uncached = rasterize_kitty_clipped(&image, full, full).expect("raster");
        let mut cache = KittyRasterCache::new();
        cache.set_enabled(false);
        assert!(!cache.is_enabled());
        assert!(
            !cache.insert(key, uncached.clone()),
            "disabled insert is a no-op"
        );
        assert!(cache.is_empty());
        assert_eq!(cache.get(&key), None, "disabled lookup always misses");
        // The uncached path still produces the bytes (disable-path paints).
        assert_eq!(rasterize_kitty_clipped(&image, full, full), Some(uncached));
    }

    #[test]
    fn cache_hands_out_owned_buffers_punch_stays_safe() {
        let image = solid_red_2x2();
        let full = RectPx::new(0, 0, 2, 2);
        let key = key_for(&image, full, full);
        let uncached = rasterize_kitty_clipped(&image, full, full).expect("raster");
        let mut cache = KittyRasterCache::new();
        assert!(cache.insert(key, uncached.clone()));
        // Mutating a hit (like the S7 cursor punch zeroes blit bytes)
        // must not corrupt the resident entry: the next hit is pristine.
        let mut first_hit = cache.get(&key).expect("hit");
        first_hit.fill(0);
        assert_eq!(
            cache.get(&key),
            Some(uncached),
            "hits are owned clones; punch-style mutation cannot poison the cache"
        );
    }

    #[test]
    fn cache_invalidate_image_drops_only_that_image() {
        let mut cache = KittyRasterCache::new();
        let red = solid_red_2x2();
        let blue = KittyPlacedImage {
            id: KittyImageId(3),
            rgba: [0x00, 0x00, 0xFF, 0xFF].repeat(4),
            ..red.clone()
        };
        let full = RectPx::new(0, 0, 2, 2);
        let red_key = key_for(&red, full, full);
        let blue_key = key_for(&blue, full, full);
        let red_rgba = rasterize_kitty_clipped(&red, full, full).expect("raster");
        let blue_rgba = rasterize_kitty_clipped(&blue, full, full).expect("raster");
        assert!(cache.insert(red_key, red_rgba.clone()));
        assert!(cache.insert(blue_key, blue_rgba.clone()));
        cache.invalidate_image(red.id);
        assert_eq!(cache.get(&red_key), None);
        assert_eq!(cache.get(&blue_key), Some(blue_rgba));
    }
}
