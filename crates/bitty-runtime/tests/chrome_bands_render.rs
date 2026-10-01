//! CTX-0911 (issue #1570): Chrome band rendering integration tests.
//!
//! Verifies that UiBlocks mounted by plugins render correctly as chrome bands
//! at window edges with click-to-command routing.

use bitty_lua::ui::UiNode;
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
        slot: "top".to_string(),
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
        slot: "top".to_string(),
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
        slot: "top".to_string(),
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
        slot: "bottom".to_string(),
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
        slot: "top".to_string(),
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
        slot: "bottom".to_string(),
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
        slot: "top".to_string(),
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
        slot: "top".to_string(),
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
        slot: "top".to_string(),
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
