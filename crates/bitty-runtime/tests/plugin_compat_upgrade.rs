//! CTX-0416: host-upgrade compat re-check (install-then-upgrade simulation).
//!
//! `compat.bitty` / `compat.plugin-api` were evaluated only at install time,
//! so an installed package whose range no longer included the running host
//! after an upgrade could still resolve and activate. Resolution and
//! activation now re-evaluate the closed requirement grammar against the
//! running host and fail closed with a typed incompatible state without
//! creating a VM. The `0.1`-line floors stay as written and bundled packages
//! keep loading on the `0.0.x` dev host (DIR-019 versioning).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::package::{LocalInstallOptions, install_local_dir};
use bitty_runtime::plugin_runtime::resolution;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, PluginRuntimeError, SettingsSource,
    SnapshotSource,
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

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-compat-upgrade-{tag}-{}-{}",
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

fn write_source(root: &Path, id: &str, version: &str, bitty_req: &str, api_req: &str) -> PathBuf {
    let source = root.join(format!("src-{version}"));
    let _ = std::fs::remove_dir_all(&source);
    std::fs::create_dir_all(source.join("lua")).expect("lua dir");
    let manifest = format!(
        "[plugin]\nid = \"{id}\"\nname = \"Upgrade Fixture\"\nversion = \"{version}\"\n\
         description = \"upgrade simulation\"\n\n[compat]\nbitty = \"{bitty_req}\"\n\
         plugin-api = \"{api_req}\"\n\n[lazy]\ncommands = [\"{id}:greet\"]\nevents = []\n"
    );
    std::fs::write(source.join("bitty-plugin.toml"), manifest).expect("manifest");
    std::fs::write(
        source.join("lua/init.lua"),
        "bitty.commands.register({ id = \"greet\", title = \"Greet\", run = function() return \"hi\" end })\nreturn {}\n",
    )
    .expect("init");
    source
}

fn runtime_with_store(store: PathBuf, tag: &str) -> PluginRuntime {
    let data = scratch(&format!("state-{tag}"));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: Some(store),
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
fn install_then_upgrade_host_fails_resolve_and_activate_closed() {
    let bench = scratch("e2e");
    let store = bench.join("store");
    let id = "xuepoo.upgrade";

    // Narrow range includes the current dev host but not the upgrade.
    let source = write_source(&bench, id, "1.0.0", ">=0.0.1,<0.0.99", "^1.0");
    install_local_dir(
        &store,
        &source,
        &LocalInstallOptions::default(),
        &mut |_| Ok(true),
    )
    .expect("install on current host")
    .expect("approved");

    let records = resolution::load_index(&store).expect("index");
    assert_eq!(records.len(), 1);
    // Running host resolves.
    resolution::resolve_record(&store, &records[0]).expect("current host resolves");

    // Simulated bitty upgrade: stored range no longer includes the host.
    let upgraded = resolution::resolve_record_with_hosts(&store, &records[0], "0.1.0", "1.0.0")
        .expect_err("upgraded host must fail closed");
    assert!(
        matches!(
            upgraded,
            PluginRuntimeError::Incompatible { ref field, .. } if field == "compat.bitty"
        ),
        "expected typed compat.bitty incompatible, got {upgraded}"
    );

    // Simulated plugin-api upgrade.
    let upgraded_api =
        resolution::resolve_record_with_hosts(&store, &records[0], "0.0.20", "2.0.0")
            .expect_err("upgraded api must fail closed");
    assert!(
        matches!(
            upgraded_api,
            PluginRuntimeError::Incompatible { ref field, .. } if field == "compat.plugin-api"
        ),
        "expected typed compat.plugin-api incompatible, got {upgraded_api}"
    );

    // Discovery with the running host still activates (no upgrade in-process).
    let plugin_id = PluginId::new(id).expect("id");
    let mut rt = runtime_with_store(store.clone(), "current");
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let report = rt.activate(&plugin_id).expect("activate on current host");
    assert_eq!(report.state, LifecycleState::Active);

    let _ = std::fs::remove_dir_all(&bench);
}

#[test]
fn incompatible_dev_plugin_fails_activate_without_vm() {
    // A dev-root plugin whose range excludes the running host must fail at
    // activation with a typed incompatible state and never create a VM.
    let dev_root = scratch("dev-incompat");
    let id = "xuepoo.devstale";
    let package_dir = dev_root.join(id);
    std::fs::create_dir_all(package_dir.join("lua")).expect("dirs");
    std::fs::write(
        package_dir.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Dev Stale\"\nversion = \"0.1.0\"\n\
             description = \"stale dev\"\n\n[compat]\nbitty = \">=9.9.9,<10.0.0\"\n\
             plugin-api = \"^1.0\"\n\n[lazy]\ncommands = [\"{id}:greet\"]\nevents = []\n"
        ),
    )
    .expect("manifest");
    std::fs::write(
        package_dir.join("lua/init.lua"),
        "bitty.commands.register({ id = \"greet\", title = \"Greet\", run = function() return \"hi\" end })\nreturn {}\n",
    )
    .expect("init");

    let data = scratch("dev-state");
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![dev_root.clone()],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::Nil)),
    });
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let plugin_id = PluginId::new(id).expect("id");
    let error = rt
        .activate(&plugin_id)
        .expect_err("stale dev must fail closed");
    assert!(
        matches!(
            error,
            PluginRuntimeError::Incompatible { ref field, .. } if field == "compat.bitty"
        ),
        "expected typed incompatible, got {error}"
    );
    assert!(
        matches!(rt.state(&plugin_id), Some(LifecycleState::Failed(_))),
        "failed generation must be terminal"
    );
    assert!(
        !rt.host_has_plugin(&plugin_id),
        "no host entry may survive a compat rejection"
    );
    assert!(
        !rt.host_owns_command(&format!("{id}:greet")),
        "no command ownership may survive a compat rejection"
    );

    let _ = std::fs::remove_dir_all(&dev_root);
}

#[test]
fn bundled_floor_stays_and_loads_on_dev_host() {
    // The `0.1`-line floor stays as written, but bundled provenance skips the
    // re-check so the `0.0.x` dev host keeps loading (DIR-019 versioning).
    let bundled_root = scratch("bundled-floor");
    let id = "xuepoo.bundledfloor";
    let package_dir = bundled_root.join(id);
    std::fs::create_dir_all(package_dir.join("lua")).expect("dirs");
    std::fs::write(
        package_dir.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Bundled Floor\"\nversion = \"0.1.0\"\n\
             description = \"floor fixture\"\n\n[compat]\nbitty = \">=0.1,<1.0\"\n\
             plugin-api = \"^1.0\"\n\n[lazy]\ncommands = [\"{id}:greet\"]\nevents = []\n"
        ),
    )
    .expect("manifest");
    std::fs::write(
        package_dir.join("lua/init.lua"),
        "bitty.commands.register({ id = \"greet\", title = \"Greet\", run = function() return \"hi\" end })\nreturn {}\n",
    )
    .expect("init");

    let data = scratch("bundled-state");
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: vec![bundled_root.clone()],
        third_party_roots: Vec::new(),
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::Nil)),
    });
    assert_eq!(rt.discover().len(), 1);
    let plugin_id = PluginId::new(id).expect("id");
    let report = rt
        .activate(&plugin_id)
        .expect("bundled floor loads on dev host");
    assert_eq!(report.state, LifecycleState::Active);

    let _ = std::fs::remove_dir_all(&bundled_root);
}
