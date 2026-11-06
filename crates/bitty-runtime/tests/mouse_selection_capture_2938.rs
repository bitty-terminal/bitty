//! Mouse selection versus app mouse capture (bitty#2938).
//!
//! The owner report: dragging over the opencode TUI selects nothing inside
//! Bitty while the same TUI selects in Ghostty. These probes drive
//! [`Runtime::handle_mouse_input_at`] / [`Runtime::handle_cursor_moved`] at
//! the runtime level (no GUI) with a fake pane whose modes arrive through the
//! normal PTY byte path, and pin the four ordered hypotheses from the issue:
//!
//! 1. stale capture (missed reset / mode kept after the alt screen or the
//!    app session ends);
//! 2. focused-view mismatch (pointer over an unfocused pane);
//! 3. Shift latch (the selection escape must bypass capture);
//! 4. alt-screen selection (the primary screen must select with no capture).
//!
//! It also pins the wire shape opencode's own selection depends on: drag
//! motion carries the held button identity (`ESC[<32;..M`), not the "no
//! button" code (`ESC[<35;..M`). Opencode/opentui classifies button-3 motion
//! as `move`, so a no-button drag never reaches its selection path.

#![forbid(unsafe_code)]
// Unix-only: the app pane is a real `cat -v` PTY spawned through `/bin/sh`,
// and `require_pty!` skips only where no PTY backend exists at all (Windows
// has ConPTY, so it would run these probes against a shell it does not
// have). Mirrors `mouse_report_panes.rs`; the mode/selection probes that need
// no PTY keep their coverage on Unix.
#![cfg(unix)]

use std::time::{Duration, Instant};

use bitty_platform::{
    CursorPosition, ModifiersState, MouseButton, MouseEvent, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{LayoutNode, PresentFrame, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const PRIMARY: ViewId = ViewId::new(1);
const PANE: ViewId = ViewId::new(2);

/// SGR normal tracking (press/release) plus SGR coordinates.
const SGR_PRESS_TRACKING: &[u8] = b"\x1b[?1000h\x1b[?1006h";
/// SGR button-event tracking (drag) plus SGR coordinates.
const SGR_BUTTON_TRACKING: &[u8] = b"\x1b[?1002h\x1b[?1006h";
/// SGR any-event tracking (hover + drag) plus SGR coordinates.
const SGR_ANY_TRACKING: &[u8] = b"\x1b[?1003h\x1b[?1006h";

/// Upper bound for a pane report to round-trip through the pane's PTY echo.
const ECHO_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting on PTY echo.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Headless runtime whose primary grid parses `modes` through the PTY path.
fn primary_with_modes(modes: &[u8]) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.force_headless_clipboard();
    rt.handle_pty_bytes(modes);
    rt.handle_pty_bytes(b"one two three\nfour five six\n");
    rt
}

/// Horizontal split: headless primary on the left, a real `cat -v` pane on
/// the right whose app enables `modes`.
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

/// Physical position of a headless primary cell (default 9x19 metrics,
/// 8px padding + 14px decoration inset).
fn primary_cell(col: u16, row: u16) -> CursorPosition {
    const PAD: f64 = 8.0;
    const DECORATION: f64 = 14.0;
    CursorPosition {
        x: PAD + DECORATION + f64::from(col) * 9.0,
        y: PAD + DECORATION + f64::from(row) * 19.0,
    }
}

fn press(rt: &mut Runtime, button: MouseButton) {
    rt.handle_mouse_input(MouseEvent::new(button, PressState::Pressed));
}

fn release(rt: &mut Runtime, button: MouseButton) {
    rt.handle_mouse_input(MouseEvent::new(button, PressState::Released));
}

/// A left press, one drag waypoint, and the release: the gesture under test.
fn left_drag(rt: &mut Runtime, start: CursorPosition, waypoint: CursorPosition) {
    rt.handle_cursor_moved(start);
    press(rt, MouseButton::Left);
    rt.handle_cursor_moved(waypoint);
    release(rt, MouseButton::Left);
}

fn shift_held(rt: &mut Runtime) {
    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(ModifiersState {
            shift: true,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
}

fn pane_text(rt: &Runtime) -> String {
    rt.pane_snapshot(&PANE)
        .map(|snap| snap.cells.iter().map(|cell| cell.glyph).collect())
        .unwrap_or_default()
}

/// Pumps the pane until its echoed text contains `needle` or times out.
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

/// Pumps the pane for [`ECHO_TIMEOUT`]'s absence window and returns the text.
fn pump_for_absence(rt: &mut Runtime) -> String {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        std::thread::sleep(POLL_INTERVAL);
    }
    pane_text(rt)
}

/// Echoed SGR report for `button` at zero-based `(row, col)`.
fn sgr(button: u8, row: u16, col: u16, pressed: bool) -> String {
    let fin = if pressed { 'M' } else { 'm' };
    format!("^[[<{button};{};{}{fin}", col + 1, row + 1)
}

// ---------------------------------------------------------------------------
// Probe 1: no tracking -> selection, no PTY bytes
// ---------------------------------------------------------------------------

#[test]
fn tracking_off_drag_selects_without_pty_bytes() {
    let mut rt = primary_with_modes(b"");
    assert!(!rt.mouse_capture_active(), "no mode sequence was sent");

    left_drag(&mut rt, primary_cell(0, 0), primary_cell(5, 0));

    assert!(
        rt.selection().is_some(),
        "a drag over a non-tracking pane must start a selection"
    );
    assert_eq!(rt.selection_owner(), Some(PRIMARY));
    assert_eq!(
        rt.selection_text().as_deref(),
        Some("one t"),
        "the drag selects the cells it covered"
    );
    assert!(
        rt.pending_input().is_empty(),
        "selection writes no PTY bytes: {:?}",
        rt.pending_input()
    );
}

// ---------------------------------------------------------------------------
// Probe 2: SGR tracking -> encoded bytes, no selection
// ---------------------------------------------------------------------------

#[test]
fn sgr_tracking_drag_encodes_and_skips_selection() {
    let mut rt = primary_with_modes(SGR_PRESS_TRACKING);
    assert!(rt.mouse_capture_active(), "1000h+1006h must capture");

    left_drag(&mut rt, primary_cell(2, 1), primary_cell(6, 1));

    let bytes = rt.drain_pending_input();
    let wire = String::from_utf8_lossy(&bytes);
    assert!(
        wire.contains("\x1b[<0;3;2M"),
        "press must encode SGR at the pointer cell: {wire:?}"
    );
    assert!(
        wire.contains("\x1b[<0;7;2m"),
        "release must encode SGR at the release cell: {wire:?}"
    );
    assert!(
        rt.selection().is_none(),
        "a tracking pane consumes the drag instead of selecting"
    );
}

// ---------------------------------------------------------------------------
// Probe 3: Shift bypasses capture
// ---------------------------------------------------------------------------

#[test]
fn shift_drag_bypasses_tracking_capture() {
    let mut rt = primary_with_modes(SGR_PRESS_TRACKING);
    assert!(rt.mouse_capture_active());
    shift_held(&mut rt);
    assert!(rt.shift_pressed(), "the platform modifier state must latch");

    left_drag(&mut rt, primary_cell(0, 0), primary_cell(5, 0));

    assert!(
        rt.selection().is_some(),
        "Shift must force the selection path over a tracking pane"
    );
    assert_eq!(rt.selection_text().as_deref(), Some("one t"));
    assert!(
        rt.pending_input().is_empty(),
        "Shift+selection writes no PTY bytes: {:?}",
        rt.pending_input()
    );
}

// ---------------------------------------------------------------------------
// Probe 4: split, pointer over the unfocused pane
// ---------------------------------------------------------------------------

#[test]
fn split_drag_over_unfocused_pane_selects_there() {
    bitty_test_support::require_pty!();
    // The focused primary tracks the mouse; the pointer drags over the
    // unfocused plain pane. The capture decision must read the pane under
    // the pointer, not the focused one.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.force_headless_clipboard();
    let frame = frame_of(&rt, PANE);
    rt.spawn_shell_for_view(PANE, "/bin/sh", &["-c", "cat -v"], frame.cols, frame.rows)
        .expect("spawn plain pane");
    rt.handle_pty_bytes(SGR_PRESS_TRACKING);
    rt.handle_pty_bytes(b"primary text");
    assert!(rt.set_focus(PRIMARY));
    assert!(rt.mouse_capture_active(), "the focused pane tracks");

    let start = cell_center(&rt, PANE, 0, 0);
    let waypoint = cell_center(&rt, PANE, 0, 4);
    rt.handle_cursor_moved(start);
    press(&mut rt, MouseButton::Left);
    rt.handle_cursor_moved(waypoint);
    release(&mut rt, MouseButton::Left);

    assert_eq!(
        rt.selection_owner(),
        Some(PANE),
        "the drag selects in the pane under the pointer, not the focused tracker"
    );
    let text = pump_for_absence(&mut rt);
    assert!(
        !text.contains("^[[<"),
        "the focused tracker must receive nothing: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// Probe 5: resets and session end restore selection
// ---------------------------------------------------------------------------

#[test]
fn reset_1000l_restores_selection() {
    let mut rt = primary_with_modes(SGR_PRESS_TRACKING);
    assert!(rt.mouse_capture_active());
    rt.handle_pty_bytes(b"\x1b[?1000l\x1b[?1006l");
    assert!(!rt.mouse_capture_active(), "1000l must drop capture");

    left_drag(&mut rt, primary_cell(0, 0), primary_cell(4, 0));

    assert!(rt.selection().is_some(), "selection works after the reset");
    assert!(
        rt.pending_input().is_empty(),
        "no bytes after the reset: {:?}",
        rt.pending_input()
    );
}

#[test]
fn alt_screen_without_tracking_selects() {
    // Hypothesis 4: the selection path must work in the alternate screen
    // when the app does not capture the pointer.
    let mut rt = primary_with_modes(b"");
    rt.handle_pty_bytes(b"\x1b[?1049h");
    rt.handle_pty_bytes(b"\x1b[Halt text");
    assert!(!rt.mouse_capture_active());

    left_drag(&mut rt, primary_cell(0, 0), primary_cell(5, 0));

    assert_eq!(
        rt.selection_text().as_deref(),
        Some("alt t"),
        "the alternate screen selects like the primary screen"
    );
    assert!(rt.pending_input().is_empty());
}

#[test]
fn click_modifier_snapshot_bypasses_capture_without_latch() {
    // Hypothesis 3: the click's own modifier snapshot is authoritative even
    // when the key-event latch missed the physical Shift press.
    let mut rt = primary_with_modes(SGR_PRESS_TRACKING);
    assert!(rt.mouse_capture_active());
    let shift = ModifiersState {
        shift: true,
        control: false,
        alt: false,
        super_pressed: false,
    };

    rt.handle_cursor_moved(primary_cell(0, 0));
    rt.handle_mouse_input(MouseEvent::with_modifiers(
        MouseButton::Left,
        PressState::Pressed,
        shift,
    ));
    rt.handle_cursor_moved(primary_cell(5, 0));
    rt.handle_mouse_input(MouseEvent::with_modifiers(
        MouseButton::Left,
        PressState::Released,
        shift,
    ));

    assert!(!rt.shift_pressed(), "the key-event latch never saw Shift");
    assert!(
        rt.selection().is_some(),
        "the click-time modifier snapshot must force selection"
    );
    assert!(
        rt.pending_input().is_empty(),
        "no bytes may reach the tracking app: {:?}",
        rt.pending_input()
    );
}

#[test]
fn alt_screen_exit_clears_mouse_capture() {
    let mut rt = primary_with_modes(b"");
    // An app enables tracking on the primary screen, then enters the alt
    // screen (the snapshot therefore contains tracking=on), then exits.
    rt.handle_pty_bytes(SGR_PRESS_TRACKING);
    rt.handle_pty_bytes(b"\x1b[?1049h");
    assert!(rt.mouse_capture_active());
    rt.handle_pty_bytes(b"\x1b[?1049l");
    assert!(
        !rt.mouse_capture_active(),
        "leaving the alternate screen must not restore a tracking mode"
    );

    left_drag(&mut rt, primary_cell(0, 0), primary_cell(4, 0));
    assert!(
        rt.selection().is_some(),
        "the primary screen selects after the alt-screen exit"
    );
}

#[test]
fn pane_session_exit_clears_mouse_capture() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(PRIMARY, 80, 24)),
        LayoutNode::leaf(View::new(PANE, 80, 24)),
    ));
    rt.force_headless_clipboard();
    let frame = frame_of(&rt, PANE);
    // The pane's app enables tracking, then its process exits. The pane
    // keeps its grid (scrollback), so the mode register must not keep
    // capturing after the session ends.
    rt.spawn_shell_for_view(
        PANE,
        "/bin/sh",
        &["-c", "printf '\\033[?1000h\\033[?1006h'; sleep 0.2"],
        frame.cols,
        frame.rows,
    )
    .expect("spawn exiting app pane");
    assert!(rt.set_focus(PANE));

    let deadline = Instant::now() + ECHO_TIMEOUT;
    let mut status = None;
    let mut saw_capture = false;
    while status.is_none() && Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        saw_capture |= rt.mouse_capture_active();
        status = rt.pane_try_wait(&PANE);
        std::thread::sleep(POLL_INTERVAL);
    }
    assert!(status.is_some(), "the pane child must exit");
    assert!(
        saw_capture,
        "the app must have enabled tracking before it exited"
    );

    rt.handle_cursor_moved(cell_center(&rt, PANE, 0, 0));
    press(&mut rt, MouseButton::Left);
    assert!(
        rt.selection_owner() == Some(PANE),
        "a dead pane's mode register must not capture the pointer"
    );
}

// ---------------------------------------------------------------------------
// Wire shape: drag motion carries the held button
// ---------------------------------------------------------------------------

#[test]
fn button_tracking_reports_drag_motion_with_held_button() {
    bitty_test_support::require_pty!();
    // Mode 1002 is the drag mode most TUIs use (`vim`, `less`, opencode).
    let mut rt = split_with_app_pane(SGR_BUTTON_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 1));
    press(&mut rt, MouseButton::Left);
    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 4));

    // Left button (0) + motion bit (32) = 32; button 3 + motion = 35 is
    // "no button", which apps classify as hover, not drag.
    let drag = sgr(32, 1, 4, true);
    let text = wait_for_pane_text(&mut rt, &drag);
    assert!(
        text.contains(&drag),
        "1002 drag motion must carry the held left button: {text:?}"
    );
    assert!(
        !text.contains(&sgr(35, 1, 4, true)),
        "drag motion must not be the no-button code: {text:?}"
    );
    release(&mut rt, MouseButton::Left);
}

#[test]
fn opencode_startup_sequence_reports_button_identity() {
    bitty_test_support::require_pty!();
    // The exact DECSET order the opencode TUI emits at startup (captured
    // from `opencode` under a PTY): alt screen, then 1000/1002/1003/1006,
    // leaving any-event tracking active.
    const OPENCODE_STARTUP: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
    let mut rt = split_with_app_pane(OPENCODE_STARTUP);
    assert!(rt.set_focus(PANE));
    assert!(
        rt.mouse_capture_active(),
        "opencode must capture the pointer"
    );

    // Hover first: no button held, so the app sees the no-button motion code.
    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 2));
    let hover = sgr(35, 2, 2, true);
    let text = wait_for_pane_text(&mut rt, &hover);
    assert!(text.contains(&hover), "hover reports button 3: {text:?}");

    // Press and drag: the app must see the left button on the motion report,
    // which is what opentui classifies as a drag and turns into a selection.
    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 1));
    press(&mut rt, MouseButton::Left);
    rt.handle_cursor_moved(cell_center(&rt, PANE, 2, 6));
    let drag = sgr(32, 2, 6, true);
    let text = wait_for_pane_text(&mut rt, &drag);
    assert!(
        text.contains(&drag),
        "opencode drag motion must carry the held left button: {text:?}"
    );
    release(&mut rt, MouseButton::Left);
}

#[test]
fn any_tracking_keeps_hover_and_drag_distinct() {
    bitty_test_support::require_pty!();
    let mut rt = split_with_app_pane(SGR_ANY_TRACKING);
    assert!(rt.set_focus(PANE));

    // Hover with no button: code 3 + motion = 35.
    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 2));
    let hover = sgr(35, 1, 2, true);
    let text = wait_for_pane_text(&mut rt, &hover);
    assert!(text.contains(&hover), "hover reports button 3: {text:?}");

    // Drag with the left button held: code 0 + motion = 32.
    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 1));
    press(&mut rt, MouseButton::Left);
    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 5));
    let drag = sgr(32, 1, 5, true);
    let text = wait_for_pane_text(&mut rt, &drag);
    assert!(
        text.contains(&drag),
        "any-event drag must report the held button: {text:?}"
    );
    release(&mut rt, MouseButton::Left);
}

// ---------------------------------------------------------------------------
// Merged contract (issue #2939): a live drag keeps its own receiver
// ---------------------------------------------------------------------------

#[test]
fn live_drag_keeps_its_receiver_and_held_button_issue_2939() {
    bitty_test_support::require_pty!();
    // `focus_follows_mouse` is on by default (issue #2939 made hover focus
    // ignore capture), so without the drag guard the pointer leaving the
    // app's pane mid-drag would move the keyboard to the primary pane and
    // the held-button drag reports would follow it. A captured drag owns the
    // gesture until its release: focus stays, and the report keeps the held
    // button clamped at the receiving pane's edge.
    let mut rt = split_with_app_pane(SGR_BUTTON_TRACKING);
    assert!(rt.set_focus(PANE));

    rt.handle_cursor_moved(cell_center(&rt, PANE, 1, 1));
    press(&mut rt, MouseButton::Left);
    rt.handle_cursor_moved(cell_center(&rt, PRIMARY, 1, 1));

    assert_eq!(
        rt.focused_view(),
        Some(PANE),
        "a held captured drag must not let hover re-home focus"
    );
    let clamped = sgr(32, 1, 0, true);
    let text = wait_for_pane_text(&mut rt, &clamped);
    assert!(
        text.contains(&clamped),
        "the drag must keep the held button at the pane's edge: {text:?}"
    );

    release(&mut rt, MouseButton::Left);
    // Released: motion is hover again and follows the pointer (issue #2939).
    rt.handle_cursor_moved(cell_center(&rt, PRIMARY, 1, 1));
    assert_eq!(
        rt.focused_view(),
        Some(PRIMARY),
        "after the release hover must follow the pointer again"
    );
}
