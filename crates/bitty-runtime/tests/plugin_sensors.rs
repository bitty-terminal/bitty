//! Native sensor conformance: `system.cpu` / `system.memory` /
//! `system.battery` against the live runtime directory (CTX-1199,
//! bitty#2019/#2011/#2016).
//!
//! Native publication plus host dispatch (no Lua provider): the runtime owns
//! `bitty-terminal.sensors`, a granted bar consumer resolves and calls
//! `get()` through real VMs, and an ungranted consumer observes absence.
//! Headless and Windows-safe (temp dirs only, no processes or sockets; the
//! sysfs fallback is a temp root, UPower stays absent).

mod common;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, HostServices, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::system_sensors::SensorHost;
use bitty_runtime::plugin_runtime::{
    PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-sensors-{tag}-{}-{}",
        std::process::id(),
        counter
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(third_party_roots: Vec<PathBuf>) -> PluginRuntime {
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: None,
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot),
    })
}

fn write_sensor_plugin(
    root: &Path,
    id: &str,
    services_toml: &str,
    capabilities_toml: &str,
    commands: &[&str],
    init_src: &str,
) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let commands_toml = commands
        .iter()
        .map(|command| format!("\"{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let capabilities_section = if capabilities_toml.is_empty() {
        String::new()
    } else {
        format!("[capabilities]\n{capabilities_toml}\n")
    };
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Test"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

{services_toml}
{capabilities_section}[lazy]
commands = [{commands_toml}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

const SENSOR_REQUIRE_MANIFEST: &str = r#"[services.required]
"system.cpu" = "^1.0"
"system.memory" = "^1.0"
"system.battery" = "^1.0"
"#;

const SENSOR_GRANT_ALL: &str =
    "platform.cpu-read = true\nplatform.memory-read = true\nplatform.battery-read = true";

const BAR_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local cpu = bitty.services.get("system.cpu")
local cpu_snap = cpu.get({})
bitty.store.set("cpu_nil", cpu_snap == nil)
local mem = bitty.services.get("system.memory")
local mem_snap = mem.get({})
bitty.store.set("mem_pct", mem_snap.pct)
bitty.store.set("mem_stale", mem_snap.stale_age_ms)
local bat = bitty.services.get("system.battery")
local bat_snap = bat.get({})
bitty.store.set("bat_capacity", bat_snap.capacity_pct)
bitty.store.set("bat_state", bat_snap.state)
return {}
"#;

const NOG_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local ok, err = pcall(bitty.services.get, "system.memory")
bitty.store.set("code", ok and "NONE" or err.code)
local opt = bitty.services.get("system.memory", { optional = true })
bitty.store.set("opt_nil", opt == nil)
return {}
"#;

const CPU_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0  50 0 50 400 0 0 0 0 0 0\n";
const CPU_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\ncpu0  100 0 100 400 0 0 0 0 0 0\n";
const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\n";

fn bar_id() -> PluginId {
    PluginId::new("xuepoo.bar").expect("id")
}

fn nog_id() -> PluginId {
    PluginId::new("xuepoo.nog").expect("id")
}

fn store_bool(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<bool> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::Bool(value)) => Some(value),
            _ => None,
        })
    })
}

fn store_number(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<f64> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::Number(value)) => Some(value),
            Some(LuaValue::Integer(value)) => Some(value as f64),
            _ => None,
        })
    })
}

fn store_string(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<String> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::String(text)) => Some(text),
            _ => None,
        })
    })
}

fn fixture_host(sysfs_tag: &str) -> (SensorHost, PathBuf, Rc<Cell<usize>>, Rc<Cell<usize>>) {
    let cpu_calls = Rc::new(Cell::new(0));
    let mem_calls = Rc::new(Cell::new(0));
    let cpu_texts = Rc::new(RefCell::new(
        vec![Some(CPU_A.to_string()), Some(CPU_B.to_string())].into_iter(),
    ));
    let mem_texts = Rc::new(RefCell::new(vec![Some(MEMINFO.to_string())].into_iter()));
    let cpu_probed = Rc::clone(&cpu_calls);
    let mem_probed = Rc::clone(&mem_calls);
    let cpu_source: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
        cpu_probed.set(cpu_probed.get() + 1);
        cpu_texts.borrow_mut().next().unwrap_or(None)
    });
    let mem_source: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
        mem_probed.set(mem_probed.get() + 1);
        mem_texts.borrow_mut().next().unwrap_or(None)
    });
    let sysfs_root = temp_dir(sysfs_tag).join("power");
    std::fs::create_dir_all(&sysfs_root).expect("sysfs root");
    let bat = sysfs_root.join("BAT0");
    std::fs::create_dir_all(&bat).expect("bat dir");
    std::fs::write(bat.join("type"), "Battery\n").expect("type");
    std::fs::write(bat.join("capacity"), "63\n").expect("capacity");
    std::fs::write(bat.join("status"), "Discharging\n").expect("status");
    std::fs::write(bat.join("power_now"), "5000000\n").expect("power");
    let host = SensorHost::for_test(cpu_source, mem_source, sysfs_root.clone(), 0);
    (host, sysfs_root, cpu_calls, mem_calls)
}

fn setup(tag: &str) -> (PluginRuntime, PathBuf, PathBuf) {
    let root = temp_dir(tag);
    let third = root.join("third");
    write_sensor_plugin(
        &third,
        "xuepoo.bar",
        SENSOR_REQUIRE_MANIFEST,
        SENSOR_GRANT_ALL,
        &["xuepoo.bar:probe"],
        BAR_INIT,
    );
    write_sensor_plugin(
        &third,
        "xuepoo.nog",
        "[services.required]\n\"system.memory\" = \"^1.0\"\n",
        "",
        &["xuepoo.nog:probe"],
        NOG_INIT,
    );
    let rt = runtime(vec![third]);
    let (host, sysfs_root, _, _) = fixture_host(&format!("{tag}-host"));
    *rt.sensor_host().borrow_mut() = host;
    (rt, root, sysfs_root)
}

#[test]
fn native_sensors_publish_once_and_survive_plugin_lifecycle() {
    let (mut rt, root, sysfs_root) = setup("publish-once");
    assert_eq!(rt.discover().len(), 2);
    // Native publication lands at construction: three records before any
    // plugin activates, all from the native component.
    assert_eq!(rt.service_directory().borrow().published_count(), 14);
    rt.activate(&bar_id()).expect("bar activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 14);
    rt.activate(&nog_id()).expect("nog activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 14);
    rt.suspend(&bar_id()).expect("suspend");
    assert_eq!(rt.service_directory().borrow().published_count(), 14);
    rt.resume(&bar_id()).expect("resume");
    rt.dispose(&bar_id()).expect("dispose");
    assert_eq!(rt.service_directory().borrow().published_count(), 14);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(sysfs_root.parent().expect("parent"));
}

#[test]
fn granted_bar_serves_ungranted_matches_absent_through_live_vms() {
    let (mut rt, root, sysfs_root) = setup("live-bridge");
    rt.discover();
    rt.activate(&bar_id()).expect("bar activates");
    rt.activate(&nog_id()).expect("nog activates");

    // Granted bar: memory and battery serve live tables, cpu hides its
    // first poll (baseline establishes, never fabricated zeros).
    assert_eq!(store_bool(&rt, &bar_id(), "cpu_nil"), Some(true));
    let pct = store_number(&rt, &bar_id(), "mem_pct").expect("memory pct");
    assert!((pct - 50.0).abs() < 1e-9, "got {pct}");
    assert_eq!(store_number(&rt, &bar_id(), "mem_stale"), Some(0.0));
    let capacity = store_number(&rt, &bar_id(), "bat_capacity").expect("battery capacity");
    assert!((capacity - 63.0).abs() < 1e-9, "got {capacity}");
    assert_eq!(
        store_string(&rt, &bar_id(), "bat_state"),
        Some("discharging".to_string())
    );

    // Ungranted consumer: resolution fails exactly like an absent service.
    assert_eq!(
        store_string(&rt, &nog_id(), "code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    assert_eq!(store_bool(&rt, &nog_id(), "opt_nil"), Some(true));

    // Second cpu window serves through the same live bridge: advance the
    // fixture clock past the 2 s ceiling and resolve-call again.
    rt.sensor_host().borrow_mut().set_manual_now_ms(Some(2000));
    let services = rt.services(&bar_id()).expect("bar services");
    let route = services
        .service_resolve("system.cpu", None, false)
        .expect("cpu resolves")
        .expect("route");
    let second = services
        .service_call(
            &route.provider,
            route.generation,
            &route.iface,
            &route.version,
            "get",
            &LuaValue::Nil,
        )
        .expect("second cpu serves");
    match second {
        LuaValue::Table(_) => {}
        other => panic!("second cpu serves a table, got {other:?}"),
    }

    // Broker recorded the native calls with owner plus backing heads.
    let calls = rt.service_call_ledger().borrow().calls();
    let sensor_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.provider == "bitty-terminal.sensors")
        .collect();
    assert!(sensor_calls.len() >= 4, "got {}", sensor_calls.len());
    for call in &sensor_calls {
        assert_eq!(call.method, "get");
        assert!(
            [
                "platform.cpu-read",
                "platform.memory-read",
                "platform.battery-read"
            ]
            .contains(&call.heads.first().map(String::as_str).unwrap_or("")),
            "heads: {:?}",
            call.heads
        );
    }

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(sysfs_root.parent().expect("parent"));
}

#[test]
fn sensor_tables_carry_no_policy_and_no_control_surface() {
    let (mut rt, root, sysfs_root) = setup("no-policy");
    rt.discover();
    rt.activate(&bar_id()).expect("bar activates");
    let services = rt.services(&bar_id()).expect("bar services");
    for iface in ["system.cpu", "system.memory", "system.battery"] {
        let route = services
            .service_resolve(iface, None, false)
            .expect("resolves")
            .expect("route");
        // Read-only by construction: only `get` is published.
        assert_eq!(route.methods, vec!["get".to_string()]);
        let value = services
            .service_call(
                &route.provider,
                route.generation,
                &route.iface,
                &route.version,
                "get",
                &LuaValue::Nil,
            )
            .expect("get serves or hides");
        // Cpu may hide as nil on its first poll; anything served must be
        // raw numbers/states only.
        if value != LuaValue::Nil {
            let rendered = format!("{value:?}").to_lowercase();
            for forbidden in [
                "warning",
                "critical",
                "threshold",
                "alert",
                "icon",
                "glyph",
                "bucket",
            ] {
                assert!(
                    !rendered.contains(forbidden),
                    "{iface} must not carry '{forbidden}': {rendered}"
                );
            }
        }
        let error = services
            .service_call(
                &route.provider,
                route.generation,
                &route.iface,
                &route.version,
                "set",
                &LuaValue::Nil,
            )
            .expect_err("control spelling must fail closed");
        assert_eq!(error.code, "E_SERVICE_GONE");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(sysfs_root.parent().expect("parent"));
}
