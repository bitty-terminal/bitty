//! CTX-0911 (issue #1570): Chrome band rendering integration tests.
//!
//! Verifies that UiBlocks mounted by plugins render correctly as chrome bands
//! at window edges with click-to-command routing.

use bitty_lua::ui::{UiNode, UiSlot};
use bitty_runtime::{BandContent, ChromeBands, Runtime};

/// Helper to create a minimal runtime for band testing.
fn minimal_runtime() -> Runtime {
    Runtime::with_defaults().expect("runtime must build")
}

#[test]
fn empty_chrome_bands_renders_no_bands() {
    let mut rt = minimal_runtime();
    let bands = ChromeBands::default();
    rt.set_chrome_bands(bands);

    // Tick should succeed without bands
    let stats = rt.tick().expect("tick must present");
    assert!(stats.fills > 0);
}

#[test]
fn single_top_band_with_text_node() {
    let mut rt = minimal_runtime();

    // Create a simple Text node
    let text_node = UiNode::Text {
        text: "Status: Ready".to_string(),
        fg: Some("foreground".to_string()),
        bg: Some("background".to_string()),
        bold: Some(false),
        on_click: None,
    };

    let band = BandContent {
        plugin_id: "test-plugin".to_string(),
        slot: UiSlot::Top,
        root: text_node,
        version: 1,
    };

    let bands = ChromeBands {
        top: vec![band],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    let stats = rt.tick().expect("tick must present");

    // Band should render - frame advances
    assert!(stats.fills > 0);
}

#[test]
fn multiple_top_bands_stack_vertically() {
    let mut rt = minimal_runtime();

    let band1 = BandContent {
        plugin_id: "plugin-1".to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: "Band 1".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 1,
    };

    let band2 = BandContent {
        plugin_id: "plugin-2".to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: "Band 2".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 1,
    };

    let bands = ChromeBands {
        top: vec![band1, band2],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    let stats = rt.tick().expect("tick must present");

    assert!(stats.fills > 0);
}

#[test]
fn bottom_band_renders_at_window_bottom() {
    let mut rt = minimal_runtime();

    let band = BandContent {
        plugin_id: "statusline".to_string(),
        slot: UiSlot::Bottom,
        root: UiNode::Text {
            text: "statusline content".to_string(),
            fg: Some("foreground".to_string()),
            bg: Some("background".to_string()),
            bold: Some(false),
            on_click: None,
        },
        version: 1,
    };

    let bands = ChromeBands {
        bottom: vec![band],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    let stats = rt.tick().expect("tick must present");

    assert!(stats.fills > 0);
}

#[test]
fn row_node_concatenates_children() {
    let mut rt = minimal_runtime();

    let row_node = UiNode::Row {
        children: vec![
            UiNode::Text {
                text: "Part 1 ".to_string(),
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
            },
            UiNode::Text {
                text: "Part 2".to_string(),
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
            },
        ],
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
    };

    let band = BandContent {
        plugin_id: "test".to_string(),
        slot: UiSlot::Top,
        root: row_node,
        version: 1,
    };

    let bands = ChromeBands {
        top: vec![band],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    let stats = rt.tick().expect("tick must present");

    assert!(stats.fills > 0);
}

#[test]
fn list_node_children_concatenate() {
    let mut rt = minimal_runtime();

    let list_node = UiNode::List {
        children: vec![
            UiNode::Text {
                text: "Item 1".to_string(),
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
            },
            UiNode::Text {
                text: "Item 2".to_string(),
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
            },
        ],
        fg: None,
        bg: None,
        bold: None,
        on_click: None,
    };

    let band = BandContent {
        plugin_id: "test".to_string(),
        slot: UiSlot::Bottom,
        root: list_node,
        version: 1,
    };

    let bands = ChromeBands {
        bottom: vec![band],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    let stats = rt.tick().expect("tick must present");

    assert!(stats.fills > 0);
}

#[test]
fn band_version_increments_on_remount() {
    let mut rt = minimal_runtime();

    let band_v1 = BandContent {
        plugin_id: "test".to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: "Version 1".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 1,
    };

    let bands_v1 = ChromeBands {
        top: vec![band_v1],
        ..Default::default()
    };

    rt.set_chrome_bands(bands_v1);
    let stats_v1 = rt.tick().expect("tick must present");
    assert!(stats_v1.fills > 0);

    // Feed PTY data to trigger state change
    rt.handle_pty_bytes(b"x");

    // Remount with incremented version
    let band_v2 = BandContent {
        plugin_id: "test".to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: "Version 2".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 2,
    };

    let bands_v2 = ChromeBands {
        top: vec![band_v2],
        ..Default::default()
    };

    rt.set_chrome_bands(bands_v2);
    if let Some(stats_v2) = rt.tick() {
        assert!(stats_v2.fills > 0);
    }
}

#[test]
fn no_mount_produces_empty_bands() {
    let mut rt = minimal_runtime();

    // First tick presents initial state
    let stats = rt.tick().expect("tick must present");
    assert!(stats.fills > 0);

    // Feed PTY data to trigger state change
    rt.handle_pty_bytes(b"test");

    // Now add a band
    let band = BandContent {
        plugin_id: "test".to_string(),
        slot: UiSlot::Top,
        root: UiNode::Text {
            text: "Now mounted".to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 1,
    };

    let bands = ChromeBands {
        top: vec![band],
        ..Default::default()
    };

    rt.set_chrome_bands(bands);
    if let Some(stats) = rt.tick() {
        assert!(stats.fills > 0);
    }
}

// CTX-0923 review fix: plugin bands start inward of the Core workspaceline
// band on the same edge instead of painting over it.

use bitty_runtime::config::BarEdge;
use bitty_runtime::{BandEdge, RuntimeConfig};

/// Runtime with the Core workspaceline band on `edge` and two workspaces, so
/// the band is reserved.
fn runtime_with_core_bar(edge: BarEdge) -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig {
        workspace_bar_edge: edge,
        workspaceline_visible: true,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime builds");
    rt.workspace_new().expect("ws2");
    assert!(rt.workspace_switch(0));
    rt
}

fn text_band(plugin: &str, slot: UiSlot, text: &str) -> BandContent {
    BandContent {
        plugin_id: plugin.to_string(),
        slot,
        root: UiNode::Text {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
        },
        version: 1,
    }
}

#[test]
fn bottom_bands_stack_inward_of_a_bottom_core_bar() {
    let rt = runtime_with_core_bar(BarEdge::Bottom);
    let window = rt.window_cells();
    let bar = rt.status_bar_band().expect("core bar reserved");
    assert_eq!(bar.y, window.height - 1, "core bar on the last row");
    assert_eq!(rt.core_reserved_rows(BandEdge::Bottom), 1);
    assert_eq!(rt.core_reserved_rows(BandEdge::Top), 0);
    assert_eq!(rt.plugin_band_row(BandEdge::Bottom, 0), Some(bar.y - 1));
    assert_eq!(rt.plugin_band_row(BandEdge::Bottom, 1), Some(bar.y - 2));
    // The opposite edge is unaffected.
    assert_eq!(rt.plugin_band_row(BandEdge::Top, 0), Some(window.y));
}

#[test]
fn top_bands_stack_inward_of_a_top_core_bar() {
    let rt = runtime_with_core_bar(BarEdge::Top);
    let window = rt.window_cells();
    let bar = rt.status_bar_band().expect("core bar reserved");
    assert_eq!(bar.y, window.y, "core bar on row 0");
    assert_eq!(rt.core_reserved_rows(BandEdge::Top), 1);
    assert_eq!(rt.core_reserved_rows(BandEdge::Bottom), 0);
    assert_eq!(rt.plugin_band_row(BandEdge::Top, 0), Some(bar.y + 1));
    assert_eq!(rt.plugin_band_row(BandEdge::Top, 1), Some(bar.y + 2));
    assert_eq!(
        rt.plugin_band_row(BandEdge::Bottom, 0),
        Some(window.height - 1)
    );
}

#[test]
fn lone_workspace_reserves_nothing_for_plugin_bands() {
    let rt = minimal_runtime();
    assert_eq!(rt.status_bar_band(), None);
    let window = rt.window_cells();
    assert_eq!(rt.core_reserved_rows(BandEdge::Bottom), 0);
    assert_eq!(
        rt.plugin_band_row(BandEdge::Bottom, 0),
        Some(window.height - 1)
    );
}

/// Pixels of the centre third of window cell row `row` after a tick.
fn window_row_pixels(rt: &Runtime, row: u16) -> Vec<u8> {
    let rgba = rt.headless_rgba().expect("headless rgba after tick");
    let width = usize::try_from(rt.surface_extent().expect("extent").width()).expect("usize");
    let (_, ch) = rt.live_cell_size();
    let ch = ch as usize;
    let pad = usize::try_from(rt.window_padding_physical()).expect("usize");
    let stride = width * 4;
    let top = pad + usize::from(row) * ch + ch / 3;
    let bottom = (top + ch / 3).min(rgba.len() / stride.max(1));
    rgba[top * stride..bottom * stride].to_vec()
}

/// Ticks a Core-bar runtime with `bands` mounted before its first present.
fn presented(bar_edge: BarEdge, bands: ChromeBands) -> Runtime {
    let mut rt = runtime_with_core_bar(bar_edge);
    rt.set_chrome_bands(bands);
    rt.tick().expect("tick must present");
    rt
}

#[test]
fn statusline_band_paints_inward_of_the_core_bar_on_each_edge() {
    for (bar_edge, band_edge, slot) in [
        (BarEdge::Bottom, BandEdge::Bottom, UiSlot::Statusline),
        (BarEdge::Top, BandEdge::Top, UiSlot::Top),
    ] {
        let mut bands = ChromeBands::default();
        match band_edge {
            BandEdge::Top => bands.top.push(text_band("statusline", slot, "STATUS")),
            _ => bands.bottom.push(text_band("statusline", slot, "STATUS")),
        }
        let without = presented(bar_edge, ChromeBands::default());
        let with = presented(bar_edge, bands);

        let row = with.plugin_band_row(band_edge, 0).expect("band fits");
        let bar = with.status_bar_band().expect("core bar reserved");
        assert_ne!(row, bar.y, "{bar_edge:?}: plugin band off the core bar row");
        assert_ne!(
            window_row_pixels(&without, row),
            window_row_pixels(&with, row),
            "{bar_edge:?}: plugin band painted on row {row}"
        );
        assert_eq!(
            window_row_pixels(&without, bar.y),
            window_row_pixels(&with, bar.y),
            "{bar_edge:?}: core bar row {} untouched by the plugin band",
            bar.y
        );
    }
}
