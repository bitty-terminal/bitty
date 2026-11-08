//! bitty#3032: a pane whose grid holds text must never present an empty frame.
//!
//! Live report (0.0.36): opening `agy`'s subagent view left the pane blank -
//! no glyphs and no cell background - while `bitty ctl terminal text` still
//! returned the 25 conversation lines from the grid. These tests drive the
//! real present path (state -> shaped runs -> draw list -> headless RGBA)
//! through primary and pane alternate-screen switches, including a switch
//! that lands while the surface is hidden, one that follows an unconfirmed
//! present, a coalesced-present deferral across an alt-screen latch transition,
//! and retained-leaf reuse across an `Atlas::try_grow` epoch bump.
//!
//! Which of these actually pin the fix is recorded per test. The pre-fix tree
//! (main, 69aa4a7c) still passes every frame-*content* case below, because the
//! reported blank frame needs the retained-leaf store to hand back an empty
//! list, and no public input can make a leaf render empty on demand in this
//! headless seam. The behaviours that *do* discriminate are pinned by
//! `runtime::present::empty_leaf_guard_tests` and
//! `runtime::present::coalesced_deferral_tests` (in-crate: they need private
//! state or the empty-render seam) and by the app-level
//! `input_proof_during_a_speculative_reopen_queues_a_from_truth_frame`.

#![forbid(unsafe_code)]

use bitty_runtime::{PresentStats, Runtime, RuntimeConfig};
#[cfg(unix)]
use bitty_runtime::{View, ViewId};
use std::time::{Duration, Instant};

/// Theme background the headless compositor clears to (`bitty dark`).
const THEME_BG: [u8; 4] = [30, 30, 46, 255];

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("deterministic runtime must build")
}

/// Non-background pixels anywhere in the last presented frame.
fn ink_pixels(rt: &Runtime) -> usize {
    let Some(rgba) = rt.headless_rgba() else {
        return 0;
    };
    rgba.chunks_exact(4).filter(|px| *px != THEME_BG).count()
}

fn assert_frame_has_ink(name: &str, rt: &Runtime, stats: &PresentStats) {
    assert!(
        stats.fills > 0 || stats.glyphs > 0,
        "{name}: presented draw list is empty (fills={}, glyphs={})",
        stats.fills,
        stats.glyphs
    );
    assert!(
        ink_pixels(rt) > 0,
        "{name}: presented frame has no non-background pixels"
    );
}

/// A frame that reused a retained leaf list.
///
/// `PresentStats::glyphs` counts primitives carried over from the retained
/// list, while `glyphs_emitted` counts what the renderer actually produced this
/// frame (a reused leaf emits none). A frame presenting mostly un-emitted
/// glyphs therefore reused retained primitives.
fn reused_retained_leaf(stats: &PresentStats) -> bool {
    stats.glyphs > 0 && stats.glyphs_emitted.saturating_mul(2) <= stats.glyphs as u64
}

/// Primary screen with content, then an alternate-screen switch that carries
/// its own content in the same PTY batch.
///
/// Pre-fix (69aa4a7c): passes. The first frame and the alt switch both force a
/// full render, so no retained list is reused; this pins the alt-grid content
/// contract, not the blank-frame fix.
#[test]
fn primary_alt_screen_switch_with_content_paints_the_alt_grid() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"primary screen content\r\nsecond row\r\n");
    rt.tick().expect("primary frame presents");

    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Halt screen content\r\nALT-ROW-TWO");
    let stats = rt.tick().expect("alt frame presents");
    assert_frame_has_ink("primary alt switch", &rt, &stats);

    let text = rt
        .snapshot()
        .cells
        .iter()
        .map(|cell| cell.glyph)
        .collect::<String>();
    assert!(
        text.contains("ALT-ROW-TWO"),
        "Terminal Truth must hold the alt content, got {text:?}"
    );
}

/// Same switch while the surface is hidden; the reveal frame must paint the
/// alt grid from truth.
///
/// Pre-fix (69aa4a7c): passes. A reveal queues a full frame on its own; kept as
/// occlusion coverage.
#[test]
fn alt_screen_switch_while_hidden_paints_on_reveal() {
    let t0 = Instant::now();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"primary screen content\r\n");
    rt.tick_at(t0).expect("primary frame presents");

    rt.set_surface_visibility_at(t0 + Duration::from_millis(1), false);
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Hhidden alt content\r\nHIDDEN-ALT-ROW");
    // Hidden ticks retain damage and present nothing.
    assert!(rt.tick_at(t0 + Duration::from_millis(2)).is_none());

    rt.set_surface_visibility_at(t0 + Duration::from_millis(3), true);
    let stats = rt
        .tick_at(t0 + Duration::from_millis(4))
        .expect("reveal frame presents");
    assert_frame_has_ink("hidden alt switch", &rt, &stats);
}

/// Same switch after a committed present whose compositor confirmation never
/// arrived (bitty#2989 shape): the corrective repaint must paint the alt grid.
///
/// Pre-fix (69aa4a7c): passes. The expired confirmation arms a full repaint by
/// itself; the input-reveal seam that the fix changes is pinned by the
/// app-level `input_proof_during_a_speculative_reopen_queues_a_from_truth_frame`.
#[test]
fn alt_screen_switch_after_unconfirmed_present_paints() {
    let t0 = Instant::now();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"primary screen content\r\n");
    rt.tick_at(t0).expect("primary frame presents");
    // Arm the compositor confirmation bound without a real GPU present, then
    // let it expire: this is the corrective-repaint path.
    rt.arm_present_confirmation_for_test(t0);

    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Hunconfirmed alt content\r\nUNCONFIRMED-ALT");
    let stats = rt
        .tick_at(t0 + Duration::from_millis(400))
        .expect("alt frame presents");
    assert_frame_has_ink("unconfirmed alt switch", &rt, &stats);
}

/// Replays the captured agy help transcript byte-for-byte and requires every
/// presented frame after the alt-screen switch to carry ink.
///
/// Pre-fix (69aa4a7c): passes. The transcript never makes a leaf render empty,
/// so the retained store never holds the empty list this issue is about; what
/// the test can pin here is that the reuse path is entered at all (asserted
/// below - the fixture produces only a couple of such frames out of ~174, so
/// the precondition is not incidental). The refusal itself is pinned by
/// `runtime::present::empty_leaf_guard_tests`.
#[test]
fn captured_agy_help_transcript_never_presents_an_empty_frame() {
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("deterministic runtime must build");
    // The fixture was captured at 100x30; the window extent, not the config
    // fields, decides the live grid, so resize to that content size.
    rt.handle_resize(bitty_platform::PhysicalSize::new(
        100 * 9 + 44,
        32 * 19 + 44,
    ))
    .expect("resize applies");
    assert_eq!(rt.snapshot().width, 100);
    assert!(rt.snapshot().height >= 30, "the 30-row fixture must fit");
    let bytes = include_bytes!("../../bitty-term-state/tests/data/agy-help-100x30.raw");
    let mut presented = 0usize;
    let mut reused = 0usize;
    for chunk in bytes.chunks(37) {
        rt.handle_pty_bytes(chunk);
        if let Some(stats) = rt.tick() {
            presented += 1;
            if reused_retained_leaf(&stats) {
                reused += 1;
            }
            let snapshot = rt.snapshot();
            let has_text = snapshot
                .cells
                .iter()
                .any(|cell| cell.glyph != ' ' && !cell.spacer);
            if has_text {
                assert!(
                    stats.fills > 0 || stats.glyphs > 0,
                    "frame {presented} presented an empty draw list with text on screen \
                     (alt={}, fills={}, glyphs={})",
                    rt.state().alt_screen_active(),
                    stats.fills,
                    stats.glyphs
                );
                assert!(
                    ink_pixels(&rt) > 0,
                    "frame {presented} presented no non-background pixels with text on screen"
                );
            }
        }
    }
    assert!(presented > 0, "the transcript must present frames");
    // The reported defect is a stale or empty *retained* list dragged into a
    // later frame, so it can only be reached on a frame that reuses one.
    // Without this the transcript may present only full renders and pin
    // nothing: assert the fixture actually enters the reuse path.
    assert!(
        reused > 0,
        "the transcript must take the retained-reuse path at least once \
         ({presented} frames presented); otherwise this test never reaches the \
         code bitty#3032 fixes"
    );

    // One more reuse frame after the transcript has settled: a ring pulse is
    // a paint-only frame, so every clean leaf reuses its retained list.
    let view = rt.focused_view().expect("focused view");
    let pulse = Instant::now();
    assert!(
        rt.trigger_fx_pulse(view, pulse),
        "the fx ring pulse must arm a paint-only frame"
    );
    rt.tick_at(pulse + Duration::from_millis(1))
        .expect("pulse frame presents");
    let stats = rt
        .tick_at(pulse + Duration::from_millis(16))
        .expect("second pulse frame presents");
    assert!(
        reused_retained_leaf(&stats),
        "the settled transcript leaf must be reused on a paint-only frame \
         (glyphs={}, emitted={})",
        stats.glyphs,
        stats.glyphs_emitted
    );
    assert_frame_has_ink("settled transcript reuse", &rt, &stats);
}

/// The app wraps its alt-screen redraw in `DECSET 2026` and never resets it:
/// the bounded defer window must still commit the alt grid.
///
/// Pre-fix (69aa4a7c): passes. The defer bound commits the frame either way;
/// kept as synchronized-update coverage.
#[test]
fn alt_switch_inside_a_synchronized_update_paints() {
    let t0 = Instant::now();
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"primary content\r\n");
    rt.tick_at(t0).expect("primary frame");
    rt.handle_pty_bytes(b"\x1b[?2026h\x1b[?1049h\x1b[2J\x1b[Hsync alt content\r\nSYNC-ALT");
    // Inside the defer window nothing presents; the bound commits it.
    assert!(rt.tick_at(t0 + Duration::from_millis(1)).is_none());
    let stats = rt
        .tick_at(t0 + bitty_runtime::SYNC_UPDATE_DEFER_TIMEOUT + Duration::from_millis(1))
        .expect("bound frame presents");
    assert_frame_has_ink("sync alt switch", &rt, &stats);
}

/// A pane with a deep scrollback ring still paints after the alternate-screen
/// switch.
///
/// The live report's shape was 9,421 retained lines / ~93 MB in a 150+ column
/// grid. Replaying all of it cost ~60 s and ~93 MB per run without adding
/// coverage: the present path has no byte budget, so the ring only has to be
/// deep enough to leave history behind the viewport. Manual reproduction of the
/// live measurement: resize to 2000x800, push 9,421 lines of the same shape,
/// and assert `state().retained_heap_bytes() > 93 * 1024 * 1024`.
///
/// Pre-fix (69aa4a7c): passes. The alt switch forces a full render regardless
/// of the ring size; the ring never feeds a present-path decision.
#[test]
fn live_retained_heap_shape_still_paints_after_alt_switch() {
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("deterministic runtime must build");
    // A wide content grid (>= 150 columns): the live pane's measurement is at
    // this width, and the ring is retained per cell.
    rt.handle_resize(bitty_platform::PhysicalSize::new(2000, 800))
        .expect("resize applies");
    assert!(
        rt.snapshot().width >= 150,
        "need the live pane's wide grid, got {}",
        rt.snapshot().width
    );
    for i in 0..600 {
        rt.handle_pty_bytes(format!("history line {i:05} padded to 34 cells\r\n").as_bytes());
    }
    rt.tick().expect("history frame presents");
    assert!(
        rt.state().retained_heap_bytes() > 0,
        "the history ring must be retained"
    );
    assert!(
        rt.scrollback_len() >= 500,
        "the measured shape retains a deep history ring, got {}",
        rt.scrollback_len()
    );

    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Halt view after history\r\nALT-AFTER-HISTORY");
    let stats = rt.tick().expect("alt frame presents");
    assert_frame_has_ink("live retained-heap alt switch", &rt, &stats);
}

/// The `?47` alternate-screen variant (no clear on entry) must paint the alt
/// grid content too.
///
/// Pre-fix (69aa4a7c): passes - same full render as the `?1049` case.
#[test]
fn primary_alt_47_switch_with_content_paints() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"primary content\r\n");
    rt.tick().expect("primary frame");
    rt.handle_pty_bytes(b"\x1b[?47h\x1b[2J\x1b[Halt 47 content\r\nALT47-ROW");
    let stats = rt.tick().expect("alt 47 frame presents");
    assert_frame_has_ink("alt 47 switch", &rt, &stats);
}

/// A scrolled primary view entering the alternate screen must drop the stale
/// scroll offset and paint the alt grid.
///
/// Pre-fix (69aa4a7c): passes. The alt switch forces a full render, so the
/// stale scroll offset never reaches the reuse path.
#[test]
fn scrolled_view_alt_switch_paints_the_alt_grid() {
    let mut rt = make_runtime();
    for i in 0..200 {
        rt.handle_pty_bytes(format!("scroll line {i:03}\r\n").as_bytes());
    }
    rt.tick().expect("scrollback frame");
    rt.handle_wheel(bitty_platform::ScrollDelta::Lines(0.0, 5.0));
    let focused = rt.focused_view().expect("focused view");
    assert!(
        rt.layout()
            .find_leaf(focused)
            .is_some_and(|view| view.scroll_offset() > 0),
        "the view must be scrolled into history"
    );
    rt.tick().expect("scrolled frame");

    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Halt after scroll\r\nALT-SCROLLED");
    let stats = rt.tick().expect("alt frame presents");
    assert_frame_has_ink("scrolled alt switch", &rt, &stats);
}

/// A split pane that owns its own session: primary content plus a pane that
/// enters the alternate screen with content. The pane leaf must paint its own
/// alt grid, not an empty retained list.
///
/// Pre-fix (69aa4a7c): passes. The pane alt switch forces a full render; the
/// pane-session retirement the fix adds is pinned indirectly (a respawn already
/// forces a full redraw, see the respawn test).
#[test]
#[cfg(unix)]
fn pane_alt_screen_switch_with_content_paints_the_pane_grid() {
    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    let pane = ViewId::new(2);
    rt.set_layout(bitty_runtime::LayoutNode::split(
        bitty_runtime::SplitAxis::Horizontal,
        0.5,
        bitty_runtime::LayoutNode::leaf(View::new(ViewId::new(1), 40, 12)),
        bitty_runtime::LayoutNode::leaf(View::new(pane, 40, 12)),
    ));
    rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell attaches");
    rt.handle_pty_bytes(b"primary content\r\n");
    rt.handle_pane_bytes(pane, b"pane primary content\r\n");
    rt.tick().expect("first split frame presents");

    rt.handle_pane_bytes(
        pane,
        b"\x1b[?1049h\x1b[2J\x1b[Hpane alt content\r\nPANE-ALT-ROW",
    );
    let stats = rt.tick().expect("pane alt frame presents");
    assert_frame_has_ink("pane alt switch", &rt, &stats);
    let text = rt
        .pane_snapshot(&pane)
        .expect("pane snapshot")
        .cells
        .iter()
        .map(|cell| cell.glyph)
        .collect::<String>();
    assert!(
        text.contains("PANE-ALT-ROW"),
        "pane truth must hold the alt content, got {text:?}"
    );
}

/// A respawned pane must paint the new session's grid, never the previous
/// session's retained primitives.
///
/// `spawn_shell_for_view` replaces the session, so its generation counter
/// restarts and the old grid is gone; a retained leaf stored against the dead
/// session is stale by construction (bitty#3032).
///
/// Pre-fix (69aa4a7c): passes. A respawn also arms `pending_full_redraw`, so
/// the stale entry can never be the reused one in this fixture; the test pins
/// the retirement invariant, not a pre-fix failure.
#[test]
#[cfg(unix)]
fn respawned_pane_never_reuses_the_previous_sessions_retained_leaf() {
    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    let pane = ViewId::new(2);
    rt.set_layout(bitty_runtime::LayoutNode::split(
        bitty_runtime::SplitAxis::Horizontal,
        0.5,
        bitty_runtime::LayoutNode::leaf(View::new(ViewId::new(1), 40, 12)),
        bitty_runtime::LayoutNode::leaf(View::new(pane, 40, 12)),
    ));
    rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell attaches");
    rt.handle_pty_bytes(b"left column\r\n");
    rt.handle_pane_bytes(pane, b"OLD-SESSION-CONTENT\r\n");
    rt.tick().expect("old session frame presents");

    // Replace the session on the same view: the old grid and its retained
    // primitives must retire with it.
    rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell respawns");
    rt.handle_pane_bytes(pane, b"NEW-SESSION-CONTENT\r\n");
    rt.tick().expect("respawn frame presents");
    let incremental = rt.headless_rgba().expect("respawn rgba");

    // The same state rendered from truth: a stale reused leaf cannot match it.
    let now = Instant::now();
    rt.set_surface_visibility_at(now, true);
    rt.tick_at(now + Duration::from_millis(1))
        .expect("from-truth frame presents");
    let truth = rt.headless_rgba().expect("truth rgba");
    assert_eq!(
        incremental, truth,
        "a respawned pane must render from truth, not reuse the previous session's leaf"
    );
}

/// Regression for retained-leaf reuse across `Atlas::try_grow()`: growing the
/// glyph atlas advances `atlas_epoch` without incrementing `evictions`, so a
/// clean sibling leaf must re-render at the new epoch rather than presenting
/// stale pre-growth UV coordinates.
///
/// Pre-fix (69aa4a7c): passes, and the preconditions below are what keep the
/// fixture honest. The frame that grows the atlas rebuilds every leaf at the
/// new epoch, so the entry this test reuses is never actually stale, and atlas
/// slots are pixel-space (growth preserves them), so a stale reuse could not
/// change the headless pixels even if it happened. The frame-level epoch guard
/// is pinned by `runtime::present::atlas_epoch_guard_tests`; the reuse-time
/// check is defence in depth with no headless-observable consequence.
#[test]
#[cfg(unix)]
fn retained_leaf_rebuilds_when_sibling_pane_grows_the_glyph_atlas() {
    bitty_test_support::require_pty!();
    let mut rt = make_runtime();
    let pane = ViewId::new(2);
    rt.set_layout(bitty_runtime::LayoutNode::split(
        bitty_runtime::SplitAxis::Horizontal,
        0.5,
        bitty_runtime::LayoutNode::leaf(View::new(ViewId::new(1), 40, 12)),
        bitty_runtime::LayoutNode::leaf(View::new(pane, 40, 12)),
    ));
    rt.spawn_shell_for_view(pane, "/bin/sh", &["-c", "sleep 30"], 40, 12)
        .expect("pane shell attaches");
    // Wide enough that the right pane alone holds more distinct glyphs than
    // the starting atlas fits, so `Atlas::try_grow()` has to double it.
    rt.handle_resize(bitty_platform::PhysicalSize::new(4000, 1200))
        .expect("resize applies");

    rt.handle_pty_bytes(b"LEFT-PANE-ANCHOR-LINE-ONE\r\nLEFT-PANE-ANCHOR-LINE-TWO\r\n");
    rt.handle_pane_bytes(pane, b"right warmup\r\n");
    rt.tick().expect("initial split frame presents");

    let (epoch_before, _, evictions_before) = rt.probe_atlas_epochs();
    // Flood ONLY the right pane with distinct Unicode scalars so the shared
    // glyph atlas grows via `Atlas::try_grow()` (which increments `atlas_epoch`
    // without incrementing `evictions`) while the left primary pane is clean.
    let cols = rt.snapshot().width / 2;
    let rows = rt.snapshot().height;
    let mut distinct = String::new();
    let mut code = 0x0100u32;
    for _ in 0..rows {
        for _ in 0..cols {
            while char::from_u32(code).is_none_or(char::is_control) {
                code += 1;
            }
            distinct.push(char::from_u32(code).expect("checked above"));
            code += 1;
        }
        distinct.push_str("\r\n");
    }
    rt.handle_pane_bytes(pane, distinct.as_bytes());
    let stats = rt.tick().expect("atlas-growth split frame presents");
    assert_frame_has_ink("atlas growth frame", &rt, &stats);

    // The comparison below only means anything if this fixture really grew the
    // atlas in place. Assert that precondition instead of trusting the name:
    // a wholesale eviction would already have dropped every retained leaf and
    // prove nothing about the epoch check.
    let (epoch_after, _, evictions_after) = rt.probe_atlas_epochs();
    assert!(
        epoch_after > epoch_before,
        "fixture precondition: the flood must grow the glyph atlas \
         (epoch {epoch_before} -> {epoch_after})"
    );
    assert_eq!(
        evictions_after, evictions_before,
        "fixture precondition: growth must not be an eviction reset \
         (evictions {evictions_before} -> {evictions_after})"
    );

    // One more right-only update exercises `reuse_delta` on the left pane after
    // the epoch bump. The presented RGBA must be byte-identical to a fresh
    // full-frame render from truth.
    rt.handle_pane_bytes(pane, b"\r\ntrailer");
    rt.tick().expect("post-growth incremental frame presents");
    let incremental_rgba = rt.headless_rgba().expect("incremental rgba");

    // Force a full-frame rebuild via a no-op visibility reveal and compare.
    let now = Instant::now();
    rt.set_surface_visibility_at(now, true);
    rt.tick_at(now + Duration::from_millis(1))
        .expect("from-truth reveal frame presents");
    let truth_rgba = rt.headless_rgba().expect("from-truth rgba");
    assert_eq!(
        incremental_rgba, truth_rgba,
        "retained leaf after atlas growth must match a fresh from-truth render"
    );
}

/// Regression for `collect_tick_basis` + `defer_coalesced_present`: when a
/// tick inside the coalescing cadence latches an alternate-screen transition
/// and is then deferred, `pending_full_redraw` must survive to the released
/// tick so the frame still performs a full redraw.
///
/// Pre-fix (69aa4a7c): passes. A PTY alt switch arms `pending_full_redraw`
/// through its own mode hook, so the request is already durable before
/// `collect_tick_basis` runs and this shape cannot show the deferral dropping
/// it. The persistence itself is pinned by
/// `runtime::present::coalesced_deferral_tests`, which desynchronises the latch
/// directly (private state no PTY input can reach).
#[test]
fn coalesced_deferral_preserves_full_redraw_across_alt_latch() {
    let t0 = Instant::now();
    let mut rt = make_runtime();
    rt.set_min_present_interval(Duration::from_millis(16));
    rt.handle_pty_bytes(b"primary screen line\r\n");
    rt.tick_at(t0).expect("initial primary frame presents");

    // Enter alt screen inside the 16 ms cadence window. `collect_tick_basis`
    // updates `kitty_alt_screens_latched`, and `defer_coalesced_present`
    // defers the frame.
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Hdeferred alt content\r\nDEFERRED-ALT-ROW");
    assert!(
        rt.tick_at(t0 + Duration::from_millis(2)).is_none(),
        "tick inside cadence window must defer"
    );
    assert!(
        rt.present_owed(),
        "deferred alt transition must still owe a present"
    );
    // bitty#3032: the latch update that forced this full redraw happened in
    // `collect_tick_basis`, before the deferral dropped that basis. The request
    // must be persisted, or the released tick renders incrementally and can
    // reuse a retained leaf from before the alt switch.
    assert!(
        rt.probe_full_redraw_armed(),
        "a deferred alt latch must carry its full-redraw request to the next tick"
    );

    let stats = rt
        .tick_at(t0 + Duration::from_millis(20))
        .expect("released tick after cadence window must present");
    assert_frame_has_ink("deferred alt switch", &rt, &stats);
}
