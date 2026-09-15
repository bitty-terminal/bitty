//! Runtime Drop/join lifecycle regression (CTX-0472).
//!
//! Verified leaks: respawn/close detached forwarder `JoinHandle`s and fired a
//! post-destroy waker; `Runtime` had no `Drop`. Accept: bounded `Drop` +
//! join with timeout, no post-destroy wakes, leak regression tests.
//!
//! Unix-only: live PTY spawn needs POSIX shell + PTY master semantics
//! (mirrors `pty_wakeup.rs` / `pane_da_wakeup.rs`).

#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bitty_runtime::{LayoutNode, PtyWaker, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const WAIT: Duration = Duration::from_secs(10);
const TEARDOWN_BOUND: Duration = Duration::from_secs(5);

fn counting_waker() -> (PtyWaker, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let clone = Arc::clone(&count);
    let waker: PtyWaker = Arc::new(move || {
        clone.fetch_add(1, Ordering::SeqCst);
    });
    (waker, count)
}

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

/// Drop joins the primary forwarder promptly and fires no post-destroy wake.
#[test]
fn runtime_drop_joins_forwarder_without_post_destroy_wake() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let (waker, count) = counting_waker();
    rt.set_pty_waker(waker);
    rt.spawn_shell_with_args("/bin/sh", &["-c", "echo DROP-MARKER-0472"])
        .expect("spawn echo marker");
    assert!(rt.has_pty_forwarder());

    // Let the forwarder deliver at least the EOF wake before teardown.
    let deadline = Instant::now() + WAIT;
    while count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let start = Instant::now();
    drop(rt);
    let elapsed = start.elapsed();
    assert!(
        elapsed < TEARDOWN_BOUND,
        "Runtime Drop hung on forwarder join: {elapsed:?}"
    );

    // Post-destroy: a detached forwarder would still own its waker clone and
    // could fire after Drop. The counter must stay stable across a quiet
    // window (the EOF wake already fired pre-drop).
    let settled = count.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        count.load(Ordering::SeqCst),
        settled,
        "post-destroy waker fired after Runtime Drop"
    );
}

/// Respawn joins the old forwarder instead of detaching it: prompt, still
/// forwarded afterwards, and no post-respawn wake storm from the old thread.
#[test]
fn respawn_joins_old_forwarder_with_timeout() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let (waker, _count) = counting_waker();
    rt.set_pty_waker(waker);
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn quiet child");
    assert!(rt.has_pty_forwarder());

    let start = Instant::now();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("respawn must join old forwarder");
    let elapsed = start.elapsed();
    assert!(
        elapsed < TEARDOWN_BOUND,
        "respawn hung joining old forwarder: {elapsed:?}"
    );
    assert!(
        rt.has_pty_forwarder(),
        "respawn must re-arm the forwarder for the new child"
    );
}

/// Pane close joins its forwarder: prompt close, session gone, no leak.
#[test]
fn close_pane_session_joins_forwarder() {
    let mut rt = two_pane_runtime();
    let (waker, _count) = counting_waker();
    rt.set_pty_waker(waker);
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn primary");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn pane");
    assert!(rt.has_pane_forwarder(&ViewId::new(2)));

    let start = Instant::now();
    assert!(rt.close_pane_session(&ViewId::new(2)));
    let elapsed = start.elapsed();
    assert!(
        elapsed < TEARDOWN_BOUND,
        "close_pane_session hung joining forwarder: {elapsed:?}"
    );
    assert!(!rt.has_pane_session(&ViewId::new(2)));
}

/// Pane respawn joins the old pane forwarder instead of detaching it.
#[test]
fn respawn_pane_joins_old_forwarder() {
    let mut rt = two_pane_runtime();
    let (waker, _count) = counting_waker();
    rt.set_pty_waker(waker);
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn pane");
    assert!(rt.has_pane_forwarder(&ViewId::new(2)));

    let start = Instant::now();
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("respawn pane must join old forwarder");
    let elapsed = start.elapsed();
    assert!(
        elapsed < TEARDOWN_BOUND,
        "pane respawn hung joining old forwarder: {elapsed:?}"
    );
    assert!(rt.has_pane_forwarder(&ViewId::new(2)));
}

/// Explicit shutdown clears the waker, joins forwarders, and releases PTYs.
#[test]
fn explicit_shutdown_clears_waker_and_releases_ptys() {
    let mut rt = two_pane_runtime();
    let (waker, _count) = counting_waker();
    rt.set_pty_waker(waker);
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn primary");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn pane");

    let start = Instant::now();
    let joined = rt.shutdown();
    let elapsed = start.elapsed();
    assert!(
        elapsed < TEARDOWN_BOUND,
        "explicit shutdown hung: {elapsed:?}"
    );
    assert!(joined, "healthy forwarders must join");
    assert!(!rt.has_pty_waker(), "shutdown must clear the waker");
    assert!(!rt.has_pty(), "shutdown releases the primary PTY");
    assert_eq!(rt.pane_count(), 0, "shutdown releases pane sessions");
}
