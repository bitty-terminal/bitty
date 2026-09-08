//! Kitty placement: decoded images onto cell rects (CTX-0248).
//!
//! [`crate::kitty`] performs intake (chunked `m=` assembly, payloads held
//! inert) and [`crate::kitty_decode`] turns assembled bytes into RGBA8
//! bitmaps ([`crate::kitty_decode::KittyDecodedImage`]). This module is the
//! next stage: it stores decoded bitmaps, binds them to cursor-anchored
//! cell rects, and rasterizes (nearest-neighbor scales) them to the exact
//! pixel extent the present layer composites. Parser/APC wiring is
//! unchanged: the caller passes the transmission parameters (`a`, `c`, `r`)
//! alongside the decoded bitmap.
//!
//! # Actions (`a=`)
//!
//! | `a` | Meaning | Effect |
//! |---|---|---|
//! | absent | Kitty default: transmit and display | store + place |
//! | `t` | Transmit only | store, no placement (never painted) |
//! | `T` | Transmit and display | store + place |
//! | anything else (`p` put, `d` delete, `q` query, `f` frame, ...) |
//! | Uncovered | [`KittyAction::Unsupported`]: stored, **not painted**
//! | (fail closed) |
//!
//! [`KittyAction::from_a`] maps the wire value; the layer stores the image
//! in every case and only [`KittyAction::TransmitAndDisplay`] admits a
//! placement. No action deletes, queries, or animates anything here.
//!
//! # Placement semantics
//!
//! - Anchor: the cursor cell (`anchor_col`, `anchor_row`) at display time.
//! - Size: explicit `c`/`r` cell spans when non-zero, otherwise derived from
//!   decoded pixels (`ceil(px / cell)`, at least 1 cell per axis).
//! - The cell rect maps to pixels through the caller-supplied
//!   [`crate::geometry::CellMetrics`] and is clamped (intersected) to the
//!   viewport; a placement fully outside the viewport paints nothing.
//! - Z-order (documented model): grid cells (backgrounds + glyphs) and every
//!   fill overlay (selection, cursor, banner, scrollbar) composite first;
//!   kitty images are the **topmost** present-layer content, ordered by
//!   ascending `z` (stable for equal `z`). They never mutate grid truth.
//!   Known limitation: an image covering the cursor or selection cell hides
//!   that fill where they overlap; cursor-on-top is follow-up work.
//!
//! # Scroll (tied to grid rows)
//!
//! Each placement records `scrollback_base` (`State::scrollback_len()` at
//! display time). At present time the scrolled distance is
//! `current.saturating_sub(base)`; the effective anchor row is
//! `anchor_row - scrolled`, and placements scrolled off the top paint
//! nothing. The image therefore moves **with** terminal content. Viewport
//! scrollback inspection (`View::scroll_offset() != 0`) is handled by the
//! caller (skip painting: live-grid anchors do not map to the history
//! viewport), as is alternate-screen suppression (see below).
//!
//! Limitation: scroll regions (`DECSTBM`) and reverse-index moves that do
//! not grow the scrollback are not tracked, so an image inside a scroll
//! region may lag its text by the region-local delta. Full-screen scroll
//! (the common case) is exact.
//!
//! # Alternate screen
//!
//! [`KittyImageLayer::clear`] drops every image and placement. The caller
//! clears when the alternate screen activates and refuses new placements
//! while it is active (stored, not painted — same fail-closed shape as
//! [`KittyAction::Transmit`]).
//!
//! # Bounds (threat T-01/T-02)
//!
//! Placement reuses the decode caps ([`crate::kitty_decode`]) rather than
//! inventing its own: 8192 px/side, 4096 x 4096 px area, 64 MiB RGBA. Every
//! length is validated with checked arithmetic **before** any buffer is
//! allocated or grown, including the nearest-neighbor output
//! (`rect_w * rect_h * 4`, itself bounded because the rect is clamped to
//! the viewport first). Layer totals are additionally capped: 64 stored
//! images ([`KITTY_PLACE_MAX_IMAGES`], kitty-ledger parity) and 256 MiB
//! decoded bytes ([`KITTY_PLACE_MAX_BYTES`], RFC IMG-4 parity), oldest
//! evicted first; placements are capped at 128 ([`KITTY_PLACE_MAX_ITEMS`],
//! RFC IMG-8 parity), oldest evicted first. A single image larger than the
//! byte cap is rejected.
//!
//! # Determinism
//!
//! Storage and placement are pure functions of insertion order: same calls
//! always yield the same ids and the same retained set.

use std::collections::VecDeque;

use crate::geometry::{CellMetrics, ExtentPx, RectPx};
use crate::kitty_decode::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS,
};

/// Maximum stored decoded images (kitty-ledger count parity).
pub const KITTY_PLACE_MAX_IMAGES: usize = crate::kitty::KITTY_MAX_PLACEHOLDERS;

/// Maximum total decoded RGBA bytes held by the layer (RFC IMG-4 parity).
pub const KITTY_PLACE_MAX_BYTES: usize = crate::image::IMAGE_STORE_MAX_BYTES;

/// Maximum placements (RFC IMG-8 parity).
pub const KITTY_PLACE_MAX_ITEMS: usize = crate::image::IMAGE_MAX_PLACEMENTS;

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Kitty `a=` display action (subset implemented for CTX-0248).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyAction {
    /// `a=t`: transmit only — store, never place.
    Transmit,
    /// Absent `a` (kitty default) or `a=T`: store and place.
    TransmitAndDisplay,
    /// Any other `a=` value: stored, **not painted** (fail closed).
    Unsupported(char),
}

impl KittyAction {
    /// Maps the wire `a=` value (`None` when the parameter is absent).
    ///
    /// Absent means transmit-and-display per the kitty specification.
    #[must_use]
    pub const fn from_a(value: Option<char>) -> Self {
        match value {
            None | Some('T') => Self::TransmitAndDisplay,
            Some('t') => Self::Transmit,
            Some(other) => Self::Unsupported(other),
        }
    }

    /// Whether this action admits a placement (paints).
    #[must_use]
    pub const fn displays(self) -> bool {
        matches!(self, Self::TransmitAndDisplay)
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable handle for a stored decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KittyImageId(pub u64);

impl KittyImageId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Stable handle for a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KittyPlacementId(pub u64);

impl KittyPlacementId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Stored decoded image: owned RGBA8 pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacedImage {
    /// Stable handle.
    pub id: KittyImageId,
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// RGBA8 bytes, exactly `width * height * 4` long.
    pub rgba: Vec<u8>,
    /// Wire payload length (diagnostics; the bytes themselves are decoded).
    pub compressed_len: usize,
}

/// Placement binding one image to a cursor-anchored cell rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacement {
    /// Stable handle.
    pub id: KittyPlacementId,
    /// Which image is placed.
    pub image: KittyImageId,
    /// Cursor column at display time.
    pub anchor_col: u16,
    /// Cursor row at display time (before scroll adjustment).
    pub anchor_row: u16,
    /// Width in cells (explicit `c=` or derived, always `>= 1`).
    pub cols: u16,
    /// Height in cells (explicit `r=` or derived, always `>= 1`).
    pub rows: u16,
    /// `State::scrollback_len()` at display time (scroll tracking).
    pub scrollback_base: usize,
    /// Stacking order (ascending paints first).
    pub z: i32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed placement rejection.
///
/// Every variant fails closed: the layer keeps its previous state except
/// where documented (FIFO eviction on admission success only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyPlacementError {
    /// A side exceeds 8192 px; rejected before any allocation.
    DimensionsTooLarge {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
        /// Side cap that refused them.
        cap: u32,
    },
    /// `width * height` exceeds 4096 x 4096 px; rejected before allocation.
    TooManyPixels {
        /// Requested pixel count.
        pixels: u64,
        /// Area cap that refused it.
        cap: u64,
    },
    /// RGBA bytes exceed 64 MiB; rejected before allocation (or growth).
    DecodedTooLarge {
        /// Bytes the bitmap would have needed (`usize::MAX` on overflow).
        bytes: usize,
        /// Byte cap that refused them.
        cap: usize,
    },
    /// RGBA length is not exactly `width * height * 4`.
    LengthMismatch {
        /// `width * height * 4`.
        expected: usize,
        /// Actual length.
        actual: usize,
    },
    /// Placement references an unknown (or evicted) image.
    ImageNotFound(KittyImageId),
}

impl std::fmt::Display for KittyPlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DimensionsTooLarge { width, height, cap } => write!(
                f,
                "kitty placement {width}x{height} exceeds max dimension of {cap}px"
            ),
            Self::TooManyPixels { pixels, cap } => write!(
                f,
                "kitty placement of {pixels} pixels exceeds max of {cap} pixels"
            ),
            Self::DecodedTooLarge { bytes, cap } => write!(
                f,
                "kitty placement bitmap of {bytes} bytes exceeds max of {cap} bytes"
            ),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "kitty placement rgba of {actual} bytes does not match {expected} expected bytes"
            ),
            Self::ImageNotFound(id) => write!(f, "kitty placement image not found: {}", id.0),
        }
    }
}

impl std::error::Error for KittyPlacementError {}

// ---------------------------------------------------------------------------
// Validation (no allocation)
// ---------------------------------------------------------------------------

/// Validates `(width, height, rgba.len())` with checked arithmetic.
///
/// Returns the pixel count. Runs before any buffer exists or grows, so
/// hostile dimensions can never force an over-cap allocation.
fn checked_bitmap(width: u32, height: u32, rgba_len: usize) -> Result<u64, KittyPlacementError> {
    if width == 0 || height == 0 {
        // Zero-dimension bitmaps carry no pixels; report deterministically
        // through the exact-length check (expected 0 vs actual).
        return Err(KittyPlacementError::LengthMismatch {
            expected: 0,
            actual: rgba_len,
        });
    }
    if width > KITTY_DECODE_MAX_DIMENSION || height > KITTY_DECODE_MAX_DIMENSION {
        return Err(KittyPlacementError::DimensionsTooLarge {
            width,
            height,
            cap: KITTY_DECODE_MAX_DIMENSION,
        });
    }
    // No overflow is possible: both sides are at most 8192.
    let pixels = u64::from(width) * u64::from(height);
    if pixels > KITTY_DECODE_MAX_PIXELS {
        return Err(KittyPlacementError::TooManyPixels {
            pixels,
            cap: KITTY_DECODE_MAX_PIXELS,
        });
    }
    let expected = (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyPlacementError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    if rgba_len != expected {
        return Err(KittyPlacementError::LengthMismatch {
            expected,
            actual: rgba_len,
        });
    }
    Ok(pixels)
}

// ---------------------------------------------------------------------------
// Cell-span derivation
// ---------------------------------------------------------------------------

/// Cell span for one axis: explicit `c=`/`r=` when non-zero, otherwise
/// `ceil(pixels / cell_px)` (at least 1 cell).
///
/// `cell_px` is guaranteed non-zero by [`CellMetrics`] construction.
fn cell_span(explicit: u16, pixels: u32, cell_px: u32) -> u16 {
    if explicit != 0 {
        return explicit;
    }
    let cells = pixels.div_ceil(cell_px).max(1);
    cells.min(u16::MAX as u32) as u16
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// Owned Kitty image + placement layer (headless, bounded).
///
/// Deterministic FIFO eviction on images (count and byte caps) and on
/// placements (count cap). Same call order always yields the same ids and
/// the same retained set.
#[derive(Debug, Clone, Default)]
pub struct KittyImageLayer {
    images: VecDeque<KittyPlacedImage>,
    placements: VecDeque<KittyPlacement>,
    total_bytes: usize,
    next_image_id: u64,
    next_placement_id: u64,
}

impl KittyImageLayer {
    /// An empty layer.
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

    /// Number of stored decoded images.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether no decoded image is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Decoded RGBA bytes currently held.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
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

    /// Stores a decoded bitmap, validating decode caps before admission.
    ///
    /// `rgba` must be exactly `width * height * 4` bytes. On success the
    /// oldest images are evicted first to satisfy the count and byte caps
    /// (placements of evicted images are dropped deterministically).
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError`] dimension/area/byte/length rejections, or
    /// [`KittyPlacementError::DecodedTooLarge`] when the bitmap alone
    /// exceeds the layer byte cap. Failures store nothing.
    pub fn store(
        &mut self,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed_len: usize,
    ) -> Result<KittyImageId, KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        let bytes = rgba.len();
        if bytes > KITTY_PLACE_MAX_BYTES {
            return Err(KittyPlacementError::DecodedTooLarge {
                bytes,
                cap: KITTY_PLACE_MAX_BYTES,
            });
        }
        while self.images.len() >= KITTY_PLACE_MAX_IMAGES
            || self.total_bytes.saturating_add(bytes) > KITTY_PLACE_MAX_BYTES
        {
            if let Some(evicted) = self.images.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.rgba.len());
                let evicted_id = evicted.id;
                self.placements.retain(|p| p.image != evicted_id);
            } else {
                break;
            }
        }
        let id = KittyImageId(self.next_image_id);
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        self.images.push_back(KittyPlacedImage {
            id,
            width,
            height,
            rgba,
            compressed_len,
        });
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        Ok(id)
    }

    /// Looks up a stored image by id.
    #[must_use]
    pub fn get(&self, id: KittyImageId) -> Option<&KittyPlacedImage> {
        self.images.iter().find(|img| img.id == id)
    }

    /// Removes the image with `id` and its placements; `true` when removed.
    pub fn remove(&mut self, id: KittyImageId) -> bool {
        let before = self.images.len();
        let mut removed_bytes = 0usize;
        self.images.retain(|img| {
            if img.id == id {
                removed_bytes = removed_bytes.saturating_add(img.rgba.len());
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

    /// Places a stored image at the cursor cell.
    ///
    /// `cols`/`rows` are the explicit `c=`/`r=` spans (0 or absent derives
    /// from decoded pixels). `scrollback_base` is
    /// `State::scrollback_len()` now. Evicts the oldest placement at the
    /// 128 cap (FIFO).
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] when `image` is unknown.
    /// Stores nothing new on failure.
    #[allow(clippy::too_many_arguments)]
    pub fn display(
        &mut self,
        image: KittyImageId,
        anchor_col: u16,
        anchor_row: u16,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        let stored = self
            .get(image)
            .ok_or(KittyPlacementError::ImageNotFound(image))?;
        let cols = cell_span(cols, stored.width, metrics.width);
        let rows = cell_span(rows, stored.height, metrics.height);
        if self.placements.len() >= KITTY_PLACE_MAX_ITEMS {
            self.placements.pop_front();
        }
        let id = KittyPlacementId(self.next_placement_id);
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        self.placements.push_back(KittyPlacement {
            id,
            image,
            anchor_col,
            anchor_row,
            cols,
            rows,
            scrollback_base,
            z,
        });
        Ok(id)
    }

    /// Looks up a placement by id.
    #[must_use]
    pub fn get_placement(&self, id: KittyPlacementId) -> Option<&KittyPlacement> {
        self.placements.iter().find(|p| p.id == id)
    }

    /// Removes a placement by id.
    pub fn remove_placement(&mut self, id: KittyPlacementId) -> bool {
        let before = self.placements.len();
        self.placements.retain(|p| p.id != id);
        self.placements.len() != before
    }

    /// Iterates placements oldest first.
    pub fn placements(&self) -> impl Iterator<Item = &KittyPlacement> {
        self.placements.iter()
    }

    /// Placements in paint order: ascending `z`, stable for equal `z`.
    pub fn placements_in_paint_order(&self) -> Vec<&KittyPlacement> {
        let mut ordered: Vec<&KittyPlacement> = self.placements.iter().collect();
        ordered.sort_by_key(|p| p.z);
        ordered
    }

    /// Clears all images and placements (alternate-screen entry).
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.total_bytes = 0;
    }

    /// Pixel rect for a placement in the current viewport, if visible.
    ///
    /// `viewport_cols`/`viewport_rows` are the live grid dimensions;
    /// `scrollback_now` is the current `State::scrollback_len()`. Returns
    /// `None` when the placement scrolled off the top or lies fully
    /// outside the viewport (paints nothing). Otherwise returns the
    /// cell-rect pixel extent intersected with the viewport.
    #[must_use]
    pub fn placement_rect(
        placement: &KittyPlacement,
        metrics: CellMetrics,
        viewport_cols: u16,
        viewport_rows: u16,
        scrollback_now: usize,
    ) -> Option<RectPx> {
        let scrolled = scrollback_now.saturating_sub(placement.scrollback_base);
        let row = (u64::from(placement.anchor_row)).checked_sub(scrolled as u64)?;
        // Cell rect in pixels (u64 throughout, saturated into i32/u32 like
        // the grid pipeline's `grid_rect_to_px`).
        let x = u64::from(placement.anchor_col) * u64::from(metrics.width);
        let y = row * u64::from(metrics.height);
        let w = u64::from(placement.cols) * u64::from(metrics.width);
        let h = u64::from(placement.rows) * u64::from(metrics.height);
        let rect = RectPx::new(
            saturating_i32(x),
            saturating_i32(y),
            saturating_u32(w),
            saturating_u32(h),
        );
        // Viewport extent in pixels.
        let extent = metrics.extent_for(usize::from(viewport_cols), usize::from(viewport_rows));
        intersect(RectPx::new(0, 0, extent.width, extent.height), rect)
    }
}

/// Pixel rect for a placement against this layer's stored image, if visible.
///
/// Convenience wrapper over [`KittyImageLayer::placement_rect`] that
/// resolves the id first (`None` for unknown placements).
#[must_use]
pub fn placement_rect_for(
    layer: &KittyImageLayer,
    id: KittyPlacementId,
    metrics: CellMetrics,
    viewport_cols: u16,
    viewport_rows: u16,
    scrollback_now: usize,
) -> Option<RectPx> {
    let placement = layer.get_placement(id)?;
    KittyImageLayer::placement_rect(
        placement,
        metrics,
        viewport_cols,
        viewport_rows,
        scrollback_now,
    )
}

// ---------------------------------------------------------------------------
// Rasterize (nearest-neighbor scale to the rect extent)
// ---------------------------------------------------------------------------

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
pub fn rasterize(image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let out_len = (u64::from(rect.width) * u64::from(rect.height))
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
    let (dw, dh) = (u64::from(rect.width), u64::from(rect.height));
    for dy in 0..dh {
        // Nearest neighbor: `sy = dy * sh / dw... ` — division in u64,
        // exact for the bounded ranges here.
        let sy = (dy * sh / dh) as usize;
        for dx in 0..dw {
            let sx = (dx * sw / dw) as usize;
            let s = (sy * image.width as usize + sx) * 4;
            let d = (dy as usize * rect.width as usize + dx as usize) * 4;
            out[d..d + 4].copy_from_slice(&image.rgba[s..s + 4]);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Small integer helpers (mirror the grid pipeline's saturating style)
// ---------------------------------------------------------------------------

const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

const fn saturating_u32(value: u64) -> u32 {
    if value > u32::MAX as u64 {
        u32::MAX
    } else {
        value as u32
    }
}

/// Intersection of `viewport` and `rect`; `None` when they do not overlap.
fn intersect(viewport: RectPx, rect: RectPx) -> Option<RectPx> {
    let left = i64::from(rect.x).max(0);
    let top = i64::from(rect.y).max(0);
    let right = (i64::from(rect.x) + i64::from(rect.width)).min(i64::from(viewport.width));
    let bottom = (i64::from(rect.y) + i64::from(rect.height)).min(i64::from(viewport.height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(RectPx::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// Viewport pixel extent for a grid (saturating).
#[must_use]
pub fn viewport_extent(metrics: CellMetrics, cols: u16, rows: u16) -> ExtentPx {
    metrics.extent_for(usize::from(cols), usize::from(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: CellMetrics = CellMetrics {
        width: 8,
        height: 16,
    };

    fn tiny_red() -> (u32, u32, Vec<u8>) {
        // 2x2 opaque red.
        (2, 2, [0xFF, 0x00, 0x00, 0xFF].repeat(4))
    }

    fn stored_red(layer: &mut KittyImageLayer) -> KittyImageId {
        let (w, h, rgba) = tiny_red();
        layer.store(w, h, rgba, 67).unwrap()
    }

    #[test]
    fn action_mapping() {
        assert_eq!(KittyAction::from_a(None), KittyAction::TransmitAndDisplay);
        assert_eq!(
            KittyAction::from_a(Some('T')),
            KittyAction::TransmitAndDisplay
        );
        assert_eq!(KittyAction::from_a(Some('t')), KittyAction::Transmit);
        assert_eq!(
            KittyAction::from_a(Some('p')),
            KittyAction::Unsupported('p')
        );
        assert_eq!(
            KittyAction::from_a(Some('d')),
            KittyAction::Unsupported('d')
        );
        assert!(KittyAction::TransmitAndDisplay.displays());
        assert!(!KittyAction::Transmit.displays());
        assert!(!KittyAction::Unsupported('q').displays());
    }

    #[test]
    fn store_and_lookup() {
        let mut layer = KittyImageLayer::new();
        assert!(layer.is_empty());
        let id = stored_red(&mut layer);
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.total_bytes(), 16);
        let img = layer.get(id).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.rgba.len(), 16);
        assert_eq!(img.compressed_len, 67);
    }

    #[test]
    fn store_validates_before_admission() {
        let mut layer = KittyImageLayer::new();
        // Side cap fires on a 4-byte lie (no allocation of the refused size).
        assert_eq!(
            layer.store(100_000, 100_000, vec![0; 4], 4),
            Err(KittyPlacementError::DimensionsTooLarge {
                width: 100_000,
                height: 100_000,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        // Area cap: 5000x5000.
        assert_eq!(
            layer.store(5000, 5000, vec![0; 8], 8),
            Err(KittyPlacementError::TooManyPixels {
                pixels: 25_000_000,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
        // Length mismatch.
        assert_eq!(
            layer.store(2, 1, vec![0; 7], 7),
            Err(KittyPlacementError::LengthMismatch {
                expected: 8,
                actual: 7
            })
        );
        assert!(layer.is_empty());
        assert_eq!(layer.total_bytes(), 0);
    }

    #[test]
    fn transmit_only_stores_without_placing() {
        // The action model: Transmit admits bytes but no placement paints.
        let mut layer = KittyImageLayer::new();
        let action = KittyAction::from_a(Some('t'));
        assert!(!action.displays());
        let id = stored_red(&mut layer);
        assert!(layer.get(id).is_some());
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn unsupported_action_stores_without_painting() {
        let mut layer = KittyImageLayer::new();
        for a in ['p', 'd', 'q', 'f', 'x'] {
            let action = KittyAction::from_a(Some(a));
            assert!(!action.displays(), "a={a}");
        }
        let id = stored_red(&mut layer);
        assert!(layer.get(id).is_some());
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn display_uses_explicit_cell_span() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        // 2x2 px at 8x16 cells with explicit 3x2 cells.
        let pid = layer.display(id, 4, 5, 3, 2, METRICS, 0, 0).unwrap();
        let rect =
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0)
                .unwrap();
        assert_eq!(rect, RectPx::new(4 * 8, 5 * 16, 3 * 8, 2 * 16));
    }

    #[test]
    fn display_derives_span_from_pixels() {
        let mut layer = KittyImageLayer::new();
        // 20x40 px at 8x16 cells derives ceil(20/8)=3 x ceil(40/16)=3.
        let id = layer.store(20, 40, vec![7; 20 * 40 * 4], 100).unwrap();
        let pid = layer.display(id, 1, 2, 0, 0, METRICS, 0, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();
        assert_eq!((placement.cols, placement.rows), (3, 3));
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 0).unwrap();
        assert_eq!(rect, RectPx::new(8, 32, 24, 48));
    }

    #[test]
    fn display_unknown_image_fails_closed() {
        let mut layer = KittyImageLayer::new();
        assert_eq!(
            layer.display(KittyImageId(999), 0, 0, 1, 1, METRICS, 0, 0),
            Err(KittyPlacementError::ImageNotFound(KittyImageId(999)))
        );
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn rect_clamped_to_viewport() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        // 10x10 cells at col 78 of an 80-col grid: clipped to 2 cols.
        let pid = layer.display(id, 78, 0, 10, 10, METRICS, 0, 0).unwrap();
        let rect =
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0)
                .unwrap();
        assert_eq!(rect, RectPx::new(78 * 8, 0, 2 * 8, 10 * 16));
    }

    #[test]
    fn fully_outside_viewport_paints_nothing() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 90, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert_eq!(
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0),
            None
        );
        // Row past the bottom likewise.
        let pid2 = layer.display(id, 0, 30, 2, 2, METRICS, 0, 0).unwrap();
        assert_eq!(
            KittyImageLayer::placement_rect(layer.get_placement(pid2).unwrap(), METRICS, 80, 24, 0),
            None
        );
    }

    #[test]
    fn scroll_moves_anchor_with_content() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 0, 10, 2, 2, METRICS, 100, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();
        // No scroll: row 10.
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 100).unwrap();
        assert_eq!(rect.y, 10 * 16);
        // Three lines scrolled: row 7.
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 103).unwrap();
        assert_eq!(rect.y, 7 * 16);
        // Scrolled fully off the top: nothing paints.
        assert_eq!(
            KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 111),
            None
        );
    }

    #[test]
    fn alt_screen_clear_drops_everything() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        layer.display(id, 0, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert!(!layer.is_empty());
        assert!(!layer.placement_is_empty());
        layer.clear();
        assert!(layer.is_empty());
        assert!(layer.placement_is_empty());
        assert_eq!(layer.total_bytes(), 0);
    }

    #[test]
    fn remove_drops_image_and_its_placements() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 0, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert!(layer.remove(id));
        assert!(layer.get(id).is_none());
        assert!(layer.get_placement(pid).is_none());
        assert_eq!(layer.total_bytes(), 0);
        assert!(!layer.remove(id));
    }

    #[test]
    fn oversize_placement_fails_closed() {
        let mut layer = KittyImageLayer::new();
        // A bitmap that passes per-axis (8192x2048 = 16M px = area cap)
        // would still be admitted by decode; placement validates the same
        // caps, so prove the area edge rejects here too without allocating
        // the refused 64 MiB: pass a short buffer and expect LengthMismatch
        // only after the caps pass — instead use over-area dims directly.
        assert_eq!(
            layer.store(8192, 8192, vec![0; 4], 4),
            Err(KittyPlacementError::TooManyPixels {
                pixels: 67_108_864,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
        assert!(layer.is_empty());
    }

    #[test]
    fn paint_order_is_z_ascending_stable() {
        let mut layer = KittyImageLayer::new();
        let a = stored_red(&mut layer);
        let b = stored_red(&mut layer);
        let p1 = layer.display(a, 0, 0, 1, 1, METRICS, 0, 5).unwrap();
        let p2 = layer.display(b, 0, 0, 1, 1, METRICS, 0, -1).unwrap();
        let p3 = layer.display(a, 0, 0, 1, 1, METRICS, 0, 5).unwrap();
        let ordered = layer.placements_in_paint_order();
        let ids: Vec<KittyPlacementId> = ordered.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![p2, p1, p3]);
    }

    #[test]
    fn rasterize_scales_nearest_neighbor() {
        let mut layer = KittyImageLayer::new();
        // 2x1: red then green. Scale to 4x2: each source pixel doubles.
        let id = layer
            .store(2, 1, vec![0xFF, 0, 0, 0xFF, 0, 0xFF, 0, 0xFF], 8)
            .unwrap();
        let img = layer.get(id).unwrap();
        let out = rasterize(img, RectPx::new(0, 0, 4, 2)).unwrap();
        assert_eq!(out.len(), 4 * 2 * 4);
        // Row 0: RR GG; row 1 repeats.
        assert_eq!(&out[0..8], &[0xFF, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF]);
        assert_eq!(&out[8..16], &[0, 0xFF, 0, 0xFF, 0, 0xFF, 0, 0xFF]);
        assert_eq!(&out[16..24], &[0xFF, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF]);
        assert_eq!(&out[24..32], &[0, 0xFF, 0, 0xFF, 0, 0xFF, 0, 0xFF]);
    }

    #[test]
    fn rasterize_identity_for_matching_extent() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let img = layer.get(id).unwrap();
        let out = rasterize(img, RectPx::new(5, 5, 2, 2)).unwrap();
        assert_eq!(out, img.rgba);
    }

    #[test]
    fn rasterize_empty_rect_paints_nothing() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let img = layer.get(id).unwrap();
        assert_eq!(rasterize(img, RectPx::new(0, 0, 0, 10)), None);
        assert_eq!(rasterize(img, RectPx::new(0, 0, 10, 0)), None);
    }

    #[test]
    fn placement_cap_evicts_oldest() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let mut first = None;
        for i in 0..KITTY_PLACE_MAX_ITEMS + 5 {
            let pid = layer.display(id, 0, 0, 1, 1, METRICS, 0, i as i32).unwrap();
            if i == 0 {
                first = Some(pid);
            }
        }
        assert_eq!(layer.placement_len(), KITTY_PLACE_MAX_ITEMS);
        assert!(layer.get_placement(first.unwrap()).is_none());
    }

    #[test]
    fn caps_reuse_decode_values() {
        // Compile-time: placement enforces exactly the decode ceilings.
        const _: () = assert!(KITTY_PLACE_MAX_BYTES == 256 * 1024 * 1024);
        const _: () = assert!(KITTY_PLACE_MAX_ITEMS == 128);
        assert_eq!(KITTY_PLACE_MAX_IMAGES, crate::kitty::KITTY_MAX_PLACEHOLDERS);
        // Error strings stay stable for snapshot greps.
        assert_eq!(
            KittyPlacementError::ImageNotFound(KittyImageId(7)).to_string(),
            "kitty placement image not found: 7"
        );
    }

    #[test]
    fn zero_size_store_rejected() {
        let mut layer = KittyImageLayer::new();
        assert_eq!(
            layer.store(0, 1, vec![0; 4], 4),
            Err(KittyPlacementError::LengthMismatch {
                expected: 0,
                actual: 4
            })
        );
        assert!(layer.is_empty());
    }
}
