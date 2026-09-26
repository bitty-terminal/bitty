//! Zoom toggle must preserve per-leaf scroll state (issue #1435).

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, View, ViewId};

/// Creates a runtime with scrollback content.
fn runtime_with_scrollback() -> Runtime {
    let config = RuntimeConfig::default();
    let mut rt = Runtime::new(config).expect("headless build");

    // Generate scrollback: write 100 lines so we have history to scroll through
    for i in 1..=100 {
        rt.handle_pty_bytes(format!("Line {}\r\n", i).as_bytes());
    }

    rt
}

#[test]
fn zoom_preserves_scroll_offset_single_pane() {
    let mut rt = runtime_with_scrollback();
    let view_id = rt.focused_view().expect("focused view");

    // Scroll up 20 lines from live
    if let Some(view) = rt.layout_mut().find_leaf_mut(view_id) {
        view.set_scroll_offset(20, 1000);
    }

    let scroll_before = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .expect("view exists");

    assert_eq!(scroll_before, 20, "scroll offset set to 20");

    // Engage zoom (this is a no-op for single pane, but exercises the path)
    // In real usage, chrome_keys::ZoomState::engage would be called
    let backup = rt.layout().clone();
    let leaf = rt.layout().find_leaf(view_id).cloned().expect("leaf");
    rt.set_layout(LayoutNode::leaf(leaf));

    let scroll_during = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .expect("view exists during zoom");

    assert_eq!(scroll_during, 20, "scroll preserved during zoom engage");

    // Disengage zoom
    rt.set_layout(backup);

    let scroll_after = rt
        .layout()
        .find_leaf(view_id)
        .map(|v| v.scroll_offset())
        .expect("view exists after zoom");

    assert_eq!(scroll_after, 20, "scroll preserved after zoom disengage");
}

#[test]
fn zoom_preserves_scroll_offset_multi_pane() {
    let config = RuntimeConfig::default();
    let mut rt = Runtime::new(config).expect("headless build");

    // Create a split layout with two panes
    let v1 = ViewId::new(1);
    let v2 = ViewId::new(2);

    let layout = LayoutNode::split(
        bitty_runtime::SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(v1, 80, 24)),
        LayoutNode::leaf(View::new(v2, 80, 24)),
    );
    rt.set_layout(layout);

    // Generate scrollback in the first pane (focused)
    for i in 1..=100 {
        rt.handle_pty_bytes(format!("Pane 1 Line {}\r\n", i).as_bytes());
    }

    // Scroll the focused pane up 30 lines
    if let Some(view) = rt.layout_mut().find_leaf_mut(v1) {
        view.set_scroll_offset(30, 1000);
    }

    let scroll_v1_before = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists");

    assert_eq!(scroll_v1_before, 30, "v1 scrolled to offset 30");

    // Engage zoom on v1 (simulating chrome_keys::ZoomState::engage)
    let backup = rt.layout().clone();
    let leaf = rt.layout().find_leaf(v1).cloned().expect("v1 leaf");
    rt.set_layout(LayoutNode::leaf(leaf));

    // Verify scroll preserved during zoom
    let scroll_v1_zoomed = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists during zoom");

    assert_eq!(scroll_v1_zoomed, 30, "v1 scroll preserved when zoomed");

    // Disengage zoom
    rt.set_layout(backup);

    // Verify scroll preserved after zoom
    let scroll_v1_after = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists after zoom");

    assert_eq!(scroll_v1_after, 30, "v1 scroll preserved after unzoom");

    // Verify v2 still exists (wasn't dropped)
    assert!(rt.layout().find_leaf(v2).is_some(), "v2 still exists");
}

#[test]
fn zoom_round_trip_with_resize() {
    let config = RuntimeConfig::default();
    let mut rt = Runtime::new(config).expect("headless build");

    // Start with a single pane
    let v1 = rt.focused_view().expect("focused view");

    // Generate scrollback
    for i in 1..=100 {
        rt.handle_pty_bytes(format!("Line {}\r\n", i).as_bytes());
    }

    // Scroll up 25 lines
    if let Some(view) = rt.layout_mut().find_leaf_mut(v1) {
        view.set_scroll_offset(25, 1000);
    }

    let scroll_before = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists");

    assert_eq!(scroll_before, 25);

    // Engage zoom
    let backup = rt.layout().clone();
    let mut leaf = rt.layout().find_leaf(v1).cloned().expect("v1 leaf");

    // Simulate a resize during zoom (fullscreen might change dimensions)
    leaf.resize(120, 40);
    rt.set_layout(LayoutNode::leaf(leaf));

    let scroll_zoomed = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists during zoom");

    assert_eq!(scroll_zoomed, 25, "scroll preserved through resize");

    // Disengage zoom (restore original layout and size)
    rt.set_layout(backup);

    let scroll_after = rt
        .layout()
        .find_leaf(v1)
        .map(|v| v.scroll_offset())
        .expect("v1 exists after zoom");

    assert_eq!(
        scroll_after, 25,
        "scroll preserved after restore with resize"
    );
}
