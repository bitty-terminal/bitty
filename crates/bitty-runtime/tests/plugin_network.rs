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
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
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
bitty.commands.register({
  id = "probe",
  title = "Probe",
  description = "Report whether bitty.network is visible.",
  run = function(_args)
    local visible = bitty.network ~= nil and bitty.network.echo ~= nil
    bitty.store.set("network_visible", visible)
    return visible and "yes" or "no"
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
