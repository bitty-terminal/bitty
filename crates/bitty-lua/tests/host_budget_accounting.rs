#![forbid(unsafe_code)]

//! RC-1 accounting across the host boundary (bitty#2285).
//!
//! Two independent defects made the bounded git-panel `open` workload suspend
//! on RC-1 while it performed almost no Lua work:
//!
//! 1. **Wall-clock charged host I/O.** The wall budget is a deadline on Lua
//!    execution, but `BudgetMark` measured raw wall time, so the wait inside a
//!    supervised `process.spawn` counted against the 50 ms budget. Two real
//!    `git` invocations (~70 ms) suspended a callback that had used ~140k
//!    instructions. Host-call time is now credited per callback
//!    ([`bitty_lua::HOST_CALL_CREDIT_MAX_MS`]); the spawn's own deadline and
//!    the cheap-call deadline are still checked in raw time.
//! 2. **Quadratic pattern fuel.** `string.gmatch` charged the whole remaining
//!    scan window on every iteration (phodopus `patterns.rs`), so parsing the
//!    8 KiB-capped `git branch -a` output burned ~10.05M instructions for
//!    ~40 bytes of work per line. gmatch/gsub now charge each scan position
//!    once, keeping the scan linear.
//!
//! Both directions are pinned here: the bounded workload completes, and a
//! genuinely runaway script still trips the instruction budget fail-closed.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BridgeError, ExecuteOutcome, HostServices, LuaValue, LuaVm, MarshallingLimits, SuspendReason,
};

/// One real `git` invocation measured ~35 ms on the reporting host.
const SPAWN_WAIT_MS: u64 = 35;

/// Capped tool output the git panel parses (`MAX_SPAWN_OUTPUT_BYTES`).
const TOOL_OUTPUT_BYTES: usize = 8 * 1024;

/// Fuel ceiling for the bounded spawn-bearing workload. The measured cost is
/// well under 1M; the ceiling catches a return of the ~10M quadratic charge
/// with an order of magnitude of headroom.
const BOUNDED_WORKLOAD_FUEL_CEILING: u64 = 1_000_000;

/// Host stub that blocks like a real tool spawn and records effects.
#[derive(Default)]
struct BlockingSpawn {
    waits: Cell<u32>,
    snapshot_delay_ms: u64,
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl BlockingSpawn {
    fn stored_int(&self, key: &str) -> Option<i64> {
        match self.store.borrow().get(key) {
            Some(LuaValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    fn stored_str(&self, key: &str) -> Option<String> {
        match self.store.borrow().get(key) {
            Some(LuaValue::String(value)) => Some(value.clone()),
            _ => None,
        }
    }
}

impl HostServices for BlockingSpawn {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        self.store.borrow_mut().insert(key.to_string(), value);
        Ok(())
    }

    fn settings_get(&self, _key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(None)
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        std::thread::sleep(Duration::from_millis(self.snapshot_delay_ms));
        Ok(LuaValue::table([
            ("title", LuaValue::String("scratch".to_string())),
            ("zones", LuaValue::array(Vec::new())),
        ]))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Ok(true)
    }

    fn process_spawn(&self, args: &[String]) -> Result<LuaValue, BridgeError> {
        self.waits.set(self.waits.get() + 1);
        std::thread::sleep(Duration::from_millis(SPAWN_WAIT_MS));
        let output = tool_output(&args.join(" "));
        Ok(LuaValue::table([
            ("output", LuaValue::String(output)),
            ("stderr", LuaValue::String(String::new())),
            ("truncated", LuaValue::Bool(false)),
            ("exit_code", LuaValue::Integer(0)),
            ("execution_id", LuaValue::Integer(1)),
            ("untrusted", LuaValue::Bool(true)),
        ]))
    }
}

/// An 8 KiB `git branch -a`-shaped listing: one line per branch.
fn tool_output(_argv: &str) -> String {
    let mut text = String::new();
    let mut index = 0usize;
    while text.len() < TOOL_OUTPUT_BYTES {
        if index == 0 {
            text.push_str("* main\n");
        } else {
            text.push_str("  remotes/origin/feature/branch-");
            text.push_str(&format!("{index:05}"));
            text.push('\n');
        }
        index += 1;
    }
    text
}

fn vm_with(services: BlockingSpawn) -> (LuaVm, Rc<BlockingSpawn>) {
    let mut vm =
        build_plugin_vm("xuepoo.accounting-2285", Some(VmBudgets::default())).expect("budgets");
    let services = Rc::new(services);
    let host: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(host, MarshallingLimits::default(), 50)
        .expect("install");
    (vm, services)
}

#[test]
fn blocking_spawn_wait_does_not_consume_the_wall_budget() {
    let (mut vm, services) = vm_with(BlockingSpawn::default());
    // Shaped like git-panel `open`: two real tool invocations whose combined
    // wait (~70 ms) exceeds the 50 ms RC-1 wall budget on its own.
    let outcome = vm
        .execute(
            r#"
            local first = bitty.process.spawn({ "branch", "-a" })
            local second = bitty.process.spawn({ "status", "--porcelain" })
            bitty.store.set("spawned_bytes", #first.output + #second.output)
        "#,
        )
        .expect("execute");

    match outcome {
        ExecuteOutcome::Completed {
            instructions_used,
            wall_elapsed_ms,
            ..
        } => {
            assert_eq!(services.waits.get(), 2, "both spawns must run");
            assert!(
                wall_elapsed_ms >= 2 * SPAWN_WAIT_MS,
                "raw wall must include the blocking host waits, got {wall_elapsed_ms} ms"
            );
            assert!(
                instructions_used < BOUNDED_WORKLOAD_FUEL_CEILING,
                "host waits must not consume instruction budget, used {instructions_used}"
            );
        }
        other => panic!("blocking host waits must not suspend RC-1, got {other:?}"),
    }
    assert!(
        services.stored_int("spawned_bytes").unwrap_or(0) > 0,
        "both spawn results must be delivered to Lua"
    );

    let snapshot = vm.budget_snapshot();
    assert!(
        snapshot.host_call_credit_ms >= 2 * SPAWN_WAIT_MS,
        "host-call credit must cover both waits, got {} ms",
        snapshot.host_call_credit_ms
    );
    let charged_lua_ms = snapshot
        .wall_elapsed_ms
        .saturating_sub(snapshot.store_commit_credit_ms)
        .saturating_sub(snapshot.host_call_credit_ms);
    assert!(
        charged_lua_ms < snapshot.wall_budget_ms,
        "charged Lua time must stay under the wall budget, got {charged_lua_ms} ms"
    );
}

#[test]
fn gmatch_over_bounded_tool_output_stays_linear() {
    // The defect this pins is *fuel*, so the proof is deterministic fuel
    // accounting, not wall time. The wall budget is generous on purpose:
    // unoptimized CI builds execute these ~290k instructions in ~50 ms on
    // Windows, and gating a linearity proof on a wall deadline would measure
    // runner speed instead of the charge model. Wall budgeting is covered by
    // the spawn-wait and slow-call tests above.
    let mut vm = build_plugin_vm(
        "xuepoo.gmatch-2285",
        Some(VmBudgets {
            wall_budget_ms: 60_000,
            ..VmBudgets::default()
        }),
    )
    .expect("budgets");
    let services = Rc::new(BlockingSpawn::default());
    let host: Rc<dyn HostServices> = services.clone();
    vm.install_host_module(host, MarshallingLimits::default(), 50)
        .expect("install");

    // A git-panel-shaped line: leading spaces plus a ref name.
    fn parse(vm: &mut LuaVm, lines: usize) -> u64 {
        let outcome = vm
            .execute(&format!(
                r#"
                local line = "  remotes/origin/feature/branch-00042\n"
                local text = string.rep(line, {lines})
                local count = 0
                for item in string.gmatch(text, "[^\n]+") do
                  local trimmed = string.gsub(string.gsub(item, "^%s+", ""), "%s+$", "")
                  if #trimmed > 0 then
                    count = count + 1
                  end
                end
                bitty.store.set("parsed", count)
                "#
            ))
            .expect("execute");
        match outcome {
            ExecuteOutcome::Completed {
                instructions_used, ..
            } => instructions_used,
            other => panic!("bounded tool-output parse must not suspend RC-1, got {other:?}"),
        }
    }

    // `TOOL_OUTPUT_BYTES` (8 KiB) is the consumer's hard cap, so that is the
    // worst case; a quarter of it is the linear-scaling reference.
    let small_lines = TOOL_OUTPUT_BYTES / 4 / 43;
    let large_lines = TOOL_OUTPUT_BYTES / 43;
    let small_cost = parse(&mut vm, small_lines);
    let large_cost = parse(&mut vm, large_lines);

    assert!(
        large_cost < BOUNDED_WORKLOAD_FUEL_CEILING,
        "an 8 KiB gmatch scan must stay linear and bounded, used {large_cost}"
    );
    // Four times the input costs about four times the fuel. The pre-fix
    // quadratic charge grew ~16x (and suspended at 10M before completing).
    assert!(
        large_cost <= small_cost.saturating_mul(6),
        "gmatch fuel must grow linearly with input: {small_cost} -> {large_cost}"
    );
    assert!(
        services.stored_int("parsed").unwrap_or(0) > 100,
        "the fixture must parse many lines, got {:?}",
        services.stored_int("parsed")
    );
}

#[test]
fn runaway_lua_still_suspends_fail_closed() {
    // A small instruction budget with a generous wall budget makes the control
    // deterministic in debug builds: the instruction dimension trips first, so
    // this pins the instruction budget itself (not a timing race).
    let mut vm = build_plugin_vm(
        "xuepoo.runaway-2285",
        Some(VmBudgets {
            instruction_budget: 100_000,
            wall_budget_ms: 60_000,
            ..VmBudgets::default()
        }),
    )
    .expect("budgets");
    let services: Rc<dyn HostServices> = Rc::new(BlockingSpawn::default());
    vm.install_host_module(services, MarshallingLimits::default(), 50)
        .expect("install");
    let outcome = vm
        .execute("local n = 0 while true do n = n + 1 end")
        .expect("execute");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(reason, SuspendReason::InstructionBudgetExceeded { .. }),
                "runaway Lua must trip the instruction budget, got {reason:?}"
            );
        }
        other => panic!("a `while true` loop must never complete, got {other:?}"),
    }
    assert!(vm.is_suspended(), "the VM must stay suspended fail-closed");
    assert!(
        vm.execute("return 1").is_err(),
        "a suspended VM must refuse further execution"
    );
}

#[test]
fn slow_cheap_host_call_still_fails_closed_with_timeout() {
    // Host-call credit is an RC-1 budget concept, not a relaxation of the
    // cheap-call deadline: a read that misses its own 50 ms deadline still
    // reports `E_TIMEOUT` to the plugin.
    let (mut vm, services) = vm_with(BlockingSpawn {
        snapshot_delay_ms: 80,
        ..BlockingSpawn::default()
    });
    let outcome = vm
        .execute(
            r#"
            local ok, err = pcall(bitty.terminal.snapshot, { scope = "semantic" })
            bitty.store.set("ok", ok and 1 or 0)
            bitty.store.set("code", err and err.code or "NONE")
        "#,
        )
        .expect("execute");
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "the pcall must contain the timeout, got {outcome:?}"
    );
    assert_eq!(services.stored_int("ok"), Some(0));
    assert_eq!(
        services.stored_str("code").as_deref(),
        Some("E_TIMEOUT"),
        "a slow cheap call must still time out"
    );
}
