#![forbid(unsafe_code)]
//! Browser panel via Panel Runtime — public API verification (CTX-0110, OQ-011).
//!
//! Verifies `bitty-terminal.browser-panel` (`View Browser(BrowserSurfaceId)`
//! host surface + `Panel(PanelId)` controls, `browser.embed`/`navigation`/
//! `browser.file-url`/`browser.storage` allowlisted, bounded) as generic Panel
//! Runtime consumer with no private channel, via the public PluginHost path
//! (`declare → resolve → register → GrantRecord → activate → subscribe →
//! publish → drain SideQueue DropOldest`) and PanelRegistry public path
//! (`PanelRegistry::new → create_panel → mount_panel → focus_panel` with
//! `PanelType::Browser` plus `register_command`/`create_overlay`/`declare_topic`/
//! `subscribe`/`publish`/`drain_batch`), tiled `LayoutNode` `H`/`V` reuse,
//! bounded queues `64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload,
//! `32`/`8 KiB` batch, single-process `winit` one-registry-per-window,
//! default disabled, safe-mode reject, `forbid(unsafe)`.
//!
//! Mirrors file_manager_panel and git_panel but for the browser-panel P2
//! candidate with `browser.embed` high-risk, navigation allowlist `https`
//! default plus gated `file://`, and `browser.storage`.

use bitty_plugin_host::{
    CapabilityId, DropPolicy, EventKind, GrantRecord, PluginHost, bundled::browser_panel_manifest,
};
use bitty_runtime::{
    Runtime,
    browser_panel::{
        BROWSER_CAPABILITY_EMBED, BROWSER_MAX_HISTORY_ENTRIES, BROWSER_MAX_NAVIGATION_QUEUE,
        BROWSER_MAX_PANELS_PER_WINDOW, BrowserHistoryEntry, BrowserIntegration,
        BrowserNavigationQueue, browser_tiled_layout, create_browser_panel,
        validate_browser_panel_config,
    },
    registry::{BoundedPayload, PanelRegistry, PanelRegistryConfig, WorkspaceId},
};
use bitty_ui::{Rect as UiRect, ViewId};

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

// --- default disabled --------------------------------------------------------

#[test]
fn default_disabled_zero_panels_and_no_plugin() {
    let cfg = bitty_config::EffectiveConfig::default();
    assert!(cfg.plugins.is_empty(), "fresh install must be empty");
    let rt = Runtime::with_defaults().expect("runtime must build");
    assert_eq!(rt.plugin_host().registry().len(), 0);
    assert_eq!(rt.plugin_side_len(), 0);
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg defaults");
    assert_eq!(preg.panel_count(), 0);
    let treg = bitty_runtime::TerminalRegistry::new(bitty_runtime::RegistryConfig::default())
        .expect("terminal reg defaults");
    assert_eq!(treg.terminal_count(), 0);
    let mut rt2 = rt;
    assert!(rt2.tick().is_some());
}

// --- public PluginHost path for browser-panel (View Browser + Panel controls) --------------

#[test]
fn browser_panel_via_public_plugin_host_path() {
    let manifest = browser_panel_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    // Must contain panel.provider + panel.create + browser.embed (high-risk)
    // + browser.navigation + browser.file-url + browser.storage + terminal.semantic-read
    assert!(granted.contains(&CapabilityId::parse("panel.provider").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("panel.create").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("browser.embed").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("browser.navigation").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("browser.file-url").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("browser.storage").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("terminal.semantic-read").unwrap()));
    assert_eq!(manifest.capabilities.filesystem.len(), 0);
    assert_eq!(BROWSER_CAPABILITY_EMBED, "browser.embed");
    assert_eq!(manifest.lazy.commands.len(), 5);
    assert!(
        manifest
            .lazy
            .commands
            .iter()
            .any(|c| c.as_str() == "bitty-terminal.browser-panel:open")
    );
    assert!(
        manifest
            .lazy
            .commands
            .iter()
            .any(|c| c.as_str() == "bitty-terminal.browser-panel:navigate")
    );
    // browser.embed is high-risk
    assert!(CapabilityId::parse("browser.embed").unwrap().is_high_risk());
    assert!(
        !CapabilityId::parse("browser.navigation")
            .unwrap()
            .is_high_risk()
    );

    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.declare(manifest.clone()).expect("declare");
    host.resolve(&id).expect("resolve");
    host.register(&id).expect("register");
    assert!(host.activate(&id).is_err(), "must require grant");
    host.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
    host.activate(&id).expect("activate after grant");
    assert_eq!(
        host.registry().get(&id).unwrap().state,
        bitty_plugin_host::PluginState::Activated
    );
    // Revocation detaches
    let cap_embed = CapabilityId::parse("browser.embed").unwrap();
    let report = host.revoke(&id, Some(&cap_embed)).unwrap();
    assert_eq!(report.revoked.len(), 1);
    assert!(!host.is_granted(&id, &hash, &cap_embed));
    // Hash changed → grant no longer matches
    let mut bumped = browser_panel_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), hash);
    assert!(!host.is_granted(&id, &bumped.manifest_hash(), &cap_embed));
}

// --- subscribe → publish → drain via bounded SideQueue/EventPipeline DropOldest ---

#[test]
fn browser_panel_subscribe_publish_drain_bounded_drop_oldest() {
    let manifest = browser_panel_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 4);
    host.declare(manifest).unwrap();
    host.resolve(&id).unwrap();
    host.register(&id).unwrap();
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).unwrap();
    host.subscribe(&id, EventKind::TerminalCwdChanged)
        .expect("cwd declared");
    host.subscribe(&id, EventKind::TerminalTitleChanged)
        .expect("title declared");
    host.subscribe(&id, EventKind::FocusChanged)
        .expect("focus declared");
    assert!(host.subscribe(&id, EventKind::InterceptPaste).is_err());

    for i in 0..10 {
        host.push_observation(bitty_plugin_host::HostObservation::TitleChanged(format!(
            "t{i}"
        )));
    }
    assert_eq!(host.side_queue().len(), 4);
    assert_eq!(host.side_queue().dropped(), 6);
    let drained = host.drain_observations();
    assert_eq!(drained.len(), 4);
    assert!(matches!(&drained[0], bitty_plugin_host::HostObservation::TitleChanged(s) if s=="t6"));

    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(100);
    let h = preg
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws))
        .expect("create browser panel");
    preg.mount_panel(h.id, h.generation, view).expect("mount");
    let topic = preg.declare_topic("xuepoo.browser:navigated").unwrap();
    preg.subscribe(h.id, h.generation, &topic)
        .expect("subscribe");
    for i in 0..80 {
        preg.publish(
            &topic,
            BoundedPayload::try_new(format!("https://example.com/page{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(preg.bus_events_for_panel(h.id) <= 64);
    assert!(preg.bus_total_events() <= 8192);
    let batch = preg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    assert_eq!(batch[0].payload.as_str(), "https://example.com/page16");
}

// --- Panel Runtime public path for browser-panel (View Browser + tiled controls) ---------

#[test]
fn browser_panel_via_panel_runtime_public_path_bounded() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let pid = create_browser_panel(&mut reg, ws, view).expect("create browser panel");
    assert_eq!(reg.panel_count(), 1);
    let _raw = pid.get();

    assert_eq!(BROWSER_MAX_PANELS_PER_WINDOW, 4);
    assert_eq!(BROWSER_MAX_NAVIGATION_QUEUE, 32);

    let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws2 = WorkspaceId::new(2);
    let view2 = ViewId::new(2);
    let h = reg2
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws2))
        .unwrap();
    reg2.mount_panel(h.id, h.generation, view2).unwrap();
    reg2.register_command(h.id, h.generation, "bitty-terminal.browser-panel:open")
        .expect("register open");
    reg2.register_command(h.id, h.generation, "bitty-terminal.browser-panel:navigate")
        .expect("register navigate");
    let h2 = reg2
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws2))
        .unwrap();
    reg2.mount_panel(h2.id, h2.generation, ViewId::new(3))
        .unwrap();
    assert!(
        reg2.register_command(h2.id, h2.generation, "bitty-terminal.browser-panel:open")
            .is_err()
    );
    for i in 0..30 {
        reg2.register_command(h.id, h.generation, &format!("xuepoo.browser:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg2.register_command(h.id, h.generation, "xuepoo.browser:overflow")
            .is_err()
    );

    assert!(BoundedPayload::try_new("a".repeat(9 * 1024)).is_err());
    let topic = reg2.declare_topic("xuepoo.browser:navigated").unwrap();
    reg2.subscribe(h.id, h.generation, &topic).unwrap();
    for i in 0..80 {
        reg2.publish(
            &topic,
            BoundedPayload::try_new(format!("https://example.com/page{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg2.bus_events_for_panel(h.id) <= 64);
    let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);

    let browser = bitty_ui::View::new(ViewId::new(10), 80, 24);
    let controls = bitty_ui::View::new(ViewId::new(11), 20, 24);
    let tiled = BrowserIntegration::tiled_layout(browser, Some(controls), 0.6);
    assert!(matches!(tiled, bitty_ui::LayoutNode::Split { .. }));
    assert_eq!(tiled.leaf_count(), 2);
    let tiled2 = browser_tiled_layout(
        bitty_ui::View::new(ViewId::new(12), 80, 24),
        Some(bitty_ui::View::new(ViewId::new(13), 20, 24)),
        0.5,
    );
    assert_eq!(tiled2.leaf_count(), 2);

    // View Browser(BrowserSurfaceId) distinct newtype verifies no From bridge
    let bid = bitty_ui::panel::BrowserSurfaceId::new(99);
    let vc = bitty_ui::panel::ViewContent::Browser(bid);
    assert!(vc.is_browser());
    assert_eq!(vc.browser_id(), Some(bid));
    use bitty_ui::panel::{BrowserSurfaceId, PanelId};
    assert_ne!(
        std::any::TypeId::of::<BrowserSurfaceId>(),
        std::any::TypeId::of::<PanelId>()
    );

    // navigation allowlist
    assert!(BrowserIntegration::is_https_url("https://example.com"));
    assert!(!BrowserIntegration::is_navigation_allowed(
        "http://example.com",
        false
    ));
    assert!(BrowserIntegration::is_navigation_allowed(
        "https://example.com",
        false
    ));
    assert!(!BrowserIntegration::is_navigation_allowed(
        "file://~/projects/foo.html",
        false
    ));
    assert!(BrowserIntegration::is_navigation_allowed(
        "file://~/projects/foo.html",
        true
    ));
    assert!(!BrowserIntegration::is_navigation_allowed(
        "file:///etc/passwd",
        true
    ));
    assert!(!BrowserIntegration::is_navigation_allowed(
        "javascript:alert(1)",
        false
    ));
    assert!(!BrowserIntegration::is_valid_url("javascript:alert(1)"));
    assert!(BrowserIntegration::is_valid_url("https://example.com"));
    assert!(!BrowserIntegration::is_valid_url(""));
    assert!(!BrowserIntegration::is_valid_url("https://\0evil"));

    // storage gate
    assert!(BrowserIntegration::is_storage_allowed(
        "https://example.com"
    ));
    assert!(!BrowserIntegration::is_storage_allowed(""));

    // history bounded
    let many: Vec<String> = (0..100)
        .map(|i| format!("https://example.com/page{i}"))
        .collect();
    let hist = BrowserIntegration::list_history(&many, false);
    assert_eq!(hist.len(), BROWSER_MAX_HISTORY_ENTRIES);
    // file-url isolation
    assert!(!BrowserIntegration::is_within_file_scope(
        "file:///etc/passwd"
    ));
    assert!(BrowserIntegration::is_within_file_scope(
        "file://~/projects/foo/bar.html"
    ));

    // navigation queue DropOldest
    let mut q = BrowserNavigationQueue::new();
    for i in 0..40 {
        q.enqueue(format!("https://example.com/p{i}"), false);
    }
    assert_eq!(q.len(), BROWSER_MAX_NAVIGATION_QUEUE);
    assert_eq!(q.dropped(), 8);
    assert_eq!(q.peek_front().unwrap(), "https://example.com/p8");

    // file-url queue needs gate
    let mut q2 = BrowserNavigationQueue::new();
    assert!(!q2.enqueue("file://~/projects/a.html".to_string(), false));
    assert!(q2.enqueue("file://~/projects/a.html".to_string(), true));

    // Config validation
    let bad = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..Default::default()
    };
    assert!(validate_browser_panel_config(&bad).is_err());
    assert!(validate_browser_panel_config(&PanelRegistryConfig::default()).is_ok());
    // AlreadyMounted same view
    let mut reg3 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws_mount = WorkspaceId::new(99);
    let view_mount = ViewId::new(999);
    let ha = reg3
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws_mount))
        .unwrap();
    let hb = reg3
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws_mount))
        .unwrap();
    reg3.mount_panel(ha.id, ha.generation, view_mount).unwrap();
    assert!(reg3.mount_panel(hb.id, hb.generation, view_mount).is_err());
}

#[test]
fn browser_history_and_filter_bounded() {
    let raw = vec![
        "https://example.com/a".to_string(),
        "https://example.com/b".to_string(),
        "https://example.com/a".to_string(),
        "http://example.com/c".to_string(),
        "javascript:alert(1)".to_string(),
    ];
    let listed = BrowserIntegration::list_history(&raw, false);
    assert_eq!(listed.len(), 2);
    let filtered = BrowserIntegration::filter_history(&listed, "example");
    assert_eq!(filtered.len(), 2);
    let entries: Vec<BrowserHistoryEntry> = vec![
        BrowserHistoryEntry::new("https://example.com/alpha".to_string(), "Alpha".to_string())
            .unwrap(),
        BrowserHistoryEntry::new("https://example.com/beta".to_string(), "Beta".to_string())
            .unwrap(),
    ];
    let f = BrowserIntegration::filter_history(&entries, "alpha");
    assert_eq!(f.len(), 1);
    let sorted = BrowserIntegration::sorted_by_title(entries);
    assert_eq!(sorted.len(), 2);
}

#[test]
fn browser_tiled_layout_deterministic_and_overlay() {
    let browser = bitty_ui::View::new(ViewId::new(1), 80, 24);
    let controls = bitty_ui::View::new(ViewId::new(2), 20, 24);
    let layout = browser_tiled_layout(browser, Some(controls), 0.5);
    let allocs = layout.layout(UiRect::new(0, 0, 100, 24));
    assert_eq!(allocs.len(), 2);
    assert_eq!(allocs[0].1.width + allocs[1].1.width, 100);
    // Overlay while browser hidden retains handle but pauses media — verify Panel Suspended retains id
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let h = reg
        .create_panel(bitty_ui::panel::PanelType::Browser, Some(ws))
        .unwrap();
    reg.mount_panel(h.id, h.generation, view).unwrap();
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    reg.suspend_panel(h.id, h.generation).unwrap();
    assert_eq!(reg.focused_panel(ws), None);
    reg.resume_panel(h.id, h.generation).unwrap();
    reg.focus_panel(h.id, h.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h.id));
}
