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
    LifecycleState, MAX_WALL_RECOVERY_ATTEMPTS, PluginRuntime, PluginRuntimeConfig, SettingsSource,
    SnapshotSource,
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
