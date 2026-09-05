//! Selection clearing regression tests (CTX-0166, issue #268).
//!
//! Scope is CLEARING only: left-click/Esc/typing dismiss the highlight plus
//! selection state, new drags replace old, and clearing forces the next tick
//! to present (frame-on-demand). Range algebra (`bitty-ui::selection`) is
//! owned by CTX-0168 and untouched here.
//!
//! Headless, no display server; clipboard forced headless so `wl-paste`
//! capture is proven via headless buffers deterministically.

#![forbid(unsafe_code)]

use bitty_platform::{
    CursorPosition, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey, PressState,
};
use bitty_runtime::Runtime;
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed_text(rt: &mut Runtime, text: &str) {
    rt.handle_pty_bytes(text.as_bytes());
}

fn char_key(logical: &str, text: Option<&str>, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn mouse_press(button: MouseButton) -> MouseEvent {
    MouseEvent {
        button,
        state: PressState::Pressed,
    }
}

fn mouse_release(button: MouseButton) -> MouseEvent {
    MouseEvent {
        button,
        state: PressState::Released,
    }
}

fn select_hello(rt: &mut Runtime) {
    feed_text(rt, "hello world");
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 4));
    assert!(rt.has_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
}

#[test]
fn left_click_clears_selection_and_forces_present() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    // Present the selection once so the next tick is idle.
    assert!(rt.tick().is_some());
    assert_eq!(rt.tick(), None);
    // Single left-click elsewhere (press+release same cell, no drag).
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    // Press replaces the old range with a collapsed empty selection, so the
    // highlight is already gone even before release.
    assert!(
        !rt.has_selection(),
        "left press must dismiss the old highlight"
    );
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert!(!rt.has_selection());
    assert!(rt.selection().is_none());
    // Clearing state must force a present (frame-on-demand).
    assert!(rt.tick().is_some(), "clearing selection must force present");
    assert_eq!(rt.tick(), None, "must return to idle after clear present");
}

#[test]
fn left_press_without_cursor_still_clears() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    // Fresh runtime never saw CursorMoved for this click path: drive the
    // button directly with `last_cursor == None` (no cursor tracking).
    assert!(rt.last_cursor().is_none());
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    assert!(
        !rt.has_selection(),
        "click without cursor tracking must not leave a stale rect"
    );
    assert!(rt.tick().is_some(), "clear must force present");
}

#[test]
fn esc_clears_selection_and_still_sends_byte() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    rt.drain_pending_input();
    let out = rt.handle_key_event(named_key(NamedKey::Escape, PressState::Pressed));
    assert!(!rt.has_selection(), "Esc must clear the highlight");
    assert!(rt.selection().is_none());
    // Standard terminals still deliver Esc to the PTY while dismissing.
    assert_eq!(out, Some(vec![27]), "Esc must still reach the shell");
    assert_eq!(rt.pending_input(), b"\x1b");
    assert!(rt.tick().is_some(), "Esc-clear must force present");
}

#[test]
fn typing_clears_selection_and_delivers_bytes() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    rt.drain_pending_input();
    let out = rt.handle_key_event(char_key("a", Some("a"), PressState::Pressed));
    assert!(!rt.has_selection(), "typing must clear the highlight");
    assert_eq!(out, Some(b"a".to_vec()));
    assert_eq!(rt.pending_input(), b"a");
    assert!(rt.tick().is_some(), "typing-clear must force present");
}

#[test]
fn new_drag_replaces_old_selection() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    // New drag via the mouse path replaces the old range.
    rt.handle_cursor_moved(CursorPosition {
        x: 9.0 * 6.0,
        y: 0.0,
    });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 9.0 * 10.0,
        y: 0.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert!(rt.has_selection());
    assert_eq!(rt.selection_text().as_deref(), Some("world"));
    // `wl-paste` equivalent: auto-copy on release syncs both clipboards.
    assert_eq!(rt.clipboard().headless_contents(), "world");
    assert_eq!(rt.primary_contents(), "world");
}

#[test]
fn modifier_only_and_key_release_preserve_selection() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    // Shift press (modifier-only) must not dismiss — it arms shift-override.
    assert!(
        rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed))
            .is_none()
    );
    assert!(rt.has_selection(), "modifier-only must preserve selection");
    // Key release must not dismiss either.
    assert!(
        rt.handle_key_event(char_key("a", Some("a"), PressState::Released))
            .is_none()
    );
    assert!(rt.has_selection(), "key release must preserve selection");
}

#[test]
fn capture_mode_click_clears_stale_highlight() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    // App enables mouse tracking (1000 + SGR 1006): clicks now go to the PTY.
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.handle_pty_bytes(b"\x1b[?1006h");
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    assert!(
        !rt.has_selection(),
        "captured click must still dismiss stale highlight"
    );
    assert_eq!(rt.pending_input(), b"\x1b[<0;1;1M");
    assert!(rt.tick().is_some(), "capture-clear must force present");
}

#[test]
fn ime_commit_clears_selection_and_delivers() {
    let mut rt = make_runtime();
    select_hello(&mut rt);
    rt.drain_pending_input();
    rt.handle_ime_commit("k".to_string());
    assert!(!rt.has_selection(), "IME commit (typing) must clear");
    assert_eq!(rt.pending_input(), b"k");
    assert!(rt.tick().is_some(), "IME-clear must force present");
}

#[test]
fn drag_auto_copy_proves_capture_for_wl_paste() {
    // Issue #268 Q3: verify capture independently of rendering. Headless
    // proof that after a drag, both clipboards hold exactly the dragged text
    // (live `wl-paste` / `wl-paste --primary` observe the same via CTX-0160).
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.handle_mouse_input(mouse_press(MouseButton::Left));
    rt.handle_cursor_moved(CursorPosition {
        x: 9.0 * 4.0,
        y: 0.0,
    });
    rt.handle_mouse_input(mouse_release(MouseButton::Left));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    assert_eq!(rt.clipboard().headless_contents(), "hello");
    assert_eq!(rt.primary_contents(), "hello");
}
