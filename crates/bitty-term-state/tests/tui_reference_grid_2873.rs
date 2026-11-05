//! bitty#2873 — per-program reference grid check.
//!
//! The issue's acceptance is "for each reported program, either the grid
//! matches the reference terminal byte for byte, or the divergence is traced
//! to one identified action". These fixtures are that check, frozen.
//!
//! Each program contributes a `.raw`/`.grid` pair:
//!
//! * `.raw` is the exact byte stream the program wrote. It was recorded with
//!   `tmux pipe-pane -O` while the program ran in a 100x30 tmux pane.
//! * `.grid` is what the reference terminal showed for that same stream, from
//!   `tmux capture-pane` (tmux 3.7c) of the same run, one row per line, right
//!   trimmed.
//!
//! Both halves are byte-identical across repeated captures of the same
//! command (no clock, no live counters), so the pair is a stable reference.
//! Replaying `.raw` through `Parser` + `State` at the same geometry must
//! reproduce `.grid` exactly.
//!
//! Regenerate with `scripts/vt-ref-diff.py --capture <program>`; it is the
//! same harness that produced these files and the evidence matrix in the
//! issue.

#![forbid(unsafe_code)]

use bitty_term_state::State;
use bitty_vt::Parser;

const COLS: usize = 100;
const ROWS: usize = 30;

/// Right-trimmed row text, wide-pair spacers omitted (a reference terminal
/// prints a double-width glyph once, not once per cell).
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

fn assert_matches_reference(program: &str, raw: &[u8], expected: &str) {
    let mut state = State::new();
    state.resize(COLS, ROWS);
    let mut parser = Parser::new();
    parser.advance(raw, |action| {
        state.apply(&action);
    });

    let expected: Vec<&str> = expected.lines().map(str::trim_end).collect();
    let got: Vec<String> = (0..ROWS).map(|row| row_text(&state, row)).collect();
    let mut mismatches = Vec::new();
    for (row, want) in expected.iter().enumerate() {
        let have = got.get(row).map(String::as_str).unwrap_or("");
        if have != *want {
            mismatches.push((row, have.to_string(), (*want).to_string()));
        }
    }
    // Rows past the fixture's last line must be blank.
    for (row, have) in got.iter().enumerate().skip(expected.len()) {
        if !have.is_empty() {
            mismatches.push((row, have.clone(), String::new()));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{program}: replay diverges from the reference grid on {} row(s): {mismatches:#?}",
        mismatches.len()
    );
    assert!(state.check_invariants().is_ok(), "{program}: invariants");
}

#[test]
fn vim_grid_matches_the_reference_terminal() {
    // `vim -u NONE -N` on a 200-line file, drawn once and left idle. Vim is
    // the densest consumer of absolute positioning, EL and scroll regions in
    // the reported corpus.
    assert_matches_reference(
        "vim",
        include_bytes!("data/vim-100x30.raw"),
        include_str!("data/vim-100x30.grid"),
    );
}

#[test]
fn less_grid_matches_the_reference_terminal() {
    assert_matches_reference(
        "less",
        include_bytes!("data/less-100x30.raw"),
        include_str!("data/less-100x30.grid"),
    );
}
