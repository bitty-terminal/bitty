#![forbid(unsafe_code)]
//! CTX-1147 (#1975): floating detaches from tiling, restore reinserts via
//! the placement policy.
//!
//! Floating a panel detaches it from the tiling into the workspace-local
//! float store (the vacated slot collapses, siblings refill — no ghost
//! chrome is structurally possible), and untoggling re-inserts it as a
//! fresh split via the #1804 balanced placement policy. The same `ViewId`
//! is preserved across the round trip. Detached floats present at
//! [`OverlayTier`](bitty_runtime::OverlayTier)::`Float` with anchored float
//! geometry (see `bitty_ui::presentation::float_frame`) and elevated border
//! chrome, painted before pinned leaves in detach order.

use bitty_platform::{CursorPosition, MouseButton, NamedKey, PressState};
use bitty_runtime::{
    LayoutNode, OverlayTier, PresentFrame, PresentationMode, Runtime, RuntimeConfig, SplitAxis,
    UiRect, View, ViewId,
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
fn detach_collapses_slot_siblings_refill_no_ghost() {
    // Issue #1975: after floating, the vacated slot collapses (siblings
    // refill the container) and only the float chrome shows — no slot
    // outline can ghost because no slot remains.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    let before = rt.present_frames();
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|frame| frame.tier.is_none()));

    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    // The tree holds only the survivor at the full container.
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(2)]);
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);
    let allocations = rt.layout_allocations();
    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0].1, UiRect::new(0, 0, 80, 24));
    // Detach moves focus off the floated leaf onto the survivor.
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));

    // The scene still presents both: the tiled survivor plus the float.
    let after = rt.present_frames();
    assert_eq!(after.len(), 2);
    let floated = after
        .iter()
        .find(|frame| frame.view == ViewId::new(1))
        .expect("detached leaf still presents");
    let base = after
        .iter()
        .find(|frame| frame.view == ViewId::new(2))
        .expect("survivor still presents");
    assert_eq!(floated.tier, Some(OverlayTier::Float));
    assert_eq!(base.tier, None);
    // Anchored float geometry replaces the old slot: the float frame is the
    // centered composite, never the vacated slot allocation (which no
    // longer exists). Border parity with pinned floats: composited overlay
    // layers carry the decoration border as-is.
    assert_ne!(floated.frame, base.frame);
    // The stable sort lifts the float above base: it paints last.
    assert_eq!(after.last().expect("frames").view, ViewId::new(1));

    // Restore re-inserts a fresh split back to tiling.
    let restored = rt
        .float_restore(ViewId::new(1))
        .expect("restore must apply");
    assert_eq!(restored, ViewId::new(1));
    assert!(!rt.floated_occupied());
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(2), ViewId::new(1)]);
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Tiled
    );
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    let back = rt.present_frames();
    assert_eq!(back.len(), 2);
    assert!(back.iter().all(|frame| frame.tier.is_none()));
}

#[test]
fn restore_reinserts_wide_side_by_side_and_tall_stacked() {
    // The #1804 balanced policy drives the fresh split: a physically wide
    // survivor docks side-by-side, a tall one stacks.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    // Sole survivor covers the 80x24 container: physically wide (24 cells
    // * 2.0 aspect = 48 < 80), so the fresh split docks side-by-side.
    let restored = rt
        .float_restore(ViewId::new(2))
        .expect("restore must apply");
    assert_eq!(restored, ViewId::new(2));
    let bounds = UiRect::new(0, 0, 80, 24);
    let alloc = rt.layout().layout(bounds);
    assert_eq!(alloc.len(), 2);
    for (_, rect) in &alloc {
        assert_eq!(rect.height, bounds.height);
    }

    // Tall container: the fresh split stacks vertically instead.
    let mut rt = make_runtime();
    rt.set_layout(LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(ViewId::new(1), 30, 24)),
        LayoutNode::leaf(View::new(ViewId::new(2), 30, 24)),
    ));
    rt.set_container(UiRect::new(0, 0, 30, 24));
    rt.float_detach(ViewId::new(2)).expect("detach must apply");
    rt.float_restore(ViewId::new(2))
        .expect("restore must apply");
    let tall = UiRect::new(0, 0, 30, 24);
    let alloc = rt.layout().layout(tall);
    assert_eq!(alloc.len(), 2);
    for (_, rect) in &alloc {
        assert_eq!(rect.width, tall.width, "stacked leaves keep full width");
    }
}

#[test]
fn toggle_routes_detach_then_restore_with_rejects() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    assert_eq!(
        rt.toggle_floating(Some(ViewId::new(1)))
            .expect("toggle to detach"),
        None
    );
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);
    assert_eq!(
        rt.toggle_floating(Some(ViewId::new(1)))
            .expect("toggle to restore"),
        Some(ViewId::new(1))
    );
    assert!(!rt.floated_occupied());

    assert!(rt.toggle_floating(None).is_err(), "toggle needs a target");
    assert!(
        rt.toggle_floating(Some(ViewId::new(404))).is_err(),
        "unknown id detaches nothing"
    );
    // A single-leaf layout refuses to strand itself.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(7), 80, 24)));
    assert!(
        rt.toggle_floating(Some(ViewId::new(7))).is_err(),
        "detach must not strand a single-leaf layout"
    );

    // String dispatch routes the same verb and rejects unknown commands
    // with state untouched.
    install(&mut rt, two_pane());
    assert_eq!(
        rt.apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(ViewId::new(1)))
            .expect("command detaches"),
        None
    );
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);
    let before = rt.layout().clone();
    let error = rt
        .apply_floating_command("bitty.workspace:nope", Some(ViewId::new(1)))
        .expect_err("unknown command fails closed");
    assert!(error.contains("unknown floating command"), "{error}");
    assert_eq!(rt.layout().clone(), before);
    assert_eq!(
        rt.apply_floating_command(bitty_runtime::FLOATING_CMD_TOGGLE, Some(ViewId::new(1)))
            .expect("command restores"),
        Some(ViewId::new(1))
    );
}

#[test]
fn round_trip_focus_stays_sane() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    assert!(rt.set_focus(ViewId::new(3)));
    rt.float_detach(ViewId::new(3)).expect("detach must apply");
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "detach re-homes focus onto the first survivor"
    );
    assert!(
        rt.set_focus(ViewId::new(3)),
        "a detached float is a valid focus target"
    );
    rt.float_restore(ViewId::new(3))
        .expect("restore must apply");
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(3)),
        "restore focuses the re-inserted leaf"
    );
}

#[test]
fn click_focuses_detached_float_over_the_base() {
    // Pointer routing follows paint: the detached float wins over the base
    // leaf it covers.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert!(rt.set_focus(ViewId::new(2)), "park focus on the base leaf");
    let pos = frame_point(&rt, ViewId::new(1), 0.75, 0.5);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(ViewId::new(1)),
        "the present hit test resolves the visible float"
    );
    rt.handle_cursor_moved(pos);
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "left click must focus the detached float, not the covered base leaf"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn alt_drag_grabs_detached_float_and_restore_resets() {
    // Alt+Left-press on a detached float grabs it for an Alt+drag (focus
    // follows, no selection starts); motion re-anchors the frame; restore
    // drops the offset with the fresh split.
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert!(rt.set_focus(ViewId::new(2)), "park focus on the base leaf");

    let pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    rt.handle_cursor_moved(pos);
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Pressed));
    rt.handle_mouse_input(press(MouseButton::Left));
    assert!(
        rt.alt_drag_active(),
        "Alt+press on a detached float must grab"
    );
    assert!(
        !rt.tiled_drag_active(),
        "a detached leaf must never start a tiled move"
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
    rt.handle_mouse_input(release(MouseButton::Left));
    assert!(!rt.alt_drag_active());
    assert!(
        !rt.has_selection(),
        "drag release must not commit a selection (desync guard)"
    );
    rt.handle_key_event(named_key(NamedKey::Alt, PressState::Released));

    // Restore drops the re-anchor discipline: the leaf returns as a plain
    // tiled split.
    rt.float_restore(ViewId::new(1))
        .expect("restore must apply");
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Tiled
    );
}

#[test]
fn detached_float_survives_session_round_trip_with_identical_present() {
    // In-memory capture/apply round-trip (no file I/O, no backend): the
    // detached float rides its workspace slot and presents identically.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    let fresh = frame_of(&rt, ViewId::new(1));
    assert_eq!(fresh.tier, Some(OverlayTier::Float));

    let snap = rt.capture_session_snapshot();
    assert_eq!(snap.workspaces.len(), 1);
    assert_eq!(snap.workspaces[0].floats.len(), 1);
    let float = &snap.workspaces[0].floats[0];
    assert_eq!(float.view.id(), ViewId::new(1));
    assert_eq!(float.view.presentation(), PresentationMode::Floating);

    let mut restored = make_runtime();
    restored.set_container(UiRect::new(0, 0, 80, 24));
    let summary = restored
        .apply_session_snapshot(&snap)
        .expect("captured snapshot applies");
    assert_eq!(summary.panes, 2, "surviving leaf plus the detached entry");
    assert_eq!(restored.floated_views(), vec![ViewId::new(1)]);
    assert_eq!(restored.layout().leaf_ids(), vec![ViewId::new(2)]);
    let back = frame_of(&restored, ViewId::new(1));
    assert_eq!(back.tier, Some(OverlayTier::Float));
    assert_eq!(back.frame, fresh.frame, "geometry recomputes identically");
    assert_eq!(back.content, fresh.content);
    assert_eq!(back.border, fresh.border);

    // Restore-after-load re-inserts the floating panel as a fresh split.
    let id = restored
        .float_restore(ViewId::new(1))
        .expect("restore must apply");
    assert_eq!(id, ViewId::new(1));
    assert!(!restored.floated_occupied());
    assert_eq!(restored.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn detached_floats_stay_with_their_workspace() {
    // Floats are workspace-local (unlike pins): switching away hides them
    // with their slot, and switching back restores them.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(1)).expect("detach must apply");

    rt.workspace_new().expect("second workspace must open");
    assert!(
        !rt.floated_occupied(),
        "the fresh workspace starts with no detached floats"
    );
    assert!(
        rt.present_frames()
            .iter()
            .all(|frame| frame.view != ViewId::new(1)),
        "the detached float stays in its workspace"
    );

    rt.workspace_prev();
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);
    assert_eq!(frame_of(&rt, ViewId::new(1)).tier, Some(OverlayTier::Float));
}

#[test]
fn pinned_interplay_is_unchanged_and_justified() {
    // A detached float is not in any layout: pinning it fails closed (pin
    // through untoggle-then-pin), and detaching a pinned leaf fails closed
    // (restore through unpin first). Unpin still returns the floating panel
    // to the active workspace tree.
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.float_detach(ViewId::new(1)).expect("detach must apply");
    assert!(
        rt.pin_floating(ViewId::new(1)).is_err(),
        "a detached float is not pinnable until restored"
    );
    assert_eq!(rt.floated_views(), vec![ViewId::new(1)]);

    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.pin_floating(ViewId::new(2)).expect("pin must apply");
    assert!(
        rt.float_detach(ViewId::new(2)).is_err(),
        "a pinned leaf is not detachable until unpinned"
    );
    let restored = rt.unpin_floating(ViewId::new(2)).expect("unpin must apply");
    assert_eq!(restored, ViewId::new(2));
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(2))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating,
        "unpin keeps the floating panel contract"
    );
}
