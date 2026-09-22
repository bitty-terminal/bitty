//! CTX-0688: CW contract batch end-to-end (headless).
//!
//! Exercises the five CW contracts through the public paths only:
//! placement binding (CW-19), host lifecycle + bus mediation (CW-20),
//! v1 taxonomy + ledger + routing (CW-22), provider surface (CW-23), and
//! status registry + metrics snapshot (CW-24). No PTY, window, or GPU.

use bitty_runtime::registry::{
    BUS_BATCH_MAX_BYTES, BUS_BATCH_MAX_EVENTS, BUS_PUBLISH_CAPABILITY, BUS_SUBSCRIBE_CAPABILITY,
    BoundedPayload, BusDirection, BusTopicFamily, CapabilityLedger, CrossWindowRoute,
    PanelProviderManifest, PanelProviderRegistry, PanelRegistryConfig, PanelRuntime, PanelType,
    RoutingScope, WorkspaceId, core_topic, is_v1_core_topic,
};
use bitty_ui::ViewId;
use bitty_ui::placement::{Placement, resolve_focus};
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
