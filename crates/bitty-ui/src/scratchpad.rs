//! Hidden per-window scratchpad slot (CW-10, issue #989).
//!
//! A scratchpad leaf is parked outside the layout tree in a single hidden
//! slot owned by the window (one [`ScratchpadSlot`] instance per window).
//! Hiding detaches the leaf via [`LayoutNode::remove_leaf`](crate::layout::LayoutNode::remove_leaf)
//! and stamps it [`PresentationMode::Scratchpad`](crate::presentation::PresentationMode::Scratchpad)
//! through the CW-08 gate; showing restores it beside its recorded anchor
//! via [`LayoutNode::insert_beside`](crate::layout::LayoutNode::insert_beside)
//! and stamps it back to `Tiled`. Toggling routes through the workspace
//! command registry as `bitty.workspace:scratchpad-toggle` (see
//! [`SCRATCHPAD_CMD_TOGGLE`] and [`apply_scratchpad_toggle`]).
//!
//! # Decision: single slot, not a bounded stack
//!
//! The issue left open whether the slot holds one leaf or a bounded stack.
//! This slice implements a single slot: hide fails with
//! [`ScratchpadError::Occupied`] while a leaf is parked, and the parked
//! leaf's mode, anchor, and drop side are preserved verbatim for restore.
//! A stack would need ordering, eviction, and per-entry anchor policy with
//! no consumer yet; if a second consumer arrives, the open item is to
//! revisit, not to pre-build.
//!
//! # Distinct from `Visibility`
//!
//! The parked state here is storage (the leaf is out of the tree), never a
//! display flag. `Visibility::ScratchpadHidden` (computed display state in
//! the `bitty-runtime` registry) is deliberately untouched: this module
//! never reads or writes `Visibility`, and no variant is added anywhere.
//!
//! Deterministic, headless, bounded (at most one parked leaf); this module
//! adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::SplitAxis;
use crate::layout::LayoutNode;
use crate::presentation::PresentationMode;
use crate::view::{View, ViewId};

/// Workspace command toggling the hidden scratchpad slot.
///
/// `<owner>.<name>:<command>` grammar (see
/// [`QualifiedCommand`](crate::panel::QualifiedCommand)); routed by
/// [`apply_scratchpad_toggle`]. With `target = Some(id)` and a vacant slot
/// the leaf hides; with an occupied slot the parked leaf shows (the target
/// is ignored); with a vacant slot and `target = None` the call fails with
/// [`ScratchpadError::MissingTarget`].
pub const SCRATCHPAD_CMD_TOGGLE: &str = "bitty.workspace:scratchpad-toggle";

/// Error for [`ScratchpadSlot`] hide/show/toggle and [`apply_scratchpad_toggle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScratchpadError {
    /// A leaf is already parked; hide leaves the tree untouched.
    Occupied,
    /// Nothing is parked; show leaves the tree untouched.
    Vacant,
    /// Toggle-to-hide needs a target leaf id.
    MissingTarget,
    /// No leaf with this id exists; the tree is untouched.
    LeafNotFound(ViewId),
    /// The [`PresentationMode::can_transition`] gate rejected the mode
    /// stamp; the tree is restored to its prior shape.
    Rejected {
        from: PresentationMode,
        to: PresentationMode,
    },
    /// `command` is not [`SCRATCHPAD_CMD_TOGGLE`].
    UnknownCommand(String),
}

impl std::fmt::Display for ScratchpadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Occupied => f.write_str("scratchpad slot already occupied"),
            Self::Vacant => f.write_str("scratchpad slot is empty"),
            Self::MissingTarget => f.write_str("scratchpad hide needs a target leaf"),
            Self::LeafNotFound(id) => write!(f, "scratchpad target not found: {id}"),
            Self::Rejected { from, to } => {
                write!(f, "scratchpad transition rejected: {from} -> {to}")
            }
            Self::UnknownCommand(cmd) => write!(f, "unknown scratchpad command: {cmd}"),
        }
    }
}

impl std::error::Error for ScratchpadError {}

/// Parked leaf plus its restore anchor: the neighboring leaf id at hide
/// time and which side the parked leaf returns to (`after == false` puts
/// the restored leaf before the anchor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HiddenEntry {
    view: View,
    anchor: Option<ViewId>,
    after: bool,
}

impl HiddenEntry {
    /// The parked leaf (stamped `Scratchpad`).
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The neighboring leaf id recorded at hide time, if any.
    #[must_use]
    pub fn anchor(&self) -> Option<ViewId> {
        self.anchor
    }
}

/// Single hidden per-window scratchpad slot (CW-10).
///
/// Holds at most one parked [`HiddenEntry`]; all transitions are total and
/// deterministic. The slot itself never paints and never enters the layout
/// solver: it only stores the detached leaf between hide and show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScratchpadSlot {
    hidden: Option<HiddenEntry>,
}

impl ScratchpadSlot {
    /// Creates an empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self { hidden: None }
    }

    /// True when no leaf is parked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hidden.is_none()
    }

    /// The parked leaf, if any.
    #[must_use]
    pub fn peek(&self) -> Option<&View> {
        self.hidden.as_ref().map(|h| &h.view)
    }

    /// Parks leaf `id`: detaches it from `tree` and stamps it `Scratchpad`.
    ///
    /// Fails with [`ScratchpadError::Occupied`] (tree untouched) while a
    /// leaf is parked, or [`ScratchpadError::LeafNotFound`] for an unknown
    /// id. When the CW-08 gate rejects the stamp, the leaf is restored to
    /// its prior position and [`ScratchpadError::Rejected`] is returned.
    pub fn hide(&mut self, tree: &mut LayoutNode, id: ViewId) -> Result<(), ScratchpadError> {
        if self.hidden.is_some() {
            return Err(ScratchpadError::Occupied);
        }
        let ids = tree.leaf_ids();
        let pos = ids
            .iter()
            .position(|&v| v == id)
            .ok_or(ScratchpadError::LeafNotFound(id))?;
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
            return Err(ScratchpadError::LeafNotFound(id));
        };
        let from = view.presentation();
        if !PresentationMode::request_transition(&mut view, PresentationMode::Scratchpad) {
            Self::restore_view(tree, view, anchor, after);
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Scratchpad,
            });
        }
        self.hidden = Some(HiddenEntry {
            view,
            anchor,
            after,
        });
        Ok(())
    }

    /// Restores the parked leaf beside its anchor and stamps it `Tiled`.
    ///
    /// Returns the restored leaf id. Fails with [`ScratchpadError::Vacant`]
    /// when nothing is parked. A missing anchor (leaf closed meanwhile)
    /// falls back to docking beside the first live leaf; an empty tree
    /// becomes the restored leaf.
    pub fn show(&mut self, tree: &mut LayoutNode) -> Result<ViewId, ScratchpadError> {
        let entry = self.hidden.take().ok_or(ScratchpadError::Vacant)?;
        let mut view = entry.view;
        let from = view.presentation();
        if !PresentationMode::request_transition(&mut view, PresentationMode::Tiled) {
            self.hidden = Some(HiddenEntry {
                view,
                anchor: entry.anchor,
                after: entry.after,
            });
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Tiled,
            });
        }
        let id = view.id();
        Self::restore_view(tree, view, entry.anchor, entry.after);
        Ok(id)
    }

    /// Toggles the slot: shows the parked leaf when occupied, otherwise
    /// hides `target`. Returns the restored id on show, `None` on hide.
    pub fn toggle(
        &mut self,
        tree: &mut LayoutNode,
        target: Option<ViewId>,
    ) -> Result<Option<ViewId>, ScratchpadError> {
        if self.hidden.is_some() {
            return self.show(tree).map(Some);
        }
        let id = target.ok_or(ScratchpadError::MissingTarget)?;
        self.hide(tree, id).map(|()| None)
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

/// Applies workspace command `command` to `slot`/`tree` (CW-10
/// command-registry path).
///
/// Only [`SCRATCHPAD_CMD_TOGGLE`] is accepted; anything else fails with
/// [`ScratchpadError::UnknownCommand`] and touches neither the slot nor
/// the tree. Otherwise delegates to [`ScratchpadSlot::toggle`].
pub fn apply_scratchpad_toggle(
    slot: &mut ScratchpadSlot,
    tree: &mut LayoutNode,
    command: &str,
    target: Option<ViewId>,
) -> Result<Option<ViewId>, ScratchpadError> {
    if command != SCRATCHPAD_CMD_TOGGLE {
        return Err(ScratchpadError::UnknownCommand(command.to_owned()));
    }
    slot.toggle(tree, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    #[test]
    fn hide_show_roundtrip_restores_order_and_modes() {
        let mut tree = pair();
        let mut slot = ScratchpadSlot::new();
        assert!(slot.is_empty());

        slot.hide(&mut tree, ViewId::new(1)).expect("hide leaf 1");
        assert!(!slot.is_empty());
        // Leaf detached; parked copy stamped Scratchpad through the gate.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
        assert_eq!(
            slot.peek().expect("parked").presentation(),
            PresentationMode::Scratchpad
        );

        let restored = slot.show(&mut tree).expect("show");
        assert_eq!(restored, ViewId::new(1));
        assert!(slot.is_empty());
        // Anchor was leaf 2 (following), restored before it: order kept.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert_eq!(
            tree.find_leaf(ViewId::new(1)).expect("leaf").presentation(),
            PresentationMode::Tiled
        );
        // Layout covers the container exactly after restore.
        let alloc = tree.layout(Rect::new(0, 0, 80, 24));
        assert_eq!(alloc.len(), 2);
        assert_eq!(alloc[0].1.width as u32 + alloc[1].1.width as u32, 80);
    }

    #[test]
    fn hide_occupied_or_unknown_leaves_tree_untouched() {
        let mut tree = pair();
        let mut slot = ScratchpadSlot::new();
        slot.hide(&mut tree, ViewId::new(1)).expect("hide");
        let before = tree.clone();
        assert_eq!(
            slot.hide(&mut tree, ViewId::new(2)),
            Err(ScratchpadError::Occupied)
        );
        assert_eq!(tree, before);
        assert_eq!(
            slot.hide(&mut tree, ViewId::new(404)),
            Err(ScratchpadError::Occupied),
            "occupancy is checked before lookup"
        );
        assert_eq!(tree, before);

        let mut slot2 = ScratchpadSlot::new();
        let before2 = tree.clone();
        assert_eq!(
            slot2.hide(&mut tree, ViewId::new(404)),
            Err(ScratchpadError::LeafNotFound(ViewId::new(404)))
        );
        assert_eq!(tree, before2);
    }

    #[test]
    fn show_vacant_fails() {
        let mut tree = pair();
        let mut slot = ScratchpadSlot::new();
        assert_eq!(slot.show(&mut tree), Err(ScratchpadError::Vacant));
        assert_eq!(tree, pair(), "vacant show touches nothing");
    }

    #[test]
    fn toggle_hides_then_shows_target_ignored_on_show() {
        let mut tree = pair();
        let mut slot = ScratchpadSlot::new();
        assert_eq!(
            slot.toggle(&mut tree, None),
            Err(ScratchpadError::MissingTarget)
        );
        assert_eq!(slot.toggle(&mut tree, Some(ViewId::new(2))), Ok(None));
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1)]);
        // Occupied: target ignored, parked leaf returns.
        assert_eq!(
            slot.toggle(&mut tree, Some(ViewId::new(1))),
            Ok(Some(ViewId::new(2)))
        );
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
    }

    #[test]
    fn unknown_command_rejected_before_lookup() {
        let mut tree = pair();
        let mut slot = ScratchpadSlot::new();
        let before = tree.clone();
        assert_eq!(
            apply_scratchpad_toggle(
                &mut slot,
                &mut tree,
                "bitty.workspace:nope",
                Some(ViewId::new(1))
            ),
            Err(ScratchpadError::UnknownCommand(
                "bitty.workspace:nope".to_string()
            ))
        );
        assert_eq!(tree, before);
        assert!(slot.is_empty());
        // Registered command routes through the registry grammar path.
        let mut registry = crate::panel::CommandRegistry::new();
        let owner = crate::panel::PanelId::new(7);
        registry
            .register(owner, SCRATCHPAD_CMD_TOGGLE)
            .expect("toggle command registers");
        assert_eq!(registry.owner_of(SCRATCHPAD_CMD_TOGGLE), Some(owner));
        assert_eq!(
            apply_scratchpad_toggle(
                &mut slot,
                &mut tree,
                SCRATCHPAD_CMD_TOGGLE,
                Some(ViewId::new(1))
            ),
            Ok(None)
        );
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)]);
    }

    #[test]
    fn show_after_anchor_closed_docks_beside_first_live_leaf() {
        let mut tree = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            leaf(1),
            LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(2), leaf(3)),
        );
        let mut slot = ScratchpadSlot::new();
        // Hide leaf 2; anchor is leaf 3 (following).
        slot.hide(&mut tree, ViewId::new(2)).expect("hide");
        // Close the anchor meanwhile.
        assert!(tree.remove_leaf(ViewId::new(3)).is_some());
        let restored = slot.show(&mut tree).expect("show with dead anchor");
        assert_eq!(restored, ViewId::new(2));
        assert_eq!(tree.leaf_count(), 2);
        assert!(tree.find_leaf(ViewId::new(2)).is_some());
        let alloc = tree.layout(Rect::new(0, 0, 80, 24));
        assert_eq!(alloc.len(), 2);
    }

    #[test]
    fn show_into_emptied_tree_becomes_leaf() {
        let mut tree = LayoutNode::leaf(View::new(ViewId::new(1), 80, 24));
        let mut slot = ScratchpadSlot::new();
        slot.hide(&mut tree, ViewId::new(1))
            .expect("hide sole leaf");
        assert_eq!(tree.leaf_count(), 0);
        assert_eq!(slot.show(&mut tree), Ok(ViewId::new(1)));
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1)]);
    }
}
