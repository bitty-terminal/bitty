//! Wide-cell interaction with `ICH` (insert blank characters), bitty#2873.
//!
//! The reported defect: with a wide pair to the right of the insertion
//! point, `CSI @` blanked the pair's lead instead of shifting the pair, so
//! the whole row came out empty. These tests drive the wire bytes from the
//! issue through the real parser into [`State`].

#![forbid(unsafe_code)]

use bitty_term_state::{Cell, State};

/// Feeds raw bytes through the vt parser into `state`.
fn feed(state: &mut State, bytes: &[u8]) {
    let mut parser = bitty_vt::Parser::new();
    let mut actions = Vec::new();
    parser.advance(bytes, |action| actions.push(action));
    for action in &actions {
        state.apply(action);
    }
}

fn cell(state: &State, row: usize, col: usize) -> Cell {
    let snapshot = state.snapshot();
    snapshot.cells[row * snapshot.width + col]
}

/// RFC invariant 2 shape check without repairing: no orphan spacer, no
/// unpaired wide lead.
fn assert_pairs_intact(state: &State, row: usize, ctx: &str) {
    let snapshot = state.snapshot();
    for col in 0..snapshot.width {
        let cell = snapshot.cells[row * snapshot.width + col];
        if cell.spacer {
            assert!(col > 0, "{ctx}: orphan spacer at col 0");
            let lead = snapshot.cells[row * snapshot.width + col - 1];
            assert!(
                lead.width == 2 && !lead.spacer,
                "{ctx}: orphan spacer at col {col}"
            );
        } else if cell.width == 2 {
            assert!(
                col + 1 < snapshot.width && snapshot.cells[row * snapshot.width + col + 1].spacer,
                "{ctx}: unpaired wide lead at col {col}"
            );
        }
    }
}

#[test]
fn issue_repro_ich_left_of_wide_pair_keeps_the_glyph() {
    // The exact repro from bitty#2873: 100x30, print one wide glyph then
    // ICH over it.
    let mut state = State::new();
    state.resize(100, 30);

    feed(&mut state, b"\x1b[1;96H\xe4\xb8\xad");
    feed(&mut state, b"\x1b[1;96H\x1b[1@");

    assert!(
        cell(&state, 0, 95).is_blank(),
        "the inserted column is blank"
    );
    let lead = cell(&state, 0, 96);
    assert_eq!(lead.glyph, '中', "wide lead must shift one column right");
    assert_eq!(lead.width, 2);
    assert!(!lead.spacer);
    assert!(
        cell(&state, 0, 97).spacer,
        "the spacer must travel with its lead"
    );
    assert_pairs_intact(&state, 0, "ICH at a wide lead");
}

#[test]
fn ich_left_of_wide_pair_pushes_the_following_tail() {
    let mut state = State::new();
    state.resize(20, 3);

    feed(&mut state, "中ab".as_bytes());
    feed(&mut state, b"\x1b[1;1H\x1b[1@");

    assert!(cell(&state, 0, 0).is_blank());
    assert_eq!(cell(&state, 0, 1).glyph, '中');
    assert!(cell(&state, 0, 2).spacer);
    assert_eq!(cell(&state, 0, 3).glyph, 'a');
    assert_eq!(cell(&state, 0, 4).glyph, 'b');
    assert_pairs_intact(&state, 0, "ICH with a tail");
}

#[test]
fn cursor_on_spacer_snaps_to_the_lead_then_ich_shifts_the_pair() {
    // Invariant 3: the cursor never rests on a spacer half, so `CSI 1;2H`
    // over the pair's trailing half normalizes onto the lead at col 0. ICH
    // there moves both halves together. The split-pair path (ICH at a cell
    // the cursor cannot occupy) is exercised directly in the `grid` unit
    // tests.
    let mut state = State::new();
    state.resize(20, 3);

    feed(&mut state, "中ab".as_bytes());
    // 1-based col 2 is the spacer half of the pair.
    feed(&mut state, b"\x1b[1;2H\x1b[1@");

    assert!(cell(&state, 0, 0).is_blank(), "inserted column is blank");
    assert_eq!(cell(&state, 0, 1).glyph, '中', "pair shifts one right");
    assert!(cell(&state, 0, 2).spacer, "spacer travels with its lead");
    assert_eq!(cell(&state, 0, 3).glyph, 'a');
    assert_eq!(cell(&state, 0, 4).glyph, 'b');
    assert_pairs_intact(&state, 0, "ICH after a spacer snap");
}

#[test]
fn ich_past_right_margin_clamps_without_panicking() {
    let mut state = State::new();
    state.resize(10, 2);

    feed(&mut state, "abcdefghij".as_bytes());
    // Cursor to the last column, then ask for far more inserts than fit.
    feed(&mut state, b"\x1b[1;10H\x1b[99@");

    let snapshot = state.snapshot();
    assert_eq!(snapshot.width, 10);
    assert_eq!(snapshot.height, 2);
    assert_eq!(cell(&state, 0, 8).glyph, 'i', "cells left of the cut stay");
    assert!(
        cell(&state, 0, 9).is_blank(),
        "clamped insert blanks the tail"
    );
    assert!(
        snapshot.cells.len() == snapshot.width * snapshot.height,
        "row geometry is preserved"
    );
    assert_pairs_intact(&state, 0, "clamped ICH");
}
