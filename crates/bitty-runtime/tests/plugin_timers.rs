//! CTX-1244 (issue #2107): host one-shot timer pump.
//!
//! Armed plugin timers fire on cadence while idle (fake-clock advance
//! delivers ticks and runs callbacks); unarmed idle stays deadline-free
//! (zero-submit preserved). Cold-tick only, never the hot path.

mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
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
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("terminal_id", LuaValue::Integer(1)),
        ]))),
    });
    // Harness headroom for activation only; see `HARNESS_WALL_BUDGET_MS`.
    rt.set_vm_wall_budget_ms(Some(HARNESS_WALL_BUDGET_MS));
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
    rt.dispatch_command(&id, "stop", &[]).expect("cancel");
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
