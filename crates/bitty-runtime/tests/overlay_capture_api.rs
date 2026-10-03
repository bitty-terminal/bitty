//! Focusable-overlay and transient input-capture host API (CTX-0941, W-28).
//!
//! Phase-A coverage over the accepted W-01 contract: the Lua lifecycle
//! (`bitty.ui.overlay.acquire` / `update` / `poll` / `release`) with the
//! decided spellings, per-plugin budgets (256-event queue, 4096-byte calls,
//! v1 scene budgets per update, 30 s idle timeout, single global owner),
//! version/compat validation that disables with a diagnostic and never
//! partially activates, the guaranteed-release matrix on all six paths with
//! terminal input restored, and the `overlay.released` bus event observed
//! without polling.

mod common;

use std::path::{Path, PathBuf};

use bitty_lua::{HostServices, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot(LuaValue);

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, bitty_lua::BridgeError> {
        if scope != "semantic" {
            return Err(bitty_lua::BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-overlay-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(roots: Vec<PathBuf>, data_dir: PathBuf) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: std::rc::Rc::new(MapSettings::default()),
        snapshot: std::rc::Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))),
    });
    common::install_stub_backend(&mut rt);
    rt
}

fn write_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    events: &[&str],
    plugin_api: &str,
    init_src: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps_toml = capabilities
        .iter()
        .map(|c| format!("{c} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let commands_toml = commands
        .iter()
        .map(|c| format!("\"{id}:{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let events_toml = events
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Overlay API Test"
version = "0.1.0"
description = "overlay capture host api test"

[compat]
plugin-api = "{plugin_api}"

[capabilities]
{caps_toml}

[lazy]
commands = [{commands_toml}]
events = [{events_toml}]
claims = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

fn plugin_id(id: &str) -> PluginId {
    PluginId::new(id).expect("valid id")
}

fn store_value(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<LuaValue> {
    rt.services(id)
        .and_then(|services| services.with_store(|store| store.get(key)))
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    id: PluginId,
}

impl Fixture {
    fn activate_full(
        tag: &str,
        id: &str,
        capabilities: &[&str],
        commands: &[&str],
        events: &[&str],
        init_src: &str,
    ) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin(&root, id, capabilities, commands, events, "^1.0", init_src);
        let mut runtime = runtime(vec![root.clone()], data.clone());
        runtime.discover();
        let id = plugin_id(id);
        let report = runtime.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        Self {
            root,
            data,
            runtime,
            id,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

const FOCUS_GRANTS: &[&str] = &["ui.rich", "ui.overlay", "ui.overlay.focus"];

/// Full Lua lifecycle per the accepted spellings: spec acquire returns a
/// handle, update replaces content, poll reports the active table, release
/// with a disposition ends the session, and the next poll reports the exact
/// release reason.
#[test]
fn overlay_lifecycle_spec_acquire_update_poll_release() {
    let mut fixture = Fixture::activate_full(
        "lifecycle",
        "bitty-featured.uilifecycle",
        FOCUS_GRANTS,
        &["probe"],
        &[],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local h = bitty.ui.overlay.acquire({ title = "Palette", placeholder = "Type…" })
            local updated = bitty.ui.overlay.update(h, { kind = "Text", text = "results" })
            local live = bitty.ui.overlay.poll(h)
            local released = bitty.ui.overlay.release(h, "submitted")
            local after = bitty.ui.overlay.poll(h)
            local again = bitty.ui.overlay.release(h)
            bitty.store.set(key .. "_handle", type(h) == "number" and h or -1)
            bitty.store.set(key .. "_updated", updated)
            bitty.store.set(key .. "_live_status", live.status)
            bitty.store.set(key .. "_released", released)
            bitty.store.set(key .. "_after_status", after.status)
            bitty.store.set(key .. "_after_reason", after.reason or "NONE")
            bitty.store.set(key .. "_after_events", #after.events)
            bitty.store.set(key .. "_again", again)
            return true
          end,
        })
        return {}
        "#,
    );
    fixture
        .runtime
        .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
        .expect("dispatch");
    let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("spec acquire must return a handle, got {other:?}"),
    };
    assert!(handle > 0, "opaque session handle must be positive");
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_updated"),
        Some(LuaValue::Bool(true))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_live_status"),
        Some(LuaValue::String("active".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_released"),
        Some(LuaValue::Bool(true))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_after_status"),
        Some(LuaValue::String("released".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_after_reason"),
        Some(LuaValue::String("submitted".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_after_events"),
        Some(LuaValue::Integer(0))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_again"),
        Some(LuaValue::Bool(false)),
        "idempotent release is a success-without-effect, never an error"
    );
    assert!(
        !fixture.runtime.overlay_capture().borrow().is_active(),
        "released session holds no capture"
    );
    assert!(
        !fixture.runtime.push_overlay_input("key", "k"),
        "terminal input is restored after release"
    );
}

/// Budgets hold: a 257-event burst keeps the newest 256 with the sticky
/// overflow flag, an over-bound spec fails with a value-shape error, and an
/// invalid release reason is a validation error that leaves the session
/// unchanged.
#[test]
fn overlay_budgets_exhaustion_overflow_and_call_ceilings() {
    let mut fixture = Fixture::activate_full(
        "budgets",
        "bitty-featured.uibudgets",
        FOCUS_GRANTS,
        &["probe"],
        &[],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local h = bitty.ui.overlay.acquire({ title = "Test" })
            bitty.store.set(key .. "_handle", h)
            local big_ok, big_err = pcall(bitty.ui.overlay.acquire, { title = string.rep("x", 2048) })
            bitty.store.set(key .. "_big", big_ok and "NONE" or big_err.code)
            local bad_ok, bad_err = pcall(bitty.ui.overlay.release, h, "bogus")
            bitty.store.set(key .. "_bad", bad_ok and "NONE" or bad_err.code)
            local live = bitty.ui.overlay.poll(h)
            bitty.store.set(key .. "_still", live.status)
            return true
          end,
        })
        return {}
        "#,
    );
    fixture
        .runtime
        .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
        .expect("dispatch");
    let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("acquire must return a handle, got {other:?}"),
    };
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_big"),
        Some(LuaValue::String("E_VALUE_BYTES".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_bad"),
        Some(LuaValue::String("E_DEF_INVALID".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_still"),
        Some(LuaValue::String("active".to_string())),
        "an invalid reason leaves the session unchanged"
    );
    // 257-event burst: the oldest is dropped, the newest 256 survive, and the
    // sticky flag is set. The session stays active (overflow never releases).
    for index in 0..257 {
        assert!(
            fixture
                .runtime
                .push_overlay_input("text", &format!("{index}")),
            "capture holds during the burst"
        );
    }
    assert!(
        fixture.runtime.overlay_capture().borrow().is_active(),
        "overflow must not auto-release"
    );
    let services = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    let poll = services
        .ui_overlay_poll_detailed(handle, 256)
        .expect("detailed poll");
    assert!(poll.active, "burst owner still holds the session");
    assert_eq!(poll.events.len(), 256);
    assert_eq!(poll.events[0].text, "1", "oldest event is dropped");
    assert!(poll.overflowed, "sticky overflow flag is set");
    assert!(poll.reason.is_none());
}

/// A version mismatch disables the plugin with a diagnostic and no partial
/// activation: no VM, no services, no capture.
#[test]
fn overlay_compat_mismatch_disables_without_partial_activation() {
    let root = temp_dir("compat-root");
    let data = temp_dir("compat-data");
    let id = "bitty-featured.uicompat";
    write_plugin(&root, id, FOCUS_GRANTS, &[], &[], "^99.0", r#"return {}"#);
    let mut rt = runtime(vec![root.clone()], data.clone());
    rt.discover();
    let pid = plugin_id(id);
    let error = rt.activate(&pid).expect_err("mismatch must fail");
    assert_eq!(error.code(), "E_INCOMPATIBLE");
    assert!(
        matches!(rt.state(&pid), Some(LifecycleState::Failed(_))),
        "mismatch disables with a diagnostic"
    );
    assert!(
        rt.services(&pid).is_none(),
        "no partial activation survives"
    );
    assert!(
        !rt.overlay_capture().borrow().is_active(),
        "no capture without activation"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

/// Guaranteed release on every path: cancel, submit, focus-switch, unload,
/// crash, and timeout each restore terminal input, report the exact reason
/// on the next poll, and queue the bus observation.
#[test]
fn capture_guaranteed_release_matrix_restores_input() {
    // Cancel and submit dispositions through explicit release.
    for (tag, disposition, reason) in [
        ("cancel", "cancelled", "cancelled"),
        ("submit", "submitted", "submitted"),
    ] {
        let mut fixture = Fixture::activate_full(
            tag,
            &format!("bitty-featured.uirel{tag}"),
            FOCUS_GRANTS,
            &["probe"],
            &[],
            r#"
            bitty.commands.register({
              id = "probe",
              title = "Probe",
              run = function(key)
                local h = bitty.ui.overlay.acquire({ title = "Test" })
                bitty.store.set(key .. "_handle", h)
                return true
              end,
            })
            return {}
            "#,
        );
        fixture
            .runtime
            .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
            .expect("dispatch");
        let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
            Some(LuaValue::Integer(handle)) => handle,
            other => panic!("acquire must return a handle, got {other:?}"),
        };
        assert!(fixture.runtime.push_overlay_input("key", "k"));
        let services = fixture
            .runtime
            .services(&fixture.id)
            .expect("services")
            .clone();
        assert!(
            services
                .ui_overlay_release_with_reason(handle, Some(disposition))
                .expect("release"),
            "{tag}: release succeeds"
        );
        assert!(
            !fixture.runtime.push_overlay_input("key", "k"),
            "{tag}: terminal input is restored"
        );
        let after = services
            .ui_overlay_poll_detailed(handle, 8)
            .expect("poll after release");
        assert!(!after.active);
        assert_eq!(after.reason.as_deref(), Some(reason));
        let drained = fixture.runtime.drain_overlay_released();
        assert_eq!(drained.len(), 1, "{tag}: one bus observation is queued");
        assert_eq!(drained[0].owner, fixture.id.as_str());
        assert_eq!(drained[0].reason, reason);
    }

    // Focus switch through the application revoke path.
    {
        let mut fixture = Fixture::activate_full(
            "focus",
            "bitty-featured.uirelfocus",
            FOCUS_GRANTS,
            &["probe"],
            &[],
            r#"
            bitty.commands.register({
              id = "probe",
              title = "Probe",
              run = function(key)
                local h = bitty.ui.overlay.acquire({ title = "Test" })
                bitty.store.set(key .. "_handle", h)
                return true
              end,
            })
            return {}
            "#,
        );
        fixture
            .runtime
            .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
            .expect("dispatch");
        let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
            Some(LuaValue::Integer(handle)) => handle,
            other => panic!("acquire must return a handle, got {other:?}"),
        };
        assert!(fixture.runtime.push_overlay_input("key", "k"));
        assert!(
            fixture.runtime.revoke_overlay_capture(),
            "focus switch revokes"
        );
        assert!(
            !fixture.runtime.push_overlay_input("key", "k"),
            "terminal input is restored after focus switch"
        );
        let services = fixture
            .runtime
            .services(&fixture.id)
            .expect("services")
            .clone();
        let after = services
            .ui_overlay_poll_detailed(handle, 8)
            .expect("poll after focus switch");
        assert!(!after.active);
        assert_eq!(after.reason.as_deref(), Some("focus_switched"));
        let drained = fixture.runtime.drain_overlay_released();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, "focus_switched");
    }

    // Unload through dispose.
    {
        let mut fixture = Fixture::activate_full(
            "unload",
            "bitty-featured.uirelunload",
            FOCUS_GRANTS,
            &["probe"],
            &[],
            r#"
            bitty.commands.register({
              id = "probe",
              title = "Probe",
              run = function(key)
                local h = bitty.ui.overlay.acquire({ title = "Test" })
                bitty.store.set(key .. "_handle", h)
                return true
              end,
            })
            return {}
            "#,
        );
        fixture
            .runtime
            .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
            .expect("dispatch");
        assert!(fixture.runtime.push_overlay_input("key", "k"));
        fixture.runtime.dispose(&fixture.id).expect("dispose");
        assert!(
            !fixture.runtime.overlay_capture().borrow().is_active(),
            "unload revokes capture"
        );
        assert!(
            !fixture.runtime.push_overlay_input("key", "k"),
            "terminal input is restored after unload"
        );
        let drained = fixture.runtime.drain_overlay_released();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, "unloaded");
    }

    // Crash through a failed activation rollback.
    {
        let root = temp_dir("crash-root");
        let data = temp_dir("crash-data");
        let id = "bitty-featured.uirelcrash";
        write_plugin(
            &root,
            id,
            FOCUS_GRANTS,
            &[],
            &[],
            "^1.0",
            r#"
            local h = bitty.ui.overlay.acquire({ title = "Test" })
            error("activation crashes after acquiring the capture")
            "#,
        );
        let mut rt = runtime(vec![root.clone()], data.clone());
        rt.discover();
        let pid = plugin_id(id);
        let _ = rt.activate(&pid);
        assert!(matches!(rt.state(&pid), Some(LifecycleState::Failed(_))));
        assert!(!rt.overlay_capture().borrow().is_active());
        assert!(!rt.push_overlay_input("key", "k"));
        let drained = rt.drain_overlay_released();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, "crashed");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    // Timeout through the Core-side idle deadline.
    {
        let mut fixture = Fixture::activate_full(
            "timeout",
            "bitty-featured.uireltimeout",
            FOCUS_GRANTS,
            &["probe"],
            &[],
            r#"
            bitty.commands.register({
              id = "probe",
              title = "Probe",
              run = function(key)
                local h = bitty.ui.overlay.acquire({ title = "Test" })
                bitty.store.set(key .. "_handle", h)
                return true
              end,
            })
            return {}
            "#,
        );
        fixture
            .runtime
            .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
            .expect("dispatch");
        let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
            Some(LuaValue::Integer(handle)) => handle,
            other => panic!("acquire must return a handle, got {other:?}"),
        };
        assert!(
            fixture
                .runtime
                .overlay_capture()
                .borrow_mut()
                .force_expire(fixture.id.as_str(), handle),
            "live capture is rewound"
        );
        assert!(
            fixture.runtime.expire_overlay_captures(),
            "the Core tick revokes the expired capture"
        );
        assert!(
            !fixture.runtime.push_overlay_input("key", "k"),
            "terminal input is restored after timeout"
        );
        let services = fixture
            .runtime
            .services(&fixture.id)
            .expect("services")
            .clone();
        let after = services
            .ui_overlay_poll_detailed(handle, 8)
            .expect("poll after timeout");
        assert!(!after.active);
        assert_eq!(after.reason.as_deref(), Some("timeout"));
        let drained = fixture.runtime.drain_overlay_released();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, "timeout");
    }
}

/// A session that ends on the Core-side idle timeout unmounts its
/// spec-acquired surface: no orphan block survives expiry, and the next
/// session presents only the new surface.
#[test]
fn overlay_spec_surface_unmounted_on_expiry() {
    let mut fixture = Fixture::activate_full(
        "expire-surface",
        "bitty-featured.uiexpsurface",
        FOCUS_GRANTS,
        &["probe"],
        &[],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local h = bitty.ui.overlay.acquire({ title = key })
            bitty.store.set(key .. "_handle", h)
            return true
          end,
        })
        return {}
        "#,
    );
    fixture
        .runtime
        .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
        .expect("dispatch");
    let handle = match store_value(&fixture.runtime, &fixture.id, "x_handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("acquire must return a handle, got {other:?}"),
    };
    assert!(
        fixture
            .runtime
            .overlay_capture()
            .borrow_mut()
            .force_expire(fixture.id.as_str(), handle),
        "live capture is rewound"
    );
    assert!(
        fixture.runtime.expire_overlay_captures(),
        "the Core tick revokes the expired capture"
    );
    assert!(
        fixture
            .runtime
            .ui_blocks()
            .iter()
            .all(|(id, _, _, _)| id.as_str() != fixture.id.as_str()),
        "expiry unmounts the spec surface: no orphan block survives"
    );
    // The next session starts clean with only its own surface retained.
    fixture
        .runtime
        .dispatch_command(&fixture.id, "probe", &[LuaValue::String("y".to_string())])
        .expect("dispatch");
    let retained: Vec<_> = fixture
        .runtime
        .ui_blocks()
        .into_iter()
        .filter(|(id, _, _, _)| id.as_str() == fixture.id.as_str())
        .collect();
    assert_eq!(retained.len(), 1, "only the live surface is retained");
    let services = fixture
        .runtime
        .services(&fixture.id)
        .expect("services")
        .clone();
    let live = match store_value(&fixture.runtime, &fixture.id, "y_handle") {
        Some(LuaValue::Integer(handle)) => handle,
        other => panic!("re-acquire must return a handle, got {other:?}"),
    };
    assert_ne!(handle, live, "sessions mint distinct handles");
    assert!(
        services
            .ui_overlay_poll_detailed(live, 8)
            .expect("poll")
            .active,
        "the new session holds the capture"
    );
}

/// Session end is observable without polling: a subscriber of
/// `overlay.released` observes `{ owner, reason }` on the next cold-path
/// delivery, and no phase may intercept or veto it.
#[test]
fn overlay_released_bus_event_observed_without_poll() {
    let mut holder = Fixture::activate_full(
        "holder",
        "bitty-featured.uirelholder",
        FOCUS_GRANTS,
        &["probe"],
        &[],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local h = bitty.ui.overlay.acquire({ title = "Test" })
            bitty.ui.overlay.release(h, "cancelled")
            return true
          end,
        })
        return {}
        "#,
    );
    // A second plugin in the same runtime subscribes to the bus event and
    // records the observed payload. Observation needs no capture grant.
    let watcher = "bitty-featured.uirelwatcher";
    write_plugin(
        &holder.root,
        watcher,
        &[],
        &[],
        &["overlay.released"],
        "^1.0",
        r#"
        bitty.events.subscribe("overlay.released", function(envelope)
          bitty.store.set("owner", envelope.payload.owner)
          bitty.store.set("reason", envelope.payload.reason)
        end)
        return {}
        "#,
    );
    holder.runtime.discover();
    let watcher_id = plugin_id(watcher);
    let report = holder
        .runtime
        .activate(&watcher_id)
        .expect("watcher activates");
    assert_eq!(report.state, LifecycleState::Active);
    holder
        .runtime
        .dispatch_command(&holder.id, "probe", &[LuaValue::String("x".to_string())])
        .expect("dispatch");
    // Mimic the application tick: drain ended sessions and deliver each
    // observation through the event pipeline on the cold path.
    let ended = holder.runtime.drain_overlay_released();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].owner, holder.id.as_str());
    assert_eq!(ended[0].reason, "cancelled");
    let mut delivered = 0;
    for event in &ended {
        let payload = LuaValue::table([
            ("owner", LuaValue::String(event.owner.clone())),
            ("reason", LuaValue::String(event.reason.clone())),
        ]);
        delivered += holder.runtime.deliver_event("overlay.released", &payload);
    }
    assert_eq!(delivered, 1, "exactly the subscriber observes the release");
    assert_eq!(
        store_value(&holder.runtime, &watcher_id, "owner"),
        Some(LuaValue::String(holder.id.as_str().to_string()))
    );
    assert_eq!(
        store_value(&holder.runtime, &watcher_id, "reason"),
        Some(LuaValue::String("cancelled".to_string()))
    );
}

/// Without `ui.overlay.focus` every capture call fails with
/// `E_CAPABILITY_DENIED` naming the capability, while the v1 presentation
/// slot keeps working: existing overlay consumers are unaffected.
#[test]
fn overlay_focus_denied_without_grant_v1_slot_unaffected() {
    let mut fixture = Fixture::activate_full(
        "deny",
        "bitty-featured.uireldeny",
        &["ui.rich", "ui.overlay"],
        &["probe"],
        &[],
        r#"
        bitty.commands.register({
          id = "probe",
          title = "Probe",
          run = function(key)
            local ok, h = pcall(bitty.ui.mount, "overlay", { kind = "Text", text = "modal" })
            bitty.store.set(key .. "_mount_ok", ok)
            bitty.store.set(key .. "_handle", ok and h or -1)
            local a_ok, a_err = pcall(bitty.ui.overlay.acquire, h)
            bitty.store.set(key .. "_acquire", a_ok and "NONE" or a_err.code)
            bitty.store.set(key .. "_message", a_ok and "NONE" or a_err.message)
            local s_ok, s_err = pcall(bitty.ui.overlay.acquire, { title = "Test" })
            bitty.store.set(key .. "_spec", s_ok and "NONE" or s_err.code)
            return true
          end,
        })
        return {}
        "#,
    );
    fixture
        .runtime
        .dispatch_command(&fixture.id, "probe", &[LuaValue::String("x".to_string())])
        .expect("dispatch");
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_mount_ok"),
        Some(LuaValue::Bool(true)),
        "v1 presentation mount is unaffected"
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_acquire"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    assert_eq!(
        store_value(&fixture.runtime, &fixture.id, "x_spec"),
        Some(LuaValue::String("E_CAPABILITY_DENIED".to_string()))
    );
    let message = match store_value(&fixture.runtime, &fixture.id, "x_message") {
        Some(LuaValue::String(message)) => message,
        other => panic!("denial must carry a message, got {other:?}"),
    };
    assert!(
        message.contains("ui.overlay.focus"),
        "denial names the missing capability: {message}"
    );
    assert!(
        !fixture.runtime.overlay_capture().borrow().is_active(),
        "denied acquire starts no session"
    );
}
