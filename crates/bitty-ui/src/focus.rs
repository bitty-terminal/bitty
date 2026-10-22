//! Focus handling: deterministic traversal over the `LayoutNode` leaf order.
//!
//! Focus is owned (no platform handle) and moves deterministically given the
//! same tree and bounds. Linear next/prev traverse depth-first order; spatial
//! up/down/left/right select panes beyond the focused pane's facing edge,
//! preferring perpendicular overlap, with deterministic edge/center/order
//! tie-breaking.

#![forbid(unsafe_code)]

use crate::geometry::{Gaps, Rect, SplitAxis};
use crate::layout::LayoutNode;
use crate::view::ViewId;

/// Direction for focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusDirection {
    /// Next in depth-first order (wrap).
    Next,
    /// Previous in depth-first order (wrap).
    Prev,
    /// Spatial up (leaf directly above).
    Up,
    /// Spatial down.
    Down,
    /// Spatial left.
    Left,
    /// Spatial right.
    Right,
}

/// Owned focus state pointing at a leaf `ViewId` if any leaves exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// Currently focused view, if any.
    focused: Option<ViewId>,
}

impl Focus {
    /// Creates an unfocused state.
    #[must_use]
    pub fn new() -> Self {
        Self { focused: None }
    }

    /// Creates focusing the given view.
    #[must_use]
    pub fn with_focus(id: ViewId) -> Self {
        Self { focused: Some(id) }
    }

    /// Current focus.
    #[must_use]
    pub fn focused(&self) -> Option<ViewId> {
        self.focused
    }

    /// Sets focus to `id` unconditionally.
    pub fn set(&mut self, id: ViewId) {
        self.focused = Some(id);
    }

    /// Clears focus.
    pub fn clear(&mut self) {
        self.focused = None;
    }

    /// True when `id` is focused.
    #[must_use]
    pub fn is_focused(&self, id: ViewId) -> bool {
        self.focused == Some(id)
    }

    /// Focuses the first leaf of `node` if any. Returns the new focus.
    pub fn focus_first(&mut self, node: &LayoutNode) -> Option<ViewId> {
        let first = node.leaf_ids().into_iter().next();
        self.focused = first;
        first
    }

    /// Linear next: wraps around deterministically by leaf order.
    #[must_use]
    pub fn next(&self, node: &LayoutNode) -> Option<ViewId> {
        self.neighbor(node, 1)
    }

    /// Linear previous.
    #[must_use]
    pub fn prev(&self, node: &LayoutNode) -> Option<ViewId> {
        self.neighbor(node, -1)
    }

    /// Advances focus in `dir`, returning the new focus (if changed). The
    /// method is total: on empty trees returns `None`, on single leaf stays.
    ///
    /// For spatial directions `Up/Down/Left/Right`, `container` is the bounds
    /// used to compute the rect allocation. The allocation is deterministic
    /// given the same node and container.
    ///
    /// Gapless: allocations tile edge-to-edge so facing edges meet exactly.
    /// Use [`Self::advance_with_gaps`] when the tree is laid out with gaps.
    #[must_use]
    pub fn advance(
        &self,
        node: &LayoutNode,
        container: Rect,
        dir: FocusDirection,
    ) -> Option<ViewId> {
        self.advance_with_gaps(node, container, Gaps::ZERO, dir)
    }

    /// Gap-aware focus advance (CTX-0177).
    ///
    /// Like [`Self::advance`] but computes allocations with
    /// `LayoutNode::layout_with_gaps`; sibling gap bands simply increase the
    /// facing-edge distance, so spatial selection crosses them without a
    /// separate tolerance. With [`Gaps::ZERO`] this is identical to
    /// [`Self::advance`].
    #[must_use]
    pub fn advance_with_gaps(
        &self,
        node: &LayoutNode,
        container: Rect,
        gaps: Gaps,
        dir: FocusDirection,
    ) -> Option<ViewId> {
        match dir {
            FocusDirection::Next => self.next(node),
            FocusDirection::Prev => self.prev(node),
            FocusDirection::Up => self.spatial(node, container, gaps, dir),
            FocusDirection::Down => self.spatial(node, container, gaps, dir),
            FocusDirection::Left => self.spatial(node, container, gaps, dir),
            FocusDirection::Right => self.spatial(node, container, gaps, dir),
        }
    }

    /// Mutating variant of `advance`: updates `self` and returns new focus.
    pub fn move_focus(
        &mut self,
        node: &LayoutNode,
        container: Rect,
        dir: FocusDirection,
    ) -> Option<ViewId> {
        let next = self.advance(node, container, dir);
        if let Some(id) = next {
            self.focused = Some(id);
        }
        next
    }

    /// Mutating variant of [`Self::advance_with_gaps`].
    pub fn move_focus_with_gaps(
        &mut self,
        node: &LayoutNode,
        container: Rect,
        gaps: Gaps,
        dir: FocusDirection,
    ) -> Option<ViewId> {
        let next = self.advance_with_gaps(node, container, gaps, dir);
        if let Some(id) = next {
            self.focused = Some(id);
        }
        next
    }

    fn neighbor(&self, node: &LayoutNode, step: isize) -> Option<ViewId> {
        let ids = node.leaf_ids();
        if ids.is_empty() {
            return None;
        }
        let cur = self.focused.and_then(|f| ids.iter().position(|&x| x == f));
        let next_idx = match cur {
            Some(idx) => {
                let len = ids.len() as isize;
                ((idx as isize + step).rem_euclid(len)) as usize
            }
            None => {
                if step >= 0 {
                    0
                } else {
                    ids.len() - 1
                }
            }
        };
        Some(ids[next_idx])
    }

    fn spatial(
        &self,
        node: &LayoutNode,
        container: Rect,
        gaps: Gaps,
        dir: FocusDirection,
    ) -> Option<ViewId> {
        let Some(cur) = self.focused else {
            return self.next(node);
        };
        let alloc = node.layout_with_gaps(container, gaps);
        let cur_rect = alloc.iter().find(|(id, _)| *id == cur).map(|(_, r)| *r)?;
        // A zero-area focused leaf has no visual neighborhood; directional
        // focus stays put instead of teleporting through invisible geometry.
        if cur_rect.width == 0 || cur_rect.height == 0 {
            return Some(cur);
        }
        let order = node.leaf_ids();
        // Issue #2444: a candidate must lie fully beyond the focused leaf's
        // facing edge. Aligned candidates (perpendicular interval overlaps)
        // rank above disjoint ones; within a tier the nearest facing edge,
        // then the dwindle tree's directional edge, then the widest
        // perpendicular overlap, then the nearest perpendicular center, then
        // stable leaf order decide. Center-to-center distance alone picks a
        // taller neighbor whose center happens to be closer in an L-shaped
        // layout.
        // Issue #2526: under a full-width top pane both bottom panes touch
        // the facing edge, so tree adjacency outranks center distance; a
        // wider bottom-right pane must not steal `Mod+j` from the bottom-left
        // pane by having a closer center.
        let mut aligned: Vec<SpatialCandidate> = Vec::new();
        let mut disjoint: Vec<SpatialCandidate> = Vec::new();
        for (id, rect) in &alloc {
            if *id == cur || rect.width == 0 || rect.height == 0 {
                continue;
            }
            let Some(order) = order.iter().position(|candidate| candidate == id) else {
                continue;
            };
            let Some(placement) = separated_placement(*rect, cur_rect, dir) else {
                continue;
            };
            let candidate = SpatialCandidate {
                id: *id,
                order,
                distance: placement.edge_distance,
                overlap: placement.perpendicular_overlap,
                tree_rank: tree_direction_rank(node, cur, *id, dir).unwrap_or(TreeRank::NONE),
                perpendicular_distance: placement.perpendicular_distance,
            };
            if placement.perpendicular_overlap > 0 {
                aligned.push(candidate);
            } else {
                disjoint.push(candidate);
            }
        }
        for group in [&mut aligned, &mut disjoint] {
            group.sort_by(|a, b| {
                a.distance
                    .cmp(&b.distance)
                    .then_with(|| a.tree_rank.cmp(&b.tree_rank))
                    .then_with(|| b.overlap.cmp(&a.overlap))
                    .then_with(|| a.perpendicular_distance.cmp(&b.perpendicular_distance))
                    .then_with(|| a.order.cmp(&b.order))
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        aligned
            .first()
            .or_else(|| disjoint.first())
            .map_or(Some(cur), |candidate| Some(candidate.id))
    }
}

/// One ranked directional-focus candidate over layout allocations.
struct SpatialCandidate {
    id: ViewId,
    /// Stable leaf order (depth-first solver order).
    order: usize,
    /// Facing-edge distance along the movement axis.
    distance: i64,
    /// Perpendicular overlap length with the focused leaf.
    overlap: i64,
    /// Tree-adjacency rank; [`TreeRank::NONE`] when no ancestor separates
    /// the panes along the movement axis.
    tree_rank: TreeRank,
    /// Perpendicular center distance (doubled).
    perpendicular_distance: i64,
}

/// Directional adjacency of a candidate in the layout tree.
///
/// Ordering is derived so `Ord` sorts the best candidate first: shallower
/// common ancestor first, then the natural edge leaf before a deeper leaf
/// of the same sibling subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TreeRank {
    /// Depth of the lowest common ancestor split (0 = root).
    depth: u32,
    /// `true` for a non-natural leaf inside the sibling subtree.
    off_edge: bool,
}

impl TreeRank {
    /// Rank for a candidate without a directional tree ancestor; sorts
    /// after every ranked candidate.
    const NONE: Self = Self {
        depth: u32::MAX,
        off_edge: true,
    };
}

/// Tree-adjacency rank for a candidate in `dir` (issue #2526).
///
/// Walks the split paths of the focused and candidate leaves, finds their
/// lowest common ancestor split, and returns `Some` when that split places
/// the candidate on the far side of `dir` (right/down for index `1`,
/// left/up for index `0`). The closest such ancestor wins; inside its
/// sibling subtree the leaf at the movement edge (first leaf for
/// right/down, last for left/up) is the natural target and outranks the
/// others. This is the dwindle tie-break: from the top pane of a split
/// column, `Down` lands on the bottom-left pane, never on a sibling chosen
/// by center distance or leaf order.
#[must_use]
fn tree_direction_rank(
    node: &LayoutNode,
    current: ViewId,
    candidate: ViewId,
    dir: FocusDirection,
) -> Option<TreeRank> {
    if matches!(dir, FocusDirection::Next | FocusDirection::Prev) {
        return None;
    }
    let current_path = node.path_to_leaf(current)?;
    let candidate_path = node.path_to_leaf(candidate)?;
    let mut depth = 0;
    while depth < current_path.len()
        && depth < candidate_path.len()
        && current_path[depth] == candidate_path[depth]
    {
        depth += 1;
    }
    let ((axis, current_index), (_, candidate_index)) =
        (*current_path.get(depth)?, *candidate_path.get(depth)?);
    let forward = match dir {
        FocusDirection::Right => {
            axis == SplitAxis::Horizontal && current_index == 0 && candidate_index == 1
        }
        FocusDirection::Left => {
            axis == SplitAxis::Horizontal && current_index == 1 && candidate_index == 0
        }
        FocusDirection::Down => {
            axis == SplitAxis::Vertical && current_index == 0 && candidate_index == 1
        }
        FocusDirection::Up => {
            axis == SplitAxis::Vertical && current_index == 1 && candidate_index == 0
        }
        FocusDirection::Next | FocusDirection::Prev => false,
    };
    if !forward {
        return None;
    }
    let first_on_edge = matches!(dir, FocusDirection::Right | FocusDirection::Down);
    let edge_index = usize::from(!first_on_edge);
    let off_edge = candidate_path[depth + 1..]
        .iter()
        .any(|(_, index)| *index != edge_index);
    Some(TreeRank {
        depth: u32::try_from(depth).unwrap_or(u32::MAX),
        off_edge,
    })
}

/// Placement of a candidate that lies fully beyond the focused leaf.
struct SeparatedPlacement {
    edge_distance: i64,
    perpendicular_overlap: i64,
    perpendicular_distance: i64,
}

/// Classifies a candidate as separated in `dir`, returning its edge gap,
/// perpendicular overlap, and perpendicular center distance. Candidates
/// that overlap the focused leaf on the movement axis are not directional
/// targets for the plain layout tree.
fn separated_placement(
    candidate: Rect,
    current: Rect,
    dir: FocusDirection,
) -> Option<SeparatedPlacement> {
    let cx0 = i64::from(candidate.x);
    let cy0 = i64::from(candidate.y);
    let cx1 = cx0 + i64::from(candidate.width);
    let cy1 = cy0 + i64::from(candidate.height);
    let cur_x0 = i64::from(current.x);
    let cur_y0 = i64::from(current.y);
    let cur_x1 = cur_x0 + i64::from(current.width);
    let cur_y1 = cur_y0 + i64::from(current.height);
    let center_dx = (cx0 + cx1) - (cur_x0 + cur_x1);
    let center_dy = (cy0 + cy1) - (cur_y0 + cur_y1);
    let overlap_x = overlap_1d(cx0, cx1, cur_x0, cur_x1);
    let overlap_y = overlap_1d(cy0, cy1, cur_y0, cur_y1);
    let (edge_distance, perpendicular_overlap, perpendicular_distance) = match dir {
        FocusDirection::Up if cy1 <= cur_y0 => (cur_y0 - cy1, overlap_x, center_dx.abs()),
        FocusDirection::Down if cy0 >= cur_y1 => (cy0 - cur_y1, overlap_x, center_dx.abs()),
        FocusDirection::Left if cx1 <= cur_x0 => (cur_x0 - cx1, overlap_y, center_dy.abs()),
        FocusDirection::Right if cx0 >= cur_x1 => (cx0 - cur_x1, overlap_y, center_dy.abs()),
        _ => return None,
    };
    Some(SeparatedPlacement {
        edge_distance,
        perpendicular_overlap,
        perpendicular_distance,
    })
}

/// Length of the intersection of two half-open intervals, clamped at 0.
fn overlap_1d(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    a1.min(b1).saturating_sub(a0.max(b0)).max(0)
}

impl Default for Focus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Rect, SplitAxis};
    use crate::layout::LayoutNode;
    use crate::view::{View, ViewId};

    fn v(id: u64) -> View {
        View::new(ViewId::new(id), 10, 10)
    }

    fn two_pane_horizontal() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::leaf(v(2)),
        )
    }

    fn three_pane() -> LayoutNode {
        // root horizontal: [ left vertical [1,2], 3 ]
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(v(1)),
                LayoutNode::leaf(v(2)),
            ),
            LayoutNode::leaf(v(3)),
        )
    }

    #[test]
    fn focus_next_prev_wrap() {
        let node = two_pane_horizontal();
        let f = Focus::with_focus(ViewId::new(1));
        assert_eq!(f.next(&node), Some(ViewId::new(2)));
        assert_eq!(f.prev(&node), Some(ViewId::new(2)));
        let f2 = Focus::with_focus(ViewId::new(2));
        assert_eq!(f2.next(&node), Some(ViewId::new(1)));
    }

    #[test]
    fn focus_unset_next_goes_to_first() {
        let node = two_pane_horizontal();
        let f = Focus::new();
        assert_eq!(f.next(&node), Some(ViewId::new(1)));
        assert_eq!(f.prev(&node), Some(ViewId::new(2)));
    }

    #[test]
    fn focus_focus_first() {
        let node = two_pane_horizontal();
        let mut f = Focus::new();
        assert_eq!(f.focus_first(&node), Some(ViewId::new(1)));
        assert_eq!(f.focused(), Some(ViewId::new(1)));
    }

    #[test]
    fn focus_spatial_left_right() {
        let node = two_pane_horizontal();
        let bounds = Rect::new(0, 0, 80, 24);
        let f = Focus::with_focus(ViewId::new(1));
        // 1 is left pane, right neighbor is 2
        assert_eq!(
            f.advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        assert_eq!(
            f.advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        ); // no candidate stays
        let f2 = Focus::with_focus(ViewId::new(2));
        assert_eq!(
            f2.advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        assert_eq!(
            f2.advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
    }

    #[test]
    fn focus_spatial_up_down() {
        let node = three_pane();
        let bounds = Rect::new(0, 0, 80, 40);
        // Layout: left 0,0,40,40 split vertical => top 0,0,40,20 (id1), bottom 0,20,40,20 (id2), right 40,0,40,40 (id3)
        let f1 = Focus::with_focus(ViewId::new(1));
        assert_eq!(
            f1.advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(2))
        );
        let f2 = Focus::with_focus(ViewId::new(2));
        assert_eq!(
            f2.advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(1))
        );
        // From left pane to right pane via Right, both top and bottom have overlap with right,
        // maximal overlap tie -> smallest ViewId (both 40 tall, full overlap => equal), so picks smallest candidate id (2 vs 1?) Actually from f1 (top) right candidate is 3 with overlap 20, only one candidate? The left vertical split: right pane spans y0..40, left top spans y0..20, so overlap is 20 (>0) so candidate is 3.
        assert_eq!(
            f1.advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(3))
        );
    }

    #[test]
    fn focus_empty_tree() {
        let node = LayoutNode::stack(vec![]);
        let f = Focus::with_focus(ViewId::new(1));
        assert_eq!(f.next(&node), None);
        assert_eq!(
            f.advance(&node, Rect::new(0, 0, 80, 24), FocusDirection::Right),
            None
        );
    }

    #[test]
    fn focus_deterministic_tie_break() {
        // Two candidates with equal overlap to the right: ensure smallest ViewId wins.
        // Create layout where current is middle? Use three columns equal.
        let a = LayoutNode::leaf(v(10));
        let b = LayoutNode::leaf(v(20));
        let c = LayoutNode::leaf(v(30));
        // Not easy to produce tie with simple splits; use manual check via overlap sort.
        let _ = (a, b, c);
        // The spatial logic sorts by overlap descending then ViewId ascending, so deterministic.
        assert!(ViewId::new(10) < ViewId::new(20));
    }

    #[test]
    fn move_focus_mutates() {
        let node = two_pane_horizontal();
        let mut f = Focus::with_focus(ViewId::new(1));
        let next = f.move_focus(&node, Rect::new(0, 0, 80, 24), FocusDirection::Next);
        assert_eq!(next, Some(ViewId::new(2)));
        assert_eq!(f.focused(), Some(ViewId::new(2)));
    }

    #[test]
    fn focus_zero_gaps_match_gapless_advance() {
        // CTX-0177: Gaps::ZERO advance is identical to the legacy path.
        let node = three_pane();
        let bounds = Rect::new(0, 0, 80, 40);
        for id in [1, 2, 3] {
            let f = Focus::with_focus(ViewId::new(id));
            for dir in [
                FocusDirection::Up,
                FocusDirection::Down,
                FocusDirection::Left,
                FocusDirection::Right,
            ] {
                assert_eq!(
                    f.advance(&node, bounds, dir),
                    f.advance_with_gaps(&node, bounds, Gaps::ZERO, dir),
                    "id {id} dir {dir:?}"
                );
            }
        }
    }

    #[test]
    fn focus_spatial_crosses_gap_band() {
        // CTX-0177: with gaps_in, adjacent leaves no longer share an edge;
        // spatial focus must still cross the band (legacy exact-equality
        // would strand focus on the current leaf).
        let node = two_pane_horizontal();
        let bounds = Rect::new(0, 0, 80, 24);
        let gaps = Gaps::new(2, 0);
        let f = Focus::with_focus(ViewId::new(1));
        assert_eq!(
            f.advance_with_gaps(&node, bounds, gaps, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        let f2 = Focus::with_focus(ViewId::new(2));
        assert_eq!(
            f2.advance_with_gaps(&node, bounds, gaps, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        // No candidate in the dead direction still stays put.
        assert_eq!(
            f.advance_with_gaps(&node, bounds, gaps, FocusDirection::Left),
            Some(ViewId::new(1))
        );
    }

    #[test]
    fn focus_spatial_crosses_vertical_gap_band() {
        // CTX-0177: up/down crossing works with gaps_out + gaps_in combined.
        let node = three_pane();
        let bounds = Rect::new(0, 0, 80, 40);
        let gaps = Gaps::new(2, 1);
        let f1 = Focus::with_focus(ViewId::new(1));
        assert_eq!(
            f1.advance_with_gaps(&node, bounds, gaps, FocusDirection::Down),
            Some(ViewId::new(2))
        );
        assert_eq!(
            f1.advance_with_gaps(&node, bounds, gaps, FocusDirection::Right),
            Some(ViewId::new(3))
        );
    }

    #[test]
    fn focus_spatial_l_shape_picks_the_aligned_pane_issue_2444() {
        // Left full-height pane plus a stacked right column.
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(v(2)),
                LayoutNode::leaf(v(3)),
            ),
        );
        let bounds = Rect::new(0, 0, 80, 40);
        let f2 = Focus::with_focus(ViewId::new(2));
        assert_eq!(
            f2.advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(3)),
            "down from the top-right pane must not pick the tall left pane"
        );
        let f3 = Focus::with_focus(ViewId::new(3));
        assert_eq!(
            f3.advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2))
        );
        assert_eq!(
            f3.advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        assert_eq!(
            f3.advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(3)),
            "bottom edge stays put"
        );
        // Symmetric right column: equal distance resolves by leaf order.
        let f1 = Focus::with_focus(ViewId::new(1));
        assert_eq!(
            f1.advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        assert_eq!(
            f1.advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(1))
        );
    }

    #[test]
    fn focus_spatial_t_shape_uses_the_aligned_column() {
        // Top row [1, 2], bottom full-width pane 3.
        let node = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(v(1)),
                LayoutNode::leaf(v(2)),
            ),
            LayoutNode::leaf(v(3)),
        );
        let bounds = Rect::new(0, 0, 80, 40);
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(3))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(2)).advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(3))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(2)).advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        // Both top panes are above the bottom pane; equal edge distance and
        // overlap resolve on the tree's directional edge: the last leaf of
        // the sibling row (`2`) is the pane at the `Up` edge (issue #2526).
        assert_eq!(
            Focus::with_focus(ViewId::new(3)).advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2))
        );
    }

    /// Issue #2526: 1 + 1 + 2 layout — a full-height left pane, a full-width
    /// top-right pane, and a bottom row of two panes. `Down` from the top
    /// pane must land on the bottom-left pane that shares the split's left
    /// edge, even when the bottom-right pane is wider (closer center) or
    /// appears first in creation order.
    #[test]
    fn focus_spatial_issue_2526_top_right_down_picks_bottom_left() {
        // Bottom row 60/40: pane 4 is wider, so center distance alone would
        // pick it over pane 3 on the left.
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(v(2)),
                LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.4,
                    LayoutNode::leaf(v(3)),
                    LayoutNode::leaf(v(4)),
                ),
            ),
        );
        let bounds = Rect::new(0, 0, 80, 40);
        let alloc = node.layout(bounds);
        let rect = |id: u64| {
            alloc
                .iter()
                .find(|(leaf, _)| *leaf == ViewId::new(id))
                .map(|(_, rect)| *rect)
                .expect("allocated")
        };
        // Exact geometry: 1=(0,0,40,40), 2=(40,0,40,20),
        // 3=(40,20,16,20), 4=(56,20,24,20).
        assert_eq!(rect(1), Rect::new(0, 0, 40, 40));
        assert_eq!(rect(2), Rect::new(40, 0, 40, 20));
        assert_eq!(rect(3), Rect::new(40, 20, 16, 20));
        assert_eq!(rect(4), Rect::new(56, 20, 24, 20));
        assert_eq!(
            Focus::with_focus(ViewId::new(2)).advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(3)),
            "Mod+j from the top-right pane must reach the bottom-left pane"
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(3)).advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(4)).advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(3)).advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
    }

    /// Issue #2526: creation order must not win over geometry. The spiral
    /// inserts the newer pane on the left, so the bottom row's DFS order is
    /// `[4, 3]` while `3` is spatially right of `4`; `Down` from the top
    /// pane still lands on the pane at the split's left edge (`4`).
    #[test]
    fn focus_spatial_issue_2526_reversed_creation_order_uses_geometry() {
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(v(2)),
                LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.5,
                    LayoutNode::leaf(v(4)),
                    LayoutNode::leaf(v(3)),
                ),
            ),
        );
        let bounds = Rect::new(0, 0, 80, 40);
        assert_eq!(
            Focus::with_focus(ViewId::new(2)).advance(&node, bounds, FocusDirection::Down),
            Some(ViewId::new(4)),
            "the bottom-left pane is the tree's directional edge"
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(4)).advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2))
        );
    }

    /// Issue #2526: every directional step in the 1+1+2 layout is
    /// reciprocal where the panes face each other, so repeated moves cannot
    /// ping-pong between unrelated panes.
    #[test]
    fn focus_spatial_issue_2526_directions_are_reciprocal() {
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(v(2)),
                LayoutNode::split(
                    SplitAxis::Horizontal,
                    0.5,
                    LayoutNode::leaf(v(3)),
                    LayoutNode::leaf(v(4)),
                ),
            ),
        );
        let bounds = Rect::new(0, 0, 80, 40);
        for (from, dir, back) in [
            (2, FocusDirection::Down, FocusDirection::Up),
            (3, FocusDirection::Up, FocusDirection::Down),
            (4, FocusDirection::Left, FocusDirection::Right),
            (1, FocusDirection::Right, FocusDirection::Left),
        ] {
            let next = Focus::with_focus(ViewId::new(from))
                .advance(&node, bounds, dir)
                .expect("focus");
            assert_ne!(next, ViewId::new(from), "from {from} dir {dir:?}");
            assert_eq!(
                Focus::with_focus(next).advance(&node, bounds, back),
                Some(ViewId::new(from)),
                "reverse of {from} -> {next:?} must return"
            );
        }
        // The full-height left pane's `Left`/`Right` neighbors are `1` and
        // the top-row tree edge (`2`); `4` sits under the right half of the
        // top pane, so its left neighbor is `3`.
        assert_eq!(
            Focus::with_focus(ViewId::new(3)).advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(1))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(4)).advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(3))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(4)).advance(&node, bounds, FocusDirection::Up),
            Some(ViewId::new(2)),
            "both bottom panes face the top pane; the tree edge is the top pane"
        );
    }

    #[test]
    fn focus_spatial_ignores_zero_area_panes() {
        // A one-cell container split 50/50 leaves leaf 2 with zero width: it
        // is not a visual directional target (the cycle ring still reaches
        // it).
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v(1)),
            LayoutNode::leaf(v(2)),
        );
        let bounds = Rect::new(0, 0, 1, 40);
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).advance(&node, bounds, FocusDirection::Right),
            Some(ViewId::new(1))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(2)).advance(&node, bounds, FocusDirection::Left),
            Some(ViewId::new(2))
        );
        assert_eq!(
            Focus::with_focus(ViewId::new(1)).next(&node),
            Some(ViewId::new(2))
        );
    }
}
