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
//! - `Character` keys use the `text` payload when present (layout + modifier
//!   aware, e.g. `Ctrl+C` → `"\x03"` when winit reports it), falling back to
//!   the logical character string.
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

use crate::event::{KeyEvent, LogicalKey, NamedKey, PressState};

/// Maximum bytes a single key may produce (Fn keys and CSI sequences are tiny).
const MAX_ENCODED_LEN: usize = 8;

/// Encodes `event` into the terminal input bytes that should be written to
/// the PTY master.
///
/// Returns `None` when the event should not produce input (release, synthetic,
/// repeat of a modifier-only key, unmapped `Other`, dead key without
/// composition, etc.). The caller may synthesize `KeyEvent`s headlessly via
/// owned construction and drive this function without a display server.
pub fn encode_key_event(event: &KeyEvent) -> Option<Vec<u8>> {
    if event.state != PressState::Pressed {
        return None;
    }
    if event.is_synthetic {
        return None;
    }

    match &event.logical_key {
        LogicalKey::Character(ch) => {
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    // `text` already reflects modifiers (e.g. Ctrl+C → "\x03").
                    // Use it verbatim; it may contain multi-byte UTF-8.
                    return Some(text.as_bytes().to_vec());
                }
            }
            if ch.is_empty() {
                None
            } else {
                Some(ch.as_bytes().to_vec())
            }
        }
        LogicalKey::Named(named) => {
            if let Some(seq) = encode_named_key(*named) {
                return Some(seq.to_vec());
            }
            // Fallback: unmapped named keys may still carry text (e.g. an
            // unmodeled key whose text is printable). Use it when available.
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    return Some(text.as_bytes().to_vec());
                }
            }
            None
        }
        LogicalKey::Dead(maybe_char) => {
            if let Some(ch) = maybe_char {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                return Some(s.as_bytes().to_vec());
            }
            // Dead key without composition produces no input until composition.
            None
        }
        LogicalKey::Unidentified => {
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    return Some(text.as_bytes().to_vec());
                }
            }
            None
        }
    }
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

    #[test]
    fn repeat_still_encodes() {
        let mut ev = make_char("a", Some("a"), PressState::Pressed, false);
        ev.repeat = true;
        assert_eq!(encode_key_event(&ev), Some(b"a".to_vec()));
    }
}
