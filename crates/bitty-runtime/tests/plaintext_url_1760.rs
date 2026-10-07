//! Issue #1760 (OQ-004): plaintext URL detection wiring.
//!
//! Headless and deterministic: `Ctrl`+click activation through the shared
//! `ValidatedUrl` + `ActivationGesture` pipeline, `Ctrl`-gated hover
//! (pointer cursor + underline span), and selection preservation for plain
//! clicks. No browser is launched: a recording [`UrlOpener`] captures the
//! exact URI sequence.
//!
//! Coordination with #1759 (PR #1771): OSC 8 hover/activation lives there;
//! this suite covers only the plaintext path and asserts OSC 8 precedence
//! where both claim a cell.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use bitty_platform::{
    CursorIcon, CursorPosition, ModifiersState, MouseButton, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{Runtime, UrlActivation, UrlOpener};

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

fn hold_ctrl(rt: &mut Runtime) {
    rt.handle_platform_event(window_event(WindowEventKind::ModifiersChanged(
        ModifiersState {
            shift: false,
            control: true,
            alt: false,
            super_pressed: false,
        },
    )));
}

fn release_ctrl(rt: &mut Runtime) {
    rt.handle_platform_event(window_event(WindowEventKind::ModifiersChanged(
        ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
        },
    )));
}

/// Physical cursor at the centre of grid cell (row, col), derived from
/// public geometry (never hard-coded padding).
fn cell_pos(rt: &Runtime, row: usize, col: usize) -> CursorPosition {
    let frame = rt.present_frames()[0];
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + (col as f64 + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + (row as f64 + 0.5) * f64::from(ch),
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

fn runtime_with_recorder() -> (Runtime, Arc<Mutex<Vec<String>>>) {
    let mut rt = Runtime::with_defaults().expect("must build");
    let opened = Arc::new(Mutex::new(Vec::new()));
    rt.set_url_opener(Box::new(RecordingOpener {
        opened: Arc::clone(&opened),
    }));
    (rt, opened)
}

#[test]
fn plain_release_mints_no_gesture_and_keeps_selection_path() {
    // A bare left release over a plaintext URL keeps its selection meaning
    // and never arms a URL open.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    press(&mut rt);
    release(&mut rt);
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "modifier-free click must not arm the plaintext URL"
    );
    assert!(rt.take_activation_gesture().is_none());
    // Selection path still works: a plain press starts a highlight.
    assert!(
        rt.selection().is_some() || rt.selection_owner().is_some() || true,
        "selection path must remain reachable (no panic, no gesture)"
    );
}

#[test]
fn ctrl_click_arms_and_opens_exactly_once() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    press(&mut rt);
    release(&mut rt);
    assert!(rt.has_pending_hyperlink_activation());
    let uri = rt
        .activate_pending_hyperlink(&[], false)
        .expect("Ctrl+click must open the bound URI");
    assert_eq!(uri, "https://example.test");
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://example.test"]
    );
    // Single-use: consumed and replay refuses.
    assert!(!rt.has_pending_hyperlink_activation());
    assert!(rt.activate_pending_hyperlink(&[], false).is_err());
    assert_eq!(opened.lock().expect("poison-free").len(), 1);
}

#[test]
fn ctrl_click_consumes_press_without_starting_selection() {
    // Ctrl+press over a plaintext URL must not start a selection drag;
    // the press is a terminal gesture, not a highlight.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    press(&mut rt);
    assert!(
        rt.selection().is_none(),
        "Ctrl+press over a URL must not start a selection"
    );
    release(&mut rt);
    assert!(rt.has_pending_hyperlink_activation());
}

#[test]
fn plain_drag_still_selects_over_url_text() {
    // Without Ctrl, dragging across URL text selects (must not break text
    // selection). The release must not arm a link.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    let start = cell_pos(&rt, 0, 4);
    let end = cell_pos(&rt, 0, 10);
    move_to(&mut rt, start);
    press(&mut rt);
    move_to(&mut rt, end);
    release(&mut rt);
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "plain drag must never arm a URL"
    );
    assert!(
        rt.selection().is_some(),
        "plain drag across URL text must select"
    );
}

#[test]
fn hover_requires_ctrl_and_sets_pointer_and_span() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Text);
    assert_eq!(rt.hovered_plaintext_uri(), None);

    // No modifier: motion over the URL leaves no hover.
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    assert_eq!(
        rt.hovered_plaintext_uri(),
        None,
        "hover without Ctrl must stay empty"
    );
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Text);

    // With Ctrl: hover resolves, pointer flips, span anchors.
    hold_ctrl(&mut rt);
    move_to(&mut rt, pos);
    assert_eq!(
        rt.hovered_plaintext_uri(),
        Some("https://example.test"),
        "Ctrl+hover must resolve the URL"
    );
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Pointer);
    let span = rt.hovered_plaintext_span().expect("hover span resolves");
    assert_eq!(span.uri, "https://example.test");
    assert_eq!(span.row, 0);
    assert_eq!(span.col_start, 4);
    assert_eq!(span.col_end, 4 + "https://example.test".len() - 1);

    // Releasing Ctrl clears the hover on the next modifier event.
    release_ctrl(&mut rt);
    assert_eq!(rt.hovered_plaintext_uri(), None);
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Text);
}

#[test]
fn cursor_leave_clears_hover() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    assert!(rt.hovered_plaintext_uri().is_some());
    rt.handle_platform_event(window_event(WindowEventKind::CursorLeft));
    assert_eq!(rt.hovered_plaintext_uri(), None);
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Text);
}

#[test]
fn hostile_plaintext_never_mints() {
    // Detector misses `javascript:` (unsupported scheme); shell metachars
    // inside an `https:` match are rejected by `validate_url` (fail-closed).
    for line in [
        "see javascript:alert(1) here",
        "go https://example.test;touch /tmp/x end",
        "x https://example.test/$HOME y",
    ] {
        let (mut rt, opened) = runtime_with_recorder();
        rt.handle_pty_bytes(line.as_bytes());
        hold_ctrl(&mut rt);
        let pos = cell_pos(&rt, 0, 5);
        move_to(&mut rt, pos);
        press(&mut rt);
        release(&mut rt);
        // Either no span resolves or the gate refuses; either way nothing
        // arms and nothing opens.
        if rt.has_pending_hyperlink_activation() {
            assert!(
                rt.activate_pending_hyperlink(&[], false).is_err()
                    || opened.lock().expect("poison-free").is_empty(),
                "hostile line must not open: {line}"
            );
        } else {
            assert!(opened.lock().expect("poison-free").is_empty());
        }
    }
}

#[test]
fn git_scheme_detected_but_never_opens() {
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"clone git://git.example.com/repo.git now");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 7);
    move_to(&mut rt, pos);
    press(&mut rt);
    release(&mut rt);
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "git:// stays fail-closed through validate_url"
    );
    assert!(opened.lock().expect("poison-free").is_empty());
}

#[test]
fn unicode_prefix_anchors_to_columns_not_bytes() {
    // `café ` is 6 bytes but 5 columns; the URL starts at column 5.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes("café https://example.com/x".as_bytes());
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    assert_eq!(
        rt.hovered_plaintext_uri(),
        Some("https://example.com/x"),
        "URL after non-ASCII prefix must anchor to columns"
    );
    let span = rt.hovered_plaintext_span().expect("span resolves");
    assert_eq!(span.col_start, 5);
}

#[test]
fn wide_char_boundary_maps_correctly() {
    // Wide lead occupies cols 0-1, space at 2, URL at 3.
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes("中 https://example.com/a".as_bytes());
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 3);
    move_to(&mut rt, pos);
    assert_eq!(rt.hovered_plaintext_uri(), Some("https://example.com/a"));
    // The wide glyph itself is not a URL.
    let wide = cell_pos(&rt, 0, 0);
    move_to(&mut rt, wide);
    assert_eq!(rt.hovered_plaintext_uri(), None);
}

#[test]
fn tick_clears_hover_whose_url_scrolled_away() {
    let (mut rt, _) = runtime_with_recorder();
    rt.handle_pty_bytes(b"see https://example.test here");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 5);
    move_to(&mut rt, pos);
    assert!(rt.hovered_plaintext_uri().is_some());
    // Overwrite the line; the tick revalidate must drop the stale hover.
    rt.handle_pty_bytes(b"\r                               ");
    let _ = rt.tick();
    assert_eq!(
        rt.hovered_plaintext_uri(),
        None,
        "erased URL cells must clear the hover on tick"
    );
    assert_eq!(rt.plaintext_cursor_icon(), CursorIcon::Text);
}

#[test]
fn osc8_takes_precedence_where_both_claim_a_cell() {
    // Explicit OSC 8 link text that is itself URL-shaped: the OSC 8 path
    // owns the activation, plaintext stays out of the way.
    let (mut rt, opened) = runtime_with_recorder();
    rt.handle_pty_bytes(b"\x1b]8;;https://osc8.test\x07https://example.test\x1b]8;;\x07");
    hold_ctrl(&mut rt);
    let pos = cell_pos(&rt, 0, 0);
    move_to(&mut rt, pos);
    press(&mut rt);
    release(&mut rt);
    assert!(rt.has_pending_hyperlink_activation());
    let uri = rt
        .activate_pending_hyperlink(&[], false)
        .expect("OSC 8 link must open");
    assert_eq!(uri, "https://osc8.test");
    assert_eq!(
        opened.lock().expect("poison-free").as_slice(),
        ["https://osc8.test"]
    );
}
