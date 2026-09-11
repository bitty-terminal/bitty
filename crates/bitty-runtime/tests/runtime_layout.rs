//! Runtime Layout, focus, container, and gap tests.
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_platform::{CursorPosition, PhysicalSize};
use bitty_runtime::{
    FocusDirection, Gaps, LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn make_gapped_runtime(gaps_in: u16, gaps_out: u16) -> Runtime {
    // CTX-0177: headless runtime with panel gaps (default 9x19 live cell).
    Runtime::new(RuntimeConfig {
        gaps_in,
        gaps_out,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build")
}

fn two_pane_layout() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

#[test]
fn default_layout_is_single_leaf_and_focused() {
    let rt = make_runtime();
    assert_eq!(rt.leaf_count(), 1);
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(rt.container(), UiRect::new(0, 0, 80, 24));
    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 1);
    assert_eq!(allocs[0].0, ViewId::new(1));
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 80, 24));
}

#[test]
fn set_layout_replaces_tree_and_updates_focus() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(10), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(20), 40, 24)),
    );
    rt.set_layout(split);
    assert_eq!(rt.leaf_count(), 2);
    // Focus should move to first leaf of new tree since old focus (1) no longer exists
    assert_eq!(rt.focused_view(), Some(ViewId::new(10)));
    let ids = rt.layout().leaf_ids();
    assert_eq!(ids, vec![ViewId::new(10), ViewId::new(20)]);
}

#[test]
fn set_layout_retains_focus_when_still_present() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(split);
    // Focused view 1 still exists, should be retained
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    rt.set_focus(ViewId::new(2));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    // Replace with another tree containing 2 but not 1 -> focus moves to first leaf
    let split2 = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
    );
    rt.set_layout(split2);
    assert_eq!(rt.focused_view(), Some(ViewId::new(2))); // 2 still present, retained
    let split3 = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(3), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(4), 40, 24)),
    );
    rt.set_layout(split3);
    assert_eq!(rt.focused_view(), Some(ViewId::new(3))); // 2 gone, first leaf becomes focused
}

#[test]
fn reflow_updates_view_origins_and_sizes() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
    );
    rt.set_layout(split);
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let allocs = rt.reflow_layout();
    assert_eq!(allocs.len(), 2);
    // Horizontal split 80 cols -> 40 each
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 40, 24));
    assert_eq!(allocs[1].1, UiRect::new(40, 0, 40, 24));
    // Views themselves must have been reflowed
    let v1 = rt.layout().find_leaf(ViewId::new(1)).unwrap();
    assert_eq!(v1.origin(), bitty_ui::Point::new(0, 0));
    assert_eq!(v1.cols(), 40);
    assert_eq!(v1.rows(), 24);
    let v2 = rt.layout().find_leaf(ViewId::new(2)).unwrap();
    assert_eq!(v2.origin(), bitty_ui::Point::new(40, 0));
    assert_eq!(v2.cols(), 40);
}

#[test]
fn gaps_default_zero_and_allocations_tile_edge_to_edge() {
    // CTX-0177: zero gaps preserve the legacy tiling exactly.
    let rt = make_runtime();
    assert_eq!(rt.gaps(), Gaps::ZERO);
    let mut rt = make_runtime();
    rt.set_layout(two_pane_layout());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let allocs = rt.reflow_layout();
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 40, 24));
    assert_eq!(allocs[1].1, UiRect::new(40, 0, 40, 24));
}

#[test]
fn gaps_allocations_exclude_gap_bands() {
    // CTX-0177: gapped allocations skip the bands; per-leaf rendering
    // translates these origins so the bands stay background.
    let mut rt = make_gapped_runtime(2, 1);
    rt.set_layout(two_pane_layout());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let allocs = rt.reflow_layout();
    assert_eq!(allocs[0].1, UiRect::new(1, 1, 38, 22));
    assert_eq!(allocs[1].1, UiRect::new(41, 1, 38, 22));
    // Reflowed views carry the gapped origins/sizes.
    let v2 = rt.layout().find_leaf(ViewId::new(2)).unwrap();
    assert_eq!(v2.origin(), bitty_ui::Point::new(41, 1));
    assert_eq!((v2.cols(), v2.rows()), (38, 22));
}

#[test]
fn cursor_to_cell_subtracts_outer_gap_and_decoration() {
    // CTX-0177: with gaps_out = 2 cells at the default 9x19 live cell,
    // the grid origin shifts by (18px, 38px); the mapping must subtract
    // it (0157 math) instead of drifting by the gap. CTX-0223 adds the
    // default 8px window padding first; CTX-0294/CTX-0333 adds the default
    // decoration outer gap + border + content inset (6 + 2 + 6 = 14px at
    // scale 1.0), so the probe moves to pad + gap + decoration + 2 cells =
    // (58, 98).
    let rt = make_gapped_runtime(0, 2);
    // col = (58 - 8 - 18 - 14) / 9 = 2, row = (98 - 8 - 38 - 14) / 19 = 2.
    let pos = CursorPosition { x: 58.0, y: 98.0 };
    assert_eq!(rt.cursor_to_cell(pos), CellPos::new(2, 2));
    // A click inside the outer gap clamps to the first cell (never
    // negative, never panics).
    let in_gap = CursorPosition { x: 9.0, y: 19.0 };
    assert_eq!(rt.cursor_to_cell(in_gap), CellPos::new(0, 0));
    // Zero-gap, zero-padding, undecorated runtime keeps the legacy
    // mapping bit-identical (CTX-0177 cell algebra with no px decoration).
    let plain = Runtime::new(RuntimeConfig {
        window_padding: 0,
        decoration: bitty_runtime::Decoration::ZERO,
        ..RuntimeConfig::default()
    })
    .expect("zero padding builds");
    assert_eq!(
        plain.cursor_to_cell(CursorPosition { x: 18.0, y: 38.0 }),
        CellPos::new(2, 2)
    );
}

#[test]
fn leaf_hit_testing_accounts_for_inner_and_outer_gaps() {
    // CTX-0177: leaf-aware hit-testing over a gapped two-pane layout
    // (allocations a=(1,1,38,22), b=(41,1,38,22) at 9x19 live cells).
    // Physical x for container col c is pad + outer_px + c * 9 + 1
    // (CTX-0223: the default 8px window padding inset comes first).
    let mut rt = make_gapped_runtime(2, 1);
    rt.set_layout(two_pane_layout());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
    let at = |c: u16, r: u16| CursorPosition {
        x: 8.0 + 9.0 + f64::from(c) * 9.0 + 1.0,
        y: 8.0 + 19.0 + f64::from(r) * 19.0 + 1.0,
    };
    // Inside left leaf: local cell is container minus leaf origin.
    assert_eq!(
        rt.cursor_to_leaf_cell(at(1, 5)),
        Some((ViewId::new(1), CellPos::new(4, 0)))
    );
    // Inside right leaf: container col 41 -> local col 0.
    assert_eq!(
        rt.cursor_to_leaf_cell(at(41, 5)),
        Some((ViewId::new(2), CellPos::new(4, 0)))
    );
    // Inner gap band (cols 39..41) belongs to no leaf.
    assert_eq!(rt.cursor_to_leaf_cell(at(39, 5)), None);
    assert_eq!(rt.cursor_to_leaf_cell(at(40, 5)), None);
    // Outer gap (col 0 / row 0) belongs to no leaf.
    assert_eq!(rt.cursor_to_leaf_cell(at(0, 5)), None);
    assert_eq!(rt.cursor_to_leaf_cell(at(10, 0)), None);
    // Container-cell lookup agrees.
    assert_eq!(rt.leaf_at_container_cell(1, 5), Some(ViewId::new(1)));
    assert_eq!(rt.leaf_at_container_cell(41, 5), Some(ViewId::new(2)));
    assert_eq!(rt.leaf_at_container_cell(39, 5), None);
    assert_eq!(rt.leaf_at_container_cell(0, 0), None);
}

#[test]
fn move_focus_crosses_gap_band() {
    // CTX-0177: spatial focus still moves across the inner gap band.
    let mut rt = make_gapped_runtime(2, 0);
    rt.set_layout(two_pane_layout());
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(rt.move_focus(FocusDirection::Right), Some(ViewId::new(2)));
    assert_eq!(rt.move_focus(FocusDirection::Left), Some(ViewId::new(1)));
}

#[test]
fn focus_movement_next_prev_and_spatial() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(split);
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let next = rt.move_focus(FocusDirection::Next);
    assert_eq!(next, Some(ViewId::new(2)));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    let prev = rt.move_focus(FocusDirection::Prev);
    assert_eq!(prev, Some(ViewId::new(1)));
    // Spatial right from left pane goes to right pane
    let right = rt.move_focus(FocusDirection::Right);
    assert_eq!(right, Some(ViewId::new(2)));
    let left = rt.move_focus(FocusDirection::Left);
    assert_eq!(left, Some(ViewId::new(1)));
}

#[test]
fn deterministic_layout_same_tree_same_container() {
    let mut rt = make_runtime();
    let tree = LayoutNode::split(
        SplitAxis::Vertical,
        0.3,
        LayoutNode::leaf(View::new(ViewId::new(5), 80, 10)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.7,
            LayoutNode::leaf(View::new(ViewId::new(6), 40, 14)),
            LayoutNode::leaf(View::new(ViewId::new(7), 40, 14)),
        ),
    );
    rt.set_layout(tree.clone());
    rt.set_container(UiRect::new(0, 0, 100, 40));
    let a1 = rt.reflow_layout();
    let mut rt2 = make_runtime();
    rt2.set_layout(tree);
    rt2.set_container(UiRect::new(0, 0, 100, 40));
    let a2 = rt2.reflow_layout();
    assert_eq!(a1, a2, "layout must be deterministic");
}

#[test]
fn handle_resize_updates_container_and_reflows() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    );
    rt.set_layout(split);
    // Resize to 800x600 pixels with readable cell 9x19, minus the
    // default 8px window padding inset (CTX-0223) => 87x30 cells
    rt.handle_resize(PhysicalSize::new(800, 600))
        .expect("resize");
    assert_eq!(rt.container(), UiRect::new(0, 0, 87, 30));
    let allocs = rt.layout_allocations();
    // Horizontal split of 87 -> 43 + 44
    assert_eq!(allocs[0].1.width, 43);
    assert_eq!(allocs[1].1.width, 44);
    assert_eq!(allocs[0].1.height, 30);
}

#[test]
fn set_container_headless_seam_without_gpu() {
    let mut rt = make_runtime();
    let split = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 10, 10)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 10)),
    );
    rt.set_layout(split);
    // Drive layout math headlessly without surface resize
    rt.set_container(UiRect::new(0, 0, 60, 20));
    let allocs = rt.layout_allocations();
    assert_eq!(allocs[0].1, UiRect::new(0, 0, 30, 20));
    assert_eq!(allocs[1].1, UiRect::new(30, 0, 30, 20));
    // Tick must still present via headless seam (surface is still 720x456,
    // but layout container is 60x20 cells; rendering will still composite
    // correctly, and no window is required).
    rt.handle_pty_bytes(b"headless");
    assert!(rt.tick().is_some());
    assert!(rt.is_headless());
    assert!(rt.headless_rgba().is_some());
}
