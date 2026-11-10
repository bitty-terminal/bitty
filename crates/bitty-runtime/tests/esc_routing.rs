//! Scoped `Esc` routing (CTX-0475, `bitty` issue #756).
//!
//! `cancel_pending_on_escape` must only *consume* an `Esc` that cancels a
//! real confirmation gate (suspicious paste, workspace kill-confirm,
//! view/window close-confirm). Informational overlays — the extension-owned
//! help/which-key panel is one — never own the key, because a fullscreen app
//! (vim/less) treats `Esc` as a mode key and a swallowed press desyncs its
//! state machine.
//!
//! These tests are the probe matrix for that scope: they fail on a
//! pre-fix `Esc` swallow and pin the gate semantics.
#![forbid(unsafe_code)]

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::{CloseConfirmMode, Runtime, RuntimeConfig, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn runtime_with_close_confirm() -> Runtime {
    Runtime::new(RuntimeConfig {
        close_confirm: CloseConfirmMode::Always,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build")
}

fn esc(state: PressState) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn char_press(ch: char) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: Some(ch.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn control_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Control),
        text: None,
        location: KeyLocation::Left,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn ctrl_d(logical: char) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(logical.to_string()),
        text: Some("\u{4}".to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn enter_fullscreen(rt: &mut Runtime) {
    // Alternate-screen entry, the fullscreen-app contract vim/less use.
    rt.handle_pty_bytes(b"\x1b[?1049h");
    rt.handle_pty_bytes(b"vim");
}

/// Regression guard: `Esc` with no confirmation gate armed still encodes.
#[test]
fn esc_without_gates_still_encodes() {
    let mut rt = make_runtime();
    assert_eq!(
        rt.handle_key_event(esc(PressState::Pressed)),
        Some(vec![27])
    );
}

/// An `Esc` release never produces bytes and cancels nothing.
#[test]
fn esc_release_never_delivers() {
    let mut rt = make_runtime();
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Released));

    assert_eq!(out, None);
    assert!(rt.pending_input().is_empty(), "release never delivers");
}

/// A non-`Esc` key is unaffected: it encodes normally.
#[test]
fn non_esc_key_still_encodes() {
    let mut rt = make_runtime();
    rt.drain_pending_input();

    assert_eq!(rt.handle_key_event(char_press('a')), Some(vec![b'a']));
    assert_eq!(rt.pending_input(), b"a");
}

/// The paste gate still owns `Esc`: cancelling consumes the press (the
/// dismissal must not also drive shell/vim state on the aborted paste).
#[test]
fn esc_cancels_pending_paste_gate_and_is_consumed() {
    let mut rt = make_runtime();
    assert!(
        rt.paste_text_via_gate("one\ntwo".to_string()),
        "multi-line paste gates"
    );
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert_eq!(out, None, "paste-cancel Esc is consumed, never PTY input");
    assert!(!rt.has_pending_paste(), "paste cancelled");
    assert!(rt.pending_input().is_empty(), "cancel must not deliver");
}

/// Paste delivery is preserved under a fullscreen app: `Esc` still cancels
/// the gate (the documented cancel gesture) and is consumed there.
#[test]
fn esc_cancels_paste_gate_in_fullscreen_and_is_consumed() {
    let mut rt = make_runtime();
    enter_fullscreen(&mut rt);
    assert!(rt.paste_text_via_gate("one\ntwo".to_string()));
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert_eq!(out, None);
    assert!(!rt.has_pending_paste());
    assert!(rt.pending_input().is_empty(), "no Esc leaks to the PTY");
}

/// Selection clearing (CTX-0166) composes with the dismissal: the highlight
/// clears and the `Esc` is delivered.
#[test]
fn esc_clears_selection_and_delivers() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"select me");
    rt.select_all();
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert!(!rt.has_selection(), "press clears the highlight");
    assert_eq!(out, Some(vec![27]), "Esc still reaches the shell");
}

/// The view/window close-confirm gate owns `Esc` and consumes it.
#[test]
fn esc_cancels_close_confirm_gate_and_is_consumed() {
    let mut rt = runtime_with_close_confirm();
    rt.view_close_request(ViewId::new(1));
    assert!(rt.has_pending_close_confirm(), "close-confirm armed");
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert_eq!(out, None, "close-confirm cancel is consumed");
    assert!(!rt.has_pending_close_confirm(), "arm cancelled");
    assert!(rt.pending_input().is_empty());
}

/// `Alt`-modified `Esc` in a fullscreen app is still not a pure dismissal:
/// the modified encoding reaches the PTY.
#[test]
fn alt_esc_in_fullscreen_delivers() {
    let mut rt = make_runtime();
    enter_fullscreen(&mut rt);
    // Alt is tracked from modifiers; press Alt then Esc (metaSendsEscape).
    let alt = KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Alt),
        text: None,
        location: KeyLocation::Left,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    };
    rt.handle_key_event(alt);
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert!(
        out.is_some(),
        "an Alt+Esc still routes instead of vanishing into an overlay"
    );
    assert!(!rt.pending_input().is_empty(), "bytes reached the PTY");
}

/// Issue #1336: `Ctrl+D` while a paste pends drops the paste (no `0x04`
/// to the shell, no exit) instead of delivering EOF behind the banner.
#[test]
fn ctrl_d_with_pending_paste_drops_without_bytes() {
    let mut rt = make_runtime();
    rt.paste_text("line1\nline2");
    assert!(rt.has_pending_paste(), "multi-line paste gates");
    assert!(rt.paste_banner_text().is_some(), "banner shows");
    // Latch Control the way a held modifier key does.
    let _ = rt.handle_key_event(control_press());
    rt.drain_pending_input();

    let out = rt.handle_key_event(ctrl_d('d'));

    assert_eq!(out, None, "dismissing Ctrl+D is consumed");
    assert!(!rt.has_pending_paste(), "pending paste dropped");
    assert!(
        rt.drain_pending_input().is_empty(),
        "no EOF byte behind the banner"
    );
    assert!(
        rt.paste_banner_text().is_none(),
        "banner clears with the gate"
    );
    // A second Ctrl+D with nothing pending encodes EOF normally again.
    let out = rt.handle_key_event(ctrl_d('d'));
    assert_eq!(out, Some(vec![0x04]), "EOF encodes once the gate is gone");
    assert_eq!(rt.drain_pending_input(), b"\x04");
}

/// Issue #1336: the shifted (`D`) reporting of the same chord dismisses
/// too — the exit-intent gesture wins while the banner shows.
#[test]
fn ctrl_shift_d_with_pending_paste_drops_without_bytes() {
    let mut rt = make_runtime();
    rt.paste_text("line1\nline2");
    assert!(rt.has_pending_paste(), "multi-line paste gates");
    let _ = rt.handle_key_event(control_press());
    rt.drain_pending_input();

    let out = rt.handle_key_event(ctrl_d('D'));

    assert_eq!(out, None, "dismissing Ctrl+Shift+D is consumed");
    assert!(!rt.has_pending_paste(), "pending paste dropped");
    assert!(rt.drain_pending_input().is_empty());
}

/// Issue #1336: plain typing still passes behind the banner (the paste
/// gate captures nothing); only the dismiss gestures consume.
#[test]
fn plain_typing_behind_pending_paste_still_delivers() {
    let mut rt = make_runtime();
    rt.paste_text("line1\nline2");
    assert!(rt.has_pending_paste(), "multi-line paste gates");
    rt.drain_pending_input();

    let out = rt.handle_key_event(char_press('a'));

    assert_eq!(out, Some(vec![b'a']), "typing is not swallowed");
    assert!(rt.has_pending_paste(), "paste still pends");
    assert_eq!(rt.drain_pending_input(), b"a");
}

/// Issue #1336: a `Ctrl+D` release (or a bare `d` without Control) never
/// dismisses — only a real pressed `Ctrl+D` chord does.
#[test]
fn ctrl_d_release_and_bare_d_keep_pending_paste() {
    let mut rt = make_runtime();
    rt.paste_text("line1\nline2");
    assert!(rt.has_pending_paste(), "multi-line paste gates");
    let _ = rt.handle_key_event(control_press());
    rt.drain_pending_input();

    let mut released = ctrl_d('d');
    released.state = PressState::Released;
    assert_eq!(rt.handle_key_event(released), None);
    assert!(rt.has_pending_paste(), "release dismisses nothing");

    // Release Control: a bare `d` is typing, not a dismiss.
    let control_up = KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Control),
        text: None,
        location: KeyLocation::Left,
        state: PressState::Released,
        repeat: false,
        is_synthetic: false,
    };
    let _ = rt.handle_key_event(control_up);
    rt.drain_pending_input();
    assert_eq!(
        rt.handle_key_event(char_press('d')),
        Some(vec![b'd']),
        "bare d types"
    );
    assert!(rt.has_pending_paste(), "bare d dismisses nothing");
    assert_eq!(rt.drain_pending_input(), b"d");
}
