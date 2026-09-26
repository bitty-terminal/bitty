//! Issue #1348: mouse drag-resize on panel (split) borders.
//!
//! Headless pins: a plain left press on a split divider grabs it (no
//! selection starts, no focus moves), motion adjusts the adjacent split
//! ratio live through the same clamped geometry the keyboard resize path
//! uses, overshoot clamps fail-closed at `[MIN_RATIO, MAX_RATIO]`, sizes
//! persist in the tree across reflows, and the gesture ends on release or
//! when the cursor leaves the window. Shift/Alt presses keep their
//! existing selection/Alt+drag routing.
use bitty_platform::{
    CursorPosition, ModifiersState, MouseButton, PlatformEvent, PressState, WindowEventKind,
    WindowId,
};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry: 8px window padding, 9x19 cells, zero gaps. The
/// `+4.0`/`+9.0` inset lands inside the target cell (never on an edge).
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn two_pane_horizontal() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn two_pane_vertical() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn ratio(rt: &Runtime) -> f32 {
    rt.layout()
        .split_ratio_at(&[])
        .expect("two-pane root must expose a ratio")
}

fn set_modifiers(rt: &mut Runtime, shift: bool, alt: bool) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(ModifiersState {
            shift,
            control: false,
            alt,
            super_pressed: false,
        }),
    });
}

fn cursor_left(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    });
}

#[test]
fn border_press_grabs_divider_without_selection_or_focus_move() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Column 40 is the zero-gap boundary line between the 40-col panes.
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "border press must grab the divider"
    );
    assert!(
        rt.selection().is_none(),
        "grabbing press must not start a selection"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a border owns no leaf: focus must not move"
    );
}

#[test]
fn drag_right_grows_first_pane_live() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // +8 cells on an 80-col container: 0.5 -> 0.6, applied live.
    rt.handle_cursor_moved(cell_pixels(48, 12));
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    let left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, rect)| rect)
        .expect("left pane must be allocated");
    assert_eq!(left.width, 48);
    // Incremental: a further +4 cells lands at 0.65.
    rt.handle_cursor_moved(cell_pixels(52, 12));
    assert!((ratio(&rt) - 0.65).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn drag_left_shrinks_first_pane() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.handle_cursor_moved(cell_pixels(30, 12));
    let expected = 0.5 + (-9.0f32) / 80.0;
    assert!((ratio(&rt) - expected).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn overshoot_clamps_fail_closed_and_tree_survives() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    // Far-right teleport clamps at MAX_RATIO instead of breaking the tree.
    rt.handle_cursor_moved(cell_pixels(79, 12));
    assert!((ratio(&rt) - LayoutNode::MAX_RATIO).abs() < f32::EPSILON);
    assert_eq!(rt.leaf_count(), 2);
    assert!(rt.border_drag_active(), "clamped drag stays armed");
    // Far-left teleport clamps at MIN_RATIO.
    rt.handle_cursor_moved(cell_pixels(0, 12));
    assert!((ratio(&rt) - LayoutNode::MIN_RATIO).abs() < f32::EPSILON);
    assert_eq!(rt.leaf_count(), 2);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn pane_press_starts_selection_not_drag() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.border_drag_active(),
        "interior press must not grab a divider"
    );
    assert!(
        rt.selection().is_some(),
        "interior press keeps the selection path"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn release_ends_drag_and_later_motion_is_inert() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(48, 12));
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert!(
        (ratio(&rt) - 0.6).abs() < 1e-6,
        "motion after release must not resize"
    );
}

#[test]
fn shift_press_on_border_forces_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    set_modifiers(&mut rt, true, false);
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.border_drag_active(),
        "Shift keeps the accessibility escape: no grab"
    );
    assert!(
        rt.selection().is_some(),
        "Shift+press keeps the selection path"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn alt_press_on_border_keeps_alt_drag_routing() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    set_modifiers(&mut rt, false, true);
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.border_drag_active(),
        "Alt presses belong to the Alt+drag/block-selection paths"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn vertical_split_consumes_row_delta_only() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_vertical());
    // Row 12 is the zero-gap boundary line between the 12-row panes.
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // Pure column motion: consumed, ratio untouched.
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert!((ratio(&rt) - 0.5).abs() < 1e-6);
    assert!(rt.border_drag_active());
    // +4 rows on a 24-row container: 0.5 -> 0.5 + 4/24.
    rt.handle_cursor_moved(cell_pixels(60, 16));
    let expected = 0.5 + 4.0f32 / 24.0;
    assert!((ratio(&rt) - expected).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn dragged_size_persists_across_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(48, 12));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    // Ratios live in the tree: reflow (and any later present) preserves
    // the dragged size per layout.
    rt.reflow_layout();
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    let left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, rect)| rect)
        .expect("left pane must be allocated");
    assert_eq!(left, UiRect::new(0, 0, 48, 24));
}

#[test]
fn hover_query_reports_divider_axis() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    assert_eq!(
        rt.border_drag_hover_at(cell_pixels(40, 12)),
        Some(SplitAxis::Horizontal),
        "hovering the divider reports its axis for cursor feedback"
    );
    assert_eq!(
        rt.border_drag_hover_at(cell_pixels(10, 12)),
        None,
        "interior hover reports no divider"
    );
}

#[test]
fn cursor_leaving_window_ends_drag() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    cursor_left(&mut rt);
    assert!(!rt.border_drag_active());
}

// ---------------------------------------------------------------------------
// Issue #1445: Corner-drag resize (Hyprland/Niri 4-way model)
// ---------------------------------------------------------------------------

/// 2x2 grid: four panes in a quad layout.
fn four_pane_grid() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(1), 40, 12)),
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 12)),
        ),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 12)),
            LayoutNode::leaf(View::new(ViewId::new(4), 40, 12)),
        ),
    )
}

#[test]
fn corner_drag_grabs_both_perpendicular_splits() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    // Column 39-40, row 11-12 is near the center corner where splits meet.
    // Use (39, 11) to ensure we're inside the left child's bounds and can
    // hit both the horizontal split at x=40 and vertical split at y=12.
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "corner press must grab perpendicular splits"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn corner_drag_resizes_all_four_adjacent_panels() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    // Grab near the center corner - use (39, 11) to be inside left child's
    // bounds so we can hit both the horizontal [] and vertical [0] splits.
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // Drag right (+8 cols) and down (+4 rows): resizes horizontally via []
    // and vertically via [0].
    rt.handle_cursor_moved(cell_pixels(47, 15));

    // Check horizontal resize: left column grows, right column shrinks.
    // Top-left pane (ViewId 1) should be in a 48-wide column.
    let top_left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, rect)| rect)
        .expect("top-left pane must be allocated");
    assert_eq!(
        top_left.width, 48,
        "corner drag right should grow left column (all left panes)"
    );

    // Check vertical resize: only left column's vertical split [0] resizes.
    // Top-left grows to 16 rows, bottom-left shrinks to 8 rows.
    assert_eq!(
        top_left.height, 16,
        "corner drag down should grow top-left pane via [0] vertical split"
    );

    let bottom_left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(2))
        .map(|(_, rect)| rect)
        .expect("bottom-left pane must be allocated");
    assert_eq!(
        bottom_left.height, 8,
        "corner drag down should shrink bottom-left pane"
    );

    // Right column (32 wide) keeps its 50/50 vertical split (12 rows each).
    let top_right = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(3))
        .map(|(_, rect)| rect)
        .expect("top-right pane must be allocated");
    assert_eq!(
        top_right.width, 32,
        "corner drag right should shrink right column"
    );
    assert_eq!(
        top_right.height, 12,
        "right column's vertical split [1] should be unaffected"
    );

    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn corner_drag_left_and_up_shrinks_first_quadrant() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    // Drag left (-8 cols) and up (-4 rows).
    rt.handle_cursor_moved(cell_pixels(31, 7));

    // Horizontal: left column shrinks to 32, right grows to 48.
    // Vertical: only [0] split in left column resizes.
    let top_left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, rect)| rect)
        .expect("top-left pane must be allocated");
    assert_eq!(
        top_left.width, 32,
        "corner drag left should shrink left column"
    );
    // Dragging from (39, 11) to (31, 7) is a delta of (-8, -4).
    // The vertical split ratio change is -4/24 = -0.167, so 0.5 - 0.167 ≈ 0.333.
    // On a 24-row container, 0.333 * 24 = 8 rows, but cell boundaries may round to 7.
    assert!(
        top_left.height >= 7 && top_left.height <= 8,
        "corner drag up should shrink top-left pane to ~8 rows, got {}",
        top_left.height
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn corner_drag_persists_both_ratios_across_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(47, 15));
    rt.handle_mouse_input(release(MouseButton::Left));

    // Horizontal split [] ratio changed (left column grew).
    let h_ratio = rt
        .layout()
        .split_ratio_at(&[])
        .expect("root horizontal split must exist");
    assert!(
        (h_ratio - 0.6).abs() < 1e-6,
        "horizontal ratio should be 0.6"
    );

    // Vertical split [0] in left column changed (top-left grew).
    let v_ratio_left = rt
        .layout()
        .split_ratio_at(&[0])
        .expect("left vertical split must exist");
    let expected_v = 16.0 / 24.0; // ~0.667
    assert!(
        (v_ratio_left - expected_v).abs() < 1e-6,
        "left vertical ratio should be ~0.667, got {v_ratio_left}"
    );

    // Vertical split [1] in right column unchanged (still 0.5).
    let v_ratio_right = rt
        .layout()
        .split_ratio_at(&[1])
        .expect("right vertical split must exist");
    assert!(
        (v_ratio_right - 0.5).abs() < 1e-6,
        "right vertical ratio should stay 0.5"
    );

    // Reflow preserves all ratios.
    rt.reflow_layout();
    assert_eq!(
        rt.layout().split_ratio_at(&[]),
        Some(h_ratio),
        "horizontal ratio must persist"
    );
    assert_eq!(
        rt.layout().split_ratio_at(&[0]),
        Some(v_ratio_left),
        "left vertical ratio must persist"
    );
    assert_eq!(
        rt.layout().split_ratio_at(&[1]),
        Some(v_ratio_right),
        "right vertical ratio must persist"
    );
}
