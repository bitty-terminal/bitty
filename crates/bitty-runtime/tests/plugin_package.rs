//! End-to-end package-manager proof (CTX-0406): a real externally authored
//! plugin is installed from a local directory into the XDG store, discovered
//! and activated by the runtime, exercised, updated, reloaded, disabled, and
//! uninstalled. The recorded capability grant is enforced at activation.
//!
//! Everything here is deterministic and offline: the "external" source is a
//! local directory copied by the package manager, and the update proof swaps
//! the staged version in place.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, HostServices, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::package::{
    LocalInstallOptions, install_local_dir, set_enabled, uninstall,
};
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource, load_index,
    write_index,
};

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

const PLUGIN_ID: &str = "xuepoo.external";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-package-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Write a minimal externally authored plugin source directory.
fn write_external_source(
    root: &Path,
    version: &str,
    greeting: &str,
    capability: Option<&str>,
) -> PathBuf {
    let source = root.join(format!("external-{version}"));
    let _ = std::fs::remove_dir_all(&source);
    std::fs::create_dir_all(source.join("lua")).expect("lua dir");
    let capabilities = capability
        .map(|capability| format!("[capabilities]\n{capability} = true\n"))
        .unwrap_or_default();
    let manifest = format!(
        "[plugin]\n\
         id = \"{PLUGIN_ID}\"\n\
         name = \"External Fixture\"\n\
         version = \"{version}\"\n\
         description = \"externally authored test plugin\"\n\n\
         [compat]\n\
         bitty = \">=0.0.1,<1.0\"\n\
         plugin-api = \"^1.0\"\n\n\
         {capabilities}\n\
         [lazy]\n\
         commands = [\"{PLUGIN_ID}:greet\"]\n\
         events = []\n"
    );
    std::fs::write(source.join("bitty-plugin.toml"), manifest).expect("manifest");
    std::fs::write(
        source.join("lua/init.lua"),
        format!(
            "bitty.commands.register({{\n  id = \"greet\",\n  title = \"Greet\",\n  \
             run = function() return \"{greeting}\" end,\n}})\nreturn {{}}\n"
        ),
    )
    .expect("init");
    source
}

fn runtime(store_root: PathBuf, safe_mode: bool, tag: &str) -> PluginRuntime {
    let data = scratch(&format!("state-{tag}"));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: Some(data),
        store_root: Some(store_root),
        bundled_roots: Vec::new(),
        third_party_roots: Vec::new(),
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([(
            "version",
            LuaValue::Integer(1),
        )]))),
    })
}

#[test]
fn external_plugin_install_load_run_update_reload_disable_uninstall() {
    let bench = scratch("e2e");
    let store = bench.join("store");
    let id = PluginId::new(PLUGIN_ID).expect("id");

    // Install v1 from an external local directory.
    let source_v1 = write_external_source(&bench, "1.0.0", "hello v1", None);
    let report = install_local_dir(
        &store,
        &source_v1,
        &LocalInstallOptions::default(),
        &mut |_| Ok(true),
    )
    .expect("install")
    .expect("approved");
    assert_eq!(report.version, "1.0.0");
    assert!(!report.updated);
    assert!(
        store
            .join("packages/xuepoo.external/1.0.0/lua/init.lua")
            .is_file()
    );

    // The runtime discovers and activates it, and the command runs for real.
    let mut rt = runtime(store.clone(), false, "e2e");
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Unloaded));
    let activated = rt.activate(&id).expect("activate");
    assert_eq!(activated.state, LifecycleState::Active);
    assert_eq!(activated.commands, 1);
    let greeting = rt.dispatch_command(&id, "greet", &[]).expect("dispatch");
    assert_eq!(greeting, LuaValue::String("hello v1".to_string()));

    // Update to v2 through a second local directory.
    let source_v2 = write_external_source(&bench, "2.0.0", "hello v2", None);
    let update = install_local_dir(
        &store,
        &source_v2,
        &LocalInstallOptions::default(),
        &mut |_| Ok(true),
    )
    .expect("update")
    .expect("approved");
    assert!(update.updated);
    assert_eq!(update.previous_version.as_deref(), Some("1.0.0"));
    assert!(
        store.join("packages/xuepoo.external/1.0.0").is_dir(),
        "previous version is retained"
    );

    // Reload tears down generation N and activates the new revision.
    let reloaded = rt.reload(&id).expect("reload");
    assert_eq!(reloaded.state, LifecycleState::Active);
    let greeting = rt.dispatch_command(&id, "greet", &[]).expect("dispatch");
    assert_eq!(greeting, LuaValue::String("hello v2".to_string()));

    // Disable: the desired state flips atomically and discovery skips it.
    assert!(set_enabled(&store, PLUGIN_ID, false).expect("disable"));
    let mut disabled = runtime(store.clone(), false, "disabled");
    assert_eq!(
        disabled.discover().len(),
        0,
        "disabled record must not load"
    );
    assert!(set_enabled(&store, PLUGIN_ID, true).expect("re-enable"));

    // Uninstall removes both the record and the staged tree.
    let removed = uninstall(&store, PLUGIN_ID).expect("uninstall");
    assert!(removed.tree_removed);
    assert!(load_index(&store).expect("index").is_empty());
    assert!(!store.join("packages/xuepoo.external").exists());
    let mut empty = runtime(store, false, "empty");
    assert_eq!(empty.discover().len(), 0);

    let _ = std::fs::remove_dir_all(&bench);
}

#[test]
fn recorded_grant_is_enforced_at_activation() {
    let bench = scratch("grant");
    let store = bench.join("store");
    let id = PluginId::new(PLUGIN_ID).expect("id");
    let source = write_external_source(&bench, "1.0.0", "hello", Some("terminal.semantic-read"));
    install_local_dir(
        &store,
        &source,
        &LocalInstallOptions::default(),
        &mut |_| Ok(true),
    )
    .expect("install")
    .expect("approved");

    // Full grant: the capability-gated host call is allowed.
    let mut granted_rt = runtime(store.clone(), false, "granted");
    granted_rt.discover();
    granted_rt.activate(&id).expect("activate");
    let services = granted_rt.services(&id).cloned().expect("services");
    assert!(
        services.terminal_snapshot("semantic").is_ok(),
        "declared and granted capability must be usable"
    );

    // Narrowed grant in the store record: the host policy rejects activation
    // before any VM is created (fail closed, deny by default).
    let mut records = load_index(&store).expect("index");
    records[0].granted.clear();
    write_index(&store, &records).expect("rewrite index");
    let mut narrowed_rt = runtime(store.clone(), false, "narrowed");
    narrowed_rt.discover();
    assert!(
        narrowed_rt.activate(&id).is_err(),
        "a record that does not cover the declared capabilities must fail closed"
    );

    // A record that grants an undeclared capability is store tampering.
    records[0].granted = vec!["platform.notify".to_string()];
    write_index(&store, &records).expect("rewrite index");
    let mut tampered_rt = runtime(store, false, "tampered");
    tampered_rt.discover();
    assert!(
        tampered_rt.activate(&id).is_err(),
        "undeclared recorded grant must fail closed"
    );

    let _ = std::fs::remove_dir_all(&bench);
}
