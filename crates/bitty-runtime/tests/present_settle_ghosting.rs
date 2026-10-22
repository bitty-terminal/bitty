//! bitty#2486 regression: rapid full-screen selector movement must never
//! present a half-applied repaint.
//!
//! A many-row CLI selector repaints the whole screen for every cursor move.
//! The tty hands that repaint to the runtime as several reads; presenting
//! between them commits an old selection row beside its replacement (the
//! reported flicker/ghosting). The PTY-quiescence settle gate holds the frame
//! until the burst is quiet, so every committed frame matches the terminal
//! truth of a complete logical repaint.
//!
//! The fixture is deterministic: fixed rows, fixed virtual clock, cursor
//! hidden, zero-duration animations, and a headless software present.

use std::time::{Duration, Instant};

use bitty_platform::PhysicalSize;
use bitty_runtime::{
    AnimationKind, LayoutNode, PRESENT_SETTLE_MAX, Runtime, RuntimeConfig, View, ViewId,
};

/// Settle window used by the test (the shipped GUI value is 2 ms).
const SETTLE: Duration = Duration::from_millis(2);
const ROWS: usize = 24;
const CHUNKS: usize = 3;

/// Deterministic 80x24 runtime with the cursor hidden (the selector draws
/// its own `→` marker; a blinking hardware cursor would add noise).
fn runtime() -> Runtime {
    let mut config = RuntimeConfig::default();
    config.animations.duration_ms = [0; AnimationKind::COUNT];
    let mut rt =
        Runtime::with_deterministic_rasterizer(config).expect("deterministic runtime must build");
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, ROWS)));
    rt.handle_resize(PhysicalSize::new(736, 472))
        .expect("resize applies");
    rt.handle_pty_bytes(b"\x1b[?25l");
    rt
}

/// One complete selector repaint: every row rewritten, the selected row in
/// reverse video behind a `→` marker. `CSI K` erases the tail of each row
/// (the shape a real selector uses when shortening a highlighted entry).
fn selector_rows(selected: usize) -> Vec<Vec<u8>> {
    (0..ROWS)
        .map(|row| {
            let mut out = format!("\x1b[{};1H\x1b[K", row + 1).into_bytes();
            if row == selected {
                out.extend_from_slice(b"\x1b[7m");
            }
            out.extend_from_slice(
                format!(
                    "{} provider-item-{row:02}",
                    if row == selected { "→" } else { " " }
                )
                .as_bytes(),
            );
            if row == selected {
                out.extend_from_slice(b"\x1b[0m");
            }
            out
        })
        .collect()
}

/// The same repaint split into `CHUNKS` tty-sized reads at row boundaries
/// (a real read can split escapes too; this fixture keeps the logical frame
/// intact so a failure names presentation, not parser reassembly).
fn selector_chunks(selected: usize) -> Vec<Vec<u8>> {
    let rows = selector_rows(selected);
    let per_chunk = rows.len().div_ceil(CHUNKS);
    rows.chunks(per_chunk).map(|group| group.concat()).collect()
}

fn full_frame(selected: usize) -> Vec<u8> {
    selector_rows(selected).concat()
}

/// Frames are compared byte-for-byte; report the first divergent pixel.
fn assert_frames_equal(rt: &Runtime, truth: &Runtime, context: &str) {
    let gated = rt.headless_rgba().expect("gated frame");
    let expected = truth.headless_rgba().expect("truth frame");
    if gated == expected {
        return;
    }
    let width = rt.surface_extent().expect("surface extent").width() as usize;
    let first = gated
        .chunks_exact(4)
        .zip(expected.chunks_exact(4))
        .position(|(a, b)| a != b)
        .unwrap_or(usize::MAX);
    panic!(
        "{context}: first diff pixel {first} ({},{}); gated={:?} truth={:?}",
        first % width,
        first / width,
        gated.get(first * 4..first * 4 + 4),
        expected.get(first * 4..first * 4 + 4),
    );
}

#[test]
fn rapid_selector_frames_match_terminal_truth() {
    let t0 = Instant::now();
    let mut gated = runtime();
    gated.set_present_settle(SETTLE);
    assert_eq!(gated.present_settle(), SETTLE);
    let mut truth = runtime();

    // Frame 0: both runtimes commit the same complete screen.
    let first = full_frame(0);
    gated.handle_pty_bytes(&first);
    gated.note_present_input_at(t0);
    assert!(
        gated.tick_at(t0 + Duration::from_micros(500)).is_none(),
        "even the first burst waits for quiescence"
    );
    assert!(
        gated.tick_at(t0 + SETTLE).is_some(),
        "quiet deadline presents frame 0"
    );
    truth.handle_pty_bytes(&first);
    assert!(truth.tick_at(t0).is_some());
    assert_frames_equal(&gated, &truth, "frame 0");

    let mut presented = 1u32;
    for step in 1..=12usize {
        let base = t0 + Duration::from_millis(10 * step as u64);
        let chunks = selector_chunks(step % ROWS);
        let mut last_at = base;
        for (index, chunk) in chunks.iter().enumerate() {
            let at = base + Duration::from_micros(250 * index as u64);
            gated.handle_pty_bytes(chunk);
            gated.note_present_input_at(at);
            last_at = at;
            assert!(
                gated.tick_at(at + Duration::from_micros(100)).is_none(),
                "step {step} chunk {index}: a mid-burst tick must defer"
            );
            assert_eq!(
                gated.present_coalescing_deadline(),
                Some(at + SETTLE),
                "step {step} chunk {index}: the app wake must be armed at the settle deadline"
            );
        }

        // Terminal truth: the same logical frame applied atomically.
        truth.handle_pty_bytes(&full_frame(step % ROWS));
        assert!(truth.tick_at(last_at).is_some(), "truth presents the frame");

        let stats = gated
            .tick_at(last_at + SETTLE)
            .expect("quiet deadline presents the complete repaint");
        assert_eq!(
            stats.generation,
            gated.state().generation(),
            "the committed frame must carry the whole burst generation"
        );
        presented += 1;
        assert_frames_equal(&gated, &truth, &format!("step {step}"));
        assert_eq!(
            gated.present_coalescing_deadline(),
            None,
            "a committed frame clears the wake deadline"
        );
    }
    assert_eq!(presented, 13, "one committed frame per logical repaint");
}

#[test]
fn ungated_runtime_presents_a_half_applied_repaint() {
    // Pre-fix control (the regression's failing half): without the settle
    // gate, the tick after the first read commits a partial screen that
    // differs from the complete logical frame. This is the exact artifact
    // #2486 reports; the gate above is what removes it.
    let t0 = Instant::now();
    let mut ungated = runtime();
    let mut truth = runtime();

    let first = full_frame(0);
    ungated.handle_pty_bytes(&first);
    assert!(ungated.tick_at(t0).is_some());
    truth.handle_pty_bytes(&first);
    assert!(truth.tick_at(t0).is_some());

    // Move the highlight from row 0 to row 20: the first read clears the
    // old row while the new row is still two reads away, so the partial
    // frame is visibly wrong (no highlight at all).
    let chunks = selector_chunks(20);
    ungated.handle_pty_bytes(&chunks[0]);
    assert!(
        ungated.tick_at(t0 + Duration::from_millis(1)).is_some(),
        "the ungated path presents at the first read"
    );
    truth.handle_pty_bytes(&full_frame(20));
    assert!(truth.tick_at(t0 + Duration::from_millis(1)).is_some());
    let partial = ungated.headless_rgba().expect("partial frame");
    let complete = truth.headless_rgba().expect("complete frame");
    assert!(
        partial != complete,
        "the pre-fix frame must show the half-applied repaint"
    );

    // Reassembling the rest heals the grid, proving the artifact is
    // presentation timing, not lost invalidation: the terminal truth is
    // untouched by the mid-burst present.
    ungated.handle_pty_bytes(&chunks[1]);
    ungated.handle_pty_bytes(&chunks[2]);
    assert!(ungated.tick_at(t0 + Duration::from_millis(2)).is_some());
    assert_eq!(
        ungated.snapshot().cells,
        truth.snapshot().cells,
        "the healed grid must equal terminal truth"
    );
    let healed = ungated.headless_rgba().expect("healed frame");
    assert!(
        healed != partial,
        "the healed frame must repaint the mid-burst rows"
    );
}

#[test]
fn continuous_output_still_presents_within_the_cap() {
    // A producer that never goes quiet (a log flood, `yes`) must not stall
    // presentation: past PRESENT_SETTLE_MAX since the burst began the latest
    // state commits even though bytes keep arriving.
    let t0 = Instant::now();
    let mut rt = runtime();
    rt.set_present_settle(SETTLE);
    rt.handle_pty_bytes(&full_frame(1));
    rt.note_present_input_at(t0);
    assert!(rt.tick_at(t0 + SETTLE).is_some(), "settle the first frame");

    let burst_start = t0 + Duration::from_millis(20);
    for step in 0..7u64 {
        let at = burst_start + Duration::from_millis(step);
        rt.handle_pty_bytes(&full_frame((step as usize + 2) % ROWS));
        rt.note_present_input_at(at);
        assert!(
            rt.tick_at(at + Duration::from_micros(500)).is_none(),
            "step {step}: inside the settle window the frame defers"
        );
    }
    // The cap, not a quiet gap, opens this present: the last byte arrived
    // 0.5 ms ago but the burst has been open for the full bound.
    let stats = rt
        .tick_at(burst_start + PRESENT_SETTLE_MAX)
        .expect("the burst cap must release the present");
    assert_eq!(stats.generation, rt.state().generation());
    assert_eq!(rt.present_coalescing_deadline(), None);
}

#[test]
fn zero_settle_preserves_every_dirty_tick() {
    // The default is the pre-#2486 behavior: embedders that do not opt in
    // (headless tests, deterministic runs) present every dirty tick.
    let t0 = Instant::now();
    let mut rt = runtime();
    assert_eq!(rt.present_settle(), Duration::ZERO);
    rt.handle_pty_bytes(&full_frame(0));
    assert!(
        rt.tick_at(t0).is_some(),
        "the zero window must present immediately"
    );
    assert_eq!(rt.presents_coalesced(), 0);
    assert_eq!(rt.present_coalescing_deadline(), None);
}

#[test]
fn cadence_and_settle_compose_to_the_later_deadline() {
    // The settle window closes first but the cadence gate still holds the
    // frame; the armed wake must be the later of the two, not the settle
    // deadline (which would present early and defeat the cadence).
    let t0 = Instant::now();
    let interval = Duration::from_millis(16);
    let mut rt = runtime();
    rt.set_present_settle(SETTLE);
    rt.set_min_present_interval(interval);

    rt.handle_pty_bytes(&full_frame(0));
    rt.note_present_input_at(t0);
    assert!(
        rt.tick_at(t0 + SETTLE).is_some(),
        "the first frame opens once the settle window closes"
    );

    rt.handle_pty_bytes(&full_frame(1));
    rt.note_present_input_at(t0 + Duration::from_millis(3));
    assert!(
        rt.tick_at(t0 + Duration::from_millis(5)).is_none(),
        "the cadence gate still holds the frame after the settle window"
    );
    assert_eq!(
        rt.present_coalescing_deadline(),
        Some(t0 + Duration::from_millis(18)),
        "the wake must be the later cadence deadline (present at 2 ms + 16 ms)"
    );
    assert!(
        rt.tick_at(t0 + Duration::from_millis(18)).is_some(),
        "the later deadline presents the held frame"
    );
    assert_eq!(rt.present_coalescing_deadline(), None);
}

#[test]
fn an_idle_query_burst_does_not_poison_the_next_repaint() {
    // Review of bitty#2486: PTY output can be consumed without producing
    // damage (a kitty status query only queues a reply). The idle tick must
    // close that burst; otherwise its timestamps keep the settle cap in the
    // past and the next real repaint presents on its first read, exactly the
    // half-applied frame the gate exists to prevent.
    let t0 = Instant::now();
    let mut rt = runtime();
    rt.set_present_settle(SETTLE);
    rt.handle_pty_bytes(&full_frame(0));
    rt.note_present_input_at(t0);
    assert!(rt.tick_at(t0 + SETTLE).is_some(), "settle the first frame");

    let generation = rt.state().generation();
    // `a=q` is answered from the store and never mutates the grid or the
    // generation (no storage, no placement, no damage).
    rt.handle_pty_bytes(b"\x1b_Gi=31,a=q\x1b\\");
    assert_eq!(
        rt.state().generation(),
        generation,
        "a status query must not advance the grid generation"
    );
    rt.note_present_input_at(t0 + Duration::from_millis(10));
    assert!(
        rt.tick_at(t0 + Duration::from_millis(11)).is_none(),
        "the query-only burst is an idle tick"
    );

    // Long after the previous burst's 8 ms cap, a new split repaint must
    // still wait out its own settle window before presenting.
    let next = t0 + Duration::from_millis(50);
    let chunks = selector_chunks(2);
    rt.handle_pty_bytes(&chunks[0]);
    rt.note_present_input_at(next);
    assert!(
        rt.tick_at(next + Duration::from_micros(500)).is_none(),
        "a fresh burst must defer even though the previous idle burst aged out"
    );
    rt.handle_pty_bytes(&chunks[1]);
    rt.handle_pty_bytes(&chunks[2]);
    assert!(
        rt.tick_at(next + SETTLE).is_some(),
        "the quiet deadline presents the complete repaint"
    );
}
