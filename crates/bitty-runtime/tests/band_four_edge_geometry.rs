#![forbid(unsafe_code)]
//! bitty#3177 (bitty#2478 item 4): one acceptance test for the four-edge
//! chrome band layout.
//!
//! Every existing band test mounts at most two edges: the `left` column band
//! alone, the `right` column band alone, or `top` + `left` together
//! (`crates/bitty-runtime/src/runtime/band_host.rs`), plus per-edge input
//! routing (`tests/band_column_input.rs`). Nothing proved that the four
//! reservations compose: that the exclusive container loses `top` + `bottom`
//! rows *and* `left` + `right` columns in one solve, that the four granted
//! rects stay disjoint from each other and from the content, and that input
//! still resolves to the correct edge with all four mounted.
//!
//! The frame below is the vision's composition — a `left` tree, a `right`
//! inspector, a `bottom` status band, and a `top` bar — mounted from four
//! distinct plugin ids on a real `Runtime`.
//!
//! Expected geometry is written as literal constants derived from the window
//! size this test chooses plus the declared band extents. It is deliberately
//! *not* recomputed through the same host helpers the production code uses
//! (`plugin_band_column_budget`, `band_row_container`, ...), so a broken
//! reservation cannot satisfy an assertion that re-derives it.

use bitty_lua::ui::{ClickArg, ClickCommand, UiNode, UiRegion, UiRegionKind, UiSlot};
use bitty_platform::{CursorPosition, MouseButton, PressState};
use bitty_runtime::{
    BandContent, BandEdge, BandHoverPhase, BandMount, ChromeBands, Runtime, UiRect,
};

/// Window grid the acceptance frame is solved in, in cells: 120 columns and
/// 40 rows. Both are chosen by the test, not by the runtime defaults.
const WINDOW: UiRect = UiRect::new(0, 0, 120, 40);

/// Columns the `left` band declares (the width of its widest root child).
const LEFT_W: u16 = 8;
/// Columns the `right` band declares.
const RIGHT_W: u16 = 11;
/// Rows a row band reserves: one window row per band, on each horizontal edge.
const ROW_BAND_ROWS: u16 = 1;

const TOP_PLUGIN: &str = "aa-top";
const BOTTOM_PLUGIN: &str = "bb-bottom";
const LEFT_PLUGIN: &str = "cc-left";
const RIGHT_PLUGIN: &str = "dd-right";

/// Physical center of window cell `(row, col)`.
fn cell_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released)
}

/// One clickable, hoverable band item in `plugin`'s namespace.
fn item(plugin: &str, label: &str, id: &str) -> UiNode {
    let region = UiRegion {
        id: id.to_string(),
        kind: UiRegionKind::Item,
    };
    let command = |verb: &str| {
        Some(ClickCommand {
            command: format!("{plugin}:{verb}"),
            args: vec![("id".to_string(), ClickArg::String(id.to_string()))],
            region: Some(region.clone()),
        })
    };
    UiNode::Text {
        text: label.to_string(),
        fg: None,
        bg: None,
        bold: None,
        on_click: command("focus"),
        on_scroll: None,
        anchor: None,
        on_hover: command("detail"),
    }
}

fn band(plugin: &str, slot: UiSlot, root: UiNode) -> BandContent {
    BandContent {
        plugin_id: plugin.to_string(),
        slot,
        root,
        version: 1,
    }
}

fn mount_of(plugin: &str, slot: UiSlot) -> BandMount {
    BandMount {
        plugin_id: plugin.to_string(),
        slot,
        version: 1,
        generation: 1,
        handle: 1,
    }
}

/// Mounts one band on each of the four edges, from four distinct plugins.
fn mount_four_edges(rt: &mut Runtime) {
    rt.set_chrome_bands_with_mounts(
        ChromeBands {
            top: vec![band(
                TOP_PLUGIN,
                UiSlot::Top,
                UiNode::row(vec![item(TOP_PLUGIN, "topband", "top:item")]),
            )],
            bottom: vec![band(
                BOTTOM_PLUGIN,
                UiSlot::Bottom,
                UiNode::row(vec![item(BOTTOM_PLUGIN, "statusbar", "bottom:item")]),
            )],
            left: vec![band(
                LEFT_PLUGIN,
                UiSlot::Left,
                UiNode::column(vec![item(LEFT_PLUGIN, "leftband", "left:item")]),
            )],
            right: vec![band(
                RIGHT_PLUGIN,
                UiSlot::Right,
                UiNode::column(vec![item(RIGHT_PLUGIN, "inspector!!", "right:item")]),
            )],
        },
        vec![
            mount_of(TOP_PLUGIN, UiSlot::Top),
            mount_of(BOTTOM_PLUGIN, UiSlot::Bottom),
            mount_of(LEFT_PLUGIN, UiSlot::Left),
            mount_of(RIGHT_PLUGIN, UiSlot::Right),
        ],
    );
}

/// Presents one frame, which publishes the four bands' origins (the production
/// boundary: only a presented frame commits origins as executable).
fn present(rt: &mut Runtime) {
    rt.tick().expect("tick presents all four bands");
}

/// Content rect implied by the window and the four declared extents.
fn expected_content() -> UiRect {
    UiRect::new(
        WINDOW.x + LEFT_W,
        WINDOW.y + ROW_BAND_ROWS,
        WINDOW.width - LEFT_W - RIGHT_W,
        WINDOW.height - 2 * ROW_BAND_ROWS,
    )
}

/// Granted rect of one band, built from the window and the declared extents.
fn expected_band_rect(edge: BandEdge) -> UiRect {
    let content = expected_content();
    match edge {
        BandEdge::Top => UiRect::new(WINDOW.x, WINDOW.y, WINDOW.width, ROW_BAND_ROWS),
        BandEdge::Bottom => UiRect::new(
            WINDOW.x,
            WINDOW.y + WINDOW.height - ROW_BAND_ROWS,
            WINDOW.width,
            ROW_BAND_ROWS,
        ),
        BandEdge::Left => UiRect::new(WINDOW.x, content.y, LEFT_W, content.height),
        BandEdge::Right => UiRect::new(
            WINDOW.x + WINDOW.width - RIGHT_W,
            content.y,
            RIGHT_W,
            content.height,
        ),
    }
}

/// Slot that mounts onto `edge`, spelled out so the test does not lean on a
/// runtime mapping helper.
fn edge_slot(edge: BandEdge) -> UiSlot {
    match edge {
        BandEdge::Top => UiSlot::Top,
        BandEdge::Bottom => UiSlot::Bottom,
        BandEdge::Left => UiSlot::Left,
        BandEdge::Right => UiSlot::Right,
    }
}

/// Overlap test on half-open cell ranges; zero-extent rects touch nothing.
fn intersects(a: UiRect, b: UiRect) -> bool {
    a.x < b.x.saturating_add(b.width)
        && b.x < a.x.saturating_add(a.width)
        && a.y < b.y.saturating_add(b.height)
        && b.y < a.y.saturating_add(a.height)
}

#[test]
fn four_edges_reserve_their_own_extent_and_compose_one_content_rect() {
    // Acceptance 1 and 2: each edge reserves exactly its declared extent from
    // the correct window edge, and the content rect is the window minus all
    // four reservations at once — not just the last one applied.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    rt.set_window_cells(WINDOW);
    assert_eq!(rt.window_cells(), WINDOW, "the test chose the window grid");
    mount_four_edges(&mut rt);
    present(&mut rt);

    // Horizontal edges: one band each, one window row each.
    assert_eq!(rt.plugin_band_budget(), (1, 1));
    assert_eq!(rt.visible_band_count(BandEdge::Top), 1);
    assert_eq!(rt.visible_band_count(BandEdge::Bottom), 1);
    assert_eq!(rt.visible_band_row(BandEdge::Top, 0), Some(WINDOW.y));
    assert_eq!(
        rt.visible_band_row(BandEdge::Bottom, 0),
        Some(WINDOW.y + WINDOW.height - 1)
    );
    assert_eq!(rt.visible_band_row(BandEdge::Top, 1), None);
    assert_eq!(rt.visible_band_row(BandEdge::Bottom, 1), None);

    // Vertical edges: the declared widths, measured from their own window edge.
    assert_eq!(rt.plugin_band_column_budget(), (LEFT_W, RIGHT_W));
    assert_eq!(rt.visible_band_column_count(BandEdge::Left), 1);
    assert_eq!(rt.visible_band_column_count(BandEdge::Right), 1);
    assert_eq!(rt.visible_band_column(BandEdge::Left, 0), Some(WINDOW.x));
    assert_eq!(
        rt.visible_band_column(BandEdge::Right, 0),
        Some(WINDOW.x + WINDOW.width - RIGHT_W)
    );
    assert_eq!(rt.visible_band_column(BandEdge::Left, 1), None);
    assert_eq!(rt.visible_band_column(BandEdge::Right, 1), None);

    // The row carve loses both horizontal reservations...
    assert_eq!(
        rt.band_row_container(),
        UiRect::new(
            WINDOW.x,
            WINDOW.y + ROW_BAND_ROWS,
            WINDOW.width,
            WINDOW.height - 2 * ROW_BAND_ROWS
        )
    );
    // ...and the full carve loses the vertical reservations from that rect, so
    // the four reservations compose on both axes at once.
    let content = expected_content();
    assert_eq!(content, UiRect::new(8, 1, 101, 38), "hand-computed frame");
    assert_eq!(rt.band_exclusive_container(), content);
    // The reflowed layout container the leaves and PTYs are sized from agrees.
    assert_eq!(rt.container(), content);
    assert_eq!(
        content.width + LEFT_W + RIGHT_W,
        WINDOW.width,
        "both column reservations left the content width"
    );
    assert_eq!(
        content.height + 2 * ROW_BAND_ROWS,
        WINDOW.height,
        "both row reservations left the content height"
    );
    assert!(
        !rt.band_geometry_violated(),
        "four mounted bands must not violate exclusive geometry"
    );
}

#[test]
fn four_edge_band_rects_are_disjoint_from_each_other_and_the_content() {
    // Acceptance 2 with explicit rects: the four bands and the content
    // partition the window. Asserting pairwise non-overlap on independently
    // built rects pins each band's corner as well as its extent, so a band
    // shifted onto a neighbour fails here even if every budget still matches.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    rt.set_window_cells(WINDOW);
    mount_four_edges(&mut rt);
    present(&mut rt);

    let content = expected_content();
    let rects = [
        (BandEdge::Top, expected_band_rect(BandEdge::Top)),
        (BandEdge::Bottom, expected_band_rect(BandEdge::Bottom)),
        (BandEdge::Left, expected_band_rect(BandEdge::Left)),
        (BandEdge::Right, expected_band_rect(BandEdge::Right)),
        (BandEdge::Top, content),
    ];
    // Every band rect and the content rect sit inside the window. The window
    // origin is the grid origin, so containment is the upper bound only.
    for (index, (edge, rect)) in rects.iter().enumerate() {
        assert!(
            rect.x.saturating_add(rect.width) <= WINDOW.x + WINDOW.width
                && rect.y.saturating_add(rect.height) <= WINDOW.y + WINDOW.height,
            "{edge:?} rect {rect:?} escapes the window"
        );
        // Pairwise disjoint (the last entry is the content rect).
        for (other_edge, other) in rects.iter().skip(index + 1) {
            assert!(
                !intersects(*rect, *other),
                "{edge:?} {rect:?} overlaps {other_edge:?} {other:?}"
            );
        }
    }
    assert_eq!(content, UiRect::new(8, 1, 101, 38));

    // The production per-item region table agrees with those rects: the one
    // item of each band lands on its own band's line and inside its own band
    // width, never on a neighbour's.
    for edge in [
        BandEdge::Top,
        BandEdge::Bottom,
        BandEdge::Left,
        BandEdge::Right,
    ] {
        let band_rect = expected_band_rect(edge);
        let regions = rt
            .band_item_regions(edge, 0)
            .unwrap_or_else(|error| panic!("{edge:?} must emit a region: {error:?}"));
        assert_eq!(regions.len(), 1, "{edge:?} has one root child");
        let region = &regions[0];
        assert_eq!(region.edge, edge, "{edge:?} region carries its edge");
        assert_eq!(
            region.row, band_rect.y,
            "{edge:?} region paints on its own band line"
        );
        // Row-band columns are window-relative; column-band columns are
        // measured from the band's own left edge, and both start at the band.
        let (start, end) = (region.start_col, region.end_col);
        assert!(
            start < end && end <= usize::from(band_rect.width),
            "{edge:?} region {start}..{end} must fit its {} granted columns",
            band_rect.width
        );
        assert_eq!(start, 0, "{edge:?} first item starts at its band origin");
    }
}

#[test]
fn routing_resolves_each_edge_with_all_four_mounted() {
    // Acceptance 3: with all four bands mounted, a click and a hover in each
    // band resolve to that band's own edge and plugin — never to a neighbour
    // and never to the terminal.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    rt.set_window_cells(WINDOW);
    mount_four_edges(&mut rt);
    present(&mut rt);

    let content = expected_content();
    // One target cell inside each band, derived from the explicit rects.
    let targets = [
        (BandEdge::Top, WINDOW.y, WINDOW.x + 1, TOP_PLUGIN),
        (
            BandEdge::Bottom,
            WINDOW.y + WINDOW.height - 1,
            WINDOW.x + 1,
            BOTTOM_PLUGIN,
        ),
        (BandEdge::Left, content.y, WINDOW.x + 1, LEFT_PLUGIN),
        (
            BandEdge::Right,
            content.y,
            WINDOW.x + WINDOW.width - RIGHT_W + 1,
            RIGHT_PLUGIN,
        ),
    ];

    for (edge, row, col, plugin) in targets {
        rt.handle_cursor_moved(cell_pixels(&rt, row, col));
        rt.handle_mouse_input(press());
        rt.handle_mouse_input(release());
        let clicks = rt.drain_band_clicks();
        assert_eq!(clicks.len(), 1, "{edge:?} click routes exactly once");
        assert_eq!(clicks[0].plugin_id, plugin, "{edge:?} click owner");
        assert_eq!(clicks[0].command, "focus");
        let origin = clicks[0]
            .origin
            .as_ref()
            .unwrap_or_else(|| panic!("{edge:?} click carries a presented origin"));
        assert_eq!(origin.edge, edge, "{edge:?} click resolves its own edge");
        assert_eq!(origin.slot, edge_slot(edge), "{edge:?} click slot matches");
        match edge {
            BandEdge::Top | BandEdge::Bottom => {
                let expected = if edge == BandEdge::Top {
                    WINDOW.y
                } else {
                    WINDOW.y + WINDOW.height - 1
                };
                assert_eq!(origin.row, expected, "{edge:?} origin row");
                assert_eq!(origin.column, WINDOW.x, "{edge:?} row band spans the width");
            }
            BandEdge::Left | BandEdge::Right => {
                assert_eq!(origin.row, content.y, "{edge:?} origin row");
                assert_eq!(
                    origin.column,
                    expected_band_rect(edge).x,
                    "{edge:?} origin column is its own granted column"
                );
            }
        }
    }

    // The click pass moved the cursor across the bands, so clear whatever
    // hover transitions it queued before hovering each edge deliberately.
    let _ = rt.drain_band_hovers();

    // The hover path resolves the same way, one enter transition per edge.
    for (edge, row, col, plugin) in targets {
        rt.handle_cursor_moved(cell_pixels(&rt, row, col));
        let enters: Vec<_> = rt
            .drain_band_hovers()
            .into_iter()
            .filter(|event| event.phase == BandHoverPhase::Enter)
            .collect();
        assert_eq!(enters.len(), 1, "{edge:?} hover enters once");
        assert_eq!(enters[0].plugin_id, plugin, "{edge:?} hover owner");
        assert_eq!(enters[0].command, "detail");
        let origin = enters[0]
            .origin
            .as_ref()
            .unwrap_or_else(|| panic!("{edge:?} hover carries a presented origin"));
        assert_eq!(origin.edge, edge, "{edge:?} hover resolves its own edge");
    }
    assert_eq!(rt.band_host_stats().clicks_routed, 4);
}

#[test]
fn four_edges_in_a_window_too_small_for_them_degrade_without_underflow() {
    // Acceptance 4 (the boundary this build actually guarantees): a window
    // too small for any reservation degrades **every** edge to zero rather
    // than clipping a band into the content or underflowing a rect. The row
    // budget refuses when `top + bottom + content floor` exceeds the window
    // height, and the column plan refuses when each side's share of the
    // spare columns is zero, so both axes fall back to the whole window.
    //
    // The build has no typed "unfittable" refusal for a band mount; the
    // documented contract is total degradation (see `plugin_band_budget` and
    // `column_band_plan` in `crates/bitty-runtime/src/runtime/band_host.rs`).
    // This test pins that: no panic, no reservation, no negative extent.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let tiny = UiRect::new(0, 0, 2, 2);
    rt.set_window_cells(tiny);
    mount_four_edges(&mut rt);
    // A fully degraded frame has no band rect to paint, so the present path
    // may legitimately produce no frame; what matters is that it never panics.
    let _ = rt.tick();

    assert_eq!(
        rt.plugin_band_budget(),
        (0, 0),
        "two row bands plus the content floor cannot fit two rows"
    );
    assert_eq!(
        rt.plugin_band_column_budget(),
        (0, 0),
        "two columns leave each side no spare column to share"
    );
    for edge in [
        BandEdge::Top,
        BandEdge::Bottom,
        BandEdge::Left,
        BandEdge::Right,
    ] {
        assert_eq!(rt.visible_band_count(edge), 0, "{edge:?} reserves no rows");
        assert_eq!(
            rt.visible_band_column_count(edge),
            0,
            "{edge:?} reserves no columns"
        );
        assert_eq!(rt.visible_band_row(edge, 0), None, "{edge:?} row");
        assert_eq!(rt.visible_band_column(edge, 0), None, "{edge:?} column");
    }
    assert_eq!(
        rt.band_exclusive_container(),
        tiny,
        "a fully degraded frame hands the whole window to the content"
    );
    assert_eq!(rt.container(), tiny);
    assert!(!rt.band_geometry_violated());

    // The same four mounts in a window too narrow for both declared column
    // widths is bounded, not underflowed: each side is granted its half of
    // the spare columns, the bands clip into that share, and the content keeps
    // at least half the window.
    let narrow = UiRect::new(0, 0, 24, 10);
    rt.set_window_cells(narrow);
    mount_four_edges(&mut rt);
    let _ = rt.tick();
    let (left, right) = rt.plugin_band_column_budget();
    assert!(
        left > 0 && right > 0,
        "both sides are still served in a narrow window"
    );
    assert!(
        left <= LEFT_W && right <= RIGHT_W,
        "a reservation never exceeds the band's declared extent"
    );
    assert!(
        left < LEFT_W && right < RIGHT_W,
        "both bands clip into their share here, which is the bounded case"
    );
    let content = rt.band_exclusive_container();
    assert_eq!(
        content.width + left + right,
        narrow.width,
        "the column carve is exact at any window width"
    );
    assert_eq!(
        content.height + 2 * ROW_BAND_ROWS,
        narrow.height,
        "the row carve is exact at any window width"
    );
    assert!(
        content.width >= narrow.width.div_ceil(2),
        "the content keeps at least half the window"
    );
    assert!(!rt.band_geometry_violated());
}
