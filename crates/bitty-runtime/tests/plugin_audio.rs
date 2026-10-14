//! Audio provider conformance: `audio.state` / `audio.control` /
//! `audio.spectrum` against the live runtime directory (CTX-1196,
//! bitty#2009/#2013).
//!
//! Two-plugin activation over real VMs: the provider publishes the three
//! audio interfaces backed by their capability heads, a read-granted bar
//! consumer resolves state and spectrum while control stays
//! indistinguishable from absent, and a control-granted consumer calls
//! through. Headless and Windows-safe (temp dirs only, no processes or
//! sockets; the monitor seam stays injected — no `pactl` runs here).
//!
//! CTX-1199: counts include the three runtime-owned native sensor records,
//! so audio-only expectations shift by three.

mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::LuaValue;
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use std::collections::BTreeMap;

use bitty_lua::BridgeError;

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
        "bitty-plugin-audio-{tag}-{}-{}",
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

/// Write a plugin package under `root/<id>/`.
///
/// `services_toml` holds the raw `[services.provided]`/`[services.required]`
/// body; `capabilities_toml` holds the raw `[capabilities]` body.
fn write_audio_plugin(
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

/// `[capabilities]` body granting every audio head (provider side).
const AUDIO_GRANT_ALL: &str =
    "audio.spectrum = true\naudio.state.read = true\naudio.state.control = true";

/// `[capabilities]` body granting read only (bar side: no control head).
const AUDIO_GRANT_READ: &str = "audio.spectrum = true\naudio.state.read = true";

const AUDIO_PROVIDE_MANIFEST: &str = r#"[services.provided]
"audio.state" = { version = "1.0.0", heads = ["audio.state.read"] }
"audio.control" = { version = "1.0.0", heads = ["audio.state.control"] }
"audio.spectrum" = { version = "1.0.0", heads = ["audio.spectrum"] }
"#;

const AUDIO_REQUIRE_MANIFEST: &str = r#"[services.required]
"audio.state" = ">=1.0"
"audio.control" = ">=1.0"
"audio.spectrum" = ">=1.0"
"#;

const AUDIO_PROVIDER_INIT: &str = r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
bitty.services.provide("audio.state", {
  get = function(args)
    return { volume = 50, muted = false, port = "analog-output-speaker", stale_age_ms = 0 }
  end,
})
bitty.services.provide("audio.control", {
  set_volume = function(args) return { volume = 60 } end,
  toggle_mute = function(args) return { muted = true } end,
})
bitty.services.provide("audio.spectrum", {
  spectrum = function(args)
    local n = args.buckets or 8
    local levels = {}
    for i = 1, n do levels[i] = 0.5 end
    return { levels = levels }
  end,
})
return {}
"#;

const BAR_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local state = bitty.services.get("audio.state")
local snapshot = state.get({})
bitty.store.set("volume", snapshot.volume)
bitty.store.set("port", snapshot.port)
local spectrum = bitty.services.get("audio.spectrum")
local meter = spectrum.spectrum({ buckets = 8 })
bitty.store.set("buckets", #meter.levels)
local ok, err = pcall(bitty.services.get, "audio.control")
bitty.store.set("control_code", ok and "NONE" or err.code)
local opt = bitty.services.get("audio.control", { optional = true })
bitty.store.set("control_nil", opt == nil)
return {}
"#;

const CTL_INIT: &str = r#"
bitty.commands.register({
  id = "probe",
  title = "Probe",
  run = function() return "probed" end,
})
local control = bitty.services.get("audio.control")
local ok_vol, vol = pcall(control.set_volume, { volume = 60 })
bitty.store.set("set_volume", ok_vol and vol.volume or -1)
local ok_mute, mute = pcall(control.toggle_mute, {})
bitty.store.set("toggle_mute", ok_mute and (mute.muted and 1 or 0) or -1)
return {}
"#;

fn audio_id() -> PluginId {
    PluginId::new("xuepoo.audio").expect("id")
}

fn bar_id() -> PluginId {
    PluginId::new("xuepoo.bar").expect("id")
}

fn ctl_id() -> PluginId {
    PluginId::new("xuepoo.ctl").expect("id")
}

fn store_string(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<String> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::String(text)) => Some(text),
            _ => None,
        })
    })
}

fn store_int(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<i64> {
    rt.services(id).and_then(|services| {
        services.with_store(|store| match store.get(key) {
            Some(LuaValue::Integer(value)) => Some(value),
            _ => None,
        })
    })
}

fn setup(tag: &str) -> (PluginRuntime, PathBuf) {
    let root = temp_dir(tag);
    let third = root.join("third");
    write_audio_plugin(
        &third,
        "xuepoo.audio",
        AUDIO_PROVIDE_MANIFEST,
        AUDIO_GRANT_ALL,
        &["xuepoo.audio:ping"],
        AUDIO_PROVIDER_INIT,
    );
    write_audio_plugin(
        &third,
        "xuepoo.bar",
        AUDIO_REQUIRE_MANIFEST,
        AUDIO_GRANT_READ,
        &["xuepoo.bar:probe"],
        BAR_INIT,
    );
    write_audio_plugin(
        &third,
        "xuepoo.ctl",
        AUDIO_REQUIRE_MANIFEST,
        AUDIO_GRANT_ALL,
        &["xuepoo.ctl:probe"],
        CTL_INIT,
    );
    let rt = runtime(vec![third]);
    (rt, root)
}

#[test]
fn audio_read_and_spectrum_serve_while_control_stays_absent() {
    // bitty#2013 acceptance: read works ungranted-control; bitty#2009
    // acceptance: ungranted consume is indistinguishable from absent.
    let (mut rt, root) = setup("read-absent");
    assert_eq!(rt.discover().len(), 3);

    rt.activate(&audio_id()).expect("provider activates");
    assert_eq!(rt.service_directory().borrow().published_count(), 6);
    rt.activate(&bar_id()).expect("bar activates");

    assert_eq!(store_int(&rt, &bar_id(), "volume"), Some(50));
    assert_eq!(
        store_string(&rt, &bar_id(), "port"),
        Some("analog-output-speaker".to_string())
    );
    assert_eq!(store_int(&rt, &bar_id(), "buckets"), Some(8));
    assert_eq!(
        store_string(&rt, &bar_id(), "control_code"),
        Some("E_SERVICE_RESOLUTION".to_string())
    );
    assert_eq!(
        rt.services(&bar_id())
            .expect("services")
            .with_store(|store| store.get("control_nil")),
        Some(LuaValue::Bool(true))
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn audio_control_granted_consumer_calls_through() {
    // The same live records serve a control-granted consumer: only the
    // grant differs.
    let (mut rt, root) = setup("control-through");
    rt.discover();
    rt.activate(&audio_id()).expect("provider activates");
    rt.activate(&ctl_id()).expect("control consumer activates");

    assert_eq!(store_int(&rt, &ctl_id(), "set_volume"), Some(60));
    assert_eq!(store_int(&rt, &ctl_id(), "toggle_mute"), Some(1));

    // The broker recorded the control calls with owner + backing head.
    let calls = rt.service_call_ledger().borrow().calls();
    let control_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.iface == "audio.control")
        .collect();
    assert_eq!(control_calls.len(), 2);
    for call in control_calls {
        assert_eq!(call.provider, "xuepoo.audio");
        assert_eq!(call.heads, vec!["audio.state.control".to_string()]);
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn audio_unbacked_provide_rejected_at_record() {
    // dynapi gating (bitty#2005) covers the audio heads unchanged: an
    // unbacked `audio.state` provision rejects at record time naming the
    // interface and the heads field.
    let root = temp_dir("unbacked");
    let third = root.join("third");
    write_audio_plugin(
        &third,
        "xuepoo.solo",
        "[services.provided]\n\"audio.state\" = { version = \"1.0.0\" }\n",
        AUDIO_GRANT_READ,
        &["xuepoo.solo:ping"],
        r#"
bitty.commands.register({
  id = "ping",
  title = "Ping",
  run = function() return "pong" end,
})
return {}
"#,
    );
    let mut rt = runtime(vec![third]);
    let results = rt.discover();
    assert_eq!(results.len(), 1);
    let err = format!(
        "{}",
        results[0]
            .1
            .as_ref()
            .expect_err("unbacked provide must reject at install")
    );
    assert!(err.contains("audio.state"), "names the iface: {err}");
    assert!(err.contains("heads"), "names the field: {err}");
    assert_eq!(rt.service_directory().borrow().published_count(), 3);
    let _ = std::fs::remove_dir_all(&root);
}
