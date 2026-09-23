//! M1-03 acceptance evidence: DECSET 2026 synchronized updates end-to-end.
//!
//! Issue #1128. The mechanism landed in `bitty` `ef73c82` (CTX-0380): the VT
//! parser maps `CSI ? 2026 h/l` to `Mode::SynchronizedUpdate`, the runtime
//! defers committing frames while any visible grid has the mode set, and
//! presentation resumes on mode clear or after the bounded deferral window
//! (`SYNC_UPDATE_DEFER_TIMEOUT`, 100 ms). The oracle is the accepted
//! `compatibility-milestone-rfc.md` M1 row "Synchronized updates | DECSET
//! 2026 | Required" (`docs/` submodule, `specifications/`) and
//! `terminal-state-rfc.md`.
//!
//! These tests lock the four claims the issue names — mode set/clear parse,
//! the deferred-present batching window, the DECRQM reply byte shape for
//! 2026, and hold/commit semantics — through a headless state+present
//! harness. They are *evidence*, not a re-implementation: the assertions
//! bind to the same `Runtime::handle_pty_bytes -> tick_at -> headless_rgba`
//! path `bitty --headless` uses.
#![forbid(unsafe_code)]

use bitty_runtime::{
    AnimationPolicy, PresentStats, Runtime, RuntimeConfig, SYNC_UPDATE_DEFER_TIMEOUT,
};
use std::time::{Duration, Instant};

/// Host-font-free runtime (CTX-0492 seam): the composited RGBA derives from
/// `RuntimeConfig` alone, so a pixel-level hold assertion is portable across
/// CI hosts and the golden digest below is stable.
fn harness() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("deterministic headless runtime must build")
}

fn replies_text(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

/// FNV-1a 64-bit over the presented frame; fixed constants so the digest is
/// stable across toolchains (mirrors `present_golden.rs`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn digest(rt: &Runtime, stats: &PresentStats) -> u64 {
    let mut bytes = Vec::with_capacity(128);
    for value in [
        stats.frame,
        stats.fills as u64,
        stats.rounded_fills as u64,
        stats.glyphs as u64,
        stats.cells_examined,
        stats.glyphs_emitted,
        stats.generation,
        stats.images as u64,
        stats.backgrounds as u64,
        stats.images_skipped as u64,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.push(u8::from(stats.headless));
    match rt.headless_rgba() {
        Some(rgba) => {
            bytes.push(1);
            bytes.extend_from_slice(&rgba);
        }
        None => bytes.push(0),
    }
    fnv1a(&bytes)
}

/// Mode set/clear parse: raw PTY bytes toggle the live mode register, and a
/// query split across two reads is still recognized once.
#[test]
fn decset_2026_set_clear_parse_toggles_live_mode() {
    let mut rt = harness();
    assert!(!rt.synchronized_update_active(), "off at power-on");
    rt.handle_pty_bytes(b"\x1b[?2026h");
    assert!(rt.synchronized_update_active(), "DECSET 2026 sets the mode");
    rt.handle_pty_bytes(b"\x1b[?2026l");
    assert!(!rt.synchronized_update_active(), "DECRST 2026 clears it");
    // Split across PTY reads: the parser must not toggle on the partial form.
    rt.handle_pty_bytes(b"\x1b[?20");
    assert!(!rt.synchronized_update_active(), "partial query is inert");
    rt.handle_pty_bytes(b"26h");
    assert!(
        rt.synchronized_update_active(),
        "mode set completes across reads"
    );
}

/// DECRQM reply byte shape for 2026: exact `CSI ? 2026 ; Ps $ y` bytes, with
/// Ps reflecting the live register (2 reset, 1 set).
#[test]
fn decrqm_2026_reply_byte_shape_tracks_live_mode() {
    let mut rt = harness();
    rt.handle_pty_bytes(b"\x1b[?2026$p");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b[?2026;2$y".to_vec()],
        "recognized and reset by default"
    );
    rt.handle_pty_bytes(b"\x1b[?2026h\x1b[?2026$p");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b[?2026;1$y".to_vec()],
        "set reports 1"
    );
    rt.handle_pty_bytes(b"\x1b[?2026l\x1b[?2026$p");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b[?2026;2$y".to_vec()],
        "cleared reports 2 again"
    );
    // A DECRQM issued while the mode is held is still answered promptly: the
    // deferral applies to presentation, never to the reply queue.
    let t0 = Instant::now();
    rt.handle_pty_bytes(b"\x1b[?2026h\x1b[?2026$p");
    assert!(rt.tick_at(t0).is_none(), "presentation deferred");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b[?2026;1$y".to_vec()],
        "DECRQM answered during the hold window"
    );
    rt.handle_pty_bytes(b"\x1b[?2026l");
}

/// Hold semantics: inside the batching window no frame is committed, the
/// presented RGBA is byte-identical to the pre-hold baseline, yet the live
/// terminal state already carries the batched output (presentation is stale,
/// Terminal Truth is current).
#[test]
fn synchronized_update_holds_presentation_while_state_advances() {
    let mut rt = harness();
    assert!(rt.tick().is_some(), "baseline full redraw");
    let baseline = rt.headless_rgba().expect("baseline rgba");
    let t0 = Instant::now();
    rt.handle_pty_bytes(b"\x1b[?2026hAAAA");
    rt.handle_pty_bytes(b"BBBB");
    rt.handle_pty_bytes(b"CCCC");
    assert!(rt.synchronized_update_active());
    assert!(
        rt.tick_at(t0).is_none(),
        "no frame committed immediately after DECSET"
    );
    assert!(
        rt.tick_at(t0 + SYNC_UPDATE_DEFER_TIMEOUT / 4).is_none(),
        "still held a quarter into the window"
    );
    assert!(
        rt.tick_at(t0 + SYNC_UPDATE_DEFER_TIMEOUT / 2).is_none(),
        "still held halfway through the window"
    );
    let held = rt.headless_rgba().expect("held rgba");
    assert_eq!(
        held, baseline,
        "the held frame equals the baseline bytes: nothing was presented"
    );
    let text: String = rt.snapshot().cells.iter().map(|cell| cell.glyph).collect();
    assert!(
        text.contains("AAABBBBCCCC"),
        "live state already carries every batched write, got {text:?}"
    );
}

/// Commit semantics: the mode clear commits exactly one batched frame — the
/// frame number advances once, the composited pixels now reflect all writes
/// made during the hold, and the path returns to idle afterwards.
#[test]
fn synchronized_update_clear_commits_one_batched_frame_then_idles() {
    let mut rt = harness();
    let baseline = rt.tick().expect("baseline full redraw");
    let baseline_rgba = rt.headless_rgba().expect("baseline rgba");
    let t0 = Instant::now();
    rt.handle_pty_bytes(b"\x1b[?2026hsync-A");
    assert!(rt.tick_at(t0).is_none(), "held");
    rt.handle_pty_bytes(b"-B\x1b[?2026l");
    assert!(!rt.synchronized_update_active());
    let commit = rt
        .tick_at(t0 + Duration::from_millis(1))
        .expect("mode clear commits the batched frame");
    assert_eq!(
        commit.frame,
        baseline.frame + 1,
        "exactly one frame is committed by the clear"
    );
    assert!(commit.glyphs > 0, "the batched text is presented");
    let committed = rt.headless_rgba().expect("committed rgba");
    assert_ne!(
        committed, baseline_rgba,
        "the committed frame reflects the writes made during the hold"
    );
    assert!(
        rt.tick_at(t0 + Duration::from_millis(2)).is_none(),
        "after the commit the path returns to idle"
    );
}

/// The batching window is bounded at exactly 100 ms: a hung process that never
/// sends the reset still gets its latest state committed at the bound, and a
/// still-active mode opens a fresh window instead of presenting every tick.
#[test]
fn synchronized_update_window_is_bounded_at_100ms() {
    assert_eq!(
        SYNC_UPDATE_DEFER_TIMEOUT,
        Duration::from_millis(100),
        "the contour/iTerm2-proposal consensus bound"
    );
    let mut rt = harness();
    assert!(rt.tick().is_some(), "baseline full redraw");
    let t0 = Instant::now();
    rt.handle_pty_bytes(b"\x1b[?2026hhung");
    assert!(rt.tick_at(t0).is_none(), "deferred inside the bound");
    assert!(
        rt.tick_at(t0 + SYNC_UPDATE_DEFER_TIMEOUT - Duration::from_millis(1))
            .is_none(),
        "still deferred one millisecond before the bound"
    );
    let committed = rt
        .tick_at(t0 + SYNC_UPDATE_DEFER_TIMEOUT)
        .expect("the bound itself commits, a hung mode never stalls presentation");
    assert!(committed.glyphs > 0);
    assert!(
        rt.synchronized_update_active(),
        "the mode is left exactly as the application set it (no silent reset)"
    );
    // Fresh damage inside the next window defers again; the bound is not a
    // one-shot that then presents on every tick.
    rt.handle_pty_bytes(b"more");
    assert!(
        rt.tick_at(t0 + SYNC_UPDATE_DEFER_TIMEOUT + Duration::from_millis(1))
            .is_none(),
        "a still-active mode opens a fresh bounded window"
    );
}

/// Pixel-level golden: the exact committed frame of a synchronized-update
/// batch is pinned by digest, so a future change to the hold/commit path that
/// alters the presented pixels or counters is caught here rather than only in
/// downstream visual review.
#[test]
fn golden_synchronized_update_commit_frame() {
    let mut rt = harness();
    let baseline = rt.tick().expect("baseline full redraw");
    assert_eq!(baseline.frame, 1);
    let t0 = Instant::now();
    rt.handle_pty_bytes(b"\x1b[?2026hsync-A");
    assert!(rt.tick_at(t0).is_none());
    rt.handle_pty_bytes(b"-B\x1b[?2026l");
    let commit = rt.tick_at(t0 + Duration::from_millis(1)).expect("commit");
    let actual = digest(&rt, &commit);
    assert_eq!(
        actual, 0x636f_64b4_dce4_c871,
        "synchronized-update commit digest changed (actual 0x{actual:016x})"
    );
}
