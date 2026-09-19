//! Mode 1007 alternate scroll and legacy mouse coordinate emission
//! (CTX-0566, issues #970 and #1127).
//!
//! Headless end-to-end tests proving:
//!
//! - DECSET/DECRST `?1007` is parsed, stored, and reported by DECRQM; with
//!   it set, wheel events in the alternate screen translate to cursor-key
//!   sequences instead of being dropped or sent as mouse reports, and
//!   DECRST restores the previous (viewport-scroll) behavior.
//! - The runtime emits all four coordinate encodings (X10 default/SGR
//!   1006/UTF-8 1005/urxvt 1015), not only SGR, and DECRST of an encoding
//!   mode restores the X10 default.
//!
//! Wire shapes follow xterm `ctlseqs.txt` "Mouse Tracking" (X10
//! `CSI M CbCxCy`, SGR `CSI < Cb;Cx;Cy M|m`, UTF-8 `CSI M` + three UTF-8
//! codepoints, urxvt `CSI Cb;Cx;Cy M`) and the read-only reference
//! implementations in `recording/references/{xterm,ghostty,neovim}`.
//!
//! Cross-platform: headless state + synthetic events only (no PTY spawn).

use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState, ScrollDelta};
use bitty_runtime::{Runtime, RuntimeConfig};

fn headless() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.force_headless_clipboard();
    rt
}

fn press(button: MouseButton) -> MouseEvent {
    MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> MouseEvent {
    MouseEvent::new(button, PressState::Released)
}

/// Move the synthetic cursor to (0, 0) and drain any accumulated bytes so
/// the next assertion sees only the event under test.
fn at_origin(rt: &mut Runtime) {
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.drain_pending_input();
}

fn pending(rt: &Runtime) -> Vec<u8> {
    rt.pending_input().to_vec()
}

fn replies(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

// ---------------------------------------------------------------------------
// (b) DECRQM 1007 reply bytes
// ---------------------------------------------------------------------------

#[test]
fn decrqm_reports_1007_alternate_scroll_state() {
    let mut rt = headless();
    // Reset by default (xterm alternateScroll resource default is false).
    rt.handle_pty_bytes(b"\x1b[?1007$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1007;2$y".to_vec()]);
    // Set reports 1.
    rt.handle_pty_bytes(b"\x1b[?1007h");
    rt.handle_pty_bytes(b"\x1b[?1007$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1007;1$y".to_vec()]);
    // Reset reports 2 again.
    rt.handle_pty_bytes(b"\x1b[?1007l");
    rt.handle_pty_bytes(b"\x1b[?1007$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1007;2$y".to_vec()]);
}

// ---------------------------------------------------------------------------
// (a) Mode 1007 alternate scroll
// ---------------------------------------------------------------------------

#[test]
fn alternate_scroll_wheel_in_alt_screen_emits_cursor_keys() {
    let mut rt = headless();
    // Alternate screen + alternate scroll, no mouse tracking.
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1007h");
    rt.drain_pending_input();
    // One notch = default 3 lines -> three Up cursor keys (normal mode).
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(pending(&rt), b"\x1b[A\x1b[A\x1b[A");
    rt.drain_pending_input();
    // Wheel down returns Down cursor keys.
    rt.handle_wheel(ScrollDelta::Lines(0.0, -1.0));
    assert_eq!(pending(&rt), b"\x1b[B\x1b[B\x1b[B");
    // The alt screen has no scrollback: alternate scroll must never move
    // the viewport offset.
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    let offset = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX);
    assert_eq!(offset, 0, "alternate scroll must not scroll the viewport");
}

#[test]
fn alternate_scroll_honors_application_cursor_keys() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1h\x1b[?1007h");
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(pending(&rt), b"\x1bOA\x1bOA\x1bOA");
}

#[test]
fn alternate_scroll_reset_restores_previous_behavior() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1007h");
    // Set: the wheel translates to cursor keys.
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(pending(&rt), b"\x1b[A\x1b[A\x1b[A");
    // DECRST 1007: translation stops; the alt screen has no scrollback, so
    // the wheel is again inert (no cursor keys, no mouse report).
    rt.handle_pty_bytes(b"\x1b[?1007l");
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(pending(&rt), b"");
}

#[test]
fn mouse_tracking_takes_precedence_over_alternate_scroll() {
    // xterm/ghostty give explicit mouse reporting precedence: a TUI that
    // enabled `?1000` expects wheel button reports, not cursor keys, even
    // with `?1007` set on the alternate screen.
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1007h\x1b[?1000h\x1b[?1006h");
    at_origin(&mut rt);
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let pending = String::from_utf8_lossy(rt.pending_input()).into_owned();
    assert!(
        pending.contains("\x1b[<64;"),
        "mouse reporting must win over alternate scroll, got {pending:?}"
    );
    assert!(
        !pending.contains("\x1b[A"),
        "alternate scroll must not emit cursor keys while reporting, got {pending:?}"
    );
}

#[test]
fn alternate_scroll_only_applies_in_alternate_screen() {
    let mut rt = headless();
    // 1007 set but the primary screen is active: the wheel must still
    // scroll the viewport and emit no bytes.
    for i in 0..60 {
        rt.handle_pty_bytes(format!("line {i:02}\n").as_bytes());
    }
    rt.handle_pty_bytes(b"\x1b[?1007h");
    rt.drain_pending_input();
    let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;
    let view_id = rt.focused_view().unwrap_or(bitty_ui::ViewId::new(1));
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(pending(&rt), b"");
    let offset = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX);
    assert_eq!(offset, lines_per_notch);
}

// ---------------------------------------------------------------------------
// (c) Legacy coordinate encodings
// ---------------------------------------------------------------------------

#[test]
fn legacy_x10_encoding_is_default_and_encodes_button_plus_32() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1000h");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Left));
    // CSI M Cb Cx Cy with each value + 32; left press = code 0 -> 0x20.
    assert_eq!(pending(&rt), b"\x1b[M\x20\x21\x21");
    rt.drain_pending_input();
    // Legacy release collapses to button 3 (no button identity).
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(pending(&rt), b"\x1b[M\x23\x21\x21");
}

#[test]
fn legacy_utf8_encoding_matches_x10_for_small_coordinates() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1005h");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Right));
    // Right press = code 2 -> +32 = 0x22; coords 1 -> 0x21 each.
    assert_eq!(pending(&rt), b"\x1b[M\x22\x21\x21");
}

#[test]
fn legacy_urxvt_encoding_uses_decimal_parameters() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1015h");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Middle));
    // Middle press = code 1 -> +32 = 33; decimal coords 1;1.
    assert_eq!(pending(&rt), b"\x1b[33;1;1M");
    rt.drain_pending_input();
    // Release collapses to button 3 -> 35, decimal, no M/m distinction.
    rt.handle_mouse_input(release(MouseButton::Middle));
    assert_eq!(pending(&rt), b"\x1b[35;1;1M");
}

#[test]
fn sgr_encoding_still_keeps_button_identity_and_modifiers() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Right));
    assert_eq!(pending(&rt), b"\x1b[<2;1;1M");
    rt.drain_pending_input();
    rt.handle_mouse_input(release(MouseButton::Right));
    assert_eq!(pending(&rt), b"\x1b[<2;1;1m");
}

// ---------------------------------------------------------------------------
// (d) DECRST restores the X10 default
// ---------------------------------------------------------------------------

#[test]
fn reset_of_a_different_encoding_does_not_clobber_the_active_one() {
    // xterm: the coordinate encodings are mutually exclusive and a reset is
    // only effective against the matching mode.
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    // Reset 1015 (not active) must leave SGR 1006 active.
    rt.handle_pty_bytes(b"\x1b[?1015l");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Right));
    assert_eq!(pending(&rt), b"\x1b[<2;1;1M");
    // Selecting a new encoding replaces the active one.
    rt.handle_pty_bytes(b"\x1b[?1015h");
    at_origin(&mut rt);
    rt.handle_mouse_input(press(MouseButton::Right));
    assert_eq!(pending(&rt), b"\x1b[34;1;1M");
}

#[test]
fn decrst_of_each_encoding_restores_x10_default() {
    for enable in [&b"\x1b[?1005h"[..], b"\x1b[?1006h", b"\x1b[?1015h"] {
        let disable = match enable {
            b"\x1b[?1005h" => &b"\x1b[?1005l"[..],
            b"\x1b[?1006h" => &b"\x1b[?1006l"[..],
            _ => &b"\x1b[?1015l"[..],
        };
        let mut rt = headless();
        rt.handle_pty_bytes(b"\x1b[?1000h");
        rt.handle_pty_bytes(enable);
        rt.handle_pty_bytes(disable);
        at_origin(&mut rt);
        rt.handle_mouse_input(press(MouseButton::Left));
        assert_eq!(
            pending(&rt),
            b"\x1b[M\x20\x21\x21",
            "DECRST of {enable:?} must restore the X10 byte framing"
        );
    }
}
