//! Gap A + C-minimal runtime tests: discovery, activation, lifecycle, host
//! services, capture validation, and safe mode.

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

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("bitty-plugin-runtime-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(roots: Vec<PathBuf>, data_dir: PathBuf, safe_mode: bool) -> PluginRuntime {
    let mut settings = MapSettings::default();
    settings
        .0
        .insert("retention_days".to_string(), LuaValue::Integer(7));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: Some(data_dir),
        bundled_roots: roots,
        settings: Rc::new(settings),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
            (
                "zones",
                LuaValue::array(vec![
                    LuaValue::table([("kind", LuaValue::String("prompt".to_string()))]),
                    LuaValue::table([("kind", LuaValue::String("input".to_string()))]),
                ]),
            ),
        ]))),
    })
}

fn sample_id() -> PluginId {
    PluginId::new("bitty-featured.sample").expect("id")
}

#[test]
fn discovers_activates_and_dispatches_bundled_plugin() {
    let data = temp_dir("activate");
    let mut rt = runtime(vec![fixtures_root()], data.clone(), false);
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    assert_eq!(rt.package_count(), 1);

    let report = rt.activate(&sample_id()).expect("activate");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.commands, 1);
    assert_eq!(report.events, 1);
    assert!(!report.skipped_safe_mode);
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Active));

    let result = rt
        .dispatch_command(&sample_id(), "summary", &[])
        .expect("dispatch");
    assert_eq!(result, LuaValue::String("ok".to_string()));

    let notifications = rt.drain_notifications();
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].title, "Sample");

    #[cfg(unix)]
    assert!(
        data.join("bitty-featured.sample/store.json").is_file(),
        "store must be persisted atomically"
    );

    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn event_delivery_invokes_subscribed_handler() {
    let data = temp_dir("events");
    let mut rt = runtime(vec![fixtures_root()], data.clone(), false);
    rt.discover();
    rt.activate(&sample_id()).expect("activate");

    let payload = LuaValue::table([("terminal_id", LuaValue::Integer(9))]);
    assert_eq!(rt.deliver_event("terminal.opened", &payload), 1);
    assert_eq!(
        rt.services(&sample_id())
            .expect("services")
            .with_store(|store| store.get("last")),
        Some(LuaValue::Integer(9))
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn lifecycle_transitions_are_enforced() {
    let data = temp_dir("lifecycle");
    let mut rt = runtime(vec![fixtures_root()], data.clone(), false);
    rt.discover();
    rt.activate(&sample_id()).expect("activate");

    rt.suspend(&sample_id()).expect("suspend");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Suspended));
    assert!(rt.suspend(&sample_id()).is_err(), "double suspend rejected");

    rt.resume(&sample_id()).expect("resume");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Active));

    rt.dispose(&sample_id()).expect("dispose");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Disposed));
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn safe_mode_creates_no_third_party_vm() {
    let data = temp_dir("safe");
    let mut rt = runtime(vec![fixtures_root()], data.clone(), true);
    rt.discover();
    let report = rt.activate(&sample_id()).expect("activate");
    assert!(report.skipped_safe_mode);
    assert_eq!(report.commands, 0);
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Unloaded));
    assert!(rt.dispatch_command(&sample_id(), "summary", &[]).is_err());
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn undeclared_registration_fails_capture_validation() {
    let data = temp_dir("capture");
    let root = temp_dir("capture-root");
    let plugin = root.join("bad");
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        r#"[plugin]
id = "bitty-featured.bad"
name = "Bad"
version = "0.1.0"
description = "bad"

[compat]
plugin-api = "^1.0"

[lazy]
commands = []
events = []
"#,
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"bitty.commands.register({ id = "not-reserved", title = "x", run = function() end })"#,
    )
    .expect("init");

    let mut rt = runtime(vec![root.clone()], data.clone(), false);
    rt.discover();
    let id = PluginId::new("bitty-featured.bad").expect("id");
    let result = rt.activate(&id);
    assert!(result.is_err(), "undeclared command must fail closed");
    assert!(matches!(rt.state(&id), Some(LifecycleState::Failed(_))));
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

/// Environment-gated end-to-end activation of the real `activity` plugin.
///
/// Set `BITTY_ACTIVITY_PLUGIN_DIR` to the directory containing the activity
/// `bitty-plugin.toml` to run it; unset skips with a notice.
#[test]
fn real_activity_plugin_activates() {
    let Ok(dir) = std::env::var("BITTY_ACTIVITY_PLUGIN_DIR") else {
        eprintln!("SKIP: set BITTY_ACTIVITY_PLUGIN_DIR to run the real activity activation");
        return;
    };
    let data = temp_dir("activity");
    let mut rt = runtime(vec![PathBuf::from(&dir)], data.clone(), false);
    rt.discover();
    let id = PluginId::new("bitty-featured.activity").expect("id");
    assert!(
        rt.discovered_ids().contains(&id),
        "activity must be discovered from {dir}"
    );
    let report = rt.activate(&id).expect("activity activates");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.commands, 2, "summary + clear");
    assert_eq!(report.events, 6, "declared observation + lifecycle events");
    let result = rt
        .dispatch_command(&id, "summary", &[])
        .expect("summary runs");
    assert!(matches!(result, LuaValue::String(_)));
    let _ = std::fs::remove_dir_all(&data);
}
