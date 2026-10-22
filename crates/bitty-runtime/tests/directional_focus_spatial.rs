//! Spatial directional-focus regression coverage (issue #2444).
//!
//! `Mod+h/j/k/l` must follow the actual pane geometry, not center-to-center
//! distance: in an L-shaped layout the tall neighbor's center can be closer
//! to the focused pane than the pane directly below it. Each test asserts
//! the exact target pane, including edge stops and deterministic ties.

use bitty_runtime::{
    FocusDirection, LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn make_gapped_runtime(gaps_in: u16, gaps_out: u16) -> Runtime {
    Runtime::new(RuntimeConfig {
        gaps_in,
        gaps_out,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build")
}

fn view(id: u64) -> View {
    View::new(ViewId::new(id), 80, 24)
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt.reflow_layout();
}

fn move_from(rt: &mut Runtime, from: u64, dir: FocusDirection) -> Option<ViewId> {
    assert!(
        rt.set_focus(ViewId::new(from)),
        "view {from} must be focusable"
    );
    rt.move_focus(dir)
}

/// Golden table: focus `from`, move `dir`, assert the exact target.
fn assert_spatial_table(
    name: &str,
    layout: LayoutNode,
    rows: &[(u64, FocusDirection, Option<u64>)],
) {
    let mut rt = make_runtime();
    install(&mut rt, layout);
    for &(from, dir, expected) in rows {
        assert_eq!(
            move_from(&mut rt, from, dir),
            expected.map(ViewId::new),
            "{name}: focus from {from} in {dir:?}"
        );
    }
}

/// Owner repro: left full-height, right split into top (`2`) and bottom
/// (`3`) halves.
fn l_shape() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(view(1)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(view(2)),
            LayoutNode::leaf(view(3)),
        ),
    )
}

fn three_columns() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        1.0 / 3.0,
        LayoutNode::leaf(view(1)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(2)),
            LayoutNode::leaf(view(3)),
        ),
    )
}

fn asymmetric_columns() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.2,
        LayoutNode::leaf(view(1)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.4,
            LayoutNode::leaf(view(2)),
            LayoutNode::leaf(view(3)),
        ),
    )
}

fn t_shape() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1)),
            LayoutNode::leaf(view(2)),
        ),
        LayoutNode::leaf(view(3)),
    )
}

fn nested_splits() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Vertical,
        0.4,
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1)),
            LayoutNode::leaf(view(2)),
        ),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(3)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(view(4)),
                LayoutNode::leaf(view(5)),
            ),
        ),
    )
}

fn full_height_with_three_stack() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(view(1)),
        LayoutNode::split(
            SplitAxis::Vertical,
            1.0 / 3.0,
            LayoutNode::leaf(view(2)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(view(3)),
                LayoutNode::leaf(view(4)),
            ),
        ),
    )
}

fn grid_2x2() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(1)),
            LayoutNode::leaf(view(2)),
        ),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(view(3)),
            LayoutNode::leaf(view(4)),
        ),
    )
}

#[test]
fn l_shape_down_from_top_right_lands_on_bottom_right_issue_2444() {
    let mut rt = make_runtime();
    install(&mut rt, l_shape());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Down),
        Some(ViewId::new(3)),
        "down from the top-right pane must land on the pane directly below"
    );
    assert_eq!(rt.focused_view(), Some(ViewId::new(3)), "focus commits");
}

#[test]
fn l_shape_directions_are_spatially_exact() {
    let mut rt = make_runtime();
    install(&mut rt, l_shape());
    // Top-right pane.
    assert_eq!(move_from(&mut rt, 2, FocusDirection::Up), None);
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Down),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Left),
        Some(ViewId::new(1))
    );
    assert_eq!(move_from(&mut rt, 2, FocusDirection::Right), None);
    // Bottom-right pane.
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Up),
        Some(ViewId::new(2))
    );
    assert_eq!(move_from(&mut rt, 3, FocusDirection::Down), None);
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Left),
        Some(ViewId::new(1))
    );
    assert_eq!(move_from(&mut rt, 3, FocusDirection::Right), None);
    // Full-height left pane: no vertical neighbor and no left neighbor.
    assert_eq!(move_from(&mut rt, 1, FocusDirection::Up), None);
    assert_eq!(move_from(&mut rt, 1, FocusDirection::Down), None);
    assert_eq!(move_from(&mut rt, 1, FocusDirection::Left), None);
    // Symmetric right column: equal edge/center distances resolve by stable
    // layout order, so the top-right pane wins every time.
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2))
    );
}

#[test]
fn three_column_directions_are_spatially_exact() {
    let mut rt = make_runtime();
    install(&mut rt, three_columns());
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Right),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Left),
        Some(ViewId::new(2))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Left),
        Some(ViewId::new(1))
    );
    for id in [1, 2, 3] {
        assert_eq!(move_from(&mut rt, id, FocusDirection::Up), None);
        assert_eq!(move_from(&mut rt, id, FocusDirection::Down), None);
    }
}

#[test]
fn full_height_with_unequal_split_prefers_the_aligned_pane() {
    // The right column is not symmetric: top 25%, bottom 75%. From the
    // top-right pane, down must still pick the bottom-right pane; from the
    // full-height left pane, right picks the closer-center bottom pane.
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(view(1)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.25,
            LayoutNode::leaf(view(2)),
            LayoutNode::leaf(view(3)),
        ),
    );
    let mut rt = make_runtime();
    install(&mut rt, layout);
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Down),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Up),
        Some(ViewId::new(2))
    );
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(3)),
        "the 75%-tall bottom pane has the nearer center on an unequal split"
    );
}

#[test]
fn equal_distance_ties_use_stable_layout_order() {
    let mut rt = make_runtime();
    install(&mut rt, l_shape());
    // Both right panes are the same distance to the right and equally far
    // from the left pane's center; layout order (`2` before `3`) decides.
    for _ in 0..2 {
        assert_eq!(
            move_from(&mut rt, 1, FocusDirection::Right),
            Some(ViewId::new(2))
        );
        assert_eq!(
            move_from(&mut rt, 1, FocusDirection::Right),
            Some(ViewId::new(2)),
            "the tie must not flip with focus history"
        );
    }
    // 2x2 grid ties on the same axis resolve by layout order as well.
    let mut rt = make_runtime();
    install(&mut rt, grid_2x2());
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2))
    );
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Right),
        Some(ViewId::new(4))
    );
}

#[test]
fn two_by_two_grid_focus_is_unchanged() {
    let mut rt = make_runtime();
    install(&mut rt, grid_2x2());
    let expectations = [
        (1, FocusDirection::Up, None),
        (1, FocusDirection::Down, Some(3)),
        (1, FocusDirection::Left, None),
        (1, FocusDirection::Right, Some(2)),
        (2, FocusDirection::Up, None),
        (2, FocusDirection::Down, Some(4)),
        (2, FocusDirection::Left, Some(1)),
        (2, FocusDirection::Right, None),
        (3, FocusDirection::Up, Some(1)),
        (3, FocusDirection::Down, None),
        (3, FocusDirection::Left, None),
        (3, FocusDirection::Right, Some(4)),
        (4, FocusDirection::Up, Some(2)),
        (4, FocusDirection::Down, None),
        (4, FocusDirection::Left, Some(3)),
        (4, FocusDirection::Right, None),
    ];
    for (from, dir, expected) in expectations {
        assert_eq!(
            move_from(&mut rt, from, dir),
            expected.map(ViewId::new),
            "grid focus from {from} in {dir:?}"
        );
    }
}

#[test]
fn l_shape_directions_cross_gap_bands() {
    let mut rt = make_gapped_runtime(2, 1);
    install(&mut rt, l_shape());
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Down),
        Some(ViewId::new(3)),
        "the sibling gap band must not hide the aligned pane"
    );
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Up),
        Some(ViewId::new(2))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Left),
        Some(ViewId::new(1))
    );
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2))
    );
}

#[test]
fn overlays_do_not_hijack_aligned_tile_moves() {
    // 2x2 grid with the bottom-right pane detached: the centered float is
    // visually below-left of the top-left pane's center, but the aligned
    // tiled neighbors must still win.
    let mut rt = make_runtime();
    install(&mut rt, grid_2x2());
    rt.float_detach(ViewId::new(4)).expect("detach 4 to float");
    assert_eq!(
        rt.layout().leaf_ids(),
        vec![ViewId::new(1), ViewId::new(2), ViewId::new(3)]
    );
    assert_eq!(rt.floated_views(), vec![ViewId::new(4)]);
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2)),
        "a centered float must not outrank the aligned right pane"
    );
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Down),
        Some(ViewId::new(3)),
        "a centered float must not outrank the aligned pane below"
    );
}

#[test]
fn tiled_pane_reaches_overlay_by_center_when_no_tile_is_aligned() {
    // Detaching the two right-column panes leaves one full-container tile.
    // The first float is exactly centered (no directional intent); the
    // cascaded second float is offset right, so it is the overlay target.
    let mut rt = make_runtime();
    install(&mut rt, three_columns());
    rt.float_detach(ViewId::new(3)).expect("detach 3 to float");
    rt.float_detach(ViewId::new(2)).expect("detach 2 to float");
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        Some(ViewId::new(2)),
        "center intent must reach the offset overlay when no tile is aligned"
    );
}

#[test]
fn overlay_focus_moves_to_the_tile_in_each_direction() {
    // The centered float has no separated candidate among the remaining
    // 2x2 tiles, so the center fallback resolves each direction; equal
    // perpendicular distances fall back to stable layout order.
    let mut rt = make_runtime();
    install(&mut rt, grid_2x2());
    rt.float_detach(ViewId::new(4)).expect("detach 4 to float");
    assert_eq!(
        move_from(&mut rt, 4, FocusDirection::Left),
        Some(ViewId::new(1))
    );
    assert_eq!(
        move_from(&mut rt, 4, FocusDirection::Up),
        Some(ViewId::new(1)),
        "top-left and top-right tie; stable order picks top-left"
    );
    assert_eq!(
        move_from(&mut rt, 4, FocusDirection::Down),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 4, FocusDirection::Right),
        Some(ViewId::new(2))
    );
}

#[test]
fn golden_spatial_tables_for_whole_layout_class_issue_2444() {
    // Every row is an exact-target assertion: no "some pane in the
    // direction" allowance, so a regression to center-distance or
    // insertion-order ranking fails on the specific shape.
    assert_spatial_table(
        "L-shape",
        l_shape(),
        &[
            (1, FocusDirection::Right, Some(2)),
            (1, FocusDirection::Down, None),
            (2, FocusDirection::Down, Some(3)),
            (2, FocusDirection::Left, Some(1)),
            (2, FocusDirection::Right, None),
            (3, FocusDirection::Up, Some(2)),
            (3, FocusDirection::Left, Some(1)),
            (3, FocusDirection::Down, None),
        ],
    );
    assert_spatial_table(
        "T-shape",
        t_shape(),
        &[
            (1, FocusDirection::Down, Some(3)),
            (2, FocusDirection::Down, Some(3)),
            (1, FocusDirection::Right, Some(2)),
            (2, FocusDirection::Left, Some(1)),
            (3, FocusDirection::Up, Some(1)),
            (1, FocusDirection::Up, None),
            (2, FocusDirection::Up, None),
            (3, FocusDirection::Left, None),
            (3, FocusDirection::Right, None),
            (3, FocusDirection::Down, None),
        ],
    );
    assert_spatial_table(
        "nested-splits",
        nested_splits(),
        &[
            (1, FocusDirection::Down, Some(3)),
            (1, FocusDirection::Right, Some(2)),
            (2, FocusDirection::Down, Some(4)),
            (2, FocusDirection::Left, Some(1)),
            (3, FocusDirection::Up, Some(1)),
            (3, FocusDirection::Right, Some(5)),
            (4, FocusDirection::Up, Some(2)),
            (4, FocusDirection::Down, Some(5)),
            (4, FocusDirection::Left, Some(3)),
            (5, FocusDirection::Up, Some(4)),
            (5, FocusDirection::Left, Some(3)),
            (5, FocusDirection::Right, None),
        ],
    );
    assert_spatial_table(
        "full-height-plus-three-stack",
        full_height_with_three_stack(),
        &[
            (1, FocusDirection::Right, Some(3)),
            (1, FocusDirection::Down, None),
            (2, FocusDirection::Down, Some(3)),
            (2, FocusDirection::Left, Some(1)),
            (3, FocusDirection::Up, Some(2)),
            (3, FocusDirection::Down, Some(4)),
            (3, FocusDirection::Left, Some(1)),
            (4, FocusDirection::Up, Some(3)),
            (4, FocusDirection::Right, None),
        ],
    );
    assert_spatial_table(
        "asymmetric-columns",
        asymmetric_columns(),
        &[
            (1, FocusDirection::Right, Some(2)),
            (2, FocusDirection::Right, Some(3)),
            (3, FocusDirection::Left, Some(2)),
            (2, FocusDirection::Left, Some(1)),
            (1, FocusDirection::Down, None),
            (3, FocusDirection::Up, None),
        ],
    );
    assert_spatial_table(
        "three-column",
        three_columns(),
        &[
            (1, FocusDirection::Right, Some(2)),
            (2, FocusDirection::Right, Some(3)),
            (3, FocusDirection::Left, Some(2)),
            (2, FocusDirection::Left, Some(1)),
            (1, FocusDirection::Down, None),
            (2, FocusDirection::Up, None),
            (3, FocusDirection::Down, None),
        ],
    );
}

#[test]
fn cycle_wrap_and_order_are_unchanged() {
    // `focus_next`/`focus_prev` stay a stable order ring, not geometry:
    // the tree order is [1, 2, 3] and wraps both ways.
    let mut rt = make_runtime();
    install(&mut rt, l_shape());
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Next),
        Some(ViewId::new(1))
    );
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Prev),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Next),
        Some(ViewId::new(3))
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Prev),
        Some(ViewId::new(1))
    );
}

#[test]
fn zero_area_panes_are_not_directional_targets() {
    // A one-pixel-wide container split 50/50 leaves leaf 2 with zero width.
    // Directional focus must not land on it, but the cycle ring still
    // reaches it for recovery.
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(view(1)),
        LayoutNode::leaf(view(2)),
    );
    let mut rt = Runtime::new(RuntimeConfig {
        cell_width: 1,
        gaps_in: 0,
        gaps_out: 0,
        decoration: bitty_ui::Decoration::ZERO,
        ..RuntimeConfig::default()
    })
    .expect("bare config must build");
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 1, 24));
    rt.reflow_layout();
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Right),
        None,
        "an invisible zero-width pane is not a spatial neighbor"
    );
    assert_eq!(
        move_from(&mut rt, 1, FocusDirection::Next),
        Some(ViewId::new(2)),
        "cycle keeps zero-area panes reachable"
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Left),
        None,
        "a zero-area focused pane has no directional neighborhood"
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Prev),
        Some(ViewId::new(1))
    );
}

#[test]
fn empty_layout_directional_focus_is_a_noop() {
    let mut rt = make_runtime();
    rt.set_layout(LayoutNode::stack(vec![]));
    assert_eq!(rt.focused_view(), None);
    for dir in [
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::Left,
        FocusDirection::Right,
    ] {
        assert_eq!(rt.move_focus(dir), None, "direction {dir:?}");
    }
}

#[test]
fn hidden_scratchpad_is_not_a_directional_target() {
    let mut rt = make_runtime();
    install(&mut rt, l_shape());
    rt.scratchpad_hide(ViewId::new(3))
        .expect("hiding the bottom-right pane leaves two live panes");
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
    assert!(
        !rt.present_frames()
            .iter()
            .any(|frame| frame.view == ViewId::new(3)),
        "a parked scratchpad pane paints nothing"
    );
    assert_eq!(
        move_from(&mut rt, 2, FocusDirection::Down),
        None,
        "a hidden scratchpad pane is not a spatial neighbor"
    );
    assert_eq!(
        rt.scratchpad_show().expect("show the parked pane"),
        ViewId::new(3)
    );
    assert_eq!(
        move_from(&mut rt, 3, FocusDirection::Left),
        Some(ViewId::new(2)),
        "the restored pane participates in geometry again"
    );
}

#[test]
fn single_leaf_fullscreen_proxy_stays_put() {
    // The zoom/fullscreen proxy is installed as a single-leaf layout. It has
    // no spatial neighbor, so focus stays put; the chrome layer restores
    // zoom before dispatching a focus chord.
    let mut rt = make_runtime();
    install(&mut rt, LayoutNode::leaf(view(2)));
    for dir in [
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::Left,
        FocusDirection::Right,
    ] {
        assert_eq!(move_from(&mut rt, 2, dir), None, "direction {dir:?}");
        assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    }
}
