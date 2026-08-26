//! Image store/placement: typed stub pending OQ-008.
//!
//! The Terminal State RFC places "image protocol placement semantics
//! (OQ-008)" explicitly out of scope, and ADR-0003 records the image-store
//! role for this crate while leaving decoding placement to the future image
//! RFC. This module therefore provides only the typed seam that downstream
//! crates (render, plugin host) compile against; no storage, decode, or
//! placement behavior exists yet, and none is claimed.
//!
//! When the image RFC under OQ-008 lands, its accepted store/placement
//! model becomes the sole implementation behind these types; until then any
//! action or environment input touching images is semantically inert here.

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

/// Placeholder image store.
///
/// Always empty in this milestone: [`ImageStore::len`] returns zero and
/// [`ImageStore::lookup`] never resolves. The type exists so dependent
/// crates have a stable, honest seam instead of inventing their own.
#[derive(Debug, Clone, Default)]
pub struct ImageStore {
    /// Reserved so a future non-trivial representation stays
    /// source-compatible for construction paths routed through `new`.
    _private: (),
}

impl ImageStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored images; always `0` pending OQ-008.
    #[must_use]
    pub fn len(&self) -> usize {
        0
    }

    /// Whether the store holds no images; always `true` pending OQ-008.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        true
    }

    /// Looks an id up; always `None` pending OQ-008.
    #[must_use]
    pub fn lookup(&self, _id: ImageId) -> Option<()> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_store_is_always_empty() {
        let store = ImageStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.lookup(ImageId(1)).is_none());
    }
}
