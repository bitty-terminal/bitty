//! Input-ownership contract for `Enter` while a pending paste confirmation
//! is visible (issue #2632).
//!
//! Normative form: the ownership table on
//! `Runtime::confirm_pending_paste_on_enter` (bitty-runtime). This file is
//! the executable form of that table: for each negotiated encoder path —
//! legacy (flags `0`), Kitty without report-events (flags `1`), Kitty with
//! report-events (flags `1|2`), and the `CSI u` repeat/release path
//! (`report-all-keys` on top, flags `1|2|8`) — it pins which frame confirms
//! the pending paste, which frames are consumed, and which frames route
//! ordinarily to the PTY. A confirmed paste must never additionally emit its
//! own `CR`, a legacy Enter byte, or a `CSI u` frame; a non-confirming frame
//! must never accidentally confirm or leak duplicate input. Cancellation
//! (`Esc`, `Ctrl+D`), focus loss, the confirmation timeout (issue #1438),
//! pending-absent, and already-consumed (post-confirm lifecycle) events are
//! covered on each path.
//!
//! History: the paste-confirmation gate landed under issue #2265 (PR
//! #2269); the confirmation timeout under issue #1438; Enter-confirm routing
//! under CTX-1372; the confirm-lifecycle repeat/release swallow under
//! CTX-1505. The separately scoped negotiated `Shift+Enter` behavior (PR
//! #2628) is unchanged: modified Enter keeps its negotiated identity while a
//! paste pends.
//!
//! Frame naming below: `CR` is the spec-legacy Enter byte, and a `CSI u`
//! frame is the negotiated Kitty form (`CSI 13;MODS[:TYPE]u`). Under Kitty
//! without `report-all-keys`, bare `Enter` deliberately stays on the spec's
//! legacy exception (press/repeat emit `CR`, release emits nothing); only
//! `report-all-keys` negotiates the full `CSI 13u`/`13;1:2u`/`13;1:3u`
//! forms, and modified `Enter` keeps its `CSI u` identity under
//! disambiguate-family negotiation.
//!
//! Headless, bounded, deterministic; `cargo test` on CI without X11/Wayland;
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::{Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.force_headless_clipboard();
    rt
}

/// Runtime with the 1 s minimum paste-confirmation timeout, for lifecycle
/// tests (issue #1438 validation seam).
fn make_runtime_with_timeout() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig {
        paste_confirm_timeout: std::time::Duration::from_secs(1),
        ..RuntimeConfig::default()
    })
    .expect("configured runtime must build");
    rt.force_headless_clipboard();
    rt
}

/// Gate a multi-line paste so the confirmation banner is visible.
fn gate_pending_paste(rt: &mut Runtime, text: &str) {
    rt.clipboard_mut().set_text(text.to_string()).unwrap();
    rt.drain_pending_input();
    assert!(
        rt.paste_from_clipboard().unwrap().unwrap(),
        "multi-line paste must require confirmation"
    );
    assert!(rt.has_pending_paste(), "paste must stay pending");
    assert_eq!(rt.pending_input(), b"", "no silent delivery while pending");
}

/// An `Enter` frame in the platform's shape.
fn enter(state: PressState, repeat: bool) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Enter),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat,
        is_synthetic: false,
    }
}

/// Feed a modifier key so the tracked chord state moves. This does not
/// drain the pending-input queue: forwarded frames must stay observable.
fn hold_modifier(rt: &mut Runtime, key: NamedKey, state: PressState) {
    rt.handle_key_event(KeyEvent {
        logical_key: LogicalKey::Named(key),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    });
}

fn esc_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn ctrl_d_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character("d".to_string()),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

// ── Path A: legacy encoding (no Kitty negotiation) ─────────────────────

#[test]
fn legacy_confirm_press_consumed_paste_exact_no_cr() {
    let mut rt = make_runtime();
    let text = "line1\nline2";
    gate_pending_paste(&mut rt, text);
    // Confirm press is consumed: no CR reaches the PTY.
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert!(!rt.has_pending_paste());
    // The delivered bytes are exactly the paste: no trailing CR, no doubled
    // paste, no legacy Enter byte.
    assert_eq!(rt.pending_input(), text.as_bytes());
    assert_eq!(rt.pending_input(), b"line1\nline2");
}

#[test]
fn legacy_confirm_lifecycle_repeat_and_release_consumed_no_bytes() {
    // The confirming press owns its OS repeat and paired release: neither
    // can submit an extra CR behind the paste.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        None,
        "press confirms and is consumed"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        None,
        "confirm-lifecycle repeat is consumed"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    // The lifecycle is spent after the release: a later Enter is ordinary
    // input, exactly once.
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "post-lifecycle Enter press is ordinary CR, exactly once"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\r");
}

#[test]
fn legacy_gate_visible_repeat_before_confirm_is_ordinary_press() {
    // A platform repeat frame is a physical Enter press: while the gate is
    // visible and no confirm lifecycle is live, it is the confirm gesture.
    // Its follow-up repeats are then consumed by the lifecycle, so a held
    // Enter delivers the paste exactly once and never leaks CR.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        None,
        "repeat-as-first-press confirms and is consumed"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        None,
        "held Enter repeat is consumed by the confirm lifecycle"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2", "no extra CR");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"line1\nline2");
}

#[test]
fn legacy_modified_enter_never_confirms_and_keeps_identity() {
    // Shift/Ctrl/Alt/Super modify Enter into application input; while a
    // paste pends they must not confirm and must keep their legacy identity,
    // byte-for-byte, exactly once. (Alt+Enter is ESC-prefixed by xterm
    // metaSendsEscape semantics.)
    for (key, name, expected) in [
        (NamedKey::Shift, "shift", &b"\r"[..]),
        (NamedKey::Control, "ctrl", &b"\r"[..]),
        (NamedKey::Alt, "alt", &b"\x1b\r"[..]),
        (NamedKey::Super, "super", &b"\r"[..]),
    ] {
        let mut rt = make_runtime();
        gate_pending_paste(&mut rt, "line1\nline2");
        hold_modifier(&mut rt, key, PressState::Pressed);
        assert_eq!(
            rt.handle_key_event(enter(PressState::Pressed, false)),
            Some(expected.to_vec()),
            "{name}+Enter must not confirm and must keep its legacy bytes"
        );
        assert!(rt.has_pending_paste(), "{name}+Enter must not confirm");
        assert_eq!(
            rt.pending_input(),
            expected,
            "{name}+Enter must forward exactly its legacy frame once"
        );
        hold_modifier(&mut rt, key, PressState::Released);
        // The gate is untouched: plain Enter still confirms afterwards.
        assert_eq!(
            rt.handle_key_event(enter(PressState::Pressed, false)),
            None,
            "{name}+Enter must be ordinary routing, the gate still confirms"
        );
        assert_eq!(
            rt.pending_input(),
            [expected, "line1\nline2".as_bytes()].concat(),
            "{name}+Enter frame stays exactly once, followed by the paste"
        );
    }
}

#[test]
fn legacy_esc_cancel_consumes_press_then_enter_is_ordinary() {
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    // Esc cancels: consumed, no delivery, and a cancellation never arms the
    // confirm lifecycle.
    assert_eq!(
        rt.handle_key_event(esc_press()),
        None,
        "cancelling Esc is consumed"
    );
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"", "cancel delivers nothing");
    // Post-cancel frames route ordinarily: press CR once, repeat CR once.
    // A cancelled gate must not swallow or double Enter input.
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "post-cancel Enter press is ordinary CR"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        Some(b"\r".to_vec()),
        "post-cancel Enter repeat is ordinary CR, no leaked gate"
    );
    assert_eq!(rt.pending_input(), b"\r\r");
}

#[test]
fn legacy_focus_loss_disarms_lifecycle_gate_survives_exactly_once() {
    // Focus loss drops the confirm lifecycle (CTX-1505) so the delayed
    // repeat/release of a confirmed Enter can never swallow a later Enter
    // after focus returns. The pending paste itself is not cancelled by
    // focus loss; the confirmed delivery stays exactly once.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(
        rt.pending_input(),
        b"line1\nline2",
        "paste delivered exactly once"
    );
    rt.set_focused(false);
    // The paired release arrives after focus loss: ordinary routing (legacy
    // releases produce nothing), no CR, delivery unchanged.
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"line1\nline2");
    // A fresh plain Enter after focus returns is ordinary input, exactly
    // once — proof the stale lifecycle cannot swallow it.
    rt.set_focused(true);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "focus loss must disarm the confirm lifecycle"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\r");
}

#[test]
fn legacy_timeout_auto_cancel_enter_consumes_without_delivery() {
    // Issue #1438: an expired gate must never deliver. The confirming Enter
    // press finds the gate expired; it is consumed (the banner was visible)
    // and delivers nothing — no CR rides along.
    let mut rt = make_runtime_with_timeout();
    gate_pending_paste(&mut rt, "line1\nline2");
    std::thread::sleep(std::time::Duration::from_millis(1300));
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        None,
        "Enter over an expired gate is consumed, never CR"
    );
    assert_eq!(rt.pending_input(), b"", "expired paste delivers nothing");
    // The lifecycle resolves on the paired release; routing is ordinary then.
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "post-expiry Enter is ordinary input, exactly once"
    );
    assert_eq!(rt.pending_input(), b"\r");
}

#[test]
fn legacy_gate_visible_release_without_confirm_routes_ordinarily() {
    // A release reaching a visible-but-never-confirmed gate (the gate armed
    // while the physical key was already down) is NOT a confirm gesture and
    // routes ordinarily: legacy emits no bytes, nothing is delivered, the
    // pending paste survives, and the later press of that key still
    // confirms.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None,
        "unconfirmed gate never consumes a release or delivers"
    );
    assert!(rt.has_pending_paste(), "release must not disturb the gate");
    assert_eq!(rt.pending_input(), b"");
    // The held key's press is still the confirm gesture once it arrives.
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert!(!rt.has_pending_paste());
    assert_eq!(
        rt.pending_input(),
        b"line1\nline2",
        "paste delivered exactly once"
    );
}

// ── Path B: Kitty negotiated, no report-events (flags 1) ───────────────

#[test]
fn kitty_no_report_gate_absent_bare_enter_legacy_release_silent() {
    // Spec legacy exception: bare Enter keeps its legacy bytes even under
    // Kitty negotiation without report-events. No gate: press CR once,
    // release silent.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>1u");
    rt.drain_pending_input();
    assert!(!rt.has_pending_paste());
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "bare Enter stays legacy under flags=1"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"\r", "release never adds a frame");
}

#[test]
fn kitty_no_report_confirm_never_emits_cr_or_csi_u() {
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    rt.handle_pty_bytes(b"\x1b[>1u");
    rt.drain_pending_input();
    // The confirm press is consumed before any encoder runs: neither the
    // legacy form nor a CSI u frame may leak behind the paste.
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert!(!rt.has_pending_paste());
    assert_eq!(
        rt.pending_input(),
        b"line1\nline2",
        "exact paste bytes only"
    );
    // The lifecycle consumes repeat and release under any negotiated path.
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, true)), None);
    assert_eq!(rt.pending_input(), b"line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "ordinary spec-legacy Enter after a spent lifecycle"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\r");
}

#[test]
fn kitty_no_report_shift_enter_with_pending_keeps_negotiated_identity() {
    // Separately scoped decision (PR #2628 behavior must not change):
    // modified Enter is negotiated CSI-u here and while a paste pends it
    // must neither confirm nor lose its identity; exactly one frame.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>1u");
    rt.drain_pending_input();
    gate_pending_paste(&mut rt, "line1\nline2");
    hold_modifier(&mut rt, NamedKey::Shift, PressState::Pressed);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13;2u".to_vec()),
        "Shift+Enter keeps its negotiated CSI u identity"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert!(rt.has_pending_paste(), "Shift+Enter must not confirm");
    assert_eq!(
        rt.pending_input(),
        b"\x1b[13;2u",
        "single negotiated frame, no paste bytes"
    );
    hold_modifier(&mut rt, NamedKey::Shift, PressState::Released);
    // Plain Enter still confirms on the negotiated runtime afterwards.
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(rt.pending_input(), b"\x1b[13;2uline1\nline2");
}

// ── Path C: Kitty with report-events (flags 1|2) ───────────────────────

#[test]
fn kitty_report_events_gate_absent_bare_enter_exception_wiring() {
    // Gate absent, flags=3: bare Enter stays on the spec legacy exception
    // (press/repeat legacy CR, release silent) — each frame exactly once.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>3u");
    rt.drain_pending_input();
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        Some(b"\r".to_vec()),
        "bare Enter repeat keeps the legacy exception shape under flags=3"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"\r\r");
}

#[test]
fn kitty_report_events_modified_enter_frames_forwarded_exactly() {
    // Gate absent, flags=3: a modified Enter reports press + repeat (:2) +
    // release (:3) — the CSI u event-type path. Each frame exactly once, no
    // duplicates.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>3u");
    rt.drain_pending_input();
    hold_modifier(&mut rt, NamedKey::Control, PressState::Pressed);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13;5u".to_vec()),
        "ctrl+Enter press frame"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        Some(b"\x1b[13;5:2u".to_vec()),
        "ctrl+Enter repeat :2 frame"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        Some(b"\x1b[13;5:3u".to_vec()),
        "ctrl+Enter release :3 frame"
    );
    assert_eq!(rt.pending_input(), b"\x1b[13;5u\x1b[13;5:2u\x1b[13;5:3u");
}

#[test]
fn kitty_report_events_pending_gate_confirms_and_consumes_frames() {
    // Gate visible over flags=3: the plain Enter press confirms (consumed,
    // no CR, no CSI u); repeat and release stay consumed by the lifecycle;
    // the post-consumption press routes ordinarily, exactly once.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    rt.handle_pty_bytes(b"\x1b[>3u");
    rt.drain_pending_input();
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(rt.pending_input(), b"line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        None,
        "repeat consumed"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"line1\nline2", "no leaked CR or CSI u");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec()),
        "post-consumption press forwards once"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\r");
}

#[test]
fn kitty_report_events_alt_enter_with_pending_keeps_identity_no_confirm() {
    // Gate visible + report-events: modified Enter frames are ordinary key
    // routing exactly once (no confirm, no paste bytes, no duplicates), and
    // the gate stays confirmable by plain Enter afterwards.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>3u");
    rt.drain_pending_input();
    gate_pending_paste(&mut rt, "line1\nline2");
    hold_modifier(&mut rt, NamedKey::Alt, PressState::Pressed);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13;3u".to_vec()),
        "alt+Enter press keeps its negotiated frame"
    );
    assert!(rt.has_pending_paste(), "alt+Enter must not confirm");
    assert_eq!(rt.pending_input(), b"\x1b[13;3u");
    hold_modifier(&mut rt, NamedKey::Alt, PressState::Released);
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"\x1b[13;3uline1\nline2");
}

// ── Path D: Kitty report-all-keys (flags 1|2|8) — full CSI u frames ─────

#[test]
fn kitty_report_all_gate_absent_frames_forwarded_exactly() {
    // No gate, flags=11: report-all-keys encodes bare Enter fully — press
    // CSI 13u, repeat CSI 13;1:2u, release CSI 13;1:3u. Each exactly once.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>11u");
    rt.drain_pending_input();
    assert!(!rt.has_pending_paste());
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13u".to_vec()),
        "report-all press frame"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        Some(b"\x1b[13;1:2u".to_vec()),
        "report-all repeat :2 frame"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        Some(b"\x1b[13;1:3u".to_vec()),
        "report-all release :3 frame"
    );
    assert_eq!(rt.pending_input(), b"\x1b[13u\x1b[13;1:2u\x1b[13;1:3u");
}

#[test]
fn kitty_report_all_confirm_consumes_no_csi_u_leak() {
    // Gate visible over flags=11: the confirming press is consumed so no
    // CSI 13u can reach the PTY behind the paste; repeat/release stay
    // consumed too; post-consumption frames forward the full CSI u forms,
    // exactly once each — consumed-vs-forwarded ownership proven both ways.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    rt.handle_pty_bytes(b"\x1b[>11u");
    rt.drain_pending_input();
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        None,
        "confirm press must not emit CSI 13u"
    );
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"line1\nline2");
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        None,
        "no :2 leak"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None,
        "no :3 leak"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2");
    // Post-consumption: frames forward exactly as the gate-absent baseline.
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, true)),
        Some(b"\x1b[13;1:2u".to_vec())
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        Some(b"\x1b[13;1:3u".to_vec())
    );
    assert_eq!(
        rt.pending_input(),
        b"line1\nline2\x1b[13u\x1b[13;1:2u\x1b[13;1:3u"
    );
}

#[test]
fn kitty_report_all_focus_loss_release_routes_ordinarily_then_input_restores() {
    // Focus loss disarms the confirm lifecycle after confirmation; the
    // delayed release routes ordinarily (a real physically-released key
    // under report-all-keys gets its negotiated :3 frame — this is ordinary
    // key routing, not paste-confirm output), and a later Enter after focus
    // returns is ordinary input, exactly once, never swallowed by the stale
    // lifecycle.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[>11u");
    rt.drain_pending_input();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(rt.pending_input(), b"line1\nline2");
    rt.set_focused(false);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        Some(b"\x1b[13;1:3u".to_vec()),
        "post-focus-loss release is ordinary negotiated routing, exactly once"
    );
    rt.set_focused(true);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\x1b[13u".to_vec()),
        "post-focus-loss Enter is ordinary negotiated input, exactly once"
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\x1b[13;1:3u\x1b[13u");
}

// ── Cancellation across paths: Ctrl+D keeps routing ordinary ───────────

#[test]
fn ctrl_d_cancel_then_enter_frames_are_ordinary_under_report_events() {
    // Issue #1336: Ctrl+D dismisses without delivery. A gate cancelled
    // through Ctrl+D must not leave any lifecycle armed and must not leak
    // paste bytes or CR; Enter frames after the dismissal route ordinarily.
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    rt.handle_pty_bytes(b"\x1b[>3u");
    rt.drain_pending_input();
    hold_modifier(&mut rt, NamedKey::Control, PressState::Pressed);
    assert_eq!(
        rt.handle_key_event(ctrl_d_press()),
        None,
        "Ctrl+D dismissal consumed"
    );
    assert!(!rt.has_pending_paste());
    assert_eq!(rt.pending_input(), b"", "no delivery, no leaked bytes");
    hold_modifier(&mut rt, NamedKey::Control, PressState::Released);
    // Ordinary routing afterwards, exactly once: press CR, release silent
    // (spec legacy exception under flags=3).
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(rt.pending_input(), b"\r");
}

#[test]
fn already_consumed_lifecycle_late_press_stays_consumed() {
    // "Already consumed" edge: a late press of the confirming Enter (frame
    // replayed on the wire, e.g. a backend quirk) belongs to the confirm
    // lifecycle — consumed exactly, no duplicate paste, no CR; the
    // lifecycle still resolves on its release; later plain Enter routes
    // ordinarily (the gate is already gone).
    let mut rt = make_runtime();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        None,
        "late press stays consumed"
    );
    assert_eq!(
        rt.pending_input(),
        b"line1\nline2",
        "no duplicate paste, no CR"
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(
        rt.handle_key_event(enter(PressState::Pressed, false)),
        Some(b"\r".to_vec())
    );
    assert_eq!(rt.pending_input(), b"line1\nline2\r");
}

// ── Bracketed-paste identity (issue #2265 / PR #2269 delivery seam) ─────

#[test]
fn bracketed_mode_confirms_with_bracketed_delivery_only() {
    // When `?2004` is negotiated, the confirming Enter delivers through the
    // bracketed seam; no CR or CSI frame may ride along.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[?2004h");
    rt.drain_pending_input();
    gate_pending_paste(&mut rt, "line1\nline2");
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, false)), None);
    assert_eq!(
        rt.pending_input(),
        b"\x1b[200~line1\nline2\x1b[201~",
        "bracketed delivery exactly, nothing extra"
    );
    assert_eq!(rt.handle_key_event(enter(PressState::Pressed, true)), None);
    assert_eq!(
        rt.handle_key_event(enter(PressState::Released, false)),
        None
    );
    assert_eq!(
        rt.pending_input(),
        b"\x1b[200~line1\nline2\x1b[201~",
        "no extra CR behind the bracketed paste"
    );
}
