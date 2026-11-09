//! xterm `modifyOtherKeys` negotiation and encoding, end to end over the
//! headless `Runtime` (CTX-1756).
//!
//! Acceptance shape:
//!
//! - `CSI > 4 ; Ps m` sets the level, `CSI > 4 m` resets it to `0`, and
//!   `CSI ? 4 m` replies `CSI > 4 ; Ps m` with the live level;
//! - an out-of-range level is ignored (inert unknown-CSI telemetry), never
//!   clamped into a different level;
//! - the level reaches the encoder through the focused pane's mode register
//!   and the runtime mirror (including the inspect modifier snapshot);
//! - `Shift+Enter` becomes `CSI 27 ; 2 ; 13 ~` instead of the bare `\r` that
//!   submits a TUI prompt;
//! - the protocol precedence is Kitty flags, then the xterm level, then the
//!   legacy bytes, and the legacy bytes stay byte-identical when neither is
//!   negotiated.

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn named_key(named: NamedKey, state: PressState) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn char_key(logical: &str, text: Option<&str>) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn replies(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

/// Presses `modifier` and returns the encoding of `key` while it is held.
fn encode_with_modifier(rt: &mut Runtime, modifier: NamedKey, key: &KeyEvent) -> Option<Vec<u8>> {
    rt.handle_key_event(named_key(modifier, PressState::Pressed));
    let encoded = rt.handle_key_event_ref(key);
    rt.handle_key_event(named_key(modifier, PressState::Released));
    encoded
}

#[test]
fn negotiation_sets_resets_and_queries_the_level() {
    let mut rt = make_runtime();
    assert_eq!(rt.modify_other_keys_level(), 0, "default is off");

    rt.handle_pty_bytes(b"\x1b[>4;1m");
    assert_eq!(rt.modify_other_keys_level(), 1);

    rt.handle_pty_bytes(b"\x1b[>4;2m");
    assert_eq!(rt.modify_other_keys_level(), 2);

    rt.handle_pty_bytes(b"\x1b[?4m");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b[>4;2m".to_vec()],
        "CSI ? 4 m replies with the live level in XTMODKEYS form"
    );

    rt.handle_pty_bytes(b"\x1b[>4m");
    assert_eq!(rt.modify_other_keys_level(), 0, "CSI > 4 m resets to 0");

    rt.handle_pty_bytes(b"\x1b[?4m");
    assert_eq!(replies(&mut rt), vec![b"\x1b[>4;0m".to_vec()]);
}

#[test]
fn out_of_range_level_is_ignored_not_clamped() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>4;2m");
    // xterm level 3 is not implemented; 4+ is not a level at all.
    rt.handle_pty_bytes(b"\x1b[>4;3m\x1b[>4;9m");
    assert_eq!(rt.modify_other_keys_level(), 2, "level must not change");
    rt.handle_pty_bytes(b"\x1b[?4m");
    assert_eq!(replies(&mut rt), vec![b"\x1b[>4;2m".to_vec()]);
}

#[test]
fn inspect_snapshot_carries_the_negotiated_level() {
    // State -> focused-pane mirror -> inspect modifier snapshot.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>4;1m");
    assert_eq!(rt.modify_other_keys_level(), 1);
    let snapshot = rt.inspect_modifier_snapshot();
    assert_eq!(snapshot.modify_other_keys_level, 1);
    assert_eq!(snapshot.enhanced_keyboard_flags, 0);
}

#[test]
fn shift_enter_inserts_a_newline_under_level_one() {
    // The exact TUI regression: `CSI > 4 ; 2 m`-style negotiation must stop
    // Shift+Enter from arriving as a bare `\r` (submit).
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>4;1m");
    let enter = named_key(NamedKey::Enter, PressState::Pressed);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Shift, &enter),
        Some(b"\x1b[27;2;13~".to_vec())
    );
    // Level 2 applies every modifier to `Enter` too, so the bytes are the
    // same; the difference is that level 2 also escape-codes plain text.
    rt.handle_pty_bytes(b"\x1b[>4;2m");
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Shift, &enter),
        Some(b"\x1b[27;2;13~".to_vec())
    );
}

#[test]
fn ctrl_a_under_level_two_is_the_xterm_form() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>4;2m");
    let ctrl_a = char_key("a", None);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Control, &ctrl_a),
        Some(b"\x1b[27;5;97~".to_vec())
    );
}

#[test]
fn kitty_flags_win_over_the_xterm_level() {
    // Kitty dropped the xterm option; a non-zero flag register takes
    // precedence, so Shift+Enter is `CSI 13 ; 2 u` even with level 2 active.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>4;2m\x1b[=1u");
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
    assert_eq!(rt.modify_other_keys_level(), 2);
    let enter = named_key(NamedKey::Enter, PressState::Pressed);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Shift, &enter),
        Some(b"\x1b[13;2u".to_vec()),
        "the Kitty disambiguate form wins over modifyOtherKeys"
    );
}

#[test]
fn legacy_bytes_are_unchanged_when_neither_protocol_is_negotiated() {
    // Differential proof: with flags 0 and level 0 the encoder is the legacy
    // one, byte for byte, for the keys the new path would otherwise claim.
    let mut rt = make_runtime();
    assert_eq!(rt.enhanced_keyboard_flags(), 0);
    assert_eq!(rt.modify_other_keys_level(), 0);

    let enter = named_key(NamedKey::Enter, PressState::Pressed);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Shift, &enter),
        Some(b"\x1b[27;2;13~".to_vec()),
        "legacy Shift+Enter emits CSI 27;2;13~"
    );
    let ctrl_a = char_key("a", None);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Control, &ctrl_a),
        Some(vec![0x01])
    );
    let up = named_key(NamedKey::ArrowUp, PressState::Pressed);
    assert_eq!(
        encode_with_modifier(&mut rt, NamedKey::Shift, &up),
        Some(b"\x1b[A".to_vec())
    );
}
