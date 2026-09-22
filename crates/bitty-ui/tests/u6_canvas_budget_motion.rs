//! U-6 family integration coverage (UX-23/UX-24/UX-25, CTX-0671).
//!
//! Candidate behavior: bounded `Canvas` display lists replayed by the
//! compositor at refresh, UI/GPU budget tiers with refuse-vs-degrade
//! admission, and the Core-owned motion hierarchy. Cross-module checks
//! the unit tests inside `canvas`, `budget`, and `motion` do not cover
//! alone: canvas revisions gating compositor replays, canvas demand
//! feeding budget admission, and motion scopes driving panel values.

#![forbid(unsafe_code)]

use bitty_ui::{
    Admission, BudgetTier, CanvasCommand, CanvasDisplayList, CanvasLayer, MotionConfig,
    MotionScope, MotionValue, ResourceUsage, UiNode, UiNodeId, UiNodeKind, UiTree, tree_nodes,
};

fn canvas_node(id: u64) -> UiNode {
    UiNode::leaf(UiNodeId::new(id), UiNodeKind::Canvas)
}

fn sample_list(node: u64) -> CanvasDisplayList {
    let mut list = CanvasDisplayList::new(UiNodeId::new(node));
    list.push(CanvasCommand::FillRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    })
    .expect("valid rect");
    list.push(CanvasCommand::Text {
        x: 1,
        y: 1,
        body: "panel".to_string(),
    })
    .expect("valid text");
    list
}

fn tree_with_canvas() -> UiNode {
    UiNode::new(
        UiNodeId::new(1),
        UiNodeKind::Box,
        vec![
            UiNode::leaf(UiNodeId::new(2), UiNodeKind::Terminal),
            canvas_node(3),
        ],
    )
}

// ---------------------------------------------------------------------------
// Canvas revision gates compositor replay (UX-25)
// ---------------------------------------------------------------------------

/// The compositor replays retained lists only while the layer revision
/// moves: unchanged resubmissions schedule nothing.
#[test]
fn canvas_revision_gates_compositor_replay() {
    let mut layer = CanvasLayer::new();
    let mut replays = 0u32;
    let mut seen = layer.revision();
    let mut submit = |layer: &mut CanvasLayer, list: CanvasDisplayList, seen: &mut u64| {
        let report = layer.submit(list).expect("valid list");
        if layer.revision() != *seen {
            replays += 1;
            *seen = layer.revision();
        }
        report
    };
    let first = submit(&mut layer, sample_list(3), &mut seen);
    assert!(first.changed);
    let second = submit(&mut layer, sample_list(3), &mut seen);
    assert!(!second.changed);
    let mut changed = sample_list(3);
    changed
        .push(CanvasCommand::Line {
            x0: 0,
            y0: 0,
            x1: 7,
            y1: 7,
        })
        .expect("valid line");
    let third = submit(&mut layer, changed, &mut seen);
    assert!(third.changed);
    assert_eq!(replays, 2, "exactly two revisions replay");
}

/// Canvas lists address retained-tree nodes: the list node matches a
/// `Canvas` leaf in the current tree revision.
#[test]
fn canvas_lists_address_retained_tree_nodes() {
    let tree = UiTree::new(tree_with_canvas()).expect("valid tree");
    let mut layer = CanvasLayer::new();
    layer.submit(sample_list(3)).expect("valid list");
    let list = layer.get(UiNodeId::new(3)).expect("retained list");
    assert_eq!(list.node(), UiNodeId::new(3));
    assert_eq!(list.len(), 2);
    // The tree owns the node identity; the layer only replays for it.
    assert_eq!(tree.root().count_nodes(), 3);
}

// ---------------------------------------------------------------------------
// Canvas demand feeds budget admission (UX-24 + UX-25)
// ---------------------------------------------------------------------------

/// Per-submission accounting sums tree nodes and canvas draws into one
/// admission input; a small scene is accepted outright.
#[test]
fn small_scene_accounting_is_accepted() {
    let tree = tree_with_canvas();
    let list = sample_list(3);
    let usage = ResourceUsage::new(tree_nodes(&tree), 0, 0, 0)
        .saturating_add(ResourceUsage::draws(list.len()));
    assert_eq!(usage, ResourceUsage::new(3, 0, 0, 2));
    let budget = BudgetTier::Standard.caps();
    budget.check(usage).expect("small scene fits");
    assert_eq!(budget.admit(usage), Admission::Accepted);
}

/// A blur-heavy canvas scene degrades (keeps a complete surface with
/// reduced blur) while a node-heavy tree refuses fail-closed.
#[test]
fn blur_degrades_but_nodes_refuse() {
    let budget = BudgetTier::Essential.caps();
    // Essential allows no blur: blur-only overuse degrades to zero blur.
    let blurry = ResourceUsage::new(4, 0, 4096, 8);
    let admitted = budget.admit(blurry);
    assert_eq!(
        admitted,
        Admission::Degraded(ResourceUsage::new(4, 0, 0, 8))
    );
    assert!(admitted.is_admitted());
    // Node overuse refuses even when everything else fits.
    let heavy = ResourceUsage::new(budget.max_nodes + 1, 0, 0, 0);
    match budget.admit(heavy) {
        Admission::Refused(_) => {}
        other => panic!("node overuse must refuse, got {other:?}"),
    }
    assert!(!budget.admit(heavy).is_admitted());
}

// ---------------------------------------------------------------------------
// Motion scopes drive panel values (UX-23)
// ---------------------------------------------------------------------------

/// Panel open/move/close values resolve through the hierarchy: Lua
/// retargets, Rust steps, and reduced motion settles instantly with no
/// wakeups.
#[test]
fn hierarchy_drives_panel_values_to_settled() {
    let config = MotionConfig::default().with_close(bitty_ui::MotionSpec::INSTANT);
    let open_spec = config.resolve(MotionScope::Open);
    let close_spec = config.resolve(MotionScope::Close);
    assert_eq!(open_spec, bitty_ui::MotionSpec::default());
    assert!(close_spec.is_instant());

    let mut open_value = MotionValue::new(0.0, open_spec);
    open_value.set_target(1.0);
    assert!(open_value.needs_wakeup());
    assert_eq!(open_value.step(1.0), 1.0);
    assert!(!open_value.needs_wakeup());

    let mut close_value = MotionValue::new(1.0, close_spec);
    close_value.set_target(0.0);
    assert!(close_value.is_settled());
    assert!(!close_value.needs_wakeup());
}

/// Reduced motion forces every hierarchy scope instant end to end.
#[test]
fn reduced_motion_settles_every_scope_without_wakeups() {
    let config = MotionConfig::default().with_reduced_motion(true);
    for scope in [
        MotionScope::Default,
        MotionScope::Panel,
        MotionScope::Open,
        MotionScope::Move,
        MotionScope::Close,
    ] {
        let mut value = MotionValue::new(0.0, config.resolve(scope));
        value.set_target(1.0);
        assert!(value.is_settled(), "{scope} must settle instantly");
        assert!(!value.needs_wakeup(), "{scope} must need no wakeup");
    }
}
