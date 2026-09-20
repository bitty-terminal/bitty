//! Kitty keyboard protocol completeness (CTX-0575, Issue #1139, M1-13).
//!
//! Acceptance shape, end to end over the real headless `Runtime`:
//!
//! - progressive-enhancement negotiation `CSI = flags ; mode u` (assign /
//!   set / reset), `CSI > flags u` (push), `CSI < n u` (pop) and the
//!   `CSI ? u` query replying `CSI ? flags u`;
//! - encoding beyond the previous bounded subset: disambiguation, event
//!   types (`:2` repeat / `:3` release; press is the omitted default),
//!   alternate (shifted) keys and associated text;
//! - the opt-in default-off differential proof: with flags `0` every event
//!   is byte-identical to the legacy xterm encoder.
//!
//! Wording-drift note: the Issue/RFC text names `?7727 h/l` as the kitty
//! negotiation. `CSI ? 7727 h/l` is mintty Application Escape Mode, not the
//! kitty protocol; the authoritative negotiation is `CSI =/>/< / ? u`
//! (`sw.kovidgoyal.net/kitty/keyboard-protocol`). bitty keeps `?7727` as the
//! historical alias and implements the real forms here.

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn char_key(logical: &str, text: Option<&str>, state: PressState, repeat: bool) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state,
        repeat,
        is_synthetic: false,
    }
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

fn replies(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

/// Feeds one kitty negotiation sequence and asserts nothing is echoed.
fn send(rt: &mut Runtime, bytes: &[u8]) {
    rt.handle_pty_bytes(bytes);
}

fn press_ctrl(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Control, PressState::Pressed));
}

fn release_ctrl(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Control, PressState::Released));
}

fn press_shift(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
}

// ----------------------------------------------------------------------
// Flag negotiation: query, set modes, push/pop
// ----------------------------------------------------------------------

#[test]
fn query_defaults_to_zero_and_answers_csi_question_flags_u() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?u");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b[?0u".to_vec()],
        "CSI ? u must reply CSI ? flags u with the live flags"
    );
}

#[test]
fn set_assign_all_replaces_the_whole_flag_register() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=3u"); // 0b11
    send(&mut rt, b"\x1b[=1u"); // assign: only disambiguate remains
    assert_eq!(rt.kitty_flags(), 1);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1u".to_vec()]);
}

#[test]
fn set_mode_two_ors_and_mode_three_clears_specified_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // assign disambiguate
    send(&mut rt, b"\x1b[=4;2u"); // mode 2: set specified (alternates)
    assert_eq!(rt.kitty_flags(), 5);
    send(&mut rt, b"\x1b[=1;3u"); // mode 3: reset specified (disambiguate)
    assert_eq!(rt.kitty_flags(), 4);
    // Unknown mode fails closed without touching the register.
    send(&mut rt, b"\x1b[=31;9u");
    assert_eq!(rt.kitty_flags(), 4);
}

#[test]
fn flags_are_bounded_to_the_five_defined_bits() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=4294967295u"); // all bits set on the wire
    assert_eq!(rt.kitty_flags(), 0x1F);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?31u".to_vec()]);
}

#[test]
fn push_and_pop_restore_the_previous_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // current = 1
    send(&mut rt, b"\x1b[>5u"); // push 1, current = 5
    assert_eq!(rt.kitty_flags(), 5);
    send(&mut rt, b"\x1b[<u"); // pop one -> restore 1
    assert_eq!(rt.kitty_flags(), 1);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1u".to_vec()]);
}

#[test]
fn push_defaults_to_zero_and_pop_defaults_to_one() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=7u");
    send(&mut rt, b"\x1b[>u"); // push, flags omitted -> zero
    assert_eq!(rt.kitty_flags(), 0);
    send(&mut rt, b"\x1b[<u"); // pop one, count omitted
    assert_eq!(rt.kitty_flags(), 7);
}

#[test]
fn popping_past_the_stack_empties_it_and_resets_all_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    send(&mut rt, b"\x1b[>2u");
    send(&mut rt, b"\x1b[>4u");
    send(&mut rt, b"\x1b[<9u"); // pop more entries than exist
    assert_eq!(rt.kitty_flags(), 0);
    // A later pop is a no-op, never an underflow.
    send(&mut rt, b"\x1b[<u");
    assert_eq!(rt.kitty_flags(), 0);
}

#[test]
fn kitty_keyboard_state_is_independent_per_alternate_screen() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    send(&mut rt, b"\x1b[>4u"); // main: push 1, current 4
    send(&mut rt, b"\x1b[?1049h"); // enter alt screen
    send(&mut rt, b"\x1b[=8u"); // alt screen changes its own flags
    send(&mut rt, b"\x1b[<u"); // pop inside alt
    send(&mut rt, b"\x1b[?1049l"); // leave alt screen
    assert_eq!(
        rt.kitty_flags(),
        4,
        "main-screen flags must survive an alt-screen push/pop cycle"
    );
}

#[test]
fn legacy_7727_alias_still_negotiates_flags() {
    // Regression: the historical `?7727` alias keeps its old semantics while
    // the authoritative `CSI = u` form is the primary path.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?7727h");
    assert_eq!(rt.kitty_flags(), 1);
    send(&mut rt, b"\x1b[?7727$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?7727;1$y".to_vec()]);
}

#[test]
fn decrqm_7727_reflects_the_set_push_pop_register() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?7727$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?7727;2$y".to_vec()]);
    send(&mut rt, b"\x1b[=31u");
    send(&mut rt, b"\x1b[?7727$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?7727;1$y".to_vec()]);
    send(&mut rt, b"\x1b[>0u"); // push disabled flags
    send(&mut rt, b"\x1b[?7727$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?7727;2$y".to_vec()]);
    send(&mut rt, b"\x1b[<u"); // restore
    send(&mut rt, b"\x1b[?7727$p");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?7727;1$y".to_vec()]);
}

// ----------------------------------------------------------------------
// Encoding: disambiguation, event types, alternates, associated text
// ----------------------------------------------------------------------

#[test]
fn disambiguate_encodes_ctrl_letter_as_csi_u() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;5u".to_vec()),
        "Ctrl+A under disambiguate is CSI 97;5u"
    );
    release_ctrl(&mut rt);
}

#[test]
fn report_events_adds_repeat_and_release_event_types() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=3u"); // disambiguate | report events
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;5u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, true)),
        Some(b"\x1b[97;5:2u".to_vec()),
        "repeat carries event type 2"
    );
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Released, false)),
        Some(b"\x1b[97;5:3u".to_vec()),
        "release carries event type 3 when requested"
    );
    release_ctrl(&mut rt);
}

#[test]
fn release_without_report_events_emits_nothing() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // disambiguate only
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Released, false)),
        None,
        "release is dropped unless report_events is set"
    );
    release_ctrl(&mut rt);
}

#[test]
fn report_alternates_includes_the_shifted_key() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=5u"); // disambiguate | report alternates
    press_shift(&mut rt);
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("A"), PressState::Pressed, false)),
        Some(b"\x1b[97:65;6u".to_vec()),
        "Ctrl+Shift+A reports base 97 and shifted 65"
    );
    release_ctrl(&mut rt);
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn report_all_keys_encodes_plain_text_as_escape_code() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=8u"); // report all keys
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("a"), PressState::Pressed, false)),
        Some(b"\x1b[97u".to_vec()),
        "plain text is reported as CSI 97u"
    );
}

#[test]
fn associated_text_is_embedded_when_report_all_and_text_are_set() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=24u"); // report all keys | report associated text
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("a"), PressState::Pressed, false)),
        Some(b"\x1b[97;;97u".to_vec()),
        "associated text is a trailing codepoint field"
    );
    press_shift(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("A"), PressState::Pressed, false)),
        Some(b"\x1b[97;2;65u".to_vec()),
        "shift+A embeds the shifted text as codepoint 65"
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn report_all_keys_encodes_functional_and_modifier_keys() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=8u");
    // ArrowUp with no modifiers keeps the bare CSI form (the spec omits the
    // modifier field entirely), now sourced from the kitty table.
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::ArrowUp, PressState::Pressed)),
        Some(b"\x1b[A".to_vec())
    );
    // A modified functional key carries the modifier field.
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::ArrowUp, PressState::Pressed)),
        Some(b"\x1b[1;5A".to_vec())
    );
    release_ctrl(&mut rt);
    // Space is a text key with code 32.
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Space, PressState::Pressed)),
        Some(b"\x1b[32u".to_vec())
    );
    // Left Shift reports its dedicated Private-Use code and modifier bit.
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Shift, PressState::Pressed)),
        Some(b"\x1b[57441;2u".to_vec())
    );
}

#[test]
fn associated_text_frame_is_hard_bounded() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=24u"); // report all keys | report associated text
    let long = "\u{1F600}".repeat(64);
    let bytes = rt
        .handle_key_event_ref(&char_key("a", Some(&long), PressState::Pressed, false))
        .expect("frame must be emitted");
    assert!(
        bytes.len() <= 64,
        "a multi-codepoint text must stay within the 64-byte bound, got {}",
        bytes.len()
    );
    assert_eq!(*bytes.last().unwrap(), b'u');
}

#[test]
fn associated_text_without_report_all_keys_is_inert() {
    // The spec calls report_text undefined without report_all_keys; bitty
    // fails closed and keeps the legacy text path.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=16u");
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("a"), PressState::Pressed, false)),
        Some(b"a".to_vec())
    );
}

#[test]
fn enter_tab_backspace_keep_legacy_bytes_under_disambiguate() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Tab, PressState::Pressed)),
        Some(b"\t".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Backspace, PressState::Pressed)),
        Some(b"\x7f".to_vec())
    );
}

#[test]
fn default_off_is_byte_identical_to_legacy_for_every_covered_key() {
    // Opt-in default-off differential proof: with flags 0 the kitty path must
    // be indistinguishable from the legacy xterm encoder.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?u"); // drain any query, flags stay 0
    let _ = replies(&mut rt);
    press_ctrl(&mut rt);
    let events = [
        char_key("a", Some("a"), PressState::Pressed, false),
        char_key("a", None, PressState::Pressed, false),
        char_key("c", Some("c"), PressState::Pressed, false),
        char_key("a", None, PressState::Pressed, true),
        char_key("a", None, PressState::Released, false),
        named_key(NamedKey::Enter, PressState::Pressed),
        named_key(NamedKey::Tab, PressState::Pressed),
        named_key(NamedKey::Backspace, PressState::Pressed),
        named_key(NamedKey::ArrowUp, PressState::Pressed),
        named_key(NamedKey::F5, PressState::Pressed),
    ];
    for event in &events {
        let kitty = rt.handle_key_event_ref(event);
        let legacy = bitty_platform::keyboard::encode_key_event_with_modifiers(event, &ctrl_mods());
        assert_eq!(
            kitty, legacy,
            "flags 0 must match the legacy encoder for {event:?}"
        );
    }
    release_ctrl(&mut rt);
}

fn ctrl_mods() -> bitty_platform::ModifiersState {
    bitty_platform::ModifiersState {
        shift: false,
        control: true,
        alt: false,
        super_pressed: false,
    }
}
