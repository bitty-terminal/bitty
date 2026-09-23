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
