//! Streaming Kitty graphics intake (bounded, headless-testable, no decode).
//!
//! OQ-008 decision remains **open**. The image RFC has not landed, so this
//! module performs intake and bounds only: chunked `APC G` payloads are
//! assembled exactly, held inertly, and exposed for headless queries. No
//! base64 decode, no pixel allocation, no placement calculation, and no
//! renderer coupling occur here. Full Kitty rendering stays a separate epic.
//!
//! # Chunk state machine (`m=`)
//!
//! The Kitty graphics protocol streams large images as `m=1` continuation
//! chunks followed by one `m=0` final chunk (Ghostty parity:
//! `graphics_storage.zig`, `graphics_image.zig`). The mapping here is:
//!
//! | Wire | API |
//! |---|---|
//! | first `m=1` chunk (carries the `G` params) | [`KittyGraphicsStub::begin_chunk`] |
//! | middle `m=1` chunk (payload only) | [`KittyGraphicsStub::append_chunk`] with `more = true` |
//! | final `m=0` chunk (payload tail) | [`KittyGraphicsStub::append_chunk`] with `more = false` |
//! | lone `m=0` transmission (no chunking) | [`KittyGraphicsStub::ingest`] (single-shot path) |
//!
//! At most one transmission is in flight per ledger. [`KittyChunkOutcome`]
//! reports `NeedMore` while chunks accumulate and `Completed` with the new
//! [`KittyPlaceholderId`] once the `m=0` tail assembles. [`KittyPlaceholder`]
//! records from the chunked path hold the **exact** assembled bytes
//! (`assembled == true`); the single-shot [`KittyGraphicsStub::ingest`] path
//! keeps its historical truncation at [`KITTY_MAX_PAYLOAD_BYTES`]
//! (`assembled == false`) and is otherwise unchanged.
//!
//! # Bounds (threat T-01/T-02)
//!
//! - Count cap: [`KITTY_MAX_PLACEHOLDERS`] (64) entries, oldest evicted.
//! - Single-shot per-entry cap: [`KITTY_MAX_PAYLOAD_BYTES`] (4096) bytes,
//!   deterministic truncation.
//! - Ledger cap: [`KITTY_LEDGER_MAX_BYTES`] (320 MiB, Ghostty `total_limit`
//!   parity) over **stored + in-flight** bytes. Admission evicts the oldest
//!   entries first (FIFO, Ghostty "prune prior to reserving" pattern); a
//!   single transmission larger than the cap is rejected.
//!
//! # Fail-closed behavior
//!
//! | Condition | Result |
//! |---|---|
//! | `append_chunk` with no open stream (orphan `m=1`/`m=0`) | `Err(Orphan)`, state unchanged |
//! | `begin_chunk` while a stream is open (gap/overlap) | `Err(AlreadyInProgress)`, open stream kept |
//! | chunk growth past the ledger cap | `Err(Oversize)`, in-flight stream dropped, nothing stored |
//! | single transmission larger than the cap | `Err(Oversize)`, nothing stored |
//!
//! Length checks run **before** any buffer growth (`checked_add` against the
//! cap), so a hostile chunk can never force an over-cap allocation.
//!
//! # What is not implemented
//!
//! Placement row/col, z-index, animation, compression, base64 decode,
//! fallback rendering, pixel decoding, and any `APC G`/`DCS` parser
//! integration are all **deferred** to the image RFC / rendering epic. Until
//! then every placeholder is inert for rendering.

use std::collections::VecDeque;

use crate::geometry::{CellMetrics, RectPx};

/// Maximum kitty placeholders retained (matches `IMAGE_STORE_MAX_ENTRIES`).
pub const KITTY_MAX_PLACEHOLDERS: usize = bitty_term_state::IMAGE_STORE_MAX_ENTRIES;

/// Maximum payload bytes per single-shot placeholder (matches
/// `IMAGE_STORE_MAX_PAYLOAD_BYTES` / `BoundedBytes::MAX_LEN`).
///
/// Applies to [`KittyGraphicsStub::ingest`] only. Chunked transmissions
/// assemble exactly up to [`KITTY_LEDGER_MAX_BYTES`].
pub const KITTY_MAX_PAYLOAD_BYTES: usize = bitty_term_state::IMAGE_STORE_MAX_PAYLOAD_BYTES;

/// Maximum total bytes per ledger: stored placeholders plus the in-flight
/// chunk buffer.
///
/// Ghostty `total_limit` parity (`320 * 1000 * 1000`). Use
/// [`KittyGraphicsStub::with_ledger_cap`] to configure a smaller ledger
/// (Kitty allows a configured limit); the default is this constant.
pub const KITTY_LEDGER_MAX_BYTES: usize = 320 * 1000 * 1000;

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
/// Single-shot payloads are the truncated APC bytes as delivered; chunked
/// payloads are the exact assembled bytes. No decode has occurred either
/// way, and `width`/`height` are placeholder 1×1 until the image RFC defines
/// placement semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlaceholder {
    /// Stable handle.
    pub id: KittyPlaceholderId,
    /// Stored payload length. Bounded by [`KITTY_MAX_PAYLOAD_BYTES`] on the
    /// single-shot path, by the ledger cap on the chunked path.
    pub payload_len: usize,
    /// Stored payload bytes (bounded, inert).
    pub payload: Box<[u8]>,
    /// Whether this entry assembled from an `m=` chunk stream (`true`) or
    /// arrived via the single-shot path (`false`).
    pub assembled: bool,
    /// Placeholder width in cells (always 1 in this milestone).
    pub width_cells: u16,
    /// Placeholder height in cells (always 1 in this milestone).
    pub height_cells: u16,
    /// Row where this placeholder was anchored (origin-agnostic; stored as
    /// insertion order until row anchoring is specified).
    pub anchor_row: Option<usize>,
}

/// Outcome of [`KittyGraphicsStub::append_chunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyChunkOutcome {
    /// More chunks are still expected (`m=1`); `buffered` is the total bytes
    /// held in flight. No placeholder was admitted.
    NeedMore {
        /// In-flight buffered bytes after this chunk.
        buffered: usize,
    },
    /// The `m=0` tail completed the stream. The exact assembled payload was
    /// admitted as one placeholder.
    Completed {
        /// Handle of the admitted placeholder.
        id: KittyPlaceholderId,
        /// Exact assembled payload length.
        total_len: usize,
    },
}

/// Typed intake rejection for the `m=` chunk state machine.
///
/// Every variant fails closed: `Orphan` and `AlreadyInProgress` leave the
/// ledger untouched, while `Oversize` drops only the offending in-flight
/// stream and stores nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyChunkError {
    /// `append_chunk` arrived with no open stream (orphan continuation or
    /// orphan final). Ledger unchanged.
    Orphan,
    /// `begin_chunk` arrived while a stream is already open (gap/overlap).
    /// The open stream is kept; abort it explicitly first.
    AlreadyInProgress,
    /// Growth to `needed` bytes would exceed the ledger `cap`. The in-flight
    /// stream (if any) is dropped and nothing is stored.
    Oversize {
        /// Bytes the ledger would have had to hold.
        needed: usize,
        /// Ledger cap that refused them.
        cap: usize,
    },
}

impl std::fmt::Display for KittyChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Orphan => write!(f, "kitty chunk without an open stream"),
            Self::AlreadyInProgress => write!(f, "kitty stream already in flight"),
            Self::Oversize { needed, cap } => {
                write!(
                    f,
                    "kitty stream of {needed} bytes exceeds ledger cap of {cap} bytes"
                )
            }
        }
    }
}

impl std::error::Error for KittyChunkError {}

/// In-flight `m=1` transmission awaiting its `m=0` tail.
#[derive(Debug, Clone)]
struct PendingTransmission {
    buf: Vec<u8>,
    anchor_row: Option<usize>,
}

/// Bounded, deterministic kitty graphics intake (headless).
///
/// Oldest placeholder is evicted when at capacity (FIFO). Single-shot
/// ingestion is deterministic: same payload bytes and same insertion order
/// always yield the same `KittyPlaceholderId` and the same retained set.
/// Chunked ingestion assembles byte-exact payloads under the ledger cap.
#[derive(Debug, Clone)]
pub struct KittyGraphicsStub {
    entries: VecDeque<KittyPlaceholder>,
    next_id: u64,
    pending: Option<PendingTransmission>,
    ledger_cap: usize,
}

impl Default for KittyGraphicsStub {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyGraphicsStub {
    /// An empty ledger with the default [`KITTY_LEDGER_MAX_BYTES`] cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
            pending: None,
            ledger_cap: KITTY_LEDGER_MAX_BYTES,
        }
    }

    /// An empty ledger with a custom total-bytes cap (Kitty parity: the
    /// limit is configurable).
    ///
    /// Intended for embedders with tighter budgets and for tests that must
    /// exercise cap behavior without allocating hundreds of megabytes.
    #[must_use]
    pub fn with_ledger_cap(ledger_cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
            pending: None,
            ledger_cap,
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

    /// Ledger cap (max stored + in-flight bytes).
    #[must_use]
    pub const fn ledger_cap(&self) -> usize {
        self.ledger_cap
    }

    /// Bytes currently held by stored placeholders.
    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.payload_len).sum()
    }

    /// Bytes currently buffered in the in-flight chunk stream (`0` when idle).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.as_ref().map_or(0, |stream| stream.buf.len())
    }

    /// Whether an `m=1` stream is open awaiting more chunks.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Total ledger pressure: stored plus in-flight bytes.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.stored_bytes().saturating_add(self.pending_len())
    }

    /// Ingests a raw APC payload as an inert placeholder (single-shot `m=0`).
    ///
    /// `payload` is truncated to [`KITTY_MAX_PAYLOAD_BYTES`] deterministically.
    /// No base64 decode occurs. Returns the assigned id. `anchor_row` is
    /// optional in this draft; future placement semantics will require an
    /// explicit grid anchor. Oldest entries are evicted first to stay within
    /// the count and ledger caps.
    pub fn ingest(&mut self, payload: &[u8], anchor_row: Option<usize>) -> KittyPlaceholderId {
        let id = KittyPlaceholderId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let truncated_len = payload.len().min(KITTY_MAX_PAYLOAD_BYTES);
        self.evict_to_fit(truncated_len);
        if self.entries.len() >= KITTY_MAX_PLACEHOLDERS {
            self.entries.pop_front();
        }
        self.entries.push_back(KittyPlaceholder {
            id,
            payload_len: truncated_len,
            payload: payload[..truncated_len].to_vec().into_boxed_slice(),
            assembled: false,
            width_cells: 1,
            height_cells: 1,
            anchor_row,
        });
        id
    }

    /// Opens an `m=1` chunk stream with its first payload piece.
    ///
    /// Evicts oldest entries first to reserve `first.len()` bytes under the
    /// ledger cap (FIFO, Ghostty "prune prior to reserving" pattern).
    ///
    /// # Errors
    ///
    /// - [`KittyChunkError::AlreadyInProgress`] when a stream is already
    ///   open; the open stream is kept.
    /// - [`KittyChunkError::Oversize`] when `first` alone exceeds the ledger
    ///   cap; nothing is stored and no stream opens.
    pub fn begin_chunk(
        &mut self,
        first: &[u8],
        anchor_row: Option<usize>,
    ) -> Result<(), KittyChunkError> {
        if self.pending.is_some() {
            return Err(KittyChunkError::AlreadyInProgress);
        }
        if first.len() > self.ledger_cap {
            return Err(KittyChunkError::Oversize {
                needed: first.len(),
                cap: self.ledger_cap,
            });
        }
        self.evict_to_fit(first.len());
        self.pending = Some(PendingTransmission {
            buf: first.to_vec(),
            anchor_row,
        });
        Ok(())
    }

    /// Feeds the next `m=` chunk into the open stream.
    ///
    /// `more == true` buffers the piece and reports
    /// [`KittyChunkOutcome::NeedMore`]; `more == false` treats the piece as
    /// the `m=0` tail, admits the exact assembled payload as one placeholder
    /// (evicting oldest first to fit), and reports
    /// [`KittyChunkOutcome::Completed`].
    ///
    /// Lengths are checked before any buffer growth, so oversize input can
    /// never force an over-cap allocation.
    ///
    /// # Errors
    ///
    /// - [`KittyChunkError::Orphan`] when no stream is open; ledger unchanged.
    /// - [`KittyChunkError::Oversize`] when growth would exceed the ledger
    ///   cap; the in-flight stream is dropped and nothing is stored.
    pub fn append_chunk(
        &mut self,
        next: &[u8],
        more: bool,
    ) -> Result<KittyChunkOutcome, KittyChunkError> {
        if self.pending.is_none() {
            return Err(KittyChunkError::Orphan);
        }
        let cap = self.ledger_cap;
        let needed = self.pending_len().saturating_add(next.len());
        if needed > cap {
            self.pending = None;
            return Err(KittyChunkError::Oversize { needed, cap });
        }
        if !more {
            let stream = self.pending.take().expect("open stream");
            return Ok(self.admit_assembled(stream, next));
        }
        self.evict_to_fit(needed);
        let stream = self.pending.as_mut().expect("open stream");
        stream.buf.extend_from_slice(next);
        Ok(KittyChunkOutcome::NeedMore {
            buffered: stream.buf.len(),
        })
    }

    /// Admits a completed stream's exact bytes as one placeholder.
    ///
    /// The stream was already taken and `tail` length-checked by
    /// `append_chunk`; eviction here only re-checks the fit after the
    /// reservation made while buffering.
    fn admit_assembled(
        &mut self,
        mut stream: PendingTransmission,
        tail: &[u8],
    ) -> KittyChunkOutcome {
        stream.buf.extend_from_slice(tail);
        let total_len = stream.buf.len();
        self.evict_to_fit(total_len);
        if self.entries.len() >= KITTY_MAX_PLACEHOLDERS {
            self.entries.pop_front();
        }
        let id = KittyPlaceholderId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.entries.push_back(KittyPlaceholder {
            id,
            payload_len: total_len,
            payload: stream.buf.into_boxed_slice(),
            assembled: true,
            width_cells: 1,
            height_cells: 1,
            anchor_row: stream.anchor_row,
        });
        KittyChunkOutcome::Completed { id, total_len }
    }

    /// Abandons the open `m=1` stream without storing anything.
    ///
    /// Returns `true` when a stream was actually discarded.
    pub fn abort_chunk(&mut self) -> bool {
        self.pending.take().is_some()
    }

    /// Evicts oldest placeholders until `needed` more bytes fit the ledger.
    ///
    /// No-op for `needed == 0`. Callers guarantee `needed <= ledger_cap`, so
    /// the loop always terminates with room.
    fn evict_to_fit(&mut self, needed: usize) {
        if needed == 0 {
            return;
        }
        while self.stored_bytes().saturating_add(needed) > self.ledger_cap {
            if self.entries.pop_front().is_none() {
                break;
            }
        }
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

    /// Clears all placeholders and abandons any open chunk stream.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending = None;
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
    ///
    /// All arithmetic saturates (CTX-0253 F4): hostile local config can
    /// carry extreme cell metrics, and `anchor_row` is `usize`, so the raw
    /// `as i32` / `as u32` casts they replaced could wrap. Compositors clip
    /// in `i64`; this mirrors that by computing in `u64` and clamping to
    /// the `i32`/`u32` ranges.
    #[must_use]
    pub fn placeholder_rects(&self, metrics: CellMetrics) -> Vec<(KittyPlaceholderId, RectPx)> {
        let mut rects = Vec::new();
        for entry in &self.entries {
            let Some(row) = entry.anchor_row else {
                continue;
            };
            let width = saturating_u32(
                u64::from(entry.width_cells).saturating_mul(u64::from(metrics.width)),
            );
            let height = saturating_u32(
                u64::from(entry.height_cells).saturating_mul(u64::from(metrics.height)),
            );
            let x = 0;
            let y = saturating_i32((row as u64).saturating_mul(u64::from(metrics.height)));
            rects.push((entry.id, RectPx::new(x, y, width, height)));
        }
        rects
    }
}

/// Saturating `u64` -> `i32` (mirrors `kitty_place` and the grid pipeline).
const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

/// Saturating `u64` -> `u32` (mirrors `kitty_place` and the grid pipeline).
const fn saturating_u32(value: u64) -> u32 {
    if value > u32::MAX as u64 {
        u32::MAX
    } else {
        value as u32
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
        assert_eq!(stub.ledger_cap(), KITTY_LEDGER_MAX_BYTES);
        assert!(!stub.has_pending());
        assert_eq!(stub.total_bytes(), 0);
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
        assert!(!entry.assembled);
        assert_eq!(stub.stored_bytes(), 19);
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
    fn placeholder_rects_saturate_on_hostile_config() {
        // CTX-0253 F4: extreme cell metrics (hostile local config) plus a
        // huge anchor row must saturate, never wrap the old `as i32` /
        // `as u32` casts (debug panic / release wrap). Compositors clip in
        // `i64`; the rect clamps to the `i32`/`u32` maxima instead.
        let mut stub = KittyGraphicsStub::new();
        stub.ingest(b"hostile", Some(usize::MAX));
        let metrics = CellMetrics {
            width: u32::MAX,
            height: u32::MAX,
        };
        let rects = stub.placeholder_rects(metrics);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].1.x, 0);
        assert_eq!(rects[0].1.y, i32::MAX);
        assert_eq!(rects[0].1.width, u32::MAX);
        assert_eq!(rects[0].1.height, u32::MAX);
    }

    #[test]
    fn placeholder_rects_saturate_huge_row_normal_metrics() {
        // Row overflow alone (normal metrics) still clamps the `y` origin.
        let mut stub = KittyGraphicsStub::new();
        stub.ingest(b"far", Some(usize::MAX));
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };
        let rects = stub.placeholder_rects(metrics);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].1.y, i32::MAX);
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
        assert_eq!(KITTY_LEDGER_MAX_BYTES, 320 * 1000 * 1000);
    }

    #[test]
    fn chunked_assembly_is_exact() {
        let mut stub = KittyGraphicsStub::new();
        let full: Vec<u8> = (0..5000_u32).map(|i| (i % 251) as u8).collect();
        stub.begin_chunk(&full[..1000], Some(4)).unwrap();
        assert!(stub.has_pending());
        assert_eq!(stub.pending_len(), 1000);
        assert_eq!(stub.len(), 0);

        let outcome = stub.append_chunk(&full[1000..3000], true).unwrap();
        assert_eq!(outcome, KittyChunkOutcome::NeedMore { buffered: 3000 });

        let outcome = stub.append_chunk(&full[3000..], false).unwrap();
        let (id, total_len) = match outcome {
            KittyChunkOutcome::Completed { id, total_len } => (id, total_len),
            KittyChunkOutcome::NeedMore { .. } => panic!("expected completion"),
        };
        assert_eq!(total_len, 5000);
        assert!(!stub.has_pending());
        let entry = stub.get(id).unwrap();
        assert!(entry.assembled);
        assert_eq!(entry.payload_len, 5000);
        assert_eq!(&*entry.payload, &full[..]);
        assert_eq!(entry.anchor_row, Some(4));
        assert_eq!(stub.stored_bytes(), 5000);
    }

    #[test]
    fn chunked_ids_are_deterministic() {
        let chunks: [&[u8]; 3] = [b"alpha-", b"beta-", b"gamma"];
        let run = || {
            let mut stub = KittyGraphicsStub::new();
            stub.begin_chunk(chunks[0], None).unwrap();
            stub.append_chunk(chunks[1], true).unwrap();
            match stub.append_chunk(chunks[2], false).unwrap() {
                KittyChunkOutcome::Completed { id, .. } => {
                    (id, stub.get(id).unwrap().payload.to_vec())
                }
                KittyChunkOutcome::NeedMore { .. } => panic!("expected completion"),
            }
        };
        let (id_a, payload_a) = run();
        let (id_b, payload_b) = run();
        assert_eq!(id_a, id_b);
        assert_eq!(payload_a, b"alpha-beta-gamma");
        assert_eq!(payload_a, payload_b);
    }

    #[test]
    fn orphan_append_fails_closed() {
        let mut stub = KittyGraphicsStub::new();
        assert_eq!(
            stub.append_chunk(b"m=1 orphan", true),
            Err(KittyChunkError::Orphan)
        );
        assert_eq!(
            stub.append_chunk(b"m=0 orphan", false),
            Err(KittyChunkError::Orphan)
        );
        assert!(!stub.has_pending());
        assert_eq!(stub.len(), 0);
        // Ledger still usable afterwards.
        let id = stub.ingest(b"fine", None);
        assert_eq!(&*stub.get(id).unwrap().payload, b"fine");
    }

    #[test]
    fn begin_twice_keeps_open_stream() {
        let mut stub = KittyGraphicsStub::new();
        stub.begin_chunk(b"first-", None).unwrap();
        assert_eq!(
            stub.begin_chunk(b"second-", None),
            Err(KittyChunkError::AlreadyInProgress)
        );
        // Original stream is intact and completes exactly.
        stub.append_chunk(b"middle-", true).unwrap();
        match stub.append_chunk(b"tail", false).unwrap() {
            KittyChunkOutcome::Completed { id, total_len } => {
                assert_eq!(total_len, 17);
                assert_eq!(&*stub.get(id).unwrap().payload, b"first-middle-tail");
            }
            KittyChunkOutcome::NeedMore { .. } => panic!("expected completion"),
        }
    }

    #[test]
    fn oversize_chunk_drops_stream_and_stores_nothing() {
        let mut stub = KittyGraphicsStub::with_ledger_cap(32);
        stub.begin_chunk(b"0123456789", None).unwrap();
        assert_eq!(
            stub.append_chunk(b"012345678901234567890123456789", true),
            Err(KittyChunkError::Oversize {
                needed: 40,
                cap: 32
            })
        );
        assert!(!stub.has_pending());
        assert_eq!(stub.pending_len(), 0);
        assert_eq!(stub.len(), 0);
        // Stub is reusable after the fail-closed drop.
        stub.begin_chunk(b"ok", None).unwrap();
        match stub.append_chunk(b"!", false).unwrap() {
            KittyChunkOutcome::Completed { id, total_len } => {
                assert_eq!(total_len, 3);
                assert_eq!(&*stub.get(id).unwrap().payload, b"ok!");
            }
            KittyChunkOutcome::NeedMore { .. } => panic!("expected completion"),
        }
    }

    #[test]
    fn first_chunk_over_cap_rejected() {
        let mut stub = KittyGraphicsStub::with_ledger_cap(8);
        assert_eq!(
            stub.begin_chunk(b"0123456789", None),
            Err(KittyChunkError::Oversize { needed: 10, cap: 8 })
        );
        assert!(!stub.has_pending());
        assert_eq!(stub.len(), 0);
    }

    #[test]
    fn ledger_evicts_oldest_to_fit_assembled() {
        let mut stub = KittyGraphicsStub::with_ledger_cap(32);
        let old = stub.ingest(b"old-entry-1234567", None);
        assert_eq!(stub.stored_bytes(), 17);
        stub.begin_chunk(b"0123456789", None).unwrap();
        stub.append_chunk(b"ab", true).unwrap();
        match stub.append_chunk(b"cdef", false).unwrap() {
            KittyChunkOutcome::Completed { id, total_len } => {
                assert_eq!(total_len, 16);
                assert_eq!(&*stub.get(id).unwrap().payload, b"0123456789abcdef");
            }
            KittyChunkOutcome::NeedMore { .. } => panic!("expected completion"),
        }
        // 17 + 16 > 32, so the oldest entry was evicted to fit.
        assert!(stub.get(old).is_none());
        assert!(stub.total_bytes() <= 32);
    }

    #[test]
    fn abort_discards_stream() {
        let mut stub = KittyGraphicsStub::new();
        assert!(!stub.abort_chunk());
        stub.begin_chunk(b"partial-", None).unwrap();
        assert!(stub.abort_chunk());
        assert!(!stub.has_pending());
        assert!(!stub.abort_chunk());
        assert_eq!(
            stub.append_chunk(b"tail", false),
            Err(KittyChunkError::Orphan)
        );
        assert_eq!(stub.len(), 0);
    }

    #[test]
    fn clear_abandons_open_stream() {
        let mut stub = KittyGraphicsStub::new();
        stub.begin_chunk(b"partial-", None).unwrap();
        stub.clear();
        assert!(!stub.has_pending());
        assert_eq!(
            stub.append_chunk(b"tail", false),
            Err(KittyChunkError::Orphan)
        );
    }

    #[test]
    fn chunk_error_display() {
        assert_eq!(
            KittyChunkError::Orphan.to_string(),
            "kitty chunk without an open stream"
        );
        assert_eq!(
            KittyChunkError::AlreadyInProgress.to_string(),
            "kitty stream already in flight"
        );
        assert_eq!(
            KittyChunkError::Oversize {
                needed: 40,
                cap: 32
            }
            .to_string(),
            "kitty stream of 40 bytes exceeds ledger cap of 32 bytes"
        );
    }
}
