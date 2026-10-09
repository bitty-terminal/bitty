#![forbid(unsafe_code)]
//! CTX-1058 (#1844 P1): the Mod+a toggle path is visible at the
//! `present_frames` level.
//!
//! Toggling a tiled leaf to `Floating` lifts it to
//! [`OverlayTier`](bitty_runtime::OverlayTier)::`Float` with anchored float
//! geometry (see `bitty_ui::presentation::float_frame`) and elevated border
//! chrome, and it paints after base content. The tiling solver allocations
//! stay byte-identical across the toggle (slot restore), and toggling back
//! restores the exact prior frame.

use bitty_runtime::{LayoutNode, OverlayTier, Runtime, SplitAxis, UiRect, View, ViewId};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn two_pane() -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
    )
}

fn install(rt: &mut Runtime, layout: LayoutNode) {
    rt.set_layout(layout);
    rt.set_container(UiRect::new(0, 0, 80, 24));
}

fn toggle(rt: &mut Runtime, id: ViewId) {
    let mut tree = rt.layout().clone();
    bitty_ui::presentation::toggle_floating(&mut tree, id).expect("toggle must apply");
    rt.set_layout(tree);
}

#[test]
fn floating_toggle_lifts_tier_geometry_and_paint_order() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());

    let before = rt.present_frames();
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|frame| frame.tier.is_none()));
    let tiled_frame = *before
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("leaf 1 presents");
    let tiled_allocations = rt.layout_allocations();

    // The toggle path: Tiled -> Floating through the workspace command
    // primitive the Mod+a dispatch stamps through.
    toggle(&mut rt, ViewId::new(1));

    let after = rt.present_frames();
    assert_eq!(after.len(), 2);
    let floated = after
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("leaf 1 still presents");
    let base = after
        .iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("leaf 2 still presents");
    // Tier flips None -> Float while the untouched sibling stays base.
    assert_eq!(floated.tier, Some(OverlayTier::Float));
    assert_eq!(base.tier, None);
    // Anchored float geometry replaces the solver allocation.
    assert_ne!(floated.frame, tiled_frame.frame);
    assert_ne!(floated.content, tiled_frame.content);
    // Elevated float chrome: one extra border px, content kept inside it.
    assert_eq!(
        floated.border,
        tiled_frame
            .border
            .saturating_add(bitty_ui::presentation::FLOAT_BORDER_EXTRA)
    );
    assert!(floated.border > tiled_frame.border);
    // Stable sort lifts the float above base: it paints last.
    assert_eq!(after.last().expect("frames").view, ViewId::new(1));
    // Slot restore: the solver never saw the mode stamp.
    assert_eq!(rt.layout_allocations(), tiled_allocations);

    // Toggling back restores the exact prior present frame.
    toggle(&mut rt, ViewId::new(1));
    let restored = rt.present_frames();
    assert_eq!(restored.len(), 2);
    assert!(restored.iter().all(|frame| frame.tier.is_none()));
    let back = restored
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("leaf 1 presents");
    assert_eq!(back.frame, tiled_frame.frame);
    assert_eq!(back.content, tiled_frame.content);
    assert_eq!(back.border, tiled_frame.border);
    assert_eq!(rt.layout_allocations(), tiled_allocations);
}

#[test]
fn structural_tier_wins_over_mode_stamp() {
    // A leaf inside a structural overlay keeps its structural tier even when
    // stamped Floating: the overlay composition owns its paint position, so
    // the present override must not split it apart.
    use bitty_runtime::OverlayLayer;
    let bounds = UiRect::new(5, 5, 20, 10);
    let tree = LayoutNode::overlay_stack(
        LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
        vec![OverlayLayer::new(
            OverlayTier::Popup,
            LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            bounds,
        )],
    );
    let mut rt = make_runtime();
    install(&mut rt, tree);
    toggle(&mut rt, ViewId::new(2));
    let frames = rt.present_frames();
    let overlay = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("overlay leaf presents");
    assert_eq!(overlay.tier, Some(OverlayTier::Popup));
    assert_eq!(frames.last().expect("frames").view, ViewId::new(2));
}
