//! CTX-1244 (issue #2107): host one-shot timer pump.
//!
//! Armed plugin timers fire on cadence while idle (fake-clock advance
//! delivers ticks and runs callbacks); unarmed idle stays deadline-free
//! (zero-submit preserved). Cold-tick only, never the hot path.

mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bitty_lua::{
    BridgeError, ComponentBridgeRequest, ComponentMethod, LuaValue, RC1_WALL_CLOCK_BUDGET_MS,
};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    CoreTargetInfo, LifecycleState, MAX_WALL_RECOVERY_ATTEMPTS, PluginRuntime, PluginRuntimeConfig,
    PluginRuntimeError, SettingsSource, SnapshotSource, TargetSource,
};

/// Activation wall budget for this harness.
///
/// These fixtures activate in a few milliseconds, but RC-1 (50 ms) is
/// calibrated for release builds: a sharded CI runner has been measured
/// suspending a fixture `init.lua` at 79 ms
/// (`WallClockExceeded { elapsed_ms: 79, budget_ms: 50 }`, bitty#2427). Timer
/// semantics, not runner speed, are under test here, so only the wall
/// dimension is widened (the bitty#1744/#1727 treatment). Production keeps
/// `RC1_WALL_CLOCK_BUDGET_MS`; activation-budget enforcement stays pinned by
/// `bitty-lua/tests/measurement_lua.rs`, `load_gate.rs`, and
/// `bar_activation_budget.rs`.
const HARNESS_WALL_BUDGET_MS: u64 = 10 * RC1_WALL_CLOCK_BUDGET_MS;

/// Wall time the slow snapshot fixture charges inside a timer callback
/// (bitty#2900).
///
/// The host call is charged to the callback's RC-1 wall time (only supervised
/// spawn waits are credited), so a delay of twice the harness budget crosses
/// the slice on any runner without depending on CPU speed. It stays far below
/// `HARNESS_HOST_CALL_DEADLINE_MS`, so the call itself returns normally and
/// the suspension is the callback budget, not a host-call timeout.
const SLOW_SNAPSHOT_DELAY: Duration = Duration::from_millis(2 * HARNESS_WALL_BUDGET_MS);

#[derive(Default)]
struct MapSettings(std::collections::BTreeMap<String, LuaValue>);

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

/// Snapshot source that charges a deterministic amount of host-call wall time
/// (bitty#2900). Used to overrun a timer callback's RC-1 slice without a busy
/// loop or a runner-speed assumption.
struct SlowSnapshot {
    value: LuaValue,
    delay: Duration,
}

impl SnapshotSource for SlowSnapshot {
    fn snapshot(&self, scope: &str) -> Result<LuaValue, BridgeError> {
        if scope != "semantic" {
            return Err(BridgeError::new(
                "validation",
                "E_SNAPSHOT_SCOPE_UNSUPPORTED",
                "bad scope",
            ));
        }
        std::thread::sleep(self.delay);
        Ok(self.value.clone())
    }
}

/// Fixed live Core target source for the targeting-session cleanup probe.
struct FixedTargets(Vec<CoreTargetInfo>);

impl TargetSource for FixedTargets {
    fn targets(&self) -> Result<Vec<CoreTargetInfo>, BridgeError> {
        Ok(self.0.clone())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    // Unique per test + process so parallel nextest shards never share a dir.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bitty-timer-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn runtime(data_dir: PathBuf, roots: Vec<PathBuf>) -> PluginRuntime {
    runtime_with_snapshot(
        data_dir,
        roots,
        Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
        ]))),
    )
}

fn runtime_with_snapshot(
    data_dir: PathBuf,
    roots: Vec<PathBuf>,
    snapshot: Rc<dyn SnapshotSource>,
) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(MapSettings::default()),
        snapshot,
    });
    // Harness headroom for activation only; see `HARNESS_WALL_BUDGET_MS`.
    rt.set_vm_wall_budget_ms(Some(HARNESS_WALL_BUDGET_MS));
    // bitty#2800: host calls are charged on their own raw wall clock; widen
    // that probe-only dimension too.
    rt.set_host_call_deadline_ms(Some(common::HARNESS_HOST_CALL_DEADLINE_MS));
    rt
}

fn write_plugin(root: &Path, id: &str, init_src: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n[lazy]\ncommands = [\"{id}:ok\"]\nevents = []\n"
        ),
    )
    .expect("manifest");
    let body = format!(
        "bitty.commands.register({{ id = \"ok\", title = \"ok\", run = function() return \"ok\" end }})\n{init_src}"
    );
    std::fs::write(plugin.join("lua/init.lua"), body).expect("init");
}

/// Write a plugin with explicit capabilities and no command registrations.
fn write_capability_plugin(root: &Path, id: &str, capabilities: &[&str], init_src: &str) {
    write_full_plugin(root, id, capabilities, &[], &[], "", init_src);
}

/// Write a plugin with explicit capabilities, commands, events, and an
/// optional `[services.provided]` body (the failure-path probes).
fn write_full_plugin(
    root: &Path,
    id: &str,
    capabilities: &[&str],
    commands: &[&str],
    events: &[&str],
    services_toml: &str,
    init_src: &str,
) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    let caps_toml = capabilities
        .iter()
        .map(|capability| format!("{capability} = true"))
        .collect::<Vec<_>>()
        .join("\n");
    let commands_toml = commands
        .iter()
        .map(|command| format!("\"{id}:{command}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let events_toml = events
        .iter()
        .map(|event| format!("\"{event}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let services_section = if services_toml.is_empty() {
        String::new()
    } else {
        format!("{services_toml}\n")
    };
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n{caps_toml}\n\n{services_section}[lazy]\ncommands = [{commands_toml}]\nevents = [{events_toml}]\n"
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src).expect("init");
}

fn store_get(rt: &PluginRuntime, id: &PluginId, key: &str) -> Option<LuaValue> {
    rt.services(id)
        .and_then(|services| services.with_store(|store| store.get(key)))
}

#[test]
fn unarmed_idle_has_no_timer_deadline() {
    // Zero-submit preserved: no plugins, no timers -> no wake.
    let data = temp_dir("unarmed-empty");
    let mut rt = runtime(data.clone(), Vec::new());
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers(), 0);
    let _ = std::fs::remove_dir_all(&data);

    // Activated plugin with no timers also arms nothing.
    let data = temp_dir("unarmed-active");
    let root = temp_dir("unarmed-active-root");
    let id = PluginId::new("bitty-featured.notimer").expect("id");
    write_plugin(&root, "bitty-featured.notimer", "-- no timers\n");
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers(), 0);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn armed_timers_fire_on_fake_clock_advance() {
    // Harness proof: fake-clock advance delivers ticks and runs callbacks.
    let data = temp_dir("armed-fire");
    let root = temp_dir("armed-fire-root");
    let id = PluginId::new("bitty-featured.tick").expect("id");
    write_plugin(
        &root,
        "bitty-featured.tick",
        r#"
        bitty.timers.create(100, function() bitty.store.set("tick1", 1) end)
        bitty.timers.create(200, function() bitty.store.set("tick2", 2) end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    // Stub backend so `bitty.store.set` commits in-memory.
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");

    let first = rt.timer_deadline().expect("armed timers must arm a wake");
    // Before the first deadline nothing fires.
    let before = first
        .checked_sub(Duration::from_millis(50))
        .unwrap_or(first);
    // `before` may underflow to `first` itself when the clock is near the
    // start; only assert emptiness when it is strictly earlier.
    if before < first {
        assert_eq!(rt.pump_due_timers_at(before), 0);
        assert_eq!(store_get(&rt, &id, "tick1"), None);
    }
    // First cadence tick fires exactly one callback.
    assert_eq!(rt.pump_due_timers_at(first), 1);
    assert_eq!(store_get(&rt, &id, "tick1"), Some(LuaValue::Integer(1)));
    assert_eq!(store_get(&rt, &id, "tick2"), None);
    // Second cadence tick fires the follow-up; afterwards nothing is armed.
    let second = rt.timer_deadline().expect("second timer still armed");
    assert!(second > first, "deadlines advance on cadence");
    assert_eq!(rt.pump_due_timers_at(second), 1);
    assert_eq!(store_get(&rt, &id, "tick2"), Some(LuaValue::Integer(2)));
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers_at(second), 0);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn one_shot_timers_fire_at_most_once() {
    let data = temp_dir("oneshot");
    let root = temp_dir("oneshot-root");
    let id = PluginId::new("bitty-featured.once").expect("id");
    write_plugin(
        &root,
        "bitty-featured.once",
        r#"
        bitty.timers.create(10, function()
          local n = bitty.store.get("n") or 0
          bitty.store.set("n", n + 1)
        end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let deadline = rt.timer_deadline().expect("armed");
    assert_eq!(rt.pump_due_timers_at(deadline), 1);
    assert_eq!(store_get(&rt, &id, "n"), Some(LuaValue::Integer(1)));
    // Same instant again fires nothing; the one shot is consumed.
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    assert_eq!(store_get(&rt, &id, "n"), Some(LuaValue::Integer(1)));
    assert_eq!(rt.timer_deadline(), None);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cancelled_timers_never_fire() {
    // `timers.cancel` from a command removes the deadline without firing.
    let data = temp_dir("cancel");
    let root = temp_dir("cancel-root");
    let id = PluginId::new("bitty-featured.cancel").expect("id");
    write_plugin(
        &root,
        "bitty-featured.cancel",
        r#"
        local h = bitty.timers.create(100, function() bitty.store.set("fired", true) end)
        bitty.store.set("h", h)
        bitty.commands.register({ id = "stop", title = "stop", run = function()
          local handle = bitty.store.get("h")
          return bitty.timers.cancel(handle)
        end })
        "#,
    );
    // Manifest above only declares `:ok`; add `:stop` explicitly.
    let plugin = root.join("bitty-featured.cancel");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        "[plugin]\nid = \"bitty-featured.cancel\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n[capabilities]\n[lazy]\ncommands = [\"bitty-featured.cancel:ok\", \"bitty-featured.cancel:stop\"]\nevents = []\n",
    )
    .expect("manifest with stop");
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert!(rt.timer_deadline().is_some());
    common::settle_dispatch(&mut rt, &id, "stop", &[]).expect_value("stop");
    // The cancel syncs on the next pump: no wake, no fire.
    assert_eq!(rt.pump_due_timers(), 0);
    assert_eq!(rt.timer_deadline(), None);
    let far = std::time::Instant::now() + Duration::from_secs(60);
    assert_eq!(rt.pump_due_timers_at(far), 0);
    assert_eq!(store_get(&rt, &id, "fired"), None);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn suspended_timers_do_not_fire_or_wake() {
    let data = temp_dir("suspend");
    let root = temp_dir("suspend-root");
    let id = PluginId::new("bitty-featured.pause").expect("id");
    write_plugin(
        &root,
        "bitty-featured.pause",
        r#"bitty.timers.create(50, function() bitty.store.set("fired", true) end)"#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let armed = rt.timer_deadline().expect("armed");
    rt.suspend(&id).expect("suspend");
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers_at(armed), 0);
    assert_eq!(store_get(&rt, &id, "fired"), None);
    rt.resume(&id).expect("resume");
    assert!(rt.timer_deadline().is_some());
    let deadline = rt.timer_deadline().expect("re-armed on resume");
    assert_eq!(rt.pump_due_timers_at(deadline), 1);
    assert_eq!(store_get(&rt, &id, "fired"), Some(LuaValue::Bool(true)));
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disposed_timers_clear_the_wake() {
    let data = temp_dir("dispose");
    let root = temp_dir("dispose-root");
    let id = PluginId::new("bitty-featured.gone").expect("id");
    write_plugin(
        &root,
        "bitty-featured.gone",
        r#"bitty.timers.create(50, function() bitty.store.set("fired", true) end)"#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert!(rt.timer_deadline().is_some());
    rt.dispose(&id).expect("dispose");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Disposed));
    assert_eq!(rt.timer_deadline(), None);
    let far = std::time::Instant::now() + Duration::from_secs(60);
    assert_eq!(rt.pump_due_timers_at(far), 0);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_callback_wall_overrun_fails_the_generation_without_phantom_fires() {
    // bitty#2900: a timer callback that overruns RC-1 used to leave the plugin
    // `Active` with a suspended VM: every later drive was refused, `fired`
    // still counted phantom work, and the last painted blocks stayed frozen on
    // screen. The overrun now fails the generation through the same cleanup as
    // a failed activation, and the reason names the exceeded budget.
    let data = temp_dir("overrun");
    let root = temp_dir("overrun-root");
    let id = PluginId::new("bitty-featured.overrun").expect("id");
    write_capability_plugin(
        &root,
        "bitty-featured.overrun",
        &["terminal.semantic-read", "ui.rich"],
        r#"
        bitty.ui.mount("bottom", { kind = "Text", text = "timer" })
        bitty.timers.create(100, function()
          -- The host call charges deterministic wall time; the loop then
          -- reaches a slice boundary so the drive observes the overrun
          -- instead of finishing inside the slow host step.
          bitty.terminal.snapshot({ scope = "semantic" })
          while true do end
        end)
        "#,
    );
    let snapshot = Rc::new(SlowSnapshot {
        value: LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
        ]),
        delay: SLOW_SNAPSHOT_DELAY,
    });
    let mut rt = runtime_with_snapshot(data.clone(), vec![root.clone()], snapshot);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(rt.ui_blocks().len(), 1, "fixture mounted one UI block");
    let deadline = rt.timer_deadline().expect("armed");

    // The callback starts, charges the slow host call, and is then aborted by
    // the wall check: that is one attempted fire, not zero. The generation
    // must fail in the same pump.
    assert_eq!(rt.pump_due_timers_at(deadline), 1);
    let reason = match rt.state(&id) {
        Some(LifecycleState::Failed(reason)) => reason.clone(),
        other => panic!("terminal overrun must fail the generation, got {other:?}"),
    };
    assert!(
        reason.contains("RC-1 wall")
            && reason.contains(&format!("budget_ms={HARNESS_WALL_BUDGET_MS}")),
        "failure reason must name the exceeded wall budget: {reason}"
    );
    assert!(
        rt.ui_blocks().is_empty(),
        "failed generation's UI blocks must be cleared"
    );
    assert!(
        rt.services(&id).is_none(),
        "failed generation must drop its services"
    );
    assert!(
        rt.effective_budgets(&id).is_none(),
        "failed generation must drop its budgets"
    );
    let debug_row = rt
        .debug_plugins()
        .into_iter()
        .find(|row| row.id == id.as_str())
        .expect("debug row");
    assert_eq!(debug_row.state, "failed");
    assert_eq!(rt.timer_deadline(), None, "failed generation arms no wake");

    // The refused drive is not a fire: the same deadline and any later one
    // select nothing for the failed generation.
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    let far = deadline + Duration::from_secs(3600);
    assert_eq!(rt.pump_due_timers_at(far), 0);
    assert_eq!(rt.timer_deadline(), None);

    // Reload from `Failed` (including `recover_wall_failed_plugins`) is the
    // documented recovery path.
    assert_eq!(rt.recover_wall_failed_plugins(), 1);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    let report = rt.reload(&id).expect("reload from active");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_lua_error_that_says_rc1_wall_is_never_auto_recovered() {
    // CTX-1788 (PR #3109 round-2): recovery used to key off
    // `reason.contains("RC-1 wall")`. A plugin controls its own error text, so
    // `error("RC-1 wall")` in `init.lua` used to earn the same automatic
    // reload a genuine slice timeout gets. Classification is now the typed
    // verdict the suspend path recorded, so a rendered reason that merely
    // mentions the phrase is never recovered.
    let data = temp_dir("wall-text");
    let root = temp_dir("wall-text-root");
    let id = PluginId::new("bitty-featured.walltext").expect("id");
    write_capability_plugin(
        &root,
        "bitty-featured.walltext",
        &["terminal.semantic-read"],
        r#"
        error("RC-1 wall budget_ms=50 elapsed_ms=999")
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    rt.discover();
    let err = rt.activate(&id).expect_err("init.lua must fail");
    assert!(
        err.to_string().contains("RC-1 wall"),
        "the fixture must reproduce the old substring trigger: {err}"
    );
    let reason = match rt.state(&id) {
        Some(LifecycleState::Failed(reason)) => reason.clone(),
        other => panic!("failed activation must leave Failed, got {other:?}"),
    };
    assert!(
        reason.contains("RC-1 wall"),
        "the rendered reason still carries the plugin-chosen text: {reason}"
    );

    // The discriminating assertion: the same phrase in the same state, but no
    // typed wall verdict, must never auto-recover — three passes to be sure
    // the attempt budget cannot mask a one-shot.
    //
    // The fixture is rewritten to a body that WOULD activate cleanly first, so
    // this test cannot pass for the wrong reason: under the old substring
    // predicate the entry is selected, retried, and `recover_wall_failed_plugins`
    // returns 1 (the retry succeeds); under the typed classification nothing is
    // selected and it returns 0. Before this rewrite the fixture failed on
    // every activation, so both implementations returned 0 and the test proved
    // nothing (PR #3109 round-3 review, probe C).
    write_capability_plugin(
        &root,
        "bitty-featured.walltext",
        &["terminal.semantic-read"],
        "-- a retry would activate cleanly\n",
    );
    for attempt in 1..=MAX_WALL_RECOVERY_ATTEMPTS {
        assert_eq!(
            rt.recover_wall_failed_plugins(),
            0,
            "pass {attempt}: a Lua error naming RC-1 wall is not a slice timeout"
        );
        assert!(
            matches!(rt.state(&id), Some(LifecycleState::Failed(_))),
            "pass {attempt}: the generation must stay failed"
        );
    }
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_failure_releases_service_annotations_and_component_tickets() {
    // bitty#2900 review F1-F3: a live generation that fails on a timer must
    // release the authority a failed activation never acquired: published
    // service records, the targeting session and its shared annotation
    // layer, and component tickets/submissions.
    let data = temp_dir("cleanup");
    let root = temp_dir("cleanup-root");
    let id = PluginId::new("bitty-featured.cleanup").expect("id");
    write_full_plugin(
        &root,
        "bitty-featured.cleanup",
        &[
            "runtime.inspect",
            "terminal.semantic-read",
            "ui.rich",
            "ui.overlay",
            "ui.overlay.focus",
        ],
        &[],
        &[],
        "[services.provided]\n\"timer.health\" = { version = \"1.0.0\", heads = [\"runtime.inspect\"] }",
        r#"
        bitty.services.provide("timer.health", {
          ping = function() return { ok = true } end,
        })
        local handle = bitty.ui.mount("overlay", { kind = "Text", text = "targets" })
        bitty.ui.targets.session_start(handle, 80, { { x = 0, y = 0 } }, { "acme.cmd:panel" })
        bitty.timers.create(100, function()
          bitty.terminal.snapshot({ scope = "semantic" })
          while true do end
        end)
        "#,
    );
    let snapshot = Rc::new(SlowSnapshot {
        value: LuaValue::table([("version", LuaValue::Integer(1))]),
        delay: SLOW_SNAPSHOT_DELAY,
    });
    let mut rt = runtime_with_snapshot(data.clone(), vec![root.clone()], snapshot);
    rt.set_target_source(Rc::new(FixedTargets(vec![CoreTargetInfo {
        kind: "panel".to_string(),
        id: 11,
        workspace: Some(7),
        rect: Some((1, 2, 20, 10)),
    }])));
    let native_records = rt.service_directory().borrow().published_count();
    rt.discover();
    rt.activate(&id).expect("activate");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(
        rt.service_directory().borrow().published_count(),
        native_records + 1,
        "provider record published"
    );
    assert_eq!(
        rt.service_directory()
            .borrow()
            .active_for("timer.health")
            .len(),
        1
    );
    assert!(
        rt.target_annotations().is_some(),
        "fixture published an annotation layer"
    );
    let ticket = rt
        .component_request_queue()
        .borrow_mut()
        .enqueue(
            id.as_str(),
            "timer-cleanup-hash",
            ComponentBridgeRequest {
                component: "net".to_string(),
                method: ComponentMethod::Get,
                url: "https://example.invalid/ping".to_string(),
                headers: Vec::new(),
                body: None,
                timeout_ms: 1_000,
            },
            Instant::now(),
        )
        .expect("enqueue component request");
    assert_eq!(rt.component_request_queue().borrow().pending(), 1);

    let deadline = rt.timer_deadline().expect("armed");
    assert_eq!(rt.pump_due_timers_at(deadline), 1);
    assert!(matches!(rt.state(&id), Some(LifecycleState::Failed(_))));
    assert!(
        rt.service_directory()
            .borrow()
            .active_for("timer.health")
            .is_empty(),
        "a failed generation must not keep a resolvable service record"
    );
    assert_eq!(
        rt.service_directory().borrow().published_count(),
        native_records,
        "failure must revoke the provider record"
    );
    assert!(
        rt.target_annotations().is_none(),
        "failure must clear the shared annotation layer"
    );
    assert_eq!(
        rt.component_request_queue().borrow().pending(),
        0,
        "failure must purge queued component submissions"
    );
    assert_eq!(rt.component_request_queue().borrow().in_flight(), 0);
    let poll_error = rt
        .component_request_queue()
        .borrow_mut()
        .poll(id.as_str(), ticket)
        .expect_err("failed generation's ticket is purged");
    assert_eq!(poll_error.code, "E_COMPONENT_TICKET");

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_due_while_command_parked_is_not_counted_and_stays_active() {
    // bitty#2900 review F5: a parked command owns the VM for the tick. The
    // due timer drive is refused before execution, so it is not a fire and
    // the generation is not failed.
    let data = temp_dir("parked");
    let root = temp_dir("parked-root");
    let id = PluginId::new("bitty-featured.parked").expect("id");
    write_full_plugin(
        &root,
        "bitty-featured.parked",
        &["terminal.semantic-read"],
        &["slow"],
        &[],
        "",
        r#"
        bitty.commands.register({
          id = "slow",
          title = "slow",
          run = function()
            bitty.terminal.snapshot({ scope = "semantic" })
            while true do end
          end,
        })
        bitty.timers.create(100, function() bitty.store.set("timer_ran", true) end)
        "#,
    );
    let snapshot = Rc::new(SlowSnapshot {
        value: LuaValue::table([("version", LuaValue::Integer(1))]),
        delay: SLOW_SNAPSHOT_DELAY,
    });
    let mut rt = runtime_with_snapshot(data.clone(), vec![root.clone()], snapshot);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let deadline = rt.timer_deadline().expect("armed");
    let parked = rt
        .dispatch_command(&id, "slow", &[])
        .expect_err("slow command must park");
    assert!(
        matches!(parked, PluginRuntimeError::Parked { .. }),
        "expected a parked command, got {parked:?}"
    );
    assert_eq!(rt.parked_command_count(), 1);
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(store_get(&rt, &id, "timer_ran"), None);
    rt.dispose(&id).expect("dispose cancels the park");
    assert_eq!(rt.parked_command_count(), 0);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_due_while_command_parked_stays_armed_and_fires_after_resume() {
    // bitty#2899: a refused timer drive must not consume the one-shot. A
    // parked command owns the VM for the tick, so the due timer keeps its
    // deadline and fires once the park clears. Consuming it would silently
    // delete a pending tick, and a plugin's finite activation chain (the bar
    // pre-arms 32 one-shot ticks) would then die one refused pump at a time
    // until its cadence stopped for good.
    let data = temp_dir("parked-keep");
    let root = temp_dir("parked-keep-root");
    let id = PluginId::new("bitty-featured.parkedkeep").expect("id");
    write_full_plugin(
        &root,
        "bitty-featured.parkedkeep",
        &["terminal.semantic-read"],
        &["slow"],
        &[],
        "",
        r#"
        bitty.commands.register({
          id = "slow",
          title = "slow",
          run = function()
            for _ = 1, 3 do
              pcall(bitty.terminal.snapshot, { scope = "semantic" })
            end
            return "done"
          end,
        })
        bitty.timers.create(100, function() bitty.store.set("timer_ran", true) end)
        "#,
    );
    let snapshot = Rc::new(SlowSnapshot {
        value: LuaValue::table([("version", LuaValue::Integer(1))]),
        delay: SLOW_SNAPSHOT_DELAY,
    });
    let mut rt = runtime_with_snapshot(data.clone(), vec![root.clone()], snapshot);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let deadline = rt.timer_deadline().expect("armed");
    let parked = rt
        .dispatch_command(&id, "slow", &[])
        .expect_err("slow command must park");
    assert!(
        matches!(parked, PluginRuntimeError::Parked { .. }),
        "expected a parked command, got {parked:?}"
    );
    assert_eq!(
        rt.pump_due_timers_at(deadline),
        0,
        "a refused drive is not a fire"
    );
    assert_eq!(
        rt.timer_deadline(),
        Some(deadline),
        "the refused one-shot keeps its deadline for a later pump"
    );
    assert_eq!(store_get(&rt, &id, "timer_ran"), None);
    // Resume the parked command on the cold phase until it completes.
    for _ in 0..200 {
        if rt.parked_command_count() == 0 {
            break;
        }
        let _ = rt.pump_parked_commands();
    }
    assert_eq!(rt.parked_command_count(), 0, "the command completes");
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(
        rt.pump_due_timers_at(deadline),
        1,
        "the kept one-shot fires once the park clears"
    );
    assert_eq!(
        store_get(&rt, &id, "timer_ran"),
        Some(LuaValue::Bool(true)),
        "the callback ran"
    );
    assert_eq!(
        rt.timer_deadline(),
        None,
        "a fired one-shot leaves no wake (zero-submit preserved)"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_due_after_foreign_suspension_fails_with_refusal_reason() {
    // bitty#2900 review F4/F5: the suspension can be caused by another
    // dispatch. A timer drive that never started must not claim the callback
    // exceeded a budget; it reports the refusal and still fails the
    // generation.
    let data = temp_dir("foreign-suspend");
    let root = temp_dir("foreign-suspend-root");
    let id = PluginId::new("bitty-featured.foreign-suspend").expect("id");
    write_full_plugin(
        &root,
        "bitty-featured.foreign-suspend",
        &[],
        &[],
        &["terminal.opened"],
        "",
        r#"
        bitty.events.subscribe("terminal.opened", function() while true do end end)
        bitty.timers.create(10000, function() bitty.store.set("timer_ran", true) end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    rt.discover();
    rt.activate(&id).expect("activate");
    let deadline = rt.timer_deadline().expect("armed");
    assert_eq!(
        rt.deliver_event(
            "terminal.opened",
            &LuaValue::table([("terminal_id", LuaValue::Integer(1))])
        ),
        0,
        "the overrun event handler does not complete"
    );
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    let reason = match rt.state(&id) {
        Some(LifecycleState::Failed(reason)) => reason.clone(),
        other => panic!("a refused timer drive must fail the generation, got {other:?}"),
    };
    assert!(
        reason.contains("timer drive refused: VM was already suspended"),
        "refused drive must not claim the timer callback ran: {reason}"
    );
    assert!(
        reason.contains("budget"),
        "refusal must still name the suspension budget: {reason}"
    );
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_callback_within_budget_keeps_the_generation_active() {
    // Control for bitty#2900: an ordinary timer that completes inside the
    // slice keeps firing and never transitions the generation.
    let data = temp_dir("within-budget");
    let root = temp_dir("within-budget-root");
    let id = PluginId::new("bitty-featured.within").expect("id");
    write_plugin(
        &root,
        "bitty-featured.within",
        r#"
        bitty.timers.create(100, function() bitty.store.set("first", 1) end)
        bitty.timers.create(200, function() bitty.store.set("second", 2) end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let first = rt.timer_deadline().expect("armed");
    assert_eq!(rt.pump_due_timers_at(first), 1);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(store_get(&rt, &id, "first"), Some(LuaValue::Integer(1)));
    let second = rt.timer_deadline().expect("second timer still armed");
    assert_eq!(rt.pump_due_timers_at(second), 1);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(store_get(&rt, &id, "second"), Some(LuaValue::Integer(2)));
    assert_eq!(rt.timer_deadline(), None);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn timer_callback_lua_error_is_contained_and_keeps_the_generation_active() {
    // bitty#2900 classification control: a plain Lua error does not suspend
    // the VM, so it stays the documented contained failure (the one shot is
    // consumed, the generation remains usable). Only terminal VM death fails
    // the plugin.
    let data = temp_dir("lua-error");
    let root = temp_dir("lua-error-root");
    let id = PluginId::new("bitty-featured.lua-error").expect("id");
    write_plugin(
        &root,
        "bitty-featured.lua-error",
        r#"
        bitty.timers.create(100, function() error("timer boom") end)
        bitty.timers.create(200, function() bitty.store.set("after_error", true) end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    let first = rt.timer_deadline().expect("armed");
    assert_eq!(rt.pump_due_timers_at(first), 1);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    let second = rt.timer_deadline().expect("generation still usable");
    assert_eq!(rt.pump_due_timers_at(second), 1);
    assert_eq!(rt.state(&id), Some(&LifecycleState::Active));
    assert_eq!(
        store_get(&rt, &id, "after_error"),
        Some(LuaValue::Bool(true))
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ten_plan_repeating_chains_survive_past_15s() {
    // CTX-1247 (#2107 follow-up): 10-module bar-plan mimic.
    //
    // Bar arms per=3 timers per module (1 repeating chain + 2 one-shot
    // follow-ups, `chain_ticks(10)==3`) over cadences
    // {2000,2000,5000,3000,5000,5000,5000,2000,250,60000}. Every repeating
    // tick re-arms via pcall (E_DEF_LIMIT swallowed, counted as
    // `rearm_fail`). Pre-fix fired rows are never pruned from the VM
    // capture, so the 64-cap counts dead rows and every successful re-arm
    // permanently consumes a slot: at 64 all creates fail and lineages die
    // ~15s (fires_15-59s==0). Post-fix fired rows are pruned on fire, the
    // cap covers live timers only, fires continue past 15s, rearm_fail==0.
    // Fake-clock deadline-to-deadline to 70s; cold-tick only.
    let data = temp_dir("ten-plan");
    let root = temp_dir("ten-plan-root");
    let id = PluginId::new("bitty-featured.tenplan").expect("id");
    write_plugin(
        &root,
        "bitty-featured.tenplan",
        r#"
        bitty.store.set("fires", 0)
        bitty.store.set("rearm_fail", 0)
        local cadences = {2000,2000,5000,3000,5000,5000,5000,2000,250,60000}
        local per = 3
        for _, ms in ipairs(cadences) do
          local cb
          cb = function()
            local n = bitty.store.get("fires") or 0
            bitty.store.set("fires", n + 1)
            local ok, _ = pcall(function() return bitty.timers.create(ms, cb) end)
            if not ok then
              local f = bitty.store.get("rearm_fail") or 0
              bitty.store.set("rearm_fail", f + 1)
            end
          end
          local ok, _ = pcall(function() return bitty.timers.create(ms, cb) end)
          if not ok then
            local f = bitty.store.get("rearm_fail") or 0
            bitty.store.set("rearm_fail", f + 1)
          end
          for k = 2, per do
            local delay = ms * k
            local ok2, _ = pcall(function()
              return bitty.timers.create(delay, function()
                local n = bitty.store.get("fires") or 0
                bitty.store.set("fires", n + 1)
              end)
            end)
            if not ok2 then
              local f = bitty.store.get("rearm_fail") or 0
              bitty.store.set("rearm_fail", f + 1)
            end
          end
        end
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert!(rt.timer_deadline().is_some(), "10-plan must arm");

    let t0 = Instant::now();
    let end = t0 + Duration::from_secs(70);
    let mut fires_mid: usize = 0;
    let mut total_fires: usize = 0;
    // Deadline-to-deadline fake clock: always pump at the earliest wake.
    while let Some(deadline) = rt.timer_deadline() {
        if deadline > end {
            break;
        }
        let elapsed = deadline.saturating_duration_since(t0);
        let fired = rt.pump_due_timers_at(deadline);
        total_fires += fired;
        if elapsed >= Duration::from_secs(15) && elapsed <= Duration::from_secs(59) {
            fires_mid += fired;
        }
        // Guard against an infinite pump loop hiding a regression.
        if total_fires > 50_000 {
            panic!("runaway timer loop");
        }
    }

    let rearm_fail = match store_get(&rt, &id, "rearm_fail") {
        Some(LuaValue::Integer(n)) => n,
        other => panic!("rearm_fail must be an integer, got {other:?}"),
    };
    assert_eq!(
        rearm_fail, 0,
        "repeating re-arms must never hit E_DEF_LIMIT (live cap only)"
    );
    assert!(
        fires_mid > 0,
        "10-plan lineages must keep firing 15-59s (got fires_mid==0, total=={total_fires})"
    );
    // Sanity: the 250ms spectrum chain alone fires ~180x in 15-59s; require
    // a comfortable margin past flake while still catching a total stall.
    assert!(
        total_fires > 100,
        "70s run must deliver substantial fires (got {total_fires})"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn at_cap_repeating_chain_reuses_freed_slot() {
    // CTX-1247 CodeRabbit follow-up: at the 64 live-timer cap a firing
    // callback must reuse the freed slot. The pump prunes the fired row
    // before invoking the callback (clone-then-consume-then-invoke), so a
    // `timers.create` inside the callback sees 63 live rows, not 64.
    // Prune-after would fail this with E_DEF_LIMIT even though the one-shot
    // deadline is gone.
    let data = temp_dir("at-cap");
    let root = temp_dir("at-cap-root");
    let id = PluginId::new("bitty-featured.atcap").expect("id");
    write_plugin(
        &root,
        "bitty-featured.atcap",
        r#"
        bitty.store.set("fires", 0)
        bitty.store.set("rearm_fail", 0)
        local cb
        cb = function()
          local n = bitty.store.get("fires") or 0
          bitty.store.set("fires", n + 1)
          local ok, _ = pcall(function() return bitty.timers.create(100, cb) end)
          if not ok then
            local f = bitty.store.get("rearm_fail") or 0
            bitty.store.set("rearm_fail", f + 1)
          end
        end
        -- Fill to the 64 live-timer cap: 1 repeating chain + 63 one-shots.
        local ok, _ = pcall(function() return bitty.timers.create(100, cb) end)
        if not ok then
          bitty.store.set("rearm_fail", 999)
        end
        for _ = 2, 64 do
          bitty.timers.create(100000, function()
            local n = bitty.store.get("fires") or 0
            bitty.store.set("fires", n + 1)
          end)
        end
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert!(rt.timer_deadline().is_some(), "at-cap plan must arm");

    let deadline = rt.timer_deadline().expect("first wake at 100ms");
    let fired = rt.pump_due_timers_at(deadline);
    assert_eq!(fired, 1, "exactly the repeating chain fires first");
    let rearm_fail = match store_get(&rt, &id, "rearm_fail") {
        Some(LuaValue::Integer(n)) => n,
        other => panic!("rearm_fail must be an integer, got {other:?}"),
    };
    assert_eq!(
        rearm_fail, 0,
        "at-cap re-arm must reuse the freed slot (prune-before-invoke)"
    );
    // The chain re-armed: another wake is armed for the next cadence.
    assert!(
        rt.timer_deadline().is_some(),
        "re-armed chain must keep a wake"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn command_and_event_timer_creates_and_cancels_sync_deadline_immediately_3069() {
    let data = temp_dir("cmd-evt-sync-3069");
    let root = temp_dir("cmd-evt-sync-3069-root");
    let id = PluginId::new("bitty-featured.cmdevtsync").expect("id");
    write_full_plugin(
        &root,
        "bitty-featured.cmdevtsync",
        &[],
        &["arm", "cancel"],
        &["terminal.opened"],
        "",
        r#"
        bitty.commands.register({
          id = "arm",
          title = "arm",
          run = function()
            local h = bitty.timers.create(100, function()
              bitty.store.set("cmd_fired", true)
            end)
            bitty.store.set("cmd_h", h)
            return h
          end,
        })
        bitty.commands.register({
          id = "cancel",
          title = "cancel",
          run = function()
            local h = bitty.store.get("cmd_h")
            return bitty.timers.cancel(h)
          end,
        })
        bitty.events.subscribe("terminal.opened", function()
          bitty.timers.create(50, function()
            bitty.store.set("evt_fired", true)
          end)
        end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()]);
    common::install_stub_backend(&mut rt);
    rt.discover();
    rt.activate(&id).expect("activate");
    assert_eq!(rt.timer_deadline(), None, "initially unarmed");

    // Creating a timer inside a command immediately arms `timer_deadline()`
    // without requiring a prior `pump_due_timers()` call.
    common::settle_dispatch(&mut rt, &id, "arm", &[]).expect_value("arm");
    assert!(
        rt.timer_deadline().is_some(),
        "timer created in command must arm timer_deadline() immediately"
    );

    // Cancelling that timer inside a command immediately clears `timer_deadline()`.
    common::settle_dispatch(&mut rt, &id, "cancel", &[]).expect_value("cancel");
    assert_eq!(
        rt.timer_deadline(),
        None,
        "timer cancelled in command must clear timer_deadline() immediately"
    );

    // Creating a timer inside an event callback also arms `timer_deadline()` immediately.
    assert_eq!(
        rt.deliver_event(
            "terminal.opened",
            &LuaValue::table([("terminal_id", LuaValue::Integer(1))])
        ),
        1
    );
    let evt_deadline = rt
        .timer_deadline()
        .expect("timer created in event callback must arm timer_deadline() immediately");
    assert_eq!(rt.pump_due_timers_at(evt_deadline), 1);
    assert_eq!(store_get(&rt, &id, "evt_fired"), Some(LuaValue::Bool(true)));
    assert_eq!(rt.timer_deadline(), None);

    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}
