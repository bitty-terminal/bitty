//! Integration: drive the real `bitty-vt` parser over its seed fixtures
//! and through this crate's state machine (RFC "Fuzzing" clause: parser +
//! state as one harness; totality means no panic and all eight invariants
//! hold after every action).

#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;

use bitty_term_state::{InvariantViolation, State};

fn seeds_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bitty-vt/seeds")
}

fn sorted_seeds() -> Vec<(String, Vec<u8>)> {
    let mut seeds: Vec<(String, Vec<u8>)> = fs::read_dir(seeds_dir())
        .expect("bitty-vt seeds directory must exist")
        .map(|entry| entry.expect("readable seed entry").path())
        .filter(|path| path.is_file())
        .map(|path| {
            let name = path
                .file_name()
                .expect("named seed")
                .to_string_lossy()
                .into_owned();
            (name, fs::read(&path).expect("readable seed bytes"))
        })
        .collect();
    seeds.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!seeds.is_empty(), "seed corpus must not be empty");
    seeds
}

#[test]
fn every_seed_applies_totally_with_invariants_holding() {
    for (name, bytes) in sorted_seeds() {
        let mut parser = bitty_vt::Parser::new();
        let mut actions = Vec::new();
        parser.advance(&bytes, |action| actions.push(action));

        let mut state = State::new();
        for action in &actions {
            let _damage = state.apply(action);
            if let Err(violation) = state.check_invariants() {
                panic!("seed {name}: invariant violated after {action:?}: {violation:?}");
            }
        }
        // Sanity: the state machine consumed everything without I/O or
        // panic, and produced a well-formed snapshot.
        let snapshot = state.snapshot();
        assert_eq!(snapshot.version, bitty_term_state::SNAPSHOT_VERSION);
        assert_eq!(snapshot.cells.len(), state.width() * state.height());
    }
}

#[test]
fn every_seed_replays_with_identical_streams_and_hash() {
    for (name, bytes) in sorted_seeds() {
        let mut parser = bitty_vt::Parser::new();
        let mut actions = Vec::new();
        parser.advance(&bytes, |action| actions.push(action));

        let parse_twice = {
            let mut second_parser = bitty_vt::Parser::new();
            let mut second_actions = Vec::new();
            second_parser.advance(&bytes, |action| second_actions.push(action));
            second_actions
        };
        assert_eq!(actions, parse_twice, "seed {name}: parser replay diverged");

        let run = || {
            let mut state = State::new();
            let mut stream = Vec::new();
            for action in &actions {
                let damage = state.apply(action);
                stream.extend_from_slice(&common_damage_bytes(&damage));
            }
            (stream, state.state_hash())
        };
        let (stream_a, hash_a) = run();
        let (stream_b, hash_b) = run();
        assert_eq!(stream_a, stream_b, "seed {name}: damage streams diverged");
        assert_eq!(hash_a, hash_b, "seed {name}: hashes diverged");
    }
}

#[test]
fn malformed_seed_reports_no_invariant_violations_midstream() {
    // 11-malformed-resync.bin is the adversarial fixture: it must apply
    // totally, and any violation must name an RFC invariant.
    let target = sorted_seeds()
        .into_iter()
        .find(|(name, _)| name.starts_with("11-malformed"))
        .expect("malformed-resync fixture present");
    let (name, bytes) = target;
    let mut parser = bitty_vt::Parser::new();
    let mut actions = Vec::new();
    parser.advance(&bytes, |action| actions.push(action));
    let mut state = State::new();
    for action in actions {
        state.apply(&action);
        match state.check_invariants() {
            Ok(()) => {}
            Err(InvariantViolation::CursorOutOfBounds { .. }) => {
                panic!("{name}: cursor escaped screen bounds");
            }
            Err(other) => panic!("{name}: {other:?}"),
        }
    }
}

/// Minimal deterministic little-endian damage encoding (test-local copy of
/// the shared helper to keep this file self-contained).
fn common_damage_bytes(damage: &bitty_term_state::Damage) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + damage.regions.len() * 12);
    out.extend_from_slice(&damage.generation.to_le_bytes());
    out.extend_from_slice(&(damage.regions.len() as u32).to_le_bytes());
    for region in &damage.regions {
        match *region {
            bitty_term_state::DamagedRegion::Grid(rect) => {
                out.push(1);
                out.extend_from_slice(&rect.top.to_le_bytes());
                out.extend_from_slice(&rect.left.to_le_bytes());
                out.extend_from_slice(&rect.bottom.to_le_bytes());
                out.extend_from_slice(&rect.right.to_le_bytes());
            }
            bitty_term_state::DamagedRegion::Scrollback {
                first_line_id,
                count,
            } => {
                out.push(2);
                out.extend_from_slice(&first_line_id.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
        }
    }
    out
}
