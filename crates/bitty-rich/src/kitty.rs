//! Kitty graphics protocol stub (bounded, headless-testable, no decode).
//!
//! OQ-008 decision remains **open**. The image RFC has not landed, so this
//! module provides only an inert placeholder store that mirrors the bounds
//! of `bitty_term_state::image::ImageStore` (64 entries, 4096 bytes each,
//! oldest eviction). No base64 decode, no pixel allocation, no placement
//! calculation, and no renderer coupling occur here.
//!
//! # What is implemented
//!
//! - A bounded FIFO of [`KittyPlaceholder`] records, each holding a
//!   truncated APC payload prefix and placeholder dimensions (1×1 cells).
//! - Headless ingestion via [`KittyGraphicsStub::ingest`] that truncates
//!   payloads deterministically (`same bytes → same prefix → same id` when
//!   insertion order is equal).
//! - Pure queries (`get`, `iter`, `len`) for tests and a future
//!   `presentation` layer that can turn placeholders into [`RectPx`] via
//!   supplied [`CellMetrics`].
//!
//! # What is not implemented
//!
//! Placement row/col, z-index, animation, chunked transmission (`m=`),
//! compression, fallback rendering, and any `APC G`/`DCS` parser
//! integration are all **deferred** to the image RFC. Until then every
//! placeholder is inert for rendering.

use std::collections::VecDeque;

use crate::geometry::{CellMetrics, RectPx};

/// Maximum kitty placeholders retained (matches `IMAGE_STORE_MAX_ENTRIES`).
pub const KITTY_MAX_PLACEHOLDERS: usize = bitty_term_state::IMAGE_STORE_MAX_ENTRIES;

/// Maximum payload bytes per placeholder (matches
/// `IMAGE_STORE_MAX_PAYLOAD_BYTES` / `BoundedBytes::MAX_LEN`).
pub const KITTY_MAX_PAYLOAD_BYTES: usize = bitty_term_state::IMAGE_STORE_MAX_PAYLOAD_BYTES;

/// Opaque handle for a kitty placeholder (mirrors `ImageId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KittyPlaceholderId(pub(crate) u64);

impl KittyPlaceholderId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Inert placeholder for a kitty graphics payload.
///
/// Payload is the truncated APC bytes as delivered; no decode has occurred
/// and `width`/`height` are placeholder 1×1 until the image RFC defines
/// placement semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlaceholder {
    /// Stable handle.
    pub id: KittyPlaceholderId,
    /// Truncated payload length.
    pub payload_len: usize,
    /// Truncated payload bytes (bounded, inert).
    pub payload: Box<[u8]>,
    /// Placeholder width in cells (always 1 in this milestone).
    pub width_cells: u16,
    /// Placeholder height in cells (always 1 in this milestone).
    pub height_cells: u16,
    /// Row where this placeholder was anchored (origin-agnostic; stored as
    /// insertion order until row anchoring is specified).
    pub anchor_row: Option<usize>,
}

/// Bounded, deterministic kitty graphics stub (headless).
///
/// Oldest placeholder is evicted when at capacity (FIFO). Ingestion is
/// deterministic: same payload bytes and same insertion order always yield
/// the same `KittyPlaceholderId` and the same retained set.
#[derive(Debug, Clone)]
pub struct KittyGraphicsStub {
    entries: VecDeque<KittyPlaceholder>,
    next_id: u64,
}

impl Default for KittyGraphicsStub {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyGraphicsStub {
    /// An empty stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Number of retained placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no placeholder is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Capacity bound (max retained placeholders).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        KITTY_MAX_PLACEHOLDERS
    }

    /// Ingests a raw APC payload as an inert placeholder.
    ///
    /// `payload` is truncated to [`KITTY_MAX_PAYLOAD_BYTES`] deterministically.
    /// No base64 decode occurs. Returns the assigned id. `anchor_row` is
    /// optional in this draft; future placement semantics will require an
    /// explicit grid anchor.
    pub fn ingest(&mut self, payload: &[u8], anchor_row: Option<usize>) -> KittyPlaceholderId {
        let id = KittyPlaceholderId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let truncated_len = payload.len().min(KITTY_MAX_PAYLOAD_BYTES);
        let placeholder = KittyPlaceholder {
            id,
            payload_len: truncated_len,
            payload: payload[..truncated_len].to_vec().into_boxed_slice(),
            width_cells: 1,
            height_cells: 1,
            anchor_row,
        };

        if self.entries.len() >= KITTY_MAX_PLACEHOLDERS {
            self.entries.pop_front();
        }
        self.entries.push_back(placeholder);
        id
    }

    /// Looks up a placeholder by id.
    #[must_use]
    pub fn get(&self, id: KittyPlaceholderId) -> Option<&KittyPlaceholder> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Removes the placeholder with `id`; `true` when removed.
    pub fn remove(&mut self, id: KittyPlaceholderId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Clears all placeholders.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Iterates placeholders oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &KittyPlaceholder> {
        self.entries.iter()
    }

    /// Headless overlay geometry for placeholders that have an anchor row.
    ///
    /// Each anchored placeholder maps to one [`RectPx`] at
    /// `(0, anchor_row * cell.height)` with size `width_cells * width` by
    /// `height_cells * height` (always 1×1 in this draft). Unanchored
    /// placeholders contribute no rectangle — placement is deferred.
    #[must_use]
    pub fn placeholder_rects(&self, metrics: CellMetrics) -> Vec<(KittyPlaceholderId, RectPx)> {
        let mut rects = Vec::new();
        for entry in &self.entries {
            let Some(row) = entry.anchor_row else {
                continue;
            };
            let width = u32::from(entry.width_cells) * metrics.width;
            let height = u32::from(entry.height_cells) * metrics.height;
            let x = 0;
            let y = (row as u64 * u64::from(metrics.height)) as i32;
            rects.push((entry.id, RectPx::new(x, y, width, height)));
        }
        rects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_term_state::image::{IMAGE_STORE_MAX_ENTRIES, IMAGE_STORE_MAX_PAYLOAD_BYTES};

    #[test]
    fn new_is_empty() {
        let stub = KittyGraphicsStub::new();
        assert!(stub.is_empty());
        assert_eq!(stub.len(), 0);
        assert_eq!(stub.capacity(), IMAGE_STORE_MAX_ENTRIES);
    }

    #[test]
    fn ingest_and_lookup() {
        let mut stub = KittyGraphicsStub::new();
        let id = stub.ingest(b"APC G fancy payload", Some(2));
        assert_eq!(stub.len(), 1);
        let entry = stub.get(id).unwrap();
        assert_eq!(entry.payload_len, 19);
        assert_eq!(&*entry.payload, b"APC G fancy payload");
        assert_eq!(entry.anchor_row, Some(2));
    }

    #[test]
    fn truncation_is_deterministic() {
        let mut a = KittyGraphicsStub::new();
        let mut b = KittyGraphicsStub::new();
        let long = vec![0xAB_u8; IMAGE_STORE_MAX_PAYLOAD_BYTES + 77];
        let id_a = a.ingest(&long, None);
        let id_b = b.ingest(&long, None);
        assert_eq!(
            a.get(id_a).unwrap().payload_len,
            IMAGE_STORE_MAX_PAYLOAD_BYTES
        );
        assert_eq!(
            b.get(id_b).unwrap().payload_len,
            IMAGE_STORE_MAX_PAYLOAD_BYTES
        );
        assert_eq!(a.get(id_a).unwrap().payload, b.get(id_b).unwrap().payload);
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn bounded_evicts_oldest() {
        let mut stub = KittyGraphicsStub::new();
        let mut ids = Vec::new();
        for i in 0..KITTY_MAX_PLACEHOLDERS + 5 {
            ids.push(stub.ingest(&[i as u8], None));
        }
        assert_eq!(stub.len(), KITTY_MAX_PLACEHOLDERS);
        for evicted in ids.iter().take(5) {
            assert!(stub.get(*evicted).is_none());
        }
        for kept in ids.iter().skip(5) {
            assert!(stub.get(*kept).is_some());
        }
    }

    #[test]
    fn remove_and_clear() {
        let mut stub = KittyGraphicsStub::new();
        let id = stub.ingest(b"x", None);
        assert!(stub.remove(id));
        assert!(stub.get(id).is_none());
        assert!(!stub.remove(id));
        stub.ingest(b"a", None);
        stub.ingest(b"b", None);
        stub.clear();
        assert!(stub.is_empty());
    }

    #[test]
    fn placeholder_rects_only_for_anchored() {
        let mut stub = KittyGraphicsStub::new();
        stub.ingest(b"unanchored", None);
        stub.ingest(b"anchored", Some(3));
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let rects = stub.placeholder_rects(metrics);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].1.y, 3 * 16);
        assert_eq!(rects[0].1.width, 8);
        assert_eq!(rects[0].1.height, 16);
    }

    #[test]
    fn deterministic_ids() {
        let mut a = KittyGraphicsStub::new();
        let mut b = KittyGraphicsStub::new();
        assert_eq!(a.ingest(b"one", None), b.ingest(b"one", None));
        assert_eq!(a.ingest(b"two", None), b.ingest(b"two", None));
    }

    #[test]
    fn caps_match_term_state_image_store() {
        assert_eq!(KITTY_MAX_PLACEHOLDERS, IMAGE_STORE_MAX_ENTRIES);
        assert_eq!(KITTY_MAX_PAYLOAD_BYTES, IMAGE_STORE_MAX_PAYLOAD_BYTES);
    }
}
