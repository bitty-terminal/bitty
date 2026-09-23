#![forbid(unsafe_code)]
//! Panel border + inter-panel gap regression (#1342).
//!
//! Owner live report: split Panel borders render too wide with no visible
//! gap between adjacent panels. The contract pinned here:
//!
//! - the default border is thin (`1` logical px, `DEFAULT_BORDER_PX`);
//! - adjacent sibling frames keep a visible background gap band
//!   (`gaps_in`, `6` logical px by default) — rings never touch;
//! - the paint-only outline widths and the focused/idle colors resolve
//!   per `View` and honor explicit config (thin default inherits `border`);
//! - out-of-range geometry/width values fail closed, never clamp.
//!
//! Every assertion is headless RGBA or pure frame math.

use bitty_runtime::{Decoration, LayoutNode, Runtime, RuntimeConfig, SplitAxis, View, ViewId};

fn two_pane_runtime() -> Runtime {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("default runtime builds");
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    rt.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    rt.set_focus(ViewId::new(1));
    rt
}

fn probe(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let idx = (y * width + x) * 4;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

#[test]
fn default_border_is_thin_and_sibling_gap_is_visible() {
    // Thin default: the solver-level border equals the 1px default, and the
    // sibling band equals the visible 6px default gap.
    assert_eq!(Decoration::default().border, 1);
    assert_eq!(Decoration::default().gaps_in, 6);
    let mut rt = two_pane_runtime();
    let frames = rt.present_frames();
    assert_eq!(frames.len(), 2);
    for frame in &frames {
        assert_eq!(frame.border, 1, "default ring is 1px, not wide");
    }
    let left = &frames[0];
    let right = &frames[1];
    let gap = right.frame.x - (left.frame.x + left.frame.width as i32);
    assert_eq!(
        gap,
        i32::from(Decoration::default().gaps_in),
        "sibling frames keep a visible gap band"
    );

    rt.tick().expect("tick presents");
    let rgba = rt.headless_rgba().expect("rgba after tick");
    let extent = rt.config().window_extent();
    let width = usize::try_from(extent.width()).expect("width fits");
    let pad = usize::try_from(rt.window_padding_physical()).expect("pad fits");
    let bg = bitty_render::grid::DEFAULT_BG;
    let focused = bitty_runtime::config::DEFAULT_OUTLINE_FOCUSED;
    // Mid-height row, clear of the radius-6 rounded corners.
    let y = pad + usize::try_from(left.frame.y).expect("y fits") + 100;
    let left_edge = pad + usize::try_from(left.frame.x).expect("x fits");
    let right_edge = pad + usize::try_from(right.frame.x).expect("x fits");
    // Focused pane: exactly 1px of accent, then background.
    assert_eq!(probe(&rgba, width, left_edge, y), focused);
    assert_eq!(
        probe(&rgba, width, left_edge + 1, y),
        bg,
        "focused ring stops at 1px"
    );
    // The sibling gap band is clear background between the rings.
    let left_ring_end = left_edge + usize::try_from(left.frame.width).expect("w fits");
    for x in left_ring_end..right_edge {
        assert_eq!(
            probe(&rgba, width, x, y),
            bg,
            "gap band must be bg at x={x}"
        );
    }
    assert_eq!(
        right_edge - left_ring_end,
        usize::from(Decoration::default().gaps_in),
        "painted gap matches the configured gap size"
    );
    // Idle pane: 1px ring in the idle color family (translucent gray
    // composited over bg, so not byte-equal to the configured color), then
    // background. Focused and idle stay distinguishable.
    let idle_px = probe(&rgba, width, right_edge, y);
    assert_ne!(idle_px, focused, "idle must differ from focused");
    assert_ne!(idle_px, bg, "idle ring must be visible");
    assert_eq!(
        probe(&rgba, width, right_edge + 1, y),
        bg,
        "idle ring stops at 1px"
    );
}

#[test]
fn outline_width_and_color_keys_resolve_per_view() {
    // Config keys: base/focused/idle widths plus focused/idle colors flow
    // into the painted rings; the thin default inherits `border`.
    // Defaults are the ratified accent/idle pair and must differ.
    let focused = bitty_runtime::config::DEFAULT_OUTLINE_FOCUSED;
    let idle = bitty_runtime::config::DEFAULT_OUTLINE_IDLE;
    assert_eq!(focused, [0x33, 0xCC, 0xFF, 0xFF]);
    assert_eq!(idle, [0x59, 0x59, 0x59, 0xAA]);
    assert_ne!(focused, idle);

    // Explicit per-view widths/colors paint into each pane's ring.
    let mut wide = Runtime::new(RuntimeConfig {
        outline_focused: [0xFF, 0x00, 0x00, 0xFF],
        outline_idle: [0x00, 0xFF, 0x00, 0xFF],
        outline_width_focused: Some(3),
        outline_width_idle: Some(1),
        ..RuntimeConfig::default()
    })
    .expect("explicit outline builds");
    wide.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    ));
    wide.set_container(bitty_runtime::UiRect::new(0, 0, 80, 24));
    wide.set_focus(ViewId::new(1));
    wide.tick().expect("tick presents");
    let rgba = wide.headless_rgba().expect("rgba");
    let extent = wide.config().window_extent();
    let width = usize::try_from(extent.width()).expect("width fits");
    let pad = usize::try_from(wide.window_padding_physical()).expect("pad fits");
    let frames = wide.present_frames();
    let fy = pad + usize::try_from(frames[0].frame.y).expect("y fits") + 100;
    let fx0 = pad + usize::try_from(frames[0].frame.x).expect("x fits");
    assert_eq!(
        probe(&rgba, width, fx0 + 2, fy),
        [0xFF, 0x00, 0x00, 0xFF],
        "focused ring reaches its configured 3px width"
    );
    assert_eq!(
        probe(&rgba, width, fx0 + 3, fy),
        bitty_render::grid::DEFAULT_BG,
        "focused ring stops at 3px"
    );
    let iy = pad + usize::try_from(frames[1].frame.y).expect("y fits") + 100;
    let ix0 = pad + usize::try_from(frames[1].frame.x).expect("x fits");
    assert_eq!(
        probe(&rgba, width, ix0, iy),
        [0x00, 0xFF, 0x00, 0xFF],
        "idle ring paints its configured color"
    );
    assert_eq!(
        probe(&rgba, width, ix0 + 1, iy),
        bitty_render::grid::DEFAULT_BG,
        "idle ring stops at 1px"
    );
}

#[test]
fn panel_geometry_and_widths_fail_closed() {
    // Fail-closed defaults: out-of-range decoration and outline widths are
    // rejected, never clamped into a wide border.
    let mut rt = two_pane_runtime();
    assert!(
        rt.set_decoration(Decoration::new(6, 6, 9, 6, 6)).is_err(),
        "border 9 exceeds the 0..=8 bound"
    );
    assert!(
        rt.set_decoration(Decoration::new(33, 6, 1, 6, 6)).is_err(),
        "gaps_in 33 exceeds the 0..=32 bound"
    );
    assert!(
        Runtime::new(RuntimeConfig {
            outline_width_focused: Some(17),
            ..RuntimeConfig::default()
        })
        .is_err(),
        "outline width 17 exceeds the 0..=16 bound"
    );
    // The rejected values leave the live decoration untouched.
    assert_eq!(rt.decoration(), Decoration::default());
}
