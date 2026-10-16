//! CTX-1271 (issue #2140): float drag is Alt-only under Hyprland.
//!
//! Headed evidence (recording/ctx-1271, run 20261016T102510Z): a Super+drag
//! on a detached float moves the whole Hyprland window (the compositor's
//! `SUPER+mouse:272 movewindow` binding) and re-tiles it on release, so the
//! float visibly bounces back and can never be placed at an edge. Bitty
//! cannot suppress the compositor gesture, so the in-app float move and
//! edge-resize grabs must not claim Super: Alt+drag moves floats to edges
//! and sticks (run 20261016T102357Z), Super+drag moves the window.
//!
//! Headless pins (virtual clock, no PTY): Super+press on a structural float
//! interior grabs nothing and arms no resize; Alt+press still grabs.

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{LayoutNode, Runtime, UiRect, View, ViewId};

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
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn move_cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + 14.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + 14.0 + f64::from(row) * 19.0 + 9.0,
    }
}

fn overlay_tree() -> LayoutNode {
    LayoutNode::overlay(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 10, 5)),
        UiRect::new(50, 10, 10, 5),
    )
}

fn super_press(rt: &mut Runtime, pos: CursorPosition) {
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
}

fn super_release(rt: &mut Runtime) {
    rt.handle_mouse_input(bitty_platform::MouseEvent::new(
        MouseButton::Left,
        PressState::Released,
    ));
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

fn alt_press(rt: &mut Runtime, pos: CursorPosition) {
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
}

#[test]
fn float_super_press_grabs_no_drag_or_resize_issue_2140() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    // Cell 55,12 sits inside the structural float bounds 50..60 x 10..15.
    super_press(&mut rt, move_cell_pixels(55, 12));
    assert!(
        !rt.alt_drag_active(),
        "Super+press must not grab the float move (compositor owns Super+drag)"
    );
    assert!(
        !rt.float_resize_active(),
        "Super+press must not arm the float edge resize either"
    );
    super_release(&mut rt);
    assert_eq!(
        rt.layout()
            .overlay_bounds_containing(ViewId::new(2))
            .expect("float must still exist"),
        UiRect::new(50, 10, 10, 5),
        "a refused Super grab leaves the float bounds untouched"
    );
}

#[test]
fn float_alt_press_still_grabs_issue_2140() {
    let mut rt = make_runtime();
    rt.set_layout(overlay_tree());
    alt_press(&mut rt, move_cell_pixels(55, 12));
    assert!(
        rt.alt_drag_active(),
        "Alt+press on the float interior must still grab (issue #2140 keeps Alt)"
    );
}
