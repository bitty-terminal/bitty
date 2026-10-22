//! Issue #2524: resize/zoom must not retain state between passes.
//!
//! The runtime already bounds scrollback lines and kitty image bytes. What a
//! long interactive session exercises is the *geometry churn*: every window
//! resize and every zoom toggle drives `set_container`/`set_layout` +
//! `reflow_layout` + `tick`, and each pass must leave only the terminal truth
//! it started with. A retained per-pass buffer, duplicated grid, or stale
//! scrollback snapshot shows up here as monotonic `owned_bytes` growth even
//! though no PTY bytes arrived.
//!
//! The assertions are deliberately structural (accounting + line counts), not
//! wall-clock or RSS: host scheduling and the allocator must not make a CI
//! gate flaky. The first passes may legitimately rewrap history for the new
//! width; the test requires the accounting to converge within one percent and
//! then stay byte-identical for a long tail of identical passes.

#![forbid(unsafe_code)]

use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId};

/// Provider-neutral scrollback cap used by the regression scenario.
const SCROLLBACK_LINES: usize = 5000;
/// Panes in the scenario: the largest per-window layout the strip supports.
const PANES: usize = 6;
/// Lines fed through the primary parser; > cap so the ring evicts.
const FED_LINES: usize = SCROLLBACK_LINES + 1000;
/// Warm-up passes that may rewrap scrollback for the cycled widths.
const WARMUP_PASSES: usize = 40;
/// Identical passes after convergence; any retained buffer shows up here.
const STEADY_PASSES: usize = 100;

/// Window cell sizes cycled by a pass (grow and shrink around the start).
const SIZES: [(u16, u16); 4] = [(80, 24), (120, 40), (60, 18), (160, 50)];

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn six_leaves() -> LayoutNode {
    // Two rows of three panes; deterministic nested splits.
    let row_a = LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2));
    let row_a = LayoutNode::split(SplitAxis::Horizontal, 0.5, row_a, leaf(3));
    let row_b = LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(4), leaf(5));
    let row_b = LayoutNode::split(SplitAxis::Horizontal, 0.5, row_b, leaf(6));
    LayoutNode::split(SplitAxis::Vertical, 0.5, row_a, row_b)
}

fn runtime_with_scrollback() -> Runtime {
    let config = RuntimeConfig {
        cols: 80,
        rows: 24,
        scrollback: SCROLLBACK_LINES,
        ..RuntimeConfig::default()
    };
    let mut runtime = Runtime::new(config).expect("runtime");
    runtime.set_container(UiRect::new(0, 0, 80, 24));
    runtime.set_layout(six_leaves());
    runtime.reflow_layout();

    let mut batch = Vec::with_capacity(FED_LINES * 48);
    for line in 0..FED_LINES {
        batch.extend_from_slice(
            format!("issue 2524 line {line:05} 0123456789 abcdefghij\r\n").as_bytes(),
        );
    }
    runtime.handle_pty_bytes(&batch);
    let _ = runtime.tick();
    runtime
}

/// One full geometry pass: every size, with a zoom toggle on alternating
/// sizes. No PTY bytes are delivered, so terminal truth must not change.
fn geometry_pass(runtime: &mut Runtime) {
    let backup = runtime.layout().clone();
    let focused = runtime.focused_view().expect("focused leaf");
    let leaf = backup
        .find_leaf(focused)
        .cloned()
        .expect("focused leaf exists");
    for (index, (cols, rows)) in SIZES.iter().enumerate() {
        runtime.set_container(UiRect::new(0, 0, *cols, *rows));
        runtime.reflow_layout();
        let _ = runtime.tick();
        if index % 2 == 0 {
            runtime.set_layout(LayoutNode::leaf(leaf.clone()));
            let _ = runtime.tick();
            runtime.set_layout(backup.clone());
            let _ = runtime.tick();
        }
    }
}

#[test]
fn resize_and_zoom_passes_converge_and_stay_byte_stable() {
    let mut runtime = runtime_with_scrollback();
    assert_eq!(runtime.leaf_count(), PANES);
    // The ring is capped, not the feed count.
    let scrollback = runtime.state().scrollback_len();
    assert!(
        scrollback <= SCROLLBACK_LINES,
        "scrollback {scrollback} exceeds cap {SCROLLBACK_LINES}"
    );
    assert!(scrollback > 0, "scenario must retain scrollback");

    let initial = runtime
        .workspace_memory_report()
        .aggregate
        .unique_owned_bytes;
    for _ in 0..WARMUP_PASSES {
        geometry_pass(&mut runtime);
    }
    let warm = runtime.workspace_memory_report();
    let warm_bytes = warm.aggregate.unique_owned_bytes;
    // Rewrap normalisation only: one percent of the retained state, never a
    // per-pass buffer.
    assert!(
        warm_bytes <= initial + initial / 100,
        "warm-up grew {initial} -> {warm_bytes}; resize/zoom must not accumulate"
    );

    for pass in 0..STEADY_PASSES {
        geometry_pass(&mut runtime);
        let after = runtime.workspace_memory_report();
        assert_eq!(
            after, warm,
            "pass {pass} retained state: identical resize/zoom passes must be byte-stable"
        );
        assert_eq!(runtime.state().scrollback_len(), scrollback);
    }
}

#[test]
fn resize_and_zoom_do_not_retain_kitty_layer_bytes() {
    let mut runtime = runtime_with_scrollback();
    geometry_pass(&mut runtime);
    let images = runtime.kitty_image_count();
    let placements = runtime.kitty_placement_count();
    let raster = runtime.kitty_raster_bytes();
    for _ in 0..10 {
        geometry_pass(&mut runtime);
    }
    assert_eq!(runtime.kitty_image_count(), images);
    assert_eq!(runtime.kitty_placement_count(), placements);
    assert_eq!(
        runtime.kitty_raster_bytes(),
        raster,
        "geometry churn must not grow the raster cache"
    );
    assert!(runtime.kitty_raster_bytes() <= bitty_rich::KITTY_RASTER_CACHE_MAX_BYTES);
}
