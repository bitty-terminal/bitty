#![forbid(unsafe_code)]
//! Issue #2896: the PTY grid must never advertise more rows than are painted.
//!
//! The invariant every test here asserts, at scale 1.0 / 1.25 / 1.5 / 1.6:
//!
//! `rows * painted_cell_height + 2 * padding <= content_height`
//!
//! with the *same* cell metric used for sizing (the PTY/state grid), painting
//! (the present frame the grid is drawn through) and pointer hit-testing.
//!
//! The metric is one value on purpose: `Runtime::live_cell_metrics()` is the
//! renderer cell, and both `grid_from_physical` and `present_frames` read it.
//! What can still diverge is *when* the layout container (the window's cell
//! count) is derived from it: a metric adopted without re-deriving the
//! container leaves the grid at the old count, which is the #2896 symptom —
//! a full-screen TUI loses its top rows while `stty size` still reports the
//! old (larger) row count.
use bitty_platform::PhysicalSize;
use bitty_runtime::{PresentFrame, Runtime};

/// Logical window from the #2896 report.
const LOGICAL_W: f64 = 1566.0;
const LOGICAL_H: f64 = 935.0;

/// Scales the issue names plus the two neighbors that expose rounding drift.
const SCALES: [f64; 4] = [1.0, 1.25, 1.5, 1.6];

fn physical_for(scale: f64) -> PhysicalSize {
    PhysicalSize::new(
        (LOGICAL_W * scale).round() as u32,
        (LOGICAL_H * scale).round() as u32,
    )
}

fn runtime_at(scale: f64) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    // Drive the production route, not a convenience one: the runtime's own
    // `WindowEventKind::ScaleFactorChanged` handler calls
    // `apply_dpi_scale(factor, None)` (resize.rs), i.e. the no-extent
    // adoption whose container re-derivation is the #2896 fix. Passing the
    // physical extent explicitly here would bypass the fallback and make
    // every assertion below an arithmetic identity.
    rt.apply_dpi_scale(scale, None);
    assert_eq!(rt.dpi_scale(), scale, "scale must be adopted verbatim");
    rt
}

/// The primary owner's present frame — the rectangle the grid is painted
/// through and the pointer is hit-tested against.
fn primary_frame(rt: &Runtime) -> PresentFrame {
    let primary = rt.primary_view().expect("runtime has a primary view");
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .expect("primary frame is presented")
}

fn row_text(snapshot: &bitty_term_state::Snapshot, row: usize) -> String {
    let start = row * snapshot.width;
    let end = start + snapshot.width;
    snapshot.cells[start..end]
        .iter()
        .map(|cell| cell.glyph)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn grid_fits_the_content_rect_at_every_scale() {
    for scale in SCALES {
        let rt = runtime_at(scale);
        let (cell_w, cell_h) = rt.live_cell_size();
        let frame = primary_frame(&rt);
        let content = frame.content;
        let grid_h = u32::from(frame.rows) * cell_h;
        let grid_w = u32::from(frame.cols) * cell_w;

        assert!(
            grid_h <= content.height,
            "scale {scale}: {0} rows * {cell_h}px = {grid_h}px overflows the \
             {1}px content height by {2}px",
            frame.rows,
            content.height,
            grid_h - content.height
        );
        // "No unused row": one more row would not fit. This is what makes a
        // full-screen TUI use the whole pane instead of leaving a blank band.
        assert!(
            content.height < grid_h + cell_h,
            "scale {scale}: content height {}px leaves room for another \
             {cell_h}px row past {grid_h}px",
            content.height
        );
        assert!(
            grid_w <= content.width,
            "scale {scale}: {0} cols * {cell_w}px = {grid_w}px overflows the \
             {1}px content width by {2}px",
            frame.cols,
            content.width,
            grid_w - content.width
        );
        assert!(
            content.width < grid_w + cell_w,
            "scale {scale}: content width {}px leaves room for another \
             {cell_w}px column past {grid_w}px",
            content.width
        );

        // Painted grid origin: the present path translates by the window
        // padding, then by the content rect (`px_add(pad, content.y + row *
        // cell_h)`), so the last painted row must still land inside the
        // window.
        let pad = rt.window_padding_physical();
        let painted_bottom = u32::try_from(content.y).unwrap_or(0) + pad + grid_h;
        assert!(
            painted_bottom <= physical_for(scale).height(),
            "scale {scale}: painted grid bottom {painted_bottom}px exceeds the \
             {}px window",
            physical_for(scale).height()
        );
    }
}

#[test]
fn pty_size_equals_the_painted_grid_at_every_scale() {
    for scale in SCALES {
        let rt = runtime_at(scale);
        let snapshot = rt.snapshot();
        let frame = primary_frame(&rt);

        assert_eq!(
            (snapshot.width, snapshot.height),
            (usize::from(frame.cols), usize::from(frame.rows)),
            "scale {scale}: the state grid (what the PTY winsize follows) must \
             match the presented frame exactly"
        );
        // The PTY winsize is `self.cols`/`self.rows`, adopted from that same
        // frame in `reflow_content_to_chrome` (resize.rs), and the state grid
        // asserted above is exactly what `resize_with_report` sized from it.
    }
}

/// The numbered full-screen fixture from the issue: exactly one screenful,
/// `01..=rows`, no trailing newline so the fixture does not scroll. Row 1 of
/// the output must land on the top painted row, and the last row must be
/// painted in full.
fn numbered_fixture_at(scale: f64) -> (Runtime, usize) {
    let mut rt = runtime_at(scale);
    let rows = usize::from(primary_frame(&rt).rows);
    let mut feed = String::new();
    for line in 1..=rows {
        if line > 1 {
            feed.push_str("\r\n");
        }
        feed.push_str(&format!("{line:02}"));
    }
    rt.handle_pty_bytes(feed.as_bytes());
    (rt, rows)
}

#[test]
fn numbered_fixture_keeps_row_one_visible_at_every_scale() {
    for scale in SCALES {
        let (rt, rows) = numbered_fixture_at(scale);

        let snapshot = rt.snapshot();
        assert_eq!(
            snapshot.height, rows,
            "scale {scale}: a full screenful must not reflow the grid"
        );
        assert_eq!(
            row_text(&snapshot, 0),
            "01",
            "scale {scale}: the first line of a full-screen TUI must be on the \
             top painted row (the #2896 top-row clipping)"
        );
        assert_eq!(
            row_text(&snapshot, rows - 1),
            format!("{rows:02}"),
            "scale {scale}: the last row must be painted in full"
        );
    }
}

#[test]
fn pointer_in_the_last_cell_maps_to_the_last_cell_at_every_scale() {
    for scale in SCALES {
        // Same numbered fixture as the visibility test: a full-screen TUI is
        // what the reporter was clicking into, and the hit-test must agree
        // with the grid that fixture sized (rows) and painted.
        let (rt, rows) = numbered_fixture_at(scale);
        let snapshot = rt.snapshot();
        assert_eq!(
            snapshot.height, rows,
            "scale {scale}: the fixture must fit the adopted grid"
        );
        let (cell_w, cell_h) = rt.live_cell_size();
        let frame = primary_frame(&rt);
        let content = frame.content;
        let pad = f64::from(rt.window_padding_physical());
        // Centre of the bottom-right cell, in physical window pixels, through
        // the same translation the paint path uses (padding + content origin).
        let pos = bitty_platform::CursorPosition {
            x: pad
                + f64::from(content.x)
                + f64::from(frame.cols.saturating_sub(1)) * f64::from(cell_w)
                + f64::from(cell_w) / 2.0,
            y: pad
                + f64::from(content.y)
                + f64::from(frame.rows.saturating_sub(1)) * f64::from(cell_h)
                + f64::from(cell_h) / 2.0,
        };
        let (view, cell) = rt
            .cursor_to_present_cell(pos)
            .unwrap_or_else(|| panic!("scale {scale}: the last cell must be addressable"));
        assert_eq!(view, frame.view, "scale {scale}: hit-test view");
        assert_eq!(
            (cell.row, cell.col),
            (frame.rows - 1, frame.cols - 1),
            "scale {scale}: a pixel in the last row/column must map to the \
             last cell, not a cell outside the painted grid"
        );
        // ...and the cell it resolved really is the fixture's last line, not
        // a row past the painted content.
        assert_eq!(
            row_text(&snapshot, cell.row as usize),
            format!("{rows:02}"),
            "scale {scale}: the bottom-right hit must land on the numbered \
             fixture's last row"
        );
    }
}

#[test]
fn scale_adopted_without_an_extent_still_fits_the_surface() {
    // The `ScaleFactorChanged` path adopts the new cell with no physical
    // extent when no window handle is at hand (resize.rs). The layout
    // container must be re-derived from the configured surface extent then:
    // otherwise the grid keeps the cell count of the *old* metric and the
    // new, taller cell paints it out of the window by the metric ratio —
    // #2896's "PTY advertises more rows than are painted".
    let mut rt = Runtime::with_defaults().expect("default runtime builds");
    rt.handle_resize(PhysicalSize::new(1500, 900))
        .expect("resize reflows");
    let before = rt.snapshot();
    rt.apply_dpi_scale(1.6, None);

    let surface = rt.surface_extent().expect("surface configured");
    let (cell_w, cell_h) = rt.live_cell_size();
    let frame = primary_frame(&rt);
    let grid_w = u32::from(frame.cols) * cell_w;
    let grid_h = u32::from(frame.rows) * cell_h;

    assert!(
        grid_h <= surface.height() && grid_w <= surface.width(),
        "adopting 1.6 without an explicit extent left a {grid_w}x{grid_h} grid \
         in a {}x{} surface (was {}x{} cells)",
        surface.width(),
        surface.height(),
        before.width,
        before.height
    );
    let snapshot = rt.snapshot();
    assert_eq!(
        (snapshot.width, snapshot.height),
        (usize::from(frame.cols), usize::from(frame.rows)),
        "the PTY grid must follow the re-derived container"
    );
}
