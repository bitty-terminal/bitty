//! CTX-0386: per-pane damage tracking — splits must not re-render clean
//! panes.
//!
//! Issue #642: `tick` forced a full per-leaf repaint whenever any pane
//! session existed (`use_full = !self.pane_sessions.is_empty() || ...`), so
//! every presented frame re-examined every split pane's cells and re-emitted
//! its glyphs. These tests pin the replacement contract:
//!
//! - a frame driven by one pane's output re-renders only that pane:
//!   [`PresentStats::cells_examined`] stays within the damaged pane's cell
//!   budget while the composited frame still carries the quiet pane;
//! - the quiet pane's pixels are byte-identical across the incremental frame
//!   (a reused retained list, never a blanked or stale tile);
//! - focus changes remain genuine full invalidation (every leaf re-renders);
//! - a pane that produced output while it was not visible paints its
//!   accumulated content when it becomes visible again (no stale retained
//!   list survives a visibility round trip).
//!
//! Headless and deterministic: no wall clock, no display server. The
//! live-spawn tests need a PTY and a POSIX shell (`/bin/sh`).

// Every scenario in this file spawns a POSIX pane shell, so the whole test
// target is unix-only; the per-test `#[cfg(unix)]` gates remain as intent
// markers and keep `require_pty!` scoped to a real spawn.
#![cfg(unix)]

use bitty_runtime::{AnimationPolicy, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

/// RFC-0002 animations off: these tests pin damage/work containment, not the
/// panel transitions, so every change presents exactly one frame.
fn instant_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("instant runtime must build")
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 40, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 40, 24)),
    )
}

/// Writes a full-width marker row into the primary grid (cursor parks on the
/// next row, so row content and cursor ink never share a row).
fn write_primary_marker(rt: &mut Runtime, row_1based: u8, glyph: u8) {
    assert!((1..=9).contains(&row_1based), "single-digit rows only");
    let mut seq = vec![0x1b, b'[', b'0' + row_1based, b';', b'1', b'H'];
    seq.extend(std::iter::repeat_n(glyph, 80));
    rt.handle_pty_bytes(&seq);
    let snap = rt.snapshot();
    let row = usize::from(row_1based - 1);
    assert!(
        snap.cells
            .chunks(snap.width)
            .nth(row)
            .is_some_and(|cells| cells.iter().all(|c| c.glyph as u8 == glyph)),
        "marker row {row_1based} must read back full-width"
    );
}

/// Physical-pixel bounds of one leaf tile: the decorated content frame
/// translated exactly like the present layer (physical px + window padding).
fn tile_bounds(rt: &Runtime, id: ViewId) -> (usize, usize, usize, usize) {
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"));
    (
        usize::try_from(frame.content.x.max(0)).expect("content x fits") + pad,
        usize::try_from(frame.content.y.max(0)).expect("content y fits") + pad,
        frame.content.width as usize,
        frame.content.height as usize,
    )
}

/// Bytes of one leaf tile from the last presented headless frame.
fn tile_bytes(rt: &Runtime, id: ViewId) -> Vec<u8> {
    let rgba = rt.headless_rgba().expect("rgba after present");
    let extent = rt.config().window_extent();
    let sw = usize::try_from(extent.width()).expect("surface width fits");
    let (x, y, w, h) = tile_bounds(rt, id);
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = ((y + row) * sw + x) * 4;
        out.extend_from_slice(&rgba[start..start + w * 4]);
    }
    out
}

/// Bytes of one leaf tile minus the in-grid status bar band (#1349): the
/// bar text legitimately changes across a workspace round trip (one
/// workspace becomes two), so chrome pixels are masked and only pane
/// content is compared.
///
/// The band is the last snapshot row as the renderer paints it: top at
/// `(rows - 1) * cell_h`, one cell row high, `cols * cell_w` wide. The
/// division `content / rows` must NOT be used here — its floor remainder
/// drifts from the renderer's live cell metrics (e.g. 428px / 22 rows =
/// 19px cells with 10px spare, so the painted band starts 10px above the
/// naive `height - 19` line). Cell metrics are the default headless
/// 9x19 (same geometry the `mouse_chrome.rs` `cell_pixels` helper pins).
fn tile_content_bytes(rt: &Runtime, id: ViewId) -> Vec<u8> {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"));
    strip_bar_band(
        &tile_bytes(rt, id),
        frame.content.width as usize,
        frame.content.height as usize,
        usize::from(frame.rows),
        usize::from(frame.cols),
    )
}

/// Masks the painted bar band from captured tile bytes (total: every
/// index is clamped, never panics).
fn strip_bar_band(bytes: &[u8], tile_w: usize, tile_h: usize, rows: usize, cols: usize) -> Vec<u8> {
    const CELL_W: usize = 9;
    const CELL_H: usize = 19;
    let band_top = rows.saturating_sub(1).saturating_mul(CELL_H);
    let band_w = cols.saturating_mul(CELL_W).min(tile_w);
    let mut out = Vec::with_capacity(bytes.len());
    for r in 0..tile_h {
        let start = r.saturating_mul(tile_w).saturating_mul(4);
        let end = start.saturating_add(tile_w.saturating_mul(4));
        let Some(row) = bytes.get(start..end.min(bytes.len())) else {
            break;
        };
        if r >= band_top && r < band_top.saturating_add(CELL_H) {
            // Band row: keep only the right-of-bar remainder.
            let cut = band_w.saturating_mul(4).min(row.len());
            out.extend_from_slice(&row[cut..]);
        } else {
            out.extend_from_slice(row);
        }
    }
    out
}

/// Viewport cell count of one leaf for this frame (the renderer's full-leaf
/// work budget).
fn pane_cells(rt: &Runtime, id: ViewId) -> u64 {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == id)
        .unwrap_or_else(|| panic!("leaf {id:?} must have a present frame"));
    u64::from(frame.cols) * u64::from(frame.rows)
}

/// Two-pane split with a live pane shell on `ViewId(2)` and a marker in the
/// primary owner `ViewId(1)`, advanced to a stable (idle) frame.
#[cfg(unix)]
fn stable_split() -> Runtime {
    let mut rt = instant_runtime();
    write_primary_marker(&mut rt, 2, b'M');
    assert!(rt.tick().is_some(), "initial frame must present");
    rt.set_layout(two_pane());
    assert!(rt.tick().is_some(), "split must present");
    rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "sleep 30"], 40, 24)
        .expect("pane shell must spawn");
    assert!(rt.tick().is_some(), "spawn must present a full frame");
    assert_eq!(rt.tick(), None, "must idle after the spawn frame");
    rt
}

#[cfg(unix)]
#[test]
fn split_output_reprocesses_only_the_damaged_pane() {
    bitty_test_support::require_pty!();
    let mut rt = stable_split();

    let quiet_before = tile_bytes(&rt, ViewId::new(1));
    let pane_before = tile_bytes(&rt, ViewId::new(2));
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b[4;1HBBBB\r\n");
    let stats = rt.tick().expect("pane output must present");

    let damaged = pane_cells(&rt, ViewId::new(2));
    let both = damaged + pane_cells(&rt, ViewId::new(1));
    eprintln!(
        "CTX-0386 incremental frame: cells_examined={} glyphs_emitted={} (damaged pane budget={damaged}, both panes={both})",
        stats.cells_examined, stats.glyphs_emitted
    );
    assert!(
        stats.cells_examined <= damaged + 1,
        "one pane's output must re-examine only that pane \
         (examined={}, damaged pane={damaged}, both panes={both}; +1 is the \
         synthetic 1x1 plan probe)",
        stats.cells_examined,
    );
    assert!(
        stats.cells_examined < both,
        "incremental frame must not re-examine the quiet pane"
    );
    assert!(
        stats.glyphs_emitted < stats.glyphs as u64,
        "the quiet pane's {} glyphs must be reused, not re-emitted \
         (only {} glyphs were emitted this frame)",
        stats.glyphs,
        stats.glyphs_emitted,
    );
    assert_ne!(
        tile_bytes(&rt, ViewId::new(2)),
        pane_before,
        "the damaged pane must repaint its own tile"
    );
    assert_eq!(
        tile_bytes(&rt, ViewId::new(1)),
        quiet_before,
        "the quiet pane tile must be reused byte-identically, never blanked"
    );
    assert_eq!(rt.tick(), None, "incremental frame must return to idle");
}

#[cfg(unix)]
#[test]
fn focus_change_reprocesses_every_leaf() {
    bitty_test_support::require_pty!();
    let mut rt = stable_split();

    let owner_before = tile_bytes(&rt, ViewId::new(1));
    assert!(rt.set_focus(ViewId::new(2)), "focus the live pane");
    let stats = rt.tick().expect("focus change must present");

    let both = pane_cells(&rt, ViewId::new(1)) + pane_cells(&rt, ViewId::new(2));
    eprintln!(
        "CTX-0386 focus frame: cells_examined={} (both panes={both})",
        stats.cells_examined
    );
    assert!(
        stats.cells_examined >= both,
        "focus is a genuine full invalidation: every leaf must re-render \
         (examined={}, both panes={both})",
        stats.cells_examined,
    );
    assert_ne!(
        tile_bytes(&rt, ViewId::new(1)),
        owner_before,
        "the unfocused owner must repaint without its cursor"
    );
    assert_eq!(rt.tick(), None, "focus frame must return to idle");
}

#[cfg(unix)]
#[test]
fn hidden_pane_output_paints_when_visible_again() {
    bitty_test_support::require_pty!();
    let mut rt = stable_split();

    let owner_before = tile_bytes(&rt, ViewId::new(1));
    let empty_pane = tile_bytes(&rt, ViewId::new(2));

    // Hide the split pane behind a fresh workspace.
    let ws = rt.workspace_new().expect("new workspace");
    assert_eq!(ws, 1);
    assert!(rt.tick().is_some(), "workspace switch must present");
    assert_eq!(rt.tick(), None, "workspace frame must idle");

    // Pane output arrives while the pane is not visible.
    rt.handle_pane_bytes(ViewId::new(2), b"\x1b[5;1HHIDDEN");
    assert!(
        rt.tick().is_some(),
        "hidden pane output must keep frame-on-demand awake"
    );
    assert_eq!(rt.tick(), None);

    // Visible again: the committed output must paint, the owner marker must
    // survive the round trip, and no retained list may resurrect stale bytes.
    // (#1349: chrome masked — the bar text changed with the workspace count.)
    assert!(rt.workspace_switch(0), "switch back to the split workspace");
    assert!(rt.tick().is_some(), "switch back must present");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("owner leaf must have a present frame");
    assert_eq!(
        tile_content_bytes(&rt, ViewId::new(1)),
        strip_bar_band(
            &owner_before,
            frame.content.width as usize,
            frame.content.height as usize,
            usize::from(frame.rows),
            usize::from(frame.cols),
        ),
        "primary owner must repaint its marker after the visibility round trip"
    );
    assert_ne!(
        tile_bytes(&rt, ViewId::new(2)),
        empty_pane,
        "the pane must show the output produced while it was hidden"
    );
    assert_eq!(rt.tick(), None, "round trip must return to idle");
}
