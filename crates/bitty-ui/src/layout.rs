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

/// Z-index tier for floating overlays (CTX-0217, FIND-0002).
///
/// Tiers order stacked overlays deterministically, lowest paints first
/// (bottom) and highest paints last (top):
///
/// - `Editor`: base-level editor chrome; paints directly above the base node,
///   below all floats.
/// - `Float`: generic floating panes. This is the default tier used by
///   [`LayoutNode::overlay`], so overlays built before tiers existed keep
///   their legacy paint position.
/// - `Popup`: transient popups (completion menus, hovers, context menus)
///   above floats.
/// - `Messages`: notifications and banners; always topmost.
///
/// The enum derives `Ord`, so `Editor < Float < Popup < Messages` and tiers
/// sort with [`slice::sort`] / [`slice::sort_by_key`]. Discriminants are
/// explicit (`0..=3`) to keep the order stable across refactors.
///
/// Conflict rule: overlays on different tiers compose strictly in tier order,
/// independent of construction order. Overlays on the *same* tier resolve by
/// construction order (stable): the later-constructed overlay paints above
/// (after) the earlier one, matching [`LayoutNode::Stack`]'s "last is
/// top-most" convention. [`LayoutNode::overlay_stack`] applies both rules;
/// hand-nested [`LayoutNode::Overlay`] trees keep legacy depth-first paint
/// order (inner before outer) and tiers on such trees are annotations only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OverlayTier {
    /// Base-level editor chrome (bottom, `0`).
    Editor = 0,
    /// Generic floating panes; legacy default (`1`).
    Float = 1,
    /// Transient popups above floats (`2`).
    Popup = 2,
    /// Notifications and banners; always topmost (`3`).
    Messages = 3,
}

impl OverlayTier {
    /// Lowest paint tier ([`OverlayTier::Editor`]).
    pub const BOTTOM: Self = Self::Editor;
    /// Highest paint tier ([`OverlayTier::Messages`]).
    pub const TOP: Self = Self::Messages;
}

impl Default for OverlayTier {
    /// Legacy default: un-tiered overlays behave as [`OverlayTier::Float`].
    fn default() -> Self {
        Self::Float
    }
}

/// One floating layer composed above a base node by
/// [`LayoutNode::overlay_stack`].
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayLayer {
    /// Z-index tier of this layer.
    pub tier: OverlayTier,
    /// Layer content.
    pub node: LayoutNode,
    /// Desired layer bounds in container-local coordinates (clipped to container).
    pub bounds: Rect,
}

impl OverlayLayer {
    /// Creates a floating layer at `tier`.
    #[must_use]
    pub fn new(tier: OverlayTier, node: LayoutNode, bounds: Rect) -> Self {
        Self { tier, node, bounds }
    }
}

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
///   The overlay carries an [`OverlayTier`]; stacked overlays composed with
///   [`LayoutNode::overlay_stack`] paint in tier order (see [`OverlayTier`]
///   for the same-tier conflict rule).
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
        /// Z-index tier of the overlay (see [`OverlayTier`]).
        tier: OverlayTier,
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

    /// Creates an adaptive split with a Hyprland dwindle-style axis chosen from
    /// the container's cell dimensions (CTX-0209, CR-UI-01).
    ///
    /// Wide containers split side-by-side (`SplitAxis::Horizontal`, vertical
    /// divider) and tall containers split stacked (`SplitAxis::Vertical`,
    /// horizontal divider), keeping new panes squarish. Square containers
    /// tie-break to side-by-side, matching Hyprland's
    /// `splitTop = box.h * split_width_multiplier > box.w` (false for a
    /// square at the default multiplier `1.0`).
    ///
    /// Opt-in: [`Self::split`] keeps its explicit axis and is unaffected.
    /// Ratio clamping is identical (delegates to [`Self::split`]).
    #[must_use]
    pub fn smart_split(container: Rect, ratio: f32, first: LayoutNode, second: LayoutNode) -> Self {
        Self::smart_split_with_multiplier(container, ratio, first, second, 1.0)
    }

    /// Like [`Self::smart_split`] with an explicit width multiplier, mirroring
    /// Hyprland's `dwindle:split_width_multiplier`: the container splits
    /// stacked when `height * width_multiplier > width`, side-by-side
    /// otherwise. Non-finite or non-positive multipliers fall back to `1.0`
    /// so the constructor stays total.
    #[must_use]
    pub fn smart_split_with_multiplier(
        container: Rect,
        ratio: f32,
        first: LayoutNode,
        second: LayoutNode,
        width_multiplier: f32,
    ) -> Self {
        Self::split(
            smart_split_axis(container, width_multiplier),
            ratio,
            first,
            second,
        )
    }

    /// Creates a stack. Empty stacks are allowed (total solver) but contain no leaves.
    #[must_use]
    pub fn stack(children: Vec<LayoutNode>) -> Self {
        Self::Stack(children)
    }

    /// Creates an overlay at the legacy default tier ([`OverlayTier::Float`]).
    ///
    /// Tiering is annotation-only for a single overlay: the solver output is
    /// byte-identical to [`Self::overlay_tiered`] with [`OverlayTier::Float`].
    #[must_use]
    pub fn overlay(base: LayoutNode, overlay: LayoutNode, bounds: Rect) -> Self {
        Self::overlay_tiered(base, overlay, bounds, OverlayTier::default())
    }

    /// Creates an overlay at an explicit [`OverlayTier`].
    ///
    /// The tier does not change this node's geometry or its position in the
    /// solver output; it orders the overlay against sibling tiers when the
    /// tree is composed with [`Self::overlay_stack`].
    #[must_use]
    pub fn overlay_tiered(
        base: LayoutNode,
        overlay: LayoutNode,
        bounds: Rect,
        tier: OverlayTier,
    ) -> Self {
        Self::Overlay {
            base: Box::new(base),
            overlay: Box::new(overlay),
            bounds,
            tier,
        }
    }

    /// Composes `base` with stacked floating `layers`, deterministically.
    ///
    /// Layers are stable-sorted by [`OverlayTier`] (lowest first), so paint
    /// order is always `base`, then tiers `Editor < Float < Popup <
    /// Messages`, regardless of input order. Layers on the same tier keep
    /// their input order with later entries painting above (after) earlier
    /// ones. The result folds into nested [`LayoutNode::Overlay`] nodes
    /// (lowest tier innermost), so the gapless and gap-aware solvers traverse
    /// it exactly like a hand-built overlay chain: a single-layer stack is
    /// structurally identical to [`Self::overlay_tiered`], and an empty
    /// `layers` vec returns `base` unchanged.
    #[must_use]
    pub fn overlay_stack(base: LayoutNode, mut layers: Vec<OverlayLayer>) -> Self {
        layers.sort_by_key(|l| l.tier);
        layers.into_iter().fold(base, |acc, l| {
            Self::overlay_tiered(acc, l.node, l.bounds, l.tier)
        })
    }

    /// Returns the [`OverlayTier`] of an [`LayoutNode::Overlay`] node, or
    /// `None` for any other variant.
    #[must_use]
    pub fn overlay_tier(&self) -> Option<OverlayTier> {
        if let Self::Overlay { tier, .. } = self {
            Some(*tier)
        } else {
            None
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
                ..
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

/// Picks a [`SplitAxis`] from container cell dimensions, mirroring Hyprland's
/// dwindle heuristic `splitTop = box.h * split_width_multiplier > box.w`
/// (CTX-0209, CR-UI-01).
///
/// Returns [`SplitAxis::Vertical`] (stacked, top/bottom) when the container is
/// taller than it is wide after the multiplier, [`SplitAxis::Horizontal`]
/// (side-by-side, left/right) otherwise — including the square tie. The
/// comparison runs in `f32` cell space (exact for `u16` ranges); non-finite or
/// non-positive `width_multiplier` falls back to `1.0`. Total over all inputs,
/// including empty bounds (which yield [`SplitAxis::Horizontal`]).
#[must_use]
pub fn smart_split_axis(container: Rect, width_multiplier: f32) -> SplitAxis {
    let mult = if width_multiplier.is_finite() && width_multiplier > 0.0 {
        width_multiplier
    } else {
        1.0
    };
    if container.height as f32 * mult > container.width as f32 {
        SplitAxis::Vertical
    } else {
        SplitAxis::Horizontal
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

    #[test]
    fn smart_split_axis_wide_is_side_by_side() {
        // CTX-0209: wide containers split left/right (vertical divider), so
        // new panes stay squarish. Mirrors Hyprland `splitTop = h > w`.
        assert_eq!(
            smart_split_axis(Rect::new(0, 0, 80, 24), 1.0),
            SplitAxis::Horizontal
        );
        let node = LayoutNode::smart_split(
            Rect::new(0, 0, 80, 24),
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout(Rect::new(0, 0, 80, 24));
        assert_eq!(alloc.len(), 2);
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        assert_eq!(a.width + b.width, 80);
        assert_eq!(a.height, 24);
        assert_eq!(b.height, 24);
    }

    #[test]
    fn smart_split_axis_tall_is_stacked() {
        // CTX-0209: tall containers split top/bottom (horizontal divider).
        assert_eq!(
            smart_split_axis(Rect::new(0, 0, 24, 80), 1.0),
            SplitAxis::Vertical
        );
        let node = LayoutNode::smart_split(
            Rect::new(0, 0, 24, 80),
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let alloc = node.layout(Rect::new(0, 0, 24, 80));
        assert_eq!(alloc.len(), 2);
        let (_, a) = alloc[0];
        let (_, b) = alloc[1];
        assert_eq!(a.height + b.height, 80);
        assert_eq!(a.width, 24);
        assert_eq!(b.width, 24);
    }

    #[test]
    fn smart_split_axis_square_tie_breaks_side_by_side() {
        // CTX-0209: a square container ties (`h * 1.0 > w` is false) and
        // splits side-by-side, matching Hyprland's default first split.
        assert_eq!(
            smart_split_axis(Rect::new(0, 0, 40, 40), 1.0),
            SplitAxis::Horizontal
        );
    }

    #[test]
    fn smart_split_multiplier_shifts_threshold() {
        // CTX-0209: mirrors `dwindle:split_width_multiplier`; 40 * 2.0 > 60
        // flips a wide container to stacked, while 1.0 keeps it side-by-side.
        let bounds = Rect::new(0, 0, 60, 40);
        assert_eq!(smart_split_axis(bounds, 2.0), SplitAxis::Vertical);
        assert_eq!(smart_split_axis(bounds, 1.0), SplitAxis::Horizontal);
        // Degenerate multipliers fall back to 1.0 (total constructor).
        assert_eq!(smart_split_axis(bounds, 0.0), SplitAxis::Horizontal);
        assert_eq!(smart_split_axis(bounds, f32::NAN), SplitAxis::Horizontal);
        // Empty bounds are total and deterministic.
        assert_eq!(smart_split_axis(Rect::zero(), 1.0), SplitAxis::Horizontal);
    }

    #[test]
    fn smart_split_matches_explicit_split_layout() {
        // CTX-0209: the adaptive constructor delegates to `split`, so its
        // allocations are byte-identical to the equivalent explicit split and
        // the explicit axis+ratio API is unaffected.
        let wide = Rect::new(0, 0, 80, 24);
        let smart = LayoutNode::smart_split(
            wide,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let explicit = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        assert_eq!(smart, explicit);
        assert_eq!(smart.layout(wide), explicit.layout(wide));

        let tall = Rect::new(0, 0, 24, 80);
        let smart = LayoutNode::smart_split(
            tall,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        let explicit = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        assert_eq!(smart, explicit);
        assert_eq!(smart.layout(tall), explicit.layout(tall));

        // Ratio clamping flows through the same path as `split`.
        let clamped = LayoutNode::smart_split(
            wide,
            5.0,
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
        );
        if let LayoutNode::Split { axis, ratio, .. } = clamped {
            assert_eq!(axis, SplitAxis::Horizontal);
            assert_eq!(ratio, LayoutNode::MAX_RATIO);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn overlay_tier_ordering_and_defaults() {
        // CTX-0217: tiers order Editor < Float < Popup < Messages with stable
        // explicit discriminants; the legacy constructor defaults to Float.
        assert!(OverlayTier::Editor < OverlayTier::Float);
        assert!(OverlayTier::Float < OverlayTier::Popup);
        assert!(OverlayTier::Popup < OverlayTier::Messages);
        assert_eq!(OverlayTier::Editor as u8, 0);
        assert_eq!(OverlayTier::Float as u8, 1);
        assert_eq!(OverlayTier::Popup as u8, 2);
        assert_eq!(OverlayTier::Messages as u8, 3);
        assert_eq!(OverlayTier::default(), OverlayTier::Float);
        assert_eq!(OverlayTier::BOTTOM, OverlayTier::Editor);
        assert_eq!(OverlayTier::TOP, OverlayTier::Messages);

        let node = LayoutNode::overlay(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            Rect::new(0, 0, 5, 5),
        );
        assert_eq!(node.overlay_tier(), Some(OverlayTier::Float));
        let tiered = LayoutNode::overlay_tiered(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            Rect::new(0, 0, 5, 5),
            OverlayTier::Messages,
        );
        assert_eq!(tiered.overlay_tier(), Some(OverlayTier::Messages));
        assert_eq!(LayoutNode::leaf(view(1, 1, 1)).overlay_tier(), None);
    }

    #[test]
    fn single_overlay_layout_byte_identical() {
        // CTX-0217: tiering must not change existing single-overlay behavior.
        // `overlay()` and `overlay_tiered(..., Float)` produce identical
        // allocations, pinned to exact rects.
        let bounds = Rect::new(0, 0, 80, 24);
        let over_bounds = Rect::new(10, 5, 20, 10);
        let legacy = LayoutNode::overlay(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            over_bounds,
        );
        let tiered = LayoutNode::overlay_tiered(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            over_bounds,
            OverlayTier::Float,
        );
        // Same tier default => structurally identical trees.
        assert_eq!(legacy, tiered);
        let a = legacy.layout(bounds);
        let b = tiered.layout(bounds);
        assert_eq!(a, b);
        assert_eq!(
            a,
            vec![(ViewId::new(1), bounds), (ViewId::new(2), over_bounds),]
        );
        // Gap-aware solver agrees as well.
        assert_eq!(
            legacy.layout_with_gaps(bounds, Gaps::ZERO),
            tiered.layout_with_gaps(bounds, Gaps::ZERO)
        );
    }

    #[test]
    fn overlay_stack_single_layer_matches_overlay() {
        // CTX-0217: a one-layer stack is exactly `overlay_tiered`.
        let base = LayoutNode::leaf(view(1, 1, 1));
        let over_bounds = Rect::new(70, 20, 20, 10);
        let stacked = LayoutNode::overlay_stack(
            base,
            vec![OverlayLayer::new(
                OverlayTier::Popup,
                LayoutNode::leaf(view(2, 1, 1)),
                over_bounds,
            )],
        );
        let direct = LayoutNode::overlay_tiered(
            LayoutNode::leaf(view(1, 1, 1)),
            LayoutNode::leaf(view(2, 1, 1)),
            over_bounds,
            OverlayTier::Popup,
        );
        assert_eq!(stacked, direct);
        // Empty stacks return the base unchanged (total constructor).
        let bare = LayoutNode::leaf(view(1, 1, 1));
        assert_eq!(LayoutNode::overlay_stack(bare.clone(), vec![]), bare);
    }

    #[test]
    fn overlay_stack_orders_by_tier_regardless_of_input() {
        // CTX-0217: paint order follows tiers even when layers arrive
        // scrambled; the run is deterministic.
        let bounds = Rect::new(0, 0, 80, 24);
        let layer = |tier, id: u64| {
            OverlayLayer::new(
                tier,
                LayoutNode::leaf(view(id, 1, 1)),
                Rect::new(0, 0, 10, 5),
            )
        };
        let scrambled = vec![
            layer(OverlayTier::Messages, 40),
            layer(OverlayTier::Editor, 10),
            layer(OverlayTier::Popup, 30),
            layer(OverlayTier::Float, 20),
        ];
        let node = LayoutNode::overlay_stack(LayoutNode::leaf(view(1, 1, 1)), scrambled);
        let alloc = node.layout(bounds);
        let ids: Vec<ViewId> = alloc.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![
                ViewId::new(1),
                ViewId::new(10),
                ViewId::new(20),
                ViewId::new(30),
                ViewId::new(40),
            ]
        );
        // Determinism check: second run identical.
        assert_eq!(alloc, node.layout(bounds));
        // Tiers are recorded outermost-first (Messages on top).
        assert_eq!(node.overlay_tier(), Some(OverlayTier::Messages));
    }

    #[test]
    fn overlay_stack_same_tier_is_stable_later_on_top() {
        // CTX-0217 conflict rule: same-tier ties keep construction order and
        // later entries paint above (after) earlier ones.
        let bounds = Rect::new(0, 0, 80, 24);
        let layer = |id: u64| {
            OverlayLayer::new(
                OverlayTier::Float,
                LayoutNode::leaf(view(id, 1, 1)),
                Rect::new(0, 0, 10, 5),
            )
        };
        let fwd =
            LayoutNode::overlay_stack(LayoutNode::leaf(view(1, 1, 1)), vec![layer(2), layer(3)]);
        let ids: Vec<ViewId> = fwd.layout(bounds).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![ViewId::new(1), ViewId::new(2), ViewId::new(3)]);

        let rev =
            LayoutNode::overlay_stack(LayoutNode::leaf(view(1, 1, 1)), vec![layer(3), layer(2)]);
        let rev_ids: Vec<ViewId> = rev.layout(bounds).iter().map(|(id, _)| *id).collect();
        assert_eq!(
            rev_ids,
            vec![ViewId::new(1), ViewId::new(3), ViewId::new(2)]
        );
    }
}
