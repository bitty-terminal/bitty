//! CTX-0476 hostile probes: `poll_pty` byte/time budgets and waker-merge bounds.
//!
//! Entirely gated to `cfg(unix)`: needs a real PTY child emitting a burst.
//! No filesystem or network access beyond spawning the PTY child itself.

#![cfg(unix)]

use std::time::{Duration, Instant};

use bitty_runtime::{
    POLL_PTY_MAX_BYTES, POLL_PTY_MAX_CHUNKS, POLL_PTY_SYNC_MAX_CHUNKS, POLL_PTY_SYNC_TIME_BUDGET,
    POLL_PTY_TIME_BUDGET, Runtime, RuntimeConfig,
};

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn poll_budgets_match_accepted_pipeline_bounds() {
    // 64 chunks x 8 KiB = 512 KiB matches the worst-case total buffered
    // across both pump stages (2 x 256 KiB, issue #3122 doubled the channel).
    // The time budget keeps one poll inside a frame (10 ms order, mirrors
    // spawn-poll cadence).
    assert_eq!(POLL_PTY_MAX_CHUNKS, 64);
    assert_eq!(POLL_PTY_MAX_BYTES, 64 * 8 * 1024);
    assert_eq!(POLL_PTY_TIME_BUDGET, Duration::from_millis(10));
    assert_eq!(
        POLL_PTY_MAX_BYTES,
        POLL_PTY_MAX_CHUNKS * bitty_pty::READ_CHUNK_SIZE
    );
}

#[test]
fn synchronized_poll_budgets_exceed_the_ordinary_ones() {
    // Issue #3122: a `DECSET 2026` batch (a TUI `writePrepend` during
    // expand/collapse or a panel switch) is atomic, so it gets a larger
    // burst allowance — but it stays a hard bound.
    assert_eq!(POLL_PTY_SYNC_MAX_CHUNKS, 256);
    assert_eq!(POLL_PTY_SYNC_TIME_BUDGET, Duration::from_millis(20));
    let sync_chunks: usize = POLL_PTY_SYNC_MAX_CHUNKS;
    let ordinary_chunks: usize = POLL_PTY_MAX_CHUNKS;
    let sync_budget: Duration = POLL_PTY_SYNC_TIME_BUDGET;
    let ordinary_budget: Duration = POLL_PTY_TIME_BUDGET;
    assert!(
        sync_chunks > ordinary_chunks,
        "sync burst {sync_chunks} must exceed the ordinary {ordinary_chunks} chunks"
    );
    assert!(
        sync_budget > ordinary_budget,
        "sync budget {sync_budget:?} must exceed the ordinary {ordinary_budget:?}"
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
    //
    // No `tick` in the loop: state updates happen in `handle_pty_bytes`, and
    // a software present here would dominate the loop on headless CI without
    // exercising anything under test.
    //
    // #1524: `WAIT` bounds a stall, not the whole drain. The deadline moves
    // forward whenever a poll makes progress, so a starved runner that
    // drains slowly but steadily passes, while a pump that stops delivering
    // still fails after `WAIT` without progress.
    let mut deadline = Instant::now() + WAIT;
    let mut polls = 1usize;
    let mut productive_polls = 1usize;
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
        let text: String = rt.snapshot().cells.iter().map(|c| c.glyph).collect();
        if text.contains("DONE-MARKER-0476") {
            found = true;
            break;
        }
        if n == 0 {
            std::thread::sleep(Duration::from_millis(20));
        } else {
            productive_polls += 1;
            deadline = Instant::now() + WAIT;
        }
        // Safety: a bounded poll needs at most a handful of productive
        // rounds for 300 KiB; hundreds of them means the pump lost data or
        // spins. Idle polls while the child is still starting are not
        // counted.
        assert!(
            productive_polls < 500,
            "burst never converged after {productive_polls} productive polls"
        );
    }
    assert!(
        found,
        "DONE marker never arrived after {total_chunks} chunks in {polls} polls"
    );
    // Loss floor: every chunk is at most `READ_CHUNK_SIZE`, so delivering the
    // whole 300 KiB burst requires at least this many chunks. Fewer means the
    // pump dropped data even though the tail marker arrived.
    let min_chunks = 300_000usize.div_ceil(bitty_pty::READ_CHUNK_SIZE);
    assert!(
        total_chunks >= min_chunks,
        "burst under-delivered: {total_chunks} chunks < {min_chunks} for 300 KiB"
    );
}
