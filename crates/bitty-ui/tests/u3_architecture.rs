//! U-3 widget architecture integration coverage (UX-16/UX-17, CTX-0668).
//!
//! Candidate behavior from
//! `bitty-terminal-docs/specifications/ui-runtime-candidate.md` (U-3).
//! Cross-module checks that the unit tests inside `ui_levels` and
//! `widget_mech` do not cover alone: every Level-1 primitive classifies
//! into the five-level contract, mechanism state keys on the canonical
//! `uitree::UiNodeId`, and a tree revision carrying complex widgets still
//! schedules paint frame-on-demand.

#![forbid(unsafe_code)]

use bitty_ui::uitree::UiNodeId;
use bitty_ui::{
    CanvasMech, ScrollMech, TextInputMech, UiLevel, UiNode, UiNodeKind, UiTree, VirtualListMech,
    check_flow, level_of, may_depend_on,
};

/// The mechanism structs key on the canonical identity: this only
/// compiles when `node()` returns the `uitree` id type, never a copy.
fn assert_canonical(id: UiNodeId) -> u64 {
    id.get()
}

fn complex_widgets() -> Vec<UiNodeKind> {
    vec![
        UiNodeKind::ScrollView,
        UiNodeKind::VirtualList,
        UiNodeKind::TextInput {
            value: String::new(),
            placeholder: String::new(),
        },
        UiNodeKind::Canvas,
    ]
}

// ---------------------------------------------------------------------------
// UX-16: five levels classify every primitive (L2-L4 unpopulated)
// ---------------------------------------------------------------------------

/// L0 mechanisms compose downward into nothing: a mechanism must never
/// depend on Lua-emitted, core, domain, or app levels.
#[test]
fn mechanism_level_depends_on_nothing_above() {
    for higher in [
        UiLevel::Primitive,
        UiLevel::Core,
        UiLevel::Domain,
        UiLevel::App,
    ] {
        assert!(
            !may_depend_on(UiLevel::Mechanism, higher),
            "l0 must not depend on {higher}"
        );
        check_flow(UiLevel::Mechanism, higher).expect_err("upward flow must fail");
    }
    assert!(may_depend_on(UiLevel::Mechanism, UiLevel::Mechanism));
}

/// Apps sit at the top: they may compose every lower level.
#[test]
fn app_level_composes_everything_below() {
    for lower in [
        UiLevel::Mechanism,
        UiLevel::Primitive,
        UiLevel::Core,
        UiLevel::Domain,
        UiLevel::App,
    ] {
        assert!(may_depend_on(UiLevel::App, lower), "l4 may compose {lower}");
        check_flow(UiLevel::App, lower).expect("downward flow must pass");
    }
}

/// Complex widgets classify L0 while staying ordinary tree nodes: the
/// retained tree still owns identity and revision for them.
#[test]
fn complex_widgets_are_l0_tree_nodes() {
    for (index, kind) in complex_widgets().into_iter().enumerate() {
        let id = index as u64 + 100;
        assert_eq!(level_of(&kind), UiLevel::Mechanism);
        let node = UiNode::leaf(UiNodeId::new(id), kind);
        assert_eq!(node.id().get(), id);
    }
}

// ---------------------------------------------------------------------------
// UX-17: mechanism state binds tree identity, appearance stays out
// ---------------------------------------------------------------------------

/// Each mechanism binds the canonical node id; appearance (content,
/// color, font) appears nowhere in the mechanism structs.
#[test]
fn mechanisms_bind_canonical_node_ids() {
    let list = VirtualListMech::new(UiNodeId::new(7), 50, 20, 200).expect("valid list");
    let input = TextInputMech::new(UiNodeId::new(8), "hi".to_string()).expect("valid input");
    let scroll = ScrollMech::new(UiNodeId::new(9), 1000, 200).expect("valid scroll");
    let canvas = CanvasMech::new(UiNodeId::new(10), 320, 240).expect("valid canvas");
    assert_eq!(assert_canonical(list.node()), 7);
    assert_eq!(assert_canonical(input.node()), 8);
    assert_eq!(assert_canonical(scroll.node()), 9);
    assert_eq!(assert_canonical(canvas.node()), 10);
}

/// A virtualized window over a scrolled list instantiates exactly the
/// intersecting rows; Lua renders them, Rust counts them.
#[test]
fn virtual_window_instantiates_intersecting_rows() {
    let mut list = VirtualListMech::new(UiNodeId::new(7), 1000, 24, 100).expect("valid list");
    list.set_offset(50);
    assert_eq!(list.visible_range(), 2..7);
    assert_eq!(list.visible_range().len(), 5);
}

/// IME composition commits through the mechanism while the value stays
/// char-bounded under the shared tree text cap.
#[test]
fn ime_preedit_commits_into_bounded_value() {
    let mut input = TextInputMech::new(UiNodeId::new(8), String::new()).expect("valid input");
    input.set_preedit("hello".to_string()).expect("preedit");
    input.commit_preedit().expect("commit");
    assert_eq!(input.value(), "hello");
    assert_eq!(input.cursor(), 5);
}

/// Scroll settles clamp at content ends; canvas queues close at budget.
#[test]
fn scroll_and_canvas_budgets_hold_end_to_end() {
    let mut scroll = ScrollMech::new(UiNodeId::new(9), 500, 200).expect("valid scroll");
    scroll.scroll_by(-1_000_000);
    assert!(scroll.is_at_top());
    scroll.scroll_by(1_000_000);
    assert!(scroll.is_at_bottom());
    assert_eq!(scroll.offset(), 300);

    let mut canvas = CanvasMech::new(UiNodeId::new(10), 640, 480).expect("valid canvas");
    canvas.push_commands(100).expect("queue");
    assert_eq!(canvas.commands(), 100);
    canvas.clear();
    assert_eq!(canvas.budget_remaining(), bitty_ui::MAX_CANVAS_COMMANDS);
}

// ---------------------------------------------------------------------------
// U-1 x U-3: revisions carrying complex widgets stay frame-on-demand
// ---------------------------------------------------------------------------

/// A tree with L0 widget leaves still bumps its revision only on
/// structural change: mechanisms never schedule paint by themselves.
#[test]
fn widget_tree_revision_stays_frame_on_demand() {
    let root = || {
        UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Box,
            vec![
                UiNode::leaf(UiNodeId::new(2), UiNodeKind::VirtualList),
                UiNode::leaf(
                    UiNodeId::new(3),
                    UiNodeKind::TextInput {
                        value: "v".to_string(),
                        placeholder: String::new(),
                    },
                ),
                UiNode::leaf(UiNodeId::new(4), UiNodeKind::Canvas),
            ],
        )
    };
    let mut tree = UiTree::new(root()).expect("valid tree");
    let same = tree.apply_revision(root()).expect("valid revision");
    assert!(!same.changed);
    assert_eq!(tree.revision(), 0);
    let changed_root = UiNode::new(
        UiNodeId::new(1),
        UiNodeKind::Box,
        vec![
            UiNode::leaf(UiNodeId::new(2), UiNodeKind::VirtualList),
            UiNode::leaf(
                UiNodeId::new(3),
                UiNodeKind::TextInput {
                    value: "v2".to_string(),
                    placeholder: String::new(),
                },
            ),
            UiNode::leaf(UiNodeId::new(4), UiNodeKind::Canvas),
        ],
    );
    let changed = tree.apply_revision(changed_root).expect("valid revision");
    assert!(changed.changed);
    assert_eq!(tree.revision(), 1);
    assert_eq!(changed.changes.len(), 1);
    assert_eq!(changed.changes[0].id.get(), 3);
}
