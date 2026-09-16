//! Scoped `Esc` routing (CTX-0475, `bitty` issue #756).
//!
//! `cancel_pending_on_escape` must only *consume* an `Esc` that cancels a
//! real confirmation gate (suspicious paste, workspace kill-confirm,
//! view/window close-confirm). The CTX-0265 help popup is informational, not
//! modal: dismissing it must not swallow the key, because a fullscreen app
//! (vim/less) treats `Esc` as a mode key and the swallowed press desyncs its
//! state machine.
//!
//! These tests are the probe matrix for that scope: they fail on the
//! pre-fix `Esc` swallow and pin the unchanged gate semantics.
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

fn show_help(rt: &mut Runtime) {
    rt.set_help_rows(vec![
        "alt+x  toggle_zoom".to_string(),
        "ctrl+shift+v  paste_from_clipboard".to_string(),
    ]);
    assert!(rt.toggle_help(), "help toggles on");
}

fn enter_fullscreen(rt: &mut Runtime) {
    // Alternate-screen entry, the fullscreen-app contract vim/less use.
    rt.handle_pty_bytes(b"\x1b[?1049h");
    rt.handle_pty_bytes(b"vim");
}

/// Issue #756 headline: `Esc` in vim while the help popup is visible must
/// dismiss the popup AND reach the PTY.
#[test]
fn esc_with_help_visible_reaches_fullscreen_pty() {
    let mut rt = make_runtime();
    enter_fullscreen(&mut rt);
    show_help(&mut rt);
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert!(!rt.help_visible(), "Esc dismissed the help popup");
    assert_eq!(
        out,
        Some(vec![27]),
        "dismissal Esc must reach the fullscreen app"
    );
    assert_eq!(
        rt.pending_input(),
        b"\x1b",
        "Esc byte delivered, not swallowed"
    );
}

/// The help overlay is informational everywhere, not only under alt-screen:
/// a shell prompt receives the `Esc` too.
#[test]
fn esc_with_help_visible_reaches_plain_pty() {
    let mut rt = make_runtime();
    show_help(&mut rt);
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert!(!rt.help_visible(), "Esc dismissed the help popup");
    assert_eq!(
        out,
        Some(vec![27]),
        "informational overlay does not consume"
    );
    assert_eq!(rt.pending_input(), b"\x1b");
}

/// Regression guard: with no overlay at all the same `Esc` still encodes.
#[test]
fn esc_without_overlays_still_encodes() {
    let mut rt = make_runtime();
    assert_eq!(
        rt.handle_key_event(esc(PressState::Pressed)),
        Some(vec![27])
    );
}

/// An `Esc` release is not a dismissal and never produces bytes.
#[test]
fn esc_release_does_not_dismiss_help_or_deliver() {
    let mut rt = make_runtime();
    show_help(&mut rt);
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Released));

    assert_eq!(out, None);
    assert!(rt.help_visible(), "release never dismisses");
    assert!(rt.pending_input().is_empty(), "release never delivers");
}

/// A non-`Esc` key is unaffected: help stays visible and the key encodes.
#[test]
fn non_esc_key_with_help_visible_still_encodes() {
    let mut rt = make_runtime();
    show_help(&mut rt);
    rt.drain_pending_input();

    assert_eq!(rt.handle_key_event(char_press('a')), Some(vec![b'a']));
    assert!(rt.help_visible(), "help is not modal");
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

/// Help plus paste together: one `Esc` drops both, and the paste gate keeps
/// the consume.
#[test]
fn esc_drops_help_and_paste_together_and_consumes() {
    let mut rt = make_runtime();
    show_help(&mut rt);
    assert!(rt.paste_text_via_gate("one\ntwo".to_string()));
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert_eq!(out, None, "a real gate owns the consume");
    assert!(!rt.help_visible(), "help also dismissed");
    assert!(!rt.has_pending_paste(), "paste cancelled");
    assert!(rt.pending_input().is_empty());
}

/// Selection clearing (CTX-0166) composes with the dismissal: the highlight
/// clears, help hides, and the `Esc` is delivered.
#[test]
fn esc_clears_selection_dismisses_help_and_delivers() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"select me");
    rt.select_all();
    show_help(&mut rt);
    rt.drain_pending_input();

    let out = rt.handle_key_event(esc(PressState::Pressed));

    assert!(!rt.has_selection(), "press clears the highlight");
    assert!(!rt.help_visible(), "help dismissed");
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

/// `Alt`-modified `Esc` in a fullscreen app with help shown is still not a
/// pure dismissal: help hides and the modified encoding reaches the PTY.
#[test]
fn alt_esc_with_help_visible_dismisses_and_delivers() {
    let mut rt = make_runtime();
    enter_fullscreen(&mut rt);
    show_help(&mut rt);
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

    assert!(!rt.help_visible());
    assert!(
        out.is_some(),
        "an Alt+Esc still routes instead of vanishing into the overlay"
    );
    assert!(!rt.pending_input().is_empty(), "bytes reached the PTY");
}
