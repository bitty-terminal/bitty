#![forbid(unsafe_code)]
//! CW-26 workspaceline/statusline consumer dogfood (CTX-0646, issue #1004).
//!
//! Batch verification for the panel/chrome consumer wiring slice (with
//! CW-25 palette entry, CTX-0647): the three first-party chrome consumers —
//! palette (`bitty-terminal.palette`), statusline
//! (`bitty-terminal.statusline`), and workspace/workspaceline
//! (`bitty-terminal.workspace`, claim `workspaceline`) — already exercise
//! the public `PanelRegistry` path and stay bundled-disabled by default.
//!
//! Proves in one place, via public paths only:
//! - default disabled: fresh `EffectiveConfig` has zero plugins, a fresh
//!   `PanelRegistry` has zero panels, `Runtime::tick` still presents
//! - workspace consumer: `workspace_manifest` carries the canonical
//!   `workspaceline` claim through the public PluginHost path
//!   (`declare -> resolve -> register -> GrantRecord -> activate`)
//! - statusline consumer: `StatuslineIntegration::render` observes committed
//!   `State` only, plus `create_statusline_panel` via the public
//!   PanelRegistry path
//! - palette consumer: `PaletteIntegration::filter_entries` plus
//!   `create_palette_panel` / `create_palette_overlay` /
//!   `register_palette_command` via the public PanelRegistry path
//! - workspaceline presentation: `Runtime::workspaceline_text` reflects
//!   workspace lifecycle, plus `create_workspace_panel` via the public path
//! - bounded `DropOldest` event bus shared across the three consumers
//!   (`64` per-sub / `8192` global, `8 KiB` payload, `32` / `8 KiB` batch)
//! - safe-mode parity: safe `Runtime` rejects `bitty-terminal.*` without
//!   panic while still ticking
//!
//! Enablement decision (recorded): keep bundled-disabled. All three
//! consumers verify green through the generic Panel Runtime path with no
//! private channel, but none is enabled by default — activation stays
//! explicit user opt-in via `EffectiveConfig.plugins`. No default keybinding
//! (CW-25 `toggle_palette` parses but ships unbound) and no auto-claim.
//! `forbid(unsafe)`.

use bitty_plugin_host::{CapabilityId, DropPolicy, GrantRecord, PluginHost, bundled};
use bitty_runtime::{
    Runtime,
    palette::{
        PaletteIntegration, create_palette_overlay, create_palette_panel, register_palette_command,
    },
    registry::{BoundedPayload, PanelRegistry, PanelRegistryConfig, WorkspaceId},
    statusline::{StatuslineIntegration, create_statusline_panel},
    workspace::{WorkspaceIntegration, create_workspace_panel},
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{Rect as UiRect, View, ViewId, panel::PanelType};
use bitty_vt::BoundedString;

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
fn workspaceline_text_reflects_workspace_lifecycle() {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    assert_eq!(rt.workspaceline_text(), "1:ws1* (1)");
    rt.workspace_new().expect("second workspace");
    assert_eq!(rt.workspaceline_text(), "1:ws1 2:ws2* (2)");
}

#[test]
fn statusline_consumer_observes_committed_state_via_public_panel_path() {
    // Observation-only render from committed State (no grid mutation).
    let mut state = State::new();
    assert_eq!(StatuslineIntegration::render(&state), "");
    state.apply(&TerminalAction::OscCwd {
        url: BoundedString::new("file:///home/user/projects/foo"),
    });
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("dogfood"),
    });
    let rendered = StatuslineIntegration::render(&state);
    assert!(rendered.contains("cwd:file:///home/user/projects/foo"));
    assert!(rendered.contains("title:dogfood"));
    assert!(StatuslineIntegration::is_render_bounded(&rendered));

    // Panel via the public PanelRegistry path only (helper create).
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let id = create_statusline_panel(&mut reg, ws, ViewId::new(1)).expect("statusline panel");
    assert_eq!(reg.panel_count(), 1);
    let _ = id.get();
    // Second panel on the same view must fail-closed (AlreadyMounted).
    let h2 = reg
        .create_panel(PanelType::Helper, Some(ws))
        .expect("second panel");
    assert!(
        reg.mount_panel(h2.id, h2.generation, ViewId::new(1))
            .is_err()
    );
}

#[test]
fn palette_consumer_overlay_and_command_via_public_panel_path() {
    // Filter helper over the command list (bounded, case-insensitive).
    let entries = vec![
        "bitty-terminal.palette:toggle".to_string(),
        "bitty-terminal.workspace:new".to_string(),
    ];
    let filtered = PaletteIntegration::filter_entries(&entries, "palette");
    assert_eq!(filtered, vec!["bitty-terminal.palette:toggle".to_string()]);

    // Panel + overlay + command via the public PanelRegistry path only.
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let palette_id = create_palette_panel(&mut reg, ws, ViewId::new(1)).expect("palette panel");
    assert_eq!(reg.panel_count(), 1);
    let overlay = create_palette_overlay(
        &mut reg,
        UiRect::new(30, 7, 20, 10),
        "palette",
        Some("toggle".to_string()),
    )
    .expect("palette overlay");
    assert!(overlay > 0);
    // Command registry needs the panel generation: use a directly created
    // handle (same public path the helper wraps).
    let h = reg.create_panel(PanelType::Helper, Some(ws)).unwrap();
    reg.mount_panel(h.id, h.generation, ViewId::new(2)).unwrap();
    let qualified = register_palette_command(
        &mut reg,
        h.id,
        h.generation,
        "bitty-terminal.palette:toggle",
    )
    .expect("register palette toggle");
    assert_eq!(qualified.as_str(), "bitty-terminal.palette:toggle");
    let _ = palette_id.get();
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
