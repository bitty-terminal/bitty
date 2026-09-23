//! CTX-0751 (issue #1344): the terminal present path consumes the armed
//! hint overlay.
//!
//! `HintOverlayPresent` was produced into `CwPresentPlan` with no reader,
//! so arming the Leader session never painted a label. These tests drive
//! the live [`Runtime`] owner headless — arm/disarm plus
//! [`Runtime::cw_hint_overlay_cells`] against hand-built present frames —
//! so removing the wiring fails them. No PTY server, window, GPU,
//! wall-clock, or filesystem.
//!
//! - disarmed / empty batch resolves to no cells (fail-closed when absent);
//! - armed command targets resolve to column 0 of their prompt's live
//!   viewport row in the primary owner's frame;
//! - view anchors badge the leaf's top-left cell; panel anchors skip
//!   fail-closed (no panel-to-view map in the present path, OQ-051);
//! - alt-screen command rows skip fail-closed.

use bitty_render::geometry::RectPx;
use bitty_runtime::cw_present::CwHintProvider;
use bitty_runtime::{HintScope, LayoutNode, PresentFrame, Runtime, RuntimeConfig, View, ViewId};

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime must build")
}

/// One decorated leaf allocation for `view` with an 80x24 content grid.
fn frame(view: ViewId) -> PresentFrame {
    PresentFrame {
        view,
        frame: RectPx::new(0, 0, 80 * 9, 24 * 19),
        content: RectPx::new(0, 0, 80 * 9, 24 * 19),
        cols: 80,
        rows: 24,
        border: 0,
        radius: 0,
        tier: None,
    }
}

/// Single-pane layout owning the primary state on view 1.
fn solo_layout() -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(1), 80, 24))
}

/// Feeds one full `OSC 133` command cycle (prompt / input / output / done).
fn mark_command(rt: &mut Runtime) {
    rt.handle_pty_bytes(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07output\x1b]133;D;0\x07");
}

#[test]
fn hint1344_disarmed_resolves_no_cells() {
    // Fail-closed when absent: no session, no overlay, even with frames.
    let rt = runtime();
    assert!(!rt.cw_hint_is_armed());
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))])
            .is_empty()
    );
    // Unknown views also resolve nothing.
    assert!(rt.cw_hint_overlay_cells(&[]).is_empty());
}

#[test]
fn hint1344_empty_batch_resolves_no_cells() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 0, "no zones and no providers: empty batch");
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))])
            .is_empty(),
        "empty overlay paints nothing"
    );
    rt.cw_hint_disarm();
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))])
            .is_empty()
    );
}

#[test]
fn hint1344_armed_command_resolves_to_prompt_row() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    mark_command(&mut rt);
    let latest = rt.cw_latest_command_id().expect("marked command");
    let _ = latest;
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 1);
    let cells = rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))]);
    assert_eq!(cells.len(), 1, "one armed label paints one pill");
    let cell = &cells[0];
    assert_eq!(cell.view, ViewId::new(1));
    assert_eq!(cell.col, 0);
    assert_eq!(cell.label, "a");
    assert_eq!(cell.width_cells(), 1);
    assert!(cell.row < 24, "pill sits inside the presented window");
}

#[test]
fn hint1344_three_commands_paint_distinct_prompt_rows() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    for _ in 0..3 {
        mark_command(&mut rt);
        rt.handle_pty_bytes(b"\r\n");
    }
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 3);
    let cells = rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))]);
    assert_eq!(cells.len(), 3, "every armed label paints");
    let expected: Vec<String> = rt
        .cw_hint_collect(HintScope(1), 1)
        .labels()
        .iter()
        .map(|l| l.label.clone())
        .collect();
    let labels: Vec<&str> = cells.iter().map(|c| c.label.as_str()).collect();
    let expected_refs: Vec<&str> = expected.iter().map(|s| s.as_str()).collect();
    assert_eq!(labels, expected_refs, "paint follows label order");
    let mut rows: Vec<u16> = cells.iter().map(|c| c.row).collect();
    rows.sort_unstable();
    rows.dedup();
    assert_eq!(rows.len(), 3, "one pill per prompt row, never stacked");
    assert!(cells.iter().all(|c| c.col == 0 && c.view == ViewId::new(1)));
}

#[test]
fn hint1344_view_anchor_badges_leaf_panel_anchor_skips() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    // One panel target plus one view target through the cross-panel engine.
    assert!(
        rt.cw_hint_register(CwHintProvider::with_views(7, &[2]).expect("fits cap")),
        "provider registers"
    );
    let count = rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(count, 2);
    let frames = vec![frame(ViewId::new(1)), frame(ViewId::new(2))];
    let cells = rt.cw_hint_overlay_cells(&frames);
    assert_eq!(
        cells.len(),
        1,
        "view anchor badges its leaf; the panel anchor skips fail-closed"
    );
    assert_eq!(cells[0].view, ViewId::new(2));
    assert_eq!((cells[0].col, cells[0].row), (0, 0));
    assert!(!cells[0].label.is_empty());
}

#[test]
fn hint1344_alt_screen_skips_command_pills() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    mark_command(&mut rt);
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))]).len(), 1);
    // Entering the alternate screen invalidates primary-grid rows: the
    // overlay must vanish, not point at alt-screen content.
    rt.handle_pty_bytes(b"\x1b[?1049h");
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))])
            .is_empty(),
        "alt-screen paints no command pills"
    );
    rt.handle_pty_bytes(b"\x1b[?1049l");
}

#[test]
fn hint1344_missing_owner_frame_paints_nothing() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    mark_command(&mut rt);
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    // The primary owner's frame is gone (closed leaf): fail closed.
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(9))])
            .is_empty(),
        "no owner frame means no pills"
    );
    assert!(rt.cw_hint_overlay_cells(&[]).is_empty());
}

#[test]
fn hint1344_disarm_clears_cells() {
    let mut rt = runtime();
    rt.set_layout(solo_layout());
    mark_command(&mut rt);
    rt.cw_hint_arm(HintScope(1), 1, &[]).expect("arms");
    assert_eq!(rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))]).len(), 1);
    rt.cw_hint_disarm();
    assert!(
        rt.cw_hint_overlay_cells(&[frame(ViewId::new(1))])
            .is_empty(),
        "disarm removes the overlay"
    );
}
