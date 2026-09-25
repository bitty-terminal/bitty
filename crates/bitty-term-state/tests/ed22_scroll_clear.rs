//! `ED 22` scroll-and-clear semantics (issue #1396).
//!
//! `ESC [ 22 J` (kitty extension, adopted by ghostty) scrolls the visible
//! screen into the scrollback and then clears the screen. Retained
//! scrollback content is preserved (no `ED 3` semantics), the capture is
//! bounded by the configured scrollback capacity, and the alternate screen
//! — which owns no scrollback — only clears.

use bitty_term_state::{Cell, State, TerminalAction};
use bitty_vt::{Col, ControlChar, EraseDisplayMode, GraphemeCell, Mode, Row};

fn print_char(c: char) -> TerminalAction {
    TerminalAction::Print(GraphemeCell::from(c))
}

fn print_str(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&print_char(c));
    }
}

/// Carriage return + line feed, matching what a shell emits per line.
fn newline(state: &mut State) {
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
}

fn scroll_and_clear(state: &mut State) {
    state.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::ScrollAndClear,
    });
}

fn row_text(cells: &[Cell]) -> String {
    cells.iter().map(|c| c.glyph).collect()
}

fn line_text(state: &State, index: usize) -> String {
    match state.scrollback_line(index) {
        Some(line) => row_text(&line.cells),
        None => panic!("scrollback line {index} is not retained"),
    }
}

fn assert_screen_blank(state: &State, ctx: &str) {
    let bad = state
        .snapshot()
        .cells
        .iter()
        .filter(|cell| !cell.is_blank())
        .count();
    assert_eq!(bad, 0, "{ctx}: ED 22 must leave every visible cell blank");
}

#[test]
fn ed22_scrolls_visible_content_into_scrollback_then_clears() {
    let mut s = State::new();
    let h = s.height();

    s.apply(&TerminalAction::CursorPosition {
        row: Row(1),
        col: Col(1),
    });
    print_str(&mut s, "alpha");
    assert_eq!(s.scrollback_len(), 0, "no history before ED 22");

    scroll_and_clear(&mut s);

    assert_screen_blank(&s, "plain screen");
    assert_eq!(
        s.scrollback_len(),
        h,
        "the whole visible screen scrolls into the scrollback"
    );
    let first = line_text(&s, 0);
    assert!(
        first.starts_with("alpha"),
        "capture must start with the top visible row, got {first:?}"
    );
    assert!(
        line_text(&s, 1).trim().is_empty(),
        "the blank row below the marker is captured in place"
    );
    assert!(
        match s.scrollback_line(0) {
            Some(line) => !line.wrapped,
            None => panic!("captured line 0 is not retained"),
        },
        "a hard-broken first row stays unwrapped"
    );
    assert!(!s.alt_screen_active());
}

#[test]
fn ed22_preserves_retained_scrollback_content() {
    let mut s = State::new();
    let h = s.height();
    for i in 0..(h + 3) {
        print_str(&mut s, &format!("hist-{i}"));
        newline(&mut s);
    }
    let before_len = s.scrollback_len();
    assert!(before_len >= 3, "test needs real history, got {before_len}");
    let before: Vec<(u64, String)> = (0..before_len)
        .map(|i| (s.scrollback_line(i).map_or(0, |l| l.id), line_text(&s, i)))
        .collect();

    scroll_and_clear(&mut s);

    assert_eq!(
        s.scrollback_len(),
        before_len + h,
        "ED 22 appends the visible screen; nothing is evicted at default capacity"
    );
    let after: Vec<(u64, String)> = (0..before_len)
        .map(|i| (s.scrollback_line(i).map_or(0, |l| l.id), line_text(&s, i)))
        .collect();
    assert_eq!(
        after, before,
        "retained scrollback content must be untouched (no ED 3 semantics)"
    );
    let captured: Vec<String> = (before_len..before_len + h)
        .map(|i| line_text(&s, i))
        .collect();
    let newest = format!("hist-{}", h + 2);
    assert!(
        captured.iter().any(|line| line.starts_with(&newest)),
        "the newest visible row must be captured, got {captured:?}"
    );
}

#[test]
fn ed22_capture_is_bounded_by_scrollback_capacity() {
    let mut s = State::with_scrollback_lines(3);
    let h = s.height();
    print_str(&mut s, "content");
    scroll_and_clear(&mut s);

    assert_eq!(
        s.scrollback_len(),
        3,
        "retention is clamped to the configured capacity"
    );
    assert_eq!(
        s.scrollback_evicted_total(),
        (h - 3) as u64,
        "oldest-first eviction reuses the bounded push path"
    );
    assert!(
        s.scrollback()
            .all(|line| row_text(&line.cells).trim().is_empty()),
        "only the newest (blank tail) rows survive the bounded capture"
    );
    assert_screen_blank(&s, "capacity 3");

    // Capacity 0 disables retention; ED 22 still clears the screen.
    let mut s0 = State::with_scrollback_lines(0);
    print_str(&mut s0, "content");
    scroll_and_clear(&mut s0);
    assert_eq!(s0.scrollback_len(), 0, "zero capacity retains nothing");
    assert_screen_blank(&s0, "capacity 0");
}

#[test]
fn ed22_on_alternate_screen_clears_without_capturing() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreen,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    print_str(&mut s, "alt-ui");

    scroll_and_clear(&mut s);

    assert_eq!(
        s.scrollback_len(),
        0,
        "alt-screen content must not leak into primary history"
    );
    assert_screen_blank(&s, "alternate screen");
}
