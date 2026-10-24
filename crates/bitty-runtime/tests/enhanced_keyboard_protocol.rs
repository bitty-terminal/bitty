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

use bitty_platform::{
    KeyEvent, KeyLocation, LogicalKey, ModifiersState, NamedKey, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
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

fn numpad_char_key(logical: &str, text: Option<&str>) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Numpad,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn numpad_named_key(named: NamedKey) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: None,
        location: KeyLocation::Numpad,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn press_super(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Pressed));
}

fn press_shift(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
}

fn release_super(rt: &mut Runtime) {
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
}

fn hold_modifiers(rt: &mut Runtime, modifiers: &[NamedKey]) {
    for &modifier in modifiers {
        rt.handle_key_event(named_key(modifier, PressState::Pressed));
    }
}

fn release_modifiers(rt: &mut Runtime, modifiers: &[NamedKey]) {
    for &modifier in modifiers.iter().rev() {
        rt.handle_key_event(named_key(modifier, PressState::Released));
    }
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
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1u".to_vec()]);
}

#[test]
fn set_mode_two_ors_and_mode_three_clears_specified_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // assign disambiguate
    send(&mut rt, b"\x1b[=4;2u"); // mode 2: set specified (alternates)
    assert_eq!(rt.enhanced_keyboard_flags(), 5);
    send(&mut rt, b"\x1b[=1;3u"); // mode 3: reset specified (disambiguate)
    assert_eq!(rt.enhanced_keyboard_flags(), 4);
    // Unknown mode fails closed without touching the register.
    send(&mut rt, b"\x1b[=31;9u");
    assert_eq!(rt.enhanced_keyboard_flags(), 4);
}

#[test]
fn flags_are_bounded_to_the_five_defined_bits() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=4294967295u"); // all bits set on the wire
    assert_eq!(rt.enhanced_keyboard_flags(), 0x1F);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?31u".to_vec()]);
}

#[test]
fn push_and_pop_restore_the_previous_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // current = 1
    send(&mut rt, b"\x1b[>5u"); // push 1, current = 5
    assert_eq!(rt.enhanced_keyboard_flags(), 5);
    send(&mut rt, b"\x1b[<u"); // pop one -> restore 1
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?1u".to_vec()]);
}

#[test]
fn push_defaults_to_zero_and_pop_defaults_to_one() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=7u");
    send(&mut rt, b"\x1b[>u"); // push, flags omitted -> zero
    assert_eq!(rt.enhanced_keyboard_flags(), 0);
    send(&mut rt, b"\x1b[<u"); // pop one, count omitted
    assert_eq!(rt.enhanced_keyboard_flags(), 7);
}

#[test]
fn popping_past_the_stack_empties_it_and_resets_all_flags() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    send(&mut rt, b"\x1b[>2u");
    send(&mut rt, b"\x1b[>4u");
    send(&mut rt, b"\x1b[<9u"); // pop more entries than exist
    assert_eq!(rt.enhanced_keyboard_flags(), 0);
    // A later pop is a no-op, never an underflow.
    send(&mut rt, b"\x1b[<u");
    assert_eq!(rt.enhanced_keyboard_flags(), 0);
}

#[test]
fn enhanced_keyboard_state_is_independent_per_alternate_screen() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    send(&mut rt, b"\x1b[>4u"); // main: push 1, current 4
    send(&mut rt, b"\x1b[?1049h"); // enter alt screen
    send(&mut rt, b"\x1b[=8u"); // alt screen changes its own flags
    send(&mut rt, b"\x1b[<u"); // pop inside alt
    send(&mut rt, b"\x1b[?1049l"); // leave alt screen
    assert_eq!(
        rt.enhanced_keyboard_flags(),
        4,
        "main-screen flags must survive an alt-screen push/pop cycle"
    );
}

#[test]
fn never_negotiated_alt_screen_reports_zero_flags() {
    // F3 (review PX-3072): the alt screen must hold an independent register.
    // Main negotiated 4; entering a fresh alt screen must report 0, and main
    // must be restored untouched on exit.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=4u");
    send(&mut rt, b"\x1b[?1049h");
    send(&mut rt, b"\x1b[?u");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b[?0u".to_vec()],
        "alt screen that never negotiated reports flags 0, not main's 4"
    );
    send(&mut rt, b"\x1b[?1049l");
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?4u".to_vec()]);
}

#[test]
fn alt_screen_reentrant_register_survives() {
    // F3: the alt screen's own register is retained across alt sessions.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?1049h");
    send(&mut rt, b"\x1b[=2u"); // alt negotiates 2
    send(&mut rt, b"\x1b[?1049l");
    send(&mut rt, b"\x1b[?1049h"); // re-enter
    send(&mut rt, b"\x1b[?u");
    assert_eq!(replies(&mut rt), vec![b"\x1b[?2u".to_vec()]);
    send(&mut rt, b"\x1b[?1049l");
}

#[test]
fn legacy_7727_alias_still_negotiates_flags() {
    // Regression: the historical `?7727` alias keeps its old semantics while
    // the authoritative `CSI = u` form is the primary path.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[?7727h");
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
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
fn disambiguate_does_not_report_shift_only_text_keys() {
    // F1 (review PX-3072): the Disambiguate flag covers Esc/alt/ctrl/ctrl+alt/
    // shift+alt — never shift alone. A shifted text key keeps its text
    // (report-all-keys is off), matching kitty's SEND_TEXT_TO_CHILD path.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u"); // disambiguate only
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", Some("A"), PressState::Pressed, false)),
        Some(b"A".to_vec()),
        "shift+A stays on the text path under CSI = 1u"
    );
    assert_eq!(
        rt.handle_key_event_ref(&char_key("1", Some("!"), PressState::Pressed, false)),
        Some(b"!".to_vec()),
        "shift+1 stays on the text path under CSI = 1u"
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn disambiguate_reports_shift_alt_but_not_shift_only() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;4u".to_vec()),
        "shift+alt+A is in the disambiguated set"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn disambiguate_reports_ctrl_space_as_csi_u() {
    // F2 (review PX-3072): ctrl+Space is a disambiguated chord and must be
    // `CSI 32;5u`, not the legacy NUL byte.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Space, PressState::Pressed)),
        Some(b"\x1b[32;5u".to_vec()),
        "ctrl+Space under disambiguate is CSI 32;5u"
    );
    release_ctrl(&mut rt);
}

#[test]
fn bare_space_stays_text_under_disambiguate() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Space, PressState::Pressed)),
        Some(b" ".to_vec())
    );
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
    // CTX-1267 (#2142a): Shift alone is the fcitx5 IME toggle and must
    // always reach the IME, never the PTY — even under report-all-keys.
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Shift, PressState::Pressed)),
        None,
        "Shift alone is IME-owned under report-all (#2142a)"
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
    for (key, expected) in [
        (NamedKey::Enter, b"\r".as_slice()),
        (NamedKey::Tab, b"\t".as_slice()),
        (NamedKey::Backspace, b"\x7f".as_slice()),
        (NamedKey::Delete, b"\x1b[3~".as_slice()),
    ] {
        assert_eq!(
            rt.handle_key_event_ref(&named_key(key, PressState::Pressed)),
            Some(expected.to_vec()),
            "unmodified {key:?} must retain its legacy bytes under disambiguation"
        );
    }
}

#[test]
fn shift_enter_is_distinct_only_after_kitty_negotiation() {
    let mut rt = make_runtime();

    // Without an application opt-in, legacy Enter remains CR and Shift+Enter
    // fails closed to the same legacy byte: no guessed escape sequence is sent
    // to applications that did not negotiate one.
    press_shift(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\r".to_vec()),
        "unnegotiated Shift+Enter must stay legacy CR"
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));

    // Kitty's progressive push form is the protocol an application uses to
    // opt in. The modifier field 2 means Shift (raw mask 1 plus one), so the
    // application can bind this key to an inserted newline rather than submit.
    send(&mut rt, b"\x1b[>1u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\r".to_vec()),
        "ordinary Enter must retain its legacy CR"
    );
    press_shift(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\x1b[13;2u".to_vec()),
        "negotiated Shift+Enter must be CSI 13;2u"
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn negotiated_shift_enter_preserves_repeat_and_release_event_types() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>3u"); // disambiguate + report events
    press_shift(&mut rt);

    let mut repeat = named_key(NamedKey::Enter, PressState::Pressed);
    repeat.repeat = true;
    assert_eq!(
        rt.handle_key_event_ref(&repeat),
        Some(b"\x1b[13;2:2u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Released)),
        Some(b"\x1b[13;2:3u".to_vec())
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn platform_modifier_event_reaches_shift_enter_encoder() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>1u");

    rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::ModifiersChanged(ModifiersState {
            shift: true,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    });
    assert!(!rt.handle_platform_event(PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::KeyboardInput(named_key(NamedKey::Enter, PressState::Pressed)),
    }));
    assert_eq!(rt.drain_pending_input(), b"\x1b[13;2u");
}

#[test]
fn shift_enter_is_not_encoded_by_an_unrelated_alternate_flag() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>4u"); // alternate-key reporting alone
    press_shift(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\r".to_vec()),
        "Shift+Enter must not emit CSI-u without disambiguation or all-keys"
    );
    rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
}

#[test]
fn modified_functional_keys_are_table_driven_csi_u_frames() {
    struct Case {
        name: &'static str,
        key: NamedKey,
        location: KeyLocation,
        modifiers: &'static [NamedKey],
        expected: &'static [u8],
    }

    const CASES: &[Case] = &[
        Case {
            name: "Shift+Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Shift],
            expected: b"\x1b[13;2u",
        },
        Case {
            name: "Ctrl+Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control],
            expected: b"\x1b[13;5u",
        },
        Case {
            name: "Alt+Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Alt],
            expected: b"\x1b[13;3u",
        },
        Case {
            name: "Ctrl+Shift+Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control, NamedKey::Shift],
            expected: b"\x1b[13;6u",
        },
        Case {
            name: "Shift+Tab",
            key: NamedKey::Tab,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Shift],
            expected: b"\x1b[9;2u",
        },
        Case {
            name: "Ctrl+Tab",
            key: NamedKey::Tab,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control],
            expected: b"\x1b[9;5u",
        },
        Case {
            name: "Alt+Tab",
            key: NamedKey::Tab,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Alt],
            expected: b"\x1b[9;3u",
        },
        Case {
            name: "Ctrl+Shift+Tab",
            key: NamedKey::Tab,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control, NamedKey::Shift],
            expected: b"\x1b[9;6u",
        },
        Case {
            name: "Shift+Backspace",
            key: NamedKey::Backspace,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Shift],
            expected: b"\x1b[127;2u",
        },
        Case {
            name: "Ctrl+Backspace",
            key: NamedKey::Backspace,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control],
            expected: b"\x1b[127;5u",
        },
        Case {
            name: "Alt+Backspace",
            key: NamedKey::Backspace,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Alt],
            expected: b"\x1b[127;3u",
        },
        Case {
            name: "Ctrl+Shift+Backspace",
            key: NamedKey::Backspace,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control, NamedKey::Shift],
            expected: b"\x1b[127;6u",
        },
        Case {
            name: "Shift+Delete",
            key: NamedKey::Delete,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Shift],
            expected: b"\x1b[3;2~",
        },
        Case {
            name: "Ctrl+Delete",
            key: NamedKey::Delete,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control],
            expected: b"\x1b[3;5~",
        },
        Case {
            name: "Alt+Delete",
            key: NamedKey::Delete,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Alt],
            expected: b"\x1b[3;3~",
        },
        Case {
            name: "Ctrl+Shift+Delete",
            key: NamedKey::Delete,
            location: KeyLocation::Standard,
            modifiers: &[NamedKey::Control, NamedKey::Shift],
            expected: b"\x1b[3;6~",
        },
        Case {
            name: "Shift+numpad Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Numpad,
            modifiers: &[NamedKey::Shift],
            expected: b"\x1b[57414;2u",
        },
        Case {
            name: "Ctrl+numpad Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Numpad,
            modifiers: &[NamedKey::Control],
            expected: b"\x1b[57414;5u",
        },
        Case {
            name: "Alt+numpad Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Numpad,
            modifiers: &[NamedKey::Alt],
            expected: b"\x1b[57414;3u",
        },
        Case {
            name: "Ctrl+Shift+numpad Enter",
            key: NamedKey::Enter,
            location: KeyLocation::Numpad,
            modifiers: &[NamedKey::Control, NamedKey::Shift],
            expected: b"\x1b[57414;6u",
        },
    ];

    for case in CASES {
        let mut rt = make_runtime();
        send(&mut rt, b"\x1b[>1u");
        hold_modifiers(&mut rt, case.modifiers);
        let event = KeyEvent {
            logical_key: LogicalKey::Named(case.key),
            text: None,
            location: case.location,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(
            rt.handle_key_event_ref(&event),
            Some(case.expected.to_vec()),
            "{} must use CSI-u and not a duplicate legacy byte",
            case.name
        );
        assert_eq!(
            rt.drain_pending_input(),
            case.expected,
            "{} routed once",
            case.name
        );
        release_modifiers(&mut rt, case.modifiers);
    }
}

#[test]
fn modified_functional_key_repeat_and_release_frames_are_table_driven() {
    let cases = [
        (
            NamedKey::Enter,
            &[NamedKey::Shift][..],
            b"\x1b[13;2" as &[u8],
        ),
        (NamedKey::Tab, &[NamedKey::Control][..], b"\x1b[9;5"),
        (NamedKey::Backspace, &[NamedKey::Alt][..], b"\x1b[127;3"),
        (
            NamedKey::Delete,
            &[NamedKey::Control, NamedKey::Shift][..],
            b"\x1b[3;6",
        ),
    ];

    for (key, modifiers, prefix) in cases {
        let mut rt = make_runtime();
        send(&mut rt, b"\x1b[>3u");
        hold_modifiers(&mut rt, modifiers);
        let mut repeat = named_key(key, PressState::Pressed);
        repeat.repeat = true;
        let mut expected = prefix.to_vec();
        expected.extend_from_slice(if key == NamedKey::Delete {
            b":2~"
        } else {
            b":2u"
        });
        assert_eq!(rt.handle_key_event_ref(&repeat), Some(expected.clone()));
        assert_eq!(rt.drain_pending_input(), expected);

        let mut expected_release = prefix.to_vec();
        expected_release.extend_from_slice(if key == NamedKey::Delete {
            b":3~"
        } else {
            b":3u"
        });
        assert_eq!(
            rt.handle_key_event_ref(&named_key(key, PressState::Released)),
            Some(expected_release.clone())
        );
        assert_eq!(rt.drain_pending_input(), expected_release);
        release_modifiers(&mut rt, modifiers);
    }
}

#[test]
fn report_all_encodes_bare_functional_keys_without_legacy_duplicates() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>9u"); // disambiguate + report all keys
    for (key, expected) in [
        (NamedKey::Enter, b"\x1b[13u" as &[u8]),
        (NamedKey::Tab, b"\x1b[9u"),
        (NamedKey::Backspace, b"\x1b[127u"),
        (NamedKey::Delete, b"\x1b[3~"),
    ] {
        assert_eq!(
            rt.handle_key_event_ref(&named_key(key, PressState::Pressed)),
            Some(expected.to_vec()),
            "report-all {key:?} must not also emit its legacy byte"
        );
        assert_eq!(rt.drain_pending_input(), expected);
    }
}

// ----------------------------------------------------------------------
// CTX-0755 (Issue #1362): super/hyper/meta modifiers, keypad codes,
// media/volume keys — the declared CSI-u coverage delta.
// ----------------------------------------------------------------------

#[test]
fn super_modifier_encodes_bit_8_under_disambiguate() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    press_super(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;9u".to_vec()),
        "Super+A under disambiguate is CSI 97;9u (super bit 8 -> 9)"
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Space, PressState::Pressed)),
        Some(b"\x1b[32;9u".to_vec()),
        "Super+Space under disambiguate is CSI 32;9u"
    );
    release_super(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"a".to_vec()),
        "releasing Super returns to the text path"
    );
}

#[test]
fn hyper_and_meta_modifiers_encode_bits_16_and_32() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    rt.handle_key_event(named_key(NamedKey::Hyper, PressState::Pressed));
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;17u".to_vec()),
        "Hyper+A under disambiguate is CSI 97;17u (hyper bit 16 -> 17)"
    );
    rt.handle_key_event(named_key(NamedKey::Hyper, PressState::Released));
    rt.handle_key_event(named_key(NamedKey::Meta, PressState::Pressed));
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Pressed, false)),
        Some(b"\x1b[97;33u".to_vec()),
        "Meta+A under disambiguate is CSI 97;33u (meta bit 32 -> 33)"
    );
    rt.handle_key_event(named_key(NamedKey::Meta, PressState::Released));
}

#[test]
fn report_all_keys_reports_super_hyper_meta_presses() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=8u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Super, PressState::Pressed)),
        Some(b"\x1b[57444;9u".to_vec())
    );
    rt.handle_key_event(named_key(NamedKey::Super, PressState::Released));
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Hyper, PressState::Pressed)),
        Some(b"\x1b[57445;17u".to_vec())
    );
    rt.handle_key_event(named_key(NamedKey::Hyper, PressState::Released));
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Meta, PressState::Pressed)),
        Some(b"\x1b[57446;33u".to_vec())
    );
    rt.handle_key_event(named_key(NamedKey::Meta, PressState::Released));
}

#[test]
fn numpad_digits_encode_kp_codes_under_disambiguate() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    assert_eq!(
        rt.handle_key_event_ref(&numpad_char_key("1", Some("1"))),
        Some(b"\x1b[57400u".to_vec()),
        "numpad 1 under disambiguate is KP_1"
    );
    assert_eq!(
        rt.handle_key_event_ref(&numpad_char_key(".", Some("."))),
        Some(b"\x1b[57409u".to_vec()),
        "numpad decimal under disambiguate is KP_DECIMAL"
    );
    // The standard-located twin keeps the text path.
    assert_eq!(
        rt.handle_key_event_ref(&char_key("1", Some("1"), PressState::Pressed, false)),
        Some(b"1".to_vec())
    );
}

#[test]
fn numpad_keys_keep_legacy_bytes_without_enhancement() {
    let mut rt = make_runtime();
    assert_eq!(
        rt.handle_key_event_ref(&numpad_char_key("1", Some("1"))),
        Some(b"1".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&numpad_named_key(NamedKey::Enter)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&numpad_named_key(NamedKey::ArrowUp)),
        Some(b"\x1b[A".to_vec())
    );
}

#[test]
fn numpad_named_keys_encode_kp_codes_under_disambiguate() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=1u");
    // Numpad Enter is NOT covered by the bare-Enter legacy exception: the
    // exception types `reset` on the main key, while keypad keys must stay
    // distinguishable.
    assert_eq!(
        rt.handle_key_event_ref(&numpad_named_key(NamedKey::Enter)),
        Some(b"\x1b[57414u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&numpad_named_key(NamedKey::ArrowUp)),
        Some(b"\x1b[57419u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&numpad_named_key(NamedKey::Home)),
        Some(b"\x1b[57423u".to_vec())
    );
    // The standard-located twins keep their legacy/disambiguated forms.
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Enter, PressState::Pressed)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::ArrowUp, PressState::Pressed)),
        Some(b"\x1b[A".to_vec())
    );
}

#[test]
fn media_and_volume_keys_encode_under_report_all() {
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[=8u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::MediaPlay, PressState::Pressed)),
        Some(b"\x1b[57428u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::MediaTrackNext, PressState::Pressed)),
        Some(b"\x1b[57435u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::AudioVolumeMute, PressState::Pressed)),
        Some(b"\x1b[57440u".to_vec())
    );
    // The legacy path has no encoding for these keys: nothing is emitted.
    let mut legacy = make_runtime();
    assert_eq!(
        legacy.handle_key_event_ref(&named_key(NamedKey::MediaPlay, PressState::Pressed)),
        None
    );
}

#[test]
fn default_off_ignores_numpad_location_and_extra_modifiers() {
    // Extension of the differential proof: numpad location and the
    // super/hyper/meta latches change nothing while flags are 0.
    let mut rt = make_runtime();
    press_super(&mut rt);
    let events = [
        numpad_char_key("1", Some("1")),
        numpad_named_key(NamedKey::Enter),
        numpad_named_key(NamedKey::ArrowUp),
        char_key("a", Some("a"), PressState::Pressed, false),
        named_key(NamedKey::MediaPlay, PressState::Pressed),
    ];
    for event in &events {
        let enhanced = rt.handle_key_event_ref(event);
        let legacy = bitty_platform::keyboard::encode_key_event_with_modifiers(
            event,
            &bitty_platform::ModifiersState {
                shift: false,
                control: false,
                alt: false,
                super_pressed: false,
            },
        );
        assert_eq!(
            enhanced, legacy,
            "flags 0 must match the legacy encoder for {event:?}"
        );
    }
    release_super(&mut rt);
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
        let enhanced = rt.handle_key_event_ref(event);
        let legacy = bitty_platform::keyboard::encode_key_event_with_modifiers(event, &ctrl_mods());
        assert_eq!(
            enhanced, legacy,
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

// ----------------------------------------------------------------------
// CTX-0947 (Issue #1665): push-form (`CSI > flags u`) acceptance cases.
// The encoding now routes through
// `bitty_platform::keyboard::encode_key_event_kitty_protocol` whenever the
// focused view's flags are non-zero; flags 0 keeps the byte-identical
// legacy fallback.
// ----------------------------------------------------------------------

#[test]
fn push_disambiguate_reports_ctrl_i_as_csi_u() {
    // `CSI > 1 u` pushes disambiguate: Ctrl+I encodes as the `i` codepoint
    // with the ctrl modifier, distinct from the bare Tab key.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>1u");
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("i", None, PressState::Pressed, false)),
        Some(b"\x1b[105;5u".to_vec()),
        "Ctrl+I under CSI > 1u is CSI 105;5u"
    );
    release_ctrl(&mut rt);
    // Bare Tab keeps its legacy byte under disambiguate-only (spec
    // exception so a crashed program can still type `reset`); pushing
    // report-all-keys as well lifts the exception to `CSI 9u` (the
    // zero-modifier `;1` field is omitted per the reference `serialize`).
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Tab, PressState::Pressed)),
        Some(b"\t".to_vec())
    );
    send(&mut rt, b"\x1b[>9u");
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Tab, PressState::Pressed)),
        Some(b"\x1b[9u".to_vec()),
        "bare Tab under disambiguate|report-all is CSI 9u"
    );
    send(&mut rt, b"\x1b[<u");
    assert_eq!(rt.enhanced_keyboard_flags(), 1);
}

#[test]
fn push_report_events_reports_release_as_event_type_3() {
    // flags&2 requested via push: key-up frames carry the `:3` event type.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>3u");
    assert_eq!(rt.enhanced_keyboard_flags(), 3);
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("a", None, PressState::Released, false)),
        Some(b"\x1b[97;5:3u".to_vec()),
        "key-up carries event type 3 when flags&2 is pushed"
    );
    release_ctrl(&mut rt);
}

#[test]
fn popped_to_zero_restores_legacy_bytes() {
    // Popping back to flags 0 restores the byte-identical legacy fallback,
    // including the Ctrl+I C0 byte the disambiguated form replaces.
    let mut rt = make_runtime();
    send(&mut rt, b"\x1b[>1u");
    send(&mut rt, b"\x1b[<u");
    assert_eq!(rt.enhanced_keyboard_flags(), 0);
    press_ctrl(&mut rt);
    assert_eq!(
        rt.handle_key_event_ref(&char_key("i", None, PressState::Pressed, false)),
        Some(b"\x09".to_vec()),
        "flags 0 restores the legacy Ctrl+I byte"
    );
    assert_eq!(
        rt.handle_key_event_ref(&named_key(NamedKey::Tab, PressState::Pressed)),
        Some(b"\t".to_vec())
    );
    release_ctrl(&mut rt);
}
