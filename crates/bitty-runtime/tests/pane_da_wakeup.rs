//! Split-pane DA wakeup + per-pane reply routing (CTX-0230, high).
//!
//! Live symptom: fresh split-pane fish shells stalled ~10 s on
//! Primary-Device-Attributes (`ESC[c`) while the primary pane answered,
//! plus transient render garble during concurrent split+output.
//!
//! Root causes (verified by reading, fixed here):
//!
//! - R1: pane PTY output never woke the event loop. `set_pty_waker`
//!   promoted only the primary reader into a wakeup-forwarder thread;
//!   pane readers stayed direct, drained only inside `poll_pty` from event
//!   handlers. With `ControlFlow::Wait` the loop slept through pane-only
//!   output, so a split shell's `ESC[c` sat unanswered until an incidental
//!   wakeup. Pane readers now promote exactly like the primary one.
//! - R2: `poll_pty_timeout` never pumped pane sessions on its idle paths
//!   (no-primary-reader early return; primary-timeout `None => 0`), so
//!   timeout-driven consumers starved split-shell queries even with bytes
//!   ready. Both paths now drain panes.
//!
//! Unix-only: spawning needs a POSIX shell plus PTY master semantics
//! (mirrors `real_pty.rs` / `pane_sessions.rs`).

#![cfg(unix)]

use std::time::Duration;

use bitty_runtime::{LayoutNode, PtyWaker, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

const TIMEOUT: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_millis(20);

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

fn pane_text(rt: &Runtime, view: &ViewId) -> String {
    match rt.pane_snapshot(view) {
        Some(snap) => snap.cells.iter().map(|c| c.glyph).collect(),
        None => String::new(),
    }
}

fn primary_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|c| c.glyph).collect()
}

fn noop_waker() -> PtyWaker {
    std::sync::Arc::new(|| {})
}

/// R2: pane output must arrive through the timeout pump path while the
/// primary shell stays quiet. Before the fix, `poll_pty_timeout` returned
/// `0` without touching pane sessions whenever the primary had no data, so
/// this marker never arrived.
#[test]
fn pane_output_arrives_via_timeout_path_while_primary_quiet() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn quiet primary shell");
    rt.spawn_shell_for_view(
        ViewId::new(2),
        "/bin/sh",
        &["-c", "echo PANE-TIMEOUT-MARKER-0230; sleep 30"],
        40,
        12,
    )
    .expect("spawn pane shell");

    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut arrived = false;
    while std::time::Instant::now() < deadline {
        // NOTE: deliberately never `poll_pty` here: the timeout path alone
        // must keep split panes live.
        let _ = rt.poll_pty_timeout(Duration::from_millis(100));
        rt.tick();
        if pane_text(&rt, &ViewId::new(2)).contains("PANE-TIMEOUT-MARKER-0230") {
            arrived = true;
            break;
        }
        std::thread::sleep(STEP);
    }
    assert!(
        arrived,
        "pane output never arrived via poll_pty_timeout while primary was quiet"
    );
}

/// R2 companion: panes pump even when the primary shell was never spawned
/// (the old code returned `0` immediately without a primary reader).
#[test]
fn pane_output_arrives_via_timeout_path_without_primary_shell() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell_for_view(
        ViewId::new(2),
        "/bin/sh",
        &["-c", "echo PANE-NOPRIMARY-MARKER-0230; sleep 30"],
        40,
        12,
    )
    .expect("spawn pane shell");
    assert!(!rt.has_pty_reader(), "test needs no primary reader");

    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut arrived = false;
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty_timeout(Duration::from_millis(100));
        rt.tick();
        if pane_text(&rt, &ViewId::new(2)).contains("PANE-NOPRIMARY-MARKER-0230") {
            arrived = true;
            break;
        }
        std::thread::sleep(STEP);
    }
    assert!(
        arrived,
        "pane output never arrived via poll_pty_timeout without a primary shell"
    );
}

/// Routing: a Primary-DA query (`ESC[c`) and a Secondary-DA query
/// (`ESC[>c`) injected into one pane's byte stream must be answered on that
/// pane's own PTY master — 5 bytes for `ESC[?6c`, a `CSI > ... c` reply for
/// secondary — with nothing leaking into the primary reply queue, and the
/// primary query path must keep working.
#[test]
fn device_attributes_round_trip_per_pane_without_cross_leak() {
    let mut rt = two_pane_runtime();
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn primary shell");
    let view = ViewId::new(2);
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn pane shell");

    // Primary DA into the pane: exactly `ESC[?6c` (5 bytes) to the pane.
    rt.handle_pane_bytes(view, b"\x1b[c");
    assert_eq!(
        rt.write_pane_replies(view),
        5,
        "primary-DA reply must flush 5 bytes (ESC[?6c) to the pane master"
    );
    assert!(
        rt.take_replies().is_empty(),
        "pane query must not leak into the primary reply queue"
    );

    // Secondary DA into the pane: a well-formed `CSI > 0 ; ver ; 1 c`.
    rt.handle_pane_bytes(view, b"\x1b[>c");
    let secondary = rt.write_pane_replies(view);
    assert!(
        secondary > 0,
        "secondary-DA reply must flush bytes to the pane master"
    );
    assert!(
        rt.take_replies().is_empty(),
        "pane query must not leak into the primary reply queue"
    );

    // A leaf without a session flushes nothing.
    assert_eq!(
        rt.write_pane_replies(ViewId::new(1)),
        0,
        "session-less leaf must flush zero reply bytes"
    );

    // Primary path still answers headlessly (queued for take_replies).
    rt.handle_pty_bytes(b"\x1b[c");
    let queued = rt.take_replies();
    assert_eq!(queued.len(), 1, "primary DA must queue exactly one reply");
    assert_eq!(&queued[0][..], b"\x1b[?6c");
}

/// R1 mechanism: installing a waker promotes pane readers into forwarders —
/// both for sessions spawned after the install and for pre-existing ones —
/// so pane-only output wakes a `Wait`-parked event loop.
#[test]
fn pane_forwarder_armed_when_waker_installed() {
    // Waker first, sessions after (live split ordering).
    let mut rt = two_pane_runtime();
    rt.set_pty_waker(noop_waker());
    rt.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn primary shell");
    assert!(rt.has_pty_forwarder(), "primary must promote");
    rt.spawn_shell_for_view(
        ViewId::new(2),
        "/bin/sh",
        &["-c", "echo PANE-WAKE-MARKER-0230; sleep 30"],
        40,
        12,
    )
    .expect("spawn pane shell");
    assert!(
        rt.has_pane_forwarder(&ViewId::new(2)),
        "pane spawned after set_pty_waker must promote"
    );
    assert!(
        !rt.has_pane_forwarder(&ViewId::new(1)),
        "session-less leaf must report no forwarder"
    );

    // Sessions first, waker after (late attach ordering).
    let mut late = two_pane_runtime();
    late.spawn_shell_with_args("/bin/sh", &["-c", "sleep 30"])
        .expect("spawn primary shell");
    late.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("spawn pane shell");
    assert!(
        !late.has_pane_forwarder(&ViewId::new(2)),
        "no waker yet: pane must stay direct"
    );
    late.set_pty_waker(noop_waker());
    assert!(
        late.has_pane_forwarder(&ViewId::new(2)),
        "set_pty_waker must promote pre-existing panes"
    );

    // Forwarded pane output still drains through the non-blocking pump.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut arrived = false;
    while std::time::Instant::now() < deadline {
        let _ = rt.poll_pty();
        rt.tick();
        if pane_text(&rt, &ViewId::new(2)).contains("PANE-WAKE-MARKER-0230") {
            arrived = true;
            break;
        }
        std::thread::sleep(STEP);
    }
    assert!(arrived, "forwarded pane output never arrived via poll_pty");
}

/// Anti-garble at the grid level: while the primary streams output, split
/// mid-stream and stream in the new pane concurrently. Neither grid may
/// show the other's rows (no mirroring / mixed rows), and leaf allocations
/// must stay disjoint.
#[test]
fn concurrent_split_output_keeps_grids_disjoint() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("headless build");
    rt.spawn_shell_with_args(
        "/bin/sh",
        &[
            "-c",
            "i=1; while [ $i -le 300 ]; do echo PRIMARY-STREAM-$i; i=$((i+1)); done; sleep 30",
        ],
    )
    .expect("spawn streaming primary shell");

    // Let the primary stream start, then split mid-stream.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut split_done = false;
    while std::time::Instant::now() < deadline && !split_done {
        let _ = rt.poll_pty();
        rt.tick();
        if primary_text(&rt).contains("PRIMARY-STREAM") {
            let layout = LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            );
            rt.set_layout(layout);
            rt.spawn_shell_for_view(
                ViewId::new(2),
                "/bin/sh",
                &[
                    "-c",
                    "i=1; while [ $i -le 300 ]; do echo PANE-STREAM-$i; i=$((i+1)); done; sleep 30",
                ],
                40,
                12,
            )
            .expect("spawn streaming pane shell");
            split_done = true;
        }
        std::thread::sleep(STEP);
    }
    assert!(split_done, "primary stream never started");

    // Drain both streams concurrently to completion.
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let _ = rt.poll_pty();
        rt.tick();
        let prim = primary_text(&rt);
        let pane = pane_text(&rt, &ViewId::new(2));
        if pane.contains("PANE-STREAM-300")
            && (prim.contains("PRIMARY-STREAM-300") || std::time::Instant::now() >= deadline)
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(STEP);
    }

    let prim = primary_text(&rt);
    let pane = pane_text(&rt, &ViewId::new(2));
    assert!(
        pane.contains("PANE-STREAM-"),
        "pane stream output never arrived"
    );
    assert!(
        !prim.contains("PANE-STREAM-"),
        "primary grid shows pane rows (mixed render)"
    );
    assert!(
        !pane.contains("PRIMARY-STREAM-"),
        "pane grid shows primary rows (mixed render)"
    );

    // Leaf allocations must be pairwise disjoint (no superimposition).
    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 2, "expected exactly two leaf allocations");
    for (i, (_, a)) in allocs.iter().enumerate() {
        for (_, b) in &allocs[i + 1..] {
            let (ax, ay, aw, ah) = (
                u32::from(a.x),
                u32::from(a.y),
                u32::from(a.width),
                u32::from(a.height),
            );
            let (bx, by, bw, bh) = (
                u32::from(b.x),
                u32::from(b.y),
                u32::from(b.width),
                u32::from(b.height),
            );
            let x_overlap = ax < bx + bw && bx < ax + aw;
            let y_overlap = ay < by + bh && by < ay + ah;
            assert!(
                !(x_overlap && y_overlap),
                "leaf allocations overlap: {a:?} vs {b:?}"
            );
        }
    }
}
