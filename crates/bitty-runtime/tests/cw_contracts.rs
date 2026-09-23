//! CTX-0688: CW contract batch end-to-end (headless).
//!
//! Exercises the five CW contracts through the public paths only:
//! placement binding (CW-19), host lifecycle + bus mediation (CW-20),
//! v1 taxonomy + ledger + routing (CW-22), provider surface (CW-23), and
//! status registry + metrics snapshot (CW-24). No PTY, window, or GPU.
//!
//! CTX-0721 extends the batch: routable delivery envelope (issue #1001,
//! `OQ-058` `SMO-1..SMO-4`), F-3 re-confirmation vs `ADR-0013`
//! (issue #995), frozen v1 surface per the `OQ-056` deferral
//! (issue #1000), and Option A placement acceptance (issue #998,
//! `RFC-OQ-3`).

use bitty_runtime::registry::{
    AgentMessage, BUS_BATCH_MAX_BYTES, BUS_BATCH_MAX_EVENTS, BUS_PUBLISH_CAPABILITY,
    BUS_SUBSCRIBE_CAPABILITY, BUS_V1_BUDGET, BoundedPayload, BusDirection, BusTopicFamily,
    CapabilityLedger, CrossWindowRoute, MessageKind, PanelProviderManifest, PanelProviderRegistry,
    PanelRegistryConfig, PanelRuntime, PanelType, RoutableError, RoutingScope, V1_CORE_TOPICS,
    WorkspaceId, core_topic, is_v1_core_topic,
};
use bitty_ui::ViewId;
use bitty_ui::placement::{PLACEMENT_DIRECTION, Placement, PlacementOption, resolve_focus};
use bitty_ui::status_registry::{StatusInputs, StatusModuleId, StatusSlots, render_module};

fn workspace() -> WorkspaceId {
    WorkspaceId::new(1)
}

#[test]
fn cw19_placement_binding_and_focus_precedence() {
    let mut placement = Placement::new();
    let panel = bitty_ui::panel::PanelId::new(1);
    placement.bind(panel, ViewId::new(10)).unwrap();
    assert_eq!(placement.view_of(panel), Some(ViewId::new(10)));
    placement.reparent(panel, ViewId::new(11)).unwrap();
    assert_eq!(placement.view_of(panel), Some(ViewId::new(11)));
    assert_eq!(
        resolve_focus(Some(panel), Some(ViewId::new(11))).unwrap(),
        bitty_ui::placement::FocusTarget::Panel(panel)
    );
}

#[test]
fn cw20_host_lifecycle_with_generation_validation() {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let handle = host
        .create_panel(PanelType::Terminal, Some(workspace()))
        .unwrap();
    host.mount_panel(handle.id, handle.generation, ViewId::new(10))
        .unwrap();
    host.focus_panel(handle.id, handle.generation, workspace())
        .unwrap();
    host.suspend_panel(handle.id, handle.generation).unwrap();
    host.resume_panel(handle.id, handle.generation).unwrap();
    host.dispose_panel(handle.id, handle.generation).unwrap();
    assert_eq!(host.panel_count(), 0);
}

#[test]
fn cw22_taxonomy_ledger_and_in_process_routing() {
    let topic = core_topic(BusTopicFamily::Git, "branch-changed").unwrap();
    assert!(is_v1_core_topic(topic.as_str()));
    let granted = [BUS_PUBLISH_CAPABILITY.to_string()].into_iter().collect();
    assert!(CapabilityLedger::can_publish(&granted));
    assert!(!CapabilityLedger::can_subscribe(
        &std::collections::BTreeSet::new()
    ));
    assert_eq!(
        CrossWindowRoute::new(3, 3).resolve().unwrap(),
        RoutingScope::InProcess
    );
    assert!(CrossWindowRoute::new(3, 4).resolve().is_err());
    let _ = BusDirection::Publish;
}

#[test]
fn cw23_provider_registration_gates_panel_creation() {
    let mut providers = PanelProviderRegistry::new();
    let manifest = PanelProviderManifest::parse("example.git", vec![PanelType::Helper], 1).unwrap();
    let generation = providers.register(manifest, true).unwrap();
    assert!(generation >= 1);
    assert_eq!(
        providers.providers_for_type(PanelType::Helper),
        ["example.git".to_string()]
    );
}

#[test]
fn cw24_status_slots_compose_with_metrics_snapshot() {
    let left = ["workspace", "cwd", "git"]
        .iter()
        .map(|raw| StatusModuleId::parse(raw).unwrap())
        .collect();
    let center = [StatusModuleId::parse("clock").unwrap()]
        .into_iter()
        .collect();
    let right = ["cpu", "memory", "network"]
        .iter()
        .map(|raw| StatusModuleId::parse(raw).unwrap())
        .collect();
    let slots = StatusSlots {
        left,
        center,
        right,
    };
    slots.validate().unwrap();
    let inputs = StatusInputs {
        workspace: Some("ws-1".to_string()),
        cpu_percent: Some(12.0),
        clock_text: "12:00".to_string(),
        ..StatusInputs::default()
    };
    let order = slots.render_order();
    assert_eq!(order.len(), 7);
    let texts: Vec<String> = order
        .iter()
        .filter_map(|id| render_module(id, &inputs))
        .map(|segment| segment.text)
        .collect();
    assert!(texts.contains(&"ws-1".to_string()));
    assert!(texts.contains(&"cpu 12%".to_string()));
}

#[test]
fn cw20_cw22_host_mediated_bus_roundtrip() {
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
    let topic = host.declare_topic("example.git:branch-changed").unwrap();
    host.subscribe(subscriber.id, subscriber.generation, &topic)
        .unwrap();
    host.publish(
        publisher.id,
        publisher.generation,
        &topic,
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
}

// ---------------------------------------------------------------------------
// CTX-0721: routable envelope through the live host (issue #1001)
// ---------------------------------------------------------------------------

fn routable_host() -> PanelRuntime {
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    for owner in ["example.cmd", "example.worker"] {
        let manifest = PanelProviderManifest::parse(owner, vec![PanelType::Helper], 1).unwrap();
        host.register_panel_provider(manifest, true).unwrap();
    }
    host
}

fn routable_message(id: u64, kind: MessageKind, priority: u8) -> AgentMessage {
    AgentMessage::new(
        id,
        "example.cmd",
        "example.worker",
        Some(7),
        Some(11),
        kind,
        BoundedPayload::try_new("work").unwrap(),
        100,
        200,
        priority,
    )
    .unwrap()
}

#[test]
fn ctx0721_routable_delivery_through_host() {
    let mut host = routable_host();
    host.route_message(routable_message(1, MessageKind::DelegationRequest, 10), 50)
        .unwrap();
    assert_eq!(host.routable_len(), 1);
    // Deduplication: same identity fails closed without state change.
    assert_eq!(
        host.route_message(routable_message(1, MessageKind::DelegationRequest, 10), 50),
        Err(RoutableError::DuplicateMessageId { message_id: 1 })
    );
    assert_eq!(host.routable_len(), 1);
    // Priority drain returns the envelope and clears inflight.
    let drained = host.drain_routable_by_priority(8);
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].message_id(), 1);
    assert_eq!(host.routable_len(), 0);
}

#[test]
fn ctx0721_routable_routing_fails_closed_for_unknown_recipient() {
    let mut host = routable_host();
    let stray = AgentMessage::new(
        9,
        "example.cmd",
        "example.ghost",
        None,
        None,
        MessageKind::Observation,
        BoundedPayload::try_new("stray").unwrap(),
        0,
        0,
        0,
    )
    .unwrap();
    assert_eq!(
        host.route_message(stray, 0),
        Err(RoutableError::UnknownRecipient {
            recipient: "example.ghost".to_string(),
        })
    );
    assert_eq!(host.routable_len(), 0);
}

#[test]
fn ctx0721_routable_cancel_and_expiry_are_explicit() {
    let mut host = routable_host();
    host.route_message(routable_message(2, MessageKind::StatusUpdate, 1), 0)
        .unwrap();
    let cancelled = host.cancel_message(2).unwrap();
    assert_eq!(cancelled.message_id(), 2);
    assert_eq!(
        host.cancel_message(2),
        Err(RoutableError::UnknownMessageId { message_id: 2 })
    );
    host.route_message(routable_message(3, MessageKind::Observation, 0), 0)
        .unwrap();
    let swept = host.sweep_expired_messages(201);
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].message_id(), 3);
    assert_eq!(host.routable_len(), 0);
}

// ---------------------------------------------------------------------------
// CTX-0721: frozen v1 surface (issue #1000, OQ-056 deferral)
// ---------------------------------------------------------------------------

#[test]
fn ctx0721_v1_surface_frozen_per_oq056_deferral() {
    assert_eq!(V1_CORE_TOPICS.len(), 7);
    for raw in V1_CORE_TOPICS {
        assert!(is_v1_core_topic(raw));
    }
    // Budget envelope restates the same ceilings the bus enforces.
    assert_eq!(BUS_V1_BUDGET.per_subscription, 64);
    assert_eq!(BUS_V1_BUDGET.per_panel_events, 1024);
    assert_eq!(BUS_V1_BUDGET.global_events, 8192);
    // Candidate per-family capabilities stay naming-only, unenforced.
    assert_eq!(
        CapabilityLedger::candidate_capability(BusTopicFamily::Git, BusDirection::Publish),
        "panel.bus.publish"
    );
}

// ---------------------------------------------------------------------------
// CTX-0721: Option A placement acceptance (issue #998, RFC-OQ-3)
// ---------------------------------------------------------------------------

#[test]
fn ctx0721_placement_accepts_option_a() {
    assert_eq!(PLACEMENT_DIRECTION, PlacementOption::A);
    // Host mirrors mount into the Option A binding map.
    let mut host = PanelRuntime::new(PanelRegistryConfig::default()).unwrap();
    let handle = host
        .create_panel(PanelType::Terminal, Some(workspace()))
        .unwrap();
    host.mount_panel(handle.id, handle.generation, ViewId::new(30))
        .unwrap();
    assert_eq!(host.placement_view_of(handle.id), Some(ViewId::new(30)));
    assert_eq!(host.placement_panel_of(ViewId::new(30)), Some(handle.id));
}
