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

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{
    LayoutNode, OverlayTier, PresentFrame, Runtime, RuntimeConfig, SplitAxis, UiRect, View, ViewId,
};

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

fn frame_of(rt: &Runtime, view: ViewId) -> PresentFrame {
    rt.present_frames()
        .into_iter()
        .find(|frame| frame.view == view)
        .unwrap_or_else(|| panic!("{view:?} must be presented"))
}

/// Physical cursor position at fractional (`fx`, `fy`) offsets inside
/// `view`'s present hit-test frame, derived from public geometry only (no
/// hard-coded pixel, padding, or decoration constants).
fn frame_point(rt: &Runtime, view: ViewId, fx: f64, fy: f64) -> CursorPosition {
    let frame = frame_of(rt, view);
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + f64::from(frame.frame.x) + f64::from(frame.frame.width) * fx,
        y: pad + f64::from(frame.frame.y) + f64::from(frame.frame.height) * fy,
    }
}

fn named_key(named: NamedKey, state: PressState) -> bitty_platform::KeyEvent {
    bitty_platform::KeyEvent {
        logical_key: bitty_platform::LogicalKey::Named(named),
        text: None,
        location: bitty_platform::KeyLocation::Standard,
        state,
        repeat: false,
        is_synthetic: false,
    }
}

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
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
fn floated_content_origin_stays_inside_degenerate_frame() {
    // CodeRabbit 1873: on a degenerate (tiny) container the capped insets
    // must keep the content origin inside the float frame instead of
    // overshooting past its far edge.
    let mut rt = make_runtime();
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(UiRect::new(0, 0, 4, 2));
    toggle(&mut rt, ViewId::new(1));
    let frames = rt.present_frames();
    let floated = frames
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("leaf presents");
    assert_eq!(floated.tier, Some(OverlayTier::Float));
    assert!(floated.content.x >= floated.frame.x);
    assert!(floated.content.y >= floated.frame.y);
    assert!(
        floated.content.x <= floated.frame.x + floated.frame.width as i32,
        "content x overshoots float frame"
    );
    assert!(
        floated.content.y <= floated.frame.y + floated.frame.height as i32,
        "content y overshoots float frame"
    );
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

#[test]
fn click_focuses_topmost_mode_float() {
    // CTX-1058 (#1844 P2): pointer routing follows paint. Leaf 1 floats over
    // the centered float frame while its solver slot stays the left half, so
    // a point on the right side of the float is visually the float but
    // geometrically inside leaf 2's slot. Click-to-focus must resolve the
    // visible (topmost painted) leaf, agreeing with the selection press hit
    // test — not the base leaf painted beneath.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    assert!(rt.set_focus(ViewId::new(2)), "park focus on the base leaf");
    // Right side of the float frame: visually the float, geometrically in
    // leaf 2's slot (the anchored float covers ~80% centered).
    let pos = frame_point(&rt, ViewId::new(1), 0.75, 0.5);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(ViewId::new(1)),
        "the present hit test resolves the visible float"
    );
    assert_eq!(
        rt.cursor_to_leaf_cell(pos).map(|(view, _)| view),
        Some(ViewId::new(2)),
        "the solver hit test still sees the covered slot (tiled-drag model)"
    );
    rt.handle_cursor_moved(pos);
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(2)),
        "hover alone must not move focus when disabled"
    );
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "left click must focus the topmost float, not the covered base leaf"
    );
    assert!(
        !rt.has_selection(),
        "the grid-less float press selects nothing instead of the base grid"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn alt_drag_grabs_mode_floating_leaf() {
    // CTX-1058 (#1844 P2): Alt+Left-press on a mode-floating leaf grabs it
    // for an Alt+drag (focus follows, no selection starts) instead of
    // falling through to the Mod tiled-drag, which would re-parent the float
    // on release. The anchored float geometry has no stored position to
    // move, so motion keeps the drag armed with the slot byte-identical.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    assert!(rt.set_focus(ViewId::new(2)), "park focus on the base leaf");
    let slots = rt.layout_allocations();
    // Right side of the float frame, as above: the old solver-order lookup
    // resolved the covered base leaf (and tiled-dragged it); the present
    // order resolves the float.
    let pos = frame_point(&rt, ViewId::new(1), 0.75, 0.5);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(ViewId::new(1)),
        "the present hit test resolves the visible float"
    );
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(rt.alt_drag_active(), "Alt+press on a mode float must grab");
    assert!(
        !rt.tiled_drag_active(),
        "a floating leaf must never start a tiled move"
    );
    assert!(
        !rt.is_selection_dragging(),
        "the grabbing press must not start selection"
    );
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "grabbing focuses the dragged float"
    );
    // Motion owns the gesture (no selection, no hover steal) while the
    // anchored geometry — and the solver slot — stay exactly put.
    rt.handle_cursor_moved(frame_point(&rt, ViewId::new(1), 0.25, 0.5));
    assert!(rt.alt_drag_active(), "motion keeps a mode-float drag armed");
    assert!(!rt.has_selection(), "owned motion selects nothing");
    assert_eq!(
        rt.layout_allocations(),
        slots,
        "anchored float motion never moves the solver slot"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.alt_drag_active());
    assert!(
        !rt.has_selection(),
        "drag release must not commit a selection (desync guard)"
    );
    assert_eq!(
        rt.layout_allocations(),
        slots,
        "the release re-parents nothing"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));
}
