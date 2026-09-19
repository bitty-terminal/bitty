#![forbid(unsafe_code)]
//! M1-08 runtime input-encoding evidence (issue #1134, CTX-0571).
//!
//! The mode/input golden leg in `crates/bitty-compat-lab/tests/m1_mode_golden.rs`
//! pins the terminal-state side of each M1 mode. This file pins the runtime
//! *input* side through the headless `handle_pty_bytes -> handle_*` path:
//!
//! - bracketed paste (`?2004`) wraps a delivered paste with `ESC[200~` /
//!   `ESC[201~` (Input and pointer RFC: defense-in-depth), and the plain mode
//!   delivers the raw text;
//! - focus reporting (`?1004`) emits `ESC[I` / `ESC[O` on window focus
//!   change, and is silent when disabled;
//! - alternate scroll (`?1007`) on the alternate screen translates wheel
//!   notches to cursor keys, and mouse reporting takes precedence.
//!
//! Oracle: accepted `compatibility-milestone-rfc.md` M1 rows (bracketed paste
//! 2004, focus events 1004, alternate scroll 1007) and the mouse/wheel
//! behavior locked by `mouse_encoding.rs` (CTX-0566).

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PlatformEvent, PressState, ScrollDelta,
    WindowEventKind, WindowId,
};
use bitty_runtime::{Runtime, RuntimeConfig};

fn headless() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless build")
}

/// Moves the synthetic cursor to (0, 0) and drains accumulated bytes so the
/// next assertion sees only the event under test.
fn at_origin(rt: &mut Runtime) {
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    rt.drain_pending_input();
}

fn win(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    }
}

/// Confirming a suspicious (newline-bearing) paste wraps it with the
/// bracketed-paste delimiters when `?2004` is on, and delivers raw bytes when
/// it is off.
#[test]
fn bracketed_paste_mode_wraps_and_plain_mode_does_not() {
    // Mode on: the wrap is emitted.
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?2004h");
    assert!(
        rt.paste_text_via_gate(String::from("line1\nline2")),
        "multi-line paste arms the inspection gate"
    );
    rt.drain_pending_input();
    assert!(rt.confirm_pending_paste(true), "gate confirms");
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[200~line1\nline2\x1b[201~",
        "bracketed mode wraps the confirmed paste"
    );

    // Mode off (default): no delimiters.
    let mut rt = headless();
    assert!(
        rt.paste_text_via_gate(String::from("line1\nline2")),
        "multi-line paste arms the inspection gate"
    );
    rt.drain_pending_input();
    assert!(rt.confirm_pending_paste(true), "gate confirms");
    assert_eq!(
        rt.drain_pending_input(),
        b"line1\nline2",
        "plain mode delivers the raw text"
    );
}

/// Focus reporting emits `ESC[I` on focus-in and `ESC[O` on focus-out while
/// `?1004` is set, and stays silent otherwise.
#[test]
fn focus_reporting_emits_in_and_out_only_when_enabled() {
    // A window starts focused, so the first transition under test is out.
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1004h");
    rt.handle_platform_event(win(WindowEventKind::Focused(false)));
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[O",
        "focus-out reports ESC[O while 1004 is set"
    );
    rt.handle_platform_event(win(WindowEventKind::Focused(true)));
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[I",
        "focus-in reports ESC[I while 1004 is set"
    );

    // Reset: no focus bytes on either transition.
    rt.handle_pty_bytes(b"\x1b[?1004l");
    rt.handle_platform_event(win(WindowEventKind::Focused(false)));
    assert_eq!(rt.drain_pending_input(), b"", "reset 1004 is silent");
    rt.handle_platform_event(win(WindowEventKind::Focused(true)));
    assert_eq!(rt.drain_pending_input(), b"", "reset 1004 is silent");
}

/// Alternate scroll `?1007`: on the alternate screen a wheel notch translates
/// to cursor keys (SS3 under DECCKM), the reset stops the translation, and the
/// mode is inert on the primary screen (where the viewport scrolls instead).
#[test]
fn alternate_scroll_translates_wheel_on_alt_screen() {
    // 1007 set on the alternate screen: one notch scrolls the default three
    // lines -> three Up cursor keys.
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1007h");
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[A\x1b[A\x1b[A",
        "a wheel-up notch becomes three Up cursor keys"
    );
    // DECCKM (`?1`) selects the SS3 form.
    rt.handle_pty_bytes(b"\x1b[?1h");
    rt.handle_wheel(ScrollDelta::Lines(0.0, -1.0));
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1bOB\x1bOB\x1bOB",
        "DECCKM encodes the wheel translation as SS3 Down"
    );

    // DECRST 1007 restores the previous behavior: the alt screen has no
    // scrollback, so the wheel is inert.
    rt.handle_pty_bytes(b"\x1b[?1007l");
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        rt.drain_pending_input(),
        b"",
        "reset 1007 stops translation"
    );

    // 1007 set but the primary screen active: the wheel scrolls the viewport
    // and emits no bytes.
    let mut rt = headless();
    for i in 0..60 {
        rt.handle_pty_bytes(format!("line {i:02}\n").as_bytes());
    }
    rt.handle_pty_bytes(b"\x1b[?1007h");
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        rt.drain_pending_input(),
        b"",
        "1007 is inert on the primary screen"
    );
}

/// Mouse reporting takes precedence over alternate scroll: a full-screen TUI
/// that enabled a mouse-tracking mode expects wheel button reports, not cursor
/// keys, even with `?1007` set on the alternate screen (xterm/ghostty parity).
#[test]
fn mouse_reporting_takes_precedence_over_alternate_scroll() {
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

    // A synthetic mouse press still reports under the mouse mode.
    rt.drain_pending_input();
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[<0;1;1M",
        "SGR mouse press reports normally on the alternate screen"
    );
}
