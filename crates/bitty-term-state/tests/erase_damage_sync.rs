//! Tests verifying that damage rectangles synchronize with wide-character
//! boundary expansion during erases and overwrites (issue #1555, CTX-0887).

#![forbid(unsafe_code)]

use bitty_term_state::{DamagedRegion, State, TerminalAction};
use bitty_vt::{Col, Count, EraseDisplayMode, EraseLineMode, GraphemeCell, Row};

fn print_str(state: &mut State, s: &str) {
    for c in s.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

fn set_cursor(state: &mut State, row: u16, col: u16) {
    state.apply(&TerminalAction::CursorPosition {
        row: Row(row),
        col: Col(col),
    });
}

fn get_cell(state: &State, row: usize, col: usize) -> bitty_term_state::Cell {
    let snap = state.snapshot();
    snap.cells[row * snap.width + col]
}

#[test]
fn erase_in_line_right_on_spacer_damages_leading_column() {
    let mut state = State::new();
    // Print CJK "中" at row 1, col 1-2 (1-based: col 1 is lead, col 2 is spacer)
    set_cursor(&mut state, 1, 1);
    print_str(&mut state, "中");

    // Move cursor back to col 2 (the spacer half)
    set_cursor(&mut state, 1, 2);

    // Erase to end of line (like backspace / prompt redraw)
    let damage = state.apply(&TerminalAction::EraseInLine {
        mode: EraseLineMode::Right,
    });

    assert!(
        get_cell(&state, 0, 0).is_blank(),
        "leading half at col 0 must be erased"
    );
    assert!(
        get_cell(&state, 0, 1).is_blank(),
        "spacer at col 1 must be erased"
    );

    // Check damage: must cover row 0, col 0 (0-based)
    let has_leading_damage = damage.regions.iter().any(|r| match r {
        DamagedRegion::Grid(rect) => rect.top == 0 && rect.bottom == 0 && rect.left == 0,
        _ => false,
    });
    assert!(
        has_leading_damage,
        "Damage region must cover expanded leading column 0, got: {:?}",
        damage.regions
    );
}

#[test]
fn erase_in_line_left_on_leading_damages_trailing_spacer() {
    let mut state = State::new();
    // Print "A" at col 1, "中" at cols 2-3 (1-based)
    set_cursor(&mut state, 1, 1);
    print_str(&mut state, "A中B");

    // Move cursor to col 2 (the leading half of "中")
    set_cursor(&mut state, 1, 2);

    // Erase to start of line: expands right to cover the trailing spacer at col 3 (0-based col 2)
    let damage = state.apply(&TerminalAction::EraseInLine {
        mode: EraseLineMode::Left,
    });

    assert!(get_cell(&state, 0, 0).is_blank());
    assert!(get_cell(&state, 0, 1).is_blank());
    assert!(get_cell(&state, 0, 2).is_blank());
    assert_eq!(get_cell(&state, 0, 3).glyph, 'B');

    // Check damage: must cover through at least col 2 (0-based)
    let covers_spacer = damage.regions.iter().any(|r| match r {
        DamagedRegion::Grid(rect) => {
            rect.top == 0 && rect.bottom == 0 && rect.left == 0 && rect.right >= 2
        }
        _ => false,
    });
    assert!(
        covers_spacer,
        "Damage region must cover expanded spacer at col 2, got: {:?}",
        damage.regions
    );
}

#[test]
fn erase_chars_on_spacer_damages_leading_column() {
    let mut state = State::new();
    set_cursor(&mut state, 1, 1);
    print_str(&mut state, "A中B");

    // Move cursor to col 3 (1-based, spacer at 0-based col 2)
    set_cursor(&mut state, 1, 3);

    // ECH 1: erases 1 char at spacer -> must expand left to col 1 (0-based)
    let damage = state.apply(&TerminalAction::EraseChars { n: Count(1) });

    assert!(
        get_cell(&state, 0, 1).is_blank(),
        "leading half must be erased"
    );
    assert!(get_cell(&state, 0, 2).is_blank(), "spacer must be erased");

    let covers_lead = damage.regions.iter().any(|r| match r {
        DamagedRegion::Grid(rect) => {
            rect.top == 0 && rect.bottom == 0 && rect.left <= 1 && rect.right >= 2
        }
        _ => false,
    });
    assert!(
        covers_lead,
        "Damage region for ECH must cover leading half at col 1 and spacer at col 2, got: {:?}",
        damage.regions
    );
}

#[test]
fn erase_display_below_on_spacer_damages_leading_column_and_full_subsequent_rows() {
    let mut state = State::new();
    set_cursor(&mut state, 2, 1);
    print_str(&mut state, "中XYZ");

    // Move cursor to col 2 (spacer of "中" on row 2, 1-based)
    set_cursor(&mut state, 2, 2);

    let damage = state.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::Below,
    });

    assert!(
        get_cell(&state, 1, 0).is_blank(),
        "leading half must be erased"
    );
    assert!(get_cell(&state, 1, 1).is_blank(), "spacer must be erased");

    // Damage must cover row 1 col 0
    let covers_row1_col0 = damage.regions.iter().any(|r| match r {
        DamagedRegion::Grid(rect) => rect.top <= 1 && rect.bottom >= 1 && rect.left == 0,
        _ => false,
    });
    assert!(
        covers_row1_col0,
        "ED Below must damage leading col 0 on row 1, got: {:?}",
        damage.regions
    );
}

#[test]
fn print_over_wide_spacer_damages_correct_row_and_cleared_lead() {
    let mut state = State::new();
    // Put "中" on row 5, cols 10-11 (1-based: row 5, col 10)
    set_cursor(&mut state, 5, 10);
    print_str(&mut state, "中");

    // Move cursor to col 11 (the spacer half, 0-based col 10)
    // Invariant 1 steps the cursor back to the leading half at col 9.
    set_cursor(&mut state, 5, 11);

    // Print ASCII 'x' over the leading half; the trailing spacer at col 10 must be blanked
    let damage = state.apply(&TerminalAction::Print(GraphemeCell::from('x')));

    // Row 4 (0-based) col 9 has 'x', col 10 was the trailing spacer and must be blanked
    assert_eq!(get_cell(&state, 4, 9).glyph, 'x');
    assert!(
        get_cell(&state, 4, 10).is_blank(),
        "trailing spacer at col 10 must be blanked"
    );

    // The damaged rect must cover row 4 cols 9..=10, NOT row 10!
    let covers_row4_and_spacer = damage.regions.iter().any(|r| match r {
        DamagedRegion::Grid(rect) => {
            rect.top == 4 && rect.bottom == 4 && rect.left <= 9 && rect.right >= 10
        }
        _ => false,
    });
    assert!(
        covers_row4_and_spacer,
        "Printing over wide char must damage row 4 covering both lead (col 9) and spacer (col 10), got: {:?}",
        damage.regions
    );
}
