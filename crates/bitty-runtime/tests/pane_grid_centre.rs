#![forbid(unsafe_code)]
//! bitty#3284 (CTX-1920): the floored pane grid is centred inside its
//! content rect instead of pooling the whole sub-cell remainder below
//! the last row.
//!
//! The geometry under test: the window grid floors the padded window to
//! whole cells (`(surface - 2 * padding) / cell`), the decoration then
//! carves `2 * (gaps_out + border + content_inset)` = 26 px out of that
//! frame, and the content grid floors again. The sub-cell leftover is
//! therefore a function of the CELL HEIGHT — the window size only moves
//! the whole-row count — which is why the issue demands the margins at
//! "every cell height where the content box is not a whole number of
//! rows". At the shipped defaults (736x472 window, 8px padding, 9x19
//! cells) the content box is 694x430: a 77x22 grid with 1 px of column
//! leftover and 12 px of row leftover, and every paint/cursor/selection
//! origin sat at the content top-left, so the 12 px landed entirely
//! below the last row (first row top margin 7 px against the frame,
//! last row bottom margin 19 px).
//!
//! The fix splits the leftover evenly per axis: the first painted row's
//! top margin and the last row's bottom margin stay within one pixel,
//! and the same rule centres the horizontal leftover (1 px at the
//! shipped defaults floors to a 0 px split, so that placement is
//! unchanged). An exact-multiple content box keeps the grid flush with
//! the inset.
//!
//! Every margin is measured on the composited headless frame
//! ([`Runtime::headless_rgba`]) against the content box
//! ([`Runtime::present_frames`] supplies the reference, unchanged by the
//! fix: the content rect keeps its symmetric inset, only the floored
//! grid's placement inside it moves). The whole grid is selected
//! (cursor hidden first) and selection fills are exact cell rectangles
//! — the same paint the `selection` golden pins — so the first/last
//! selected row and column are the painted grid's extent; the interior
//! row probes avoid the outline ring.
//! The floored state-grid dims (the PTY winsize source) are pinned per
//! case from an independent derivation, so a sizing regression fails
//! alongside a placement one.
//!
//! Before the fix, `row_leftover_is_split_above_and_below_the_grid`
//! fails for every leftover >= 2 with the top margin `0` and the bottom
//! margin the full leftover, `horizontal_leftover_is_centred_by_the_same_rule`
//! fails its 2 px case the same way, and
//! `pointer_resolves_the_cell_whose_band_contains_it` fails the
//! one-pixel-above-the-row-edge probes (they resolved the row below).
//! The exact-multiple control and the in-cell pointer probes pass on
//! both trees.
//!
//! The vertical sweep uses cell heights 27..=44 at the fixed default
//! window, which walks the row leftover through 1..=18 — the complete
//! set of non-multiple cases for any cell height, since only the
//! leftover, not the absolute height, moves the grid.

use bitty_platform::{CursorPosition, PhysicalSize};
use bitty_runtime::{Runtime, RuntimeConfig, ViewId};
use bitty_ui::CellPos;

/// Shipped-default window extent (736x472 physical px).
const WINDOW: (u32, u32) = (736, 472);
/// Window padding inset (8 physical px, both sides).
const PAD: u32 = 8;
/// Decoration carved from the window-grid frame per axis:
/// `2 * (gaps_out 6 + border 1 + content_inset 6)` = 26 px.
const DECORATION: u32 = 26;
/// Shipped-default cell (9x19 physical px).
const CELL_W: u32 = 9;
const CELL_H: u32 = 19;

/// A runtime whose glyph rasterizer is deterministic (no host font
/// stack), so the painted frame is byte-stable across hosts.
fn runtime_with_cell(cell_w: u32, cell_h: u32) -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        cell_width: cell_w,
        cell_height: cell_h,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build")
}

/// Window-grid cells on one axis: the padded window floored to whole
/// cells.
fn window_cells(surface: u32, cell: u32) -> u32 {
    (surface - 2 * PAD) / cell
}

/// Content-grid cells on one axis: the window grid shrunk by the
/// decoration, floored to whole cells.
fn content_cells(window_cells: u32, cell: u32) -> u32 {
    (window_cells * cell - DECORATION) / cell
}

/// Sub-cell leftover on one axis: what the second floor leaves between
/// the content box and the whole cells it admits.
fn leftover(window_cells: u32, cell: u32) -> u32 {
    window_cells * cell - DECORATION - content_cells(window_cells, cell) * cell
}

/// RGBA pixel at `(x, y)` of the last presented headless frame.
fn pixel(buf: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let start = (y * width + x) * 4;
    let slice = buf.get(start..start + 4).expect("pixel inside frame");
    [slice[0], slice[1], slice[2], slice[3]]
}

/// Physical rows in `[y0, y1)` carrying `color` inside `[x0, x1)`.
fn rows_with_color(
    buf: &[u8],
    width: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    color: [u8; 4],
) -> Vec<usize> {
    (y0..y1)
        .filter(|y| (x0..x1).any(|x| pixel(buf, width, x, *y) == color))
        .collect()
}

/// Physical columns in `[x0, x1)` carrying `color` inside `[y0, y1)`.
fn cols_with_color(
    buf: &[u8],
    width: usize,
    y0: usize,
    y1: usize,
    x0: usize,
    x1: usize,
    color: [u8; 4],
) -> Vec<usize> {
    (x0..x1)
        .filter(|x| (y0..y1).any(|y| pixel(buf, width, *x, y) == color))
        .collect()
}

/// Presents the whole primary grid selected, and returns the composited
/// frame with the resolved selection fill color.
///
/// Selection fills are exact cell rectangles (the same paint the
/// `selection` golden pins), so the first/last selected row and column
/// are the painted grid's extent. The cursor is hidden first (DECSET 25)
/// so its fill cannot punch a hole into the selection color; the
/// content-box centre is inside the painted grid for every cell the
/// fixtures use (the split is at most half a cell), so it resolves the
/// fill color without assuming where the grid sits.
fn present_selected_grid(rt: &mut Runtime) -> (Vec<u8>, usize, [u8; 4]) {
    rt.handle_pty_bytes(b"\x1b[?25l");
    rt.tick().expect("the cursor hide must present");
    rt.select_all();
    rt.tick().expect("the selection must present");
    let frame = rt.present_frames()[0];
    let buf = rt.headless_rgba().expect("headless frame after tick");
    let width =
        usize::try_from(rt.surface_extent().expect("surface").width()).expect("width fits usize");
    let (window_x, window_y) = rt.window_grid_origin();
    let centre_x = usize::try_from(frame.content.x).expect("content x fits")
        + window_x as usize
        + frame.content.width as usize / 2;
    let centre_y = usize::try_from(frame.content.y).expect("content y fits")
        + window_y as usize
        + frame.content.height as usize / 2;
    let fill = pixel(&buf, width, centre_x, centre_y);
    (buf, width, fill)
}

#[test]
fn row_leftover_is_split_above_and_below_the_grid() {
    // Cell heights 27..=44 walk the row leftover 1..=18 at the shipped
    // window: leftover = cell - 26 while the cell stays below two rows
    // of decoration plus one row of grid.
    for cell_h in 27..=44u32 {
        let rem = leftover(window_cells(WINDOW.1, cell_h), cell_h);
        assert_eq!(rem, cell_h - DECORATION, "fixture must sweep 1..=18");
        let rows = content_cells(window_cells(WINDOW.1, cell_h), cell_h);
        let mut rt = runtime_with_cell(CELL_W, cell_h);
        rt.handle_resize(PhysicalSize::new(WINDOW.0, WINDOW.1))
            .expect("default window must reflow");
        let frame = rt.present_frames()[0];
        let (buf, width, fill) = present_selected_grid(&mut rt);
        // The state grid is the PTY winsize source: the centring is
        // paint-only, so the floored dims never move.
        let snap = rt.snapshot();
        assert_eq!(
            (snap.width, snap.height),
            (
                content_cells(window_cells(WINDOW.0, CELL_W), CELL_W) as usize,
                rows as usize
            ),
            "cell_h {cell_h}: the state grid must keep its floored dims"
        );
        let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
        let content_x = usize::try_from(frame.content.x).expect("content x fits") + pad;
        let slack = (WINDOW.1 - 2 * PAD) % cell_h;
        let content_y =
            usize::try_from(frame.content.y).expect("content y fits") + pad + (slack / 2) as usize;
        let content_h = frame.content.height as usize;
        // Interior column probe: away from the outline ring and both
        // horizontal leftover stripes.
        let probe_x0 = content_x + 10 * CELL_W as usize;
        let probe_x1 = probe_x0 + 3 * CELL_W as usize;
        let painted = rows_with_color(&buf, width, probe_x0, probe_x1, 0, WINDOW.1 as usize, fill);
        let first = *painted
            .first()
            .unwrap_or_else(|| panic!("cell_h {cell_h}: no painted row found"));
        let last = *painted
            .last()
            .unwrap_or_else(|| panic!("cell_h {cell_h}: no painted row found"));
        let top = first - content_y;
        let bottom = content_y + content_h - 1 - last;
        assert_eq!(
            top,
            (rem / 2) as usize,
            "cell_h {cell_h}: half the leftover must sit above the first row"
        );
        assert_eq!(
            bottom,
            (rem - rem / 2) as usize,
            "cell_h {cell_h}: the rest of the leftover must sit below the last row"
        );
        assert_eq!(
            last - first,
            rows as usize * cell_h as usize - 1,
            "cell_h {cell_h}: the painted span is exactly the whole rows"
        );
    }
}

#[test]
fn horizontal_leftover_is_centred_by_the_same_rule() {
    // A 28 px cell leaves 2 px of column leftover: 1 px each side. The
    // shipped-default 9 px cell leaves 1 px, which floors to a 0 px
    // split, so today's left-anchored placement (left 7, right 8 against
    // the frame) is unchanged.
    for (cell_w, left, right) in [(28u32, 1u32, 1u32), (CELL_W, 0, 1)] {
        let rem = leftover(window_cells(WINDOW.0, cell_w), cell_w);
        let cols = content_cells(window_cells(WINDOW.0, cell_w), cell_w);
        let mut rt = runtime_with_cell(cell_w, CELL_H);
        rt.handle_resize(PhysicalSize::new(WINDOW.0, WINDOW.1))
            .expect("default window must reflow");
        let frame = rt.present_frames()[0];
        let (buf, width, fill) = present_selected_grid(&mut rt);
        let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
        let content_x = usize::try_from(frame.content.x).expect("content x fits") + pad;
        let content_w = frame.content.width as usize;
        let painted = cols_with_color(
            &buf,
            width,
            0,
            WINDOW.1 as usize,
            0,
            WINDOW.0 as usize,
            fill,
        );
        let first = *painted
            .first()
            .unwrap_or_else(|| panic!("cell_w {cell_w}: no painted column found"));
        let last = *painted
            .last()
            .unwrap_or_else(|| panic!("cell_w {cell_w}: no painted column found"));
        assert_eq!(
            first - content_x,
            left as usize,
            "cell_w {cell_w}: the horizontal split must sit left of the grid"
        );
        assert_eq!(
            content_x + content_w - 1 - last,
            right as usize,
            "cell_w {cell_w}: the rest of the split must sit right of the grid"
        );
        assert_eq!(
            last - first,
            cols as usize * cell_w as usize - 1,
            "cell_w {cell_w}: the painted span is exactly the whole columns"
        );
        let _ = rem;
    }
}

#[test]
fn exact_multiple_content_box_keeps_the_grid_flush_with_the_inset() {
    // A 13 px cell divides the 26 px decoration exactly, so both axes
    // are exact multiples: no leftover anywhere.
    let cell = 13u32;
    let cols = content_cells(window_cells(WINDOW.0, cell), cell);
    let rows = content_cells(window_cells(WINDOW.1, cell), cell);
    assert_eq!(
        (
            leftover(window_cells(WINDOW.0, cell), cell),
            leftover(window_cells(WINDOW.1, cell), cell)
        ),
        (0, 0),
        "fixture must be an exact multiple"
    );
    let mut rt = runtime_with_cell(cell, cell);
    rt.handle_resize(PhysicalSize::new(WINDOW.0, WINDOW.1))
        .expect("default window must reflow");
    let frame = rt.present_frames()[0];
    let (buf, width, fill) = present_selected_grid(&mut rt);
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits usize");
    let content_x = usize::try_from(frame.content.x).expect("content x fits") + pad;
    let slack = (WINDOW.1 - 2 * PAD) % cell;
    let content_y =
        usize::try_from(frame.content.y).expect("content y fits") + pad + (slack / 2) as usize;
    let content_w = frame.content.width as usize;
    let content_h = frame.content.height as usize;
    let painted_rows = rows_with_color(
        &buf,
        width,
        content_x,
        content_x + content_w,
        0,
        WINDOW.1 as usize,
        fill,
    );
    let painted_cols = cols_with_color(
        &buf,
        width,
        0,
        WINDOW.1 as usize,
        0,
        WINDOW.0 as usize,
        fill,
    );
    let first_y = *painted_rows.first().expect("painted rows");
    let last_y = *painted_rows.last().expect("painted rows");
    let first_x = *painted_cols.first().expect("painted cols");
    let last_x = *painted_cols.last().expect("painted cols");
    assert_eq!(first_y - content_y, 0, "no leftover above the grid");
    assert_eq!(content_y + content_h - 1 - last_y, 0, "no leftover below");
    assert_eq!(first_x - content_x, 0, "no leftover left of the grid");
    assert_eq!(content_x + content_w - 1 - last_x, 0, "no leftover right");
    assert_eq!(
        last_y - first_y + 1,
        rows as usize * cell as usize,
        "the grid paints exactly the content height"
    );
    assert_eq!(
        last_x - first_x + 1,
        cols as usize * cell as usize,
        "the grid paints exactly the content width"
    );
}

#[test]
fn pointer_resolves_the_cell_whose_band_contains_it() {
    // Shipped defaults: the content box is 694x430 at (21, 21)
    // window-relative, the 77x22 grid is centred inside it, so the
    // painted grid origin is (21, 27) - 12 px of row leftover split
    // 6/6, 1 px of column leftover floored to a 0 px split.
    let mut rt = runtime_with_cell(CELL_W, CELL_H);
    rt.handle_resize(PhysicalSize::new(WINDOW.0, WINDOW.1))
        .expect("default window must reflow");
    rt.tick().expect("first frame presents");
    let grid_x = 21.0;
    let grid_y = 27.0;
    // A pointer inside a painted cell resolves to that cell through the
    // frame hit-test and the global mapping alike: one origin.
    for (row, col) in [(0u16, 0u16), (11, 38), (21, 76)] {
        let inside = CursorPosition {
            x: grid_x + f64::from(col) * 9.0 + 4.0,
            y: grid_y + f64::from(row) * 19.0 + 9.0,
        };
        assert_eq!(
            rt.cursor_to_present_cell(inside),
            Some((ViewId::new(1), CellPos::new(row, col))),
            "a pointer inside painted cell ({row}, {col}) resolves to it"
        );
        assert_eq!(
            rt.cursor_to_cell(inside),
            CellPos::new(row, col),
            "the global mapping names the same painted cell"
        );
    }
    // The odd pane-local horizontal remainder uses the painted integer
    // origin in the legacy fallback too. A cell starts at its exact edge.
    let start = CursorPosition {
        x: grid_x + 9.0,
        y: grid_y + 19.0,
    };
    assert_eq!(
        rt.cursor_to_present_cell(start),
        Some((ViewId::new(1), CellPos::new(1, 1)))
    );
    assert_eq!(rt.cursor_to_cell(start), CellPos::new(1, 1));
    // Exact band membership: the pixel just above row r's top edge
    // belongs to row r - 1, the row whose band contains it.
    for row in [1u16, 11, 21] {
        let edge = CursorPosition {
            x: grid_x + 4.0,
            y: grid_y + f64::from(row) * 19.0 - 1.0,
        };
        assert_eq!(
            rt.cursor_to_present_cell(edge),
            Some((ViewId::new(1), CellPos::new(row - 1, 0))),
            "one px above row {row}'s top edge resolves to the row above"
        );
    }
    // The split stripes above and below the grid clamp to the edge rows,
    // exactly like the decoration band always has.
    let top_stripe = CursorPosition {
        x: grid_x + 4.0,
        y: 21.0 + 2.0,
    };
    assert_eq!(
        rt.cursor_to_present_cell(top_stripe),
        Some((ViewId::new(1), CellPos::new(0, 0))),
        "a pointer in the top leftover stripe clamps to the first row"
    );
    let bottom_stripe = CursorPosition {
        x: grid_x + 4.0,
        y: 21.0 + 430.0 - 3.0,
    };
    assert_eq!(
        rt.cursor_to_present_cell(bottom_stripe),
        Some((ViewId::new(1), CellPos::new(21, 0))),
        "a pointer in the bottom leftover stripe clamps to the last row"
    );
}

#[test]
fn fractional_dpi_pointer_agrees_at_both_painted_cell_edges() {
    let mut rt = runtime_with_cell(CELL_W, CELL_H);
    let extent = PhysicalSize::new(1144, 727);
    rt.apply_dpi_scale(1.5, Some(extent));
    rt.tick().expect("fractional-DPI frame presents");

    let (cell_w, cell_h) = rt.live_cell_size();
    assert_eq!((cell_w, cell_h), (14, 29));
    let frame = rt.present_frames()[0];
    let (window_x, window_y) = rt.window_grid_origin();
    let (grid_x, grid_y) = frame.grid_origin(cell_w, cell_h);
    let edge_x = f64::from(window_x + grid_x) + f64::from(cell_w);
    let edge_y = f64::from(window_y + grid_y) + f64::from(cell_h);
    assert_eq!((edge_x, edge_y), (47.0, 73.0));

    let probes = [
        (edge_x - 0.25, edge_y, CellPos::new(1, 0)),
        (edge_x, edge_y - 0.25, CellPos::new(0, 1)),
        (edge_x, edge_y, CellPos::new(1, 1)),
    ];
    let global_cells = probes.map(|(x, y, expected)| {
        let pos = CursorPosition { x, y };
        assert_eq!(rt.cursor_to_present_cell(pos), Some((frame.view, expected)));
        rt.cursor_to_cell(pos)
    });
    assert_eq!(global_cells, probes.map(|(_, _, expected)| expected));
}
