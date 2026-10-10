#![forbid(unsafe_code)]
//! CTX-1077 (#1757 P2): pinned / sticky floating panels across workspaces.
//!
//! Pinning detaches a floating leaf into the window-global pinned store and
//! the present path composites it over the active scene, so the same leaf
//! stays visible across workspace switches without ever living in two trees
//! at once. Unpinning returns the (still floating) panel to the currently
//! active workspace. Pointer routing follows paint through the present
//! frames, and focus survives the round trip.

use bitty_platform::{CursorPosition, MouseButton, PressState};
use bitty_runtime::{
    LayoutNode, OverlayTier, PaneAttachment, PaneRoute, PaneSnapshot, PinnedSnapshot,
    PresentationMode, Runtime, RuntimeConfig, SESSION_FORMAT_VERSION, SessionSnapshot, SplitAxis,
    UiRect, View, ViewId, WorkspaceSnapshot,
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

fn toggle(rt: &mut Runtime, id: ViewId) {
    let mut tree = rt.layout().clone();
    bitty_ui::presentation::toggle_floating(&mut tree, id).expect("toggle must apply");
    rt.set_layout(tree);
}

fn frame_of(rt: &Runtime, view: ViewId) -> bitty_runtime::PresentFrame {
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

fn press(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Pressed)
}

fn release(button: MouseButton) -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(button, PressState::Released)
}

#[test]
fn pin_survives_workspace_switch_without_duplicating_views() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));

    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    assert_eq!(rt.pinned_views(), vec![ViewId::new(1)]);
    assert!(rt.pinned_occupied());
    // Pin moves focus off the pinned leaf onto the first surviving leaf.
    assert_eq!(rt.focused_view(), Some(ViewId::new(2)));
    // The live layout no longer owns the leaf, but the scene still does.
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(2)]);
    assert_eq!(frame_of(&rt, ViewId::new(1)).tier, Some(OverlayTier::Float));

    rt.workspace_new().expect("second workspace must open");
    // The same leaf presents in the new workspace scene exactly once: no
    // duplication, same identity, still floating.
    let hits: Vec<_> = rt
        .present_frames()
        .into_iter()
        .filter(|frame| frame.view == ViewId::new(1))
        .collect();
    assert_eq!(hits.len(), 1, "pinned leaf presents exactly once");
    assert_eq!(hits[0].tier, Some(OverlayTier::Float));

    rt.workspace_prev();
    let back: Vec<_> = rt
        .present_frames()
        .into_iter()
        .filter(|frame| frame.view == ViewId::new(1))
        .collect();
    assert_eq!(back.len(), 1, "pinned leaf follows the switch back");
    assert_eq!(back[0].tier, Some(OverlayTier::Float));
}

#[test]
fn unpin_returns_floating_panel_to_active_workspace() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");

    rt.workspace_new().expect("second workspace must open");
    let restored = rt.unpin_floating(ViewId::new(1)).expect("unpin must apply");
    assert_eq!(restored, ViewId::new(1));
    assert!(!rt.pinned_occupied());
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
    // Unpin keeps the Floating mode: it returns the floating panel, it does
    // not re-tile it.
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating
    );
    assert_eq!(frame_of(&rt, ViewId::new(1)).tier, Some(OverlayTier::Float));

    // Normal float behavior resumes: the float belongs to its workspace and
    // does not follow switches anymore.
    rt.workspace_prev();
    assert!(
        rt.present_frames()
            .iter()
            .all(|frame| frame.view != ViewId::new(1)),
        "unpinned float stays in its workspace"
    );
    rt.workspace_next();
    assert_eq!(frame_of(&rt, ViewId::new(1)).tier, Some(OverlayTier::Float));
}

#[test]
fn pinned_paints_above_float_above_tiled() {
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    // Leaf 2 is a same-workspace mode float; leaf 3 pins (tiled converges
    // on floating at pin time).
    toggle(&mut rt, ViewId::new(2));
    rt.pin_floating(ViewId::new(3)).expect("pin must apply");

    let frames = rt.present_frames();
    assert_eq!(frames.len(), 3);
    let tiers: Vec<_> = frames.iter().map(|frame| frame.tier).collect();
    assert_eq!(
        tiers,
        vec![None, Some(OverlayTier::Float), Some(OverlayTier::Float)],
        "tiled base first, floats after"
    );
    let order: Vec<_> = frames.iter().map(|frame| frame.view).collect();
    assert_eq!(
        order,
        vec![ViewId::new(1), ViewId::new(2), ViewId::new(3)],
        "stable sort: pinned paints above the same-tier mode float"
    );
    assert_eq!(frames.last().expect("frames").view, ViewId::new(3));
}

#[test]
fn click_focuses_pinned_float_in_another_workspace() {
    let mut rt = Runtime::new(RuntimeConfig {
        focus_follows_mouse: false,
        ..RuntimeConfig::default()
    })
    .expect("opt-out runtime must build");
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    rt.workspace_new().expect("second workspace must open");

    let pos = frame_point(&rt, ViewId::new(1), 0.5, 0.5);
    assert_eq!(
        rt.cursor_to_present_cell(pos).map(|(view, _)| view),
        Some(ViewId::new(1)),
        "the present hit test resolves the sticky float in the new workspace"
    );
    assert!(
        rt.set_focus(ViewId::new(1)),
        "a pinned leaf is a valid focus target outside the live layout"
    );
    rt.handle_cursor_moved(pos);
    rt.handle_mouse_input(press(MouseButton::Left));
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(1)),
        "left click must focus the sticky float, not the covered base leaf"
    );
    rt.handle_mouse_input(release(MouseButton::Left));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn toggle_pinned_roundtrip_and_rejects() {
    let mut rt = make_runtime();
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    rt.set_container(UiRect::new(0, 0, 80, 24));
    assert!(rt.toggle_pinned(None).is_err(), "toggle needs a target");
    assert!(
        rt.toggle_pinned(Some(ViewId::new(1))).is_err(),
        "pin must not strand a single-leaf layout"
    );
    assert!(
        rt.pin_floating(ViewId::new(404)).is_err(),
        "unknown id pins nothing"
    );
    assert!(
        rt.unpin_floating(ViewId::new(404)).is_err(),
        "unknown id unpins nothing"
    );

    install(&mut rt, two_pane());
    assert_eq!(
        rt.toggle_pinned(Some(ViewId::new(1)))
            .expect("toggle to pin"),
        None
    );
    assert_eq!(rt.pinned_count(), 1);
    assert!(
        rt.pin_floating(ViewId::new(1)).is_err(),
        "double pin refused"
    );
    assert_eq!(
        rt.toggle_pinned(Some(ViewId::new(1)))
            .expect("toggle to unpin"),
        Some(ViewId::new(1))
    );
    assert_eq!(rt.pinned_count(), 0);
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf restored")
            .presentation(),
        PresentationMode::Floating
    );
}

#[test]
fn focus_clamps_when_pinned_leaf_unpins_elsewhere() {
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    toggle(&mut rt, ViewId::new(1));
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    // Focus the sticky float, then switch away: the stashed focus names a
    // leaf outside every slot layout.
    assert!(rt.set_focus(ViewId::new(1)));
    rt.workspace_new().expect("second workspace must open");
    // Unpin into the second workspace and switch home: the home slot still
    // remembers the pinned focus, but the leaf now lives elsewhere.
    rt.unpin_floating(ViewId::new(1)).expect("unpin must apply");
    rt.workspace_prev();
    assert_eq!(
        rt.focused_view(),
        Some(ViewId::new(2)),
        "stale pinned focus clamps to the first live leaf"
    );
    assert!(
        rt.present_frames()
            .iter()
            .all(|frame| frame.view != ViewId::new(1)),
        "the unpinned float stays in its own workspace"
    );
    // And the round trip back still presents it there.
    rt.workspace_next();
    assert_eq!(frame_of(&rt, ViewId::new(1)).tier, Some(OverlayTier::Float));
    assert_eq!(rt.focused_view(), Some(ViewId::new(1)));
}

#[test]
fn stacked_pins_cascade_so_each_stays_visible() {
    // CodeRabbit 1883: stacked pins must not fully cover each other. Each
    // pin offsets by pin depth (3 right, 2 down, clamped to the container).
    let mut rt = make_runtime();
    install(&mut rt, three_pane());
    toggle(&mut rt, ViewId::new(1));
    toggle(&mut rt, ViewId::new(2));
    rt.pin_floating(ViewId::new(1)).expect("pin 1 must apply");
    rt.pin_floating(ViewId::new(2)).expect("pin 2 must apply");
    let first = frame_of(&rt, ViewId::new(1));
    let second = frame_of(&rt, ViewId::new(2));
    assert_eq!(first.tier, Some(OverlayTier::Float));
    assert_eq!(second.tier, Some(OverlayTier::Float));
    // Frames are physical pixels (scaled cells); assert cascade shape, not
    // exact cells: distinct origins, down-right direction, same size.
    assert_ne!(
        (second.frame.x, second.frame.y),
        (first.frame.x, first.frame.y),
        "second pin must not fully cover the first"
    );
    assert!(
        second.frame.x > first.frame.x && second.frame.y > first.frame.y,
        "cascade runs down-right from the earlier pin"
    );
    assert_eq!(
        (second.frame.width, second.frame.height),
        (first.frame.width, first.frame.height),
        "cascade shifts origin only, never resizes"
    );
}

#[test]
fn pinned_leaf_survives_session_round_trip_with_identical_present() {
    // CTX-1082: detached pinned leaves were lost on restart (capture
    // iterated slot layouts only). In-memory capture/apply round-trip (no
    // file I/O, no backend).
    let mut rt = make_runtime();
    install(&mut rt, two_pane());
    rt.pin_floating(ViewId::new(1)).expect("pin must apply");
    let fresh = frame_of(&rt, ViewId::new(1));
    assert_eq!(fresh.tier, Some(OverlayTier::Float));

    let snap = rt.capture_session_snapshot();
    assert_eq!(snap.version, SESSION_FORMAT_VERSION);
    // The live layout holds only the survivor; the pin rides the snapshot.
    assert_eq!(snap.workspaces.len(), 1);
    assert_eq!(snap.pinned.len(), 1);
    let pin = &snap.pinned[0];
    assert_eq!(pin.view.id(), ViewId::new(1));
    assert_eq!(pin.view.presentation(), PresentationMode::Floating);
    assert_eq!(
        pin.attach,
        PaneAttachment::Detached,
        "no live session backs the pin"
    );
    assert_eq!(pin.anchor, Some(ViewId::new(2)));
    assert!(!pin.after, "pin anchors before the following neighbor");
    assert!(pin.cwd.is_none() && pin.scrollback.is_empty());

    let mut restored = make_runtime();
    restored.set_container(UiRect::new(0, 0, 80, 24));
    let summary = restored
        .apply_session_snapshot(&snap)
        .expect("captured snapshot applies");
    assert_eq!(summary.panes, 2, "surviving leaf plus the pinned entry");
    assert_eq!(summary.pending, 0, "detached pin earns no pending entry");
    assert_eq!(restored.pinned_views(), vec![ViewId::new(1)]);
    assert_eq!(restored.layout().leaf_ids(), vec![ViewId::new(2)]);
    assert!(
        !restored.session_pending_contains(&ViewId::new(1)),
        "detached pin earns no pending entry"
    );
    let back = frame_of(&restored, ViewId::new(1));
    assert_eq!(back.tier, Some(OverlayTier::Float));
    assert_eq!(
        back.frame, fresh.frame,
        "pinned geometry recomputes identically"
    );
    assert_eq!(back.content, fresh.content);
    assert_eq!(back.border, fresh.border);

    // Unpin-after-restore returns the floating panel beside its anchor.
    let id = restored
        .unpin_floating(ViewId::new(1))
        .expect("unpin must apply");
    assert_eq!(id, ViewId::new(1));
    assert!(!restored.pinned_occupied());
    assert_eq!(restored.focused_view(), Some(ViewId::new(1)));
    assert_eq!(
        restored
            .layout()
            .find_leaf(ViewId::new(1))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating
    );
}

#[test]
fn pinned_session_history_waits_pending_and_unpins_after_restore() {
    // CTX-1082: an attached pinned leaf's history rides the snapshot and
    // waits pending like an attached layout pane; unpin-after-restore
    // returns the floating panel. In-memory (no file I/O, no backend).
    let pane = |id: u64, attach: PaneAttachment, history: &[&str]| PaneSnapshot {
        view: ViewId::new(id),
        cwd: None,
        scrollback: history.iter().map(|line| line.to_string()).collect(),
        attach: Some(attach),
        route: PaneRoute::Terminal,
        mode: PresentationMode::Tiled,
    };
    let snap = SessionSnapshot {
        version: SESSION_FORMAT_VERSION,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(100), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(101), 80, 24)),
            ),
            focus: Some(ViewId::new(100)),
            panes: vec![
                pane(100, PaneAttachment::Primary, &["owner-history"]),
                pane(101, PaneAttachment::Detached, &[]),
            ],
        }],
        active: 0,
        mru: vec![0],
        pinned: vec![PinnedSnapshot {
            view: View::with_presentation(ViewId::new(9), 80, 24, PresentationMode::Floating),
            cwd: None,
            scrollback: vec!["pinned-history".to_string()],
            attach: PaneAttachment::Session,
            anchor: Some(ViewId::new(101)),
            after: false,
        }],
    };

    let mut rt = make_runtime();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let summary = rt.apply_session_snapshot(&snap).expect("apply valid");
    assert_eq!(summary.panes, 3, "layout leaves plus the pinned entry");
    assert_eq!(summary.pending, 1, "only the pinned session waits pending");
    assert_eq!(rt.pinned_views(), vec![ViewId::new(9)]);
    assert!(rt.session_pending_contains(&ViewId::new(9)));
    assert!(
        !rt.session_pending_contains(&ViewId::new(101)),
        "detached layout leaf earns no pending entry"
    );
    let frame = frame_of(&rt, ViewId::new(9));
    assert_eq!(frame.tier, Some(OverlayTier::Float));

    let id = rt.unpin_floating(ViewId::new(9)).expect("unpin must apply");
    assert_eq!(id, ViewId::new(9));
    assert!(!rt.pinned_occupied());
    assert_eq!(rt.focused_view(), Some(ViewId::new(9)));
    assert!(rt.layout().leaf_ids().contains(&ViewId::new(9)));
    assert_eq!(
        rt.layout()
            .find_leaf(ViewId::new(9))
            .expect("leaf back in the live tree")
            .presentation(),
        PresentationMode::Floating
    );
    // The pending restore survives the unpin: the next spawn of the leaf
    // hydrates it.
    assert!(rt.session_pending_contains(&ViewId::new(9)));
}

#[test]
fn old_version_snapshot_without_pinned_still_applies() {
    // CTX-1082 compat: a v2 snapshot (no pinned block) still loads — the
    // world restores with an empty pinned store.
    let pane = |id: u64, attach: PaneAttachment| PaneSnapshot {
        view: ViewId::new(id),
        cwd: None,
        scrollback: Vec::new(),
        attach: Some(attach),
        route: PaneRoute::Terminal,
        mode: PresentationMode::Tiled,
    };
    let snap = SessionSnapshot {
        version: 2,
        workspaces: vec![WorkspaceSnapshot {
            seq: 1,
            name: "ws1".to_string(),
            layout: LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            ),
            focus: Some(ViewId::new(1)),
            panes: vec![
                pane(1, PaneAttachment::Primary),
                pane(2, PaneAttachment::Session),
            ],
        }],
        active: 0,
        mru: vec![0],
        pinned: Vec::new(),
    };

    let mut rt = make_runtime();
    rt.set_container(UiRect::new(0, 0, 80, 24));
    let summary = rt.apply_session_snapshot(&snap).expect("v2 applies");
    assert_eq!(summary.panes, 2);
    assert_eq!(rt.layout().leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
    assert!(!rt.pinned_occupied(), "v2 load restores no pinned entries");
}
