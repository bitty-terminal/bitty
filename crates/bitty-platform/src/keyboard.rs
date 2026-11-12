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
//! - In the legacy path, `Shift+Enter` intentionally remains `CR`: without an
//!   application negotiation there is no compatible byte that distinguishes it
//!   from ordinary Enter. Under the negotiated Kitty protocol, the enhanced
//!   path emits `CSI 13;2u` for `Shift+Enter` while bare Enter keeps `CR`.
//! - Kitty keyboard protocol is M1 opt-in, not M1-required
//!   (`compatibility-milestone-rfc`): this module owns the legacy baseline
//!   and the Kitty code tables ([`ext_functional_key`],
//!   [`ext_modifier_key`], [`ext_keypad_named_key`],
//!   [`ext_keypad_char_key`]); [`encode_key_event_kitty_protocol`] composes
//!   them under the negotiated flags (CTX-0947) for the runtime's enhanced
//!   path. The legacy path here is the
//!   byte-identical fallback when the protocol is off.
//! - xterm `modifyOtherKeys` (Pp = 4) is the compatible sibling of the Kitty
//!   protocol: [`encode_key_event_modify_other_keys`] emits
//!   `CSI 27 ; {1+mask} ; {codepoint} ~` for ordinary keys and appends the
//!   modifier parameter to keys that already have their own CSI form. It is
//!   the only negotiated form that survives tmux and the VS Code terminal.
//!   Precedence is Kitty first: a non-zero Kitty flag register wins and the
//!   `modifyOtherKeys` level is ignored (kitty itself dropped the xterm
//!   option), then the level, then the legacy bytes.
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
///   `ctrl_control_byte` without consulting `text`: `Ctrl+A-Z` →
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
///
/// For terminal-mode-aware encoding (application cursor keys), use
/// [`encode_key_event_with_terminal_modes`] instead.
pub fn encode_key_event_with_modifiers(
    event: &KeyEvent,
    modifiers: &ModifiersState,
) -> Option<Vec<u8>> {
    encode_key_event_with_terminal_modes(event, modifiers, false)
}

/// Encodes `event` with modifiers and terminal mode state (CTX-0826).
///
/// This extends [`encode_key_event_with_modifiers`] with terminal mode
/// awareness. When `application_cursor_keys` is true, arrow keys and Home/End
/// encode in application mode (ESC OA/B/C/D/H/F) per DECCKM; otherwise normal
/// mode (ESC [A/B/C/D/H/F).
pub fn encode_key_event_with_terminal_modes(
    event: &KeyEvent,
    modifiers: &ModifiersState,
    application_cursor_keys: bool,
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
            // Issue #3096: match Ghostty/foot by encoding Shift+Enter and
            // Ctrl+Enter (with optional Alt) as `\x1b[27;<mod>;13~` even
            // when neither Kitty keyboard protocol nor xterm modifyOtherKeys
            // was negotiated, while keeping bare Enter as `\r`, Super+Enter as
            // `\r` (matching Runtime::modifier_snapshot), and pure Alt+Enter
            // as `\x1b\r`.
            if *named == NamedKey::Enter
                && event.location != KeyLocation::Numpad
                && (modifiers.shift || modifiers.control)
            {
                let mask = u8::from(modifiers.shift)
                    | (u8::from(modifiers.alt) << 1)
                    | (u8::from(modifiers.control) << 2);
                return Some(format!("\x1b[27;{};13~", 1 + mask).into_bytes());
            }
            if let Some(seq) = encode_named_key(*named, application_cursor_keys) {
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
/// `application_cursor_keys`: When true, encodes cursor arrows in application
/// mode (`ESC OA` through `ESC OD` per DECCKM), else normal mode (`ESC [A`
/// through `ESC [D`). Home/End follow the same rule.
///
/// Returns `None` for modifier-only or unmapped keys that should not emit.
pub fn encode_named_key(named: NamedKey, application_cursor_keys: bool) -> Option<&'static [u8]> {
    match named {
        NamedKey::Enter => Some(b"\r"),
        NamedKey::Tab => Some(b"\t"),
        NamedKey::Backspace => Some(b"\x7f"),
        NamedKey::Delete => Some(b"\x1b[3~"),
        NamedKey::Insert => Some(b"\x1b[2~"),
        NamedKey::Home => {
            if application_cursor_keys {
                Some(b"\x1bOH")
            } else {
                Some(b"\x1b[H")
            }
        }
        NamedKey::End => {
            if application_cursor_keys {
                Some(b"\x1bOF")
            } else {
                Some(b"\x1b[F")
            }
        }
        NamedKey::PageUp => Some(b"\x1b[5~"),
        NamedKey::PageDown => Some(b"\x1b[6~"),
        NamedKey::ArrowUp => {
            if application_cursor_keys {
                Some(b"\x1bOA")
            } else {
                Some(b"\x1b[A")
            }
        }
        NamedKey::ArrowDown => {
            if application_cursor_keys {
                Some(b"\x1bOB")
            } else {
                Some(b"\x1b[B")
            }
        }
        NamedKey::ArrowRight => {
            if application_cursor_keys {
                Some(b"\x1bOC")
            } else {
                Some(b"\x1b[C")
            }
        }
        NamedKey::ArrowLeft => {
            if application_cursor_keys {
                Some(b"\x1bOD")
            } else {
                Some(b"\x1b[D")
            }
        }
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

/// Kitty keyboard-protocol progressive enhancement flags (CTX-0947, Issue #1665).
///
/// Values match `sw.kovidgoyal.net/kitty/keyboard-protocol` and mirror the
/// runtime `EXT_KEY_FLAG_*` bits and the negotiated `enhanced_keyboard`
/// register: bit 1 disambiguates `Esc`/modified ASCII keys, bit 2 reports
/// repeat/release event types, bit 4 reports shifted alternates, bit 8
/// reports all keys as escape codes, bit 16 embeds associated text.
pub const KITTY_FLAG_DISAMBIGUATE: u32 = 1 << 0;
/// Kitty keyboard flag: report repeat (`:2`) and release (`:3`) event types.
pub const KITTY_FLAG_REPORT_EVENTS: u32 = 1 << 1;
/// Kitty keyboard flag: report the shifted alternate key.
pub const KITTY_FLAG_REPORT_ALTERNATES: u32 = 1 << 2;
/// Kitty keyboard flag: report text-producing keys as escape codes.
pub const KITTY_FLAG_REPORT_ALL_KEYS: u32 = 1 << 3;
/// Kitty keyboard flag: embed associated text codepoints (only with all-keys).
pub const KITTY_FLAG_REPORT_TEXT: u32 = 1 << 4;

/// Kitty keyboard-protocol modifier bits carried in the `modifiers` argument
/// of [`encode_key_event_kitty_protocol`] (shift `1`, alt `2`, ctrl `4`,
/// super `8`, hyper `16`, meta `32`); the wire encodes the mask plus one.
pub const KITTY_MOD_SHIFT: u32 = 1;
/// Kitty modifier bit: alt.
pub const KITTY_MOD_ALT: u32 = 1 << 1;
/// Kitty modifier bit: control.
pub const KITTY_MOD_CTRL: u32 = 1 << 2;
/// Kitty modifier bit: super.
pub const KITTY_MOD_SUPER: u32 = 1 << 3;
/// Kitty modifier bit: hyper.
pub const KITTY_MOD_HYPER: u32 = 1 << 4;
/// Kitty modifier bit: meta.
pub const KITTY_MOD_META: u32 = 1 << 5;

/// Maximum bytes of one Kitty keyboard-protocol frame (CTX-0947).
///
/// Matches the `input-pointer-rfc.md` Kitty bound: one key yields at most 64
/// bytes of `CSI u`; an over-bound frame is dropped (`None`), never truncated.
pub const MAX_KITTY_FRAME_BYTES: usize = 64;

/// Encodes `event` under the Kitty keyboard protocol (CTX-0947, Issue #1665).
///
/// Pure, headless, and deterministic: no display server, window, or OS state
/// is consulted beyond `event`, the `modifiers` bitmask
/// (`KITTY_MOD_*`: shift `1`, alt `2`, ctrl `4`, super `8`, hyper `16`,
/// meta `32`), and the negotiated progressive-enhancement `flags`
/// (`KITTY_FLAG_*`: disambiguate `1`, report-events `2`, alternates `4`,
/// all-keys `8`, associated-text `16`).
///
/// Returns `Some(frame)` when a `CSI` frame applies, `None` when the caller
/// must use the legacy encoder instead: `flags == 0`, synthetic events,
/// releases without report-events, shift-only text keys under disambiguate,
/// and the bare `Enter`/`Tab`/`Backspace` legacy exception (the spec keeps
/// their legacy bytes so a crashed program can still `reset`). Modified
/// functional keys, including `Shift+Enter`, use the CSI-u frame so their
/// modifier is not collapsed into the legacy key byte. The encoding mirrors
/// the reference `serialize`/`encode_key` gates
/// (`kitty/key_encoding.c`): the key code is the un-shifted codepoint, the
/// modifier field (`mods + 1`) is omitted when empty, repeat carries `:2`
/// and release `:3`, and associated text is embedded only with all-keys.
///
/// Every emitted frame is bounded to [`MAX_KITTY_FRAME_BYTES`]; an over-bound
/// frame yields `None` (drop) rather than a truncated sequence.
pub fn encode_key_event_kitty_protocol(
    event: &KeyEvent,
    modifiers: u32,
    flags: u32,
) -> Option<Vec<u8>> {
    if flags == 0 {
        return None;
    }
    if event.is_synthetic {
        return None;
    }
    let report_events = flags & KITTY_FLAG_REPORT_EVENTS != 0;
    let disambiguate = flags & KITTY_FLAG_DISAMBIGUATE != 0;
    let report_all = flags & KITTY_FLAG_REPORT_ALL_KEYS != 0;
    let report_alternates = flags & KITTY_FLAG_REPORT_ALTERNATES != 0;
    let report_text = flags & KITTY_FLAG_REPORT_TEXT != 0;

    let pressed = event.state == PressState::Pressed;
    if !pressed && !report_events {
        return None;
    }
    // Spec legacy mode: text passes through untouched unless disambiguate,
    // report-events, or report-all-keys is active.
    let legacy_mode = !disambiguate && !report_events && !report_all;

    let shift = modifiers & KITTY_MOD_SHIFT != 0;
    let alt = modifiers & KITTY_MOD_ALT != 0;
    let ctrl = modifiers & KITTY_MOD_CTRL != 0;
    let super_held = modifiers & KITTY_MOD_SUPER != 0;
    let hyper = modifiers & KITTY_MOD_HYPER != 0;
    let meta = modifiers & KITTY_MOD_META != 0;
    // A shift-only text key is *not* disambiguated (the shifted glyph is
    // already the text); only alt/ctrl/super/hyper/meta chords are, matching
    // the spec's Disambiguate escape codes list.
    let disambiguated_chord = alt || ctrl || super_held || hyper || meta;

    // Repeat (2) and release (3) carry `:n`; press (1) is the default.
    let event_type = if !report_events {
        None
    } else if event.repeat {
        Some(2)
    } else if pressed {
        None
    } else {
        Some(3)
    };

    // Text-producing character keys.
    if let LogicalKey::Character(chars) = &event.logical_key {
        let base = chars.chars().next()?;
        // The protocol key code is always the un-shifted codepoint.
        let plain_code = u32::from(base.to_lowercase().next().unwrap_or(base));
        // Numpad-located character keys decode to the dedicated `KP_*`
        // codes so applications can distinguish them from their non-keypad
        // equivalents once enhancement is active. Keypad codes carry no
        // shifted alternate.
        let keypad_code = if event.location == KeyLocation::Numpad {
            ext_keypad_char_key(chars)
        } else {
            None
        };
        let is_keypad = keypad_code.is_some();
        let report_associated_text = report_text && report_all;
        let multi_char = chars.chars().nth(1).is_some();
        // Issue #3122: non-ASCII or multi-codepoint text-producing character
        // events without a disambiguated modifier chord (for example remote
        // desktop X11/XWayland key injection or composed text delivered via
        // `LogicalKey::Character` such as CJK, emoji, or kaomoji) have no
        // physical key entry in the Kitty keyboard protocol table. When
        // associated text reporting (`REPORT_TEXT | REPORT_ALL_KEYS`) is not
        // active — or when the logical key carries multiple codepoints —
        // returning `None` falls back to the UTF-8 text path on press/repeat
        // and stays silent on release, matching Ghostty's `kittyKeyEntry`
        // fallback so TUIs enabling flags like `11` (`DISAMBIGUATE |
        // REPORT_EVENTS | REPORT_ALL_KEYS`) without `REPORT_TEXT` (`16`)
        // receive the UTF-8 text instead of dropping it into a text-less
        // `CSI <codepoint> u`. When `REPORT_TEXT` is active on a
        // single-codepoint non-ASCII key, keep the `CSI u` path and embed the
        // character codepoint on press/repeat even if `event.text` was omitted.
        if ((!chars.is_ascii() && !report_associated_text) || multi_char)
            && !disambiguated_chord
            && !is_keypad
        {
            return None;
        }
        let code = keypad_code.unwrap_or(plain_code);
        let shifted = if is_keypad {
            None
        } else if report_alternates && shift {
            base.to_uppercase()
                .next()
                .map(u32::from)
                .filter(|value| *value != code)
        } else {
            None
        };
        let embed = if report_associated_text {
            event
                .text
                .as_deref()
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    (pressed && !chars.is_ascii() && !disambiguated_chord && !is_keypad)
                        .then_some(chars.as_str())
                })
        } else {
            None
        };
        // The shifted alternate only decorates keys that are already
        // escape-coded; it never selects the escape form by itself.
        let simple_encoding_ok = event_type.is_none() && embed.is_none();
        // Numpad keys take the keypad-code form once disambiguate is
        // active, even with no modifiers held.
        let escape_for_keypad = is_keypad && disambiguate;
        if simple_encoding_ok
            && !report_all
            && !(disambiguate && disambiguated_chord)
            && !escape_for_keypad
        {
            return None;
        }
        return kitty_key_frame(code, shifted, modifiers, event_type, embed, b'u');
    }

    // Named keys.
    if let LogicalKey::Named(named) = &event.logical_key {
        // Space is a text-producing key (code 32) rather than a functional
        // key. It leaves the legacy path when report-all-keys asks for
        // escape codes, or when disambiguate must report a chord (e.g.
        // `ctrl+Space` -> `CSI 32;5u` instead of the legacy NUL). A bare or
        // shift-only Space stays text.
        if *named == NamedKey::Space {
            if event_type.is_none() && !report_all && !(disambiguate && disambiguated_chord) {
                return None;
            }
            let embed = if report_text && report_all {
                event.text.as_deref().filter(|text| !text.is_empty())
            } else {
                None
            };
            return kitty_key_frame(32, None, modifiers, event_type, embed, b'u');
        }
        if let Some(code) = ext_modifier_key(*named, event.location) {
            // CTX-1267 (#2142a): Shift alone is the fcitx5 IME toggle and
            // must always reach the IME, never the PTY — even under
            // report-all-keys. Other modifiers still report under
            // report-all; Shift never does.
            if *named == NamedKey::Shift {
                return None;
            }
            if !report_all {
                return None;
            }
            return kitty_key_frame(code, None, modifiers, event_type, None, b'u');
        }
        if let Some((code, trailer)) = ext_functional_key(*named) {
            // Numpad-located named keys substitute the dedicated `KP_*`
            // codes and then run the same gates below, except the bare
            // Enter/Tab/Backspace legacy exception (typed `reset` after a
            // crash) applies to the main keys only: a numpad key under
            // enhancement always takes the `CSI u` form so applications can
            // distinguish it.
            let is_keypad =
                event.location == KeyLocation::Numpad && ext_keypad_named_key(*named).is_some();
            let (code, trailer) = if is_keypad {
                ext_keypad_named_key(*named).unwrap_or((code, trailer))
            } else {
                (code, trailer)
            };
            if legacy_mode {
                return None;
            }
            // Escape keeps `0x1b` unless disambiguate/report-all asks for
            // the escape-coded form (spec: Esc-vs-sequence disambiguation).
            if *named == NamedKey::Escape && !disambiguate && !report_all {
                return None;
            }
            // Bare Enter/Tab/Backspace/Delete keep their legacy bytes under
            // disambiguate/report-events; report-all-keys encodes them. A
            // modified key is intentionally outside this exception: the
            // modifier is the distinction applications need (notably
            // Shift+Enter for an inserted TUI newline). The exception covers
            // the main keys only, never numpad substitutes.
            // CTX-1267 (#2142c): Delete joins the exception so a bare
            // release never reports `CSI 3;1:3~` — one tap deletes exactly
            // once (Backspace already behaved this way).
            let bare_legacy_exception = !is_keypad
                && modifiers == 0
                && matches!(
                    named,
                    NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace | NamedKey::Delete
                );
            if bare_legacy_exception && !report_all {
                return None;
            }
            return kitty_key_frame(code, None, modifiers, event_type, None, trailer);
        }
    }
    None
}

/// Builds one Kitty `CSI` key frame per the spec's `serialize` algorithm.
///
/// `modifiers` is the raw modifier bitmask (shift `1`, alt `2`, ctrl `4`,
/// ...); the wire encodes it as `modifiers + 1`. `event_type` (`:2`/`:3`) is
/// omitted for a press. `shifted` adds the alternate shifted-key sub-field
/// and `embed` appends the associated-text codepoint field when present.
/// `trailer` is `u` for `CSI u` keys and `~`/`A`/`B`/`C`/`D`/`H`/`F`/`P`/
/// `Q`/`S` for the functional-key table.
///
/// Returns `None` when the frame would exceed [`MAX_KITTY_FRAME_BYTES`]:
/// associated text is truncated to the codepoints that still fit, and the
/// frame is dropped (never truncated) past the bound.
fn kitty_key_frame(
    code: u32,
    shifted: Option<u32>,
    modifiers: u32,
    event_type: Option<u32>,
    embed: Option<&str>,
    trailer: u8,
) -> Option<Vec<u8>> {
    // Worst case per codepoint field is 1 separator + 10 decimal digits.
    const EMBED_CODEPOINT_MAX: usize = 11;
    let embed = embed.filter(|text| {
        text.chars().any(|ch| {
            let value = ch as u32;
            value >= 0x20 && !(0x7f..=0x9f).contains(&value)
        })
    });
    let mut out = String::with_capacity(MAX_KITTY_FRAME_BYTES);
    out.push_str("\x1b[");
    let second = modifiers != 0 || event_type.is_some();
    let third = embed.is_some();
    if code != 1 || shifted.is_some() || second || third {
        out.push_str(&code.to_string());
    }
    if let Some(shifted) = shifted {
        out.push(':');
        out.push_str(&shifted.to_string());
    }
    if second || third {
        out.push(';');
        if second {
            out.push_str(&modifiers.saturating_add(1).to_string());
            if let Some(event_type) = event_type {
                out.push(':');
                out.push_str(&event_type.to_string());
            }
        }
    }
    if let Some(text) = embed {
        let mut first = true;
        for ch in text.chars() {
            let value = ch as u32;
            // Associated text must not contain control codes (spec).
            if value < 0x20 || (0x7f..=0x9f).contains(&value) {
                continue;
            }
            // Reserve one byte for the trailer; stop before it would overflow.
            if out.len() + EMBED_CODEPOINT_MAX + 1 > MAX_KITTY_FRAME_BYTES {
                break;
            }
            out.push(if first { ';' } else { ':' });
            out.push_str(&value.to_string());
            first = false;
        }
    }
    out.push(char::from(trailer));
    let frame = out.into_bytes();
    if frame.len() <= MAX_KITTY_FRAME_BYTES {
        Some(frame)
    } else {
        None
    }
}

#[allow(dead_code)]
const fn encoded_len_bound() -> usize {
    MAX_ENCODED_LEN
}

// ---------------------------------------------------------------------------
// xterm `modifyOtherKeys` encoder (Pp = 4)
// ---------------------------------------------------------------------------

/// Maximum bytes of one xterm `modifyOtherKeys` frame.
///
/// Same per-key bound as [`MAX_KITTY_FRAME_BYTES`]: the longest legal frame
/// (`CSI 27 ; 33 ; 1114111 ~`) is 20 bytes, so 64 leaves ample room and the
/// drop-not-truncate rule stays identical for both negotiated protocols.
pub const MAX_MODIFY_OTHER_KEYS_FRAME_BYTES: usize = 64;

/// Highest xterm `modifyOtherKeys` level this encoder implements.
///
/// Mirrors the parser-side bound (`bitty_vt::MODIFY_OTHER_KEYS_MAX_LEVEL`):
/// levels `0` (off), `1` ("user") and `2` ("all modifiers"). The parser
/// rejects anything higher, and the encoder fails closed for it too, so an
/// unimplemented level never silently degrades into a different encoding.
/// The constant is repeated here because `bitty-platform` is the encoder
/// layer and does not depend on the parser crate.
pub const MODIFY_OTHER_KEYS_MAX_LEVEL: u8 = 2;

/// xterm modifier bits used by the `modifyOtherKeys` parameter.
///
/// The wire parameter is `1 + mask` with shift `1`, alt `2`, control `4`,
/// super `8`, hyper `16`, meta `32`. Shift/alt/control match xterm's own
/// `MOD_SHIFT`/`MOD_ALT`/`MOD_CTRL` bit values (its "Modifier codes" table
/// maps `Alt` to `3 = 1 + 2` and `Ctrl` to `5 = 1 + 4`); the super/hyper/meta
/// extension keeps the same positional order the Kitty protocol uses.
pub const XTERM_MOD_SHIFT: u32 = 1 << 0;
/// xterm modifier bit: alt.
pub const XTERM_MOD_ALT: u32 = 1 << 1;
/// xterm modifier bit: control.
pub const XTERM_MOD_CTRL: u32 = 1 << 2;
/// xterm modifier bit: super.
pub const XTERM_MOD_SUPER: u32 = 1 << 3;
/// xterm modifier bit: hyper.
pub const XTERM_MOD_HYPER: u32 = 1 << 4;
/// xterm modifier bit: meta.
pub const XTERM_MOD_META: u32 = 1 << 5;

/// Key class of an xterm "ordinary key" for `modifyOtherKeys` gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtermOrdinaryKey {
    /// Any key that produces text (characters and `Space`).
    Text,
    /// `Enter` (`CR`, codepoint 13).
    Enter,
    /// `Tab` (`HT`, codepoint 9).
    Tab,
    /// `Escape` (`ESC`, codepoint 27).
    Escape,
    /// `Backspace` (`DEL`, codepoint 127).
    Backspace,
}

impl XtermOrdinaryKey {
    /// The codepoint xterm puts in the third parameter (`input_char` in
    /// `input.c`: the keysym when below 256, otherwise the key's single
    /// byte, otherwise its Unicode value). `Text` supplies the value the key
    /// produces, so `Shift+a` reports `A` (65) and `Ctrl+a` reports `a` (97),
    /// exactly as xterm reports the keysym it looked up.
    const fn codepoint(self, text: Option<u32>) -> Option<u32> {
        match self {
            Self::Text => text,
            Self::Enter => Some(13),
            Self::Tab => Some(9),
            Self::Escape => Some(27),
            Self::Backspace => Some(127),
        }
    }
}

/// Classifies `event` as an xterm ordinary key, or `None` for keys outside
/// that bucket.
///
/// xterm's `TypeOfKeysym` routes cursor keys, the editing keypad, function
/// keys, keypad keys, modifier keys and "special" keys to their own
/// `modify*Keys` resources; only the remaining ordinary keys can reach
/// `ModifyOtherKeys`. Numpad-located keys are filtered out by the caller
/// because xterm handles them through `modifyKeypadKeys` (which sends the
/// plain keypad byte, never a `CSI 27` frame).
fn xterm_ordinary_key(event: &KeyEvent) -> Option<(XtermOrdinaryKey, Option<u32>)> {
    match &event.logical_key {
        LogicalKey::Character(chars) => {
            let codepoint = chars.chars().next().map(u32::from);
            Some((XtermOrdinaryKey::Text, codepoint))
        }
        LogicalKey::Named(named) => {
            let key = match named {
                NamedKey::Space => XtermOrdinaryKey::Text,
                NamedKey::Enter => XtermOrdinaryKey::Enter,
                NamedKey::Tab => XtermOrdinaryKey::Tab,
                NamedKey::Escape => XtermOrdinaryKey::Escape,
                NamedKey::Backspace => XtermOrdinaryKey::Backspace,
                _ => return None,
            };
            let codepoint = if key == XtermOrdinaryKey::Text {
                Some(32)
            } else {
                None
            };
            Some((key, codepoint))
        }
        LogicalKey::Dead(_) | LogicalKey::Unidentified => None,
    }
}

/// Decides whether `level` escape-codes this ordinary key with `modifiers`.
///
/// Mirrors xterm's `ModifyOtherKeys` switch (`input.c`), transcribed to the
/// documented levels:
///
/// - level 1 ("the usual shift- and control-modifiers work as expected, but
///   other modifiers (such as alt- and meta-modifiers) cause ordinary keys to
///   be encoded as if they were function-keys"): `Enter`/`Tab`/`Escape`
///   escape-code once any modifier is held (`result = (modify_parm != 0)`);
///   `Backspace` keeps its legacy byte (`case XK_BackSpace: result = False`);
///   text keys escape-code only when a modifier the legacy encoding cannot
///   express is held, so pure shift (the glyph already carries it) and pure
///   control (the C0 byte already carries it) stay legacy.
/// - level 2 ("all of the modifiers apply"): every ordinary key escape-codes
///   with any modifier; `Backspace` joins only with control held
///   (`case 2: case XK_BackSpace: if computeMaskedModifier(state,
///   ControlMask) result = True`).
fn xterm_level_encodes(level: u8, key: XtermOrdinaryKey, modifiers: u32) -> bool {
    match level {
        1 => match key {
            XtermOrdinaryKey::Enter | XtermOrdinaryKey::Tab | XtermOrdinaryKey::Escape => true,
            XtermOrdinaryKey::Backspace => false,
            XtermOrdinaryKey::Text => modifiers != XTERM_MOD_SHIFT && modifiers != XTERM_MOD_CTRL,
        },
        2 => match key {
            XtermOrdinaryKey::Backspace => modifiers & XTERM_MOD_CTRL != 0,
            XtermOrdinaryKey::Text
            | XtermOrdinaryKey::Enter
            | XtermOrdinaryKey::Tab
            | XtermOrdinaryKey::Escape => true,
        },
        _ => false,
    }
}

/// Maps a named key that keeps its own xterm control sequence to that
/// sequence's base parameters and final byte, so a modifier can be appended
/// as `CSI {params} ; {1+mask} {final}`.
///
/// xterm encodes function-key modifiers "as parameters appended before the
/// final character of the control sequence", forcing a first parameter of
/// `1` when the unmodified form has none ("force modifier to 2nd param"), and
/// "the SS3 sent before F1 through F4 is altered to CSI when sending a
/// function key modifier as a parameter". The base values match
/// [`encode_named_key`]; the cursor keys, the VT220 editing keypad
/// (`Insert`/`Delete`/`PageUp`/`PageDown`) and `F1`..`F20` are covered.
///
/// Returns `None` for every other key: ordinary keys go through
/// `CSI 27 ; ...`, and `F21`..`F35` have no legacy sequence at all.
fn xterm_modified_functional_key(named: NamedKey) -> Option<(&'static str, u8)> {
    let pair = match named {
        NamedKey::ArrowUp => ("1", b'A'),
        NamedKey::ArrowDown => ("1", b'B'),
        NamedKey::ArrowRight => ("1", b'C'),
        NamedKey::ArrowLeft => ("1", b'D'),
        NamedKey::Home => ("1", b'H'),
        NamedKey::End => ("1", b'F'),
        NamedKey::Insert => ("2", b'~'),
        NamedKey::Delete => ("3", b'~'),
        NamedKey::PageUp => ("5", b'~'),
        NamedKey::PageDown => ("6", b'~'),
        NamedKey::F1 => ("1", b'P'),
        NamedKey::F2 => ("1", b'Q'),
        NamedKey::F3 => ("1", b'R'),
        NamedKey::F4 => ("1", b'S'),
        NamedKey::F5 => ("15", b'~'),
        NamedKey::F6 => ("17", b'~'),
        NamedKey::F7 => ("18", b'~'),
        NamedKey::F8 => ("19", b'~'),
        NamedKey::F9 => ("20", b'~'),
        NamedKey::F10 => ("21", b'~'),
        NamedKey::F11 => ("23", b'~'),
        NamedKey::F12 => ("24", b'~'),
        NamedKey::F13 => ("25", b'~'),
        NamedKey::F14 => ("26", b'~'),
        NamedKey::F15 => ("28", b'~'),
        NamedKey::F16 => ("29", b'~'),
        NamedKey::F17 => ("31", b'~'),
        NamedKey::F18 => ("32", b'~'),
        NamedKey::F19 => ("33", b'~'),
        NamedKey::F20 => ("34", b'~'),
        _ => return None,
    };
    Some(pair)
}

/// Builds one `CSI 27 ; {1+mask} ; {codepoint} ~` frame, bounded and
/// drop-not-truncate.
fn modify_other_keys_frame(modifiers: u32, codepoint: u32) -> Option<Vec<u8>> {
    let frame = format!("\x1b[27;{};{}~", modifiers.saturating_add(1), codepoint).into_bytes();
    if frame.len() <= MAX_MODIFY_OTHER_KEYS_FRAME_BYTES {
        Some(frame)
    } else {
        None
    }
}

/// Builds one `CSI {params} ; {1+mask} {final}` frame for a key that keeps
/// its own xterm form, bounded and drop-not-truncate.
fn modify_functional_key_frame(params: &str, final_byte: u8, modifiers: u32) -> Option<Vec<u8>> {
    let mut frame = format!("\x1b[{params};{}", modifiers.saturating_add(1)).into_bytes();
    frame.push(final_byte);
    if frame.len() <= MAX_MODIFY_OTHER_KEYS_FRAME_BYTES {
        Some(frame)
    } else {
        None
    }
}

/// Encodes `event` under xterm's `modifyOtherKeys` option (Pp = 4).
///
/// Pure, headless, and deterministic: no display server, window, or OS state
/// is consulted beyond `event`, the `modifiers` bitmask (`XTERM_MOD_*`, the
/// same positional layout as `KITTY_MOD_*`), and the negotiated `level`
/// (`CSI > 4 ; Ps m`).
///
/// Returns `Some(frame)` when the xterm form applies and `None` when the
/// caller must use the legacy encoder instead:
///
/// - `level == 0` (off) or a level above the implemented maximum;
/// - synthetic events and key releases (the `CSI 27` form carries no event
///   type, so a release must stay silent like the legacy path);
/// - no modifier held (`min_mod` defaults to `1`; level 3, which escape-codes
///   unmodified keys, is not implemented);
/// - ordinary keys that the level keeps on the legacy path (see the
///   level table in `xterm_level_encodes`);
/// - keys outside both the functional table and the ordinary bucket.
///
/// Keys with their own xterm control sequence (cursor keys, `Home`/`End`,
/// the editing keypad, `F1`..`F20`) keep that sequence and gain the modifier
/// parameter, as xterm's `modifyCursorKeys`/`modifyFunctionKeys` do when
/// `modifyOtherKeys` is active ("result = True" in `allowModifierParm`).
/// Every other ordinary key is sent as `CSI 27 ; {1+mask} ; {codepoint} ~`
/// with the codepoint the key produces.
///
/// Every emitted frame is bounded to [`MAX_MODIFY_OTHER_KEYS_FRAME_BYTES`];
/// an over-bound frame yields `None` (drop), never a truncated sequence.
pub fn encode_key_event_modify_other_keys(
    event: &KeyEvent,
    modifiers: u32,
    level: u8,
) -> Option<Vec<u8>> {
    // Level 0 is off; levels above the implemented maximum fail closed rather
    // than being clamped into a different encoding.
    if level == 0 || level > MODIFY_OTHER_KEYS_MAX_LEVEL {
        return None;
    }
    if event.is_synthetic || event.state != PressState::Pressed {
        return None;
    }
    // xterm's `min_mod` defaults to 1: unmodified keys never escape-code
    // (that is level 3, which is not implemented).
    if modifiers == 0 {
        return None;
    }
    // Numpad-located keys belong to xterm's `modifyKeypadKeys` resource,
    // which keeps the plain keypad byte and never appends a modifier, so they
    // stay on the legacy path here.
    if event.location == KeyLocation::Numpad {
        return None;
    }

    if let LogicalKey::Named(named) = &event.logical_key {
        if let Some((params, final_byte)) = xterm_modified_functional_key(*named) {
            return modify_functional_key_frame(params, final_byte, modifiers);
        }
    }

    let (key, text) = xterm_ordinary_key(event)?;
    if !xterm_level_encodes(level, key, modifiers) {
        return None;
    }
    modify_other_keys_frame(modifiers, key.codepoint(text)?)
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
    fn legacy_modified_enter_matches_ghostty_and_foot() {
        // bitty#3096: a TUI that never negotiated the kitty keyboard protocol
        // (`CSI > 1 u`) or xterm `modifyOtherKeys` (`CSI > 4 ; 2 m`) still
        // needs to tell "insert newline" from "submit". Ghostty and foot
        // encode Shift/Ctrl+Enter as xterm modifyOtherKeys `CSI 27;<mod>;13~`
        // unconditionally, so bitty does the same. Bare Enter stays CR,
        // Super+Enter stays CR (a compositor chord, not a terminal one), and
        // pure Alt+Enter keeps xterm `metaSendsEscape` (ESC CR).
        let shift_mods = ModifiersState {
            shift: true,
            ..ModifiersState::NONE
        };
        let shift_alt_mods = ModifiersState {
            shift: true,
            alt: true,
            ..ModifiersState::NONE
        };
        let super_mods = ModifiersState {
            super_pressed: true,
            ..ModifiersState::NONE
        };
        let enter = make_named(NamedKey::Enter, None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &shift_mods),
            Some(b"\x1b[27;2;13~".to_vec())
        );
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &ctrl_mods()),
            Some(b"\x1b[27;5;13~".to_vec())
        );
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &shift_alt_mods),
            Some(b"\x1b[27;4;13~".to_vec())
        );
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &no_mods()),
            Some(b"\r".to_vec()),
            "bare Enter must stay CR"
        );
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &alt_mods()),
            Some(b"\x1b\r".to_vec()),
            "Alt+Enter keeps xterm metaSendsEscape"
        );
        assert_eq!(
            encode_key_event_with_modifiers(&enter, &super_mods),
            Some(b"\r".to_vec()),
            "Super+Enter is a compositor chord and stays legacy CR"
        );
        // Numpad Enter is the keypad's Enter, not the modified main Enter.
        let numpad_enter = KeyEvent {
            location: KeyLocation::Numpad,
            ..make_named(NamedKey::Enter, None, PressState::Pressed, false)
        };
        assert_eq!(
            encode_key_event_with_modifiers(&numpad_enter, &shift_mods),
            Some(b"\r".to_vec()),
            "numpad Enter stays legacy CR"
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

    // ------------------------------------------------------------------
    // CTX-0947 (Issue #1665): Kitty progressive encoder differential tests.
    // ------------------------------------------------------------------

    fn kitty_char(logical: &str, text: Option<&str>, state: PressState, repeat: bool) -> KeyEvent {
        KeyEvent {
            logical_key: LogicalKey::Character(logical.to_string()),
            text: text.map(|s| s.to_string()),
            location: KeyLocation::Standard,
            state,
            repeat,
            is_synthetic: false,
        }
    }

    fn kitty_named_located(
        named: NamedKey,
        location: KeyLocation,
        state: PressState,
        repeat: bool,
    ) -> KeyEvent {
        KeyEvent {
            logical_key: LogicalKey::Named(named),
            text: None,
            location,
            state,
            repeat,
            is_synthetic: false,
        }
    }

    fn kitty_named(named: NamedKey, state: PressState) -> KeyEvent {
        kitty_named_located(named, KeyLocation::Standard, state, false)
    }

    #[test]
    fn kitty_flags_zero_yields_no_frame() {
        // flags 0 means the caller keeps the byte-identical legacy path.
        let press = kitty_char("a", Some("a"), PressState::Pressed, false);
        assert_eq!(encode_key_event_kitty_protocol(&press, 0, 0), None);
        let release = kitty_char("a", None, PressState::Released, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&release, KITTY_MOD_CTRL, 0),
            None
        );
        let tab = kitty_named(NamedKey::Tab, PressState::Pressed);
        assert_eq!(encode_key_event_kitty_protocol(&tab, 0, 0), None);
    }

    #[test]
    fn kitty_synthetic_events_yield_no_frame() {
        let mut ev = kitty_char("a", Some("a"), PressState::Pressed, false);
        ev.is_synthetic = true;
        assert_eq!(
            encode_key_event_kitty_protocol(&ev, 0, KITTY_FLAG_REPORT_ALL_KEYS),
            None
        );
    }

    #[test]
    fn kitty_tab_vs_ctrl_i_under_disambiguate() {
        // Bare Tab keeps the legacy byte under disambiguate-only (spec
        // exception so a crashed program can still type `reset`); Ctrl+I
        // disambiguates to the `i` codepoint with the ctrl modifier.
        let tab = kitty_named(NamedKey::Tab, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(&tab, 0, KITTY_FLAG_DISAMBIGUATE),
            None,
            "bare Tab stays on the legacy path under disambiguate-only"
        );
        let ctrl_i = kitty_char("i", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&ctrl_i, KITTY_MOD_CTRL, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[105;5u".to_vec()),
            "Ctrl+I under disambiguate is CSI 105;5u"
        );
    }

    #[test]
    fn kitty_report_all_encodes_bare_tab_as_csi_9u() {
        // Report-all-keys lifts the Enter/Tab/Backspace legacy exception.
        // The zero-modifier field is omitted per the reference `serialize`
        // (`CSI 9u`, not `CSI 9;1u`): `;1` is mods + 1 with no mods held.
        let tab = kitty_named(NamedKey::Tab, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(
                &tab,
                0,
                KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_ALL_KEYS
            ),
            Some(b"\x1b[9u".to_vec())
        );
    }

    #[test]
    fn kitty_enter_vs_ctrl_m() {
        // Bare Enter keeps legacy `\r` under disambiguate-only; Ctrl+M
        // disambiguates to the `m` codepoint with the ctrl modifier.
        let enter = kitty_named(NamedKey::Enter, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(&enter, 0, KITTY_FLAG_DISAMBIGUATE),
            None,
            "bare Enter stays on the legacy path under disambiguate-only"
        );
        let ctrl_m = kitty_char("m", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&ctrl_m, KITTY_MOD_CTRL, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[109;5u".to_vec()),
            "Ctrl+M under disambiguate is CSI 109;5u"
        );
        // Report-all-keys encodes even the bare Enter (code 13).
        assert_eq!(
            encode_key_event_kitty_protocol(
                &enter,
                0,
                KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_ALL_KEYS
            ),
            Some(b"\x1b[13u".to_vec())
        );
    }

    #[test]
    fn kitty_esc_vs_ctrl_open_bracket() {
        // Bare Esc takes the escape-coded form once disambiguate is active;
        // Ctrl+[ disambiguates to the `[` codepoint with the ctrl modifier.
        let esc = kitty_named(NamedKey::Escape, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(&esc, 0, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[27u".to_vec())
        );
        let ctrl_bracket = kitty_char("[", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&ctrl_bracket, KITTY_MOD_CTRL, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[91;5u".to_vec()),
            "Ctrl+[ under disambiguate is CSI 91;5u"
        );
    }

    #[test]
    fn kitty_release_carries_event_type_3_when_requested() {
        let flags = KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_EVENTS;
        let release = kitty_char("a", None, PressState::Released, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&release, KITTY_MOD_CTRL, flags),
            Some(b"\x1b[97;5:3u".to_vec()),
            "key-up carries event type 3 when report-events is set"
        );
        // Without report-events the release produces no frame at all.
        assert_eq!(
            encode_key_event_kitty_protocol(&release, KITTY_MOD_CTRL, KITTY_FLAG_DISAMBIGUATE),
            None
        );
    }

    #[test]
    fn kitty_repeat_carries_event_type_2_when_requested() {
        let flags = KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_EVENTS;
        let repeat = kitty_char("a", None, PressState::Pressed, true);
        assert_eq!(
            encode_key_event_kitty_protocol(&repeat, KITTY_MOD_CTRL, flags),
            Some(b"\x1b[97;5:2u".to_vec()),
            "auto-repeat carries event type 2 when report-events is set"
        );
    }

    #[test]
    fn kitty_ctrl_shift_modifier_bits_combine() {
        // shift (1) | ctrl (4) is mask 5, wired as 5 + 1.
        let ev = kitty_char("a", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(
                &ev,
                KITTY_MOD_CTRL | KITTY_MOD_SHIFT,
                KITTY_FLAG_DISAMBIGUATE
            ),
            Some(b"\x1b[97;6u".to_vec())
        );
    }

    #[test]
    fn kitty_shift_only_text_stays_legacy_under_disambiguate() {
        // A shift-only text key is not disambiguated: the shifted glyph is
        // already the text, so no frame is produced.
        let ev = kitty_char("a", Some("A"), PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&ev, KITTY_MOD_SHIFT, KITTY_FLAG_DISAMBIGUATE),
            None
        );
    }

    #[test]
    fn kitty_shift_only_text_stays_legacy_with_alternates() {
        // The shifted alternate decorates escape-coded keys only; it never
        // selects the escape form by itself, so Shift+a stays text even
        // when the alternate flag is negotiated.
        let ev = kitty_char("a", Some("A"), PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(
                &ev,
                KITTY_MOD_SHIFT,
                KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_ALTERNATES
            ),
            None
        );
        assert_eq!(
            encode_key_event_kitty_protocol(&ev, KITTY_MOD_SHIFT, KITTY_FLAG_REPORT_ALTERNATES),
            None,
            "alternates alone must not escape-code a shifted text key"
        );
        // Under report-all-keys the escape form is selected by that flag
        // and the shifted alternate still decorates the frame.
        assert_eq!(
            encode_key_event_kitty_protocol(
                &ev,
                KITTY_MOD_SHIFT,
                KITTY_FLAG_REPORT_ALL_KEYS | KITTY_FLAG_REPORT_ALTERNATES
            ),
            Some(b"\x1b[97:65;2u".to_vec()),
            "report-all Shift+a carries the shifted alternate"
        );
    }

    #[test]
    fn kitty_space_repeat_and_release_reported_under_report_events() {
        let flags = KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_EVENTS;
        // A bare Space press stays on the legacy path (a plain " ").
        let press = kitty_named(NamedKey::Space, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(&press, 0, flags),
            None,
            "bare Space press stays text under report-events"
        );
        // Repeat and release carry their event types like any other key.
        let repeat = kitty_named_located(
            NamedKey::Space,
            KeyLocation::Standard,
            PressState::Pressed,
            true,
        );
        assert_eq!(
            encode_key_event_kitty_protocol(&repeat, 0, flags),
            Some(b"\x1b[32;1:2u".to_vec()),
            "Space repeat carries event type 2 when report-events is set"
        );
        let release = kitty_named(NamedKey::Space, PressState::Released);
        assert_eq!(
            encode_key_event_kitty_protocol(&release, 0, flags),
            Some(b"\x1b[32;1:3u".to_vec()),
            "Space release carries event type 3 when report-events is set"
        );
    }

    #[test]
    fn kitty_keypad_codes_under_disambiguate() {
        let digit = KeyEvent {
            logical_key: LogicalKey::Character("1".to_string()),
            text: Some("1".to_string()),
            location: KeyLocation::Numpad,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        };
        assert_eq!(
            encode_key_event_kitty_protocol(&digit, 0, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[57400u".to_vec()),
            "numpad 1 under disambiguate is KP_1"
        );
        // Numpad Enter is not covered by the bare-Enter legacy exception.
        let enter = kitty_named_located(
            NamedKey::Enter,
            KeyLocation::Numpad,
            PressState::Pressed,
            false,
        );
        assert_eq!(
            encode_key_event_kitty_protocol(&enter, 0, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[57414u".to_vec())
        );
    }

    #[test]
    fn kitty_frame_is_hard_bounded() {
        // A hostile-length associated text can never grow the frame past
        // the per-key bound; over-bound frames are dropped, never truncated.
        let long = "\u{1F600}".repeat(64);
        let ev = kitty_char("a", Some(&long), PressState::Pressed, false);
        let frame = encode_key_event_kitty_protocol(
            &ev,
            0,
            KITTY_FLAG_REPORT_ALL_KEYS | KITTY_FLAG_REPORT_TEXT,
        )
        .expect("frame must be emitted");
        assert!(frame.len() <= MAX_KITTY_FRAME_BYTES);
        assert_eq!(*frame.last().unwrap(), b'u');
    }

    #[test]
    fn kitty_control_only_embed_omits_dangling_separator_and_saturates_modifiers() {
        // When `embed` contains only C0/C1 control characters and no second
        // parameter is present, `kitty_key_frame` must not emit a dangling `;`.
        assert_eq!(
            kitty_key_frame(97, None, 0, None, Some("\x07\x1b\u{0085}"), b'u'),
            Some(b"\x1b[97u".to_vec())
        );
        assert_eq!(
            kitty_key_frame(97, None, 0, None, Some(""), b'u'),
            Some(b"\x1b[97u".to_vec())
        );
        // `u32::MAX` modifiers saturate instead of overflowing `modifiers + 1`.
        assert!(kitty_key_frame(97, None, u32::MAX, None, None, b'u').is_some());
        assert!(modify_other_keys_frame(u32::MAX, 97).is_some());
        assert!(modify_functional_key_frame("1", b'A', u32::MAX).is_some());
    }

    #[test]
    fn kitty_shift_enter_under_disambiguate_is_csi_13_2u() {
        // The exact sequence a TUI relies on to tell Shift+Enter (insert a
        // newline) from Enter (submit). Previously uncovered: only the bare
        // Enter and Ctrl+M paths had regression tests.
        let enter = kitty_named(NamedKey::Enter, PressState::Pressed);
        assert_eq!(
            encode_key_event_kitty_protocol(&enter, KITTY_MOD_SHIFT, KITTY_FLAG_DISAMBIGUATE),
            Some(b"\x1b[13;2u".to_vec())
        );
    }

    #[test]
    fn kitty_non_ascii_and_multi_char_text_falls_back_to_utf8_without_chord() {
        // Issue #3122: remote desktop (RustDesk) and X11/XWayland text
        // injection deliver CJK, emoji, and kaomoji as `LogicalKey::Character`
        // without a physical Kitty key entry. Under flags `11`
        // (`DISAMBIGUATE | REPORT_EVENTS | REPORT_ALL_KEYS`, no `REPORT_TEXT`),
        // unmodified or shift-only presses must return `None` from the Kitty
        // encoder so the caller emits raw UTF-8 text on press/repeat and
        // nothing on release, while a chord (`Ctrl+中`) still disambiguates.
        let flags = KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_EVENTS | KITTY_FLAG_REPORT_ALL_KEYS;
        for sample in ["中", "😀", "╰(°▽°)╯"] {
            let press = kitty_char(sample, Some(sample), PressState::Pressed, false);
            assert_eq!(
                encode_key_event_kitty_protocol(&press, 0, flags),
                None,
                "unmodified {sample:?} must fall back to UTF-8 text"
            );
            assert_eq!(
                encode_key_event_kitty_protocol(&press, KITTY_MOD_SHIFT, flags),
                None,
                "shift-only {sample:?} must fall back to UTF-8 text"
            );
            let release = kitty_char(sample, None, PressState::Released, false);
            assert_eq!(
                encode_key_event_kitty_protocol(&release, 0, flags),
                None,
                "release of {sample:?} must not emit a spurious CSI u frame"
            );
            assert_eq!(
                encode_key_event_with_terminal_modes(&press, &ModifiersState::default(), false),
                Some(sample.as_bytes().to_vec()),
                "fallback encoder emits raw UTF-8 for {sample:?}"
            );
        }
        let ctrl_cjk = kitty_char("中", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&ctrl_cjk, KITTY_MOD_CTRL, flags),
            Some(b"\x1b[20013;5u".to_vec()),
            "explicit Ctrl chord on a non-ASCII character still disambiguates"
        );

        // When `REPORT_TEXT` (`16`) is also negotiated (flags `27` =
        // `DISAMBIGUATE | REPORT_EVENTS | REPORT_ALL_KEYS | REPORT_TEXT`),
        // single-codepoint non-ASCII keys emit `CSI u` frames with embedded
        // associated text on press/repeat and no embedded text on release,
        // including when `event.text` is absent on synthetic X11/XWayland
        // key injection. Multi-codepoint sequences still fall back to raw UTF-8.
        let flags_with_text = flags | KITTY_FLAG_REPORT_TEXT;
        let e_acute_press = kitty_char("é", Some("é"), PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&e_acute_press, 0, flags_with_text),
            Some(b"\x1b[233;;233u".to_vec())
        );
        let e_acute_repeat = kitty_char("é", Some("é"), PressState::Pressed, true);
        assert_eq!(
            encode_key_event_kitty_protocol(&e_acute_repeat, 0, flags_with_text),
            Some(b"\x1b[233;1:2;233u".to_vec())
        );
        let e_acute_release = kitty_char("é", None, PressState::Released, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&e_acute_release, 0, flags_with_text),
            Some(b"\x1b[233;1:3u".to_vec())
        );
        let textless_cjk_press = kitty_char("中", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&textless_cjk_press, 0, flags_with_text),
            Some(b"\x1b[20013;;20013u".to_vec())
        );
        let kaomoji_press = kitty_char("╰(°▽°)╯", Some("╰(°▽°)╯"), PressState::Pressed, false);
        assert_eq!(
            encode_key_event_kitty_protocol(&kaomoji_press, 0, flags_with_text),
            None,
            "multi-codepoint text still falls back to raw UTF-8 under REPORT_TEXT"
        );
    }

    // ------------------------------------------------------------------
    // xterm modifyOtherKeys (Pp = 4) encoder tests.
    // ------------------------------------------------------------------

    fn mok_shift_enter() -> KeyEvent {
        kitty_named(NamedKey::Enter, PressState::Pressed)
    }

    #[test]
    fn modify_other_keys_level_one_encodes_shift_enter() {
        // xterm level 1: `Enter` escape-codes whenever a modifier is held
        // (`case XK_Return: result = (modify_parm != 0)`), so Shift+Enter
        // stops colliding with the bare `\r` submit.
        assert_eq!(
            encode_key_event_modify_other_keys(&mok_shift_enter(), XTERM_MOD_SHIFT, 1),
            Some(b"\x1b[27;2;13~".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_level_two_encodes_ctrl_a() {
        // Level 2 ("all of the modifiers apply"): the codepoint is the value
        // the key produces (`a` = 97) and the modifier parameter is `1 + 4`.
        let ctrl_a = kitty_char("a", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_modify_other_keys(&ctrl_a, XTERM_MOD_CTRL, 2),
            Some(b"\x1b[27;5;97~".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_arrow_keeps_its_csi_form_with_the_modifier() {
        // Cursor keys keep their own xterm sequence and gain the modifier
        // parameter ("parameters appended before the final character").
        let up = kitty_named(NamedKey::ArrowUp, PressState::Pressed);
        assert_eq!(
            encode_key_event_modify_other_keys(&up, XTERM_MOD_SHIFT, 1),
            Some(b"\x1b[1;2A".to_vec())
        );
        // SS3 forms are altered to CSI when a modifier is appended.
        let f1 = kitty_named(NamedKey::F1, PressState::Pressed);
        assert_eq!(
            encode_key_event_modify_other_keys(&f1, XTERM_MOD_CTRL, 2),
            Some(b"\x1b[1;5P".to_vec())
        );
        // Keys that already carry a numeric parameter append to it.
        let delete = kitty_named(NamedKey::Delete, PressState::Pressed);
        assert_eq!(
            encode_key_event_modify_other_keys(&delete, XTERM_MOD_CTRL, 1),
            Some(b"\x1b[3;5~".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_unmodified_text_stays_legacy() {
        // min_mod defaults to 1: an unmodified key never escape-codes (that
        // is xterm level 3, which is not implemented).
        let a = kitty_char("a", Some("a"), PressState::Pressed, false);
        assert_eq!(encode_key_event_modify_other_keys(&a, 0, 2), None);
        // And level 0 means the option is off.
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_CTRL, 0),
            None
        );
    }

    #[test]
    fn modify_other_keys_level_one_keeps_shift_and_ctrl_text_legacy() {
        // "the usual shift- and control-modifiers work as expected": the
        // glyph already carries shift and the C0 byte already carries
        // control, so both stay on the legacy path at level 1.
        let a = kitty_char("a", None, PressState::Pressed, false);
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_SHIFT, 1),
            None
        );
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_CTRL, 1),
            None
        );
        // Alt (a modifier with no legacy encoding for ordinary keys) escapes
        // the key, and so does the control+shift combination.
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_ALT, 1),
            Some(b"\x1b[27;3;97~".to_vec())
        );
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_CTRL | XTERM_MOD_SHIFT, 1),
            Some(b"\x1b[27;6;97~".to_vec())
        );
        // Backspace keeps its legacy byte at level 1 and joins only with
        // control at level 2.
        let backspace = kitty_named(NamedKey::Backspace, PressState::Pressed);
        assert_eq!(
            encode_key_event_modify_other_keys(&backspace, XTERM_MOD_CTRL, 1),
            None
        );
        assert_eq!(
            encode_key_event_modify_other_keys(&backspace, XTERM_MOD_CTRL, 2),
            Some(b"\x1b[27;5;127~".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_rejects_unimplemented_levels_and_events() {
        let a = kitty_char("a", None, PressState::Pressed, false);
        // Level 3 and beyond fail closed instead of being clamped.
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_CTRL, 3),
            None
        );
        assert_eq!(
            encode_key_event_modify_other_keys(&a, XTERM_MOD_CTRL, u8::MAX),
            None
        );
        // Releases and synthetic events carry no xterm encoding.
        let release = kitty_char("a", None, PressState::Released, false);
        assert_eq!(
            encode_key_event_modify_other_keys(&release, XTERM_MOD_CTRL, 2),
            None
        );
        let synthetic = make_char("a", None, PressState::Pressed, true);
        assert_eq!(
            encode_key_event_modify_other_keys(&synthetic, XTERM_MOD_CTRL, 2),
            None
        );
        // Numpad-located keys belong to xterm's `modifyKeypadKeys` resource
        // and keep the plain keypad byte.
        let keypad_enter = kitty_named_located(
            NamedKey::Enter,
            KeyLocation::Numpad,
            PressState::Pressed,
            false,
        );
        assert_eq!(
            encode_key_event_modify_other_keys(&keypad_enter, XTERM_MOD_SHIFT, 2),
            None
        );
    }

    #[test]
    fn modify_other_keys_frame_is_hard_bounded() {
        // Every frame stays inside the per-key bound and keeps its trailer.
        let frames = [
            encode_key_event_modify_other_keys(
                &kitty_char("\u{1F600}", None, PressState::Pressed, false),
                XTERM_MOD_SHIFT | XTERM_MOD_ALT | XTERM_MOD_CTRL,
                2,
            ),
            encode_key_event_modify_other_keys(
                &kitty_named(NamedKey::F20, PressState::Pressed),
                XTERM_MOD_SHIFT | XTERM_MOD_ALT | XTERM_MOD_CTRL | XTERM_MOD_META,
                2,
            ),
        ];
        for frame in frames {
            let frame = frame.expect("frame must be emitted");
            assert!(frame.len() <= MAX_MODIFY_OTHER_KEYS_FRAME_BYTES);
            assert_eq!(*frame.last().unwrap(), b'~');
        }
    }
}
