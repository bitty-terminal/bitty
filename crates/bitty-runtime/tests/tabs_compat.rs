#![forbid(unsafe_code)]
#![allow(deprecated)]
//! Tabs compat alias test (CTX-0240, DEC-0032, CTX-0239 RFC).
//!
//! ALIAS, not flag-day (removal ≥ v0.2.0). Old id `bitty-terminal.tabs`,
//! old commands `bitty-terminal.tabs:*`, old claim `tabline`, old consts
//! `TABS_*`, old type `TabsIntegration`, and old fns `create_tabs_panel` /
//! `validate_tabs_panel_config` remain as deprecated shims delegating to the
//! `workspace` canonical names. This test is the guard: old + new resolve
//! with identical activation/frames, old emits deprecation, new does not.
//! New code must use `workspace`; test-only `xuepoo.tabs:*` strings get NO
//! compat promise and have been renamed outright.

use std::collections::BTreeSet;

use bitty_plugin_host::{
    CapabilityId, DropPolicy, GrantRecord, PluginHost,
    bundled::{
        TABS_COMMANDS, WORKSPACE_COMMANDS, bundled_manifest_for, canonicalize_ui_claim,
        canonicalize_workspace_command, deprecated_alias_warning, is_bundled,
        is_deprecated_bundled_alias, is_deprecated_claim, is_deprecated_command, tabs_manifest,
        workspace_manifest,
    },
};
use bitty_runtime::{
    Runtime,
    registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId},
    tabs::{
        TABS_MAX_PANELS_PER_WINDOW, TABS_MAX_PANELS_PER_WORKSPACE, TABS_MAX_TABS,
        TABS_PAYLOAD_MAX_BYTES, TABS_TITLE_MAX_CHARS, TabsIntegration, create_tabs_panel,
        validate_tabs_panel_config,
    },
    workspace::{
        WORKSPACE_MAX_PANELS_PER_WINDOW, WORKSPACE_MAX_PANELS_PER_WORKSPACE, WORKSPACE_MAX_TABS,
        WORKSPACE_PAYLOAD_MAX_BYTES, WORKSPACE_TITLE_MAX_CHARS, WorkspaceIntegration,
        create_workspace_panel, validate_workspace_panel_config,
    },
};
use bitty_ui::{View, ViewId};

fn granted_set_for(manifest: &bitty_plugin_host::PluginManifest) -> BTreeSet<CapabilityId> {
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
fn old_and_new_ids_both_resolve_with_deprecation_shape() {
    let old = bundled_manifest_for("bitty-terminal.tabs").expect("old alias resolves");
    let new = bundled_manifest_for("bitty-terminal.workspace").expect("new canonical resolves");
    assert_eq!(old.identity.id.as_str(), "bitty-terminal.tabs");
    assert_eq!(new.identity.id.as_str(), "bitty-terminal.workspace");
    assert!(is_bundled(&old.identity.id));
    assert!(is_bundled(&new.identity.id));
    assert!(is_deprecated_bundled_alias("bitty-terminal.tabs"));
    assert!(!is_deprecated_bundled_alias("bitty-terminal.workspace"));
    assert!(deprecated_alias_warning("bitty-terminal.tabs").is_some());
    assert!(deprecated_alias_warning("bitty-terminal.workspace").is_none());
    // Same capability shape; different ids => different hashes (grant hash-binding preserved).
    assert_eq!(
        old.capabilities.ids, new.capabilities.ids,
        "alias must not change authority"
    );
    assert_ne!(
        old.manifest_hash(),
        new.manifest_hash(),
        "different ids must hash differently (hash-bound grants)"
    );
}

#[test]
fn old_and_new_commands_dispatch_identically() {
    // Manifests list both new (3) and old (3) so `inspect command` shows both.
    let new = workspace_manifest();
    let old = tabs_manifest();
    for cmd in WORKSPACE_COMMANDS.iter().chain(TABS_COMMANDS.iter()) {
        assert!(
            new.lazy.commands.iter().any(|c| c.as_str() == *cmd),
            "workspace manifest missing {cmd}"
        );
        assert!(
            old.lazy.commands.iter().any(|c| c.as_str() == *cmd),
            "tabs shim missing {cmd}"
        );
    }
    assert_eq!(new.lazy.commands.len(), 6);
    assert_eq!(old.lazy.commands.len(), 6);
    // Canonicalization: old maps to new, new maps to itself.
    for (old_cmd, new_cmd) in [
        ("bitty-terminal.tabs:new", "bitty-terminal.workspace:new"),
        (
            "bitty-terminal.tabs:close",
            "bitty-terminal.workspace:close",
        ),
        ("bitty-terminal.tabs:next", "bitty-terminal.workspace:next"),
    ] {
        assert_eq!(canonicalize_workspace_command(old_cmd), Some(new_cmd));
        assert_eq!(canonicalize_workspace_command(new_cmd), Some(new_cmd));
        assert!(is_deprecated_command(old_cmd));
        assert!(!is_deprecated_command(new_cmd));
    }
    // Registry lifecycle identical: declare → resolve → register → grant → activate.
    for manifest in [workspace_manifest(), tabs_manifest()] {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
        host.declare(manifest).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        assert!(host.activate(&id).is_err(), "must require grant");
        host.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
        host.activate(&id).expect("activate after grant");
    }
    // `inspect command` lists both forms via the canonical manifest (which carries all 6).
    let canonical_cmds: Vec<String> = bitty_plugin_host::bundled::all_bundled_manifests()
        .into_iter()
        .flat_map(|m| m.lazy.commands.into_iter().map(|c| c.as_str().to_string()))
        .collect();
    for cmd in WORKSPACE_COMMANDS.iter().chain(TABS_COMMANDS.iter()) {
        assert!(
            canonical_cmds.iter().any(|c| c == cmd),
            "inspect command must list {cmd}"
        );
    }
}

#[test]
fn old_and_new_claims_canonicalize_to_workspaceline() {
    let new = workspace_manifest();
    assert!(new.lazy.claims.contains(&"workspaceline".to_string()));
    assert!(new.lazy.claims.contains(&"tabline".to_string()));
    let old = tabs_manifest();
    assert!(old.lazy.claims.contains(&"workspaceline".to_string()));
    assert!(old.lazy.claims.contains(&"tabline".to_string()));
    // Effective claim is workspaceline; old emits deprecation.
    assert_eq!(
        canonicalize_ui_claim("workspaceline"),
        Some("workspaceline")
    );
    assert_eq!(canonicalize_ui_claim("tabline"), Some("workspaceline"));
    assert!(is_deprecated_claim("tabline"));
    assert!(!is_deprecated_claim("workspaceline"));
    assert_eq!(canonicalize_ui_claim("nope"), None);
}

#[test]
fn deprecated_consts_types_and_fns_delegate_identically() {
    assert_eq!(TABS_MAX_TABS, WORKSPACE_MAX_TABS);
    assert_eq!(
        TABS_MAX_PANELS_PER_WORKSPACE,
        WORKSPACE_MAX_PANELS_PER_WORKSPACE
    );
    assert_eq!(TABS_MAX_PANELS_PER_WINDOW, WORKSPACE_MAX_PANELS_PER_WINDOW);
    assert_eq!(TABS_PAYLOAD_MAX_BYTES, WORKSPACE_PAYLOAD_MAX_BYTES);
    assert_eq!(TABS_TITLE_MAX_CHARS, WORKSPACE_TITLE_MAX_CHARS);

    let views = vec![
        View::new(ViewId::new(1), 80, 24),
        View::new(ViewId::new(2), 80, 24),
    ];
    let via_old = TabsIntegration::stack_for_tabs(views.clone());
    let via_new = WorkspaceIntegration::stack_for_workspace(views);
    assert_eq!(
        via_old.layout(bitty_ui::Rect::new(0, 0, 80, 24)),
        via_new.layout(bitty_ui::Rect::new(0, 0, 80, 24))
    );
    assert_eq!(
        TabsIntegration::tab_count(&via_old),
        WorkspaceIntegration::workspace_count(&via_new)
    );
    assert_eq!(
        TabsIntegration::tab_ids(&via_old),
        WorkspaceIntegration::workspace_ids(&via_new)
    );
    assert_eq!(
        WorkspaceIntegration::is_stack(&via_old),
        WorkspaceIntegration::is_stack(&via_new)
    );
    assert_eq!(
        TabsIntegration::has_tabs(&via_old),
        WorkspaceIntegration::has_workspaces(&via_new)
    );
    assert_eq!(
        TabsIntegration::find_tab(&via_old, ViewId::new(1)).map(|v| v.id()),
        WorkspaceIntegration::find_workspace(&via_new, ViewId::new(1)).map(|v| v.id())
    );

    let mut state = bitty_term_state::State::new();
    state.apply(&bitty_term_state::TerminalAction::OscTitle {
        text: bitty_vt::BoundedString::new("hello"),
    });
    assert_eq!(
        TabsIntegration::tab_title(&state),
        WorkspaceIntegration::workspace_title(&state)
    );

    // Panel fns delegate: both create + validate identically.
    let mut reg_old = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let mut reg_new = PanelRegistry::new(PanelRegistryConfig::default()).unwrap();
    let ws = WorkspaceId::new(7);
    let view = ViewId::new(7);
    let pid_old = create_tabs_panel(&mut reg_old, ws, view).expect("old fn");
    let pid_new = create_workspace_panel(&mut reg_new, ws, view).expect("new fn");
    assert_eq!(reg_old.panel_count(), reg_new.panel_count());
    assert_eq!(pid_old.get(), pid_new.get());
    assert!(validate_tabs_panel_config(&PanelRegistryConfig::default()).is_ok());
    assert!(validate_workspace_panel_config(&PanelRegistryConfig::default()).is_ok());
    let bad = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..Default::default()
    };
    assert!(validate_tabs_panel_config(&bad).is_err());
    assert!(validate_workspace_panel_config(&bad).is_err());
}

#[test]
fn grants_preserved_across_alias_hash_bound() {
    // Old grant activates old manifest; new grant activates new manifest.
    // Hash mismatch (version bump) fails for both — no bypass via alias.
    for manifest in [workspace_manifest(), tabs_manifest()] {
        let id = manifest.id().clone();
        let hash = manifest.manifest_hash();
        let granted = granted_set_for(&manifest);
        let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
        host.declare(manifest.clone()).unwrap();
        host.resolve(&id).unwrap();
        host.register(&id).unwrap();
        host.insert_grant(GrantRecord::granted(id.clone(), hash.clone(), granted, 1));
        host.activate(&id).expect("grant must activate");
        let cap = CapabilityId::parse("ui.rich").unwrap();
        assert!(host.is_granted(&id, &hash, &cap));
        // Wrong hash fails.
        assert!(!host.is_granted(&id, "other-hash", &cap));
    }
}

#[test]
fn safe_mode_rejects_both_ids_without_promotion() {
    // Preserve reject-shape: whatever `--safe` rejects today it still rejects
    // (both ids), still tickable after rejection; no new bypass.
    for id in ["bitty-terminal.tabs", "bitty-terminal.workspace"] {
        let manifest = bundled_manifest_for(id).unwrap();
        let mut host = PluginHost::new(DropPolicy::DropOldest, 16);
        host.set_safe_mode(true);
        assert!(
            host.declare(manifest).is_err(),
            "{id} must be rejected in safe-mode"
        );
        let mut rt = Runtime::with_defaults().unwrap();
        rt.set_plugin_safe_mode(true);
        assert!(
            rt.register_plugin(bundled_manifest_for(id).unwrap())
                .is_err(),
            "runtime must reject {id} in safe-mode"
        );
        assert!(
            rt.tick().is_some(),
            "runtime remains tickable after safe-mode rejection for {id}"
        );
    }
}
