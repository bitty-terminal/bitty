//! Damage overflow fallback (CTX-0468, issue #749).
//!
//! `State::damage_since` documents that generations older than the retained
//! window behave as a full-grid redraw request. Before the fix it returned
//! only the retained partial batches (under-paint) and grew the result with
//! a bare `extend_from_slice` with no bound.

#![forbid(unsafe_code)]

use bitty_term_state::{DamageRect, DamagedRegion};
use bitty_term_state::{
    GRID_COLUMNS, GRID_ROWS, State,
    damage::{DAMAGE_HISTORY_BATCHES, DAMAGE_MAX_REGIONS_PER_BATCH},
};
use bitty_vt::{GraphemeCell, TerminalAction};

fn print(state: &mut State, c: char) {
    state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
}

fn full_grid() -> DamagedRegion {
    DamagedRegion::Grid(DamageRect::full(GRID_ROWS as u16, GRID_COLUMNS as u16))
}

#[test]
fn stale_generation_falls_back_to_full_grid() {
    let mut state = State::new();
    // Push past the retained window so generation 0 is evicted.
    let total = DAMAGE_HISTORY_BATCHES + 6;
    for i in 0..total {
        print(&mut state, char::from(b'a' + (i % 26) as u8));
    }
    let regions = state.damage_since(0);
    assert_eq!(
        regions,
        vec![full_grid()],
        "stale generation must request a full-grid redraw, got {regions:?}"
    );
}

#[test]
fn window_boundary_stays_incremental() {
    let mut state = State::new();
    let mut observed: Vec<DamagedRegion> = Vec::new();
    for i in 0..DAMAGE_HISTORY_BATCHES {
        let damage = state.apply(&TerminalAction::Print(GraphemeCell::from(char::from(
            b'a' + (i % 26) as u8,
        ))));
        observed.extend_from_slice(&damage.regions);
    }
    // Exactly a full window is still fully retained: no fallback, exact
    // concatenation, and bounded by the per-batch cap shared with the
    // render-side frame planner.
    let regions = state.damage_since(0);
    assert_eq!(observed, regions);
    assert!(
        regions.len() <= DAMAGE_MAX_REGIONS_PER_BATCH,
        "windowed accumulation must stay bounded, got {}",
        regions.len()
    );
    assert_ne!(
        regions,
        vec![full_grid()],
        "a retained window must not collapse to full-grid"
    );
}

#[test]
fn up_to_date_generation_is_clean() {
    let mut state = State::new();
    print(&mut state, 'x');
    let current = state.generation();
    assert!(state.damage_since(current).is_empty());
    assert!(state.damage_since(current + 1).is_empty());
}

#[test]
fn accumulated_damage_is_bounded() {
    let mut state = State::new();
    // Fill the window; the result must never exceed the shared cap.
    for i in 0..DAMAGE_HISTORY_BATCHES {
        print(&mut state, char::from(b'a' + (i % 26) as u8));
    }
    let regions = state.damage_since(0);
    // Either the exact incremental union (bounded) or the coarse full-grid
    // fallback — never an unbounded concatenation.
    assert!(
        regions.len() <= DAMAGE_MAX_REGIONS_PER_BATCH,
        "damage_since must stay bounded, got {}",
        regions.len()
    );
    if regions.len() > 1 {
        assert_ne!(regions, Vec::<DamagedRegion>::new());
    }
}
