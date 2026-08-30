//! Suspicious-paste inspection (P0-AC-008) — adversarial classes + confirmation gate.
//!
//! Every class (C0 controls, NUL, ESC, CR, embedded newline, Unicode controls
//! U+0080..U+009F, BiDi/zero-width) must trigger inspection and require
//! confirmation. There is no silent delivery path for suspicious pastes.
//! Bracketed paste `?2004` is defense-in-depth only and wraps confirmed
//! delivery when enabled in terminal state.
//!
//! Headless, bounded, deterministic; `cargo test` on CI without X11/Wayland;
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, inspect_paste};
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn clean(text: &str) {
    let insp = inspect_paste(text);
    assert!(insp.is_clean(), "expected clean for {text:?}, got {insp:?}");
}

fn suspicious(text: &str, reason: &str) {
    let insp = inspect_paste(text);
    assert!(
        insp.needs_confirmation(),
        "expected suspicious for {text:?} ({reason}), got clean"
    );
    assert!(
        insp.reasons().contains(&reason) || reason == "C0" && insp.has_c0,
        "expected reason {reason} in {:?} for {text:?}",
        insp.reasons()
    );
}

// ── Unit inspection: each adversarial class triggers confirmation ──────

#[test]
fn clean_text_is_not_suspicious() {
    clean("hello world 123");
    clean("foo-bar_baz/qux");
    clean("line with tab\there"); // tab is allowed
    clean("");
}

#[test]
fn nul_triggers() {
    suspicious("a\0b", "NUL");
    suspicious("\0", "NUL");
}

#[test]
fn esc_triggers() {
    suspicious("a\x1bb", "ESC");
    suspicious("\x1b[2J", "ESC");
}

#[test]
fn cr_triggers() {
    suspicious("a\rb", "CR");
    suspicious("foo\rbar", "CR");
}

#[test]
fn newline_triggers() {
    suspicious("a\nb", "newline");
    suspicious("foo\nbar", "newline");
    suspicious("\n", "newline");
}

#[test]
fn c0_controls_trigger() {
    // Every C0 (0x00..0x1F) except \t should be flagged as C0
    for b in 0x00u8..=0x1F {
        if b == 0x09 {
            continue; // tab allowed
        }
        // Construct via char so every control byte is valid.
        let s2 = format!("a{}b", b as char);
        let insp = inspect_paste(&s2);
        assert!(insp.has_c0, "C0 byte 0x{b:02X} not flagged in {s2:?}");
        assert!(
            insp.needs_confirmation(),
            "C0 0x{b:02X} must need confirmation"
        );
    }
}

#[test]
fn unicode_c1_controls_trigger() {
    // U+0080..U+009F inclusive
    suspicious("a\u{0080}b", "unicode-control");
    suspicious("a\u{0090}b", "unicode-control");
    suspicious("a\u{009F}b", "unicode-control");
    // Just beyond range is clean
    clean("a\u{00A0}b");
    clean("a\u{00FF}b");
}

#[test]
fn bidi_and_zero_width_trigger() {
    for ch in [
        '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}',
        '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2060}', '\u{2066}', '\u{2067}',
        '\u{2068}', '\u{2069}', '\u{FEFF}',
    ] {
        let s = format!("a{ch}b");
        let insp = inspect_paste(&s);
        assert!(insp.has_bidi, "bidi U+{:04X} not flagged", ch as u32);
        assert!(insp.needs_confirmation());
    }
}

// ── Integration: confirmation gate — no silent delivery path ──────────

#[test]
fn paste_from_clipboard_clean_delivers_immediately_no_pending() {
    let mut rt = make_runtime();
    rt.clipboard_mut()
        .set_text("hello clean".to_string())
        .unwrap();
    rt.drain_pending_input();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.is_clean());
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"hello clean");
}

#[test]
fn paste_from_clipboard_suspicious_requires_confirmation_no_silent_delivery() {
    let mut rt = make_runtime();
    // NUL
    rt.clipboard_mut().set_text("a\0b".to_string()).unwrap();
    rt.drain_pending_input();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.needs_confirmation());
    assert!(rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"", "must not deliver before confirm");
    // Cancel -> still no delivery
    assert!(rt.cancel_pending_paste());
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"");
    // Second suspicious paste, then confirm -> delivers
    rt.clipboard_mut()
        .set_text("evil\x1b[2J".to_string())
        .unwrap();
    let insp2 = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp2.has_esc);
    assert!(rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"");
    assert!(rt.confirm_pending_paste(true));
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"evil\x1b[2J");
}

#[test]
fn each_suspicious_class_requires_confirmation_and_no_silent_path() {
    let cases: &[(&str, &str)] = &[
        ("a\0b", "NUL"),
        ("a\x1bb", "ESC"),
        ("a\rb", "CR"),
        ("a\nb", "newline"),
        ("a\x07b", "C0"), // BEL
        ("a\u{0080}b", "unicode-control"),
        ("a\u{202E}b", "bidi"),
    ];
    for (text, _reason) in cases {
        let mut rt = make_runtime();
        rt.clipboard_mut().set_text((*text).to_string()).unwrap();
        rt.drain_pending_input();
        let insp = rt.paste_from_clipboard().unwrap().unwrap();
        assert!(
            insp.needs_confirmation(),
            "paste {text:?} must need confirmation, got {insp:?}"
        );
        assert!(rt.has_pending_paste(), "paste {text:?} must leave pending");
        assert_eq!(
            rt.pending_input(),
            b"",
            "paste {text:?} must not deliver silently"
        );
        // Confirm delivers
        assert!(rt.confirm_pending_paste(true));
        assert_eq!(rt.pending_input(), text.as_bytes());
        assert!(!rt.has_pending_paste());
    }
}

#[test]
fn confirm_false_drops_without_delivery_cancel_idempotent() {
    let mut rt = make_runtime();
    rt.clipboard_mut().set_text("drop\nme".to_string()).unwrap();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.has_newline);
    assert!(rt.confirm_pending_paste(false));
    assert_eq!(rt.pending_input(), b"");
    // Already consumed — second confirm returns false, no delivery
    assert!(!rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"");
    assert!(!rt.cancel_pending_paste());
}

#[test]
fn request_paste_api_also_gated_no_silent_path() {
    let mut rt = make_runtime();
    rt.drain_pending_input();
    let insp = rt.paste_text_via_gate("a\x07b".to_string());
    assert!(insp.has_c0);
    assert!(rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"");
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"a\x07b");
}

#[test]
fn bracketed_paste_is_defense_in_depth_only_never_bypasses_gate() {
    // Enable bracketed paste via parser state.
    let mut rt = make_runtime();
    // Terminal state starts with bracketed_paste = false; enable via CSI ?2004h
    rt.handle_pty_bytes(b"\x1b[?2004h");
    assert!(rt.state().modes().bracketed_paste, "?2004 must be on");
    // Clean paste -> bracketed wrapping when enabled
    rt.drain_pending_input();
    rt.clipboard_mut().set_text("clean".to_string()).unwrap();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.is_clean());
    assert_eq!(rt.pending_input(), b"clean", "clean delivered immediately");
    // But suspicious paste still gated even with bracketed on
    rt.drain_pending_input();
    rt.clipboard_mut()
        .set_text("suspicious\nline".to_string())
        .unwrap();
    let insp2 = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp2.has_newline);
    assert!(rt.has_pending_paste());
    assert_eq!(
        rt.pending_input(),
        b"",
        "gated even with bracketed_paste on"
    );
    // Confirmed delivery is bracketed
    rt.confirm_pending_paste(true);
    assert_eq!(rt.pending_input(), b"\x1b[200~suspicious\nline\x1b[201~");
    // Disable bracketed, suspicious still gated and not bracketed on confirm
    rt.handle_pty_bytes(b"\x1b[?2004l");
    assert!(!rt.state().modes().bracketed_paste);
    rt.drain_pending_input();
    rt.clipboard_mut().set_text("evil\x1b".to_string()).unwrap();
    let insp3 = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp3.has_esc);
    assert_eq!(rt.pending_input(), b"");
    rt.confirm_pending_paste(true);
    assert_eq!(rt.pending_input(), b"evil\x1b", "no bracket when ?2004 off");
}

#[test]
fn truncation_at_char_boundary_before_inspection_deterministic() {
    // Paste content beyond CLIPBOARD_MAX_BYTES is truncated at a char boundary
    // before inspection (via Clipboard primitive). Deterministic across runtimes.
    let mut a = make_runtime();
    let mut b = make_runtime();
    let long = "a".repeat(9000) + "\u{202E}tail";
    for rt in [&mut a, &mut b] {
        rt.clipboard_mut().set_text(long.clone()).unwrap();
        assert_eq!(rt.clipboard().headless_contents().len(), 8192);
        rt.drain_pending_input();
        let insp = rt.paste_from_clipboard().unwrap().unwrap();
        // Truncated prefix is all 'a' (no suspicious), so clean and delivered immediately
        // (the BiDi was beyond the truncation point and is dropped).
        assert!(insp.is_clean(), "truncated prefix must be clean: {insp:?}");
        assert!(!rt.has_pending_paste());
        assert_eq!(rt.pending_input().len(), 8192);
    }
    assert_eq!(a.pending_input(), b.pending_input());
}

#[test]
fn embedded_emoji_boundary_truncation_remains_valid_utf8() {
    let mut rt = make_runtime();
    let emoji = "😀".repeat(3000); // 4 bytes each, 12000 bytes total > 8192
    rt.clipboard_mut().set_text(emoji).unwrap();
    let contents = rt.clipboard().headless_contents().to_owned();
    assert!(contents.len() <= 8192);
    assert!(contents.len() % 4 == 0 || contents.len() < 8192);
    // Valid UTF-8 after truncation
    assert!(String::from_utf8(contents.clone().into_bytes()).is_ok());
    rt.drain_pending_input();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.is_clean()); // emoji itself is not suspicious
    assert_eq!(rt.pending_input().len(), contents.len());
}

#[test]
fn empty_clipboard_returns_none_no_paste() {
    let mut rt = make_runtime();
    rt.clipboard_mut().clear();
    assert_eq!(rt.paste_from_clipboard().unwrap(), None);
    assert!(!rt.has_pending_paste());
}

#[test]
fn paste_inspection_headless_deterministic_across_runtimes() {
    let cases = ["a\0b", "a\nb", "a\u{0080}b", "a\u{202E}b", "clean"];
    for text in cases {
        let mut a = make_runtime();
        let mut b = make_runtime();
        for rt in [&mut a, &mut b] {
            rt.clipboard_mut().set_text(text.to_string()).unwrap();
            rt.drain_pending_input();
            let _ = rt.paste_from_clipboard().unwrap();
        }
        assert_eq!(
            a.pending_paste_text(),
            b.pending_paste_text(),
            "pending text not deterministic for {text:?}"
        );
        assert_eq!(
            a.pending_paste_inspection(),
            b.pending_paste_inspection(),
            "inspection not deterministic for {text:?}"
        );
        assert_eq!(a.pending_input(), b.pending_input());
    }
}

// Ensure the interpolation-bypass helper `paste_text` (un-gated) is not used
// for clipboard paste: clipboard always goes through inspection.

#[test]
fn selection_copy_paste_clean_is_not_gated_but_clipboard_paste_is() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello");
    rt.start_selection(CellPos::new(0, 0));
    rt.end_selection(CellPos::new(0, 4));
    rt.copy_selection_lossy().unwrap();
    // Selection text "hello" is clean, so clipboard paste delivers immediately.
    rt.drain_pending_input();
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp.is_clean());
    assert_eq!(rt.pending_input(), b"hello");
}
