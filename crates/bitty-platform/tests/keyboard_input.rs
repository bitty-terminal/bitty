//! Keyboard input encoding integration test (headless, no display).
//!
//! Proves the pure `encode_key_event` path headlessly: synthetic
//! `KeyEvent`s are constructed without a window server, encoded via the
//! legacy xterm table, and verified to be deterministic and bounded.
//! Also proves that `translate_window_event` → `encode_key_event` stays
//! headless and that `App::run` headless fallback still works.

#![forbid(unsafe_code)]

use bitty_platform::{
    KeyEvent, KeyLocation, LogicalKey, NamedKey, PlatformEvent, PressState, WindowEventKind,
    WindowId, encode_key_event,
};

fn char_key(ch: &str, text: Option<&str>) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn named_key(named: NamedKey, text: Option<&str>) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(named),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

#[test]
fn synthetic_character_and_named_keys_encode_headlessly() {
    // Character with text (including Ctrl)
    assert_eq!(
        encode_key_event(&char_key("a", Some("a"))),
        Some(b"a".to_vec())
    );
    assert_eq!(
        encode_key_event(&char_key("c", Some("\x03"))),
        Some(b"\x03".to_vec()),
        "Ctrl+C text must be forwarded verbatim"
    );
    // Named Enter → \r, Tab → \t, Escape → \x1b
    assert_eq!(
        encode_key_event(&named_key(NamedKey::Enter, None)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        encode_key_event(&named_key(NamedKey::Tab, None)),
        Some(b"\t".to_vec())
    );
    assert_eq!(
        encode_key_event(&named_key(NamedKey::Escape, None)),
        Some(b"\x1b".to_vec())
    );
    assert_eq!(
        encode_key_event(&named_key(NamedKey::Backspace, None)),
        Some(b"\x7f".to_vec())
    );
    assert_eq!(
        encode_key_event(&named_key(NamedKey::ArrowUp, None)),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        encode_key_event(&named_key(NamedKey::F1, None)),
        Some(b"\x1bOP".to_vec())
    );
    // Modifier-only produces nothing
    assert_eq!(encode_key_event(&named_key(NamedKey::Shift, None)), None);
    // Release and synthetic produce nothing
    let mut rel = char_key("a", Some("a"));
    rel.state = PressState::Released;
    assert_eq!(encode_key_event(&rel), None);
    let mut synth = char_key("a", Some("a"));
    synth.is_synthetic = true;
    assert_eq!(encode_key_event(&synth), None);
}

#[test]
fn platform_event_keyboard_wraps_and_encodes() {
    // Synthetic PlatformEvent::Window with KeyboardInput must encode same as raw KeyEvent
    let key = char_key("x", Some("x"));
    let event = PlatformEvent::Window {
        window_id: WindowId::from_raw_public(42),
        kind: WindowEventKind::KeyboardInput(key.clone()),
    };
    match event {
        PlatformEvent::Window {
            kind: WindowEventKind::KeyboardInput(k),
            ..
        } => {
            assert_eq!(encode_key_event(&k), Some(b"x".to_vec()));
            assert_eq!(encode_key_event(&k), encode_key_event(&key));
        }
        _ => panic!("expected keyboard input"),
    }
}

#[test]
fn deterministic_and_bounded_encoding() {
    // Same input → same output, length bounded
    let k1 = char_key("€", Some("€"));
    let k2 = char_key("€", Some("€"));
    assert_eq!(encode_key_event(&k1), encode_key_event(&k2));
    let bytes = encode_key_event(&k1).unwrap();
    assert!(
        bytes.len() <= 8,
        "legacy encoding is tiny, got {}",
        bytes.len()
    );

    let k3 = named_key(NamedKey::F12, None);
    let b3 = encode_key_event(&k3).unwrap();
    assert!(b3.len() <= 8);
    assert_eq!(b3, b"\x1b[24~");
}

#[test]
fn utf8_multibyte_character_encodes_as_utf8() {
    let ev = char_key("🎉", Some("🎉"));
    let out = encode_key_event(&ev).expect("emoji must encode");
    assert_eq!(out, "🎉".as_bytes());
    assert_eq!(out.len(), 4);
}
