//! CTX-0243 regression: typing/IME/paste snap a scrolled viewport to live.
//!
//! Owner live report (P0): when a command's output fills the whole screen,
//! everything typed afterward is invisible and the screen looks locked/frozen.
//!
//! Root cause: `View::scroll_offset` had zero `scroll_to_live` call sites —
//! once scrolled into history (wheel/page/scrollbar/search to read
//! fullscreen output), the viewport kept showing history while new input
//! echoed into the live grid. The typed text was invisible in the viewport,
//! the cursor gate (`scroll_offset == 0`) hid the cursor, and the history
//! viewport was static (no new output in the shown window), so the screen
//! looked frozen.
//!
//! Fix: snap the focused view to live on user input — keyboard presses
//! (`handle_key_event` incl. Esc-cancel with no bytes), raw input bytes
//! (`push_input_bytes`, covering paste delivery/IME commit/mouse SGR),
//! IME preedit/commit, and paste request/confirm/cancel. Output
//! (`handle_pty_bytes`) deliberately does NOT snap (reading history while
//! output continues must not yank); modifier-only keys do not snap.
//!
//! All tests headless and deterministic (no wall clock, no PTY spawn).

use bitty_platform::{KeyEvent, PressState};
use bitty_runtime::Runtime;

fn char_key(c: &str) -> KeyEvent {
    KeyEvent {
        logical_key: bitty_platform::LogicalKey::Character(c.to_string()),
        text: Some(c.to_string()),
        location: bitty_platform::KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn modifier_key(named: bitty_platform::NamedKey) -> KeyEvent {
    KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn fill_with_scrollback(rt: &mut Runtime) {
    for i in 0..60 {
        let line = format!("line {i:02} {}\r\n", "Z".repeat(60));
        rt.handle_pty_bytes(line.as_bytes());
    }
    assert!(rt.tick().is_some(), "fill must present");
    assert_eq!(rt.tick(), None, "must idle after fill");
    assert!(
        rt.state().scrollback_len() > 5,
        "need history to scroll, got {}",
        rt.state().scrollback_len()
    );
}

fn scroll_up(rt: &mut Runtime, notches: f32) {
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, notches));
    let vid = rt.focused_view().expect("focused");
    let off = rt
        .layout()
        .find_leaf(vid)
        .map(|v| v.scroll_offset())
        .unwrap_or(0);
    assert!(off > 0, "must be scrolled for snap tests");
    assert!(rt.tick().is_some(), "scroll must present");
}

fn focused_offset(rt: &Runtime) -> usize {
    let vid = rt.focused_view().expect("focused");
    rt.layout()
        .find_leaf(vid)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX)
}

#[test]
fn typing_snaps_scrolled_viewport_to_live_and_echo_is_visible() {
    // Core P0: fullscreen output + scrollback pressure, user scrolls to read,
    // then types — the echo must land in the visible live window.
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);

    // Type (keyboard) then simulate the shell echo.
    let _ = rt.handle_key_event(char_key("x"));
    rt.handle_pty_bytes(b"x");
    let stats = rt.tick();

    assert_eq!(
        focused_offset(&rt),
        0,
        "typing must snap viewport to live (stuck offset hides input)"
    );
    assert!(
        stats.is_some(),
        "typed echo must present after snap (got None: frozen)"
    );
    // Viewport (what the user sees) must show the echo at the live bottom.
    let vid = rt.focused_view().expect("focused");
    let view = rt.layout().find_leaf(vid).expect("leaf").clone();
    let rows = view.visible_text_rows(rt.state());
    let joined = rows.join("\n");
    assert!(
        joined.contains('x'),
        "viewport must show typed echo (invisible pre-fix)"
    );
    // Cursor gate re-arms once live.
    assert!(
        rt.snapshot().cursor.visible,
        "cursor must be visible once live"
    );
}

#[test]
fn raw_input_bytes_snap_to_live() {
    // `write_input`/`push_input_bytes` (paste delivery, headless buffer,
    // mouse SGR) must snap even without a `KeyEvent`.
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);
    rt.write_input(b"z");
    assert_eq!(focused_offset(&rt), 0, "raw input bytes must snap to live");
    // Echo still presents visibly.
    rt.handle_pty_bytes(b"z");
    assert!(rt.tick().is_some());
    let vid = rt.focused_view().expect("focused");
    let view = rt.layout().find_leaf(vid).expect("leaf").clone();
    assert!(
        view.visible_text_rows(rt.state()).join("\n").contains('z'),
        "echo after raw input must be visible"
    );
}

#[test]
fn output_does_not_yank_scrolled_viewport() {
    // Reading history while output continues must NOT yank (standard
    // terminal behavior; only input snaps).
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);
    let before = focused_offset(&rt);
    rt.handle_pty_bytes(b"new-output-while-scrolled\r\n");
    let _ = rt.tick();
    assert_eq!(
        focused_offset(&rt),
        before,
        "new PTY output must not yank a scrolled viewport"
    );
}

#[test]
fn modifier_only_does_not_snap() {
    // Holding Shift/Ctrl/Alt alone is not typing — viewport stays.
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);
    let before = focused_offset(&rt);
    let _ = rt.handle_key_event(modifier_key(bitty_platform::NamedKey::Shift));
    assert_eq!(
        focused_offset(&rt),
        before,
        "modifier-only press must not snap"
    );
}

#[test]
fn paste_request_snaps_even_when_pending_confirmation() {
    // Clean paste delivers immediately (via `write_input`); suspicious paste
    // pends confirmation with no bytes yet — both must snap on request so
    // the banner and the eventual echo are on the live window.
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);
    // Clean text: delivered immediately.
    assert!(!rt.paste_text_via_gate("hi".to_string()));
    assert_eq!(focused_offset(&rt), 0, "clean paste must snap");
    // Scroll again, then suspicious text (newline needs confirmation).
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 2.0));
    assert!(focused_offset(&rt) > 0);
    let _ = rt.tick();
    assert!(rt.paste_text_via_gate("a\nb".to_string()));
    assert!(
        rt.has_pending_paste(),
        "newline paste must pend confirmation"
    );
    assert_eq!(
        focused_offset(&rt),
        0,
        "suspicious paste request must snap even while pending"
    );
    // Confirming delivers visibly.
    assert!(rt.confirm_pending_paste(true));
    rt.handle_pty_bytes(b"a\nb");
    assert!(rt.tick().is_some());
}

#[test]
fn ime_preedit_and_commit_snap_to_live() {
    let mut rt = Runtime::with_defaults().expect("build");
    fill_with_scrollback(&mut rt);
    scroll_up(&mut rt, 2.0);
    rt.handle_ime_preedit(Some("ni".to_string()), Some(2));
    assert_eq!(focused_offset(&rt), 0, "IME preedit must snap");
    // Scroll again, then commit.
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 2.0));
    assert!(focused_offset(&rt) > 0);
    let _ = rt.tick();
    rt.handle_ime_commit("n".to_string());
    assert_eq!(focused_offset(&rt), 0, "IME commit must snap");
}
