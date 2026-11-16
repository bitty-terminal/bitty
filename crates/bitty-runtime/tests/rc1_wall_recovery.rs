//! Recovery of a plugin suspended on a terminal RC-1 wall timeout reached
//! outside the timer pump (CTX-1827, #3157 review D4).
//!
//! [`PluginRuntime::recover_wall_failed_plugins`] only selects a generation
//! that is `Failed` with `FailureClass::WallSliceTimeout`. Before this fix only
//! the timer pump recorded that verdict (`fail_runtime_on_suspend`), so a wall
//! timeout reached through command dispatch, the parked-command pump, or event
//! delivery left the plugin `Active` with a dead VM and no failure class: the
//! recovery pass never selected it, and a plugin without a timer stayed
//! suspended until an explicit reload.
//!
//! Every test asserts the *observable* consequence rather than the internal
//! field: the generation ends `Failed` with an RC-1 wall reason, its parked
//! record is gone, and a later recovery tick reloads it. Under the pre-fix code
//! the generation stayed `Active`, so those assertions fail.
//!
//! The cumulative callback envelope (`RC1_CALLBACK_CEILING_MS` = 5250 ms with
//! the production 50 ms slice) is real elapsed time by definition, so the two
//! ceiling tests spend it once each and never widen a budget. A slice-only
//! callback is terminal at the per-tick slice instead (it cannot park), which is
//! why the event fixture's wall timeout costs a fraction of a second.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    HEALTHY_TICKS_RESET_RECOVERY, LifecycleState, MAX_WALL_RECOVERY_ATTEMPTS, PluginRuntime,
    PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

const PLUGIN_ID: &str = "bitty.ctx1827-wall-recovery";

/// Charged wall time one host call costs in the parking fixture.
///
/// A fifth of the production RC-1 slice keeps every call inside the default
/// host-call deadline, so the body crosses the slice (and, for the soak body,
/// the cumulative ceiling) over several calls instead of being refused.
const TICK_MS: u64 = 10;

/// Calls the soak body makes: 700 x 10 ms is well past the 5250 ms cumulative
/// ceiling, so the pump reaches the terminal verdict with calls to spare instead
/// of finishing the body.
const SOAK_CALLS: u64 = 700;

/// One host call that alone crosses the cumulative ceiling.
///
/// The resumable ceiling is checked before the park rule, so a single oversized
/// call ends a command dispatch with a terminal wall verdict instead of a park;
/// a slice-only callback (an event handler) is refused at completion by the same
/// cumulative check. Either way the verdict is terminal and needs no second
/// call, which is the cheapest terminal wall timeout RC-1 has: the envelope is
/// `wall budget + 5.4 s of fixed credits`, so anything below it is not terminal.
const CEILING_CALL_MS: u64 = 6000;

/// Host-call deadline for the fixtures that charge more than one RC-1 slice
/// inside a single host call (production holds each call to the slice).
const HOST_CALL_DEADLINE_MS: u64 = 10_000;

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

/// Host snapshot that charges real wall time, the only clock RC-1 bounds.
struct SlowSnapshot(Duration);

impl SnapshotSource for SlowSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(self.0);
        Ok(LuaValue::table([("version", LuaValue::Integer(1))]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-ctx1827-wall-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `init.lua` with the soak loop bound substituted in (Lua cannot read Rust
/// constants): a one-call `huge` command, a soak command, a cheap `ping`, and an
/// event handler that charges one host call.
fn init_src() -> String {
    format!(
        r#"
local runs = 0
bitty.commands.register({{
  id = "huge",
  title = "One over-ceiling call",
  run = function(_args)
    pcall(bitty.terminal.snapshot, {{ scope = "semantic" }})
    runs = runs + 1
    return "huge:" .. runs
  end,
}})
bitty.commands.register({{
  id = "soak",
  title = "Soak across cold ticks",
  run = function(_args)
    for _ = 1, {SOAK_CALLS} do
      pcall(bitty.terminal.snapshot, {{ scope = "semantic" }})
    end
    runs = runs + 1
    return "soak:" .. runs
  end,
}})
bitty.commands.register({{
  id = "ping",
  title = "Cheap completion",
  run = function(_args) return "pong:" .. runs end,
}})
bitty.events.subscribe("terminal.opened", function(_event)
  pcall(bitty.terminal.snapshot, {{ scope = "semantic" }})
  runs = runs + 1
end)
return {{}}
"#
    )
}

fn write_plugin_dir(root: &Path, source: &str) {
    let plugin = root.join(PLUGIN_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PLUGIN_ID}"
name = "CTX-1827 wall recovery"
version = "0.1.0"
description = "terminal RC-1 wall recovery outside the timer pump"

[compat]
plugin-api = "^1.0"

[capabilities]
terminal.semantic-read = true

[lazy]
commands = ["{PLUGIN_ID}:huge", "{PLUGIN_ID}:soak", "{PLUGIN_ID}:ping"]
events = ["terminal.opened"]
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), source).expect("init");
}

struct World {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    id: PluginId,
}

impl World {
    fn activate(tag: &str, snapshot_delay: Duration) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin_dir(&root, &init_src());
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: Rc::new(MapSettings::default()),
            snapshot: Rc::new(SlowSnapshot(snapshot_delay)),
        });
        runtime.set_host_call_deadline_ms(Some(HOST_CALL_DEADLINE_MS));
        common::install_stub_backend(&mut runtime);
        runtime.discover();
        let id = PluginId::new(PLUGIN_ID).expect("id");
        let report = runtime.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        Self {
            root,
            data,
            runtime,
            id,
        }
    }

    /// Replace `init.lua` so a later recovery tick re-resolves a different body
    /// (the reload re-reads the module root, like a store-backed revision).
    fn rewrite_init(&self, source: &str) {
        std::fs::write(self.root.join(PLUGIN_ID).join("lua/init.lua"), source).expect("init");
    }

    /// Path of the fixture's `init.lua` entry point.
    fn init_path(&self) -> PathBuf {
        self.root.join(PLUGIN_ID).join("lua/init.lua")
    }

    /// The failure reason, panicking unless the generation failed.
    fn failure_reason(&self) -> String {
        match self.runtime.state(&self.id) {
            Some(LifecycleState::Failed(reason)) => reason.clone(),
            other => panic!("expected a failed generation, got {other:?}"),
        }
    }

    fn assert_debug_row_failed(&self) {
        let row = self
            .runtime
            .debug_plugins()
            .into_iter()
            .find(|row| row.id == PLUGIN_ID)
            .expect("debug row");
        assert_eq!(row.state, "failed", "lifecycle consumers see `failed`");
    }

    /// Deliver one event: the fixture's handler charges `SLICE_CALL_MS` in a
    /// slice-only callback, which is terminal at the per-tick wall budget.
    fn wall_fail_through_an_event(&mut self) -> usize {
        let payload = LuaValue::table([("terminal_id", LuaValue::Integer(1))]);
        self.runtime.deliver_event("terminal.opened", &payload)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn terminal_wall_timeout_through_command_dispatch_is_recovered() {
    // The single host call crosses the cumulative ceiling, so the dispatch
    // itself observes the terminal verdict: `WallPolicy::Resumable` checks the
    // ceiling before the park rule.
    let mut world = World::activate("dispatch", Duration::from_millis(CEILING_CALL_MS));
    let error = world
        .runtime
        .dispatch_command(&world.id, "huge", &[])
        .expect_err("a terminal wall verdict is never a result");
    assert!(
        !error.is_parked(),
        "the ceiling refuses the invocation, it does not defer it: {error}"
    );

    // Pre-fix the generation stayed `Active` with a dead VM here.
    assert_ne!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active),
        "a terminal wall timeout must not leave the generation active"
    );
    let reason = world.failure_reason();
    assert!(
        reason.contains("RC-1 wall") && reason.contains("command 'huge' suspended"),
        "the reason names the dispatch that died: {reason}"
    );
    world.assert_debug_row_failed();
    assert_eq!(
        world.runtime.parked_command_count(),
        0,
        "a terminal verdict must leave no parked record"
    );
    assert!(
        world
            .runtime
            .dispatch_command(&world.id, "ping", &[])
            .is_err(),
        "the dead generation serves no commands (no Active-with-dead-VM)"
    );

    // The recovery pass now selects the generation and reloads it.
    assert_eq!(
        world.runtime.recover_wall_failed_plugins(),
        1,
        "a wall-classified failure must be selected by the recovery pass"
    );
    assert_eq!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active)
    );
}

#[test]
fn terminal_wall_timeout_through_the_parked_pump_is_recovered() {
    // 10 ms per call: the first dispatch parks on the 50 ms slice, and every
    // cold tick resumes one more window until the cumulative ceiling is spent.
    let mut world = World::activate("parked", Duration::from_millis(TICK_MS));
    let error = world
        .runtime
        .dispatch_command(&world.id, "soak", &[])
        .expect_err("charged work past the wall budget must park");
    assert!(error.is_parked(), "expected a park, got {error:?}");
    assert_eq!(world.runtime.parked_command_count(), 1);

    let mut completion = None;
    for tick in 0..2000 {
        let mut completions = world.runtime.pump_parked_commands();
        if let Some(done) = completions.pop() {
            assert!(completions.is_empty(), "one completion per cold tick");
            completion = Some(done);
            break;
        }
        assert!(
            tick < 1999,
            "the soaked body must reach the cumulative ceiling"
        );
    }
    let completion = completion.expect("the pump ends the parked invocation");
    assert_eq!(completion.command, "soak");
    assert!(
        completion.outcome.is_err(),
        "a body that never finishes inside the ceiling has no result: {:?}",
        completion.outcome
    );
    assert_eq!(
        world.runtime.parked_command_count(),
        0,
        "the terminal park must be dropped, not leaked"
    );
    assert!(world.runtime.pump_parked_commands().is_empty());

    // Pre-fix the generation stayed `Active` with a dead VM here.
    let reason = world.failure_reason();
    assert!(
        reason.contains("RC-1 wall") && reason.contains("parked command 'soak' suspended"),
        "the reason names the resumed body's terminal verdict: {reason}"
    );
    world.assert_debug_row_failed();

    assert_eq!(
        world.runtime.recover_wall_failed_plugins(),
        1,
        "the parked-path failure must be selected by the recovery pass"
    );
    assert_eq!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active)
    );
}

#[test]
fn terminal_wall_timeout_through_an_event_handler_fails_the_generation() {
    // A slice-only callback cannot park, so the cumulative ceiling refuses it at
    // completion; one oversized host call is therefore terminal.
    let mut world = World::activate("event", Duration::from_millis(CEILING_CALL_MS));
    assert_eq!(
        world.wall_fail_through_an_event(),
        0,
        "a handler that died counts no completed delivery"
    );

    // Pre-fix the handler's suspension left the plugin Active and unclassified.
    let reason = world.failure_reason();
    assert!(
        reason.contains("RC-1 wall") && reason.contains("event handler for 'terminal.opened'"),
        "the reason names the event that died: {reason}"
    );
    world.assert_debug_row_failed();
    assert_eq!(world.runtime.parked_command_count(), 0);

    assert_eq!(
        world.runtime.recover_wall_failed_plugins(),
        1,
        "the event-path failure must be selected by the recovery pass"
    );
    assert_eq!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active)
    );
}

#[test]
fn a_healthy_completion_resets_the_wall_recovery_budget() {
    // `wall_recovery_attempts` is the only thing that stops the recovery pass
    // from reloading a deterministically failing plugin forever, so a completed
    // callback hands the budget back. The reset is observable through the cap:
    // every failed recovery attempt consumes one slot, and the pass stops
    // selecting the generation at `MAX_WALL_RECOVERY_ATTEMPTS`.
    //
    // The counter is primed by one real wall timeout and one successful
    // recovery (attempt 1). If the healthy `ping` completion below did not reset
    // it, the two failed attempts that follow would take it to the cap of 3 and
    // the final clean reload would never be selected; with the reset they take
    // it to 2, so one slot is left and the clean reload recovers the plugin.
    let mut world = World::activate("budget", Duration::from_millis(CEILING_CALL_MS));
    world
        .runtime
        .dispatch_command(&world.id, "huge", &[])
        .expect_err("the over-ceiling call is a terminal wall verdict");
    assert!(
        world.failure_reason().contains("RC-1 wall"),
        "priming the budget needs a real wall-classified failure"
    );
    assert_eq!(world.runtime.recover_wall_failed_plugins(), 1, "attempt 1");

    // A completed callback proves the generation healthy: it hands the budget
    // back (this is the reset under test).
    assert_eq!(
        world
            .runtime
            .dispatch_command(&world.id, "ping", &[])
            .expect("the cheap command completes inside the slice"),
        LuaValue::String("pong:0".to_string()),
        "the healthy completion itself runs"
    );

    // A second wall timeout, then failed recovery attempts. With the reset the
    // counter goes 0 -> 2 here and one slot is left for the clean reload below;
    // without it the counter goes 1 -> 3, reaches the cap, and the clean reload
    // is never selected.
    world
        .runtime
        .dispatch_command(&world.id, "huge", &[])
        .expect_err("the second over-ceiling call is terminal too");
    assert!(
        world.failure_reason().contains("RC-1 wall"),
        "the second failure is wall-classified before the attempts start"
    );

    // A recovery attempt that fails at re-resolution keeps the wall class and
    // consumes a slot; that is what lets the remaining attempts be spent.
    std::fs::remove_file(world.init_path()).expect("the module root goes away");
    for attempt in 1..=MAX_WALL_RECOVERY_ATTEMPTS - 1 {
        assert_eq!(
            world.runtime.recover_wall_failed_plugins(),
            0,
            "failed attempt {attempt} recovers nothing"
        );
        assert!(
            world.failure_reason().contains("RC-1 wall"),
            "failed attempt {attempt} keeps the wall verdict instead of \
             reclassifying the generation"
        );
    }

    // One slot is still available because the healthy completion reset the
    // counter; without the reset this selection would be refused at the cap.
    world.rewrite_init(&init_src());
    assert_eq!(
        world.runtime.recover_wall_failed_plugins(),
        1,
        "the healthy completion reset the attempt budget"
    );
    assert_eq!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active)
    );
}

#[test]
fn healthy_timer_ticks_reset_wall_recovery_budget() {
    // A transient wall spike followed by HEALTHY_TICKS_RESET_RECOVERY healthy
    // timer ticks must reset `wall_recovery_attempts` so a later transient
    // spike hours later can still recover instead of permanently stopping the
    // plugin.
    let root = temp_dir("timer-reset-root");
    let data = temp_dir("timer-reset-data");
    let healthy_timer_src = format!(
        r#"
local function arm()
  bitty.timers.create(10, function()
    arm()
  end)
end
arm()
{}
"#,
        init_src()
    );
    write_plugin_dir(&root, &healthy_timer_src);

    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![root.clone()],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(SlowSnapshot(Duration::from_millis(CEILING_CALL_MS))),
    });
    runtime.set_host_call_deadline_ms(Some(HOST_CALL_DEADLINE_MS));
    common::install_stub_backend(&mut runtime);
    runtime.discover();
    let id = PluginId::new(PLUGIN_ID).expect("id");
    assert_eq!(
        runtime.activate(&id).expect("activate").state,
        LifecycleState::Active
    );

    // Prime attempt 1 with a real wall timeout and recovery.
    runtime
        .dispatch_command(&id, "huge", &[])
        .expect_err("over-ceiling call is terminal");
    assert_eq!(runtime.recover_wall_failed_plugins(), 1, "attempt 1");

    let mut now = std::time::Instant::now();
    for tick in 0..HEALTHY_TICKS_RESET_RECOVERY {
        now += Duration::from_millis(20);
        assert_eq!(
            runtime.pump_due_timers_at(now),
            1,
            "healthy timer tick {tick} must fire"
        );
    }

    // Second wall timeout + (MAX_WALL_RECOVERY_ATTEMPTS - 1) failed recovery
    // attempts: because the healthy timer ticks reset the counter, one slot
    // remains for the clean reload below.
    runtime
        .dispatch_command(&id, "huge", &[])
        .expect_err("second over-ceiling call is terminal");
    let init_path = root.join(PLUGIN_ID).join("lua/init.lua");
    std::fs::remove_file(&init_path).expect("remove init.lua");
    for attempt in 1..=MAX_WALL_RECOVERY_ATTEMPTS - 1 {
        assert_eq!(
            runtime.recover_wall_failed_plugins(),
            0,
            "failed attempt {attempt} recovers nothing"
        );
    }
    std::fs::write(&init_path, &healthy_timer_src).expect("restore init.lua");
    assert_eq!(
        runtime.recover_wall_failed_plugins(),
        1,
        "healthy timer ticks must have reset the attempt budget"
    );
    assert_eq!(runtime.state(&id), Some(&LifecycleState::Active));

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn throttled_recovery_respects_backoff_between_attempts() {
    let mut world = World::activate("backoff", Duration::from_millis(CEILING_CALL_MS));
    world
        .runtime
        .dispatch_command(&world.id, "huge", &[])
        .expect_err("wall timeout");

    let t0 = std::time::Instant::now();
    // Remove init.lua so recovery attempt 1 fails and arms the 2s backoff.
    std::fs::remove_file(world.init_path()).expect("remove init");
    assert_eq!(world.runtime.recover_wall_failed_plugins_throttled(t0), 0);

    // Restore init.lua: a tick 100ms later must be skipped by backoff, preserving attempts.
    world.rewrite_init(&init_src());
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_millis(100)),
        0,
        "throttled recovery must wait for backoff before consuming another attempt"
    );

    // Once the 2s backoff expires, recovery runs and succeeds.
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_secs(3)),
        1
    );
    assert_eq!(
        world.runtime.state(&world.id),
        Some(&LifecycleState::Active)
    );
}

// ── CTX-1855 review round: attempt budget, backoff window, reset sites ────
//
// The throttled recovery pass exists so a transient wall spike cannot spend the
// whole recovery budget in milliseconds. Three host invariants keep that true,
// and each one is asserted here through an observable consequence rather than
// through the private fields:
//
// 1. one intent spends one attempt and arms the window (`config.reloaded` used
//    to spend two, so two reloads killed a wall-failed plugin for good),
// 2. every attempt arms the window, throttled or not, so an unthrottled call
//    can never be followed by a second attempt inside it,
// 3. every *healthy* reset site hands the window back, so a real wall fault
//    that arrives right after a success is not deferred by a stale backoff.
//
// The fixture below costs ~50 ms of wall per fault: a slice-only event handler
// that spins in pure Lua crosses the RC-1 slice, which is a terminal
// `RC-1 wall` verdict with no host-call credits involved.

const WINDOW_ID: &str = "bitty.ctx1855-recovery-window";

/// Cadence for the healthy-tick pump; one pump fires the 10 ms chain once.
const WINDOW_TICK: Duration = Duration::from_millis(20);

/// `init.lua` for the window fixture: a faulting event handler, a cheap event
/// handler, a cheap command, a command that parks once and then completes, and a
/// self-arming 10 ms timer (the healthy-tick reset needs timer ticks).
fn window_init_src() -> String {
    r#"
bitty.commands.register({
  id = "ping",
  title = "Cheap completion",
  run = function() return "pong" end,
})
bitty.commands.register({
  id = "slow",
  title = "Parks once, then completes",
  run = function()
    for _ = 1, 6 do
      pcall(bitty.terminal.snapshot, { scope = "semantic" })
    end
    return "slow-done"
  end,
})
bitty.events.subscribe("terminal.opened", function(_event)
  -- Pure Lua: no host-call credit absorbs it, so the slice wall budget is the
  -- only bound and the handler is terminal at the 50 ms slice.
  local n = 0
  while true do n = n + 1 end
end)
bitty.events.subscribe("workspace.changed", function(_event) return true end)
local function arm()
  bitty.timers.create(10, function() arm() end)
end
arm()
return {}
"#
    .to_string()
}

fn write_window_plugin(root: &Path, source: &str) {
    let plugin = root.join(WINDOW_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{WINDOW_ID}"
name = "CTX-1855 recovery window"
version = "0.1.0"
description = "attempt budget and backoff window fixture"

[compat]
plugin-api = "^1.0"

[capabilities]
terminal.semantic-read = true
workspace.read = true

[lazy]
commands = ["{WINDOW_ID}:ping", "{WINDOW_ID}:slow"]
events = ["terminal.opened", "workspace.changed"]
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), source).expect("init");
}

struct WindowWorld {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    id: PluginId,
}

impl WindowWorld {
    fn activate(tag: &str) -> Self {
        let root = temp_dir(&format!("window-{tag}-root"));
        let data = temp_dir(&format!("window-{tag}-data"));
        write_window_plugin(&root, &window_init_src());
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: Rc::new(MapSettings::default()),
            snapshot: Rc::new(SlowSnapshot(Duration::from_millis(TICK_MS))),
        });
        runtime.set_host_call_deadline_ms(Some(HOST_CALL_DEADLINE_MS));
        common::install_stub_backend(&mut runtime);
        runtime.discover();
        let id = PluginId::new(WINDOW_ID).expect("id");
        assert_eq!(
            runtime.activate(&id).expect("activate").state,
            LifecycleState::Active
        );
        Self {
            root,
            data,
            runtime,
            id,
        }
    }

    fn init_path(&self) -> PathBuf {
        self.root.join(WINDOW_ID).join("lua/init.lua")
    }

    fn break_module(&self) {
        std::fs::remove_file(self.init_path()).expect("the module root goes away");
    }

    fn restore_module(&self) {
        std::fs::write(self.init_path(), window_init_src()).expect("restore init.lua");
    }

    /// Fail the generation on a real `RC-1 wall` verdict (terminal, classified).
    fn fault(&mut self) {
        let payload = LuaValue::table([("terminal_id", LuaValue::Integer(1))]);
        assert_eq!(
            self.runtime.deliver_event("terminal.opened", &payload),
            0,
            "a handler that died counts no completed delivery"
        );
        self.assert_failed("a slice-only handler that spins past the slice");
    }

    fn assert_failed(&self, why: &str) {
        match self.runtime.state(&self.id) {
            Some(LifecycleState::Failed(reason)) => assert!(
                reason.contains("RC-1 wall"),
                "{why} must classify as an RC-1 wall timeout: {reason}"
            ),
            other => panic!("{why} must fail the generation, got {other:?}"),
        }
    }

    fn assert_active(&self) {
        assert_eq!(
            self.runtime.state(&self.id),
            Some(&LifecycleState::Active),
            "the generation must be healthy again"
        );
    }

    /// Spend one *failed* attempt at `at`: the module root is gone, so the
    /// reload cannot resolve and the wall class is preserved for a later try.
    fn spend_failed_attempt(&mut self, at: std::time::Instant) {
        self.break_module();
        assert_eq!(
            self.runtime.recover_wall_failed_plugins_throttled(at),
            0,
            "an unresolvable module root recovers nothing"
        );
        self.assert_failed("a failed recovery attempt keeps the wall verdict");
        assert!(
            self.runtime.wall_recovery_deadline().is_some(),
            "an attempt must arm the backoff window"
        );
    }

    /// Reach the state the reset sites have to handle: `Active` again while the
    /// window from an earlier failed attempt is still armed.
    ///
    /// `activate` is the host's explicit retry and does not touch the recovery
    /// bookkeeping, so the entry keeps both the attempt and the window.
    fn active_with_armed_window(&mut self, at: std::time::Instant) {
        self.fault();
        self.spend_failed_attempt(at);
        self.restore_module();
        self.runtime.activate(&self.id).expect("re-activate");
        self.assert_active();
    }

    /// Fault again and ask the throttled pass at `at` to retry.
    ///
    /// `1` means the window was gone and the attempt ran (the reset site under
    /// test cleared it); `0` means a still-armed window skipped the attempt.
    fn probe_after_reset(&mut self, at: std::time::Instant) -> usize {
        self.fault();
        self.runtime.recover_wall_failed_plugins_throttled(at)
    }
}

impl Drop for WindowWorld {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn a_config_reload_intent_spends_one_attempt_and_arms_the_window() {
    // The host config branch clears the throttle (a reload deserves an
    // immediate retry) and lets the single trailing throttled pass do the work.
    // One intent must therefore cost exactly one attempt, and that attempt must
    // arm the window: two intents used to cost three attempts and permanently
    // stop a wall-failed plugin.
    let mut world = WindowWorld::activate("intent");
    world.fault();
    world.break_module();
    let t0 = std::time::Instant::now();

    for intent in 0..2 {
        world.runtime.clear_wall_recovery_throttle();
        assert_eq!(
            world
                .runtime
                .recover_wall_failed_plugins_throttled(t0 + WINDOW_TICK),
            0,
            "intent {intent} cannot recover while the module root is gone"
        );
        world.assert_failed("a failed intent keeps the wall verdict");
        assert!(
            world.runtime.wall_recovery_deadline().is_some(),
            "intent {intent} must arm the backoff window"
        );
        // The intent spends one attempt, not two: without the clear the
        // throttled pass on the next intent would be skipped instead.
        assert_eq!(
            world
                .runtime
                .recover_wall_failed_plugins_throttled(t0 + WINDOW_TICK),
            0,
            "intent {intent} armed the window, so an immediate second pass is a no-op"
        );
    }

    // Two intents spent two of the three slots, so one is left: the plugin
    // recovers once the module root resolves again. Two attempts per intent
    // would have hit the cap here and left it wall-failed for good.
    world.restore_module();
    world.runtime.clear_wall_recovery_throttle();
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + WINDOW_TICK),
        1,
        "two config reloads must leave one attempt of the budget"
    );
    world.assert_active();
}

#[test]
fn a_reload_intent_does_not_donate_attempts_to_a_failing_plugin() {
    // Clearing the throttle must not clear the attempt budget: a reload loop
    // over a plugin that fails deterministically still stops at the cap.
    let mut world = WindowWorld::activate("donate");
    world.fault();
    world.break_module();
    let t0 = std::time::Instant::now();
    for attempt in 0..MAX_WALL_RECOVERY_ATTEMPTS {
        world.runtime.clear_wall_recovery_throttle();
        assert_eq!(
            world
                .runtime
                .recover_wall_failed_plugins_throttled(t0 + WINDOW_TICK),
            0,
            "failed intent {attempt} recovers nothing"
        );
    }
    world.restore_module();
    for extra in 0..3 {
        world.runtime.clear_wall_recovery_throttle();
        assert_eq!(
            world
                .runtime
                .recover_wall_failed_plugins_throttled(t0 + WINDOW_TICK),
            0,
            "intent {extra} after the cap must not revive the plugin"
        );
        world.assert_failed("the cap keeps the plugin stopped");
    }
}

#[test]
fn an_unthrottled_attempt_arms_the_backoff_window() {
    // Defense in depth: the explicit unthrottled entry point arms the window it
    // just consumed, so a throttled pass on the same tick cannot spend a second
    // attempt. Before this, only the throttled path armed it.
    let mut world = WindowWorld::activate("unthrottled");
    world.fault();
    world.break_module();
    let t0 = std::time::Instant::now();
    assert_eq!(
        world.runtime.recover_wall_failed_plugins(),
        0,
        "the unresolvable module root recovers nothing"
    );
    assert!(
        world.runtime.wall_recovery_deadline().is_some(),
        "an unthrottled attempt must arm the window too"
    );

    // The module root now resolves: a throttled pass that still honored the
    // stale "no window" state would recover here and spend attempt 2.
    world.restore_module();
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_millis(100)),
        0,
        "the armed window must skip the throttled pass on the same tick"
    );
    world.assert_failed("nothing recovered inside the window");
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_secs(3)),
        1,
        "the pass retries once the backoff expires"
    );
    world.assert_active();
}

#[test]
fn a_successful_recovery_clears_the_backoff_window() {
    // A clean reload proves the generation healthy, so it must hand the whole
    // budget back — window included. Leaving the window armed defers a real
    // wall fault that arrives right after the success by up to the backoff.
    let mut world = WindowWorld::activate("success");
    world.fault();
    let t0 = std::time::Instant::now();
    world.spend_failed_attempt(t0);
    world.restore_module();
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_secs(3)),
        1,
        "the backoff expires and the recovery succeeds"
    );
    world.assert_active();
    assert_eq!(
        world.runtime.wall_recovery_deadline(),
        None,
        "a successful recovery leaves no pending window"
    );

    // A new fault right after the success: the throttled pass must retry
    // immediately. With the stale window from the successful attempt (which the
    // arming above sets) this retry would be skipped.
    world.fault();
    assert_eq!(
        world
            .runtime
            .recover_wall_failed_plugins_throttled(t0 + Duration::from_secs(3) + WINDOW_TICK),
        1,
        "a fault right after a success must not wait out a stale window"
    );
    world.assert_active();
}

#[test]
fn healthy_timer_ticks_clear_the_wall_recovery_window() {
    // The healthy-tick reset (`HEALTHY_TICKS_RESET_RECOVERY` completed ticks)
    // hands back the attempt budget; it must hand back the window with it.
    let mut world = WindowWorld::activate("tick-reset");
    let t0 = std::time::Instant::now();
    world.active_with_armed_window(t0);
    let mut now = std::time::Instant::now();
    for tick in 0..HEALTHY_TICKS_RESET_RECOVERY {
        now += WINDOW_TICK;
        assert_eq!(
            world.runtime.pump_due_timers_at(now),
            1,
            "healthy timer tick {tick} must fire"
        );
    }
    assert_eq!(
        world.probe_after_reset(t0 + Duration::from_millis(100)),
        1,
        "the healthy-tick reset must clear the window so a new fault retries at once"
    );
    world.assert_active();
}

#[test]
fn reload_clears_the_wall_recovery_window() {
    // An explicit reload is the host's own retry; it clears the attempt budget,
    // and the window has to go with it.
    let mut world = WindowWorld::activate("reload-reset");
    let t0 = std::time::Instant::now();
    world.active_with_armed_window(t0);
    world.runtime.reload(&world.id).expect("reload");
    assert_eq!(
        world.probe_after_reset(t0 + Duration::from_millis(100)),
        1,
        "reload must clear the window so a new fault retries at once"
    );
    world.assert_active();
}

#[test]
fn a_completed_command_clears_the_wall_recovery_window() {
    // A command that returns inside the slice proves the generation healthy.
    let mut world = WindowWorld::activate("command-reset");
    let t0 = std::time::Instant::now();
    world.active_with_armed_window(t0);
    assert_eq!(
        world
            .runtime
            .dispatch_command(&world.id, "ping", &[])
            .expect("the cheap command completes"),
        LuaValue::String("pong".to_string())
    );
    assert_eq!(
        world.probe_after_reset(t0 + Duration::from_millis(100)),
        1,
        "a completed command must clear the window so a new fault retries at once"
    );
    world.assert_active();
}

#[test]
fn a_completed_parked_command_clears_the_wall_recovery_window() {
    // The cold-tick resume of a parked command is the other completion site: a
    // body that finishes inside the cumulative envelope proves the generation
    // healthy just as a direct completion does.
    let mut world = WindowWorld::activate("parked-reset");
    let t0 = std::time::Instant::now();
    world.active_with_armed_window(t0);
    let parked = world
        .runtime
        .dispatch_command(&world.id, "slow", &[])
        .expect_err("60 ms of charged work parks at the 50 ms slice");
    assert!(parked.is_parked(), "expected a park, got {parked:?}");
    let completion = world
        .runtime
        .pump_parked_commands()
        .pop()
        .expect("the parked body resumes on the cold tick");
    assert_eq!(completion.command, "slow");
    match completion.outcome {
        Ok(value) => assert_eq!(
            value,
            LuaValue::String("slow-done".to_string()),
            "the resumed body completes inside the envelope"
        ),
        Err(error) => panic!("the resumed body must complete, got {error}"),
    }
    assert_eq!(
        world.probe_after_reset(t0 + Duration::from_millis(100)),
        1,
        "a completed parked command must clear the window so a new fault retries at once"
    );
    world.assert_active();
}

#[test]
fn a_delivered_event_clears_the_wall_recovery_window() {
    // A handler that returns cleanly is the third completion site.
    let mut world = WindowWorld::activate("event-reset");
    let t0 = std::time::Instant::now();
    world.active_with_armed_window(t0);
    let payload = LuaValue::table([("terminal_id", LuaValue::Integer(1))]);
    assert_eq!(
        world.runtime.deliver_event("workspace.changed", &payload),
        1,
        "the cheap handler completes"
    );
    assert_eq!(
        world.probe_after_reset(t0 + Duration::from_millis(100)),
        1,
        "a delivered event must clear the window so a new fault retries at once"
    );
    world.assert_active();
}
