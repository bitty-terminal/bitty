#![forbid(unsafe_code)]
//! M1-08 mode/input golden snapshots per M1 mode (issue #1134, CTX-0571).
//!
//! The accepted Compatibility Milestone RFC requires "Automated integration
//! tests driving a headless terminal state instance per mode (alternate
//! screen, bracketed paste, mouse modes 1000/1002/1003/1006, focus 1004,
//! alternate scroll 1007, 2026, DECSCUSR); golden snapshots committed"
//! (`docs/specifications/compatibility-milestone-rfc.md`, acceptance
//! evidence). Terminal-state RFC replay guarantee 2 makes the canonical
//! `State::state_hash` the machine-checkable form of "same input, identical
//! state", so each fixture under `tests/compat/modes/corpus/` pins that hash
//! plus the semantic mode register and any emitted reply bytes.
//!
//! Oracle: accepted `compatibility-milestone-rfc.md` M1 protocol matrix
//! (mouse 1000/1002/1003/1006, focus 1004, alternate scroll 1007, bracketed
//! paste 2004, synchronized updates 2026, alternate screen 47/1049, cursor
//! shape DECSCUSR) and `terminal-state-rfc.md`. A fixture whose hash or mode
//! register disagreed with the accepted RFC would be a reported defect, not
//! an encoded golden.

use std::path::PathBuf;

use bitty_term_state::{CursorStyle, MouseCoordinateEncoding, MouseTrackingMode, State};
use bitty_vt::Parser;

/// Canonical-hash version this golden binds to (`CANONICAL_HASH_VERSION`).
const HASH_VERSION: u32 = 7;

fn corpus_dir() -> PathBuf {
    bitty_compat_lab::workspace_root().join("tests/compat/modes/corpus")
}

fn read(name: &str) -> Vec<u8> {
    let path = corpus_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

/// Replays one fixture byte-for-byte, asserting determinism and invariants,
/// and returns the parsed bytes plus the final state hash, generation, and
/// drained reply bytes.
fn replay(name: &str) -> (Vec<u8>, u64, u64, Vec<Vec<u8>>) {
    let bytes = read(name);
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(&bytes, |a| actions.push(a));

    let mut state = State::new();
    for action in &actions {
        state.apply(action);
    }
    state
        .check_invariants()
        .unwrap_or_else(|e| panic!("{name}: invariant violation {e:?}"));
    let hash = state.state_hash();
    let generation = state.generation();

    // Determinism: full replay from scratch must reach the same hash.
    let mut parser2 = Parser::new();
    let mut actions2 = Vec::new();
    parser2.advance(&bytes, |a| actions2.push(a));
    let mut state2 = State::new();
    for action in &actions2 {
        state2.apply(action);
    }
    assert_eq!(hash, state2.state_hash(), "{name}: replay hash diverged");

    let replies = state.take_replies().iter().map(|r| r.to_vec()).collect();
    (bytes, hash, generation, replies)
}

/// Replays and returns the final state (determinism already asserted).
fn state_of(name: &str) -> State {
    let bytes = read(name);
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(&bytes, |a| actions.push(a));
    let mut state = State::new();
    for action in &actions {
        state.apply(action);
    }
    state
}

/// Asserts the canonical hash, generation, and reply bytes of one fixture.
fn assert_golden(name: &str, hash: u64, generation: u64, reply_hex: &[&str]) {
    let (bytes, actual_hash, actual_generation, replies) = replay(name);
    assert!(
        bytes.len() <= 8 * 1024,
        "{name}: {} bytes exceeds MAX_CORPUS_BYTES",
        bytes.len()
    );
    assert_eq!(
        actual_hash, hash,
        "{name}: canonical state hash changed (actual 0x{actual_hash:016x}, golden 0x{hash:016x})"
    );
    assert_eq!(actual_generation, generation, "{name}: generation changed");
    let hex: Vec<String> = replies
        .iter()
        .map(|r| r.iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    let expected: Vec<String> = reply_hex.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(hex, expected, "{name}: emitted reply bytes changed");
}

#[test]
fn golden_version_is_pinned() {
    assert_eq!(
        bitty_term_state::canonical_public::CANONICAL_HASH_VERSION,
        HASH_VERSION,
        "M1-08 goldens must be re-recorded when the canonical hash version bumps"
    );
}

#[test]
fn golden_mouse_tracking_modes_set_and_clear() {
    // Each pair locks the set state and the set-then-cleared state.
    assert_golden("18-mouse-x10-9-on.bin", 0x4434_3432_dd16_14c7, 9, &[]);
    assert_golden("18-mouse-x10-9-off.bin", 0x42aa_af5d_5901_16a1, 10, &[]);
    assert_golden("01-mouse-1000-on.bin", 0x1d7f_6da1_49af_58fa, 9, &[]);
    assert_golden("01-mouse-1000-off.bin", 0xd660_51c8_9cec_f61d, 10, &[]);
    assert_golden("02-mouse-1002-on.bin", 0xfe1f_69cc_9f8f_43a6, 9, &[]);
    assert_golden("02-mouse-1002-off.bin", 0xc2df_8ec1_6fc4_4bde, 10, &[]);
    assert_golden("03-mouse-1003-on.bin", 0x3276_efaf_255c_f6be, 9, &[]);
    assert_golden("03-mouse-1003-off.bin", 0x75db_6e0a_76a1_907f, 10, &[]);

    // Semantic register: 1000 normal, 1002 button, 1003 any; cleared = None.
    assert_eq!(
        state_of("01-mouse-1000-on.bin").modes().mouse_tracking,
        Some(MouseTrackingMode::Normal)
    );
    assert_eq!(
        state_of("02-mouse-1002-on.bin").modes().mouse_tracking,
        Some(MouseTrackingMode::Button)
    );
    assert_eq!(
        state_of("03-mouse-1003-on.bin").modes().mouse_tracking,
        Some(MouseTrackingMode::Any)
    );
    // X10 (`?9`) is the legacy pre-1000 tracking level.
    assert_eq!(
        state_of("18-mouse-x10-9-on.bin").modes().mouse_tracking,
        Some(MouseTrackingMode::X10)
    );
    assert_eq!(
        state_of("18-mouse-x10-9-off.bin").modes().mouse_tracking,
        None,
        "DECRST 9 clears X10 tracking"
    );
    for off in [
        "01-mouse-1000-off.bin",
        "02-mouse-1002-off.bin",
        "03-mouse-1003-off.bin",
    ] {
        assert_eq!(
            state_of(off).modes().mouse_tracking,
            None,
            "{off}: DECRST clears mouse tracking"
        );
    }
}

/// xterm `charproc.c` keeps one `send_mouse_pos` slot, so the mouse-tracking
/// protocols are mutually exclusive and the last DECSET wins; any matching
/// DECRST disables reporting outright. This locks that parity (the accepted
/// Input and pointer RFC names xterm as the oracle for the mode on/off
/// matrix) rather than asserting a per-mode flag.
#[test]
fn mouse_tracking_modes_are_one_mutually_exclusive_slot() {
    let parse = |bytes: &[u8]| {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        parser.advance(bytes, |a| actions.push(a));
        let mut state = State::new();
        for action in &actions {
            state.apply(action);
        }
        state
    };
    // Last DECSET wins.
    assert_eq!(
        parse(b"\x1b[?1000h\x1b[?1002h").modes().mouse_tracking,
        Some(MouseTrackingMode::Button)
    );
    // Any DECRST clears the single slot (xterm `enabled ? mode : MOUSE_OFF`).
    assert_eq!(
        parse(b"\x1b[?1003h\x1b[?1000l").modes().mouse_tracking,
        None
    );
    // Coordinate encodings are a separate mutually-exclusive slot: a reset of
    // a non-active encoding leaves the active one intact (CTX-0566).
    assert_eq!(
        parse(b"\x1b[?1006h\x1b[?1015l")
            .modes()
            .mouse_coordinate_encoding,
        Some(MouseCoordinateEncoding::Sgr)
    );
}

#[test]
fn golden_mouse_coordinate_encodings_set_and_clear() {
    // M1 requires SGR 1006; legacy 1005/1015 encodings lock the same path.
    assert_golden("04-mouse-sgr-1006-on.bin", 0x69c2_1c23_a79d_2b44, 10, &[]);
    assert_golden("04-mouse-sgr-1006-off.bin", 0x5e84_4848_a084_5838, 12, &[]);
    assert_golden("06-legacy-1005-on.bin", 0xd653_7d1a_e8a4_6b01, 10, &[]);
    assert_golden("06-legacy-1005-off.bin", 0x025e_361d_521a_8c5a, 12, &[]);
    assert_golden("07-legacy-1015-on.bin", 0xb40e_b4e8_520e_c9ba, 10, &[]);
    assert_golden("07-legacy-1015-off.bin", 0x0a46_0dd1_c39c_29fb, 12, &[]);

    let sgr = state_of("04-mouse-sgr-1006-on.bin");
    assert_eq!(
        sgr.modes().mouse_coordinate_encoding,
        Some(MouseCoordinateEncoding::Sgr)
    );
    assert_eq!(
        sgr.modes().mouse_tracking,
        Some(MouseTrackingMode::Normal),
        "the fixture sets 1000 alongside 1006"
    );
    assert_eq!(
        state_of("06-legacy-1005-on.bin")
            .modes()
            .mouse_coordinate_encoding,
        Some(MouseCoordinateEncoding::Utf8)
    );
    assert_eq!(
        state_of("07-legacy-1015-on.bin")
            .modes()
            .mouse_coordinate_encoding,
        Some(MouseCoordinateEncoding::Urxvt)
    );
    for off in [
        "04-mouse-sgr-1006-off.bin",
        "06-legacy-1005-off.bin",
        "07-legacy-1015-off.bin",
    ] {
        assert_eq!(
            state_of(off).modes().mouse_coordinate_encoding,
            None,
            "{off}: DECRST clears the matching encoding"
        );
    }
}

#[test]
fn golden_bracketed_paste_focus_and_synchronized_update() {
    assert_golden(
        "08-bracketed-paste-2004-on.bin",
        0x379e_da59_2803_77ab,
        9,
        &[],
    );
    assert_golden(
        "08-bracketed-paste-2004-off.bin",
        0x329f_8357_26d8_a644,
        10,
        &[],
    );
    assert_golden("09-focus-1004-on.bin", 0x8c92_5a74_21eb_e410, 9, &[]);
    assert_golden("09-focus-1004-off.bin", 0x1d1f_6100_d72c_bda5, 10, &[]);
    assert_golden("10-sync-2026-on.bin", 0xe9b8_1821_0775_0054, 9, &[]);
    assert_golden("10-sync-2026-off.bin", 0x7876_3cc6_2713_7699, 10, &[]);

    assert!(
        state_of("08-bracketed-paste-2004-on.bin")
            .modes()
            .bracketed_paste
    );
    assert!(
        !state_of("08-bracketed-paste-2004-off.bin")
            .modes()
            .bracketed_paste
    );
    assert!(state_of("09-focus-1004-on.bin").modes().focus_events);
    assert!(!state_of("09-focus-1004-off.bin").modes().focus_events);
    assert!(state_of("10-sync-2026-on.bin").modes().synchronized_update);
    assert!(!state_of("10-sync-2026-off.bin").modes().synchronized_update);
}

#[test]
fn golden_alternate_scroll_and_decckm() {
    assert_golden("05-alt-scroll-1007-on.bin", 0xaa7b_4807_27e8_d4ee, 9, &[]);
    assert_golden("05-alt-scroll-1007-off.bin", 0x3d2d_8577_732e_0159, 10, &[]);
    assert_golden("13-decckm-on.bin", 0x90c5_11f3_179b_cd2d, 9, &[]);
    assert_golden("13-decckm-off.bin", 0x3da6_ed6c_0600_019a, 10, &[]);

    assert!(
        state_of("05-alt-scroll-1007-on.bin")
            .modes()
            .alternate_scroll
    );
    assert!(
        !state_of("05-alt-scroll-1007-off.bin")
            .modes()
            .alternate_scroll
    );
    assert!(state_of("13-decckm-on.bin").modes().application_cursor_keys);
    assert!(
        !state_of("13-decckm-off.bin")
            .modes()
            .application_cursor_keys
    );
}

#[test]
fn golden_alternate_screen_entry_and_exit() {
    assert_golden("11-alt-screen-1049-on.bin", 0x3d69_c915_214a_447a, 9, &[]);
    assert_golden("11-alt-screen-1049-off.bin", 0x443c_e5f9_12d5_ab96, 10, &[]);
    assert_golden("12-alt-screen-47-on.bin", 0xa7aa_bded_3be7_206e, 9, &[]);
    assert_golden("12-alt-screen-47-off.bin", 0xb073_92d9_8b5c_8275, 10, &[]);

    assert!(state_of("11-alt-screen-1049-on.bin").alt_screen_active());
    assert!(!state_of("11-alt-screen-1049-off.bin").alt_screen_active());
    assert!(state_of("12-alt-screen-47-on.bin").alt_screen_active());
    assert!(!state_of("12-alt-screen-47-off.bin").alt_screen_active());
}

/// DECSCUSR: the fixture ends on a **non-default** shape (`CSI 6 SP q`,
/// `SteadyBar`), so the golden binds real `CursorStyle` handling — a no-op
/// would leave the cursor at the power-on `CursorStyle::Default` and change
/// both the canonical hash and this assertion.
#[test]
fn golden_decscusr_cursor_shape() {
    assert_golden("16-decscusr.bin", 0x562a_1fd9_66f6_435c, 42, &[]);
    assert_eq!(
        state_of("16-decscusr.bin").cursor().cursor_style,
        CursorStyle::SteadyBar,
        "the last DECSCUSR shape must reach the live cursor"
    );
}

#[test]
fn golden_mode_transitions_and_set_sweep() {
    // Every M1 mode set then cleared: the register returns to power-on and
    // no M1 private mode remains flagged.
    assert_golden("14-mode-transitions.bin", 0xe401_d27a_7a7d_d402, 34, &[]);
    let cleared = state_of("14-mode-transitions.bin");
    let m = cleared.modes();
    assert_eq!(m.mouse_tracking, None);
    assert_eq!(m.mouse_coordinate_encoding, None);
    assert!(!m.alternate_scroll);
    assert!(!m.bracketed_paste);
    assert!(!m.focus_events);
    assert!(!m.synchronized_update);
    assert!(!m.application_cursor_keys);
    assert!(!cleared.alt_screen_active());
    assert!(m.auto_wrap, "DECAWM stays at its default");
    assert!(!m.insert);
    assert!(!m.line_feed_new_line);

    // Every M1 mode left set: all flags present at once.
    assert_golden("15-mode-set-sweep.bin", 0x23a3_1f5c_af5c_9803, 21, &[]);
    let set = state_of("15-mode-set-sweep.bin");
    let m = set.modes();
    assert_eq!(m.mouse_tracking, Some(MouseTrackingMode::Any));
    assert_eq!(
        m.mouse_coordinate_encoding,
        Some(MouseCoordinateEncoding::Urxvt)
    );
    assert!(m.alternate_scroll);
    assert!(m.bracketed_paste);
    assert!(m.focus_events);
    assert!(m.synchronized_update);
    assert!(m.application_cursor_keys);
    assert!(
        !set.alt_screen_active(),
        "the sweep does not enter alt screen"
    );
}

#[test]
fn golden_term_state_emitted_reply_bytes() {
    // DSR 6 -> CSI row;col R and DA1 -> CSI ? 6 c, queued by State (no I/O).
    assert_golden(
        "17-mode-status-reply.bin",
        0xb98c_4138_b824_650e,
        6,
        &["1b5b333b3752", "1b5b3f3663"],
    );
    let state = state_of("17-mode-status-reply.bin");
    assert_eq!(
        state.modes().mouse_tracking,
        Some(MouseTrackingMode::Normal)
    );
    assert!(state.modes().bracketed_paste);
    assert!(state.modes().application_cursor_keys);
}
