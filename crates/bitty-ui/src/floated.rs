//! Workspace-local detached floating panels (CTX-1147, issue #1975).
//!
//! Floating a panel detaches it from tiling: the leaf leaves the layout tree
//! via [`LayoutNode::remove_leaf`](crate::layout::LayoutNode::remove_leaf)
//! (the vacated slot collapses and siblings refill, exactly like the
//! `close_view` tiling-first close) and waits in a workspace-local store,
//! stamped [`PresentationMode::Floating`](crate::presentation::PresentationMode::Floating)
//! through the CW-08 gate. Untoggling re-inserts it as a fresh split via the
//! accepted #1804 balanced placement policy
//! ([`balanced_bisect_choice`](crate::layout::balanced_bisect_choice):
//! largest-area live leaf, physical-aspect
//! [`balanced_split_axis`](crate::layout::balanced_split_axis), `0.5`,
//! after) and stamps it back to `Tiled`. The same [`ViewId`](crate::view::ViewId)
//! is preserved across the round trip, so pane sessions stay keyed
//! uninterrupted. Ghost outlines are structurally impossible: no slot
//! remains while detached, so no slot chrome can paint.
//!
//! Unlike the scratchpad (at most one parked leaf, anchor-preserving show)
//! and unlike the pinned store (window-global sticky floats restored beside
//! a recorded anchor, still floating), this store is workspace-local (one
//! [`FloatStore`] per workspace slot, stashed and loaded on switch) and its
//! restore is a fresh balanced split back to tiling — the original position
//! is deliberately forgotten. That affinity difference is why detached
//! floats do not unify with [`PinnedStore`](crate::pinned::PinnedStore):
//! unifying would leak workspace-local floats across switches (or need an
//! affinity flag conflating two lifetimes), while a separate store keeps
//! each lifetime honest.
//!
//! The store itself never paints and never enters the layout solver: the
//! runtime composites detached leaves over the active workspace scene at
//! present time (same [`OverlayTier::Float`](crate::layout::OverlayTier)
//! tier as pinned leaves, painted before them in detach order), which is
//! what lets siblings refill the container while the float stays visible.
//!
//! # Distinct from `Visibility`
//!
//! The detached state here is storage (the leaf is out of the tree), never a
//! display flag. `Visibility` (computed display state in the `bitty-runtime`
//! registry) is deliberately untouched: this module never reads or writes
//! `Visibility`, and no variant is added anywhere.
//!
//! Deterministic, headless; this module adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::Rect;
use crate::layout::{LayoutNode, balanced_bisect_choice, balanced_split_axis};
use crate::presentation::PresentationMode;
use crate::view::{View, ViewId};

/// Split ratio for the fresh re-insert split (even halves, matching the
/// #1804 quadrant policy).
pub const FLOAT_RESTORE_RATIO: f32 = 0.5;

/// Error for [`FloatStore`] detach/restore/toggle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FloatError {
    /// The leaf is already detached; the tree is untouched.
    AlreadyDetached(ViewId),
    /// The leaf is not detached; the tree is untouched.
    NotDetached(ViewId),
    /// No leaf with this id exists; the tree is untouched.
    LeafNotFound(ViewId),
    /// Only [`PresentationMode::Tiled`] and [`PresentationMode::Floating`]
    /// leaves detach (they converge on `Floating`); `Fullscreen` and
    /// `Scratchpad` have their own commands. The tree is untouched.
    UnsupportedMode { current: PresentationMode },
    /// The [`PresentationMode::can_transition`] gate rejected the stamp;
    /// the detached view is kept (never lost) and the error reported.
    Rejected {
        from: PresentationMode,
        to: PresentationMode,
    },
    /// Detaching would strand an empty live layout (the tree holds a single
    /// leaf); the tree is untouched.
    StrandedLayout,
    /// Toggle needs a target leaf id.
    MissingTarget,
}

impl std::fmt::Display for FloatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyDetached(id) => write!(f, "leaf already detached: {id}"),
            Self::NotDetached(id) => write!(f, "leaf is not detached: {id}"),
            Self::LeafNotFound(id) => write!(f, "float target not found: {id}"),
            Self::UnsupportedMode { current } => {
                write!(f, "float needs a tiled or floating leaf, found {current}")
            }
            Self::Rejected { from, to } => {
                write!(f, "float transition rejected: {from} -> {to}")
            }
            Self::StrandedLayout => f.write_str("float would strand an empty layout"),
            Self::MissingTarget => f.write_str("float toggle needs a target leaf"),
        }
    }
}

impl std::error::Error for FloatError {}

/// Workspace-local detached-float store (CTX-1147, issue #1975).
///
/// Holds every detached [`View`] floating outside the live layout; all
/// transitions are total and deterministic. The store itself never paints
/// and never enters the layout solver: it only stores detached leaves
/// between detach and restore.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FloatStore {
    entries: Vec<View>,
}

impl FloatStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// True when no leaf is detached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of detached leaves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when `id` is detached.
    #[must_use]
    pub fn contains(&self, id: ViewId) -> bool {
        self.entries.iter().any(|view| view.id() == id)
    }

    /// The detached leaf `id`, if detached.
    #[must_use]
    pub fn get(&self, id: ViewId) -> Option<&View> {
        self.entries.iter().find(|view| view.id() == id)
    }

    /// Mutable access to the detached leaf `id`, if detached (present-time
    /// reflow keeps stored dims fresh without re-parenting).
    #[must_use]
    pub fn find_mut(&mut self, id: ViewId) -> Option<&mut View> {
        self.entries.iter_mut().find(|view| view.id() == id)
    }

    /// Detached leaf ids in detach order (present-time paint order).
    #[must_use]
    pub fn ids(&self) -> Vec<ViewId> {
        self.entries.iter().map(View::id).collect()
    }

    /// Discards detached leaf `id` without restoring it to any tree
    /// (issue #2141: dead-float teardown after its child exits).
    ///
    /// Returns the removed [`View`] when present, `None` otherwise. The
    /// tree is never touched.
    pub fn discard(&mut self, id: ViewId) -> Option<View> {
        let pos = self.entries.iter().position(|view| view.id() == id)?;
        Some(self.entries.remove(pos))
    }

    /// Reinstalls a previously captured detached leaf (session restore,
    /// CTX-1147): pushes `view` without touching any layout tree. Detach
    /// order is the restore order.
    ///
    /// The caller validates the snapshot first; like [`Self::detach`] this
    /// stays total and never fails.
    pub fn restore_detached(&mut self, view: View) {
        self.entries.push(view);
    }

    /// Detaches leaf `id` from `tree` and stamps it `Floating`.
    ///
    /// The vacated slot collapses via
    /// [`LayoutNode::remove_leaf`](crate::layout::LayoutNode::remove_leaf),
    /// so siblings refill the container and no slot chrome can ghost.
    /// Tiled leaves converge on `Floating` so one verb detaches either
    /// state (an in-tree `Floating` leaf, e.g. from an unpin, detaches the
    /// same way). Fails with [`FloatError::AlreadyDetached`] (tree
    /// untouched) while the leaf is detached,
    /// [`FloatError::LeafNotFound`] for an unknown id,
    /// [`FloatError::UnsupportedMode`] for `Fullscreen`/`Scratchpad`
    /// leaves (checked before removal, so the tree is untouched), or
    /// [`FloatError::StrandedLayout`] when the tree holds a single leaf
    /// (an empty live layout would strand focus and present).
    pub fn detach(&mut self, tree: &mut LayoutNode, id: ViewId) -> Result<(), FloatError> {
        if self.contains(id) {
            return Err(FloatError::AlreadyDetached(id));
        }
        if tree.leaf_count() <= 1 && tree.leaf_ids().contains(&id) {
            return Err(FloatError::StrandedLayout);
        }
        let current = tree
            .find_leaf(id)
            .map(View::presentation)
            .ok_or(FloatError::LeafNotFound(id))?;
        if !matches!(
            current,
            PresentationMode::Tiled | PresentationMode::Floating
        ) {
            return Err(FloatError::UnsupportedMode { current });
        }
        let Some(mut view) = tree.remove_leaf(id) else {
            return Err(FloatError::LeafNotFound(id));
        };
        if !PresentationMode::request_transition(&mut view, PresentationMode::Floating) {
            Self::insert_fresh(tree, view, &[], Rect::zero());
            return Err(FloatError::Rejected {
                from: current,
                to: PresentationMode::Floating,
            });
        }
        self.entries.push(view);
        Ok(())
    }

    /// Restores detached leaf `id` into `tree` as a fresh split and stamps
    /// it back to `Tiled`.
    ///
    /// Placement follows the accepted #1804 balanced policy over the live
    /// `allocations` (solver order, usually
    /// `tree.layout_with_gaps(container, gaps)`): the largest-area leaf is
    /// split on its physical-aspect
    /// [`balanced_split_axis`](crate::layout::balanced_split_axis) at
    /// [`FLOAT_RESTORE_RATIO`] with the restored leaf after it (right or
    /// down). `container` is the fallback axis source when `allocations`
    /// is empty. An empty tree becomes the restored leaf. A stale target
    /// (anchor closed meanwhile cannot happen here — there is no anchor)
    /// falls back to docking beside the first live leaf; the view is never
    /// lost. Returns the restored leaf id. Fails with
    /// [`FloatError::NotDetached`] when the leaf is not detached.
    pub fn restore(
        &mut self,
        tree: &mut LayoutNode,
        id: ViewId,
        allocations: &[(ViewId, Rect)],
        container: Rect,
    ) -> Result<ViewId, FloatError> {
        let pos = self
            .entries
            .iter()
            .position(|view| view.id() == id)
            .ok_or(FloatError::NotDetached(id))?;
        let mut view = self.entries.remove(pos);
        let from = view.presentation();
        if !PresentationMode::request_transition(&mut view, PresentationMode::Tiled) {
            self.entries.insert(pos.min(self.entries.len()), view);
            return Err(FloatError::Rejected {
                from,
                to: PresentationMode::Tiled,
            });
        }
        let id = view.id();
        Self::insert_fresh(tree, view, allocations, container);
        Ok(id)
    }

    /// Toggles the store: restores `target` when detached, otherwise
    /// detaches it. Returns the restored id on restore, `None` on detach.
    pub fn toggle(
        &mut self,
        tree: &mut LayoutNode,
        target: Option<ViewId>,
        allocations: &[(ViewId, Rect)],
        container: Rect,
    ) -> Result<Option<ViewId>, FloatError> {
        let id = target.ok_or(FloatError::MissingTarget)?;
        if self.contains(id) {
            return self.restore(tree, id, allocations, container).map(Some);
        }
        self.detach(tree, id).map(|()| None)
    }

    /// Puts `view` back into `tree` as a fresh balanced split (or the sole
    /// leaf of an empty tree). Last-resort recovery; never drops the view.
    fn insert_fresh(
        tree: &mut LayoutNode,
        view: View,
        allocations: &[(ViewId, Rect)],
        container: Rect,
    ) {
        if tree.leaf_count() == 0 {
            *tree = LayoutNode::leaf(view);
            return;
        }
        let (target, axis) = balanced_bisect_choice(allocations, container);
        if let Some(sibling) = target {
            if tree.leaf_ids().contains(&sibling)
                && tree.insert_beside(sibling, &view, axis, FLOAT_RESTORE_RATIO, true)
            {
                return;
            }
        }
        // Largest-leaf target gone (or no allocations): dock after the
        // first live leaf on its own balanced axis.
        let first = tree
            .leaf_ids()
            .into_iter()
            .next()
            .expect("non-empty tree has a leaf");
        let first_rect = allocations
            .iter()
            .find(|(leaf_id, _)| *leaf_id == first)
            .map(|(_, rect)| *rect)
            .unwrap_or(container);
        let first_axis = balanced_split_axis(first_rect);
        if !tree.insert_beside(first, &view, first_axis, FLOAT_RESTORE_RATIO, true) {
            // Unreachable for a non-empty tree, but never lose the view:
            // wrap the whole tree in a split carrying it.
            let old = std::mem::replace(tree, LayoutNode::stack(Vec::new()));
            *tree = LayoutNode::split(first_axis, FLOAT_RESTORE_RATIO, old, LayoutNode::leaf(view));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::SplitAxis;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn floating_leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::with_presentation(
            ViewId::new(id),
            40,
            24,
            PresentationMode::Floating,
        ))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    fn triple() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            leaf(1),
            LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(2), leaf(3)),
        )
    }

    fn allocations_of(tree: &LayoutNode, bounds: Rect) -> Vec<(ViewId, Rect)> {
        tree.layout(bounds)
    }

    #[test]
    fn detach_collapses_slot_and_stamps_floating() {
        let mut tree = pair();
        let mut store = FloatStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store
            .detach(&mut tree, ViewId::new(1))
            .expect("detach leaf 1");
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
        assert!(store.contains(ViewId::new(1)));
        assert!(!store.contains(ViewId::new(2)));
        // Leaf detached: the slot collapses to the survivor and the parked
        // copy is stamped Floating through the gate.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
        assert_eq!(
            store.get(ViewId::new(1)).expect("parked").presentation(),
            PresentationMode::Floating
        );
        assert_eq!(store.ids(), vec![ViewId::new(1)]);
        // Siblings refill: the survivor covers the whole container.
        let bounds = Rect::new(0, 0, 80, 24);
        let alloc = tree.layout(bounds);
        assert_eq!(alloc.len(), 1);
        assert_eq!(alloc[0].1, bounds);
    }

    #[test]
    fn detach_tiled_and_floating_converge() {
        let mut tree = pair();
        let mut store = FloatStore::new();
        store
            .detach(&mut tree, ViewId::new(2))
            .expect("detach tiled");
        assert_eq!(
            store.get(ViewId::new(2)).expect("parked").presentation(),
            PresentationMode::Floating
        );

        let mut tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, floating_leaf(1), leaf(2));
        store
            .detach(&mut tree, ViewId::new(1))
            .expect("detach floating");
        assert_eq!(
            store.get(ViewId::new(1)).expect("parked").presentation(),
            PresentationMode::Floating
        );
    }

    #[test]
    fn detach_rejects_without_touching_the_tree() {
        let mut tree = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::with_presentation(
                ViewId::new(1),
                40,
                24,
                PresentationMode::Fullscreen,
            )),
            leaf(2),
        );
        let mut store = FloatStore::new();
        let before = tree.clone();
        assert_eq!(
            store.detach(&mut tree, ViewId::new(1)),
            Err(FloatError::UnsupportedMode {
                current: PresentationMode::Fullscreen
            })
        );
        assert_eq!(tree, before, "rejected detach touches nothing");

        let mut scratch_tree = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::with_presentation(
                ViewId::new(7),
                40,
                24,
                PresentationMode::Scratchpad,
            )),
            leaf(8),
        );
        assert_eq!(
            store.detach(&mut scratch_tree, ViewId::new(7)),
            Err(FloatError::UnsupportedMode {
                current: PresentationMode::Scratchpad
            })
        );

        let mut tree = pair();
        store
            .detach(&mut tree, ViewId::new(1))
            .expect("detach leaf 1");
        assert_eq!(
            store.detach(&mut tree, ViewId::new(1)),
            Err(FloatError::AlreadyDetached(ViewId::new(1)))
        );
        assert_eq!(
            store.detach(&mut tree, ViewId::new(404)),
            Err(FloatError::LeafNotFound(ViewId::new(404)))
        );
    }

    #[test]
    fn detach_refuses_to_strand_a_single_leaf_layout() {
        let mut tree = leaf(1);
        let mut store = FloatStore::new();
        assert_eq!(
            store.detach(&mut tree, ViewId::new(1)),
            Err(FloatError::StrandedLayout)
        );
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1)]);
        assert!(store.is_empty());
    }

    #[test]
    fn restore_reinserts_wide_as_side_by_side() {
        // 80x24 container: physically wide (24 cells * 2.0 aspect = 48 <
        // 80), so the fresh split docks side-by-side after the survivor.
        let mut tree = pair();
        let mut store = FloatStore::new();
        store.detach(&mut tree, ViewId::new(2)).expect("detach");
        let bounds = Rect::new(0, 0, 80, 24);
        let allocations = allocations_of(&tree, bounds);
        let restored = store
            .restore(&mut tree, ViewId::new(2), &allocations, bounds)
            .expect("restore");
        assert_eq!(restored, ViewId::new(2));
        assert!(store.is_empty());
        // Back to tiling, ordered survivor-then-restored, covering the
        // container exactly between them.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert_eq!(
            tree.find_leaf(ViewId::new(2)).expect("leaf").presentation(),
            PresentationMode::Tiled
        );
        let alloc = tree.layout(bounds);
        assert_eq!(alloc.len(), 2);
        let total: u32 = alloc.iter().map(|(_, rect)| u32::from(rect.width)).sum();
        assert_eq!(total, 80);
        assert_eq!(
            store.restore(&mut tree, ViewId::new(2), &[], bounds),
            Err(FloatError::NotDetached(ViewId::new(2)))
        );
    }

    #[test]
    fn restore_reinserts_tall_as_stacked() {
        // Narrow container: the lone survivor is physically tall, so the
        // fresh split stacks vertically.
        let bounds = Rect::new(0, 0, 30, 24);
        let mut tree = LayoutNode::leaf(View::new(ViewId::new(1), 30, 24));
        // Single-leaf trees refuse detach; build the detached state via a
        // pair in a wider scratch tree, then restore into the tall tree.
        let mut scratch = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(9), 30, 24)),
            LayoutNode::leaf(View::new(ViewId::new(2), 30, 24)),
        );
        let mut store = FloatStore::new();
        store.detach(&mut scratch, ViewId::new(2)).expect("detach");
        let allocations = allocations_of(&tree, bounds);
        store
            .restore(&mut tree, ViewId::new(2), &allocations, bounds)
            .expect("restore");
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        // Tall survivor (30 < 24 * 2.0): stacked split.
        let alloc = tree.layout(bounds);
        assert_eq!(alloc.len(), 2);
        let total: u32 = alloc.iter().map(|(_, rect)| u32::from(rect.height)).sum();
        assert_eq!(total, 24);
        assert_eq!(
            alloc[0].1.width, bounds.width,
            "stacked leaves keep full width"
        );
    }

    #[test]
    fn restore_into_empty_tree_becomes_leaf() {
        let mut scratch = pair();
        let mut store = FloatStore::new();
        store.detach(&mut scratch, ViewId::new(1)).expect("detach");
        let mut tree = LayoutNode::stack(Vec::new());
        let bounds = Rect::new(0, 0, 80, 24);
        let restored = store
            .restore(&mut tree, ViewId::new(1), &[], bounds)
            .expect("restore into empty");
        assert_eq!(restored, ViewId::new(1));
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1)]);
        assert_eq!(
            tree.find_leaf(ViewId::new(1)).expect("leaf").presentation(),
            PresentationMode::Tiled
        );
    }

    #[test]
    fn restore_bisects_the_largest_live_leaf() {
        // Triple tree, detach leaf 3: the remaining pair splits 40/40, so
        // the restore bisects the first largest leaf in solver order.
        let mut tree = triple();
        let mut store = FloatStore::new();
        store.detach(&mut tree, ViewId::new(3)).expect("detach");
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        let bounds = Rect::new(0, 0, 80, 24);
        let allocations = allocations_of(&tree, bounds);
        store
            .restore(&mut tree, ViewId::new(3), &allocations, bounds)
            .expect("restore");
        assert_eq!(tree.leaf_count(), 3);
        assert!(tree.leaf_ids().contains(&ViewId::new(3)));
        let alloc = tree.layout(bounds);
        assert_eq!(alloc.len(), 3);
    }

    #[test]
    fn toggle_detaches_then_restores_and_needs_a_target() {
        let mut tree = pair();
        let mut store = FloatStore::new();
        let bounds = Rect::new(0, 0, 80, 24);
        assert_eq!(
            store.toggle(&mut tree, None, &[], bounds),
            Err(FloatError::MissingTarget)
        );
        assert_eq!(
            store
                .toggle(&mut tree, Some(ViewId::new(1)), &[], bounds)
                .expect("toggle to detach"),
            None
        );
        assert!(store.contains(ViewId::new(1)));
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
        let allocations = allocations_of(&tree, bounds);
        assert_eq!(
            store
                .toggle(&mut tree, Some(ViewId::new(1)), &allocations, bounds)
                .expect("toggle to restore"),
            Some(ViewId::new(1))
        );
        assert!(store.is_empty());
        assert_eq!(tree.leaf_count(), 2);
    }

    #[test]
    fn find_mut_reflows_stored_dims() {
        let mut tree = pair();
        let mut store = FloatStore::new();
        store.detach(&mut tree, ViewId::new(1)).expect("detach");
        let view = store.find_mut(ViewId::new(1)).expect("stored");
        assert!(view.reflow_to_rect(Rect::new(0, 0, 10, 8)));
        assert_eq!(store.get(ViewId::new(1)).expect("parked").cols(), 10);
        assert!(store.find_mut(ViewId::new(404)).is_none());
    }

    #[test]
    fn discard_removes_without_touching_tree_issue_2141() {
        let mut tree = pair();
        let mut store = FloatStore::new();
        store.detach(&mut tree, ViewId::new(1)).expect("detach");
        let before = tree.clone();
        let removed = store.discard(ViewId::new(1)).expect("discard returns view");
        assert_eq!(removed.id(), ViewId::new(1));
        assert!(store.is_empty());
        assert!(!store.contains(ViewId::new(1)));
        assert_eq!(tree, before, "discard never touches the tree");
        assert!(store.discard(ViewId::new(1)).is_none());
        assert!(store.discard(ViewId::new(404)).is_none());
    }

    #[test]
    fn multiple_detaches_keep_detach_order() {
        let mut tree = triple();
        let mut store = FloatStore::new();
        store.detach(&mut tree, ViewId::new(3)).expect("detach 3");
        store.detach(&mut tree, ViewId::new(1)).expect("detach 1");
        assert_eq!(store.len(), 2);
        assert_eq!(store.ids(), vec![ViewId::new(3), ViewId::new(1)]);
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
    }
}
