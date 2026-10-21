#![forbid(unsafe_code)]
//! Issue #2411: Waybar-style band groups, bounded hover events, and a button
//! routing audit, all through Core-owned pointer geometry.
//!
//! - Group alignment: a band root `Row` resolves `left`/`center`/`right`
//!   anchors against the presented width, so the center group is centered on
//!   the full row (never "after left") and the right group is flush to the
//!   inset edge at 80/120/200 columns. Click/scroll/hover hit regions follow
//!   the same width-aware runs, gap cells stay unclaimed, and over-subscribed
//!   narrow rows clip deterministically instead of overlapping or panicking.
//! - Hover: `on_hover` on a region-bound band node queues at most one
//!   transition per region per drain, carries only the stable region id and
//!   an `enter`/`leave` phase, and never arms capture or a modal.
//! - Buttons: left/right/middle presses on one region-bound item resolve to
//!   three distinct `button` values through the real pointer path.
//! - Icons: PUA glyphs keep the text path's cell width (one cell per glyph)
//!   and item regions are derived from those cells, so an icon-first module
//!   has exact hit targets. Icon maps are plugin data; Core adds no protocol.

use bitty_lua::ui::{
    ClickArg, ClickCommand, UI_BUTTON_ARG, UI_BUTTON_LEFT, UI_BUTTON_MIDDLE, UI_BUTTON_RIGHT,
    UI_HOVER_PHASE_ARG, UI_HOVER_PHASE_ENTER, UI_HOVER_PHASE_LEAVE, UI_REGION_ARG, UiAnchor,
    UiNode, UiRegion, UiRegionKind, UiSlot,
};
use bitty_platform::{
    CursorPosition, MouseButton, PhysicalSize, PlatformEvent, PressState, WindowEventKind, WindowId,
};
use bitty_runtime::{
    BAND_HOVER_QUEUE_MAX, BandButton, BandContent, BandEdge, BandHoverPhase, BandHoverRequest,
    BandMount, ChromeBands, Runtime, flatten_band_runs, flatten_band_runs_in_width,
};

const PLUGIN: &str = "test-bar";

fn runtime_width(cols: u16) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(
        cw.saturating_mul(u32::from(cols))
            .saturating_add(pad.saturating_mul(2)),
        ch.saturating_mul(24).saturating_add(pad.saturating_mul(2)),
    ))
    .expect("resize");
    rt.tick().expect("tick");
    assert_eq!(rt.window_cells().width, cols, "window width seam");
    rt
}

/// Mounts the top band with a live mount projection and presents it, so the
/// hover fence runs against a real published origin.
fn mount_top_version(rt: &mut Runtime, root: UiNode, version: u32) -> u16 {
    set_band_version(rt, root, version);
    rt.tick().expect("tick");
    rt.plugin_band_row(BandEdge::Top, 0)
        .expect("top band occupies a row")
}

/// Replaces the top band and its mount projection without presenting, so the
/// live tree is newer than the last published frame.
fn set_band_version(rt: &mut Runtime, root: UiNode, version: u32) {
    rt.set_chrome_bands_with_mounts(
        ChromeBands {
            top: vec![BandContent {
                plugin_id: PLUGIN.to_string(),
                slot: UiSlot::Top,
                root,
                version,
            }],
            ..Default::default()
        },
        vec![BandMount {
            plugin_id: PLUGIN.to_string(),
            slot: UiSlot::Top,
            version,
            generation: 1,
            handle: 1,
        }],
    );
}

fn mount_top(rt: &mut Runtime, root: UiNode) -> u16 {
    mount_top_version(rt, root, 1)
}

/// Moves onto `(row, col)`, drains the produced transitions, and confirms any
/// enter as delivered (what the application does after dispatching it).
fn hover_and_confirm(rt: &mut Runtime, row: u16, col: u16) -> Vec<BandHoverRequest> {
    rt.handle_cursor_moved(cell_pixels(rt, row, col));
    let events = rt.drain_band_hovers();
    for event in &events {
        if event.phase == BandHoverPhase::Enter {
            rt.confirm_band_hover(&event.region);
        }
    }
    events
}

fn click(command: &str, region: &str) -> ClickCommand {
    ClickCommand {
        command: command.to_string(),
        args: vec![("id".to_string(), ClickArg::String(region.to_string()))],
        region: Some(UiRegion {
            id: region.to_string(),
            kind: UiRegionKind::Module,
        }),
    }
}

fn hover(command: &str, region: &str) -> ClickCommand {
    ClickCommand {
        command: command.to_string(),
        args: Vec::new(),
        region: Some(UiRegion {
            id: region.to_string(),
            kind: UiRegionKind::Module,
        }),
    }
}

/// One interactive module: a `Row` atom that is hoverable and clickable as
/// one region (the Waybar module shape).
fn module(region: &str, label: &str) -> UiNode {
    UiNode::Row {
        anchor: None,
        children: vec![UiNode::text(label)],
        fg: None,
        bg: None,
        bold: None,
        on_click: Some(click("focus", region)),
        on_scroll: None,
        on_hover: Some(hover("detail", region)),
    }
}

/// A module group placed at `anchor`.
fn group(anchor: UiAnchor, children: Vec<UiNode>) -> UiNode {
    UiNode::Row {
        anchor: Some(anchor),
        children,
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
        on_scroll: None,
        on_hover: None,
    }
}

fn separator() -> UiNode {
    UiNode::text(" ")
}

fn waybar_band() -> UiNode {
    UiNode::Row {
        anchor: None,
        children: vec![
            group(
                UiAnchor::Left,
                vec![module("ws:1", "1"), separator(), module("ws:2", "2")],
            ),
            group(UiAnchor::Center, vec![module("clock", "12:34")]),
            group(UiAnchor::Right, vec![module("battery", "80%")]),
        ],
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
        on_scroll: None,
        on_hover: None,
    }
}

/// Physical center of window cell `(row, col)`.
fn cell_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

fn click_cell(rt: &mut Runtime, row: u16, col: u16, button: MouseButton) {
    let pos = cell_pixels(rt, row, col);
    rt.handle_cursor_moved(pos);
    rt.handle_mouse_input_at(press(button), std::time::Instant::now());
    rt.handle_mouse_input_at(release(button), std::time::Instant::now());
}

/// Run-bound columns of the waybar band at `width`.
fn run_columns(root: &UiNode, width: usize) -> Vec<(usize, usize, String)> {
    let (text, runs) = flatten_band_runs_in_width(root, width);
    let chars: Vec<char> = text.chars().collect();
    runs.iter()
        .map(|run| {
            let label: String = chars[run.start_char..run.end_char].iter().collect();
            (run.start_col, run.end_col, label)
        })
        .collect()
}

#[test]
fn groups_align_left_center_right_at_80_120_200_columns() {
    let root = waybar_band();
    for width in [80usize, 120, 200] {
        let columns = run_columns(&root, width);
        let left: Vec<_> = columns
            .iter()
            .filter(|(_, _, label)| label == "1" || label == "2")
            .collect();
        assert_eq!(left[0].0, 0, "left group starts at the inset edge");
        assert_eq!(left[1].1, 3, "left group keeps declaration order");

        let clock = columns
            .iter()
            .find(|(_, _, label)| label == "12:34")
            .expect("center module");
        assert_eq!(
            clock.0,
            (width - 5) / 2,
            "center is true centering at {width} columns"
        );

        let battery = columns
            .iter()
            .find(|(_, _, label)| label == "80%")
            .expect("right module");
        assert_eq!(battery.1, width, "right group is flush at {width} columns");
        assert_eq!(battery.0, width - 3);
    }
}

#[test]
fn groups_are_single_row_with_gaps_and_no_overlap() {
    let root = waybar_band();
    for width in [80usize, 120, 200] {
        let columns = run_columns(&root, width);
        for window in columns.windows(2) {
            assert!(
                window[0].1 <= window[1].0,
                "runs never overlap at {width}: {:?}",
                window
            );
        }
        for (_, end, _) in &columns {
            assert!(*end <= width, "runs never exceed the row at {width}");
        }
    }
}

#[test]
fn hit_regions_follow_anchored_columns_and_gaps_stay_unclaimed() {
    let mut rt = runtime_width(80);
    let root = waybar_band();
    let row = mount_top(&mut rt, root.clone());
    assert_eq!(rt.window_cells().width, 80);

    // Center module cells resolve to the clock region.
    click_cell(&mut rt, row, 39, MouseButton::Left);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "center module click routed");
    assert_eq!(clicks[0].region.as_ref().expect("region").id, "clock");

    // Right module cell resolves to the battery region at the inset edge.
    click_cell(&mut rt, row, 78, MouseButton::Left);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "right module click routed");
    assert_eq!(clicks[0].region.as_ref().expect("region").id, "battery");

    // A gap cell between the left and center groups is not clickable: the
    // press is chrome (row-wide) but the release routes nothing.
    let before = rt.band_host_stats();
    click_cell(&mut rt, row, 20, MouseButton::Left);
    assert!(rt.drain_band_clicks().is_empty(), "gap cell routes nothing");
    let after = rt.band_host_stats();
    assert!(
        after.clicks_unclaimed > before.clicks_unclaimed,
        "gap release counts unclaimed"
    );

    // Every item region stays inside its painted columns.
    let regions = rt.band_item_regions(BandEdge::Top, 0).expect("regions");
    let ids: Vec<(&str, usize, usize)> = regions
        .iter()
        .map(|item| (item.region.id.as_str(), item.start_col, item.end_col))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("ws:1", 0, 1),
            ("ws:2", 2, 3),
            ("clock", 37, 42),
            ("battery", 77, 80),
        ]
    );
}

#[test]
fn narrow_rows_degrade_by_clipping_without_overlap_or_panic() {
    let root = waybar_band();
    // Far narrower than the three groups together.
    for width in [1usize, 2, 5, 9, 20] {
        let columns = run_columns(&root, width);
        for window in columns.windows(2) {
            assert!(
                window[0].1 <= window[1].0,
                "clipped runs stay disjoint at {width}: {:?}",
                window
            );
        }
        for (start, end, _) in &columns {
            assert!(*start <= *end, "no inverted spans at {width}");
            assert!(*end <= width, "no run past the row at {width}");
        }
        // Left priority: the left group always occupies its leading cells.
        if let Some((start, _, label)) = columns.first() {
            assert!(label == "1" || width == 0, "left group wins at {width}");
            assert_eq!(*start, 0);
        }
    }
    // A zero-width row emits nothing and does not panic.
    let (text, runs) = flatten_band_runs_in_width(&root, 0);
    assert!(text.is_empty());
    assert!(runs.iter().all(|run| run.start_col == run.end_col));
}

#[test]
fn narrow_hit_regions_are_clipped_not_saturated() {
    let mut rt = runtime_width(20);
    let row = mount_top(&mut rt, waybar_band());
    // At 20 columns the left group owns cells 0..3, so cell 3 is a gap and the
    // center group clips to the free middle instead of overlapping the left
    // group or saturating into its last span.
    let regions = rt.band_item_regions(BandEdge::Top, 0).expect("regions");
    for item in &regions {
        assert!(item.end_col <= 20);
        assert!(item.start_col < item.end_col);
        if item.region.id == "clock" {
            assert!(
                item.start_col >= 3,
                "center never paints over the left group"
            );
        }
    }
    let _ = row;
}

#[test]
fn hover_reports_enter_and_leave_per_region_without_capture() {
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());

    let events = hover_and_confirm(&mut rt, row, 39);
    assert_eq!(events.len(), 1, "one enter transition");
    assert_eq!(events[0].region.id, "clock");
    assert_eq!(events[0].phase, BandHoverPhase::Enter);
    assert_eq!(events[0].plugin_id, PLUGIN);
    assert!(events[0].origin.is_some(), "real presented origin");
    assert!(
        !rt.band_release_armed(),
        "hover never arms a release swallow"
    );

    // Steady hover inside the same region dispatches nothing.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 40));
    assert!(rt.drain_band_hovers().is_empty(), "steady hover is idle");

    // Leaving to a gap reports one leave with the captured region id.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 20));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].region.id, "clock");
    assert_eq!(events[0].phase, BandHoverPhase::Leave);

    // Moving between modules is a leave plus an enter, in that order.
    let _ = hover_and_confirm(&mut rt, row, 39);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 78));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 2);
    assert_eq!(
        (events[0].region.id.as_str(), events[0].phase),
        ("clock", BandHoverPhase::Leave)
    );
    assert_eq!(
        (events[1].region.id.as_str(), events[1].phase),
        ("battery", BandHoverPhase::Enter)
    );
    rt.confirm_band_hover(&events[1].region);

    // The hovered rect is the live region geometry (overlay anchor input).
    let region = rt.hovered_band_region().expect("live hover region");
    assert_eq!(region.region.id, "battery");
    assert_eq!((region.start_col, region.end_col), (77, 80));

    // Focus loss hides the tooltip: one leave, no capture/modal side effect.
    rt.set_focused(false);
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, BandHoverPhase::Leave);
    assert!(rt.hovered_band_region().is_none());
}

#[test]
fn hover_reversed_transition_coalesces_to_one_dispatch() {
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());

    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    rt.handle_cursor_moved(cell_pixels(&rt, row, 20));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1, "one dispatch per region per drain");
    assert_eq!(events[0].phase, BandHoverPhase::Leave);
    assert!(rt.band_host_stats().hovers_coalesced >= 1);

    // Re-entering collapses the pending leave into the newer enter.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    rt.handle_cursor_moved(cell_pixels(&rt, row, 20));
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, BandHoverPhase::Enter);
    assert_eq!(events[0].region.id, "clock");
}

#[test]
fn hover_queue_is_capped_and_drops_fail_closed() {
    // Nine distinct hover regions in one band: a sweep that outruns the drain
    // keeps at most the cap and counts the drops instead of growing.
    let mut children = Vec::new();
    for index in 0..9 {
        children.push(module(&format!("m{index}"), "x"));
        children.push(separator());
    }
    let root = UiNode::Row {
        anchor: None,
        children: vec![group(UiAnchor::Left, children)],
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
        on_scroll: None,
        on_hover: None,
    };
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, root);
    // Cells: m0 at 0, space, m1 at 2, ... m8 at 16.
    for index in 0..9u16 {
        rt.handle_cursor_moved(cell_pixels(&rt, row, index * 2));
    }
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), BAND_HOVER_QUEUE_MAX);
    assert!(rt.band_host_stats().hover_queue_drops >= 1);
    assert!(rt.band_host_stats().hovers_routed >= 1);
}

#[test]
fn left_right_middle_clicks_carry_distinct_buttons_and_region_ids() {
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());

    for (button, expected) in [
        (MouseButton::Left, BandButton::Left),
        (MouseButton::Right, BandButton::Right),
        (MouseButton::Middle, BandButton::Middle),
    ] {
        click_cell(&mut rt, row, 39, button);
        let clicks = rt.drain_band_clicks();
        assert_eq!(clicks.len(), 1, "{expected:?} routed");
        assert_eq!(clicks[0].button, expected);
        assert_eq!(clicks[0].region.as_ref().expect("region").id, "clock");
        assert_eq!(clicks[0].command, "focus");

        // The dispatched table carries the same distinct spelling.
        let args = bitty_runtime::band_click_args_table(
            &clicks[0].args,
            clicks[0].region.as_ref(),
            clicks[0].button,
        );
        let bitty_lua::host::LuaValue::Table(pairs) = args else {
            panic!("click args are a table");
        };
        let text = |name: &str| {
            pairs.iter().find_map(|(key, value)| match (key, value) {
                (
                    bitty_lua::host::LuaValue::String(key),
                    bitty_lua::host::LuaValue::String(value),
                ) if key == name => Some(value.as_str()),
                _ => None,
            })
        };
        assert_eq!(text(UI_REGION_ARG), Some("clock"));
        assert_eq!(
            text(UI_BUTTON_ARG),
            Some(match expected {
                BandButton::Left => UI_BUTTON_LEFT,
                BandButton::Right => UI_BUTTON_RIGHT,
                BandButton::Middle => UI_BUTTON_MIDDLE,
            })
        );
    }
}

#[test]
fn hover_dispatch_table_carries_only_region_and_phase() {
    let request = bitty_runtime::BandHoverRequest {
        plugin_id: PLUGIN.to_string(),
        command: "detail".to_string(),
        args: vec![("label".to_string(), ClickArg::String("clock".to_string()))],
        region: UiRegion {
            id: "clock".to_string(),
            kind: UiRegionKind::Module,
        },
        phase: BandHoverPhase::Enter,
        origin: None,
    };
    let args = bitty_runtime::band_hover_args_table(&request.args, &request.region, request.phase);
    let bitty_lua::host::LuaValue::Table(pairs) = args else {
        panic!("hover args are a table");
    };
    let keys: Vec<&str> = pairs
        .iter()
        .filter_map(|(key, _)| match key {
            bitty_lua::host::LuaValue::String(key) => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec!["label", UI_REGION_ARG, UI_HOVER_PHASE_ARG]);
    assert!(
        pairs
            .iter()
            .any(|(_, value)| matches!(value, bitty_lua::host::LuaValue::String(phase) if phase == UI_HOVER_PHASE_ENTER))
    );
    assert!(
        !keys.iter().any(|key| *key == "x" || *key == "y"),
        "hover carries no pointer coordinates"
    );
    // The leave phase is a distinct spelling, not a boolean.
    let leave = bitty_runtime::band_hover_args_table(&[], &request.region, BandHoverPhase::Leave);
    let bitty_lua::host::LuaValue::Table(pairs) = leave else {
        panic!("leave args are a table");
    };
    assert!(pairs.iter().any(
        |(_, value)| matches!(value, bitty_lua::host::LuaValue::String(phase) if phase == UI_HOVER_PHASE_LEAVE)
    ));
}

#[test]
fn icon_glyphs_keep_text_path_cell_widths() {
    // A Nerd Font clock glyph (U+E800 private-use) plus a label. Icon maps are
    // plugin data: Core only owes deterministic cell width through the text
    // path, so the hit region matches what an icon-first module paints.
    let root = UiNode::Row {
        anchor: None,
        children: vec![group(
            UiAnchor::Left,
            vec![module("clock", "\u{e800} 12:34")],
        )],
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
        on_scroll: None,
        on_hover: None,
    };
    let (text, runs) = flatten_band_runs_in_width(&root, 80);
    assert_eq!(text, "\u{e800} 12:34");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].end_col, 7, "one cell per PUA glyph plus label");

    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, root);
    let regions = rt.band_item_regions(BandEdge::Top, 0).expect("regions");
    assert_eq!(regions[0].start_col, 0);
    assert_eq!(regions[0].end_col, 7);

    // Both the glyph cell and the label cells resolve to the same item: the
    // icon is part of the module's hit region, never a dead cell.
    for col in [0u16, 2, 6] {
        click_cell(&mut rt, row, col, MouseButton::Left);
        let clicks = rt.drain_band_clicks();
        assert_eq!(clicks.len(), 1, "cell {col} routes");
        assert_eq!(clicks[0].region.as_ref().expect("region").id, "clock");
    }
}

fn cursor_left() -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::CursorLeft,
    }
}

#[test]
fn cursor_left_ends_band_hover_with_one_leave() {
    // Review B1: leaving the window must not leave a tooltip anchored with the
    // plugin still believing it is hovered.
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    let enter = rt.drain_band_hovers();
    assert_eq!(enter.len(), 1);
    assert_eq!(enter[0].phase, BandHoverPhase::Enter);

    rt.handle_platform_event(cursor_left());
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1, "exactly one leave on CursorLeft");
    assert_eq!(events[0].region.id, "clock");
    assert_eq!(events[0].phase, BandHoverPhase::Leave);
    assert!(
        rt.hovered_band_region().is_none(),
        "no live hover survives the pointer leaving the window"
    );

    // A second CursorLeft is a no-op (no duplicate leave).
    rt.handle_platform_event(cursor_left());
    assert!(rt.drain_band_hovers().is_empty());
}

#[test]
fn unfocused_motion_never_starts_band_hover() {
    // Review B2: a background window must not dispatch plugin commands from
    // pointer motion.
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());
    rt.set_focused(false);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    assert!(
        rt.drain_band_hovers().is_empty(),
        "no hover while unfocused"
    );
    assert!(rt.hovered_band_region().is_none());

    rt.set_focused(true);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    let events = rt.drain_band_hovers();
    assert_eq!(
        events.len(),
        1,
        "focus gain re-arms hover on the next motion"
    );
    assert_eq!(events[0].phase, BandHoverPhase::Enter);
}

#[test]
fn overlay_capture_clears_band_hover_once_and_mutes_motion() {
    // Review B2: a focusable overlay capture owns the cold phase; a tooltip
    // must not stay visible or re-arm under the modal.
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    assert_eq!(rt.drain_band_hovers().len(), 1, "enter delivered");

    rt.set_hover_suppressed_by_overlay(true);
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1, "capture start queues exactly one leave");
    assert_eq!(events[0].phase, BandHoverPhase::Leave);
    assert!(rt.hovered_band_region().is_none());

    // Motion while capture holds is inert, even over the same region.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    assert!(rt.drain_band_hovers().is_empty());
    assert!(rt.hovered_band_region().is_none());

    // Releasing capture does not re-enter until the pointer moves again.
    rt.set_hover_suppressed_by_overlay(false);
    assert!(rt.drain_band_hovers().is_empty());
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].phase, BandHoverPhase::Enter);
}

#[test]
fn capture_teardown_drops_pending_transitions_and_keeps_one_leave() {
    let mut rt = runtime_width(80);
    let row = mount_top(&mut rt, waybar_band());
    // Two un-drained transitions are pending when capture starts.
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    rt.handle_cursor_moved(cell_pixels(&rt, row, 78));
    rt.set_hover_suppressed_by_overlay(true);
    let events = rt.drain_band_hovers();
    assert_eq!(events.len(), 1, "pending enters dropped, one leave kept");
    assert_eq!(events[0].region.id, "battery");
    assert_eq!(events[0].phase, BandHoverPhase::Leave);
}

#[test]
fn unanchored_empty_leaf_matches_the_unbounded_walk() {
    // Review N1 golden: an unanchored scene with an empty Text leaf keeps the
    // same text and run columns in the width-aware walk (including the
    // zero-width empty run).
    let root = UiNode::row(vec![
        UiNode::text("ab"),
        UiNode::text(""),
        UiNode::text("c"),
    ]);
    let (unbounded_text, unbounded_runs) = flatten_band_runs(&root);
    let (wide_text, wide_runs) = flatten_band_runs_in_width(&root, 80);
    assert_eq!(unbounded_text, wide_text);
    let columns = |runs: &[bitty_runtime::BandRun]| {
        runs.iter()
            .map(|run| (run.start_col, run.end_col))
            .collect::<Vec<_>>()
    };
    assert_eq!(columns(&unbounded_runs), columns(&wide_runs));
    assert_eq!(columns(&wide_runs), vec![(0, 2), (2, 2), (2, 3)]);

    // A non-empty leaf that cannot fit its slot still emits no run at all.
    let clipped = UiNode::row(vec![UiNode::text("ab"), UiNode::text("c")]);
    let (text, runs) = flatten_band_runs_in_width(&clipped, 2);
    assert_eq!(text, "ab");
    assert_eq!(columns(&runs), vec![(0, 2)]);
}

#[test]
fn leave_after_a_band_version_bump_is_delivered() {
    // Review B1: a clock tick bumps the band version after the enter was
    // delivered. The leave must still reach the plugin (it only hides/cleans
    // up), even though the enter's presented origin is now stale.
    let mut rt = runtime_width(80);
    let row = mount_top_version(&mut rt, waybar_band(), 1);
    let enter = hover_and_confirm(&mut rt, row, 39);
    assert_eq!(enter.len(), 1);
    assert!(enter[0].origin.is_some(), "real presented origin");

    // Live band v2, published frame still v1: the stored origin is stale.
    set_band_version(&mut rt, waybar_band(), 2);

    // The pointer leaves the item (and the band).
    rt.handle_cursor_moved(cell_pixels(&rt, row + 5, 0));
    let leave = rt.drain_band_hovers();
    assert_eq!(leave.len(), 1, "exactly one leave after the version bump");
    assert_eq!(leave[0].phase, BandHoverPhase::Leave);
    assert_eq!(leave[0].region.id, "clock");
    assert_eq!(
        rt.band_host_stats().hovers_denied,
        0,
        "cleanup is never fenced"
    );
    assert!(rt.hovered_band_region().is_none());
}

#[test]
fn leave_after_band_removal_is_delivered() {
    // Review B1: the band disappears entirely (unmount/edge change). The
    // plugin still receives exactly one leave for the item it was told about.
    let mut rt = runtime_width(80);
    let row = mount_top_version(&mut rt, waybar_band(), 1);
    let enter = hover_and_confirm(&mut rt, row, 39);
    assert_eq!(enter.len(), 1);

    rt.set_chrome_bands_with_mounts(ChromeBands::default(), Vec::new());
    let leave = rt.drain_band_hovers();
    assert_eq!(leave.len(), 1, "exactly one leave after removal");
    assert_eq!(leave[0].phase, BandHoverPhase::Leave);
    assert_eq!(leave[0].region.id, "clock");
    assert_eq!(
        rt.band_host_stats().hovers_denied,
        0,
        "cleanup is never fenced"
    );
    assert!(rt.hovered_band_region().is_none());
}

#[test]
fn deferred_enter_is_delivered_once_the_band_is_presented() {
    // Review B2: the pointer enters while the live band is newer than the last
    // presented frame. The enter is deferred (not dropped forever): the cold
    // phase re-derives it as soon as the frame is published, without waiting
    // for another pointer motion, and a later leave still works.
    let mut rt = runtime_width(80);
    let row = mount_top_version(&mut rt, waybar_band(), 1);

    // Live band v2, published frame still v1.
    set_band_version(&mut rt, waybar_band(), 2);
    rt.handle_cursor_moved(cell_pixels(&rt, row, 39));
    assert!(
        rt.drain_band_hovers().is_empty(),
        "enter is deferred, never queued without a presented origin"
    );
    assert_eq!(rt.band_host_stats().hovers_deferred, 1);
    assert!(
        rt.hovered_band_region().is_none(),
        "no tooltip anchor before the plugin hears about the item"
    );

    // Revalidation before the frame publishes still waits.
    rt.revalidate_band_hover();
    assert!(rt.drain_band_hovers().is_empty());

    // The frame publishes v2; revalidation now queues a fenced enter.
    rt.tick().expect("present the new band");
    rt.revalidate_band_hover();
    let enter = rt.drain_band_hovers();
    assert_eq!(enter.len(), 1, "deferred enter delivered after present");
    assert_eq!(enter[0].phase, BandHoverPhase::Enter);
    assert_eq!(enter[0].region.id, "clock");
    let origin = enter[0].origin.clone().expect("presented origin");
    assert!(rt.band_origin_is_current(&origin));
    rt.confirm_band_hover(&enter[0].region);
    assert!(rt.hovered_band_region().is_some());

    // A delivered enter is not re-fired by revalidation on later frames.
    rt.revalidate_band_hover();
    assert!(rt.drain_band_hovers().is_empty(), "no duplicate enter");
    assert_eq!(rt.band_host_stats().hovers_denied, 0);

    // Leaving still delivers exactly one leave.
    rt.handle_cursor_moved(cell_pixels(&rt, row + 5, 0));
    let leave = rt.drain_band_hovers();
    assert_eq!(leave.len(), 1);
    assert_eq!(leave[0].phase, BandHoverPhase::Leave);
}

#[test]
fn abandoned_enter_owes_no_leave_and_reenters_on_motion() {
    // A refused enter (no state the plugin ever heard about) is abandoned
    // silently; the next motion starts a fresh hover instead of retrying or
    // sending a leave the plugin never earned.
    let mut rt = runtime_width(80);
    let row = mount_top_version(&mut rt, waybar_band(), 1);
    assert_eq!(hover_and_confirm(&mut rt, row, 39).len(), 1);
    rt.abandon_band_hover();
    assert!(rt.hovered_band_region().is_none());
    assert!(
        rt.drain_band_hovers().is_empty(),
        "no leave is owed for an undelivered enter"
    );
    let again = hover_and_confirm(&mut rt, row, 39);
    assert_eq!(again.len(), 1, "motion re-enters after an abandonment");
}
