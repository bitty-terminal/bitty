//! Issue #1759 (R-005): OSC 8 hyperlink click-to-open live UX.
//!
//! Headless and deterministic: Ctrl/Cmd+LeftClick activation, TUI mouse
//! interception when the modifier is held, hover pointer/preview/span
//! state, sanitized URL preview, and the single-use gesture open path.
//! No browser is launched: a recording [`UrlOpener`] captures the exact
//! URI sequence, and the platform spawn itself stays covered by the
//! existing `spawn_validated_url` boundary (no shell interpolation).

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use bitty_platform::{
    CursorIcon, CursorPosition, ModifiersState, MouseButton, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{HYPERLINK_PREVIEW_MAX_CHARS, Runtime, UrlActivation, UrlOpener};

#[derive(Default)]
struct RecordingOpener {
    opened: Arc<Mutex<Vec<String>>>,
}

impl UrlOpener for RecordingOpener {
    fn open_url(&self, activation: UrlActivation) -> Result<(), bitty_platform::PlatformError> {
        self.opened
            .lock()
            .expect("poison-free")
            .push(activation.uri().to_owned());
        Ok(())
    }
}

fn window_id() -> WindowId {
    WindowId::from_raw_public(1)
}

fn window_event(kind: WindowEventKind) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: window_id(),
        kind,
    }
}

fn hold_modifier(rt: &mut Runtime, control: bool, super_pressed: bool) {
    rt.handle_platform_event(window_event(WindowEventKind::ModifiersChanged(
        ModifiersState {
            shift: false,
            control,
            alt: false,
            super_pressed,
        },
    )));
}

fn hold_ctrl(rt: &mut Runtime) {
    hold_modifier(rt, true, false);
}

/// Physical cursor at the centre of grid cell (row 0, col 0), derived
/// from public geometry: the OSC 8 link text starts at grid col 0.
fn link_cell(rt: &Runtime) -> CursorPosition {
    let frame = rt.present_frames()[0];
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + 0.5 * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + 0.5 * f64::from(ch),
    }
}

fn move_to(rt: &mut Runtime, pos: CursorPosition) {
    rt.handle_platform_event(window_event(WindowEventKind::CursorMoved(pos)));
}

fn press(rt: &mut Runtime) {
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(
        bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed),
    )));
}

fn release(rt: &mut Runtime) {
    rt.handle_platform_event(window_event(WindowEventKind::MouseInput(
        bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released),
    )));
}

fn ctrl_click(rt: &mut Runtime) {
    hold_ctrl(rt);
    let pos = link_cell(rt);
    move_to(rt, pos);
    press(rt);
    release(rt);
}

fn runtime_with_recorder() -> (Runtime, Arc<Mutex<Vec<String>>>) {
    let mut rt = Runtime::with_defaults().expect("must build");
    let opened = Arc::new(Mutex::new(Vec::new()));
    rt.set_url_opener(Box::new(RecordingOpener {
        opened: Arc::clone(&opened),
    }));
    (rt, opened)
}

#[test]
fn plain_release_mints_no_gesture() {
    // Standardized activation: a bare left release over a safe link keeps
    // its selection meaning and never arms a URL open.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    press(&mut rt);
    release(&mut rt);
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "a modifier-free click must not arm the link"
    );
    assert!(rt.take_activation_gesture().is_none());
}

#[test]
fn ctrl_click_arms_and_opens_exactly_once() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    ctrl_click(&mut rt);
    assert!(rt.has_pending_hyperlink_activation());
    let uri = rt
        .activate_pending_hyperlink(&[], false)
        .expect("Ctrl+click must open the bound URI");
    assert_eq!(uri, "https://example.test");
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://example.test"]
    );
    // Single-use: the gesture is consumed and a replay refuses.
    assert!(!rt.has_pending_hyperlink_activation());
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert_eq!(opened.lock().expect("poison-free").len(), 1);
}

#[test]
fn super_click_arms_for_macos_cmd() {
    // macOS Cmd arrives as the Super latch and authorizes like Ctrl.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    hold_modifier(&mut rt, false, true);
    assert!(
        rt.hyperlink_activation_modifier_held(),
        "Super alone must satisfy the gesture modifier"
    );
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    release(&mut rt);
    assert!(
        rt.has_pending_hyperlink_activation(),
        "Cmd+click must arm the link"
    );
}

#[test]
fn no_modifier_means_no_gesture_modifier_held() {
    let rt = Runtime::with_defaults().expect("must build");
    assert!(
        !rt.hyperlink_activation_modifier_held(),
        "a fresh runtime holds no gesture modifier"
    );
}

#[test]
fn tui_capture_bypass_consumes_both_click_halves() {
    // A mouse-tracking TUI (1000 + SGR 1006) never sees either half of a
    // Ctrl+click over a link: no SGR bytes reach the child, and the
    // terminal arms the gesture instead.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    assert!(rt.mouse_capture_active(), "tracking must capture");
    rt.drain_pending_input();
    hold_ctrl(&mut rt);
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    press(&mut rt);
    assert!(
        rt.pending_input().is_empty(),
        "the Ctrl+press must not reach the TUI"
    );
    release(&mut rt);
    assert!(
        rt.pending_input().is_empty(),
        "the Ctrl+release must not reach the TUI"
    );
    assert!(
        rt.has_pending_hyperlink_activation(),
        "the intercepted click arms the link"
    );
}

#[test]
fn plain_release_under_capture_still_reports_to_the_child() {
    // Without the modifier, capture behavior is unchanged: the release is
    // encoded for the app and no gesture mints.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    assert!(rt.mouse_capture_active());
    rt.drain_pending_input();
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    release(&mut rt);
    assert!(
        !rt.pending_input().is_empty(),
        "a plain release under capture still reports to the child"
    );
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "and it never arms a link"
    );
}

#[test]
fn hover_sets_pointer_preview_and_span() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    assert_eq!(rt.hyperlink_cursor_icon(), CursorIcon::Text);
    assert_eq!(rt.hovered_hyperlink_uri(), None);
    assert_eq!(rt.hovered_hyperlink_preview(), None);

    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    assert_eq!(
        rt.hovered_hyperlink_uri(),
        Some("https://example.test"),
        "hover must resolve the link URI"
    );
    assert_eq!(
        rt.hyperlink_cursor_icon(),
        CursorIcon::Pointer,
        "hover must request the pointer shape"
    );
    assert_eq!(
        rt.hovered_hyperlink_preview().as_deref(),
        Some("https://example.test"),
        "short URIs preview in full"
    );
    let span = rt.hovered_hyperlink_span().expect("hover span resolves");
    assert_eq!(span.uri, "https://example.test");
    assert_eq!(span.row, 0);
    assert_eq!(span.col_start, 0);
    assert_eq!(span.col_end, 3, "the 4-cell 'link' span");

    // Leaving the link restores the I-beam and drops the preview.
    move_to(&mut rt, CursorPosition { x: 1.0, y: 1.0 });
    assert_eq!(rt.hovered_hyperlink_uri(), None);
    assert_eq!(rt.hovered_hyperlink_preview(), None);
    assert_eq!(rt.hyperlink_cursor_icon(), CursorIcon::Text);
}

#[test]
fn cursor_leave_clears_hover() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    assert!(rt.hovered_hyperlink_uri().is_some());
    rt.handle_platform_event(window_event(WindowEventKind::CursorLeft));
    assert_eq!(rt.hovered_hyperlink_uri(), None);
    assert_eq!(rt.hyperlink_cursor_icon(), CursorIcon::Text);
}

#[test]
fn hover_rejects_hostile_schemes() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    assert_eq!(
        rt.hovered_hyperlink_uri(),
        None,
        "hostile output never hovers"
    );
    assert_eq!(rt.hyperlink_cursor_icon(), CursorIcon::Text);
    assert_eq!(rt.hovered_hyperlink_preview(), None);
}

#[test]
fn preview_truncates_long_urls_with_ellipsis() {
    let (mut rt, _) = runtime_with_recorder();
    let long = format!("https://example.test/{}", "a".repeat(200));
    let seq = format!("\x1b]8;;{long}\x07link\x1b]8;;\x07");
    rt.handle_pty_bytes(seq.as_bytes());
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    let preview = rt
        .hovered_hyperlink_preview()
        .expect("long safe URL still previews");
    assert_eq!(
        preview.chars().count(),
        HYPERLINK_PREVIEW_MAX_CHARS + 1,
        "preview is bounded plus one ellipsis"
    );
    assert!(
        preview.ends_with('…'),
        "truncation must be visibly marked: {preview}"
    );
    assert!(
        preview.starts_with("https://example.test/"),
        "the true host stays visible at the front: {preview}"
    );
    assert!(
        !preview.chars().any(|ch| ch.is_control()),
        "the preview carries no control payload"
    );
}

#[test]
fn tick_clears_a_hover_whose_link_scrolled_away() {
    // Terminal output can erase the hovered link under a stationary
    // pointer; the per-tick revalidate must drop the stale affordance.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://example.test\x07link\x1b]8;;\x07");
    let pos = link_cell(&rt);
    move_to(&mut rt, pos);
    assert!(rt.hovered_hyperlink_uri().is_some());
    rt.handle_pty_bytes(b"\r    ");
    let _ = rt.tick();
    assert_eq!(
        rt.hovered_hyperlink_uri(),
        None,
        "erased link cells must clear the hover on tick"
    );
    assert_eq!(rt.hyperlink_cursor_icon(), CursorIcon::Text);
}
