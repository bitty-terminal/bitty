//! Pseudo-tiling fixed-dimension and aspect-ratio panels (CTX-1079, issue #1758).
//!
//! Hyprland-pseudo-like: a tiled leaf keeps its solver slot (the layout
//! solver ignores the flag, so slot restore stays byte-identical and sibling
//! allocations never move), but its present viewport is centered inside the
//! allocated slot at the preferred character-grid size. The surrounding
//! gutter shows the window background: no content stretch, no overlap into
//! neighbours.
//!
//! Fail-closed: a slot smaller than the preferred size in either axis falls
//! back to plain tiled fill; all arithmetic is saturating so degenerate
//! slots never produce negative rects (mirroring the P1 float clamp
//! precedent in [`crate::presentation::float_frame`]).
//!
//! Storage lives on [`crate::view::View`] as an optional
//! [`PseudoConstraint`] (none by default). All transitions route through the
//! helpers here; the solver never reads the flag.
//!
//! Deterministic, headless; this module adds no new crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::{Rect, Size};
use crate::layout::LayoutNode;
use crate::view::ViewId;

/// Default preferred width for a fresh pseudo toggle (matches the
/// conventional 80-column grid used across layout tests).
pub const DEFAULT_PSEUDO_COLS: u16 = 80;
/// Default preferred height for a fresh pseudo toggle (matches the
/// conventional 24-row grid used across layout tests).
pub const DEFAULT_PSEUDO_ROWS: u16 = 24;

/// Workspace command toggling the pseudo flag on one leaf.
///
/// `<owner>.<name>:<command>` grammar (see
/// [`QualifiedCommand`](crate::panel::QualifiedCommand)); routed by
/// [`apply_pseudo_toggle`]. Enabling without an explicit size uses the
/// default fixed grid; disabling restores plain tiled fill with the solver
/// slot byte-identical.
pub const PSEUDO_CMD_TOGGLE: &str = "bitty.workspace:pseudo-toggle";

/// Preferred viewport constraint for a pseudo-tiled leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PseudoConstraint {
    /// Fixed character-grid dimensions (cols x rows, each at least 1).
    Fixed(Size),
    /// Aspect ratio width over height (both non-zero); resolved against the
    /// live slot to the largest fitting centered rect.
    Aspect { num: u32, den: u32 },
}

impl PseudoConstraint {
    /// Fixed constraint, clamping each axis to at least 1 so the
    /// constructor stays total (mirrors [`crate::view::View`] minimums).
    #[must_use]
    pub fn fixed(cols: u16, rows: u16) -> Self {
        Self::Fixed(Size::new(cols.max(1), rows.max(1)))
    }

    /// Aspect constraint, or `None` when either side is zero (fail-closed:
    /// the caller touches nothing on invalid input).
    #[must_use]
    pub fn aspect(num: u32, den: u32) -> Option<Self> {
        if num == 0 || den == 0 {
            None
        } else {
            Some(Self::Aspect { num, den })
        }
    }

    /// Resolves this constraint against `slot` to the present viewport.
    #[must_use]
    pub fn resolve(self, slot: Rect) -> Rect {
        match self {
            Self::Fixed(preferred) => pseudo_viewport(slot, preferred),
            Self::Aspect { num, den } => pseudo_viewport_aspect(slot, num, den),
        }
    }
}

/// Fixed-size pseudo viewport: `preferred` centered inside `slot`.
///
/// Returns `slot` unchanged (plain tiled fill) when `slot` is smaller than
/// `preferred` in either axis, when either rect is empty, or when `slot`
/// itself is empty. Otherwise returns the centered `preferred`-sized rect,
/// which is always contained in `slot`. Saturating math throughout.
#[must_use]
pub fn pseudo_viewport(slot: Rect, preferred: Size) -> Rect {
    if slot.is_empty() || preferred.is_empty() {
        return slot;
    }
    if slot.width < preferred.width || slot.height < preferred.height {
        return slot;
    }
    let dx = slot.width.saturating_sub(preferred.width) / 2;
    let dy = slot.height.saturating_sub(preferred.height) / 2;
    Rect::new(
        slot.x.saturating_add(dx),
        slot.y.saturating_add(dy),
        preferred.width,
        preferred.height,
    )
}

/// Aspect-ratio pseudo viewport: largest centered rect inside `slot` with
/// aspect `num` over `den`.
///
/// Fail-closed to `slot` on empty slots and on zero/non-fitting aspects.
/// Uses `u64` intermediates so `u16` slots with large ratios never overflow.
/// The result is always contained in `slot` and at least 1x1 for non-empty
/// slots.
#[must_use]
pub fn pseudo_viewport_aspect(slot: Rect, num: u32, den: u32) -> Rect {
    if slot.is_empty() || num == 0 || den == 0 {
        return slot;
    }
    let sw = u64::from(slot.width);
    let sh = u64::from(slot.height);
    let n = u64::from(num);
    let d = u64::from(den);
    // Compare sw*d vs sh*n without overflow (u64 holds the products).
    let (w, h) = if sw.saturating_mul(d) > sh.saturating_mul(n) {
        // Slot wider than target: height limits the width.
        let w = ((sh.saturating_mul(n) / d).clamp(1, sw)) as u16;
        (w, slot.height)
    } else {
        // Slot taller than (or equal to) target: width limits the height.
        let h = ((sw.saturating_mul(d) / n).clamp(1, sh)) as u16;
        (slot.width, h)
    };
    pseudo_viewport(slot, Size::new(w, h))
}

/// Resolves an optional constraint against `slot`: `None` means plain tiled
/// fill (`slot`), `Some` centers via [`PseudoConstraint::resolve`].
#[must_use]
pub fn resolve_pseudo_viewport(slot: Rect, constraint: Option<PseudoConstraint>) -> Rect {
    match constraint {
        None => slot,
        Some(c) => c.resolve(slot),
    }
}

/// Error for pseudo toggle/set/clear helpers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PseudoError {
    /// `command` is not [`PSEUDO_CMD_TOGGLE`]; the tree is untouched.
    UnknownCommand(String),
    /// No leaf with `target` id exists in `tree`; untouched.
    LeafNotFound(ViewId),
    /// Aspect has a zero side; the leaf is untouched.
    InvalidAspect { num: u32, den: u32 },
}

impl std::fmt::Display for PseudoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(cmd) => write!(f, "unknown pseudo toggle command: {cmd}"),
            Self::LeafNotFound(id) => write!(f, "pseudo target not found: {id}"),
            Self::InvalidAspect { num, den } => {
                write!(
                    f,
                    "invalid pseudo aspect {num}/{den}: sides must be non-zero"
                )
            }
        }
    }
}

impl std::error::Error for PseudoError {}

/// Toggles the pseudo flag on leaf `target`.
///
/// When the leaf has no constraint, stamps the default fixed grid
/// (`DEFAULT_PSEUDO_COLS` x `DEFAULT_PSEUDO_ROWS`) and returns `true`;
/// when it already has one, clears it and returns `false` (plain tiled
/// fill, solver slot byte-identical). The presentation mode is untouched:
/// pseudo stays a tiled-leaf flag.
pub fn toggle_pseudo(tree: &mut LayoutNode, target: ViewId) -> Result<bool, PseudoError> {
    let leaf = tree
        .find_leaf(target)
        .ok_or(PseudoError::LeafNotFound(target))?;
    let enabling = leaf.pseudo_constraint().is_none();
    let leaf = tree
        .find_leaf_mut(target)
        .ok_or(PseudoError::LeafNotFound(target))?;
    if enabling {
        leaf.set_pseudo_constraint(Some(PseudoConstraint::fixed(
            DEFAULT_PSEUDO_COLS,
            DEFAULT_PSEUDO_ROWS,
        )));
    } else {
        leaf.clear_pseudo();
    }
    Ok(enabling)
}

/// Sets a fixed preferred size on leaf `target` (explicit config path for
/// `views.<selector>.pseudo_size`). Dimensions clamp to at least 1x1 so the
/// helper stays total. Returns the stamped size.
pub fn set_pseudo_size(
    tree: &mut LayoutNode,
    target: ViewId,
    cols: u16,
    rows: u16,
) -> Result<Size, PseudoError> {
    let size = Size::new(cols.max(1), rows.max(1));
    let leaf = tree
        .find_leaf_mut(target)
        .ok_or(PseudoError::LeafNotFound(target))?;
    leaf.set_pseudo_constraint(Some(PseudoConstraint::Fixed(size)));
    Ok(size)
}

/// Sets an aspect-ratio constraint on leaf `target`. Zero sides fail with
/// [`PseudoError::InvalidAspect`] and touch nothing.
pub fn set_pseudo_aspect(
    tree: &mut LayoutNode,
    target: ViewId,
    num: u32,
    den: u32,
) -> Result<PseudoConstraint, PseudoError> {
    let constraint =
        PseudoConstraint::aspect(num, den).ok_or(PseudoError::InvalidAspect { num, den })?;
    let leaf = tree
        .find_leaf_mut(target)
        .ok_or(PseudoError::LeafNotFound(target))?;
    leaf.set_pseudo_constraint(Some(constraint));
    Ok(constraint)
}

/// Clears the pseudo flag on leaf `target`. Returns whether a constraint was
/// present (true means the leaf now renders plain tiled fill).
pub fn clear_pseudo(tree: &mut LayoutNode, target: ViewId) -> Result<bool, PseudoError> {
    let leaf = tree
        .find_leaf_mut(target)
        .ok_or(PseudoError::LeafNotFound(target))?;
    let had = leaf.pseudo_constraint().is_some();
    leaf.clear_pseudo();
    Ok(had)
}

/// Applies workspace command `command` as a pseudo toggle on leaf `target`.
///
/// Only [`PSEUDO_CMD_TOGGLE`] is accepted; anything else fails with
/// [`PseudoError::UnknownCommand`] and touches nothing. Otherwise delegates
/// to [`toggle_pseudo`].
pub fn apply_pseudo_toggle(
    tree: &mut LayoutNode,
    command: &str,
    target: ViewId,
) -> Result<bool, PseudoError> {
    if command != PSEUDO_CMD_TOGGLE {
        return Err(PseudoError::UnknownCommand(command.to_owned()));
    }
    toggle_pseudo(tree, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::SplitAxis;
    use crate::view::View;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
    }

    #[test]
    fn fixed_viewport_centers_when_slot_is_larger() {
        let slot = Rect::new(0, 0, 100, 40);
        let preferred = Size::new(80, 24);
        assert_eq!(pseudo_viewport(slot, preferred), Rect::new(10, 8, 80, 24));
    }

    #[test]
    fn fixed_viewport_centers_inside_offset_slots() {
        let slot = Rect::new(5, 7, 100, 40);
        assert_eq!(
            pseudo_viewport(slot, Size::new(80, 24)),
            Rect::new(15, 15, 80, 24)
        );
    }

    #[test]
    fn fixed_viewport_falls_back_when_slot_is_smaller() {
        let preferred = Size::new(80, 24);
        // Narrower in one axis.
        assert_eq!(
            pseudo_viewport(Rect::new(0, 0, 70, 40), preferred),
            Rect::new(0, 0, 70, 40)
        );
        // Shorter in one axis.
        assert_eq!(
            pseudo_viewport(Rect::new(0, 0, 100, 20), preferred),
            Rect::new(0, 0, 100, 20)
        );
        // Exact fit still centers to itself (no gutter).
        assert_eq!(
            pseudo_viewport(Rect::new(3, 4, 80, 24), preferred),
            Rect::new(3, 4, 80, 24)
        );
    }

    #[test]
    fn fixed_viewport_is_total_over_degenerate_inputs() {
        // Empty slots and empty preferred sizes fall back without panics.
        assert!(pseudo_viewport(Rect::zero(), Size::new(80, 24)).is_empty());
        assert_eq!(
            pseudo_viewport(Rect::new(0, 0, 80, 24), Size::new(0, 24)),
            Rect::new(0, 0, 80, 24)
        );
        assert_eq!(
            pseudo_viewport(Rect::new(0, 0, 80, 24), Size::new(80, 0)),
            Rect::new(0, 0, 80, 24)
        );
        // Odd remainders floor toward the top-left; the result stays inside.
        let slot = Rect::new(0, 0, 81, 25);
        let view = pseudo_viewport(slot, Size::new(80, 24));
        assert_eq!(view, Rect::new(0, 0, 80, 24));
        assert!(slot.contains(view));
    }

    #[test]
    fn aspect_viewport_fits_largest_centered_rect() {
        // 100x40 slot with 16:9 target: height limits -> 71x40 centered.
        let view = pseudo_viewport_aspect(Rect::new(0, 0, 100, 40), 16, 9);
        assert_eq!(view, Rect::new(14, 0, 71, 40));
        // 40x100 slot with 16:9 target: width limits -> 40x22 centered.
        let view = pseudo_viewport_aspect(Rect::new(0, 0, 40, 100), 16, 9);
        assert_eq!(view, Rect::new(0, 39, 40, 22));
        // Square slot with 1:1 stays full.
        assert_eq!(
            pseudo_viewport_aspect(Rect::new(2, 3, 40, 40), 1, 1),
            Rect::new(2, 3, 40, 40)
        );
    }

    #[test]
    fn aspect_viewport_fails_closed() {
        let slot = Rect::new(0, 0, 80, 24);
        assert_eq!(pseudo_viewport_aspect(slot, 0, 16), slot);
        assert_eq!(pseudo_viewport_aspect(slot, 16, 0), slot);
        assert_eq!(pseudo_viewport_aspect(Rect::zero(), 16, 9), Rect::zero());
        // Degenerate 1x1 slot still yields a contained 1x1.
        let view = pseudo_viewport_aspect(Rect::new(3, 7, 1, 1), 16, 9);
        assert_eq!(view, Rect::new(3, 7, 1, 1));
        assert!(Rect::new(3, 7, 1, 1).contains(view));
    }

    #[test]
    fn toggle_sets_default_then_clears_with_slot_restore() {
        use crate::geometry::Gaps;
        let bounds = Rect::new(0, 0, 100, 40);
        let mut tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2));
        let before = tree.layout_with_gaps(bounds, Gaps::ZERO);
        assert!(toggle_pseudo(&mut tree, ViewId::new(1)).expect("enable"));
        let leaf_view = tree.find_leaf(ViewId::new(1)).expect("leaf");
        assert_eq!(
            leaf_view.pseudo_constraint(),
            Some(PseudoConstraint::Fixed(Size::new(
                DEFAULT_PSEUDO_COLS,
                DEFAULT_PSEUDO_ROWS
            )))
        );
        // Solver ignores the flag: allocations byte-identical.
        assert_eq!(tree.layout_with_gaps(bounds, Gaps::ZERO), before);
        assert!(!toggle_pseudo(&mut tree, ViewId::new(1)).expect("disable"));
        assert_eq!(
            tree.find_leaf(ViewId::new(1))
                .expect("leaf")
                .pseudo_constraint(),
            None
        );
        assert_eq!(tree.layout_with_gaps(bounds, Gaps::ZERO), before);
    }

    #[test]
    fn solver_allocations_for_other_leaves_are_unchanged() {
        use crate::geometry::Gaps;
        let bounds = Rect::new(0, 0, 120, 40);
        let base = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::split(SplitAxis::Vertical, 0.5, leaf(1), leaf(2)),
            leaf(3),
        );
        let mut stamped = base.clone();
        set_pseudo_size(&mut stamped, ViewId::new(1), 40, 20).expect("stamp");
        let plain = base.layout_with_gaps(bounds, Gaps::ZERO);
        let constrained = stamped.layout_with_gaps(bounds, Gaps::ZERO);
        assert_eq!(plain, constrained);
        // Other leaves keep their exact slots.
        for id in [ViewId::new(2), ViewId::new(3)] {
            let a = plain.iter().find(|(v, _)| *v == id).expect("plain");
            let b = constrained.iter().find(|(v, _)| *v == id).expect("stamped");
            assert_eq!(a, b);
        }
        // The pseudo leaf resolves to a centered viewport inside its slot.
        let slot = constrained
            .iter()
            .find(|(v, _)| *v == ViewId::new(1))
            .expect("slot")
            .1;
        let view = stamped
            .find_leaf(ViewId::new(1))
            .expect("leaf")
            .pseudo_constraint()
            .expect("flag")
            .resolve(slot);
        assert!(slot.contains(view));
    }

    #[test]
    fn set_and_clear_roundtrip_through_registry_command() {
        let mut tree = leaf(7);
        assert_eq!(
            set_pseudo_size(&mut tree, ViewId::new(7), 60, 20).expect("set"),
            Size::new(60, 20)
        );
        assert!(tree.find_leaf(ViewId::new(7)).expect("leaf").is_pseudo());
        assert!(clear_pseudo(&mut tree, ViewId::new(7)).expect("clear"));
        assert!(!tree.find_leaf(ViewId::new(7)).expect("leaf").is_pseudo());
        // Registry path toggles on then off.
        assert!(
            apply_pseudo_toggle(&mut tree, PSEUDO_CMD_TOGGLE, ViewId::new(7)).expect("toggle on")
        );
        assert!(
            !apply_pseudo_toggle(&mut tree, PSEUDO_CMD_TOGGLE, ViewId::new(7)).expect("toggle off")
        );
        // Unknown command and missing leaf touch nothing.
        let before = tree.clone();
        assert!(matches!(
            apply_pseudo_toggle(&mut tree, "bitty.workspace:nope", ViewId::new(7)),
            Err(PseudoError::UnknownCommand(_))
        ));
        assert_eq!(tree, before);
        assert!(matches!(
            apply_pseudo_toggle(&mut tree, PSEUDO_CMD_TOGGLE, ViewId::new(404)),
            Err(PseudoError::LeafNotFound(_))
        ));
        assert_eq!(tree, before);
    }

    #[test]
    fn aspect_set_rejects_zero_sides_without_touching_tree() {
        let mut tree = leaf(9);
        let before = tree.clone();
        assert!(matches!(
            set_pseudo_aspect(&mut tree, ViewId::new(9), 0, 16),
            Err(PseudoError::InvalidAspect { .. })
        ));
        assert_eq!(tree, before);
        let constraint = set_pseudo_aspect(&mut tree, ViewId::new(9), 16, 9).expect("aspect");
        assert_eq!(constraint, PseudoConstraint::Aspect { num: 16, den: 9 });
    }
}
