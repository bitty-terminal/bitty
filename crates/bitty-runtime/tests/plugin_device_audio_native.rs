//! Native device+audio conformance: `network.*` / `bluetooth.*` /
//! `backlight.*` / `idle.*` plus `audio.*` against the live runtime
//! directory (CTX-1238, issue #2100).
//!
//! Native publication plus host dispatch (no Lua provider): the runtime owns
//! `bitty-terminal.devices` (8 ifaces) plus `bitty-terminal.audio` (3 ifaces)
//! plus `bitty-terminal.sensors` (3 ifaces), a granted bar consumer resolves
//! and calls through real VMs, and an ungranted consumer observes absence.
//! Headless and Windows-safe (temp dirs only, no processes or sockets; the
//! network/sysfs roots plus the audio spawner stay injected fixtures).

mod common;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, HostServices, LuaValue};
use bitty_plugin_host::AudioError;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::audio::{AudioHost, SampledSpawnFn};
use bitty_runtime::plugin_runtime::device_providers::host::DeviceHost;
use bitty_runtime::plugin_runtime::system_sensors::SensorHost;
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-devpub-{tag}-{}-{}",
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

fn write_plugin(
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

const REQUIRE_MANIFEST: &str = r#"[services.required]
"network.state" = "^1.0"
"network.essid" = "^1.0"
"bluetooth.state" = "^1.0"
"bluetooth.control" = "^1.0"
"backlight.state" = "^1.0"
"backlight.control" = "^1.0"
"idle.state" = "^1.0"
"idle.control" = "^1.0"
"audio.state" = ">=1.0"
"audio.control" = ">=1.0"
"audio.spectrum" = ">=1.0"
"#;

const GRANT_ALL: &str = r#"platform.network.read = true
platform.essid.read = true
platform.bluetooth.read = true
platform.bluetooth.control = true
platform.backlight.read = true
platform.backlight.control = true
platform.idle.read = true
platform.idle.control = true
audio.spectrum = true
audio.state.read = true
audio.state.control = true"#;

const BAR_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local net = bitty.services.get("network.state")
local net_snap = net.get({})
bitty.store.set("net_link", net_snap.link)
bitty.store.set("net_nil", net_snap == nil)
local essid = bitty.services.get("network.essid")
local essid_snap = essid.get({})
bitty.store.set("essid", essid_snap.essid)
local bt = bitty.services.get("bluetooth.state")
local bt_snap = bt.get({})
bitty.store.set("bt_powered", bt_snap.powered)
local bl = bitty.services.get("backlight.state")
local bl_snap = bl.get({})
bitty.store.set("bl_pct", bl_snap.pct)
local idle = bitty.services.get("idle.state")
local idle_snap = idle.get({})
bitty.store.set("idle_inhibited", idle_snap.inhibited)
local audio = bitty.services.get("audio.state")
local audio_snap = audio.get({})
bitty.store.set("vol", audio_snap.volume_pct)
bitty.store.set("port", audio_snap.port)
local bt_ctl = bitty.services.get("bluetooth.control")
local ok_bt = pcall(bt_ctl.set_power, { on = true })
bitty.store.set("bt_ctl_ok", ok_bt)
local idle_ctl = bitty.services.get("idle.control")
local ok_idle = pcall(idle_ctl.set, { on = true })
bitty.store.set("idle_ctl_ok", ok_idle)
local audio_ctl = bitty.services.get("audio.control")
local ok_vol, vol = pcall(audio_ctl.set_volume, { volume = 60 })
bitty.store.set("audio_vol_ok", ok_vol and vol.ok or false)
return {}
"#;

const NOG_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local ok, err = pcall(bitty.services.get, "network.state")
bitty.store.set("code", ok and "NONE" or err.code)
local opt = bitty.services.get("network.state", { optional = true })
bitty.store.set("opt_nil", opt == nil)
local ok2, err2 = pcall(bitty.services.get, "audio.state")
bitty.store.set("audio_code", ok2 and "NONE" or err2.code)
return {}
"#;

const CPU_A: &str = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0  50 0 50 400 0 0 0 0 0 0\n";
const CPU_B: &str = "cpu  150 0 150 900 0 0 0 0 0 0\ncpu0  100 0 100 400 0 0 0 0 0 0\n";
const MEMINFO: &str = "MemTotal:        2097152 kB\nMemAvailable:    1048576 kB\n";
const VOLUME_FIXTURE: &str = "Volume: front-left: 32768 /  50% / -18.06 dB\n";
const MUTE_FIXTURE: &str = "Mute: no\n";
const SINKS_FIXTURE: &str = "Sink #0\n\tState: RUNNING\n\tActive Port: analog-output-headphones\n";

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

fn store_string(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<String> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::String(text)) => Some(text),
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

fn fixture_sensor_host(sysfs_tag: &str) -> (SensorHost, PathBuf) {
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
    let host = SensorHost::for_test(cpu_source, mem_source, sysfs_root.clone(), 0);
    (host, sysfs_root)
}

fn fixture_device_host(tag: &str) -> (DeviceHost, PathBuf) {
    use bitty_platform::bluetooth_state::RawBluetoothDevice;
    let root = temp_dir(tag);
    let class_net = root.join("class_net");
    let wlan = class_net.join("wlan0");
    std::fs::create_dir_all(&wlan).expect("wlan dir");
    std::fs::write(wlan.join("operstate"), "up\n").expect("operstate");
    std::fs::write(wlan.join("wireless"), "").expect("wireless");
    let proc_route = root.join("route");
    std::fs::write(
        &proc_route,
        "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\nwlan0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n",
    )
    .expect("route");
    let proc_dev = root.join("dev");
    std::fs::write(
        &proc_dev,
        "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n wlan0: 1000000    100    0    0    0     0          0         0  500000      50    0    0    0     0       0          0\n",
    )
    .expect("dev");
    let bl_root = root.join("backlight");
    let dev = bl_root.join("intel_backlight");
    std::fs::create_dir_all(&dev).expect("backlight dev");
    std::fs::write(dev.join("brightness"), "300\n").expect("brightness");
    std::fs::write(dev.join("max_brightness"), "1200\n").expect("max");
    let host = DeviceHost::for_test(
        class_net,
        proc_route,
        proc_dev,
        Some("Home".to_string()),
        true,
        Some(true),
        Some(vec![(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01".to_string(),
            RawBluetoothDevice {
                address: "AA:BB:CC:DD:EE:01".to_string(),
                alias: "Headphones".to_string(),
                connected: true,
                bonded: true,
            },
        )]),
        bl_root,
        true,
        Some(false),
        0,
    );
    (host, root)
}

fn fixture_audio_host() -> AudioHost {
    use bitty_plugin_host::AudioError;
    use bitty_runtime::plugin_runtime::audio::SpawnFn;
    let spawn: SpawnFn = Rc::new(move |argv: &[String]| {
        let text = argv.join(" ");
        if text.contains("get-sink-volume") {
            Ok(VOLUME_FIXTURE.to_string())
        } else if text.contains("get-sink-mute") {
            Ok(MUTE_FIXTURE.to_string())
        } else if text.contains("list") {
            Ok(SINKS_FIXTURE.to_string())
        } else if text.contains("set-sink") {
            Ok(String::new())
        } else {
            Err(AudioError::unavailable("unexpected argv in fixture"))
        }
    });
    AudioHost::for_test(spawn, 0)
}

fn setup(tag: &str) -> (PluginRuntime, PathBuf, PathBuf, PathBuf) {
    let root = temp_dir(tag);
    let third = root.join("third");
    write_plugin(
        &third,
        "xuepoo.bar",
        REQUIRE_MANIFEST,
        GRANT_ALL,
        &["xuepoo.bar:probe"],
        BAR_INIT,
    );
    write_plugin(
        &third,
        "xuepoo.nog",
        "[services.required]\n\"network.state\" = \"^1.0\"\n\"audio.state\" = \">=1.0\"\n",
        "",
        &["xuepoo.nog:probe"],
        NOG_INIT,
    );
    let mut rt = runtime(vec![third]);
    // bitty#2425: this fixture bar activates with ~2 ms of real work, but the
    // RC-1 wall dimension is load-sensitive: under sharded CI the same chunk
    // measured 53 ms against the 50 ms production budget and suspended two
    // tests in this binary. Widen only the wall dimension for this harness
    // (10x headroom, the bitty#1744/#1727 treatment). Production keeps
    // `RC1_WALL_CLOCK_BUDGET_MS`; budget enforcement stays pinned by
    // `bitty-lua/tests/measurement_lua.rs` and `load_gate.rs`, and the
    // activation regression below runs on the production budget.
    rt.set_vm_wall_budget_ms(Some(10 * bitty_lua::RC1_WALL_CLOCK_BUDGET_MS));
    let (sensor, sensor_root) = fixture_sensor_host(&format!("{tag}-sensor"));
    *rt.sensor_host().borrow_mut() = sensor;
    let (device, device_root) = fixture_device_host(&format!("{tag}-device"));
    *rt.device_host().borrow_mut() = device;
    *rt.audio_host().borrow_mut() = fixture_audio_host();
    (rt, root, sensor_root, device_root)
}

const SLOW_AUDIO_MANIFEST: &str = r#"[services.required]
"audio.state" = ">=1.0"
"#;

const SLOW_AUDIO_GRANT: &str = "audio.state.read = true";

/// Polls the audio provider at activation and records whether the first read
/// was hidden. A live monitor lands its sample off-thread, so the first read
/// must hide and activation must stay cheap.
const SLOW_AUDIO_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local audio = bitty.services.get("audio.state")
local ok, snap = pcall(audio.get, {})
bitty.store.set("first_hidden", not ok)
if ok then
  bitty.store.set("vol", snap.volume_pct)
end
return {}
"#;

#[test]
fn bar_activation_never_blocks_on_a_slow_audio_monitor() {
    // bitty#2425: the live provider read is three blocking `pactl` spawns
    // (92 ms cold on the reference host). A plugin that polls
    // `audio.state.get()` during `init.lua` used to spend that time inside
    // the RC-1 50 ms slice and fail activation (`smoke-bar-heavy`: 159/172
    // ms). The sampled monitor moves the read off the executor thread, so
    // activation completes on the production budget while the reader is
    // still running.
    let root = temp_dir("slow-audio");
    let third = root.join("third");
    write_plugin(
        &third,
        "xuepoo.slow",
        SLOW_AUDIO_MANIFEST,
        SLOW_AUDIO_GRANT,
        &["xuepoo.slow:probe"],
        SLOW_AUDIO_INIT,
    );
    let mut rt = runtime(vec![third]);
    let spawn: SampledSpawnFn = std::sync::Arc::new(|argv: &[String]| {
        // Three composed spawns per read: 300 ms total, six times the
        // production 50 ms slice.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let text = argv.join(" ");
        if text.contains("get-sink-volume") {
            Ok(VOLUME_FIXTURE.to_string())
        } else if text.contains("get-sink-mute") {
            Ok(MUTE_FIXTURE.to_string())
        } else if text.contains("list") {
            Ok(SINKS_FIXTURE.to_string())
        } else {
            Err(AudioError::unavailable("unexpected argv in fixture"))
        }
    });
    *rt.audio_host().borrow_mut() = AudioHost::sampled_for_test(spawn, 0);
    let id = PluginId::new("xuepoo.slow").expect("id");
    rt.discover();
    // Production RC-1: an inline 400 ms read would suspend here.
    let report = rt.activate(&id).expect("activation stays inside the slice");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(
        store_bool(&rt, &id, "first_hidden"),
        Some(true),
        "the first read hides instead of blocking activation"
    );
    // The off-thread read lands and serves through the same gate.
    let mut served = None;
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        if let Ok(value) =
            rt.audio_host()
                .borrow_mut()
                .call("audio.state", "get", &LuaValue::Nil, true)
        {
            served = Some(value);
            break;
        }
    }
    let served = served.expect("the sampled read lands");
    assert_eq!(served.get("volume_pct"), Some(&LuaValue::Integer(50)));
    let _ = std::fs::remove_dir_all(&root);
}

const POLL_LOOP_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local audio = bitty.services.get("audio.state")
local served = 0
for _ = 1, 5000 do
  local ok = pcall(audio.get, {})
  if ok then served = served + 1 end
end
bitty.store.set("served", served)
return {}
"#;

#[test]
fn repeated_audio_polls_never_extend_the_slice_with_host_work() {
    // bitty#2425 security half: a plugin that hammers `audio.state.get()`
    // cannot convert the call into unbounded host work. Every call is a
    // cache/hide answer; exactly one composed read (three spawns) runs per
    // 0.5 Hz window, off-thread, and the plugin's own loop stays bounded by
    // the RC-1 instruction budget. 5 000 calls complete inside the
    // production 50 ms slice.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = temp_dir("poll-loop");
    let third = root.join("third");
    write_plugin(
        &third,
        "xuepoo.loop",
        SLOW_AUDIO_MANIFEST,
        SLOW_AUDIO_GRANT,
        &["xuepoo.loop:probe"],
        POLL_LOOP_INIT,
    );
    let mut rt = runtime(vec![third]);
    let spawns = std::sync::Arc::new(AtomicUsize::new(0));
    let probed = std::sync::Arc::clone(&spawns);
    let spawn: SampledSpawnFn = std::sync::Arc::new(move |argv: &[String]| {
        probed.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let text = argv.join(" ");
        if text.contains("get-sink-volume") {
            Ok(VOLUME_FIXTURE.to_string())
        } else if text.contains("get-sink-mute") {
            Ok(MUTE_FIXTURE.to_string())
        } else if text.contains("list") {
            Ok(SINKS_FIXTURE.to_string())
        } else {
            Err(AudioError::unavailable("unexpected argv in fixture"))
        }
    });
    *rt.audio_host().borrow_mut() = AudioHost::sampled_for_test(spawn, 0);
    let id = PluginId::new("xuepoo.loop").expect("id");
    rt.discover();
    let report = rt
        .activate(&id)
        .expect("5 000 polls stay inside the production slice");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(
        store_number(&rt, &id, "served"),
        Some(0.0),
        "every poll before the first sample hides, none blocks"
    );
    // One in-flight composed read at most: three spawns, never 5 000.
    assert!(
        spawns.load(Ordering::SeqCst) <= 3,
        "poll loop multiplied monitor work: {} spawns",
        spawns.load(Ordering::SeqCst)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn native_device_audio_publish_once_and_survive_plugin_lifecycle() {
    let (mut rt, root, sensor_root, device_root) = setup("publish-once");
    assert_eq!(rt.discover().len(), 2);
    // Native publication lands at construction: 3 sensors + 8 devices + 3
    // audio = 14 records before any plugin activates, all runtime-owned.
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
    let _ = std::fs::remove_dir_all(sensor_root.parent().expect("parent"));
    let _ = std::fs::remove_dir_all(&device_root);
}

#[test]
fn granted_bar_serves_ungranted_matches_absent_through_live_vms() {
    let (mut rt, root, sensor_root, device_root) = setup("live-bridge");
    rt.discover();
    rt.activate(&bar_id()).expect("bar activates");
    rt.activate(&nog_id()).expect("nog activates");

    // Granted bar: every native read serves live tables through the broker.
    assert_eq!(
        store_string(&rt, &bar_id(), "net_link"),
        Some("wifi".to_string())
    );
    assert_eq!(store_bool(&rt, &bar_id(), "net_nil"), Some(false));
    assert_eq!(
        store_string(&rt, &bar_id(), "essid"),
        Some("Home".to_string())
    );
    assert_eq!(store_bool(&rt, &bar_id(), "bt_powered"), Some(true));
    assert_eq!(store_number(&rt, &bar_id(), "bl_pct"), Some(25.0));
    assert_eq!(store_bool(&rt, &bar_id(), "idle_inhibited"), Some(false));
    assert_eq!(store_number(&rt, &bar_id(), "vol"), Some(50.0));
    assert_eq!(
        store_string(&rt, &bar_id(), "port"),
        Some("analog-output-headphones".to_string())
    );
    // Granted controls drive their backends headlessly.
    assert_eq!(store_bool(&rt, &bar_id(), "bt_ctl_ok"), Some(true));
    assert_eq!(store_bool(&rt, &bar_id(), "idle_ctl_ok"), Some(true));
    assert_eq!(store_bool(&rt, &bar_id(), "audio_vol_ok"), Some(true));

    // Ungranted consumer: resolution fails exactly like an absent service.
    assert_eq!(
        store_string(&rt, &nog_id(), "code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    assert_eq!(store_bool(&rt, &nog_id(), "opt_nil"), Some(true));
    assert_eq!(
        store_string(&rt, &nog_id(), "audio_code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );

    // Broker recorded the native calls with owner plus backing heads.
    let calls = rt.service_call_ledger().borrow().calls();
    let device_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.provider == "bitty-terminal.devices")
        .collect();
    assert!(device_calls.len() >= 7, "got {}", device_calls.len());
    let audio_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.provider == "bitty-terminal.audio")
        .collect();
    assert!(!audio_calls.is_empty(), "audio calls recorded");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(sensor_root.parent().expect("parent"));
    let _ = std::fs::remove_dir_all(&device_root);
}

#[test]
fn device_audio_tables_carry_no_policy_and_ungranted_control_fails_closed() {
    let (mut rt, root, sensor_root, device_root) = setup("no-policy");
    rt.discover();
    rt.activate(&bar_id()).expect("bar activates");
    let services = rt.services(&bar_id()).expect("bar services");
    for iface in [
        "network.state",
        "network.essid",
        "bluetooth.state",
        "backlight.state",
        "idle.state",
        "audio.state",
        "audio.spectrum",
    ] {
        let route = services
            .service_resolve(iface, None, false)
            .expect("resolves")
            .expect("route");
        let spectrum_args = LuaValue::table([("buckets", LuaValue::Integer(8))]);
        let (method, args) = match iface {
            "audio.spectrum" => ("spectrum", &spectrum_args),
            _ => ("get", &LuaValue::Nil),
        };
        let value = services
            .service_call(
                &route.provider,
                route.generation,
                &route.iface,
                &route.version,
                method,
                args,
            )
            .unwrap_or(LuaValue::Nil);
        // Spectrum has no live pactl source: hide as gone maps to Nil here
        // via unwrap_or; anything served must be raw numbers/states only.
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
    }
    // Control without its head fails closed as gone (pinned handle).
    let nog_services = rt.services(&nog_id());
    if let Some(nog) = nog_services {
        let _ = nog;
    }
    // Direct host check: ungranted device control touches no backend.
    let device = rt.device_host();
    let before_bt = device.borrow().bluetooth_calls().len();
    let before_idle = device.borrow().idle_calls().len();
    let mut probe = device.borrow_mut();
    let denied = probe
        .call(
            "bluetooth.control",
            "set_power",
            &LuaValue::table([("on", LuaValue::Bool(true))]),
            &[],
        )
        .expect_err("ungranted control fails");
    assert_eq!(denied.code, "E_NOT_PERMITTED");
    drop(probe);
    assert_eq!(device.borrow().bluetooth_calls().len(), before_bt);
    assert_eq!(device.borrow().idle_calls().len(), before_idle);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(sensor_root.parent().expect("parent"));
    let _ = std::fs::remove_dir_all(&device_root);
}
