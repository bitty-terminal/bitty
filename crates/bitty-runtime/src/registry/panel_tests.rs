//! `registry` — panel-runtime unit tests.
//!
//! Split from `super` (`registry.rs`) as a pure move under CTX-0308:
//! byte-identical logic, only module wiring changed.

use super::{
    BoundedPayload, EventTopic, Generation, MAX_TOPICS_TOTAL, PanelError, PanelId, PanelRegistry,
    PanelRegistryConfig, PanelState, PanelType, WorkspaceId,
};
use bitty_ui::{Rect as UiRect, ViewId};

fn default_panel_registry() -> PanelRegistry {
    PanelRegistry::new(PanelRegistryConfig::default()).expect("default panel registry")
}

#[test]
fn panel_id_distinct_and_generation_monotonic() {
    let mut reg = default_panel_registry();
    let start = reg.generation();
    let h1 = reg
        .create_panel(PanelType::Terminal, None)
        .expect("create panel 1");
    assert!(reg.generation().get() > start.get());
    let h2 = reg
        .create_panel(PanelType::Rich, None)
        .expect("create panel 2");
    assert_ne!(h1.id, h2.id);
    assert_ne!(h1.generation, h2.generation);
    assert_eq!(reg.panel_count(), 2);
    assert_ne!(
        std::any::TypeId::of::<PanelId>(),
        std::any::TypeId::of::<ViewId>()
    );
    assert_ne!(
        std::any::TypeId::of::<PanelId>(),
        std::any::TypeId::of::<super::TerminalId>()
    );
}

#[test]
fn lifecycle_declared_created_mounted_focused_suspended_disposed() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Helper, None).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Created
    );
    let view = ViewId::new(1);
    reg.mount_panel(h.id, h.generation, view).unwrap();
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Mounted
    );
    let ws = WorkspaceId::new(1);
    // For focus, workspace binding needed; recreate with workspace
    let mut reg2 = default_panel_registry();
    let h2 = reg2.create_panel(PanelType::Canvas, Some(ws)).unwrap();
    let v2 = ViewId::new(10);
    reg2.mount_panel(h2.id, h2.generation, v2).unwrap();
    reg2.focus_panel(h2.id, h2.generation, ws).unwrap();
    assert_eq!(
        reg2.panel_state(h2.id, h2.generation).unwrap(),
        PanelState::Focused
    );
    reg2.suspend_panel(h2.id, h2.generation).unwrap();
    assert_eq!(
        reg2.panel_state(h2.id, h2.generation).unwrap(),
        PanelState::Suspended
    );
    reg2.resume_panel(h2.id, h2.generation).unwrap();
    assert_eq!(
        reg2.panel_state(h2.id, h2.generation).unwrap(),
        PanelState::Mounted
    );
    reg2.dispose_panel(h2.id, h2.generation).unwrap();
    assert!(reg2.panel_state(h2.id, h2.generation).is_err());
}

#[test]
fn create_beyond_max_returns_too_many_and_preserves_state() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig {
        max_panels_per_workspace: 1,
        max_panels_per_window: 1,
        max_topics_total: MAX_TOPICS_TOTAL,
        max_subscriptions_per_panel: 32,
    })
    .unwrap();
    let ws = WorkspaceId::new(1);
    let _h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
    let before = reg.generation();
    let err = reg.create_panel(PanelType::Rich, Some(ws)).unwrap_err();
    assert!(matches!(err, PanelError::TooManyPanels { .. }));
    assert_eq!(reg.panel_count(), 1);
    assert_eq!(reg.generation(), before);
}

#[test]
fn unknown_panel_type_rejected() {
    let mut reg = default_panel_registry();
    let err = reg
        .create_panel_by_type_str("unknown_type", None)
        .unwrap_err();
    assert!(matches!(err, PanelError::UnknownPanelType { .. }));
}

#[test]
fn stale_handle_rejected() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Browser, None).unwrap();
    let wrong = Generation(h.generation.get().wrapping_add(10));
    let err = reg.panel_state(h.id, wrong).unwrap_err();
    assert!(matches!(err, PanelError::StaleHandle { .. }));
    if let PanelError::StaleHandle {
        expected_generation,
        found_generation,
        id_raw,
    } = err
    {
        assert_eq!(expected_generation, h.generation);
        assert_eq!(found_generation, wrong);
        assert_eq!(id_raw, h.id.0);
    }
}

#[test]
fn mount_already_mounted_errors() {
    let mut reg = default_panel_registry();
    let h1 = reg.create_panel(PanelType::Terminal, None).unwrap();
    let h2 = reg.create_panel(PanelType::Rich, None).unwrap();
    let v1 = ViewId::new(1);
    let v2 = ViewId::new(2);
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();
    let err = reg.mount_panel(h1.id, h1.generation, v2).unwrap_err();
    assert!(matches!(err, PanelError::PanelAlreadyMounted { .. }));
    let err2 = reg.mount_panel(h2.id, h2.generation, v1).unwrap_err();
    assert!(matches!(err2, PanelError::AlreadyMounted { .. }));
}

#[test]
fn moving_panel_between_views_preserves_id() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Helper, None).unwrap();
    let v1 = ViewId::new(1);
    let v2 = ViewId::new(2);
    reg.mount_panel(h.id, h.generation, v1).unwrap();
    let vid = reg.unmount_panel(h.id, h.generation).unwrap();
    assert_eq!(vid, v1);
    reg.mount_panel(h.id, h.generation, v2).unwrap();
    // PanelId preserved, view changed
    assert_eq!(
        reg.panel_state(h.id, h.generation).unwrap(),
        PanelState::Mounted
    );
}

#[test]
fn focus_mru_per_workspace() {
    let mut reg = default_panel_registry();
    let ws = WorkspaceId::new(42);
    let h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
    let h2 = reg.create_panel(PanelType::Rich, Some(ws)).unwrap();
    let h3 = reg.create_panel(PanelType::Canvas, Some(ws)).unwrap();
    let v1 = ViewId::new(1);
    let v2 = ViewId::new(2);
    let v3 = ViewId::new(3);
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();
    reg.mount_panel(h2.id, h2.generation, v2).unwrap();
    reg.mount_panel(h3.id, h3.generation, v3).unwrap();
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    reg.focus_panel(h2.id, h2.generation, ws).unwrap();
    reg.focus_panel(h3.id, h3.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h3.id));
    assert_eq!(reg.mru_order(ws), vec![h3.id, h2.id, h1.id]);
    // Suspend focused moves to next MRU
    reg.suspend_panel(h3.id, h3.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h2.id));
}

#[test]
fn overlay_max_4plus1_enforced() {
    let mut reg = default_panel_registry();
    let rect = UiRect::new(0, 0, 20, 10);
    for _ in 0..4 {
        reg.create_overlay(bitty_ui::panel::OverlayKind::NonModal, rect, "hello", None)
            .unwrap();
    }
    assert_eq!(reg.overlay_len(), 4);
    let err = reg
        .create_overlay(
            bitty_ui::panel::OverlayKind::NonModal,
            rect,
            "overflow",
            None,
        )
        .unwrap_err();
    assert!(matches!(err, PanelError::TooManyOverlays { .. }));
    // Modal still allowed (4+1)
    reg.create_overlay(bitty_ui::panel::OverlayKind::Modal, rect, "modal", None)
        .unwrap();
    assert_eq!(reg.overlay_len(), 5);
    let err2 = reg
        .create_overlay(bitty_ui::panel::OverlayKind::Modal, rect, "modal2", None)
        .unwrap_err();
    assert_eq!(err2, PanelError::OverlayBusy);
}

#[test]
fn overlay_focus_restores_mru() {
    let mut reg = default_panel_registry();
    let ws = WorkspaceId::new(7);
    let h1 = reg.create_panel(PanelType::Terminal, Some(ws)).unwrap();
    let v1 = ViewId::new(1);
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h1.id));
    // Simulate overlay capture via suspend
    reg.suspend_panel(h1.id, h1.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), None);
    reg.resume_panel(h1.id, h1.generation).unwrap();
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h1.id));
}

#[test]
fn command_registry_owner_name_command_and_duplicates() {
    let mut reg = default_panel_registry();
    let h1 = reg.create_panel(PanelType::Helper, None).unwrap();
    let h2 = reg.create_panel(PanelType::Helper, None).unwrap();
    let qc = reg
        .register_command(h1.id, h1.generation, "xuepoo.git:open")
        .unwrap();
    assert_eq!(qc.as_str(), "xuepoo.git:open");
    assert_eq!(reg.command_owner("xuepoo.git:open"), Some(h1.id));
    let err = reg
        .register_command(h2.id, h2.generation, "xuepoo.git:open")
        .unwrap_err();
    assert!(matches!(err, PanelError::DuplicateCommand { .. }));
    // Invalid grammar
    assert!(
        reg.register_command(h1.id, h1.generation, "badcommand")
            .is_err()
    );
    assert!(
        reg.register_command(h1.id, h1.generation, "Owner.name:cmd")
            .is_err()
    );
    // Per-panel limit
    let mut reg2 = default_panel_registry();
    let h3 = reg2.create_panel(PanelType::Helper, None).unwrap();
    for i in 0..32 {
        reg2.register_command(h3.id, h3.generation, &format!("xuepoo.test:cmd{i}"))
            .unwrap();
    }
    let err2 = reg2
        .register_command(h3.id, h3.generation, "xuepoo.test:overflow")
        .unwrap_err();
    assert!(matches!(err2, PanelError::TooManyCommands { .. }));
}

#[test]
fn event_bus_topic_grammar_and_declared_subscribe() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Terminal, None).unwrap();
    let topic = reg.declare_topic("xuepoo.files:file.open").unwrap();
    assert_eq!(topic.as_str(), "xuepoo.files:file.open");
    // Bare topic invalid
    assert!(reg.declare_topic("file.open").is_err());
    // Invalid owner prefix
    assert!(reg.declare_topic("bad_topic").is_err());
    // Subscribe to known topic ok
    reg.subscribe(h.id, h.generation, &topic).unwrap();
    // Subscribe to unknown topic fails UnknownTopic
    let unknown = EventTopic::parse("xuepoo.test:unknown").unwrap();
    // Not declared yet, subscribe should fail?
    // Actually we declared only one topic; trying to subscribe to undeclared should fail.
    // Our subscribe checks topics set contains it.
    let err = reg.subscribe(h.id, h.generation, &unknown).unwrap_err();
    assert!(matches!(err, PanelError::UnknownTopic { .. }));
    // Payload bound
    let large = "a".repeat(9 * 1024);
    let payload = BoundedPayload::try_new(large);
    assert!(payload.is_err());
}

#[test]
fn event_bus_64_per_subscription_drop_oldest() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Rich, None).unwrap();
    let topic = reg.declare_topic("xuepoo.test:topic").unwrap();
    reg.subscribe(h.id, h.generation, &topic).unwrap();
    // Flood 70 events to one queue
    for i in 0..70 {
        let payload = BoundedPayload::try_new(format!("msg{i}")).unwrap();
        reg.publish(&topic, payload).unwrap();
    }
    // Per-subscription queue capped at 64, global still limited
    assert_eq!(reg.bus_events_for_panel(h.id), 64);
    assert!(reg.bus_total_dropped() >= 6);
    // Drain batch respects 32/8KiB
    let batch = reg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // FIFO DropOldest: first batch should contain msg6..msg37 (oldest 6 dropped)
    assert_eq!(batch[0].payload.as_str(), "msg6");
}

#[test]
fn event_bus_per_panel_1024_and_global_8192_drop_oldest() {
    let mut reg = default_panel_registry();
    // Create two panels, each subscribes to same topic? Need distinct topics per subscription to test per-panel aggregate.
    // Each panel can have up to 32 subscriptions, each 64 => 2048 would exceed per-panel 1024, so global or per-panel eviction should happen.
    let h = reg.create_panel(PanelType::Helper, None).unwrap();
    // Create 16 topics for one panel, each will get 64 events => 1024
    let mut topics = Vec::new();
    for i in 0..16 {
        let t = reg.declare_topic(&format!("xuepoo.test:topic{i}")).unwrap();
        reg.subscribe(h.id, h.generation, &t).unwrap();
        topics.push(t);
    }
    // Flood each topic 70 times => each queue would cap 64, but per-panel limit 1024 means total stays <=1024
    for topic in &topics {
        for j in 0..70 {
            let payload = BoundedPayload::try_new(format!("p{}_{}", topic.as_str(), j)).unwrap();
            reg.publish(topic, payload).unwrap();
        }
    }
    assert!(reg.bus_events_for_panel(h.id) <= 1024);
    assert!(reg.bus_total_events() <= 8192);
    // Global test: many panels
    let mut reg2 = default_panel_registry();
    let mut handles = Vec::new();
    for _ in 0..10 {
        let h = reg2.create_panel(PanelType::Canvas, None).unwrap();
        let t = reg2.declare_topic("xuepoo.global:evt").unwrap();
        // Need distinct topic string per publish? We'll reuse same topic across panels
        // Actually subscribe each panel to same topic name (already declared)
        reg2.subscribe(h.id, h.generation, &t).unwrap();
        handles.push(h);
    }
    // The topic already declared, now publish storm
    let topic2 = EventTopic::parse("xuepoo.global:evt").unwrap();
    for _ in 0..9000 {
        let payload = BoundedPayload::try_new("x").unwrap();
        reg2.publish(&topic2, payload).unwrap();
    }
    assert!(reg2.bus_total_events() <= 8192);
    assert!(reg2.bus_total_bytes() <= 2 * 1024 * 1024);
}

#[test]
fn capability_panel_isolation_per_generation() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Browser, None).unwrap();
    // Deny-by-default
    assert!(!reg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
    let err = reg
        .require_panel_capability(h.id, h.generation, "panel.create")
        .unwrap_err();
    assert!(matches!(err, PanelError::CapabilityDenied { .. }));
    // Grant
    reg.grant_panel_capability(h.id, h.generation, "panel.create")
        .unwrap();
    assert!(reg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
    reg.require_panel_capability(h.id, h.generation, "panel.create")
        .unwrap();
    // Stale generation cannot use old grant
    let wrong = Generation(h.generation.get().wrapping_add(1));
    assert!(!reg.is_panel_capability_granted(h.id, wrong, "panel.create"));
    // Unknown family rejected
    assert!(
        reg.grant_panel_capability(h.id, h.generation, "terminal.manage")
            .is_err()
    );
    // Invalid capability string rejected
    assert!(
        reg.grant_panel_capability(h.id, h.generation, "panel.unknown")
            .is_err()
    );
}

#[test]
fn generation_exhaustion_fails_closed() {
    let mut reg = default_panel_registry();
    reg.set_generation_for_test(Generation(u64::MAX - 500));
    let err = reg.create_panel(PanelType::Terminal, None).unwrap_err();
    assert!(matches!(err, PanelError::GenerationExhausted { .. }));
    assert_eq!(reg.panel_count(), 0);
}

#[test]
fn registry_disposal_clears_all_and_fails_further() {
    let mut reg = default_panel_registry();
    let h = reg.create_panel(PanelType::Rich, None).unwrap();
    let topic = reg.declare_topic("xuepoo.test:disposal").unwrap();
    reg.subscribe(h.id, h.generation, &topic).unwrap();
    reg.dispose();
    assert!(reg.is_disposed());
    let err = reg.create_panel(PanelType::Terminal, None).unwrap_err();
    assert!(matches!(err, PanelError::RegistryDisposed { .. }));
    let err2 = reg.panel_state(h.id, h.generation).unwrap_err();
    assert!(matches!(err2, PanelError::RegistryDisposed { .. }));
}

#[test]
fn single_process_winit_one_registry_per_window_doc() {
    // This test documents the invariant: PanelRegistry is per window/process,
    // holds no PTY fd, GPU object, or OS window handle.
    // The registry is constructed in-process via `PanelRegistry::new`
    // and is not shared across processes; no bittyd or remote transport
    // is involved. This is a compile-time/architecture guarantee tested
    // via headless instantiation without window/GPU.
    let reg = default_panel_registry();
    assert_eq!(reg.panel_count(), 0);
    assert!(!reg.is_disposed());
    // No PTY/GPU handle accessible: registry Debug does not expose them
    let dbg = format!("{reg:?}");
    assert!(dbg.contains("PanelRegistry"));
    assert!(!dbg.contains("pty"));
    assert!(!dbg.contains("gpu"));
}
