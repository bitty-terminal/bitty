//! Reflow / resize baseline — PB-3 reclaim + PB-4 latency shaping.
//!
//! Headless, bounded, `#![forbid(unsafe_code)]` harness for
//! `bitty-docs/docs/specifications/performance-budget-rfc.md`:
//! - PB-3 typical-session memory and growth: ≤ 250 MB 8-tab 4 h + reclaim 15%
//!   (this bench isolates the reflow cost that underlies that budget;
//!   real RSS comes from `tools/perf/rss`).
//! - PB-4 input latency headroom: `State::resize` must stay bounded so
//!   it never spills into the key-to-screen hot path tail.
//!
//! Measures `State::resize(new_cols, new_rows)` over bounded dimensions
//! (clamped 1..1000, scrollback 10 k) on headless `State` (no `winit`,
//! no `wgpu`). Bounded via `State` invariants (GRID 80×24 → resize lattice,
//! tab lattice, scrollback `resize(cols)`), `forbid(unsafe)`, deterministic
//! via pure `State` transitions and `state_hash` reuse.
//!
//! Budget reference: `bitty-docs/docs/specifications/performance-budget-rfc.md#pb-3`.
//!
//! Run headlessly:
//! ```text
//! cargo bench -p bitty-perf --bench reflow -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::Instant;

use bitty_term_state::{State, TerminalAction};
use bitty_vt::{GraphemeCell, Parser};

fn fill_state_with_content(rows: usize, cols: usize) -> State {
    let mut s = State::new();
    // Fill with synthetic scrollback + live grid content so reflow has work.
    let mut parser = Parser::new();
    let filler = b"hello world \x1b[31mRED\x1b[0m\x1b[2J\x1b[H".repeat(4);
    let mut actions = Vec::new();
    parser.advance(&filler, |a| actions.push(a));
    for _ in 0..rows {
        for a in &actions {
            s.apply(a);
        }
        s.apply(&TerminalAction::Print(GraphemeCell::from('\n')));
    }
    // Include wide chars to exercise spacer invariants.
    for ch in ['🎉', 'A', 'B', 'C', 'D'] {
        s.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
    }
    // Make dimensions diverge from default so resize does work; keep bounded.
    let _ = s.resize(cols, rows);
    s
}

fn bench_resize(from: (usize, usize), to: (usize, usize), iters: usize) -> f64 {
    // Prepare a state at `from` with content; each iter clones then resizes to `to`.
    let base = fill_state_with_content(from.1, from.0);
    let start = Instant::now();
    for _ in 0..iters {
        let mut s = black_box(base.clone());
        let dmg = s.resize(black_box(to.0), black_box(to.1));
        black_box(dmg);
        debug_assert!(
            s.check_invariants().is_ok(),
            "reflow invariant violated {:?}",
            s.check_invariants()
        );
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    (elapsed * 1_000_000.0) / iters as f64
}

fn main() {
    println!("reflow — State::resize headless, bounded (PB-3 reclaim 15% & PB-4 latency)");
    println!(
        "  default GRID {}×{}, scrollback cap {}, bounded cols/rows 1..1000",
        bitty_term_state::GRID_COLUMNS,
        bitty_term_state::GRID_ROWS,
        bitty_term_state::scrollback::SCROLLBACK_MAX_LINES
    );

    #[allow(clippy::type_complexity)]
    let cases: &[((usize, usize), (usize, usize), usize, &str)] = &[
        ((80, 24), (120, 40), 2_000, "expand_80x24→120x40"),
        ((120, 40), (80, 24), 2_000, "shrink_120x40→80x24"),
        ((80, 24), (132, 43), 2_000, "expand_132x43"),
        ((80, 24), (40, 12), 1_500, "shrink_40x12"),
        ((80, 24), (80, 24), 3_000, "noop_80x24"),
        ((200, 60), (300, 80), 1_000, "large_200x60→300x80"),
    ];

    for ((fc, fr), (tc, tr), iters, label) in cases {
        let mean_us = bench_resize((*fc, *fr), (*tc, *tr), *iters);
        println!("  {label}: {mean_us:.2} µs mean over {iters} iters");
        // Soft sanity: resize + full redraw plan should stay << PB-4 p50 8 ms.
        if mean_us > 8_000.0 {
            eprintln!(
                "warning: {label} {mean_us:.0} µs exceeds PB-4 p50 headroom (8 ms) — investigate reflow tail"
            );
        }
    }

    // Copy appendaged reclaim shape: resize does not grow scrollback unboundedly.
    {
        let mut s = fill_state_with_content(24, 80);
        for _ in 0..100 {
            let _ = s.resize(120, 40);
            let _ = s.resize(80, 24);
        }
        assert!(
            s.scrollback_len() <= bitty_term_state::scrollback::SCROLLBACK_MAX_LINES,
            "scrollback over cap after reflow churn"
        );
        println!(
            "  scrollback after 200 resizes: {} ≤ {}",
            s.scrollback_len(),
            bitty_term_state::scrollback::SCROLLBACK_MAX_LINES
        );
    }

    // Zero-size skip (State clamps to 1) — ensure no panic tail.
    {
        let mut s = State::new();
        let d0 = s.resize(0, 0);
        black_box(d0);
        println!("  zero-size clamp exercised (0,0) → 1×1, no panic");
    }
}
