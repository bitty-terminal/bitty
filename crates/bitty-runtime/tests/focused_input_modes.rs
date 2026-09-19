//! Focused-input primary-mode attribution (CTX-0532, Issue #919).
//!
//! Mode-sensitive input paths used to read the runtime-global caches /
//! primary state instead of the focused pane's own session, so a focus
//! transition with no intervening PTY pump attributed input to the newly
//! focused pane using the previous pane's modes: mouse capture, Kitty
//! keyboard encoding, focus reporting, and bracketed paste.
//!
//! This file pins the acceptance shape end to end with a real split:
//! an "app" pane (mouse tracking + Kitty via PTY mode sequences) and a
//! plain pane, with the focus transition under test having no pump between.
//!
//! Unix-only: spawning needs a POSIX shell plus PTY master semantics
//! (mirrors `pane_sessions.rs`).

#![cfg(unix)]

use std::time::{Duration, Instant};

use bitty_platform::{
    CursorPosition, KeyEvent, KeyLocation, LogicalKey, MouseButton, MouseEvent, NamedKey,
    PressState,
};
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const TIMEOUT: Duration = Duration::from_secs(10);

fn two_pane_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    rt.set_layout(layout);
    rt
}

fn ctrl_a() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(String::from("a")),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn control_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Control),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn pane_text(rt: &Runtime, view: &ViewId) -> String {
    match rt.pane_snapshot(view) {
        Some(snap) => snap.cells.iter().map(|c| c.glyph).collect(),
        None => String::new(),
    }
}

fn primary_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|c| c.glyph).collect()
}

#[test]
fn focus_transition_reattributes_capture_and_kitty_without_pump() {
    bitty_test_support::require_pty!();
    let mut rt = two_pane_runtime();
    // Primary pane is the "app": mouse tracking (1000 + SGR 1006) and
    // Kitty progressive flags (7727).
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h\x1b[?7727h");
    // Plain split pane owns its own shell and no modes.
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn plain pane shell");

    // Focus the app pane: app modes are active.
    assert!(rt.set_focus(ViewId::new(1)));
    assert!(rt.mouse_capture_active(), "app pane must capture the mouse");
    assert_ne!(rt.kitty_flags(), 0, "app pane must enable Kitty flags");

    // Switch to the plain pane with NO PTY pump in between: the plain
    // pane's modes must win, never the previous pane's.
    assert!(rt.set_focus(ViewId::new(2)));
    assert!(
        !rt.mouse_capture_active(),
        "plain pane inherited the app pane's mouse capture"
    );
    assert_eq!(
        rt.kitty_flags(),
        0,
        "plain pane inherited the app pane's Kitty flags"
    );

    // Synthesized key encodes with the focused (plain) pane's modes:
    // legacy Ctrl+A, never the app's Kitty CSI-u sequence.
    rt.track_modifiers_from_key(&control_press());
    let bytes = rt
        .handle_key_event_ref(&ctrl_a())
        .expect("ctrl+a must encode");
    assert_eq!(
        bytes,
        vec![0x01],
        "encoding must use the focused pane's modes"
    );
}

#[test]
fn focus_transition_reattributes_bracketed_paste_without_pump() {
    bitty_test_support::require_pty!();
    let mut rt = two_pane_runtime();
    // Plain primary pane; the split pane is the app that enabled bracketed
    // paste on its own grid.
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "cat -v"], 40, 12)
        .expect("spawn app pane shell");
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b[?2004h");

    // Focus the app pane with no pump after its mode arrived.
    assert!(rt.set_focus(ViewId::new(2)));

    // A confirmed paste must be bracketed with the focused pane's mode.
    assert!(
        rt.paste_text_via_gate(String::from("hello\n")),
        "newline paste arms the inspection gate"
    );
    assert!(rt.confirm_pending_paste(true), "gate must confirm");

    // `cat -v` renders the bracket bytes visibly, so the pane grid is the
    // outcome evidence: no brackets means the plain primary mode won.
    let deadline = Instant::now() + TIMEOUT;
    let mut text = pane_text(&rt, &ViewId::new(2));
    while !text.contains("^[[200~") && Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        text = pane_text(&rt, &ViewId::new(2));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        text.contains("^[[200~"),
        "focused pane's bracketed-paste mode was ignored; pane text: {text:?}"
    );
}

#[test]
fn focus_transition_reattributes_mouse_capture_encoding() {
    bitty_test_support::require_pty!();
    let mut rt = two_pane_runtime();
    // Primary pane is the "app": Normal tracking (1000) + SGR encoding
    // (1006); both shells echo through `cat -v` so encoded bytes are
    // visible on the grid that received them.
    rt.spawn_shell_with_args("/bin/sh", &["-c", "cat -v"])
        .expect("spawn primary app shell");
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "cat -v"], 40, 12)
        .expect("spawn plain pane shell");

    // Cell geometry: default 9x19 cells, 8px padding, zero gaps; the split
    // halves the 80x24 container, so column 45 is inside the right pane and
    // column 5 inside the left one.
    let now = Instant::now();
    let pane2_pos = CursorPosition { x: 413.0, y: 103.0 };
    let pane1_pos = CursorPosition { x: 53.0, y: 103.0 };

    // Focus the plain pane with no pump; a press there must take the
    // selection path (no capture), never emit the app pane's SGR bytes.
    assert!(rt.set_focus(ViewId::new(2)));
    rt.handle_cursor_moved_at(pane2_pos, now);
    rt.handle_mouse_input_at(MouseEvent::new(MouseButton::Left, PressState::Pressed), now);
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(2)),
        "press over the plain pane must keep its focus"
    );
    assert!(
        rt.selection().is_some(),
        "plain pane must take the selection path, not captured SGR encoding"
    );

    // Focus the app pane, press again: capture encodes and routes to the
    // focused pane's own shell (visible via `cat -v`).
    rt.clear_selection();
    assert!(rt.set_focus(ViewId::new(1)));
    rt.handle_cursor_moved_at(pane1_pos, now);
    rt.handle_mouse_input_at(MouseEvent::new(MouseButton::Left, PressState::Pressed), now);
    assert!(
        rt.selection().is_none(),
        "app pane capture must consume the press"
    );
    let deadline = Instant::now() + TIMEOUT;
    let mut text = primary_text(&rt);
    while !text.contains("^[[<0;") && Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        text = primary_text(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        text.contains("^[[<0;"),
        "app pane must receive its own captured SGR bytes; primary text: {text:?}"
    );
}
