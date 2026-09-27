#![forbid(unsafe_code)]
//! Pointer consumers address the right pane in split layouts (CTX-0804,
//! issue #1477).
//!
//! Mouse reports, click-to-focus under capture, and OSC 8 hyperlink
//! activation used the primary-global `cursor_to_cell`, which subtracts no
//! leaf origin and clamps to the primary grid. In a split this made an app in
//! a non-primary pane (vim, less, htop, yazi) receive offset coordinates,
//! kept focus trapped in a mouse-tracking pane, and armed hyperlinks from the
//! primary grid when another pane was clicked.
//!
//! The contract pinned here, following the input-pointer and view-lifecycle
//! RFCs:
//!
//! - mouse press, release, motion, and wheel reports carry the receiving
//!   pane's own grid cells, clamped (never dropped) at its edge;
//! - a left press on another pane moves focus first when a mouse-tracking
//!   app is involved, so the click reaches the pane it landed on;
//! - hyperlink activation resolves the link in the clicked pane's grid.
//!
//! Every pointer position derives from public geometry. Unix-only: the
//! receiving pane is a real `cat -v` PTY, so encoded reports become visible
//! text on that pane's own grid (mirrors `focused_input_modes.rs`).

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitty_platform::{
    CursorPosition, MouseButton, MouseEvent, PlatformEvent, PressState, ScrollDelta,
    WindowEventKind, WindowId,
};
use bitty_runtime::{
    LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, UrlActivation, UrlOpener,
    View, ViewId,
};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

/// Upper bound for a report to round-trip through the pane's PTY echo.
const ECHO_TIMEOUT: Duration = Duration::from_secs(10);
/// Window in which an unexpected report would have echoed back.
const ABSENCE_WINDOW: Duration = Duration::from_millis(500);
/// Poll interval while waiting on PTY echo.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// SGR normal tracking plus SGR coordinates.
const SGR_PRESS_TRACKING: &[u8] = b"\x1b[?1000h\x1b[?1006h";
/// SGR any-event tracking (hover motion) plus SGR coordinates.
const SGR_ANY_MOTION: &[u8] = b"\x1b[?1003h\x1b[?1006h";

// ---------------------------------------------------------------------------
// Fixtures and geometry (public seams only)
// ---------------------------------------------------------------------------

/// Horizontal split: headless primary grid on the left, a `cat -v` pane on
/// the right whose app enabled `modes`.
fn split_with_app_pane(modes: &[u8]) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.force_headless_clipboard();
    let frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "cat -v"], frame.cols, frame.rows)
        .expect("spawn app pane");
    rt.handle_pane_bytes(PANE, modes);
    rt.handle_pty_bytes(b"primary text");
    rt
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Centre of frame-local cell `(row, col)` of `view`'s content frame.
fn cell_center(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y) + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn pane_text(rt: &Runtime) -> String {
    rt.pane_snapshot(&PANE)
        .map(|snap| snap.cells.iter().map(|cell| cell.glyph).collect())
        .unwrap_or_default()
}

/// Pumps the pane until its echoed text contains `needle` or the timeout
/// expires, and returns the last text seen.
fn wait_for_pane_text(rt: &mut Runtime, needle: &str) -> String {
    let deadline = Instant::now() + ECHO_TIMEOUT;
    let mut text = pane_text(rt);
    while !text.contains(needle) && Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        std::thread::sleep(POLL_INTERVAL);
        text = pane_text(rt);
    }
    text
}

/// Pumps the pane for [`ABSENCE_WINDOW`] and returns the text seen.
fn pump_for_absence(rt: &mut Runtime) -> String {
    let deadline = Instant::now() + ABSENCE_WINDOW;
    while Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        std::thread::sleep(POLL_INTERVAL);
    }
    pane_text(rt)
}

fn press(rt: &mut Runtime, button: MouseButton) {
    rt.handle_mouse_input(MouseEvent::new(button, PressState::Pressed));
}

fn release(rt: &mut Runtime, button: MouseButton) {
    rt.handle_mouse_input(MouseEvent::new(button, PressState::Released));
}

/// Echoed SGR report for button code `button` at zero-based `(row, col)`.
fn sgr(button: u8, row: u16, col: u16, pressed: bool) -> String {
    let fin = if pressed { 'M' } else { 'm' };
    format!("^[[<{button};{};{}{fin}", col + 1, row + 1)
}

// ---------------------------------------------------------------------------
// Report coordinates are pane-local
// ---------------------------------------------------------------------------

#[test]
fn press_and_release_report_the_pane_own_cells() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_PRESS_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 3));
    press(&mut rt, MouseButton::Left);
    let expected_press = sgr(0, 2, 3, true);
    let text = wait_for_pane_text(&mut rt, &expected_press);
    assert!(
        text.contains(&expected_press),
        "press must report pane-local cell (2, 3); pane text: {text:?}"
    );

    release(&mut rt, MouseButton::Left);
    let expected_release = sgr(0, 2, 3, false);
    let text = wait_for_pane_text(&mut rt, &expected_release);
    assert!(
        text.contains(&expected_release),
        "release must report pane-local cell (2, 3); pane text: {text:?}"
    );
    assert!(
        rt.selection().is_none(),
        "a captured click never starts a selection"
    );
}

#[test]
fn motion_reports_clamp_at_the_receiving_pane_edge() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_ANY_MOTION);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 5));
    // Motion carries the "no button" code 3 plus the motion bit 32.
    let inside = sgr(35, 1, 5, true);
    let text = wait_for_pane_text(&mut rt, &inside);
    assert!(
        text.contains(&inside),
        "hover inside the pane reports its own cell; pane text: {text:?}"
    );

    // Over the primary pane on the same row: clamped to the receiver's first
    // column, not dropped and never a primary-grid column.
    rt.handle_cursor_moved(cell_center(&rt, PRIMARY, 1, 2));
    let clamped = sgr(35, 1, 0, true);
    let text = wait_for_pane_text(&mut rt, &clamped);
    assert!(
        text.contains(&clamped),
        "motion outside the pane clamps at its edge; pane text: {text:?}"
    );
}

#[test]
fn wheel_reports_use_the_focused_pane_cells() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_PRESS_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 3));
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    // Wheel up is button 64 in SGR.
    let expected = sgr(64, 2, 3, true);
    let text = wait_for_pane_text(&mut rt, &expected);
    assert!(
        text.contains(&expected),
        "wheel must report pane-local cell (2, 3); pane text: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// Click-to-focus under capture
// ---------------------------------------------------------------------------

#[test]
fn clicking_another_pane_leaves_a_capturing_app() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_PRESS_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PRIMARY, 0, 1));
    press(&mut rt, MouseButton::Left);

    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "a click on another pane moves focus out of the capturing app"
    );
    assert_eq!(
        rt.selection_owner(),
        Some(PRIMARY),
        "the non-capturing pane takes the selection path"
    );
    release(&mut rt, MouseButton::Left);
    let text = pump_for_absence(&mut rt);
    assert!(
        !text.contains("^[[<"),
        "the app must not receive a click that landed on another pane: {text:?}"
    );
}

#[test]
fn clicking_a_capturing_pane_reaches_its_app() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_PRESS_TRACKING);
    assert!(rt.set_focus(PRIMARY));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 3));
    press(&mut rt, MouseButton::Left);

    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "the click focuses the pane it landed on"
    );
    assert!(
        rt.selection().is_none(),
        "the capturing app consumes the click instead of a selection"
    );
    let expected = sgr(0, 2, 3, true);
    let text = wait_for_pane_text(&mut rt, &expected);
    assert!(
        text.contains(&expected),
        "the app receives the click at its own cell; pane text: {text:?}"
    );
    release(&mut rt, MouseButton::Left);
}

#[test]
fn shift_click_on_another_pane_keeps_focus_and_selects_there() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_PRESS_TRACKING);
    rt.handle_pane_bytes(PANE, b"pane words");
    assert!(rt.set_focus(PRIMARY));
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift: true,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });

    rt.handle_cursor_moved(cell_center(&rt, PANE, 0, 0));
    press(&mut rt, MouseButton::Left);

    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "Shift is the selection escape: focus stays"
    );
    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "Shift+press selects in the hit pane, bypassing its capture"
    );
    release(&mut rt, MouseButton::Left);
}

#[test]
fn clicking_inside_a_focused_capturing_float_reaches_its_app() {
    bitty_test_support::require_pty!();
    // A mouse-tracking app in a float that holds focus, over a plain base
    // leaf. The capture pre-focus must resolve the *visible* float under the
    // pointer; resolving the base leaf painted beneath it stole focus from
    // the app and dropped its click.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::overlay(
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 30, 8)),
        UiRect::new(20, 6, 30, 8),
    ));
    rt.force_headless_clipboard();
    let frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "cat -v"], frame.cols, frame.rows)
        .expect("spawn float app");
    rt.handle_pane_bytes(PANE, SGR_PRESS_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 2));
    press(&mut rt, MouseButton::Left);
    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "a click inside the focused float keeps focus on the float"
    );
    assert!(
        rt.selection().is_none(),
        "the float's app consumes the click instead of a selection"
    );
    let expected = sgr(0, 1, 2, true);
    let text = wait_for_pane_text(&mut rt, &expected);
    assert!(
        text.contains(&expected),
        "the float's app receives the click at its own cell; pane text: {text:?}"
    );
    release(&mut rt, MouseButton::Left);
}

// ---------------------------------------------------------------------------
// Hyperlink activation resolves the clicked pane
// ---------------------------------------------------------------------------

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

fn click_release_at(rt: &mut Runtime, pos: CursorPosition) {
    let window_id = WindowId::from_raw_public(1);
    rt.handle_platform_event(PlatformEvent::Window {
        window_id,
        kind: WindowEventKind::CursorMoved(pos),
    });
    for state in [PressState::Pressed, PressState::Released] {
        rt.handle_platform_event(PlatformEvent::Window {
            window_id,
            kind: WindowEventKind::MouseInput(MouseEvent::new(MouseButton::Left, state)),
        });
    }
}

/// OSC 8 hyperlink `uri` wrapped around `text`.
fn osc8(uri: &str, text: &str) -> Vec<u8> {
    format!("\x1b]8;;{uri}\x07{text}\x1b]8;;\x07").into_bytes()
}

#[test]
fn hyperlink_activation_reads_the_clicked_pane_grid() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(b"");
    let opened = Arc::new(Mutex::new(Vec::new()));
    rt.set_url_opener(Box::new(RecordingOpener {
        opened: Arc::clone(&opened),
    }));
    // The whole first primary row is a link, so the old primary-global
    // mapping armed it for a click anywhere on that row of *any* pane.
    let primary_cols = usize::from(frame_of(&rt, PRIMARY).cols);
    rt.handle_pty_bytes(b"\r\x1b[2K");
    rt.handle_pty_bytes(&osc8("https://primary.test", &"p".repeat(primary_cols)));
    rt.handle_pane_bytes(PANE, b"plain");

    let plain = cell_center(&rt, PANE, 0, 1);
    click_release_at(&mut rt, plain);
    assert!(
        !rt.has_pending_hyperlink_activation(),
        "a click on plain pane text must not arm the primary grid's link"
    );

    rt.handle_pane_bytes(PANE, b"\r\n");
    rt.handle_pane_bytes(PANE, &osc8("https://pane.test", "link"));
    let link = cell_center(&rt, PANE, 1, 1);
    click_release_at(&mut rt, link);
    assert!(
        rt.has_pending_hyperlink_activation(),
        "a click on the pane's own link arms it"
    );
    let uri = rt
        .activate_pending_hyperlink(&[], false)
        .expect("the armed pane link opens");
    assert_eq!(uri, "https://pane.test");
    assert_eq!(
        *opened.lock().expect("poison-free"),
        vec![String::from("https://pane.test")]
    );
}
