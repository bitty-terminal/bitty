//! Compressed long-run soak for bitty#2830 (headless, bounded budget).
//!
//! The owner's unresponsiveness appeared after hours of ordinary use, so this
//! harness compresses time instead of waiting: `tick_at` receives an injected
//! clock that advances twenty-five seconds per frame, layout churn, resizes,
//! event pumps, and plugin activation cycles run against that clock, and the
//! test asserts the same bounds the hang watchdog reports: queue depths,
//! retained content bytes, pane-session count, file descriptors, and RSS.
//!
//! The wall-clock budget is a liveness guard, not a performance target: a
//! hang never finishes, while a correct run only tracks runner speed. CI runs
//! the unoptimized `test` profile on shared runners, where this workload costs
//! roughly an order of magnitude more than a developer machine; the frame
//! count is sized so the compressed span (5.5 simulated hours) still fits a
//! shard with headroom. The RSS/fd assertions are Linux-only (`/proc`); other
//! targets run the workload and keep the queue/layout assertions.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bitty_lua::{BridgeError, LuaValue};
use bitty_platform::{PhysicalSize, PlatformEvent, WindowEventKind, WindowId};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::hang_watchdog::HangGauges;
use bitty_runtime::plugin_runtime::{
    LifecycleState, PluginRuntime, PluginRuntimeConfig, SettingsSource, SnapshotSource,
};
use bitty_runtime::{LayoutNode, Runtime, RuntimeConfig, View, ViewId};
use bitty_ui::SplitAxis;

/// Liveness budget on a shared CI shard.
///
/// Measured on the unoptimized `test` profile: 171 s for 2 000 frames before
/// the workload was halved and the per-frame simulated step doubled. The
/// current 800-frame run keeps the same 20 000 simulated seconds; this guard
/// leaves headroom for a loaded runner while nextest's own slow-timeout sits
/// above it.
const SOAK_BUDGET_CI: Duration = Duration::from_secs(150);
/// Liveness budget on a development host.
const SOAK_BUDGET_LOCAL: Duration = Duration::from_secs(60);
/// Frames in the compressed run (twenty-five simulated seconds each).
///
/// 800 x 25 s preserves the pre-recalibration 20 000 simulated seconds
/// (~5.5 h) while cutting the number of presented frames to 40 % of the
/// original budget-busting run.
const FRAMES: u64 = 800;
/// Simulated seconds per frame.
const SECONDS_PER_FRAME: u64 = 25;
/// Split/merge cadence in frames (400 simulated seconds).
const LAYOUT_CHURN_FRAMES: u64 = 16;
/// Window-resize cadence in frames (100 simulated seconds).
const RESIZE_FRAMES: u64 = 4;
/// Cold-ring drain cadence in frames (2 000 simulated seconds).
const COLD_DRAIN_FRAMES: u64 = 80;
/// Serializes the soak tests: RSS and fd counts are process-wide.
///
/// Each integration test binary is its own process under nextest, but a
/// plain `cargo test` runs both tests on threads of one process. The churn
/// test creates Lua VMs and store backends, so its transient descriptors and
/// memory must not fall inside the other test's measurement window.
static PROCESS_METRICS_LOCK: Mutex<()> = Mutex::new(());
/// Hard cap on RSS growth across the compressed run (allocator noise included).
#[cfg(target_os = "linux")]
const MAX_RSS_GROWTH_KIB: u64 = 64 * 1024;
/// Hard cap on file-descriptor growth across the compressed run.
#[cfg(target_os = "linux")]
const MAX_FD_GROWTH: i64 = 16;

fn make_runtime() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig::default())
        .expect("default deterministic runtime must build")
}

fn two_pane_layout(first: ViewId, second: ViewId) -> LayoutNode {
    LayoutNode::split(
        SplitAxis::Horizontal,
        0.5,
        LayoutNode::leaf(View::new(first, 80, 24)),
        LayoutNode::leaf(View::new(second, 80, 24)),
    )
}

/// Resident set size in KiB from `/proc/self/status`.
///
/// `VmRSS` is already in KiB, so no page-size lookup is needed (and the
/// lock-sensitive `smaps` traversal is avoided).
#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("VmRSS:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn fd_count() -> i64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as i64)
        .unwrap_or(0)
}

#[test]
fn compressed_long_run_stays_bounded() {
    let _serial = PROCESS_METRICS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut rt = make_runtime();
    rt.tick_at(Instant::now()).expect("first full redraw");

    #[cfg(target_os = "linux")]
    let (rss_start, fd_start) = (rss_kib(), fd_count());

    let start = Instant::now();
    let clock = Instant::now();
    let mut presented = 0u64;
    for frame in 0..FRAMES {
        let now = clock + Duration::from_secs(frame * SECONDS_PER_FRAME);
        rt.handle_pty_bytes(b"soak \x1b[31mred\x1b[0m \x1b]0;title\x07\r\n");
        // Layout churn: split/merge every 400 simulated seconds (a long
        // session's panel adds and closes), then resize the whole window
        // every 100 simulated seconds.
        if frame % LAYOUT_CHURN_FRAMES == 0 {
            rt.set_layout(two_pane_layout(ViewId::new(1), ViewId::new(2)));
        } else if frame % LAYOUT_CHURN_FRAMES == LAYOUT_CHURN_FRAMES / 2 {
            rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
        }
        if frame % RESIZE_FRAMES == 0 {
            let width = if (frame / RESIZE_FRAMES) % 2 == 0 {
                1_280
            } else {
                1_440
            };
            let _ = rt.handle_resize(PhysicalSize::new(width, 900));
        }
        // Daemon-style window events and a periodic config-free redraw.
        let _ = rt.handle_platform_event(PlatformEvent::Window {
            window_id: WindowId::from_raw_public(1),
            kind: WindowEventKind::Focused(frame % 2 == 0),
        });
        if rt.tick_at(now).is_some() {
            presented += 1;
        }
        // Drain the cold ring at the same simulated cadence (the plugin cold
        // phase).
        if frame % COLD_DRAIN_FRAMES == 0 {
            let _ = rt.drain_cold_events();
        }
        assert!(
            rt.cold_queue_len() <= rt.cold_queue_capacity(),
            "cold queue overflow at frame {frame}"
        );
        assert!(
            rt.plugin_side_len() <= rt.plugin_side_capacity(),
            "plugin side queue overflow at frame {frame}"
        );
    }
    // The layout returns to one leaf; no pane session may survive the churn.
    rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
    let _ = rt.tick_at(clock + Duration::from_secs(FRAMES * SECONDS_PER_FRAME));
    // CodeQL rust/cleartext-logging: the panic message must not carry any
    // value derived from the session ids; `is_empty()` alone names the defect.
    assert!(
        rt.pane_session_ids().is_empty(),
        "layout churn must not leave pane sessions"
    );

    // Real PTY pane attach/detach churn (issue #2830 asks for repeated
    // attach/detach; the layout churn above is presentation-only).
    #[cfg(unix)]
    {
        let pane_clock = clock + Duration::from_secs(FRAMES * SECONDS_PER_FRAME);
        for cycle in 0..10u64 {
            rt.set_layout(two_pane_layout(ViewId::new(1), ViewId::new(2)));
            // pty-gate-exempt: inside `#[cfg(unix)]`; /bin/sh never spawns on Windows.
            let _ = rt.spawn_shell_for_view(ViewId::new(2), "/bin/sh", &["-c", "printf x"], 40, 10);
            for step in 0..3u64 {
                let _ = rt.poll_pty();
                let _ = rt.tick_at(pane_clock + Duration::from_millis(cycle * 10 + step));
            }
            assert!(rt.close_pane_session(&ViewId::new(2)));
            assert!(
                rt.pane_session_ids().is_empty(),
                "cycle {cycle} left a pane session"
            );
            rt.set_layout(LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)));
            let _ = rt.tick_at(pane_clock + Duration::from_millis(cycle * 10 + 9));
        }
    }

    let gauges = rt.hang_gauges().snapshot();
    assert!(
        gauges.tick_count >= FRAMES,
        "every compressed frame must be counted, got {}",
        gauges.tick_count
    );
    assert!(
        gauges.content_bytes <= 64 * 1024 * 1024,
        "retained content bytes must stay bounded, got {}",
        gauges.content_bytes
    );
    assert_eq!(gauges.plugin_parked_commands, 0);
    assert_eq!(gauges.plugin_active_commands, 0);
    assert!(
        presented > FRAMES / 2,
        "most frames present, got {presented}"
    );

    let elapsed = start.elapsed();
    let is_ci = std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok();
    let budget = if is_ci {
        SOAK_BUDGET_CI
    } else {
        SOAK_BUDGET_LOCAL
    };
    assert!(
        elapsed < budget,
        "compressed soak must finish inside its budget, took {elapsed:?} (budget {budget:?})"
    );

    #[cfg(target_os = "linux")]
    {
        let rss_growth = rss_kib().saturating_sub(rss_start);
        assert!(
            rss_growth <= MAX_RSS_GROWTH_KIB,
            "RSS grew {rss_growth} KiB over the compressed run (cap {MAX_RSS_GROWTH_KIB})"
        );
        let fd_growth = fd_count() - fd_start;
        assert!(
            fd_growth <= MAX_FD_GROWTH,
            "fd count grew {fd_growth} over the compressed run (cap {MAX_FD_GROWTH})"
        );
    }
}

// ---------------------------------------------------------------------------
// Plugin activation churn: repeated activate/dispose cycles stay bounded.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MapSettings(BTreeMap<String, LuaValue>);

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

const PLUGIN_INIT: &str = r#"
bitty.commands.register({
  id = "noop",
  title = "Noop",
  run = function(_args) return "ok" end,
})
return {}
"#;

fn write_plugin(root: &Path, id: &str) -> PathBuf {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "Soak"
version = "0.1.0"
description = "soak"

[compat]
plugin-api = "^1.0"

[lazy]
commands = ["soak.plugin:noop"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), PLUGIN_INIT).expect("init");
    plugin
}

#[test]
fn plugin_activation_churn_is_bounded() {
    let _serial = PROCESS_METRICS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("bitty-hang-soak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let third_party = root.join("plugins");
    write_plugin(&third_party, "soak.plugin");
    let data = root.join("data");
    let gauges = Arc::new(HangGauges::new());
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode: false,
        data_dir: Some(data),
        store_root: None,
        bundled_roots: Vec::new(),
        third_party_roots: vec![third_party],
        settings: Rc::new(MapSettings::default()),
        snapshot: Rc::new(StaticSnapshot(LuaValue::table([(
            "version",
            LuaValue::Integer(1),
        )]))),
    });
    common::install_stub_backend(&mut rt);
    common::install_harness_seams(&mut rt);
    rt.set_hang_gauges(Arc::clone(&gauges));
    let id = PluginId::new("soak.plugin").expect("id");
    let report = rt.discover();
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(report[0].1.is_ok(), "discovery failed: {report:?}");

    for cycle in 0..50 {
        let report = rt.activate(&id).expect("activate");
        assert_eq!(report.state, LifecycleState::Active);
        assert_eq!(gauges.snapshot().plugin_workers, 1);
        let _ = rt.dispatch_command(&id, "noop", &[]).expect("command");
        assert_eq!(gauges.snapshot().plugin_active_commands, 0);
        rt.dispose(&id).expect("dispose");
        assert_eq!(
            gauges.snapshot().plugin_workers,
            0,
            "cycle {cycle} must drop the VM"
        );
        assert_eq!(gauges.snapshot().plugin_parked_commands, 0);
    }
    let _ = std::fs::remove_dir_all(&root);
}
