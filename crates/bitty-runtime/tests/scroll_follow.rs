//! CTX-0361 regression: the presented viewport must follow the live cursor.
//!
//! Live report (`recording/ctx-verify-2026-09-12b/item1-main65-typed-after-overflow.png`):
//! `seq 1 120` over a 155x42 pane leaves the presented viewport stuck on rows
//! 80-119 while the shell prompt/cursor sits below. Typed input lands in the
//! grid (`ctl terminal text` shows it) but is never painted.
//!
//! Root cause: the decorated content frame (CTX-0294) is smaller than the
//! PTY grid until the deferred reflow lands, and the live present path took
//! the *top-left* `cols x rows` window of the screen snapshot
//! (`viewport_snapshot`), cropping the bottom rows where the cursor/prompt
//! live. The scrolled-history path (`View::visible_cells`) already
//! bottom-aligns; only the live window was wrong.
//!
//! These tests observe the *presented* pixels (headless RGBA), not the
//! state-derived viewport, so a correct model with a broken present path
//! stays red. All headless and deterministic (no wall clock, no PTY spawn).

use bitty_platform::ScrollDelta;
use bitty_runtime::{AnimationPolicy, PresentFrame, Runtime, RuntimeConfig};

/// Default headless grid is 80x24 while the unified CTX-0333 decoration
/// keeps 76x22 content cells, so the live screen snapshot is 2 rows taller
/// than the presented frame — the exact mismatch the live bug hit.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build")
}

/// Feeds lines until the grid has scrolled (output overfills the screen) and
/// the cursor rests on the live bottom row, below the content frame.
fn overfill(rt: &mut Runtime, lines: usize) {
    for i in 0..lines {
        rt.handle_pty_bytes(format!("LINE-{i:03}\r\n").as_bytes());
    }
    assert!(rt.tick().is_some(), "output must present");
    let snap = rt.snapshot();
    assert!(
        rt.state().scrollback_len() > 0,
        "need scrollback pressure, got {}",
        rt.state().scrollback_len()
    );
    let frame = focused_frame(rt);
    assert!(
        snap.cursor.position.row as usize >= usize::from(frame.rows),
        "precondition: cursor row {} must sit below the {} content rows",
        snap.cursor.position.row,
        frame.rows
    );
}

fn focused_frame(rt: &Runtime) -> PresentFrame {
    let focused = rt.focused_view().expect("focused leaf");
    rt.present_frames()
        .into_iter()
        .find(|f| f.view == focused)
        .expect("focused leaf has a present frame")
}

/// Compact digest of the *bottom* presented content row band (mid-cell
/// scanline): non-modal pixel count plus an FNV-1a hash. With follow working
/// this row is the cursor/prompt row; with the top-left crop it is a stale
/// scrollback row that never changes when typing.
fn presented_bottom_row(rt: &Runtime) -> (usize, u64) {
    let rgba = rt.headless_rgba().expect("rgba after present");
    let frame = focused_frame(rt);
    let cfg = rt.config();
    let cw = usize::try_from(cfg.cell_width).expect("cell width");
    let ch = usize::try_from(cfg.cell_height).expect("cell height");
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad");
    let stride = usize::try_from(rt.surface_extent().expect("extent").width()).expect("width");
    let y = pad
        + usize::try_from(frame.content.y).expect("content y")
        + (usize::from(frame.rows) - 1) * ch
        + ch / 2;
    let x0 = pad + usize::try_from(frame.content.x).expect("content x");
    let x1 = x0 + usize::from(frame.cols) * cw;
    // The modal RGBA is the band background; count pixels that differ from it.
    let mut tally: std::collections::HashMap<[u8; 4], usize> = std::collections::HashMap::new();
    for x in x0..x1 {
        let i = (y * stride + x) * 4;
        *tally
            .entry([rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]])
            .or_insert(0) += 1;
    }
    let bg = tally
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(pixel, _)| *pixel)
        .unwrap_or([0, 0, 0, 0]);
    let mut ink = 0usize;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for x in x0..x1 {
        let i = (y * stride + x) * 4;
        let pixel = [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]];
        if pixel != bg {
            ink += 1;
        }
        for b in pixel {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    (ink, hash)
}

fn focused_offset(rt: &Runtime) -> usize {
    let focused = rt.focused_view().expect("focused leaf");
    rt.layout()
        .find_leaf(focused)
        .expect("focused leaf")
        .scroll_offset()
}

#[test]
fn typed_echo_is_painted_on_the_presented_bottom_row_after_overflow() {
    let mut rt = runtime();
    overfill(&mut rt, 60);
    let before = presented_bottom_row(&rt);

    // Type and simulate the shell echo at the live prompt row.
    rt.write_input(b"X");
    rt.handle_pty_bytes(b"X");
    assert!(rt.tick().is_some(), "echo must present");

    let after = presented_bottom_row(&rt);
    assert_ne!(
        before, after,
        "typed echo must be painted on the presented cursor row (CTX-0361: \
         top-left crop hides the live bottom)"
    );
}

#[test]
fn scroll_back_suspends_follow_and_typing_reengages_with_visible_echo() {
    let mut rt = runtime();
    overfill(&mut rt, 60);
    let live = presented_bottom_row(&rt);

    // Deliberate scroll-back: follow suspends, history shows.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 2.0));
    let offset = focused_offset(&rt);
    assert!(offset > 0, "wheel-up must scroll into history");
    assert!(rt.tick().is_some(), "scroll must present");
    let scrolled = presented_bottom_row(&rt);
    assert_ne!(scrolled, live, "scrolled view must show history");

    // Typing re-engages follow and the echo must be painted at the live
    // bottom (pre-fix the re-engaged frame equals the stale live crop).
    rt.write_input(b"Z");
    assert_eq!(focused_offset(&rt), 0, "typing must re-engage follow");
    rt.handle_pty_bytes(b"Z");
    assert!(rt.tick().is_some(), "re-engaged echo must present");
    let after = presented_bottom_row(&rt);
    assert_ne!(scrolled, after, "typing must leave the history view");
    assert_ne!(
        live, after,
        "re-engaged follow must paint the typed echo at the live bottom"
    );
}
