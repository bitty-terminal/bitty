#![forbid(unsafe_code)]
//! Issue #3171: input routing for the vertical (`left`/`right`) chrome bands.
//!
//! #3167 made a column band paintable and space-reserving, but nothing routed
//! input to it: `plugin_band_row_hit` walked rows only, `band_item_regions`
//! required a row, and `rebuild_pending_band_origins` published no origin for a
//! column band. Every case here goes through the real pointer path
//! (`handle_cursor_moved`/`handle_mouse_input`/`handle_wheel`) and asserts the
//! drain-queue contract the application dispatches:
//!
//! - a click on an item inside a `left` band routes that item's command,
//! - hover inside a `right` band resolves its item with a presented origin,
//! - a wheel tick inside a column band routes its `on_scroll` binding,
//! - a click outside every band still falls through to the terminal, while the
//!   band's whole reserved column stays chrome,
//! - column bands publish origins carrying their column geometry, and a
//!   reflow that moves the column invalidates the captured origin,
//! - per-item regions for a column band are non-empty, one line per root child,
//!   and bounded by the band's granted width,
//! - a band wider than its share is clipped to the share (not degraded to an
//!   invisible band) and still routes.

use bitty_lua::ui::{ClickArg, ClickCommand, UiNode, UiRegion, UiRegionKind, UiSlot};
use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState, ScrollDelta};
use bitty_runtime::{
    BAND_COLUMN_CONTENT_SHARE_DIVISOR, BandContent, BandEdge, BandHoverPhase, BandMount,
    BandRegionError, ChromeBands, Runtime,
};

const PLUGIN: &str = "test-bar";

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

fn click_at(rt: &mut Runtime, row: u16, col: u16) {
    rt.handle_cursor_moved(cell_pixels(rt, row, col));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
}

/// One clickable band item, optionally hoverable and scrollable.
fn item(label: &str, id: &str, hover: bool, scroll: bool) -> UiNode {
    let region = UiRegion {
        id: id.to_string(),
        kind: UiRegionKind::Item,
    };
    let command = |verb: &str| {
        Some(ClickCommand {
            command: format!("{PLUGIN}:{verb}"),
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
        on_scroll: if scroll { command("step") } else { None },
        anchor: None,
        on_hover: if hover { command("detail") } else { None },
    }
}

/// A column-shaped band whose root stacks `rows` top-down.
fn column_band(slot: UiSlot, rows: Vec<UiNode>) -> BandContent {
    BandContent {
        plugin_id: PLUGIN.to_string(),
        slot,
        root: UiNode::column(rows),
        version: 1,
    }
}

fn band_on(slot: UiSlot, band: BandContent) -> ChromeBands {
    match slot {
        UiSlot::Left => ChromeBands {
            left: vec![band],
            ..Default::default()
        },
        UiSlot::Right => ChromeBands {
            right: vec![band],
            ..Default::default()
        },
        UiSlot::Top => ChromeBands {
            top: vec![band],
            ..Default::default()
        },
        UiSlot::Bottom => ChromeBands {
            bottom: vec![band],
            ..Default::default()
        },
        _ => panic!("{slot} is not a band slot"),
    }
}

/// Mounts `band` on `slot` with a live mount projection and presents it, so
/// routing runs against a real published origin (the production path). The
/// application re-pushes this projection every tick (see
/// `TerminalApp::drive_tick`), which is what rebuilds origins after a reflow.
fn mount(rt: &mut Runtime, slot: UiSlot, band: BandContent) {
    push_band(rt, slot, band);
    rt.tick().expect("tick presents the band");
}

/// Installs `band` and its live mount projection without presenting.
fn push_band(rt: &mut Runtime, slot: UiSlot, band: BandContent) {
    rt.set_chrome_bands_with_mounts(
        band_on(slot, band),
        vec![BandMount {
            plugin_id: PLUGIN.to_string(),
            slot,
            version: 1,
            generation: 1,
            handle: 1,
        }],
    );
}

#[test]
fn click_on_an_item_inside_a_left_band_routes_it() {
    // Acceptance 1: a click on an item inside a `left` band hits that item,
    // through the same press/release pairing a row band uses.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(
            UiSlot::Left,
            vec![
                item("cpu", "item:cpu", false, false),
                item("mem", "item:mem", false, false),
            ],
        ),
    );
    let area_y = rt.band_row_container().y;
    assert_eq!(rt.visible_band_column(BandEdge::Left, 0), Some(0));
    let focused = rt.focused_view();

    // The second content line (row offset 1) owns the `mem` item.
    click_at(&mut rt, area_y.saturating_add(1), 1);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "one claimed click routes once");
    assert_eq!(clicks[0].plugin_id, PLUGIN);
    assert_eq!(clicks[0].command, "focus");
    assert_eq!(
        clicks[0].region.as_ref().map(|region| region.id.as_str()),
        Some("item:mem"),
        "the click resolves the item on its own content line"
    );
    let origin = clicks[0].origin.as_ref().expect("presented origin");
    assert_eq!(origin.edge, BandEdge::Left);
    assert_eq!(origin.column, 0);
    assert_eq!(origin.visible_index, 0);
    // Chrome owns the gesture: no focus move, no selection.
    assert_eq!(rt.focused_view(), focused, "band click moves no focus");
    assert!(!rt.has_selection(), "band click starts no selection");
    assert_eq!(rt.band_host_stats().clicks_routed, 1);
}

#[test]
fn hover_inside_a_right_band_resolves_the_item() {
    // Acceptance 2: hover inside a `right` band resolves an item and carries
    // the band's presented origin, exactly as a row band's hover does.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    mount(
        &mut rt,
        UiSlot::Right,
        column_band(UiSlot::Right, vec![item("net", "net:0", true, false)]),
    );
    let column = rt
        .visible_band_column(BandEdge::Right, 0)
        .expect("the right band is granted columns");
    let area_y = rt.band_row_container().y;

    rt.handle_cursor_moved(cell_pixels(&rt, area_y, column));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1, "one enter transition");
    assert_eq!(events[0].plugin_id, PLUGIN);
    assert_eq!(events[0].command, "detail");
    assert_eq!(events[0].phase, BandHoverPhase::Enter);
    assert_eq!(events[0].region.id, "net:0");
    let origin = events[0].origin.as_ref().expect("presented origin");
    assert_eq!(origin.edge, BandEdge::Right);
    assert_eq!(origin.column, column);
    assert!(
        !rt.band_release_armed(),
        "hover never arms a release swallow"
    );

    // Steady hover inside the same item is idle.
    rt.handle_cursor_moved(cell_pixels(&rt, area_y, column));
    assert!(rt.drain_band_hovers().is_empty(), "steady hover is idle");
}

#[test]
fn wheel_over_a_column_band_item_routes_its_scroll() {
    // Acceptance 1 for the scroll surface: a wheel tick resolves through the
    // same band line mapping the click path uses.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(UiSlot::Left, vec![item("vol", "vol", false, true)]),
    );
    let area_y = rt.band_row_container().y;
    rt.handle_cursor_moved(cell_pixels(&rt, area_y, 1));
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let scrolls = rt.drain_band_scrolls();
    assert_eq!(scrolls.len(), 1, "one claimed wheel tick routes once");
    assert_eq!(scrolls[0].plugin_id, PLUGIN);
    assert_eq!(scrolls[0].command, "step");
    assert_eq!(scrolls[0].delta, 3, "host-computed lines for one notch");
    assert!(scrolls[0].origin.is_some(), "presented origin captured");
    assert_eq!(rt.band_host_stats().scrolls_routed, 1);
}

#[test]
fn click_outside_every_band_falls_through_to_the_terminal() {
    // Acceptance 5: the reserved column is chrome, the content area is not.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(UiSlot::Left, vec![item("cpu", "item:cpu", false, false)]),
    );
    let container = rt.band_exclusive_container();
    let area_y = rt.band_row_container().y;

    // Inside the band's granted column but below its single content line: the
    // press is consumed as chrome (no selection) and claims nothing.
    let below = area_y.saturating_add(3);
    rt.handle_cursor_moved(cell_pixels(&rt, below, 1));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(cell_pixels(&rt, below, 2));
    rt.handle_mouse_input(release());
    assert!(
        !rt.has_selection(),
        "a press in the band's column is Core chrome"
    );
    assert!(
        rt.drain_band_clicks().is_empty(),
        "cells past the band text claim nothing"
    );
    assert_eq!(rt.band_host_stats().clicks_unclaimed, 1);

    // Inside the content area: nothing is routed and the selection still runs.
    let row = container.y.saturating_add(2);
    let col = container.x.saturating_add(4);
    rt.handle_cursor_moved(cell_pixels(&rt, row, col));
    rt.handle_mouse_input(press());
    rt.handle_cursor_moved(cell_pixels(&rt, row, col.saturating_add(5)));
    rt.handle_mouse_input(release());
    assert!(
        rt.drain_band_clicks().is_empty(),
        "content clicks route no band command"
    );
    assert!(rt.has_selection(), "content press-drag still selects");
}

#[test]
fn column_band_origins_are_published_with_column_geometry() {
    // Acceptance 3: a column band publishes a `BandOrigin` whose geometry is
    // its column, and the origin is current while the band is unchanged.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let window = rt.window_cells();
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(UiSlot::Left, vec![item("cpu", "cpu", false, false)]),
    );
    let area_y = rt.band_row_container().y;
    let origins = rt.published_band_origins();
    assert_eq!(origins.len(), 1, "one column band publishes one origin");
    let origin = &origins[0];
    assert_eq!(origin.edge, BandEdge::Left);
    assert_eq!(origin.slot, UiSlot::Left);
    assert_eq!(origin.column, 0);
    assert_eq!(origin.row, area_y);
    assert_eq!(origin.visible_index, 0);
    assert!(rt.band_origin_is_current(origin));

    // A reflow that moves the band's column invalidates the captured origin
    // instead of retargeting it: mount a `right` band and narrow the window.
    let right_band = || column_band(UiSlot::Right, vec![item("net", "net", false, false)]);
    push_band(&mut rt, UiSlot::Right, right_band());
    rt.tick().expect("tick presents the right band");
    let wide = rt
        .published_band_origins()
        .iter()
        .find(|origin| origin.edge == BandEdge::Right)
        .cloned()
        .expect("right band origin");
    assert_eq!(wide.column, window.width - 3);

    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(
        cw.saturating_mul(u32::from(window.width) / 2)
            .saturating_add(pad.saturating_mul(2)),
        ch.saturating_mul(u32::from(window.height))
            .saturating_add(pad.saturating_mul(2)),
    ))
    .expect("resize");
    rt.tick().expect("tick after resize");
    assert_eq!(
        rt.window_cells().width,
        window.width / 2,
        "the window narrowed"
    );
    assert!(
        !rt.band_origin_is_current(&wide),
        "a reflowed column band refuses its stale origin"
    );
    assert_eq!(
        rt.visible_band_column(BandEdge::Right, 0),
        Some(window.width / 2 - 3),
        "the band moved to the new window edge"
    );

    // The application re-pushes the mount projection every tick, so the next
    // frame publishes an origin at the new column and routing resumes.
    push_band(&mut rt, UiSlot::Right, right_band());
    rt.tick().expect("tick republishes origins");
    let narrowed = rt
        .published_band_origins()
        .iter()
        .find(|origin| origin.edge == BandEdge::Right)
        .cloned()
        .expect("right band origin after reflow");
    assert_eq!(narrowed.column, window.width / 2 - 3);
    assert!(rt.band_origin_is_current(&narrowed));
    assert!(
        !rt.band_origin_is_current(&wide),
        "the old origin stays refused"
    );
}

#[test]
fn column_band_item_regions_are_non_empty_and_bounded() {
    // Acceptance 4: regions for a column band exist, one line per root child,
    // clipped to the band's granted width — not an empty list, not an error.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(
            UiSlot::Left,
            vec![
                item("ws1", "ws:1", false, false),
                item("clocklong", "clock", false, false),
            ],
        ),
    );
    let area_y = rt.band_row_container().y;
    let width = rt.plugin_band_column_budget().0;
    assert_eq!(width, 9, "the widest root child sets the band width");

    let regions = rt
        .band_item_regions(BandEdge::Left, 0)
        .expect("a column band emits regions");
    assert_eq!(regions.len(), 2, "one region per root child");
    assert_eq!(regions[0].row, area_y);
    assert_eq!(regions[1].row, area_y.saturating_add(1));
    assert_eq!(
        (regions[0].start_col, regions[0].end_col),
        (0, 3),
        "the first line's item is clipped to its own text"
    );
    assert_eq!(regions[1].region.id, "clock");
    for region in &regions {
        assert_eq!(region.edge, BandEdge::Left);
        assert_eq!(region.plugin_id, PLUGIN);
        assert_eq!(region.version, 1);
        assert!(
            region.start_col < region.end_col && region.end_col <= usize::from(width),
            "region {:?} is bounded by the granted width",
            (region.start_col, region.end_col)
        );
    }

    // The typed error set is unchanged: an unoccupied index is still
    // `MissingBand`, on both a row edge and past the column band's index.
    assert_eq!(
        rt.band_item_regions(BandEdge::Top, 0),
        Err(BandRegionError::MissingBand)
    );
    assert_eq!(
        rt.band_item_regions(BandEdge::Left, 1),
        Err(BandRegionError::MissingBand)
    );
}

#[test]
fn column_band_wider_than_its_share_is_clipped_and_still_routes() {
    // Acceptance 6 (the unfittable-band decision): a scene wider than the
    // content share is granted the share and served there, instead of being
    // admitted as an invisible band. A 79-column line in an 80-column window
    // must leave the content half the window, not one column.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let window = rt.window_cells();
    let wide = "x".repeat(usize::from(window.width).saturating_sub(1));
    mount(
        &mut rt,
        UiSlot::Left,
        column_band(UiSlot::Left, vec![item(&wide, "wide", false, false)]),
    );
    let share = window.width / BAND_COLUMN_CONTENT_SHARE_DIVISOR;
    assert_eq!(
        rt.plugin_band_column_budget(),
        (share, 0),
        "the band is clipped to its share, never degraded to zero"
    );
    let container = rt.band_exclusive_container();
    assert_eq!(container.width, window.width - share);
    assert!(
        container.width >= window.width / 2,
        "the content keeps at least half the window"
    );

    // Visible and routable at the clipped width.
    let regions = rt
        .band_item_regions(BandEdge::Left, 0)
        .expect("the clipped band still emits regions");
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].end_col, usize::from(share));
    let area_y = rt.band_row_container().y;
    click_at(&mut rt, area_y, 1);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "the clipped band still routes its item");
    assert_eq!(
        clicks[0].region.as_ref().map(|region| region.id.as_str()),
        Some("wide")
    );
}
