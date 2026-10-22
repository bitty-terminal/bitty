//! CTX-1267 / issue #2142 headless IME-event simulation for the fcitx5 batch.
//!
//! Three sub-items, each verified headlessly without a display server:
//! (a) Shift alone never produces PTY bytes, even under Kitty report-all,
//!     so the fcitx5 Shift toggle always reaches the IME;
//! (b) a commit carrying a trailing ASCII space from a real composition
//!     (claim armed) is trimmed to the composed text, so no synthetic
//!     trailing space survives; unsolicited commits preserve verbatim;
//! (c) a bare Delete release under Kitty report-events produces no bytes,
//!     so one tap deletes exactly once (press only).

#![forbid(unsafe_code)]

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::Runtime;

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

fn send(rt: &mut Runtime, bytes: &[u8]) {
    rt.handle_pty_bytes(bytes);
}

#[test]
fn shift_alone_never_produces_pty_bytes_even_under_report_all() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    send(&mut rt, b"\x1b[=8u");
    let out = rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    assert_eq!(out, None, "Shift alone must reach the IME, never the PTY");
    assert_eq!(rt.drain_pending_input(), b"", "no Shift bytes buffered");
    let rel = rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    assert_eq!(rel, None, "Shift release is also IME-owned");
}

#[test]
fn composed_commit_trims_single_trailing_space_but_unsolicited_preserves() {
    // Real composition (claim armed): fcitx5 Space-commit carries the
    // committing Space inside the commit text on some builds; trim it.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("nihao".to_string()), Some(5));
    rt.handle_ime_commit("你好 ".to_string());
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("你好"),
        "composed commit must not carry a trailing space (#2142b)"
    );
    // Unsolicited commit (no composition, claim Free): deliberate trailing
    // space is user input and survives verbatim (Terminal Truth).
    let mut rt2 = Runtime::with_defaults().expect("headless runtime must build");
    rt2.handle_ime_commit("你好 ".to_string());
    assert_eq!(
        std::str::from_utf8(&rt2.drain_pending_input()),
        Ok("你好 "),
        "unsolicited trailing space is deliberate input and survives"
    );
}

#[test]
fn shift_toggle_commit_does_not_swallow_the_next_space() {
    // Owner reproduction: type Chinese, press Shift to switch back to
    // English. fcitx5 commits/closes the composition on the Shift press, so
    // the one-shot commit claim is armed; its echo is a modifier, not a text
    // key. The claim must not eat the first real keystroke afterwards.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("nihao".to_string()), Some(5));
    rt.handle_ime_commit(String::new());
    let press = rt.handle_key_event(named_key(NamedKey::Shift, PressState::Pressed));
    assert_eq!(press, None, "Shift toggle must not produce bytes");
    let release = rt.handle_key_event(named_key(NamedKey::Shift, PressState::Released));
    assert_eq!(release, None, "Shift release must not produce bytes");
    let space = rt.handle_key_event(named_key(NamedKey::Space, PressState::Pressed));
    assert_eq!(
        space,
        Some(b" ".to_vec()),
        "the first space after the Shift toggle must reach the PTY"
    );
    assert_eq!(rt.drain_pending_input(), b" ", "exactly one space is owed");
}

#[test]
fn delete_release_under_report_events_produces_no_bytes() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    send(&mut rt, b"\x1b[=2u");
    let press = rt.handle_key_event(named_key(NamedKey::Delete, PressState::Pressed));
    assert_eq!(
        press,
        Some(b"\x1b[3~".to_vec()),
        "Delete press still deletes once"
    );
    let release = rt.handle_key_event(named_key(NamedKey::Delete, PressState::Released));
    assert_eq!(
        release, None,
        "bare Delete release must not delete a second time (#2142c)"
    );
    assert_eq!(
        rt.drain_pending_input(),
        b"\x1b[3~",
        "one tap produces exactly one Delete"
    );
}
