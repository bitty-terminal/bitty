#![forbid(unsafe_code)]
//! Issue #2993: a second floating panel must stay reachable by keyboard.
//!
//! Owner report on 0.0.35: "With more than one floating panel, only one of
//! them can be returned to tiling with the keyboard. The second one can only
//! be focused with the mouse."
//!
//! The focus ring itself was never the problem: `focusable_view_ids` already
//! chains tiled leaves, detached floats, and pins, so `focus_next`
//! (`Ctrl+Tab`) reaches every float and `toggle_floating` tiles whichever one
//! is focused. The break was in *directional* focus (the default `Mod+hjkl`
//! panel movement): the detached-float fallback ranked the covered tiled base
//! ahead of a sibling float, because a float is centered over the base and the
//! base's center is therefore closer to the source's center than the cascaded
//! sibling's. Every direction from the top float escaped to the tiled pane
//! underneath, so the buried float could only be reached with the pointer.
//!
//! RED on origin/main: `directional_focus_reaches_the_sibling_float`.
//! Guards (green before and after): cycle reach and keyboard tiling of both
//! floats, and the tiled-only spatial tables.

use bitty_runtime::{
    FocusDirection, LayoutNode, OverlayTier, Runtime, RuntimeConfig, SplitAxis, UiRect, View,
    ViewId,
};

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
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

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

/// One tiled base (view 1) plus two detached floats (views 2 and 3) — the
/// shape the owner reported. View 3 detached last, so it paints on top.
fn two_floats() -> (Runtime, ViewId, ViewId, ViewId) {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    rt.float_detach(ViewId::new(2)).expect("detach 2");
    rt.float_detach(ViewId::new(3)).expect("detach 3");
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1)]);
    assert_eq!(
        rt.floated_views(),
        vec![ViewId::new(2), ViewId::new(3)],
        "both floats live in the detached store"
    );
    let tiers: Vec<Option<OverlayTier>> = rt
        .present_frames()
        .into_iter()
        .filter(|frame| frame.view != ViewId::new(1))
        .map(|frame| frame.tier)
        .collect();
    assert_eq!(
        tiers,
        vec![Some(OverlayTier::Float), Some(OverlayTier::Float)],
        "both floats composite on the float tier"
    );
    (rt, ViewId::new(1), ViewId::new(2), ViewId::new(3))
}

#[test]
fn directional_focus_reaches_the_sibling_float() {
    // RED on origin/main: from the top float every direction resolved to the
    // covered tiled base, so the other float was unreachable with the
    // default directional focus chords.
    let (mut rt, base, first, top) = two_floats();

    // From the tiled base, at least one direction reaches each float.
    let mut reached_from_base = Vec::new();
    for dir in [
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::Left,
        FocusDirection::Right,
    ] {
        assert!(rt.set_focus(base));
        if let Some(landed) = rt.move_focus(dir) {
            reached_from_base.push(landed);
        }
    }
    assert!(
        reached_from_base.contains(&first) && reached_from_base.contains(&top),
        "the tiled base must be able to step onto either float: {reached_from_base:?}"
    );

    // From the top float, a directional move must be able to land on the
    // sibling float it overlaps instead of only escaping to the base.
    let mut reached_from_top = Vec::new();
    for dir in [
        FocusDirection::Up,
        FocusDirection::Down,
        FocusDirection::Left,
        FocusDirection::Right,
    ] {
        assert!(rt.set_focus(top));
        if let Some(landed) = rt.move_focus(dir) {
            reached_from_top.push(landed);
        }
    }
    assert!(
        reached_from_top.contains(&first),
        "directional focus from one float must reach the other float, got {reached_from_top:?}"
    );
    assert!(
        reached_from_top.contains(&base),
        "directional focus from a float must still reach the tiled pane, got {reached_from_top:?}"
    );
}

#[test]
fn focus_cycle_and_toggle_tile_both_floats() {
    // Guard: the cycle ring already covers every float (issue #2173), so
    // `toggle_floating` on the focused float returns each of them to tiling
    // in turn.
    let (mut rt, base, first, top) = two_floats();

    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(first),
        "focus_next from the tile reaches the first float"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(top),
        "focus_next continues to the second float"
    );
    assert_eq!(
        rt.move_focus(FocusDirection::Next),
        Some(base),
        "focus_next wraps back to the tile"
    );

    for float in [first, top] {
        assert!(rt.set_focus(float), "{float:?} must be focusable");
        let restored = rt
            .apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(float))
            .expect("toggle_floating on a focused float must apply");
        assert_eq!(restored, Some(float), "{float:?} returns to tiling");
        assert!(
            rt.layout().leaf_ids().contains(&float),
            "{float:?} must be back in the tiled layout"
        );
        assert!(
            !rt.floated_views().contains(&float),
            "{float:?} must have left the float store"
        );
    }
    assert!(rt.floated_views().is_empty());
    assert_eq!(rt.layout().leaf_ids().len(), 3, "three tiled panes again");
}

#[test]
fn tiled_directional_focus_still_stops_at_the_layout_edge() {
    // Guard: the sibling-overlay preference is confined to the overlap
    // fallback. Two tiled panes never move focus across each other, and a
    // tiled pane at the layout edge keeps its directional stop.
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    assert!(rt.set_focus(ViewId::new(1)));
    assert_eq!(
        rt.move_focus(FocusDirection::Left),
        None,
        "a tiled edge keeps the layout-edge stop"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "focus stays on the tiled pane"
    );
}
