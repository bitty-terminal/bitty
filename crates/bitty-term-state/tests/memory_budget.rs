//! Terminal-state memory-budget gate for issue #1809 (CTX-1026).
//!
//! Pins the heap Bitty's terminal core retains per panel so structural
//! bloat (a wider [`Cell`], a raised scrollback default, a bigger grid,
//! an over-allocating container) fails CI instead of regressing silently:
//!
//! - [`Cell`] stays at most 64 bytes (measured 60: 4 glyph + 20 style +
//!   width/spacer/hyperlink + 24 inline combining buffer);
//! - an idle 80x24 state (primary + alt grids, empty scrollback) measures
//!   its retained heap via [`State::retained_heap_bytes`] and stays under
//!   1 MiB of owned heap;
//! - a full default scrollback (10 000 x 80 cells, populated through the
//!   public restore path) measures its retained heap the same way and
//!   stays under 56 MiB, inside the per-panel share of the `<150 MB RSS`
//!   idle budget;
//! - the documented caps stay pinned (raising any of them must update
//!   this gate together, which is the review tripwire).
//!
//! Both heap gates measure a live [`State`] (capacities plus boxed lines),
//! never constants alone: a new owned container or an over-capacity
//! allocation trips them.
//!
//! GPU/driver mappings own the rest of the reported RSS/VSZ floor and are
//! out of scope here; see the render-side gate for the font/atlas/cache
//! lines.

use std::mem::size_of;

use bitty_term_state::{
    Cell, GRID_COLUMNS, GRID_ROWS, MAX_GRID_DIM, SCROLLBACK_DEFAULT_LINES, SCROLLBACK_MAX_LINES,
    State,
};

/// Per-panel share of the issue #1809 `<150 MB RSS` idle budget assigned to
/// terminal core heap (grids + full default scrollback + tables).
const CORE_HEAP_BUDGET_BYTES: usize = 56 * 1024 * 1024;

/// Idle-state owned-heap ceiling (two grids plus empty containers).
const IDLE_HEAP_BUDGET_BYTES: usize = 1024 * 1024;

#[test]
fn cell_stays_within_byte_budget() {
    let cell = size_of::<Cell>();
    assert!(
        cell <= 64,
        "Cell grew to {cell} bytes; the scrollback math below assumes <= 64"
    );
}

#[test]
fn grid_and_caps_stay_pinned() {
    assert_eq!((GRID_COLUMNS, GRID_ROWS), (80, 24));
    assert_eq!(SCROLLBACK_DEFAULT_LINES, 10_000);
    assert_eq!(SCROLLBACK_MAX_LINES, 100_000);
    assert_eq!(MAX_GRID_DIM, 1000);
}

#[test]
fn idle_state_heap_stays_under_ceiling() {
    let state = State::new();
    assert_eq!(state.scrollback_len(), 0);
    // Measured, not derived: capacities of the two resident screens plus
    // every other owned container. Empty VecDeques allocate nothing, so
    // this is dominated by the grids.
    let idle_heap = state.retained_heap_bytes();
    assert!(
        idle_heap <= IDLE_HEAP_BUDGET_BYTES,
        "idle heap {idle_heap} must stay under {IDLE_HEAP_BUDGET_BYTES}"
    );
    // Sanity: the idle heap is kilobytes, not megabytes.
    assert!(
        idle_heap < 512 * 1024,
        "idle heap {idle_heap} exceeds 512 KiB"
    );
    // Lower bound: the two grids must actually be measured. A zero (or
    // near-zero) reading means the gauge went blind, not that memory
    // shrank.
    let min_grids = 2 * state.width() * state.height() * size_of::<Cell>();
    assert!(
        idle_heap >= min_grids,
        "idle heap {idle_heap} below grid floor {min_grids}; the gauge must measure State"
    );
}

#[test]
fn full_default_scrollback_stays_within_panel_share() {
    // Worst retained case at default settings, populated through the
    // public restore path: every default line full width. Measured via
    // `retained_heap_bytes` (ring capacity plus every boxed line), so a
    // wider Cell, a deeper default, or an over-allocating buffer trips
    // the gate.
    let mut state = State::new();
    let full_line = "x".repeat(GRID_COLUMNS);
    let lines: Vec<String> = (0..SCROLLBACK_DEFAULT_LINES)
        .map(|_| full_line.clone())
        .collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let pushed = state.restore_scrollback_text(&refs);
    assert_eq!(pushed, SCROLLBACK_DEFAULT_LINES);
    assert_eq!(state.scrollback_len(), SCROLLBACK_DEFAULT_LINES);
    let heap = state.retained_heap_bytes();
    assert!(
        heap <= CORE_HEAP_BUDGET_BYTES,
        "default-full core heap {heap} must stay under {CORE_HEAP_BUDGET_BYTES}"
    );
    // Lower bound: the full scrollback must actually be retained. A
    // reading below the cell floor means the gauge missed the buffer.
    let min_scrollback = SCROLLBACK_DEFAULT_LINES * GRID_COLUMNS * size_of::<Cell>();
    assert!(
        heap >= min_scrollback,
        "default-full heap {heap} below scrollback floor {min_scrollback}"
    );
}
