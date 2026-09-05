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

use bitty_runtime::Runtime;
use bitty_ui::CellPos;

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

fn inspect_via_runtime(text: &str) -> bool {
    let mut rt = make_runtime();
    rt.request_paste(text.to_owned())
}

fn clean(text: &str) {
    let insp = inspect_via_runtime(text);
    assert!(!insp, "expected clean for {text:?}");
}

fn suspicious(text: &str, reason: &str) {
    let insp = inspect_via_runtime(text);
    assert!(
        insp,
        "expected suspicious for {text:?} ({reason}), got clean"
    );
    let _ = reason;
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
        let insp = inspect_via_runtime(&s2);
        assert!(insp, "C0 0x{b:02X} must need confirmation in {s2:?}");
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
        let insp = inspect_via_runtime(&s);
        assert!(insp, "bidi U+{:04X} must need confirmation", ch as u32);
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
    assert!(!insp);
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
    assert!(insp);
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
    assert!(insp2);
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
        assert!(insp, "paste {text:?} must need confirmation, got {insp:?}");
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
    assert!(insp);
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
    assert!(insp);
    assert!(rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"");
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"a\x07b");
}

#[test]
fn sequential_suspicious_requests_preserve_first_pending_paste() {
    let mut rt = make_runtime();
    assert!(rt.paste_text_via_gate("first\n paste".to_string()));
    assert_eq!(rt.pending_paste_text(), Some("first\n paste"));

    assert!(rt.paste_text_via_gate("second\x1b paste".to_string()));
    assert_eq!(rt.pending_paste_text(), Some("first\n paste"));
    assert_eq!(rt.pending_input(), b"");

    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"first\n paste");
    assert!(!rt.has_pending_paste());
}

#[test]
fn public_paste_text_api_is_also_gated() {
    let mut rt = make_runtime();
    rt.paste_text("a\x1bb");
    assert!(rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"");
    assert!(
        rt.pending_paste_inspection()
            .expect("suspicious paste must be pending")
    );
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"a\x1bb");
}

#[test]
fn string_paste_apis_bound_oversized_ascii() {
    let long = "a".repeat(9000);

    let mut via_gate = make_runtime();
    let inspection = via_gate.paste_text_via_gate(long.clone());
    assert!(!inspection);
    assert!(!via_gate.has_pending_paste());
    assert_eq!(via_gate.pending_input().len(), 8192);

    let mut direct = make_runtime();
    direct.request_paste(long);
    assert!(!direct.has_pending_paste());
    assert_eq!(direct.pending_input().len(), 8192);
}

#[test]
fn string_paste_apis_bound_oversized_multibyte_suspicious_input() {
    let long = "\u{202E}".repeat(3000);
    let mut rt = make_runtime();

    rt.paste_text(&long);
    let pending = rt
        .pending_paste_text()
        .expect("suspicious text must be pending");
    let pending_len = pending.len();
    assert!(pending_len <= 8192);
    assert!(pending_len % "\u{202E}".len() == 0);
    assert!(std::str::from_utf8(pending.as_bytes()).is_ok());
    assert!(
        rt.pending_paste_inspection()
            .expect("suspicious text must have inspection")
    );
    assert_eq!(rt.pending_input(), b"");
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input().len(), pending_len);
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
    assert!(!insp);
    assert_eq!(rt.pending_input(), b"clean", "clean delivered immediately");
    // But suspicious paste still gated even with bracketed on
    rt.drain_pending_input();
    rt.clipboard_mut()
        .set_text("suspicious\nline".to_string())
        .unwrap();
    let insp2 = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp2);
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
    assert!(insp3);
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
        assert!(!insp, "truncated prefix must be clean");
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
    assert!(!insp); // emoji itself is not suspicious
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

// Selection copy and string-input paste both remain behind the inspection gate.

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
    assert!(!insp);
    assert_eq!(rt.pending_input(), b"hello");
}

// ── CTX-0186: multi-line paste must never silently drop ────────────────
//
// Owner verify 2026-09-05 (issue #287): 2+ line clipboard content reaches the
// clipboard but right-click does nothing; outside multi-line content cannot
// paste via chords or right-click, while single-line works. The
// suspicious-paste gate holds newline content as pending-confirmation with no
// confirmation UI surfaced, so the paste is silently dropped.
//
// Contract under test (same paths the chord/right-click use):
// - first paste of 2-line text is gated (pending, nothing delivered) AND
//   visible via a bounded summary (line count + reason), never silent;
// - repeating the identical paste is explicit confirmation and delivers;
// - right-click follows the same visible/confirmable path;
// - Esc while pending cancels without delivery and without leaking Esc.

#[test]
fn ctx0186_multiline_chord_paste_is_visible_and_repeat_confirms() {
    let mut rt = make_runtime();
    rt.clipboard_mut()
        .set_text("line1\nline2".to_string())
        .unwrap();
    rt.drain_pending_input();
    // Same entry the Ctrl+Shift+V chrome action uses.
    let insp = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(insp, "2-line paste must be gated");
    assert!(rt.has_pending_paste());
    assert_eq!(
        rt.pending_input(),
        b"",
        "gated paste must not deliver before confirm"
    );
    // Visible, not silent: bounded summary names the line count + reason.
    let summary = rt
        .pending_paste_summary()
        .expect("pending paste must be visible, never silent");
    assert!(
        summary.contains("2 lines"),
        "summary must name line count: {summary:?}"
    );
    assert!(
        summary.contains("newline"),
        "summary must name reason: {summary:?}"
    );
    // Explicit repeat of the identical paste confirms and delivers.
    rt.drain_pending_input();
    let insp2 = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(
        !insp2,
        "repeating the identical pending paste confirms delivery"
    );
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"line1\nline2");
}

#[test]
fn ctx0186_multiline_right_click_paste_is_visible_and_repeat_confirms() {
    use bitty_platform::{CursorPosition, MouseButton, MouseEvent, PressState};
    let mut rt = make_runtime();
    rt.clipboard_mut()
        .set_text("aaa\nbbb\nccc".to_string())
        .unwrap();
    rt.drain_pending_input();
    rt.handle_cursor_moved(CursorPosition { x: 0.0, y: 0.0 });
    // Same entry the right-click mouse path uses.
    rt.handle_mouse_input(MouseEvent {
        button: MouseButton::Right,
        state: PressState::Pressed,
    });
    assert!(rt.has_pending_paste(), "3-line right-click must gate");
    assert_eq!(rt.pending_input(), b"", "no silent delivery before confirm");
    let summary = rt
        .pending_paste_summary()
        .expect("pending paste must be visible, never silent");
    assert!(
        summary.contains("3 lines"),
        "summary must name line count: {summary:?}"
    );
    // Second identical right-click confirms and delivers both lines.
    rt.handle_mouse_input(MouseEvent {
        button: MouseButton::Right,
        state: PressState::Pressed,
    });
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"aaa\nbbb\nccc");
}

#[test]
fn ctx0186_esc_cancels_pending_without_delivery_or_esc_leak() {
    use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
    let mut rt = make_runtime();
    rt.clipboard_mut().set_text("one\ntwo".to_string()).unwrap();
    rt.drain_pending_input();
    assert!(rt.paste_from_clipboard().unwrap().unwrap());
    assert!(rt.has_pending_paste());
    // Esc while pending cancels the dialog; Esc itself must not leak to the PTY.
    let cancelled = rt.handle_key_event(KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    });
    assert_eq!(
        cancelled, None,
        "cancelling Esc is consumed, never PTY input"
    );
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"", "cancel must not deliver");
}

#[test]
fn ctx0186_different_clipboard_while_pending_preserves_first() {
    // TOCTOU pin: swapping the clipboard between presses must not smuggle
    // new content through the repeat-to-confirm path.
    let mut rt = make_runtime();
    rt.clipboard_mut()
        .set_text("first\npaste".to_string())
        .unwrap();
    rt.drain_pending_input();
    assert!(rt.paste_from_clipboard().unwrap().unwrap());
    assert_eq!(rt.pending_paste_text(), Some("first\npaste"));
    rt.clipboard_mut()
        .set_text("second\npaste".to_string())
        .unwrap();
    let still_pending = rt.paste_from_clipboard().unwrap().unwrap();
    assert!(still_pending, "different content must not confirm");
    assert_eq!(rt.pending_paste_text(), Some("first\npaste"));
    assert_eq!(rt.pending_input(), b"");
    assert!(rt.confirm_pending_paste(true));
    assert_eq!(rt.pending_input(), b"first\npaste");
}
