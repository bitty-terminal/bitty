//! Runtime Pane-session tests.
//!
//! pty-gate-exempt-file: every spawn call in this file asserts rejection
//! (blank program or unknown view; validation precedes any spawn, no PTY is
//! ever created), so no live-spawn gate is needed (CTX-0267).
//!
//! Moved verbatim from the inline `runtime.rs` unit tests as part of
//! the CTX-0232 pure-move split. Adaptations are wiring only:
//! `super::*` became explicit imports and the private `layout` field
//! reads became the public `layout()` getter (identical semantics).
use bitty_runtime::{Runtime, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

#[test]
fn spawn_shell_blank_program_rejected_without_touching_pty() {
    let mut rt = make_runtime();
    assert!(rt.spawn_shell("").is_err());
    assert!(rt.spawn_shell("   ").is_err());
}

#[test]
fn pane_sessions_default_to_empty() {
    // CTX-0176: single-pane runtimes own no pane session; the primary
    // PTY/state path is untouched.
    let mut rt = make_runtime();
    assert_eq!(rt.pane_count(), 0);
    assert!(rt.pane_session_ids().is_empty());
    assert!(!rt.has_pane_session(&ViewId::new(2)));
    assert_eq!(rt.pane_pid(&ViewId::new(2)), None);
    assert!(rt.pane_snapshot(&ViewId::new(2)).is_none());
    assert!(!rt.close_pane_session(&ViewId::new(2)));
}

#[test]
fn pane_spawn_rejects_blank_program_and_unknown_view() {
    // CTX-0176: validation precedes any spawn; no PTY is touched.
    let mut rt = make_runtime();
    assert!(
        rt.spawn_shell_for_view(ViewId::new(1), "", &[], 80, 24)
            .is_err()
    );
    assert!(
        rt.spawn_shell_for_view(ViewId::new(1), "   ", &[], 80, 24)
            .is_err()
    );
    assert!(!rt.has_pty());
    assert_eq!(rt.pane_count(), 0);
    // Leaf 9 is not in the single-leaf default layout.
    assert!(
        rt.spawn_shell_for_view(ViewId::new(9), "/bin/sh", &[], 80, 24)
            .is_err()
    );
    assert_eq!(rt.pane_count(), 0);
}

#[test]
fn pane_pump_and_poll_are_noops_without_sessions() {
    // CTX-0176: unknown ids never touch the primary pipeline, polling an
    // unspawned runtime still drains nothing, and the single-pane input
    // path still lands in the headless buffer.
    let mut rt = make_runtime();
    rt.handle_pane_bytes(ViewId::new(2), b"hello");
    rt.handle_pane_bytes(ViewId::new(2), b"");
    assert_eq!(rt.poll_pty(), 0);
    rt.push_input_bytes(b"abc");
    assert_eq!(rt.pending_input(), b"abc");
}
