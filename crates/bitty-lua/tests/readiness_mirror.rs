#![forbid(unsafe_code)]

//! Readiness mirror for the Phodopus-to-bitty Lua migration (CTX-0600, RUN-29).
//!
//! Each test maps one executable gate item from Phodopus
//! `crates/phodopus/tests/readiness_gate.rs` (pinned at
//! `phodopus@1653c51f7fbda5e93fa99aefb0e5be58dfacfeb0`, see
//! `crates/bitty-lua/Cargo.toml`) to an assertion over the same behavior
//! observed through the bitty seam (`gate::build_plugin_vm`, `LuaVm`,
//! `ExecuteOutcome`/`BoundedExecution`, `VmBudgetSnapshot`, stable `E_*`
//! bridge codes). The mirror re-asserts the mapped property directly so it
//! fails if the behavior regresses; the Phodopus source suites remain the
//! authoritative evidence for the engine internals.
//!
//! Deltas from the engine gate are asserted, not hidden: `os` is narrowed
//! (`time`/`clock`/`date`) rather than absent, wall-clock enforcement goes
//! through the synthetic `execute_with_elapsed` hook plus the structural
//! `check_budgets` helper (real 1 ms timing is sub-tick-flaky), and host-op
//! cancellation surfaces as the typed bridge `E_TIMEOUT` (a parked executor
//! with no registered host future is auto-cancelled fail-closed inside the
//! drive loop). No test asserts on wall-clock durations.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bitty_lua::error::{E_BUDGET_MEMORY, bridge_error_for_suspend};
use bitty_lua::gate::{VmBudgets, build_plugin_vm};
use bitty_lua::{
    BoundedExecution, BridgeError, ExecuteOutcome, HostServices, LuaValue, LuaVm, MAX_CHUNK_BYTES,
    MarshallingLimits, SuspendReason, VmError,
};

/// Build a VM with default RC budgets through the fail-closed gate.
fn gate_vm(id: impl Into<String>) -> LuaVm {
    build_plugin_vm(id, Some(VmBudgets::default())).expect("default budgets are valid")
}

/// Build a VM with explicit budgets through the fail-closed gate.
fn gate_vm_with(
    id: impl Into<String>,
    instruction_budget: u64,
    wall_budget_ms: u64,
    warning_ms: u64,
    memory_limit: usize,
) -> LuaVm {
    build_plugin_vm(
        id,
        Some(VmBudgets {
            instruction_budget,
            wall_budget_ms,
            warning_ms,
            memory_limit,
        }),
    )
    .expect("explicit budgets are valid")
}

fn assert_completed(outcome: ExecuteOutcome, what: &str) {
    assert!(
        matches!(outcome, ExecuteOutcome::Completed { .. }),
        "{what} must complete, got {outcome:?}"
    );
}

// ── shared host stub ────────────────────────────────────────────────────────

/// Minimal host stub: in-memory store/settings plus an injectable settings
/// delay for the cancellation (deadline) mirror.
#[derive(Default)]
struct MirrorServices {
    store: RefCell<BTreeMap<String, LuaValue>>,
    settings_delay_ms: u64,
}

impl HostServices for MirrorServices {
    fn store_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        Ok(self.store.borrow().get(key).cloned())
    }

    fn store_set(&self, key: &str, value: LuaValue) -> Result<(), BridgeError> {
        self.store.borrow_mut().insert(key.to_string(), value);
        Ok(())
    }

    fn settings_get(&self, key: &str) -> Result<Option<LuaValue>, BridgeError> {
        if self.settings_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.settings_delay_ms));
        }
        Ok(self.store.borrow().get(key).cloned())
    }

    fn terminal_snapshot(&self, _scope: &str) -> Result<LuaValue, BridgeError> {
        Err(BridgeError::capability_denied("terminal.semantic-read"))
    }

    fn notify_show(&self, _payload: &LuaValue) -> Result<bool, BridgeError> {
        Err(BridgeError::capability_denied("platform.notify"))
    }
}

fn install(vm: &mut LuaVm, services: Rc<MirrorServices>, deadline_ms: u64) {
    let services_dyn: Rc<dyn HostServices> = services;
    vm.install_host_module(services_dyn, MarshallingLimits::default(), deadline_ms)
        .expect("host module installs");
}

// ── scratch module roots (derived paths only, never hardcoded) ─────────────

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempRoot {
    path: std::path::PathBuf,
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn temp_root(tag: &str) -> TempRoot {
    let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitty-lua-mirror-{tag}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp module root");
    let canonical = std::fs::canonicalize(&dir).expect("canonicalize temp module root");
    TempRoot { path: canonical }
}

// ── RC-1: instruction budget ────────────────────────────────────────────────

/// Gate RC-1 (instruction half): a finite workload past a tiny instruction
/// budget suspends fail-closed with an attributed reason, further execution
/// is refused, and an explicit reset re-grants a working VM.
#[test]
fn rc1_instruction_budget_suspends_and_reset_regrants() {
    let mut vm = gate_vm_with("mirror.rc1-instr", 100, 60_000, 8, 32 * 1024 * 1024);
    let outcome = vm
        .execute("local s = 0 for i = 1, 5000 do s = s + i end assert(s == 12502500)")
        .expect("drive works");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(
                    reason,
                    SuspendReason::InstructionBudgetExceeded { used, budget: 100 }
                    if used >= 100
                ),
                "must name the exhausted budget, got {reason:?}"
            );
        }
        other => panic!("finite work past a 100-instruction budget must suspend, got {other:?}"),
    }
    assert!(vm.is_suspended());
    assert_eq!(vm.suspension_count(), 1);

    // Fail-closed: the suspended VM refuses new work without touching the heap.
    let err = vm
        .execute("return 1")
        .expect_err("suspended VM must refuse");
    assert!(
        matches!(err, VmError::Suspended { .. }),
        "refusal must be typed Suspended, got {err:?}"
    );
    assert_eq!(vm.suspension_count(), 1);

    // Explicit re-grant recovers the same instance.
    vm.reset();
    let outcome = vm.execute("return 1").expect("drive works after reset");
    assert_completed(outcome, "post-reset execution");
    assert!(!vm.is_suspended());
    assert_eq!(vm.suspension_count(), 1);
}

/// Gate RC-1 (wall-clock half): an already-expired synthetic elapsed
/// suspends deterministically through `execute_with_elapsed`, and the
/// structural `check_budgets` helper pins the warning/suspend thresholds
/// without measuring real time.
#[test]
fn rc1_wall_budget_suspends_through_synthetic_hook() {
    let mut vm = gate_vm("mirror.rc1-wall");
    let outcome = vm
        .execute_with_elapsed("return 1", Duration::from_millis(50))
        .expect("synthetic drive works");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(
                    reason,
                    SuspendReason::WallClockExceeded {
                        elapsed_ms: 50,
                        budget_ms: 50,
                    }
                ),
                "must attribute the wall exceed, got {reason:?}"
            );
        }
        other => panic!("expired synthetic elapsed must suspend, got {other:?}"),
    }
    assert!(vm.is_suspended());

    // Structural thresholds: warning at 8 ms without suspend, hard limit at
    // 50 ms, memory dimension independent of the clock.
    let fresh = gate_vm("mirror.rc1-wall-structural");
    let (suspend, warning) = fresh.check_budgets(8, 100, 1024);
    assert!(suspend.is_none(), "warning threshold must not suspend");
    assert!(warning, "warning threshold must flag");
    let (suspend, warning) = fresh.check_budgets(50, 100, 1024);
    assert!(
        matches!(suspend, Some(SuspendReason::WallClockExceeded { .. })),
        "hard limit must suspend"
    );
    assert!(warning);
}

// ── RC-2: memory quota ──────────────────────────────────────────────────────

/// Gate RC-2: allocation past a tiny heap ceiling is refused fail-closed and
/// attributed to the quota. The refusal surfaces either as a host-polled
/// suspension (stable `E_BUDGET_MEMORY` bridge code, matching limit) or as a
/// builder hard-quota runtime error without suspending; it never completes.
#[test]
fn rc2_memory_quota_refuses_over_ceiling_allocation() {
    const LIMIT: usize = 64 * 1024;
    let mut vm = gate_vm_with("mirror.rc2", 10_000_000, 60_000, 8, LIMIT);
    // Distinct elements: identical content would intern to one shared string
    // and never trip enforcement.
    let outcome = vm
        .execute(r#"local t = {} for i = 1, 1000 do t[i] = string.rep(tostring(i) .. "abcdefgh", 16) end"#)
        .expect("drive works");
    match outcome {
        ExecuteOutcome::Suspended { reason, .. } => {
            assert!(
                matches!(reason, SuspendReason::MemoryExceeded { limit: LIMIT, .. }),
                "suspension must name the quota, got {reason:?}"
            );
            let bridge = bridge_error_for_suspend(&reason);
            assert_eq!(bridge.code, E_BUDGET_MEMORY);
            assert!(vm.is_suspended());
            let snapshot = vm.budget_snapshot();
            assert!(snapshot.suspended);
            assert!(snapshot.memory_used > 0);
        }
        ExecuteOutcome::RuntimeError { message } => {
            assert!(!message.is_empty(), "quota error must carry a message");
            assert!(!vm.is_suspended(), "hard-quota refusal must not suspend");
        }
        ExecuteOutcome::Completed { .. } => {
            panic!("a ~200 KiB allocation under a 64 KiB ceiling must never complete");
        }
    }
}

// ── FS-1: denial leaves no partial state ────────────────────────────────────

/// Gate FS-1: a refused chunk commits nothing (typed `Budget` error, no
/// suspension, instance immediately reusable), and a failing `require`
/// loader leaves no cached sentinel behind (repeat requires keep failing,
/// healthy modules still resolve).
#[test]
fn fs1_denial_leaves_no_partial_state() {
    // Oversized chunk: refused before touching the VM.
    let mut vm = gate_vm("mirror.fs1-chunk");
    let oversized = "x".repeat(MAX_CHUNK_BYTES + 1);
    let err = vm
        .execute(&oversized)
        .expect_err("oversized chunk must be refused");
    assert!(
        matches!(err, VmError::Budget(_)),
        "refusal must be a typed Budget error, got {err:?}"
    );
    assert!(!vm.is_suspended());
    assert_eq!(vm.suspension_count(), 0);
    let outcome = vm.execute("return 1").expect("drive works");
    assert_completed(outcome, "post-refusal execution");

    // Failing loader: no sentinel cached, healthy modules unaffected.
    let root = temp_root("fs1");
    std::fs::write(root.path.join("boom.lua"), "error('loader exploded')")
        .expect("write failing module");
    std::fs::write(root.path.join("safe.lua"), "return 'safe'").expect("write healthy module");
    let mut vm = gate_vm("mirror.fs1-require");
    vm.with_module_root(root.path.clone());
    install(&mut vm, Rc::new(MirrorServices::default()), 50);
    let outcome = vm
        .execute(
            r#"local ok1 = pcall(require, "boom")
            local ok2 = pcall(require, "boom")
            assert(ok1 == false and ok2 == false)
            assert(require("safe") == "safe")"#,
        )
        .expect("drive works");
    assert_completed(
        outcome,
        "failed requires must stay failed without poisoning the cache",
    );
    assert!(!vm.is_suspended());
}

// ── FS-3: fault containment ─────────────────────────────────────────────────

/// Gate FS-3: a fault suspends only its owning VM. The faulted instance
/// refuses further work, a sibling instance is unaffected, and an explicit
/// reset recovers the faulted instance.
#[test]
fn fs3_fault_is_contained_to_owning_vm() {
    let mut faulted = gate_vm_with("mirror.fs3-faulted", 100, 60_000, 8, 32 * 1024 * 1024);
    let outcome = faulted
        .execute("local s = 0 for i = 1, 5000 do s = s + i end assert(s == 12502500)")
        .expect("drive works");
    assert!(
        matches!(outcome, ExecuteOutcome::Suspended { .. }),
        "faulted VM must suspend, got {outcome:?}"
    );
    let err = faulted.execute("return 1").expect_err("must refuse");
    assert!(matches!(err, VmError::Suspended { .. }));

    // Sibling instance shares nothing with the fault.
    let mut sibling = gate_vm("mirror.fs3-sibling");
    let outcome = sibling.execute("return 3 + 4").expect("drive works");
    assert_completed(outcome, "sibling execution");
    assert!(!sibling.is_suspended());

    // The faulted instance recovers through the explicit re-grant.
    faulted.reset();
    let outcome = faulted.execute("return 1 + 1").expect("drive works");
    assert_completed(outcome, "recovered execution");
}

// ── FS-5: recovery keeps the instance usable ────────────────────────────────

/// Gate FS-5: a quota refusal caught by Lua `pcall` leaves the instance
/// usable — the caught chunk completes, and the same VM executes new work
/// without suspension.
#[test]
fn fs5_recovery_keeps_instance_usable() {
    // A single 300 KiB `string.rep` under a 64 KiB ceiling is refused by the
    // builder hard quota as a catchable Lua error (never a suspension).
    let mut vm = gate_vm_with("mirror.fs5", 10_000_000, 60_000, 8, 64 * 1024);
    let outcome = vm
        .execute(
            r#"local ok = pcall(string.rep, "a", 300 * 1024)
            assert(ok == false)
            recovery_flag = "still-works""#,
        )
        .expect("drive works");
    assert_completed(outcome, "pcall-caught quota refusal");
    assert!(!vm.is_suspended());
    assert_eq!(vm.suspension_count(), 0);

    let outcome = vm.execute("return 1").expect("drive works");
    assert_completed(outcome, "post-recovery execution");
    assert!(!vm.is_suspended());
}

// ── FS-9: no bypass surface ─────────────────────────────────────────────────

/// Gate FS-9: no Lua-visible probe weakens a configured ceiling. A script
/// that inspects globals and then attempts a 1 MiB allocation under a
/// 64 KiB quota is still refused, and the quota recorded in the snapshot is
/// unchanged by hostile Lua.
#[test]
fn fs9_no_bypass_surface_exists() {
    const LIMIT: usize = 64 * 1024;
    let mut vm = gate_vm_with("mirror.fs9", 10_000_000, 60_000, 8, LIMIT);
    let outcome = vm
        .execute(
            r#"local probe = tostring(_G.BITTY_DEBUG) .. tostring(os) .. tostring(io)
            local ok = pcall(string.rep, "x", 1024 * 1024)
            assert(ok == false, "quota must hold regardless of probing: " .. probe)"#,
        )
        .expect("drive works");
    assert_completed(outcome, "probed-but-refused allocation");
    assert!(!vm.is_suspended());
    let snapshot = vm.budget_snapshot();
    assert_eq!(snapshot.memory_limit, LIMIT);
    assert_eq!(vm.memory_limit(), LIMIT);
}

// ── stdlib allowlist ────────────────────────────────────────────────────────

/// Stdlib allowlist (post-swap baseline): no ambient `io` authority, narrowed
/// `os` (`time`/`clock`/`date` only), traceback-only `debug`, text-only
/// `load`, empty `package.path` with no native loader, and the retained
/// bounded `string`/`table`/`utf8` surface. One Lua-asserted chunk: any
/// allowlist regression turns completion into a runtime error.
#[test]
fn stdlib_allowlist_denies_ambient_authority() {
    let mut vm = gate_vm("mirror.stdlib");
    let outcome = vm
        .execute(
            r#"assert(io == nil and loadfile == nil and dofile == nil)
            assert(type(load) == "function")
            assert(type(require) == "function")
            assert(type(package) == "table" and package.path == ""
                and package.cpath == nil and package.loadlib == nil)
            assert(type(debug) == "table" and type(debug.traceback) == "function")
            local names = {} for k in pairs(debug) do names[#names + 1] = k end
            table.sort(names)
            assert(table.concat(names, ",") == "traceback")
            assert(type(os) == "table")
            assert(type(os.time) == "function" and type(os.clock) == "function"
                and type(os.date) == "function")
            assert(os.execute == nil and os.getenv == nil and os.remove == nil
                and os.tmpname == nil)
            assert(string.byte("AB", 1) == 65)
            assert(string.char(72, 105) == "Hi")
            assert(string.format("%s (%d)", "Hi", 131) == "Hi (131)")
            assert(table.concat({"a", "b", "c"}, ",") == "a,b,c")
            local items = {3, 1, 2} table.sort(items)
            assert(items[1] == 1 and items[2] == 2 and items[3] == 3)
            assert(utf8.len("abc") == 3)
            local _, mode_err = load("return 1", "c", "b")
            assert(mode_err ~= nil)
            local _, sig_err = load("Lua", "c", "t")
            assert(sig_err ~= nil)"#,
        )
        .expect("drive works");
    assert_completed(outcome, "allowlist assertions");
}

// ── module isolation ────────────────────────────────────────────────────────

/// Module isolation: dotted names resolve inside the capability root,
/// traversal and unknown modules fail closed as catchable errors, and the VM
/// stays usable after every refusal.
#[test]
fn module_isolation_denies_escape() {
    let root = temp_root("isolation");
    std::fs::write(root.path.join("safe.lua"), "return 'safe'").expect("write module");
    std::fs::create_dir_all(root.path.join("foo")).expect("create package dir");
    std::fs::write(root.path.join("foo").join("bar.lua"), "return 'bar'")
        .expect("write nested module");

    let mut vm = gate_vm("mirror.isolation");
    vm.with_module_root(root.path.clone());
    install(&mut vm, Rc::new(MirrorServices::default()), 50);

    // Dotted names resolve inside the root.
    let outcome = vm
        .execute(r#"assert(require("safe") == "safe") assert(require("foo.bar") == "bar")"#)
        .expect("drive works");
    assert_completed(outcome, "in-root resolution");

    // Traversal, absolute-style escape, and unknown modules fail closed.
    for probe in [
        r#"local ok = pcall(require, "../outside") assert(ok == false)"#,
        r#"local ok = pcall(require, "foo..bar") assert(ok == false)"#,
        r#"local ok = pcall(require, "absent.module") assert(ok == false)"#,
    ] {
        let outcome = vm.execute(probe).expect("drive works");
        assert_completed(outcome, "refused resolution stays refused");
        assert!(!vm.is_suspended());
    }
}

// ── cancellation ────────────────────────────────────────────────────────────

/// Cancellation (bitty-seam mapping): a host call past its deadline is
/// cancelled fail-closed with the typed bridge `E_TIMEOUT`, catchable
/// through Lua `pcall`, and the VM keeps serving new work afterwards.
/// Stimulus uses a 30 ms service sleep against a 1 ms deadline (wide,
/// non-flaky margin); no assertion measures time.
#[test]
fn cancellation_surfaces_catchable_timeout_and_keeps_vm_usable() {
    let services = Rc::new(MirrorServices {
        settings_delay_ms: 30,
        ..MirrorServices::default()
    });
    let mut vm = gate_vm("mirror.cancel");
    install(&mut vm, services.clone(), 1);

    let outcome = vm
        .execute_bounded(
            r#"local ok, err = pcall(bitty.settings.get, "x")
            assert(ok == false)
            assert(type(err) == "table" and err.code == "E_TIMEOUT")
            bitty.store.set("cancel-code", err.code)"#,
        )
        .expect("drive works");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "cancelled call must unwind through pcall, got {outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("cancel-code"),
        Some(&LuaValue::String("E_TIMEOUT".to_string()))
    );
    assert!(!vm.is_suspended());
    assert_eq!(vm.suspension_count(), 0);

    // The same VM serves fast host calls immediately afterwards.
    let outcome = vm
        .execute_bounded(r#"bitty.store.set("k", "v")"#)
        .expect("drive works");
    assert!(
        matches!(outcome, BoundedExecution::Completed),
        "post-cancel execution must complete, got {outcome:?}"
    );
    assert_eq!(
        services.store.borrow().get("k"),
        Some(&LuaValue::String("v".to_string()))
    );
    assert!(!vm.is_suspended());
}
