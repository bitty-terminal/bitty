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
    // CTX-0478: over-limit payloads are rejected with a typed error instead
    // of being silently truncated; the previous value is preserved.
    let mut cb = Clipboard::new_headless();
    cb.set_text("seed".to_string()).expect("seed");
    let long = "x".repeat(CLIPBOARD_MAX_BYTES + 64);
    match cb.set_text(long) {
        Err(bitty_platform::PlatformError::ClipboardPayloadTooLarge { len, max }) => {
            assert_eq!(max, CLIPBOARD_MAX_BYTES);
            assert_eq!(len, CLIPBOARD_MAX_BYTES + 64);
        }
        other => panic!("expected ClipboardPayloadTooLarge, got {other:?}"),
    }
    assert_eq!(cb.headless_contents(), "seed");
    assert_eq!(cb.primary_contents(), "seed");
    let emoji = "😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 5);
    assert!(matches!(
        cb.set_primary(emoji),
        Err(bitty_platform::PlatformError::ClipboardPayloadTooLarge { .. })
    ));
}

#[test]
fn direct_read_rejects_while_bounded_reads_clip() {
    // CTX-0478 review: a simulated over-limit system read rejects through the
    // direct API (typed error) but clips through the bounded reads the paste
    // and OSC 52 reply seams use; the lossy helpers share the bounded path
    // instead of going empty.
    let mut cb = Clipboard::new_headless();
    cb.simulate_system_text_for_test("z".repeat(CLIPBOARD_MAX_BYTES + 512));
    assert!(matches!(
        cb.get_text(),
        Err(bitty_platform::PlatformError::ClipboardPayloadTooLarge { .. })
    ));
    let text = cb.get_text_bounded().expect("bounded clipboard read");
    assert_eq!(text.len(), CLIPBOARD_MAX_BYTES);
    assert_eq!(cb.get_text_lossy(), text);
    let primary = cb.get_primary_bounded().expect("bounded primary read");
    assert_eq!(primary.len(), CLIPBOARD_MAX_BYTES);
    assert_eq!(cb.get_primary_lossy(), primary);
    // At the exact cap both reads agree; nothing is clipped.
    cb.simulate_system_text_for_test(String::from("ok"));
    assert_eq!(cb.get_text().expect("at-limit read"), "ok");
    assert_eq!(cb.get_text_bounded().expect("at-limit bounded read"), "ok");
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

#[test]
fn bounded_reads_report_whether_they_clipped() {
    // R-004 truncated-paste telemetry: the paste seam attributes a
    // platform-layer clip exactly once, so the bounded reads must report
    // whether they cut. A simulated over-limit system value stands in for
    // a hostile native clipboard without a display server.
    let mut cb = Clipboard::new_headless();
    assert!(
        !cb.last_bounded_read_truncated(),
        "flag starts clear after construction"
    );
    cb.simulate_system_text_for_test("y".repeat(CLIPBOARD_MAX_BYTES + 32));
    let text = cb.get_text_bounded().expect("bounded read clips");
    assert_eq!(text.len(), CLIPBOARD_MAX_BYTES);
    assert!(
        cb.last_bounded_read_truncated(),
        "over-limit bounded read must report the clip"
    );
    // An in-cap value clears the flag again on the next bounded read.
    cb.simulate_system_text_for_test(String::from("small"));
    assert_eq!(cb.get_text_bounded().expect("in-cap read"), "small");
    assert!(
        !cb.last_bounded_read_truncated(),
        "in-cap bounded read must clear the flag"
    );
    // The direct rejecting read never touches the flag.
    cb.simulate_system_text_for_test("z".repeat(CLIPBOARD_MAX_BYTES + 1));
    assert!(cb.get_text().is_err(), "direct read rejects over-limit");
    assert!(
        !cb.last_bounded_read_truncated(),
        "direct read must leave the flag untouched"
    );
    // The primary bounded read shares the same flag.
    let primary = cb
        .get_primary_bounded()
        .expect("bounded primary read clips");
    assert_eq!(primary.len(), CLIPBOARD_MAX_BYTES);
    assert!(
        cb.last_bounded_read_truncated(),
        "over-limit primary read must report the clip"
    );
}
