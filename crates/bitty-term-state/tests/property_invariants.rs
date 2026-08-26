//! Property-based invariant testing (RFC "Property-based and replay
//! tests").
//!
//! Random action sequences are applied to a fresh [`State`]; after every
//! action all RFC "Grid and state invariants" must hold and every
//! resource bound must be respected. Debug builds additionally assert the
//! same invariants inside `State::apply` itself.

#![forbid(unsafe_code)]

mod common;

use proptest::prelude::*;

use bitty_term_state::{
    GRID_COLUMNS, GRID_ROWS, REPLY_CAP_BYTES, SCROLLBACK_MAX_LINES, SNAPSHOT_VERSION, State,
    ZONE_RECORDS_MAX,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn invariants_hold_under_random_action_sequences(
        actions in prop::collection::vec(common::arb_action(), 0..150),
    ) {
        let mut state = State::new();
        for action in &actions {
            state.apply(action);
            let verdict = state.check_invariants();
            prop_assert!(
                verdict.is_ok(),
                "invariant violated after {:?}: {:?}",
                action,
                verdict
            );
            prop_assert_eq!(state.width(), GRID_COLUMNS);
            prop_assert_eq!(state.height(), GRID_ROWS);
            prop_assert!(state.scrollback_len() <= SCROLLBACK_MAX_LINES);
            prop_assert!(state
                .scrollback()
                .all(|line| line.cells.len() == GRID_COLUMNS));
            prop_assert!(state.zones().count() <= ZONE_RECORDS_MAX);
        }
    }

    #[test]
    fn generation_advances_once_per_action(
        actions in prop::collection::vec(common::arb_action(), 1..60),
    ) {
        let mut state = State::new();
        for (expected_generation, action) in actions.iter().enumerate() {
            let damage = state.apply(action);
            prop_assert_eq!(damage.generation as usize, expected_generation + 1);
            prop_assert_eq!(state.generation(), damage.generation);
        }
    }

    #[test]
    fn snapshots_are_versioned_and_sized(
        actions in prop::collection::vec(common::arb_action(), 0..80),
    ) {
        let mut state = State::new();
        for action in &actions {
            state.apply(action);
        }
        let snapshot = state.snapshot();
        prop_assert_eq!(snapshot.version, SNAPSHOT_VERSION);
        prop_assert_eq!(snapshot.cells.len(), GRID_COLUMNS * GRID_ROWS);
        prop_assert_eq!(snapshot.generation, state.generation());
        prop_assert_eq!(
            snapshot.cursor.position.row,
            state.cursor().position.row
        );
        prop_assert_eq!(snapshot.modes.origin, state.modes().origin);
    }

    #[test]
    fn reply_budget_is_respected_under_pressure(
        sizes in prop::collection::vec(1_usize..REPLY_CAP_BYTES / 2, 0..24),
    ) {
        let mut state = State::new();
        let mut queued_total = 0usize;
        let mut expect_overflow = false;
        for size in &sizes {
            let payload: Box<[u8]> = vec![0x42u8; *size].into_boxed_slice();
            if queued_total + payload.len() <= REPLY_CAP_BYTES {
                queued_total += payload.len();
            } else {
                expect_overflow = true;
            }
            state.apply(&bitty_vt::TerminalAction::Reply { bytes: payload });
            prop_assert_eq!(state.replies_overflowed(), expect_overflow);
            let drained = state.take_replies();
            prop_assert_eq!(drained.iter().map(|reply| reply.len()).sum::<usize>(), queued_total);
            // Draining resets both queue and flag.
            queued_total = 0;
            expect_overflow = false;
            prop_assert!(!state.replies_overflowed());
        }
    }
}
