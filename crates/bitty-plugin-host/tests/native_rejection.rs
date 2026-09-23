#![forbid(unsafe_code)]

//! Native-plugin rejection audit — CTX-0619 (SEC-14, `R-017`, `P0-AC-018`).
//!
//! `P0-AC-018` pass threshold: each of `.so` / `.dll` / `.dylib` is rejected
//! at install and at activation, and no native-loading code path is reachable
//! from plugin packages.
//!
//! What this file pins, by seam:
//! - **Install**: [`reject_native_artifact_files`] rejects every artifact
//!   type (plus case-fold, versioned-`.so`, and path-nested evasions) with a
//!   `native_reject` stage error before any pipeline stage runs; benign Lua
//!   package names pass.
//! - **Activation**: a manifest whose `[tools.*]` declaration names a native
//!   artifact is rejected at `declare` (tool names admit no `.`, so no
//!   extension can survive validation) and therefore never reaches
//!   `resolve` / `register` / `activate`. Activation itself takes only a
//!   [`PluginId`] and decides purely on registry + grant state — there is no
//!   loader argument, no file lookup, and no `dlopen` reachable (the crate
//!   has no native-loading dependency and forbids `unsafe`).

use std::collections::BTreeSet;

use bitty_plugin_host::{
    CapabilityId, CapabilityRequests, Compat, DropPolicy, GrantRecord, LazyTriggers, PluginHost,
    PluginId, PluginIdentity, PluginManifest, PluginState, ToolDeclaration,
    is_native_artifact_file_name, reject_native_artifact_files,
};

fn minimal_manifest(id: &str) -> PluginManifest {
    PluginManifest {
        identity: PluginIdentity {
            id: PluginId::new(id).expect("valid plugin id"),
            name: "Native audit fixture".to_string(),
            version: "0.1.0".to_string(),
            description: "R-017 activation-path fixture".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(">=0.5,<1.0".to_string()),
            plugin_api: Some("^1.0".to_string()),
        },
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: CapabilityRequests::default(),
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: Vec::new(),
            claims: Vec::new(),
        },
        raw_bytes_len: 256,
    }
}

fn host() -> PluginHost {
    PluginHost::new(DropPolicy::DropOldest, 16)
}

// ── install seam ────────────────────────────────────────────────────────────

#[test]
fn install_rejects_each_native_artifact_type() {
    for name in ["payload.so", "payload.dll", "payload.dylib"] {
        assert!(
            is_native_artifact_file_name(name),
            "{name} must be rejected at install"
        );
        reject_native_artifact_files([name]).expect_err("native artifact must block install");
    }
}

#[test]
fn install_rejects_native_evasion_spellings() {
    for name in [
        "PAYLOAD.SO",
        "Payload.Dll",
        "LIB.DYLIB",
        "libevil.so.1",
        "libevil.so.1.2",
        "nested/dir/payload.so",
        "nested\\dir\\payload.dll",
    ] {
        assert!(
            is_native_artifact_file_name(name),
            "{name} must be rejected at install"
        );
    }
}

#[test]
fn install_passes_lua_package_names() {
    let benign = [
        "init.lua",
        "main.lua",
        "README.md",
        "icon.png",
        "locales/en.toml",
    ];
    for name in benign {
        assert!(
            !is_native_artifact_file_name(name),
            "{name} must pass the native screen"
        );
    }
    reject_native_artifact_files(benign).expect("benign names must pass");
}

// ── activation seam ─────────────────────────────────────────────────────────

#[test]
fn native_tool_names_rejected_at_declare_never_reach_activation() {
    for tool in ["evil.so", "evil.dll", "evil.dylib", "EVIL.SO"] {
        let mut manifest = minimal_manifest("xuepoo.native");
        manifest.tools = vec![ToolDeclaration {
            tool: tool.to_string(),
            required: false,
            version_req: ">=2.30".to_string(),
        }];
        let mut host = host();
        let err = host
            .declare(manifest)
            .expect_err("native tool name must be rejected at declare");
        assert!(
            err.to_string().contains(tool),
            "rejection must quote the native name, got: {err}"
        );
        // Rejected at declare: nothing was inserted, so no lifecycle exists.
        let id = PluginId::new("xuepoo.native").expect("valid plugin id");
        assert!(host.registry().get(&id).is_none());
    }
}

#[test]
fn activation_decides_on_registry_and_grants_only() {
    // A capability-bearing manifest reaches Registered, then activation
    // fails closed on the missing grant — the only authority consulted is
    // the grant store, never a file or loader.
    let mut manifest = minimal_manifest("xuepoo.gated");
    manifest
        .capabilities
        .ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    let mut host = host();
    let id = PluginId::new("xuepoo.gated").expect("valid plugin id");
    host.declare(manifest.clone()).expect("declare");
    host.resolve(&id).expect("resolve");
    host.register(&id).expect("register");
    let err = host.activate(&id).expect_err("missing grant must block");
    assert!(err.to_string().contains("missing grant record"));

    // The matching grant unblocks: outcome is a pure function of grant state.
    let mut granted = BTreeSet::new();
    granted.insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    host.insert_grant(GrantRecord::granted(
        id.clone(),
        manifest.manifest_hash(),
        granted,
        1,
    ));
    host.activate(&id).expect("granted activation must succeed");
    assert_eq!(
        host.registry().get(&id).expect("registered plugin").state,
        PluginState::Activated
    );
}

#[test]
fn capability_free_activation_needs_no_files() {
    // Least authority: no declared capabilities, no grant record, no files —
    // the full lifecycle completes on registry state alone.
    let mut host = host();
    let id = PluginId::new("xuepoo.plain").expect("valid plugin id");
    host.declare(minimal_manifest("xuepoo.plain"))
        .expect("declare");
    host.resolve(&id).expect("resolve");
    host.register(&id).expect("register");
    host.activate(&id).expect("activate without files");
    assert_eq!(
        host.registry().get(&id).expect("active plugin").state,
        PluginState::Activated
    );
}
