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
use bitty_runtime::{
    BandContent, ChromeBands, LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};
use bitty_ui::{CellPos, Selection};

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
/// headless geometry: 8px window padding, 6px decoration outer inset at
/// scale 1, 9x19 cells, zero gaps. The `+4.0`/`+9.0` inset lands inside
/// the target cell (never on an edge).
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 6.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 6.0 + f64::from(row) * 19.0 + 9.0,
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

/// Allocation of one pane, for exact geometry assertions.
fn pane(rt: &Runtime, id: u64) -> UiRect {
    rt.layout_allocations()
        .into_iter()
        .find(|(leaf, _)| *leaf == ViewId::new(id))
        .map(|(_, rect)| rect)
        .unwrap_or_else(|| panic!("p{id} must be allocated"))
}

/// Every pane stays inside `bounds`, panes never overlap, and the allocation
/// area equals the container area (zero-gap tiling partitions exactly).
fn assert_partition(rt: &Runtime, bounds: UiRect) {
    let alloc = rt.layout_allocations();
    let area: u64 = alloc
        .iter()
        .map(|(_, rect)| u64::from(rect.width) * u64::from(rect.height))
        .sum();
    assert_eq!(
        area,
        u64::from(bounds.width) * u64::from(bounds.height),
        "tiled panes must cover the container exactly: {alloc:?}"
    );
    for (i, (id, rect)) in alloc.iter().enumerate() {
        assert!(
            rect.right() <= bounds.right() && rect.bottom() <= bounds.bottom(),
            "p{id:?} escapes the container: {rect:?}"
        );
        for (other_id, other) in alloc.iter().skip(i + 1) {
            assert!(
                rect.intersection(*other).is_none(),
                "p{id:?} overlaps p{other_id:?}: {rect:?} vs {other:?}"
            );
        }
    }
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
    rt.handle_cursor_moved(cell_pixels(39, 12));
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
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // +8 cells on an 80-col container: 0.5 -> 0.6, applied live.
    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    let left = rt
        .layout_allocations()
        .into_iter()
        .find(|(id, _)| *id == ViewId::new(1))
        .map(|(_, rect)| rect)
        .expect("left pane must be allocated");
    assert_eq!(left.width, 48);
    // Incremental: a further +4 cells lands at 0.65.
    rt.handle_cursor_moved(cell_pixels(51, 12));
    assert!((ratio(&rt) - 0.65).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn internal_live_drag_commit_preserves_active_drag() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert!(
        rt.border_drag_active(),
        "internal commit must keep the drag armed"
    );
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);

    rt.handle_cursor_moved(cell_pixels(51, 12));
    assert!((ratio(&rt) - 0.65).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
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
fn resize_cancels_border_drag_before_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.handle_resize(bitty_platform::PhysicalSize::new(800, 600))
        .expect("valid resize");
    assert!(
        !rt.border_drag_active(),
        "resize must cancel the stale drag"
    );

    let resized = rt.layout().clone();
    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert_eq!(
        rt.layout(),
        &resized,
        "motion from the canceled drag must not mutate reflowed geometry"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn chrome_band_refresh_cancels_border_drag_before_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.set_chrome_bands(ChromeBands {
        bottom: vec![BandContent {
            plugin_id: "test-band".to_string(),
            slot: bitty_lua::ui::UiSlot::Bottom,
            root: bitty_lua::ui::UiNode::text("band"),
            version: 1,
        }],
        ..ChromeBands::default()
    });
    rt.tick().expect("band refresh must present");
    assert!(
        !rt.border_drag_active(),
        "container refresh must cancel the stale drag"
    );

    let refreshed = rt.layout().clone();
    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert_eq!(
        rt.layout(),
        &refreshed,
        "motion from the canceled drag must not mutate refreshed geometry"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn set_container_cancels_active_border_drag_before_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.set_container(UiRect::new(0, 0, 72, 24));
    assert!(!rt.border_drag_active());
    let changed = rt.layout().clone();
    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert_eq!(rt.layout(), &changed);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn set_window_cells_cancels_active_border_drag_before_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.set_window_cells(UiRect::new(0, 0, 72, 24));
    assert!(!rt.border_drag_active());
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn reflow_layout_cancels_active_border_drag_before_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    rt.reflow_layout();
    assert!(!rt.border_drag_active());
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn canceled_border_drag_release_does_not_finalize_existing_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.set_selection(Selection::simple(CellPos::new(0, 0), CellPos::new(0, 1)));
    assert!(rt.is_selection_dragging());

    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.set_window_cells(UiRect::new(0, 0, 72, 24));
    assert!(!rt.border_drag_active());

    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        rt.is_selection_dragging(),
        "the canceled border press must consume its later release"
    );
}

#[test]
fn canceled_border_drag_release_is_consumed_under_mouse_capture() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    // External geometry changes leave a release tombstone so the consumed
    // border press cannot fall through into selection or auto-copy.
    rt.set_window_cells(UiRect::new(0, 0, 72, 24));
    assert!(!rt.border_drag_active());

    // Enable capture after cancellation: the orphan release must still be
    // consumed by the canceled host gesture rather than sent to the app.
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    assert!(rt.mouse_capture_active());
    rt.drain_pending_input();
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        rt.pending_input().is_empty(),
        "canceled border release must not reach a newly capturing app"
    );

    // The tombstone is one-shot: a later ordinary release follows capture
    // routing normally instead of being swallowed by the stale drag.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        !rt.pending_input().is_empty(),
        "after the canceled release, ordinary capture releases remain routed"
    );
}

#[test]
fn layout_mut_cancels_active_border_drag_before_replacement() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    // Keep the same split path and axis so stale motion would visibly apply
    // the old press delta to this replacement if layout_mut failed to cancel.
    let replacement = LayoutNode::split(
        SplitAxis::Horizontal,
        0.25,
        LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(4), 80, 24)),
    );
    *rt.layout_mut() = replacement;
    assert!(
        !rt.border_drag_active(),
        "layout mutation must cancel the drag"
    );
    assert!((ratio(&rt) - 0.25).abs() < 1e-6);

    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert!((ratio(&rt) - 0.25).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.is_selection_dragging());
}

#[test]
fn external_layout_replacement_cancels_border_drag() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());

    let replacement = LayoutNode::split(
        SplitAxis::Horizontal,
        0.25,
        LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(4), 80, 24)),
    );
    rt.set_layout(replacement);
    assert!(
        !rt.border_drag_active(),
        "layout replacement must cancel the stale drag"
    );

    let replaced = rt.layout().clone();
    assert!((ratio(&rt) - 0.25).abs() < 1e-6);
    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert_eq!(
        rt.layout(),
        &replaced,
        "motion from the canceled drag must not mutate replacement geometry"
    );

    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.set_layout_closing(
        LayoutNode::leaf(View::new(ViewId::new(5), 80, 24)),
        ViewId::new(1),
    );
    assert!(
        !rt.border_drag_active(),
        "closing layout replacement must cancel the stale drag"
    );
    let replaced = rt.layout().clone();
    assert!(!rt.update_border_drag_at(cell_pixels(47, 12), std::time::Instant::now()));
    assert_eq!(
        rt.layout(),
        &replaced,
        "motion after set_layout_closing must not mutate replacement geometry"
    );
}

#[test]
fn overshoot_clamps_at_minimum_pane_size_and_tree_survives() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    // Far-right teleport clamps at the 10-column minimum for the second
    // pane instead of breaking the tree (issue #2572).
    rt.handle_cursor_moved(cell_pixels(79, 12));
    assert!((ratio(&rt) - 0.875).abs() < 1e-6, "70/80 columns");
    assert_eq!(pane(&rt, 1).width, 70);
    assert_eq!(pane(&rt, 2).width, 10);
    assert_eq!(rt.leaf_count(), 2);
    assert!(rt.border_drag_active(), "clamped drag stays armed");
    // Far-left teleport clamps at the 10-column minimum for the first pane.
    rt.handle_cursor_moved(cell_pixels(0, 12));
    assert!((ratio(&rt) - 0.125).abs() < 1e-6, "10/80 columns");
    assert_eq!(pane(&rt, 1).width, 10);
    assert_eq!(pane(&rt, 2).width, 70);
    assert_eq!(rt.leaf_count(), 2);
    assert_partition(&rt, UiRect::new(0, 0, 80, 24));
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
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(47, 12));
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
    let before = ratio(&rt);
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.border_drag_active(),
        "Shift keeps the accessibility escape: no grab"
    );
    // CTX-0803 (#1476): the press falls through to the selection path, and
    // the selection path is View-owned. The divider cell lies inside the
    // right pane's frame, and that pane owns no grid here (no shell, and the
    // primary grid belongs to the left pane), so the press selects nothing
    // instead of fabricating a selection bound to the *left* pane's grid.
    // The pre-CTX-0803 assertion (`selection().is_some()`) passed only
    // because of that primary binding.
    assert_eq!(
        rt.selection_owner(),
        None,
        "a press in a pane that owns no grid must not select another pane's grid"
    );
    assert!(
        rt.selection().is_none(),
        "fail closed: no grid under the pointer means no selection"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        (ratio(&rt) - before).abs() < 1e-6,
        "Shift+press must not resize the split"
    );
}

/// Companion to [`shift_press_on_border_forces_selection`]: the same Shift
/// escape over a pane that *does* own a grid still selects, and the selection
/// is owned by that pane (CTX-0803).
#[test]
fn shift_press_selects_in_the_pane_that_owns_the_grid() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    let primary = rt.primary_view().expect("headless runtime pins a primary");
    set_modifiers(&mut rt, true, false);
    rt.handle_cursor_moved(cell_pixels(4, 6));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(!rt.border_drag_active(), "no divider under the pointer");
    assert_eq!(
        rt.selection_owner(),
        Some(primary),
        "the selection belongs to the pane under the pointer"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn alt_press_on_border_keeps_alt_drag_routing() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    set_modifiers(&mut rt, false, true);
    rt.handle_cursor_moved(cell_pixels(39, 12));
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
    // Row 11 sits on the headed 6px band between the 12-row panes.
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // Pure column motion: consumed, ratio untouched.
    rt.handle_cursor_moved(cell_pixels(60, 11));
    assert!((ratio(&rt) - 0.5).abs() < 1e-6);
    assert!(rt.border_drag_active());
    // +4 rows on a 24-row container: 0.5 -> 0.5 + 4/24.
    rt.handle_cursor_moved(cell_pixels(60, 15));
    let expected = 0.5 + 4.0f32 / 24.0;
    assert!((ratio(&rt) - expected).abs() < 1e-6);
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn dragged_size_persists_across_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(47, 12));
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
        rt.border_drag_hover_at(cell_pixels(39, 12)),
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
    rt.handle_cursor_moved(cell_pixels(39, 12));
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

/// T junction: the right row divider is deliberately not collinear with the
/// left row divider, so a hit at the left T must not grab the right sibling.
fn t_junction_grid() -> LayoutNode {
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
            0.75,
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 12)),
            LayoutNode::leaf(View::new(ViewId::new(4), 40, 12)),
        ),
    )
}

fn assert_t_junction_drag_invariants(
    rt: &Runtime,
    expected_splits: &[(Vec<usize>, SplitAxis)],
    expected_leaf_ids: &[ViewId],
) {
    assert_eq!(
        rt.border_drag_splits(),
        Some(expected_splits.to_vec()),
        "the grabbed split set must not change during motion"
    );
    assert_eq!(rt.layout().leaf_ids(), expected_leaf_ids);
    assert_eq!(rt.leaf_count(), expected_leaf_ids.len());
    rt.layout()
        .validate_structure()
        .expect("dragged layout must remain structurally valid");
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a T junction owns no leaf"
    );
    assert!(
        rt.selection().is_none(),
        "a border drag starts no selection"
    );
    assert_partition(rt, UiRect::new(0, 0, 80, 24));

    // The non-collinear right sibling is never a grabbed split. Its row
    // boundary therefore stays at 18/24 even while the root moves.
    assert!((rt.layout().split_ratio_at(&[1]).unwrap() - 0.75).abs() < 1e-6);
    assert_eq!(pane(rt, 3).y, 0);
    assert_eq!(pane(rt, 3).height, 18);
    assert_eq!(pane(rt, 4).y, 18);
    assert_eq!(pane(rt, 4).height, 6);
}

#[test]
fn t_junction_drag_grabs_only_collinear_splits() {
    let mut rt = make_runtime();
    rt.set_layout(t_junction_grid());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));

    // The exact left T is the root horizontal split plus the left child
    // vertical split. The right child's row divider is lower and is not part
    // of this collinear line.
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active(), "the left T must grab");
    assert_eq!(
        rt.border_drag_splits(),
        Some(vec![
            (Vec::new(), SplitAxis::Horizontal),
            (vec![0], SplitAxis::Vertical),
        ])
    );
    assert_eq!(
        rt.layout().split_ratio_at(&[1]),
        Some(0.75),
        "the non-collinear right row divider remains separate"
    );
    assert!(rt.selection().is_none());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    assert_eq!(rt.border_drag_splits(), None);
}

#[test]
fn t_junction_drag_uses_press_anchor_for_incremental_motion() {
    let mut rt = make_runtime();
    rt.set_layout(t_junction_grid());
    let expected_splits = vec![
        (Vec::new(), SplitAxis::Horizontal),
        (vec![0], SplitAxis::Vertical),
    ];
    let expected_leaf_ids = vec![
        ViewId::new(1),
        ViewId::new(2),
        ViewId::new(3),
        ViewId::new(4),
    ];
    let press_anchor = cell_pixels(39, 11);

    rt.handle_cursor_moved(press_anchor);
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);
    let at_press = rt.layout_allocations();

    // A no-op motion must preserve the press-time allocation exactly.
    rt.handle_cursor_moved(press_anchor);
    assert_eq!(rt.layout_allocations(), at_press);
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    // Each one-cell motion is measured from the original press, not from the
    // previous motion. A cumulative implementation would produce 43 cells on
    // the second step instead of 42.
    rt.handle_cursor_moved(cell_pixels(40, 11));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 41.0 / 80.0).abs() < 1e-6);
    assert!((rt.layout().split_ratio_at(&[0]).unwrap() - 0.5).abs() < 1e-6);
    assert_eq!(pane(&rt, 1).width, 41);
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    rt.handle_cursor_moved(cell_pixels(41, 11));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 42.0 / 80.0).abs() < 1e-6);
    assert_eq!(pane(&rt, 1).width, 42);
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    // A diagonal motion updates both grabbed axes from the same press anchor.
    rt.handle_cursor_moved(cell_pixels(42, 12));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 43.0 / 80.0).abs() < 1e-6);
    assert!((rt.layout().split_ratio_at(&[0]).unwrap() - 13.0 / 24.0).abs() < 1e-6);
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 43, 13));
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    // Move backward across the press anchor. The absolute position follows
    // immediately in both directions rather than accumulating the prior
    // forward motion.
    rt.handle_cursor_moved(cell_pixels(38, 10));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 39.0 / 80.0).abs() < 1e-6);
    assert!((rt.layout().split_ratio_at(&[0]).unwrap() - 11.0 / 24.0).abs() < 1e-6);
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 39, 11));
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    // Returning to the press cell returns both splits to their original
    // allocations, regardless of the preceding incremental motions.
    rt.handle_cursor_moved(press_anchor);
    assert_eq!(rt.layout_allocations(), at_press);
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    // Clamp both grabbed axes, then recover one axis while the other remains
    // clamped. This also proves that a clamp does not create a dead zone.
    rt.handle_cursor_moved(cell_pixels(79, 0));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 0.875).abs() < 1e-6);
    assert!((rt.layout().split_ratio_at(&[0]).unwrap() - 4.0 / 24.0).abs() < 1e-6);
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 70, 4));
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    rt.handle_cursor_moved(cell_pixels(79, 11));
    assert!((rt.layout().split_ratio_at(&[]).unwrap() - 0.875).abs() < 1e-6);
    assert!((rt.layout().split_ratio_at(&[0]).unwrap() - 0.5).abs() < 1e-6);
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 70, 12));
    assert_t_junction_drag_invariants(&rt, &expected_splits, &expected_leaf_ids);

    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    assert_eq!(rt.border_drag_splits(), None);
    assert!(
        rt.selection().is_none(),
        "release must not commit a selection"
    );
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let after_release = rt.layout_allocations();
    rt.handle_cursor_moved(cell_pixels(0, 23));
    assert_eq!(rt.layout_allocations(), after_release);
}

#[test]
fn junction_drag_grabs_cross_with_both_row_segments() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Issue #2572: the center gap cell is the four-pane inner junction.
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "center junction press must grab the cross"
    );
    let splits = rt.border_drag_splits().expect("active drag exposes splits");
    assert_eq!(
        splits.len(),
        3,
        "junction must grab the column split plus both row segments, got {splits:?}"
    );
    assert!(
        splits.contains(&(Vec::new(), SplitAxis::Horizontal)),
        "shared vertical divider (root split), got {splits:?}"
    );
    assert!(
        splits.contains(&(vec![0], SplitAxis::Vertical)),
        "left row segment [0], got {splits:?}"
    );
    assert!(
        splits.contains(&(vec![1], SplitAxis::Vertical)),
        "right row segment [1], got {splits:?}"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a junction owns no leaf: focus must not move"
    );
    assert!(
        rt.selection().is_none(),
        "junction press starts no selection"
    );
    assert_eq!(
        rt.layout().leaf_ids(),
        vec![
            ViewId::new(1),
            ViewId::new(2),
            ViewId::new(3),
            ViewId::new(4)
        ],
        "a resize must never reorder panes"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(
        rt.layout().leaf_ids(),
        vec![
            ViewId::new(1),
            ViewId::new(2),
            ViewId::new(3),
            ViewId::new(4)
        ],
        "release must commit the same pane order"
    );
}

#[test]
fn junction_drag_resizes_all_four_panels_on_both_axes() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    // Diagonal drag (+8 columns, +4 rows) from the junction gap cell.
    rt.handle_cursor_moved(cell_pixels(47, 15));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 48, 16), "top-left");
    assert_eq!(pane(&rt, 2), UiRect::new(0, 16, 48, 8), "bottom-left");
    assert_eq!(pane(&rt, 3), UiRect::new(48, 0, 32, 16), "top-right");
    assert_eq!(
        pane(&rt, 4),
        UiRect::new(48, 16, 32, 8),
        "bottom-right must move with the shared row line"
    );
    assert_partition(&rt, UiRect::new(0, 0, 80, 24));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn junction_drag_left_and_up_shrinks_both_columns() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    // Diagonal drag (-8 columns, -4 rows) from the junction gap cell.
    rt.handle_cursor_moved(cell_pixels(31, 7));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 32, 8), "top-left");
    assert_eq!(pane(&rt, 2), UiRect::new(0, 8, 32, 16), "bottom-left");
    assert_eq!(pane(&rt, 3), UiRect::new(32, 0, 48, 8), "top-right");
    assert_eq!(pane(&rt, 4), UiRect::new(32, 8, 48, 16), "bottom-right");
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn junction_drag_persists_both_ratios_across_reflow() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    rt.handle_cursor_moved(cell_pixels(47, 15));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!((ratio(&rt) - 0.6).abs() < 1e-6, "root ratio 0.6");
    let expected_v = 16.0f32 / 24.0;
    for path in [vec![0], vec![1]] {
        let got = rt.layout().split_ratio_at(&path).expect("row split");
        assert!(
            (got - expected_v).abs() < 1e-6,
            "row split {path:?} ratio should be {expected_v}, got {got}"
        );
    }
    rt.reflow_layout();
    assert!((ratio(&rt) - 0.6).abs() < 1e-6);
    for path in [vec![0], vec![1]] {
        let got = rt.layout().split_ratio_at(&path).expect("row split");
        assert!((got - expected_v).abs() < 1e-6);
    }
}

#[test]
fn junction_drag_clamps_at_minimum_without_disturbing_the_other_axis() {
    let mut rt = make_runtime();
    rt.set_layout(four_pane_grid());
    rt.handle_cursor_moved(cell_pixels(39, 11));
    rt.handle_mouse_input(press(MouseButton::Left));
    // Extreme diagonal: +40 columns would leave an 80-col column; -11 rows
    // would invert the row. Both clamp to the 10x4 per-pane minimum.
    rt.handle_cursor_moved(cell_pixels(79, 0));
    assert_eq!(
        pane(&rt, 1),
        UiRect::new(0, 0, 70, 4),
        "horizontal clamp must not be disturbed by the vertical clamp"
    );
    assert_eq!(pane(&rt, 2), UiRect::new(0, 4, 70, 20));
    assert_eq!(pane(&rt, 3), UiRect::new(70, 0, 10, 4));
    assert_eq!(pane(&rt, 4), UiRect::new(70, 4, 10, 20));
    assert_partition(&rt, UiRect::new(0, 0, 80, 24));
    // Reverse vertically while horizontal stays clamped: the absolute
    // press anchor means the row follows immediately (no clamp dead zone).
    rt.handle_cursor_moved(cell_pixels(79, 11));
    assert_eq!(
        pane(&rt, 1),
        UiRect::new(0, 0, 70, 12),
        "vertical motion must recover with no dead zone"
    );
    assert_eq!(pane(&rt, 2), UiRect::new(0, 12, 70, 12));
    // Push the other way: vertical clamps at the maximum row, horizontal
    // stays put.
    rt.handle_cursor_moved(cell_pixels(79, 23));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 70, 20));
    assert_eq!(pane(&rt, 2), UiRect::new(0, 20, 70, 4));
    assert_eq!(pane(&rt, 3), UiRect::new(70, 0, 10, 20));
    assert_eq!(pane(&rt, 4), UiRect::new(70, 20, 10, 4));
    assert_partition(&rt, UiRect::new(0, 0, 80, 24));
    rt.handle_mouse_input(release(MouseButton::Left));
}

#[test]
fn junction_drag_with_gaps_clamps_each_pane_to_10x4() {
    // Issue #2572: the drag clamp is gap-aware. `gaps_in = 2` reserves two
    // cells between siblings before the 10x4 minima are applied.
    let mut rt = Runtime::new(RuntimeConfig {
        gaps_in: 2,
        gaps_out: 0,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build");
    rt.set_layout(four_pane_grid());
    // The dead-center gap square used to be the ambiguous no-grab case;
    // it is now the junction.
    rt.handle_cursor_moved(cell_pixels(40, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active(), "gapped center junction must grab");
    let splits = rt.border_drag_splits().expect("active drag exposes splits");
    assert_eq!(
        splits.len(),
        3,
        "gapped junction is still a cross: {splits:?}"
    );
    // Extreme diagonal: +39 columns, -12 rows.
    rt.handle_cursor_moved(cell_pixels(79, 0));
    // Width: usable 78 -> first 68, gap 2, second 10. Height: usable 22 ->
    // first 4, gap 2, second 18.
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 68, 4));
    assert_eq!(pane(&rt, 2), UiRect::new(0, 6, 68, 18));
    assert_eq!(pane(&rt, 3), UiRect::new(70, 0, 10, 4));
    assert_eq!(pane(&rt, 4), UiRect::new(70, 6, 10, 18));
    for (id, rect) in rt.layout_allocations() {
        assert!(
            rect.width >= bitty_runtime::MIN_PANE_COLS
                && rect.height >= bitty_runtime::MIN_PANE_ROWS,
            "p{id:?} below the gap-aware 10x4 minimum: {rect:?}"
        );
    }
    // Vertical clamp at the maximum: first 18, second 4, width unchanged.
    rt.handle_cursor_moved(cell_pixels(79, 23));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 68, 18));
    assert_eq!(pane(&rt, 2), UiRect::new(0, 20, 68, 4));
    assert_eq!(pane(&rt, 3), UiRect::new(70, 0, 10, 18));
    assert_eq!(pane(&rt, 4), UiRect::new(70, 20, 10, 4));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn deco_outer_shifts_divider_hover_to_headed_origin() {
    // CTX-1168 issue 1983: the layout-point origin includes the 6px decoration
    // outer inset at scale 1. The headed divider hovers its axis, the stale
    // pad-only divider pixels sit one cell inside the left pane and hover
    // nothing, and the outer band hovers nothing. Both divider assertions
    // fail pre-fix where the mapping is pad-only.
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    assert_eq!(
        rt.border_drag_hover_at(cell_pixels(39, 12)),
        Some(SplitAxis::Horizontal),
        "headed divider must hover its axis"
    );
    let stale = CursorPosition {
        x: 8.0 + f64::from(39) * 9.0 + 4.0,
        y: 8.0 + f64::from(12) * 19.0 + 9.0,
    };
    assert_eq!(
        rt.border_drag_hover_at(stale),
        None,
        "stale pad-only divider pixels must miss headed"
    );
    let outer = CursorPosition { x: 11.0, y: 245.0 };
    assert_eq!(
        rt.border_drag_hover_at(outer),
        None,
        "outer band must hover no divider"
    );
}
