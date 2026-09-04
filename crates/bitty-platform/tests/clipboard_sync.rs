//! Headless clipboard + primary sync regression test (CTX-0160, issue #260).
//!
//! Runs on CI without X11/Wayland: `Clipboard::new_headless` never touches the
//! OS, so these assertions prove the sync contract deterministically. Live
//! display proof (select → `wl-paste`, `wl-copy` → paste) is manual evidence
//! recorded under `recording/ctx-0160/` and never asserted here.

use bitty_platform::Clipboard;
use bitty_platform::clipboard::{CLIPBOARD_MAX_BYTES, display_backend_hint, is_wayland_session};

#[test]
fn headless_set_syncs_clipboard_and_primary() {
    let mut cb = Clipboard::new_headless();
    cb.set_text("hello wayland".to_string())
        .expect("headless set succeeds");
    assert_eq!(cb.headless_contents(), "hello wayland");
    assert_eq!(cb.primary_contents(), "hello wayland");
    assert_eq!(cb.get_text().expect("clipboard read"), "hello wayland");
    assert_eq!(cb.get_primary().expect("primary read"), "hello wayland");
}

#[test]
fn headless_primary_write_does_not_clobber_clipboard() {
    let mut cb = Clipboard::new_headless();
    cb.set_text("clipboard".to_string()).expect("set clipboard");
    cb.set_primary("primary".to_string()).expect("set primary");
    assert_eq!(cb.get_text().expect("clipboard"), "clipboard");
    assert_eq!(cb.get_primary().expect("primary"), "primary");
}

#[test]
fn headless_clear_empties_both_buffers_and_surfaces_ok() {
    let mut cb = Clipboard::new_headless();
    cb.set_text("data".to_string()).expect("set");
    cb.try_clear().expect("headless try_clear succeeds");
    assert_eq!(cb.headless_contents(), "");
    assert_eq!(cb.primary_contents(), "");
}

#[test]
fn headless_payloads_stay_bounded_on_both_selections() {
    let mut cb = Clipboard::new_headless();
    let long = "x".repeat(CLIPBOARD_MAX_BYTES + 64);
    cb.set_text(long).expect("bounded set");
    assert_eq!(cb.headless_contents().len(), CLIPBOARD_MAX_BYTES);
    assert_eq!(cb.primary_contents().len(), CLIPBOARD_MAX_BYTES);
    let emoji = "😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 5);
    cb.set_primary(emoji).expect("bounded primary");
    assert!(cb.primary_contents().len() <= CLIPBOARD_MAX_BYTES);
}

#[test]
fn backend_hint_matches_wayland_env_signal() {
    let expected = if is_wayland_session() {
        "wayland"
    } else {
        "x11"
    };
    assert_eq!(display_backend_hint(), expected);
    assert_eq!(Clipboard::new_headless().backend_hint(), "headless");
}

#[test]
fn new_never_panics_without_display() {
    let _ = Clipboard::new();
}
