//! Per-pane shell sessions (CTX-0176, Issue #274): every split leaf owns a
//! private shell/PTY with its own grid, and input routes to the focused
//! leaf only — panes never mirror, input never broadcasts.
//!
//! Unix-only: spawning needs a POSIX shell plus PTY master semantics
//! (mirrors `real_pty.rs`).

#![cfg(unix)]

use std::time::Duration;

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const TIMEOUT: Duration = Duration::from_secs(10);

fn two_pane_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let layout = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    );
    rt.set_layout(layout);
    rt
}

fn primary_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|c| c.glyph).collect()
}

fn pane_text(rt: &Runtime, view: &ViewId) -> String {
    match rt.pane_snapshot(view) {
        Some(snap) => snap.cells.iter().map(|c| c.glyph).collect(),
        None => String::new(),
    }
}

/// Polls (primary + pane pumps) and ticks until the pane grid shows
/// `needle` or the timeout expires.
fn wait_for_pane_text(rt: &mut Runtime, view: ViewId, needle: &str) -> bool {
    let deadline = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if pane_text(rt, &view).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Polls and ticks until the primary grid shows `needle` or times out.
fn wait_for_primary_text(rt: &mut Runtime, needle: &str) -> bool {
    let deadline = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if primary_text(rt).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn split_leaves_own_distinct_shells_with_isolated_grids() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "echo pane-primary; sleep 30"])
        .expect("spawn primary shell");
    rt.spawn_shell_for_view(
        ViewId::new(2),
        "/bin/sh",
        &["-c", "echo pane-split; sleep 30"],
        40,
        12,
    )
    .expect("spawn pane shell");
    assert!(rt.has_pane_session(&ViewId::new(2)));
    assert!(!rt.has_pane_session(&ViewId::new(1)));
    assert_eq!(rt.pane_count(), 1);
    assert_eq!(rt.pane_session_ids(), vec![ViewId::new(2)]);
    // Distinct live children behind the two leaves.
    let primary_pid = rt.pty_pid().expect("primary pid");
    let pane_pid = rt.pane_pid(&ViewId::new(2)).expect("pane pid");
    assert_ne!(primary_pid, pane_pid);

    // Each grid shows only its own shell's output: no mirroring.
    assert!(
        wait_for_primary_text(&mut rt, "pane-primary"),
        "primary shell output never arrived"
    );
    assert!(
        wait_for_pane_text(&mut rt, ViewId::new(2), "pane-split"),
        "pane shell output never arrived"
    );
    let _ = rt.poll_pty();
    rt.tick();
    assert!(
        !primary_text(&rt).contains("pane-split"),
        "primary grid mirrored the pane shell"
    );
    assert!(
        !pane_text(&rt, &ViewId::new(2)).contains("pane-primary"),
        "pane grid mirrored the primary shell"
    );
}

#[test]
fn input_routes_to_focused_pane_only() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell("/bin/sh").expect("spawn primary shell");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 40, 12)
        .expect("spawn pane shell");

    // Focus the split leaf and type: kernel echo plus shell output land
    // there, never in the primary grid.
    assert!(rt.set_focus(ViewId::new(2)));
    rt.push_input_bytes(b"echo MARKER-FOCUSED-PANE\n");
    assert!(
        wait_for_pane_text(&mut rt, ViewId::new(2), "MARKER-FOCUSED-PANE"),
        "focused-pane input never visibly landed"
    );
    let _ = rt.poll_pty();
    rt.tick();
    assert!(
        !primary_text(&rt).contains("MARKER-FOCUSED-PANE"),
        "input leaked into the unfocused primary pane"
    );

    // Reverse: focus the primary leaf; the split grid stays clean.
    assert!(rt.set_focus(ViewId::new(1)));
    rt.push_input_bytes(b"echo MARKER-PRIMARY-PANE\n");
    assert!(
        wait_for_primary_text(&mut rt, "MARKER-PRIMARY-PANE"),
        "primary-pane input never visibly landed"
    );
    let _ = rt.poll_pty();
    rt.tick();
    assert!(
        !pane_text(&rt, &ViewId::new(2)).contains("MARKER-PRIMARY-PANE"),
        "input leaked into the unfocused split pane"
    );
}

#[test]
fn multipane_input_falls_back_to_buffer_without_writer() {
    // Pane session live, primary shell never spawned: focused-pane bytes
    // take the pane writer (nothing buffered), while the writer-less
    // primary leaf still lands in the bounded headless buffer.
    let mut rt = two_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 40, 12)
        .expect("spawn pane shell");
    assert!(rt.set_focus(ViewId::new(2)));
    rt.push_input_bytes(b"routed-to-pane");
    assert_eq!(rt.pending_input_len(), 0);
    assert!(rt.set_focus(ViewId::new(1)));
    rt.push_input_bytes(b"buffered");
    assert_eq!(rt.pending_input(), b"buffered");
}

#[test]
fn pane_grid_tracks_leaf_allocation_across_layout_changes() {
    // Horizontal 80x24 split: leaf 2 owns the right 40x24. Spawn its
    // session deliberately off-size, then install a vertical split where
    // leaf 2 owns the bottom 80x12: set_layout re-syncs the pane grid
    // (+ PTY winsize) to the new allocation instead of rendering stale.
    let mut rt = two_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 10, 10)
        .expect("spawn pane shell");
    let before = rt.pane_snapshot(&ViewId::new(2)).expect("pane grid");
    assert_eq!((before.width, before.height), (10, 10));
    rt.set_layout(LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    let after = rt.pane_snapshot(&ViewId::new(2)).expect("pane grid");
    assert_eq!((after.width, after.height), (80, 12));
}

#[test]
fn close_pane_session_tears_down_child() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &[], 40, 12)
        .expect("spawn pane shell");
    assert!(rt.pane_pid(&ViewId::new(2)).is_some());
    assert!(rt.close_pane_session(&ViewId::new(2)));
    assert!(!rt.has_pane_session(&ViewId::new(2)));
    assert_eq!(rt.pane_pid(&ViewId::new(2)), None);
    assert!(rt.pane_session_ids().is_empty());
    assert!(rt.pane_snapshot(&ViewId::new(2)).is_none());
    // Ticking a layout whose leaf lost its session falls back to the
    // shared grid without incident.
    let _ = rt.poll_pty();
    let _ = rt.tick();
    assert!(!rt.close_pane_session(&ViewId::new(2)));
}
