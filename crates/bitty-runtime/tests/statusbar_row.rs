#![forbid(unsafe_code)]
//! No-Core-bar default (W-104/CTX-0956): Core reserves no workspace bar
//! band, paints no bar, and routes no bar clicks — the `bar` plugin owns
//! workspace/status UX over the generic band mechanism (C1-C3, pinned in
//! `host_chrome_gaps.rs`).
//!
//! These tests pin headlessly (no window, adapter, or display server) that
//! the retired Core bar leaves the full window grid to terminal truth:
//!
//! - with any workspace count, and with `show_bar = false`, the leaf and
//!   primary grid rows equal the full window rows (snapshot height; the
//!   kernel PTY winsize is pinned by a POSIX live-shell test), and no Core
//!   chrome paints outside grid cells;
//! - toggling visibility, crossing the one/two workspace boundary, changing
//!   the edge, and resizing never reserve a row or reflow the grid for Core;
//! - a press on the last window row is terminal content (selection / mouse
//!   capture), never consumed chrome: no workspace switch, no
//!   release-swallow pairing;
//! - a window too small for content keeps content (no band to hide);
//! - no Core bar paints on the alternate screen (a fullscreen app owns
//!   every row of its grid);
//! - quiet workspace switches and renames still present through the normal
//!   allocation/focus/redraw damage (no bar-text signal);
//! - split-border drag maps through the window origin with no band offset.

use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState};
use bitty_runtime::config::BarEdge;
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId};

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

/// True when any pixel of window row `row` (cell-row band in window cells)
/// differs from the theme background.
fn window_row_painted(rt: &Runtime, row: u16) -> bool {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let (cw, ch) = rt.live_cell_size();
    let (cw, ch) = (cw as usize, ch as usize);
    let pad = usize::try_from(rt.window_padding_physical()).expect("usize");
    let window = rt.window_cells();
    let stride = (width * 4).max(1);
    // Sample the middle third of the cell row only: the Core decoration
    // ring of an adjacent frame may touch the row's outer pixel lines, but
    // painted content fills the whole cell.
    let top = pad + usize::from(row) * ch + ch / 3;
    let bottom = top + ch / 3;
    let bg = bitty_render::grid::DEFAULT_BG;
    for y in top..bottom.min(rgba.len() / stride) {
        for c in 0..usize::from(window.width) {
            let x = pad + c * cw + cw / 2;
            if x >= width {
                break;
            }
            if probe(&rgba, width, x, y) != bg {
                return true;
            }
        }
    }
    false
}

/// True when any pixel of the cell whose center is `center` differs from
/// the theme background (the cell box is sampled whole so glyph strokes
/// off the exact center still count).
fn cell_has_ink(rt: &Runtime, center: CursorPosition) -> bool {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let height = rgba.len() / (width * 4).max(1);
    let (cw, ch) = rt.live_cell_size();
    let (cw, ch) = (f64::from(cw), f64::from(ch));
    let left = (center.x - cw / 2.0).max(0.0) as usize;
    let top = (center.y - ch / 2.0).max(0.0) as usize;
    let right = ((center.x + cw / 2.0) as usize).min(width);
    let bottom = ((center.y + ch / 2.0) as usize).min(height);
    let bg = bitty_render::grid::DEFAULT_BG;
    (top..bottom).any(|y| (left..right).any(|x| probe(&rgba, width, x, y) != bg))
}

/// Physical pixel at the center of window cell `(row, col)`.
fn window_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

/// Physical pixel at the center of leaf content cell `(row, col)`.
fn content_pixels(rt: &Runtime, view: ViewId, row: u16, col: u16) -> CursorPosition {
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .expect("leaf presented");
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.content.x.max(0)) + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + f64::from(frame.content.y.max(0)) + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released)
}

fn primary_frame_rows(rt: &Runtime) -> u16 {
    let primary = rt.primary_view().expect("primary owner");
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .map(|frame| frame.rows)
        .expect("primary presented")
}

fn runtime_with(edge: BarEdge, visible: bool) -> Runtime {
    Runtime::new(RuntimeConfig {
        workspace_bar_edge: edge,
        workspaceline_visible: visible,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime builds")
}

#[test]
fn two_workspaces_reserve_no_core_row_and_the_grid_keeps_it() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let window = rt.window_cells();
    let full_rows = primary_frame_rows(&rt);
    assert_eq!(rt.status_bar_band(), None, "lone workspace: no band");
    assert_eq!(rt.container(), window, "lone workspace: full window");

    rt.workspace_new().expect("ws2 reserves no Core band");
    assert_eq!(rt.status_bar_band(), None, "no Core bar with two");
    assert_eq!(
        rt.container(),
        window,
        "container keeps the full window with two workspaces"
    );
    // The primary owner lives in ws1: switch back and check its grid.
    assert!(rt.workspace_switch(0));
    assert_eq!(
        primary_frame_rows(&rt),
        full_rows,
        "leaf content rows keep every window row"
    );
    assert_eq!(
        rt.snapshot().height,
        usize::from(full_rows),
        "primary grid snapshot height follows the full leaf"
    );
}

// POSIX-only: spawns /bin/sh (same gate as `split_live_reflow.rs`).
#[cfg(unix)]
#[test]
fn no_core_band_leaves_the_primary_pty_winsize_full() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.tick().expect("first full redraw");
    rt.spawn_shell("/bin/sh").expect("primary shell must spawn");

    rt.workspace_new().expect("ws2 reserves no Core band");
    assert!(rt.workspace_switch(0));
    let snap = rt.snapshot();
    let cols = u16::try_from(snap.width).expect("u16");
    let full = u16::try_from(snap.height).expect("u16");
    assert_eq!(
        rt.pty_size(),
        Some((cols, full)),
        "kernel winsize equals the full primary grid"
    );
    // The retained visibility setting moves no Core band, so the winsize
    // is unchanged by the toggle.
    rt.set_workspaceline_visible(false);
    assert_eq!(
        rt.pty_size(),
        Some((cols, full)),
        "toggling the retired bar gives back no row"
    );
}

#[test]
fn no_core_bar_paints_over_grid_cells() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2 with no Core bar");
    assert!(rt.workspace_switch(0));
    assert_eq!(rt.status_bar_band(), None);
    // Fill every grid row with ink up to the last row.
    let rows = rt.snapshot().height;
    let mut bytes = Vec::new();
    for row in 0..rows {
        bytes.extend_from_slice(format!("\x1b[{};1HROW{row:02}", row + 1).as_bytes());
    }
    rt.handle_pty_bytes(&bytes);
    let stats = rt.tick().expect("first frame must present");
    assert!(stats.glyphs > 0);
    // The grid owns every content row: no Core band paints anywhere.
    let snap = rt.snapshot();
    // The last grid row keeps its own content: no bar paints on it.
    let last = snap.height - 1;
    let text: String = (0..5)
        .map(|col| snap.cells[last * snap.width + col].glyph)
        .collect();
    assert_eq!(
        text,
        format!("ROW{last:02}"),
        "last grid row is terminal truth"
    );
    for col in 0..snap.width {
        assert!(
            !snap.cells[last * snap.width + col].style.attributes.inverse,
            "grid truth must not carry bar styling (col {col})"
        );
    }
    // Pixel truth: the last content row paints its own `ROWxx` ink and
    // keeps the theme background past the text, where a bar row would be
    // filled edge to edge with inverse styling.
    let primary = rt.primary_view().expect("primary");
    let last_row = u16::try_from(last).expect("u16");
    assert!(
        (0..5).any(|col| cell_has_ink(&rt, content_pixels(&rt, primary, last_row, col))),
        "last content row paints its ROW{last:02} glyphs"
    );
    let width = u16::try_from(snap.width).expect("u16");
    for col in 8..width {
        assert!(
            !cell_has_ink(&rt, content_pixels(&rt, primary, last_row, col)),
            "last content row col {col} must be background, not bar fill"
        );
    }
    // Every leaf stays within the window grid in cell units: with no
    // band there is no reserved row to stay clear of, and nothing paints
    // outside the window.
    let window = rt.window_cells();
    for frame in rt.present_frames() {
        assert!(
            frame.rows <= window.height,
            "leaf frame {frame:?} must stay inside the window"
        );
    }
}

#[test]
fn workspace_count_and_show_bar_false_keep_every_window_row() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    assert_eq!(rt.status_bar_band(), None);
    assert_eq!(rt.container(), rt.window_cells());
    let full_rows = primary_frame_rows(&rt);

    let mut hidden = runtime_with(BarEdge::Bottom, false);
    hidden.workspace_new().expect("ws2");
    assert_eq!(hidden.status_bar_band(), None);
    assert_eq!(hidden.container(), hidden.window_cells());
    assert!(hidden.workspace_switch(0));
    assert_eq!(primary_frame_rows(&hidden), full_rows);
    let _ = hidden.tick();
    let last = hidden.window_cells().height - 1;
    assert!(
        !window_row_painted(&hidden, last),
        "with no Core bar the last window row paints only grid content"
    );
    let _ = rt.tick();
}

#[test]
fn visibility_toggle_and_workspace_count_never_reflow_for_core_bar() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let full = usize::from(primary_frame_rows(&rt));
    let window = rt.window_cells();
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert_eq!(
        rt.snapshot().height,
        full,
        "1 -> 2 workspaces reserves nothing"
    );
    assert_eq!(rt.container(), window);

    rt.set_workspaceline_visible(false);
    assert_eq!(
        rt.snapshot().height,
        full,
        "hiding the retired bar releases nothing"
    );
    assert_eq!(rt.container(), rt.window_cells());
    rt.set_workspaceline_visible(true);
    assert_eq!(
        rt.snapshot().height,
        full,
        "showing reserves nothing either"
    );

    rt.workspace_close_index(2).expect("close ws2");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(
        rt.snapshot().height,
        full,
        "2 -> 1 workspaces releases nothing"
    );
    assert_eq!(rt.status_bar_band(), None);
}

#[test]
fn resize_keeps_the_full_grid_without_a_core_band() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 40 + pad * 2, ch * 10 + pad * 2))
        .expect("resize");
    assert_eq!(rt.window_cells(), UiRect::new(0, 0, 40, 10));
    assert_eq!(rt.container(), UiRect::new(0, 0, 40, 10));
    assert_eq!(rt.status_bar_band(), None);
}

#[test]
fn bar_edge_setting_moves_no_core_band() {
    let mut rt = runtime_with(BarEdge::Top, true);
    let window = rt.window_cells();
    let full = usize::from(primary_frame_rows(&rt));
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert_eq!(rt.status_bar_band(), None, "top edge reserves no Core band");
    assert_eq!(rt.container(), window, "content keeps the full window");
    assert_eq!(rt.snapshot().height, full);
    let _ = rt.tick();

    // A press on window row 0 maps to content row 0, not to bar chrome.
    let primary = rt.primary_view().expect("primary");
    rt.handle_pty_bytes(b"\x1b[1;1Halpha");
    rt.handle_cursor_moved(content_pixels(&rt, primary, 0, 1));
    assert_eq!(rt.cursor_to_cell(content_pixels(&rt, primary, 0, 1)).row, 0);

    // Live edge change moves no Core band and reflows nothing.
    rt.set_workspace_bar_edge(BarEdge::Bottom);
    assert_eq!(rt.status_bar_band(), None);
    assert_eq!(rt.container(), window);
    assert_eq!(rt.snapshot().height, full);
}

#[test]
fn press_on_the_last_window_row_is_terminal_content_not_chrome() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    // Press on the primary owner's grid (the state-backed leaf): the last
    // content row is grid content now (no Core band below it), so a
    // press-drag there selects instead of switching workspaces. Content
    // coordinates come from the presented frame so decoration offsets
    // cannot push the press into the window ring.
    assert!(rt.workspace_switch(0));
    let focused = rt.focused_view().expect("focused");
    let rows = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == focused)
        .map(|frame| frame.rows)
        .expect("frame");
    rt.handle_cursor_moved(content_pixels(&rt, focused, rows - 1, 0));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(content_pixels(&rt, focused, rows - 1, 5));
    rt.handle_mouse_input(release());
    assert_eq!(
        rt.active_workspace_index(),
        0,
        "content press switches no workspace"
    );
    assert!(rt.has_selection(), "the last row belongs to selection");
    assert!(
        rt.drain_band_clicks().is_empty(),
        "no plugin band mounted: nothing routes"
    );
}

#[test]
fn top_edge_press_on_row_zero_is_grid_content() {
    let mut rt = runtime_with(BarEdge::Top, true);
    rt.workspace_new().expect("ws2");
    rt.workspace_new().expect("ws3");
    // Press on the primary owner's grid (the state-backed leaf): row 0 is
    // grid content with no Core bar, so a press-drag selects instead of
    // switching workspaces.
    assert!(rt.workspace_switch(0));
    assert_eq!(rt.workspaceline_text(), "1:ws1* 2:ws2 3:ws3 (3)");
    let focused = rt.focused_view().expect("focused");
    rt.handle_cursor_moved(content_pixels(&rt, focused, 0, 8));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(content_pixels(&rt, focused, 0, 12));
    rt.handle_mouse_input(release());
    assert_eq!(
        rt.active_workspace_index(),
        0,
        "content press switches no workspace"
    );
    assert!(rt.has_selection(), "row 0 belongs to selection");
}

#[test]
fn press_on_the_last_grid_row_is_terminal_content_not_chrome() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    let focused = rt.focused_view().expect("focused");
    let rows = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == focused)
        .map(|frame| frame.rows)
        .expect("frame");
    rt.handle_cursor_moved(content_pixels(&rt, focused, rows - 1, 0));
    rt.handle_mouse_input(press());
    assert_eq!(
        rt.active_workspace_index(),
        1,
        "the last content row is no longer bar chrome"
    );
}

#[test]
fn tiny_window_keeps_content_without_a_core_band() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch + pad * 2))
        .expect("resize to one row");
    assert_eq!(rt.window_cells().height, 1);
    assert_eq!(rt.status_bar_band(), None, "no Core band at any size");
    assert_eq!(rt.container(), rt.window_cells());
    assert!(rt.snapshot().height >= 1);
    let _ = rt.tick();
}

#[test]
fn short_window_with_default_decoration_keeps_the_full_grid() {
    // CTX-0873: the effective-minimum-content floor stays (it also gates
    // the plugin-band exclusive zone), but with no Core bar a short window
    // keeps every row for content instead of hiding a band.
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    assert_eq!(rt.decoration(), bitty_ui::Decoration::default());
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0), "primary owner lives in ws1");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch * 2 + pad * 2))
        .expect("resize to two rows");
    assert_eq!(rt.window_cells().height, 2);
    assert_eq!(rt.status_bar_band(), None, "no Core band at two rows");
    assert_eq!(rt.container(), rt.window_cells());
    let _ = rt.tick();

    // Growing changes nothing for Core: still no band, still full grid.
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch * 8 + pad * 2))
        .expect("grow");
    assert_eq!(rt.status_bar_band(), None);
    assert_eq!(rt.container(), rt.window_cells());
    let primary = rt.primary_view().expect("primary");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .expect("primary presented");
    assert!(frame.content.height >= ch, "at least one real content row");
}

#[test]
fn quiet_workspace_switch_and_rename_present_without_a_core_bar() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
    assert!(
        rt.tick().is_some(),
        "allocation/focus change must force a present on a quiet grid"
    );
    assert_eq!(rt.status_bar_band(), None, "no Core band paints the switch");
    // A rename with a quiet grid also presents (redraw flag, not a bar
    // text signal), and the data string follows.
    let _ = rt.tick();
    rt.workspace_rename(0, "alpha").expect("rename");
    assert!(rt.tick().is_some(), "rename forces a frame");
    assert_eq!(rt.workspaceline_text(), "1:alpha 2:ws2* (2)");
}

#[test]
fn no_core_bar_paints_on_the_alternate_screen() {
    // A fullscreen app owns every row of its grid; with the Core bar
    // retired nothing paints outside that grid on the alternate screen.
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    let _ = rt.tick();
    assert_eq!(rt.status_bar_band(), None);
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H\x1b[2J");
    let _ = rt.tick();
    let last = rt.window_cells().height - 1;
    assert!(
        !window_row_painted(&rt, last),
        "no Core bar paints over alt screen"
    );
    // A press on the last row is grid content: it switches no workspace.
    rt.handle_cursor_moved(window_pixels(&rt, last, 0));
    rt.handle_mouse_input(press());
    assert_eq!(rt.active_workspace_index(), 1);
}

#[test]
fn last_row_press_focuses_the_clicked_pane_not_chrome() {
    // With no Core bar there is no chrome to consume the press first, even
    // under mouse tracking: a press on the last content row focuses the
    // clicked pane (standard focus-follows-click), switches no workspace,
    // and arms no band gesture. Plugin-band capture ordering stays pinned
    // in `host_chrome_gaps.rs` (C1).
    let view_a = ViewId::new(1);
    let view_b = ViewId::new(2);
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(view_a, 80, 24)),
        LayoutNode::leaf(View::new(view_b, 80, 24)),
    ));
    rt.workspace_new().expect("ws2 with no Core bar");
    assert!(rt.workspace_switch(0));
    assert!(rt.set_focus(view_a), "pane A must be focusable");
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();

    // A content cell of pane B's last row: the press must reach terminal
    // focus logic, never bar chrome.
    let rows_b = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == view_b)
        .map(|frame| frame.rows)
        .expect("pane B presented");
    rt.handle_cursor_moved(content_pixels(&rt, view_b, rows_b - 1, 1));
    rt.handle_mouse_input(press());
    assert_eq!(
        rt.focused_view(),
        Some(view_b),
        "content press focuses the clicked pane"
    );
    assert_eq!(
        rt.active_workspace_index(),
        0,
        "content press switches no workspace"
    );
    assert!(!rt.band_release_armed(), "no band gesture arms");
    rt.handle_mouse_input(release());
    assert_eq!(rt.focused_view(), Some(view_b));
    assert!(
        rt.drain_band_clicks().is_empty(),
        "no plugin band mounted: nothing routes"
    );
}

#[test]
fn split_border_drag_without_a_top_band_uses_the_window_origin() {
    // `cursor_to_layout_point` (border drag) and the leaf
    // hit-test share one origin (window cells) with no band offset, so a
    // divider grab maps through the undecorated grid origin.
    let view_a = ViewId::new(1);
    let view_b = ViewId::new(2);
    let mut rt = runtime_with(BarEdge::Top, true);
    rt.set_layout(LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(view_a, 80, 24)),
        LayoutNode::leaf(View::new(view_b, 80, 24)),
    ));
    rt.workspace_new().expect("ws2 with no Core bar");
    assert!(rt.workspace_switch(0));
    let container = rt.container();
    assert_eq!(container.y, 0, "no top band shifts the container");
    let rect_of = |rt: &Runtime, id: ViewId| {
        rt.layout_allocations()
            .into_iter()
            .find(|(view, _)| *view == id)
            .map(|(_, rect)| rect)
            .expect("leaf allocated")
    };
    let top = rect_of(&rt, view_a);
    let bottom = rect_of(&rt, view_b);
    assert_eq!(top.y, 0, "first leaf starts at the window origin");
    let divider = bottom.y;
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    let at = |col: u16, row: u16| CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };

    // Row 0 is pane A content row 0 through the leaf hit-test.
    assert_eq!(
        rt.cursor_to_leaf_cell(at(40, top.y))
            .map(|(id, cell)| (id, cell.row)),
        Some((view_a, 0)),
        "row 0 is pane A content row 0"
    );
    // The divider row resolves to the vertical split and drags over the
    // full-height container.
    assert_eq!(
        rt.border_drag_hover_at(at(40, divider)),
        Some(SplitAxis::Vertical)
    );

    let before = rt.layout().split_ratio_at(&[]).expect("ratio");
    rt.handle_cursor_moved(at(40, divider));
    rt.handle_mouse_input(press());
    assert!(rt.border_drag_active(), "divider grabs with no top band");
    rt.handle_cursor_moved(at(40, divider + 4));
    let after = rt.layout().split_ratio_at(&[]).expect("ratio");
    let expected = before + 4.0 / f32::from(container.height);
    assert!(
        (after - expected).abs() < 1e-6,
        "+4 rows over the {}-row container: {before} -> {after}, want {expected}",
        container.height
    );
    rt.handle_mouse_input(release());
    assert!(!rt.border_drag_active());
    assert_eq!(rect_of(&rt, view_a).y, 0, "drag keeps the window origin");
}
