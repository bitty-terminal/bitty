#![forbid(unsafe_code)]
//! File manager via Panel Runtime — public API verification (CTX-0108, OQ-011).
//!
//! Verifies `bitty-terminal.file-manager` (tiled Panel, fs.read + optional fs.write)
//! as generic Panel Runtime consumer with no private channel, via the public
//! PluginHost path (`declare → resolve → register → GrantRecord → activate →
//! subscribe → publish → drain SideQueue DropOldest`) and PanelRegistry public
//! path (`PanelRegistry::new → create_panel → mount_panel → focus_panel` with
//! `PanelType::Helper` plus `register_command`/`create_overlay`/`declare_topic`/
//! `subscribe`/`publish`/`drain_batch`), tiled `LayoutNode` `H`/`V` reuse, bounded
//! queues `64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload, `32`/`8 KiB`
//! batch, single-process `winit` one-registry-per-window, default disabled,
//! safe-mode reject, `forbid(unsafe)`.
//!
//!mirrors palette_statusline_project_panel but for the file-manager P1 candidate
//! with tiled workspace and fs isolation.

use bitty_plugin_host::{
    CapabilityId, DropPolicy, EventKind, GrantRecord, PluginHost, bundled::file_manager_manifest,
};
use bitty_runtime::{
    Runtime, RuntimeConfig,
    file_manager::{
        FILE_MANAGER_FS_READ_PATTERN, FILE_MANAGER_FS_WRITE_PATTERN, FILE_MANAGER_MAX_ENTRIES,
        FileEntry, FileKind, FileManagerIntegration, create_file_manager_panel,
        file_manager_tiled_layout, validate_file_manager_panel_config,
    },
    registry::{BoundedPayload, PanelRegistry, PanelRegistryConfig, WorkspaceId},
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{Rect as UiRect, ViewId};
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

// --- public PluginHost path for file-manager (tiled Panel + fs) --------------

#[test]
fn file_manager_via_public_plugin_host_path() {
    let manifest = file_manager_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    // Must contain panel.provider + panel.create + terminal.semantic-read + fs.read + fs.write
    assert!(granted.contains(&CapabilityId::parse("panel.provider").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("panel.create").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("terminal.semantic-read").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("fs.read:~/projects/**").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("fs.write:~/projects/**").unwrap()));
    assert_eq!(manifest.capabilities.filesystem.len(), 2);
    assert_eq!(FILE_MANAGER_FS_READ_PATTERN, "~/projects/**");
    assert_eq!(FILE_MANAGER_FS_WRITE_PATTERN, "~/projects/**");
    assert_eq!(manifest.lazy.commands.len(), 3);
    assert!(
        manifest
            .lazy
            .commands
            .iter()
            .any(|c| c.as_str() == "bitty-terminal.file-manager:open")
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
    let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
    let report = host.revoke(&id, Some(&cap_read)).unwrap();
    assert_eq!(report.revoked.len(), 1);
    assert!(!host.is_granted(&id, &hash, &cap_read));
    // Hash changed → grant no longer matches
    let mut bumped = file_manager_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), hash);
    assert!(!host.is_granted(&id, &bumped.manifest_hash(), &cap_read));
}

// --- subscribe → publish → drain via bounded SideQueue/EventPipeline DropOldest ---

#[test]
fn file_manager_subscribe_publish_drain_bounded_drop_oldest() {
    let manifest = file_manager_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 4);
    host.declare(manifest).unwrap();
    host.resolve(&id).unwrap();
    host.register(&id).unwrap();
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).unwrap();
    // File-manager subscribes to cwd/title/focus (observation-only, no hot-path)
    host.subscribe(&id, EventKind::TerminalCwdChanged)
        .expect("cwd declared");
    host.subscribe(&id, EventKind::TerminalTitleChanged)
        .expect("title declared");
    host.subscribe(&id, EventKind::FocusChanged)
        .expect("focus declared");
    assert!(host.subscribe(&id, EventKind::InterceptPaste).is_err());

    // Flood side queue beyond 4 → DropOldest newest survive
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

    // Panel EventBus bounded 64 per-sub DropOldest for file open events
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(100);
    let h = preg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .expect("create panel");
    preg.mount_panel(h.id, h.generation, view).expect("mount");
    let topic = preg.declare_topic("xuepoo.files:listing").unwrap();
    preg.subscribe(h.id, h.generation, &topic)
        .expect("subscribe");
    for i in 0..80 {
        preg.publish(
            &topic,
            BoundedPayload::try_new(format!("~/projects/file{i}.txt")).unwrap(),
        )
        .unwrap();
    }
    assert!(preg.bus_events_for_panel(h.id) <= 64);
    assert!(preg.bus_total_events() <= 8192);
    let batch = preg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // 8 KiB batch limit: first batch after DropOldest 80->64 should start at file16
    assert_eq!(batch[0].payload.as_str(), "~/projects/file16.txt");
}

// --- Panel Runtime public path for file-manager (tiled, fs isolation) ---------

#[test]
fn file_manager_panel_via_panel_runtime_public_path_bounded() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let pid = create_file_manager_panel(&mut reg, ws, view).expect("create file-manager panel");
    assert_eq!(reg.panel_count(), 1);
    let _raw = pid.get();

    // Command registry single owner: register open, duplicate rejected
    let mut reg2 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws2 = WorkspaceId::new(2);
    let view2 = ViewId::new(2);
    let h = reg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    reg2.mount_panel(h.id, h.generation, view2).unwrap();
    reg2.register_command(h.id, h.generation, "bitty-terminal.file-manager:open")
        .expect("register open");
    reg2.register_command(h.id, h.generation, "bitty-terminal.file-manager:preview")
        .expect("register preview");
    let h2 = reg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    reg2.mount_panel(h2.id, h2.generation, ViewId::new(3))
        .unwrap();
    assert!(
        reg2.register_command(h2.id, h2.generation, "bitty-terminal.file-manager:open")
            .is_err()
    );
    // Per-panel 32 bound
    for i in 0..30 {
        reg2.register_command(h.id, h.generation, &format!("xuepoo.fm:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg2.register_command(h.id, h.generation, "xuepoo.fm:overflow")
            .is_err()
    );

    // Payload 8 KiB bound
    assert!(BoundedPayload::try_new("a".repeat(9 * 1024)).is_err());
    // Batch 32/8 KiB via drain (non-coalescable topic for bounded test)
    let topic = reg2.declare_topic("xuepoo.files:listing").unwrap();
    reg2.subscribe(h.id, h.generation, &topic).unwrap();
    for i in 0..80 {
        reg2.publish(
            &topic,
            BoundedPayload::try_new(format!("~/projects/dir/file{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg2.bus_events_for_panel(h.id) <= 64);
    let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);

    // Tiled layout is H split, not a new primitive
    let main = bitty_ui::View::new(ViewId::new(10), 80, 24);
    let preview = bitty_ui::View::new(ViewId::new(11), 40, 24);
    let tiled = FileManagerIntegration::tiled_layout(main, Some(preview), 0.5);
    assert!(matches!(tiled, bitty_ui::LayoutNode::Split { .. }));
    assert_eq!(tiled.leaf_count(), 2);

    // fs isolation via helper
    assert!(FileManagerIntegration::is_within_read_scope(
        "~/projects/foo"
    ));
    assert!(!FileManagerIntegration::is_within_read_scope("/etc/passwd"));
    assert!(!FileManagerIntegration::is_within_read_scope(
        "~/projects/../evil"
    ));
    assert!(FileManagerIntegration::is_fs_write_allowed(
        "~/projects/foo/bar"
    ));
    assert!(!FileManagerIntegration::is_fs_write_allowed("/tmp/evil"));

    // Config validation bounded fail-closed
    let bad = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..Default::default()
    };
    assert!(validate_file_manager_panel_config(&bad).is_err());
    assert!(validate_file_manager_panel_config(&PanelRegistryConfig::default()).is_ok());
    // AlreadyMounted: same view cannot host two panels in same registry
    let mut reg3 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws_mount = WorkspaceId::new(99);
    let view_mount = ViewId::new(999);
    let ha = reg3
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws_mount))
        .unwrap();
    let hb = reg3
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws_mount))
        .unwrap();
    reg3.mount_panel(ha.id, ha.generation, view_mount).unwrap();
    assert!(reg3.mount_panel(hb.id, hb.generation, view_mount).is_err());

    // Overlay via public path: 4+1 bound, modal exclusivity
    let mut reg4 = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let container = UiRect::new(0, 0, 80, 24);
    let mut overlay_ids = Vec::new();
    for _ in 0..4 {
        let oid = reg4
            .create_overlay(
                bitty_ui::panel::OverlayKind::NonModal,
                container,
                "hello",
                None,
            )
            .unwrap();
        overlay_ids.push(oid);
    }
    // 5th non-modal fails
    assert!(
        reg4.create_overlay(
            bitty_ui::panel::OverlayKind::NonModal,
            container,
            "overflow",
            None
        )
        .is_err()
    );
    // Modal still allowed (4+1)
    let modal_id = reg4
        .create_overlay(
            bitty_ui::panel::OverlayKind::Modal,
            container,
            "modal",
            None,
        )
        .expect("modal under 4+1");
    overlay_ids.push(modal_id);
    assert_eq!(reg4.overlay_len(), 5);
    // Second modal fails (OverlayBusy)
    assert!(
        reg4.create_overlay(
            bitty_ui::panel::OverlayKind::Modal,
            container,
            "modal2",
            None
        )
        .is_err()
    );
    // Dismiss one non-modal, can add another
    let first_id = overlay_ids[0];
    reg4.dismiss_overlay(first_id);
    assert_eq!(reg4.overlay_len(), 4);
    reg4.create_overlay(
        bitty_ui::panel::OverlayKind::NonModal,
        container,
        "again",
        None,
    )
    .unwrap();
    assert_eq!(reg4.overlay_len(), 5);
    // Panel capability deny-by-default per (PanelId,generation)
    let h3 = reg4
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    assert!(
        reg4.require_panel_capability(h3.id, h3.generation, "panel.provider")
            .is_err()
    );
    reg4.grant_panel_capability(h3.id, h3.generation, "panel.provider")
        .unwrap();
    assert!(reg4.is_panel_capability_granted(h3.id, h3.generation, "panel.provider"));
    // Invalid panel capability family rejected
    assert!(
        reg4.grant_panel_capability(h3.id, h3.generation, "fs.read:~/projects/**")
            .is_err()
    );
}

// --- file-manager helpers pure bounded (listing, filter, tiled) --------------

#[test]
fn file_manager_helpers_pure_bounded_and_tiled_deterministic() {
    // Listing helper: sorted deduped bounded 128
    let raw = vec![
        "~/projects/b".to_string(),
        "~/projects/a".to_string(),
        "~/projects/a".to_string(),
        "/etc/passwd".to_string(),
        "~/projects/c".to_string(),
    ];
    let listed = FileManagerIntegration::list_entries(&raw);
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].path, "~/projects/a");
    assert_eq!(listed[1].path, "~/projects/b");
    // Bounded at 128
    let many: Vec<String> = (0..200)
        .map(|i| format!("~/projects/file{i}.txt"))
        .collect();
    assert_eq!(
        FileManagerIntegration::list_entries(&many).len(),
        FILE_MANAGER_MAX_ENTRIES
    );
    // Filter bounded, case-insensitive over name/path
    let entries = FileManagerIntegration::list_entries(&many[..10]);
    let filtered = FileManagerIntegration::filter_entries(&entries, "file1");
    assert!(!filtered.is_empty() && filtered.len() <= 10);
    let filtered_ci = FileManagerIntegration::filter_entries(&entries, "FILE1");
    assert_eq!(filtered.len(), filtered_ci.len());
    // Tiled layout deterministic H split reuse
    let main = bitty_ui::View::new(ViewId::new(20), 80, 24);
    let preview = bitty_ui::View::new(ViewId::new(21), 40, 24);
    let tiled = FileManagerIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.5);
    let allocs = tiled.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2);
    // Solo (no preview) is single leaf
    let solo = FileManagerIntegration::tiled_layout(main, None, 0.5);
    assert_eq!(solo.leaf_count(), 1);
    // Vertical stack
    let v1 = bitty_ui::View::new(ViewId::new(30), 80, 12);
    let v2 = bitty_ui::View::new(ViewId::new(31), 80, 12);
    let stack = FileManagerIntegration::vertical_stack(vec![v1, v2]);
    assert!(matches!(stack, bitty_ui::LayoutNode::Stack(_)));
    // FileEntry creation bounded
    let entry =
        FileEntry::from_path("~/projects/foo.txt".to_string(), Some(FileKind::File)).unwrap();
    assert_eq!(entry.name, "foo.txt");
    assert!(!entry.truncated);
    assert_eq!(entry.kind, FileKind::File);
    // Long name truncated
    let long_name = "a".repeat(200);
    let long_path = format!("~/projects/{long_name}");
    let long_entry = FileEntry::from_path(long_path, None).unwrap();
    assert_eq!(long_entry.name.chars().count(), 128);
    assert!(long_entry.truncated);
    // Invalid path yields None
    assert!(FileEntry::from_path("/etc/passwd".to_string(), None).is_none());
    assert!(FileEntry::from_path("~/projects/../evil".to_string(), None).is_none());
    // Outside scope rejected by is_valid/is_within
    assert!(!FileManagerIntegration::is_valid_path(""));
    assert!(!FileManagerIntegration::is_valid_path("~/projects/\0evil"));
}

// --- safe-mode rejects file-manager without panic ------------------------------

#[test]
fn safe_mode_rejects_file_manager_without_panic() {
    let manifest = file_manager_manifest();
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.set_safe_mode(true);
    assert!(host.declare(manifest.clone()).is_err());
    host.set_safe_mode(false);
    assert!(host.declare(manifest.clone()).is_ok());

    let mut rt = Runtime::with_defaults().unwrap();
    rt.set_plugin_safe_mode(true);
    assert!(rt.register_plugin(file_manager_manifest()).is_err());
    assert!(rt.tick().is_some());
    rt.set_plugin_safe_mode(false);
    assert!(rt.register_plugin(file_manager_manifest()).is_ok());
}

// --- no private channel: third-party parity -----------------------------------

#[test]
fn file_manager_has_no_private_channel_parity_with_third_party() {
    let bundled = file_manager_manifest();
    let mut third = bundled.clone();
    third.identity.id = bitty_plugin_host::PluginId::new("xuepoo.file-manager-mirror").unwrap();
    third.identity.name = "Third Party Mirror".to_string();
    for (which, manifest) in [("bundled", bundled), ("third", third)] {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
        host.declare(manifest)
            .unwrap_or_else(|e| panic!("{which} declare: {e}"));
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        assert!(host.activate(&id).is_err(), "{which} must require grant");
        host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
        host.activate(&id)
            .unwrap_or_else(|e| panic!("{which} activate: {e}"));
    }
}

// --- bounded, headless, forbid(unsafe), single-process winit ------------------

#[test]
fn file_manager_is_headless_and_forbid_unsafe_single_process_winit() {
    let host = PluginHost::new(DropPolicy::DropOldest, 8);
    assert!(!host.is_safe_mode());
    assert!(host.side_queue().is_empty());
    let preg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    assert_eq!(preg.panel_count(), 0);
    let treg =
        bitty_runtime::TerminalRegistry::new(bitty_runtime::RegistryConfig::default()).unwrap();
    assert_eq!(treg.terminal_count(), 0);
    let rt = Runtime::with_defaults().unwrap();
    assert!(rt.is_headless());
    // Command registry is headless, no window/GPU, bounded 32
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(99);
    let h = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    reg.mount_panel(h.id, h.generation, ViewId::new(99))
        .unwrap();
    for i in 0..32 {
        reg.register_command(h.id, h.generation, &format!("xuepoo.fm:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg.register_command(h.id, h.generation, "xuepoo.fm:overflow")
            .is_err()
    );
    // Overlay manager 4+1 headless
    let container = UiRect::new(0, 0, 80, 24);
    for _ in 0..4 {
        reg.create_overlay(
            bitty_ui::panel::OverlayKind::NonModal,
            container,
            "hello",
            None,
        )
        .unwrap();
    }
    let dbg = format!("{preg:?}");
    assert!(dbg.contains("PanelRegistry"));
    assert!(!dbg.contains("pty"));
    let dbg2 = format!("{treg:?}");
    assert!(dbg2.contains("TerminalRegistry"));
}

// --- command registry bounded and overlay focus MRU ---------------------------

#[test]
fn file_manager_command_registry_bounded_and_overlay_focus_mru() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(10);
    let v1 = ViewId::new(10);
    let v2 = ViewId::new(11);
    let h1 = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    let h2 = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    reg.mount_panel(h1.id, h1.generation, v1).unwrap();
    reg.mount_panel(h2.id, h2.generation, v2).unwrap();
    // Register commands per panel, up to 32 bound.
    for i in 0..32 {
        reg.register_command(h1.id, h1.generation, &format!("xuepoo.fm:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg.register_command(h1.id, h1.generation, "xuepoo.fm:overflow")
            .is_err()
    );
    // Duplicate across panels rejected
    assert!(
        reg.register_command(h2.id, h2.generation, "xuepoo.fm:cmd0")
            .is_err()
    );
    // Focus MRU per Workspace per Window (PanelFocus)
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    reg.focus_panel(h2.id, h2.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h2.id));
    assert_eq!(reg.mru_order(ws), vec![h2.id, h1.id]);
    assert_eq!(reg.command_owner("xuepoo.fm:cmd0"), Some(h1.id));
    // Generation isolation: stale handle rejected
    let bad_gen = bitty_runtime::registry::Generation(h1.generation.get().wrapping_add(100));
    assert!(
        reg.register_command(h1.id, bad_gen, "xuepoo.fm:stale")
            .is_err()
    );
    // Overlay text truncated at char boundary via FileManagerIntegration
    let long = "a".repeat(200);
    let truncated = FileManagerIntegration::truncate_name(&long);
    assert_eq!(truncated.chars().count(), 128);
}

// --- panel reactive via EventBus, no hot path --------------------------------

#[test]
fn file_manager_panel_reactive_via_eventbus_no_hot_path() {
    // File-manager observes via EventBus bounded DropOldest: declare → subscribe → publish → drain
    // Use non-coalescable topic for bounded test (file.open is coalescable latest-wins)
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let h = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    reg.mount_panel(h.id, h.generation, view).unwrap();
    let topic = reg.declare_topic("xuepoo.files:listing").unwrap();
    reg.subscribe(h.id, h.generation, &topic).unwrap();
    for i in 0..70 {
        reg.publish(
            &topic,
            BoundedPayload::try_new(format!("~/projects/file{i}.txt")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg.bus_events_for_panel(h.id) <= 64);
    let batch = reg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // Filtering is pure bounded, no hot-path
    let raw: Vec<String> = (0..50).map(|i| format!("~/projects/file{i}.txt")).collect();
    let entries = FileManagerIntegration::list_entries(&raw);
    let filtered = FileManagerIntegration::filter_entries(&entries, "file1");
    assert!(filtered.len() <= 128);
    assert!(
        filtered
            .iter()
            .all(|e| e.name.contains("file1") || e.path.contains("file1"))
    );
    // State observation remains pure
    let mut state = State::new();
    state.apply(&TerminalAction::OscCwd {
        url: BoundedString::new("file:///home/user/projects/foo"),
    });
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("file-manager"),
    });
    assert!(state.cwd_report().is_some());
    assert!(!state.title().is_empty());
    // Rendering remains deterministic
    let rendered_again = FileManagerIntegration::list_entries(&raw);
    assert_eq!(entries, rendered_again);
}

// --- fs isolation via CapabilityId and helper --------------------------------

#[test]
fn file_manager_fs_isolation_via_capability_id_and_helper() {
    let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
    assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
    assert_eq!(cap_read.as_str(), "fs.read:~/projects/**");
    let cap_write = CapabilityId::parse("fs.write:~/projects/**").unwrap();
    assert_eq!(cap_write.family(), bitty_plugin_host::CapabilityFamily::Fs);
    let outside = CapabilityId::parse("fs.read:/tmp/**").unwrap();
    assert_ne!(cap_read, outside);
    // Host grant isolation already proven via manifest above, here also verify helper rejects outside
    assert!(!FileManagerIntegration::is_within_read_scope("/tmp/evil"));
    assert!(!FileManagerIntegration::is_within_read_scope(
        "~/projects/foo/../../etc/passwd"
    ));
    assert!(FileManagerIntegration::is_within_read_scope(
        "~/projects/foo/bar"
    ));
    assert!(FileManagerIntegration::is_fs_allowed("~/projects/foo/bar"));
    assert!(FileManagerIntegration::is_fs_write_allowed(
        "~/projects/foo/bar"
    ));
    assert!(!FileManagerIntegration::is_fs_allowed(
        "~/projects/foo/../../etc"
    ));
    assert!(!FileManagerIntegration::is_fs_write_allowed("/etc/passwd"));
    // Bounded listing already verified
    let many: Vec<String> = (0..200).map(|i| format!("~/projects/file{i}")).collect();
    assert_eq!(
        FileManagerIntegration::list_entries(&many).len(),
        FILE_MANAGER_MAX_ENTRIES
    );
    // Manifest hash deterministic and panel.* present
    let m = file_manager_manifest();
    assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
    assert!(
        m.capabilities
            .ids
            .contains(&CapabilityId::parse("panel.provider").unwrap())
    );
    assert!(
        m.capabilities
            .ids
            .contains(&CapabilityId::parse("panel.create").unwrap())
    );
}

// --- Runtime side-queue DropOldest for file-manager observations --------------

#[test]
fn runtime_side_queue_drop_oldest_for_observations() {
    let mut rt = Runtime::with_defaults().expect("runtime");
    rt.handle_pty_bytes(b"\x1b]0;file-manager-title\x07");
    rt.handle_pty_bytes(b"\x1b]7;file:///home/user/projects/proj\x07");
    let obs = rt.drain_plugin_observations();
    assert!(obs.iter().any(|o| matches!(o, bitty_plugin_host::HostObservation::TitleChanged(s) if s=="file-manager-title")));
    assert!(obs.iter().any(|o| matches!(o, bitty_plugin_host::HostObservation::CwdChanged(s) if s.contains("file:///home/user/projects/proj"))));
    // Tiled layout via file-manager helper is pure, not hot-path, bounded
    let main = bitty_ui::View::new(ViewId::new(1), 80, 24);
    let preview = bitty_ui::View::new(ViewId::new(2), 40, 24);
    let tiled = FileManagerIntegration::tiled_layout(main, Some(preview), 0.6);
    assert!(matches!(tiled, bitty_ui::LayoutNode::Split { .. }));
    // Flood side queue beyond 128 (default) -> DropOldest newest survive
    let mut rt2 =
        Runtime::with_plugin_host_capacity(RuntimeConfig::default(), DropPolicy::DropOldest, 64, 4)
            .expect("rt small side");
    for i in 0..10 {
        rt2.push_plugin_observation(bitty_plugin_host::HostObservation::TitleChanged(format!(
            "t{i}"
        )));
    }
    assert_eq!(rt2.plugin_side_len(), 4);
    assert!(rt2.plugin_side_dropped() > 0);
    let drained = rt2.drain_plugin_observations();
    assert_eq!(drained.len(), 4);
    assert!(matches!(&drained[0], bitty_plugin_host::HostObservation::TitleChanged(s) if s=="t6"));
    // Panel capability per generation deny-by-default
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let h = preg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    preg.mount_panel(h.id, h.generation, view).unwrap();
    assert!(!preg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
    preg.grant_panel_capability(h.id, h.generation, "panel.create")
        .unwrap();
    assert!(preg.is_panel_capability_granted(h.id, h.generation, "panel.create"));
    // Stale generation fails
    let stale = bitty_runtime::registry::Generation(h.generation.get().wrapping_add(1));
    assert!(!preg.is_panel_capability_granted(h.id, stale, "panel.create"));
    assert!(
        preg.require_panel_capability(h.id, stale, "panel.create")
            .is_err()
    );
}

// --- tiled Panel reuse LayoutNode H/V determinism ----------------------------

#[test]
fn file_manager_tiled_panel_reuses_layout_hv_deterministically() {
    // File manager proves tiled Panel(PanelId) workspace reuse of LayoutNode H/V
    // with panel content, not a PTY, bounded 32 leaves, PR-1..PR-12.
    let main = bitty_ui::View::new(ViewId::new(1), 80, 24);
    let preview = bitty_ui::View::new(ViewId::new(2), 40, 24);
    let tiled = file_manager_tiled_layout(main.clone(), Some(preview.clone()), 0.5);
    assert_eq!(tiled.leaf_count(), 2);
    let allocs = tiled.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2);
    // Solo file manager panel (no preview) is single leaf
    let solo = file_manager_tiled_layout(main.clone(), None, 0.5);
    assert_eq!(solo.leaf_count(), 1);
    // Vertical stack for file list details
    let v1 = bitty_ui::View::new(ViewId::new(3), 80, 12);
    let v2 = bitty_ui::View::new(ViewId::new(4), 80, 12);
    let stack = FileManagerIntegration::vertical_stack(vec![v1, v2]);
    assert_eq!(stack.leaf_count(), 2);
    // PanelId distinct from ViewId via type system
    let pid = bitty_runtime::registry::PanelId::new(1);
    let vid = ViewId::new(1);
    assert_ne!(
        std::any::TypeId::of::<bitty_runtime::registry::PanelId>(),
        std::any::TypeId::of::<ViewId>()
    );
    assert_eq!(pid.get(), vid.0);
    // PanelRegistry single-process winit: one registry per window, Generation monotonic
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(10);
    let h1 = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    let h2 = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    assert_ne!(h1.id, h2.id);
    assert!(h2.generation.get() > h1.generation.get() || h2.generation == h1.generation);
    // PR-1..PR-12 defaults validated before allocation via registry defaults
    assert_eq!(reg.config().max_panels_per_workspace, 16);
    assert_eq!(reg.config().max_panels_per_window, 32);
}
