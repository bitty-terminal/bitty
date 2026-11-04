//! Layout guard for bitty#2873: the Antigravity help page writes its left
//! band, erases the rest of that band with `ECH` (`CSI n X`), then uses an
//! absolute `CUP` to the description column.
//!
//! The observed render bug is a previous frame's tab row ("general commands
//! shortcuts") surviving in that band and reading as duplicated text. A
//! dropped or mis-ranged `ECH` (or a `CUP` that does not land on the
//! description column) reproduces that shape, so this test pins both the
//! erase band and the column, plus the damage that a damage-only painter
//! needs to clear the old glyphs.

#![forbid(unsafe_code)]

use bitty_term_state::{DamagedRegion, State};
use bitty_vt::Parser;

const COLS: usize = 100;
const ROWS: usize = 30;
const ROW: usize = 9; // 0-based row 10

fn feed(state: &mut State, bytes: &[u8]) -> Vec<DamagedRegion> {
    let mut damage = Vec::new();
    let mut parser = Parser::new();
    parser.advance(bytes, |action| {
        damage.extend(state.apply(&action).regions.iter().cloned());
    });
    damage
}

fn row_text(state: &State, row: usize) -> String {
    state
        .live_grid_row(row)
        .expect("row in range")
        .iter()
        .filter(|cell| !cell.spacer)
        .map(|cell| cell.glyph)
        .collect()
}

#[test]
fn ech_clears_the_left_band_before_the_description_column() {
    let mut state = State::new();
    state.resize(COLS, ROWS);

    // Previous frame: the tab row occupies the same band the help page must
    // clear for this line.
    feed(&mut state, b"\x1b[10;4Hgeneral commands shortcuts");
    assert!(row_text(&state, ROW).contains("general commands shortcuts"));

    // Exact shape agy 1.5.0 emits for "Open slash commands": `> /`, 29
    // erased cells, absolute column 33, two spaces, description.
    let damage = feed(
        &mut state,
        b"\x1b[10;1H> /\x1b[29X\x1b[10;33H\x1b[m  Open slash commands",
    );

    let row = row_text(&state, ROW);
    assert!(row.starts_with("> /"), "row {row:?}");
    assert!(
        row[3..32].chars().all(|ch| ch == ' '),
        "ECH 29 must clear columns 4..=32 (1-based), got {row:?}"
    );
    assert_eq!(&row[32..34], "  ", "CUP 10;33 plus two spaces");
    assert_eq!(
        &row[34..53],
        "Open slash commands",
        "description must start at 0-based column 34"
    );
    assert!(
        !row.contains("general"),
        "the previous tab row must not survive: {row:?}"
    );

    // A painter that follows damage alone needs the erased band marked;
    // state-only correctness is not enough for the reported symptom.
    assert!(
        damage.iter().any(|region| matches!(
            region,
            DamagedRegion::Grid(rect)
                if rect.top as usize == ROW
                    && rect.bottom as usize == ROW
                    && rect.left <= 3
                    && rect.right >= 31
        )),
        "ECH damage must cover the erased band, got {damage:?}"
    );
}
