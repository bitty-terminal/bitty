//! Keyboard input encoding for terminal PTY write.
//!
//! This module owns the legacy xterm-style encoding from an owned [`KeyEvent`]
//! (crate::event::KeyEvent) to the byte sequence that must be written to the
//! PTY master. The function is pure, headless, and deterministic: no display
//! server, window, or OS state is consulted beyond the fields of the event.
//! Synthetic and release events produce no output; the caller decides PTY
//! write policy (buffer vs. direct write) and window-focus suppression.
//!
//! # Encoding policy (M1 legacy)
//!
//! - `Character` keys with tracked control state synthesize C0 bytes from the
//!   logical character (`Ctrl+A-Z` → `0x01-0x1A`, `Ctrl+Space` → `0x00`,
//!   `Ctrl+[` → `ESC`, `Ctrl+\` → `0x1C`, `Ctrl+]` → `0x1D`, `Ctrl+^` →
//!   `0x1E`, `Ctrl+_` → `0x1F`, `Ctrl+?` → `0x7F`) instead of trusting winit
//!   `text`, which is `None` (or the bare letter) for `Ctrl+letter` on
//!   Wayland. Unmapped control chords fall back to the text path so no key
//!   is swallowed.
//! - Without control state, `Character` keys use the `text` payload when
//!   present (layout + modifier aware), falling back to the logical
//!   character string.
//! - Tracked alt state prefixes the whole input with `ESC` (xterm
//!   `metaSendsEscape` semantics); `Ctrl+Space` arrives as named `Space` and
//!   encodes to `NUL` under control.
//! - Named keys map to classical VT sequences (`Enter` → `"\r"`, `ArrowUp` →
//!   `"\x1b[A"`, `F1` → `"\x1bOP"`, etc.). Modifier-only names (`Shift`,
//!   `Control`, `Alt`, …) produce no bytes; `Other`/`Unidentified` fall back
//!   to `text` when available, otherwise nothing.
//! - Kitty keyboard protocol is explicitly deferred (M1 opt-in, progressive
//!   enhancement per `compatibility-milestone-rfc`). Only the legacy baseline
//!   is encoded here so headless CI and real winit keyboards agree.
//!
//! The table mirrors xterm's legacy encoding sufficient for shells, editors,
//! and TUIs; application-cursor / keypad nuances (`DECCKM` etc.) are deferred
//! and documented as such.

#![forbid(unsafe_code)]

use crate::event::{KeyEvent, LogicalKey, ModifiersState, NamedKey, PressState};

/// Maximum bytes a single key may produce (Fn keys and CSI sequences are tiny).
const MAX_ENCODED_LEN: usize = 8;

/// Encodes `event` into the terminal input bytes that should be written to
/// the PTY master, assuming no modifiers are held.
///
/// This is the headless default: release, synthetic, modifier-only, unmapped
/// `Other`, and composition-less dead keys produce `None` as documented on
/// [`encode_key_event_with_modifiers`]. Callers with a tracked modifier
/// snapshot (the runtime's `ModifiersChanged` / modifier-key state) must use
/// [`encode_key_event_with_modifiers`] instead so `Ctrl+letter` synthesizes
/// C0 bytes on Wayland, where winit reports `text=None` for control chords.
pub fn encode_key_event(event: &KeyEvent) -> Option<Vec<u8>> {
    encode_key_event_with_modifiers(
        event,
        &ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
        },
    )
}

/// Encodes `event` with an explicit tracked-modifier snapshot into the
/// terminal input bytes that should be written to the PTY master.
///
/// Pure, headless, and deterministic: no display server, window, or OS state
/// is consulted beyond `event` and `modifiers`. Returns `None` when the event
/// should not produce input (release, synthetic, repeat of a modifier-only
/// key, unmapped `Other`, dead key without composition, etc.). The caller may
/// synthesize `KeyEvent`s headlessly via owned construction and drive this
/// function without a display server.
///
/// Modifier semantics (xterm legacy):
///
/// - `control` synthesizes C0 bytes from the logical character via
///   [`ctrl_control_byte`] without consulting `text`: `Ctrl+A-Z` →
///   `0x01-0x1A` (case-insensitive), plus the `Space`/`[`/`\`/`]`/`^`/`_`/`?`
///   equivalents. A control chord with no C0 mapping (digits, other
///   punctuation) falls through to the text path so the key still produces
///   its bare input instead of being swallowed.
/// - `alt` prefixes the resulting input with `ESC` (`metaSendsEscape`).
///   Combined `Ctrl+Alt+letter` therefore yields `ESC` plus the C0 byte.
/// - `NamedKey::Space` under `control` is `NUL` (`0x00`); `Tab`/`Enter`/
///   `Escape` already equal their control codes (`0x09`/`0x0D`/`0x1B`), and
///   all other named keys keep their legacy sequences (CSI modifier encoding
///   stays deferred to the keymap slice).
/// - `shift` has no legacy effect: the logical character already reflects it
///   (`^`/`_`/`?` arrive shifted), and `super_pressed` is ignored (Super
///   chords are compositor-reserved on the supported targets).
pub fn encode_key_event_with_modifiers(
    event: &KeyEvent,
    modifiers: &ModifiersState,
) -> Option<Vec<u8>> {
    if event.state != PressState::Pressed {
        return None;
    }
    if event.is_synthetic {
        return None;
    }

    match &event.logical_key {
        LogicalKey::Character(ch) => {
            if modifiers.control {
                if let Some(byte) = ctrl_control_byte(ch) {
                    if modifiers.alt {
                        return Some(vec![0x1b, byte]);
                    }
                    return Some(vec![byte]);
                }
            }
            let body: Vec<u8> = match &event.text {
                Some(text) if !text.is_empty() => text.as_bytes().to_vec(),
                _ if ch.is_empty() => return None,
                _ => ch.as_bytes().to_vec(),
            };
            if modifiers.alt {
                return Some(esc_prefix(&body));
            }
            Some(body)
        }
        LogicalKey::Named(named) => {
            if modifiers.control && *named == NamedKey::Space {
                if modifiers.alt {
                    return Some(vec![0x1b, 0x00]);
                }
                return Some(vec![0x00]);
            }
            if let Some(seq) = encode_named_key(*named) {
                if modifiers.alt {
                    return Some(esc_prefix(seq));
                }
                return Some(seq.to_vec());
            }
            // Fallback: unmapped named keys may still carry text (e.g. an
            // unmodeled key whose text is printable). Use it when available.
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    if modifiers.alt {
                        return Some(esc_prefix(text.as_bytes()));
                    }
                    return Some(text.as_bytes().to_vec());
                }
            }
            None
        }
        LogicalKey::Dead(maybe_char) => {
            if let Some(ch) = maybe_char {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                if modifiers.alt {
                    return Some(esc_prefix(s.as_bytes()));
                }
                return Some(s.as_bytes().to_vec());
            }
            // Dead key without composition produces no input until composition.
            None
        }
        LogicalKey::Unidentified => {
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    if modifiers.alt {
                        return Some(esc_prefix(text.as_bytes()));
                    }
                    return Some(text.as_bytes().to_vec());
                }
            }
            None
        }
    }
}

/// Maps a single logical character under tracked control state to its legacy
/// C0 byte (xterm `ch & 0x1F`, with the `?` → `DEL` special case).
///
/// Returns `None` for characters with no C0 mapping so the caller falls back
/// to the text path instead of swallowing the key. The input must be exactly
/// one character; multi-character logical strings never map.
fn ctrl_control_byte(logical: &str) -> Option<u8> {
    let mut chars = logical.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match first {
        'a'..='z' => Some(first as u8 - b'a' + 1),
        'A'..='Z' => Some(first as u8 - b'A' + 1),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Prefixes `body` with `ESC` (xterm `metaSendsEscape` semantics).
fn esc_prefix(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(0x1b);
    out.extend_from_slice(body);
    out
}

/// Maps an explicitly modeled [`NamedKey`] to its legacy VT byte sequence.
///
/// Returns `None` for modifier-only or unmapped keys that should not emit.
pub fn encode_named_key(named: NamedKey) -> Option<&'static [u8]> {
    match named {
        NamedKey::Enter => Some(b"\r"),
        NamedKey::Tab => Some(b"\t"),
        NamedKey::Backspace => Some(b"\x7f"),
        NamedKey::Delete => Some(b"\x1b[3~"),
        NamedKey::Insert => Some(b"\x1b[2~"),
        NamedKey::Home => Some(b"\x1b[H"),
        NamedKey::End => Some(b"\x1b[F"),
        NamedKey::PageUp => Some(b"\x1b[5~"),
        NamedKey::PageDown => Some(b"\x1b[6~"),
        NamedKey::ArrowUp => Some(b"\x1b[A"),
        NamedKey::ArrowDown => Some(b"\x1b[B"),
        NamedKey::ArrowRight => Some(b"\x1b[C"),
        NamedKey::ArrowLeft => Some(b"\x1b[D"),
        NamedKey::Escape => Some(b"\x1b"),
        NamedKey::Space => Some(b" "),
        // F-keys: xterm / xterm-256color legacy
        NamedKey::F1 => Some(b"\x1bOP"),
        NamedKey::F2 => Some(b"\x1bOQ"),
        NamedKey::F3 => Some(b"\x1bOR"),
        NamedKey::F4 => Some(b"\x1bOS"),
        NamedKey::F5 => Some(b"\x1b[15~"),
        NamedKey::F6 => Some(b"\x1b[17~"),
        NamedKey::F7 => Some(b"\x1b[18~"),
        NamedKey::F8 => Some(b"\x1b[19~"),
        NamedKey::F9 => Some(b"\x1b[20~"),
        NamedKey::F10 => Some(b"\x1b[21~"),
        NamedKey::F11 => Some(b"\x1b[23~"),
        NamedKey::F12 => Some(b"\x1b[24~"),
        NamedKey::F13 => Some(b"\x1b[25~"),
        NamedKey::F14 => Some(b"\x1b[26~"),
        NamedKey::F15 => Some(b"\x1b[28~"),
        NamedKey::F16 => Some(b"\x1b[29~"),
        NamedKey::F17 => Some(b"\x1b[31~"),
        NamedKey::F18 => Some(b"\x1b[32~"),
        NamedKey::F19 => Some(b"\x1b[33~"),
        NamedKey::F20 => Some(b"\x1b[34~"),
        // F21-F35 require modifyOtherKeys / CSI modifier encoding to avoid
        // collisions with legacy sequences (Insert \x1b[2~, Delete \x1b[3~,
        // PageUp \x1b[5~, PageDown \x1b[6~, F5 \x1b[15~, etc.). Deferred to
        // follow-up task before M1 when the modifier table lands.
        NamedKey::F21
        | NamedKey::F22
        | NamedKey::F23
        | NamedKey::F24
        | NamedKey::F25
        | NamedKey::F26
        | NamedKey::F27
        | NamedKey::F28
        | NamedKey::F29
        | NamedKey::F30
        | NamedKey::F31
        | NamedKey::F32
        | NamedKey::F33
        | NamedKey::F34
        | NamedKey::F35 => None,

        // Modifier and state keys produce no terminal input.
        NamedKey::Shift
        | NamedKey::Control
        | NamedKey::Alt
        | NamedKey::AltGraph
        | NamedKey::Meta
        | NamedKey::Super
        | NamedKey::Hyper
        | NamedKey::Fn
        | NamedKey::FnLock
        | NamedKey::CapsLock
        | NamedKey::NumLock
        | NamedKey::ScrollLock
        | NamedKey::Symbol
        | NamedKey::SymbolLock => None,

        // Host / media / browser / power keys: no terminal input in legacy.
        NamedKey::PrintScreen
        | NamedKey::Pause
        | NamedKey::ContextMenu
        | NamedKey::Copy
        | NamedKey::Cut
        | NamedKey::Paste
        | NamedKey::Undo
        | NamedKey::Redo
        | NamedKey::Find
        | NamedKey::Select
        | NamedKey::Again
        | NamedKey::Props
        | NamedKey::Execute
        | NamedKey::Help
        | NamedKey::AudioVolumeMute
        | NamedKey::AudioVolumeDown
        | NamedKey::AudioVolumeUp
        | NamedKey::MediaPlay
        | NamedKey::MediaPause
        | NamedKey::MediaPlayPause
        | NamedKey::MediaStop
        | NamedKey::MediaTrackNext
        | NamedKey::MediaTrackPrevious
        | NamedKey::BrowserBack
        | NamedKey::BrowserForward
        | NamedKey::BrowserRefresh
        | NamedKey::BrowserStop
        | NamedKey::BrowserSearch
        | NamedKey::BrowserHome
        | NamedKey::BrowserFavorites
        | NamedKey::LaunchMail
        | NamedKey::LaunchApplication1
        | NamedKey::LaunchApplication2
        | NamedKey::Eject
        | NamedKey::Power
        | NamedKey::WakeUp
        | NamedKey::Standby
        | NamedKey::Hibernate
        | NamedKey::Soft1
        | NamedKey::Soft2
        | NamedKey::Soft3
        | NamedKey::Soft4
        | NamedKey::Clear => None,

        NamedKey::Other => None,
    }
}

#[allow(dead_code)]
const fn encoded_len_bound() -> usize {
    MAX_ENCODED_LEN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyLocation, LogicalKey, NamedKey, PressState};

    fn make_char(ch: &str, text: Option<&str>, state: PressState, synthetic: bool) -> KeyEvent {
        KeyEvent {
            logical_key: LogicalKey::Character(ch.to_string()),
            text: text.map(|s| s.to_string()),
            location: KeyLocation::Standard,
            state,
            repeat: false,
            is_synthetic: synthetic,
        }
    }

    fn make_named(
        named: NamedKey,
        text: Option<&str>,
        state: PressState,
        synthetic: bool,
    ) -> KeyEvent {
        KeyEvent {
            logical_key: LogicalKey::Named(named),
            text: text.map(|s| s.to_string()),
            location: KeyLocation::Standard,
            state,
            repeat: false,
            is_synthetic: synthetic,
        }
    }

    #[test]
    fn character_uses_text_when_present() {
        let ev = make_char("a", Some("a"), PressState::Pressed, false);
        assert_eq!(encode_key_event(&ev), Some(b"a".to_vec()));
        let ctrl = make_char("c", Some("\x03"), PressState::Pressed, false);
        assert_eq!(encode_key_event(&ctrl), Some(b"\x03".to_vec()));
        let euro = make_char("€", Some("€"), PressState::Pressed, false);
        assert_eq!(encode_key_event(&euro), Some("€".as_bytes().to_vec()));
    }

    #[test]
    fn character_falls_back_to_logical_when_text_none() {
        let ev = make_char("z", None, PressState::Pressed, false);
        assert_eq!(encode_key_event(&ev), Some(b"z".to_vec()));
    }

    #[test]
    fn release_and_synthetic_produce_none() {
        let rel = make_char("a", Some("a"), PressState::Released, false);
        assert_eq!(encode_key_event(&rel), None);
        let synth = make_char("a", Some("a"), PressState::Pressed, true);
        assert_eq!(encode_key_event(&synth), None);
        let rel_named = make_named(NamedKey::Enter, None, PressState::Released, false);
        assert_eq!(encode_key_event(&rel_named), None);
    }

    #[test]
    fn named_keys_encode_to_legacy_sequences() {
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Enter,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(NamedKey::Tab, None, PressState::Pressed, false)),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Backspace,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Escape,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::ArrowUp,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::ArrowDown,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::ArrowRight,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::ArrowLeft,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[D".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Home,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(NamedKey::End, None, PressState::Pressed, false)),
            Some(b"\x1b[F".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(NamedKey::F1, None, PressState::Pressed, false)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(NamedKey::F5, None, PressState::Pressed, false)),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn modifier_only_named_produces_none() {
        for named in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::CapsLock,
        ] {
            assert_eq!(
                encode_key_event(&make_named(named, None, PressState::Pressed, false)),
                None,
                "modifier {named:?} should produce no input"
            );
        }
    }

    #[test]
    fn unmapped_other_with_text_falls_back() {
        let ev = make_named(NamedKey::Other, Some("x"), PressState::Pressed, false);
        assert_eq!(encode_key_event(&ev), Some(b"x".to_vec()));
        let ev2 = make_named(NamedKey::Other, None, PressState::Pressed, false);
        assert_eq!(encode_key_event(&ev2), None);
    }

    #[test]
    fn dead_key_with_char_encodes_utf8() {
        let ev = KeyEvent {
            logical_key: LogicalKey::Dead(Some('^')),
            text: None,
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(encode_key_event(&ev), Some("^".as_bytes().to_vec()));
        let ev2 = KeyEvent {
            logical_key: LogicalKey::Dead(None),
            text: None,
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(encode_key_event(&ev2), None);
    }

    #[test]
    fn unidentified_fallback_to_text() {
        let ev = KeyEvent {
            logical_key: LogicalKey::Unidentified,
            text: Some("z".to_string()),
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(encode_key_event(&ev), Some(b"z".to_vec()));
        let ev2 = KeyEvent {
            logical_key: LogicalKey::Unidentified,
            text: None,
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(encode_key_event(&ev2), None);
    }

    fn ctrl_mods() -> ModifiersState {
        ModifiersState {
            shift: false,
            control: true,
            alt: false,
            super_pressed: false,
        }
    }

    fn alt_mods() -> ModifiersState {
        ModifiersState {
            shift: false,
            control: false,
            alt: true,
            super_pressed: false,
        }
    }

    fn ctrl_alt_mods() -> ModifiersState {
        ModifiersState {
            shift: false,
            control: true,
            alt: true,
            super_pressed: false,
        }
    }

    fn no_mods() -> ModifiersState {
        ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
        }
    }

    fn char_event(logical: &str, text: Option<&str>) -> KeyEvent {
        make_char(logical, text, PressState::Pressed, false)
    }

    #[test]
    fn ctrl_letters_synthesize_c0_regardless_of_text() {
        // CTX-0154 matrix: every Ctrl+A-Z must yield 0x01-0x1A from the
        // logical character plus tracked control state, whether winit
        // reports text=None (Wayland) or the bare letter.
        for (index, letter) in ('a'..='z').enumerate() {
            let expected = vec![index as u8 + 1];
            let logical = letter.to_string();
            let upper = logical.to_ascii_uppercase();
            // Wayland: text=None.
            assert_eq!(
                encode_key_event_with_modifiers(&char_event(&logical, None), &ctrl_mods()),
                Some(expected.clone()),
                "Ctrl+{logical} with text=None must synthesize"
            );
            // Wayland variant: text is the bare letter.
            assert_eq!(
                encode_key_event_with_modifiers(
                    &char_event(&logical, Some(&logical)),
                    &ctrl_mods()
                ),
                Some(expected.clone()),
                "Ctrl+{logical} with bare-letter text must synthesize"
            );
            // Shift-insensitive: uppercase logical maps identically.
            assert_eq!(
                encode_key_event_with_modifiers(&char_event(&upper, None), &ctrl_mods()),
                Some(expected.clone()),
                "Ctrl+{upper} must match Ctrl+{logical}"
            );
            // Platform-synthesized text (X11-style C0 text) agrees verbatim:
            // no double encoding when winit already reports the byte.
            let c0 = [index as u8 + 1];
            let text = std::str::from_utf8(&c0).expect("C0 is valid UTF-8");
            assert_eq!(
                encode_key_event_with_modifiers(&char_event(&logical, Some(text)), &ctrl_mods()),
                Some(expected.clone()),
                "Ctrl+{logical} with C0 text must stay {expected:02x?}"
            );
        }
    }

    #[test]
    fn ctrl_symbol_legacy_equivalents() {
        // xterm C0 table: Space/[/\/]/^/_/? under control.
        let cases: &[(&str, u8)] = &[
            (" ", 0x00),
            ("[", 0x1b),
            ("\\", 0x1c),
            ("]", 0x1d),
            ("^", 0x1e),
            ("_", 0x1f),
            ("?", 0x7f),
        ];
        for (logical, byte) in cases {
            for text in [None, Some(*logical)] {
                assert_eq!(
                    encode_key_event_with_modifiers(&char_event(logical, text), &ctrl_mods()),
                    Some(vec![*byte]),
                    "Ctrl+{logical} must synthesize 0x{byte:02x}"
                );
            }
        }
        // Ctrl+Space also arrives as named Space on winit.
        assert_eq!(
            encode_key_event_with_modifiers(
                &make_named(NamedKey::Space, None, PressState::Pressed, false),
                &ctrl_mods()
            ),
            Some(vec![0x00])
        );
        // Unmapped control chords (digits, other punctuation) fall back to
        // the text path instead of being swallowed.
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("1", Some("1")), &ctrl_mods()),
            Some(b"1".to_vec())
        );
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("1", None), &ctrl_mods()),
            Some(b"1".to_vec())
        );
        // Multi-character logical strings never map to C0.
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("ab", None), &ctrl_mods()),
            Some(b"ab".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape_xterm_meta_sends_escape() {
        // Alt+letter prefixes the whole input with ESC.
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("x", Some("x")), &alt_mods()),
            Some(vec![0x1b, b'x'])
        );
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("x", None), &alt_mods()),
            Some(vec![0x1b, b'x'])
        );
        // Alt+Enter prefixes the legacy sequence.
        assert_eq!(
            encode_key_event_with_modifiers(
                &make_named(NamedKey::Enter, None, PressState::Pressed, false),
                &alt_mods()
            ),
            Some(vec![0x1b, b'\r'])
        );
        // Alt+ArrowUp prefixes the CSI sequence.
        assert_eq!(
            encode_key_event_with_modifiers(
                &make_named(NamedKey::ArrowUp, None, PressState::Pressed, false),
                &alt_mods()
            ),
            Some(b"\x1b\x1b[A".to_vec())
        );
        // Ctrl+Alt+letter yields ESC plus the C0 byte.
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("f", None), &ctrl_alt_mods()),
            Some(vec![0x1b, 0x06])
        );
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("c", Some("c")), &ctrl_alt_mods()),
            Some(vec![0x1b, 0x03])
        );
        // Ctrl+Alt+Space (named) yields ESC NUL.
        assert_eq!(
            encode_key_event_with_modifiers(
                &make_named(NamedKey::Space, None, PressState::Pressed, false),
                &ctrl_alt_mods()
            ),
            Some(vec![0x1b, 0x00])
        );
        // Without modifiers the legacy outputs are unchanged.
        assert_eq!(
            encode_key_event_with_modifiers(&char_event("x", Some("x")), &no_mods()),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn no_modifiers_match_legacy_encoder() {
        // The no-modifier snapshot must agree exactly with the legacy entry
        // point, so existing callers observe no behavior change.
        let events = [
            char_event("a", Some("a")),
            char_event("z", None),
            char_event("c", Some("\x03")),
            char_event("€", Some("€")),
            make_named(NamedKey::Enter, None, PressState::Pressed, false),
            make_named(NamedKey::Tab, None, PressState::Pressed, false),
            make_named(NamedKey::Escape, None, PressState::Pressed, false),
            make_named(NamedKey::Space, None, PressState::Pressed, false),
            make_named(NamedKey::ArrowUp, None, PressState::Pressed, false),
            make_named(NamedKey::F12, None, PressState::Pressed, false),
            make_named(NamedKey::Shift, None, PressState::Pressed, false),
            make_named(NamedKey::Other, Some("x"), PressState::Pressed, false),
            make_char("a", Some("a"), PressState::Released, false),
            make_char("a", Some("a"), PressState::Pressed, true),
        ];
        for event in &events {
            assert_eq!(
                encode_key_event_with_modifiers(event, &no_mods()),
                encode_key_event(event),
                "no-modifier snapshot must match legacy for {event:?}"
            );
        }
    }

    #[test]
    fn repeat_still_encodes() {
        let mut ev = make_char("a", Some("a"), PressState::Pressed, false);
        ev.repeat = true;
        assert_eq!(encode_key_event(&ev), Some(b"a".to_vec()));
    }
}
