//! Accessibility seam parity tests (CTX-1176, closes #1996).
//!
//! Pins Core retirement onto the `bitty-a11y` `SnapshotHost` seam against the
//! accepted contract: role vocabulary equivalence, terminal text-run projection
//! with the stated fidelity boundary, bounded inputs fail-closed (256-name,
//! 4096-node, 256-announcement, 8-pending caps), stale-generation rejection
//! with nothing swapped, and no retained handles on the Core side. Headless,
//! deterministic, bounded: no window, no GPU, no PTY, no filesystem, no clock.

#![forbid(unsafe_code)]

use bitty_a11y::{
    A11yError, Adapter, AnnouncementKind, AnnouncementQueue, ChromeKind, Generation,
    HeadlessBackend, InteractivePurpose, MAX_ACCESSIBLE_NAME_LEN, MAX_ANNOUNCEMENT_TEXT_LEN,
    MAX_PENDING_ANNOUNCEMENTS, MAX_SNAPSHOT_NODES, Role, SceneKind, needs_refresh, role_of,
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::a11y_host::{
    A11ySettings, FIDELITY_BOUNDARY, UiSnapshotHost, a11y_role_of, terminal_runs,
};
use bitty_ui::{UiNode, UiNodeId, UiNodeKind, UiTree};
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

fn tree_snapshot() -> bitty_term_state::Snapshot {
    let mut state = State::new();
    feed_line(&mut state, "hello");
    prints(&mut state, "world");
    state.apply(&TerminalAction::OscTitle {
        text: "editor".into(),
    });
    state.snapshot()
}

// ---------------------------------------------------------------------------
// Seam constants: pinned ceilings owned by the extension
// ---------------------------------------------------------------------------

#[test]
fn seamceilings_match_accepted_bounds() {
    assert_eq!(MAX_ACCESSIBLE_NAME_LEN, 256);
    assert_eq!(MAX_SNAPSHOT_NODES, 4096);
    assert_eq!(MAX_ANNOUNCEMENT_TEXT_LEN, 256);
    assert_eq!(MAX_PENDING_ANNOUNCEMENTS, 8);
}

// ---------------------------------------------------------------------------
// Role vocabulary parity: extension Role spells what Core A11yRole spelled
// ---------------------------------------------------------------------------

#[test]
fn role_spellings_match_retired_vocabulary() {
    let expected: &[(Role, &str)] = &[
        (Role::Text, "text"),
        (Role::Group, "group"),
        (Role::Code, "code"),
        (Role::Table, "table"),
        (Role::List, "list"),
        (Role::Button, "button"),
        (Role::Input, "input"),
        (Role::Separator, "separator"),
        (Role::Image, "image"),
    ];
    for (role, spelling) in expected {
        assert_eq!(role.as_str(), *spelling);
        assert_eq!(role.to_string(), *spelling);
    }
}

#[test]
fn scene_kind_mapping_matches_retired_table() {
    let expected: &[(SceneKind, Role)] = &[
        (SceneKind::Text, Role::Text),
        (SceneKind::Row, Role::Group),
        (SceneKind::Column, Role::Group),
        (SceneKind::Block, Role::Group),
        (SceneKind::Image, Role::Image),
        (SceneKind::CodeBlock, Role::Code),
        (SceneKind::Table, Role::Table),
        (SceneKind::List, Role::List),
        (SceneKind::Rule, Role::Separator),
    ];
    for (kind, role) in expected {
        assert_eq!(role_of(*kind), Ok(*role), "kind {kind:?} must map");
    }
    assert_eq!(
        role_of(SceneKind::Unknown),
        Err(A11yError::UnmappedSceneKind)
    );
}

#[test]
fn ui_primitive_mapping_matches_retired_values() {
    assert_eq!(a11y_role_of(&UiNodeKind::Terminal), Role::Text);
    assert_eq!(
        a11y_role_of(&UiNodeKind::Text(String::from("t"))),
        Role::Text
    );
    assert_eq!(a11y_role_of(&UiNodeKind::Box), Role::Group);
    assert_eq!(a11y_role_of(&UiNodeKind::Image), Role::Image);
    assert_eq!(a11y_role_of(&UiNodeKind::VirtualList), Role::List);
    assert_eq!(
        a11y_role_of(&UiNodeKind::TextInput {
            value: String::new(),
            placeholder: String::new(),
        }),
        Role::Input
    );
}

#[test]
fn interactive_purposes_resolve_from_declared_purpose() {
    assert_eq!(
        InteractivePurpose::parse("button"),
        Ok(InteractivePurpose::Button)
    );
    assert_eq!(
        InteractivePurpose::parse("input"),
        Ok(InteractivePurpose::Input)
    );
    assert_eq!(
        InteractivePurpose::parse("switch"),
        Err(A11yError::UnmappedSceneKind)
    );
    assert_eq!(
        InteractivePurpose::parse(""),
        Err(A11yError::UnmappedSceneKind)
    );
    assert_eq!(InteractivePurpose::Button.role(), Role::Button);
    assert_eq!(InteractivePurpose::Input.role(), Role::Input);
}

/// Drift coupling: every `bitty-rich` `SceneNode` variant must map to an
/// extension `SceneKind` with a resolved role (or fail closed for `Unknown`).
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
}

// ---------------------------------------------------------------------------
// Terminal projection: text runs plus cursor, read-only, stated boundary
// ---------------------------------------------------------------------------

#[test]
fn fidelity_boundary_is_stated_not_implied() {
    for token in ["cursor", "SGR", "graphics", "images"] {
        assert!(
            FIDELITY_BOUNDARY.contains(token),
            "boundary must name {token}"
        );
    }
}

#[test]
fn terminal_runs_expose_readable_text_and_cursor() {
    let mut state = State::new();
    feed_line(&mut state, "hello");
    prints(&mut state, "world");
    let snapshot = state.snapshot();

    let runs = terminal_runs(&snapshot);
    assert!(runs.len() >= 2);
    assert_eq!(runs[0].row, 0);
    assert_eq!(runs[0].text, "hello");
    assert_eq!(runs[1].row, 1);
    assert_eq!(runs[1].text.trim(), "world");
}

#[test]
fn terminal_blank_rows_produce_no_runs() {
    let state = State::new();
    let snapshot = state.snapshot();
    assert!(terminal_runs(&snapshot).is_empty());
}

#[test]
fn terminal_projection_is_named_by_title_and_read_only() {
    let mut state = State::new();
    prints(&mut state, "data");
    state.apply(&TerminalAction::OscTitle {
        text: "editor".into(),
    });
    let before = state.snapshot();
    let runs = terminal_runs(&before);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "data");
    assert_eq!(state.snapshot(), before);
}

// ---------------------------------------------------------------------------
// Host ingest round trip through the single seam
// ---------------------------------------------------------------------------

fn standard_host() -> UiSnapshotHost {
    let mut host = UiSnapshotHost::new("terminal window", "shell");
    host.push_row(0, "$ echo hi");
    host.push_row(1, "hi");
    host = host.with_cursor(1, 2, true);
    host.push_chrome(ChromeKind::Bar, "status bar", Some("utf-8"));
    host.push_scene(SceneKind::Text, 41);
    host.push_interactive(InteractivePurpose::Button, "follow link", true);
    host.set_focus_label(Some("shell"));
    host.push_notice(AnnouncementKind::Focus, "shell focused");
    host
}

#[test]
fn host_ingest_expose_round_trip() {
    let host = standard_host();
    let generation = Generation::initial();
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, generation, None)
        .expect("host ingest");
    let snapshot = adapter.snapshot().expect("live snapshot");
    assert_eq!(snapshot.generation(), generation);
    assert!(snapshot.lookup_label("shell").is_some());
    assert!(snapshot.lookup_label("status bar").is_some());
    assert!(snapshot.lookup_label("follow link").is_some());

    let mut backend = HeadlessBackend::new();
    backend.set_permission(true);
    adapter.expose_to(&mut backend).expect("expose");
    assert_eq!(backend.exposures().len(), 1);
    let drained = adapter.announce_to(&mut backend).expect("announce");
    assert_eq!(drained, 1);
}

#[test]
fn host_from_real_ui_state_projects_terminal_and_inputs() {
    let snapshot = tree_snapshot();
    let tree = UiTree::new(UiNode::new(
        UiNodeId::new(1),
        UiNodeKind::Box,
        vec![
            UiNode::leaf(UiNodeId::new(2), UiNodeKind::Text(String::from("hello"))),
            UiNode::leaf(
                UiNodeId::new(3),
                UiNodeKind::TextInput {
                    value: String::from("query"),
                    placeholder: String::from("search"),
                },
            ),
        ],
    ))
    .expect("valid tree");
    let before = snapshot.clone();
    let host = UiSnapshotHost::from_terminal_and_tree(&snapshot, &tree);
    assert_eq!(snapshot, before);
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("host ingest");
    let live = adapter.snapshot().expect("live snapshot");
    assert!(live.lookup_label("editor").is_some());
    assert!(live.lookup_label("query").is_some());
}

// ---------------------------------------------------------------------------
// Bounds: fail-closed on the Core side of the seam
// ---------------------------------------------------------------------------

#[test]
fn host_rejects_overlong_names_at_256_boundary() {
    let long = "n".repeat(MAX_ACCESSIBLE_NAME_LEN + 1);
    let mut host = UiSnapshotHost::new(&long, "shell");
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    assert!(matches!(
        adapter.ingest_host(&host, Generation::initial(), None),
        Err(A11yError::NameTooLong { .. })
    ));

    let mut host = UiSnapshotHost::new("window", "shell");
    host.push_chrome(ChromeKind::Rail, &long, None);
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    assert!(matches!(
        adapter.ingest_host(&host, Generation::initial(), None),
        Err(A11yError::NameTooLong { .. })
    ));

    let at_cap = "n".repeat(MAX_ACCESSIBLE_NAME_LEN);
    let mut host = UiSnapshotHost::new(&at_cap, "shell");
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("boundary length is accepted");
}

#[test]
fn host_rejects_unmapped_scene_kind_fail_closed() {
    let mut host = UiSnapshotHost::new("window", "shell");
    host.push_scene(SceneKind::Text, 1);
    host.push_scene(SceneKind::Unknown, 2);
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    assert_eq!(
        adapter.ingest_host(&host, Generation::initial(), None),
        Err(A11yError::UnmappedSceneKind)
    );
    assert!(adapter.snapshot().is_none());
}

#[test]
fn host_rejects_modal_without_overlay_surface() {
    let mut host = UiSnapshotHost::new("window", "shell");
    host.set_modal_overlay(true);
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    assert_eq!(
        adapter.ingest_host(&host, Generation::initial(), None),
        Err(A11yError::InvalidStructure)
    );
}

#[test]
fn host_enforces_4096_node_cap_fail_closed() {
    let mut host = UiSnapshotHost::new("window", "shell");
    for row in 0..(MAX_SNAPSHOT_NODES as u32 + 10) {
        host.push_row(row, "x");
    }
    host.set_focus_label(None);
    let mut adapter = Adapter::new();
    let err = adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect_err("node cap must fail closed");
    assert!(matches!(err, A11yError::TooManyNodes { .. }));
    assert!(adapter.snapshot().is_none());
}

#[test]
fn host_rejects_overlong_announcement_text() {
    let long = "n".repeat(MAX_ANNOUNCEMENT_TEXT_LEN + 1);
    let mut host = UiSnapshotHost::new("window", "shell");
    host.set_focus_label(None);
    host.push_notice(AnnouncementKind::Focus, &long);
    let mut adapter = Adapter::new();
    assert!(matches!(
        adapter.ingest_host(&host, Generation::initial(), None),
        Err(A11yError::AnnouncementTooLong { .. })
    ));
    assert!(adapter.snapshot().is_none());
}

// ---------------------------------------------------------------------------
// Stale generations: rejected with nothing swapped
// ---------------------------------------------------------------------------

#[test]
fn stale_generation_rejected_with_nothing_swapped() {
    let host = standard_host();
    let mut adapter = Adapter::new();
    let advanced = Generation::initial().advance().advance();
    adapter
        .ingest_host(&host, advanced, None)
        .expect("advanced ingest");
    let live_generation = adapter.generation();
    assert_eq!(live_generation, advanced);
    let stale = standard_host();
    let err = adapter
        .ingest_host(&stale, Generation::initial(), None)
        .expect_err("stale ingest must fail");
    assert!(matches!(err, A11yError::StaleGeneration { .. }));
    assert_eq!(adapter.generation(), live_generation);
}

// ---------------------------------------------------------------------------
// No retained handles: each ingest re-binds focus fresh
// ---------------------------------------------------------------------------

#[test]
fn host_retains_no_handles_across_generations() {
    let host = standard_host();
    let mut adapter = Adapter::new();
    let first = Generation::initial();
    adapter
        .ingest_host(&host, first, None)
        .expect("first ingest");
    let second = first.advance();
    adapter
        .ingest_host(&host, second, None)
        .expect("second ingest re-binds fresh");
    assert_eq!(adapter.generation(), second);
    let stale = standard_host();
    assert!(matches!(
        adapter.ingest_host(&stale, first, None),
        Err(A11yError::StaleGeneration { .. })
    ));
}

#[test]
fn chrome_stays_out_of_tab_order_while_state_is_exposed() {
    let host = standard_host();
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("ingest");
    let snapshot = adapter.snapshot().expect("live snapshot");
    let tab_order = snapshot.tab_order();
    assert!(!tab_order.is_empty());
    for handle in &tab_order {
        let view = snapshot.get(*handle).expect("known handle");
        assert!(
            !matches!(view, bitty_a11y::NodeView::Chrome { .. }),
            "chrome must never be a tab stop"
        );
    }
}

// ---------------------------------------------------------------------------
// Announcements: one bounded notice per transition, coalesced + rate-limited
// ---------------------------------------------------------------------------

#[test]
fn announcements_coalesce_consecutive_duplicates() {
    let mut queue = AnnouncementQueue::new();
    assert!(queue.is_empty());
    assert_eq!(queue.push_focus("editor"), Ok(true));
    assert_eq!(queue.push_focus("editor"), Ok(false));
    assert_eq!(queue.len(), 1);
    let drained = queue.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].kind(), AnnouncementKind::Focus);
    assert_eq!(drained[0].text(), "editor");
    assert!(queue.is_empty());
}

#[test]
fn announcements_rate_limit_drops_oldest() {
    let mut queue = AnnouncementQueue::new();
    for i in 0..(MAX_PENDING_ANNOUNCEMENTS + 3) {
        let text = format!("state {i}");
        assert_eq!(queue.push_state(&text), Ok(true));
    }
    assert_eq!(queue.len(), MAX_PENDING_ANNOUNCEMENTS);
    let drained = queue.drain();
    assert_eq!(drained.len(), MAX_PENDING_ANNOUNCEMENTS);
    assert_eq!(drained[0].text(), "state 3");
    let last = format!("state {}", MAX_PENDING_ANNOUNCEMENTS + 2);
    assert_eq!(drained.last().expect("nonempty").text(), last.as_str());
}

// ---------------------------------------------------------------------------
// Settings composition: core-owned reduced motion + high contrast
// ---------------------------------------------------------------------------

#[test]
fn settings_reduced_motion_collapses_to_final_state() {
    use bitty_ui::MotionSpec;
    let authored = MotionSpec::default();
    assert!(!authored.is_instant());
    let reduced = A11ySettings::new().with_reduced_motion(true);
    assert!(reduced.reduced_motion());
    let collapsed = reduced.resolve_motion(authored);
    assert_eq!(collapsed, MotionSpec::INSTANT);
    assert!(collapsed.is_instant());
}

#[test]
fn settings_defaults_leave_motion_and_tokens_untouched() {
    use bitty_ui::MotionSpec;
    let settings = A11ySettings::default();
    assert_eq!(settings, A11ySettings::new());
    assert!(!settings.reduced_motion());
    assert!(!settings.high_contrast());
    let authored = MotionSpec::default();
    assert_eq!(settings.resolve_motion(authored), authored);
    assert_eq!(settings.high_contrast_outlines(), None);
}

#[test]
fn settings_high_contrast_selects_compliant_outlines() {
    use bitty_ui::theme::{SAFE_BORDER_FOCUSED, SAFE_BORDER_IDLE};
    let settings = A11ySettings::new().with_high_contrast(true);
    assert!(settings.high_contrast());
    let (focused, idle) = settings
        .high_contrast_outlines()
        .expect("pair while high contrast");
    assert_eq!(focused, SAFE_BORDER_FOCUSED);
    assert_eq!(idle, SAFE_BORDER_IDLE);
}

// ---------------------------------------------------------------------------
// Hot path: invalidation-driven refresh, no timer, no plugin code
// ---------------------------------------------------------------------------

#[test]
fn hot_path_refresh_is_invalidation_driven() {
    use bitty_a11y::InvalidationSource;
    assert!(!needs_refresh(None));
    for source in [
        InvalidationSource::FocusChange,
        InvalidationSource::AsyncStateChange,
        InvalidationSource::SceneUpdate,
        InvalidationSource::TerminalUpdate,
        InvalidationSource::ChromeUpdate,
    ] {
        assert!(
            needs_refresh(Some(source)),
            "source {source:?} must refresh"
        );
    }
}
