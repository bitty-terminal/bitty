//! Gap A + C-minimal runtime tests: discovery, activation, lifecycle, host
//! services, capture validation, atomic rollback, provenance-based safe mode.

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

fn runtime(
    bundled_roots: Vec<PathBuf>,
    third_party_roots: Vec<PathBuf>,
    data_dir: PathBuf,
    safe_mode: bool,
) -> PluginRuntime {
    let mut settings = MapSettings::default();
    settings
        .0
        .insert("retention_days".to_string(), LuaValue::Integer(7));
    PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots,
        third_party_roots,
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

/// Write a minimal plugin package under `root/<id>/`.
fn write_plugin(root: &Path, id: &str, commands: &[&str], init_src: &str) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let commands_toml = commands
        .iter()
        .map(|command| format!("\"{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
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

[lazy]
commands = [{commands_toml}]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
    plugin
}

fn sample_id() -> PluginId {
    PluginId::new("bitty-featured.sample").expect("id")
}

#[test]
fn discovers_activates_and_dispatches_bundled_plugin() {
    let data = temp_dir("activate");
    let mut rt = runtime(Vec::new(), vec![fixtures_root()], data.clone(), false);
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
    let mut rt = runtime(Vec::new(), vec![fixtures_root()], data.clone(), false);
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
    let mut rt = runtime(Vec::new(), vec![fixtures_root()], data.clone(), false);
    rt.discover();
    rt.activate(&sample_id()).expect("activate");

    rt.suspend(&sample_id()).expect("suspend");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Suspended));
    assert!(rt.suspend(&sample_id()).is_err(), "double suspend rejected");

    rt.resume(&sample_id()).expect("resume");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Active));

    rt.dispose(&sample_id()).expect("dispose");
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Disposed));
    assert!(
        !rt.host_has_plugin(&sample_id()),
        "dispose purges host identity so Disposed -> activate is possible"
    );

    // A disposed generation can be activated again from clean host state.
    let report = rt
        .activate(&sample_id())
        .expect("re-activate after dispose");
    assert_eq!(report.state, LifecycleState::Active);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn safe_mode_creates_no_third_party_vm() {
    let data = temp_dir("safe");
    let mut rt = runtime(Vec::new(), vec![fixtures_root()], data.clone(), true);
    rt.discover();
    let report = rt.activate(&sample_id()).expect("activate");
    assert!(report.skipped_safe_mode);
    assert_eq!(report.commands, 0);
    assert_eq!(rt.state(&sample_id()), Some(&LifecycleState::Unloaded));
    assert!(rt.dispatch_command(&sample_id(), "summary", &[]).is_err());
    assert!(
        !rt.host_has_plugin(&sample_id()),
        "no host entry under --safe"
    );
    let _ = std::fs::remove_dir_all(&data);
}

/// A package whose id looks like the built-in namespace must not obtain a VM
/// under `--safe` when its provenance is third-party (root-based trust).
#[test]
fn bundled_named_third_party_package_skipped_under_safe_mode() {
    let root = temp_dir("provenance");
    let id = PluginId::new("bitty.evil").expect("id");
    write_plugin(&root, "bitty.evil", &[], "return {}");

    let data_safe = temp_dir("provenance-safe");
    let mut rt = runtime(Vec::new(), vec![root.clone()], data_safe.clone(), true);
    rt.discover();
    let report = rt.activate(&id).expect("activate");
    assert!(
        report.skipped_safe_mode,
        "a self-declared built-in id must not confer bundled trust"
    );
    assert_eq!(rt.state(&id), Some(&LifecycleState::Unloaded));
    assert!(!rt.host_has_plugin(&id));
    let _ = std::fs::remove_dir_all(&data_safe);

    // The same third-party package loads without `--safe`.
    let data = temp_dir("provenance-open");
    let mut rt = runtime(Vec::new(), vec![root.clone()], data.clone(), false);
    rt.discover();
    let report = rt.activate(&id).expect("activate");
    assert_eq!(report.state, LifecycleState::Active);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

/// A package discovered under a trusted `bundled` root is provenance-bundled
/// and is still activated under `--safe` (RFC A.4 rule 6 only forbids
/// third-party VMs).
#[test]
fn bundled_root_package_loads_under_safe_mode() {
    let root = temp_dir("bundled-root");
    let id = PluginId::new("bitty.builtin").expect("id");
    write_plugin(&root, "bitty.builtin", &[], "return {}");

    let data = temp_dir("bundled-data");
    let mut rt = runtime(vec![root.clone()], Vec::new(), data.clone(), true);
    rt.discover();
    let report = rt.activate(&id).expect("activate");
    assert!(
        !report.skipped_safe_mode,
        "bundled provenance loads under --safe"
    );
    assert_eq!(report.state, LifecycleState::Active);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn undeclared_registration_fails_capture_validation_and_rolls_back() {
    let data = temp_dir("capture");
    let root = temp_dir("capture-root");
    let id = PluginId::new("bitty-featured.bad").expect("id");
    write_plugin(
        &root,
        "bitty-featured.bad",
        &["bitty-featured.bad:ok"],
        r#"bitty.commands.register({ id = "not-reserved", title = "x", run = function() end })"#,
    );

    let mut rt = runtime(Vec::new(), vec![root.clone()], data.clone(), false);
    rt.discover();
    let result = rt.activate(&id);
    assert!(result.is_err(), "undeclared command must fail closed");
    assert!(matches!(rt.state(&id), Some(LifecycleState::Failed(_))));
    assert!(
        !rt.host_has_plugin(&id),
        "partial host activation must be purged"
    );
    assert!(
        !rt.host_owns_command("bitty-featured.bad:not-reserved"),
        "failed generation must not retain command ownership"
    );
    assert!(
        !rt.host_owns_command("bitty-featured.bad:ok"),
        "reserved command ownership must be released"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

/// After a failed activation purges host state, repairing the plugin and
/// retrying must succeed (no `Duplicate` from a stale host generation).
#[test]
fn failed_activation_allows_clean_retry() {
    let data = temp_dir("rollback-retry");
    let root = temp_dir("rollback-retry-root");
    let id = PluginId::new("bitty-featured.retry").expect("id");
    let plugin = write_plugin(
        &root,
        "bitty-featured.retry",
        &["bitty-featured.retry:ok"],
        r#"bitty.commands.register({ id = "wrong", title = "x", run = function() end })"#,
    );

    let mut rt = runtime(Vec::new(), vec![root.clone()], data.clone(), false);
    rt.discover();
    assert!(rt.activate(&id).is_err(), "first attempt fails");
    assert!(matches!(rt.state(&id), Some(LifecycleState::Failed(_))));
    assert!(!rt.host_has_plugin(&id));

    // Repair only the module (the manifest is unchanged) and retry.
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"bitty.commands.register({ id = "ok", title = "ok", run = function() return "ok" end })"#,
    )
    .expect("rewrite init");
    let report = rt
        .activate(&id)
        .expect("retry must succeed from clean host state");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.commands, 1);
    assert!(rt.host_owns_command("bitty-featured.retry:ok"));
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

/// Every mechanism-stage failure must leave no partial activation: the host
/// identity is purged, command ownership released, and the local generation is
/// terminally failed.
#[test]
fn mechanism_failures_roll_back_host_state() {
    for (tag, init_src) in [
        ("runtime-error", r#"error("boom")"#),
        ("suspended", "while true do end"),
        ("invalid-syntax", "this is not lua"),
    ] {
        let data = temp_dir(&format!("rollback-{tag}"));
        let root = temp_dir(&format!("rollback-{tag}-root"));
        let id = PluginId::new("bitty-featured.stage").expect("id");
        write_plugin(
            &root,
            "bitty-featured.stage",
            &["bitty-featured.stage:ok"],
            init_src,
        );

        let mut rt = runtime(Vec::new(), vec![root.clone()], data.clone(), false);
        rt.discover();
        assert!(rt.activate(&id).is_err(), "{tag}: activation must fail");
        assert!(
            matches!(rt.state(&id), Some(LifecycleState::Failed(_))),
            "{tag}: generation must be Failed"
        );
        assert!(
            !rt.host_has_plugin(&id),
            "{tag}: host identity must be purged"
        );
        assert!(
            !rt.host_owns_command("bitty-featured.stage:ok"),
            "{tag}: command ownership must be released"
        );
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A store-open failure occurs before the VM is created but after the policy
/// half committed, so it must still roll the host back.
#[test]
fn store_open_failure_rolls_back_host_state() {
    let data = temp_dir("rollback-store");
    let root = temp_dir("rollback-store-root");
    let id = PluginId::new("bitty-featured.store").expect("id");
    write_plugin(
        &root,
        "bitty-featured.store",
        &["bitty-featured.store:ok"],
        "return {}",
    );
    // A store file over the file ceiling makes `open_store` fail closed.
    let store_dir = data.join("bitty-featured.store");
    std::fs::create_dir_all(&store_dir).expect("store dir");
    std::fs::write(store_dir.join("store.json"), vec![b'a'; 200_000]).expect("oversized store");

    let mut rt = runtime(Vec::new(), vec![root.clone()], data.clone(), false);
    rt.discover();
    assert!(
        rt.activate(&id).is_err(),
        "oversized store must fail closed"
    );
    assert!(matches!(rt.state(&id), Some(LifecycleState::Failed(_))));
    assert!(!rt.host_has_plugin(&id));
    assert!(!rt.host_owns_command("bitty-featured.store:ok"));
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
    let mut rt = runtime(Vec::new(), vec![PathBuf::from(&dir)], data.clone(), false);
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
