//! Image store/placement: bounded placeholder stub pending OQ-008 decision.
//!
//! The Terminal State RFC places "image protocol placement semantics
//! (OQ-008)" explicitly out of scope, and ADR-0003 records the image-store
//! role for this crate while leaving decoding placement to the future image
//! RFC. This module provides a **bounded, headless-testable placeholder**
//! that downstream crates (render, rich presentation, plugin host) can
//! compile against without requiring actual raster decode.
//!
//! # Drift from `bitty-docs/docs/interfaces/rich-content.md` and OQ-008
//!
//! - `bitty-docs/docs/interfaces/rich-content.md` remains a **draft** (not
//!   accepted). It sketches `RichBlock`/`Image`/`SceneNode` candidates that
//!   are **not** implemented here.
//! - OQ-008 decision remains **open**. No image bytes are decoded, no pixel
//!   allocation tracks untrusted dimensions, and no renderer coupling exists
//!   here. This store holds at most [`IMAGE_STORE_MAX_ENTRIES`] opaque
//!   placeholders, each truncated to [`IMAGE_STORE_MAX_PAYLOAD_BYTES`], with
//!   oldest-first eviction. Placement, animation, and renderer contract
//!   await the image RFC with security review. Until then every entry is
//!   inert for rendering: it affects only bounded bookkeeping and
//!   deterministic tests, never GPU or filesystem state.
//!
//! # Bounds (threats T-01, T-02)
//!
//! - Count cap: [`IMAGE_STORE_MAX_ENTRIES`] (64) entries.
//! - Per-entry payload cap: [`IMAGE_STORE_MAX_PAYLOAD_BYTES`] (4096) bytes,
//!   deterministic truncation — same byte stream always yields same stored
//!   prefix.
//! - Total bytes cap: `entries * 4096` worst case, well inside process
//!   budget and independent of claimed dimensions.
//! - No decoded pixel buffer is allocated here; `lookup` returns only
//!   metadata (`width`/`height` are placeholder 1x1 until decoding exists).

use std::collections::VecDeque;

/// Maximum number of placeholder images retained (bounded per T-01).
pub const IMAGE_STORE_MAX_ENTRIES: usize = 64;

/// Maximum stored payload bytes per placeholder (bounded per T-02).
///
/// Chosen to match [`bitty_vt::BoundedBytes::MAX_LEN`] / [`bitty_vt::BoundedString::MAX_LEN`]
/// so OSC/APC payload truncation is consistent across layers.
pub const IMAGE_STORE_MAX_PAYLOAD_BYTES: usize = 4096;

/// Opaque identity of a stored image, reserved for the OQ-008 design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub(crate) u64);

impl ImageId {
    /// Numeric value (diagnostics and tests only; ordering carries no
    /// semantics until the OQ-008 model exists).
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Placeholder metadata for a stored image.
///
/// No pixel decode has occurred; this is only the bounded, inert record
/// that a future image RFC will replace with a real decoded store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlaceholder {
    /// Stable identifier assigned at insertion.
    pub id: ImageId,
    /// Truncated payload length (0..= [`IMAGE_STORE_MAX_PAYLOAD_BYTES`]).
    pub payload_len: usize,
    /// Truncated payload bytes (bounded). Retained so the stub is
    /// headless-testable without requiring external fixtures.
    pub payload: Box<[u8]>,
    /// Placeholder width in cells (always 1 until placement semantics land).
    pub width_cells: u16,
    /// Placeholder height in cells (always 1 until placement semantics land).
    pub height_cells: u16,
}

/// Bounded placeholder store.
///
/// Headless-testable (`new` → `insert_placeholder` → `lookup` → `clear`)
/// without any decoder, GPU, or I/O dependency. Oldest entry is evicted
/// first when at capacity, preserving bounded memory under untrusted input
/// (threat T-01). The type is deterministic: same insertion order yields
/// same eviction order and same `ImageId` sequence.
#[derive(Debug, Clone)]
pub struct ImageStore {
    entries: VecDeque<ImagePlaceholder>,
    next_id: u64,
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
            entries: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Number of stored placeholders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store holds no placeholders.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Maximum entries this store will retain (capacity bound).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        IMAGE_STORE_MAX_ENTRIES
    }

    /// Inserts a bounded placeholder, truncating `payload` to
    /// [`IMAGE_STORE_MAX_PAYLOAD_BYTES`] and evicting the oldest entry when
    /// at capacity. Returns the assigned [`ImageId`].
    ///
    /// No decoding occurs; the payload is stored inertly for headless tests
    /// only. The same byte stream always yields the same truncation and the
    /// same id sequence for a given insertion history (deterministic).
    pub fn insert_placeholder(&mut self, payload: &[u8]) -> ImageId {
        let id = ImageId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let truncated_len = payload.len().min(IMAGE_STORE_MAX_PAYLOAD_BYTES);
        let placeholder = ImagePlaceholder {
            id,
            payload_len: truncated_len,
            payload: payload[..truncated_len].to_vec().into_boxed_slice(),
            width_cells: 1,
            height_cells: 1,
        };

        if self.entries.len() >= IMAGE_STORE_MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(placeholder);
        id
    }

    /// Looks an id up; returns metadata when present, else `None`.
    #[must_use]
    pub fn lookup(&self, id: ImageId) -> Option<&ImagePlaceholder> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Removes the entry with `id` when present; `true` when removed.
    pub fn remove(&mut self, id: ImageId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    /// Clears all placeholders.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Iterates placeholders oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &ImagePlaceholder> {
        self.entries.iter()
    }

    /// Drain oldest-first iterator (for tests/tools; bounded).
    pub fn drain_ordered(&self) -> Vec<ImageId> {
        self.entries.iter().map(|e| e.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty() {
        let store = ImageStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.lookup(ImageId(1)).is_none());
        assert_eq!(store.capacity(), IMAGE_STORE_MAX_ENTRIES);
    }

    #[test]
    fn insert_and_lookup_roundtrip() {
        let mut store = ImageStore::new();
        let id = store.insert_placeholder(b"hello");
        assert_eq!(store.len(), 1);
        let entry = store.lookup(id).unwrap();
        assert_eq!(entry.payload_len, 5);
        assert_eq!(&*entry.payload, b"hello");
        assert_eq!(entry.width_cells, 1);
    }

    #[test]
    fn payload_truncation_is_deterministic() {
        let mut a = ImageStore::new();
        let mut b = ImageStore::new();
        let long = vec![0xAB_u8; IMAGE_STORE_MAX_PAYLOAD_BYTES + 100];
        let id_a = a.insert_placeholder(&long);
        let id_b = b.insert_placeholder(&long);
        assert_eq!(
            a.lookup(id_a).unwrap().payload_len,
            IMAGE_STORE_MAX_PAYLOAD_BYTES
        );
        assert_eq!(
            b.lookup(id_b).unwrap().payload_len,
            IMAGE_STORE_MAX_PAYLOAD_BYTES
        );
        assert_eq!(
            a.lookup(id_a).unwrap().payload,
            b.lookup(id_b).unwrap().payload
        );
    }

    #[test]
    fn bounded_evicts_oldest() {
        let mut store = ImageStore::new();
        let mut ids = Vec::new();
        for i in 0..IMAGE_STORE_MAX_ENTRIES + 5 {
            ids.push(store.insert_placeholder(&[i as u8]));
        }
        assert_eq!(store.len(), IMAGE_STORE_MAX_ENTRIES);
        // First 5 should have been evicted.
        for evicted in ids.iter().take(5) {
            assert!(store.lookup(*evicted).is_none());
        }
        for kept in ids.iter().skip(5) {
            assert!(store.lookup(*kept).is_some());
        }
    }

    #[test]
    fn remove_clears_entry() {
        let mut store = ImageStore::new();
        let id = store.insert_placeholder(b"x");
        assert!(store.remove(id));
        assert!(store.lookup(id).is_none());
        assert!(!store.remove(id));
    }

    #[test]
    fn clear_empties() {
        let mut store = ImageStore::new();
        store.insert_placeholder(b"a");
        store.insert_placeholder(b"b");
        store.clear();
        assert!(store.is_empty());
    }

    #[test]
    fn deterministic_ids() {
        let mut s1 = ImageStore::new();
        let mut s2 = ImageStore::new();
        let a1 = s1.insert_placeholder(b"one");
        let b1 = s2.insert_placeholder(b"one");
        assert_eq!(a1, b1);
        let a2 = s1.insert_placeholder(b"two");
        let b2 = s2.insert_placeholder(b"two");
        assert_eq!(a2, b2);
    }
}
