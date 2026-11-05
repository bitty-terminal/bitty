//! bitty#2873 — the DECAWM deferred-wrap latch across a reposition.
//!
//! The owner report is "text that looks duplicated, missing spaces" in
//! full-screen TUIs. Candidate (2) of the audit was the deferred-wrap latch:
//! a write that lands in the last column with autowrap on leaves the cursor
//! on that column with a wrap *pending*, and every following action must
//! resolve that latch exactly as a reference terminal does. A latch resolved
//! one column to the left turns an in-place overwrite into a two-glyph tail,
//! which is precisely the duplicated-text shape.
//!
//! Every expectation below is reference behaviour, not current behaviour:
//!
//! * xterm's `CASE_BS` (`if (do_wrap) do_wrap = False; else if (cur_col > 0)
//!   cur_col--`) and tmux 3.7c agree that BS only consumes the latch.
//! * The erase/insert actions (`EL`, `ECH`, `DCH`, `ICH`, `ED`, `IL`, `SGR`)
//!   never move the cursor, so they leave the latch latched; both references
//!   then wrap the next print onto the following row.
//! * Every cursor motion (`CUP`, `CHA`, `CUB`, `CUU`, `CR`, `TAB`, `LF`,
//!   `IND`, `RI`, `NEL`) consumes the latch.
//!
//! The scripts are raw bytes so they exercise `Parser` + `State` together:
//! a parser that dropped or mis-split one of these sequences fails here too.

#![forbid(unsafe_code)]

use bitty_term_state::{DamagedRegion, State};
use bitty_vt::Parser;

const COLS: usize = 20;
const ROWS: usize = 6;
/// The row every case works on (0-based); `\x1b[3;1H` addresses it.
const ROW: usize = 2;

fn new_state() -> State {
    let mut state = State::new();
    state.resize(COLS, ROWS);
    state
}

/// Feed one byte script through the real parser, returning applied damage.
fn feed(state: &mut State, bytes: &[u8]) -> Vec<DamagedRegion> {
    let mut damage = Vec::new();
    let mut parser = Parser::new();
    parser.advance(bytes, |action| {
        damage.extend(state.apply(&action).regions.iter().cloned());
    });
    damage
}

/// Right-trimmed row text, wide-pair spacers omitted.
fn row_text(state: &State, row: usize) -> String {
    state
        .live_grid_row(row)
        .expect("row in range")
        .iter()
        .filter(|cell| !cell.spacer)
        .map(|cell| cell.glyph)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// `\x1b[3;1H` + one full row of `x`: the cursor sits on the last column with
/// a deferred wrap latched.
fn latched_row() -> Vec<u8> {
    let mut script = b"\x1b[3;1H".to_vec();
    script.extend(std::iter::repeat_n(b'x', COLS));
    script
}

/// The cell holding `glyph` (the fixture rows are unambiguous).
fn find_glyph(state: &State, needle: char) -> Option<(usize, u16)> {
    (0..ROWS).find_map(|row| {
        state
            .live_grid_row(row)?
            .iter()
            .position(|cell| !cell.spacer && cell.glyph == needle)
            .map(|col| (row, col as u16))
    })
}

/// Where a probe glyph printed after `tail` lands, and whether the latch was
/// still armed just before that print.
fn probe(tail: &[u8]) -> (usize, u16, bool) {
    let mut state = new_state();
    let mut script = latched_row();
    script.extend_from_slice(tail);
    feed(&mut state, &script);
    let latched = state.cursor().pending_wrap;
    feed(&mut state, b"Q");
    let (row, col) = find_glyph(&state, 'Q').expect("probe glyph must be placed");
    (row, col, latched)
}

#[test]
fn overstrike_at_the_right_margin_overwrites_in_place() {
    // `man`/`less` bold runs are overstrike: glyph, BS, same glyph. When the
    // pair lands on the last column the overwrite must stay in place.
    let mut state = new_state();
    let mut script = b"\x1b[3;1H".to_vec();
    script.extend(std::iter::repeat_n(b'x', COLS - 1));
    script.extend_from_slice(b"a\x08A");
    let damage = feed(&mut state, &script);

    let text = row_text(&state, ROW);
    assert_eq!(
        text,
        format!("{}A", "x".repeat(COLS - 1)),
        "BS at the latched margin must overwrite column {}, got {text:?}",
        COLS - 1
    );
    assert!(
        !text.ends_with("aA"),
        "the pre-overstrike glyph must not survive: {text:?}"
    );
    // The overwrite re-latches: the cursor is still on the last column and the
    // next print belongs on the following row.
    assert_eq!(state.cursor().position.col, COLS as u16 - 1);
    assert!(state.cursor().pending_wrap);
    // A damage-only painter must be told to repaint the cell that changed.
    let last = COLS as u16 - 1;
    assert!(
        damage.iter().any(|region| matches!(
            region,
            DamagedRegion::Grid(rect) if rect.top as usize == ROW
                && rect.bottom as usize == ROW
                && rect.left <= last
                && rect.right >= last
        )),
        "the overwritten cell must be damaged, got {damage:?}"
    );
    assert!(state.check_invariants().is_ok());
}

#[test]
fn a_bold_overstrike_run_across_the_margin_has_no_duplicate_tail() {
    // Twenty `a\bA` pairs: one net column each, ending exactly on the last
    // column. Every overwrite is in place, so the row holds twenty `A`s and
    // nothing else; stepping left would leave `a` stranded in the last cell.
    let mut state = new_state();
    let mut script = b"\x1b[3;1H".to_vec();
    for _ in 0..COLS {
        script.extend_from_slice(b"a\x08A");
    }
    feed(&mut state, &script);

    assert_eq!(row_text(&state, ROW), "A".repeat(COLS));
    assert!(state.check_invariants().is_ok());
}

#[test]
fn backspace_consumes_the_latch_and_keeps_the_column() {
    let mut state = new_state();
    feed(&mut state, &latched_row());
    assert_eq!(state.cursor().position.col, COLS as u16 - 1);
    assert!(state.cursor().pending_wrap);

    feed(&mut state, b"\x08");
    assert_eq!(
        state.cursor().position.col,
        COLS as u16 - 1,
        "BS with a pending wrap must not step left"
    );
    assert!(!state.cursor().pending_wrap);

    // With the latch consumed, the next glyph is placed in the same cell and
    // latches the wrap again.
    feed(&mut state, b"B");
    assert_eq!(state.cursor().position.row, ROW as u16);
    let cells = state.live_grid_row(ROW).expect("row in range");
    assert_eq!(cells[COLS - 1].glyph, 'B');
    assert!(state.cursor().pending_wrap);
    assert!(state.check_invariants().is_ok());
}

#[test]
fn backspace_still_steps_left_without_a_latch() {
    let mut state = new_state();
    feed(&mut state, b"\x1b[3;1Habc\x08Z");
    assert_eq!(row_text(&state, ROW), "abZ");
    assert_eq!(state.cursor().position.col, 3);
    assert!(!state.cursor().pending_wrap);
}

#[test]
fn backspace_at_column_zero_stays_at_column_zero() {
    let mut state = new_state();
    feed(&mut state, b"\x1b[3;1H\x08Q");
    assert_eq!(row_text(&state, ROW), "Q");
    assert_eq!(state.cursor().position.col, 1);
}

/// Actions that never move the cursor leave the latch armed: the next print
/// wraps onto the following row. Verified against tmux 3.7c for every entry.
#[test]
fn erase_and_attribute_actions_keep_the_latch() {
    let cases: &[(&str, &[u8])] = &[
        ("EL 0", b"\x1b[K"),
        ("EL 1", b"\x1b[1K"),
        ("EL 2", b"\x1b[2K"),
        ("ECH", b"\x1b[2X"),
        ("DCH", b"\x1b[1P"),
        ("ICH", b"\x1b[1@"),
        ("ED 1", b"\x1b[1J"),
        ("IL", b"\x1b[L"),
        ("SGR", b"\x1b[1m"),
    ];
    for (name, tail) in cases {
        let (row, col, stayed_latched) = probe(tail);
        assert!(stayed_latched, "{name} must not clear the deferred wrap");
        assert_eq!(
            (row, col),
            (ROW + 1, 0),
            "{name}: the next print must wrap onto the following row"
        );
    }
}

/// Actions that move the cursor consume the latch, so the next print lands
/// where the motion put the cursor instead of wrapping.
///
/// `LF`/`IND` keep the column (ECMA-48: "the same character position of the
/// following line"), so the probe lands on the last column of the next row.
/// tmux advances a *latched* LF by two rows instead; ECMA-48 and xterm's
/// `Index` are the reference here, and no program in the #2873 corpus emits a
/// bare LF against a pending wrap (they send CR LF or an absolute CUP).
#[test]
fn cursor_motion_consumes_the_latch() {
    // tail, expected (row, col) of the probe glyph.
    let cases: &[(&str, &[u8], usize, u16)] = &[
        ("CUP", b"\x1b[5;3H", 4, 2),
        ("CHA", b"\x1b[4G", ROW, 3),
        ("CUB", b"\x1b[2D", ROW, COLS as u16 - 3),
        ("CUU", b"\x1b[1A", ROW - 1, COLS as u16 - 1),
        ("CR", b"\r", ROW, 0),
        ("TAB", b"\t", ROW, COLS as u16 - 1),
        ("LF", b"\n", ROW + 1, COLS as u16 - 1),
        ("IND", b"\x1bD", ROW + 1, COLS as u16 - 1),
        ("RI", b"\x1bM", ROW - 1, COLS as u16 - 1),
        ("NEL", b"\x1bE", ROW + 1, 0),
    ];
    for (name, tail, row, col) in cases {
        let (got_row, got_col, wrapped) = probe(tail);
        assert!(
            !wrapped,
            "{name} must consume the deferred wrap before the next print"
        );
        assert_eq!(
            (got_row, got_col),
            (*row, *col),
            "{name}: probe glyph landed in the wrong cell"
        );
    }
}

/// `?7l` disarms the latch outright, and re-enabling autowrap does not bring
/// a stale wrap back.
#[test]
fn autowrap_toggle_clears_the_latch() {
    let mut state = new_state();
    feed(&mut state, b"\x1b[?7l\x1b[3;1H");
    feed(&mut state, &[b'x'; COLS]);
    assert_eq!(state.cursor().position.col, COLS as u16 - 1);
    assert!(
        !state.cursor().pending_wrap,
        "autowrap off must never latch a deferred wrap"
    );
    feed(&mut state, b"Q");
    assert_eq!(state.cursor().position.row, ROW as u16);
    let cells = state.live_grid_row(ROW).expect("row in range");
    assert_eq!(cells[COLS - 1].glyph, 'Q');

    let mut armed = new_state();
    feed(&mut armed, &latched_row());
    assert!(armed.cursor().pending_wrap);
    feed(&mut armed, b"\x1b[?7hQ");
    assert_eq!(
        armed.cursor().position.row,
        ROW as u16,
        "?7h must not re-arm a consumed latch"
    );
}
