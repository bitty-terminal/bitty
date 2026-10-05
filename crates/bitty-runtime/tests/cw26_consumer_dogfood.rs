#![forbid(unsafe_code)]
//! CW-26 workspaceline consumer dogfood (CTX-0646, issue #1004).
//!
//! Batch verification for the panel/chrome consumer wiring slice. The
//! bundled palette and statusline Rust consumers were removed after the
//! OQ-053 split (CTX-0922; they now ship as the `palette` and `statusline`
//! Lua plugins), so the remaining first-party consumer is
//! workspace/workspaceline (`bitty-terminal.workspace`, claim
//! `workspaceline`), which still exercises the public `PanelRegistry` path
//! and stays bundled-disabled by default.
//!
//! Proves in one place, via public paths only:
//! - default disabled: fresh `EffectiveConfig` has zero plugins, a fresh
//!   `PanelRegistry` has zero panels, `Runtime::tick` still presents
//! - workspace consumer: `workspace_manifest` carries the canonical
//!   `workspaceline` claim through the public PluginHost path
//!   (`declare -> resolve -> register -> GrantRecord -> activate`)
//! - workspace queries: `workspace_names`/`active_workspace_index` reflect
//!   workspace lifecycle, plus `create_workspace_panel` via the public path
//!   (CTX-0979: Core draws no display; presentation belongs to the bar
//!   plugin)
//! - bounded `DropOldest` event bus shared across several panels
//!   (`64` per-sub / `8192` global, `8 KiB` payload, `32` / `8 KiB` batch)
//! - safe-mode parity: safe `Runtime` rejects `bitty-terminal.*` without
//!   panic while still ticking
//!
//! Enablement decision (recorded): keep bundled-disabled. Activation stays
//! explicit user opt-in via `EffectiveConfig.plugins`; no auto-claim.
//! `forbid(unsafe)`.

use bitty_plugin_host::{CapabilityId, DropPolicy, GrantRecord, PluginHost, bundled};
use bitty_runtime::{
    Runtime,
    registry::{BoundedPayload, PanelRegistry, PanelRegistryConfig, WorkspaceId},
    workspace::{WorkspaceIntegration, create_workspace_panel},
};
use bitty_ui::{View, ViewId, panel::PanelType};

fn granted_set_for(
    manifest: &bitty_plugin_host::PluginManifest,
) -> std::collections::BTreeSet<CapabilityId> {
    let mut set = manifest.capabilities.ids.clone();
    for req in &manifest.capabilities.filesystem {
        for pat in &req.paths {
            let s = match req.access {
                bitty_plugin_host::FsAccess::Read => format!("fs.read:{pat}"),
                bitty_plugin_host::FsAccess::Write => format!("fs.write:{pat}"),
            };
            set.insert(CapabilityId::parse(&s).unwrap());
        }
    }
    set
}

#[test]
fn default_disabled_zero_consumers_and_tick_still_presents() {
    let cfg = bitty_config::EffectiveConfig::default();
    assert!(cfg.plugins.is_empty(), "fresh install must be empty");
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    assert_eq!(rt.plugin_host().registry().len(), 0);
    assert_eq!(rt.plugin_side_len(), 0);
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg defaults");
    assert_eq!(preg.panel_count(), 0);
    assert!(rt.tick().is_some());
}

#[test]
fn workspace_consumer_workspaceline_claim_via_public_host_path() {
    let manifest = bundled::workspace_manifest();
    assert!(
        manifest
            .lazy
            .claims
            .contains(&bundled::WORKSPACELINE_CLAIM.to_string())
    );
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    assert!(granted.contains(&CapabilityId::parse("ui.rich").unwrap()));

    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.declare(manifest).expect("declare");
    host.resolve(&id).expect("resolve");
    host.register(&id).expect("register");
    assert!(host.activate(&id).is_err(), "must require grant");
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).expect("activate after grant");
    assert_eq!(
        host.registry().get(&id).unwrap().state,
        bitty_plugin_host::PluginState::Activated
    );
}

#[test]
fn workspace_queries_reflect_workspace_lifecycle() {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    assert_eq!(rt.workspace_names(), vec![String::from("ws1")]);
    assert_eq!(rt.active_workspace_index(), 0);
    rt.workspace_new().expect("second workspace");
    assert_eq!(
        rt.workspace_names(),
        vec![String::from("ws1"), String::from("ws2")]
    );
    assert_eq!(rt.active_workspace_index(), 1);
}

#[test]
fn workspace_panel_via_public_panel_path_stack_semantics() {
    // Workspaces reuse LayoutNode::stack (no new primitive).
    let views = vec![
        View::new(ViewId::new(1), 80, 24),
        View::new(ViewId::new(2), 80, 24),
    ];
    let stack = WorkspaceIntegration::stack_for_workspace(views);
    assert!(WorkspaceIntegration::is_stack(&stack));
    assert_eq!(WorkspaceIntegration::workspace_count(&stack), 2);

    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let id = create_workspace_panel(&mut reg, ws, ViewId::new(1)).expect("workspace panel");
    assert_eq!(reg.panel_count(), 1);
    let _ = id.get();
}

#[test]
fn shared_event_bus_bounded_drop_oldest_across_consumers() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(7);
    let mut panels = Vec::new();
    for view in [11, 12, 13] {
        let h = reg.create_panel(PanelType::Helper, Some(ws)).unwrap();
        reg.mount_panel(h.id, h.generation, ViewId::new(view))
            .unwrap();
        panels.push(h);
    }
    let topic = reg.declare_topic("xuepoo.dogfood:status-update").unwrap();
    for h in &panels {
        reg.subscribe(h.id, h.generation, &topic).unwrap();
    }
    for i in 0..80 {
        reg.publish(
            &topic,
            BoundedPayload::try_new(format!("status{i}")).unwrap(),
        )
        .unwrap();
    }
    for h in &panels {
        assert!(reg.bus_events_for_panel(h.id) <= 64);
    }
    assert!(reg.bus_total_events() <= 8192);
    let large = "a".repeat(9 * 1024);
    assert!(BoundedPayload::try_new(large).is_err());
    let batch = reg.drain_batch(panels[0].id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    assert_eq!(batch[0].payload.as_str(), "status16");
}

#[test]
fn safe_mode_rejects_bundled_and_runtime_still_ticks() {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    rt.set_plugin_safe_mode(true);
    assert!(
        rt.register_plugin(bundled::workspace_manifest()).is_err(),
        "safe mode must reject bundled as non-builtin"
    );
    assert!(rt.tick().is_some());
    assert_eq!(rt.plugin_host().registry().len(), 0);
}
