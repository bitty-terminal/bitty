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
    SYNC_UPDATE_MAX_DEFER_TIMEOUT,
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
/// downstream visual review. Since #1349 the digest also covers the
/// in-grid status bar row when it paints; CTX-0838 (#1441) hides the
/// lone-workspace bar, so this single-workspace commit carries no bar.
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

/// Issue #3122: while a TUI is actively streaming a multi-chunk `writePrepend`
/// batch inside `DECSET 2026` (`note_present_input_at` called on each PTY read),
/// the 100 ms idle timeout is extended up to `SYNC_UPDATE_MAX_DEFER_TIMEOUT`
/// (1,000 ms) so intermediate top-to-bottom scroll steps never tear onto the
/// screen mid-batch.
#[test]
fn active_streaming_synchronized_update_extends_past_100ms_until_max_cap() {
    assert_eq!(SYNC_UPDATE_MAX_DEFER_TIMEOUT, Duration::from_millis(1000));
    let mut rt = harness();
    assert!(rt.tick().is_some(), "baseline full redraw");
    let t0 = Instant::now();

    rt.handle_pty_bytes(b"\x1b[?2026hchunk-0");
    rt.note_present_input_at(t0);
    assert!(rt.tick_at(t0).is_none());

    // Stream chunks every 60 ms up to 300 ms (well past the 100 ms idle bound).
    for step in 1..=5 {
        let now = t0 + Duration::from_millis(step * 60);
        rt.handle_pty_bytes(format!(" chunk-{step}").as_bytes());
        rt.note_present_input_at(now);
        assert!(
            rt.tick_at(now).is_none(),
            "active streaming at {} ms must stay deferred",
            step * 60
        );
    }

    // Mode exit at 320 ms commits the entire batch in a single frame.
    let t_exit = t0 + Duration::from_millis(320);
    rt.handle_pty_bytes(b" done\x1b[?2026l");
    rt.note_present_input_at(t_exit);
    let stats = rt
        .tick_at(t_exit)
        .expect("mode exit after 320 ms streaming burst commits one frame");
    assert!(stats.glyphs > 0);

    // A runaway continuous producer inside `?2026h` still commits at the 1000 ms hard cap.
    let t_runaway = t0 + Duration::from_millis(400);
    rt.handle_pty_bytes(b"\x1b[?2026hrunaway");
    rt.note_present_input_at(t_runaway);
    assert!(rt.tick_at(t_runaway).is_none());
    for step in 1..=15 {
        let now = t_runaway + Duration::from_millis(step * 60);
        rt.handle_pty_bytes(b".");
        rt.note_present_input_at(now);
        if now.saturating_duration_since(t_runaway) < SYNC_UPDATE_MAX_DEFER_TIMEOUT {
            assert!(
                rt.tick_at(now).is_none(),
                "still inside 1000 ms cap at +{} ms",
                step * 60
            );
        }
    }
    let committed = rt
        .tick_at(t_runaway + SYNC_UPDATE_MAX_DEFER_TIMEOUT)
        .expect("1000 ms hard cap commits even under continuous input");
    assert!(committed.glyphs > 0);
}

/// Issue #3122 regression: a multi-chunk `writePrepend`-shaped burst — cursor
/// moved up, history written row by row, all inside one `?2026h`/`?2026l`
/// pair and fed the way a TUI feeds it (one PTY read at a time, each noted
/// through `note_present_input`) — commits no intermediate top-to-bottom
/// frame and exactly one frame when the batch closes.
#[test]
fn multi_chunk_scrollback_prepend_coalesces_into_one_frame() {
    let mut rt = harness();
    let baseline = rt.tick().expect("baseline full redraw");
    assert_eq!(rt.scrollback_len(), 0, "empty scrollback at baseline");

    // `uv.TerminalRenderer::writePrepend`: home, walk up, then each history
    // line ends with `EL` + CRLF before the active viewport is redrawn.
    let mut body = Vec::from(&b"\x1b[?2026h\r"[..]);
    for line in 0..60 {
        body.extend_from_slice(format!("prepend-{line:02}\x1b[K\r\n").as_bytes());
    }

    let t0 = Instant::now();
    let mut now = t0;
    let mut commits = 0usize;
    for (index, chunk) in body.chunks(48).enumerate() {
        now = t0 + Duration::from_millis(8 * (index as u64 + 1));
        rt.handle_pty_bytes(chunk);
        rt.note_present_input_at(now);
        if rt.tick_at(now).is_some() {
            commits += 1;
        }
    }
    assert_eq!(
        commits, 0,
        "an open synchronized batch must not commit intermediate frames"
    );
    assert!(
        rt.synchronized_update_active(),
        "the batch is still open after {} chunks",
        body.len() / 48 + 1
    );

    let t_exit = now + Duration::from_millis(8);
    rt.handle_pty_bytes(b"\x1b[?2026l");
    let commit = rt
        .tick_at(t_exit)
        .expect("closing the batch commits the whole prepend");
    assert_eq!(
        commit.frame,
        baseline.frame + 1,
        "exactly one frame is committed for the entire multi-chunk batch"
    );
    assert!(
        rt.scrollback_len() > 0,
        "the prepended history is retained as scrollback"
    );
    let text: String = rt.snapshot().cells.iter().map(|cell| cell.glyph).collect();
    assert!(
        text.contains("prepend-59"),
        "the last prepended line is on screen, got {text:?}"
    );
    assert!(
        rt.tick_at(t_exit + Duration::from_millis(20)).is_none(),
        "after the commit the path returns to idle"
    );
}

/// Issue #3122: a synchronized update left open on a background workspace must
/// not defer frames in the active workspace.
#[test]
fn background_workspace_synchronized_update_does_not_defer_active_workspace() {
    let mut rt = harness();
    assert!(rt.tick().is_some(), "baseline full redraw");
    rt.handle_pty_bytes(b"\x1b[?2026hbackground");
    assert!(rt.synchronized_update_active());

    // Switch to a fresh workspace: primary view (ViewId(1)) stays in workspace 1.
    assert!(rt.workspace_new().is_ok());
    assert!(
        !rt.synchronized_update_active(),
        "background workspace DECSET 2026 must not hold the active workspace"
    );
}
