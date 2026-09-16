//! CTX-0486 hostile probes: OSC 8 `file:` URL policy and clipboard
//! default-capture posture at the public `bitty-rich` boundary.
//!
//! Reviewer-facing red/green probes for the rich-surfaces remainder (parent
//! issue #748). Red run against the pre-fix tree (recorded in the PR body):
//! `is_safe_hyperlink_uri("file:///etc/passwd")` was `true`, and a default
//! `ClipboardState` retained an OSC 52 write (`is_empty()` was false).
//! They now stay as regression evidence.

use bitty_rich::clipboard::{
    ClipboardGrantScope, ClipboardOutcome, ClipboardPolicy, ClipboardState,
};
use bitty_rich::hyperlink::is_safe_hyperlink_uri;
use bitty_vt::{BoundedBytes, ClipboardOp, TerminalAction};

fn write(data: &[u8]) -> TerminalAction {
    TerminalAction::OscClipboard {
        op: ClipboardOp::Write,
        data: BoundedBytes::new(data.to_vec()),
    }
}

fn read() -> TerminalAction {
    TerminalAction::OscClipboard {
        op: ClipboardOp::Read,
        data: BoundedBytes::new(b"?".to_vec()),
    }
}

#[test]
fn osc8_file_scheme_links_are_never_presented() {
    for uri in [
        "file:///etc/passwd",
        "file:///tmp/report.txt",
        "file://attacker/share",
        "file://server/share",
        "FILE:///etc/passwd",
    ] {
        assert!(
            !is_safe_hyperlink_uri(uri),
            "file: URI presented as a clickable link: {uri}"
        );
    }
}

#[test]
fn clipboard_default_posture_does_not_capture() {
    let mut state = ClipboardState::new();
    assert_eq!(state.policy(), ClipboardPolicy::Gated);
    let outcome = state.handle_action(&write(b"secret"));
    assert_eq!(outcome, ClipboardOutcome::WriteDenied);
    assert!(state.is_empty());
    assert!(state.last_write().is_none());
    assert_eq!(state.captured_writes(), 0);

    // A granted read over a default (non-Allow) state cannot expose the
    // payload the terminal tried to seed.
    let token = state
        .grant_read(ClipboardGrantScope(7))
        .expect("OS entropy available in tests");
    match state.handle_action_with_token(&read(), Some(&token), ClipboardGrantScope(7)) {
        ClipboardOutcome::ReadGranted { data } => {
            assert!(data.as_bytes().is_empty(), "uncaptured payload leaked");
        }
        other => panic!("expected ReadGranted, got {other:?}"),
    }
}

#[test]
fn clipboard_capture_requires_explicit_allow() {
    let mut allowed = ClipboardState::with_policy(ClipboardPolicy::Allow);
    assert!(matches!(
        allowed.handle_action(&write(b"consented")),
        ClipboardOutcome::WriteCaptured { .. }
    ));
    assert_eq!(
        allowed.last_write().expect("captured").data.as_bytes(),
        b"consented"
    );

    for policy in [ClipboardPolicy::Gated, ClipboardPolicy::Denied] {
        let mut state = ClipboardState::with_policy(policy);
        assert_eq!(
            state.handle_action(&write(b"secret")),
            ClipboardOutcome::WriteDenied,
            "policy {policy:?} captured a write"
        );
        assert!(state.is_empty());
        assert_eq!(state.denied_writes(), 1);
    }
}
