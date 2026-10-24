//! Per-plugin execution budgets (bitty#2304).
//!
//! Pins the tighten-only contract end to end: manifest `[limits]` values and
//! the user `plugin-budgets.lua` layers reach the production `VmBudgets`
//! construction, absent keys keep the compiled host defaults, no layer can
//! widen the compiled ceiling, `--safe` ignores both tightening layers, and a
//! tighter fuel/wall/memory bound is actually observed by the VM.
//!
//! Headless and Windows-safe: temp directories only, no sockets or processes.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{ExecuteOutcome, SuspendReason};
use bitty_plugin_host::budgets::{
    BudgetCeilings, BudgetSource, BudgetValues, EffectiveBudgets, HOST_FUEL_INSTRUCTIONS_CEILING,
    HOST_MEMORY_BYTES_CEILING, HOST_WALL_MS_CEILING, HOST_WARNING_MS_CEILING, PluginBudgetFile,
};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    BridgeError, EmptySettings, LifecycleState, LuaValue, PluginRuntime, PluginRuntimeConfig,
    SnapshotSource,
};

const PLUGIN_ID: &str = "xuepoo.budget";

struct NoSnapshot;

impl SnapshotSource for NoSnapshot {
    fn snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-budgets-{tag}-{}-{counter}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write one dev package with an explicit `[limits]` body and a `spin`
/// command that never finishes on its own.
fn write_plugin(root: &Path, limits_toml: &str) -> PathBuf {
    let plugin = root.join(PLUGIN_ID);
    std::fs::create_dir_all(plugin.join("lua")).expect("dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            r#"[plugin]
id = "{PLUGIN_ID}"
name = "Budget probe"
version = "0.1.0"
description = "test"

[compat]
plugin-api = "^1.0"

[lazy]
commands = ["{PLUGIN_ID}:spin"]
events = []
{limits_toml}"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        r#"
        bitty.commands.register({
          id = "spin",
          title = "Spin forever",
          run = function()
            local n = 0
            while true do n = n + 1 end
          end,
        })
        return {}
        "#,
    )
    .expect("init");
    plugin
}

fn runtime(root: &Path, safe_mode: bool, bundled: bool) -> PluginRuntime {
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        safe_mode,
        data_dir: None,
        store_root: None,
        bundled_roots: if bundled {
            vec![root.to_path_buf()]
        } else {
            Vec::new()
        },
        third_party_roots: if bundled {
            Vec::new()
        } else {
            vec![root.to_path_buf()]
        },
        settings: Rc::new(EmptySettings),
        snapshot: Rc::new(NoSnapshot),
    });
    rt.discover();
    rt
}

fn pin_id() -> PluginId {
    PluginId::new(PLUGIN_ID).expect("id")
}

/// Drain a command that may park at the wall slice until it reaches a
/// terminal decision (mirrors the production cold tick).
fn terminal_error(
    rt: &mut PluginRuntime,
    first: bitty_runtime::plugin_runtime::PluginRuntimeError,
) -> String {
    if !first.is_parked() {
        return first.to_string();
    }
    for _ in 0..200 {
        let mut completions = rt.pump_parked_commands();
        if let Some(completion) = completions.pop() {
            return match completion.outcome {
                Err(error) => error.to_string(),
                Ok(value) => panic!("a runaway command must never return {value:?}"),
            };
        }
    }
    panic!("parked command never reached a terminal decision");
}

#[test]
fn host_ceilings_match_the_vm_defaults() {
    let defaults = VmBudgets::default();
    assert_eq!(
        BudgetCeilings::HOST.fuel_instructions,
        defaults.instruction_budget
    );
    assert_eq!(BudgetCeilings::HOST.wall_ms, defaults.wall_budget_ms);
    assert_eq!(BudgetCeilings::HOST.memory_bytes, defaults.memory_limit);
    assert_eq!(BudgetCeilings::HOST.warning_ms, defaults.warning_ms);
}

#[test]
fn manifest_limits_reach_the_production_vm() {
    let root = temp_dir("manifest");
    write_plugin(
        &root,
        "[limits]\nfuel_instructions = 200000\nwall_ms = 40\nmemory_bytes = 16777216\nwarning_ms = 4\n",
    );
    let mut rt = runtime(&root, false, false);
    let id = pin_id();
    rt.activate(&id).expect("tight activation");

    let budgets = *rt.effective_budgets(&id).expect("budgets recorded");
    assert_eq!(budgets.fuel_instructions, 200_000);
    assert_eq!(budgets.wall_ms, 40);
    assert_eq!(budgets.memory_bytes, 16 * 1024 * 1024);
    assert_eq!(budgets.warning_ms, 4);
    assert_eq!(budgets.sources.fuel_instructions, BudgetSource::Manifest);

    // The VM observes the tighter fuel: the infinite loop is stopped long
    // before the 10M default (a wall park is drained to its terminal fuel
    // decision).
    let first = rt
        .invoke_command(&format!("{PLUGIN_ID}:spin"), &LuaValue::Table(Vec::new()))
        .expect_err("runaway must be stopped");
    let rendered = terminal_error(&mut rt, first);
    assert!(
        rendered.contains("InstructionBudgetExceeded"),
        "tight fuel must be the terminal bound: {rendered}"
    );
}

#[test]
fn absent_limits_keep_the_host_defaults() {
    let root = temp_dir("absent");
    write_plugin(&root, "");
    let mut rt = runtime(&root, false, false);
    let id = pin_id();
    rt.activate(&id).expect("default activation");
    let budgets = *rt.effective_budgets(&id).expect("budgets recorded");
    assert_eq!(
        budgets,
        EffectiveBudgets::host_defaults(BudgetCeilings::HOST)
    );
    assert_eq!(budgets.fuel_instructions, HOST_FUEL_INSTRUCTIONS_CEILING);
    assert_eq!(budgets.wall_ms, HOST_WALL_MS_CEILING);
    assert_eq!(budgets.memory_bytes, HOST_MEMORY_BYTES_CEILING);
    assert_eq!(budgets.warning_ms, HOST_WARNING_MS_CEILING);
    assert_eq!(budgets.sources.wall_ms, BudgetSource::HostDefault);
}

#[test]
fn user_layers_tighten_and_cannot_widen() {
    let root = temp_dir("user");
    write_plugin(
        &root,
        "[limits]\nfuel_instructions = 200000\nwall_ms = 40\n",
    );
    let mut rt = runtime(&root, false, false);
    let mut file = PluginBudgetFile {
        default: BudgetValues {
            fuel_instructions: Some(100_000),
            ..BudgetValues::default()
        },
        ..PluginBudgetFile::default()
    };
    file.plugins.insert(
        PLUGIN_ID.to_string(),
        BudgetValues {
            // Above the manifest declaration but inside the host ceiling:
            // inert, never widening.
            wall_ms: Some(45),
            ..BudgetValues::default()
        },
    );
    rt.set_budget_overrides(file);
    let id = pin_id();
    rt.activate(&id).expect("user-tightened activation");
    let budgets = *rt.effective_budgets(&id).expect("budgets recorded");
    assert_eq!(budgets.fuel_instructions, 100_000);
    assert_eq!(budgets.sources.fuel_instructions, BudgetSource::UserDefault);
    assert_eq!(budgets.wall_ms, 40, "user wall cannot widen the manifest");
    assert_eq!(budgets.sources.wall_ms, BudgetSource::Manifest);
}

#[test]
fn safe_mode_ignores_manifest_and_user_budgets() {
    let root = temp_dir("safe");
    write_plugin(
        &root,
        "[limits]\nfuel_instructions = 200000\nwall_ms = 10\nmemory_bytes = 8388608\n",
    );
    let mut rt = runtime(&root, true, true);
    let file = PluginBudgetFile {
        default: BudgetValues {
            fuel_instructions: Some(50_000),
            ..BudgetValues::default()
        },
        ..PluginBudgetFile::default()
    };
    rt.set_budget_overrides(file);
    let id = pin_id();
    let report = rt
        .activate(&id)
        .expect("bundled activation under safe mode");
    assert!(!report.skipped_safe_mode);
    assert_eq!(
        *rt.effective_budgets(&id).expect("budgets recorded"),
        EffectiveBudgets::host_defaults(BudgetCeilings::HOST),
        "safe mode runs the host defaults"
    );
}

#[test]
fn rejected_user_file_fails_closed_but_safe_mode_still_works() {
    let root = temp_dir("rejected");
    write_plugin(&root, "");

    let mut normal = runtime(&root, false, false);
    normal.set_budget_file_error("line 1: expected a bare non-negative integer");
    let id = pin_id();
    let error = normal
        .activate(&id)
        .expect_err("a rejected budget file fails activation closed");
    let rendered = error.to_string();
    assert!(rendered.contains("plugin-budgets.lua"), "{rendered}");
    assert_eq!(
        normal.state(&id),
        Some(&LifecycleState::Failed(error.to_string()))
    );

    let mut safe = runtime(&root, true, true);
    safe.set_budget_file_error("line 1: expected a bare non-negative integer");
    safe.activate(&id)
        .expect("safe mode ignores the rejected file and runs host defaults");
    assert_eq!(
        *safe.effective_budgets(&id).expect("budgets recorded"),
        EffectiveBudgets::host_defaults(BudgetCeilings::HOST)
    );
}

#[test]
fn tighter_fuel_suspends_where_the_default_completes() {
    // Deterministic VM-level contrast: 100k iterations fit comfortably in the
    // 10M default fuel but exhaust a 1k tightening. Wall is widened on both
    // sides so fuel is the only dimension that can decide.
    let source = "local n = 0 while n < 100000 do n = n + 1 end return n";
    let mut tight = build_plugin_vm(
        "xuepoo.tight",
        Some(VmBudgets {
            instruction_budget: 1_000,
            wall_budget_ms: 60_000,
            warning_ms: 8,
            memory_limit: HOST_MEMORY_BYTES_CEILING,
        }),
    )
    .expect("tight VM");
    let outcome = tight.execute(source).expect("execute");
    assert!(
        matches!(
            outcome,
            ExecuteOutcome::Suspended { ref reason, .. }
                if matches!(reason, SuspendReason::InstructionBudgetExceeded { budget: 1_000, .. })
        ),
        "tight fuel must suspend: {outcome:?}"
    );

    let mut default = build_plugin_vm(
        "xuepoo.default",
        Some(VmBudgets {
            wall_budget_ms: 60_000,
            ..VmBudgets::default()
        }),
    )
    .expect("default VM");
    let outcome = default.execute(source).expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "default fuel must complete the same loop: {outcome:?}"
    );
}

#[test]
fn tighter_wall_and_memory_are_observed_by_the_vm() {
    let mut wall = build_plugin_vm(
        "xuepoo.wall",
        Some(VmBudgets {
            wall_budget_ms: 1,
            warning_ms: 0,
            ..VmBudgets::default()
        }),
    )
    .expect("wall VM");
    let outcome = wall.execute("while true do end").expect("execute");
    assert!(
        matches!(
            outcome,
            ExecuteOutcome::Suspended { ref reason, .. }
                if matches!(reason, SuspendReason::WallClockExceeded { budget_ms: 1, .. })
        ),
        "tight wall must suspend: {outcome:?}"
    );

    let mut memory = build_plugin_vm(
        "xuepoo.memory",
        Some(VmBudgets {
            memory_limit: 64 * 1024,
            ..VmBudgets::default()
        }),
    )
    .expect("memory VM");
    let outcome = memory
        .execute(
            "local t = {} for i=1,1000 do t[i] = string.rep(tostring(i) .. \"abcdefgh\", 16) end",
        )
        .expect("execute");
    assert!(
        matches!(
            outcome,
            ExecuteOutcome::Suspended { ref reason, .. }
                if matches!(reason, SuspendReason::MemoryExceeded { .. })
        ) || matches!(outcome, ExecuteOutcome::RuntimeError { .. }),
        "tight memory must suspend or fail the allocation: {outcome:?}"
    );
}
