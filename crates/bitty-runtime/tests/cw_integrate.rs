//! CTX-0700: CW candidate live-wiring integration.
//!
//! Each test drives a live-path owner ([`Runtime`], [`PanelRuntime`], or
//! [`StatuslineIntegration`]) rather than calling the candidate module
//! directly, so removing the wiring fails these tests while the
//! in-module unit tests keep passing.
//!
//! - #980 (CW-01): fold toggle/expand/collapse into the present path via
//!   `Runtime::cw_fold_apply` / `cw_fold_projection` / `cw_present_plan`.
//! - #982 (CW-03): composer overlay + input routing + editor flag via
//!   `Runtime::cw_composer_*` / `cw_input_route` / `cw_composer_feed`.
//! - #983 (CW-04): cross-panel hint API via the single live
//!   `CwHintEngine` owned by `Runtime`.
//! - #998 (CW-20): panel host shape via `PanelRuntime` (lifecycle plus the
//!   live placement mirror and provider gate) and
//!   `create_statusline_panel_via_host`.
//! - #1000 (CW-22): event bus v1 via `declare_core_topic` / `publish_core` /
//!   `check_window_route` routing a real `git.branch-changed` event.
//! - #1002 (CW-24): status system via `render_registry_slots` reading the
//!   `status_registry` composition.
//!
//! Headless only: no PTY, window, GPU, wall-clock, or filesystem.

use bitty_rich::blocks::{CommandBlock, CommandId, CommandState, SemanticRange};
use bitty_rich::composer::ComposerKeyEvent;
use bitty_rich::hints::{
    HintAction, HintActions, HintAnchor, HintBatch, HintKind, HintRegistry, HintScope,
};
use bitty_rich::scene::{
    BlockAnchor, BlockId, RichBlock, Scene, SceneNode, ScrollBehavior, StyledSpan,
};
use bitty_rich::shell::CommandRegion;
use bitty_runtime::Runtime;
use bitty_runtime::config::RuntimeConfig;
use bitty_runtime::cw_present::{CwFoldAction, CwHintProvider, CwInputRoute};
use bitty_runtime::registry::{
    BUS_BATCH_MAX_BYTES, BUS_BATCH_MAX_EVENTS, BUS_PUBLISH_CAPABILITY, BUS_SUBSCRIBE_CAPABILITY,
    BoundedPayload, BusTopicFamily, PanelProviderManifest, PanelRegistryConfig, PanelRuntime,
    PanelType, RoutingScope, WorkspaceId, is_v1_core_topic,
};
use bitty_runtime::statusline::{
    StatuslineIntegration, create_statusline_panel_via_host, status_inputs_from_state,
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::ViewId;
use bitty_ui::panel::{BrowserSurfaceId, PanelId, ViewContent};
use bitty_ui::status_registry::{StatusModuleId, StatusSlots};
use bitty_ui::uitree::UiNodeId;
use bitty_vt::BoundedString;

fn workspace() -> WorkspaceId {
    WorkspaceId::new(1)
}

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime must build")
}

fn test_block(anchor: u64) -> CommandBlock {
    CommandBlock {
        id: CommandId(anchor),
        command_range: SemanticRange::new(anchor, anchor),
        output_range: None,
        cwd: None,
        exit_code: None,
        state: CommandState::Completed,
        region: CommandRegion {
            prompt_start: Some(anchor),
            input_start: None,
            output_start: None,
            output_end: None,
            exit_code: None,
            prompt_row: None,
            input_row: None,
            output_row: None,
            output_end_row: None,
        },
    }
}

fn test_scene_block(id: u64, text: &str) -> RichBlock {
    RichBlock::new(
        BlockId(id),
        BlockAnchor::Zone(id),
        SceneNode::Text(StyledSpan {
            text: text.to_string(),
            bold: false,
            italic: false,
        }),
        ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .expect("test block fits scene caps")
}

fn apply_cwd(state: &mut State, url: &str) {
    state.apply(&TerminalAction::OscCwd {
        url: BoundedString::new(url),
    });
}

#[test]
fn cw980_fold_toggle_flows_into_live_present_plan() {
    let mut rt = runtime();
    let blocks = vec![test_block(1), test_block(2)];

    // Live fold starts unfolded: both blocks visible.
    let projection = rt.cw_fold_projection(&blocks);
    assert_eq!(projection.visible, vec![CommandId(1), CommandId(2)]);
    assert!(projection.hidden.is_empty());

    // Toggle through the live Runtime path (not the module directly).
    assert!(rt.cw_fold_apply(CommandId(1), CwFoldAction::Toggle));
    assert!(rt.cw_fold_is_folded(CommandId(1)));
    let projection = rt.cw_fold_projection(&blocks);
    assert_eq!(projection.visible, vec![CommandId(2)]);
    assert_eq!(projection.hidden, vec![CommandId(1)]);

    // Expand is idempotent; collapse re-hides.
    assert!(rt.cw_fold_apply(CommandId(1), CwFoldAction::Expand));
    assert!(!rt.cw_fold_is_folded(CommandId(1)));
    assert!(rt.cw_fold_apply(CommandId(2), CwFoldAction::Collapse));
    assert!(rt.cw_fold_is_folded(CommandId(2)));

    // The top-level present plan reflects the live fold state.
    let batch = HintBatch::build(0, &HintRegistry::new());
    let scene = Scene::new();
    let plan = rt.cw_present_plan(
        ViewId::new(1),
        7,
        &blocks,
        &batch,
        &scene,
        ViewContent::Panel(PanelId::new(9)),
        UiNodeId::new(3),
    );
    assert_eq!(plan.generation, 7);
    assert_eq!(plan.visible_command_ids, vec![CommandId(1)]);
    assert_eq!(plan.hidden_command_ids, vec![CommandId(2)]);
    assert_eq!(plan.hint_overlay_cost, 0);
    assert!(plan.nonterminal.is_some());
}

#[test]
fn cw982_composer_overlay_routing_and_editor_flag_through_runtime() {
    let mut rt = runtime();

    // Closed by default: input routes to the PTY byte-identically.
    assert!(!rt.cw_composer_is_open());
    assert_eq!(rt.cw_input_route(), CwInputRoute::Pty);
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::printable('x')),
        bitty_runtime::cw_present::CwComposerFeed::PtyPassthrough
    );
    assert!(!rt.cw_composer_snapshot().is_open());

    // Explicit open routes to the composer; printable text inserts.
    rt.cw_composer_open();
    assert!(rt.cw_composer_is_open());
    assert_eq!(rt.cw_input_route(), CwInputRoute::Composer);
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::printable('h')),
        bitty_runtime::cw_present::CwComposerFeed::Inserted
    );
    assert_eq!(rt.cw_composer_content(), "h");
    assert_eq!(rt.cw_composer_snapshot().draft_bytes, 1);

    // External-editor request is a routing flag only: no process spawns and
    // the session stays open.
    assert_eq!(
        rt.cw_composer_feed(ComposerKeyEvent::alt_e()),
        bitty_runtime::cw_present::CwComposerFeed::EditorRequested
    );
    assert!(rt.cw_composer_is_open());
    rt.cw_composer_apply_external("echo hi")
        .expect("fits composer cap");
    assert_eq!(rt.cw_composer_content(), "echo hi");

    // Submit frames the bracketed-paste PTY write and auto-closes.
    match rt.cw_composer_feed(ComposerKeyEvent::ctrl_enter()) {
        bitty_runtime::cw_present::CwComposerFeed::Submitted(frame) => {
            assert!(frame.starts_with(b"\x1b[200~"));
            assert!(frame.ends_with(b"\x1b[201~\r"));
        }
        other => panic!("expected submit frame, got {other:?}"),
    }
    assert!(!rt.cw_composer_is_open());
    assert_eq!(rt.cw_input_route(), CwInputRoute::Pty);

    // Close is idempotent and preserves the closed default.
    rt.cw_composer_close();
    assert!(!rt.cw_composer_is_open());
}

#[test]
fn cw983_single_hint_engine_owns_labels_and_dispatch_through_runtime() {
    let mut rt = runtime();
    assert_eq!(rt.cw_hint_provider_count(), 0);

    // Register two panel providers through the live Runtime owner.
    assert!(rt.cw_hint_register(CwHintProvider::with_views(1, &[11, 12]).expect("fits cap")));
    assert!(rt.cw_hint_register(CwHintProvider::new(2)));
    assert_eq!(rt.cw_hint_provider_count(), 2);
    // Duplicate panel registration fails closed.
    assert!(!rt.cw_hint_register(CwHintProvider::new(1)));
    assert_eq!(rt.cw_hint_provider_count(), 2);

    // One collection owns every label across panels: single batch, unique
    // labels, zero overlay-slot cost.
    let batch = rt.cw_hint_collect(HintScope(4), 9);
    assert_eq!(batch.generation, 9);
    assert_eq!(batch.len(), 4);
    let mut labels: Vec<&str> = batch.labels.iter().map(|l| l.label.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), 4);

    // Dispatch through the live Runtime mutates only the live fold state.
    let mut registry = HintRegistry::new();
    registry
        .register(
            HintKind::CommandBlock,
            HintAnchor::Command(CommandId(9)),
            HintScope(0),
            HintActions::default_for(HintKind::CommandBlock),
        )
        .expect("fits hint caps");
    let command_batch = HintBatch::build(1, &registry);
    let label = command_batch.labels[0].label.clone();
    let outcome = rt
        .cw_hint_dispatch(&command_batch, &label, HintAction::ToggleFold)
        .expect("toggle fold dispatches");
    assert_eq!(
        outcome,
        bitty_rich::hints::DispatchOutcome::FoldToggled {
            id: CommandId(9),
            folded: true,
        }
    );
    assert!(rt.cw_fold_is_folded(CommandId(9)));
}

#[test]
fn cw998_host_lifecycle_with_placement_mirror_and_provider_gate() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();

    // Provider gate: unknown owners fail closed without allocating.
    assert_eq!(host.panel_count(), 0);
    assert!(
        host.create_panel_for_provider("example.git", PanelType::Helper, Some(workspace()))
            .is_err()
    );
    assert_eq!(host.panel_count(), 0);

    // Register a provider through the live host, then create through it.
    let manifest = PanelProviderManifest::parse("example.git", vec![PanelType::Helper], 1).unwrap();
    host.register_panel_provider(manifest, true).unwrap();
    assert_eq!(
        host.providers_for_type(PanelType::Helper),
        ["example.git".to_string()]
    );
    let handle = host
        .create_panel_for_provider("example.git", PanelType::Helper, Some(workspace()))
        .unwrap();

    // Mount mirrors into the live placement map.
    host.mount_panel(handle.id, handle.generation, ViewId::new(10))
        .unwrap();
    assert_eq!(host.placement_view_of(handle.id), Some(ViewId::new(10)));
    assert_eq!(host.placement_panel_of(ViewId::new(10)), Some(handle.id));
    assert_eq!(host.placement_len(), 1);

    // Focus/suspend/resume flow through the host shape unchanged.
    host.focus_panel(handle.id, handle.generation, workspace())
        .unwrap();
    host.suspend_panel(handle.id, handle.generation).unwrap();
    host.resume_panel(handle.id, handle.generation).unwrap();

    // Unmount clears the mirror without destroying identity.
    let view = host.unmount_panel(handle.id, handle.generation).unwrap();
    assert_eq!(view, ViewId::new(10));
    assert_eq!(host.placement_view_of(handle.id), None);
    assert_eq!(host.placement_len(), 0);

    // Statusline panel creation through the host facade (issue #998 live
    // consumer from the statusline side).
    let status_id =
        create_statusline_panel_via_host(&mut host, WorkspaceId::new(2), ViewId::new(20))
            .expect("statusline panel via host");
    assert_eq!(host.placement_view_of(status_id), Some(ViewId::new(20)));

    // Dispose retires the handle and clears the mirror.
    host.dispose_panel(handle.id, handle.generation).unwrap();
    assert_eq!(host.placement_view_of(handle.id), None);
}

#[test]
fn cw1000_v1_taxonomy_routes_real_git_event_in_process() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let publisher = host
        .create_panel(PanelType::Helper, Some(workspace()))
        .unwrap();
    let subscriber = host
        .create_panel(PanelType::Helper, Some(workspace()))
        .unwrap();
    host.grant_capability(publisher.id, publisher.generation, BUS_PUBLISH_CAPABILITY)
        .unwrap();
    host.grant_capability(
        subscriber.id,
        subscriber.generation,
        BUS_SUBSCRIBE_CAPABILITY,
    )
    .unwrap();

    // A real v1 Core topic minted through the accepted grammar.
    let topic = host
        .declare_core_topic(BusTopicFamily::Git, "branch-changed")
        .unwrap();
    assert!(is_v1_core_topic(topic.as_str()));
    assert_eq!(topic.as_str(), "bitty.panel:git.branch-changed");

    // Live host-mediated publish/subscribe round trip on the real type.
    host.subscribe(subscriber.id, subscriber.generation, &topic)
        .unwrap();
    host.publish_core(
        publisher.id,
        publisher.generation,
        BusTopicFamily::Git,
        "branch-changed",
        BoundedPayload::try_new("main").unwrap(),
    )
    .unwrap();
    let events = host
        .drain_batch(
            subscriber.id,
            subscriber.generation,
            topic.as_str(),
            BUS_BATCH_MAX_EVENTS,
            BUS_BATCH_MAX_BYTES,
        )
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload.as_str(), "main");

    // Routing scope: same-window is in-process, cross-window fails closed.
    assert_eq!(
        PanelRuntime::check_window_route(3, 3).unwrap(),
        RoutingScope::InProcess
    );
    assert!(PanelRuntime::check_window_route(3, 4).is_err());
}

#[test]
fn cw1002_statusline_renders_through_registry_slots() {
    let mut state = State::new();
    apply_cwd(&mut state, "file:///home/user/projects/foo");

    // Snapshot bridge reads committed state only.
    let inputs = status_inputs_from_state(&state, "12:00");
    assert_eq!(
        inputs.cwd.as_deref(),
        Some("file:///home/user/projects/foo")
    );
    assert_eq!(inputs.clock_text, "12:00");

    // Live registry composition: cwd + clock in slot order, metrics as
    // placeholders, battery hidden without hardware.
    let slots = StatusSlots {
        left: ["workspace", "cwd"]
            .iter()
            .map(|raw| StatusModuleId::parse(raw).unwrap())
            .collect(),
        center: [StatusModuleId::parse("clock").unwrap()]
            .into_iter()
            .collect(),
        right: ["cpu", "battery"]
            .iter()
            .map(|raw| StatusModuleId::parse(raw).unwrap())
            .collect(),
    };
    let rendered = StatuslineIntegration::render_registry_slots(&state, &slots, "12:00").unwrap();
    assert!(rendered.contains("file:///home/user/projects/foo"));
    assert!(rendered.contains("12:00"));
    assert!(rendered.contains("cpu \u{2014}"));
    assert!(!rendered.contains("bat "));
    assert!(rendered.chars().count() <= bitty_runtime::statusline::STATUSLINE_MAX_CHARS);
    // Deterministic and pure: twice yields the same string.
    let again = StatuslineIntegration::render_registry_slots(&state, &slots, "12:00").unwrap();
    assert_eq!(rendered, again);

    // Slot keys report render order left → center → right.
    assert_eq!(
        StatuslineIntegration::registry_slot_keys(&slots),
        ["workspace", "cwd", "clock", "cpu", "battery"]
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    );

    // Empty slots collapse to empty (no fallback pollution).
    let empty = StatusSlots::default();
    assert_eq!(
        StatuslineIntegration::render_registry_slots(&state, &empty, "12:00").unwrap(),
        ""
    );

    // Duplicate membership fails closed without rendering.
    let dup = StatusSlots {
        left: [StatusModuleId::parse("cwd").unwrap()]
            .into_iter()
            .collect(),
        center: Vec::new(),
        right: [StatusModuleId::parse("cwd").unwrap()]
            .into_iter()
            .collect(),
    };
    assert!(StatuslineIntegration::render_registry_slots(&state, &dup, "12:00").is_err());

    // Scene budget input for the present plan stays bounded alongside the
    // status composition (guards the shared present-path caps).
    let mut scene = Scene::new();
    for id in 1..=3u64 {
        scene.insert(test_scene_block(id, "hello")).unwrap();
    }
    assert_eq!(scene.len(), 3);
}

// CTX-0736: OQ-051 scene-consumption render path (issues #985 CW-06 /
// #990 CW-11).
//
// Each test drives the live render-path derivation
// (`Runtime::cw_present_plan_for_host`) against the PanelRuntime-owned
// scene slots, so removing the host resolution fails these tests while
// the in-module unit tests keep passing.

fn scene_with_blocks(count: u64) -> Scene {
    let mut scene = Scene::new();
    for id in 1..=count {
        scene.insert(test_scene_block(id, "hello")).unwrap();
    }
    scene
}

#[test]
fn cw985_panel_scene_consumed_in_render_plan_via_host() {
    let rt = runtime();
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let handle = host
        .create_panel(PanelType::Rich, Some(workspace()))
        .unwrap();
    host.attach_panel_scene(handle, scene_with_blocks(3))
        .unwrap();

    let blocks = vec![test_block(1)];
    let batch = HintBatch::build(0, &HintRegistry::new());
    let plan = rt.cw_present_plan_for_host(
        &host,
        ViewId::new(1),
        7,
        &blocks,
        &batch,
        ViewContent::Panel(handle.id),
        UiNodeId::new(3),
    );
    // The attached scene reaches the frame paint budget through the host.
    assert_eq!(plan.generation, 7);
    assert_eq!(
        plan.scene.block_ids,
        vec![BlockId(1), BlockId(2), BlockId(3)]
    );
    assert_eq!(plan.scene.shed, 0);
    // The panel leaf also carries the beyond-grid payload (issue #990).
    let payload = plan
        .nonterminal
        .expect("panel leaf carries beyond-grid payload");
    assert_eq!(payload.panel, handle.id.get());
    assert_eq!(payload.node, UiNodeId::new(3));
    assert_eq!(payload.overlay_cost(), 0);
}

#[test]
fn cw985_missing_scene_fails_closed_in_render_plan() {
    let rt = runtime();
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let handle = host
        .create_panel(PanelType::Rich, Some(workspace()))
        .unwrap();

    // No scene attached: the frame budgets nothing and never crashes, while
    // the leaf content is still described beyond the grid.
    let blocks = vec![test_block(1)];
    let batch = HintBatch::build(0, &HintRegistry::new());
    let plan = rt.cw_present_plan_for_host(
        &host,
        ViewId::new(1),
        7,
        &blocks,
        &batch,
        ViewContent::Panel(handle.id),
        UiNodeId::new(3),
    );
    assert!(plan.scene.block_ids.is_empty());
    assert_eq!(plan.scene.shed, 0);
    assert!(plan.nonterminal.is_some());
}

#[test]
fn cw985_terminal_leaf_keeps_grid_path_in_render_plan() {
    let rt = runtime();
    let host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let blocks = vec![test_block(1)];
    let batch = HintBatch::build(0, &HintRegistry::new());
    // Terminal and Empty leaves never leave the grid path: no scene budget
    // and no beyond-grid payload, even with scenes attached elsewhere.
    for content in [ViewContent::Terminal(7), ViewContent::Empty] {
        let plan = rt.cw_present_plan_for_host(
            &host,
            ViewId::new(1),
            7,
            &blocks,
            &batch,
            content,
            UiNodeId::new(3),
        );
        assert!(plan.scene.block_ids.is_empty());
        assert!(plan.nonterminal.is_none());
    }
}

#[test]
fn cw990_browser_leaf_beyond_grid_payload_in_render_plan() {
    let rt = runtime();
    let host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let blocks = vec![test_block(1)];
    let batch = HintBatch::build(0, &HintRegistry::new());
    // Browser leaves carry no host-owned scene slot (empty budget) but do
    // carry the beyond-grid payload addressed by the canonical node.
    let plan = rt.cw_present_plan_for_host(
        &host,
        ViewId::new(1),
        7,
        &blocks,
        &batch,
        ViewContent::Browser(BrowserSurfaceId::new(9)),
        UiNodeId::new(3),
    );
    assert!(plan.scene.block_ids.is_empty());
    let payload = plan
        .nonterminal
        .expect("browser leaf carries beyond-grid payload");
    assert_eq!(payload.panel, 9);
    assert_eq!(payload.node, UiNodeId::new(3));
    assert_eq!(payload.overlay_cost(), 0);
}
