//! Multi-click word/line/block selection (CTX-0385, issue #641).
//!
//! Headless integration coverage for the bounded click state machine:
//! - click-count tracking (single/double/triple, timeout/distance/fourth-wrap)
//! - double-click word boundaries, triple-click full line
//! - drag after multi-click extends word/line-wise; `Alt` gives a block
//! - no regression to single-click stream drags
//!
//! All timing uses the virtual-clock seam (`handle_mouse_input_at` /
//! `handle_cursor_moved_at`) so no test sleeps. No display server needed.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_platform::{CursorPosition, ModifiersState, MouseButton, MouseEvent, PressState};
use bitty_runtime::Runtime;
use bitty_ui::{CellPos, SelectionKind};

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn feed_text(rt: &mut Runtime, text: &str) {
    rt.handle_pty_bytes(text.as_bytes());
}

/// Physical position for a grid cell with the readable 9x19 cell metrics.
///
/// Includes the default 8px window padding inset and the default Core-owned
/// decoration inset (14px at scale 1.0), matching the existing selection
/// integration tests.
fn cell_pos(col: u16, row: u16) -> CursorPosition {
    const PAD: f64 = 8.0;
    const DECORATION: f64 = 14.0;
    CursorPosition {
        x: PAD + DECORATION + f64::from(col) * 9.0,
        y: PAD + DECORATION + f64::from(row) * 19.0,
    }
}

fn press_at(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
    rt.handle_mouse_input_at(MouseEvent::new(MouseButton::Left, PressState::Pressed), now);
}

fn release_at(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
    rt.handle_mouse_input_at(
        MouseEvent::new(MouseButton::Left, PressState::Released),
        now,
    );
}

fn move_at(rt: &mut Runtime, col: u16, row: u16, now: Instant) {
    rt.handle_cursor_moved_at(cell_pos(col, row), now);
}

#[test]
fn double_click_selects_word() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    // Two quick presses inside "world" (col 8).
    press_at(&mut rt, 8, 0, base);
    release_at(&mut rt, 8, 0, base + Duration::from_millis(50));
    press_at(&mut rt, 8, 0, base + Duration::from_millis(150));
    release_at(&mut rt, 8, 0, base + Duration::from_millis(200));
    assert_eq!(rt.last_click_count(), 2);
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Word));
    assert_eq!(rt.selection_text().as_deref(), Some("world"));
}

#[test]
fn triple_click_selects_full_line() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    for (i, dt) in [0u64, 120, 240].iter().enumerate() {
        let now = base + Duration::from_millis(*dt);
        press_at(&mut rt, 2, 0, now);
        release_at(&mut rt, 2, 0, now + Duration::from_millis(30));
        if i < 2 {
            assert!(rt.last_click_count() <= 2);
        }
    }
    assert_eq!(rt.last_click_count(), 3);
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Line));
    // Full-row line with edge-trimmed padding collapses to the content.
    assert_eq!(rt.selection_text().as_deref(), Some("hello world"));
}

#[test]
fn fourth_quick_press_wraps_to_single() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    for k in 0..4u64 {
        let now = base + Duration::from_millis(k * 100);
        press_at(&mut rt, 1, 0, now);
        release_at(&mut rt, 1, 0, now + Duration::from_millis(20));
    }
    // 1-2-3-1 cycle: the fourth press is single again (no word/line).
    assert_eq!(rt.last_click_count(), 1);
    assert!(
        rt.selection_kind() != Some(SelectionKind::Word)
            && rt.selection_kind() != Some(SelectionKind::Line),
        "fourth press must not be word/line, got {:?}",
        rt.selection_kind()
    );
}

#[test]
fn single_click_drag_stays_simple_no_regression() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    press_at(&mut rt, 0, 0, base);
    move_at(&mut rt, 4, 0, base + Duration::from_millis(30));
    release_at(&mut rt, 4, 0, base + Duration::from_millis(60));
    assert_eq!(rt.last_click_count(), 1);
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Simple));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
}

#[test]
fn double_click_drag_extends_by_words() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "foo bar baz");
    let base = Instant::now();
    // Double-click inside "foo".
    press_at(&mut rt, 1, 0, base);
    release_at(&mut rt, 1, 0, base + Duration::from_millis(40));
    press_at(&mut rt, 1, 0, base + Duration::from_millis(120));
    // Drag into "bar" while held, then release.
    move_at(&mut rt, 5, 0, base + Duration::from_millis(160));
    release_at(&mut rt, 5, 0, base + Duration::from_millis(200));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Word));
    assert_eq!(rt.selection_text().as_deref(), Some("foo bar"));
}

#[test]
fn triple_click_drag_extends_by_lines() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "line1\r\nline2\r\nline3");
    let base = Instant::now();
    for k in 0..3u64 {
        let now = base + Duration::from_millis(k * 100);
        press_at(&mut rt, 1, 0, now);
        if k < 2 {
            release_at(&mut rt, 1, 0, now + Duration::from_millis(20));
        }
    }
    // Still held after the third press: drag down two rows, then release.
    let drag = base + Duration::from_millis(320);
    move_at(&mut rt, 2, 2, drag);
    release_at(&mut rt, 2, 2, drag + Duration::from_millis(40));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Line));
    assert_eq!(rt.selection_text().as_deref(), Some("line1\nline2\nline3"));
}

#[test]
fn slow_second_press_restarts_at_single() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    press_at(&mut rt, 8, 0, base);
    release_at(&mut rt, 8, 0, base + Duration::from_millis(20));
    // 600ms later: beyond the 500ms chain window.
    let late = base + Duration::from_millis(600);
    press_at(&mut rt, 8, 0, late);
    assert_eq!(rt.last_click_count(), 1);
    release_at(&mut rt, 8, 0, late + Duration::from_millis(20));
    // A lone single press+release leaves no selection (collapsed clears).
    assert!(!rt.has_selection());
}

#[test]
fn far_second_press_restarts_at_single() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    press_at(&mut rt, 1, 0, base);
    release_at(&mut rt, 1, 0, base + Duration::from_millis(20));
    press_at(&mut rt, 20, 0, base + Duration::from_millis(100));
    assert_eq!(rt.last_click_count(), 1);
}

#[test]
fn drag_breaks_chain_next_press_is_single() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world foo bar");
    let base = Instant::now();
    // A real drag (press moves before release).
    press_at(&mut rt, 0, 0, base);
    move_at(&mut rt, 6, 0, base + Duration::from_millis(30));
    release_at(&mut rt, 6, 0, base + Duration::from_millis(60));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Simple));
    // A quick nearby press is single again, not double (drag broke the chain).
    let next = base + Duration::from_millis(120);
    press_at(&mut rt, 1, 0, next);
    assert_eq!(rt.last_click_count(), 1);
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Simple));
}

#[test]
fn non_left_press_breaks_chain() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    let base = Instant::now();
    press_at(&mut rt, 1, 0, base);
    release_at(&mut rt, 1, 0, base + Duration::from_millis(20));
    // Right-click paste in between (fail-soft headless: empty clipboard).
    rt.handle_cursor_moved_at(cell_pos(1, 0), base + Duration::from_millis(60));
    rt.handle_mouse_input_at(
        MouseEvent::new(MouseButton::Right, PressState::Pressed),
        base + Duration::from_millis(60),
    );
    press_at(&mut rt, 1, 0, base + Duration::from_millis(120));
    assert_eq!(rt.last_click_count(), 1);
}

#[test]
fn double_click_on_delimiter_yields_no_selection() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "a b");
    let base = Instant::now();
    // Column 1 is the space between words.
    press_at(&mut rt, 1, 0, base);
    release_at(&mut rt, 1, 0, base + Duration::from_millis(30));
    press_at(&mut rt, 1, 0, base + Duration::from_millis(100));
    release_at(&mut rt, 1, 0, base + Duration::from_millis(130));
    assert_eq!(rt.last_click_count(), 2);
    assert_eq!(rt.selection_kind(), None);
    assert!(!rt.has_selection());
}

#[test]
fn double_click_word_boundary_underscore_and_punctuation() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "foo_bar baz-qux");
    let base = Instant::now();
    // Underscore is a word char: "foo_bar" is one word.
    press_at(&mut rt, 2, 0, base);
    release_at(&mut rt, 2, 0, base + Duration::from_millis(30));
    press_at(&mut rt, 2, 0, base + Duration::from_millis(100));
    release_at(&mut rt, 2, 0, base + Duration::from_millis(130));
    assert_eq!(rt.selection_text().as_deref(), Some("foo_bar"));
    // Hyphen is a delimiter: "baz-qux" splits; click in "qux" gives "qux".
    rt.clear_selection();
    let later = base + Duration::from_millis(1000);
    press_at(&mut rt, 12, 0, later);
    release_at(&mut rt, 12, 0, later + Duration::from_millis(30));
    press_at(&mut rt, 12, 0, later + Duration::from_millis(100));
    release_at(&mut rt, 12, 0, later + Duration::from_millis(130));
    assert_eq!(rt.selection_text().as_deref(), Some("qux"));
}

#[test]
fn double_click_wide_char_word_expands() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "a\u{4e2d}b c");
    let base = Instant::now();
    // '中' (col 1, spacer col 2) joins a word with its ASCII neighbours.
    press_at(&mut rt, 1, 0, base);
    release_at(&mut rt, 1, 0, base + Duration::from_millis(30));
    press_at(&mut rt, 1, 0, base + Duration::from_millis(100));
    release_at(&mut rt, 1, 0, base + Duration::from_millis(130));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Word));
    assert_eq!(rt.selection_text().as_deref(), Some("a\u{4e2d}b"));
}

#[test]
fn alt_press_starts_block_selection() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "abcd\r\nefgh\r\nijkl");
    // Hold Alt (tiled layout: the float grab fails soft, selection wins).
    rt.handle_platform_event(bitty_platform::PlatformEvent::Window {
        window_id: bitty_platform::WindowId::from_raw_public(1),
        kind: bitty_platform::WindowEventKind::ModifiersChanged(ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        }),
    });
    let base = Instant::now();
    press_at(&mut rt, 1, 0, base);
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Block));
    move_at(&mut rt, 2, 2, base + Duration::from_millis(40));
    release_at(&mut rt, 2, 2, base + Duration::from_millis(80));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Block));
    assert_eq!(rt.selection_text().as_deref(), Some("bc\nfg\njk"));
    // The block rectangle normalizes independent of drag direction.
    assert_eq!(
        rt.selection().expect("block").normalized().start,
        CellPos::new(0, 1)
    );
}

#[test]
fn direct_word_line_block_apis_are_typed_for_copy_mode_and_search() {
    let mut rt = make_runtime();
    feed_text(&mut rt, "hello world");
    // Copy-mode (CTX-0384) and search UI (CTX-0383) build on these without
    // going through the mouse path.
    rt.start_word_selection(CellPos::new(0, 1));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Word));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
    rt.start_line_selection(CellPos::new(0, 3));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Line));
    assert_eq!(rt.selection_text().as_deref(), Some("hello world"));
    rt.start_block_selection(CellPos::new(0, 0));
    rt.update_selection(CellPos::new(0, 4));
    rt.end_selection(CellPos::new(0, 4));
    assert_eq!(rt.selection_kind(), Some(SelectionKind::Block));
    assert_eq!(rt.selection_text().as_deref(), Some("hello"));
}
