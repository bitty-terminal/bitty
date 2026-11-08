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

/// Primary screen with content, then an alternate-screen switch that carries
/// its own content in the same PTY batch.
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
    for chunk in bytes.chunks(37) {
        rt.handle_pty_bytes(chunk);
        if let Some(stats) = rt.tick() {
            presented += 1;
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
}

/// The app wraps its alt-screen redraw in `DECSET 2026` and never resets it:
/// the bounded defer window must still commit the alt grid.
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

/// The live 93 MB `retained_heap_bytes` shape: 9,421 scrollback lines at the
/// live pane width. That figure is the scrollback ring, not a retained frame;
/// no present-path byte budget exists, so the alt switch must still paint.
#[test]
fn live_retained_heap_shape_still_paints_after_alt_switch() {
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("deterministic runtime must build");
    // A wide content grid (>= 150 columns): 9,421 retained lines at this
    // width reproduce the live pane's 93 MB scrollback measurement.
    rt.handle_resize(bitty_platform::PhysicalSize::new(2000, 800))
        .expect("resize applies");
    assert!(
        rt.snapshot().width >= 150,
        "need the live pane's wide grid, got {}",
        rt.snapshot().width
    );
    for i in 0..9_421 {
        rt.handle_pty_bytes(format!("history line {i:05} padded to 34 cells\r\n").as_bytes());
    }
    rt.tick().expect("history frame presents");
    let retained = rt.state().retained_heap_bytes();
    assert!(
        rt.scrollback_len() >= 9_300,
        "the measured shape retains the history ring, got {}",
        rt.scrollback_len()
    );
    assert!(
        retained > 93 * 1024 * 1024,
        "the live 93 MB retained heap must be reproduced, got {retained} bytes"
    );

    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[2J\x1b[Halt view after history\r\nALT-AFTER-HISTORY");
    let stats = rt.tick().expect("alt frame presents");
    assert_frame_has_ink("live retained-heap alt switch", &rt, &stats);
}

/// The `?47` alternate-screen variant (no clear on entry) must paint the alt
/// grid content too.
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

/// Regression for retained-leaf reuse across `Atlas::try_grow()`: growing the
/// glyph atlas advances `atlas_epoch` without incrementing `evictions`, so a
/// clean sibling leaf must re-render at the new epoch rather than presenting
/// stale pre-growth UV coordinates.
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

    rt.handle_pty_bytes(b"LEFT-PANE-ANCHOR-LINE-ONE\r\nLEFT-PANE-ANCHOR-LINE-TWO\r\n");
    rt.handle_pane_bytes(pane, b"right warmup\r\n");
    rt.tick().expect("initial split frame presents");

    // Flood ONLY the right pane with distinct Unicode scalars so the shared
    // glyph atlas grows via `Atlas::try_grow()` (which increments `atlas_epoch`
    // without incrementing `evictions`) while the left primary pane is clean.
    let mut distinct = String::new();
    for code in 0x0100u32..0x0700u32 {
        if let Some(ch) = char::from_u32(code) {
            distinct.push(ch);
            if (code - 0x0100) % 32 == 31 {
                distinct.push_str("\r\n");
            }
        }
    }
    rt.handle_pane_bytes(pane, distinct.as_bytes());
    let stats = rt.tick().expect("atlas-growth split frame presents");
    assert_frame_has_ink("atlas growth frame", &rt, &stats);

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

    let stats = rt
        .tick_at(t0 + Duration::from_millis(20))
        .expect("released tick after cadence window must present");
    assert_frame_has_ink("deferred alt switch", &rt, &stats);
}
