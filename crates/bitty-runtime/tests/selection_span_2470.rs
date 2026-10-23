#![forbid(unsafe_code)]
//! Issue #2470: one visual selection spans scrollback pages.
//!
//! Copy mode / pointer selection used to be capped at the visible page: the
//! wheel while dragging did not scroll (or was forwarded to a mouse-tracking
//! app), PageUp/PageDown narrowed the range to the new viewport, and a live
//! anchor silently slid onto different lines after pruning.
//!
//! These tests pin the fixed contract end-to-end:
//!
//! - wheel/page scrolling while a drag is held extends the range and pins the
//!   press endpoint by combined-buffer identity;
//! - a pointer held in the top/bottom painted row auto-scrolls one bounded
//!   line per interval (no runaway when no room remains);
//! - a drag owns the wheel over PTY mouse reporting, while an idle wheel still
//!   reports to the app and Shift keeps its viewport escape;
//! - committed text stays byte-exact across scrolls and denies stale rows
//!   instead of copying different lines after pruning.
//!
//! All pointer positions derive from public geometry (`present_frames`,
//! `live_cell_size`, `window_padding_physical`); no hard-coded pixel values.

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState, ScrollDelta};
use bitty_runtime::{PresentFrame, Runtime, RuntimeConfig, ViewId};

/// Feed one numbered line per row so the copied text is self-describing.
fn line_text(index: usize) -> String {
    format!("L{index:03} text")
}

fn feed_lines(rt: &mut Runtime, first: usize, count: usize) {
    for index in first..first + count {
        rt.handle_pty_bytes(format!("{}\r\n", line_text(index)).as_bytes());
    }
}

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn view_id(rt: &Runtime) -> ViewId {
    rt.focused_view()
        .or(rt.primary_view())
        .expect("a focused or primary view")
}

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Physical cursor position at a vertical cell edge, row centre.
fn cell_edge(rt: &Runtime, view: ViewId, row: u16, col_edge: u16) -> CursorPosition {
    let frame = frame_of(rt, view);
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x) + f64::from(col_edge) * f64::from(cw),
        y: pad + f64::from(frame.content.y) + f64::from(row) * f64::from(ch) + f64::from(ch) / 2.0,
    }
}

fn scroll_offset(rt: &Runtime) -> usize {
    let id = view_id(rt);
    rt.layout()
        .find_leaf(id)
        .map(|view| view.scroll_offset())
        .unwrap_or(usize::MAX)
}

fn rows(rt: &Runtime) -> u16 {
    rt.layout()
        .find_leaf(view_id(rt))
        .expect("focused leaf")
        .rows()
}

fn press(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Pressed));
}

fn release(rt: &mut Runtime) {
    rt.handle_mouse_input(MouseEvent::new(MouseButton::Left, PressState::Released));
}

fn wheel_notch() -> usize {
    RuntimeConfig::default().scroll_lines_per_notch as usize
}

/// Caret column just past the content of one fed line ("L123 text" is 9
/// cells), so a drag from column 0 through this edge selects the full line.
fn line_edge() -> u16 {
    u16::try_from(line_text(0).len()).expect("line width fits a cell column")
}

/// Combined-buffer row of a frame row at `offset`.
///
/// This is the documented viewport contract: the frame shows
/// `view.rows()` cells starting `offset` rows above the live bottom. It is
/// independent of the selection code under test.
fn buffer_row(rt: &Runtime, offset: usize, frame_row: u16) -> usize {
    let state = rt.state();
    let total = state.scrollback_len() + state.height();
    total - usize::from(rows(rt)) - offset + usize::from(frame_row)
}

/// Assert the copied text is exactly the contiguous numbered lines
/// `first..=last`, in buffer order (oldest first).
fn assert_contiguous_span(text: &str, first: usize, last: usize) {
    let copied: Vec<&str> = text.lines().collect();
    let expected: Vec<String> = (first..=last).map(line_text).collect();
    assert_eq!(
        copied,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "selection must export every line between {first} and {last} exactly once"
    );
}

#[test]
fn wheel_while_dragging_spans_pages_and_exports_exact_text() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 0, 120);
    rt.tick().expect("warm-up frame presents");
    let sb = rt.state().scrollback_len();
    assert!(sb > 48, "need multi-page scrollback, got {sb}");
    assert_eq!(scroll_offset(&rt), 0, "must start live");

    let id = view_id(&rt);
    let (press_row, pointer_row) = (20, 2);
    let press_buffer = buffer_row(&rt, 0, press_row);

    rt.handle_cursor_moved(cell_edge(&rt, id, press_row, line_edge()));
    press(&mut rt);
    rt.handle_cursor_moved(cell_edge(&rt, id, pointer_row, 0));

    // Wheel far enough that the pointer endpoint lands in history while the
    // button stays held.
    let notch = wheel_notch();
    let notches = ((usize::from(rows(&rt)) + 8) / notch.max(1)).max(1);
    rt.handle_wheel(ScrollDelta::Lines(0.0, notches as f32));
    let offset = scroll_offset(&rt);
    assert_eq!(
        offset,
        notch * notches,
        "drag wheel must scroll into history"
    );
    assert!(rt.is_selection_dragging(), "scroll must not end the drag");

    release(&mut rt);
    assert!(!rt.is_selection_dragging(), "release commits the drag");

    // The press endpoint is pinned by buffer identity; the pointer endpoint
    // stays under the cursor in the newly revealed page.
    let first = buffer_row(&rt, offset, pointer_row);
    let last = press_buffer;
    let text = rt.selection_text().expect("multi-page drag must copy");
    assert!(
        text.lines().count() > usize::from(rows(&rt)),
        "one selection must exceed one visible page, got {} lines",
        text.lines().count()
    );
    assert_contiguous_span(&text, first, last);
    // The clipboard export path is the same exact span (issue #2470).
    let exported = rt
        .copy_selection_to_clipboard()
        .expect("headless clipboard write")
        .expect("multi-page clipboard export");
    assert_eq!(exported, text, "clipboard export must be the full span");

    // Scrolling further after the commit must not truncate or re-anchor it.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 3.0));
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(text.as_str()),
        "committed buffer range survives later wheel scrolling"
    );
}

#[test]
fn page_keys_keep_the_anchor_and_extend_a_held_drag() {
    let mut rt = Runtime::new(RuntimeConfig {
        page_scroll_percent: 50,
        ..RuntimeConfig::default()
    })
    .expect("page-scroll runtime must build");
    rt.force_headless_clipboard();
    feed_lines(&mut rt, 0, 200);
    rt.tick().expect("warm-up frame presents");
    let id = view_id(&rt);
    let page = usize::from(rows(&rt)) * 50 / 100;
    assert!(page > 1, "50% paging must move more than one row");

    // Committed selection near the live bottom: paging past it keeps both the
    // buffer range and the exported text exactly.
    rt.handle_cursor_moved(cell_edge(&rt, id, 18, 0));
    press(&mut rt);
    rt.handle_cursor_moved(cell_edge(&rt, id, 19, line_edge()));
    release(&mut rt);
    let expected_first = buffer_row(&rt, 0, 18);
    let expected_last = buffer_row(&rt, 0, 19);
    let committed = rt.selection_text().expect("committed selection text");
    assert_contiguous_span(&committed, expected_first, expected_last);

    assert!(rt.scroll_focused_page(true), "page up scrolls");
    assert_eq!(scroll_offset(&rt), page, "50% of the view rows");
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(committed.as_str()),
        "page up must not narrow or shift the committed range"
    );
    assert!(rt.scroll_focused_page(false), "page down scrolls back");
    assert_eq!(scroll_offset(&rt), 0);
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(committed.as_str()),
        "page down must not narrow or shift the committed range"
    );

    // A held drag pages the same way: the press stays pinned and the pointer
    // endpoint moves into the newly revealed rows.
    rt.handle_cursor_moved(cell_edge(&rt, id, 20, line_edge()));
    press(&mut rt);
    assert!(rt.scroll_focused_page(true), "page up while dragging");
    assert_eq!(scroll_offset(&rt), page);
    assert!(rt.is_selection_dragging(), "paging must not end the drag");
    rt.handle_cursor_moved(cell_edge(&rt, id, 0, 0));
    release(&mut rt);

    let first = buffer_row(&rt, page, 0);
    let last = buffer_row(&rt, 0, 20);
    let text = rt.selection_text().expect("paged drag must copy");
    assert!(
        text.lines().count() > usize::from(rows(&rt)),
        "paged drag must span more than one page, got {} lines",
        text.lines().count()
    );
    assert_contiguous_span(&text, first, last);
}

#[test]
fn edge_drag_autoscrolls_into_history_bounded() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 0, 120);
    rt.tick().expect("warm-up frame presents");
    let sb = rt.state().scrollback_len();
    let id = view_id(&rt);
    let rows = rows(&rt);
    let press_row = rows - 2;
    let t0 = Instant::now();

    rt.handle_cursor_moved_at(cell_edge(&rt, id, press_row, line_edge()), t0);
    press(&mut rt);
    // Move to the top painted row: this arms one-line auto-scroll up.
    rt.handle_cursor_moved_at(cell_edge(&rt, id, 0, 0), t0);
    assert!(rt.is_selection_dragging());
    assert_eq!(scroll_offset(&rt), 0, "arming must not scroll by itself");
    let deadline = rt
        .selection_autoscroll_deadline()
        .expect("edge row arms auto-scroll");

    // Before the interval elapses: no step.
    rt.tick_at(t0 + Duration::from_millis(1));
    assert_eq!(scroll_offset(&rt), 0, "step is cadence-bounded");

    // Each elapsed interval advances exactly one line.
    let mut now = deadline;
    for expected in 1..=12 {
        rt.tick_at(now);
        assert_eq!(
            scroll_offset(&rt),
            expected,
            "one armed step per interval, at step {expected}"
        );
        now += Duration::from_millis(1);
        // A second tick inside the same interval must not advance again.
        rt.tick_at(now);
        assert_eq!(scroll_offset(&rt), expected, "no catch-up bursts");
        now = now
            .checked_add(Duration::from_millis(39))
            .expect("clock advances");
    }
    assert!(
        rt.is_selection_dragging(),
        "auto-scroll keeps the drag alive"
    );

    release(&mut rt);
    assert!(
        rt.selection_autoscroll_deadline().is_none(),
        "release disarms edge auto-scroll"
    );
    let text = rt.selection_text().expect("auto-scrolled selection text");
    let first = buffer_row(&rt, 12, 0);
    let last = buffer_row(&rt, 0, press_row);
    assert!(
        text.lines().count() > usize::from(rows),
        "auto-scroll must reveal a page beyond the press page"
    );
    assert_contiguous_span(&text, first, last);

    // Reaching the top of the transcript stops the scroll instead of pinning
    // the loop: no runaway and no wrap into live.
    rt.handle_cursor_moved_at(cell_edge(&rt, id, press_row, line_edge()), now);
    press(&mut rt);
    rt.handle_cursor_moved_at(cell_edge(&rt, id, 0, 0), now);
    let mut tick_time = now + Duration::from_millis(50);
    for _ in 0..(sb + 8) {
        rt.tick_at(tick_time);
        tick_time += Duration::from_millis(50);
    }
    assert_eq!(
        scroll_offset(&rt),
        sb,
        "auto-scroll clamps at the oldest row"
    );
    assert!(
        rt.selection_autoscroll_deadline().is_none(),
        "an exhausted range disarms the wake"
    );
    release(&mut rt);
}

#[test]
fn edge_drag_autoscrolls_down_within_bounds() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 0, 120);
    rt.tick().expect("warm-up frame presents");
    let id = view_id(&rt);
    let rows = rows(&rt);
    rt.handle_wheel(ScrollDelta::Lines(0.0, 10.0));
    let max_offset = scroll_offset(&rt);
    assert!(max_offset > 1, "need a scrolled viewport");
    let t0 = Instant::now();

    rt.handle_cursor_moved_at(cell_edge(&rt, id, 4, 0), t0);
    press(&mut rt);
    rt.handle_cursor_moved_at(cell_edge(&rt, id, rows - 1, line_edge()), t0);
    assert!(rt.is_selection_dragging());
    let deadline = rt
        .selection_autoscroll_deadline()
        .expect("bottom edge arms auto-scroll down");

    let mut now = deadline;
    for _ in 0..max_offset + 5 {
        rt.tick_at(now);
        now += Duration::from_millis(50);
    }
    assert_eq!(scroll_offset(&rt), 0, "downward auto-scroll stops at live");
    assert!(
        rt.selection_autoscroll_deadline().is_none(),
        "live bottom disarms the wake"
    );
    // Leave the edge before releasing so the committed end is the last
    // content row rather than the blank row under the cursor.
    rt.handle_cursor_moved(cell_edge(&rt, id, rows - 2, line_edge()));
    release(&mut rt);

    let text = rt.selection_text().expect("down-scrolled selection text");
    let first = buffer_row(&rt, max_offset, 4);
    let last = buffer_row(&rt, 0, rows - 2);
    assert_contiguous_span(&text, first, last);
}

#[test]
fn drag_owns_wheel_over_pty_mouse_capture() {
    let mut rt = make_runtime();
    feed_lines(&mut rt, 0, 60);
    rt.tick().expect("warm-up frame presents");
    let id = view_id(&rt);
    rt.handle_pty_bytes(b"\x1b[?1000h\x1b[?1006h");
    rt.drain_pending_input();

    // Without a drag, capture precedence is unchanged: the wheel goes to the
    // app and the viewport stays live.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(scroll_offset(&rt), 0, "captured wheel must not scroll");
    assert!(
        String::from_utf8_lossy(rt.pending_input()).contains("\x1b[<64;"),
        "captured wheel-up must report button 64, got {:?}",
        String::from_utf8_lossy(rt.pending_input())
    );
    rt.drain_pending_input();

    // Shift is the documented selection escape: the press starts a Core drag
    // even over a mouse-reporting app.
    set_shift(&mut rt, true);
    rt.handle_cursor_moved(cell_edge(&rt, id, 20, line_edge()));
    press(&mut rt);
    assert!(
        rt.is_selection_dragging(),
        "Shift+press selects under capture"
    );

    // While the drag is in flight the selection owns the wheel: the viewport
    // scrolls and the app receives no report (it never saw the press).
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert_eq!(
        scroll_offset(&rt),
        wheel_notch(),
        "drag wheel must scroll even under capture"
    );
    assert_eq!(
        rt.pending_input(),
        b"",
        "drag scroll must not leak a wheel report to the PTY"
    );
    // The pointer endpoint is the older row now: address it from column 0 so
    // the exported span starts at the line beginning.
    rt.handle_cursor_moved(cell_edge(&rt, id, 20, 0));

    // The committed range is fixed at release; later app input cannot move it.
    let expected_first = buffer_row(&rt, wheel_notch(), 20);
    let expected_last = buffer_row(&rt, 0, 20);
    release(&mut rt);

    // Releasing Shift restores app ownership for the next wheel. The report
    // itself snaps the viewport to live (CTX-0243 input policy), so only the
    // routing is asserted here: the app receives the wheel again.
    set_shift(&mut rt, false);
    rt.drain_pending_input();
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert!(
        String::from_utf8_lossy(rt.pending_input()).contains("\x1b[<64;"),
        "capture precedence must be restored after the drag, got {:?}",
        String::from_utf8_lossy(rt.pending_input())
    );

    let text = rt
        .selection_text()
        .expect("selection committed under capture");
    assert_contiguous_span(&text, expected_first, expected_last);
}

fn set_shift(rt: &mut Runtime, shift: bool) {
    rt.handle_platform_event(bitty_platform::PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind: bitty_platform::WindowEventKind::ModifiersChanged(bitty_platform::ModifiersState {
            shift,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
}

#[test]
fn live_anchor_survives_scroll_and_denies_pruned_lines() {
    let mut rt = Runtime::new(RuntimeConfig {
        scrollback: 40,
        ..RuntimeConfig::default()
    })
    .expect("bounded-scrollback runtime must build");
    rt.force_headless_clipboard();
    feed_lines(&mut rt, 0, 40);
    rt.tick().expect("warm-up frame presents");
    let id = view_id(&rt);

    // Select two live rows; these endpoints carry no scrollback line id yet.
    rt.handle_cursor_moved(cell_edge(&rt, id, 10, 0));
    press(&mut rt);
    rt.handle_cursor_moved(cell_edge(&rt, id, 11, line_edge()));
    release(&mut rt);
    let expected_first = buffer_row(&rt, 0, 10);
    let expected_last = buffer_row(&rt, 0, 11);
    let committed = rt.selection_text().expect("live selection text");
    assert_contiguous_span(&committed, expected_first, expected_last);

    // Page loads: the selected rows scroll into retained history under new
    // output. The range keeps its lines rather than following the viewport.
    feed_lines(&mut rt, 40, 20);
    assert!(
        expected_first < rt.state().scrollback_len(),
        "selected rows must now be history"
    );
    assert_eq!(
        rt.selection_text().as_deref(),
        Some(committed.as_str()),
        "a live anchor keeps its history position across page loads"
    );

    // A further page load pushes those lines out of the bounded scrollback.
    feed_lines(&mut rt, 60, 60);
    assert_eq!(
        rt.state().scrollback_len(),
        40,
        "scrollback ring must be full"
    );
    assert!(
        rt.selection_text().is_none(),
        "pruned lines must deny rather than copy unrelated content"
    );
    assert!(
        !rt.has_selection(),
        "a stale range must report no selection"
    );
}
