#![forbid(unsafe_code)]
//! Selection inside detached floating panels (issue #2523).
//!
//! Detached floats are not leaves of the tiling tree, so the fail-closed
//! selection guard used to drop every press inside one: mouse selection and
//! its highlight never worked there. These tests prove the float grid is a
//! first-class selection owner while the tiled panes keep their behavior.

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PressState, ScrollDelta, WindowEventKind, WindowId,
};
use bitty_runtime::{
    LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime builds");
    rt.handle_pty_bytes(b"abcdef\r\nghijkl");
    rt
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

fn edge_x(rt: &Runtime, frame: &PresentFrame, col: u16) -> f64 {
    let (cw, _) = rt.live_cell_size();
    f64::from(rt.window_padding_physical())
        + f64::from(frame.content.x)
        + f64::from(col) * f64::from(cw)
}

fn edge_y(rt: &Runtime, frame: &PresentFrame, row: u16) -> f64 {
    let (_, ch) = rt.live_cell_size();
    f64::from(rt.window_padding_physical())
        + f64::from(frame.content.y)
        + f64::from(row) * f64::from(ch)
        + f64::from(ch) / 2.0
}

fn at(rt: &Runtime, frame: &PresentFrame, col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: edge_x(rt, frame, col),
        y: edge_y(rt, frame, row),
    }
}

fn window_event(kind: WindowEventKind) -> bitty_platform::PlatformEvent {
    bitty_platform::PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    }
}

fn drag(rt: &mut Runtime, start: CursorPosition, end: CursorPosition) {
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(start)));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Pressed,
    ))));
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(end)));
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(MouseEvent::new(
        MouseButton::Left,
        PressState::Released,
    ))));
}

/// Asserts the active selection adds at least one fill versus the same frame
/// with the selection cleared (issue #2523 review: background fills make a
/// bare `fills > 0` vacuous).
fn assert_selection_adds_fills(rt: &mut Runtime, context: &str) {
    let selected = rt
        .tick()
        .unwrap_or_else(|| panic!("{context}: selection must force a present"))
        .fills;
    rt.clear_selection();
    let cleared = rt
        .tick()
        .unwrap_or_else(|| panic!("{context}: clearing must force a present"))
        .fills;
    assert!(
        selected > cleared,
        "{context}: selection must add fills (selected {selected}, cleared {cleared})"
    );
}

/// Two-pane runtime whose second pane owns a real PTY grid with scrollback.
fn session_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless runtime builds");
    install(&mut rt, two_pane());
    let frame = frame_of(&rt, ViewId::new(2));
    rt.spawn_shell_for_view(
        ViewId::new(2),
        "/bin/sh",
        &["-c", "sleep 30"],
        frame.cols,
        frame.rows,
    )
    .expect("spawn pane shell");
    for i in 0..60 {
        rt.handle_pane_bytes(ViewId::new(2), format!("prow{i:02} txt\r\n").as_bytes());
    }
    // Settle the presented geometry before pointer mapping (the first frame
    // reflows the leaf to its content extent).
    rt.tick().expect("the warm-up frame must present");
    rt
}

/// Selects the first five cells of the pane's top visible row and returns the
/// copied text.
fn drag_top_row(rt: &mut Runtime, view: ViewId) -> String {
    let frame = frame_of(rt, view);
    let start = at(rt, &frame, 0, 0);
    let end = at(rt, &frame, 5, 0);
    drag(rt, start, end);
    assert_eq!(rt.selection_owner(), Some(view));
    rt.selection_text().expect("selection text")
}

#[test]
fn detached_float_supports_pointer_selection_and_paint() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));
    rt.float_detach(ViewId::new(1)).expect("detach view 1");
    assert!(
        rt.present_frames()
            .iter()
            .any(|frame| frame.view == ViewId::new(1)),
        "the detached float still presents"
    );

    let frame = frame_of(&rt, ViewId::new(1));
    assert!(frame.content.width > 0 && frame.content.height > 0);
    let start = at(&rt, &frame, 1, 0);
    let end = at(&rt, &frame, 4, 0);
    drag(&mut rt, start, end);
    assert_eq!(
        rt.selection_owner(),
        Some(ViewId::new(1)),
        "the float owns the selection"
    );
    assert_eq!(
        rt.selection().map(|sel| sel.normalized()),
        Some(bitty_ui::SelectionRange::new(
            CellPos::new(0, 1),
            CellPos::new(0, 3)
        )),
        "one cell of pointer travel selects one cell in the float"
    );
    assert_eq!(rt.selection_text().as_deref(), Some("bcd"));
    assert_selection_adds_fills(&mut rt, "detached float selection");

    // The I-beam predicate follows the same float grid.
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(at(
        &rt, &frame, 2, 0,
    ))));
    assert!(rt.pointer_over_selectable_text());
}

#[test]
fn scrolled_detached_float_paints_history_selection() {
    bitty_test_support::require_pty!();
    let mut rt = session_runtime();
    rt.float_detach(ViewId::new(2)).expect("detach pane 2");
    assert!(rt.set_focus(ViewId::new(2)), "focus the detached float");
    // Settle the float's presented geometry before pointer mapping.
    rt.tick().expect("the detached float must present");

    // Baseline: the live top row.
    let live_top = drag_top_row(&mut rt, ViewId::new(2));
    rt.clear_selection();

    // Scroll the float's own viewport into history, then select its top row.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let history_top = drag_top_row(&mut rt, ViewId::new(2));
    assert_ne!(
        live_top, history_top,
        "the float viewport must have scrolled before the selection"
    );
    assert!(
        history_top.starts_with("prow"),
        "the scrolled float selects history text, got {history_top:?}"
    );
    assert_selection_adds_fills(&mut rt, "scrolled detached float selection");
}

#[test]
fn scrolled_pinned_leaf_paints_history_selection() {
    bitty_test_support::require_pty!();
    let mut rt = session_runtime();
    rt.pin_floating(ViewId::new(2)).expect("pin pane 2");
    assert_eq!(rt.pinned_views(), vec![ViewId::new(2)]);
    assert!(rt.set_focus(ViewId::new(2)), "focus the pinned leaf");
    // Settle the pinned leaf's presented geometry before pointer mapping.
    rt.tick().expect("the pinned leaf must present");

    let live_top = drag_top_row(&mut rt, ViewId::new(2));
    rt.clear_selection();

    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let history_top = drag_top_row(&mut rt, ViewId::new(2));
    assert_ne!(
        live_top, history_top,
        "the pinned viewport must have scrolled before the selection"
    );
    assert!(
        history_top.starts_with("prow"),
        "the scrolled pin selects history text, got {history_top:?}"
    );
    assert_selection_adds_fills(&mut rt, "scrolled pinned leaf selection");
}
