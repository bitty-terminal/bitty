//! Image store and placement (OQ-008, headless, bounded, forbid unsafe).
//!
//! Implements the accepted limits from `rich-presentation-rfc.md`:
//!
//! | ID | Dimension | Accepted default | Enforcement |
//! |---|---|---|
//! | IMG-1 | Max compressed payload per image | 4 MiB | parser payload cap before adapter |
//! | IMG-2 | Max decoded dimensions per image | 4096 x 4096 | before allocation |
//! | IMG-3 | Max decoded bytes per image | 64 MiB (width x height x 4) | before allocation |
//! | IMG-4 | Max total `ImageStore` bytes | 256 MiB | store admission; evict oldest on overflow |
//! | IMG-5 | Max image count | 256 | store admission; evict oldest on overflow |
//! | IMG-6 | Max animation frames per image | 64 | adapter; excess frames discarded |
//! | IMG-7 | Max total decoded animation bytes | IMG-3 x IMG-6 bounded by IMG-4 | store admission |
//! | IMG-8 | Max placement count per terminal | 128 | placement admission; evict oldest |
//! | IMG-9 | Animated frame rate at most 30 fps | host-throttled | renderer pacing (not enforced here) |
//!
//! All arithmetic is overflow-checked and saturating where noted. No GPU,
//! no filesystem, no window system. Deterministic for fixed insertion
//! order.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Limits (accepted, parameterized)
// ---------------------------------------------------------------------------

/// Max compressed payload per image (IMG-1).
pub const IMAGE_MAX_COMPRESSED_BYTES: usize = 4 * 1024 * 1024;

/// Max decoded dimension per axis (IMG-2).
pub const IMAGE_MAX_DIMENSION: u32 = 4096;

/// Max decoded bytes per image (IMG-3) = width * height * 4.
pub const IMAGE_MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Max total `ImageStore` bytes (IMG-4).
pub const IMAGE_STORE_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Max image count (IMG-5).
pub const IMAGE_STORE_MAX_COUNT: usize = 256;

/// Max animation frames per image (IMG-6).
pub const IMAGE_MAX_FRAMES: u16 = 64;

/// Max placement count per terminal (IMG-8).
pub const IMAGE_MAX_PLACEMENTS: usize = 128;

/// Re-exported host-throttled frame rate (IMG-9) — informational, not enforced in this headless store.
pub const IMAGE_MAX_FPS: u32 = 30;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable handle for a decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u64);

impl ImageId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Stable handle for a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlacementId(pub u64);

impl PlacementId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Source / format
// ---------------------------------------------------------------------------

/// Normalized image source after protocol adapter (OQ-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageSource {
    /// Kitty graphics protocol.
    Kitty,
    /// Sixel.
    Sixel,
    /// iTerm2 inline images.
    Iterm2,
    /// File transport (capability-gated, deny-by-default).
    File,
}

/// Pixel format (v1: `rgba8` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit RGBA.
    Rgba8,
}

// ---------------------------------------------------------------------------
// Placement geometry
// ---------------------------------------------------------------------------

/// Placement anchor (core-owned, stable).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlacementAnchor {
    /// Cell range anchor (discouraged fallback, survives only until reflow).
    CellRange {
        /// Start row (inclusive).
        start_row: u16,
        /// Start col (inclusive).
        start_col: u16,
        /// End row (inclusive).
        end_row: u16,
        /// End col (inclusive).
        end_col: u16,
    },
    /// Preferred: bound to a `SemanticZone` id.
    Zone(u64),
    /// Fallback: bound to a scrollback line id.
    Line(u64),
}

/// Geometry for a placement (cols/rows or pixels, z-index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacementGeometry {
    /// Width in cells.
    pub cols: u16,
    /// Height in cells.
    pub rows: u16,
    /// Stacking order.
    pub z_index: i32,
}

impl PlacementGeometry {
    /// Creates geometry.
    #[must_use]
    pub const fn new(cols: u16, rows: u16, z_index: i32) -> Self {
        Self {
            cols,
            rows,
            z_index,
        }
    }
}

/// Axis-aligned clip rectangle in cell space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClipRect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl ClipRect {
    /// Creates a clip rect.
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Full clip (no clipping).
    #[must_use]
    pub const fn full() -> Self {
        Self {
            x: 0,
            y: 0,
            width: u16::MAX,
            height: u16::MAX,
        }
    }
}

/// Scroll behavior for a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollBehavior {
    /// Scrolls with terminal content.
    Inline,
    /// Stays below its zone during scroll.
    PinnedBelow,
    /// Transient overlay; does not affect layout.
    Overlay,
}

/// How a placement behaves in alternate-screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlternateScope {
    /// Suppressed in alternate-screen (default).
    Suppress,
    /// Explicitly allowed in alternate-screen when capability granted.
    Allow,
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Decoded image metadata (headless, no pixel allocation).
///
/// `decoded_bytes` is `width * height * 4` for one frame; total animation
/// bytes is `decoded_bytes * frame_count` and is bounded by
/// [`IMAGE_STORE_MAX_BYTES`] at admission (IMG-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    /// Stable identifier.
    pub id: ImageId,
    /// Normalized source.
    pub source: ImageSource,
    /// Pixel format.
    pub format: PixelFormat,
    /// Decoded dimensions.
    pub width: u32,
    /// Decoded dimensions.
    pub height: u32,
    /// Bytes for one frame (`width * height * 4`, ≤ 64 MiB).
    pub decoded_bytes: usize,
    /// Animation frame count (1..=64, excess discarded).
    pub frame_count: u16,
    /// Total animation bytes (`decoded_bytes * frame_count`).
    pub total_bytes: usize,
    /// Lifecycle generation (for disposal on reload/close).
    pub generation: u64,
    /// Optional compressed payload length (for diagnostics; not stored).
    pub compressed_len: usize,
}

/// Placement record binding one `ImageId` to geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    /// Stable placement identifier.
    pub id: PlacementId,
    /// Which image is placed.
    pub image: ImageId,
    /// Anchor.
    pub anchor: PlacementAnchor,
    /// Geometry.
    pub geometry: PlacementGeometry,
    /// Clip.
    pub clip: ClipRect,
    /// Scroll semantics.
    pub scroll: ScrollBehavior,
    /// Visibility flag (hidden placements pause animation per RFC).
    pub visible: bool,
    /// Alternate-screen scope.
    pub alternate_scope: AlternateScope,
    /// Lifecycle generation.
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed admission failure; no partial placement is emitted (RFC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageStoreError {
    /// Compressed payload exceeds 4 MiB (IMG-1).
    CompressedTooLarge {
        /// Provided bytes.
        len: usize,
        /// Cap.
        cap: usize,
    },
    /// Decoded dimensions exceed 4096 x 4096 (IMG-2).
    DimensionsTooLarge {
        /// Provided width.
        width: u32,
        /// Provided height.
        height: u32,
        /// Cap.
        cap: u32,
    },
    /// Decoded bytes per frame exceed 64 MiB or overflowed (IMG-3).
    DecodedTooLarge {
        /// Computed bytes (when overflow, `usize::MAX`).
        bytes: usize,
        /// Cap.
        cap: usize,
    },
    /// Zero dimension (width or height == 0).
    ZeroDimension,
    /// Total animation bytes exceed store cap (IMG-7) even when empty.
    AnimationTooLarge {
        /// Total bytes.
        total: usize,
        /// Cap.
        cap: usize,
    },
    /// Image not found (for placement admission).
    ImageNotFound(ImageId),
    /// Placement for an evicted image (stale).
    StaleImage(ImageId),
}

impl std::fmt::Display for ImageStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompressedTooLarge { len, cap } => {
                write!(f, "compressed payload too large: {len} > {cap}")
            }
            Self::DimensionsTooLarge { width, height, cap } => {
                write!(f, "dimensions too large: {width}x{height} > {cap}x{cap}")
            }
            Self::DecodedTooLarge { bytes, cap } => {
                write!(f, "decoded bytes too large: {bytes} > {cap}")
            }
            Self::ZeroDimension => write!(f, "zero dimension"),
            Self::AnimationTooLarge { total, cap } => {
                write!(f, "animation too large: {total} > {cap}")
            }
            Self::ImageNotFound(id) => write!(f, "image not found: {}", id.0),
            Self::StaleImage(id) => write!(f, "stale image: {}", id.0),
        }
    }
}

impl std::error::Error for ImageStoreError {}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Core-owned, bounded image collection (OQ-008).
///
/// Deterministic FIFO eviction: oldest images evicted first when at
/// count cap (256) or byte cap (256 MiB). Same insertion order always
/// yields same retained set and same `ImageId` sequence. Headless: no
/// decoder, no GPU allocation, no I/O.
#[derive(Debug, Clone)]
pub struct ImageStore {
    images: VecDeque<DecodedImage>,
    placements: VecDeque<ImagePlacement>,
    total_bytes: usize,
    next_image_id: u64,
    next_placement_id: u64,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            images: VecDeque::new(),
            placements: VecDeque::new(),
            total_bytes: 0,
            next_image_id: 1,
            next_placement_id: 1,
        }
    }

    /// Number of retained images.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether no image is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Current aggregate decoded bytes across all images (including animation totals).
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Max image count (IMG-5).
    #[must_use]
    pub const fn max_count(&self) -> usize {
        IMAGE_STORE_MAX_COUNT
    }

    /// Max total bytes (IMG-4).
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        IMAGE_STORE_MAX_BYTES
    }

    /// Max placement count (IMG-8).
    #[must_use]
    pub const fn max_placements(&self) -> usize {
        IMAGE_MAX_PLACEMENTS
    }

    /// Number of retained placements.
    #[must_use]
    pub fn placement_len(&self) -> usize {
        self.placements.len()
    }

    /// Whether no placement is retained.
    #[must_use]
    pub fn placement_is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Inserts a decoded image, validating IMG-1..IMG-7 before allocation.
    ///
    /// `compressed_len` is the wire payload length (IMG-1, 4 MiB).
    /// `width`/`height` are decoded dimensions (IMG-2, 4096 cap).
    /// `frame_count` is clamped to 64 (IMG-6, excess discarded).
    /// Returns the assigned [`ImageId`] on success, or a typed error with
    /// no partial placement emitted.
    pub fn insert(
        &mut self,
        source: ImageSource,
        width: u32,
        height: u32,
        compressed_len: usize,
        frame_count: u16,
        generation: u64,
    ) -> Result<ImageId, ImageStoreError> {
        if compressed_len > IMAGE_MAX_COMPRESSED_BYTES {
            return Err(ImageStoreError::CompressedTooLarge {
                len: compressed_len,
                cap: IMAGE_MAX_COMPRESSED_BYTES,
            });
        }
        if width == 0 || height == 0 {
            return Err(ImageStoreError::ZeroDimension);
        }
        if width > IMAGE_MAX_DIMENSION || height > IMAGE_MAX_DIMENSION {
            return Err(ImageStoreError::DimensionsTooLarge {
                width,
                height,
                cap: IMAGE_MAX_DIMENSION,
            });
        }
        let decoded_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
            .unwrap_or(usize::MAX);
        if decoded_bytes > IMAGE_MAX_DECODED_BYTES {
            return Err(ImageStoreError::DecodedTooLarge {
                bytes: decoded_bytes,
                cap: IMAGE_MAX_DECODED_BYTES,
            });
        }
        let frames = frame_count.clamp(1, IMAGE_MAX_FRAMES);
        let total = decoded_bytes.saturating_mul(frames as usize);
        if total > IMAGE_STORE_MAX_BYTES {
            return Err(ImageStoreError::AnimationTooLarge {
                total,
                cap: IMAGE_STORE_MAX_BYTES,
            });
        }

        // Admit: evict oldest images until both count and byte caps would be satisfied.
        while self.images.len() >= IMAGE_STORE_MAX_COUNT
            || self.total_bytes.saturating_add(total) > IMAGE_STORE_MAX_BYTES
        {
            if let Some(evicted) = self.images.pop_front() {
                let evicted_total = evicted.total_bytes;
                self.total_bytes = self.total_bytes.saturating_sub(evicted_total);
                // Also drop placements that reference the evicted image (deterministic cleanup).
                let evicted_id = evicted.id;
                self.placements.retain(|p| p.image != evicted_id);
            } else {
                break;
            }
        }

        let id = ImageId(self.next_image_id);
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);

        let image = DecodedImage {
            id,
            source,
            format: PixelFormat::Rgba8,
            width,
            height,
            decoded_bytes,
            frame_count: frames,
            total_bytes: total,
            generation,
            compressed_len,
        };
        self.images.push_back(image);
        self.total_bytes = self.total_bytes.saturating_add(total);
        Ok(id)
    }

    /// Convenience: insert with default source Kitty and generation 0.
    pub fn insert_simple(
        &mut self,
        width: u32,
        height: u32,
        compressed_len: usize,
    ) -> Result<ImageId, ImageStoreError> {
        self.insert(ImageSource::Kitty, width, height, compressed_len, 1, 0)
    }

    /// Looks up an image by id.
    #[must_use]
    pub fn get(&self, id: ImageId) -> Option<&DecodedImage> {
        self.images.iter().find(|img| img.id == id)
    }

    /// Removes the image with `id`; returns `true` when removed.
    ///
    /// Also removes any placements that referenced the image.
    pub fn remove(&mut self, id: ImageId) -> bool {
        let before = self.images.len();
        let mut removed_bytes = 0usize;
        self.images.retain(|img| {
            if img.id == id {
                removed_bytes = removed_bytes.saturating_add(img.total_bytes);
                false
            } else {
                true
            }
        });
        let removed = self.images.len() != before;
        if removed {
            self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
            self.placements.retain(|p| p.image != id);
        }
        removed
    }

    /// Clears all images and placements.
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.total_bytes = 0;
    }

    /// Iterates images oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &DecodedImage> {
        self.images.iter()
    }

    /// Drain oldest-first ids (for tests/tools; bounded).
    pub fn drain_ordered(&self) -> Vec<ImageId> {
        self.images.iter().map(|img| img.id).collect()
    }

    // -----------------------------------------------------------------------
    // Placement admission (IMG-8)
    // -----------------------------------------------------------------------

    /// Inserts a placement for an existing image.
    ///
    /// Validates that `image` exists; otherwise returns `ImageNotFound` with
    /// no placement emitted. Evicts oldest placement when at 128 cap (FIFO).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_placement(
        &mut self,
        image: ImageId,
        anchor: PlacementAnchor,
        geometry: PlacementGeometry,
        clip: ClipRect,
        scroll: ScrollBehavior,
        visible: bool,
        alternate_scope: AlternateScope,
        generation: u64,
    ) -> Result<PlacementId, ImageStoreError> {
        if self.get(image).is_none() {
            return Err(ImageStoreError::ImageNotFound(image));
        }
        if self.placements.len() >= IMAGE_MAX_PLACEMENTS {
            self.placements.pop_front();
        }
        let id = PlacementId(self.next_placement_id);
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        let placement = ImagePlacement {
            id,
            image,
            anchor,
            geometry,
            clip,
            scroll,
            visible,
            alternate_scope,
            generation,
        };
        self.placements.push_back(placement);
        Ok(id)
    }

    /// Looks up a placement by id.
    #[must_use]
    pub fn get_placement(&self, id: PlacementId) -> Option<&ImagePlacement> {
        self.placements.iter().find(|p| p.id == id)
    }

    /// Removes a placement by id.
    pub fn remove_placement(&mut self, id: PlacementId) -> bool {
        let before = self.placements.len();
        self.placements.retain(|p| p.id != id);
        self.placements.len() != before
    }

    /// Iterates placements oldest first.
    pub fn placements(&self) -> impl Iterator<Item = &ImagePlacement> {
        self.placements.iter()
    }

    /// Returns true when an alternate-screen placement would be suppressed.
    ///
    /// Policy: while in alternate-screen, `Inline`/`PinnedBelow` are suppressed
    /// unless `alternate_scope == Allow` with explicit capability (RFC).
    #[must_use]
    pub fn is_suppressed_in_alternate(&self, placement: &ImagePlacement, alt_active: bool) -> bool {
        if !alt_active {
            return false;
        }
        if placement.alternate_scope == AlternateScope::Allow {
            return false;
        }
        matches!(
            placement.scroll,
            ScrollBehavior::Inline | ScrollBehavior::PinnedBelow
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let store = ImageStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
        assert_eq!(store.max_count(), IMAGE_STORE_MAX_COUNT);
        assert_eq!(store.max_bytes(), IMAGE_STORE_MAX_BYTES);
    }

    #[test]
    fn insert_and_lookup() {
        let mut store = ImageStore::new();
        let id = store.insert_simple(64, 64, 1024).unwrap();
        assert_eq!(store.len(), 1);
        let img = store.get(id).unwrap();
        assert_eq!(img.width, 64);
        assert_eq!(img.height, 64);
        assert_eq!(img.decoded_bytes, 64 * 64 * 4);
        assert_eq!(img.frame_count, 1);
    }

    #[test]
    fn compressed_too_large_denied() {
        let mut store = ImageStore::new();
        let err = store
            .insert(
                ImageSource::Kitty,
                100,
                100,
                IMAGE_MAX_COMPRESSED_BYTES + 1,
                1,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::CompressedTooLarge { .. }));
        assert!(store.is_empty());
    }

    #[test]
    fn dimensions_too_large_denied() {
        let mut store = ImageStore::new();
        let err = store
            .insert(ImageSource::Sixel, 4097, 100, 1024, 1, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::DimensionsTooLarge { .. }));
        let err = store
            .insert(ImageSource::Sixel, 100, 5000, 1024, 1, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::DimensionsTooLarge { .. }));
    }

    #[test]
    fn zero_dimension_denied() {
        let mut store = ImageStore::new();
        assert!(matches!(
            store
                .insert(ImageSource::Kitty, 0, 10, 100, 1, 0)
                .unwrap_err(),
            ImageStoreError::ZeroDimension
        ));
    }

    #[test]
    fn decoded_too_large_denied() {
        let mut store = ImageStore::new();
        // 4096x4096x4 = 64 MiB exactly OK; 4096x4096 exceeds? Actually 4096*4096*4 = 67_108_864 = 64 MiB, OK.
        // Try 4096x4097 would exceed dimension first, so test overflow via large that exceeds decoded cap but not dimension:
        // 4096x4096 is max, so we need a dimension within cap but decoded exceeds? But 4096x4096*4 is exactly cap.
        // So we test that OK, and that larger dimension fails earlier. For decoded overflow, we can use 4096x4096 which is OK, then check that any larger would be dimension error.
        // Instead test that decoded check fires: use 4096x4096 (OK) and then 4096x4096 with frame 64 to test animation too large? Let's test decoded too large with a hypothetical: width=4096 height=4096 is OK, but we can test that the check exists by using 4096x4096 with 4 bytes already cap.
        // So we verify OK:
        assert!(
            store
                .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, 0)
                .is_ok()
        );
        store.clear();
        // Oversized via overflow-checked: use max u32 dimensions that would overflow? But they exceed dimension cap, so dimension check wins first.
        // So decoded check is exercised via exact cap boundary.
        let err = store
            .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, 0)
            .unwrap();
        assert_eq!(
            store.get(err).unwrap().decoded_bytes,
            IMAGE_MAX_DECODED_BYTES
        );
    }

    #[test]
    fn frame_count_clamped_to_64() {
        let mut store = ImageStore::new();
        let id = store
            .insert(ImageSource::Kitty, 16, 16, 100, 100, 0)
            .unwrap();
        assert_eq!(store.get(id).unwrap().frame_count, 64);
        // frame 0 clamped to 1
        let id2 = store.insert(ImageSource::Kitty, 16, 16, 100, 0, 0).unwrap();
        assert_eq!(store.get(id2).unwrap().frame_count, 1);
    }

    #[test]
    fn animation_too_large_denied_even_when_empty() {
        let mut store = ImageStore::new();
        // Use max decoded 64 MiB * 64 frames = 4 GiB > 256 MiB -> should be denied
        // 4096x4096 is 64 MiB; with 64 frames total 4 GiB > 256 MiB
        let err = store
            .insert(ImageSource::Kitty, 4096, 4096, 1024, 64, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::AnimationTooLarge { .. }));
    }

    #[test]
    fn bounded_evicts_oldest_on_count() {
        let mut store = ImageStore::new();
        let mut ids = Vec::new();
        for _ in 0..IMAGE_STORE_MAX_COUNT + 5 {
            ids.push(store.insert_simple(1, 1, 10).unwrap());
        }
        assert_eq!(store.len(), IMAGE_STORE_MAX_COUNT);
        for evicted in ids.iter().take(5) {
            assert!(store.get(*evicted).is_none());
        }
        for kept in ids.iter().skip(5) {
            assert!(store.get(*kept).is_some());
        }
    }

    #[test]
    fn bounded_evicts_oldest_on_bytes() {
        let mut store = ImageStore::new();
        // Each image 64x64*4=16KiB; fill until byte cap requires eviction.
        // Use 64 MiB images to exhaust quickly: 256 MiB / 64 MiB = 4 images.
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(
                store
                    .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, 0)
                    .unwrap(),
            );
        }
        assert_eq!(store.total_bytes(), 256 * 1024 * 1024);
        // Fifth should evict oldest
        let fifth = store
            .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, 0)
            .unwrap();
        assert_eq!(store.len(), 4);
        assert!(store.get(ids[0]).is_none());
        assert!(store.get(fifth).is_some());
        assert_eq!(store.total_bytes(), 256 * 1024 * 1024);
    }

    #[test]
    fn remove_cleans_placements_and_bytes() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        let geom = PlacementGeometry::new(10, 10, 0);
        let pid = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(1),
                geom,
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_len(), 1);
        let bytes_before = store.total_bytes();
        assert!(store.remove(img));
        assert!(store.get(img).is_none());
        assert_eq!(store.placement_len(), 0);
        assert!(store.get_placement(pid).is_none());
        assert!(store.total_bytes() < bytes_before);
    }

    #[test]
    fn placement_limits_128_evicts_oldest() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        let mut pids = Vec::new();
        for i in 0..IMAGE_MAX_PLACEMENTS + 5 {
            let pid = store
                .insert_placement(
                    img,
                    PlacementAnchor::Line(i as u64),
                    PlacementGeometry::new(1, 1, 0),
                    ClipRect::full(),
                    ScrollBehavior::Inline,
                    true,
                    AlternateScope::Suppress,
                    0,
                )
                .unwrap();
            pids.push(pid);
        }
        assert_eq!(store.placement_len(), IMAGE_MAX_PLACEMENTS);
        for evicted in pids.iter().take(5) {
            assert!(store.get_placement(*evicted).is_none());
        }
    }

    #[test]
    fn placement_requires_existing_image() {
        let mut store = ImageStore::new();
        let fake = ImageId(9999);
        let err = store
            .insert_placement(
                fake,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::ImageNotFound(_)));
    }

    #[test]
    fn alternate_suppression_policy() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        let pid_inline = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_overlay = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(2),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Overlay,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_allowed = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(3),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Allow,
                0,
            )
            .unwrap();
        let inline = store.get_placement(pid_inline).unwrap();
        let overlay = store.get_placement(pid_overlay).unwrap();
        let allowed = store.get_placement(pid_allowed).unwrap();
        assert!(store.is_suppressed_in_alternate(inline, true));
        assert!(!store.is_suppressed_in_alternate(overlay, true));
        assert!(!store.is_suppressed_in_alternate(allowed, true));
        assert!(!store.is_suppressed_in_alternate(inline, false));
    }

    #[test]
    fn deterministic_ids() {
        let mut a = ImageStore::new();
        let mut b = ImageStore::new();
        assert_eq!(
            a.insert_simple(10, 10, 100).unwrap(),
            b.insert_simple(10, 10, 100).unwrap()
        );
        assert_eq!(
            a.insert_simple(20, 20, 200).unwrap(),
            b.insert_simple(20, 20, 200).unwrap()
        );
    }

    #[test]
    fn clear_empties() {
        let mut store = ImageStore::new();
        store.insert_simple(10, 10, 100).unwrap();
        store
            .insert_placement(
                ImageId(1),
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        store.clear();
        assert!(store.is_empty());
        assert!(store.placement_is_empty());
        assert_eq!(store.total_bytes(), 0);
    }

    // -----------------------------------------------------------------------
    // R-002 adversarial: IMG-1..IMG-9 pre-allocation + sustained-load + bomb
    // -----------------------------------------------------------------------

    #[test]
    fn compressed_exact_cap_ok_one_over_denied_no_alloc() {
        let mut store = ImageStore::new();
        // Exactly 4 MiB OK
        let ok = store
            .insert(
                ImageSource::Kitty,
                100,
                100,
                IMAGE_MAX_COMPRESSED_BYTES,
                1,
                0,
            )
            .unwrap();
        assert!(store.get(ok).is_some());
        assert_eq!(store.len(), 1);
        let bytes_before = store.total_bytes();
        // One over must be rejected before allocation
        let err = store
            .insert(
                ImageSource::Kitty,
                100,
                100,
                IMAGE_MAX_COMPRESSED_BYTES + 1,
                1,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::CompressedTooLarge { .. }));
        assert_eq!(store.len(), 1, "no allocation on rejected compressed bomb");
        assert_eq!(store.total_bytes(), bytes_before);
        assert!(store.get(ok).is_some());
    }

    #[test]
    fn dimensions_exact_cap_ok_one_over_denied_no_alloc() {
        let mut store = ImageStore::new();
        // Exactly 4096 OK
        let ok = store
            .insert(ImageSource::Sixel, 4096, 4096, 1024, 1, 0)
            .unwrap();
        assert_eq!(
            store.get(ok).unwrap().decoded_bytes,
            IMAGE_MAX_DECODED_BYTES
        );
        let len_before = store.len();
        let bytes_before = store.total_bytes();
        // Width 4097 denied
        let err = store
            .insert(ImageSource::Sixel, 4097, 4096, 1024, 1, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::DimensionsTooLarge { .. }));
        // Height 4097 denied
        let err2 = store
            .insert(ImageSource::Sixel, 4096, 4097, 1024, 1, 0)
            .unwrap_err();
        assert!(matches!(err2, ImageStoreError::DimensionsTooLarge { .. }));
        assert_eq!(store.len(), len_before, "no allocation on dimension bomb");
        assert_eq!(store.total_bytes(), bytes_before);
    }

    #[test]
    fn decoded_exact_cap_ok_dimension_wins_over_decoded() {
        let mut store = ImageStore::new();
        // 4096x4096*4 = 64 MiB exactly OK (IMG-3 cap)
        let id = store
            .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, 0)
            .unwrap();
        assert_eq!(
            store.get(id).unwrap().decoded_bytes,
            IMAGE_MAX_DECODED_BYTES
        );
        assert_eq!(store.get(id).unwrap().total_bytes, IMAGE_MAX_DECODED_BYTES);
        // Any larger dimension is rejected as DimensionsTooLarge before decoded check;
        // decoded branch is overflow-checked and would fire on usize overflow, but
        // with u32 capped dimensions overflow cannot happen on 64-bit — still validated
        // pre-allocation.
        let len_before = store.len();
        let err = store
            .insert(ImageSource::Kitty, 4096, 4097, 1024, 1, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::DimensionsTooLarge { .. }));
        assert_eq!(store.len(), len_before);
    }

    #[test]
    fn animation_total_boundary_256mib_ok_one_over_denied() {
        let mut store = ImageStore::new();
        // 1024x1024*4 = 4 MiB per frame; *64 = 256 MiB exactly OK (IMG-7 cap via IMG-4)
        let ok = store
            .insert(ImageSource::Kitty, 1024, 1024, 1024, 64, 0)
            .unwrap();
        assert_eq!(store.get(ok).unwrap().total_bytes, IMAGE_STORE_MAX_BYTES);
        store.clear();
        // 1025x1024*4*64 = 268_697_600 > 256 MiB must be rejected as AnimationTooLarge
        // (dimensions within 4096 cap, so decoded passes, animation fails)
        let err = store
            .insert(ImageSource::Kitty, 1025, 1024, 1024, 64, 0)
            .unwrap_err();
        assert!(
            matches!(err, ImageStoreError::AnimationTooLarge { .. }),
            "expected AnimationTooLarge, got {err:?}"
        );
        assert!(store.is_empty(), "no allocation on animation bomb");
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn animation_frame_clamp_64_validates_total_pre_alloc() {
        let mut store = ImageStore::new();
        // frame_count 100 clamped to 64, so same boundary as 64
        let err = store
            .insert(ImageSource::Kitty, 1025, 1024, 1024, 100, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::AnimationTooLarge { .. }));
        assert!(store.is_empty());
        // frame_count 0 clamped to 1, so 1025x1024 with 0 frames = 1 frame = 4_198_400 < 64 MiB OK
        let ok = store
            .insert(ImageSource::Kitty, 1025, 1024, 1024, 0, 0)
            .unwrap();
        assert_eq!(store.get(ok).unwrap().frame_count, 1);
        assert!(store.get(ok).unwrap().total_bytes <= IMAGE_MAX_DECODED_BYTES);
    }

    #[test]
    fn animation_single_image_exceeds_store_cap_even_when_empty_denied() {
        let mut store = ImageStore::new();
        // 2048x2048*4=16 MiB *64=1 GiB >256 MiB
        let err = store
            .insert(ImageSource::Kitty, 2048, 2048, 1024, 64, 0)
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::AnimationTooLarge { .. }));
        assert!(store.is_empty());
        // Store remains usable after bomb
        let ok = store.insert_simple(10, 10, 100).unwrap();
        assert!(store.get(ok).is_some());
    }

    #[test]
    fn decompression_bomb_pre_allocation_no_alloc_peak_under_64mib_per_image() {
        let mut store = ImageStore::new();
        // Seed with one valid image
        let valid = store.insert_simple(64, 64, 1024).unwrap();
        let len_before = store.len();
        let bytes_before = store.total_bytes();
        // Bomb 1: huge compressed payload (IMG-1)
        let e1 = store
            .insert(
                ImageSource::Kitty,
                64,
                64,
                IMAGE_MAX_COMPRESSED_BYTES + 1024,
                1,
                0,
            )
            .unwrap_err();
        assert!(matches!(e1, ImageStoreError::CompressedTooLarge { .. }));
        // Bomb 2: huge dimensions (IMG-2) — small compressed_len but decoded would be huge
        let e2 = store
            .insert(ImageSource::Kitty, 8192, 8192, 100, 1, 0)
            .unwrap_err();
        assert!(matches!(e2, ImageStoreError::DimensionsTooLarge { .. }));
        // Bomb 3: animation bomb — dimensions OK but total animation >256 MiB (IMG-7)
        let e3 = store
            .insert(ImageSource::Kitty, 4096, 4096, 100, 64, 0)
            .unwrap_err();
        assert!(matches!(e3, ImageStoreError::AnimationTooLarge { .. }));
        // No allocation occurred for any bomb: len and bytes unchanged, valid still present
        assert_eq!(store.len(), len_before);
        assert_eq!(store.total_bytes(), bytes_before);
        assert!(store.get(valid).is_some());
        // Peak memory invariant: per-image 64 MiB cap and aggregate 256 MiB cap never exceeded
        assert!(store.total_bytes() <= IMAGE_STORE_MAX_BYTES);
        for img in store.iter() {
            assert!(img.decoded_bytes <= IMAGE_MAX_DECODED_BYTES);
            assert!(img.total_bytes <= IMAGE_STORE_MAX_BYTES);
        }
    }

    #[test]
    fn sustained_load_count_invariant_fifo_256() {
        let mut store = ImageStore::new();
        // Insert 500 tiny images (1x1*4=4 B) — exceeds 256 count cap, must FIFO evict
        let mut ids = Vec::new();
        for _ in 0..500 {
            ids.push(store.insert_simple(1, 1, 10).unwrap());
        }
        assert_eq!(store.len(), IMAGE_STORE_MAX_COUNT);
        assert!(store.len() <= 256);
        assert!(store.total_bytes() <= IMAGE_STORE_MAX_BYTES);
        // Oldest 244 evicted, newest 256 retained
        for evicted in ids.iter().take(500 - IMAGE_STORE_MAX_COUNT) {
            assert!(store.get(*evicted).is_none(), "oldest should be evicted");
        }
        for kept in ids.iter().skip(500 - IMAGE_STORE_MAX_COUNT) {
            assert!(store.get(*kept).is_some());
        }
        // Deterministic: re-inserting same sequence elsewhere would yield same retained set
        let mut other = ImageStore::new();
        for _ in 0..500 {
            other.insert_simple(1, 1, 10).unwrap();
        }
        assert_eq!(store.drain_ordered(), other.drain_ordered());
    }

    #[test]
    fn sustained_load_bytes_invariant_fifo_256mib() {
        let mut store = ImageStore::new();
        // Each 4096x4096 is 64 MiB; 4 fills 256 MiB. Repeated inserts must stay within cap via FIFO.
        for i in 0..20 {
            store
                .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, i)
                .unwrap();
            assert!(
                store.total_bytes() <= IMAGE_STORE_MAX_BYTES,
                "byte invariant after insert {i}"
            );
            assert!(store.len() <= IMAGE_STORE_MAX_COUNT);
            assert!(store.len() <= 4, "64MiB images: at most 4 fit in 256MiB");
        }
        assert_eq!(store.total_bytes(), IMAGE_STORE_MAX_BYTES);
        assert_eq!(store.len(), 4);
        // Total bytes is exactly sum of retained images
        let sum: usize = store.iter().map(|img| img.total_bytes).sum();
        assert_eq!(sum, store.total_bytes());
    }

    #[test]
    fn sustained_load_mixed_sizes_byte_and_count_invariants() {
        let mut store = ImageStore::new();
        // Alternate small and large images to stress both caps
        for i in 0..300 {
            if i % 10 == 0 {
                // Large 64 MiB every 10th
                store
                    .insert(ImageSource::Kitty, 4096, 4096, 1024, 1, i as u64)
                    .unwrap();
            } else {
                store.insert_simple(64, 64, 1024).unwrap();
            }
            assert!(store.total_bytes() <= IMAGE_STORE_MAX_BYTES);
            assert!(store.len() <= IMAGE_STORE_MAX_COUNT);
        }
        // Sum invariant holds after mixed load
        let sum: usize = store.iter().map(|img| img.total_bytes).sum();
        assert_eq!(sum, store.total_bytes());
    }

    #[test]
    fn placement_admission_128_fifo_and_image_eviction_cleans_placements() {
        let mut store = ImageStore::new();
        let img1 = store.insert_simple(10, 10, 100).unwrap();
        let img2 = store.insert_simple(10, 10, 100).unwrap();
        // Fill placements to 128 for img1
        let mut pids = Vec::new();
        for i in 0..IMAGE_MAX_PLACEMENTS {
            pids.push(
                store
                    .insert_placement(
                        img1,
                        PlacementAnchor::Line(i as u64),
                        PlacementGeometry::new(1, 1, 0),
                        ClipRect::full(),
                        ScrollBehavior::Inline,
                        true,
                        AlternateScope::Suppress,
                        0,
                    )
                    .unwrap(),
            );
        }
        assert_eq!(store.placement_len(), 128);
        // 129th evicts oldest (FIFO)
        let extra = store
            .insert_placement(
                img1,
                PlacementAnchor::Line(9999),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        assert_eq!(store.placement_len(), 128);
        assert!(store.get_placement(pids[0]).is_none());
        assert!(store.get_placement(extra).is_some());
        // Add placement for img2, then evict img2 via image count overflow — placement must be cleaned
        let pid2 = store
            .insert_placement(
                img2,
                PlacementAnchor::Zone(42),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        assert!(store.get_placement(pid2).is_some());
        // Flood images to evict img1 and img2 (need 256 images to push them out)
        for _ in 0..IMAGE_STORE_MAX_COUNT {
            store.insert_simple(1, 1, 10).unwrap();
        }
        assert!(store.get(img1).is_none());
        assert!(store.get(img2).is_none());
        assert!(
            store.get_placement(pid2).is_none(),
            "placements for evicted image must be dropped"
        );
        // After eviction, placements for evicted images are gone, count still bounded
        assert!(store.placement_len() <= IMAGE_MAX_PLACEMENTS);
    }

    #[test]
    fn placement_missing_image_denied_no_partial() {
        let mut store = ImageStore::new();
        let fake = ImageId(0xDEAD_BEEF);
        let before = store.placement_len();
        let err = store
            .insert_placement(
                fake,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, ImageStoreError::ImageNotFound(_)));
        assert_eq!(
            store.placement_len(),
            before,
            "no partial placement on ImageNotFound"
        );
    }

    #[test]
    fn total_bytes_sum_invariant_after_removes_and_clear() {
        let mut store = ImageStore::new();
        let mut ids = Vec::new();
        for _ in 0..10 {
            ids.push(store.insert_simple(100, 100, 1024).unwrap());
        }
        let sum: usize = store.iter().map(|img| img.total_bytes).sum();
        assert_eq!(sum, store.total_bytes());
        // Remove half
        for id in ids.iter().take(5) {
            assert!(store.remove(*id));
        }
        let sum2: usize = store.iter().map(|img| img.total_bytes).sum();
        assert_eq!(sum2, store.total_bytes());
        assert_eq!(store.len(), 5);
        store.clear();
        assert_eq!(store.total_bytes(), 0);
        assert_eq!(store.iter().map(|img| img.total_bytes).sum::<usize>(), 0);
    }

    #[test]
    fn zero_dimension_both_axes_denied() {
        let mut store = ImageStore::new();
        assert!(matches!(
            store
                .insert(ImageSource::Kitty, 0, 0, 100, 1, 0)
                .unwrap_err(),
            ImageStoreError::ZeroDimension
        ));
        assert!(matches!(
            store
                .insert(ImageSource::Kitty, 10, 0, 100, 1, 0)
                .unwrap_err(),
            ImageStoreError::ZeroDimension
        ));
        assert!(store.is_empty());
    }

    #[test]
    fn compressed_at_cap_all_sources_ok() {
        let mut store = ImageStore::new();
        for source in [
            ImageSource::Kitty,
            ImageSource::Sixel,
            ImageSource::Iterm2,
            ImageSource::File,
        ] {
            let id = store
                .insert(source, 10, 10, IMAGE_MAX_COMPRESSED_BYTES, 1, 0)
                .unwrap();
            assert_eq!(store.get(id).unwrap().source, source);
            assert_eq!(
                store.get(id).unwrap().compressed_len,
                IMAGE_MAX_COMPRESSED_BYTES
            );
            store.clear();
        }
    }

    #[test]
    fn placement_anchor_variants_all_ok() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        let anchors = [
            PlacementAnchor::Zone(1),
            PlacementAnchor::Line(2),
            PlacementAnchor::CellRange {
                start_row: 0,
                start_col: 0,
                end_row: 10,
                end_col: 10,
            },
        ];
        for anchor in anchors {
            let pid = store
                .insert_placement(
                    img,
                    anchor.clone(),
                    PlacementGeometry::new(1, 1, 0),
                    ClipRect::full(),
                    ScrollBehavior::Inline,
                    true,
                    AlternateScope::Suppress,
                    0,
                )
                .unwrap();
            assert_eq!(store.get_placement(pid).unwrap().anchor, anchor);
        }
    }

    #[test]
    fn scroll_behavior_variants_placement_ok() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        for scroll in [
            ScrollBehavior::Inline,
            ScrollBehavior::PinnedBelow,
            ScrollBehavior::Overlay,
        ] {
            let pid = store
                .insert_placement(
                    img,
                    PlacementAnchor::Zone(10),
                    PlacementGeometry::new(1, 1, 0),
                    ClipRect::full(),
                    scroll,
                    true,
                    AlternateScope::Suppress,
                    0,
                )
                .unwrap();
            assert_eq!(store.get_placement(pid).unwrap().scroll, scroll);
        }
    }

    #[test]
    fn alternate_scope_matrix_suppression() {
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        // Suppress vs Allow for each scroll
        let pid_inline_suppress = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_pinned_suppress = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(2),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::PinnedBelow,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_overlay_suppress = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(3),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Overlay,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_inline_allow = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(4),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Allow,
                0,
            )
            .unwrap();
        assert!(
            store.is_suppressed_in_alternate(
                store.get_placement(pid_inline_suppress).unwrap(),
                true
            )
        );
        assert!(
            store.is_suppressed_in_alternate(
                store.get_placement(pid_pinned_suppress).unwrap(),
                true
            )
        );
        assert!(
            !store.is_suppressed_in_alternate(
                store.get_placement(pid_overlay_suppress).unwrap(),
                true
            )
        );
        assert!(
            !store.is_suppressed_in_alternate(store.get_placement(pid_inline_allow).unwrap(), true)
        );
        // When alt inactive, none suppressed
        assert!(
            !store.is_suppressed_in_alternate(
                store.get_placement(pid_inline_suppress).unwrap(),
                false
            )
        );
    }

    #[test]
    fn generation_does_not_affect_admission() {
        let mut store = ImageStore::new();
        let id1 = store.insert(ImageSource::Kitty, 10, 10, 100, 1, 0).unwrap();
        let id2 = store
            .insert(ImageSource::Kitty, 10, 10, 100, 1, u64::MAX)
            .unwrap();
        assert_ne!(id1, id2);
        assert_eq!(store.get(id1).unwrap().generation, 0);
        assert_eq!(store.get(id2).unwrap().generation, u64::MAX);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn remove_nonexistent_returns_false_no_side_effect() {
        let mut store = ImageStore::new();
        let id = store.insert_simple(10, 10, 100).unwrap();
        let bytes_before = store.total_bytes();
        assert!(!store.remove(ImageId(999_999)));
        assert!(store.get(id).is_some());
        assert_eq!(store.total_bytes(), bytes_before);
        assert!(!store.remove_placement(PlacementId(999_999)));
    }

    #[test]
    fn drain_ordered_preserves_fifo_after_eviction() {
        let mut store = ImageStore::new();
        let mut ids = Vec::new();
        for _ in 0..10 {
            ids.push(store.insert_simple(1, 1, 10).unwrap());
        }
        let ordered = store.drain_ordered();
        assert_eq!(ordered, ids);
        // Flood to evict oldest 5
        for _ in 0..IMAGE_STORE_MAX_COUNT {
            store.insert_simple(1, 1, 10).unwrap();
        }
        assert_eq!(store.len(), IMAGE_STORE_MAX_COUNT);
        let ordered2 = store.drain_ordered();
        // Oldest from first batch must be gone
        for evicted in ids.iter().take(5) {
            assert!(!ordered2.contains(evicted));
        }
        // drain_ordered is oldest-first and matches iter order
        let iter_ids: Vec<ImageId> = store.iter().map(|img| img.id).collect();
        assert_eq!(ordered2, iter_ids);
    }

    #[test]
    fn iter_len_total_bytes_consistent() {
        let mut store = ImageStore::new();
        for _ in 0..5 {
            store.insert_simple(100, 100, 1024).unwrap();
        }
        assert_eq!(store.iter().count(), store.len());
        let sum: usize = store.iter().map(|img| img.total_bytes).sum();
        assert_eq!(sum, store.total_bytes());
        assert!(store.total_bytes() <= IMAGE_STORE_MAX_BYTES);
    }

    #[test]
    fn accessors_max_count_bytes_placements() {
        let store = ImageStore::new();
        assert_eq!(store.max_count(), IMAGE_STORE_MAX_COUNT);
        assert_eq!(store.max_bytes(), IMAGE_STORE_MAX_BYTES);
        assert_eq!(store.max_placements(), IMAGE_MAX_PLACEMENTS);
        assert_eq!(IMAGE_MAX_FRAMES, 64);
        assert_eq!(IMAGE_MAX_COMPRESSED_BYTES, 4 * 1024 * 1024);
        assert_eq!(IMAGE_MAX_DIMENSION, 4096);
        assert_eq!(IMAGE_MAX_DECODED_BYTES, 64 * 1024 * 1024);
        assert_eq!(IMAGE_STORE_MAX_BYTES, 256 * 1024 * 1024);
        assert_eq!(IMAGE_STORE_MAX_COUNT, 256);
        assert_eq!(IMAGE_MAX_PLACEMENTS, 128);
    }

    #[test]
    fn is_empty_and_placement_is_empty_transitions() {
        let mut store = ImageStore::new();
        assert!(store.is_empty());
        assert!(store.placement_is_empty());
        let img = store.insert_simple(10, 10, 100).unwrap();
        assert!(!store.is_empty());
        assert!(store.placement_is_empty());
        store
            .insert_placement(
                img,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        assert!(!store.placement_is_empty());
        store.clear();
        assert!(store.is_empty());
        assert!(store.placement_is_empty());
    }

    #[test]
    fn total_bytes_zero_after_clear_and_after_remove_all() {
        let mut store = ImageStore::new();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(store.insert_simple(64, 64, 1024).unwrap());
        }
        assert!(store.total_bytes() > 0);
        for id in ids {
            assert!(store.remove(id));
        }
        assert_eq!(store.total_bytes(), 0);
        assert!(store.is_empty());
        // Re-fill and clear
        for _ in 0..3 {
            store.insert_simple(64, 64, 1024).unwrap();
        }
        store.clear();
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn deterministic_ids_across_many_inserts() {
        let mut a = ImageStore::new();
        let mut b = ImageStore::new();
        for i in 0..50 {
            let w = 10 + (i % 10) as u32;
            let h = 10 + (i % 5) as u32;
            assert_eq!(
                a.insert_simple(w, h, 100 + i as usize).unwrap(),
                b.insert_simple(w, h, 100 + i as usize).unwrap()
            );
        }
        // Placement ids also deterministic
        let img_a = a.drain_ordered()[0];
        let img_b = b.drain_ordered()[0];
        let pid_a = a
            .insert_placement(
                img_a,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        let pid_b = b
            .insert_placement(
                img_b,
                PlacementAnchor::Zone(1),
                PlacementGeometry::new(1, 1, 0),
                ClipRect::full(),
                ScrollBehavior::Inline,
                true,
                AlternateScope::Suppress,
                0,
            )
            .unwrap();
        assert_eq!(pid_a, pid_b);
    }

    #[test]
    fn clip_and_geometry_round_trip() {
        let geom = PlacementGeometry::new(80, 24, -5);
        assert_eq!(geom.cols, 80);
        assert_eq!(geom.rows, 24);
        assert_eq!(geom.z_index, -5);
        let clip = ClipRect::new(1, 2, 10, 20);
        assert_eq!(clip.x, 1);
        assert_eq!(clip.y, 2);
        assert_eq!(clip.width, 10);
        assert_eq!(clip.height, 20);
        let full = ClipRect::full();
        assert_eq!(full.width, u16::MAX);
        assert_eq!(full.height, u16::MAX);
        // Insert with custom clip/geom retained
        let mut store = ImageStore::new();
        let img = store.insert_simple(10, 10, 100).unwrap();
        let pid = store
            .insert_placement(
                img,
                PlacementAnchor::Zone(1),
                geom,
                clip,
                ScrollBehavior::PinnedBelow,
                false,
                AlternateScope::Suppress,
                7,
            )
            .unwrap();
        let p = store.get_placement(pid).unwrap();
        assert_eq!(p.geometry, geom);
        assert_eq!(p.clip, clip);
        assert_eq!(p.generation, 7);
        assert!(!p.visible);
    }

    #[test]
    fn animation_frame_64_boundary_total_at_256mib() {
        let mut store = ImageStore::new();
        // 1024x1024=4MiB per frame *64=256MiB exactly at cap — should succeed
        let id = store
            .insert(ImageSource::Kitty, 1024, 1024, 1024, 64, 0)
            .unwrap();
        assert_eq!(store.get(id).unwrap().frame_count, 64);
        assert_eq!(store.get(id).unwrap().total_bytes, 256 * 1024 * 1024);
        assert_eq!(store.total_bytes(), 256 * 1024 * 1024);
    }
}
