#![forbid(unsafe_code)]
//! bitty#3263 (CTX-1906): bottom plugin bands land on the window's **physical**
//! bottom inset instead of the floored cell grid.
//!
//! The reported defect: the bar/devtools chrome at the bottom of a Bitty window
//! sat a noticeable blank strip above the window's bottom edge. The window
//! padding (8 logical px) is intended; the extra gap was the sub-cell row
//! remainder
//!
//! ```text
//! remainder = (surface_height - 2 * padding) % cell_height
//! ```
//!
//! left over because the grid is derived by flooring the padded content extent
//! to whole rows while the bottom band was painted at `pad + row * cell_height`,
//! i.e. at the *cell* grid's bottom edge rather than the window's.
//!
//! These regressions run the shipped path end to end: a real [`Runtime`], a
//! real resize, a really mounted band, and the headless composited frame
//! ([`Runtime::headless_rgba`]). They read the painted pixels for the band's
//! origin and ask the public hit-test for the same physical point, so "paint
//! geometry" and "interaction geometry" are proven to be one mapping instead of
//! two helpers that happen to agree.
//!
//! Cases: an exact multiple (which must be unchanged), a non-multiple surface,
//! a stacked pair of bottom bands, and a fractional-DPI surface. Every case also
//! pins that the top-band origin, the layout container, the grid, and the PTY
//! winsize are untouched.
//!
//! Before the fix, `band_bottom_is_the_physical_inset_on_a_remainder_surface`,
//! `stacked_bottom_bands_tile_the_zone_with_no_seam`, and
//! `fractional_dpi_surface_flushes_the_band_to_the_inset` fail with the band's
//! last painted row exactly `remainder` above the inset (captured output is in
//! the PR body); the exact-multiple control passes on both trees.

use bitty_lua::ui::{ClickArg, ClickCommand, UiNode, UiSlot};
use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState};
use bitty_runtime::{
    BandContent, BandEdge, ChromeBands, Runtime, RuntimeConfig, resolve_band_token,
};

/// Window rows the fixtures size their window for.
const ROWS: u16 = 20;
/// Window columns the fixtures size their window for.
const COLS: u16 = 40;

/// A runtime whose glyph rasterizer is deterministic (no host font stack), so
/// the painted frame is byte-stable across hosts.
fn runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("runtime must build")
}

/// Surface extent for `rows` window rows plus `remainder` physical pixels, with
/// an exact multiple on the horizontal axis so only the vertical remainder is
/// under test.
fn extent_for(rt: &Runtime, rows: u16, remainder: u32) -> (u32, u32) {
    let (cell_w, cell_h) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    (
        pad * 2 + u32::from(COLS) * cell_w,
        pad * 2 + u32::from(rows) * cell_h + remainder,
    )
}

/// Resizes `rt` to `(width, height)`; returns the surface height.
fn resize(rt: &mut Runtime, width: u32, height: u32) -> u32 {
    rt.handle_resize(PhysicalSize::new(width, height))
        .expect("resize must reflow");
    rt.surface_extent().expect("surface extent").height()
}

/// One `bottom` band row: `bg` names a live theme token so the paint is
/// identifiable in the composited frame, and `on_click` makes it routable.
fn bottom_band(plugin: &str, text: &str, bg: &str) -> BandContent {
    BandContent {
        plugin_id: plugin.to_string(),
        slot: UiSlot::Bottom,
        root: UiNode::Text {
            text: text.to_string(),
            fg: Some("foreground".to_string()),
            bg: Some(bg.to_string()),
            bold: None,
            on_click: Some(ClickCommand {
                command: format!("{plugin}:focus"),
                args: vec![("id".to_string(), ClickArg::Number(1.0))],
                region: None,
            }),
            on_scroll: None,
            anchor: None,
            on_hover: None,
        },
        version: 1,
    }
}

/// One `top` band row of plain text (unclickable).
fn top_band(plugin: &str, text: &str, bg: &str) -> BandContent {
    BandContent {
        plugin_id: plugin.to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: text.to_string(),
            fg: Some("foreground".to_string()),
            bg: Some(bg.to_string()),
            bold: None,
            on_click: None,
            on_scroll: None,
            anchor: None,
            on_hover: None,
        },
        version: 1,
    }
}

/// RGBA color of a theme token as this theme resolves it.
fn token(rt: &Runtime, name: &str) -> [u8; 4] {
    resolve_band_token(name, &rt.config().theme).expect("live theme token")
}

/// Physical pixel at `(x, y)` of the last presented headless frame.
fn pixel(buf: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let start = (y * width + x) * 4;
    let slice = buf.get(start..start + 4).expect("pixel inside frame");
    [slice[0], slice[1], slice[2], slice[3]]
}

/// Physical rows in `[y0, y1)` carrying at least one pixel equal to `color`
/// inside `[x0, x1)`.
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
        .filter(|row| (x0..x1).any(|x| pixel(buf, width, x, *row) == color))
        .collect()
}

/// Physical rows in `[y0, y1)` where some pixel inside `[x0, x1)` differs from
/// `background` — i.e. "band ink is painted here".
fn rows_with_ink(
    buf: &[u8],
    width: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    background: [u8; 4],
) -> Vec<usize> {
    (y0..y1)
        .filter(|row| (x0..x1).any(|x| pixel(buf, width, x, *row) != background))
        .collect()
}

/// Everything one case needs after a tick: the composited frame plus the live
/// physical numbers the assertions are written against.
struct Painted {
    buffer: Vec<u8>,
    width: usize,
    /// Physical window padding (both sides).
    pad: i32,
    /// Surface height in physical px.
    surface_h: i32,
    /// Live cell metrics.
    cell_w: u32,
    cell_h: u32,
    /// Probe column range (band cells 0..=2).
    probe: (usize, usize),
    /// Physical y of the layout container's bottom edge: the top of the bottom
    /// band zone.
    stack_top: i32,
}

impl Painted {
    fn capture(rt: &Runtime) -> Self {
        let (cell_w, cell_h) = rt.live_cell_size();
        let extent = rt.surface_extent().expect("surface extent");
        let pad = i32::try_from(rt.window_padding_physical()).expect("padding fits i32");
        let surface_h = i32::try_from(extent.height()).expect("surface height fits i32");
        let container = rt.band_row_container();
        let buffer = rt.headless_rgba().expect("headless frame after tick");
        let width = usize::try_from(extent.width()).expect("surface width fits usize");
        let pad_px = usize::try_from(pad).expect("padding fits usize");
        let probe_end = pad_px + 3 * usize::try_from(cell_w).expect("cell width fits usize");
        Self {
            buffer,
            width,
            pad,
            surface_h,
            cell_w,
            cell_h,
            probe: (pad_px, probe_end),
            stack_top: pad
                + i32::from(container.y.saturating_add(container.height))
                    * i32::try_from(cell_h).unwrap_or(i32::MAX),
        }
    }

    /// First physical row below the bottom inset: the window padding.
    fn inset(&self) -> i32 {
        self.surface_h - self.pad
    }

    fn cell_h(&self) -> i32 {
        i32::try_from(self.cell_h).unwrap_or(i32::MAX)
    }

    /// Rows carrying `color` within `[y0, y1)`.
    fn color_rows(&self, color: [u8; 4], y0: i32, y1: i32) -> Vec<usize> {
        rows_with_color(
            &self.buffer,
            self.width,
            self.probe.0,
            self.probe.1,
            usize::try_from(y0.max(0)).expect("non-negative"),
            usize::try_from(y1.max(0)).expect("non-negative"),
            color,
        )
    }

    /// Rows carrying any non-background pixel within `[y0, y1)`.
    fn ink_rows(&self, background: [u8; 4], y0: i32, y1: i32) -> Vec<usize> {
        rows_with_ink(
            &self.buffer,
            self.width,
            self.probe.0,
            self.probe.1,
            usize::try_from(y0.max(0)).expect("non-negative"),
            usize::try_from(y1.max(0)).expect("non-negative"),
            background,
        )
    }

    /// Physical center of band cell 0, for hit-test probes at physical row `y`.
    fn probe_point(&self, y: i32) -> CursorPosition {
        CursorPosition {
            x: f64::from(self.pad) + f64::from(self.cell_w) / 2.0,
            y: f64::from(y) + 0.5,
        }
    }

    /// The hit-test claims exactly the painted rows on `edge` — and nothing one
    /// pixel outside them. This is the "paint and interaction are one mapping"
    /// assertion.
    fn assert_hit_matches_paint(&self, rt: &Runtime, painted: &[usize], edge: BandEdge) {
        assert!(!painted.is_empty(), "the fixture must paint a band");
        let first = i32::try_from(painted[0]).expect("row fits i32");
        let last = i32::try_from(*painted.last().expect("non-empty")).expect("row fits i32");
        for row in painted {
            let y = i32::try_from(*row).expect("row fits i32");
            let hit = rt
                .plugin_band_cell_hit(self.probe_point(y))
                .unwrap_or_else(|| panic!("painted band row {y} must be hit-testable"));
            assert_eq!(
                hit.edge, edge,
                "physical row {y} belongs to the {edge:?} band"
            );
        }
        assert!(
            rt.plugin_band_cell_hit(self.probe_point(first - 1))
                .is_none(),
            "one pixel above the painted band ({first}) is not band chrome"
        );
        assert!(
            rt.plugin_band_cell_hit(self.probe_point(last + 1))
                .is_none(),
            "one pixel below the painted band ({last}) is not band chrome"
        );
    }
}

/// Primary leaf's presented row count.
fn primary_rows(rt: &Runtime) -> u16 {
    let primary = rt.primary_view().expect("primary owner");
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == primary)
        .map(|frame| frame.rows)
        .expect("primary presented")
}

/// Sizes a runtime to `rows` window rows plus `remainder` physical pixels and
/// asserts the fixture landed on the intended geometry.
fn fixture(rows: u16, remainder: u32) -> Runtime {
    let mut rt = runtime();
    let (width, height) = extent_for(&rt, rows, remainder);
    let surface_h = resize(&mut rt, width, height);
    let (_, cell_h) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    assert_eq!(
        rt.window_cells().height,
        rows,
        "the fixture controls the window row count"
    );
    assert_eq!(
        (surface_h - 2 * pad) - u32::from(rows) * cell_h,
        remainder,
        "fixture remainder"
    );
    rt
}

// ---------------------------------------------------------------------------
// The zero-remainder control: an exact multiple is untouched
// ---------------------------------------------------------------------------

#[test]
fn exact_multiple_surface_keeps_the_band_on_its_cell_row() {
    // No remainder: the physical anchor and the cell grid coincide, so the
    // band must land exactly where it always did.
    let mut rt = fixture(ROWS, 0);
    let bg = token(&rt, "bright_red");
    let background = token(&rt, "background");
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the band");
    assert_eq!(rt.band_host_stats().paint_violations, 0);

    let painted = Painted::capture(&rt);
    assert_eq!(
        painted.inset(),
        painted.pad + i32::from(ROWS) * painted.cell_h(),
        "an exact multiple fills the padded content"
    );
    let band_rows = painted.ink_rows(background, painted.stack_top, painted.inset());
    assert_eq!(
        band_rows,
        (painted.stack_top..painted.inset())
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the band owns exactly the last cell row"
    );
    assert_eq!(
        painted
            .color_rows(bg, painted.stack_top, painted.inset())
            .last(),
        Some(&usize::try_from(painted.inset() - 1).expect("row")),
        "the band fill reaches the bottom inset"
    );
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);
}

// ---------------------------------------------------------------------------
// The defect: the remainder must be absorbed above the band, never below it
// ---------------------------------------------------------------------------

#[test]
fn band_bottom_is_the_physical_inset_on_a_remainder_surface() {
    // With a 7 px remainder the old cell-row origin left the band's last
    // painted row 7 px above the inset. The fix keeps the band's content-facing
    // edge where it was and grows the band down to the inset.
    let remainder = 7;
    let mut rt = fixture(ROWS, remainder);
    let bg = token(&rt, "bright_red");
    let background = token(&rt, "background");
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the band");
    assert_eq!(rt.band_host_stats().paint_violations, 0);

    let painted = Painted::capture(&rt);
    let inset = painted.inset();
    // Only the window padding is left blank below the band.
    let padding_rows = painted.color_rows(background, inset, painted.surface_h);
    assert_eq!(
        padding_rows,
        (inset..painted.surface_h)
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the window padding is the only gap below the band"
    );
    let band_rows = painted.ink_rows(background, painted.stack_top, painted.surface_h);
    assert_eq!(
        band_rows.first(),
        Some(&usize::try_from(painted.stack_top).expect("row")),
        "the band's content-facing edge does not move"
    );
    assert_eq!(
        band_rows.last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the band's last painted row is the inset edge, {remainder} px lower than before"
    );
    assert_eq!(
        band_rows.len(),
        usize::try_from(inset - painted.stack_top).expect("rows"),
        "the band is one contiguous run from the container edge to the inset"
    );
    // The strip the old rule left blank is now the band's own fill.
    for y in inset - i32::try_from(remainder).expect("remainder")..inset {
        assert_eq!(
            painted.color_rows(bg, y, y + 1),
            vec![usize::try_from(y).expect("row")],
            "row {y} of the old remainder strip is band fill, not window background"
        );
    }
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);

    // Routing follows the paint: a press in the strip that used to be blank now
    // belongs to the band and routes its command.
    rt.handle_cursor_moved(painted.probe_point(inset - 1));
    rt.handle_mouse_input(bitty_platform::MouseEvent::new(
        MouseButton::Left,
        PressState::Pressed,
    ));
    rt.handle_mouse_input(bitty_platform::MouseEvent::new(
        MouseButton::Left,
        PressState::Released,
    ));
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "the bottom strip routes to the band");
    assert_eq!(clicks[0].plugin_id, "bar");
    assert_eq!(clicks[0].command, "focus");
    assert_eq!(
        rt.band_host_stats().clicks_routed,
        1,
        "the routed click is accounted"
    );
}

#[test]
fn stacked_bottom_bands_tile_the_zone_with_no_seam() {
    // Two stacked bands plus a remainder: the stack must stay contiguous from
    // the container's bottom edge to the inset, with no seam between the bands
    // and no new gap above the stack.
    let mut rt = fixture(ROWS, 7);
    let bar = token(&rt, "bright_red");
    let devtools = token(&rt, "bright_green");
    let background = token(&rt, "background");
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![
            bottom_band("bar", "status bar", "bright_red"),
            bottom_band("devtools", "devtools", "bright_green"),
        ],
        ..Default::default()
    });
    rt.tick().expect("tick presents the stack");
    assert_eq!(rt.band_host_stats().paint_violations, 0);
    assert_eq!(rt.plugin_band_budget(), (0, 2), "two bottom rows reserved");
    assert_eq!(
        rt.band_row_container().height,
        ROWS - 2,
        "the container still loses exactly two rows"
    );

    let painted = Painted::capture(&rt);
    let inset = painted.inset();
    let stack_top = painted.stack_top;
    let band_rows = painted.ink_rows(background, stack_top, painted.surface_h);
    assert_eq!(
        band_rows.last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the outer band ends on the inset, 7 px lower than before"
    );
    assert_eq!(
        band_rows.first(),
        Some(&usize::try_from(stack_top).expect("row")),
        "the stack starts at the container's bottom edge"
    );
    assert_eq!(
        band_rows.len(),
        usize::try_from(inset - stack_top).expect("rows"),
        "the stack is contiguous: no seam between bands, no gap above"
    );
    assert!(
        painted.color_rows(background, stack_top, inset).is_empty(),
        "no background row inside the bottom band zone"
    );
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);

    // Each band keeps its own one-cell budget: the outer band is the last cell
    // row of the zone, the inner one owns everything above it.
    let outer_top = inset - painted.cell_h();
    let bar_rows = painted.color_rows(bar, stack_top, inset);
    let devtools_rows = painted.color_rows(devtools, stack_top, inset);
    assert!(
        !bar_rows.is_empty() && !devtools_rows.is_empty(),
        "both bands paint"
    );
    assert!(
        bar_rows
            .iter()
            .all(|row| i32::try_from(*row).expect("row") >= outer_top),
        "the outer band owns only the last cell row"
    );
    assert!(
        devtools_rows
            .iter()
            .all(|row| i32::try_from(*row).expect("row") < outer_top),
        "the inner band owns everything above it"
    );
    for row in &bar_rows {
        let hit = rt
            .plugin_band_cell_hit(painted.probe_point(i32::try_from(*row).expect("row")))
            .expect("outer band row routes");
        assert_eq!(hit.visible_index, 0, "outer band index");
    }
    for row in &devtools_rows {
        let hit = rt
            .plugin_band_cell_hit(painted.probe_point(i32::try_from(*row).expect("row")))
            .expect("inner band row routes");
        assert_eq!(hit.visible_index, 1, "inner band index");
    }
}

#[test]
fn fractional_dpi_surface_flushes_the_band_to_the_inset() {
    // Fractional DPI is where the remainder is unavoidable: the scaled cell is
    // not a divisor of the logical window. The rule must hold on scaled cells
    // and scaled padding too.
    let mut rt = runtime();
    let (cell_w, cell_h) = rt.live_cell_size();
    let pad_base = rt.window_padding_physical();
    let scale = 1.5_f64;
    let scaled_cell_w = (f64::from(cell_w) * scale).round() as u32;
    let scaled_cell_h = (f64::from(cell_h) * scale).round() as u32;
    let scaled_pad = (f64::from(pad_base) * scale).round() as u32;
    let remainder = 17;
    let extent = PhysicalSize::new(
        scaled_pad * 2 + u32::from(COLS) * scaled_cell_w,
        scaled_pad * 2 + u32::from(ROWS) * scaled_cell_h + remainder,
    );
    rt.apply_dpi_scale(scale, Some(extent));
    assert_eq!(rt.dpi_scale(), scale);
    assert_eq!(
        rt.live_cell_size(),
        (scaled_cell_w, scaled_cell_h),
        "the live cell is the scaled cell"
    );
    assert_eq!(
        rt.window_padding_physical(),
        scaled_pad,
        "the inset scales with DPI"
    );
    let (width, height) = extent_for(&rt, ROWS, remainder);
    let surface_h = resize(&mut rt, width, height);
    assert_eq!(rt.window_cells().height, ROWS);
    assert_eq!(
        (surface_h - 2 * scaled_pad) - u32::from(ROWS) * scaled_cell_h,
        remainder,
        "fractional fixture remainder"
    );

    let bg = token(&rt, "bright_red");
    let background = token(&rt, "background");
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the band");
    assert_eq!(rt.band_host_stats().paint_violations, 0);

    let painted = Painted::capture(&rt);
    let inset = painted.inset();
    assert_eq!(
        inset,
        i32::try_from(surface_h - scaled_pad).expect("inset"),
        "the inset is the scaled padding"
    );
    let band_rows = painted.ink_rows(background, painted.stack_top, painted.surface_h);
    assert_eq!(
        band_rows.last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the band reaches the scaled inset, {remainder} px lower than before"
    );
    assert_eq!(
        painted
            .color_rows(bg, painted.stack_top, painted.surface_h)
            .last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the band fill reaches the scaled inset"
    );
    assert_eq!(
        painted.color_rows(background, inset, painted.surface_h),
        (inset..painted.surface_h)
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "only the scaled padding remains blank below the band"
    );
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);
}

// ---------------------------------------------------------------------------
// Everything else keeps its geometry
// ---------------------------------------------------------------------------

#[test]
fn top_band_origin_is_unchanged_on_a_remainder_surface() {
    // The remainder is a *bottom* problem: the top edge is anchored by the
    // padding, so a top band must stay on its cell row even when the window
    // leaves a sub-cell strip at the bottom.
    let mut rt = fixture(ROWS, 7);
    let background = token(&rt, "background");
    rt.set_chrome_bands(ChromeBands {
        top: vec![top_band("bar", "top bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the top band");
    assert_eq!(rt.visible_band_row(BandEdge::Top, 0), Some(0));

    let painted = Painted::capture(&rt);
    let top_rows = painted.ink_rows(background, 0, painted.pad + painted.cell_h());
    assert_eq!(
        top_rows,
        (painted.pad..painted.pad + painted.cell_h())
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the top band stays on row 0, exactly one cell tall"
    );
    painted.assert_hit_matches_paint(&rt, &top_rows, BandEdge::Top);
    // The bottom strip keeps its pre-fix shape: no band was moved down there.
    let pad_rows = usize::try_from(painted.pad).expect("padding");
    assert_eq!(
        painted
            .color_rows(background, painted.inset() - 7, painted.surface_h)
            .len(),
        pad_rows + 7,
        "the bottom strip is still background plus padding"
    );
}

#[test]
fn bottom_anchor_leaves_the_container_grid_and_pty_size_unchanged() {
    // The fix is paint-and-routing only: the reserved rows, the layout
    // container, the leaf grid, and the PTY winsize must stay the numbers a
    // pre-#3263 tree produced (one window row per band, no more, no less).
    let mut rt = fixture(ROWS, 7);
    let mut control = fixture(ROWS, 7);
    control.tick().expect("the band-free control presents");
    assert!(
        control.plugin_band_row(BandEdge::Bottom, 0).is_none(),
        "the control runtime mounts no band"
    );

    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the band");
    let window = rt.window_cells();
    assert_eq!(
        rt.container(),
        bitty_runtime::UiRect::new(
            window.x,
            window.y,
            window.width,
            window.height.saturating_sub(1)
        ),
        "the container loses exactly one row"
    );
    assert_eq!(
        rt.plugin_band_row(BandEdge::Bottom, 0),
        Some(window.height.saturating_sub(1)),
        "the band still owns the last cell row"
    );
    assert_eq!(
        rt.band_row_container().height,
        window.height.saturating_sub(1),
        "the row carve is one band row"
    );
    // Grid: the primary leaf is one row shorter than the band-free control, and
    // the PTY winsize follows the grid, so the fix changed neither.
    let rows_with_band = primary_rows(&rt);
    let rows_without_band = primary_rows(&control);
    assert_eq!(
        rows_with_band + 1,
        rows_without_band,
        "the band still costs exactly one leaf row"
    );
    let snapshot = rt.snapshot();
    assert_eq!(
        usize::from(rows_with_band),
        snapshot.height,
        "the primary grid follows the reduced leaf"
    );
    if let Some((cols, rows)) = rt.pty_size() {
        assert_eq!(
            (cols, rows),
            (
                u16::try_from(snapshot.width).expect("columns"),
                u16::try_from(snapshot.height).expect("rows")
            ),
            "the winsize still equals the band-reduced primary grid"
        );
    }
}
