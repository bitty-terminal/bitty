//! Headless regression for #1338: wheel scrolls scrollback, the alternate
//! screen stays fail-closed, and the overlay scrollbar geometry resolves.
//!
//! - Wheel-up (`Lines y > 0`, `Pixels py > 0`) moves the focused viewport
//!   into history; wheel-down returns toward live.
//! - Shift+wheel forces the viewport path even while mouse tracking is
//!   active (the app gets no SGR wheel report).
//! - On the alternate screen a plain wheel is inert (no viewport mutation,
//!   no input bytes); `?1007` still translates to cursor keys and mouse
//!   tracking still takes precedence.
//! - The scrollbar track resolves whenever scrollback exists, `always`
//!   paints without hover, and no thumb resolves on the alternate screen.

#![forbid(unsafe_code)]

use bitty_platform::{PlatformEvent, ScrollDelta, WindowEventKind, WindowId};
use bitty_runtime::{Runtime, RuntimeConfig, ScrollbarMode, ViewId};

fn headless() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("defaults must build");
    rt.force_headless_clipboard();
    rt
}

fn window() -> WindowId {
    WindowId::from_raw_public(1)
}

fn feed_scrollback(rt: &mut Runtime) {
    for i in 0..60 {
        rt.handle_pty_bytes(format!("line {i:02}\n").as_bytes());
    }
    assert!(
        rt.scrollback_len() > 5,
        "need scrollback for wheel test, got {}",
        rt.scrollback_len()
    );
}

fn offset(rt: &Runtime) -> usize {
    let id = rt.focused_view().unwrap_or(ViewId::new(1));
    rt.layout()
        .find_leaf(id)
        .map(|v| v.scroll_offset())
        .unwrap_or(usize::MAX)
}

fn set_shift(rt: &mut Runtime, shift: bool) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: window(),
        kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
}

#[test]
fn wheel_lines_and_pixels_scroll_viewport() {
    let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;
    let pixels_per_notch = RuntimeConfig::default().scroll_pixels_per_notch as f64;
    let mut rt = headless();
    feed_scrollback(&mut rt);
    assert_eq!(offset(&rt), 0, "must start at live");

    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        offset(&rt),
        lines_per_notch,
        "wheel-up (Lines y>0) must go into history by one notch"
    );
    assert_eq!(
        rt.pending_input(),
        b"",
        "viewport scroll must emit no input bytes"
    );

    rt.handle_wheel(ScrollDelta::Lines(0.0, -1.0));
    assert_eq!(offset(&rt), 0, "wheel-down must return toward live");

    rt.handle_wheel(ScrollDelta::Pixels(0.0, pixels_per_notch * 2.0));
    assert_eq!(
        offset(&rt),
        lines_per_notch * 2,
        "wheel-up (Pixels py>0) must go into history by two notches"
    );
    rt.handle_wheel(ScrollDelta::Pixels(0.0, -(pixels_per_notch * 2.0)));
    assert_eq!(offset(&rt), 0, "wheel-down (Pixels py<0) must return live");
}

#[test]
fn wheel_shift_forces_viewport_under_mouse_capture() {
    let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;
    let mut rt = headless();
    feed_scrollback(&mut rt);
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    rt.drain_pending_input();

    // Captured: the wheel reaches the app as an SGR report, no viewport.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(offset(&rt), 0, "captured wheel must not scroll");
    assert!(
        String::from_utf8_lossy(rt.pending_input()).contains("\x1b[<64;"),
        "captured wheel-up must report button 64, got {:?}",
        String::from_utf8_lossy(rt.pending_input())
    );
    rt.drain_pending_input();

    // Shift overrides capture: viewport scrolls, the app gets nothing.
    set_shift(&mut rt, true);
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        offset(&rt),
        lines_per_notch,
        "shift+wheel must scroll the viewport under capture"
    );
    assert_eq!(
        rt.pending_input(),
        b"",
        "shift+wheel must emit no mouse report"
    );
    set_shift(&mut rt, false);
}

#[test]
fn wheel_on_alt_screen_is_inert_without_modes() {
    let mut rt = headless();
    feed_scrollback(&mut rt);
    rt.handle_pty_bytes(b"\x1b[?1049h");
    rt.drain_pending_input();
    assert!(rt.state().alt_screen_active());

    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        offset(&rt),
        0,
        "alt-screen wheel must not move the viewport (no scrollback view)"
    );
    assert_eq!(
        rt.pending_input(),
        b"",
        "alt-screen wheel without modes emits no bytes"
    );

    // Paging is inert there too.
    assert!(rt.scroll_focused_page(true));
    assert_eq!(offset(&rt), 0, "page-up on alt must stay live");
}

#[test]
fn wheel_on_alt_screen_with_alternate_scroll_emits_cursor_keys() {
    let mut rt = headless();
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[?1007h");
    rt.drain_pending_input();
    let lines_per_notch = RuntimeConfig::default().scroll_lines_per_notch as usize;

    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let expected: Vec<u8> = b"\x1b[A".as_slice().repeat(lines_per_notch);
    assert_eq!(
        rt.pending_input(),
        expected.as_slice(),
        "wheel-up on alt+1007 becomes Up cursor keys"
    );
    assert_eq!(offset(&rt), 0, "alternate scroll never moves the viewport");
}

#[test]
fn scrollbar_track_resolves_and_hides_on_alt_screen() {
    let mut rt = headless();
    feed_scrollback(&mut rt);
    assert!(
        rt.scrollbar_track().is_some(),
        "track must resolve once scrollback exists"
    );

    let mut always = Runtime::new(RuntimeConfig {
        scrollbar_mode: ScrollbarMode::Always,
        ..RuntimeConfig::default()
    })
    .expect("always runtime must build");
    always.force_headless_clipboard();
    feed_scrollback(&mut always);
    always.tick().expect("first frame presents");
    assert!(
        always.scrollbar_is_visible(),
        "always mode paints without hover"
    );

    // No thumb on the alternate screen even with retained history.
    always.handle_pty_bytes(b"\x1b[?1049h");
    assert!(
        always.scrollbar_track().is_none(),
        "alt screen owns no scrollback view, so no thumb"
    );
}
