//! Pinned (sticky) floating panels across workspaces (CTX-1077, issue #1757).
//!
//! A pinned leaf is parked outside the layout tree in a window-global store
//! (one [`PinnedStore`] instance per window, mirroring the [`ScratchpadSlot`]
//! storage discipline). Pinning detaches the leaf via
//! [`LayoutNode::remove_leaf`](crate::layout::LayoutNode::remove_leaf) and
//! stamps it [`PresentationMode::Floating`](crate::presentation::PresentationMode::Floating)
//! through the CW-08 gate; unpinning restores it beside its recorded anchor
//! via [`LayoutNode::insert_beside`](crate::layout::LayoutNode::insert_beside)
//! and keeps it `Floating` (unpin returns the floating panel to the currently
//! active workspace; it does not re-tile it). Toggling routes through the
//! workspace command registry as `bitty.workspace:pin-toggle` (see
//! [`PIN_CMD_TOGGLE`] and [`apply_pin_toggle`]).
//!
//! Unlike the scratchpad (at most one parked leaf), the store holds every
//! pinned leaf: each pin consumes an existing live leaf, so the live-leaf
//! population already bounds the store and no extra cap is introduced here.
//!
//! The store itself never paints and never enters the layout solver: the
//! runtime composites parked leaves over the active workspace scene at
//! present time (same [`OverlayTier::Float`](crate::layout::OverlayTier)
//! tier as mode-floating leaves, painted after them in stable pin order),
//! which is what lets one leaf stay visible across workspace switches
//! without ever living in two trees at once.
//!
//! # Distinct from `Visibility`
//!
//! The pinned state here is storage (the leaf is out of the tree), never a
//! display flag. `Visibility` (computed display state in the `bitty-runtime`
//! registry) is deliberately untouched: this module never reads or writes
//! `Visibility`, and no variant is added anywhere.
//!
//! Deterministic, headless; this module adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::SplitAxis;
use crate::layout::LayoutNode;
use crate::presentation::PresentationMode;
use crate::view::{View, ViewId};

/// Workspace command toggling the pinned-leaf store.
///
/// `<owner>.<name>:<command>` grammar (see
/// [`QualifiedCommand`](crate::panel::QualifiedCommand)); routed by
/// [`apply_pin_toggle`]. With `target = Some(id)` a pinned leaf unpins back
/// into the tree and an unpinned floating (or tiled) leaf pins; with
/// `target = None` the call fails with [`PinnedError::MissingTarget`].
pub const PIN_CMD_TOGGLE: &str = "bitty.workspace:pin-toggle";

/// Error for [`PinnedStore`] pin/unpin/toggle and [`apply_pin_toggle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinnedError {
    /// The leaf is already pinned; the tree is untouched.
    AlreadyPinned(ViewId),
    /// The leaf is not pinned; the tree is untouched.
    NotPinned(ViewId),
    /// No leaf with this id exists; the tree is untouched.
    LeafNotFound(ViewId),
    /// Only [`PresentationMode::Tiled`] and [`PresentationMode::Floating`]
    /// leaves pin (they converge on `Floating`); `Fullscreen` and
    /// `Scratchpad` have their own commands. The tree is untouched.
    UnsupportedMode { current: PresentationMode },
    /// The [`PresentationMode::can_transition`] gate rejected the stamp;
    /// the leaf is restored to its prior position.
    Rejected {
        from: PresentationMode,
        to: PresentationMode,
    },
    /// Pinning would strand an empty live layout (the tree holds a single
    /// leaf); the tree is untouched.
    StrandedLayout,
    /// Toggle-to-pin needs a target leaf id.
    MissingTarget,
    /// `command` is not [`PIN_CMD_TOGGLE`].
    UnknownCommand(String),
}

impl std::fmt::Display for PinnedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyPinned(id) => write!(f, "leaf already pinned: {id}"),
            Self::NotPinned(id) => write!(f, "leaf is not pinned: {id}"),
            Self::LeafNotFound(id) => write!(f, "pin target not found: {id}"),
            Self::UnsupportedMode { current } => {
                write!(f, "pin needs a tiled or floating leaf, found {current}")
            }
            Self::Rejected { from, to } => {
                write!(f, "pin transition rejected: {from} -> {to}")
            }
            Self::StrandedLayout => f.write_str("pin would strand an empty layout"),
            Self::MissingTarget => f.write_str("pin toggle needs a target leaf"),
            Self::UnknownCommand(cmd) => write!(f, "unknown pin command: {cmd}"),
        }
    }
}

impl std::error::Error for PinnedError {}

/// Parked leaf plus its restore anchor: the neighboring leaf id at pin
/// time and which side the parked leaf returns to (`after == false` puts
/// the restored leaf before the anchor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedEntry {
    view: View,
    anchor: Option<ViewId>,
    after: bool,
}

impl PinnedEntry {
    /// The parked leaf (stamped `Floating`).
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The neighboring leaf id recorded at pin time, if any.
    #[must_use]
    pub fn anchor(&self) -> Option<ViewId> {
        self.anchor
    }

    /// Which side of the anchor the leaf returns to (`false` restores
    /// before the anchor).
    #[must_use]
    pub fn after(&self) -> bool {
        self.after
    }
}

/// Window-global pinned-leaf store (CTX-1077, issue #1757).
///
/// Holds every pinned [`PinnedEntry`] detached from the live layout; all
/// transitions are total and deterministic. The store itself never paints
/// and never enters the layout solver: it only stores detached leaves
/// between pin and unpin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinnedStore {
    entries: Vec<PinnedEntry>,
}

impl PinnedStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// True when no leaf is pinned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of pinned leaves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when `id` is pinned.
    #[must_use]
    pub fn contains(&self, id: ViewId) -> bool {
        self.entries.iter().any(|entry| entry.view.id() == id)
    }

    /// The pinned leaf `id`, if pinned.
    #[must_use]
    pub fn get(&self, id: ViewId) -> Option<&View> {
        self.entries
            .iter()
            .find(|entry| entry.view.id() == id)
            .map(|entry| &entry.view)
    }

    /// Mutable access to the pinned leaf `id`, if pinned (present-time
    /// reflow keeps stored dims fresh without re-parenting).
    #[must_use]
    pub fn find_mut(&mut self, id: ViewId) -> Option<&mut View> {
        self.entries
            .iter_mut()
            .find(|entry| entry.view.id() == id)
            .map(|entry| &mut entry.view)
    }

    /// Pinned leaf ids in pin order (present-time paint order).
    #[must_use]
    pub fn ids(&self) -> Vec<ViewId> {
        self.entries.iter().map(|entry| entry.view.id()).collect()
    }

    /// The recorded restore anchor of pinned leaf `id`, if pinned: the
    /// neighboring leaf id at pin time plus which side the leaf returns
    /// to (`None` outer for an unknown id).
    ///
    /// Session restore (CTX-1082) reads this to persist the anchor beside
    /// the parked leaf; [`Self::restore`] writes it back.
    #[must_use]
    pub fn anchor_of(&self, id: ViewId) -> Option<(Option<ViewId>, bool)> {
        self.entries
            .iter()
            .find(|entry| entry.view.id() == id)
            .map(|entry| (entry.anchor, entry.after))
    }

    /// Reinstalls a previously captured pinned leaf (session restore,
    /// CTX-1082): pushes `view` with its recorded anchor without touching
    /// any layout tree. Pin order is the restore order.
    ///
    /// The caller validates the snapshot first; like [`Self::pin`] this
    /// stays total and never fails.
    pub fn restore(&mut self, view: View, anchor: Option<ViewId>, after: bool) {
        self.entries.push(PinnedEntry {
            view,
            anchor,
            after,
        });
    }

    /// Pins leaf `id`: detaches it from `tree` and stamps it `Floating`.
    ///
    /// Tiled leaves converge on `Floating` so one verb pins either state.
    /// Fails with [`PinnedError::AlreadyPinned`] (tree untouched) while the
    /// leaf is pinned, [`PinnedError::LeafNotFound`] for an unknown id,
    /// [`PinnedError::UnsupportedMode`] for `Fullscreen`/`Scratchpad`
    /// leaves, or [`PinnedError::StrandedLayout`] when the tree holds a
    /// single leaf (an empty live layout would strand focus and present).
    /// When the CW-08 gate rejects the stamp, the leaf is restored to its
    /// prior position and [`PinnedError::Rejected`] is returned.
    pub fn pin(&mut self, tree: &mut LayoutNode, id: ViewId) -> Result<(), PinnedError> {
        if self.contains(id) {
            return Err(PinnedError::AlreadyPinned(id));
        }
        if tree.leaf_count() <= 1 && tree.leaf_ids().contains(&id) {
            return Err(PinnedError::StrandedLayout);
        }
        let ids = tree.leaf_ids();
        let pos = ids
            .iter()
            .position(|&v| v == id)
            .ok_or(PinnedError::LeafNotFound(id))?;
        // Anchor on the following leaf (restore before it); otherwise the
        // preceding leaf (restore after it); otherwise no anchor.
        let (anchor, after) = if let Some(&next) = ids.get(pos + 1) {
            (Some(next), false)
        } else if pos > 0 {
            (Some(ids[pos - 1]), true)
        } else {
            (None, true)
        };
        let Some(mut view) = tree.remove_leaf(id) else {
            return Err(PinnedError::LeafNotFound(id));
        };
        let from = view.presentation();
        if !matches!(from, PresentationMode::Tiled | PresentationMode::Floating) {
            Self::restore_view(tree, view, anchor, after);
            return Err(PinnedError::UnsupportedMode { current: from });
        }
        if !PresentationMode::request_transition(&mut view, PresentationMode::Floating) {
            Self::restore_view(tree, view, anchor, after);
            return Err(PinnedError::Rejected {
                from,
                to: PresentationMode::Floating,
            });
        }
        self.entries.push(PinnedEntry {
            view,
            anchor,
            after,
        });
        Ok(())
    }

    /// Unpins leaf `id`: restores it into `tree` beside its anchor and
    /// keeps it `Floating` (unpin returns the floating panel to the active
    /// workspace; re-tiling is a separate toggle).
    ///
    /// Returns the restored leaf id. Fails with [`PinnedError::NotPinned`]
    /// when the leaf is not pinned. A missing anchor (leaf closed meanwhile)
    /// falls back to docking beside the first live leaf; an empty tree
    /// becomes the restored leaf.
    pub fn unpin(&mut self, tree: &mut LayoutNode, id: ViewId) -> Result<ViewId, PinnedError> {
        let pos = self
            .entries
            .iter()
            .position(|entry| entry.view.id() == id)
            .ok_or(PinnedError::NotPinned(id))?;
        let entry = self.entries.remove(pos);
        let id = entry.view.id();
        Self::restore_view(tree, entry.view, entry.anchor, entry.after);
        Ok(id)
    }

    /// Toggles the store: unpins `target` when pinned, otherwise pins it.
    /// Returns the restored id on unpin, `None` on pin.
    pub fn toggle(
        &mut self,
        tree: &mut LayoutNode,
        target: Option<ViewId>,
    ) -> Result<Option<ViewId>, PinnedError> {
        let id = target.ok_or(PinnedError::MissingTarget)?;
        if self.contains(id) {
            return self.unpin(tree, id).map(Some);
        }
        self.pin(tree, id).map(|()| None)
    }

    /// Puts `view` back into `tree` beside `anchor` (or a fallback dock).
    fn restore_view(tree: &mut LayoutNode, view: View, anchor: Option<ViewId>, after: bool) {
        if tree.leaf_count() == 0 {
            *tree = LayoutNode::leaf(view);
            return;
        }
        if let Some(a) = anchor {
            if tree.leaf_ids().contains(&a)
                && tree.insert_beside(a, &view, SplitAxis::Horizontal, 0.5, after)
            {
                return;
            }
        }
        // Anchor gone (or never recorded): dock after the first live leaf.
        let first = tree
            .leaf_ids()
            .into_iter()
            .next()
            .expect("non-empty tree has a leaf");
        if !tree.insert_beside(first, &view, SplitAxis::Horizontal, 0.5, true) {
            // Unreachable for a non-empty tree, but never lose the view:
            // wrap the whole tree in a split carrying it.
            let old = std::mem::replace(tree, LayoutNode::stack(Vec::new()));
            *tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, old, LayoutNode::leaf(view));
        }
    }
}

/// Applies workspace command `command` to `store`/`tree` (CTX-1077
/// command-registry path).
///
/// Only [`PIN_CMD_TOGGLE`] is accepted; anything else fails with
/// [`PinnedError::UnknownCommand`] and touches neither the store nor the
/// tree. Otherwise delegates to [`PinnedStore::toggle`].
pub fn apply_pin_toggle(
    store: &mut PinnedStore,
    tree: &mut LayoutNode,
    command: &str,
    target: Option<ViewId>,
) -> Result<Option<ViewId>, PinnedError> {
    if command != PIN_CMD_TOGGLE {
        return Err(PinnedError::UnknownCommand(command.to_owned()));
    }
    store.toggle(tree, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

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

    #[test]
    fn pin_detaches_and_stamps_floating() {
        let mut tree = pair();
        let mut store = PinnedStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.pin(&mut tree, ViewId::new(1)).expect("pin leaf 1");
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
        assert!(store.contains(ViewId::new(1)));
        assert!(!store.contains(ViewId::new(2)));
        // Leaf detached; parked copy stamped Floating through the gate.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
        assert_eq!(
            store.get(ViewId::new(1)).expect("parked").presentation(),
            PresentationMode::Floating
        );
        assert_eq!(store.ids(), vec![ViewId::new(1)]);
    }

    #[test]
    fn pin_tiled_converges_on_floating() {
        let mut tree = pair();
        let mut store = PinnedStore::new();
        assert_eq!(
            tree.find_leaf(ViewId::new(2)).expect("leaf").presentation(),
            PresentationMode::Tiled
        );
        store
            .pin(&mut tree, ViewId::new(2))
            .expect("pin tiled leaf");
        assert_eq!(
            store.get(ViewId::new(2)).expect("parked").presentation(),
            PresentationMode::Floating
        );
    }

    #[test]
    fn pin_floating_keeps_floating() {
        let mut tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, floating_leaf(1), leaf(2));
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(1)).expect("pin float");
        assert_eq!(
            store.get(ViewId::new(1)).expect("parked").presentation(),
            PresentationMode::Floating
        );
    }

    #[test]
    fn pin_rejects_non_float_modes_and_double_pin() {
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
        let mut store = PinnedStore::new();
        assert_eq!(
            store.pin(&mut tree, ViewId::new(1)),
            Err(PinnedError::UnsupportedMode {
                current: PresentationMode::Fullscreen
            })
        );
        // Rejected pin restores the tree verbatim.
        assert_eq!(tree.leaf_count(), 2);
        assert!(tree.leaf_ids().contains(&ViewId::new(1)));

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
            store.pin(&mut scratch_tree, ViewId::new(7)),
            Err(PinnedError::UnsupportedMode {
                current: PresentationMode::Scratchpad
            })
        );

        let mut tree = pair();
        store.pin(&mut tree, ViewId::new(1)).expect("pin leaf 1");
        assert_eq!(
            store.pin(&mut tree, ViewId::new(1)),
            Err(PinnedError::AlreadyPinned(ViewId::new(1)))
        );
        assert_eq!(
            store.pin(&mut tree, ViewId::new(404)),
            Err(PinnedError::LeafNotFound(ViewId::new(404)))
        );
    }

    #[test]
    fn pin_refuses_to_strand_a_single_leaf_layout() {
        let mut tree = leaf(1);
        let mut store = PinnedStore::new();
        assert_eq!(
            store.pin(&mut tree, ViewId::new(1)),
            Err(PinnedError::StrandedLayout)
        );
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1)]);
        assert!(store.is_empty());
    }

    #[test]
    fn unpin_restores_floating_beside_anchor() {
        let mut tree = triple();
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(2)).expect("pin leaf 2");
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(3)]);

        let restored = store.unpin(&mut tree, ViewId::new(2)).expect("unpin");
        assert_eq!(restored, ViewId::new(2));
        assert!(store.is_empty());
        // Anchor was leaf 3 (following neighbor): restored before it, and
        // still Floating (unpin returns the floating panel; no re-tile).
        let ids = tree.leaf_ids();
        assert!(ids.contains(&ViewId::new(2)));
        assert_eq!(
            tree.find_leaf(ViewId::new(2)).expect("leaf").presentation(),
            PresentationMode::Floating
        );
        assert_eq!(
            store.unpin(&mut tree, ViewId::new(2)),
            Err(PinnedError::NotPinned(ViewId::new(2)))
        );
    }

    #[test]
    fn unpin_falls_back_when_anchor_is_gone() {
        let mut tree = triple();
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(1)).expect("pin leaf 1");
        // Anchor (leaf 2) leaves the tree before the unpin.
        let _ = tree.remove_leaf(ViewId::new(2)).expect("remove anchor");
        assert!(!tree.leaf_ids().contains(&ViewId::new(2)));
        let restored = store.unpin(&mut tree, ViewId::new(1)).expect("unpin");
        assert_eq!(restored, ViewId::new(1));
        assert!(store.is_empty());
        // Fallback dock: restored beside the first live leaf, still Floating.
        assert!(tree.leaf_ids().contains(&ViewId::new(1)));
        assert!(tree.leaf_ids().contains(&ViewId::new(3)));
        assert_eq!(
            tree.find_leaf(ViewId::new(1)).expect("leaf").presentation(),
            PresentationMode::Floating
        );
    }

    #[test]
    fn multiple_pins_keep_pin_order() {
        let mut tree = triple();
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(3)).expect("pin 3");
        store.pin(&mut tree, ViewId::new(1)).expect("pin 1");
        assert_eq!(store.len(), 2);
        assert_eq!(store.ids(), vec![ViewId::new(3), ViewId::new(1)]);
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
    }

    #[test]
    fn toggle_pins_then_unpins_and_routes_command() {
        let mut tree = pair();
        let mut store = PinnedStore::new();
        assert_eq!(
            store
                .toggle(&mut tree, Some(ViewId::new(1)))
                .expect("toggle to pin"),
            None
        );
        assert!(store.contains(ViewId::new(1)));
        assert_eq!(
            store
                .toggle(&mut tree, Some(ViewId::new(1)))
                .expect("toggle to unpin"),
            Some(ViewId::new(1))
        );
        assert!(store.is_empty());
        assert_eq!(
            store.toggle(&mut tree, None),
            Err(PinnedError::MissingTarget)
        );

        store
            .toggle(&mut tree, Some(ViewId::new(2)))
            .expect("pin leaf 2");
        assert_eq!(
            apply_pin_toggle(&mut store, &mut tree, "bitty.workspace:nope", None),
            Err(PinnedError::UnknownCommand(String::from(
                "bitty.workspace:nope"
            )))
        );
        assert!(store.contains(ViewId::new(2)));
        assert_eq!(
            apply_pin_toggle(&mut store, &mut tree, PIN_CMD_TOGGLE, Some(ViewId::new(2)))
                .expect("registry unpin"),
            Some(ViewId::new(2))
        );
        assert!(store.is_empty());
    }

    #[test]
    fn find_mut_reflows_stored_dims() {
        let mut tree = pair();
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(1)).expect("pin");
        let view = store.find_mut(ViewId::new(1)).expect("stored");
        assert!(view.reflow_to_rect(Rect::new(0, 0, 10, 8)));
        assert_eq!(store.get(ViewId::new(1)).expect("parked").cols(), 10);
        assert!(store.find_mut(ViewId::new(404)).is_none());
    }

    #[test]
    fn restore_reinstalls_captured_anchor_without_touching_tree() {
        // CTX-1082: session restore reinstalls the parked leaf plus its
        // recorded anchor; the layout tree is never touched here.
        let mut tree = triple();
        let mut store = PinnedStore::new();
        store.pin(&mut tree, ViewId::new(2)).expect("pin leaf 2");
        let parked = store.get(ViewId::new(2)).expect("parked").clone();
        let (anchor, after) = store.anchor_of(ViewId::new(2)).expect("anchor recorded");
        assert_eq!(anchor, Some(ViewId::new(3)));
        assert!(!after, "pin anchors before the following neighbor");
        assert_eq!(store.anchor_of(ViewId::new(404)), None);

        let mut revived = PinnedStore::new();
        revived.restore(parked.clone(), anchor, after);
        assert_eq!(revived.ids(), vec![ViewId::new(2)]);
        assert_eq!(revived.get(ViewId::new(2)), Some(&parked));
        assert_eq!(revived.anchor_of(ViewId::new(2)), Some((anchor, after)));
        // The tree the leaf came from is untouched by the reinstall.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(3)]);

        // Restoring into the live tree honors the carried anchor.
        let restored = revived.unpin(&mut tree, ViewId::new(2)).expect("unpin");
        assert_eq!(restored, ViewId::new(2));
        assert!(revived.is_empty());
        assert!(tree.leaf_ids().contains(&ViewId::new(2)));
    }
}
