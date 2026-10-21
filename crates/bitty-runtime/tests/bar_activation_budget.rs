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
//! Harness budget: this file widens only the wall dimension, for debug test
//! builds of the pinned 4 000-line bar (`HARNESS_WALL_BUDGET_MS`, the
//! bitty#1744/#1727 treatment). Production enforcement stays at
//! `RC1_WALL_CLOCK_BUDGET_MS` and is pinned by `bitty-lua`'s
//! `measurement_lua` / `load_gate` tests. What proves the #2425 fix here is
//! not that ceiling but the injected-reader companion below: a read that
//! takes 1.5 s (30x the production slice, 3x this headroom) must cost the
//! activation slice nothing at all, so any inline read fails this suite even
//! on a host where three warm `pactl` spawns would fit 50 ms.
//!
//! The production-budget guard for the same regression is
//! `plugin_device_audio_native::bar_activation_never_blocks_on_a_slow_audio_monitor`
//! (tiny fixture plugin, default RC-1, 1.5 s injected reader): forcing the
//! inline path there suspends at 1 500 ms against the 50 ms budget.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS, WorkspaceInfo};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::audio::{AudioHost, SampledSpawnFn};
use bitty_runtime::plugin_runtime::services::WorkspaceSource;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

const BAR_ID: &str = "bitty-terminal.bar";

/// Harness wall budget for the pinned real bar.
///
/// RC-1 (50 ms) is calibrated for release builds. This test drives the
/// pinned 4 000-line bar `init.lua` in a debug test build: ~4 ms on a fast
/// host but 61-117 ms on a sharded Linux CI runner, measured on the fixed
/// code (the audio reader is off the executor thread in both numbers).
/// Widen only the wall dimension for this harness, the bitty#1744/#1727
/// treatment: production keeps `RC1_WALL_CLOCK_BUDGET_MS`, budget
/// enforcement stays pinned by `bitty-lua/tests/measurement_lua.rs` and
/// `load_gate.rs`, and the reader-latency regression below injects a read
/// (3 x 500 ms) that exceeds this headroom, so an inline read still fails.
const HARNESS_WALL_BUDGET_MS: u64 = 10 * RC1_WALL_CLOCK_BUDGET_MS;

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
    // Tests in one binary run as parallel threads of the same process, so a
    // pid-only name would collide and the first test's cleanup would delete
    // the second test's data dir mid-activation. A process-wide counter keeps
    // each runtime's data dir unique.
    static DATA_DIR_SEQ: AtomicU64 = AtomicU64::new(0);
    let data_dir = std::env::temp_dir().join(format!(
        "bitty-bar-pin-{}-{}",
        std::process::id(),
        DATA_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
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
    // Debug-build harness headroom; see `HARNESS_WALL_BUDGET_MS`.
    rt.set_vm_wall_budget_ms(Some(HARNESS_WALL_BUDGET_MS));
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
fn three_group_bar_plan_activates_and_mounts() {
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
    let report = rt
        .activate(&bar)
        .expect("three-group bar activates and mounts");
    let elapsed = started.elapsed();
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(report.commands, 12, "pinned bar command set");
    assert_eq!(report.events, 6, "pinned bar event set");
    assert!(
        elapsed.as_millis() < u128::from(HARNESS_WALL_BUDGET_MS),
        "activation took {elapsed:?} and suspended or regressed past the harness bound"
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
    // takes 500 ms per spawn (1.5 s per composed read), three times the
    // harness headroom and thirty times the production slice. Activation
    // must not observe that time at all, so this fails on any
    // implementation that reads providers inline even on a fast host where
    // the real `pactl` happens to fit 50 ms.
    let spawn: SampledSpawnFn = std::sync::Arc::new(|argv: &[String]| {
        std::thread::sleep(std::time::Duration::from_millis(500));
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
    let started = Instant::now();
    let report = rt
        .activate(&bar)
        .expect("provider latency never enters the activation slice");
    assert!(
        started.elapsed().as_millis() < u128::from(HARNESS_WALL_BUDGET_MS),
        "activation waited on the injected 1.5 s reader: {:?}",
        started.elapsed()
    );
    assert_eq!(report.state, LifecycleState::Active);
    assert_eq!(rt.ui_blocks().len(), 1, "band still mounts while warming");
    let _ = std::fs::remove_dir_all(&data_dir);
}
