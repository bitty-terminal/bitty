//! CTX-1287 (issue #2173): multi-float workspace focus model.
//!
//! One workspace holds tiled panels plus multiple detached floats. Focus
//! must switch between ALL panels with the same shortcuts (hyprland
//! parity): focusing a float raises it to paint-top with a clear focused
//! border while the others show unfocused chrome, and directional/cycle
//! focus movement must include floats instead of stranding on tiled
//! leaves only.
//!
//! RED coverage (fails on origin/main):
//! - `set_focus` on a buried float does not raise it (paint order
//!   unchanged, focused ring stays hidden under the top float).
//! - `move_focus(Next/Prev)` skips detached floats (and pins).
//! - directional `move_focus` from a focused float returns `None`
//!   (focus stranded: hjkl/arrows dead on floats).
//!
//! Guard (passes before and after): raising must never move floats on
//! screen — the cascade anchor is stack-stable, so a focus change only
//! reorders paint, never geometry.

use bitty_runtime::{
    AnimationPolicy, FocusDirection, LayoutNode, OverlayTier, Runtime, RuntimeConfig, SplitAxis,
    UiRect, View, ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        ..RuntimeConfig::default()
    })
    .expect("headless harness runtime must build")
}

fn three_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
        ),
    )
}

fn four_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(3), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(4), 80, 24)),
            ),
        ),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

/// Detached-float view ids in paint order (Float-tier frame order).
fn painted_floats(rt: &Runtime) -> Vec<ViewId> {
    rt.present_frames()
        .into_iter()
        .filter(|frame| frame.tier == Some(OverlayTier::Float))
        .map(|frame| frame.view)
        .collect()
}

fn pixel(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * width + x) * 4;
    rgba[offset..offset + 4]
        .try_into()
        .expect("pixel must be inside the RGBA surface")
}

#[test]
fn focus_float_raises_it_to_paint_top_issue_2173() {
    // Two detached floats over one tiled leaf: paint order follows detach
    // order, so the second detach (view 3) covers the first (view 2).
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    assert_eq!(rt.floated_views(), vec![ViewId::new(2), ViewId::new(3)]);
    assert_eq!(
        painted_floats(&rt).as_slice(),
        &[ViewId::new(2), ViewId::new(3)],
        "detach order paints detached-first"
    );
    // Focusing the buried float must raise it above the other float, so
    // its focused border paints on top instead of hiding underneath.
    assert!(rt.set_focus(ViewId::new(2)), "buried float stays focusable");
    assert_eq!(
        rt.floated_views(),
        vec![ViewId::new(3), ViewId::new(2)],
        "focusing a float raises it to paint-top (issue #2173)"
    );
    assert_eq!(
        painted_floats(&rt).as_slice(),
        &[ViewId::new(3), ViewId::new(2)],
        "raised float paints last (topmost)"
    );
}

#[test]
fn raise_keeps_float_geometry_stable_issue_2173() {
    // Raising reorders paint only: no float may move on screen when focus
    // changes (the cascade anchor is stack-stable, not position-derived).
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    let before: Vec<(ViewId, _)> = rt
        .present_frames()
        .into_iter()
        .filter(|frame| frame.tier == Some(OverlayTier::Float))
        .map(|frame| (frame.view, frame.frame))
        .collect();
    assert!(rt.set_focus(ViewId::new(2)), "buried float stays focusable");
    let after: Vec<(ViewId, _)> = rt
        .present_frames()
        .into_iter()
        .filter(|frame| frame.tier == Some(OverlayTier::Float))
        .map(|frame| (frame.view, frame.frame))
        .collect();
    for (view, rect) in &before {
        let raised = after
            .iter()
            .find(|(id, _)| id == view)
            .unwrap_or_else(|| panic!("{view:?} must stay presented after raise"));
        assert_eq!(
            &raised.1, rect,
            "focus raise must not move float {view:?} on screen (issue #2173)"
        );
    }
}

#[test]
fn raised_float_paints_focused_chrome_and_other_float_idle_chrome_issue_2173() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    assert!(rt.set_focus(ViewId::new(2)), "buried float stays focusable");
    rt.tick().expect("focused float frame must present");
    let rgba = rt.headless_rgba().expect("deterministic RGBA frame");
    let extent = rt.config().window_extent();
    let width = usize::try_from(extent.width()).expect("surface width fits");
    let pad = usize::try_from(rt.window_padding_physical()).expect("padding fits");
    let focused = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("focused float frame")
        .frame;
    let idle = rt
        .present_frames()
        .into_iter()
        .find(|frame| frame.view == ViewId::new(3))
        .expect("unfocused float frame")
        .frame;
    let focused_pixel = pixel(
        &rgba,
        width,
        pad + usize::try_from(focused.x).expect("focused x fits") + focused.width as usize / 2,
        pad + usize::try_from(focused.y).expect("focused y fits"),
    );
    let idle_pixel = pixel(
        &rgba,
        width,
        pad + usize::try_from(idle.x).expect("idle x fits") + idle.width as usize - 1,
        pad + usize::try_from(idle.y).expect("idle y fits") + idle.height as usize / 2,
    );
    assert_eq!(
        focused_pixel,
        bitty_runtime::config::DEFAULT_OUTLINE_FOCUSED,
        "raised float must expose the focused accent ring"
    );
    assert_ne!(
        idle_pixel,
        bitty_runtime::config::DEFAULT_OUTLINE_FOCUSED,
        "other float must not paint focused chrome"
    );
}

#[test]
fn focus_next_cycles_through_floats_issue_2173() {
    // The focus-cycle shortcuts must reach every panel: tiled leaves plus
    // detached floats in stable detach order with wrap.
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    assert!(rt.set_focus(ViewId::new(1)), "tiled leaf focusable");
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(2)),
        "focus_next from the tile must reach the first float (issue #2173)"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(3)),
        "focus_next must continue across floats"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(1)),
        "focus_next wraps back to the tile"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Prev),
        Some(ViewId::new(3)),
        "focus_prev from the tile must reach the topmost float"
    );
}

#[test]
fn directional_focus_from_tiled_edge_does_not_cycle_into_overlays_issue_2173() {
    // A directional move from a tiled edge must preserve the layout behavior:
    // with no candidate in that direction, focus stays put instead of
    // jumping to another tile, detached float, or pin.
    let mut rt = make_runtime();
    install(&mut rt, four_pane());
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    rt.pin_floating(ViewId::new(4)).expect("pin 4");
    assert!(rt.set_focus(ViewId::new(1)), "tiled leaf focusable");

    assert_eq!(
        rt.move_focus(FocusDirection::Left),
        None,
        "directional focus at a tiled edge must not cycle into an overlay"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "a tiled edge move must leave focus on the tiled leaf"
    );
}

#[test]
fn directional_focus_from_float_reaches_tile_issue_2173() {
    // Directional focus movement must never strand on a float: hjkl/arrows
    // from a focused float land on a tile instead of returning None.
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    for dir in [
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::Left,
        FocusDirection::Right,
    ] {
        assert!(rt.set_focus(ViewId::new(2)), "float focusable");
        assert!(
            rt.move_focus(dir).is_some(),
            "directional focus {dir:?} from a float must land somewhere (issue #2173)"
        );
    }
    // The tiled leaf under the floats is always one valid landing spot.
    assert!(rt.set_focus(ViewId::new(3)), "second float focusable");
    let landed = rt
        .move_focus(FocusDirection::Left)
        .expect("left from a float must land");
    assert!(
        rt.layout().leaf_ids().contains(&landed) || rt.floated_views().contains(&landed),
        "directional landing {landed:?} must be a live panel"
    );
}

#[test]
fn focus_next_reaches_pinned_issue_2173() {
    // Window-global pins are focusable panels too: the cycle ring covers
    // tiled leaves, detached floats, then pins.
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.pin_floating(ViewId::new(3)).expect("pin 3");
    assert!(rt.set_focus(ViewId::new(1)), "tiled leaf focusable");
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(2)),
        "focus_next reaches the detached float first"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(3)),
        "focus_next then reaches the pin (issue #2173)"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(ViewId::new(1)),
        "focus_next wraps past the pin to the tile"
    );
}
