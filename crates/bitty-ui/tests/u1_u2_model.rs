//! U-1/U-2 model family integration coverage (UX-13/UX-14/UX-15, CTX-0662).
//!
//! Candidate behavior from
//! `bitty-terminal-docs/specifications/ui-runtime-candidate.md` (U-1, U-2).
//! Cross-module checks that the unit tests inside `uitree` and
//! `workspace_scene` do not cover alone: retained revisions driving scene
//! attachments, identity inequality across the four layers, and the
//! frame-on-demand rule end to end.

#![forbid(unsafe_code)]

use bitty_ui::a11y::A11yRole;
use bitty_ui::{
    ActivityId, ActivityStack, LayerEntry, LayoutNode, PanelId, Rect, SceneLayer, SplitAxis,
    TerminalBinding, UiNode, UiNodeId, UiNodeKind, UiTree, View, ViewId, WorkspaceScene,
    WorkspaceSceneId, a11y_role_of,
};

fn leaf_view(id: u64) -> LayoutNode {
    LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
}

fn scene_two_leaf() -> WorkspaceScene {
    WorkspaceScene::new(
        WorkspaceSceneId::new(1),
        LayoutNode::split(SplitAxis::Vertical, 0.5, leaf_view(11), leaf_view(12)),
    )
}

fn text(id: u64, body: &str) -> UiNode {
    UiNode::leaf(UiNodeId::new(id), UiNodeKind::Text(body.to_string()))
}

// ---------------------------------------------------------------------------
// Frame-on-demand across revisions (U-1)
// ---------------------------------------------------------------------------

/// Only structural change schedules paint: identical resubmissions are
/// silent, and each change reports exactly its invalidation set.
#[test]
fn frame_on_demand_drives_paint_scheduling() {
    let root = |label: &str| {
        UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Box,
            vec![
                text(2, label),
                UiNode::leaf(UiNodeId::new(3), UiNodeKind::Terminal),
            ],
        )
    };
    let mut tree = UiTree::new(root("v1")).expect("valid tree");
    let mut paints = 0u32;
    for label in ["v1", "v1", "v2", "v2"] {
        let report = tree.apply_revision(root(label)).expect("valid revision");
        if report.changed {
            paints += 1;
        }
    }
    assert_eq!(
        paints, 1,
        "exactly one structural change schedules one paint"
    );
    assert_eq!(tree.revision(), 1);
}

/// A `UiTree` revision never mutates terminal state: the `Terminal`
/// primitive carries no handle, and the scene attachment still needs the
/// runtime-owned binding supplied separately.
#[test]
fn terminal_primitive_carries_no_handle() {
    let node = UiNode::leaf(UiNodeId::new(1), UiNodeKind::Terminal);
    assert!(node.children().is_empty());
    assert_eq!(a11y_role_of(node.kind()), A11yRole::Text);
    // The scene join needs an explicit binding the tree cannot supply.
    let mut scene = scene_two_leaf();
    scene
        .bind(PanelId::new(1), ViewId::new(11), TerminalBinding::new(900))
        .expect("bind with runtime-supplied binding");
    assert_eq!(
        scene
            .attachment_of(PanelId::new(1))
            .expect("attached")
            .terminal,
        TerminalBinding::new(900)
    );
}

// ---------------------------------------------------------------------------
// Four-layer identity (U-2)
// ---------------------------------------------------------------------------

/// `PanelId != ViewId != TerminalId` holds across a panel move: the
/// attachment is re-parented while every identity keeps its own type and
/// value domain.
#[test]
fn four_layer_move_keeps_identity_domains() {
    let mut scene = scene_two_leaf();
    let panel = PanelId::new(7);
    let terminal = TerminalBinding::new(77);
    scene.bind(panel, ViewId::new(11), terminal).expect("bind");
    scene.move_panel(panel, ViewId::new(12)).expect("move");
    let attachment = scene.attachment_of(panel).expect("attached");
    // Same panel, new view, same terminal binding: values move in their
    // own lanes, never converted into each other.
    assert_eq!(attachment.panel.get(), 7);
    assert_eq!(attachment.view, ViewId::new(12));
    assert_eq!(attachment.terminal.get(), 77);
    // The old view is freed for another panel.
    scene
        .bind(PanelId::new(8), ViewId::new(11), TerminalBinding::new(78))
        .expect("old view is reusable");
}

/// Layers compose: tiled leaves, a floating placement, and the z-order
/// contract agree on where each view lives.
#[test]
fn layers_compose_with_z_order() {
    let mut scene = scene_two_leaf();
    scene
        .place(
            SceneLayer::Floating,
            LayerEntry {
                view: ViewId::new(50),
                bounds: Rect::new(0, 0, 40, 12),
            },
        )
        .expect("place");
    assert_eq!(scene.layer_of(ViewId::new(11)), Some(SceneLayer::Tiled));
    assert_eq!(scene.layer_of(ViewId::new(50)), Some(SceneLayer::Floating));
    let order = scene.z_order();
    let ranks: Vec<u8> = order.iter().map(|(layer, _)| layer.z_rank()).collect();
    let mut sorted = ranks.clone();
    sorted.sort();
    assert_eq!(ranks, sorted, "z-order must be rank-sorted: {order:?}");
}

/// Activity navigation composes with panel attachment: the stack belongs
/// to the attached panel and survives its move.
#[test]
fn activity_stack_survives_panel_move() {
    let mut scene = scene_two_leaf();
    let panel = PanelId::new(4);
    scene
        .bind(panel, ViewId::new(11), TerminalBinding::new(44))
        .expect("bind");
    let mut stack = ActivityStack::new(panel, ActivityId::new(200));
    stack.push(ActivityId::new(201)).expect("push");
    scene.move_panel(panel, ViewId::new(12)).expect("move");
    assert_eq!(stack.panel(), panel);
    assert_eq!(stack.current(), ActivityId::new(201));
    assert_eq!(scene.view_of(panel), Some(ViewId::new(12)));
}

/// An overlay tree revision invalidates exactly the overlay subtree while
/// the scene attachment underneath is untouched.
#[test]
fn overlay_revision_scoped_to_overlay_subtree() {
    let root = |items: &[&str]| {
        UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Box,
            vec![
                UiNode::leaf(UiNodeId::new(2), UiNodeKind::Terminal),
                UiNode::new(
                    UiNodeId::new(10),
                    UiNodeKind::Overlay,
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| text(20 + index as u64, item))
                        .collect(),
                ),
            ],
        )
    };
    let mut tree = UiTree::new(root(&["a"])).expect("valid tree");
    let report = tree
        .apply_revision(root(&["a", "b"]))
        .expect("valid revision");
    assert!(report.changed);
    let changed: Vec<u64> = report
        .changes
        .iter()
        .map(|change| change.id.get())
        .collect();
    assert!(
        changed.contains(&10),
        "overlay parent resequenced: {changed:?}"
    );
    assert!(changed.contains(&21), "added item invalidated: {changed:?}");
    assert!(
        !changed.contains(&2),
        "terminal leaf untouched: {changed:?}"
    );
}
