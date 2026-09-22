//! Accessibility baseline integration tests (UX-42, CTX-0614).
//!
//! Pins the candidate baseline implemented in `bitty_ui::a11y` against the
//! candidate record
//! (`bitty-terminal-docs/specifications/accessibility-baseline-candidate.md`,
//! Draft): role mapping per scene kind with fail-closed unmapped kinds,
//! terminal text runs plus cursor with the fidelity boundary, and chrome
//! read-only exposure excluded from the tab order. Headless, deterministic,
//! bounded: no window, no GPU, no PTY, no filesystem.

#![forbid(unsafe_code)]

use bitty_term_state::{State, TerminalAction};
use bitty_ui::a11y::{
    A11yError, A11yNodeKind, A11yRole, A11yTreeBuilder, ChromeKind, ChromeNode, FIDELITY_BOUNDARY,
    InteractiveNode, MAX_A11Y_TREE_NODES, SceneKind, build_a11y_tree, chrome_tab_order,
    expose_terminal, interactive_role, role_of, terminal_text_runs, validate_scene,
};
use bitty_vt::{ControlChar, GraphemeCell};

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

fn feed_line(state: &mut State, text: &str) {
    prints(state, text);
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
}

// ---------------------------------------------------------------------------
// Role mapping: every mapped kind resolves, unmapped kinds fail closed
// ---------------------------------------------------------------------------

#[test]
fn every_mapped_scene_kind_has_a_role() {
    let expected: &[(SceneKind, A11yRole)] = &[
        (SceneKind::Text, A11yRole::Text),
        (SceneKind::Row, A11yRole::Group),
        (SceneKind::Column, A11yRole::Group),
        (SceneKind::Block, A11yRole::Group),
        (SceneKind::Image, A11yRole::Image),
        (SceneKind::CodeBlock, A11yRole::Code),
        (SceneKind::Table, A11yRole::Table),
        (SceneKind::List, A11yRole::List),
        (SceneKind::Rule, A11yRole::Separator),
    ];
    assert_eq!(SceneKind::mapped().len(), expected.len());
    for (kind, role) in expected {
        assert_eq!(role_of(*kind), Ok(*role), "kind {kind:?} must map");
    }
    // The mapped set is exactly the full set minus the fallback.
    assert_eq!(SceneKind::all().len(), SceneKind::mapped().len() + 1);
}

#[test]
fn unknown_scene_kind_fails_closed() {
    assert_eq!(
        role_of(SceneKind::Unknown),
        Err(A11yError::UnmappedSceneKind)
    );
}

#[test]
fn validate_scene_accepts_mapped_and_rejects_unmapped() {
    assert!(validate_scene(SceneKind::mapped()).is_ok());
    assert!(validate_scene(&[]).is_ok());
    assert_eq!(
        validate_scene(&[SceneKind::Text, SceneKind::Unknown, SceneKind::List]),
        Err(A11yError::UnmappedSceneKind)
    );
}

#[test]
fn interactive_roles_resolve_from_declared_purpose() {
    assert_eq!(interactive_role("button"), Ok(A11yRole::Button));
    assert_eq!(interactive_role("input"), Ok(A11yRole::Input));
}

#[test]
fn undeclared_interactive_purpose_fails_closed() {
    assert_eq!(
        interactive_role("switch"),
        Err(A11yError::UnmappedSceneKind)
    );
    assert_eq!(interactive_role(""), Err(A11yError::UnmappedSceneKind));
}

#[test]
fn interactive_node_exposes_name_state_and_activation() {
    let button = InteractiveNode::new("button", "Send", true).unwrap();
    assert_eq!(button.role(), A11yRole::Button);
    assert_eq!(button.name(), "Send");
    assert!(button.is_enabled());
    assert_eq!(button.activation_label(), Some("activate"));

    let disabled = InteractiveNode::new("input", "Search", false).unwrap();
    assert_eq!(disabled.role(), A11yRole::Input);
    assert!(!disabled.is_enabled());
    // Disabled controls expose state with no activation.
    assert_eq!(disabled.activation_label(), None);
}

#[test]
fn interactive_node_rejects_undeclared_purpose_and_overlong_name() {
    assert_eq!(
        InteractiveNode::new("menu", "Open", true),
        Err(A11yError::UnmappedSceneKind)
    );
    let long = "n".repeat(257);
    assert!(matches!(
        InteractiveNode::new("button", &long, true),
        Err(A11yError::NameTooLong { .. })
    ));
}

// ---------------------------------------------------------------------------
// Terminal exposure: text runs plus cursor, read-only, stated boundary
// ---------------------------------------------------------------------------

#[test]
fn terminal_runs_expose_readable_text_and_cursor() {
    let mut state = State::new();
    feed_line(&mut state, "hello");
    prints(&mut state, "world");
    let snapshot = state.snapshot();

    let runs = terminal_text_runs(&snapshot);
    assert!(runs.len() >= 2);
    assert_eq!(runs[0].row, 0);
    assert_eq!(runs[0].text, "hello");
    assert_eq!(runs[1].row, 1);
    // LF preserves the column, so the second line starts at column 5.
    assert_eq!(runs[1].text.trim(), "world");

    let exposure = expose_terminal(&snapshot);
    assert_eq!(exposure.cursor, snapshot.cursor.position);
    assert_eq!(exposure.cursor_visible, snapshot.cursor.visible);
    // Cursor tracks the live position: one fed line plus five more cells.
    assert_eq!(exposure.cursor.row, 1);
    assert_eq!(exposure.cursor.col, 10);
}

#[test]
fn terminal_blank_rows_produce_no_runs() {
    let state = State::new();
    let snapshot = state.snapshot();
    assert!(terminal_text_runs(&snapshot).is_empty());
}

#[test]
fn terminal_exposure_is_named_by_title_and_read_only() {
    let mut state = State::new();
    prints(&mut state, "data");
    state.apply(&TerminalAction::OscTitle {
        text: "editor".into(),
    });
    let before = state.snapshot();
    let exposure = expose_terminal(&before);
    assert_eq!(exposure.title, "editor");
    // The projection derives from the snapshot and mutates nothing.
    assert_eq!(state.snapshot(), before);
    assert_eq!(exposure.runs.len(), 1);
    assert_eq!(exposure.runs[0].text, "data");
}

#[test]
fn fidelity_boundary_is_stated_not_implied() {
    for token in ["cursor", "SGR", "graphics", "images"] {
        assert!(
            FIDELITY_BOUNDARY.contains(token),
            "boundary must name {token}"
        );
    }
}

// ---------------------------------------------------------------------------
// Chrome exposure: read-only state, excluded from the tab order
// ---------------------------------------------------------------------------

#[test]
fn chrome_is_excluded_from_tab_order_while_state_is_exposed() {
    let bar = ChromeNode::new(ChromeKind::Bar, "status bar", Some("main.rs")).unwrap();
    let tabs = ChromeNode::new(ChromeKind::TabStrip, "tabs", Some("editor")).unwrap();
    let note = ChromeNode::new(ChromeKind::Notification, "notices", None).unwrap();
    let nodes = [bar, tabs, note];

    for node in &nodes {
        assert!(!node.is_tab_stop(), "{node:?} must never be a focus target");
    }
    assert!(chrome_tab_order(&nodes).is_empty());
    assert!(chrome_tab_order(&[]).is_empty());

    // State stays exposed read-only: names and active items are readable.
    assert_eq!(nodes[0].accessible_name(), "status bar");
    assert_eq!(nodes[0].active_item(), Some("main.rs"));
    assert_eq!(nodes[1].kind(), ChromeKind::TabStrip);
    assert_eq!(nodes[1].active_item(), Some("editor"));
    assert_eq!(nodes[2].accessible_name(), "notices");
    assert_eq!(nodes[2].active_item(), None);
}

#[test]
fn chrome_rejects_overlong_names() {
    let long = "x".repeat(300);
    assert!(matches!(
        ChromeNode::new(ChromeKind::Rail, &long, None),
        Err(A11yError::NameTooLong { .. })
    ));
    assert!(matches!(
        ChromeNode::new(ChromeKind::Rail, "rail", Some(&long)),
        Err(A11yError::NameTooLong { .. })
    ));
}

/// Drift coupling: every `bitty-rich` `SceneNode` variant must map to a
/// `SceneKind` with a resolved role (or fail closed for `Unknown`).
/// Deliberately exhaustive — gains a compile error if `bitty-rich` adds a
/// variant, so the mirror in `bitty_ui::a11y` cannot drift silently.
#[test]
fn scene_kind_mirror_tracks_bitty_rich() {
    use bitty_rich::scene::{Border, CodeBlockModel, ListModel, SceneNode, StyledSpan, TableModel};

    let span = StyledSpan {
        text: String::from("t"),
        bold: false,
        italic: false,
    };
    let leaf = SceneNode::Text(span.clone());
    let cases: Vec<(SceneNode, SceneKind)> = vec![
        (leaf.clone(), SceneKind::Text),
        (SceneNode::Row(vec![leaf.clone()]), SceneKind::Row),
        (SceneNode::Column(vec![leaf.clone()]), SceneKind::Column),
        (
            SceneNode::Block {
                border: Some(Border {
                    width: 1,
                    color: String::from("#fff"),
                }),
                child: Box::new(leaf.clone()),
            },
            SceneKind::Block,
        ),
        (
            SceneNode::Image(bitty_rich::image::PlacementId(1)),
            SceneKind::Image,
        ),
        (
            SceneNode::CodeBlock(CodeBlockModel {
                lang: None,
                content: String::from("x"),
            }),
            SceneKind::CodeBlock,
        ),
        (
            SceneNode::Table(TableModel {
                rows: vec![vec![String::from("c")]],
            }),
            SceneKind::Table,
        ),
        (
            SceneNode::List(ListModel {
                items: vec![String::from("i")],
                ordered: false,
            }),
            SceneKind::List,
        ),
        (SceneNode::Rule, SceneKind::Rule),
    ];
    for (node, kind) in &cases {
        let mapped = match node {
            SceneNode::Text(_) => SceneKind::Text,
            SceneNode::Row(_) => SceneKind::Row,
            SceneNode::Column(_) => SceneKind::Column,
            SceneNode::Block { .. } => SceneKind::Block,
            SceneNode::Image(_) => SceneKind::Image,
            SceneNode::CodeBlock(_) => SceneKind::CodeBlock,
            SceneNode::Table(_) => SceneKind::Table,
            SceneNode::List(_) => SceneKind::List,
            SceneNode::Rule => SceneKind::Rule,
            SceneNode::Unknown(_) => SceneKind::Unknown,
        };
        assert_eq!(mapped, *kind);
        assert!(role_of(*kind).is_ok(), "{kind:?} must resolve a role");
    }
    assert!(matches!(
        role_of(SceneKind::Unknown),
        Err(A11yError::UnmappedSceneKind)
    ));
    assert!(matches!(
        validate_scene(&[SceneKind::Unknown]),
        Err(A11yError::UnmappedSceneKind)
    ));
}

// ---------------------------------------------------------------------------
// Accessibility tree: stable shape, fail-closed bounds, read-only build
// ---------------------------------------------------------------------------

fn tree_snapshot() -> bitty_term_state::Snapshot {
    let mut state = State::new();
    feed_line(&mut state, "hello");
    prints(&mut state, "world");
    state.apply(&TerminalAction::OscTitle {
        text: "editor".into(),
    });
    state.snapshot()
}

#[test]
fn tree_shape_is_root_terminal_chrome_scene_in_order() {
    let snapshot = tree_snapshot();
    let chrome = [
        ChromeNode::new(ChromeKind::Bar, "status bar", Some("main.rs")).unwrap(),
        ChromeNode::new(ChromeKind::Notification, "notices", None).unwrap(),
    ];
    let scene = [SceneKind::Text, SceneKind::Table];
    let before = snapshot.clone();
    let tree = build_a11y_tree(&snapshot, &chrome, &scene).unwrap();
    // The build borrows everything and mutates nothing.
    assert_eq!(snapshot, before);

    // 1 root + 1 terminal + 2 rows + 2 chrome + 2 scene.
    assert_eq!(tree.len(), 8);
    assert!(!tree.is_empty());

    let root = tree.get(tree.root()).unwrap();
    assert_eq!(root.kind(), A11yNodeKind::Root);
    assert_eq!(root.name(), "editor");
    assert_eq!(root.role(), Some(A11yRole::Group));
    assert_eq!(root.children().len(), 5);

    let order: Vec<(A11yNodeKind, &str)> = tree
        .preorder()
        .map(|node| (node.kind(), node.name()))
        .collect();
    assert_eq!(order[0], (A11yNodeKind::Root, "editor"));
    assert_eq!(order[1], (A11yNodeKind::Terminal, "editor"));
    assert_eq!(order[2].0, A11yNodeKind::TerminalRow);
    assert_eq!(order[2].1, "hello");
    assert_eq!(order[3].0, A11yNodeKind::TerminalRow);
    assert!(order[3].1.contains("world"));
    assert_eq!(
        order[4],
        (A11yNodeKind::Chrome(ChromeKind::Bar), "status bar")
    );
    assert_eq!(
        order[5],
        (A11yNodeKind::Chrome(ChromeKind::Notification), "notices")
    );
    assert_eq!(order[6], (A11yNodeKind::Scene(SceneKind::Text), "text"));
    // Scene roles attach last in caller order; preorder ends with the table.
    let names: Vec<&str> = tree.preorder().map(|node| node.name()).collect();
    assert_eq!(names.last(), Some(&"table"));

    // Terminal leaf states the live cursor; rows read as text.
    let terminal = tree
        .preorder()
        .find(|node| node.kind() == A11yNodeKind::Terminal)
        .unwrap();
    let visibility = if snapshot.cursor.visible {
        "visible"
    } else {
        "hidden"
    };
    assert_eq!(
        terminal.state(),
        Some(format!("cursor 1,10 {visibility}").as_str())
    );
    assert_eq!(terminal.role(), Some(A11yRole::Text));

    // Chrome keeps its identity and stays out of the role vocabulary.
    let bar = tree.preorder().nth(4).unwrap();
    assert_eq!(bar.role(), None);
    assert_eq!(bar.state(), Some("main.rs"));
}

#[test]
fn tree_fails_closed_on_unmapped_scene_kind() {
    let snapshot = tree_snapshot();
    assert_eq!(
        build_a11y_tree(&snapshot, &[], &[SceneKind::Text, SceneKind::Unknown]),
        Err(A11yError::UnmappedSceneKind)
    );
}

#[test]
fn tree_fails_closed_on_overlong_title_and_chrome_label() {
    let snapshot = tree_snapshot();
    let long = "n".repeat(300);
    let chrome = [ChromeNode::new(ChromeKind::Rail, "rail", None).unwrap()];
    // Overlong titles fail the root; overlong chrome labels fail at
    // construction (pinned below), so the tree build itself stays total.
    let builder = A11yTreeBuilder::new(&long);
    assert!(matches!(builder, Err(A11yError::NameTooLong { .. })));
    let bad_state = ChromeNode::new(ChromeKind::Rail, "rail", Some(&long));
    assert!(matches!(bad_state, Err(A11yError::NameTooLong { .. })));
    let tree = build_a11y_tree(&snapshot, &chrome, &[]);
    assert!(tree.is_ok());
}

#[test]
fn tree_builder_enforces_cap_foreign_handles_and_row_content_rule() {
    let mut builder = A11yTreeBuilder::new("window").unwrap();
    assert_eq!(builder.len(), 1);
    assert!(!builder.is_empty());
    let root = builder.root();
    assert_eq!(root.as_u32(), 0);

    // Row content bypasses the name cap; plain names do not.
    let long = "x".repeat(300);
    assert!(builder.push_row_text(root, &long).is_ok());
    assert!(matches!(
        builder.push(root, A11yNodeKind::TerminalRow, &long, None),
        Err(A11yError::NameTooLong { .. })
    ));

    // Interactive controls attach with their declared role (the rejected
    // overlong push above allocated nothing, so this is id 2).
    let button = builder
        .push(
            root,
            A11yNodeKind::Interactive(A11yRole::Button),
            "Send",
            Some("enabled"),
        )
        .unwrap();
    assert_eq!(button.as_usize(), 2);

    // A handle from a taller builder is out of range here.
    let mut other = A11yTreeBuilder::new("other").unwrap();
    let _ = other
        .push(other.root(), A11yNodeKind::Terminal, "t", None)
        .unwrap();
    let _ = other
        .push(other.root(), A11yNodeKind::Terminal, "t", None)
        .unwrap();
    let foreign = other
        .push(other.root(), A11yNodeKind::Terminal, "t", None)
        .unwrap();
    assert!(other.len() > builder.len());
    assert!(foreign.as_usize() >= builder.len());
    assert_eq!(
        builder.push(foreign, A11yNodeKind::Terminal, "t", None),
        Err(A11yError::UnknownParent)
    );

    // The node cap fails closed with an exact count.
    let mut full = A11yTreeBuilder::new("full").unwrap();
    let full_root = full.root();
    for _ in 1..MAX_A11Y_TREE_NODES {
        full.push(full_root, A11yNodeKind::TerminalRow, "r", None)
            .unwrap();
    }
    assert_eq!(
        full.push(full_root, A11yNodeKind::TerminalRow, "r", None),
        Err(A11yError::TooManyNodes {
            count: MAX_A11Y_TREE_NODES + 1,
            cap: MAX_A11Y_TREE_NODES,
        })
    );
    assert_eq!(full.finish().len(), MAX_A11Y_TREE_NODES);
}
