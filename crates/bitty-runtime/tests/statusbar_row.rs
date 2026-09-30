#![forbid(unsafe_code)]
//! Workspace bar chrome band (issue #1349 drawn row; CTX-0873 / #1431
//! Core-reserved band).
//!
//! #1349 first drew the bar by overlaying the last row of every leaf's
//! present snapshot, which occluded terminal content. CTX-0873 moves it into
//! a Core-owned chrome band carved out of the window grid before layout.
//! These tests pin that headlessly (no window, adapter, or display server):
//!
//! - with two workspaces the leaf and primary grid rows equal the window
//!   rows minus the band (snapshot height; the kernel PTY winsize is pinned
//!   by a POSIX live-shell test), and no grid cell is painted under the bar;
//! - with one workspace or `show_bar = false` the grid uses every window row;
//! - toggling visibility, crossing the one/two workspace boundary, and
//!   changing the edge reflow the grid immediately;
//! - `workspace.bar.edge = "top"` puts the band on row 0 and shifts content;
//! - a press on the band switches workspaces via the column hit-test and is
//!   consumed as chrome (no selection, no capture, no focus move);
//! - a window too small for band plus content hides the band;
//! - the bar stays visible on the alternate screen (it is outside the grid);
//! - a split-border drag under a top band maps the pointer through the
//!   same origin as the leaf hit-test (band row owns no divider).

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
    // a painted bar fills the whole cell.
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

/// Physical pixel at the center of band column `col`.
fn band_pixels(rt: &Runtime, col: u16) -> CursorPosition {
    let band = rt.status_bar_band().expect("band reserved");
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(band.x) + f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(band.y) + 0.5) * f64::from(ch),
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
fn two_workspaces_reserve_one_window_row_and_the_grid_excludes_it() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let window = rt.window_cells();
    let full_rows = primary_frame_rows(&rt);
    assert_eq!(rt.container(), window, "lone workspace: no band");

    rt.workspace_new().expect("ws2 reserves the band");
    assert_eq!(
        rt.container(),
        UiRect::new(0, 0, window.width, window.height - 1),
        "container = window minus the bottom band"
    );
    assert_eq!(
        rt.status_bar_band(),
        Some(UiRect::new(0, window.height - 1, window.width, 1))
    );
    // The primary owner lives in ws1: switch back and check its grid.
    assert!(rt.workspace_switch(0));
    assert_eq!(
        primary_frame_rows(&rt),
        full_rows - 1,
        "leaf content rows lose exactly the band row"
    );
    assert_eq!(
        rt.snapshot().height,
        usize::from(full_rows - 1),
        "primary grid snapshot height follows the reduced leaf"
    );
}

// POSIX-only: spawns /bin/sh (same gate as `split_live_reflow.rs`).
#[cfg(unix)]
#[test]
fn band_reservation_resizes_the_primary_pty_winsize() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.tick().expect("first full redraw");
    rt.spawn_shell("/bin/sh").expect("primary shell must spawn");

    rt.workspace_new().expect("ws2 reserves the band");
    assert!(rt.workspace_switch(0));
    let snap = rt.snapshot();
    let cols = u16::try_from(snap.width).expect("u16");
    let banded = u16::try_from(snap.height).expect("u16");
    assert_eq!(
        rt.pty_size(),
        Some((cols, banded)),
        "kernel winsize equals the band-reduced primary grid"
    );
    rt.set_workspaceline_visible(false);
    assert_eq!(
        rt.pty_size(),
        Some((cols, banded + 1)),
        "hiding the bar gives the row back to the PTY"
    );
}

#[test]
fn bar_paints_only_inside_the_band_and_never_over_grid_cells() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2 for bar");
    assert!(rt.workspace_switch(0));
    // Fill every grid row with ink up to the last row.
    let rows = rt.snapshot().height;
    let mut bytes = Vec::new();
    for row in 0..rows {
        bytes.extend_from_slice(format!("\x1b[{};1HROW{row:02}", row + 1).as_bytes());
    }
    rt.handle_pty_bytes(&bytes);
    let stats = rt.tick().expect("first frame must present");
    assert!(stats.glyphs > 0);
    let band = rt.status_bar_band().expect("band reserved");
    assert!(window_row_painted(&rt, band.y), "bar paints in its band");
    // The last grid row keeps its own content: the bar is not on it.
    let snap = rt.snapshot();
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
    // Every leaf content frame ends at or above the band's pixel top.
    let (_, ch) = rt.live_cell_size();
    let band_top = u32::from(band.y) * ch;
    for frame in rt.present_frames() {
        let bottom = u32::try_from(frame.frame.y.max(0)).expect("u32") + frame.frame.height;
        assert!(
            bottom <= band_top,
            "leaf frame {frame:?} must not reach into the band at {band_top}px"
        );
    }
}

#[test]
fn lone_workspace_and_show_bar_false_keep_every_window_row() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    assert_eq!(rt.status_bar_band(), None);
    assert_eq!(rt.container(), rt.window_cells());
    let full_rows = primary_frame_rows(&rt);

    let mut hidden = runtime_with(BarEdge::Bottom, false);
    hidden.workspace_new().expect("ws2");
    assert_eq!(hidden.status_bar_text(), None);
    assert_eq!(hidden.status_bar_band(), None);
    assert_eq!(hidden.container(), hidden.window_cells());
    assert!(hidden.workspace_switch(0));
    assert_eq!(primary_frame_rows(&hidden), full_rows);
    let _ = hidden.tick();
    let last = hidden.window_cells().height - 1;
    assert!(
        !window_row_painted(&hidden, last),
        "opted-out bar must leave the last window row background-clean"
    );
    let _ = rt.tick();
}

#[test]
fn toggling_visibility_and_workspace_count_reflow_the_grid() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let full = usize::from(primary_frame_rows(&rt));
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert_eq!(rt.snapshot().height, full - 1, "1 -> 2 workspaces reserves");

    rt.set_workspaceline_visible(false);
    assert_eq!(
        rt.snapshot().height,
        full,
        "hiding releases the band at once"
    );
    assert_eq!(rt.container(), rt.window_cells());
    rt.set_workspaceline_visible(true);
    assert_eq!(rt.snapshot().height, full - 1, "showing reserves it again");

    rt.workspace_close_index(2).expect("close ws2");
    assert_eq!(rt.workspace_count(), 1);
    assert_eq!(rt.snapshot().height, full, "2 -> 1 workspaces releases");
    assert_eq!(rt.status_bar_band(), None);
}

#[test]
fn resize_keeps_the_band_reserved() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 40 + pad * 2, ch * 10 + pad * 2))
        .expect("resize");
    assert_eq!(rt.window_cells(), UiRect::new(0, 0, 40, 10));
    assert_eq!(rt.container(), UiRect::new(0, 0, 40, 9));
    assert_eq!(rt.status_bar_band(), Some(UiRect::new(0, 9, 40, 1)));
}

#[test]
fn top_edge_puts_the_band_on_row_zero_and_shifts_content() {
    let mut rt = runtime_with(BarEdge::Top, true);
    let window = rt.window_cells();
    let full = usize::from(primary_frame_rows(&rt));
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    assert_eq!(
        rt.status_bar_band(),
        Some(UiRect::new(0, 0, window.width, 1))
    );
    assert_eq!(
        rt.container(),
        UiRect::new(0, 1, window.width, window.height - 1)
    );
    assert_eq!(rt.snapshot().height, full - 1);
    let (_, ch) = rt.live_cell_size();
    let primary = rt.primary_view().expect("primary");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .expect("primary presented");
    assert!(
        u32::try_from(frame.frame.y).expect("non-negative") >= ch,
        "content frame starts below the top band"
    );
    let _ = rt.tick();
    assert!(window_row_painted(&rt, 0), "bar paints on window row 0");

    // A press on grid row 0 maps to content row 0, not to the band.
    rt.handle_pty_bytes(b"\x1b[1;1Halpha");
    rt.handle_cursor_moved(content_pixels(&rt, primary, 0, 1));
    assert_eq!(rt.cursor_to_cell(content_pixels(&rt, primary, 0, 1)).row, 0);

    // Live edge change moves the band and reflows without changing rows.
    rt.set_workspace_bar_edge(BarEdge::Bottom);
    assert_eq!(
        rt.status_bar_band(),
        Some(UiRect::new(0, window.height - 1, window.width, 1))
    );
    assert_eq!(rt.container().y, 0);
    assert_eq!(rt.snapshot().height, full - 1);
}

#[test]
fn band_press_switches_workspace_and_is_consumed_as_chrome() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    assert_eq!(rt.active_workspace_index(), 1);
    // CTX-0874 pills: ws1 covers columns 1..6, ws2 covers 7..12.
    rt.handle_cursor_moved(band_pixels(&rt, 1));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
    assert_eq!(rt.active_workspace_index(), 0, "pill padding hits ws1");
    assert!(!rt.has_selection(), "the band is chrome: no selection");
    rt.handle_cursor_moved(band_pixels(&rt, 11));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
    assert_eq!(rt.active_workspace_index(), 1);
    // The gap between pills switches nothing but is still consumed.
    rt.handle_cursor_moved(band_pixels(&rt, 6));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
    assert_eq!(rt.active_workspace_index(), 1);
    assert!(!rt.has_selection());
}

#[test]
fn top_band_press_maps_to_the_right_workspace() {
    let mut rt = runtime_with(BarEdge::Top, true);
    rt.workspace_new().expect("ws2");
    rt.workspace_new().expect("ws3");
    assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2 3:ws3* (3)");
    // CTX-0874: the ws2 pill covers band columns 7..12 on the top edge too.
    rt.handle_cursor_moved(band_pixels(&rt, 8));
    rt.handle_mouse_input(press());
    assert_eq!(rt.active_workspace_index(), 1, "column 8 hits the ws2 pill");
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
fn tiny_window_hides_the_band_instead_of_zero_content_rows() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch + pad * 2))
        .expect("resize to one row");
    assert_eq!(rt.window_cells().height, 1);
    assert_eq!(rt.status_bar_band(), None, "no room: band hidden");
    assert_eq!(rt.container(), rt.window_cells());
    assert!(rt.snapshot().height >= 1);
    let _ = rt.tick();
}

#[test]
fn two_row_window_with_default_decoration_hides_the_band() {
    // CTX-0873: the floor is the effective minimum content, not a bare
    // container row: `MIN_CONTENT_ROWS` plus both outer cell gaps plus the
    // default decoration ring (gaps_out + border + content_inset on both
    // sides) in live rows. At two window rows that ring would leave the
    // leaf no content row under a band, so the band stays hidden.
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    assert_eq!(rt.decoration(), bitty_ui::Decoration::default());
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0), "primary owner lives in ws1");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch * 2 + pad * 2))
        .expect("resize to two rows");
    assert_eq!(rt.window_cells().height, 2);
    assert_eq!(
        rt.status_bar_band(),
        None,
        "decoration eats the content row"
    );
    assert_eq!(rt.container(), rt.window_cells());
    let _ = rt.tick();

    // Growing until the band fits reserves it and leaves a content row.
    let fits = (3..=16u32)
        .find(|rows| {
            rt.handle_resize(PhysicalSize::new(cw * 20 + pad * 2, ch * rows + pad * 2))
                .expect("grow");
            rt.status_bar_band().is_some()
        })
        .expect("a modest window reserves the band");
    let band = rt.status_bar_band().expect("band");
    assert_eq!(u32::from(band.y), fits - 1, "bottom band on the last row");
    let primary = rt.primary_view().expect("primary");
    let frame = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .expect("primary presented");
    assert!(frame.content.height >= ch, "at least one real content row");
}

#[test]
fn quiet_workspace_switch_still_presents_the_new_bar() {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    let _ = rt.tick();
    rt.workspace_new().expect("ws2");
    assert_eq!(rt.status_bar_text().as_deref(), Some("1:ws1 2:ws2* (2)"));
    let stats = rt
        .tick()
        .expect("bar-text change must force a present on a quiet grid");
    assert!(stats.glyphs > 0);
    let band = rt.status_bar_band().expect("band");
    assert!(window_row_painted(&rt, band.y));
    // A rename with a quiet grid also presents (text-change damage).
    let _ = rt.tick();
    rt.workspace_rename(0, "alpha").expect("rename");
    assert!(rt.tick().is_some(), "bar text change forces a frame");
}

#[test]
fn bar_stays_visible_on_the_alternate_screen() {
    // CTX-0873: the old in-grid overlay hid on the alternate screen because
    // a fullscreen app owns every row of its grid. The band now sits outside
    // that grid, so the bar stays visible and clickable over vim/htop.
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.workspace_new().expect("ws2");
    let _ = rt.tick();
    let band = rt.status_bar_band().expect("band");
    rt.handle_pty_bytes(b"\x1b[?1049h\x1b[H\x1b[2J");
    let _ = rt.tick();
    assert!(
        window_row_painted(&rt, band.y),
        "bar paints over alt screen"
    );
    rt.handle_cursor_moved(band_pixels(&rt, 2));
    rt.handle_mouse_input(press());
    assert_eq!(rt.active_workspace_index(), 0, "band click works on alt");
}

#[test]
fn band_press_with_a_mouse_tracking_app_is_chrome_before_capture() {
    // CTX-0808 (#1484) intent under CTX-0873: a bar press never reaches a
    // capturing app, never moves focus, and its paired release is
    // swallowed. With the band outside every frame this holds for every
    // pane, focused or not.
    let view_a = ViewId::new(1);
    let view_b = ViewId::new(2);
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(view_a, 80, 24)),
        LayoutNode::leaf(View::new(view_b, 80, 24)),
    ));
    rt.workspace_new().expect("ws2 for the bar");
    assert!(rt.workspace_switch(0));
    assert!(rt.set_focus(view_a), "pane A must be focusable");
    rt.handle_pty_bytes(b"\x1b[?1000h");
    rt.drain_pending_input();

    // Column under pane B's half of the window: the band spans both panes.
    let band = rt.status_bar_band().expect("band");
    rt.handle_cursor_moved(band_pixels(&rt, band.width - 2));
    rt.handle_mouse_input(press());
    assert_eq!(rt.focused_view(), Some(view_a), "no focus move");
    assert!(rt.pending_input().is_empty(), "no report to the app");
    assert!(!rt.has_selection());
    rt.handle_mouse_input(release());
    assert_eq!(rt.focused_view(), Some(view_a));
    assert!(rt.pending_input().is_empty(), "paired release swallowed");
}

#[test]
fn top_band_split_border_drag_maps_through_the_leaf_origin() {
    // CTX-0873: `cursor_to_layout_point` (border drag) and the leaf
    // hit-test share one origin (window cells), and allocations carry the
    // band offset, so a top band must not shift the grabbed divider.
    let view_a = ViewId::new(1);
    let view_b = ViewId::new(2);
    let mut rt = runtime_with(BarEdge::Top, true);
    rt.set_layout(LayoutNode::split(
        SplitAxis::Vertical,
        0.5,
        LayoutNode::leaf(View::new(view_a, 80, 24)),
        LayoutNode::leaf(View::new(view_b, 80, 24)),
    ));
    rt.workspace_new().expect("ws2 for the bar");
    assert!(rt.workspace_switch(0));
    let container = rt.container();
    assert_eq!(container.y, 1, "top band shifts the container");
    let rect_of = |rt: &Runtime, id: ViewId| {
        rt.layout_allocations()
            .into_iter()
            .find(|(view, _)| *view == id)
            .map(|(_, rect)| rect)
            .expect("leaf allocated")
    };
    let top = rect_of(&rt, view_a);
    let bottom = rect_of(&rt, view_b);
    assert_eq!(top.y, 1, "first leaf starts below the band");
    let divider = bottom.y;
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    let at = |col: u16, row: u16| CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    };

    // The band row owns no divider: a press there is bar chrome.
    assert_eq!(rt.border_drag_hover_at(at(40, 0)), None);
    // The divider row resolves to the vertical split, and one row above it
    // resolves to pane A's last row through the leaf hit-test.
    assert_eq!(
        rt.border_drag_hover_at(at(40, divider)),
        Some(SplitAxis::Vertical)
    );
    assert_eq!(
        rt.cursor_to_leaf_cell(at(40, top.y))
            .map(|(id, cell)| (id, cell.row)),
        Some((view_a, 0)),
        "row 1 is pane A content row 0"
    );

    let before = rt.layout().split_ratio_at(&[]).expect("ratio");
    rt.handle_cursor_moved(at(40, divider));
    rt.handle_mouse_input(press());
    assert!(rt.border_drag_active(), "divider under a top band grabs");
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
    assert_eq!(rect_of(&rt, view_a).y, 1, "drag keeps the band offset");
}

/// RGBA of the pixel at the center of band column `col`.
fn band_pixel(rt: &Runtime, col: u16) -> [u8; 4] {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let pos = band_pixels(rt, col);
    probe(&rgba, width, pos.x as usize, pos.y as usize)
}

#[test]
fn pills_paint_distinct_active_and_inactive_backgrounds_on_either_edge() {
    // CTX-0874: each workspace is a filled pill; the active pill color
    // differs from inactive ones; the margin and gaps keep the ground.
    // Pill padding cells (no glyph) are sampled so text ink never interferes.
    for edge in [BarEdge::Bottom, BarEdge::Top] {
        let mut rt = runtime_with(edge, true);
        rt.workspace_new().expect("ws2");
        let _ = rt.tick();
        let band = rt.status_bar_band().expect("band");
        if edge == BarEdge::Top {
            assert_eq!(band.y, 0, "top band on row 0");
        }
        let bg = bitty_render::grid::DEFAULT_BG;
        let inactive = band_pixel(&rt, 1);
        let active = band_pixel(&rt, 7);
        assert_ne!(inactive, bg, "{edge:?}: inactive pill is filled");
        assert_ne!(active, bg, "{edge:?}: active pill is filled");
        assert_ne!(active, inactive, "{edge:?}: active pill is distinct");
        assert_eq!(band_pixel(&rt, 0), bg, "{edge:?}: left margin");
        assert_eq!(band_pixel(&rt, 6), bg, "{edge:?}: gap");
        assert_eq!(
            band_pixel(&rt, band.width - 1),
            bg,
            "{edge:?}: right region"
        );
        // Switching moves the active color to the other pill.
        assert!(rt.workspace_switch(0));
        let _ = rt.tick();
        assert_eq!(band_pixel(&rt, 1), active, "{edge:?}: ws1 now active");
        assert_eq!(band_pixel(&rt, 7), inactive, "{edge:?}: ws2 now inactive");
    }
}
