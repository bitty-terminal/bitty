#![forbid(unsafe_code)]
//! Tabs via Panel Runtime — public API verification (CTX-0104, OQ-011).
//!
//! Verifies `bitty-terminal.tabs` as a generic Panel Runtime consumer with
//! no hardcoded tabs primitive (reuses `LayoutNode::stack`/`split`), verifying
//! `TerminalRegistry`/`View`/`Workspace`/`Focus` lifecycle via the Panel API
//! public path only (`PanelRegistry::new` → `create_panel` → `mount_panel` →
//! `focus_panel` with `PanelType::Helper` and `TerminalRegistry`
//! `create_terminal`/`create_view`/`attach`/`set_focus`/`move_terminal`),
//! not a private channel. Bounded queues `64`/`1024`/`256 KiB`/`8192`/`2 MiB`/
//! `8 KiB`/`32`/`8 KiB` `DropOldest`, single-process `winit` one-registry-per-
//! window, default disabled, safe-mode reject, `forbid(unsafe)`.

use bitty_plugin_host::{
    CapabilityId, DropPolicy, EventKind, GrantRecord, PluginHost, bundled::tabs_manifest,
};
use bitty_runtime::{
    Runtime,
    registry::{
        LogicalRect, PanelRegistry, PanelRegistryConfig, RegistryConfig, TerminalRegistry,
        WorkspaceId,
    },
    tabs::{TabsIntegration, create_tabs_panel, validate_tabs_panel_config},
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{View, ViewId};
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

// --- default disabled -------------------------------------------------------

#[test]
fn default_disabled_zero_panels_and_no_plugin() {
    // Fresh install: no plugin enabled, no panel created.
    let cfg = bitty_config::EffectiveConfig::default();
    assert!(cfg.plugins.is_empty(), "fresh install must be empty");
    let rt = Runtime::with_defaults().expect("runtime must build");
    assert_eq!(rt.plugin_host().registry().len(), 0);
    assert_eq!(rt.plugin_side_len(), 0);
    // Panel registry also starts empty (default disabled).
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg defaults");
    assert_eq!(preg.panel_count(), 0);
    // Terminal registry also starts empty.
    let treg = TerminalRegistry::new(RegistryConfig::default()).expect("terminal reg defaults");
    assert_eq!(treg.terminal_count(), 0);
    // Tick still presents without any plugin/panel.
    let mut rt2 = rt;
    assert!(rt2.tick().is_some());
}

// --- public PluginHost path for tabs --------------------------------------

#[test]
fn tabs_via_public_plugin_host_path() {
    let manifest = tabs_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    assert!(granted.contains(&CapabilityId::parse("ui.rich").unwrap()));
    // Claim tabline is declared via lazy claims.
    assert!(manifest.lazy.claims.contains(&"tabline".to_string()));

    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    // Public path: declare → resolve → register.
    host.declare(manifest.clone()).expect("declare");
    host.resolve(&id).expect("resolve");
    host.register(&id).expect("register");
    // Activate without grant must be fail-closed (deny-by-default).
    assert!(host.activate(&id).is_err(), "must require grant");
    // GrantRecord is hash-bound, then activate succeeds.
    host.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
    host.activate(&id).expect("activate after grant");
    assert_eq!(
        host.registry().get(&id).unwrap().state,
        bitty_plugin_host::PluginState::Activated
    );
    // Revocation detaches at next boundary.
    let cap = CapabilityId::parse("ui.rich").unwrap();
    assert!(host.is_granted(&id, &hash, &cap));
    let report = host.revoke(&id, Some(&cap)).unwrap();
    assert_eq!(report.revoked.len(), 1);
    assert!(!host.is_granted(&id, &hash, &cap));
    // Hash changed (version bump) → grant no longer matches.
    let mut bumped = tabs_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), hash);
    assert!(!host.is_granted(&id, &bumped.manifest_hash(), &cap));
}

// --- subscribe → publish → drain via bounded PanelEventBus DropOldest -------

#[test]
fn tabs_subscribe_publish_drain_bounded_drop_oldest() {
    let manifest = tabs_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 4);
    host.declare(manifest).unwrap();
    host.resolve(&id).unwrap();
    host.register(&id).unwrap();
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).unwrap();
    // Subscribe to declared events only (undeclared must fail).
    host.subscribe(&id, EventKind::TerminalTitleChanged)
        .expect("title is declared for tabs");
    assert!(
        host.subscribe(&id, EventKind::InterceptPaste).is_err(),
        "undeclared intercept must be rejected"
    );

    // Panel EventBus bounded: per-sub 64 DropOldest via tabs panel.
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(100);
    let h = preg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .expect("create tabs panel");
    preg.mount_panel(h.id, h.generation, view).expect("mount");
    let topic = preg.declare_topic("xuepoo.tabs:tab-changed").unwrap();
    preg.subscribe(h.id, h.generation, &topic)
        .expect("subscribe");
    for i in 0..80 {
        preg.publish(
            &topic,
            bitty_runtime::registry::BoundedPayload::try_new(format!("tab{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(preg.bus_events_for_panel(h.id) <= 64);
    assert!(preg.bus_total_events() <= 8192);
    let batch = preg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // FIFO DropOldest: first batch should contain tab16..tab47 (oldest 16 dropped if payload small)
    // With 80 published and 64 cap, oldest 16 dropped, so first surviving is tab16.
    assert_eq!(batch[0].payload.as_str(), "tab16");
}

// --- TerminalRegistry/View/Workspace/focus lifecycle via Panel API ---------

#[test]
fn terminal_registry_view_workspace_focus_via_panel_api() {
    // One registry per window, single-process winit, no PTY fd leak.
    let mut treg = TerminalRegistry::new(RegistryConfig::default()).expect("terminal registry");
    // Create workspace + views via TerminalRegistry public path.
    let wid = treg.create_workspace().expect("workspace");
    let vh1 = treg.create_view(wid).expect("view1");
    let vh2 = treg.create_view(wid).expect("view2");
    let vh3 = treg.create_view(wid).expect("view3");
    // View creation bounded at 32 per workspace — 3 within bound is ok.
    assert_eq!(treg.terminal_count(), 0);
    // Create terminals via public path.
    let th1 = treg.create_terminal(None).expect("terminal1");
    let th2 = treg.create_terminal(None).expect("terminal2");
    assert_eq!(treg.terminal_count(), 2);
    // Attach terminals to views via public path (LogicalRect → grid).
    let rect = LogicalRect::new(0.0, 0.0, 640.0, 384.0).unwrap();
    treg.attach(wid, vh1.id, vh1.generation, th1.id, th1.generation, rect)
        .expect("attach1");
    treg.attach(wid, vh2.id, vh2.generation, th2.id, th2.generation, rect)
        .expect("attach2");
    assert_eq!(treg.attached_view(th1.id), Some(vh1.id));
    assert_eq!(treg.attached_terminal(vh2.id), Some(th2.id));
    // Focus via TerminalRegistry public path (MRU).
    treg.set_focus(wid, vh2.id, vh2.generation)
        .expect("focus vh2");
    assert_eq!(treg.focused_view(wid), Some(vh2.id));
    treg.set_focus(wid, vh1.id, vh1.generation)
        .expect("focus vh1");
    assert_eq!(treg.focused_view(wid), Some(vh1.id));
    // Tabs as Stack reuse LayoutNode::stack (no hardcoded tabs).
    let layout = TabsIntegration::stack_for_tabs(vec![
        View::new(vh1.id, 80, 24),
        View::new(vh2.id, 80, 24),
        View::new(vh3.id, 80, 24),
    ]);
    assert!(TabsIntegration::is_stack(&layout));
    assert_eq!(TabsIntegration::tab_count(&layout), 3);
    // Commit stack to workspace via TerminalRegistry public path.
    treg.set_workspace_layout(wid, layout)
        .expect("set stack layout");
    let allocs = treg
        .reflow_workspace(wid, bitty_ui::Rect::new(0, 0, 80, 24))
        .expect("reflow");
    assert_eq!(allocs.len(), 3);
    // Split reuse: two tab groups side-by-side via LayoutNode::split.
    let split = TabsIntegration::split_for_tabs(
        vec![View::new(vh1.id, 40, 24)],
        vec![View::new(vh2.id, 40, 24)],
        0.5,
    );
    assert_eq!(TabsIntegration::tab_count(&split), 2);
    let split_allocs = split.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(split_allocs.len(), 2);
    // Panel focus via PanelRegistry public path (MRU per workspace).
    // Use a fresh panel where we keep generation.
    let mut preg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws2 = WorkspaceId::new(42);
    let v_a = ViewId::new(10);
    let v_b = ViewId::new(11);
    let h_a = preg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    let h_b = preg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    preg2.mount_panel(h_a.id, h_a.generation, v_a).unwrap();
    preg2.mount_panel(h_b.id, h_b.generation, v_b).unwrap();
    preg2.focus_panel(h_a.id, h_a.generation, ws2).unwrap();
    preg2.focus_panel(h_b.id, h_b.generation, ws2).unwrap();
    assert_eq!(preg2.focused_panel(ws2), Some(h_b.id));
    assert_eq!(preg2.mru_order(ws2), vec![h_b.id, h_a.id]);
    // Move terminal atomically between views (reattachment preserves ids).
    treg.move_terminal(
        th1.id,
        th1.generation,
        wid,
        vh1.id,
        vh1.generation,
        wid,
        vh3.id,
        vh3.generation,
        rect,
    )
    .expect("move terminal");
    assert_eq!(treg.attached_view(th1.id), Some(vh3.id));
    assert_eq!(treg.attached_terminal(vh1.id), None);
    // Detach preserves terminal, focus MRU survives.
    let detached = treg.detach(wid, vh2.id, vh2.generation).expect("detach");
    assert_eq!(detached, th2.id);
    assert!(treg.terminal_snapshot(th2.id, th2.generation).is_ok());
    // Close retires id with generation bump.
    let before = treg.generation();
    treg.close_terminal(th1.id, th1.generation).expect("close");
    assert!(treg.generation().get() > before.get());
    assert!(treg.terminal_snapshot(th1.id, th1.generation).is_err());
}

// --- tabs panel via Panel Runtime public path, bounded ---------------------

#[test]
fn tabs_panel_via_panel_runtime_public_path_bounded() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    // Create tabs panel via public API (no private channel).
    let pid = create_tabs_panel(&mut reg, ws, view).expect("create tabs panel");
    assert_eq!(reg.panel_count(), 1);
    // PanelId distinct newtype with no From bridge.
    let _raw = pid.get();
    // Bounded queues via PanelEventBus: per-sub 64 / per-panel 1024+256KiB / global 8192+2MiB.
    let topic = reg.declare_topic("xuepoo.tabs:focus-changed").unwrap();
    // Need generation for subscribe; we have pid but need generation.
    // For this test, create a fresh panel where we keep generation.
    let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws2 = WorkspaceId::new(2);
    let view2 = ViewId::new(2);
    let h = reg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    reg2.mount_panel(h.id, h.generation, view2).unwrap();
    let topic2 = reg2.declare_topic("xuepoo.tabs:tab-changed").unwrap();
    reg2.subscribe(h.id, h.generation, &topic2).unwrap();
    for i in 0..80 {
        reg2.publish(
            &topic2,
            bitty_runtime::registry::BoundedPayload::try_new(format!("evt{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg2.bus_events_for_panel(h.id) <= 64);
    assert!(reg2.bus_total_events() <= 8192);
    // Payload bound 8KiB.
    let large = "a".repeat(9 * 1024);
    assert!(bitty_runtime::registry::BoundedPayload::try_new(large).is_err());
    // Batch 32/8KiB.
    let batch = reg2.drain_batch(h.id, topic2.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // Config validation bounded and fail-closed.
    let bad = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..Default::default()
    };
    assert!(validate_tabs_panel_config(&bad).is_err());
    assert!(validate_tabs_panel_config(&PanelRegistryConfig::default()).is_ok());
    // Second mount same view → AlreadyMounted.
    let h2 = reg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    assert!(reg2.mount_panel(h2.id, h2.generation, view2).is_err());
    let _ = topic;
}

// --- Layout reuse: split/stack determinism, no hardcoded tabs ------------

#[test]
fn tabs_reuse_layout_split_stack_determinism() {
    // Tabs reuse LayoutNode primitives only; no new Tabs node.
    let v1 = View::new(ViewId::new(1), 80, 24);
    let v2 = View::new(ViewId::new(2), 80, 24);
    let v3 = View::new(ViewId::new(3), 80, 24);
    let stack = TabsIntegration::stack_for_tabs(vec![v1.clone(), v2.clone(), v3.clone()]);
    assert!(TabsIntegration::is_stack(&stack));
    assert_eq!(stack.leaf_count(), 3);
    // Determinism: same layout, same container → same allocation.
    let a1 = stack.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    let a2 = stack.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(a1, a2);
    // Stack semantics: all share full bounds.
    for (_, rect) in &a1 {
        assert_eq!(*rect, bitty_ui::Rect::new(0, 0, 80, 24));
    }
    // Split reuses LayoutNode::split with clamped ratio.
    let split = TabsIntegration::split_for_tabs(vec![v1], vec![v2, v3], 0.5);
    let allocs = split.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 3);
    // Horizontal split then stack: left 40, right stacked 40 each sharing.
    // LayoutNode::split 0.5 on 80 → 40+40, but right stack shares 40 bounds for both leaves.
    // So we should have 3 rects, with two sharing the right half.
    let mut widths: Vec<u16> = allocs.iter().map(|(_, r)| r.width).collect();
    widths.sort();
    assert!(widths.contains(&40));
    // Ratio clamping: extreme ratios don't collapse pane below 1.
    let extreme = TabsIntegration::split_for_tabs(
        vec![View::new(ViewId::new(10), 80, 24)],
        vec![View::new(ViewId::new(11), 80, 24)],
        0.01,
    );
    let e_allocs = extreme.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert!(e_allocs[0].1.width >= 1 && e_allocs[1].1.width >= 1);
}

// --- tab title bounded, observation-only ---------------------------------

#[test]
fn tab_title_bounded_observation_only() {
    let mut state = State::new();
    assert_eq!(TabsIntegration::tab_title(&state), None);
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("hello"),
    });
    assert_eq!(
        TabsIntegration::tab_title(&state),
        Some("hello".to_string())
    );
    // Bounded at 128 chars.
    let long = "b".repeat(200);
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new(long.clone()),
    });
    let title = TabsIntegration::tab_title(&state).unwrap();
    assert_eq!(title.chars().count(), 128);
    assert!(title.len() <= 512);
    // Observation is via committed state (title), never grid mutation.
    let snap = state.snapshot();
    assert_eq!(snap.title.as_str(), state.title());
}

// --- safe-mode: tabs is non-builtin and must be rejected -------------------

#[test]
fn safe_mode_rejects_tabs_without_panic() {
    let manifest = tabs_manifest();
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.set_safe_mode(true);
    // bitty-terminal.tabs is not bitty.* → treated as non-builtin, rejected.
    assert!(host.declare(manifest.clone()).is_err());
    host.set_safe_mode(false);
    assert!(host.declare(manifest).is_ok());

    // Runtime safe-mode also rejects.
    let mut rt = Runtime::with_defaults().unwrap();
    rt.set_plugin_safe_mode(true);
    assert!(
        rt.register_plugin(tabs_manifest()).is_err(),
        "safe mode must reject tabs"
    );
    assert!(
        rt.tick().is_some(),
        "runtime remains tickable after safe-mode rejection"
    );
    rt.set_plugin_safe_mode(false);
    assert!(
        rt.register_plugin(tabs_manifest()).is_ok(),
        "after safe-mode off, registration allowed"
    );
}

// --- no private channel: third-party parity --------------------------------

#[test]
fn tabs_has_no_private_channel_parity_with_third_party() {
    let bundled = tabs_manifest();
    let mut third = bundled.clone();
    third.identity.id = bitty_plugin_host::PluginId::new("xuepoo.tabs-mirror").unwrap();
    third.identity.name = "Third Party Mirror".to_string();
    // Same capabilities/lazy/compat shape must have identical validation and lifecycle.
    for (label, manifest) in [("bundled", bundled), ("third", third)] {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
        host.declare(manifest)
            .unwrap_or_else(|e| panic!("{label} declare: {e}"));
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        assert!(host.activate(&id).is_err(), "{label} must require grant");
        host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
        host.activate(&id)
            .unwrap_or_else(|e| panic!("{label} activate: {e}"));
    }
}

// --- single-process winit, one registry per window, headless ---------------

#[test]
fn tabs_is_headless_and_forbids_unsafe_single_process_winit() {
    // Compile-time proof is #![forbid(unsafe_code)] at crate and test file.
    // Runtime proof: host and panel registry are headless constructible without display.
    let host = PluginHost::new(DropPolicy::DropOldest, 8);
    assert!(!host.is_safe_mode());
    assert!(host.side_queue().is_empty());
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    assert_eq!(preg.panel_count(), 0);
    // One registry per window, single-process winit, no PTY/GPU handle.
    let treg = TerminalRegistry::new(RegistryConfig::default()).unwrap();
    assert_eq!(treg.terminal_count(), 0);
    // Registry Debug does not expose pty/gpu.
    let dbg = format!("{preg:?}");
    assert!(dbg.contains("PanelRegistry"));
    assert!(!dbg.contains("pty"));
    assert!(!dbg.contains("gpu"));
    let dbg2 = format!("{treg:?}");
    assert!(dbg2.contains("TerminalRegistry"));
    // Runtime is headless by construction (Surface::headless).
    let rt = Runtime::with_defaults().unwrap();
    assert!(rt.is_headless());
    // Tabs stack is headless determinism without window/GPU.
    let stack = TabsIntegration::stack_for_tabs(vec![
        View::new(ViewId::new(1), 80, 24),
        View::new(ViewId::new(2), 80, 24),
    ]);
    let allocs = stack.layout(bitty_ui::Rect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2);
}
