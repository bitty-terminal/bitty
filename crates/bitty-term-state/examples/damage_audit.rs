//! Damage-completeness audit for a raw pty capture.
//!
//! Feeds each parsed action through `State`, diffs the visible grid before
//! and after, and reports every changed cell that no damage rectangle in the
//! same batch covers. A gap means a paint layer following damage alone would
//! keep the previous cell content on screen.

use std::env;
use std::process::ExitCode;

use bitty_term_state::{DamagedRegion, State};
use bitty_vt::Parser;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: damage_audit <capture.raw> [cols] [rows]");
        return ExitCode::FAILURE;
    };
    let cols = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    let rows = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut state = State::new();
    state.resize(cols, rows);
    let mut actions = 0usize;
    let mut gaps = 0usize;
    let mut parser = Parser::new();
    parser.advance(&bytes, |action| {
        actions += 1;
        let before = state.snapshot();
        let damage = state.apply(&action);
        let after = state.snapshot();
        for row in 0..rows {
            for col in 0..cols {
                let index = row * cols + col;
                if before.cells[index] == after.cells[index] {
                    continue;
                }
                let covered = damage.regions.iter().any(|region| match region {
                    DamagedRegion::Grid(rect) => {
                        rect.top as usize <= row
                            && row <= rect.bottom as usize
                            && rect.left as usize <= col
                            && col <= rect.right as usize
                    }
                    DamagedRegion::Scroll { top, bottom, .. } => {
                        *top as usize <= row && row <= *bottom as usize
                    }
                    DamagedRegion::Scrollback { .. } => false,
                });
                if !covered {
                    gaps += 1;
                    if gaps <= 20 {
                        println!(
                            "gap action #{actions} {action:?} at row={} col={} before={:?} after={:?} damage={:?}",
                            row + 1,
                            col + 1,
                            before.cells[index],
                            after.cells[index],
                            damage.regions,
                        );
                    }
                }
            }
        }
    });
    println!("# {path}: {actions} actions, {gaps} uncovered changed cells");
    if gaps == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
