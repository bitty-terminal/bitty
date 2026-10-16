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
fn terminal_diagnostics_are_bounded_and_stable_without_a_pty() {
    let rt = make_runtime();
    let first = rt.terminal_diagnostics(ViewId::new(1));
    let second = rt.terminal_diagnostics(ViewId::new(1));
    assert_eq!(
        first, second,
        "diagnostics must be stable when state is idle"
    );
    assert_eq!(first.view_id, 1);
    assert_eq!(first.terminal_id, 1);
    assert!(!first.has_session);
    assert_eq!(first.child_pid, None);
    assert_eq!(first.child_state, "absent");
    assert!(first.scrollback_lines <= bitty_term_state::SCROLLBACK_DEFAULT_LINES);
    assert!(first.retained_heap_bytes > 0);
}

#[test]
#[cfg(unix)]
fn pane_diagnostics_keep_pid_until_close_and_drop_the_session() {
    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    let view = ViewId::new(2);
    rt.set_layout(bitty_ui::LayoutNode::stack(vec![
        bitty_ui::LayoutNode::leaf(bitty_ui::View::new(ViewId::new(1), 80, 24)),
        bitty_ui::LayoutNode::leaf(bitty_ui::View::new(view, 80, 24)),
    ]));
    rt.spawn_shell_for_view(view, "/bin/sh", &["-c", "sleep 30"], 80, 24)
        .expect("pane shell must attach");
    let running = rt.terminal_diagnostics(view);
    assert!(running.has_session);
    assert_eq!(running.child_state, "unreaped");
    assert!(running.child_pid.is_some());
    assert!(running.retained_heap_bytes > 0);

    let output = (0..64)
        .map(|line| format!("pane-line-{line}\r\n"))
        .collect::<String>();
    rt.handle_pane_bytes(view, output.as_bytes());
    let after_output = rt.terminal_diagnostics(view);
    assert!(after_output.scrollback_lines > 0);
    assert!(after_output.scrollback_lines <= bitty_term_state::SCROLLBACK_DEFAULT_LINES);
    assert!(after_output.retained_heap_bytes >= running.retained_heap_bytes);
    assert_eq!(after_output, rt.terminal_diagnostics(view));

    assert!(rt.close_pane_session(&view));
    let closed = rt.terminal_diagnostics(view);
    assert!(!closed.has_session);
    assert_eq!(closed.child_pid, None);
    assert_eq!(closed.child_state, "absent");
}

#[test]
#[cfg(unix)]
fn pane_diagnostics_distinguish_reaped_child_before_session_close() {
    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    let view = ViewId::new(2);
    rt.set_layout(bitty_ui::LayoutNode::stack(vec![
        bitty_ui::LayoutNode::leaf(bitty_ui::View::new(ViewId::new(1), 80, 24)),
        bitty_ui::LayoutNode::leaf(bitty_ui::View::new(view, 80, 24)),
    ]));
    rt.spawn_shell_for_view(view, "/bin/sh", &["-c", "exit 0"], 80, 24)
        .expect("pane shell must attach");
    let pid = rt.pane_pid(&view).expect("reaped child keeps its pid");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while rt.pane_try_wait(&view).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "short-lived pane child did not exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let diagnostics = rt.terminal_diagnostics(view);
    assert!(diagnostics.has_session);
    assert_eq!(diagnostics.child_pid, Some(pid));
    assert_eq!(diagnostics.child_state, "reaped");
    assert!(rt.close_pane_session(&view));
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
