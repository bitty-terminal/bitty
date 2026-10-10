#![forbid(unsafe_code)]
//! CTX-1079 (#1758): pseudo-tiling fixed-dimension panels at the present level.
//!
//! A pseudo leaf keeps its solver slot (allocations byte-identical, siblings
//! untouched) while its present content shrinks to the preferred grid
//! centered inside the slot. The frame stays the slot so the gutter paints
//! window background with no overlap into neighbours. Degenerate slots fall
//! back to plain tiled fill.

use bitty_runtime::{
    LayoutNode, PresentFrame, PseudoConstraint, Runtime, SplitAxis, UiRect, View, ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn single() -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(1), 80, 24))
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode, container: UiRect) {
    rt.set_layout(layout);
    rt.set_container(container);
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn set_fixed(rt: &mut Runtime, id: ViewId, cols: u16, rows: u16) {
    let mut tree = rt.layout().clone();
    bitty_ui::set_pseudo_size(&mut tree, id, cols, rows).expect("pseudo set must apply");
    rt.set_layout(tree);
}

fn set_aspect(rt: &mut Runtime, id: ViewId, num: u32, den: u32) {
    let mut tree = rt.layout().clone();
    bitty_ui::set_pseudo_aspect(&mut tree, id, num, den).expect("aspect must apply");
    rt.set_layout(tree);
}

fn clear(rt: &mut Runtime, id: ViewId) {
    let mut tree = rt.layout().clone();
    bitty_ui::clear_pseudo(&mut tree, id).expect("clear must apply");
    rt.set_layout(tree);
}

#[test]
fn pseudo_constrains_grid_centered_when_slot_larger() {
    let mut rt = make_runtime();
    install(&mut rt, single(), UiRect::new(0, 0, 100, 40));
    let tiled = frame_of(&rt, ViewId::new(1));
    let tiled_allocations = rt.layout_allocations();
    let (cw, ch) = rt.live_cell_size();
    assert!(cw > 0 && ch > 0);

    // Preferred grid well inside the decorated slot.
    set_fixed(&mut rt, ViewId::new(1), 40, 12);
    let after = frame_of(&rt, ViewId::new(1));

    // Still base content: no tier lift, no border change, frame is the slot.
    assert_eq!(after.tier, None);
    assert_eq!(after.border, tiled.border);
    assert_eq!(after.frame, tiled.frame);
    // Grid honors the preferred size exactly.
    assert_eq!(after.cols, 40);
    assert_eq!(after.rows, 12);
    assert_eq!(after.content.width, 40 * cw);
    assert_eq!(after.content.height, 12 * ch);
    // Centered inside the original content: gutters match within one pixel.
    let left = after.content.x - tiled.content.x;
    let right = (tiled.content.x + tiled.content.width as i32)
        - (after.content.x + after.content.width as i32);
    assert!(left >= 0 && right >= 0, "viewport stays inside the slot");
    assert!(
        (left - right).abs() <= 1,
        "viewport centers horizontally: left {left} right {right}"
    );
    let top = after.content.y - tiled.content.y;
    let bottom = (tiled.content.y + tiled.content.height as i32)
        - (after.content.y + after.content.height as i32);
    assert!(top >= 0 && bottom >= 0, "viewport stays inside the slot");
    assert!(
        (top - bottom).abs() <= 1,
        "viewport centers vertically: top {top} bottom {bottom}"
    );
    // The constrained viewport never overlaps neighbours: it is contained
    // in the original content, which itself never overlaps.
    assert!(after.content.width <= tiled.content.width);
    assert!(after.content.height <= tiled.content.height);
    // Slot restore: the solver never saw the flag.
    assert_eq!(rt.layout_allocations(), tiled_allocations);
}

#[test]
fn pseudo_falls_back_when_slot_smaller() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane(), UiRect::new(0, 0, 80, 24));
    let tiled = frame_of(&rt, ViewId::new(1));

    // The half-slot cannot fit 80x24: fail-closed to plain tiled fill.
    set_fixed(&mut rt, ViewId::new(1), 80, 24);
    let after = frame_of(&rt, ViewId::new(1));
    assert_eq!(after.tier, None);
    assert_eq!(after.frame, tiled.frame);
    assert_eq!(after.content, tiled.content);
    assert_eq!(after.cols, tiled.cols);
    assert_eq!(after.rows, tiled.rows);
    assert_eq!(after.border, tiled.border);
}

#[test]
fn pseudo_toggle_restores_byte_identical_and_others_unchanged() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane(), UiRect::new(0, 0, 120, 40));
    let before = rt.present_frames();
    let before_allocations = rt.layout_allocations();
    let sibling_before = frame_of(&rt, ViewId::new(2));

    set_fixed(&mut rt, ViewId::new(1), 40, 12);
    // Sibling slot and frame are untouched by the pseudo leaf elsewhere.
    let sibling_after = frame_of(&rt, ViewId::new(2));
    assert_eq!(sibling_after.frame, sibling_before.frame);
    assert_eq!(sibling_after.content, sibling_before.content);
    assert_eq!(sibling_after.cols, sibling_before.cols);
    assert_eq!(sibling_after.rows, sibling_before.rows);
    assert_eq!(sibling_after.border, sibling_before.border);
    assert_eq!(sibling_after.tier, sibling_before.tier);
    let allocations = rt.layout_allocations();
    assert_eq!(allocations, before_allocations);
    for id in [ViewId::new(1), ViewId::new(2)] {
        let a = before_allocations
            .iter()
            .find(|(v, _)| *v == id)
            .expect("slot");
        let b = allocations.iter().find(|(v, _)| *v == id).expect("slot");
        assert_eq!(a, b, "slot for {id:?} is unchanged");
    }

    // Untoggling restores the exact prior present frames.
    clear(&mut rt, ViewId::new(1));
    let restored = rt.present_frames();
    assert_eq!(restored, before);
    assert_eq!(rt.layout_allocations(), before_allocations);
}

#[test]
fn pseudo_aspect_fits_and_centers() {
    let mut rt = make_runtime();
    install(&mut rt, single(), UiRect::new(0, 0, 100, 40));
    let tiled = frame_of(&rt, ViewId::new(1));
    let slot_cols = tiled.cols;
    let slot_rows = tiled.rows;

    set_aspect(&mut rt, ViewId::new(1), 16, 9);
    let after = frame_of(&rt, ViewId::new(1));
    assert_eq!(after.tier, None);
    assert_eq!(after.frame, tiled.frame);

    let fitted = PseudoConstraint::Aspect { num: 16, den: 9 }
        .resolve(UiRect::new(0, 0, slot_cols, slot_rows));
    assert!(!fitted.is_empty());
    assert_eq!(after.cols, fitted.width);
    assert_eq!(after.rows, fitted.height);
    assert!(after.content.width <= tiled.content.width);
    assert!(after.content.height <= tiled.content.height);
    assert!(after.content.x >= tiled.content.x);
    assert!(after.content.y >= tiled.content.y);
}
