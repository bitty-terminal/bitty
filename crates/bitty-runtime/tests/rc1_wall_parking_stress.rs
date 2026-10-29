//! RC-1 wall-budget parking under a loaded runner (bitty#2781).
//!
//! RC-1 is a *wall-clock* budget: it bounds how long a plugin callback may
//! stall the terminal event loop. The charge is therefore elapsed time, which
//! includes time the thread spent off-CPU. On a loaded runner the OS can
//! deschedule the test thread for a whole window, so a command body that used
//! microseconds of CPU still reaches the budget and returns
//! [`PluginRuntimeError::Parked`]. That is the documented recovery path — the
//! executor survives and the next cold tick resumes it — not a refusal.
//!
//! Two properties are pinned here:
//!
//! - [`forced_park_settles_on_the_documented_cold_tick`] (always run) injects
//!   a deterministic slow tick: `FORCED_PARK_TICKS` host calls that each
//!   charge `FORCED_PARK_TICK_MS` of wall time — twice the production 50 ms
//!   slice in total and far below the 5250 ms callback ceiling. The shared
//!   `settle_dispatch` helper must complete the body exactly once on the
//!   documented resume signal. The pre-bitty#2781 probes dispatched once and
//!   treated `Parked` as a failure, so this test fails deterministically under
//!   the old timing.
//! - [`load_harness_raw_dispatch_fails_on_any_park`] and
//!   [`load_harness_settled_dispatch_survives_the_load`] (`#[ignore]`, driven
//!   by `scripts/rc1-parking-stress.sh`) run the cheapest realistic probe from
//!   several worker threads under CPU pressure. The raw mode keeps production
//!   budgets and one dispatch, so it *fails on any parking* and prints the
//!   measured rate. The settled mode applies the probe treatment — settle the
//!   documented resume and raise only the harness host-call deadline — and
//!   must pass under the same load.
//!
//! The production budgets are never widened: the harness measures what the
//! shipped 50 ms slice does under load.

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bitty_config::ResolvedKeymap;
use bitty_lua::keymaps::KEYMAP_LIST_CALLS_PER_SECOND;
use bitty_lua::{BridgeError, LuaValue, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    EmptySettings, KeymapSource, PluginRuntime, PluginRuntimeConfig, SnapshotSource,
};

use common::RC1_RESUME_TICKS;

const PROBE_ID: &str = "bitty.rc1-parking";

/// Default worker count: the CI quality shard runs nextest with one thread
/// per runner core (four on the standard runner), which is the width that
/// produced the reported flakes.
const DEFAULT_WORKERS: usize = 4;

/// Iterations per worker. Enough for a stable rate without a long campaign.
const DEFAULT_ITERATIONS: usize = 50;

/// Wall time one forced-park host call charges.
///
/// A fifth of the production RC-1 slice keeps each call far inside the 50 ms
/// host-call deadline, so the command crosses the slice over several calls
/// instead of one oversized one.
const FORCED_PARK_TICK_MS: u64 = RC1_WALL_CLOCK_BUDGET_MS / 5;

/// Charged host time the forced-park command spends: twice the production
/// slice, so the park is guaranteed on any runner, loaded or idle, and still
/// far below the 5250 ms callback ceiling.
const FORCED_PARK_CHARGE_MS: u64 = 2 * RC1_WALL_CLOCK_BUDGET_MS;

/// Host calls the forced-park command makes, derived so the command charges
/// exactly `FORCED_PARK_CHARGE_MS`.
const FORCED_PARK_TICKS: u64 = FORCED_PARK_CHARGE_MS / FORCED_PARK_TICK_MS;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-rc1-parking-{tag}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn pid() -> PluginId {
    PluginId::new(PROBE_ID).expect("plugin id")
}

/// Fixed-row keymap source: the cheapest realistic host read a plugin can do.
#[derive(Default)]
struct FixedKeymaps(RefCell<Vec<ResolvedKeymap>>);

impl KeymapSource for FixedKeymaps {
    fn keymaps(&self) -> Result<Vec<ResolvedKeymap>, BridgeError> {
        Ok(self.0.borrow().clone())
    }
}

/// Host snapshot charged `FORCED_PARK_TICK_MS` of real wall time per call
/// (slow-tick seam).
struct SlowSnapshot;

impl SnapshotSource for SlowSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(Duration::from_millis(FORCED_PARK_TICK_MS));
        Ok(LuaValue::table([("version", LuaValue::Integer(1))]))
    }
}

const INIT_SRC: &str = r#"
local runs = 0
bitty.commands.register({ id = "list", title = "List", run = function()
  local ok, page = pcall(bitty.keymaps.list)
  if not ok then return "ERR:" .. page.code end
  return "total:" .. page.total .. ":entries:" .. #page.entries
end })
bitty.commands.register({ id = "slow", title = "Slow", run = function()
  for _ = 1, @FORCED_PARK_TICKS@ do
    pcall(bitty.terminal.snapshot, { scope = "semantic" })
  end
  runs = runs + 1
  return "slow:" .. runs
end })
bitty.commands.register({ id = "runs", title = "Runs", run = function()
  return tostring(runs)
end })
"#;

/// `INIT_SRC` with the derived forced-park loop bound substituted in.
///
/// The Lua body cannot read Rust constants, so the host-call count is a
/// placeholder replaced from `FORCED_PARK_TICKS` rather than a literal.
fn init_src() -> String {
    INIT_SRC.replace("@FORCED_PARK_TICKS@", &FORCED_PARK_TICKS.to_string())
}

fn write_plugin(root: &Path) {
    let plugin = root.join(PROBE_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PROBE_ID}"
name = "RC-1 parking stress"
version = "0.1.0"
description = "bitty#2781 wall-budget parking stress"

[compat]
plugin-api = "^1.0"

[capabilities]
runtime.keymaps.read = true
terminal.semantic-read = true

[lazy]
commands = ["{PROBE_ID}:list", "{PROBE_ID}:slow", "{PROBE_ID}:runs"]
events = []
"#
        ),
    )
    .expect("manifest");
    std::fs::write(plugin.join("lua/init.lua"), init_src()).expect("init");
}

/// One isolated runtime over a fresh plugin package.
struct World {
    runtime: PluginRuntime,
    root: PathBuf,
    data: PathBuf,
}

impl World {
    /// `harness_seams` installs the probe treatment: the documented test-only
    /// host-call and wall-window seams, both at the callback envelope.
    /// Activation is slice-only and cannot park, so the wall seam must be
    /// installed before `discover` builds the VM.
    fn activate(tag: &str, keymap_source: bool, harness_seams: bool) -> Self {
        let root = temp_dir(tag);
        write_plugin(&root);
        let data = temp_dir(&format!("{tag}-data"));
        let settings = Rc::new(EmptySettings);
        let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
            safe_mode: false,
            data_dir: Some(data.clone()),
            store_root: None,
            bundled_roots: Vec::new(),
            third_party_roots: vec![root.clone()],
            settings,
            snapshot: Rc::new(SlowSnapshot),
        });
        if harness_seams {
            runtime.set_host_call_deadline_ms(Some(common::HARNESS_HOST_CALL_DEADLINE_MS));
            runtime.set_vm_wall_budget_ms(Some(common::HARNESS_VM_WALL_BUDGET_MS));
        }
        if keymap_source {
            let source = Rc::new(FixedKeymaps::default());
            *source.0.borrow_mut() =
                bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
                    .expect("defaults");
            runtime.set_keymap_source(source);
        }
        runtime.discover();
        for (id, result) in runtime.activate_discovered() {
            result.unwrap_or_else(|error| panic!("activate {id}: {error}"));
        }
        Self {
            runtime,
            root,
            data,
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// Deterministic regression: an injected slow tick parks the callback, and
/// the documented cold-tick resume completes the body exactly once.
///
/// The charged host time is `FORCED_PARK_TICKS` x `FORCED_PARK_TICK_MS` —
/// twice the production 50 ms slice and a fraction of the 5250 ms callback
/// ceiling — so the park is guaranteed on any runner, loaded or idle, without
/// widening or shrinking any budget.
#[test]
fn forced_park_settles_on_the_documented_cold_tick() {
    let mut world = World::activate("forced-park", false, false);
    let first = common::settle_dispatch(&mut world.runtime, &pid(), "slow", &[]);
    let parked = first.parked;
    let resume_ticks = first.resume_ticks;
    assert!(
        parked,
        "{} ms of charged host time ({}x the {RC1_WALL_CLOCK_BUDGET_MS} ms RC-1 slice) must park",
        FORCED_PARK_CHARGE_MS,
        FORCED_PARK_CHARGE_MS / RC1_WALL_CLOCK_BUDGET_MS,
    );
    assert_eq!(
        first.expect_string("slow"),
        "slow:1",
        "the resumed body returns its own result"
    );
    assert!(
        resume_ticks <= RC1_RESUME_TICKS,
        "a park must settle inside the documented ceiling/window bound"
    );
    assert_eq!(world.runtime.parked_command_count(), 0);
    assert!(
        world.runtime.pump_parked_commands().is_empty(),
        "a completed command must never replay"
    );
    // Exactly one body across the park and its resume.
    assert_eq!(
        common::settle_dispatch(&mut world.runtime, &pid(), "runs", &[]).expect_string("runs"),
        "1"
    );
}

/// Wall-clock samples of one worker.
///
/// The probe body and its inputs are identical on every iteration, so the
/// clean-versus-parked wall distributions are the measurement that separates
/// "the budget accounting is wrong" from "the thread was off-CPU": a parked
/// dispatch cannot have run more code than a clean one.
#[derive(Default)]
struct WorkerReport {
    dispatches: u64,
    parks: u64,
    resume_ticks: u64,
    /// Wall time of dispatches that completed inside their first window.
    clean_wall_us: Vec<u64>,
    /// Wall time of dispatches that parked at least once.
    parked_wall_us: Vec<u64>,
    failures: Vec<String>,
}

impl WorkerReport {
    fn merge(&mut self, other: WorkerReport) {
        self.dispatches += other.dispatches;
        self.parks += other.parks;
        self.resume_ticks += other.resume_ticks;
        self.clean_wall_us.extend(other.clean_wall_us);
        self.parked_wall_us.extend(other.parked_wall_us);
        self.failures.extend(other.failures);
    }
}

/// Nearest-rank percentile of an unsorted sample set.
fn percentile(samples: &mut [u64], percent: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let rank = (samples.len() * percent).div_ceil(100).max(1);
    samples[rank - 1]
}

/// How one harness run treats a park.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Production budgets, a single dispatch: any park or host-call timeout is
    /// a failure. This is the pre-bitty#2781 probe behaviour and reproduces
    /// the CI failure.
    Raw,
    /// Probe treatment: the documented cold-tick resume plus the harness
    /// host-call deadline. Parks are counted but settle, so the probe asserts
    /// the command result instead of the machine's scheduling.
    Settled,
}

/// One worker: `iterations` dispatches of the cheapest realistic probe.
///
/// `bitty.keymaps.list` carries a per-generation call budget
/// ([`KEYMAP_LIST_CALLS_PER_SECOND`]), so a worker activates a fresh
/// generation when the budget runs out. Re-activation is outside the measured
/// dispatch.
fn worker(iterations: usize, mode: Mode) -> WorkerReport {
    let per_generation = KEYMAP_LIST_CALLS_PER_SECOND as usize;
    let mut world = World::activate("load", true, mode == Mode::Settled);
    let mut in_generation = 0usize;
    let expected = bitty_config::resolve_keymaps(&bitty_config::EffectiveConfig::default())
        .expect("defaults")
        .len();
    let expected = format!("total:{expected}:entries:{expected}");
    let mut report = WorkerReport::default();
    for _ in 0..iterations {
        if in_generation == per_generation {
            world = World::activate("load", true, mode == Mode::Settled);
            in_generation = 0;
        }
        in_generation += 1;
        let wall_before = Instant::now();
        let settled = common::settle_dispatch(&mut world.runtime, &pid(), "list", &[]);
        let wall_us = u64::try_from(wall_before.elapsed().as_micros()).unwrap_or(u64::MAX);
        report.dispatches += 1;
        if settled.parked {
            report.parks += 1;
            report.resume_ticks += u64::try_from(settled.resume_ticks).unwrap_or(u64::MAX);
            report.parked_wall_us.push(wall_us);
            if mode == Mode::Raw {
                report
                    .failures
                    .push("Parked at the RC-1 wall budget".to_string());
            }
        } else {
            report.clean_wall_us.push(wall_us);
        }
        match settled.outcome {
            Ok(LuaValue::String(value)) if value == expected => {}
            Ok(LuaValue::String(value)) if value == "ERR:E_TIMEOUT" => report
                .failures
                .push("host call exceeded its deadline (E_TIMEOUT)".to_string()),
            Ok(other) => report
                .failures
                .push(format!("expected {expected:?}, got {other:?}")),
            Err(error) => report
                .failures
                .push(format!("dispatch did not settle: {error}")),
        }
    }
    report
}

/// Shared harness body for both modes.
fn run_harness(mode: Mode, label: &str) {
    let workers: usize = std::env::var("RC1_STRESS_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_WORKERS);
    let iterations: usize = std::env::var("RC1_STRESS_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    assert!(workers > 0 && iterations > 0);

    let handles: Vec<_> = (0..workers)
        .map(|_| std::thread::spawn(move || worker(iterations, mode)))
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
    let clean_p50 = percentile(&mut report.clean_wall_us, 50);
    let clean_p99 = percentile(&mut report.clean_wall_us, 99);
    let clean_max = report.clean_wall_us.last().copied().unwrap_or(0);
    let park_p50 = percentile(&mut report.parked_wall_us, 50);
    let park_max = report.parked_wall_us.last().copied().unwrap_or(0);
    println!(
        "rc1-parking[{label}]: workers={workers} iterations={iterations} \
         dispatches={} parks={} rate={rate:.1}% resume_ticks={} | \
         clean_wall_us p50={clean_p50} p99={clean_p99} max={clean_max} | \
         parked_wall_us p50={park_p50} max={park_max}",
        report.dispatches, report.parks, report.resume_ticks,
    );
    assert!(
        report.failures.is_empty(),
        "rc1-parking[{label}]: {} of {} dispatches failed: {:?}",
        report.failures.len(),
        report.dispatches,
        &report.failures[..report.failures.len().min(5)]
    );
    assert!(report.dispatches > 0);
}

/// Pre-fix reproduction: production budgets, one dispatch, no settlement.
///
/// Fails on *any* park or host-call timeout, which is exactly what the probes
/// did before bitty#2781. Run it under load to measure the rate.
#[test]
#[ignore = "load harness; run via scripts/rc1-parking-stress.sh"]
fn load_harness_raw_dispatch_fails_on_any_park() {
    run_harness(Mode::Raw, "raw");
}

/// Post-fix shape: the same probe settles the documented cold-tick resume and
/// uses the harness host-call deadline, so it passes under the same load.
#[test]
#[ignore = "load harness; run via scripts/rc1-parking-stress.sh"]
fn load_harness_settled_dispatch_survives_the_load() {
    run_harness(Mode::Settled, "settled");
}
