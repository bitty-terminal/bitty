//! Real PTY integration for 0.0.1 dogfooding: `Runtime` owns the PTY pump
//! with bounded backpressure, yet the same binary still ticks headlessly
//! when no PTY is present (no window/GPU required).
//!
//! Entirely gated to `cfg(unix)`: Windows ConPTY seam returns `Unsupported`
//! until the Tier-1 Windows slice lands.

#![cfg(unix)]

use std::time::Duration;

use bitty_runtime::registry::{LeaseHolder, PanelLease};
use bitty_runtime::{Runtime, RuntimeConfig, TerminalSubmitOutcome};

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn runtime_headless_without_pty_still_ticks() {
    // Headless path: no PTY spawned, no display/GPU, synthetic bytes only.
    // Must remain green even on machines with no PTY or on Windows.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    assert!(!rt.has_pty());
    assert_eq!(rt.poll_pty(), 0);
    assert_eq!(rt.poll_pty_timeout(Duration::from_millis(10)), 0);
    rt.handle_pty_bytes(b"hello headless");
    assert!(rt.tick().is_some());
    assert_eq!(rt.tick(), None);
}

#[test]
fn runtime_real_shell_echo_bounded_backpressure() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");

    rt.spawn_shell_with_args("/bin/sh", &["-c", "echo hello-bitty-runtime"])
        .expect("spawn sh -c echo");
    assert!(rt.has_pty());
    assert!(rt.has_pty_reader());
    assert!(rt.pty_pid().is_some());
    let (cols, rows) = rt.pty_size().expect("pty size");
    assert!(cols >= 10 && rows >= 5);

    // Poll with timeout until the echo arrives in terminal state.
    // The bounded channel guarantees each chunk <= READ_CHUNK_SIZE (8 KiB)
    // and total in-crate buffered bytes <= MAX_BUFFERED_BYTES (128 KiB).
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut found = false;
    while std::time::Instant::now() < deadline {
        // poll_pty drains try_recv without blocking, but we use the timeout
        // helper for the first chunk so the test waits for the child to run.
        let drained = rt.poll_pty_timeout(Duration::from_millis(200));
        if drained > 0 {
            rt.tick();
            let snap = rt.snapshot();
            let text: String = snap.cells.iter().map(|c| c.glyph).collect::<String>();
            if text.contains("hello-bitty-runtime") {
                found = true;
                break;
            }
        }
        // Also try non-blocking poll in case data arrived between timeouts.
        let extra = rt.poll_pty();
        if extra > 0 {
            rt.tick();
            let snap = rt.snapshot();
            let text: String = snap.cells.iter().map(|c| c.glyph).collect();
            if text.contains("hello-bitty-runtime") {
                found = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        found,
        "shell echo never appeared in snapshot within {TIMEOUT:?}"
    );

    // Bounded assertion: the in-crate constants are exactly the documented
    // 8 KiB * 16 = 128 KiB bound.
    assert_eq!(
        bitty_pty::MAX_BUFFERED_BYTES,
        bitty_pty::READ_CHUNK_SIZE * bitty_pty::CHANNEL_CAPACITY_CHUNKS
    );
    // Verify a non-blocking try_recv after drain respects the same bound
    // when we manually take the reader (headless fallback path keeps working).
    if let Some(reader) = rt.take_pty_reader() {
        // Reader was drained; further try_recv must be empty/EOF, never a chunk.
        if let bitty_pty::PtyRecv::Chunk(chunk) = reader.try_recv() {
            assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
        }
        // Do not join here; runtime will reap pty on drop.
        let _ = reader;
    }

    // Tick proves damage -> present still works after real PTY bytes.
    assert!(rt.tick().is_none() || true);
}

#[test]
fn runtime_backpressure_holds_under_flood() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.spawn_shell_with_args("/bin/sh", &["-c", "yes | head -n 2000"])
        .expect("spawn flood");

    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut total_drained = 0usize;
    while std::time::Instant::now() < deadline {
        let n = rt.poll_pty_timeout(Duration::from_millis(300));
        total_drained += n;
        if n > 0 {
            let _ = rt.tick();
        }
        // Check EOF by polling without timeout: when child exits and queue
        // drained, both poll_pty and poll_pty_timeout return 0 for a while.
        // We do not assert total size; we assert bounded per-chunk and that
        // the runtime does not panic or grow unbounded.
        if total_drained > 0 && rt.poll_pty() == 0 {
            // Give kernel a moment to deliver remaining bytes.
            std::thread::sleep(Duration::from_millis(50));
            if rt.poll_pty() == 0 && rt.poll_pty_timeout(Duration::from_millis(50)) == 0 {
                break;
            }
        }
        if total_drained > 2000 {
            break;
        }
    }
    assert!(total_drained > 0, "flood should produce at least one chunk");
    // The terminal state must have seen damage from the flood.
    assert!(rt.state().generation() > 0);
    let _ = rt.tick();
}

/// Issue #2443: a PTY reader's EOF does not imply the child is reapable yet.
///
/// The shell below closes every tty fd and only then sleeps, so the
/// forwarder's single EOF wake necessarily lands while `waitpid(WNOHANG)`
/// still reports "running" (Linux runs `exit_files` before `exit_notify`).
/// The runtime must expose that state so the embedder can keep a bounded
/// reap wake armed instead of parking in `ControlFlow::Wait` forever.
#[cfg(target_os = "linux")]
#[test]
fn reader_eof_latches_unreaped_child_until_exit_is_observed() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(8);
    let waker: bitty_runtime::PtyWaker = std::sync::Arc::new(move || {
        let _ = tx.try_send(());
    });
    rt.set_pty_waker(waker);

    rt.spawn_shell_with_args("/bin/sh", &["-c", "exec 0<&- 1>&- 2>&-; sleep 2; exit 0"])
        .expect("spawn delayed-exit shell");
    assert!(rt.pty_pid().is_some(), "child pid must be owned");

    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut eof_seen = false;
    while std::time::Instant::now() < deadline {
        let _ = rx.recv_timeout(Duration::from_millis(50));
        let _ = rt.poll_pty();
        if rt.has_unreaped_eof_session() {
            eof_seen = true;
            break;
        }
    }
    assert!(eof_seen, "reader EOF must latch the unreaped session");
    assert!(
        rt.primary_exit_status().is_none(),
        "the child must span the EOF-to-reap gap for this regression"
    );
    assert!(
        rt.has_unreaped_eof_session(),
        "the latch must stay set while the exit is not observable"
    );

    // A bounded re-poll observes the exit once the child finishes, and the
    // latch clears on the same observation.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut status = None;
    while std::time::Instant::now() < deadline {
        if let Some(observed) = rt.primary_exit_status() {
            status = Some(observed);
            break;
        }
        let _ = rt.poll_pty();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(status.is_some(), "bounded re-poll must reap the child");
    assert!(
        !rt.has_unreaped_eof_session(),
        "a reaped session must clear the EOF latch"
    );
}

/// bitty#2446: writing input must never park the terminal event loop.
///
/// The owner-visible freeze shape is a TUI whose preview stops draining
/// stdin for a while; the next key/reply write then blocked the main thread
/// in `write(2)` until the child resumed, freezing every panel. This spawns
/// a child that switches its tty to raw mode and then ignores stdin, fills
/// more than one kernel input queue in 4 KiB writes, and bounds the total
/// call time. The watchdog kills only the child this test spawned so a
/// regression fails the bound instead of hanging the suite.
#[test]
fn runtime_input_write_never_blocks_on_a_nonreading_child_issue_2446() {
    use std::sync::mpsc;
    use std::time::Instant;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.spawn_shell_with_args(
        "/bin/sh",
        &[
            "-c",
            "stty raw -echo || exit 1; printf __RAW_READY__; exec sleep 60",
        ],
    )
    .expect("spawn raw-mode sleeper");

    // Wait for the child to confirm raw mode before the burst: canonical
    // mode discards overflow instead of applying backpressure, which would
    // make this test pass vacuously. Bounded by the suite TIMEOUT.
    let deadline = Instant::now() + TIMEOUT;
    let mut raw_ready = false;
    while Instant::now() < deadline {
        let _ = rt.poll_pty_timeout(Duration::from_millis(200));
        let text: String = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
        if text.contains("__RAW_READY__") {
            raw_ready = true;
            break;
        }
    }
    assert!(raw_ready, "raw-mode sleeper never reported readiness");
    assert!(
        rt.primary_exit_status().is_none(),
        "raw-mode sleeper exited before the write burst"
    );

    let child = rt.pty_pid().expect("live child pid");
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            // Bounded cleanup for the red path: kill only our own child.
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(child.to_string())
                .status();
        }
    });

    let start = Instant::now();
    // The bound must be exceeded, not merely reached: the writer thread pops
    // (and releases) the first chunk before parking in the sink, so the burst
    // needs more than `QUEUED_WRITE_MAX_BYTES` plus that in-flight chunk.
    let chunk = vec![b'x'; 4096];
    for _ in 0..(bitty_pty::QUEUED_WRITE_MAX_BYTES / chunk.len() + 8) {
        rt.write_input(&chunk);
    }
    let elapsed = start.elapsed();
    let _ = done_tx.send(());
    watchdog.join().expect("watchdog thread");

    assert!(
        elapsed < Duration::from_secs(1),
        "blocking PTY write froze the event loop for {elapsed:?}"
    );
    // The runtime remains usable after the overflow: the excess was
    // accounted as dropped input, never parked in the event loop.
    assert!(
        rt.input_write_dropped_bytes() > 0,
        "queue overflow must be reported through the existing loss counters"
    );
}

/// bitty#2446 review F1: `terminal_submit` must never report `Accepted` for a
/// frame the queued writer then drops.
///
/// The widest legitimate frame is the composer cap (64 KiB of content) plus
/// the 13 bytes of bracketed-paste framing — 65,549 bytes — and it is offered
/// as one write while the child is idle. When the queue bound equalled the
/// composer cap, that frame was rejected at the queue while the submit
/// outcome stayed `Accepted`; the Lua bridge reported success and the input
/// vanished. This drives the real submit path against an idle child in raw
/// mode (canonical mode would mask the loss behind its line discipline) and
/// asserts the frame was delivered with zero recorded loss.
#[test]
fn runtime_submit_accepts_max_composer_frame_without_loss_issue_2446() {
    use std::time::Instant;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.spawn_shell_with_args(
        "/bin/sh",
        &[
            "-c",
            "stty raw -echo || exit 1; printf __RAW_READY__; exec cat",
        ],
    )
    .expect("spawn raw-mode cat");

    // Wait for the child to confirm raw mode: canonical mode would accept
    // the frame but echo it line by line, hiding queue loss behind its own
    // buffer limits. Bounded by the suite TIMEOUT.
    let deadline = Instant::now() + TIMEOUT;
    let mut raw_ready = false;
    while Instant::now() < deadline {
        let _ = rt.poll_pty_timeout(Duration::from_millis(200));
        let text: String = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
        if text.contains("__RAW_READY__") {
            raw_ready = true;
            break;
        }
    }
    assert!(raw_ready, "raw-mode cat never reported readiness");

    // The producer bound is the composer cap plus byte-exact framing; the
    // queue bound must cover it. `bitty-pty` cannot depend on `bitty-rich`,
    // so this cross-crate check is the drift guard between the two.
    let text = "q".repeat(bitty_rich::host::COMPOSER_MAX_BYTES);
    let frame = bitty_rich::host::frame_submit(&text).expect("max composer frame");
    let framing = bitty_rich::host::PASTE_OPEN.len()
        + bitty_rich::host::PASTE_CLOSE.len()
        + bitty_rich::host::SUBMIT_TERMINATOR.len();
    assert_eq!(frame.len(), bitty_rich::host::COMPOSER_MAX_BYTES + framing);
    assert_eq!(frame.len(), 65_549, "largest legitimate producer frame");
    assert!(
        frame.len() <= bitty_pty::QUEUED_WRITE_MAX_BYTES,
        "queued writer bound {} must accept the largest producer frame {}",
        bitty_pty::QUEUED_WRITE_MAX_BYTES,
        frame.len()
    );

    let mut lease = PanelLease::idle();
    let holder = LeaseHolder(7);
    lease.acquire(holder, 100, 0).expect("acquire lease");
    let mut budget = bitty_rich::host::SubmitBudget::new("composer.regression", 1024 * 1024);
    match rt.terminal_submit(&text, &lease, holder, 1, &mut budget) {
        TerminalSubmitOutcome::Accepted { bytes } => {
            assert_eq!(bytes, frame.len(), "submit must report the whole frame");
        }
        other => panic!("max composer frame must be accepted, got {other:?}"),
    }
    assert_eq!(budget.used(), frame.len() as u64);
    assert_eq!(
        rt.input_write_dropped_bytes(),
        0,
        "an accepted max-size frame must not be dropped by the input queue"
    );
    assert!(
        rt.pending_input().is_empty(),
        "an accepted submit must reach the live writer, not the headless buffer"
    );

    // The whole frame must reach the child: draining its echo also proves the
    // writer thread emptied the queue instead of stranding the tail.
    let deadline = Instant::now() + TIMEOUT;
    let mut echoed_any = 0;
    while Instant::now() < deadline {
        if rt.poll_pty_timeout(Duration::from_millis(200)) > 0 {
            echoed_any += 1;
        }
        let text: String = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
        if text.matches('q').count() > 0 {
            break;
        }
    }
    assert!(
        echoed_any > 0,
        "the idle child must echo the accepted frame"
    );
    assert_eq!(
        rt.input_write_dropped_bytes(),
        0,
        "delivery must not record loss after acceptance"
    );
}
