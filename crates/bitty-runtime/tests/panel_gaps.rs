#![forbid(unsafe_code)]
//! Panel gaps regression (CTX-0240, CTX-0177 reuse, CTX-0151/0228 class).
//!
//! Reuses the CTX-0177 cell-gap algebra: the solver is content-agnostic
//! (`Rect`s per leaf) so `ViewContent::Panel(PanelId)` leaves flow through
//! the same `layout_with_gaps` path as terminal leaves. Rules pinned here:
//! - Split siblings get `gaps_in` bands painted with theme bg.
//! - Stack (workspace) leaves get `gaps_out` inset only, zero inner.
//! - Config reuses `layout.gaps_in`/`gaps_out` (`0..=16` cells); no per-type overrides.
//! - Gap bands stay bg after split + after resize (CTX-0151 stale-bg /
//!   CTX-0228 no-repaint classes). All headless, deterministic, RGBA-asserted.

use bitty_runtime::{Gaps, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

fn gapped_runtime(gaps_in: u16, gaps_out: u16) -> Runtime {
    Runtime::new(RuntimeConfig {
        gaps_in,
        gaps_out,
        ..RuntimeConfig::default()
    })
    .expect("gapped config must build")
}

fn two_pane_split() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn workspace_stack() -> LayoutNode {
    LayoutNode::stack(vec![
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ])
}

/// Physical pixel of a container cell origin (matches present layer:
/// live cell metrics at scale 1.0 + physical window padding).
fn container_cell_pixel(rt: &Runtime, col: u16, row: u16) -> (usize, usize) {
    let cfg = rt.config();
    let cw = usize::try_from(cfg.cell_width).expect("cell width fits");
    let ch = usize::try_from(cfg.cell_height).expect("cell height fits");
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    (usize::from(col) * cw + pad, usize::from(row) * ch + pad)
}

fn rgba_at(rgba: &[u8], sw: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * sw + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

#[test]
fn stack_leaves_get_gaps_out_inset_only() {
    // CTX-0238b rule: stacked leaves share full bounds; `gaps_in` between
    // them is meaningless → Stack gets `gaps_out` inset only, zero inner.
    let bounds = bitty_runtime::UiRect::new(0, 0, 80, 24);
    let stack = workspace_stack();
    let alloc = stack.layout_with_gaps(bounds, Gaps::new(2, 1));
    assert_eq!(alloc.len(), 2);
    // Both share the outer-inset rect (1,1,78,22); no inner split.
    assert_eq!(alloc[0].1, bitty_runtime::UiRect::new(1, 1, 78, 22));
    assert_eq!(alloc[1].1, bitty_runtime::UiRect::new(1, 1, 78, 22));
    // Same via runtime (panel leaves flow through the same path).
    let mut rt = gapped_runtime(2, 1);
    rt.set_layout(workspace_stack());
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    let rallocs = rt.reflow_layout();
    assert_eq!(rallocs[0].1, bitty_runtime::UiRect::new(1, 1, 78, 22));
    assert_eq!(rallocs[1].1, bitty_runtime::UiRect::new(1, 1, 78, 22));
}

#[test]
fn split_panel_leaves_exclude_gap_bands_zero_parity() {
    // Gaps::ZERO is bit-identical to legacy tiling (no behavior change).
    let bounds = bitty_runtime::UiRect::new(0, 0, 80, 24);
    let split = two_pane_split();
    assert_eq!(
        split.layout(bounds),
        split.layout_with_gaps(bounds, Gaps::ZERO)
    );
    // With gaps, split siblings exclude the bands.
    let alloc = split.layout_with_gaps(bounds, Gaps::new(2, 1));
    assert_eq!(alloc[0].1, bitty_runtime::UiRect::new(1, 1, 38, 22));
    assert_eq!(alloc[1].1, bitty_runtime::UiRect::new(41, 1, 38, 22));
}

#[test]
fn gap_bands_painted_theme_bg_after_split() {
    // CTX-0151 class: gap bands are unallocated cells — present must fill
    // them with bg every damaged frame. Headless RGBA asserts gap pixels ==
    // theme bg after split.
    let mut rt = gapped_runtime(2, 1);
    let _ = rt.tick().expect("first tick must present");
    assert_eq!(rt.tick(), None);
    rt.set_layout(two_pane_split());
    let stats = rt.tick().expect("split must present");
    assert!(stats.headless);
    let rgba = rt.headless_rgba().expect("rgba after split");
    let extent = rt.config().window_extent();
    let sw = usize::try_from(extent.width()).expect("surface width fits");
    let bg = bitty_render::grid::DEFAULT_BG;
    // Inner gap band: container cols 39..41, row 5 (inside outer inset).
    // Sample the middle of the band cell.
    let (gx, gy) = container_cell_pixel(&rt, 39, 5);
    let cw = usize::try_from(rt.config().cell_width).expect("cell width fits");
    let ch = usize::try_from(rt.config().cell_height).expect("cell height fits");
    let sx = gx + cw / 2;
    let sy = gy + ch / 2;
    assert_eq!(
        rgba_at(&rgba, sw, sx, sy),
        bg,
        "inner gap band must be theme bg"
    );
    let (gx2, gy2) = container_cell_pixel(&rt, 40, 5);
    assert_eq!(
        rgba_at(&rgba, sw, gx2 + cw / 2, gy2 + ch / 2),
        bg,
        "inner gap second cell must be theme bg"
    );
    // Outer gap: container col 0 / row 0 belongs to no leaf.
    let (ox, oy) = container_cell_pixel(&rt, 0, 5);
    assert_eq!(
        rgba_at(&rgba, sw, ox + cw / 2, oy + ch / 2),
        bg,
        "outer gap must be theme bg"
    );
    assert_eq!(rt.tick(), None, "must idle after gap present");
}

#[test]
fn gap_bands_stay_bg_after_resize() {
    // CTX-0228 class: geometry-only layout change forces a full present;
    // gap bands must stay bg after resize (no stale frame).
    let mut rt = gapped_runtime(2, 1);
    rt.set_layout(two_pane_split());
    assert!(rt.tick().is_some(), "split presents");
    assert_eq!(rt.tick(), None);
    // Resize container headlessly (geometry-only, no PTY bytes).
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 100, 30));
    let stats = rt
        .tick()
        .expect("resize without PTY bytes must still present");
    assert!(stats.headless);
    assert!(stats.fills > 0, "resize must carry fills (full-dirty)");
    let rgba = rt.headless_rgba().expect("rgba after resize");
    let extent = rt.config().window_extent();
    let sw = usize::try_from(extent.width()).expect("surface width fits");
    let bg = bitty_render::grid::DEFAULT_BG;
    let allocs = rt.layout_allocations();
    assert_eq!(allocs.len(), 2);
    // Gap band is between the two allocations along x.
    let gap_col = allocs[0].1.x + allocs[0].1.width;
    let gap_row = allocs[0].1.y + 2;
    let (gx, gy) = container_cell_pixel(&rt, gap_col, gap_row);
    let cw = usize::try_from(rt.config().cell_width).expect("cell width fits");
    let ch = usize::try_from(rt.config().cell_height).expect("cell height fits");
    assert_eq!(
        rgba_at(&rgba, sw, gx + cw / 2, gy + ch / 2),
        bg,
        "gap band must stay theme bg after resize"
    );
    assert_eq!(rt.tick(), None);
}

#[test]
fn gaps_bounds_reuse_layout_config_no_per_type_overrides() {
    // Reuse `layout.gaps_in`/`gaps_out` as-is (0..=16 cells each).
    // No per-panel-type overrides exist: the same `Gaps` applies to every leaf
    // regardless of content (terminal vs panel).
    assert_eq!(
        (
            RuntimeConfig::default().gaps_in,
            RuntimeConfig::default().gaps_out
        ),
        (0, 0)
    );
    let ok = RuntimeConfig {
        gaps_in: 16,
        gaps_out: 16,
        ..RuntimeConfig::default()
    };
    assert!(ok.validate().is_ok());
    assert!(gapped_runtime(16, 16).gaps() == Gaps::new(16, 16));
    let bad_in = RuntimeConfig {
        gaps_in: 17,
        gaps_out: 0,
        ..RuntimeConfig::default()
    };
    assert!(bad_in.validate().is_err());
    let bad_out = RuntimeConfig {
        gaps_in: 0,
        gaps_out: 17,
        ..RuntimeConfig::default()
    };
    assert!(bad_out.validate().is_err());
}
