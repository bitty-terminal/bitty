#![forbid(unsafe_code)]
//! CW-12 (issue #991): the present path consumes the tiered overlay
//! primitives (`overlay_tiered` / `overlay_stack`).
//!
//! `Runtime::present_frames` carries each leaf's innermost enclosing
//! [`OverlayTier`](bitty_runtime::OverlayTier) and stable-sorts base
//! content first, then `Editor < Float < Popup < Messages`, so stacked
//! overlays composite in tier order regardless of tree construction
//! order. Same-tier frames keep solver order.

use bitty_runtime::{LayoutNode, OverlayLayer, OverlayTier, Runtime, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn leaf(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn paint_order(rt: &Runtime) -> Vec<(ViewId, Option<OverlayTier>)> {
    rt.present_frames()
        .iter()
        .map(|frame| (frame.view, frame.tier))
        .collect()
}

#[test]
fn present_frames_paint_tier_order_despite_reverse_construction() {
    // Hand-nested tree where depth-first (construction) order disagrees
    // with tier order: the base subtree owns a Messages leaf while the
    // outer overlay sits at Editor. Depth-first emits [1, 4, 2]; the
    // present path must paint [1, 2, 4].
    let mut rt = make_runtime();
    let bounds = UiRect::new(5, 5, 20, 10);
    let tree = LayoutNode::overlay_tiered(
        LayoutNode::overlay_tiered(leaf(1), leaf(4), bounds, OverlayTier::Messages),
        leaf(2),
        bounds,
        OverlayTier::Editor,
    );
    install(&mut rt, tree);
    assert_eq!(
        paint_order(&rt),
        vec![
            (ViewId::new(1), None),
            (ViewId::new(2), Some(OverlayTier::Editor)),
            (ViewId::new(4), Some(OverlayTier::Messages)),
        ]
    );
    // The tiered frame still presents through the full tick path.
    let stats = rt.tick().expect("tiered tree must present");
    assert!(stats.headless);
}

#[test]
fn present_frames_stack_insertion_order_is_irrelevant() {
    // Same layers as `overlay_stack`, scrambled vs sorted insertion:
    // identical present order, base first then tiers lowest-first.
    let layer = |tier, id: u64| OverlayLayer::new(tier, leaf(id), UiRect::new(0, 0, 10, 5));
    let scrambled = LayoutNode::overlay_stack(
        leaf(1),
        vec![
            layer(OverlayTier::Messages, 4),
            layer(OverlayTier::Float, 2),
            layer(OverlayTier::Popup, 3),
            layer(OverlayTier::Editor, 5),
        ],
    );
    let sorted = LayoutNode::overlay_stack(
        leaf(1),
        vec![
            layer(OverlayTier::Editor, 5),
            layer(OverlayTier::Float, 2),
            layer(OverlayTier::Popup, 3),
            layer(OverlayTier::Messages, 4),
        ],
    );
    let mut rt_a = make_runtime();
    install(&mut rt_a, scrambled);
    let mut rt_b = make_runtime();
    install(&mut rt_b, sorted);
    let expected = vec![
        (ViewId::new(1), None),
        (ViewId::new(5), Some(OverlayTier::Editor)),
        (ViewId::new(2), Some(OverlayTier::Float)),
        (ViewId::new(3), Some(OverlayTier::Popup)),
        (ViewId::new(4), Some(OverlayTier::Messages)),
    ];
    assert_eq!(paint_order(&rt_a), expected);
    assert_eq!(paint_order(&rt_b), expected);
}

#[test]
fn present_frames_same_tier_keeps_solver_order() {
    // Same-tier conflict rule: later-constructed paints above (after).
    let layer =
        |id: u64| OverlayLayer::new(OverlayTier::Float, leaf(id), UiRect::new(10, 5, 30, 10));
    let mut rt = make_runtime();
    install(
        &mut rt,
        LayoutNode::overlay_stack(leaf(1), vec![layer(2), layer(3)]),
    );
    assert_eq!(
        paint_order(&rt),
        vec![
            (ViewId::new(1), None),
            (ViewId::new(2), Some(OverlayTier::Float)),
            (ViewId::new(3), Some(OverlayTier::Float)),
        ]
    );
}

#[test]
fn present_frames_untiered_tree_is_base_only() {
    // No overlays: every frame is base content, solver order unchanged.
    let mut rt = make_runtime();
    install(&mut rt, leaf(1));
    assert_eq!(paint_order(&rt), vec![(ViewId::new(1), None)]);
}
