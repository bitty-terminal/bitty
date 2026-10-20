//! Accessibility seam parity tests (CTX-1362, Refs #2257).
//!
//! Pins Core's `SnapshotHost` implementation on the `bitty-a11y` seam against
//! the accepted contract: role vocabulary equivalence, byte-exact terminal
//! text-run projection with the stated fidelity boundary, focus/tab-order
//! selection, bounded inputs fail-closed (256-name, 4096-node,
//! 256-announcement, 8-pending caps), stale-generation rejection with nothing
//! swapped, no-retained-handle behavior, the no-backend fallback, and
//! dropped-field structural failure. Headless, deterministic, bounded: no
//! window, no GPU, no PTY, no filesystem, no clock.

#![forbid(unsafe_code)]

use bitty_a11y::{
    A11yError, Adapter, AnnouncementKind, AnnouncementQueue, ChromeKind, FocusOwner, Generation,
    HeadlessBackend, InteractivePurpose, MAX_ACCESSIBLE_NAME_LEN, MAX_ANNOUNCEMENT_TEXT_LEN,
    MAX_PENDING_ANNOUNCEMENTS, MAX_SNAPSHOT_NODES, Role, SceneKind, SnapshotBuilder, SnapshotHost,
    needs_refresh, role_of,
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::UiNodeKind;
use bitty_ui::a11y_host::{
    A11ySettings, FALLBACK_ACCESSIBLE_NAME, FIDELITY_BOUNDARY, UiSnapshotHost, a11y_role_of,
    terminal_runs,
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
fn seam_ceilings_match_accepted_bounds() {
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
fn terminal_runs_keep_zero_width_scalars() {
    // CTX-1367 (#2260): a11y runs must expose the same cluster copy does.
    let mut state = State::new();
    prints(&mut state, "e\u{0301}\u{2764}\u{FE0F}\u{1F468}\u{200D}");
    let snapshot = state.snapshot();
    let runs = terminal_runs(&snapshot);
    assert_eq!(runs[0].text, "e\u{0301}\u{2764}\u{FE0F}\u{1F468}\u{200D}");
}

#[test]
fn overlong_pty_title_uses_core_fallback_and_ingests() {
    // A PTY can set a 4096-byte OSC title while the accessibility ceiling is
    // 256 characters. The host substitutes a fixed Core-owned name for both
    // the root and the terminal region instead of failing every ingest.
    let mut state = State::new();
    let long = "t".repeat(MAX_ACCESSIBLE_NAME_LEN + 1);
    state.apply(&TerminalAction::OscTitle {
        text: long.clone().into(),
    });
    prints(&mut state, "body");
    let terminal = state.snapshot();
    assert_eq!(terminal.title.as_str(), long);
    let host = UiSnapshotHost::from_terminal(&terminal);
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("overlong PTY title must not fail the ingest");
    let live = adapter.snapshot().expect("live snapshot");
    assert!(live.lookup_label(FALLBACK_ACCESSIBLE_NAME).is_some());
    assert!(live.lookup_label(&long).is_none());
    assert_eq!(live_row_texts(&adapter), vec!["body"]);
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

/// Builds a host with content distinguishable from another prefix.
fn prefixed_host(prefix: &str) -> UiSnapshotHost {
    let root = format!("{prefix} window");
    let title = format!("{prefix} shell");
    let row = format!("{prefix} row");
    let chrome = format!("{prefix} status");
    let action = format!("{prefix} action");
    let mut host = UiSnapshotHost::new(&root, &title);
    host.push_row(0, &row);
    host.push_chrome(ChromeKind::Bar, &chrome, None);
    host.push_interactive(InteractivePurpose::Button, &action, true);
    host.set_focus_label(Some(&title));
    host
}

/// Reads the live terminal-row content, in snapshot order.
fn live_row_texts(adapter: &Adapter) -> Vec<String> {
    let snapshot = adapter.snapshot().expect("live snapshot");
    snapshot
        .preorder()
        .filter_map(|handle| match snapshot.get(handle) {
            Some(bitty_a11y::NodeView::TerminalRow { text }) => Some(text.to_string()),
            _ => None,
        })
        .collect()
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

/// Independently reads the expected rows from the live grid cells.
///
/// This deliberately does not call `terminal_runs`: a mutation in the
/// projection (for example dropping zero-width scalars) must fail the
/// round-trip test, not move the expected value with it.
fn expected_runs_from_cells(snapshot: &bitty_term_state::Snapshot) -> Vec<(u32, String)> {
    let mut rows = Vec::new();
    for row in 0..snapshot.height {
        let start = row.saturating_mul(snapshot.width);
        let end = start
            .saturating_add(snapshot.width)
            .min(snapshot.cells.len());
        let Some(cells) = snapshot.cells.get(start..end) else {
            continue;
        };
        let mut text = String::new();
        for cell in cells {
            if cell.spacer {
                continue;
            }
            text.push(cell.glyph);
            for mark in &cell.zerowidth {
                text.push(*mark);
            }
        }
        let trimmed = text.trim_end();
        if !trimmed.is_empty() {
            rows.push((u32::try_from(row).unwrap_or(u32::MAX), trimmed.to_string()));
        }
    }
    rows
}

/// The Core projection round-trips through `SnapshotHost` byte-exactly.
///
/// Grid text comes from the same live terminal `Snapshot` the renderer reads;
/// the adapter must not retype, normalize, trim, or reorder a row. Selection
/// is the accessibility selection: the tab order contains the terminal leaf
/// and interactive nodes only, chrome is excluded, and the recorded focus
/// owner is the terminal leaf Core reported as focused.
#[test]
fn round_trip_matches_grid_text_and_selection_byte_exactly() {
    let mut state = State::new();
    feed_line(&mut state, "hello");
    // `feed_line` uses LF only; return to column zero so row 1 starts at the
    // left margin and the literal expectation stays readable.
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
    // A wide character and a combining mark exercise the same cell-walk the
    // renderer uses; neither may be split or normalized by the seam.
    prints(&mut state, "wide界");
    prints(&mut state, "e\u{0301}");
    state.apply(&TerminalAction::OscTitle {
        text: "editor".into(),
    });
    let terminal = state.snapshot();
    let before = terminal.clone();
    let expected_rows = expected_runs_from_cells(&terminal);

    let mut host = UiSnapshotHost::from_terminal(&terminal);
    host.push_chrome(ChromeKind::Bar, "status bar", Some("utf-8"));
    host.push_scene(SceneKind::Text, 7);
    host.push_interactive(InteractivePurpose::Button, "follow link", true);
    host.set_focus_label(Some("editor"));
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("host ingest");
    let live = adapter.snapshot().expect("live snapshot");

    // Grid rows: same order, same bytes, no truncation or retyping.
    let mut actual_rows: Vec<(u32, String)> = Vec::new();
    for handle in live.preorder() {
        let Some(bitty_a11y::NodeView::TerminalRow { text }) = live.get(handle) else {
            continue;
        };
        let anchor = adapter.resolve(handle).expect("live row anchor");
        let bitty_a11y::Anchor::TerminalRow { row } = anchor else {
            panic!("terminal row node must carry a row anchor, got {anchor:?}");
        };
        actual_rows.push((*row, text.to_string()));
    }
    assert_eq!(
        expected_rows,
        vec![
            (0, String::from("hello")),
            (1, String::from("wide界e\u{0301}")),
        ],
        "the independent cell walk must see the literal grid text"
    );
    assert_eq!(actual_rows.len(), expected_rows.len());
    for (actual, expected) in actual_rows.iter().zip(&expected_rows) {
        assert_eq!(actual.0, expected.0, "row ordinal must be preserved");
        assert_eq!(
            actual.1.as_bytes(),
            expected.1.as_bytes(),
            "row {} must round-trip byte-exactly",
            expected.0
        );
    }

    // Selection: terminal leaf first, then the interactive control; chrome
    // is never a tab stop.
    let tab_order = live.tab_order();
    assert_eq!(tab_order.len(), 2, "terminal + one interactive node");
    assert!(matches!(
        live.get(tab_order[0]),
        Some(bitty_a11y::NodeView::Terminal { .. })
    ));
    assert!(matches!(
        live.get(tab_order[1]),
        Some(bitty_a11y::NodeView::Interactive {
            purpose: InteractivePurpose::Button,
            name: "follow link",
            enabled: true,
        })
    ));
    for handle in &tab_order {
        assert!(!matches!(
            live.get(*handle),
            Some(bitty_a11y::NodeView::Chrome { .. })
        ));
    }

    // Focus association is Core-owned and points at the terminal leaf.
    let owner = adapter.focus().owner().expect("terminal leaf focused");
    assert_eq!(owner.anchor(), &bitty_a11y::Anchor::TerminalLeaf);

    // The projection is read-only: the source terminal snapshot is untouched.
    assert_eq!(terminal, before);
    assert_eq!(state.snapshot(), before);
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

#[test]
fn host_announcement_overflow_drops_oldest_not_fail_closed() {
    // W-134: the 8-pending bound is a drop-oldest queue, not a fail-closed
    // ingest error. The newest state wins; the ingest itself succeeds.
    let mut host = UiSnapshotHost::new("window", "shell");
    host.set_focus_label(None);
    let overflow = MAX_PENDING_ANNOUNCEMENTS + 2;
    for index in 0..overflow {
        host.push_notice(AnnouncementKind::AsyncState, &format!("state {index}"));
    }
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("overflow drops the oldest, it does not fail the ingest");
    let pending = adapter.pending_announcements();
    assert_eq!(pending.len(), MAX_PENDING_ANNOUNCEMENTS);
    assert_eq!(pending.first().expect("nonempty").text(), "state 2");
    assert_eq!(
        pending.last().expect("nonempty").text(),
        format!("state {}", overflow - 1).as_str()
    );
}

// ---------------------------------------------------------------------------
// Structural mutation: a dropped field fails closed with nothing swapped
// ---------------------------------------------------------------------------

/// A host that names the root but never supplies the terminal leaf.
///
/// The seam's contract requires both; a partial projection must fail before
/// anything attaches, never present a root-only tree to assistive technology.
struct DroppedTerminalHost;

impl SnapshotHost for DroppedTerminalHost {
    fn build_snapshot(&self, builder: &mut SnapshotBuilder) -> Result<(), A11yError> {
        builder.set_root_name("window")?;
        Ok(())
    }

    fn focus_for(
        &self,
        _snapshot: &bitty_a11y::Snapshot,
        _map: &bitty_a11y::HandleMap,
    ) -> Result<Option<FocusOwner>, A11yError> {
        Ok(None)
    }

    fn announcements(&self) -> Vec<(AnnouncementKind, String)> {
        Vec::new()
    }
}

#[test]
fn dropped_field_fails_closed_and_live_snapshot_preserved() {
    let old = prefixed_host("old");
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&old, Generation::initial(), None)
        .expect("old ingest");
    assert_eq!(
        adapter.ingest_host(&DroppedTerminalHost, Generation::initial(), None),
        Err(A11yError::InvalidStructure)
    );
    let live = adapter.snapshot().expect("old snapshot preserved");
    assert!(live.lookup_label("old shell").is_some());
    assert!(
        live.lookup_label("window").is_none(),
        "no partial root-only tree"
    );
    assert_eq!(live_row_texts(&adapter), vec!["old row"]);
    assert_eq!(adapter.generation(), Generation::initial());
}

// ---------------------------------------------------------------------------
// Disabled extension / platform fallback: exposure drops, Core baseline stays
// ---------------------------------------------------------------------------

#[test]
fn disabled_extension_fallback_keeps_core_baseline() {
    let terminal = tree_snapshot();
    let before = terminal.clone();
    let host = UiSnapshotHost::from_terminal(&terminal);
    let mut adapter = Adapter::new();
    adapter
        .ingest_host(&host, Generation::initial(), None)
        .expect("host ingest works with zero backends");
    let live_len = adapter.snapshot().expect("live snapshot").len();

    // Platform permission denied is the runtime "extension disabled" state:
    // nothing is exposed, nothing is lost, and the projection stays live.
    let mut backend = HeadlessBackend::new();
    assert_eq!(
        adapter.expose_to(&mut backend),
        Err(A11yError::PermissionDenied)
    );
    assert!(backend.exposures().is_empty());
    assert_eq!(adapter.snapshot().expect("live snapshot").len(), live_len);
    assert_eq!(terminal, before, "terminal truth is untouched");
    assert!(!needs_refresh(None), "no invalidation, no rebuild");
}

// ---------------------------------------------------------------------------
// Stale generations: rejected and the previous snapshot is preserved
// ---------------------------------------------------------------------------

#[test]
fn stale_generation_rejected_and_live_snapshot_preserved() {
    // The old and new hosts carry different labels, rows, and actions, so a
    // swap to the rejected snapshot would be visible in the assertions below.
    let old = prefixed_host("old");
    let new = prefixed_host("new");
    let mut adapter = Adapter::new();
    let advanced = Generation::initial().advance().advance();
    adapter
        .ingest_host(&old, advanced, None)
        .expect("advanced ingest");
    let live_generation = adapter.generation();
    assert_eq!(live_generation, advanced);
    let err = adapter
        .ingest_host(&new, Generation::initial(), None)
        .expect_err("stale ingest must fail");
    assert!(matches!(err, A11yError::StaleGeneration { .. }));
    assert_eq!(
        adapter.generation(),
        live_generation,
        "generation unchanged"
    );
    let live = adapter.snapshot().expect("old snapshot preserved");
    assert!(live.lookup_label("old shell").is_some());
    assert!(live.lookup_label("new shell").is_none());
    assert!(live.lookup_label("old status").is_some());
    assert!(live.lookup_label("new status").is_none());
    assert_eq!(live_row_texts(&adapter), vec!["old row"]);
}

// ---------------------------------------------------------------------------
// No retained handles: each ingest re-binds focus fresh
// ---------------------------------------------------------------------------

#[test]
fn fresh_ingest_rebinds_handles_without_retaining_old_ones() {
    // The second host differs from the first in labels and row content, and
    // the first generation's handles/focus must not resolve against the new
    // live projection.
    let old = prefixed_host("old");
    let new = prefixed_host("new");
    let mut adapter = Adapter::new();
    let first = Generation::initial();
    adapter
        .ingest_host(&old, first, None)
        .expect("first ingest");
    let old_handles: Vec<_> = adapter
        .snapshot()
        .expect("first snapshot")
        .preorder()
        .collect();
    let old_owner = adapter.focus().owner().expect("old focus").handle();
    assert!(old_handles.contains(&old_owner));

    let second = first.advance();
    adapter
        .ingest_host(&new, second, None)
        .expect("second ingest re-binds fresh");
    assert_eq!(adapter.generation(), second);

    let live = adapter.snapshot().expect("second snapshot");
    let new_handles: Vec<_> = live.preorder().collect();
    assert!(
        new_handles
            .iter()
            .all(|handle| !old_handles.contains(handle)),
        "handles from the previous generation must not be reused"
    );
    let err = adapter
        .resolve(old_owner)
        .expect_err("old-generation handle must not resolve");
    assert!(matches!(
        err,
        A11yError::UnknownHandle | A11yError::StaleGeneration { .. }
    ));

    let new_owner = adapter.focus().owner().expect("new focus");
    assert_eq!(adapter.focus().generation(), second);
    assert!(new_handles.contains(&new_owner.handle()));
    assert!(live.lookup_label("new shell").is_some());
    assert!(live.lookup_label("old shell").is_none());
    assert_eq!(live_row_texts(&adapter), vec!["new row"]);

    assert!(matches!(
        adapter.ingest_host(&old, first, None),
        Err(A11yError::StaleGeneration { .. })
    ));
}

#[test]
fn identical_reingest_does_not_advance_generation_or_grow() {
    let host = standard_host();
    let mut adapter = Adapter::new();
    let generation = Generation::initial();
    adapter
        .ingest_host(&host, generation, None)
        .expect("first ingest");
    let len = adapter.snapshot().expect("live snapshot").len();
    adapter
        .ingest_host(&host, generation, None)
        .expect("identical re-ingest");
    assert_eq!(adapter.generation(), generation);
    assert_eq!(adapter.snapshot().expect("live snapshot").len(), len);
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
