//! Detached primary floats stay typable end to end (CodeRabbit PR #1977).
//!
//! Entirely gated to `cfg(unix)`: attaching a live primary shell needs a
//! POSIX shell plus PTY master semantics (mirrors `pty_wakeup.rs`). The
//! headless ownership half lives in `floating_present.rs` so it also runs
//! on Windows; this file proves input bytes reach the live primary shell
//! while its leaf is detached, and keep reaching it after the
//! detach/restore cycle — never merely buffered headless.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bitty_runtime::{LayoutNode, Runtime, SplitAxis, UiRect, View, ViewId};
use bitty_test_support::require_pty;

/// Marker round-tripped through the live primary shell.
const MARKER: &str = "FLOAT-PRIMARY-LIVE-1977";
/// Bound for one shell round trip; a healthy shell answers in milliseconds.
const WAIT: Duration = Duration::from_secs(10);

fn two_pane_runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("headless build");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    rt
}

fn snapshot_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|cell| cell.glyph).collect()
}

/// Pumps the primary PTY until `needle` lands in the primary grid.
fn wait_for_primary_text(rt: &mut Runtime, needle: &str) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if snapshot_text(rt).contains(needle) {
            return true;
        }
    }
    false
}

#[test]
fn detached_primary_float_stays_typable_across_detach_restore() {
    require_pty!();
    let mut rt = two_pane_runtime();
    // Attach the primary shell on leaf 1 (focused after the layout install).
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    rt.spawn_shell_with_args("/bin/sh", &["-c", "exec sleep 30"])
        .expect("spawn primary shell");
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));

    // Detach the primary owner: ownership stays on the float.
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));
    assert!(rt.set_focus(ViewId::new(1)));

    // Input to the focused detached float must reach the live PTY writer,
    // never buffer headless (the pre-fix strand returned false here).
    assert!(
        rt.push_input_bytes(b"echo FLOAT-PRIMARY-LIVE-1977\r"),
        "input to the detached primary float must reach the shell"
    );
    assert!(
        wait_for_primary_text(&mut rt, MARKER),
        "the live shell must answer through the detached float"
    );

    // The restore cycle keeps the same owner live.
    let restored = rt
        .float_restore(ViewId::new(1))
        .expect("restore must apply");
    assert_eq!(restored, ViewId::new(1));
    assert_eq!(rt.primary_view(), Some(ViewId::new(1)));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    assert!(
        rt.push_input_bytes(b"echo BACK-AGAIN\r"),
        "input must still reach the shell after restore"
    );
    assert!(
        wait_for_primary_text(&mut rt, "BACK-AGAIN"),
        "the live shell must answer after restore"
    );
}

#[test]
fn detached_primary_float_counts_as_workspace_occupant() {
    // The primary owner may be detached: the workspace still owns the
    // primary shell, so occupancy (cycle/confirm gates) stays truthful.
    // Needs the recipe latch, so this runs live alongside the shell.
    require_pty!();
    let mut rt = two_pane_runtime();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "exec sleep 30"])
        .expect("spawn primary shell");
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert!(rt.workspace_owns_primary(0));
    assert!(rt.workspace_is_occupied(0));

    rt.workspace_new().expect("second workspace must open");
    assert!(
        rt.workspace_owns_primary(0),
        "the home slot still owns the detached primary"
    );
    assert!(
        !rt.workspace_owns_primary(1),
        "the fresh workspace owns nothing"
    );
}
