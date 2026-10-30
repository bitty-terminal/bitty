#![forbid(unsafe_code)]
//! Issue #2828: a focused pane running a mouse-reporting TUI must not swallow
//! a split/panel border press.
//!
//! Before this contract, the capture decision ran before the border-drag
//! hit test: once the focused app enabled DECSET 1000/1002/1003 (with SGR
//! 1006), every press was encoded into the PTY, so a press on a divider never
//! grabbed it and the mouse resize gesture never started.
//!
//! The contract pinned here:
//!
//! - a plain left press on a split/panel border grabs the divider before any
//!   capture report, reports nothing to the app, and the following drag
//!   resizes the pane live;
//! - the release paired with that chrome press is consumed too, including a
//!   release after the pointer left the window;
//! - presses, motion, and releases away from chrome still reach the
//!   application byte-for-byte (no TUI or shell regression);
//! - the same behavior holds for a shell pane (no mouse mode) and for the
//!   three-pane left / top-right / bottom-right workspace layout.
//!
//! Every position derives from the public headless geometry (9x19 cells,
//! 8px window padding, 6px decoration outer inset, zero gaps), matching
//! `border_drag_resize.rs`.

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PlatformEvent, PressState, WindowEventKind, WindowId,
};
use bitty_runtime::{LayoutNode, PresentFrame, Runtime, SplitAxis, UiRect, View, ViewId};

/// SGR press tracking (`?1000h`) with SGR coordinates (`?1006h`).
const SGR_PRESS_TRACKING: &[u8] = b"\x1b[?1000h\x1b[?1006h";
/// SGR button-event tracking (`?1002h`) with SGR coordinates (`?1006h`).
const SGR_DRAG_TRACKING: &[u8] = b"\x1b[?1002h\x1b[?1006h";
/// SGR any-event tracking (`?1003h`) with SGR coordinates (`?1006h`); the
/// only mode that reports motion without a held button.
const SGR_ANY_MOTION: &[u8] = b"\x1b[?1003h\x1b[?1006h";

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn press(button: MouseButton) -> MouseEvent {
    MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> MouseEvent {
    MouseEvent::new(button, PressState::Released)
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (the `+4.0`/`+9.0` inset lands inside the target cell).
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

/// The reported workspace: full-height left pane plus a right column split
/// into top-right and bottom-right panes.
fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 40, 12)),
            LayoutNode::leaf(View::new(ViewId::new(3), 40, 12)),
        ),
    )
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Centre of frame-local cell `(row, col)` of `view`'s content frame.
fn content_cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y) + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn pane(rt: &Runtime, id: u64) -> UiRect {
    rt.layout_allocations()
        .into_iter()
        .find(|(leaf, _)| *leaf == ViewId::new(id))
        .map(|(_, rect)| rect)
        .unwrap_or_else(|| panic!("p{id} must be allocated"))
}

fn root_ratio(rt: &Runtime) -> f32 {
    rt.layout()
        .split_ratio_at(&[])
        .expect("root split must expose a ratio")
}

fn cursor_left(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    });
}

/// Presses the root divider of an 80-column container and drags `+8` cells,
/// asserting the captured app sees nothing at any step and the left pane
/// lands on 48 columns. Shared by the mouse-mode and shell cases.
fn assert_root_border_drag_reports_nothing(rt: &mut Runtime, expect_capture: bool) {
    assert_eq!(
        rt.mouse_capture_active(),
        expect_capture,
        "fixture capture mode must match the application mouse mode"
    );
    rt.handle_cursor_moved(cell_pixels(39, 12));
    // A 1003 app legitimately owns hover motion before the press; the press
    // itself is the chrome decision under test.
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "a border press must grab the divider before the capture decision"
    );
    assert!(
        rt.pending_input().is_empty(),
        "the grabbing press must not be reported to the app"
    );
    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert!(
        (root_ratio(rt) - 0.6).abs() < 1e-6,
        "the drag must resize the divider live"
    );
    assert_eq!(
        pane(rt, 1).width,
        48,
        "the focused pane must grow with the drag"
    );
    assert!(
        rt.pending_input().is_empty(),
        "drag motion must not be reported to the app"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active(), "release must end the border drag");
    assert!(
        rt.pending_input().is_empty(),
        "the paired release must not be reported to the app"
    );
}

#[test]
fn mouse_reporting_border_press_drag_resizes_and_reports_nothing() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();

    assert_root_border_drag_reports_nothing(&mut rt, true);
}

#[test]
fn drag_tracking_border_press_drag_resizes_and_reports_nothing() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_DRAG_TRACKING);
    rt.drain_pending_input();

    assert_root_border_drag_reports_nothing(&mut rt, true);
}

#[test]
fn shell_pane_border_press_drag_resizes_and_reports_nothing() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());

    assert_root_border_drag_reports_nothing(&mut rt, false);
}

#[test]
fn non_border_press_and_drag_reach_the_app_byte_for_byte() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_PRESS_TRACKING);
    rt.drain_pending_input();
    assert!(rt.mouse_capture_active(), "fixture must capture");

    // Frame-local cell (row 5, col 7) on the focused left pane; the wire is
    // 1-based SGR (`col + 1`, `row + 1`).
    rt.handle_cursor_moved(content_cell_center(&rt, ViewId::new(1), 5, 7));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(
        rt.pending_input(),
        b"\x1b[<0;8;6M",
        "a content press must reach the app unchanged"
    );
    assert!(
        !rt.border_drag_active(),
        "a content press must never grab a divider"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a content press keeps focus"
    );

    rt.drain_pending_input();
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(
        rt.pending_input(),
        b"\x1b[<0;8;6m",
        "the content release must reach the app unchanged"
    );
}

#[test]
fn any_motion_drag_away_from_chrome_still_reaches_the_app() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();

    rt.handle_cursor_moved(content_cell_center(&rt, ViewId::new(1), 5, 7));
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(rt.pending_input(), b"\x1b[<0;8;6M");
    rt.drain_pending_input();

    // One cell right: button `3` + motion bit (`32`) = `35`.
    rt.handle_cursor_moved(content_cell_center(&rt, ViewId::new(1), 5, 8));
    assert_eq!(
        rt.pending_input(),
        b"\x1b[<35;9;6M",
        "1003 motion away from chrome must keep reporting"
    );
    rt.drain_pending_input();

    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(rt.pending_input(), b"\x1b[<0;9;6m");
    assert!(!rt.border_drag_active());
}

#[test]
fn three_pane_left_border_press_drag_resizes_with_mouse_reporting() {
    let mut rt = make_runtime();
    rt.set_layout(three_pane());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();
    assert!(rt.mouse_capture_active(), "fixture must capture");

    // Root divider between the full-height left pane and the right column.
    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "the left/right divider must grab in a three-pane layout"
    );
    assert!(rt.pending_input().is_empty(), "no press report");
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a border owns no leaf: focus must not move"
    );

    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert!((root_ratio(&rt) - 0.6).abs() < 1e-6);
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 48, 24), "left pane");
    assert_eq!(
        pane(&rt, 2),
        UiRect::new(48, 0, 32, 12),
        "top-right pane must follow the shared column line"
    );
    assert_eq!(
        pane(&rt, 3),
        UiRect::new(48, 12, 32, 12),
        "bottom-right pane must follow the shared column line"
    );
    assert!(rt.pending_input().is_empty(), "no drag report");

    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    assert!(rt.pending_input().is_empty(), "no release report");
}

#[test]
fn three_pane_right_column_border_resizes_while_left_mouse_owner_is_focused() {
    let mut rt = make_runtime();
    rt.set_layout(three_pane());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();
    assert!(rt.mouse_capture_active(), "fixture must capture");
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));

    // Row divider between top-right and bottom-right, with the mouse-reporting
    // left pane still holding focus.
    rt.handle_cursor_moved(cell_pixels(60, 11));
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.border_drag_active(),
        "the top/bottom divider must grab even when another pane captures"
    );
    assert!(rt.pending_input().is_empty(), "no press report");

    rt.handle_cursor_moved(cell_pixels(60, 15));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 40, 24), "left pane stays");
    assert_eq!(pane(&rt, 2), UiRect::new(40, 0, 40, 16), "top-right grows");
    assert_eq!(
        pane(&rt, 3),
        UiRect::new(40, 16, 40, 8),
        "bottom-right shrinks"
    );
    assert!(rt.pending_input().is_empty(), "no drag report");

    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
    assert!(rt.pending_input().is_empty(), "no release report");
}

#[test]
fn three_pane_shell_border_press_drag_resizes() {
    let mut rt = make_runtime();
    rt.set_layout(three_pane());
    assert!(!rt.mouse_capture_active(), "no app owns the mouse");

    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert_eq!(pane(&rt, 1), UiRect::new(0, 0, 48, 24), "left pane");
    assert_eq!(pane(&rt, 2), UiRect::new(48, 0, 32, 12), "top-right pane");
    assert_eq!(
        pane(&rt, 3),
        UiRect::new(48, 12, 32, 12),
        "bottom-right pane"
    );
    assert!(rt.pending_input().is_empty(), "no shell input");
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.border_drag_active());
}

#[test]
fn release_outside_window_ends_drag_without_reporting() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();

    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.handle_cursor_moved(cell_pixels(47, 12));
    assert!((root_ratio(&rt) - 0.6).abs() < 1e-6);

    // Outside the container origin: motion keeps the drag armed but cannot
    // map another resize delta.
    rt.handle_cursor_moved(CursorPosition { x: -40.0, y: -40.0 });
    assert!(rt.border_drag_active(), "unmappable motion stays armed");
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        !rt.border_drag_active(),
        "a release outside the window must end the drag"
    );
    assert!(
        (root_ratio(&rt) - 0.6).abs() < 1e-6,
        "the outside release must not move the split again"
    );
    assert!(
        rt.pending_input().is_empty(),
        "no click, drag, or release may reach the app"
    );
}

#[test]
fn cursor_left_then_release_swallows_the_orphan_release() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane_horizontal());
    rt.handle_pty_bytes(SGR_ANY_MOTION);
    rt.drain_pending_input();

    rt.handle_cursor_moved(cell_pixels(39, 12));
    rt.drain_pending_input();
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.border_drag_active());
    rt.handle_cursor_moved(cell_pixels(47, 12));

    cursor_left(&mut rt);
    assert!(
        !rt.border_drag_active(),
        "leaving the window ends the resize gesture"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(
        rt.pending_input().is_empty(),
        "the app never saw the press, so an outside release is not its event"
    );
}
