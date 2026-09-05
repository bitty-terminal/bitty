//! Layout algebra: `LayoutNode` tree, deterministic solver, and resize/reflow helpers.
//!
//! ADR-0003 role: the UI crate owns View/LayoutNode/split/stack/overlay/focus/resize.
//! This module provides pure, headless, deterministic layout computation: given a
//! container `Rect` and a `LayoutNode` tree, `LayoutNode::layout` produces an
//! allocation of `Rect`s to leaf views with no gaps or overlaps (except Stack/Overlay
//! semantics), while `LayoutNode::layout_with_gaps` inserts Hyprland-like
//! `gaps_in`/`gaps_out` bands (CTX-0177). Split ratios are clamped and the solver
//! is total over all inputs.

#![forbid(unsafe_code)]

use crate::geometry::{Gaps, Rect, SplitAxis};
use crate::view::{View, ViewId};

/// Layout tree for terminal panes.
///
/// Each leaf holds an owned `View`. Interiors describe composition:
///
/// - `Split`: divides the container along `axis` according to `ratio` (fraction for
///   the `first` child; `second` gets the remainder). Ratio is clamped to
///   `[MIN_RATIO, MAX_RATIO]` and the split is deterministic over integers.
/// - `Stack`: tab-like stacking where every child occupies the full container bounds;
///   the last element is considered top-most for focus/visual order.
/// - `Overlay`: a base layer plus a floating overlay clipped to the container.
///
/// The tree is deterministic: the same `LayoutNode` laid out in the same
/// `Rect` always produces the same allocation, independent of platform or HashMap ordering.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutNode {
    /// Terminal view leaf.
    Leaf(View),
    /// Binary split.
    Split {
        /// Left/right vs top/bottom.
        axis: SplitAxis,
        /// Fraction for `first` child in `(0.0, 1.0)`; clamped on construction/set.
        ratio: f32,
        /// First child (left or top).
        first: Box<LayoutNode>,
        /// Second child (right or bottom).
        second: Box<LayoutNode>,
    },
    /// Stacked children sharing the same bounds (e.g., tabs).
    Stack(Vec<LayoutNode>),
    /// Base plus overlay.
    Overlay {
        /// Underlying content (full container).
        base: Box<LayoutNode>,
        /// Overlay content.
        overlay: Box<LayoutNode>,
        /// Desired overlay bounds in container-local coordinates (clipped to container).
        bounds: Rect,
    },
}

impl LayoutNode {
    /// Minimum split ratio (prevents collapsing a pane below ~10% of container).
    pub const MIN_RATIO: f32 = 0.10;
    /// Maximum split ratio.
    pub const MAX_RATIO: f32 = 0.90;

    /// Creates a leaf node.
    #[must_use]
    pub fn leaf(view: View) -> Self {
        Self::Leaf(view)
    }

    /// Creates a split, clamping `ratio` to `[MIN_RATIO, MAX_RATIO]`.
    #[must_use]
    pub fn split(axis: SplitAxis, ratio: f32, first: LayoutNode, second: LayoutNode) -> Self {
        Self::Split {
            axis,
            ratio: clamp_ratio(ratio),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Creates a stack. Empty stacks are allowed (total solver) but contain no leaves.
    #[must_use]
    pub fn stack(children: Vec<LayoutNode>) -> Self {
        Self::Stack(children)
    }

    /// Creates an overlay.
    #[must_use]
    pub fn overlay(base: LayoutNode, overlay: LayoutNode, bounds: Rect) -> Self {
        Self::Overlay {
            base: Box::new(base),
            overlay: Box::new(overlay),
            bounds,
        }
    }

    /// Number of leaf views in this subtree.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
            Self::Stack(children) => children.iter().map(|c| c.leaf_count()).sum(),
            Self::Overlay { base, overlay, .. } => base.leaf_count() + overlay.leaf_count(),
        }
    }

    /// Leaf ids in deterministic depth-first order (left/top first).
    #[must_use]
    pub fn leaf_ids(&self) -> Vec<ViewId> {
        let mut out = Vec::with_capacity(self.leaf_count());
        self.collect_leaf_ids(&mut out);
        out
    }

    fn collect_leaf_ids(&self, out: &mut Vec<ViewId>) {
        match self {
            Self::Leaf(v) => out.push(v.id()),
            Self::Split { first, second, .. } => {
                first.collect_leaf_ids(out);
                second.collect_leaf_ids(out);
            }
            Self::Stack(children) => {
                for c in children {
                    c.collect_leaf_ids(out);
                }
            }
            Self::Overlay { base, overlay, .. } => {
                base.collect_leaf_ids(out);
                overlay.collect_leaf_ids(out);
            }
        }
    }

    /// Finds a leaf by id.
    #[must_use]
    pub fn find_leaf(&self, id: ViewId) -> Option<&View> {
        match self {
            Self::Leaf(v) if v.id() == id => Some(v),
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                first.find_leaf(id).or_else(|| second.find_leaf(id))
            }
            Self::Stack(children) => {
                for c in children {
                    if let Some(v) = c.find_leaf(id) {
                        return Some(v);
                    }
                }
                None
            }
            Self::Overlay { base, overlay, .. } => {
                base.find_leaf(id).or_else(|| overlay.find_leaf(id))
            }
        }
    }

    /// Mutable leaf lookup.
    #[must_use]
    pub fn find_leaf_mut(&mut self, id: ViewId) -> Option<&mut View> {
        match self {
            Self::Leaf(v) if v.id() == id => Some(v),
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                if let Some(v) = first.find_leaf_mut(id) {
                    Some(v)
                } else {
                    second.find_leaf_mut(id)
                }
            }
            Self::Stack(children) => {
                for c in children {
                    if let Some(v) = c.find_leaf_mut(id) {
                        return Some(v);
                    }
                }
                None
            }
            Self::Overlay { base, overlay, .. } => {
                if let Some(v) = base.find_leaf_mut(id) {
                    Some(v)
                } else {
                    overlay.find_leaf_mut(id)
                }
            }
        }
    }

    /// Updates the split ratio at `path`. `path` is a sequence of child indices
    /// where for `Split` 0 = `first`, 1 = `second`; for `Stack` it indexes the
    /// stacked children; for `Overlay` 0 = `base`, 1 = `overlay`. Returns `true`
    /// when a split was found and updated.
    pub fn set_split_ratio_at(&mut self, path: &[usize], new_ratio: f32) -> bool {
        let clamped = clamp_ratio(new_ratio);
        self.set_split_ratio_at_inner(path, clamped)
    }

    fn set_split_ratio_at_inner(&mut self, path: &[usize], clamped: f32) -> bool {
        if path.is_empty() {
            if let Self::Split { ratio, .. } = self {
                *ratio = clamped;
                return true;
            }
            return false;
        }
        let idx = path[0];
        let rest = &path[1..];
        match self {
            Self::Split { first, second, .. } => match idx {
                0 => first.set_split_ratio_at_inner(rest, clamped),
                1 => second.set_split_ratio_at_inner(rest, clamped),
                _ => false,
            },
            Self::Stack(children) => {
                if let Some(child) = children.get_mut(idx) {
                    child.set_split_ratio_at_inner(rest, clamped)
                } else {
                    false
                }
            }
            Self::Overlay { base, overlay, .. } => match idx {
                0 => base.set_split_ratio_at_inner(rest, clamped),
                1 => overlay.set_split_ratio_at_inner(rest, clamped),
                _ => false,
            },
            Self::Leaf(_) => false,
        }
    }

    /// Deterministic layout solver.
    ///
    /// Given `bounds`, returns a vector of `(ViewId, Rect)` allocations, one per
    /// leaf, in deterministic depth-first order. The solver is total: empty spaces
    /// are handled, zero-sized containers produce zero-sized leaves, and split
    /// arithmetic never panics.
    ///
    /// Gapless (zero [`Gaps`]): leaves tile edge-to-edge with no gaps or
    /// overlaps (except Stack/Overlay semantics).
    #[must_use]
    pub fn layout(&self, bounds: Rect) -> Vec<(ViewId, Rect)> {
        self.layout_with_gaps(bounds, Gaps::ZERO)
    }

    /// Deterministic layout solver with Hyprland-like gaps (CTX-0177).
    ///
    /// `gaps.outer` (`gaps_out`) insets `bounds` once; `gaps.inner`
    /// (`gaps_in`) reserves a background-colored band between sibling panes
    /// at every `Split` (nested splits each insert their own band, matching
    /// Hyprland). `Stack` children share the full inset bounds (only one is
    /// visible); `Overlay` bases fill the inset bounds while the overlay
    /// subtree is laid out inside its clipped rect with inner gaps only
    /// (positioned chrome keeps its explicit position).
    ///
    /// With [`Gaps::ZERO`] this is bit-identical to [`Self::layout`]. The
    /// solver stays total: oversized gaps collapse leaves to zero-size rects
    /// rather than panicking.
    #[must_use]
    pub fn layout_with_gaps(&self, bounds: Rect, gaps: Gaps) -> Vec<(ViewId, Rect)> {
        let mut out = Vec::with_capacity(self.leaf_count());
        let inner = gaps.inset_outer(bounds);
        self.layout_inner(inner, gaps.inner, &mut out);
        out
    }

    /// Gap-core recursion: `gap_in` applies at every `Split` below this
    /// point (the outer inset is consumed once by [`Self::layout_with_gaps`]).
    fn layout_inner(&self, bounds: Rect, gap_in: u16, out: &mut Vec<(ViewId, Rect)>) {
        match self {
            Self::Leaf(v) => {
                out.push((v.id(), bounds));
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_rect_with_gap(bounds, *axis, *ratio, gap_in);
                first.layout_inner(a, gap_in, out);
                second.layout_inner(b, gap_in, out);
            }
            Self::Stack(children) => {
                for child in children {
                    child.layout_inner(bounds, gap_in, out);
                }
            }
            Self::Overlay {
                base,
                overlay,
                bounds: overlay_bounds,
            } => {
                base.layout_inner(bounds, gap_in, out);
                // Overlay desired bounds are container-relative and clipped.
                let clipped = if let Some(inter) = overlay_bounds.clip_to(bounds) {
                    inter
                } else {
                    // Overlay completely outside container -> empty allocation clipped to container.
                    Rect::zero()
                };
                overlay.layout_inner(clipped, gap_in, out);
            }
        }
    }

    /// Reflow primitive: mutates leaf `View`s so their `cols`/`rows` and
    /// `origin` match the allocation of `self` in `container`.
    ///
    /// This is the primary resize helper: call after a window resize to update
    /// all views deterministically to the new allocation.
    pub fn reflow(&mut self, container: Rect) {
        self.reflow_with_gaps(container, Gaps::ZERO);
    }

    /// Gap-aware reflow (CTX-0177): like [`Self::reflow`] but allocates with
    /// [`Self::layout_with_gaps`], so leaf origins/sizes already exclude the
    /// gap bands. With [`Gaps::ZERO`] this is identical to [`Self::reflow`].
    pub fn reflow_with_gaps(&mut self, container: Rect, gaps: Gaps) {
        let allocations = self.layout_with_gaps(container, gaps);
        for (id, rect) in allocations {
            if let Some(view) = self.find_leaf_mut(id) {
                view.reflow_to_rect(rect);
            }
        }
    }

    /// Convenience: reflow with `Size`.
    pub fn reflow_size(&mut self, size: crate::geometry::Size) {
        self.reflow(Rect::new(0, 0, size.width, size.height));
    }

    /// Resize helper that preserves split ratios while changing the container
    /// size. Equivalent to `reflow` but also returns the new allocation list.
    #[must_use]
    pub fn resize(&mut self, container: Rect) -> Vec<(ViewId, Rect)> {
        self.resize_with_gaps(container, Gaps::ZERO)
    }

    /// Gap-aware resize (CTX-0177): like [`Self::resize`] but allocates with
    /// [`Self::layout_with_gaps`]. With [`Gaps::ZERO`] this is identical to
    /// [`Self::resize`].
    #[must_use]
    pub fn resize_with_gaps(&mut self, container: Rect, gaps: Gaps) -> Vec<(ViewId, Rect)> {
        self.reflow_with_gaps(container, gaps);
        self.layout_with_gaps(container, gaps)
    }

    /// Returns true when the node is a leaf.
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf(_))
    }
}

/// Clamps a raw ratio to `[MIN_RATIO, MAX_RATIO]`.
#[must_use]
pub fn clamp_ratio(raw: f32) -> f32 {
    if !raw.is_finite() {
        return 0.5;
    }
    raw.clamp(LayoutNode::MIN_RATIO, LayoutNode::MAX_RATIO)
}

/// Deterministic split of `bounds` according to `axis` and `ratio`.
///
/// Returns `(first_rect, second_rect)` that exactly partition `bounds` without
/// gaps or overlap (widths/heights sum to parent's). Uses `floor` on the
/// ratio product and clamps inner sizes to `[1, total-1]` when total >= 2,
/// otherwise preserves total for the first pane (second gets remainder which
/// may be zero). This is deterministic across platforms for the same `f32` bits.
///
/// Equivalent to [`split_rect_with_gap`] with a zero gap.
#[must_use]
pub fn split_rect(bounds: Rect, axis: SplitAxis, ratio: f32) -> (Rect, Rect) {
    split_rect_with_gap(bounds, axis, ratio, 0)
}

/// Deterministic split with a Hyprland-like inner gap band (CTX-0177).
///
/// Like [`split_rect`], but reserves `gap_in` cells between the children for
/// the window background: `first + gap + second` spans `bounds` along the
/// split axis. The ratio applies to the gap-subtracted space, so a 50/50
/// split stays symmetric. With `gap_in == 0` this is bit-identical to
/// [`split_rect`].
///
/// Total over all inputs: oversized gaps saturate (leaves collapse to
/// zero-size rather than panic), empty bounds yield zero rects.
#[must_use]
pub fn split_rect_with_gap(bounds: Rect, axis: SplitAxis, ratio: f32, gap_in: u16) -> (Rect, Rect) {
    if bounds.is_empty() {
        return (Rect::zero(), Rect::zero());
    }
    let r = clamp_ratio(ratio);
    match axis {
        SplitAxis::Horizontal => {
            let total = bounds.width as u32;
            if total < 2 {
                // Cannot split meaningfully; give all to first.
                let a = Rect::new(bounds.x, bounds.y, bounds.width, bounds.height);
                let b = Rect::new(
                    bounds.x.saturating_add(bounds.width),
                    bounds.y,
                    0,
                    bounds.height,
                );
                return (a, b);
            }
            // Reserve the gap first; leaves share what remains (at least 1
            // cell stays addressable so the solver never inverts).
            let gap = (u32::from(gap_in)).min(total.saturating_sub(1));
            let avail = total - gap;
            if avail < 2 {
                let a = Rect::new(bounds.x, bounds.y, 1, bounds.height);
                let b = Rect::new(
                    bounds
                        .x
                        .saturating_add(1)
                        .saturating_add(gap.min(u32::from(u16::MAX)) as u16),
                    bounds.y,
                    0,
                    bounds.height,
                );
                return (a, b);
            }
            let first_w = {
                let raw = (avail as f32 * r).floor() as u32;
                // Clamp to [1, avail-1] so neither pane collapses.
                raw.clamp(1, avail - 1) as u16
            };
            let second_w = (avail - u32::from(first_w)) as u16;
            let a = Rect::new(bounds.x, bounds.y, first_w, bounds.height);
            let b = Rect::new(
                bounds
                    .x
                    .saturating_add(first_w)
                    .saturating_add(gap.min(u32::from(u16::MAX)) as u16),
                bounds.y,
                second_w,
                bounds.height,
            );
            (a, b)
        }
        SplitAxis::Vertical => {
            let total = bounds.height as u32;
            if total < 2 {
                let a = Rect::new(bounds.x, bounds.y, bounds.width, bounds.height);
                let b = Rect::new(
                    bounds.x,
                    bounds.y.saturating_add(bounds.height),
                    bounds.width,
                    0,
                );
                return (a, b);
            }
            let gap = (u32::from(gap_in)).min(total.saturating_sub(1));
            let avail = total - gap;
            if avail < 2 {
                let a = Rect::new(bounds.x, bounds.y, bounds.width, 1);
                let b = Rect::new(
                    bounds.x,
                    bounds
                        .y
                        .saturating_add(1)
                        .saturating_add(gap.min(u32::from(u16::MAX)) as u16),
                    bounds.width,
                    0,
                );
                return (a, b);
            }
            let first_h = {
                let raw = (avail as f32 * r).floor() as u32;
                raw.clamp(1, avail - 1) as u16
            };
            let second_h = (avail - u32::from(first_h)) as u16;
            let a = Rect::new(bounds.x, bounds.y, bounds.width, first_h);
            let b = Rect::new(
                bounds.x,
                bounds
                    .y
                    .saturating_add(first_h)
                    .saturating_add(gap.min(u32::from(u16::MAX)) as u16),
                bounds.width,
                second_h,
            );
            (a, b)
        }
    }
}

/// Helper used by `focus` for deterministic leaf adjacency.
#[must_use]
pub fn overlap_len(a_start: u32, a_len: u32, b_start: u32, b_len: u32) -> u32 {
    let a_end = a_start + a_len;
    let b_end = b_start + b_len;
    let left = a_start.max(b_start);
    let right = a_end.min(b_end);
    right.saturating_sub(left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::view::{View, ViewId};

    fn view(id: u64, cols: usize, rows: usize) -> View {
        View::new(ViewId::new(id), cols, rows)
    }

    #[test]
    fn split_horizontal_covers_exactly() {
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 10, 10)),
            LayoutNode::leaf(view(2, 10, 10)),
        );
        let alloc = node.layout(bounds);
        assert_eq!(alloc.len(), 2);
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        assert_eq!(a.width + b.width, 80);
        assert_eq!(a.height, 24);
        assert_eq!(b.height, 24);
        assert_eq!(a.x, 0);
        assert_eq!(b.x, a.width);
    }

    #[test]
    fn split_vertical_with_ratio_floor() {
        let bounds = Rect::new(0, 0, 10, 10);
        let node = LayoutNode::split(
            SplitAxis::Vertical,
            0.3,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout(bounds);
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        assert_eq!(a.height, 3);
        assert_eq!(b.height, 7);
        assert_eq!(a.height + b.height, 10);
    }

    #[test]
    fn split_ratio_clamped() {
        let n = LayoutNode::split(
            SplitAxis::Horizontal,
            5.0,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        if let LayoutNode::Split { ratio, .. } = n {
            assert_eq!(ratio, LayoutNode::MAX_RATIO);
        } else {
            panic!("expected split");
        }
        let n2 = LayoutNode::split(
            SplitAxis::Horizontal,
            f32::NAN,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        if let LayoutNode::Split { ratio, .. } = n2 {
            assert_eq!(ratio, 0.5);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn stack_all_share_bounds() {
        let bounds = Rect::new(1, 2, 10, 10);
        let node = LayoutNode::stack(vec![
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            LayoutNode::leaf(view(3, 1, 1)),
        ]);
        let alloc = node.layout(bounds);
        assert_eq!(alloc.len(), 3);
        for (_, r) in alloc {
            assert_eq!(r, bounds);
        }
    }

    #[test]
    fn overlay_base_and_clipped_overlay() {
        let bounds = Rect::new(0, 0, 80, 24);
        let base = LayoutNode::leaf(view(1, 1, 1));
        let over = LayoutNode::leaf(view(2, 1, 1));
        let node = LayoutNode::overlay(base, over, Rect::new(10, 5, 20, 10));
        let alloc = node.layout(bounds);
        assert_eq!(alloc.len(), 2);
        let (_, b) = alloc[0];
        let (_, o) = alloc[1];
        assert_eq!(b, bounds);
        assert_eq!(o, Rect::new(10, 5, 20, 10));
        // Overlay exceeding container is clipped.
        let node2 = LayoutNode::overlay(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            Rect::new(70, 20, 20, 10),
        );
        let alloc2 = node2.layout(bounds);
        let (_, o2) = alloc2[1];
        assert_eq!(o2, Rect::new(70, 20, 10, 4));
    }

    #[test]
    fn nested_splits_deterministic() {
        let bounds = Rect::new(0, 0, 100, 100);
        let left_top = LayoutNode::leaf(view(1, 1, 1));
        let left_bot = LayoutNode::leaf(view(2, 1, 1));
        let left = LayoutNode::split(SplitAxis::Vertical, 0.5, left_top, left_bot);
        let right = LayoutNode::leaf(view(3, 1, 1));
        let root = LayoutNode::split(SplitAxis::Horizontal, 0.4, left, right);
        let alloc = root.layout(bounds);
        assert_eq!(alloc.len(), 3);
        // first split 0.4 of 100 = 40
        assert_eq!(alloc[0].1.width, 40);
        assert_eq!(alloc[0].1.height, 50);
        assert_eq!(alloc[1].1.width, 40);
        assert_eq!(alloc[2].1.width, 60);
        // determinism check: second run identical
        let alloc2 = root.layout(bounds);
        assert_eq!(alloc, alloc2);
    }

    #[test]
    fn reflow_updates_view_sizes() {
        let mut root = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 80, 24)),
            LayoutNode::leaf(view(2, 80, 24)),
        );
        let bounds = Rect::new(0, 0, 100, 50);
        root.reflow(bounds);
        let v1 = root.find_leaf(ViewId::new(1)).unwrap();
        let v2 = root.find_leaf(ViewId::new(2)).unwrap();
        assert_eq!(v1.cols(), 50);
        assert_eq!(v2.cols(), 50);
        assert_eq!(v1.origin().x, 0);
        assert_eq!(v2.origin().x, 50);
    }

    #[test]
    fn set_split_ratio_at_path() {
        let mut root = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(view(1, 1, 1)),
                LayoutNode::leaf(view(2, 1, 1)),
            ),
            LayoutNode::leaf(view(3, 1, 1)),
        );
        assert!(root.set_split_ratio_at(&[0], 0.7));
        if let LayoutNode::Split { first, .. } = &root {
            if let LayoutNode::Split { ratio, .. } = first.as_ref() {
                assert!((ratio - 0.7).abs() < f32::EPSILON);
            } else {
                panic!("inner not split");
            }
        }
        assert!(!root.set_split_ratio_at(&[5], 0.5));
        assert!(!root.set_split_ratio_at(&[], 0.5) || matches!(root, LayoutNode::Split { .. }));
    }

    #[test]
    fn leaf_count_and_ids_order() {
        let node = LayoutNode::stack(vec![
            LayoutNode::leaf(view(2, 1, 1)),
            LayoutNode::leaf(view(1, 1, 1)),
        ]);
        // Stack preserves insertion order; leaf_ids is deterministic depth-first.
        assert_eq!(node.leaf_count(), 2);
        assert_eq!(node.leaf_ids(), vec![ViewId::new(2), ViewId::new(1)]);
    }

    #[test]
    fn split_rect_small_total() {
        let bounds = Rect::new(0, 0, 1, 10);
        let (a, b) = split_rect(bounds, SplitAxis::Horizontal, 0.5);
        assert_eq!(a.width, 1);
        assert_eq!(b.width, 0);
    }

    #[test]
    fn split_rect_empty_bounds() {
        let (a, b) = split_rect(Rect::zero(), SplitAxis::Horizontal, 0.5);
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn zero_gaps_match_gapless_solver() {
        // CTX-0177: Gaps::ZERO must be bit-identical to the legacy solver so
        // existing users see zero change.
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Vertical,
                0.3,
                LayoutNode::leaf(view(1, 1, 1)),
                LayoutNode::leaf(view(2, 1, 1)),
            ),
            LayoutNode::leaf(view(3, 1, 1)),
        );
        assert_eq!(
            node.layout(bounds),
            node.layout_with_gaps(bounds, Gaps::ZERO)
        );
        let (a, b) = split_rect(bounds, SplitAxis::Horizontal, 0.5);
        let (ga, gb) = split_rect_with_gap(bounds, SplitAxis::Horizontal, 0.5, 0);
        assert_eq!((a, b), (ga, gb));
        let (c, d) = split_rect(bounds, SplitAxis::Vertical, 0.3);
        let (gc, gd) = split_rect_with_gap(bounds, SplitAxis::Vertical, 0.3, 0);
        assert_eq!((c, d), (gc, gd));
    }

    #[test]
    fn gaps_out_insets_all_leaves() {
        // CTX-0177: gaps_out shrinks the container once; a single leaf fills
        // the inset rect.
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::leaf(view(1, 1, 1));
        let alloc = node.layout_with_gaps(bounds, Gaps::new(0, 2));
        assert_eq!(alloc.len(), 1);
        assert_eq!(alloc[0].1, Rect::new(2, 2, 76, 20));
    }

    #[test]
    fn gaps_in_reserves_background_band_between_siblings() {
        // CTX-0177: gaps_in splits the gap-subtracted space; first + gap +
        // second spans the container and the ratio stays symmetric.
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout_with_gaps(bounds, Gaps::new(2, 0));
        assert_eq!(alloc.len(), 2);
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        // 80 - 2 gap = 78 shared 39/39.
        assert_eq!(a, Rect::new(0, 0, 39, 24));
        assert_eq!(b, Rect::new(41, 0, 39, 24));
        // The band between them belongs to no leaf (background shows through).
        assert_eq!(b.x, a.x + a.width + 2);
        assert_eq!(a.width + 2 + b.width, 80);
    }

    #[test]
    fn gaps_in_vertical_split() {
        // CTX-0177: vertical splits reserve the band along the y axis.
        let bounds = Rect::new(0, 0, 10, 10);
        let node = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout_with_gaps(bounds, Gaps::new(2, 0));
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        // 10 - 2 = 8 shared 4/4.
        assert_eq!(a, Rect::new(0, 0, 10, 4));
        assert_eq!(b, Rect::new(0, 6, 10, 4));
        assert_eq!(b.y, a.y + a.height + 2);
    }

    #[test]
    fn gaps_compose_outer_then_inner() {
        // CTX-0177: outer insets the container, then inner splits the rest.
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout_with_gaps(bounds, Gaps::new(2, 1));
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        // Inset to (1,1,78,22); 78 - 2 = 76 shared 38/38.
        assert_eq!(a, Rect::new(1, 1, 38, 22));
        assert_eq!(b, Rect::new(41, 1, 38, 22));
    }

    #[test]
    fn gaps_nested_splits_each_insert_band() {
        // CTX-0177: like Hyprland, every Split level inserts its own band.
        let bounds = Rect::new(0, 0, 81, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(view(1, 1, 1)),
                LayoutNode::leaf(view(2, 1, 1)),
            ),
            LayoutNode::leaf(view(3, 1, 1)),
        );
        let alloc = node.layout_with_gaps(bounds, Gaps::new(1, 0));
        assert_eq!(alloc.len(), 3);
        // Outer split: 81 - 1 = 80, floor(80 * 0.5) = 40 left, 40 right.
        // Inner split of the 40-wide left: 40 - 1 = 39, floor(39 * 0.5) = 19.
        assert_eq!(alloc[0].1, Rect::new(0, 0, 19, 24));
        assert_eq!(alloc[1].1, Rect::new(20, 0, 20, 24));
        assert_eq!(alloc[2].1, Rect::new(41, 0, 40, 24));
        // Deterministic across runs.
        assert_eq!(alloc, node.layout_with_gaps(bounds, Gaps::new(1, 0)));
    }

    #[test]
    fn gaps_oversized_collapse_total() {
        // CTX-0177: gaps larger than the container collapse leaves instead of
        // panicking; the solver stays total.
        let bounds = Rect::new(0, 0, 80, 24);
        let node = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout_with_gaps(bounds, Gaps::new(200, 0));
        assert_eq!(alloc.len(), 2);
        for (_, r) in &alloc {
            assert!(r.x as u32 + r.width as u32 <= 80);
        }
        // Outer larger than the container: zero-size leaves.
        let solo = LayoutNode::leaf(view(1, 1, 1));
        let alloc = solo.layout_with_gaps(bounds, Gaps::new(0, 100));
        assert!(alloc[0].1.is_empty());
    }

    #[test]
    fn gaps_reflow_updates_origins_past_gap_bands() {
        // CTX-0177: reflowed leaf origins skip the gap bands so per-leaf
        // rendering translates to the right pixels.
        let mut root = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 80, 24)),
            LayoutNode::leaf(view(2, 80, 24)),
        );
        root.reflow_with_gaps(Rect::new(0, 0, 80, 24), Gaps::new(2, 1));
        let v1 = root.find_leaf(ViewId::new(1)).unwrap();
        let v2 = root.find_leaf(ViewId::new(2)).unwrap();
        assert_eq!(v1.origin().x, 1);
        assert_eq!(v1.cols(), 38);
        assert_eq!(v2.origin().x, 41);
        assert_eq!(v2.cols(), 38);
        // Gapless reflow is unchanged.
        let mut plain = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 80, 24)),
            LayoutNode::leaf(view(2, 80, 24)),
        );
        plain.reflow(Rect::new(0, 0, 80, 24));
        assert_eq!(plain.find_leaf(ViewId::new(2)).unwrap().origin().x, 40);
    }
}
