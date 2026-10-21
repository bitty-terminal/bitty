//! Three-group bar activation inside the RC-1 slice (bitty#2425).
//!
//! The expanded runtime matrix (`smoke-bar-heavy`, `smoke-final`) shows a
//! valid three-group bar plan failing activation with
//! `WallClockExceeded { elapsed_ms: 159, budget_ms: 50 }` (reproduced at
//! 172 ms). The heavy work is the pulseaudio module's first
//! `audio.state.get()`: three blocking `pactl` spawns, measured at 92 ms
//! cold / 15-35 ms warm on the reference host, charged to the plugin's
//! activation chunk because the live reader ran on the executor thread.
//!
//! Fixture: byte-identical pin of `bitty-terminal/bar@d74dd4ca`, the
//! revision the matrix ran (the bar repository's `main` at the time):
//! `bitty-plugin.toml` sha256 `c86d8493…`, `lua/bar/init.lua` sha256
//! `3e8bd934…`. The fixture is deliberately not refreshed in place: the
//! plan, the module registry, and the provider polling shape are the
//! contract this test pins, and a bar change that needs different
//! activation work should bump the pin in its own review.
//!
//! This test keeps the production RC-1 budget (no
//! `set_vm_wall_budget_ms`), so moving provider I/O back into the executor
//! thread fails it at 50 ms instead of silently widening the harness.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS, WorkspaceInfo};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::audio::{AudioHost, SampledSpawnFn};
use bitty_runtime::plugin_runtime::services::WorkspaceSource;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

const BAR_ID: &str = "bitty-terminal.bar";

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
        Ok(LuaValue::table([
            ("version", LuaValue::Integer(1)),
            ("zones", LuaValue::array(vec![])),
        ]))
    }
}

struct ThreeWorkspaces;

impl WorkspaceSource for ThreeWorkspaces {
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        Ok((1..=3)
            .map(|id| WorkspaceInfo {
                id,
                name: format!("ws{id}"),
                active: id == 2,
                panel_count: 1,
                scratchpad_count: 0,
                scratchpad_occupied: false,
                attention: Default::default(),
            })
            .collect())
    }
}

fn str_array(values: &[&str]) -> LuaValue {
    LuaValue::array(
        values
            .iter()
            .map(|value| LuaValue::String((*value).to_string()))
            .collect(),
    )
}

/// Pinned bar plugin root (`tests/bar-pin`).
///
/// Deliberately outside `tests/fixtures/`: that directory is the bundled
/// discovery root of `plugin_runtime.rs`, and a second package there would
/// change its package counts.
fn pinned_bar_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("bar-pin")
}

fn three_group_runtime() -> (PluginRuntime, PathBuf) {
    three_group_runtime_with(None)
}

/// Build the same plan with an optional injected state sampler.
fn three_group_runtime_with(sampled: Option<SampledSpawnFn>) -> (PluginRuntime, PathBuf) {
    let root = pinned_bar_root();
    let data_dir = std::env::temp_dir().join(format!(
        "bitty-bar-pin-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let mut settings = BTreeMap::new();
    settings.insert("show".to_string(), LuaValue::Bool(true));
    settings.insert("show_single".to_string(), LuaValue::Bool(false));
    settings.insert("edge".to_string(), LuaValue::String("bottom".to_string()));
    // The matrix `bar-heavy` plan: workspaces left, clock center,
    // memory + pulseaudio right.
    settings.insert("modules_left".to_string(), str_array(&["workspaces"]));
    settings.insert("modules_center".to_string(), str_array(&["clock"]));
    settings.insert(
        "modules_right".to_string(),
        str_array(&["memory", "pulseaudio"]),
    );

    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir.clone()),
        store_root: None,
        bundled_roots: Vec::new(),
        // The root is itself one package (`discover_root` accepts a package
        // directory), so nothing else under `tests/` joins discovery.
        third_party_roots: vec![root],
        settings: Rc::new(MapSettings(settings)),
        snapshot: Rc::new(StaticSnapshot),
    });
    // Production budget: `RC1_WALL_CLOCK_BUDGET_MS` (50 ms). This test
    // never calls `set_vm_wall_budget_ms`; the fixture harness in
    // `plugin_device_audio_native` owns the widened-load variant.
    rt.set_workspace_source(Rc::new(ThreeWorkspaces));
    if let Some(spawn) = sampled {
        // Same off-thread shape as the live monitor, with a reader slow
        // enough that an inline read could never fit the slice.
        *rt.audio_host().borrow_mut() = AudioHost::sampled_for_test(spawn, 0);
    }
    common::install_stub_backend(&mut rt);
    (rt, data_dir)
}

#[test]
fn three_group_bar_plan_activates_and_mounts_inside_the_rc1_slice() {
    let (mut rt, data_dir) = three_group_runtime();
    let discovery = rt.discover();
    let bar = PluginId::new(BAR_ID).expect("id");
    assert!(
        discovery
            .iter()
            .any(|(id, result)| id == &bar && result.is_ok()),
        "pinned bar package discovers: {discovery:?}"
    );

    let started = Instant::now();
    let report = rt.activate(&bar).expect(
        "three-group bar activates inside the production RC-1 slice \
         (provider I/O must not run on the executor thread)",
    );
    let elapsed = started.elapsed();
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.commands, 12, "pinned bar command set");
    assert_eq!(report.events, 6, "pinned bar event set");
    assert!(
        elapsed.as_millis() < u128::from(RC1_WALL_CLOCK_BUDGET_MS) * 4,
        "activation took {elapsed:?}; a regression that re-inlines provider I/O \
         lands in the hundreds of milliseconds"
    );

    // The band mounted: one `ui.rich` block on the bottom edge with the
    // rendered groups as children.
    let blocks = rt.ui_blocks();
    assert_eq!(blocks.len(), 1, "exactly one mounted band");
    let (id, _slot, node, _version) = &blocks[0];
    assert_eq!(id, &bar);
    let children = match node {
        bitty_lua::UiNode::Row { children, .. } => children.len(),
        other => panic!("bar mounts a Row, got {other:?}"),
    };
    assert!(
        children >= 2,
        "workspaces + clock (+ memory) render atoms, got {children}"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn three_group_bar_plan_never_waits_for_the_audio_monitor() {
    // Deterministic companion to the live test above: the injected reader
    // takes 300 ms per spawn (900 ms per composed read), twenty times the
    // production slice. Activation must not observe that time at all, so
    // this fails on any implementation that reads providers inline even on
    // a fast host where the real `pactl` happens to fit 50 ms.
    let spawn: SampledSpawnFn = std::sync::Arc::new(|argv: &[String]| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let joined = argv.join(" ");
        if joined.contains("get-sink-volume") {
            Ok("Volume: front-left: 32768 / 50% / -18.06 dB\n".to_string())
        } else if joined.contains("get-sink-mute") {
            Ok("Mute: no\n".to_string())
        } else if joined.contains("list") {
            Ok("Sink #0\n\tState: RUNNING\n\tActive Port: analog-output-headphones\n".to_string())
        } else {
            Err(bitty_plugin_host::AudioError::unavailable(
                "unexpected argv in fixture",
            ))
        }
    });
    let (mut rt, data_dir) = three_group_runtime_with(Some(spawn));
    let bar = PluginId::new(BAR_ID).expect("id");
    rt.discover();
    let report = rt
        .activate(&bar)
        .expect("provider latency never enters the activation slice");
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(rt.ui_blocks().len(), 1, "band still mounts while warming");
    let _ = std::fs::remove_dir_all(&data_dir);
}
