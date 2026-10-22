#![forbid(unsafe_code)]
//! Selection inside detached floating panels (issue #2523).
//!
//! Detached floats are not leaves of the tiling tree, so the fail-closed
//! selection guard used to drop every press inside one: mouse selection and
//! its highlight never worked there. These tests prove the float grid is a
//! first-class selection owner while the tiled panes keep their behavior.

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PressState, WindowEventKind, WindowId,
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
    let stats = rt.tick().expect("float selection must force a present");
    assert!(stats.fills > 0, "the float highlight must paint");

    // The I-beam predicate follows the same float grid.
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(at(
        &rt, &frame, 2, 0,
    ))));
    assert!(rt.pointer_over_selectable_text());
}
