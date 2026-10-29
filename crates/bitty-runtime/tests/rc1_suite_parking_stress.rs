//! RC-1 wall-budget parking across the runtime capability suites (bitty#2800).
//!
//! bitty#2795 made the parking recovery deterministic for the probes it
//! touched, but the same wall-clock accounting still parks any plugin command
//! on a loaded runner. This sibling harness replays the command shapes of the
//! suites fixed by bitty#2800 — colorscheme image/theme, cross-VM services,
//! workspace list, debug inspect, and UI update — from several worker threads
//! under CPU pressure. Every worker uses the documented probe treatment
//! ([`common::install_harness_seams`] plus [`common::settle_dispatch`]) and
//! must return the command's own result; the production budgets are never
//! changed.
//!
//! Run it with `scripts/rc1-parking-stress.sh`, which also runs the real
//! affected suites under the same load.

#![forbid(unsafe_code)]

mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use bitty_lua::{BridgeError, LuaValue};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, LifecycleState, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
    WorkspaceInfo, WorkspaceSource,
};

const PROBE_ID: &str = "bitty.rc1-suite-probe";
const PROVIDER_ID: &str = "xuepoo.rc1-calc";
const CONSUMER_ID: &str = "xuepoo.rc1-shop";
const SERVICE_HEAD: &str = "runtime.inspect";

/// Default worker count: the CI quality shard runs nextest with one thread
/// per runner core (four on the standard runner), the width that produced the
/// reported flakes.
const DEFAULT_WORKERS: usize = 4;

/// Iterations per worker: enough to sample parking without a long campaign.
const DEFAULT_ITERATIONS: usize = 25;

struct StaticSnapshot;

impl SnapshotSource for StaticSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

/// Empty workspace source: the probe asserts the `bitty.workspace.list`
/// bridge path, not workspace data, so the row shape stays out of this
/// harness.
struct FixedWorkspaces;

impl WorkspaceSource for FixedWorkspaces {
    fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, BridgeError> {
        Ok(Vec::new())
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("bitty-rc1-suite-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Minimal PNG carrying only signature, IHDR, and IEND: the image hook sniffs
/// headers without decoding.
fn tiny_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |ctype: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(ctype);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0u8; 4]);
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IEND", &[]);
    out
}

fn lua_path(path: &Path) -> String {
    path.to_str()
        .expect("probe path is UTF-8")
        .replace('\\', "/")
}

const PROBE_SRC: &str = r##"
local function call(f, ...)
  local ok, result = pcall(f, ...)
  if ok then return "OK:" .. tostring(result) end
  return "ERR:" .. result.code
end

local theme_arg = {
  background = "#000000", foreground = "#ffffff",
  cursor = "#ff0000", selection = "#00ff00",
  ansi = {
    "#111111", "#222222", "#333333", "#444444",
    "#555555", "#666666", "#777777", "#888888",
    "#999999", "#aaaaaa", "#bbbbbb", "#cccccc",
    "#dddddd", "#eeeeee", "#abcdef", "#fedcba",
  },
}

bitty.commands.register({ id = "image_read", title = "Image read", run = function()
  local ok, d = pcall(bitty.image.read, "WALL")
  if not ok then return "ERR:" .. d.code end
  return "OK:" .. d.format .. ":" .. d.width .. "x" .. d.height
end })
bitty.commands.register({ id = "theme_preview", title = "Theme preview", run = function()
  return call(bitty.theme.preview, theme_arg)
end })

local handle = nil
local mounted, mounted_handle = pcall(bitty.ui.mount, "bottom", { kind = "Text", text = "load" })
if mounted then handle = mounted_handle end
bitty.commands.register({ id = "ui_update", title = "UI update", run = function()
  if handle == nil then return "ERR:E_UI_NO_HANDLE" end
  return "OK:" .. tostring(bitty.ui.update(handle, { kind = "Text", text = "load" }))
end })

local workspace_rows = nil
bitty.commands.register({ id = "workspace_list", title = "Workspace list", run = function()
  local ok, rows = pcall(bitty.workspace.list)
  if not ok then return "ERR:" .. rows.code end
  workspace_rows = rows
  return "OK:" .. tostring(#rows)
end })

bitty.commands.register({ id = "inspect", title = "Debug inspect", run = function()
  local ok, rows = pcall(bitty.debug.inspect, "plugins")
  if not ok then return "ERR:" .. rows.code end
  return "OK:" .. tostring(#rows)
end })
"##;

fn write_probe(root: &Path, wall: &Path) {
    let plugin = root.join(PROBE_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PROBE_ID}"
name = "RC-1 suite probe"
version = "0.1.0"
description = "bitty#2800 suite parking stress"

[compat]
plugin-api = "^1.0"

[capabilities]
platform.image-file = true
runtime.configure = true
ui.rich = true
workspace.read = true
debug.inspect = true

[lazy]
commands = ["{PROBE_ID}:image_read", "{PROBE_ID}:theme_preview", "{PROBE_ID}:ui_update", "{PROBE_ID}:workspace_list", "{PROBE_ID}:inspect"]
events = []
"#
        ),
    )
    .expect("manifest");
    let src = PROBE_SRC.replace("WALL", &lua_path(wall));
    std::fs::write(plugin.join("lua/init.lua"), src).expect("init");
}

fn write_provider(root: &Path) {
    let plugin = root.join(PROVIDER_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PROVIDER_ID}"
name = "RC-1 service provider"
version = "0.1.0"
description = "bitty#2800 suite parking stress"

[compat]
plugin-api = "^1.0"

[capabilities]
{SERVICE_HEAD} = true

[services.provided]
"rc1.add" = {{ version = "1.0.0", heads = ["{SERVICE_HEAD}"] }}

[lazy]
commands = []
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
bitty.services.provide("rc1.add", {
  add = function(args) return { sum = args.a + args.b } end,
})
return {}
"#,
    )
    .expect("init");
}

fn write_consumer(root: &Path) {
    let plugin = root.join(CONSUMER_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{CONSUMER_ID}"
name = "RC-1 service consumer"
version = "0.1.0"
description = "bitty#2800 suite parking stress"

[compat]
plugin-api = "^1.0"

[capabilities]
{SERVICE_HEAD} = true

[services.required]
"rc1.add" = ">=1.0"

[lazy]
commands = ["{CONSUMER_ID}:total"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
local calc = bitty.services.get("rc1.add")
bitty.commands.register({ id = "total", title = "Total", run = function()
  local result = calc.add({ a = 2, b = 3 })
  return result.sum
end })
return {}
"#,
    )
    .expect("init");
}

/// One isolated runtime over fresh probe packages.
struct World {
    runtime: PluginRuntime,
    root: PathBuf,
    data: PathBuf,
    probe: PluginId,
    consumer: PluginId,
}

impl World {
    fn activate(tag: &str) -> Self {
        let root = temp_dir(tag);
        let wall = root.join("paper.png");
        std::fs::write(&wall, tiny_png(3, 2)).expect("write png");
        write_probe(&root, &wall);
        write_provider(&root);
        write_consumer(&root);
        let data = temp_dir(&format!("{tag}-data"));
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings: Rc::new(EmptySettings),
            snapshot: Rc::new(StaticSnapshot),
        });
        runtime.set_image_roots(vec![root.clone()]);
        runtime.set_workspace_source(Rc::new(FixedWorkspaces));
        common::install_harness_seams(&mut runtime);
        runtime.discover();
        let provider = PluginId::new(PROVIDER_ID).expect("provider id");
        let probe = PluginId::new(PROBE_ID).expect("probe id");
        let consumer = PluginId::new(CONSUMER_ID).expect("consumer id");
        for id in [&provider, &probe, &consumer] {
            let report = runtime
                .activate(id)
                .unwrap_or_else(|error| panic!("activate {id}: {error}"));
            assert_eq!(report.state, LifecycleState::Active);
        }
        Self {
            runtime,
            root,
            data,
            probe,
            consumer,
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// Wall-clock samples of one worker.
#[derive(Default)]
struct WorkerReport {
    dispatches: u64,
    parks: u64,
    failures: Vec<String>,
}

impl WorkerReport {
    fn merge(&mut self, other: WorkerReport) {
        self.dispatches += other.dispatches;
        self.parks += other.parks;
        self.failures.extend(other.failures);
    }
}

/// One worker: replay each suite's command shape and require its own result.
fn worker(iterations: usize) -> WorkerReport {
    let mut world = World::activate("load");
    let mut report = WorkerReport::default();
    let probe = world.probe.clone();
    let consumer = world.consumer.clone();
    for _ in 0..iterations {
        for (label, id, command) in [
            ("image_read", &probe, "image_read"),
            ("theme_preview", &probe, "theme_preview"),
            ("ui_update", &probe, "ui_update"),
            ("workspace_list", &probe, "workspace_list"),
            ("inspect", &probe, "inspect"),
            ("total", &consumer, "total"),
        ] {
            let settled = common::settle_dispatch(&mut world.runtime, id, command, &[]);
            report.dispatches += 1;
            if settled.parked {
                report.parks += 1;
            }
            let ok = match (label, settled.outcome) {
                ("image_read", Ok(LuaValue::String(value))) => value.starts_with("OK:png:3x2"),
                ("theme_preview", Ok(LuaValue::String(value))) => value == "OK:true",
                ("ui_update", Ok(LuaValue::String(value))) => value == "OK:true",
                ("workspace_list", Ok(LuaValue::String(value))) => value == "OK:0",
                ("inspect", Ok(LuaValue::String(value))) => value.starts_with("OK:"),
                ("total", Ok(LuaValue::Integer(value))) => value == 5,
                (_, Ok(other)) => {
                    report
                        .failures
                        .push(format!("{label}: unexpected result {other:?}"));
                    false
                }
                (_, Err(error)) => {
                    report.failures.push(format!("{label}: {error}"));
                    false
                }
            };
            if !ok
                && !report
                    .failures
                    .iter()
                    .any(|failure| failure.starts_with(label))
            {
                report
                    .failures
                    .push(format!("{label}: result assertion failed"));
            }
            if label == "theme_preview" {
                // Bounded queue: drain the intent exactly like the app tick.
                let _ = world.runtime.drain_theme_requests();
            }
        }
    }
    report
}

/// Load harness: the runtime suite command shapes must settle and return
/// their own results, never surface a park as a refusal.
#[test]
#[ignore = "load harness; run via scripts/rc1-parking-stress.sh"]
fn suite_probes_survive_the_load() {
    let workers: usize = std::env::var("RC1_SUITE_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_WORKERS);
    let iterations: usize = std::env::var("RC1_SUITE_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    assert!(workers > 0 && iterations > 0);

    let handles: Vec<_> = (0..workers)
        .map(|_| std::thread::spawn(move || worker(iterations)))
        .collect();
    let mut report = WorkerReport::default();
    for handle in handles {
        match handle.join() {
            Ok(worker) => report.merge(worker),
            Err(_) => report.failures.push("worker panicked".to_string()),
        }
    }

    let rate = if report.dispatches == 0 {
        0.0
    } else {
        (report.parks as f64) * 100.0 / (report.dispatches as f64)
    };
    println!(
        "rc1-suite-parking: workers={workers} iterations={iterations} \
         dispatches={} parks={} rate={rate:.1}%",
        report.dispatches, report.parks,
    );
    assert!(
        report.failures.is_empty(),
        "rc1-suite-parking: {} of {} dispatches failed: {:?}",
        report.failures.len(),
        report.dispatches,
        &report.failures[..report.failures.len().min(5)]
    );
    assert!(report.dispatches > 0);
}
