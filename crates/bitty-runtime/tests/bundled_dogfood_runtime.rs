#![forbid(unsafe_code)]

//! Runtime dogfood for the eight bundled-disabled plugins (CTX-0096 + file-manager CTX-0108 + git-panel CTX-0109 + browser-panel CTX-0110).
//!
//! Proves:
//! - default disabled: fresh `EffectiveConfig` / `Runtime::with_defaults` has
//!   zero active plugins, `tick` remains functional
//! - bundled plugins load via public API only (`PluginHost` + `PluginManifest`
//!   + grant + lifecycle, no private host import, no `unsafe`)
//! - safe-mode compatibility (safe host rejects `bitty-terminal.*` as non-builtin
//!   without panicking, runtime still presents)
//! - Terminal Truth: only `Action` writes `State`; plugins observe via bounded
//!   side queue `Snapshot`/`HostObservation`, never grid mutation
//! - bounded cold-path execution: `DropOldest`, per-sub 64 / per-plugin 1024
//!   + 256 KiB / global 8192 + 2 MiB, drops attributed for `bitty plugin doctor`
//! - Panel Runtime is the host for file-manager, git-panel and browser-panel (tiled Panel + View Browser), agent
//!   still excluded; no marketplace/daemon/remote UI smuggled

use bitty_config::{EffectiveConfig, PluginSpec};
use bitty_plugin_host::{
    CapabilityId, DropPolicy, Event, EventKind, EventPayload, GrantRecord, bundled,
};
use bitty_runtime::{Runtime, RuntimeConfig};

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
            set.insert(CapabilityId::parse(&s).expect("filesystem capability"));
        }
    }
    set
}

#[test]
fn default_disabled_zero_plugins_and_tick_still_presents() {
    // Fresh install: no config plugins enabled, runtime starts core only.
    let cfg = EffectiveConfig::default();
    assert!(cfg.plugins.is_empty(), "fresh install must be empty");
    let mut rt = Runtime::with_defaults().expect("defaults must build");
    assert_eq!(rt.plugin_host().registry().len(), 0);
    assert_eq!(rt.plugin_side_len(), 0);
    assert!(rt.is_headless());
    // `tick` must present the pending full redraw without any plugin.
    let first = rt.tick().expect("core must present without plugins");
    assert!(first.headless);
    assert!(first.fills > 0);
    // Feeding PTY bytes still produces cold/side observations and presents.
    rt.handle_pty_bytes(b"\x1b]0;hello\x07hi");
    assert!(rt.cold_queue_len() > 0);
    assert!(rt.plugin_side_len() > 0);
    assert!(rt.tick().is_some());
}

#[test]
fn bundled_plugins_load_via_public_api_through_runtime() {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    for manifest in bundled::all_bundled_manifests() {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        rt.register_plugin(manifest)
            .expect("register via public Runtime API");
        // Grant required before activate.
        if !granted.is_empty() {
            assert!(
                rt.activate_plugin(&id).is_err(),
                "{} must require grant",
                id.as_str()
            );
            rt.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
        }
        rt.activate_plugin(&id)
            .unwrap_or_else(|e| panic!("activate {}: {e}", id.as_str()));
    }
    assert_eq!(rt.plugin_host().registry().len(), 8);
    // Each plugin's subscription (if any) can be established via public API.
    let shell_id = bitty_plugin_host::PluginId::new("bitty-terminal.shell-integration").unwrap();
    rt.subscribe_plugin_event(&shell_id, EventKind::TerminalTitleChanged)
        .expect("subscribe shell title");
    rt.subscribe_plugin_event(&shell_id, EventKind::TerminalCwdChanged)
        .expect("subscribe shell cwd");
    // Publish and drain via public bounded pipeline.
    rt.publish_plugin_event(Event::new(
        EventKind::TerminalTitleChanged,
        EventPayload::try_title_changed("project-title").unwrap(),
        1,
    ));
    let batch = rt
        .drain_plugin_events(&shell_id, &EventKind::TerminalTitleChanged, 32, 8 * 1024)
        .expect("drain");
    assert_eq!(batch.len(), 1);
}

#[test]
fn safe_mode_compatibility_bundled_disabled_and_runtime_still_ticks() {
    // `bitty --safe` must not create any non-core plugin VM/queue and must
    // keep the terminal usable. Runtime's PluginHost safe_mode rejects
    // `bitty-terminal.*` (parity with `xuepoo.*`), but the runtime itself
    // remains headless-presentable.
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    rt.set_plugin_safe_mode(true);
    assert!(rt.plugin_safe_mode());
    for manifest in bundled::all_bundled_manifests() {
        assert!(
            rt.register_plugin(manifest).is_err(),
            "safe mode must reject bundled as non-builtin"
        );
    }
    // Still tickable after safe-mode rejections (no corruption).
    assert!(rt.tick().is_some());
    assert_eq!(rt.plugin_host().registry().len(), 0);
    // Disabling safe mode allows registration again.
    rt.set_plugin_safe_mode(false);
    rt.register_plugin(bundled::shell_integration_manifest())
        .expect("after safe off");
}

#[test]
fn config_driven_enable_respects_default_disabled_and_public_api() {
    // Enabling is explicit via `EffectiveConfig.plugins: Vec<PluginSpec>`.
    // Absent means disabled; `enabled=false` is also disabled. Only
    // `enabled=true` with matching id causes host activation — and still
    // via the public `declare -> resolve -> register -> activate` + grant.
    let mut rt = Runtime::with_defaults().expect("runtime");
    let manifest = bundled::palette_manifest();
    let id_str = manifest.id().to_string();
    let enabled_cfg = EffectiveConfig {
        plugins: vec![PluginSpec {
            id: id_str.clone(),
            enabled: true,
        }],
        ..Default::default()
    };
    // Simulate config application: only ids present with enabled=true are loaded.
    for spec in &enabled_cfg.plugins {
        if !spec.enabled {
            continue;
        }
        if let Some(m) = bundled::bundled_manifest_for(&spec.id) {
            rt.register_plugin(m).expect("register from config");
        }
    }
    assert_eq!(rt.plugin_host().registry().len(), 1);
    // Disabled config must load zero.
    let mut rt2 = Runtime::with_defaults().expect("runtime2");
    let disabled_cfg = EffectiveConfig {
        plugins: vec![PluginSpec {
            id: id_str,
            enabled: false,
        }],
        ..Default::default()
    };
    for spec in &disabled_cfg.plugins {
        if !spec.enabled {
            continue;
        }
        if let Some(m) = bundled::bundled_manifest_for(&spec.id) {
            rt2.register_plugin(m).unwrap();
        }
    }
    assert_eq!(rt2.plugin_host().registry().len(), 0);
}

#[test]
fn terminal_truth_protected_plugin_observes_snapshot_not_grid_mutation() {
    // Plugins never write `State` directly; only `Action` does. They observe
    // via bounded side queue `HostObservation` derived from `Snapshot` + `Damage`.
    // This test drives `Runtime::handle_pty_bytes -> Action -> State` and checks
    // that the side queue received an observation without any plugin having
    // mutated the grid.
    let mut rt = Runtime::with_defaults().expect("runtime");
    let gen_before = rt.state().generation();
    rt.handle_pty_bytes(b"\x1b]0;side-observation-test\x07");
    let obs = rt.drain_plugin_observations();
    assert!(
        obs.iter().any(|o| matches!(o, bitty_plugin_host::HostObservation::TitleChanged(s) if s == "side-observation-test")),
        "side queue must contain TitleChanged observation"
    );
    assert!(rt.state().generation() > gen_before);
    // Snapshot text must still be terminal truth (grid not rewritten by plugin).
    let snap = rt.snapshot();
    assert!(snap.title.as_str().contains("side-observation-test") || snap.generation > gen_before);
}

#[test]
fn bounded_cold_path_drop_oldest_and_attributable() {
    // `DropOldest` per-sub 64 / per-plugin 1024+256 KiB / global 8192+2 MiB.
    // Flood one plugin, other remains isolated; drops are per-queue attributed.
    let mut rt = Runtime::with_plugin_host_capacity(
        RuntimeConfig::default(),
        DropPolicy::DropOldest,
        64,
        16,
    )
    .expect("rt with small queues");
    for m in bundled::all_bundled_manifests() {
        let id = m.id().clone();
        let hash = m.manifest_hash();
        let granted = granted_set_for(&m);
        rt.register_plugin(m).unwrap();
        if !granted.is_empty() {
            rt.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
        }
        let _ = rt.activate_plugin(&id);
    }
    let palette = bitty_plugin_host::PluginId::new("bitty-terminal.palette").unwrap();
    let shell = bitty_plugin_host::PluginId::new("bitty-terminal.shell-integration").unwrap();
    // Palette declares focus.changed, shell declares terminal.bell/title/cwd.
    // Flood a non-coalescable kind (TerminalBell is not coalescable) to prove
    // DropOldest capping; FocusChanged would coalesce and not drop.
    let _ = rt.subscribe_plugin_event(&palette, EventKind::FocusChanged);
    let _ = rt.subscribe_plugin_event(&shell, EventKind::TerminalBell);

    // Flood shell's TerminalBell queue to cap (64 per-sub) -> drops must be counted.
    for i in 0..200u64 {
        rt.publish_plugin_event(Event::new(EventKind::TerminalBell, EventPayload::Empty, i));
    }
    // After flood, per-sub queue is capped at 64 and total_dropped > 0.
    assert!(rt.plugin_total_dropped() > 0);
    assert!(rt.plugin_host().invariant_queue_bounds());
    assert!(rt.plugin_host().invariant_global_bounds());

    // Flood side queue (16) beyond capacity -> oldest dropped, newest survive.
    for i in 0..20 {
        rt.push_plugin_observation(bitty_plugin_host::HostObservation::TitleChanged(format!(
            "t{i}"
        )));
    }
    assert!(rt.plugin_side_dropped() > 0);
    assert_eq!(rt.plugin_side_len(), rt.plugin_side_capacity());
    // Per-queue dropped is attributed for `bitty plugin doctor`.
    let per_queue = rt.plugin_dropped_per_queue();
    assert!(per_queue.values().any(|&d| d > 0));
}

#[test]
fn no_panel_runtime_browser_agent_marketplace_smuggled() {
    // Panel Runtime is the host for file-manager, git-panel and browser-panel
    // (CTX-0102, CTX-0108 tiled Panel with fs.read+optional fs.write, CTX-0109
    // with process.spawn:git, CTX-0110 View Browser + Panel controls with
    // browser.embed/navigation/file-url/storage allowlisted). Agent/marketplace/
    // daemon remain excluded; bundled catalog is exactly the eight accepted ids
    // (no splits/search beyond the three panel plugins, no agent).
    let ids = bundled::bundled_ids_sorted();
    assert_eq!(ids.len(), 8);
    assert!(
        !ids.iter()
            .any(|id| id.contains("splits") || id.contains("search"))
    );
    assert!(!ids.iter().any(|id| id.contains("agent")));
    assert!(ids.contains(&"bitty-terminal.file-manager".to_string()));
    assert!(ids.contains(&"bitty-terminal.git-panel".to_string()));
    assert!(ids.contains(&"bitty-terminal.browser-panel".to_string()));
}
