//! Resumable RC-1 completion enforcement for app-dispatched commands
//! (bitty #2314).
//!
//! A command that reaches the 50 ms charged wall budget is parked with its
//! executor intact and resumed on later cold ticks; the body runs exactly
//! once, and lifecycle transitions cancel the park cleanly. The cumulative
//! 5250 ms ceiling is pinned at the VM layer in
//! `bitty-lua/tests/host_budget_accounting.rs`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

const PLUGIN_ID: &str = "bitty.rc1-resume";

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

impl SettingsSource for MapSettings {
    fn get(&self, key: &str) -> Option<LuaValue> {
        self.0.get(key).cloned()
    }
}

struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        // Charge 10 ms of real host wall time per call. Eight calls take the
        // command past the 50 ms charged budget without approaching the
        // instruction budget, so the park is deterministic under any runner.
        std::thread::sleep(std::time::Duration::from_millis(10));
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bitty-rc1-resume-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Registers one slow command (80 ms of charged host time, far below the
/// 5250 ms callback ceiling), a long command spanning many cold ticks, a fast
/// counter reader, and a `runs` command that reports how many bodies completed.
const INIT_SRC: &str = r#"
local runs = 0
bitty.commands.register({
  id = "slow",
  title = "Slow command",
  run = function(_args)
    for _ = 1, 8 do
      pcall(bitty.terminal.snapshot, { scope = "semantic" })
    end
    runs = runs + 1
    return "done:" .. runs
  end,
})
bitty.commands.register({
  id = "long",
  title = "Long command",
  run = function(_args)
    for _ = 1, 20 do
      pcall(bitty.terminal.snapshot, { scope = "semantic" })
      local sum = 0
      for j = 1, 500 do sum = sum + j end
    end
    runs = runs + 1
    return "long:" .. runs
  end,
})
bitty.commands.register({
  id = "runs",
  title = "Read run count",
  run = function(_args) return tostring(runs) end,
})
return {}
"#;

fn write_plugin(root: &Path) {
    let plugin = root.join(PLUGIN_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PLUGIN_ID}"
name = "RC-1 resume test"
version = "0.1.0"
description = "bitty#2314 resumable command dispatch"

[compat]
plugin-api = "^1.0"

[capabilities]
terminal.semantic-read = true

[lazy]
commands = ["{PLUGIN_ID}:slow", "{PLUGIN_ID}:long", "{PLUGIN_ID}:runs"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), INIT_SRC).expect("init");
}

struct World {
    root: PathBuf,
    data: PathBuf,
    runtime: PluginRuntime,
    id: PluginId,
}

impl World {
    fn activate(tag: &str) -> Self {
        let root = temp_dir(&format!("{tag}-root"));
        let data = temp_dir(&format!("{tag}-data"));
        write_plugin(&root);
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: Rc::new(MapSettings::default()),
            snapshot: Rc::new(StaticSnapshot),
        });
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

    fn park_slow(&mut self) {
        self.park("slow");
    }

    fn park_long(&mut self) {
        self.park("long");
    }

    fn park(&mut self, command: &str) {
        let error = self
            .runtime
            .dispatch_command(&self.id, command, &[])
            .expect_err("charged work past the wall budget must park");
        assert!(error.is_parked(), "expected a park, got {error:?}");
        assert_eq!(self.runtime.parked_command_count(), 1);
    }

    fn run_count(&mut self) -> String {
        // The real 50 ms production budget stays in force here (the park
        // assertions below depend on it). This cheap read is still settled on
        // the documented cold tick so a loaded runner cannot turn it into a
        // spurious park; bitty#2800.
        common::settle_dispatch(&mut self.runtime, &self.id, "runs", &[]).expect_string("runs")
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn slow_command_completes_exactly_once_across_cold_ticks() {
    let mut world = World::activate("slow");
    world.park_slow();

    let mut completed = Vec::new();
    for _ in 0..200 {
        let mut completions = world.runtime.pump_parked_commands();
        if !completions.is_empty() {
            completed.append(&mut completions);
            break;
        }
    }
    assert_eq!(completed.len(), 1, "the park must resume and complete");
    assert_eq!(completed[0].plugin, world.id);
    assert_eq!(completed[0].command, "slow");
    match &completed[0].outcome {
        Ok(LuaValue::String(value)) => assert_eq!(value, "done:1"),
        other => panic!("expected the slow command result, got {other:?}"),
    }
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert!(
        world.runtime.pump_parked_commands().is_empty(),
        "a completed command must never replay"
    );
    assert_eq!(
        world.run_count(),
        "1",
        "the callback body must run exactly once"
    );
}

#[test]
fn suspend_cancels_a_parked_command_cleanly() {
    let mut world = World::activate("suspend");
    world.park_long();
    world.runtime.suspend(&world.id).expect("suspend");
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert!(world.runtime.pump_parked_commands().is_empty());
    world.runtime.resume(&world.id).expect("resume");
    assert_eq!(world.run_count(), "0", "a cancelled body must not run");
}

#[test]
fn dispose_cancels_a_parked_command_cleanly() {
    let mut world = World::activate("dispose");
    world.park_long();
    world.runtime.dispose(&world.id).expect("dispose");
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert!(world.runtime.pump_parked_commands().is_empty());
    let report = world.runtime.activate(&world.id).expect("reactivate");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(world.run_count(), "0");
}

#[test]
fn reload_cancels_a_parked_command_cleanly() {
    let mut world = World::activate("reload");
    world.park_long();
    let report = world.runtime.reload(&world.id).expect("reload");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert!(world.runtime.pump_parked_commands().is_empty());
    assert_eq!(world.run_count(), "0");
}

#[test]
fn second_dispatch_while_parked_is_refused() {
    let mut world = World::activate("one-park");
    world.park_long();
    let error = world
        .runtime
        .dispatch_command(&world.id, "runs", &[])
        .expect_err("only one invocation may park per plugin");
    assert!(
        error.to_string().contains("parked invocation"),
        "expected the one-park refusal, got {error}"
    );
    assert_eq!(world.runtime.parked_command_count(), 1);
}

#[test]
fn command_resumes_across_multiple_cold_ticks() {
    let mut world = World::activate("multi-tick");
    world.park_long();
    let mut ticks = 0usize;
    let completion = loop {
        let mut completions = world.runtime.pump_parked_commands();
        if let Some(completion) = completions.pop() {
            break completion;
        }
        ticks = ticks.saturating_add(1);
        assert!(
            ticks < 200,
            "a 1 s command must finish well before the 5250 ms ceiling"
        );
    };
    assert!(ticks >= 2, "the long command must need multiple windows");
    match completion.outcome {
        Ok(LuaValue::String(value)) => assert_eq!(value, "long:1"),
        other => panic!("expected the long command result, got {other:?}"),
    }
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert_eq!(world.run_count(), "1", "one body across many resume ticks");
}
