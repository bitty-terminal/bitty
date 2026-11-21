#![forbid(unsafe_code)]
//! CTX-1922: center the whole vertical window grid, including plugin bands.
//! These fixtures resize a runtime, mount bands, and compare headless pixels
//! with public hit-tests and logical geometry. Each band retains its cell
//! grant and the extra pixel from odd slack goes below the stack.

use bitty_lua::ui::{ClickArg, ClickCommand, UiNode, UiSlot};
use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState};
use bitty_runtime::{
    BandContent, BandEdge, ChromeBands, Runtime, RuntimeConfig, resolve_band_token,
};
use bitty_ui::CellPos;

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
    /// Configured physical padding, also the horizontal inset.
    pad: i32,
    /// Expected top of the whole window stack.
    top: i32,
    /// Expected exclusive bottom of the whole window stack.
    bottom: i32,
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
        let grid_h = i32::from(rt.window_cells().height) * i32::try_from(cell_h).unwrap();
        let slack = surface_h - 2 * pad - grid_h;
        let top = pad + slack / 2;
        let container = rt.band_row_container();
        let buffer = rt.headless_rgba().expect("headless frame after tick");
        let width = usize::try_from(extent.width()).expect("surface width fits usize");
        let pad_px = usize::try_from(pad).expect("padding fits usize");
        let probe_end = pad_px + 3 * usize::try_from(cell_w).expect("cell width fits usize");
        Self {
            buffer,
            width,
            pad,
            top,
            bottom: top + grid_h,
            surface_h,
            cell_w,
            cell_h,
            probe: (pad_px, probe_end),
            stack_top: top
                + i32::from(container.y.saturating_add(container.height))
                    * i32::try_from(cell_h).unwrap_or(i32::MAX),
        }
    }

    /// First physical row below the centered stack.
    fn inset(&self) -> i32 {
        self.bottom
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

    /// Rows in `[y0, y1)` carrying a pixel that differs from the band's own
    /// `fill` color — the glyph ink painted over the capsule fill. Blended
    /// glyph pixels never equal a theme token, so "differs from the fill" is
    /// the observable for the glyph row (bitty#3282).
    fn ink_rows_over_fill(&self, fill: [u8; 4], y0: i32, y1: i32) -> Vec<usize> {
        rows_with_ink(
            &self.buffer,
            self.width,
            self.probe.0,
            self.probe.1,
            usize::try_from(y0.max(0)).expect("non-negative"),
            usize::try_from(y1.max(0)).expect("non-negative"),
            fill,
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

#[test]
fn odd_window_slack_moves_the_workspace_and_bar_together_on_resize() {
    let mut rt = Runtime::with_deterministic_rasterizer(RuntimeConfig {
        cell_width: 14,
        cell_height: 30,
        window_padding: 13,
        decoration: bitty_ui::Decoration::new(0, 0, 0, 0, 0),
        ..RuntimeConfig::default()
    })
    .expect("runtime must build");
    resize(&mut rt, 1570, 926);
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.handle_pty_bytes(b"\x1b[41m \x1b[29;1H \x1b[0m\x1b[H");
    rt.tick().expect("exact-multiple frame");
    let window = rt.window_cells();
    let frames = rt.present_frames();
    let snapshot = rt.snapshot();
    assert_eq!((window.width, window.height), (110, 30));
    let caret = rt
        .ime_cursor_area()
        .expect("focused cursor has an IME area");
    assert_eq!((caret.x, caret.y), (13, 13));

    resize(&mut rt, 1570, 935);
    rt.tick()
        .expect("pixel-only resize must present a new frame");
    assert_eq!(rt.window_cells(), window);
    assert_eq!(rt.present_frames(), frames);
    assert_eq!(rt.snapshot().generation, snapshot.generation);
    let caret = rt
        .ime_cursor_area()
        .expect("resized cursor has an IME area");
    assert_eq!(
        (caret.x, caret.y, caret.width, caret.height),
        (13, 17, 14, 30)
    );
    let painted = Painted::capture(&rt);
    let background = token(&rt, "background");
    let probe = |y| pixel(&painted.buffer, painted.width, 13, y);
    assert_eq!(probe(16), background, "17 physical rows above the stack");
    assert_ne!(probe(17), background, "the first workspace row moved");
    assert_eq!(probe(856), background);
    assert_ne!(probe(857), background);
    assert_ne!(probe(886), background, "workspace fill reaches the Bar");
    let expected_bar: Vec<_> = (887..917).collect();
    assert_eq!(painted.ink_rows(background, 887, 935), expected_bar);
    let point = |y| CursorPosition { x: 27.0, y };
    assert_eq!(rt.cursor_to_present_cell(point(16.0)), None);
    let view = rt.primary_view().expect("primary view");
    for (y, row) in [(17.0, 0), (46.999, 0), (47.0, 1), (886.999, 28)] {
        let cell = CellPos::new(row, 1);
        assert_eq!(rt.cursor_to_present_cell(point(y)), Some((view, cell)));
        assert_eq!(rt.cursor_to_cell(point(y)), cell);
    }
    assert_eq!(rt.cursor_to_present_cell(point(887.0)), None);
    assert!(rt.plugin_band_cell_hit(point(886.999)).is_none());
    assert!(rt.plugin_band_cell_hit(point(887.0)).is_some());
    assert!(rt.plugin_band_cell_hit(point(916.999)).is_some());
    assert!(rt.plugin_band_cell_hit(point(917.0)).is_none());

    rt.handle_pty_bytes(b"\x1b[29;1H\x1b[42m \x1b[0m\x1b[H");
    rt.tick()
        .expect("cell damage after the resize must present");
    let damaged = Painted::capture(&rt);
    assert_ne!(pixel(&damaged.buffer, damaged.width, 13, 886), probe(886));
    assert_eq!(pixel(&damaged.buffer, damaged.width, 13, 856), background);

    rt.set_decoration(bitty_ui::Decoration::new(0, 6, 1, 0, 0))
        .expect("apply a six-pixel outer panel gap");
    rt.tick().expect("panel decoration must present");
    let decorated = Painted::capture(&rt);
    let probe = |y| pixel(&decorated.buffer, decorated.width, 100, y);
    assert_ne!(probe(880), background, "last panel border pixel");
    assert!((881..887).all(|y| probe(y) == background));
    assert_ne!(
        probe(887),
        background,
        "only the configured gap precedes the Bar"
    );
}

#[test]
fn tiny_or_mismatched_surfaces_keep_the_base_origin() {
    let mut rt = fixture(ROWS, 9);
    let (width, height) = extent_for(&rt, ROWS, 9);
    let pad = rt.window_padding_physical() as i32;
    assert_eq!(rt.window_grid_origin(), (pad, pad + 4));

    // A column mismatch invalidates otherwise plausible vertical slack.
    rt.set_window_cells(bitty_runtime::UiRect::new(0, 0, COLS - 1, ROWS));
    assert_eq!(rt.window_grid_origin(), (pad, pad));
    rt.set_window_cells(bitty_runtime::UiRect::new(0, 0, COLS, ROWS - 1));
    assert_eq!(rt.window_grid_origin(), (pad, pad));

    resize(&mut rt, 1, height);
    assert_eq!(rt.window_cells().width, 1);
    assert_eq!(rt.window_grid_origin(), (pad, pad));
    resize(&mut rt, width, 1);
    assert_eq!(rt.window_cells().height, 1);
    assert_eq!(rt.window_grid_origin(), (pad, pad));
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    let _ = rt.tick(); // The viewport can clip every primitive on a 1 px surface.
    assert_eq!(rt.visible_band_row(BandEdge::Bottom, 0), None);
    assert_eq!(rt.band_host_stats().paint_violations, 0);
}

// ---------------------------------------------------------------------------
// The zero-remainder control: an exact multiple is untouched
// ---------------------------------------------------------------------------

#[test]
fn exact_multiple_surface_keeps_the_band_on_its_cell_row() {
    // An exact multiple keeps the base padding on both sides.
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
// Sub-cell slack belongs to the outside of the entire stack
// ---------------------------------------------------------------------------

#[test]
fn single_bottom_band_meets_the_workspace_on_a_remainder_surface() {
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
    assert_eq!(painted.top, painted.pad + 3);
    assert_eq!(painted.surface_h - inset, painted.pad + 4);
    let padding_rows = painted.color_rows(background, inset, painted.surface_h);
    assert_eq!(
        padding_rows,
        (inset..painted.surface_h)
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the base padding and the lower share of slack are background"
    );
    let content_top = inset - painted.cell_h();
    let band_rows = painted.ink_rows(background, painted.stack_top, painted.surface_h);
    assert_eq!(
        content_top, painted.stack_top,
        "the band meets the workspace"
    );
    assert_eq!(
        band_rows.first(),
        Some(&usize::try_from(content_top).expect("row")),
        "the painted band starts one cell above the inset"
    );
    assert_eq!(
        band_rows.last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the painted band still ends on the inset edge"
    );
    assert_eq!(
        band_rows.len(),
        usize::try_from(painted.cell_h()).expect("rows"),
        "the painted band is exactly one cell tall, not cell + remainder"
    );
    // The shared chrome rule owns the first row of the painted cell row, and
    // every row below it is the band's own fill.
    assert_eq!(
        painted.color_rows(bg, content_top, inset),
        (content_top + 1..inset)
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the cell row is band fill below the chrome rule"
    );
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);

    // Routing includes the last painted pixel.
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
        "the outer band ends three pixels below its exact-multiple position"
    );
    let painted_top = inset - 2 * painted.cell_h();
    assert_eq!(painted_top, stack_top, "the band stack meets the workspace");
    assert_eq!(
        band_rows.first(),
        Some(&usize::try_from(painted_top).expect("row")),
        "the painted stack starts two cells above the inset"
    );
    assert_eq!(
        band_rows.len(),
        usize::try_from(2 * painted.cell_h()).expect("rows"),
        "the painted stack is two contiguous cell rows"
    );
    assert!(
        painted
            .color_rows(background, painted_top, inset)
            .is_empty(),
        "no background row inside the painted stack"
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
fn fractional_dpi_surface_centers_the_stack_in_physical_pixels() {
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
        i32::try_from(surface_h - scaled_pad - remainder.div_ceil(2)).expect("inset"),
        "the inset includes the lower share of physical slack"
    );
    let band_rows = painted.ink_rows(background, painted.stack_top, painted.surface_h);
    assert_eq!(
        band_rows.last(),
        Some(&usize::try_from(inset - 1).expect("row")),
        "the band ends at the centered stack's bottom edge"
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
        "scaled padding and the lower share of slack remain blank"
    );
    painted.assert_hit_matches_paint(&rt, &band_rows, BandEdge::Bottom);
}

// ---------------------------------------------------------------------------
// Everything else keeps its geometry
// ---------------------------------------------------------------------------

#[test]
fn top_band_shares_the_centered_stack_origin() {
    let mut rt = fixture(ROWS, 7);
    let background = token(&rt, "background");
    let first_side_color = token(&rt, "bright_green");
    let second_side_color = token(&rt, "bright_blue");
    rt.set_chrome_bands(ChromeBands {
        top: vec![top_band("bar", "top bar", "bright_red")],
        left: vec![BandContent {
            plugin_id: "side".to_string(),
            slot: UiSlot::Left,
            root: UiNode::column(vec![
                top_band("side", "first", "bright_green").root,
                top_band("side", "second", "bright_blue").root,
            ]),
            version: 1,
        }],
        ..Default::default()
    });
    rt.tick().expect("tick presents the top band");
    assert_eq!(rt.visible_band_row(BandEdge::Top, 0), Some(0));

    let painted = Painted::capture(&rt);
    let top_rows = painted.ink_rows(background, 0, painted.top + painted.cell_h());
    assert_eq!(
        top_rows,
        (painted.pad + 3..painted.pad + 3 + painted.cell_h())
            .map(|y| usize::try_from(y).expect("row"))
            .collect::<Vec<_>>(),
        "the top band stays on row 0, exactly one cell tall"
    );
    // The next physical pixel belongs to the side band at this x.
    let side_top = painted.top + painted.cell_h();
    let probe = |y| CursorPosition {
        x: f64::from(painted.pad),
        y,
    };
    assert!(
        rt.plugin_band_cell_hit(probe(f64::from(painted.top) - 0.5))
            .is_none()
    );
    for (y, edge, row) in [
        (painted.top, BandEdge::Top, 0),
        (side_top - 1, BandEdge::Top, 0),
        (side_top, BandEdge::Left, 1),
        (side_top + painted.cell_h(), BandEdge::Left, 2),
    ] {
        let hit = rt
            .plugin_band_cell_hit(probe(f64::from(y)))
            .expect("band hit");
        assert_eq!((hit.edge, hit.row), (edge, row));
    }
    assert!(
        !painted
            .color_rows(first_side_color, side_top, side_top + 1)
            .is_empty()
    );
    let second_top = side_top + painted.cell_h();
    assert!(
        !painted
            .color_rows(second_side_color, second_top, second_top + 1)
            .is_empty()
    );
    let pad_rows = usize::try_from(painted.pad).expect("padding");
    assert_eq!(
        painted
            .color_rows(background, painted.inset(), painted.surface_h)
            .len(),
        pad_rows + 4,
        "the bottom strip has the larger share of the odd slack"
    );
}

// ---------------------------------------------------------------------------
// Everything else keeps its geometry
// ---------------------------------------------------------------------------

#[test]
fn centering_preserves_container_and_grid_sizes() {
    // The fix is paint-and-routing only: the reserved rows, the layout
    // container, and the leaf grid must stay the numbers a pre-#3263 tree
    // produced (one window row per band, no more, no less).
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
    let rows_with_band = primary_rows(&rt);
    let rows_without_band = primary_rows(&control);
    assert_eq!(
        rows_with_band + 1,
        rows_without_band,
        "the band still costs exactly one leaf row"
    );
    assert_eq!(
        usize::from(rows_with_band),
        rt.snapshot().height,
        "the primary grid follows the reduced leaf"
    );
}

// The winsize is only observable with a real child, so this leg is POSIX-only
// (same gate as `host_chrome_gaps.rs`): spawning `/bin/sh` is the fixture.
#[cfg(unix)]
#[test]
fn centering_preserves_pty_winsize() {
    let banded = |rt: &mut Runtime| {
        rt.set_chrome_bands(ChromeBands {
            bottom: vec![bottom_band("bar", "status bar", "bright_red")],
            ..Default::default()
        });
    };
    let mut rt = fixture(ROWS, 7);
    let _ = rt.tick();
    rt.spawn_shell("/bin/sh").expect("primary shell must spawn");
    let _ = rt.tick();
    let bare_winsize = rt.pty_size().expect("the shell owns a winsize");

    // Reserving a bottom band costs exactly one row on the non-multiple
    // surface. The slack is outside the grid used for PTY sizing.
    banded(&mut rt);
    rt.tick().expect("band tick reflows");
    let winsize = rt.pty_size().expect("the shell still owns a winsize");
    assert_eq!(
        winsize.0, bare_winsize.0,
        "the band reserves rows, never columns"
    );
    assert_eq!(
        winsize.1,
        bare_winsize.1 - 1,
        "the band costs exactly one PTY row, as before"
    );
    let snapshot = rt.snapshot();
    assert_eq!(
        winsize,
        (
            u16::try_from(snapshot.width).expect("columns"),
            u16::try_from(snapshot.height).expect("rows")
        ),
        "the kernel winsize equals the band-reduced primary grid"
    );
    assert_eq!(
        rt.band_row_container().height,
        rt.window_cells().height.saturating_sub(1),
        "the container keeps the band row outside the leaf"
    );
    let pixel_winsize = rt.pty_pixel_size().expect("pixel winsize");
    let (width, height) = extent_for(&rt, ROWS, 0);
    resize(&mut rt, width, height);
    assert_eq!(
        rt.pty_size().expect("winsize after pixel-only resize"),
        winsize
    );
    assert_eq!(rt.pty_pixel_size(), Some(pixel_winsize));
}

// ---------------------------------------------------------------------------
// bitty#3282: the glyph row, not just the fill
// ---------------------------------------------------------------------------

#[test]
fn bottom_band_glyphs_sit_in_the_painted_cell_row() {
    // This regression pins the glyph row itself. The deterministic
    // rasterizer paints the same ink for the same text on every surface, so an
    // exact-multiple control states the ink offsets inside a normal cell row;
    // the remainder fixture must paint them at exactly those offsets inside
    // the painted cell row. Blended glyph pixels never equal a theme token,
    // so the observable is a pixel over the band's own fill that differs from
    // it (the probe columns carry the text, the chrome rule is skipped by
    // starting one row below the painted top).
    let remainder = 7;
    let mut rt = fixture(ROWS, remainder);
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    rt.tick().expect("tick presents the band");
    assert_eq!(rt.band_host_stats().paint_violations, 0);
    let painted = Painted::capture(&rt);
    let bg = token(&rt, "bright_red");
    let inset = painted.inset();
    let content_top = inset - painted.cell_h();

    let mut control = fixture(ROWS, 0);
    control.set_chrome_bands(ChromeBands {
        bottom: vec![bottom_band("bar", "status bar", "bright_red")],
        ..Default::default()
    });
    control.tick().expect("control presents the band");
    let control = Painted::capture(&control);
    let control_top = control.inset() - control.cell_h();

    let control_ink = control.ink_rows_over_fill(bg, control_top + 1, control.inset());
    assert!(
        !control_ink.is_empty(),
        "the control band paints glyph ink over its fill"
    );
    let band_ink = painted.ink_rows_over_fill(bg, content_top + 1, inset);
    assert_eq!(
        band_ink.len(),
        control_ink.len(),
        "the band paints its glyphs over the painted cell row's fill"
    );
    let first = i32::try_from(band_ink[0]).expect("row fits i32");
    let last = i32::try_from(*band_ink.last().expect("ink rows")).expect("row fits i32");
    let control_first = i32::try_from(control_ink[0]).expect("row fits i32");
    let control_last = i32::try_from(*control_ink.last().expect("ink rows")).expect("row fits i32");
    assert_eq!(
        first - content_top,
        control_first - control_top,
        "the glyphs start at the same offset within the cell row"
    );
    assert_eq!(
        last - content_top,
        control_last - control_top,
        "the glyphs end at the same offset within the cell row"
    );
}
