//! CTX-0476 hostile probes: `poll_pty` byte/time budgets and waker-merge bounds.
//!
//! Entirely gated to `cfg(unix)`: needs a real PTY child emitting a burst.
//! No filesystem or network access beyond spawning the PTY child itself.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bitty_runtime::{
    POLL_PTY_MAX_BYTES, POLL_PTY_MAX_CHUNKS, POLL_PTY_TIME_BUDGET, Runtime, RuntimeConfig,
};

const WAIT: Duration = Duration::from_secs(15);

#[test]
fn poll_budgets_match_accepted_pipeline_bounds() {
    // 32 chunks x 8 KiB = 256 KiB matches the worst-case total buffered
    // across both pump stages (2 x 128 KiB). The time budget keeps one poll
    // inside a frame (10 ms order, mirrors spawn-poll cadence).
    assert_eq!(POLL_PTY_MAX_CHUNKS, 32);
    assert_eq!(POLL_PTY_MAX_BYTES, 32 * 8 * 1024);
    assert_eq!(POLL_PTY_TIME_BUDGET, Duration::from_millis(10));
    assert_eq!(
        POLL_PTY_MAX_BYTES,
        POLL_PTY_MAX_CHUNKS * bitty_pty::READ_CHUNK_SIZE
    );
}

#[test]
fn burst_drains_bounded_per_poll_without_loss() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    // Emit ~300 KiB (more than one poll budget) then a DONE marker. The
    // `tr` turns NULs into printable bytes so the VT parser keeps them.
    rt.spawn_shell_with_args(
        "/bin/sh",
        &[
            "-c",
            "head -c 300000 /dev/zero | tr '\\0' 'x'; echo DONE-MARKER-0476",
        ],
    )
    .expect("spawn burst child");

    // Wait until at least one chunk is available (bounded wait, no busy loop).
    let first = rt.poll_pty_timeout(Duration::from_secs(10));
    assert!(first > 0, "burst child produced no output");
    assert!(
        first <= POLL_PTY_MAX_CHUNKS,
        "single poll drained {first} chunks, budget is {}",
        POLL_PTY_MAX_CHUNKS
    );

    // Keep draining with the bounded poll: every call respects the chunk
    // budget, total progress converges, and the DONE marker arrives without
    // loss. Under the old 1024-chunk collect the first poll would have
    // drained the whole 300 KiB burst in one render-thread stall.
    let deadline = Instant::now() + WAIT;
    let mut polls = 1usize;
    let mut total_chunks = first;
    let mut found = false;
    while Instant::now() < deadline {
        let n = rt.poll_pty();
        assert!(
            n <= POLL_PTY_MAX_CHUNKS,
            "poll drained {n} chunks, budget is {}",
            POLL_PTY_MAX_CHUNKS
        );
        total_chunks += n;
        polls += 1;
        rt.tick();
        let text: String = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
        if text.contains("DONE-MARKER-0476") {
            found = true;
            break;
        }
        if n == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Safety: a bounded poll needs at most a handful of rounds for
        // 300 KiB; hundreds of rounds means the pump lost data or stalled.
        assert!(polls < 500, "burst never converged after {polls} polls");
    }
    assert!(
        found,
        "DONE marker never arrived after {total_chunks} chunks in {polls} polls"
    );
    assert!(
        total_chunks >= 2,
        "300 KiB burst should take multiple bounded polls, got {total_chunks}"
    );
}
