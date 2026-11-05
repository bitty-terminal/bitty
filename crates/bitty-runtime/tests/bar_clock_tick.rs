//! CTX-1708 (bitty#2899): the pinned bar clock capsule keeps ticking.
//!
//! The bar pre-arms a bounded chain of one-shot clock ticks at activation
//! (`M.TIMER_CHAIN_CAP = 32`, `M.CLOCK_TICK_MS = 60000`) because Plugin API v1
//! closed timer creation after activation, and each first-tick callback
//! optimistically re-arms one more tick. This file drives the pinned bar
//! (`tests/bar-pin`, byte-identical pin of `bitty-terminal/bar@d74dd4ca`)
//! through a fake runtime clock and proves the chain plus the callback re-arm
//! survives well past the 32-tick chain and past 400 clock ticks. It also
//! drives the identical chain shape at a 1 s cadence through a fixture plugin,
//! because the bar's per-module `tick_ms` option cannot be injected through the
//! v1 settings surface: the manifest declares `clock` as `string-array`, so a
//! table value reads as nil (the bar documents the same limitation).
//!
//! The capsule *text* is `os.date("%H:%M", os.time())`, so it reads the real
//! wall clock: a fake runtime-clock advance cannot change the rendered minute.
//! The advance is therefore proven by the render version (one `ui.update` per
//! fired callback, every tick) while the text is checked against the live local
//! minute at every step, which catches an epoch/zero data source (`00:00`)
//! without depending on the fake clock.
//!
//! Rejections are counted by the companion fixture, which mirrors the bar's
//! `arm_once` shape exactly (`pcall(bitty.timers.create)` with a store
//! counter): the pinned bar swallows a rejected re-arm in its own `pcall` and
//! exposes no counter, and this task must not change the bar. The bar's
//! continuation past the chain cap is the end-to-end proof that its own
//! re-arms succeeded.
//!
//! Harness budget: only the wall dimension is widened, the bitty#1744/#1727
//! treatment (`HARNESS_WALL_BUDGET_MS`). Production keeps
//! `RC1_WALL_CLOCK_BUDGET_MS`; activation-budget enforcement stays pinned by
//! `bar_activation_budget.rs`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS, UiNode, WorkspaceInfo};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::services::WorkspaceSource;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};

const BAR_ID: &str = "bitty-terminal.bar";
/// `M.CLOCK_TICK_MS` in the pinned bar.
const CLOCK_TICK_SECS: u64 = 60;
/// `M.TIMER_CHAIN_CAP` in the pinned bar: the pre-armed one-shot chain.
const TIMER_CHAIN_CAP: u64 = 32;
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

fn pinned_bar_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("bar-pin")
}

fn unique_dir(tag: &str) -> PathBuf {
    static DATA_DIR_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bitty-bar-clock-{tag}-{}-{}",
        std::process::id(),
        DATA_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("data dir");
    dir
}

fn runtime(
    data_dir: PathBuf,
    roots: Vec<PathBuf>,
    settings: BTreeMap<String, LuaValue>,
) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data_dir),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: roots,
        settings: Rc::new(MapSettings(settings)),
        snapshot: Rc::new(StaticSnapshot),
    });
    // Debug-build harness headroom; see the module docs.
    rt.set_vm_wall_budget_ms(Some(HARNESS_WALL_BUDGET_MS));
    rt.set_workspace_source(Rc::new(ThreeWorkspaces));
    common::install_stub_backend(&mut rt);
    rt
}

/// Pinned bar with the clock as the only module, so every captured timer
/// belongs to the clock and no provider module can mask a frozen chain.
fn clock_only_runtime() -> (PluginRuntime, PathBuf) {
    let mut settings = BTreeMap::new();
    settings.insert("show".to_string(), LuaValue::Bool(true));
    settings.insert("show_single".to_string(), LuaValue::Bool(false));
    settings.insert("edge".to_string(), LuaValue::String("bottom".to_string()));
    settings.insert("modules_left".to_string(), str_array(&[]));
    settings.insert("modules_center".to_string(), str_array(&["clock"]));
    settings.insert("modules_right".to_string(), str_array(&[]));
    let data_dir = unique_dir("bar");
    let rt = runtime(data_dir.clone(), vec![pinned_bar_root()], settings);
    (rt, data_dir)
}

fn collect_text(node: &UiNode, out: &mut Vec<String>) {
    match node {
        UiNode::Text { text, .. } => out.push(text.clone()),
        UiNode::Row { children, .. }
        | UiNode::Column { children, .. }
        | UiNode::List { children, .. } => {
            for child in children {
                collect_text(child, out);
            }
        }
    }
}

fn bar_texts(rt: &PluginRuntime) -> Vec<String> {
    let blocks = rt.ui_blocks();
    assert_eq!(blocks.len(), 1, "bar mounts exactly one band");
    let mut texts = Vec::new();
    collect_text(&blocks[0].2, &mut texts);
    texts
}

fn clock_text(rt: &PluginRuntime) -> String {
    let texts = bar_texts(rt);
    assert_eq!(
        texts.len(),
        1,
        "clock-only plan renders one capsule: {texts:?}"
    );
    texts[0].clone()
}

fn bar_version(rt: &PluginRuntime) -> u32 {
    rt.ui_blocks()[0].3
}

/// Whether `text` is the live local `%H:%M` (`` HH:MM ``), allowing the
/// minute boundary between the render and this check.
fn is_live_local_minute(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    if !bytes[..2].iter().all(u8::is_ascii_digit) || !bytes[3..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let now = chrono::Local::now();
    [
        now,
        now - chrono::Duration::minutes(1),
        now + chrono::Duration::minutes(1),
    ]
    .iter()
    .any(|instant| instant.format("%H:%M").to_string() == text)
}

fn activate_clock_bar(rt: &mut PluginRuntime) -> PluginId {
    let bar = PluginId::new(BAR_ID).expect("id");
    let discovery = rt.discover();
    assert!(
        discovery
            .iter()
            .any(|(id, result)| id == &bar && result.is_ok()),
        "pinned bar package discovers: {discovery:?}"
    );
    let report = rt.activate(&bar).expect("clock-only bar activates");
    assert_eq!(report.state, LifecycleState::Active);
    bar
}

/// Drive the clock to `ticks` cadence deadlines, asserting on every pump that
/// a tick fired, that the band re-rendered exactly once per fire, that the
/// capsule is the live local minute, and that another wake stayed armed.
fn drive_clock(rt: &mut PluginRuntime, ticks: u64) -> (u64, u64, Instant) {
    let base = Instant::now();
    let limit = Duration::from_secs(ticks * CLOCK_TICK_SECS);
    let mut fires = 0u64;
    let mut pumps = 0u64;
    let mut last = base;
    let mut version = bar_version(rt);
    while let Some(deadline) = rt.timer_deadline() {
        if deadline > base + limit {
            break;
        }
        if pumps > 0 {
            assert_eq!(
                deadline,
                last + Duration::from_secs(CLOCK_TICK_SECS),
                "every clock minute stays on the plugin cadence (pump {pumps})"
            );
        }
        last = deadline;
        let fired = rt.pump_due_timers_at(deadline);
        assert!(fired >= 1, "a due deadline must fire at least one tick");
        fires += fired as u64;
        pumps += 1;
        assert_eq!(
            bar_version(rt),
            version + u32::try_from(fired).expect("bounded pump"),
            "every tick re-renders exactly once (pump {pumps})"
        );
        version = bar_version(rt);
        assert!(
            is_live_local_minute(&clock_text(rt)),
            "capsule stays the live local minute after pump {pumps}"
        );
        // Deadlines are armed only from a live VM capture row and stale rows
        // are pruned at the start of every pump, so an armed wake is the
        // observable proof the capture never ran dry.
        assert!(
            rt.timer_deadline().is_some(),
            "timer chain ran dry after pump {pumps} (fires so far: {fires})"
        );
    }
    assert_eq!(pumps, ticks, "one pump per clock cadence");
    (fires, pumps, last)
}

#[test]
fn pinned_bar_clock_ticks_past_the_prearmed_chain() {
    let (mut rt, data_dir) = clock_only_runtime();
    let bar = activate_clock_bar(&mut rt);
    assert!(
        is_live_local_minute(&clock_text(&rt)),
        "activation capsule must be the live local minute, got {:?}",
        clock_text(&rt)
    );

    let (fires, _, last) = drive_clock(&mut rt, 40);
    // Pre-armed chain fires: one tick at 60 s then two per minute while the
    // chain lasts (a chain follow-up and the 60 s re-arm share the deadline),
    // so 1 + 2*31 = 63 fires through 1920 s and one re-armed tick per minute
    // afterwards: 63 + 8 = 71 fires inside 40 minutes. A rejected re-arm would
    // end the lineage at 32 fires.
    assert_eq!(
        fires,
        1 + 2 * (TIMER_CHAIN_CAP - 1) + (40 - TIMER_CHAIN_CAP),
        "chain plus re-arms must deliver every fire inside 40 minutes"
    );

    // One more minute: the repeating lineage is still armed on the plugin's
    // cadence and still fires.
    let deadline = rt
        .timer_deadline()
        .expect("clock timer remains armed past 40 ticks");
    assert_eq!(
        deadline,
        last + Duration::from_secs(CLOCK_TICK_SECS),
        "next clock tick stays on the module cadence"
    );
    assert!(
        rt.pump_due_timers_at(deadline) >= 1,
        "clock fires past the pre-armed chain"
    );
    assert!(
        is_live_local_minute(&clock_text(&rt)),
        "capsule still renders the live local minute after the chain cap"
    );
    assert_eq!(rt.state(&bar), Some(&LifecycleState::Active));
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn pinned_bar_clock_ticks_past_four_hundred_ticks() {
    let (mut rt, data_dir) = clock_only_runtime();
    let bar = activate_clock_bar(&mut rt);
    let (fires, _, _) = drive_clock(&mut rt, 400);
    // 1 + 2*31 = 63 fires through the 32-tick chain, then one re-armed tick
    // per minute for the remaining 368 minutes: 431 fires.
    assert_eq!(
        fires,
        1 + 2 * (TIMER_CHAIN_CAP - 1) + (400 - TIMER_CHAIN_CAP),
        "the clock must keep firing past 400 ticks"
    );
    assert!(
        rt.timer_deadline().is_some(),
        "clock stays armed after 400 ticks"
    );
    assert!(is_live_local_minute(&clock_text(&rt)));
    assert_eq!(rt.state(&bar), Some(&LifecycleState::Active));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Writes the fixture plugin used by the re-arm instrument and the one-shot
/// control. The manifest declares one command (`:ok`) and `init_src` is
/// appended after the registration, mirroring `plugin_timers.rs`.
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

fn store_integer(rt: &PluginRuntime, id: &PluginId, key: &str) -> i64 {
    match rt
        .services(id)
        .and_then(|services| services.with_store(|store| store.get(key)))
    {
        Some(LuaValue::Integer(n)) => n,
        other => panic!("{key} must be an integer, got {other:?}"),
    }
}

fn drive_fixture(rt: &mut PluginRuntime, limit: Duration) -> u64 {
    let base = Instant::now();
    let mut pumps = 0u64;
    while let Some(deadline) = rt.timer_deadline() {
        if deadline > base + limit {
            break;
        }
        let fired = rt.pump_due_timers_at(deadline);
        assert!(fired >= 1, "a due deadline must fire at least one tick");
        pumps += 1;
        assert!(pumps <= 10_000, "runaway timer loop");
    }
    pumps
}

#[test]
fn bar_arm_shape_rearm_is_never_rejected() {
    // Mirror of the pinned bar's `arm_once`/`chain_ticks(1)` shape with the
    // rejection counter the bar discards: a 32-tick pre-armed chain plus one
    // `pcall(bitty.timers.create)` re-arm per first-tick callback, at the bar's
    // 1 s lower bound for a fast module (the bar's floor is 200 ms).
    let data = unique_dir("arm-data");
    let root = unique_dir("arm-root");
    let id = PluginId::new("bitty-featured.arm").expect("id");
    write_plugin(
        &root,
        "bitty-featured.arm",
        r#"
        bitty.store.set("fires", 0)
        bitty.store.set("rearm_fail", 0)
        local ms = 1000
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
        -- Pre-armed chain: the first tick re-arms, follow-ups render only.
        local ok, _ = pcall(function() return bitty.timers.create(ms, cb) end)
        if not ok then
          bitty.store.set("rearm_fail", 999)
        end
        for k = 2, 32 do
          bitty.timers.create(ms * k, function()
            local n = bitty.store.get("fires") or 0
            bitty.store.set("fires", n + 1)
          end)
        end
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()], BTreeMap::new());
    rt.discover();
    rt.activate(&id).expect("activate");
    let pumps = drive_fixture(&mut rt, Duration::from_secs(400));
    assert_eq!(pumps, 400, "one pump per second");
    assert_eq!(
        store_integer(&rt, &id, "rearm_fail"),
        0,
        "callback re-arms must never hit E_DEF_LIMIT or a rate-policy rejection"
    );
    assert_eq!(
        store_integer(&rt, &id, "fires"),
        431,
        "1 s chain plus re-arms delivers every fire inside 400 s"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn one_shot_timer_control_still_fires_once() {
    // Control: the fake-clock pump still delivers an ordinary one-shot timer
    // exactly once, so the chain result is not an artifact of the driver.
    let data = unique_dir("control-data");
    let root = unique_dir("control-root");
    let id = PluginId::new("bitty-featured.control").expect("id");
    write_plugin(
        &root,
        "bitty-featured.control",
        r#"
        bitty.store.set("fired", 0)
        bitty.timers.create(10, function()
          local n = bitty.store.get("fired") or 0
          bitty.store.set("fired", n + 1)
        end)
        "#,
    );
    let mut rt = runtime(data.clone(), vec![root.clone()], BTreeMap::new());
    rt.discover();
    rt.activate(&id).expect("activate");
    let deadline = rt.timer_deadline().expect("one-shot arms a wake");
    assert_eq!(rt.pump_due_timers_at(deadline), 1);
    assert_eq!(store_integer(&rt, &id, "fired"), 1);
    assert_eq!(rt.timer_deadline(), None);
    assert_eq!(rt.pump_due_timers_at(deadline), 0);
    assert_eq!(store_integer(&rt, &id, "fired"), 1);
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&root);
}
