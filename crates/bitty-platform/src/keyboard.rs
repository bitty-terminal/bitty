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
//! - Kitty keyboard protocol is M1 opt-in, not M1-required
//!   (`compatibility-milestone-rfc`): this module owns the legacy baseline
//!   and the Kitty code tables ([`ext_functional_key`],
//!   [`ext_modifier_key`], [`ext_keypad_named_key`],
//!   [`ext_keypad_char_key`]); the runtime's `encode_key_enhanced` composes
//!   them under the negotiated flags. The legacy path here is the
//!   byte-identical fallback when the protocol is off.
//!
//! # Kitty coverage declaration (CTX-0755, Issue #1362)
//!
//! Encoded against `sw.kovidgoyal.net/kitty/keyboard-protocol`:
//! progressive flags 1/2/4/8/16 (disambiguate, report events, report
//! alternates, report all keys, report associated text); modifier bits
//! shift/alt/ctrl/super/hyper/meta; the single shifted-key alternate;
//! associated text as trailing codepoints; the functional table below
//! (including media/volume keys); and the keypad table (numpad-located
//! keys decode to `KP_*` codes under enhancement).
//!
//! Explicitly deferred (no stable platform source in winit, so no encoding
//! is guessed): the base-layout alternate key (third `code:shifted:base`
//! sub-field); caps-lock/num-lock modifier bits; key-number-`0` pure-text
//! events (every `KeyEvent` here carries its key); `ISO_LEVEL3/5_SHIFT`
//! (no winit named key); and browser/launch/eject/power keys (the spec
//! assigns them no codes, so they keep the legacy path).
//!
//! The table mirrors xterm's legacy encoding sufficient for shells, editors,
//! and TUIs; application-cursor / keypad nuances (`DECCKM` etc.) are deferred
//! and documented as such.

#![forbid(unsafe_code)]

use crate::event::{KeyEvent, KeyLocation, LogicalKey, ModifiersState, NamedKey, PressState};

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

/// Maps an explicitly modeled [`NamedKey`] to its Kitty keyboard-protocol
/// functional key code and CSI trailer (CTX-0575).
///
/// The pair is `(unicode-key-code, final-byte)` from the authoritative
/// "Functional key definitions" table
/// (`sw.kovidgoyal.net/kitty/keyboard-protocol`); the code is a Unicode
/// Private Use Area value (`57344..=63743`) except the handful of C0
/// compatibility keys (Escape/Enter/Tab/Backspace) and the number keys
/// whose trailer is `~`/`A`/`B`/`C`/`D`/`H`/`F`/`P`/`Q`/`S`.
///
/// Returns `None` for keys with no defined Kitty code (the caller keeps the
/// legacy path). Modifier keys carry an explicit side, so they are handled by
/// [`ext_modifier_key`] instead.
pub fn ext_functional_key(named: NamedKey) -> Option<(u32, u8)> {
    let pair = match named {
        NamedKey::Escape => (27, b'u'),
        NamedKey::Enter => (13, b'u'),
        NamedKey::Tab => (9, b'u'),
        NamedKey::Backspace => (127, b'u'),
        NamedKey::Insert => (2, b'~'),
        NamedKey::Delete => (3, b'~'),
        NamedKey::ArrowLeft => (1, b'D'),
        NamedKey::ArrowRight => (1, b'C'),
        NamedKey::ArrowUp => (1, b'A'),
        NamedKey::ArrowDown => (1, b'B'),
        NamedKey::PageUp => (5, b'~'),
        NamedKey::PageDown => (6, b'~'),
        NamedKey::Home => (1, b'H'),
        NamedKey::End => (1, b'F'),
        NamedKey::CapsLock => (57358, b'u'),
        NamedKey::ScrollLock => (57359, b'u'),
        NamedKey::NumLock => (57360, b'u'),
        NamedKey::PrintScreen => (57361, b'u'),
        NamedKey::Pause => (57362, b'u'),
        NamedKey::ContextMenu => (57363, b'u'),
        NamedKey::F1 => (1, b'P'),
        NamedKey::F2 => (1, b'Q'),
        NamedKey::F3 => (13, b'~'),
        NamedKey::F4 => (1, b'S'),
        NamedKey::F5 => (15, b'~'),
        NamedKey::F6 => (17, b'~'),
        NamedKey::F7 => (18, b'~'),
        NamedKey::F8 => (19, b'~'),
        NamedKey::F9 => (20, b'~'),
        NamedKey::F10 => (21, b'~'),
        NamedKey::F11 => (23, b'~'),
        NamedKey::F12 => (24, b'~'),
        NamedKey::F13 => (57376, b'u'),
        NamedKey::F14 => (57377, b'u'),
        NamedKey::F15 => (57378, b'u'),
        NamedKey::F16 => (57379, b'u'),
        NamedKey::F17 => (57380, b'u'),
        NamedKey::F18 => (57381, b'u'),
        NamedKey::F19 => (57382, b'u'),
        NamedKey::F20 => (57383, b'u'),
        NamedKey::F21 => (57384, b'u'),
        NamedKey::F22 => (57385, b'u'),
        NamedKey::F23 => (57386, b'u'),
        NamedKey::F24 => (57387, b'u'),
        NamedKey::F25 => (57388, b'u'),
        NamedKey::F26 => (57389, b'u'),
        NamedKey::F27 => (57390, b'u'),
        NamedKey::F28 => (57391, b'u'),
        NamedKey::F29 => (57392, b'u'),
        NamedKey::F30 => (57393, b'u'),
        NamedKey::F31 => (57394, b'u'),
        NamedKey::F32 => (57395, b'u'),
        NamedKey::F33 => (57396, b'u'),
        NamedKey::F34 => (57397, b'u'),
        NamedKey::F35 => (57398, b'u'),
        // Media and volume keys (CTX-0755): the spec assigns dedicated
        // Private Use codes; previously these fell through to `None` and
        // produced no input even under report-all-keys.
        NamedKey::MediaPlay => (57428, b'u'),
        NamedKey::MediaPause => (57429, b'u'),
        NamedKey::MediaPlayPause => (57430, b'u'),
        NamedKey::MediaStop => (57432, b'u'),
        NamedKey::MediaTrackNext => (57435, b'u'),
        NamedKey::MediaTrackPrevious => (57436, b'u'),
        NamedKey::AudioVolumeDown => (57438, b'u'),
        NamedKey::AudioVolumeUp => (57439, b'u'),
        NamedKey::AudioVolumeMute => (57440, b'u'),
        _ => return None,
    };
    Some(pair)
}

/// Maps a modifier [`NamedKey`] plus its physical [`KeyLocation`] to the
/// Kitty keyboard-protocol left/right key code (CTX-0575).
///
/// The spec reports `shift`/`ctrl`/`alt`/`super`/`hyper`/`meta` keys as
/// dedicated codes (57441..=57452); `Location::Standard` defaults to the left
/// variant. `AltGraph` folds to `alt`, matching the legacy modifier model.
pub const fn ext_modifier_key(named: NamedKey, location: KeyLocation) -> Option<u32> {
    let right = matches!(location, KeyLocation::Right);
    let code = match named {
        NamedKey::Shift => {
            if right {
                57447
            } else {
                57441
            }
        }
        NamedKey::Control => {
            if right {
                57448
            } else {
                57442
            }
        }
        NamedKey::Alt | NamedKey::AltGraph => {
            if right {
                57449
            } else {
                57443
            }
        }
        NamedKey::Super => {
            if right {
                57450
            } else {
                57444
            }
        }
        NamedKey::Hyper => {
            if right {
                57451
            } else {
                57445
            }
        }
        NamedKey::Meta => {
            if right {
                57452
            } else {
                57446
            }
        }
        _ => return None,
    };
    Some(code)
}

/// Maps a numpad-located [`NamedKey`] to its Kitty keyboard-protocol keypad
/// code (CTX-0755, Issue #1362).
///
/// The spec reports keypad keys as their dedicated `KP_*` codes
/// (`57399..=57427`) so applications can distinguish them from the
/// equivalent non-keypad keys once the disambiguate enhancement is active;
/// without enhancement they keep the legacy encoding of the equivalent key.
/// Only `Numpad`-located events consult this table — the caller checks
/// [`KeyLocation::Numpad`] first, so `Standard`-located keys never alias
/// here. Returns `None` for keys with no keypad form (the caller falls back
/// to [`ext_functional_key`]).
///
/// `KP_BEGIN` (`57427`) has no stable winit source and stays deferred (see
/// the module coverage declaration).
pub const fn ext_keypad_named_key(named: NamedKey) -> Option<(u32, u8)> {
    let pair = match named {
        NamedKey::Enter => (57414, b'u'),
        NamedKey::Insert => (57425, b'u'),
        NamedKey::Delete => (57426, b'u'),
        NamedKey::ArrowLeft => (57417, b'u'),
        NamedKey::ArrowRight => (57418, b'u'),
        NamedKey::ArrowUp => (57419, b'u'),
        NamedKey::ArrowDown => (57420, b'u'),
        NamedKey::PageUp => (57421, b'u'),
        NamedKey::PageDown => (57422, b'u'),
        NamedKey::Home => (57423, b'u'),
        NamedKey::End => (57424, b'u'),
        _ => return None,
    };
    Some(pair)
}

/// Maps a numpad-located character key's logical text to its Kitty
/// keyboard-protocol keypad code (CTX-0755, Issue #1362).
///
/// winit delivers numpad digits and symbols as `Character` keys with
/// [`KeyLocation::Numpad`]; the spec assigns them `KP_0..=KP_9`
/// (`57399..=57408`), `KP_DECIMAL` (`57409`), `KP_DIVIDE` (`57410`),
/// `KP_MULTIPLY` (`57411`), `KP_SUBTRACT` (`57412`), `KP_ADD` (`57413`),
/// and `KP_EQUAL` (`57415`). The input must be exactly one character;
/// anything else returns `None` so the caller keeps the text path.
/// `KP_SEPARATOR` (`57416`) has no stable single-character source across
/// layouts and stays deferred (see the module coverage declaration).
pub fn ext_keypad_char_key(logical: &str) -> Option<u32> {
    let mut chars = logical.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let code = match first {
        '0' => 57399,
        '1' => 57400,
        '2' => 57401,
        '3' => 57402,
        '4' => 57403,
        '5' => 57404,
        '6' => 57405,
        '7' => 57406,
        '8' => 57407,
        '9' => 57408,
        '.' => 57409,
        '/' => 57410,
        '*' => 57411,
        '-' => 57412,
        '+' => 57413,
        '=' => 57415,
        _ => return None,
    };
    Some(code)
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
        // CTX-0264 shell-safety: bare editing/navigation keys and the top
        // of the F-row keep their legacy bytes (the chrome intercept never
        // consumes bare presses, so these paths must stay pinned).
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Insert,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[2~".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::Delete,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::PageUp,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(
                NamedKey::PageDown,
                None,
                PressState::Pressed,
                false
            )),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            encode_key_event(&make_named(NamedKey::F12, None, PressState::Pressed, false)),
            Some(b"\x1b[24~".to_vec())
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

    #[test]
    fn ext_functional_table_matches_the_spec() {
        // Spot-check the authoritative "Functional key definitions" rows.
        assert_eq!(ext_functional_key(NamedKey::Escape), Some((27, b'u')));
        assert_eq!(ext_functional_key(NamedKey::Enter), Some((13, b'u')));
        assert_eq!(ext_functional_key(NamedKey::Tab), Some((9, b'u')));
        assert_eq!(ext_functional_key(NamedKey::Backspace), Some((127, b'u')));
        assert_eq!(ext_functional_key(NamedKey::Insert), Some((2, b'~')));
        assert_eq!(ext_functional_key(NamedKey::Delete), Some((3, b'~')));
        assert_eq!(ext_functional_key(NamedKey::ArrowUp), Some((1, b'A')));
        assert_eq!(ext_functional_key(NamedKey::Home), Some((1, b'H')));
        assert_eq!(ext_functional_key(NamedKey::End), Some((1, b'F')));
        assert_eq!(ext_functional_key(NamedKey::F1), Some((1, b'P')));
        assert_eq!(ext_functional_key(NamedKey::F3), Some((13, b'~')));
        assert_eq!(ext_functional_key(NamedKey::F12), Some((24, b'~')));
        assert_eq!(ext_functional_key(NamedKey::F13), Some((57376, b'u')));
        assert_eq!(ext_functional_key(NamedKey::Space), None);
    }

    #[test]
    fn ext_modifier_table_covers_left_and_right() {
        assert_eq!(
            ext_modifier_key(NamedKey::Shift, KeyLocation::Standard),
            Some(57441)
        );
        assert_eq!(
            ext_modifier_key(NamedKey::Shift, KeyLocation::Right),
            Some(57447)
        );
        assert_eq!(
            ext_modifier_key(NamedKey::Control, KeyLocation::Left),
            Some(57442)
        );
        assert_eq!(
            ext_modifier_key(NamedKey::Control, KeyLocation::Right),
            Some(57448)
        );
        // AltGraph folds to the alt variant.
        assert_eq!(
            ext_modifier_key(NamedKey::AltGraph, KeyLocation::Left),
            Some(57443)
        );
        assert_eq!(
            ext_modifier_key(NamedKey::Meta, KeyLocation::Right),
            Some(57452)
        );
        assert_eq!(
            ext_modifier_key(NamedKey::Enter, KeyLocation::Standard),
            None
        );
    }

    #[test]
    fn ext_functional_table_covers_media_and_volume_keys() {
        // CTX-0755: media/volume rows from the spec's functional table.
        assert_eq!(ext_functional_key(NamedKey::MediaPlay), Some((57428, b'u')));
        assert_eq!(
            ext_functional_key(NamedKey::MediaPause),
            Some((57429, b'u'))
        );
        assert_eq!(
            ext_functional_key(NamedKey::MediaPlayPause),
            Some((57430, b'u'))
        );
        assert_eq!(ext_functional_key(NamedKey::MediaStop), Some((57432, b'u')));
        assert_eq!(
            ext_functional_key(NamedKey::MediaTrackNext),
            Some((57435, b'u'))
        );
        assert_eq!(
            ext_functional_key(NamedKey::MediaTrackPrevious),
            Some((57436, b'u'))
        );
        assert_eq!(
            ext_functional_key(NamedKey::AudioVolumeDown),
            Some((57438, b'u'))
        );
        assert_eq!(
            ext_functional_key(NamedKey::AudioVolumeUp),
            Some((57439, b'u'))
        );
        assert_eq!(
            ext_functional_key(NamedKey::AudioVolumeMute),
            Some((57440, b'u'))
        );
        // Keys the spec assigns no codes to keep the legacy path.
        assert_eq!(ext_functional_key(NamedKey::BrowserBack), None);
        assert_eq!(ext_functional_key(NamedKey::LaunchMail), None);
        assert_eq!(ext_functional_key(NamedKey::Space), None);
    }

    #[test]
    fn ext_keypad_named_table_covers_navigation_and_enter() {
        // CTX-0755: numpad-located named keys decode to KP_* codes.
        assert_eq!(ext_keypad_named_key(NamedKey::Enter), Some((57414, b'u')));
        assert_eq!(
            ext_keypad_named_key(NamedKey::ArrowLeft),
            Some((57417, b'u'))
        );
        assert_eq!(
            ext_keypad_named_key(NamedKey::ArrowRight),
            Some((57418, b'u'))
        );
        assert_eq!(ext_keypad_named_key(NamedKey::ArrowUp), Some((57419, b'u')));
        assert_eq!(
            ext_keypad_named_key(NamedKey::ArrowDown),
            Some((57420, b'u'))
        );
        assert_eq!(ext_keypad_named_key(NamedKey::PageUp), Some((57421, b'u')));
        assert_eq!(
            ext_keypad_named_key(NamedKey::PageDown),
            Some((57422, b'u'))
        );
        assert_eq!(ext_keypad_named_key(NamedKey::Home), Some((57423, b'u')));
        assert_eq!(ext_keypad_named_key(NamedKey::End), Some((57424, b'u')));
        assert_eq!(ext_keypad_named_key(NamedKey::Insert), Some((57425, b'u')));
        assert_eq!(ext_keypad_named_key(NamedKey::Delete), Some((57426, b'u')));
        // No keypad form: the caller falls back to the standard table.
        assert_eq!(ext_keypad_named_key(NamedKey::Tab), None);
        assert_eq!(ext_keypad_named_key(NamedKey::F5), None);
        assert_eq!(ext_keypad_named_key(NamedKey::Space), None);
    }

    #[test]
    fn ext_keypad_char_table_covers_digits_and_symbols() {
        // CTX-0755: numpad digits/symbols decode to KP_* codes.
        for (index, digit) in ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9']
            .iter()
            .enumerate()
        {
            assert_eq!(
                ext_keypad_char_key(&digit.to_string()),
                Some(57399 + index as u32),
                "numpad {digit} must map to KP_{digit}"
            );
        }
        let symbols: &[(&str, u32)] = &[
            (".", 57409),
            ("/", 57410),
            ("*", 57411),
            ("-", 57412),
            ("+", 57413),
            ("=", 57415),
        ];
        for (logical, code) in symbols {
            assert_eq!(
                ext_keypad_char_key(logical),
                Some(*code),
                "numpad {logical} must map to {code}"
            );
        }
        // Multi-character input and plain letters keep the text path.
        assert_eq!(ext_keypad_char_key("ab"), None);
        assert_eq!(ext_keypad_char_key(""), None);
        assert_eq!(ext_keypad_char_key("a"), None);
        assert_eq!(ext_keypad_char_key(","), None);
    }
}
