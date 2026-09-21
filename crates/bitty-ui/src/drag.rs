//! Bounded undo history for workspace drag/resize sessions (CW-09).
//!
//! [`DragHistory`] records [`LayoutNode`](crate::layout::LayoutNode)
//! snapshots before each mutating drag/resize step (re-parent, handle
//! resize); `undo` pops the most recent snapshot and restores it. The
//! history is bounded ([`DRAG_HISTORY_CAP`] entries, `DropOldest`): pushing
//! past the cap discards the oldest snapshot, so memory stays capped at
//! `cap * tree-size` regardless of session length. All operations are
//! deterministic and headless; this module adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::layout::LayoutNode;

/// Maximum undo snapshots retained per drag session.
pub const DRAG_HISTORY_CAP: usize = 32;

/// Bounded LIFO undo history over [`LayoutNode`] snapshots (CW-09).
#[derive(Clone, Debug, Default)]
pub struct DragHistory {
    snapshots: Vec<LayoutNode>,
}

impl DragHistory {
    /// Creates an empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
        }
    }

    /// Number of retained snapshots (`<= [`DRAG_HISTORY_CAP`]`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// True when no snapshot is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Records `tree` as the pre-mutation snapshot; evicts the oldest when
    /// the history is at [`DRAG_HISTORY_CAP`] (`DropOldest`).
    pub fn push(&mut self, tree: &LayoutNode) {
        if self.snapshots.len() >= DRAG_HISTORY_CAP {
            self.snapshots.remove(0);
        }
        self.snapshots.push(tree.clone());
    }

    /// Restores the most recent snapshot into `tree`, returning `true`.
    /// Returns `false` with `tree` untouched when the history is empty.
    pub fn undo(&mut self, tree: &mut LayoutNode) -> bool {
        let Some(snapshot) = self.snapshots.pop() else {
            return false;
        };
        *tree = snapshot;
        true
    }

    /// Drops all retained snapshots.
    pub fn clear(&mut self) {
        self.snapshots.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::SplitAxis;
    use crate::view::{View, ViewId};

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    #[test]
    fn undo_restores_pre_drag_snapshot() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        assert!(history.is_empty());
        history.push(&tree);
        assert_eq!(history.len(), 1);
        assert!(tree.reparent_leaf(
            ViewId::new(2),
            ViewId::new(1),
            SplitAxis::Vertical,
            0.5,
            true
        ));
        assert_ne!(tree, pair());
        assert!(history.undo(&mut tree));
        assert_eq!(tree, pair());
        assert!(history.is_empty());
        // Empty history: fail-soft false, tree untouched.
        let before = tree.clone();
        assert!(!history.undo(&mut tree));
        assert_eq!(tree, before);
    }

    #[test]
    fn history_drops_oldest_at_cap() {
        let mut history = DragHistory::new();
        for i in 0..(DRAG_HISTORY_CAP + 5) {
            let tree = LayoutNode::leaf(View::new(ViewId::new(i as u64), 80, 24));
            history.push(&tree);
        }
        assert_eq!(history.len(), DRAG_HISTORY_CAP);
        // The five oldest snapshots were evicted: the newest surviving
        // undo target is the tree pushed at index 5.
        let mut probe = pair();
        assert!(history.undo(&mut probe));
        assert_eq!(
            probe.leaf_ids(),
            vec![ViewId::new((DRAG_HISTORY_CAP + 5 - 1) as u64)]
        );
        history.clear();
        assert!(history.is_empty());
    }

    #[test]
    fn undo_resize_step_returns_prior_ratio() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        history.push(&tree);
        assert!(tree.resize_split_by_drag(&[], 8, 80));
        assert!((tree.split_ratio_at(&[]).expect("split") - 0.6).abs() < 1e-6);
        assert!(history.undo(&mut tree));
        assert!((tree.split_ratio_at(&[]).expect("split") - 0.5).abs() < 1e-6);
    }
}
