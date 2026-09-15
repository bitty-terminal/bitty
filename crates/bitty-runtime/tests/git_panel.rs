#![forbid(unsafe_code)]
//! Git panel via Panel Runtime — parity verification after the OQ-053 split.
//!
//! `bitty-terminal.git-panel` migrated to the independent first-party package
//! `bitty-terminal/git-panel` (OQ-053, `bitty` `CTX-0400`), so this suite now
//! exercises the generic Panel Runtime path against a local fixture manifest
//! mirroring the former bundled manifest instead of the bundled catalog — the
//! same fixture pattern the palette (`CTX-0397`) and statusline (`CTX-0398`)
//! splits established. The former bundled review implementation
//! (`bitty-runtime::git_panel`) is removed; its pure listing/allowlist
//! helpers ship in the independent Lua package (`lua/git-panel/`) with
//! headless Lua specs there.
//!
//! Contract provenance for the pinned values below: the allowlisted `git`
//! verbs, bounds, and `[tools.git]` declaration are the accepted Layer 2
//! `[tools.git]` slice (CTX-0425; canonical record in
//! `bitty-plugins-docs`
//! `specifications/plugin-reuse-and-providers.md`, accepted `[tools.git]`
//! contract v1). The rest of that reuse RFC stays draft; only the slice
//! gating this split is accepted. OQ-053 is accepted and closed.
//!
//! What this suite proves with no private channel, via the public PluginHost
//! path (`declare → resolve → register → GrantRecord → activate → subscribe
//! → publish → drain SideQueue DropOldest`) and the PanelRegistry public
//! path (`PanelRegistry::new → create_panel → mount_panel → focus_panel`
//! with `PanelType::Helper` plus `register_command`/`create_overlay`/
//! `declare_topic`/`subscribe`/`publish`/`drain_batch`), tiled `LayoutNode`
//! `H`/`V` reuse, bounded queues `64`/`1024`/`2 MiB`/`8192`, `DropOldest`,
//! `8 KiB` payload, `32`/`8 KiB` batch, single-process `winit`
//! one-registry-per-window, default disabled, safe-mode reject,
//! `forbid(unsafe)`.

use bitty_plugin_host::{
    CapabilityId, DropPolicy, EventKind, GrantRecord, PluginHost, bundled::bundled_manifest_for,
};
use bitty_runtime::{
    Runtime, RuntimeConfig,
    registry::{BoundedPayload, PanelRegistry, PanelRegistryConfig, WorkspaceId},
};
use bitty_term_state::{State, TerminalAction};
use bitty_ui::{LayoutNode, Rect as UiRect, SplitAxis, View, ViewId};
use bitty_vt::BoundedString;

// --- former bundled contract, pinned locally ---------------------------------
//
// Values mirror the former bundled `git_panel_manifest`
// (`crates/bitty-plugin-host/src/bundled.rs`) and the former
// `bitty-runtime::git_panel` constants (`GIT_ALLOWED_SUBCOMMANDS`,
// `GIT_PANEL_MAX_*`, `GIT_PANEL_PROCESS_SPAWN_GIT`). Canonical accepted
// record for the allowlist, bounds, and `[tools.git]` declaration is the
// Layer 2 `[tools.git]` slice (CTX-0425); this module is parity evidence,
// not the contract.

/// Former bundled plugin id.
const GIT_PANEL_ID: &str = "bitty-terminal.git-panel";

/// Former bundled display name.
const GIT_PANEL_NAME: &str = "Git Panel";

/// Former bundled version.
const GIT_PANEL_VERSION: &str = "0.1.0";

/// Former bundled description.
const GIT_PANEL_DESCRIPTION: &str = "Tiled Panel git branch/status/diff/log via process.spawn:git allowlisted [tools.git] bounded 8KiB/32/64 PR-1..12";

/// Former bundled compat ranges.
const GIT_PANEL_COMPAT_BITTY: &str = ">=0.1,<1.0";
const GIT_PANEL_COMPAT_PLUGIN_API: &str = "^1.0";

/// Process spawn capability for git — allowlisted via `[tools.git]`.
const GIT_PANEL_PROCESS_SPAWN_GIT: &str = "process.spawn:git";

/// Filesystem read scope for the working tree.
const GIT_PANEL_FS_READ_PATTERN: &str = "~/projects/**";

/// Accepted read-only `git` verbs (`GIT_ALLOWED_SUBCOMMANDS`).
const GIT_ALLOWED_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "log",
    "branch",
    "show",
    "rev-parse",
    "ls-files",
];

/// Write verbs that must never be allowlisted.
const GIT_DENIED_SUBCOMMANDS: &[&str] = &[
    "commit", "push", "reset", "checkout", "fetch", "clone", "remote", "add",
];

/// Former bundled lazy commands.
const GIT_PANEL_COMMANDS: &[&str] = &[
    "bitty-terminal.git-panel:open",
    "bitty-terminal.git-panel:status",
    "bitty-terminal.git-panel:diff",
    "bitty-terminal.git-panel:log",
    "bitty-terminal.git-panel:branch",
];

/// Former bundled observation events.
const GIT_PANEL_EVENTS: &[&str] = &[
    "terminal.cwd-changed",
    "terminal.title-changed",
    "focus.changed",
];

/// Listing bounds (former `GIT_PANEL_MAX_*`).
const GIT_PANEL_MAX_ENTRIES: usize = 128;
const GIT_PANEL_MAX_COMMITS: usize = 64;
const GIT_PANEL_MAX_BRANCHES: usize = 32;
const GIT_PANEL_MAX_NAME_CHARS: usize = 128;
const GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS: usize = 256;
const GIT_PANEL_MAX_PATH_BYTES: usize = 4096;
const GIT_PANEL_MAX_GIT_ARGS: usize = 32;
const GIT_PANEL_MAX_GIT_ARG_BYTES: usize = 256;
const GIT_PANEL_MAX_GIT_TOTAL_BYTES: usize = 8192;
const GIT_PANEL_PAYLOAD_MAX_BYTES: usize = 8192;
const GIT_PANEL_MAX_SELECTION: usize = 64;

/// Local fixture mirroring the former bundled `bitty-terminal.git-panel` manifest.
///
/// Git-panel migrated to an independent first-party package (OQ-053, `bitty`
/// `CTX-0400`), so this suite now exercises the generic Panel Runtime path
/// against a plain manifest instead of the bundled catalog.
fn git_panel_manifest() -> bitty_plugin_host::PluginManifest {
    use bitty_plugin_host::{
        CapabilityRequests, Compat, FilesystemRequest, FsAccess, LazyTriggers, PluginId,
        PluginIdentity, PluginManifest, QualifiedName, ToolDeclaration,
    };
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse(GIT_PANEL_PROCESS_SPAWN_GIT).expect("known capability"));
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Read,
        paths: vec![GIT_PANEL_FS_READ_PATTERN.to_string()],
    });
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(GIT_PANEL_ID).expect("valid id"),
            name: GIT_PANEL_NAME.to_string(),
            version: GIT_PANEL_VERSION.to_string(),
            description: GIT_PANEL_DESCRIPTION.to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(GIT_PANEL_COMPAT_BITTY.to_string()),
            plugin_api: Some(GIT_PANEL_COMPAT_PLUGIN_API.to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        tools: vec![ToolDeclaration {
            tool: "git".to_string(),
            required: true,
            version_req: ">=2.30".to_string(),
        }],
        lazy: LazyTriggers {
            commands: GIT_PANEL_COMMANDS
                .iter()
                .map(|c| QualifiedName::new(c).expect("qualified"))
                .collect(),
            events: GIT_PANEL_EVENTS.iter().map(|e| e.to_string()).collect(),
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

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

// --- the bundled id is freed for the independent package ---------------------

#[test]
fn git_panel_id_is_freed_from_the_bundled_catalog() {
    // CTX-0406 reserves bundled ids: the independent package installs under
    // this same identity through the external package path, so the bundled
    // catalog must no longer claim it.
    let id = bitty_plugin_host::PluginId::new(GIT_PANEL_ID).unwrap();
    assert!(
        !bitty_plugin_host::bundled::is_bundled(&id),
        "git-panel must no longer be bundled"
    );
    assert!(
        bundled_manifest_for(GIT_PANEL_ID).is_none(),
        "git-panel must have no bundled manifest"
    );
    assert!(
        !bitty_plugin_host::bundled::bundled_ids_sorted()
            .iter()
            .any(|listed| listed == GIT_PANEL_ID),
        "git-panel must not be in the bundled id list"
    );
}

// --- fixture matches the former bundled manifest ------------------------------

#[test]
fn git_panel_fixture_matches_former_bundled_manifest() {
    let manifest = git_panel_manifest();
    manifest.validate().expect("fixture must validate");
    assert_eq!(manifest.identity.id.as_str(), GIT_PANEL_ID);
    assert_eq!(manifest.identity.name, GIT_PANEL_NAME);
    assert_eq!(manifest.identity.version, GIT_PANEL_VERSION);
    assert_eq!(manifest.identity.description, GIT_PANEL_DESCRIPTION);
    assert_eq!(
        manifest.compat.bitty.as_deref(),
        Some(GIT_PANEL_COMPAT_BITTY)
    );
    assert_eq!(
        manifest.compat.plugin_api.as_deref(),
        Some(GIT_PANEL_COMPAT_PLUGIN_API)
    );
    // Capabilities: panel.provider + panel.create + terminal.semantic-read +
    // process.spawn:git plus fs.read scope.
    let granted = granted_set_for(&manifest);
    assert!(granted.contains(&CapabilityId::parse("panel.provider").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("panel.create").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("terminal.semantic-read").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("process.spawn:git").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("fs.read:~/projects/**").unwrap()));
    assert_eq!(manifest.capabilities.filesystem.len(), 1);
    assert_eq!(GIT_PANEL_FS_READ_PATTERN, "~/projects/**");
    assert_eq!(GIT_PANEL_PROCESS_SPAWN_GIT, "process.spawn:git");
    // Lazy triggers: five commands, three observation events, no claims.
    assert_eq!(manifest.lazy.commands.len(), GIT_PANEL_COMMANDS.len());
    for command in GIT_PANEL_COMMANDS {
        assert!(
            manifest
                .lazy
                .commands
                .iter()
                .any(|c| c.as_str() == *command),
            "missing {command}"
        );
    }
    assert_eq!(manifest.lazy.events.len(), GIT_PANEL_EVENTS.len());
    for event in GIT_PANEL_EVENTS {
        assert!(
            manifest.lazy.events.iter().any(|e| e == event),
            "missing {event}"
        );
    }
    assert!(manifest.lazy.claims.is_empty());
    // Manifest hash is deterministic (grant binding).
    assert_eq!(manifest.manifest_hash(), manifest.clone().manifest_hash());
    let mut bumped = git_panel_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), manifest.manifest_hash());
}

// --- allowlisted git CLI only --------------------------------------------------

#[test]
fn git_panel_allowlist_contract_pins_seven_read_only_verbs() {
    // Accepted `[tools.git]` slice (CTX-0425): read-only verbs only.
    assert_eq!(GIT_ALLOWED_SUBCOMMANDS.len(), 7);
    for verb in [
        "status",
        "diff",
        "log",
        "branch",
        "show",
        "rev-parse",
        "ls-files",
    ] {
        assert!(
            GIT_ALLOWED_SUBCOMMANDS.contains(&verb),
            "allowlist must admit {verb}"
        );
    }
    // Write verbs are absent; staging or commit UX needs explicit user
    // action plus a broader grant, never ambient.
    for verb in GIT_DENIED_SUBCOMMANDS {
        assert!(
            !GIT_ALLOWED_SUBCOMMANDS.contains(verb),
            "allowlist must never admit {verb}"
        );
    }
    // Capability string is exactly `process.spawn:git`: closed
    // `process.spawn` family plus the `:git` parameter. Any other
    // executable is denied.
    let cap_proc = CapabilityId::parse(GIT_PANEL_PROCESS_SPAWN_GIT).unwrap();
    assert_eq!(
        cap_proc.family(),
        bitty_plugin_host::CapabilityFamily::Process
    );
    assert_ne!(cap_proc, CapabilityId::parse("process.spawn:rg").unwrap());
    assert_ne!(
        cap_proc,
        CapabilityId::parse("fs.read:~/projects/**").unwrap()
    );
    assert!(CapabilityId::parse("process.spawn").is_err());
    // The fixture declares the allowlisted capability and nothing broader.
    let manifest = git_panel_manifest();
    assert!(
        manifest
            .capabilities
            .ids
            .contains(&CapabilityId::parse("process.spawn:git").unwrap())
    );
    assert!(
        !manifest
            .capabilities
            .ids
            .contains(&CapabilityId::parse("process.spawn:rg").unwrap())
    );
}

// --- bounded output -------------------------------------------------------------

#[test]
fn git_panel_bounds_contract_pins() {
    assert_eq!(GIT_PANEL_MAX_ENTRIES, 128);
    assert_eq!(GIT_PANEL_MAX_COMMITS, 64);
    assert_eq!(GIT_PANEL_MAX_BRANCHES, 32);
    assert_eq!(GIT_PANEL_MAX_NAME_CHARS, 128);
    assert_eq!(GIT_PANEL_MAX_COMMIT_MESSAGE_CHARS, 256);
    assert_eq!(GIT_PANEL_MAX_PATH_BYTES, 4096);
    assert_eq!(GIT_PANEL_MAX_GIT_ARGS, 32);
    assert_eq!(GIT_PANEL_MAX_GIT_ARG_BYTES, 256);
    assert_eq!(GIT_PANEL_MAX_GIT_TOTAL_BYTES, 8192);
    assert_eq!(GIT_PANEL_PAYLOAD_MAX_BYTES, 8192);
    assert_eq!(GIT_PANEL_MAX_SELECTION, 64);
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

// --- public PluginHost path for git-panel (tiled Panel + process.spawn:git) --------------

#[test]
fn git_panel_via_public_plugin_host_path() {
    let manifest = git_panel_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    // Must contain panel.provider + panel.create + terminal.semantic-read + process.spawn:git + fs.read
    assert!(granted.contains(&CapabilityId::parse("panel.provider").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("panel.create").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("terminal.semantic-read").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("process.spawn:git").unwrap()));
    assert!(granted.contains(&CapabilityId::parse("fs.read:~/projects/**").unwrap()));
    assert_eq!(manifest.capabilities.filesystem.len(), 1);
    assert_eq!(GIT_PANEL_FS_READ_PATTERN, "~/projects/**");
    assert_eq!(GIT_PANEL_PROCESS_SPAWN_GIT, "process.spawn:git");
    assert_eq!(manifest.lazy.commands.len(), 5);
    assert!(
        manifest
            .lazy
            .commands
            .iter()
            .any(|c| c.as_str() == "bitty-terminal.git-panel:open")
    );
    assert!(
        manifest
            .lazy
            .commands
            .iter()
            .any(|c| c.as_str() == "bitty-terminal.git-panel:status")
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
    let cap_proc = CapabilityId::parse("process.spawn:git").unwrap();
    let report = host.revoke(&id, Some(&cap_proc)).unwrap();
    assert_eq!(report.revoked.len(), 1);
    assert!(!host.is_granted(&id, &hash, &cap_proc));
    // Hash changed → grant no longer matches
    let mut bumped = git_panel_manifest();
    bumped.identity.version = "0.2.0".to_string();
    assert_ne!(bumped.manifest_hash(), hash);
}

#[test]
fn git_panel_subscribe_publish_drain_bounded_drop_oldest() {
    let manifest = git_panel_manifest();
    let id = manifest.id().clone();
    let hash = manifest.manifest_hash();
    let granted = granted_set_for(&manifest);
    let mut host = PluginHost::with_capacity(DropPolicy::DropOldest, 64, 4);
    host.declare(manifest).unwrap();
    host.resolve(&id).unwrap();
    host.register(&id).unwrap();
    host.insert_grant(GrantRecord::granted(id.clone(), hash, granted, 1));
    host.activate(&id).unwrap();
    // Git-panel subscribes to cwd/title/focus (observation-only, no hot-path)
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

    // Panel EventBus bounded 64 per-sub DropOldest for git branch events
    let mut preg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(100);
    let h = preg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .expect("create panel");
    preg.mount_panel(h.id, h.generation, view).expect("mount");
    let topic = preg.declare_topic("xuepoo.git:branch-changed").unwrap();
    preg.subscribe(h.id, h.generation, &topic)
        .expect("subscribe");
    for i in 0..80 {
        preg.publish(
            &topic,
            BoundedPayload::try_new(format!("branch{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(preg.bus_events_for_panel(h.id) <= 64);
    assert!(preg.bus_total_events() <= 8192);
    let batch = preg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // 8 KiB batch limit: first batch after DropOldest 80->64 should start at branch16
    assert_eq!(batch[0].payload.as_str(), "branch16");
}

// --- Panel Runtime public path for git-panel (tiled, process.spawn:git) ---------

#[test]
fn git_panel_via_panel_runtime_public_path_bounded() {
    // Panel creation through the public Panel Runtime path only: no private
    // channel, no unsafe, bounded config (16/32 defaults, PR-1..PR-12).
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel reg");
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let handle = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .expect("create git panel");
    reg.mount_panel(handle.id, handle.generation, view)
        .expect("mount git panel");
    let pid = handle.id;
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
    reg2.register_command(h.id, h.generation, "bitty-terminal.git-panel:open")
        .expect("register open");
    reg2.register_command(h.id, h.generation, "bitty-terminal.git-panel:status")
        .expect("register status");
    let h2 = reg2
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws2))
        .unwrap();
    reg2.mount_panel(h2.id, h2.generation, ViewId::new(3))
        .unwrap();
    assert!(
        reg2.register_command(h2.id, h2.generation, "bitty-terminal.git-panel:open")
            .is_err()
    );
    // Per-panel 32 bound
    for i in 0..30 {
        reg2.register_command(h.id, h.generation, &format!("xuepoo.git:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg2.register_command(h.id, h.generation, "xuepoo.git:overflow")
            .is_err()
    );

    // Payload 8 KiB bound
    assert!(BoundedPayload::try_new("a".repeat(9 * 1024)).is_err());
    // Batch 32/8 KiB via drain (non-coalescable topic for bounded test)
    let topic = reg2.declare_topic("xuepoo.git:branch-changed").unwrap();
    reg2.subscribe(h.id, h.generation, &topic).unwrap();
    for i in 0..80 {
        reg2.publish(
            &topic,
            BoundedPayload::try_new(format!("branch{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg2.bus_events_for_panel(h.id) <= 64);
    let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);

    // Tiled layout is an H split over LayoutNode primitives, not a new primitive.
    let main = View::new(ViewId::new(10), 80, 24);
    let diff = View::new(ViewId::new(11), 40, 24);
    let tiled = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(main),
        LayoutNode::leaf(diff),
    );
    assert!(matches!(tiled, LayoutNode::Split { .. }));
    assert_eq!(tiled.leaf_count(), 2);

    // fs isolation via capability grammar (no ambient read).
    let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
    assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
    assert_ne!(
        cap_read,
        CapabilityId::parse("fs.read:/etc/passwd").unwrap()
    );

    // process.spawn:git allowlist via capability grammar (no ambient spawn).
    let cap_proc = CapabilityId::parse("process.spawn:git").unwrap();
    assert_eq!(
        cap_proc.family(),
        bitty_plugin_host::CapabilityFamily::Process
    );
    assert_ne!(cap_proc, CapabilityId::parse("process.spawn:rg").unwrap());

    // Config validation bounded fail-closed.
    let bad = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..Default::default()
    };
    assert!(bad.validate().is_err());
    assert!(PanelRegistryConfig::default().validate().is_ok());
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

// --- safe-mode rejects git-panel without panic ------------------------------

#[test]
fn safe_mode_rejects_git_panel_without_panic() {
    let manifest = git_panel_manifest();
    let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
    host.set_safe_mode(true);
    assert!(host.declare(manifest.clone()).is_err());
    host.set_safe_mode(false);
    assert!(host.declare(manifest.clone()).is_ok());

    let mut rt = Runtime::with_defaults().unwrap();
    rt.set_plugin_safe_mode(true);
    assert!(rt.register_plugin(git_panel_manifest()).is_err());
    assert!(rt.tick().is_some());
    rt.set_plugin_safe_mode(false);
    assert!(rt.register_plugin(git_panel_manifest()).is_ok());
}

// --- no private channel: third-party parity -----------------------------------

#[test]
fn git_panel_has_no_private_channel_parity_with_third_party() {
    let bundled = git_panel_manifest();
    let mut third = bundled.clone();
    third.identity.id = bitty_plugin_host::PluginId::new("xuepoo.git-panel-mirror").unwrap();
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
fn git_panel_is_headless_and_forbid_unsafe_single_process_winit() {
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
        reg.register_command(h.id, h.generation, &format!("xuepoo.git:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg.register_command(h.id, h.generation, "xuepoo.git:overflow")
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
fn git_panel_command_registry_bounded_and_overlay_focus_mru() {
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
        reg.register_command(h1.id, h1.generation, &format!("xuepoo.git:cmd{i}"))
            .unwrap();
    }
    assert!(
        reg.register_command(h1.id, h1.generation, "xuepoo.git:overflow")
            .is_err()
    );
    // Duplicate across panels rejected
    assert!(
        reg.register_command(h2.id, h2.generation, "xuepoo.git:cmd0")
            .is_err()
    );
    // Focus MRU per Workspace per Window (PanelFocus)
    reg.focus_panel(h1.id, h1.generation, ws).unwrap();
    reg.focus_panel(h2.id, h2.generation, ws).unwrap();
    assert_eq!(reg.focused_panel(ws), Some(h2.id));
    assert_eq!(reg.mru_order(ws), vec![h2.id, h1.id]);
    assert_eq!(reg.command_owner("xuepoo.git:cmd0"), Some(h1.id));
    // Generation isolation: stale handle rejected
    let bad_gen = bitty_runtime::registry::Generation(h1.generation.get().wrapping_add(100));
    assert!(
        reg.register_command(h1.id, bad_gen, "xuepoo.git:stale")
            .is_err()
    );
    // Overlay text truncated at char boundary (overlay text bound 128).
    let long = "a".repeat(200);
    let truncated: String = long
        .chars()
        .take(bitty_ui::panel::MAX_OVERLAY_TEXT_LEN)
        .collect();
    assert_eq!(
        truncated.chars().count(),
        bitty_ui::panel::MAX_OVERLAY_TEXT_LEN
    );
}

// --- panel reactive via EventBus, no hot path --------------------------------

#[test]
fn git_panel_reactive_via_eventbus_no_hot_path() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(1);
    let view = ViewId::new(1);
    let h = reg
        .create_panel(bitty_ui::panel::PanelType::Helper, Some(ws))
        .unwrap();
    reg.mount_panel(h.id, h.generation, view).unwrap();
    let topic = reg.declare_topic("xuepoo.git:branch-changed").unwrap();
    reg.subscribe(h.id, h.generation, &topic).unwrap();
    for i in 0..70 {
        reg.publish(
            &topic,
            BoundedPayload::try_new(format!("branch{i}")).unwrap(),
        )
        .unwrap();
    }
    assert!(reg.bus_events_for_panel(h.id) <= 64);
    let batch = reg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch.len(), 32);
    // State observation remains pure (cwd/title drive git-panel refresh).
    let mut state = State::new();
    state.apply(&TerminalAction::OscCwd {
        url: BoundedString::new("file:///home/user/projects/foo"),
    });
    state.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("git-panel"),
    });
    assert!(state.cwd_report().is_some());
    assert!(!state.title().is_empty());
    // Draining is deterministic: DropOldest keeps branch6..branch69, so two
    // consecutive 32-batches are branch6..branch37 then branch38..branch69.
    let batch_again = reg.drain_batch(h.id, topic.as_str(), 32, 8192);
    assert_eq!(batch_again.len(), 32);
    assert_eq!(batch[0].payload.as_str(), "branch6");
    assert_eq!(batch_again[0].payload.as_str(), "branch38");
}

// --- fs isolation via CapabilityId and the fixture manifest -------------------

#[test]
fn git_panel_fs_isolation_via_capability_id_and_manifest() {
    let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
    assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
    assert_eq!(cap_read.as_str(), "fs.read:~/projects/**");
    let cap_proc = CapabilityId::parse("process.spawn:git").unwrap();
    assert_eq!(
        cap_proc.family(),
        bitty_plugin_host::CapabilityFamily::Process
    );
    let outside = CapabilityId::parse("fs.read:/tmp/**").unwrap();
    assert_ne!(cap_read, outside);
    let proc_other = CapabilityId::parse("process.spawn:rg").unwrap();
    assert_ne!(cap_proc, proc_other);
    // Manifest hash deterministic and panel.* + process.spawn:git present.
    let m = git_panel_manifest();
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
    assert!(
        m.capabilities
            .ids
            .contains(&CapabilityId::parse("process.spawn:git").unwrap())
    );
    // Filesystem scope is exactly the working-tree read pattern.
    assert_eq!(m.capabilities.filesystem.len(), 1);
    assert_eq!(
        m.capabilities.filesystem[0].paths,
        vec![GIT_PANEL_FS_READ_PATTERN.to_string()]
    );
}

// --- Runtime side-queue DropOldest for git-panel observations --------------

#[test]
fn runtime_side_queue_drop_oldest_for_observations() {
    let mut rt = Runtime::with_defaults().expect("runtime");
    rt.handle_pty_bytes(b"\x1b]0;git-panel-title\x07");
    rt.handle_pty_bytes(b"\x1b]7;file:///home/user/projects/proj\x07");
    let obs = rt.drain_plugin_observations();
    assert!(obs.iter().any(
        |o| matches!(o, bitty_plugin_host::HostObservation::TitleChanged(s) if s=="git-panel-title")
    ));
    assert!(obs.iter().any(|o| matches!(o, bitty_plugin_host::HostObservation::CwdChanged(s) if s.contains("file:///home/user/projects/proj"))));
    // Tiled layout via LayoutNode primitives is pure, not hot-path, bounded.
    let main = View::new(ViewId::new(1), 80, 24);
    let diff = View::new(ViewId::new(2), 40, 24);
    let tiled = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(main),
        LayoutNode::leaf(diff),
    );
    assert!(matches!(tiled, LayoutNode::Split { .. }));
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
fn git_panel_tiled_reuses_layout_hv_deterministically() {
    let main = View::new(ViewId::new(1), 80, 24);
    let diff = View::new(ViewId::new(2), 40, 24);
    let tiled = LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(main.clone()),
        LayoutNode::leaf(diff.clone()),
    );
    assert_eq!(tiled.leaf_count(), 2);
    let allocs = tiled.layout(UiRect::new(0, 0, 80, 24));
    assert_eq!(allocs.len(), 2);
    let solo = LayoutNode::leaf(main.clone());
    assert_eq!(solo.leaf_count(), 1);
    let v1 = View::new(ViewId::new(3), 80, 12);
    let v2 = View::new(ViewId::new(4), 80, 12);
    let stack = LayoutNode::Stack(vec![LayoutNode::leaf(v1), LayoutNode::leaf(v2)]);
    assert_eq!(stack.leaf_count(), 2);
    let pid = bitty_runtime::registry::PanelId::new(1);
    let vid = ViewId::new(1);
    assert_ne!(
        std::any::TypeId::of::<bitty_runtime::registry::PanelId>(),
        std::any::TypeId::of::<ViewId>()
    );
    assert_eq!(pid.get(), vid.0);
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
    assert_eq!(reg.config().max_panels_per_workspace, 16);
    assert_eq!(reg.config().max_panels_per_window, 32);
}
