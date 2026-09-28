//! CTX-0846 (#1454): the optional `bitty.network` plugin module is registered
//! only for plugin VMs whose activation grant includes a `network.connect`
//! capability AND when a shared network runtime was installed on the host.
//!
//! Network stays an optional extension: a granted plugin on a network-less
//! host activates cleanly without the module (fail-closed, never ambient), and
//! an ungranted plugin never sees it even when a runtime exists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRecord, PluginRuntime, PluginRuntimeConfig, SettingsSource,
    SnapshotSource, SourceClass, content_digest, write_index,
};

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(LuaValue::table([("version", LuaValue::Integer(1))]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bitty-plugin-network-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(root: PathBuf, data: PathBuf) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    })
}

/// Write a minimal plugin whose manifest optionally declares a
/// `network.connect:example.com:443` capability (paired with its required
/// `[[network.egress]]` entry, per the fail-closed manifest pairing rule).
///
/// The entry point records whether `bitty.network` was visible at load time
/// into the plugin store, so a returned command can report it.
fn write_plugin(root: &Path, id: &str, network: bool) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let network_block = if network {
        "[capabilities]\n\"network.connect:example.com:443\" = true\n\n[[network.egress]]\nhost = \"example.com\"\nports = [443]\n"
    } else {
        ""
    };
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Net"
version = "0.1.0"
description = "network capability integration test"

[compat]
plugin-api = "^1.0"

{network_block}
[lazy]
commands = ["{id}:probe"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
-- Record visibility at load time (not at command dispatch): the property
-- under test is that the module is present during `init.lua` execution, so a
-- later registration cannot make the test pass.
local network_visible = bitty.network ~= nil and bitty.network.echo ~= nil
bitty.store.set("network_visible", network_visible)

bitty.commands.register({
  id = "probe",
  title = "Probe",
  description = "Report whether bitty.network was visible at load time.",
  run = function(_args)
    return network_visible and "yes" or "no"
  end,
})
return {}
"#,
    )
    .expect("init");
    plugin
}

fn plugin_id(id: &str) -> PluginId {
    PluginId::new(id).expect("id")
}

fn probe(rt: &mut PluginRuntime, id: &PluginId) -> LuaValue {
    rt.dispatch_command(id, "probe", &[]).expect("dispatch")
}

#[test]
fn granted_plugin_with_runtime_gets_network_module() {
    let root = temp_dir("granted");
    let data = temp_dir("granted-data");
    write_plugin(&root, "example.netplugin", true);
    let mut rt = runtime(root.clone(), data.clone());
    rt.set_network_runtime(Rc::new(bitty_network_lua::SharedNetworkRuntime::new()));
    assert!(rt.network_runtime().is_some(), "runtime must be installed");

    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let id = plugin_id("example.netplugin");
    let report = rt.activate(&id).expect("activate");
    assert_eq!(report.state, LifecycleState::Active);

    assert_eq!(
        probe(&mut rt, &id),
        LuaValue::String("yes".to_string()),
        "granted plugin on a network host must see bitty.network"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn granted_plugin_without_runtime_sees_no_module_and_still_activates() {
    let root = temp_dir("no-runtime");
    let data = temp_dir("no-runtime-data");
    write_plugin(&root, "example.netplugin", true);
    let mut rt = runtime(root.clone(), data.clone());
    // No `set_network_runtime`: network is optional.
    assert!(rt.network_runtime().is_none());

    rt.discover();
    let id = plugin_id("example.netplugin");
    let report = rt
        .activate(&id)
        .expect("missing optional backend must not fail activation");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(
        probe(&mut rt, &id),
        LuaValue::String("no".to_string()),
        "no runtime => no bitty.network module (fail-closed)"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn ungranted_plugin_with_runtime_sees_no_module() {
    let root = temp_dir("ungranted");
    let data = temp_dir("ungranted-data");
    write_plugin(&root, "example.plain", false);
    let mut rt = runtime(root.clone(), data.clone());
    rt.set_network_runtime(Rc::new(bitty_network_lua::SharedNetworkRuntime::new()));

    rt.discover();
    let id = plugin_id("example.plain");
    rt.activate(&id).expect("activate");
    assert_eq!(
        probe(&mut rt, &id),
        LuaValue::String("no".to_string()),
        "an ungranted plugin must never see bitty.network, even when the host has one"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

/// CodeRabbit finding (CTX-0846): the no-capability case above does not
/// exercise the declared-but-ungranted boundary. An installed package that
/// *declares* `network.connect:example.com:443` but whose consent record
/// grants nothing must never reach the network module — and the host is
/// stricter than that: activation fails closed with a grant error before any
/// VM exists, so no plugin code runs at all.
#[test]
fn declared_but_ungranted_installed_plugin_with_runtime_sees_no_module() {
    let store = temp_dir("declared-ungranted");
    let data = temp_dir("declared-ungranted-data");
    // `write_plugin(root, id, ..)` writes `<root>/<id>`; stage under a
    // scratch root, then move the package to the store-relative path the
    // record points at.
    let scratch = temp_dir("declared-ungranted-scratch");
    write_plugin(&scratch, "example.netshop", true);
    let package = store.join("packages/example.netshop/0.1.0");
    std::fs::create_dir_all(package.parent().expect("packages dir")).expect("packages");
    std::fs::rename(scratch.join("example.netshop"), &package).expect("relocate package");

    let manifest_bytes = std::fs::read(package.join("bitty-plugin.toml")).expect("manifest bytes");
    let manifest_hash =
        bitty_runtime::plugin_runtime::manifest_toml::parse_manifest(&manifest_bytes)
            .expect("manifest parses")
            .manifest_hash();
    let record = PluginRecord {
        source_class: SourceClass::Registry,
        plugin_id: "example.netshop".to_string(),
        version: "0.1.0".to_string(),
        root: "packages/example.netshop/0.1.0".to_string(),
        manifest_hash,
        content_digest: content_digest(&package).expect("digest"),
        enabled: true,
        granted: Vec::new(),
    };
    write_index(&store, std::slice::from_ref(&record)).expect("write index");

    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: Some(store.clone()),
        bundled_roots: Vec::new(),
        third_party_roots: Vec::new(),
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    });
    rt.set_network_runtime(Rc::new(bitty_network_lua::SharedNetworkRuntime::new()));

    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let id = plugin_id("example.netshop");
    // Declared capability + empty grant = fail-closed activation error; the
    // plugin never reaches a VM, so `bitty.network` is unreachable.
    let error = rt
        .activate(&id)
        .expect_err("declared-but-ungranted activation must fail closed");
    assert!(
        format!("{error:?}").contains("missing grants"),
        "expected a grant denial, got: {error:?}"
    );
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&scratch);
}
