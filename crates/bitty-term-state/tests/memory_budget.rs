//! Terminal-state memory-budget gate for issue #1809 (CTX-1026).
//!
//! Pins the heap Bitty's terminal core retains per panel so structural
//! bloat (a wider [`Cell`], a raised scrollback default, a bigger grid)
//! fails CI instead of regressing silently:
//!
//! - [`Cell`] stays at most 64 bytes (measured 60: 4 glyph + 20 style +
//!   width/spacer/hyperlink + 24 inline combining buffer);
//! - an idle 80x24 state (primary + alt grids, empty scrollback) stays
//!   under 1 MiB of owned heap;
//! - a full default scrollback (10 000 x 80 cells) stays under 56 MiB,
//!   inside the per-panel share of the `<150 MB RSS` idle budget;
//! - the documented caps stay pinned (raising any of them must update
//!   this gate together, which is the review tripwire).
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
    // Two resident screens (primary + alt) plus wrap flags; every other
    // container starts empty (VecDeque::new allocates nothing).
    let grid_bytes = 2 * state.width() * state.height() * size_of::<Cell>();
    let wraps_bytes = 2 * state.height();
    let idle_heap = grid_bytes + wraps_bytes;
    assert!(
        idle_heap <= IDLE_HEAP_BUDGET_BYTES,
        "idle heap {idle_heap} must stay under {IDLE_HEAP_BUDGET_BYTES}"
    );
    // Sanity: the idle heap is kilobytes, not megabytes.
    assert!(
        idle_heap < 512 * 1024,
        "idle heap {idle_heap} exceeds 512 KiB"
    );
}

#[test]
fn full_default_scrollback_stays_within_panel_share() {
    // Worst retained case at default settings: every default line full
    // width, plus one boxed slice allocation each (two words of bookkeeping
    // per line, negligible next to the cells).
    let line_bytes = GRID_COLUMNS * size_of::<Cell>();
    let full_scrollback = SCROLLBACK_DEFAULT_LINES * line_bytes;
    let grid_bytes = 2 * GRID_COLUMNS * GRID_ROWS * size_of::<Cell>();
    let worst_core = full_scrollback + grid_bytes;
    assert!(
        worst_core <= CORE_HEAP_BUDGET_BYTES,
        "default-full core heap {worst_core} must stay under {CORE_HEAP_BUDGET_BYTES}"
    );
}
