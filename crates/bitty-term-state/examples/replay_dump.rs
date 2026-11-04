//! Replay a raw pty capture through `bitty_vt::Parser` into
//! `bitty_term_state::State` and print the final grid, one numbered row per
//! line.
//!
//! The capture is output-only: bytes are fed in one batch, exactly as they
//! were read from the pty, with no terminal replies injected. Use the
//! committed fixture or any fresh capture:
//!
//! ```text
//! cargo run -p bitty-term-state --example replay_dump -- \
//!     crates/bitty-term-state/tests/data/agy-help-100x30.raw 100 30
//! ```

use std::env;
use std::process::ExitCode;

use bitty_term_state::State;
use bitty_vt::Parser;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: replay_dump <capture.raw> [cols] [rows]");
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
    let mut parser = Parser::new();
    parser.advance(&bytes, |action| {
        actions += 1;
        state.apply(&action);
    });

    println!("# {path}: {} bytes, {actions} actions", bytes.len());
    println!(
        "# grid {cols}x{rows}, alt_screen={}, cursor={:?}",
        state.alt_screen_active(),
        state.cursor().position
    );
    for row in 0..rows {
        let cells = state.live_grid_row(row).expect("row in range");
        let mut line = vec![' '; cols];
        for (col, cell) in cells.iter().enumerate() {
            if cell.spacer || cell.width == 0 {
                continue;
            }
            line[col] = cell.glyph;
        }
        println!("{:02}|{}|", row + 1, line.into_iter().collect::<String>());
    }
    ExitCode::SUCCESS
}
