//! Issue #1343: new panel/split must tile panes (one chrome frame each),
//! never stack repeated full-width bars vertically.
//!
//! Headless regression: drive a split through the same `set_layout` funnel
//! the keymap `new_split` and `ctl view split` arms use, then assert tiled
//! geometry on both the cell allocations and the decorated live-present
//! frames (the per-pane chrome ring source of truth).

use bitty_runtime::{LayoutNode, Runtime, SplitAxis, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("default headless runtime must build")
}

/// Mirror of the live split shape: clone the tree, split the focused leaf,
/// install via `set_layout` (the keymap/`ctl` funnel — never a direct tree
/// edit).
fn live_split(rt: &mut Runtime, axis: SplitAxis, place_new_first: bool) -> (ViewId, ViewId) {
    let focused = rt.focused_view().expect("live runtime always has focus");
    let new_id = rt.next_view_id_global();
    let mut layout = rt.layout().clone();
    assert!(
        split_focused_leaf(&mut layout, focused, axis, new_id, place_new_first),
        "focused leaf must be splittable"
    );
    rt.set_layout(layout);
    rt.set_focus(new_id);
    (focused, new_id)
}

/// Minimal copy of the focused-leaf split the keymap/`ctl` arms perform.
fn split_focused_leaf(
    layout: &mut LayoutNode,
    focused: ViewId,
    axis: SplitAxis,
    new_id: ViewId,
    place_new_first: bool,
) -> bool {
    match layout {
        LayoutNode::Leaf(view) if view.id() == focused => {
            let old = view.clone();
            let fresh = View::new(new_id, usize::from(old.cols()), usize::from(old.rows()));
            let (first, second) = if place_new_first {
                (LayoutNode::leaf(fresh), LayoutNode::leaf(old))
            } else {
                (LayoutNode::leaf(old), LayoutNode::leaf(fresh))
            };
            *layout = LayoutNode::split(axis, 0.5, first, second);
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_focused_leaf(first, focused, axis, new_id, place_new_first)
                || split_focused_leaf(second, focused, axis, new_id, place_new_first)
        }
        LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|c| split_focused_leaf(c, focused, axis, new_id, place_new_first)),
        LayoutNode::Overlay { base, overlay, .. } => {
            split_focused_leaf(base, focused, axis, new_id, place_new_first)
                || split_focused_leaf(overlay, focused, axis, new_id, place_new_first)
        }
        _ => false,
    }
}

#[test]
fn horizontal_split_tiles_side_by_side_with_one_frame_per_pane() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    let container = rt.container();
    assert!(
        container.width >= 20,
        "need room to split, got {container:?}"
    );

    let (old_id, new_id) = live_split(&mut rt, SplitAxis::Horizontal, false);
    assert_eq!(rt.leaf_count(), 2);

    // Cell allocations tile side by side: same rows, x-adjacent, neither
    // pane spans the full width (a vertical bar-stack would).
    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 2, "one allocation per pane, got {allocs:?}");
    let a = allocs
        .iter()
        .find(|(id, _)| *id == old_id)
        .expect("old pane kept");
    let b = allocs
        .iter()
        .find(|(id, _)| *id == new_id)
        .expect("new pane added");
    assert_eq!(
        a.1.y, b.1.y,
        "side-by-side panes share the row origin: {allocs:?}"
    );
    assert_eq!(
        a.1.height, b.1.height,
        "side-by-side panes share the height: {allocs:?}"
    );
    assert!(a.1.x < b.1.x, "new pane goes right: {allocs:?}");
    assert!(
        a.1.width < container.width && b.1.width < container.width,
        "no pane spans the full width (bar-stack symptom): {allocs:?}"
    );

    // Live-present chrome frames agree: exactly one frame per pane,
    // side-by-side content rects in physical pixels.
    let frames = rt.present_frames();
    assert_eq!(
        frames.len(),
        2,
        "one chrome frame per pane, got {}",
        frames.len()
    );
    let mut views: Vec<ViewId> = frames.iter().map(|f| f.view).collect();
    views.sort();
    let mut want = [old_id, new_id];
    want.sort();
    assert_eq!(views, want, "each pane owns exactly one frame");
    let fa = frames.iter().find(|f| f.view == old_id).expect("old frame");
    let fb = frames.iter().find(|f| f.view == new_id).expect("new frame");
    assert_eq!(
        fa.content.y, fb.content.y,
        "chrome frames share the row origin: {:?} vs {:?}",
        fa.content, fb.content
    );
    assert_eq!(
        fa.content.height, fb.content.height,
        "chrome frames share the height: {:?} vs {:?}",
        fa.content, fb.content
    );
    assert!(
        fa.content.x < fb.content.x,
        "chrome frames tile left/right, not stacked: {:?} vs {:?}",
        fa.content,
        fb.content
    );
    assert!(
        fa.cols < container.width && fb.cols > 0,
        "pane grids are narrowed halves, not full-width repeats"
    );
}

#[test]
fn vertical_split_tiles_top_bottom() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    let container = rt.container();

    let (old_id, new_id) = live_split(&mut rt, SplitAxis::Vertical, false);
    assert_eq!(rt.leaf_count(), 2);

    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 2, "one allocation per pane, got {allocs:?}");
    let a = allocs
        .iter()
        .find(|(id, _)| *id == old_id)
        .expect("old pane kept");
    let b = allocs
        .iter()
        .find(|(id, _)| *id == new_id)
        .expect("new pane added");
    assert_eq!(
        a.1.x, b.1.x,
        "stacked panes share the column origin: {allocs:?}"
    );
    assert_eq!(
        a.1.width, b.1.width,
        "stacked panes share the width: {allocs:?}"
    );
    assert!(a.1.y < b.1.y, "new pane goes below: {allocs:?}");
    assert!(
        a.1.height < container.height && b.1.height < container.height,
        "no pane spans the full height: {allocs:?}"
    );

    let frames = rt.present_frames();
    assert_eq!(
        frames.len(),
        2,
        "one chrome frame per pane, got {}",
        frames.len()
    );
}

#[test]
fn nested_splits_keep_tiling_without_full_width_repeats() {
    let mut rt = make_runtime();
    rt.tick().expect("first full redraw");
    let container = rt.container();

    // Right, then down inside the new pane: three tiles, no full-width bar.
    let (_, second) = live_split(&mut rt, SplitAxis::Horizontal, false);
    assert_eq!(rt.focused_view(), Some(second));
    let (_, third) = live_split(&mut rt, SplitAxis::Vertical, false);
    assert_eq!(rt.leaf_count(), 3);

    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 3, "one allocation per pane, got {allocs:?}");
    for (id, rect) in &allocs {
        assert!(
            rect.width < container.width || rect.height < container.height,
            "pane {id:?} must not cover the whole window (bar-stack symptom): {rect:?}"
        );
    }
    let _ = third;

    let frames = rt.present_frames();
    assert_eq!(
        frames.len(),
        3,
        "one chrome frame per pane, got {}",
        frames.len()
    );
    let mut views: Vec<ViewId> = frames.iter().map(|f| f.view).collect();
    views.sort();
    views.dedup();
    assert_eq!(views.len(), 3, "each pane owns exactly one frame");
}
