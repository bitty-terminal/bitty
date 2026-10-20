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
//!    instructions. The spawn wait is now credited per callback
//!    ([`bitty_lua::HOST_CALL_CREDIT_MAX_MS`]); **only** the supervised spawn
//!    is credited, and its own deadline plus the cheap-call deadline are still
//!    checked in raw time.
//! 2. **Quadratic pattern fuel.** `string.gmatch` charged the whole remaining
//!    scan window on every iteration (phodopus `patterns.rs`), so parsing the
//!    8 KiB-capped `git branch -a` output burned ~10.05M instructions for
//!    ~40 bytes of work per line. gmatch/gsub now charge each scan position
//!    once, keeping the scan linear.
//!
//! Both directions are pinned here: the bounded workload completes, a
//! genuinely runaway script still trips the instruction budget fail-closed,
//! and Lua executed through `commands.invoke`/`services.*` dispatch earns no
//! host-call credit (the guard that wraps those calls must stay uncredited).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BridgeError, ExecuteOutcome, HOST_CALL_CREDIT_MAX_MS, HostServices, InvocationOutcome,
    LuaValue, LuaVm, MarshallingLimits, RC1_CALLBACK_CEILING_MS, ServiceRoute, SuspendReason,
    VmError,
};

/// One real `git` invocation measured ~35 ms on the reporting host.
const SPAWN_WAIT_MS: u64 = 35;

/// Capped tool output the git panel parses (`MAX_SPAWN_OUTPUT_BYTES`).
const TOOL_OUTPUT_BYTES: usize = 8 * 1024;

/// One git-panel-shaped listing line (38 bytes).
const TOOL_LINE: &str = "  remotes/origin/feature/branch-00042\n";

/// Fuel ceiling for the bounded spawn-bearing workload. The measured cost is
/// well under 1M; the ceiling catches a return of the ~10M quadratic charge
/// with an order of magnitude of headroom.
const BOUNDED_WORKLOAD_FUEL_CEILING: u64 = 1_000_000;

/// Scheduling slack between the measured host call and the budget reading.
const CHARGE_TOLERANCE_MS: u64 = 5;

/// Lower bound for the 8 KiB gmatch scan: the first search must prepay the
/// scanned window and its attempt bound, plus one attempt per later match.
const GMATCH_MIN_FUEL: u64 = 100_000;

/// Lower bound for 2,000 single-byte gsub matches: the prepaid attempt bound
/// plus one Fuel per output byte.
const GSUB_MIN_FUEL: u64 = 20_000;

/// Host stub that blocks like a real tool spawn and a real callee/provider
/// dispatch, and records every effect.
struct BlockingSpawn {
    waits: Cell<u32>,
    /// Time the spawn stub actually spent blocked, measured around each sleep.
    waited: Cell<Duration>,
    /// Requested block time for each `process_spawn` call. Defaults to
    /// [`SPAWN_WAIT_MS`]; the #2314 ceiling probe chains much longer waits.
    spawn_delay_ms: u64,
    /// Delay injected into `commands.invoke` / `services.*` dispatch.
    dispatch_delay_ms: u64,
    dispatches: Cell<u32>,
    /// Time the dispatch stub actually spent, measured around each sleep.
    dispatch_waited: Cell<Duration>,
    snapshot_delay_ms: u64,
    store: RefCell<BTreeMap<String, LuaValue>>,
}

impl Default for BlockingSpawn {
    fn default() -> Self {
        Self {
            waits: Cell::new(0),
            waited: Cell::new(Duration::ZERO),
            spawn_delay_ms: SPAWN_WAIT_MS,
            dispatch_delay_ms: 0,
            dispatches: Cell::new(0),
            dispatch_waited: Cell::new(Duration::ZERO),
            snapshot_delay_ms: 0,
            store: RefCell::new(BTreeMap::new()),
        }
    }
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
        let started = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(self.spawn_delay_ms));
        self.waited.set(self.waited.get() + started.elapsed());
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

    /// A callee dispatch: this stands in for `PluginRuntime::dispatch_command`,
    /// which runs the callee's Lua. Slow dispatch is Lua time, not host I/O.
    fn command_invoke(&self, _qualified: &str, _args: &LuaValue) -> Result<LuaValue, BridgeError> {
        self.dispatches.set(self.dispatches.get() + 1);
        let started = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(self.dispatch_delay_ms));
        self.dispatch_waited
            .set(self.dispatch_waited.get() + started.elapsed());
        Ok(LuaValue::Bool(true))
    }

    /// A provider route: `service_call` below is its `vm.call_function`.
    fn service_resolve(
        &self,
        iface: &str,
        _req: Option<&str>,
        _optional: bool,
    ) -> Result<Option<ServiceRoute>, BridgeError> {
        Ok(Some(ServiceRoute {
            provider: "acme.provider".to_string(),
            generation: 1,
            iface: iface.to_string(),
            version: "1.0".to_string(),
            methods: vec!["ping".to_string()],
        }))
    }

    fn service_call(
        &self,
        _provider: &str,
        _generation: u32,
        _iface: &str,
        _version: &str,
        _method: &str,
        _args: &LuaValue,
    ) -> Result<LuaValue, BridgeError> {
        self.dispatches.set(self.dispatches.get() + 1);
        let started = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(self.dispatch_delay_ms));
        self.dispatch_waited
            .set(self.dispatch_waited.get() + started.elapsed());
        Ok(LuaValue::Bool(true))
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

/// VM for fuel-charge proofs: the wall budget is generous on purpose, because
/// the property under test is the fuel model, not runner speed (unoptimized CI
/// builds execute these scans far slower than release builds).
fn fuel_vm() -> (LuaVm, Rc<BlockingSpawn>) {
    let mut vm = build_plugin_vm(
        "xuepoo.fuel-2285",
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
    (vm, services)
}

fn run_fuel(vm: &mut LuaVm, source: &str) -> u64 {
    match vm.execute(source).expect("execute") {
        ExecuteOutcome::Completed {
            instructions_used, ..
        } => instructions_used,
        other => panic!("bounded pattern workload must not suspend RC-1, got {other:?}"),
    }
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
    // Assert the accounting contract against the wait the host *actually*
    // spent, not the requested 35 ms: runner sleeps overshoot (the macOS
    // runner turned two 35 ms waits into ~300 ms). What matters is that every
    // bounded spawn wait is credited up to the cap, so only Lua time is
    // charged.
    let waited_ms = u64::try_from(services.waited.get().as_millis()).unwrap_or(u64::MAX);
    let credited_ms = snapshot.host_call_credit_ms;
    let charged_lua_ms = snapshot
        .wall_elapsed_ms
        .saturating_sub(snapshot.store_commit_credit_ms)
        .saturating_sub(credited_ms);
    let over_cap_ms = waited_ms.saturating_sub(HOST_CALL_CREDIT_MAX_MS);

    assert!(waited_ms >= 2 * SPAWN_WAIT_MS, "both waits must block");
    assert!(
        credited_ms + CHARGE_TOLERANCE_MS >= waited_ms.min(HOST_CALL_CREDIT_MAX_MS),
        "spawn-wait credit {credited_ms} ms must cover the measured {waited_ms} ms wait \
         up to the {HOST_CALL_CREDIT_MAX_MS} ms cap"
    );
    assert!(
        charged_lua_ms <= over_cap_ms + CHARGE_TOLERANCE_MS,
        "only host time past the credit cap may be charged: charged {charged_lua_ms} ms, \
         measured {waited_ms} ms, cap {HOST_CALL_CREDIT_MAX_MS} ms"
    );
    // Two bounded spawn waits are far below the supervised-call deadline, so
    // they must never consume the Lua wall budget. This is the assertion that
    // caught the old 200 ms cap on a loaded macOS runner (two 35 ms waits took
    // ~300 ms and left 102 ms charged).
    assert!(
        charged_lua_ms < snapshot.wall_budget_ms,
        "bounded spawn waits must not consume the wall budget, got {charged_lua_ms} ms \
         (raw {} ms, credit {credited_ms} ms, measured wait {waited_ms} ms)",
        snapshot.wall_elapsed_ms
    );
}

#[test]
fn lua_dispatch_stays_charged_and_earns_no_host_call_credit() {
    // `bitty.commands.invoke` and `bitty.services.*` method dispatch run
    // callee/provider Lua through `vm.call_function`; the generic bridge guard
    // that wraps them must never be credited as host I/O (bitty#2285 review).
    //
    // The property is asserted from the budget ledger and the host-measured
    // dispatch time, not from the callback outcome: on a loaded runner a
    // single 10 ms dispatch can overshoot the bridge's 50 ms cheap-call
    // deadline and fail with `E_TIMEOUT`, which says nothing about credit.
    let (mut vm, services) = vm_with(BlockingSpawn {
        dispatch_delay_ms: 10,
        ..BlockingSpawn::default()
    });
    let outcome = vm
        .execute(
            r#"
            for _ = 1, 2 do pcall(bitty.commands.invoke, "acme.other:work") end
            local svc = bitty.services.get("acme.iface")
            pcall(svc.ping)
            pcall(svc.ping)
        "#,
        )
        .expect("execute");
    // Outcome-agnostic on purpose: a slow runner may overshoot the 10 ms
    // dispatch into the bridge's 50 ms cheap-call deadline (contained by
    // `pcall`) or suspend for wall time that is correctly charged. Neither
    // changes the credit property.
    let _ = outcome;
    assert!(
        services.dispatches.get() >= 1,
        "the dispatch must have run before the outcome resolved"
    );

    let snapshot = vm.budget_snapshot();
    let waited_ms = u64::try_from(services.dispatch_waited.get().as_millis()).unwrap_or(u64::MAX);
    assert_eq!(
        snapshot.host_call_credit_ms, 0,
        "callee/provider Lua dispatch must never earn host-call credit"
    );
    // And the dispatch time is charged, not hidden: with zero credit the
    // charged wall time must account for what the host measured.
    let charged_ms = snapshot
        .wall_elapsed_ms
        .saturating_sub(snapshot.store_commit_credit_ms)
        .saturating_sub(snapshot.host_call_credit_ms);
    assert!(
        charged_ms + CHARGE_TOLERANCE_MS >= waited_ms,
        "dispatch time must be charged: charged {charged_ms} ms, measured {waited_ms} ms"
    );
}

#[test]
fn chained_lua_dispatch_cannot_bank_credit() {
    // A tight loop of sub-deadline dispatches is the abuse the review flagged:
    // if the generic guard were credited, this would bank host-call credit
    // instead of spending the Lua wall budget.
    let (mut vm, services) = vm_with(BlockingSpawn {
        dispatch_delay_ms: 5,
        ..BlockingSpawn::default()
    });
    let outcome = vm
        .execute(r#"for _ = 1, 20 do pcall(bitty.commands.invoke, "acme.other:work") end"#)
        .expect("execute");

    let snapshot = vm.budget_snapshot();
    let waited_ms = u64::try_from(services.dispatch_waited.get().as_millis()).unwrap_or(u64::MAX);
    assert!(
        services.dispatches.get() >= 1,
        "the loop must have dispatched before the outcome resolved"
    );
    assert_eq!(
        snapshot.host_call_credit_ms, 0,
        "chained Lua dispatch must never bank host-call credit, got {outcome:?}"
    );
    let charged_ms = snapshot
        .wall_elapsed_ms
        .saturating_sub(snapshot.store_commit_credit_ms)
        .saturating_sub(snapshot.host_call_credit_ms);
    assert!(
        charged_ms + CHARGE_TOLERANCE_MS >= waited_ms,
        "chained dispatch time must be charged: charged {charged_ms} ms, measured {waited_ms} ms"
    );
}

#[test]
fn gmatch_charges_each_scan_position_once() {
    let (mut vm, services) = fuel_vm();

    // gmatch only: one pass over the listing, no per-line pattern work, so the
    // measured charge is the iterator's own scan cost.
    let lines = |bytes: usize| bytes / TOOL_LINE.len();
    let parse = |vm: &mut LuaVm, lines: usize| -> u64 {
        run_fuel(
            vm,
            &format!(
                r#"
                local text = string.rep({TOOL_LINE:?}, {lines})
                local count = 0
                for item in string.gmatch(text, "[^\n]+") do
                  count = count + #item
                end
                bitty.store.set("parsed", count)
                "#
            ),
        )
    };

    let small_cost = parse(&mut vm, lines(TOOL_OUTPUT_BYTES / 4));
    let large_cost = parse(&mut vm, lines(TOOL_OUTPUT_BYTES));

    assert!(
        large_cost < BOUNDED_WORKLOAD_FUEL_CEILING,
        "an 8 KiB gmatch scan must stay linear and bounded, used {large_cost}"
    );
    // Lower bound: the scan must actually be charged. The first search prepays
    // the scanned window (1 Fuel/byte) plus its attempt bound (16 Fuel per
    // candidate position), and every later match pays its attempt. Removing
    // either the attempt charge or the whole `fuel.consume` drops the cost by
    // an order of magnitude and must fail here (bitty#2285 review).
    assert!(
        large_cost >= GMATCH_MIN_FUEL,
        "gmatch must charge the scan window and attempt bound, used only {large_cost}"
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
fn gsub_charges_each_scan_position_once() {
    let (mut vm, _services) = fuel_vm();

    let replace = |vm: &mut LuaVm, matches: usize| -> u64 {
        run_fuel(
            vm,
            &format!(
                r#"
                local text = string.rep("a", {matches})
                bitty.store.set("out", #string.gsub(text, "a", "b"))
                "#
            ),
        )
    };

    let small_cost = replace(&mut vm, 1_000);
    let large_cost = replace(&mut vm, 2_000);

    // Lower bound: 16 Fuel per candidate position (prepaid once) plus 1 Fuel
    // per output byte. Dropping the attempt charge or the consume collapses
    // the cost to the `string.rep` charge alone.
    assert!(
        large_cost >= GSUB_MIN_FUEL,
        "gsub must charge the attempt bound per position and the output bytes, used only {large_cost}"
    );
    assert!(
        large_cost <= small_cost.saturating_mul(3),
        "gsub fuel must grow linearly with match count: {small_cost} -> {large_cost}"
    );
    assert!(
        large_cost < BOUNDED_WORKLOAD_FUEL_CEILING,
        "2,000 single-byte matches must stay far below the RC-1 budget, used {large_cost}"
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
    // The spawn credit is not a general relaxation: a read that misses its own
    // 50 ms cheap-call deadline still reports `E_TIMEOUT` to the plugin.
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

/// One app-dispatched command parked at the RC-1 wall budget resumes without
/// re-running its already-completed body (bitty #2314).
#[test]
fn slow_resumable_command_completes_exactly_once() {
    let (mut vm, _services) = vm_with(BlockingSpawn {
        snapshot_delay_ms: 10,
        ..BlockingSpawn::default()
    });
    vm.execute(
        r#"
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
          id = "runs",
          title = "Read run count",
          run = function(_args) return tostring(runs) end,
        })
        "#,
    )
    .expect("init");
    let capture = vm.take_registrations();
    let slow = capture.commands[0].run.clone();
    let runs = capture.commands[1].run.clone();

    match vm.call_function_resumable(&slow, &[]).expect("start") {
        InvocationOutcome::Parked => {}
        other => panic!("80 ms of charged work must park, got {other:?}"),
    }
    assert!(
        vm.is_parked(),
        "the VM must hold exactly one parked callback"
    );
    assert!(!vm.is_suspended(), "a park is not a fail-closed suspension");

    let mut result = None;
    for _ in 0..200 {
        match vm.resume_parked_call().expect("resume") {
            InvocationOutcome::Completed(value) => {
                result = Some(value);
                break;
            }
            InvocationOutcome::Parked => {}
        }
    }
    match result {
        Some(LuaValue::String(value)) => {
            assert_eq!(value, "done:1", "the callback body must run exactly once");
        }
        other => panic!("resumed callback must complete with its result, got {other:?}"),
    }
    assert!(!vm.is_parked());
    let count = vm.call_function(&runs, &[]).expect("read run count");
    assert_eq!(
        count,
        LuaValue::String("1".to_string()),
        "no duplicate side effect on resume"
    );
}

/// Cancelling a park drops the executor and leaves the VM usable; the
/// interrupted callback body must not continue (bitty #2314).
#[test]
fn cancelled_park_never_resumes_or_duplicates_effects() {
    let (mut vm, _services) = vm_with(BlockingSpawn {
        snapshot_delay_ms: 10,
        ..BlockingSpawn::default()
    });
    vm.execute(
        r#"
        local runs = 0
        bitty.commands.register({
          id = "slow",
          title = "Slow command",
          run = function(_args)
            for _ = 1, 20 do
              pcall(bitty.terminal.snapshot, { scope = "semantic" })
              local sum = 0
              for j = 1, 500 do sum = sum + j end
            end
            runs = runs + 1
            return runs
          end,
        })
        bitty.commands.register({
          id = "runs",
          title = "Read run count",
          run = function(_args) return tostring(runs) end,
        })
        "#,
    )
    .expect("init");
    let capture = vm.take_registrations();
    let slow = capture.commands[0].run.clone();
    let runs = capture.commands[1].run.clone();

    assert!(matches!(
        vm.call_function_resumable(&slow, &[]).expect("start"),
        InvocationOutcome::Parked
    ));
    assert!(vm.cancel_parked_call(), "the park must be present");
    assert!(!vm.is_parked());
    assert!(!vm.is_suspended());
    assert!(
        vm.resume_parked_call().is_err(),
        "a cancelled park cannot be resumed"
    );
    let count = vm.call_function(&runs, &[]).expect("read run count");
    assert_eq!(count, LuaValue::String("0".to_string()));
}

/// A callback that chains bounded supervised spawns past the 5250 ms envelope
/// is refused with the documented `WallClockExceeded` suspension (bitty #2314).
#[test]
fn chained_spawns_past_the_callback_ceiling_are_refused() {
    let (mut vm, services) = vm_with(BlockingSpawn {
        spawn_delay_ms: 1_900,
        ..BlockingSpawn::default()
    });
    vm.execute(
        r#"
        bitty.commands.register({
          id = "chain",
          title = "Chain spawns",
          run = function(_args)
            bitty.process.spawn({ "one" })
            bitty.process.spawn({ "two" })
            bitty.process.spawn({ "three" })
            return "chain-done"
          end,
        })
        "#,
    )
    .expect("init");
    let capture = vm.take_registrations();
    let chain = capture.commands[0].run.clone();

    match vm.call_function_resumable(&chain, &[]) {
        Err(VmError::Suspended {
            reason:
                SuspendReason::WallClockExceeded {
                    elapsed_ms,
                    budget_ms,
                },
        }) => {
            assert!(
                elapsed_ms >= RC1_CALLBACK_CEILING_MS,
                "ceiling reason must report cumulative wall time, got {elapsed_ms} ms"
            );
            assert_eq!(
                budget_ms, RC1_CALLBACK_CEILING_MS,
                "the reason must name the callback ceiling"
            );
        }
        other => panic!("chained spawns past the ceiling must be refused, got {other:?}"),
    }
    assert_eq!(services.waits.get(), 3, "the probe must exercise the chain");
    assert!(
        !vm.is_parked(),
        "a ceiling breach is terminal, not resumable"
    );
    assert!(
        vm.is_suspended(),
        "the VM must stay suspended fail-closed after the ceiling"
    );
}

/// The completion ceiling also covers slice-only callbacks (init/config/event/
/// timer and synchronous nested dispatch), which cannot resume (bitty #2314
/// F1). A callback that chains bounded spawns past 5250 ms raw must suspend at
/// completion instead of reporting success.
#[test]
fn slice_only_callback_past_the_ceiling_suspends() {
    let (mut vm, services) = vm_with(BlockingSpawn {
        spawn_delay_ms: 1_900,
        ..BlockingSpawn::default()
    });
    let outcome = vm
        .execute(
            r#"
            bitty.process.spawn({ "one" })
            bitty.process.spawn({ "two" })
            bitty.process.spawn({ "three" })
            bitty.store.set("chain", 1)
            "#,
        )
        .expect("execute");
    match outcome {
        ExecuteOutcome::Suspended {
            reason:
                SuspendReason::WallClockExceeded {
                    elapsed_ms,
                    budget_ms,
                },
            ..
        } => {
            assert!(
                elapsed_ms >= RC1_CALLBACK_CEILING_MS,
                "slice-only ceiling must report cumulative wall time, got {elapsed_ms} ms"
            );
            assert_eq!(
                budget_ms, RC1_CALLBACK_CEILING_MS,
                "the reason must name the callback ceiling"
            );
        }
        other => {
            panic!("slice-only chained spawns past the ceiling must be refused, got {other:?}")
        }
    }
    assert_eq!(services.waits.get(), 3, "the probe must exercise the chain");
    assert!(
        vm.is_suspended(),
        "a slice-only ceiling breach is terminal fail-closed"
    );
}
