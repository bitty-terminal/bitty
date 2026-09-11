//! CTX-0260: focus-follows-mouse hover + Alt+drag floating-pane moves.
//!
//! Headless pins: the flag defaults off (click-to-focus preserved), hover
//! moves keyboard focus only when enabled (Shift suppresses it per the
//! CTX-0181 precedent), and Alt+drag moves a floating overlay without
//! breaking selection (Shift still forces the selection path).
use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent {
        button,
        state: PressState::Pressed,
    }
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent {
        button,
        state: PressState::Released,
    }
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding inset, 9x19 cells, zero cell gaps, plus
/// the unified CTX-0294/CTX-0333 default decoration outer gap + border +
/// content inset = 14px at scale 1.0).
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 14.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 14.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn overlay_tree() -> LayoutNode {
    LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 5)),
        UiRect::new(50, 10, 10, 5),
    )
}

fn overlay_bounds(rt: &Runtime) -> UiRect {
    match rt.layout() {
        LayoutNode::Overlay { bounds, .. } => *bounds,
        other => panic!("expected overlay tree, got {other:?}"),
    }
}

#[test]
fn focus_follows_mouse_defaults_off_and_hover_keeps_focus() {
    // Default-off pin: hover never moves keyboard focus (click-to-focus
    // preserved for existing users).
    assert!(!RuntimeConfig::default().focus_follows_mouse);
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Right pane occupies container cols 40..80: hover its middle.
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "hover must not move focus when disabled"
    );
}

#[test]
fn hover_moves_focus_only_when_enabled() {
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        ..RuntimeConfig::default()
    })
    .expect("opt-in runtime must build");
    assert!(rt.config().focus_follows_mouse);
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    // Hover back returns focus (still gated, still deterministic).
    rt.handle_cursor_moved(cell_pixels(10, 12));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Hover over the outer gap/padding band keeps focus (no leaf there).
    rt.handle_cursor_moved(CursorPosition { x: 2.0, y: 2.0 });
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn shift_suppresses_hover_focus() {
    // CTX-0181 precedent: Shift forces the selection path, so Shift+hover
    // never steals focus even when the flag is on.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        ..RuntimeConfig::default()
    })
    .expect("opt-in runtime must build");
    rt.set_layout(two_pane());
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
}

#[test]
fn alt_drag_moves_floating_overlay_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    assert_eq!(overlay_bounds(&rt), UiRect::new(50, 10, 10, 5));
    // Grab the float (cell 55,12 sits inside bounds 50..60 x 10..15).
    rt.handle_cursor_moved(cell_pixels(55, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.alt_drag_active(), "Alt+press on float must grab");
    assert!(!rt.is_selection_dragging(), "grab must not start selection");
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    // Drag 5 cells right, 2 down: bounds follow exactly.
    rt.handle_cursor_moved(cell_pixels(60, 14));
    assert_eq!(overlay_bounds(&rt), UiRect::new(55, 12, 10, 5));
    // Release ends the drag with no selection commit (no auto-copy side
    // effects from a gesture that never selected).
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.alt_drag_active());
    assert!(!rt.has_selection());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn alt_press_on_tiled_layout_falls_through_to_selection() {
    // Tiled splits have no movable position: the grab fails soft and the
    // press selects normally ("without breaking selection").
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello world");
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(0, 0));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.alt_drag_active(),
        "tiled Alt+press must not grab (no float to move)"
    );
    rt.handle_cursor_moved(cell_pixels(4, 0));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(rt.has_selection(), "tiled Alt+drag still selects");
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn shift_alt_press_forces_selection_not_drag() {
    // Shift wins over chrome (CTX-0181 accessibility escape): even over a
    // float, Shift+Alt+press selects instead of grabbing.
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    rt.handle_cursor_moved(cell_pixels(55, 12));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(!rt.alt_drag_active(), "Shift must suppress the grab");
    assert!(rt.is_selection_dragging());
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn hover_delay_defers_focus_until_deadline() {
    // CTX-0334: a positive dwell delay arms a pending candidate; focus
    // moves only once `tick_at` observes the deadline.
    let delay = std::time::Duration::from_millis(150);
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: delay,
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let t0 = std::time::Instant::now();
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t0);
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "focus must stay until the dwell deadline"
    );
    assert_eq!(rt.hover_activation_deadline(), Some(t0 + delay));
    let _ = rt.tick_at(t0 + delay - std::time::Duration::from_millis(1));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)), "before deadline");
    let _ = rt.tick_at(t0 + delay);
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(2)),
        "deadline commits focus"
    );
    assert!(rt.hover_activation_deadline().is_none());
}

#[test]
fn hover_delay_preserves_clock_within_pane_and_clears_on_gap() {
    // Repeated motion inside the same pane keeps the original entry time;
    // leaving to a gap/padding band drops the pending dwell.
    let delay = std::time::Duration::from_millis(100);
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: delay,
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    let t0 = std::time::Instant::now();
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t0);
    assert_eq!(rt.hover_activation_deadline(), Some(t0 + delay));
    rt.handle_cursor_moved_at(cell_pixels(70, 12), t0 + delay / 2);
    assert_eq!(
        rt.hover_activation_deadline(),
        Some(t0 + delay),
        "same-pane motion must not reset the dwell clock"
    );
    rt.handle_cursor_moved_at(CursorPosition { x: 2.0, y: 2.0 }, t0 + delay / 2);
    assert!(rt.hover_activation_deadline().is_none());
    let _ = rt.tick_at(t0 + delay * 2);
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a cleared dwell never commits"
    );
}

#[test]
fn hover_delay_zero_activates_immediately_and_disabled_with_delay_is_inert() {
    // Delay `0` reproduces CTX-0260 immediate activation.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: std::time::Duration::ZERO,
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    assert!(rt.hover_activation_deadline().is_none());
    // Disabled: a configured delay never arms.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        focus_follows_mouse_delay: std::time::Duration::from_millis(100),
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    let t0 = std::time::Instant::now();
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t0);
    assert!(rt.hover_activation_deadline().is_none());
    let _ = rt.tick_at(t0 + std::time::Duration::from_secs(1));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn hover_delay_shift_suppresses_and_clears_pending() {
    let delay = std::time::Duration::from_millis(100);
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: delay,
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    let t0 = std::time::Instant::now();
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t0);
    assert!(rt.hover_activation_deadline().is_none());
    let _ = rt.tick_at(t0 + std::time::Duration::from_secs(1));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn hover_delay_yields_to_explicit_focus_change() {
    // If another path moves focus while a dwell pends, the pending hover
    // activation must be abandoned rather than override the explicit choice.
    let delay = std::time::Duration::from_millis(200);
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: true,
        focus_follows_mouse_delay: delay,
        ..RuntimeConfig::default()
    })
    .expect("runtime builds");
    rt.set_layout(two_pane());
    let t0 = std::time::Instant::now();
    // Dwell on pane 2 (still focused pane 1).
    rt.handle_cursor_moved_at(cell_pixels(60, 12), t0);
    // Explicit keyboard focus to pane 2, then back to pane 1.
    rt.move_focus(bitty_runtime::FocusDirection::Right);
    rt.move_focus(bitty_runtime::FocusDirection::Left);
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Deadline passes: the hover candidate must not steal focus.
    let _ = rt.tick_at(t0 + delay);
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}
