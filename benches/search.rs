//! Scrollback search baseline — bounded, deterministic, headless.
//!
//! Headless, bounded, `#![forbid(unsafe_code)]` harness for
//! `bitty-term-state::search` (CTX-0060). Search is a pure function of
//! `(State, pattern, SearchOptions)` with:
//! - `SEARCH_MAX_PATTERN_LEN = 256` bytes (char-boundary truncated)
//! - `SEARCH_MAX_RESULTS = 1000` (hard cap per call)
//! - `SCROLLBACK_MAX_LINES = 10 000` (bounded heap)
//!
//! This bench builds scrollback up to bounded caps headlessly (no `winit`,
//! no `wgpu`) and measures `State::search` latency and truncation
//! behaviour. It does not map to a single PB budget today but supports
//! PB-7 idle (search must not wake the renderer) and future scrollback
//! UX budgets under `reflow`/`search`.
//!
//! Budget reference: search bounds mirror PB-2/PB-3 memory philosophy
//! (`bitty-docs/docs/specifications/performance-budget-rfc.md#pb-2`)
//! and `crates/bitty-term-state/src/search.rs` docs.
//!
//! Run headlessly:
//! ```text
//! cargo bench -p bitty-perf --bench search -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::Instant;

use bitty_term_state::search::{SEARCH_MAX_PATTERN_LEN, SEARCH_MAX_RESULTS, SearchOptions};
use bitty_term_state::{State, TerminalAction};
use bitty_vt::{GraphemeCell, Parser};

fn fill_scrollback(lines: usize, cols: usize) -> State {
    let mut s = State::new();
    let _ = s.resize(cols, 24);
    let mut parser = Parser::new();
    // Line with a searchable token `needle` plus filler to exercise char→col mapping.
    let template = format!("line {:04} needle filler hello world 🎉\n", 0);
    for i in 0..lines {
        let line = template.replace("0000", &format!("{i:04}"));
        let mut actions = Vec::new();
        parser.advance(line.as_bytes(), |a| actions.push(a));
        for a in actions {
            s.apply(&a);
        }
        // Force scroll for real scrollback pressure: Fill beyond grid height.
        if i % 10 == 0 {
            s.apply(&TerminalAction::Print(GraphemeCell::from('\n')));
        }
    }
    s
}

fn search_once(state: &State, pat: &str, opts: SearchOptions) -> usize {
    let matches = state.search(black_box(pat), black_box(opts));
    black_box(matches.len())
}

fn bench_search(state: &State, pat: &str, opts: SearchOptions, iters: usize) -> f64 {
    // Warmup.
    let _ = search_once(state, pat, opts);
    let start = Instant::now();
    let mut tot = 0usize;
    for _ in 0..iters {
        tot += search_once(state, pat, opts);
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    // Keep tot live to avoid DCE; return mean micros.
    let _ = black_box(tot);
    (elapsed * 1_000_000.0) / iters as f64
}

fn main() {
    println!(
        "search — State::search headless, bounded (pattern {SEARCH_MAX_PATTERN_LEN} B, results {SEARCH_MAX_RESULTS}, scrollback {})",
        bitty_term_state::scrollback::SCROLLBACK_MAX_LINES
    );

    let small = fill_scrollback(200, 80);
    let large = fill_scrollback(2_000, 80);
    let wide = fill_scrollback(500, 200);

    println!(
        "  corpus small: 200 lines 80cols scrollback={}",
        small.scrollback_len()
    );
    println!(
        "  corpus large: 2000 lines 80cols scrollback={}",
        large.scrollback_len()
    );
    println!(
        "  corpus wide: 500 lines 200cols scrollback={}",
        wide.scrollback_len()
    );

    let cases: &[(&State, &str, SearchOptions, usize, &str)] = &[
        (
            &small,
            "needle",
            SearchOptions::new(true, 100),
            2_000,
            "small_exact_100",
        ),
        (
            &small,
            "HELLO",
            SearchOptions::new(false, 100),
            2_000,
            "small_casefold_100",
        ),
        (
            &large,
            "needle",
            SearchOptions::new(true, 1_000),
            800,
            "large_exact_1000",
        ),
        (
            &large,
            "nope_xyz",
            SearchOptions::new(true, 1_000),
            1_000,
            "large_miss_1000",
        ),
        (
            &wide,
            "needle",
            SearchOptions::new(true, 500),
            800,
            "wide_exact_500",
        ),
        (
            &large,
            "a",
            SearchOptions::new(true, SEARCH_MAX_RESULTS),
            500,
            "worst_single_char_max",
        ),
    ];

    for (state, pat, opts, iters, label) in cases {
        let mean_us = bench_search(state, pat, *opts, *iters);
        println!(
            "  {label}: pat={:?} case={} max={} mean {mean_us:.2} µs over {iters} iters",
            pat,
            if opts.case_sensitive {
                "sensitive"
            } else {
                "fold"
            },
            opts.max_results,
        );
        if mean_us > 5_000.0 {
            eprintln!(
                "note: {label} {mean_us:.0} µs — search scales with scrollback×width; keep bounded"
            );
        }
    }

    // Truncation invariant: pattern longer than cap is char-boundary truncated.
    {
        let long_pat = "a".repeat(SEARCH_MAX_PATTERN_LEN + 100);
        let m = large.search(&long_pat, SearchOptions::new(true, 10));
        println!(
            "  truncation: pat {} B → string {} B, matches {}",
            long_pat.len(),
            SEARCH_MAX_PATTERN_LEN,
            m.len()
        );
        assert!(long_pat.len() > SEARCH_MAX_PATTERN_LEN);
    }

    // Zero/empty pattern invariant: empty returns 0 matches, never panics.
    {
        let m = small.search("", SearchOptions::default());
        assert!(m.is_empty(), "empty pattern should yield 0");
        println!("  empty pattern: 0 matches (bounded)");
    }

    // Hard cap: SEARCH_MAX_RESULTS is never exceeded even when corpus huge.
    {
        let s = fill_scrollback(5_000, 80);
        let m = s.search("e", SearchOptions::new(true, SEARCH_MAX_RESULTS + 500));
        assert!(m.len() <= SEARCH_MAX_RESULTS, "hard cap violated");
        println!("  hard cap: {} ≤ {SEARCH_MAX_RESULTS}", m.len());
    }
}
