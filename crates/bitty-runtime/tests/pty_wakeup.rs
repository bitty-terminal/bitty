//! PTY wakeup integration: readability wakes the consumer without polling.
//!
//! Entirely gated to `cfg(unix)`: Windows ConPTY seam returns `Unsupported`
//! until the Tier-1 Windows slice lands. No filesystem or network access
//! beyond spawning the PTY child itself.

#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bitty_runtime::{Runtime, RuntimeConfig};

const MARKER: &str = "WAKEUP-MARKER-0145";
const WAIT: Duration = Duration::from_secs(10);

/// Installs a non-blocking wake counter; returns the counter plus a wake
/// signal receiver the test parks on (no polling, no manual pumps).
fn install_test_waker(rt: &mut Runtime) -> (Arc<AtomicUsize>, std::sync::mpsc::Receiver<()>) {
    let count = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(64);
    let count_clone = Arc::clone(&count);
    let waker: bitty_runtime::PtyWaker = Arc::new(move || {
        count_clone.fetch_add(1, Ordering::SeqCst);
        let _ = tx.try_send(());
    });
    rt.set_pty_waker(waker);
    (count, rx)
}

fn snapshot_text(rt: &Runtime) -> String {
    let snap = rt.snapshot();
    snap.cells.iter().map(|c| c.glyph).collect()
}

#[test]
fn pty_readability_wakes_without_manual_pumps() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    // Arm wakeup before spawn so the forwarder is active from the first byte.
    let (wakes, wake_rx) = install_test_waker(&mut rt);
    assert!(rt.has_pty_waker());
    assert!(!rt.has_pty_forwarder());

    let cmd = format!("echo {MARKER}");
    rt.spawn_shell_with_args("/bin/sh", &["-c", &cmd])
        .expect("spawn sh echo marker");
    assert!(rt.has_pty_reader());
    assert!(rt.has_pty_forwarder());

    // ZERO manual pumps before the first wake: park on the wake signal.
    wake_rx
        .recv_timeout(WAIT)
        .expect("forwarder must wake on PTY readability without polling");
    assert!(
        wakes.load(Ordering::SeqCst) >= 1,
        "wake counter must record the readability signal"
    );

    // Drive at most N wake-gated ticks (each drain follows a wake, never a
    // sleep-poll). The marker is one small chunk; one drain usually suffices.
    let deadline = std::time::Instant::now() + WAIT;
    let mut found = false;
    for _ in 0..20 {
        let _ = rt.poll_pty();
        rt.tick();
        if snapshot_text(&rt).contains(MARKER) {
            found = true;
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        // Wait for the next wake (EOF wake covers the final chunk); a timeout
        // here means no more data is coming, so drain once more and stop.
        match wake_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => continue,
            Err(_) => {
                let _ = rt.poll_pty();
                rt.tick();
                if snapshot_text(&rt).contains(MARKER) {
                    found = true;
                }
                break;
            }
        }
    }
    assert!(
        found,
        "snapshot must contain {MARKER} within wake-gated ticks (wakes={})",
        wakes.load(Ordering::SeqCst)
    );

    // Bounded contract holds on the wakeup path too.
    assert_eq!(
        bitty_pty::MAX_BUFFERED_BYTES,
        bitty_pty::READ_CHUNK_SIZE * bitty_pty::CHANNEL_CAPACITY_CHUNKS
    );
    assert_eq!(
        bitty_runtime::PTY_FORWARD_CAPACITY_CHUNKS,
        bitty_pty::CHANNEL_CAPACITY_CHUNKS
    );
}

#[test]
fn quiet_child_idles_without_wakeups() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let (wakes, _wake_rx) = install_test_waker(&mut rt);
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn quiet child");
    assert!(rt.has_pty_forwarder());

    // No polling during the quiet window: the forwarder parks in `recv`.
    std::thread::sleep(Duration::from_millis(1000));
    let count = wakes.load(Ordering::SeqCst);
    assert_eq!(
        count, 0,
        "quiet child must idle at ~0 wakeups (got {count}); no busy-loop allowed"
    );
    // A single drain after the quiet window confirms nothing arrived.
    assert_eq!(rt.poll_pty(), 0);
}
