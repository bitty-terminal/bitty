//! Issue #1694 (CTX-0966): Mod+Left-drag moves tiled panels (Hyprland-like).
//!
//! Headless pins: Mod(Alt/Super)+press on a tiled leaf grabs it (no
//! selection starts, focus follows the dragged pane), motion tracks the
//! advisory preview target live without mutating the tree, release
//! re-parents with position-based sizing (nearest-edge docking, ratio from
//! the drop position), and the release never desyncs into a selection
//! commit. Shift still forces selection; floating leaves stay on the
//! Alt+drag path; single-leaf trees fail soft to block selection.
use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{LayoutNode, Runtime, SplitAxis, View, ViewId};

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

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

/// Cursor pixels landing on container cell (col, row) under the default
/// headless geometry (8px padding, 9x19 cells, zero gaps). Matches the
/// `cursor_to_leaf_cell` / `cursor_to_layout_point` origin (no decoration
/// offset) so drop positions map to exact cells for position-based sizing.
fn cell_pixels(col: u16, row: u16) -> CursorPosition {
    CursorPosition {
        x: 8.0 + f64::from(col) * 9.0 + 4.0,
        y: 8.0 + f64::from(row) * 19.0 + 9.0,
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

#[test]
fn mod_press_grabs_tiled_without_selection_and_focuses_dragged() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Press on the left pane with Mod held.
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.tiled_drag_active(),
        "Mod+press on tiled must grab for a tiled move"
    );
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    assert!(
        !rt.is_selection_dragging(),
        "grabbing press must not start selection"
    );
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Pressing the other pane grabs that pane instead (focus follows).
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(2)));
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn super_press_grabs_tiled_for_hyprland_parity() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.tiled_drag_active(),
        "Super+press must also grab (Hyprland Mod muscle memory)"
    );
    assert_eq!(rt.tiled_drag_source(), Some(ViewId::new(1)));
    assert!(!rt.is_selection_dragging());
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

#[test]
fn motion_tracks_preview_live_without_mutating() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    assert_eq!(rt.tiled_drag_preview(), None, "no hover recorded yet");
    // Hover the right pane: preview tracks it, tree untouched.
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    assert_eq!(*rt.layout(), before, "preview must never mutate the tree");
    // Hover back over the source: preview follows.
    rt.handle_cursor_moved(cell_pixels(10, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(1)));
    assert_eq!(*rt.layout(), before);
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn release_commits_position_based_reparent_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Drop near the right edge of the right pane (col 70 in an 80-col
    // container; right pane spans 40..80): docks after with ratio 0.75,
    // so the dragged pane lands small (25%).
    rt.handle_cursor_moved(cell_pixels(70, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active(), "release must end the drag");
    assert!(
        !rt.is_selection_dragging() && !rt.has_selection(),
        "release must not desync into a selection commit (no selection was started)"
    );
    // Source re-parented beside the target; focus stayed on the dragged pane.
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let ids = rt.layout().leaf_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&ViewId::new(1)) && ids.contains(&ViewId::new(2)));
    let ratio = rt.layout().split_ratio_at(&[]).expect("root split");
    assert!(
        (ratio - 0.75).abs() < 1e-6,
        "position-based sizing: drop x 70 -> ratio 0.75, got {ratio}"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn self_drop_is_noop_without_selection() {
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    // Release back over the source leaf: no mutation, no selection.
    rt.handle_cursor_moved(cell_pixels(12, 12));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.tiled_drag_active());
    assert_eq!(
        *rt.layout(),
        before,
        "self-drop must leave the tree untouched"
    );
    assert!(!rt.has_selection(), "self-drop release must not select");
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn shift_mod_press_forces_selection_not_drag() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello world");
    rt.set_layout(two_pane());
    rt.handle_cursor_moved(cell_pixels(0, 0));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "Shift must suppress the tiled grab (CTX-0181 escape)"
    );
    assert!(rt.is_selection_dragging(), "Shift+Mod still selects");
    rt.handle_mouse_input(release(MouseButton::Left));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn single_leaf_mod_press_falls_through_to_block_selection() {
    // A move would always be a self-drop no-op, so the press preserves
    // Alt+block selection instead of grabbing.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello world");
    rt.handle_cursor_moved(cell_pixels(0, 0));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        !rt.tiled_drag_active(),
        "single-leaf Mod+press must not grab"
    );
    rt.handle_cursor_moved(cell_pixels(4, 0));
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(rt.has_selection(), "single-leaf Mod+drag still selects");
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}

#[test]
fn cursor_left_cancels_without_committing() {
    use bitty_platform::{ModifiersState, PlatformEvent, WindowEventKind, WindowId};
    let mut rt = make_runtime();
    rt.set_layout(two_pane());
    let before = rt.layout().clone();
    rt.handle_cursor_moved(cell_pixels(10, 12));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.tiled_drag_active());
    rt.handle_cursor_moved(cell_pixels(60, 12));
    assert_eq!(rt.tiled_drag_preview(), Some(ViewId::new(2)));
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    });
    assert!(!rt.tiled_drag_active(), "leave must cancel the drag");
    assert_eq!(*rt.layout(), before, "cancel must not mutate the tree");
    assert!(!rt.has_selection());
    // A stale release after the cancel must not commit or select either.
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(*rt.layout(), before);
    assert!(!rt.has_selection());
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    let _ = ModifiersState {
        shift: false,
        control: false,
        alt: false,
        super_pressed: false,
    };
}
