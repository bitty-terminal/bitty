//! `bitty-lua`: deterministic, bounded piccolo VM for plugin isolation (RC-1/RC-2).
//!
//! # Role in Bitty (no mlua conflict)
//!
//! This crate wraps the pure-Rust [`piccolo`] VM for **per-plugin isolation**
//! and, per DEC-0011, **user configuration evaluation**.
//!
//! - **Plugin VMs:** `piccolo` 0.3.x stackless VM, one instance per
//!   `(PluginId, generation)`, isolated globals/registry/module trees, restricted
//!   stdlib construction (no `io`/`os`/`debug` ambient authority, no raw
//!   metatable access to host objects, no dynamic native loading). Host
//!   privileged work happens only via capability-checked host calls injected as
//!   `Callback`s.
//! - **Config evaluation (this crate, [`config`] module):** user `init.lua` is
//!   evaluated in a fresh [`LuaVm`] with the **same** RC-1/RC-2 budgets as a
//!   plugin — config never executes with more authority than a plugin. The
//!   chunk must return a plain-data table (wezterm-style `return {...}`) that
//!   maps 1:1 to `bitty-config` plan fields; extraction is bounded and the
//!   typed schema remains the validation authority. There is no `mlua`
//!   dependency anywhere, so no `mlua::Lua` / `piccolo::Lua` type conflict.
//!
//! The crate is `std`-only, `#![forbid(unsafe_code)]`, `MSRV 1.85`,
//! `edition = "2024"`, `publish = false` (workspace), headless and
//! deterministic (no window/GPU, no randomness in harness, `Fuel`-bounded).
//!
//! # Budgets (RC-1 / RC-2, OQ-014)
//!
//! | ID  | Dimension                         | Default                              | Enforcement point |
//! |-----|-----------------------------------|--------------------------------------|---------------------|
//! | RC-1 | per-VM instruction budget       | `RC1_INSTRUCTION_BUDGET = 10_000_000` | `Fuel` counter checked before each `Executor::step` slice and inside long chunks; exceed => fail-closed suspend |
//! | RC-1 | per-VM wall-clock budget        | `RC1_WALL_CLOCK_BUDGET_MS = 50 ms`   | `Instant` deadline checked at next instruction boundary; exceed => suspend |
//! | RC-1 | warning threshold               | `RC1_WARNING_MS = 8 ms`              | sets `warning_triggered` flag, counted, does not suspend |
//! | RC-2 | per-VM heap (accounted)         | `RC2_MEMORY_PER_PLUGIN_BYTES = 32 MiB` | `Lua::total_memory()` / `gc_metrics().total_allocation()` checked before/after each slice; exceed => suspend |
//!
//! All budgets are **fail-closed**: once exceeded, the VM transitions to
//! `Suspended` and further `execute` calls are refused with
//! `VmError::Suspended`. Counts are exposed via `VmBudgetSnapshot` which is
//! `BudgetSnapshot`-compatible (same counter methodology as
//! `bitty-plugin-host::BudgetSnapshot`: deterministic, attributed, no silent
//! loss) so the host can aggregate suspensions into its `BudgetSnapshot` /
//! `bitty plugin doctor`.
//!
//! # Determinism
//!
//! Execution is deterministic given identical source and identical budgets: the
//! VM uses a single `Fuel` container initialised to `RC1_INSTRUCTION_BUDGET`
//! and never refills it within one `execute` call, `Lua::core()` stdlib with
//! no I/O (`Lua::empty` + `load_core` without `load_io`), and no wall/GPU
//! timing in the replay path. Wall-clock is measured externally for the
//! enforcement, but the harness also exposes `check_budgets` /
//! `with_budgets` for deterministic synthetic-wall tests that do not depend on
//! scheduling jitter.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use piccolo::{Closure, Executor, ExecutorMode, Fuel, Lua, StashedExecutor};

pub mod config;

pub use config::{ConfigData, ConfigOutcome, FontData, KeymapData, TerminalData, WindowData};

// ── RC budgets (aligned with bitty-plugin-host/src/event.rs) ───────────────

/// RC-1 instruction budget (candidate, OQ-014): `10^7` VM instructions per callback.
///
/// Enforced via [`Fuel`] — one fuel container per `execute` call, never
/// refilled. When `Fuel::should_continue()` becomes false and the executor is
/// still `Normal`, the VM suspends fail-closed.
pub const RC1_INSTRUCTION_BUDGET: u64 = 10_000_000;

/// RC-1 wall-clock budget (candidate, OQ-014): `50 ms` per callback.
pub const RC1_WALL_CLOCK_BUDGET_MS: u64 = 50;

/// RC-1 warning threshold (candidate): `8 ms` — sets `warning_triggered` but
/// does not suspend.
pub const RC1_WARNING_MS: u64 = 8;

/// RC-2 per-plugin memory ceiling (candidate, OQ-014): `32 MiB` accounted
/// allocations per VM.
pub const RC2_MEMORY_PER_PLUGIN_BYTES: usize = 32 * 1024 * 1024;

/// RC-3 aggregate plugin memory (candidate): `512 MiB` for all plugins (host
/// aggregate, not per-VM enforcement here; exposed for harness
/// parameterization).
pub const RC2_MEMORY_AGGREGATE_BYTES: usize = 512 * 1024 * 1024;

/// RC-6 per-plugin FD cap (candidate, exposed for parity): `16`.
pub const RC6_FD_PER_PLUGIN: usize = 16;

// ── errors ───────────────────────────────────────────────────────────────────

/// VM execution error (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmError {
    /// VM is suspended after a previous budget violation; further execution
    /// refused until reset (fail-closed per FS-7).
    Suspended {
        /// Reason for suspension.
        reason: SuspendReason,
    },
    /// Lua load/compile error.
    Load(String),
    /// Lua runtime error (pcall-style).
    Runtime(String),
    /// Budget configuration error.
    Budget(String),
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suspended { reason } => write!(f, "vm suspended: {reason:?}"),
            Self::Load(s) => write!(f, "load error: {s}"),
            Self::Runtime(s) => write!(f, "runtime error: {s}"),
            Self::Budget(s) => write!(f, "budget error: {s}"),
        }
    }
}

impl std::error::Error for VmError {}

// ── suspend reason ───────────────────────────────────────────────────────────

/// Why a VM was suspended (fail-closed, attributed).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SuspendReason {
    /// Instruction budget exceeded.
    InstructionBudgetExceeded {
        /// Instructions consumed (approx, via fuel).
        used: u64,
        /// Budget.
        budget: u64,
    },
    /// Wall-clock deadline exceeded.
    WallClockExceeded {
        /// Elapsed ms.
        elapsed_ms: u64,
        /// Budget ms.
        budget_ms: u64,
    },
    /// Memory ceiling exceeded.
    MemoryExceeded {
        /// Bytes used.
        used: usize,
        /// Limit bytes.
        limit: usize,
    },
}

// ── outcome ──────────────────────────────────────────────────────────────────

/// Outcome of `LuaVm::execute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteOutcome {
    /// Completed without budget violation.
    Completed {
        /// Approx instructions consumed.
        instructions_used: u64,
        /// Wall elapsed ms.
        wall_elapsed_ms: u64,
        /// Memory used after execution.
        memory_used: usize,
        /// Whether warning threshold was hit.
        warning_triggered: bool,
    },
    /// Suspended due to budget exceed (fail-closed).
    Suspended {
        /// Reason.
        reason: SuspendReason,
        /// Instructions consumed up to suspend.
        instructions_used: u64,
        /// Wall elapsed ms.
        wall_elapsed_ms: u64,
        /// Memory used.
        memory_used: usize,
    },
    /// Runtime error (not budget-related).
    RuntimeError {
        /// Message.
        message: String,
    },
}

// ── snapshot (BudgetSnapshot-compatible) ─────────────────────────────────────

/// Headless snapshot of VM budget adherence, compatible with
/// `bitty-plugin-host::BudgetSnapshot` counter methodology.
///
/// All fields are deterministic and derived from live VM state plus the RC
/// limits, so the snapshot needs no window/GPU. Use in harness
/// `tests/measurement_lua.rs` and `bitty plugin doctor` diagnostics. The host
/// can aggregate `suspension_count` / `warning_count` into its own
/// `BudgetSnapshot` (e.g. `total_dropped` analogue) without type coupling.
#[derive(Debug, Clone, PartialEq)]
pub struct VmBudgetSnapshot {
    /// Instruction budget (`RC1_INSTRUCTION_BUDGET`).
    pub instruction_budget: u64,
    /// Wall budget ms (`RC1_WALL_CLOCK_BUDGET_MS`).
    pub wall_budget_ms: u64,
    /// Warning ms (`RC1_WARNING_MS`).
    pub warning_ms: u64,
    /// Memory limit bytes (`RC2_MEMORY_PER_PLUGIN_BYTES`).
    pub memory_limit: usize,
    /// Approx instructions consumed in last execute (or current).
    pub instructions_used: u64,
    /// Wall elapsed ms of last execute.
    pub wall_elapsed_ms: u64,
    /// Memory used (total_allocation).
    pub memory_used: usize,
    /// Whether warning threshold was triggered.
    pub warning_triggered: bool,
    /// Whether VM is suspended.
    pub suspended: bool,
    /// Suspend reason if suspended.
    pub suspend_reason: Option<SuspendReason>,
    /// Total suspensions since creation or last reset.
    pub suspension_count: u64,
    /// Total warnings since creation.
    pub warning_count: u64,
    /// Instruction utilization `0.0..1.0+`.
    pub instruction_utilization: f64,
    /// Wall utilization.
    pub wall_utilization: f64,
    /// Memory utilization.
    pub memory_utilization: f64,
}

impl VmBudgetSnapshot {
    /// Whether budgets hold (not suspended).
    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        !self.suspended
    }
}

/// Shared result of [`LuaVm::drive_chunk`]: either a terminal budget
/// suspension, a load failure, or a finished executor awaiting result
/// extraction by the caller (`execute` vs `eval_config`).
///
/// Suspension bookkeeping (`status`, `suspension_count`, per-run metrics) is
/// already applied before returning `Suspended`; callers only translate.
#[derive(Debug)]
pub(crate) enum DriveOutcome {
    /// Budget exceeded; VM is now suspended (fail-closed).
    Suspended {
        /// Reason for suspension.
        reason: SuspendReason,
        /// Instructions consumed up to suspend.
        instructions_used: u64,
        /// Wall elapsed ms.
        wall_elapsed_ms: u64,
        /// Memory used.
        memory_used: usize,
    },
    /// Chunk failed to load/compile (message only, no source echo).
    Failed {
        /// Load error message.
        message: String,
    },
    /// Chunk finished within budgets; executor awaits mode inspection plus
    /// result extraction.
    Ready {
        /// Rooted executor handle for `take_result`.
        stashed: StashedExecutor,
    },
}

// ── VM ───────────────────────────────────────────────────────────────────────

/// Deterministic, bounded piccolo VM for one plugin identity+generation.
///
/// One instance per `(PluginId, generation)`, isolated globals/registry. Host
/// constructs the restricted stdlib via `Lua::core()` (no I/O). Budgets are
/// enforced at instruction, wall, and memory dimensions; exceed => fail-closed
/// suspend, counted. No window/GPU coupling.
pub struct LuaVm {
    id: String,
    lua: Lua,
    status: VmStatus,
    instruction_budget: u64,
    wall_budget_ms: u64,
    warning_ms: u64,
    memory_limit: usize,
    // metrics
    instructions_used: u64,
    wall_elapsed_ms: u64,
    memory_used: usize,
    warning_triggered: bool,
    warning_count: u64,
    suspension_count: u64,
    total_executions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VmStatus {
    Ready,
    Suspended(SuspendReason),
}

impl LuaVm {
    /// Create a new VM for `id` with default RC budgets.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_budgets(
            id,
            RC1_INSTRUCTION_BUDGET,
            RC1_WALL_CLOCK_BUDGET_MS,
            RC1_WARNING_MS,
            RC2_MEMORY_PER_PLUGIN_BYTES,
        )
    }

    /// Create with explicit budgets (for tests / tuning).
    pub fn with_budgets(
        id: impl Into<String>,
        instruction_budget: u64,
        wall_budget_ms: u64,
        warning_ms: u64,
        memory_limit: usize,
    ) -> Self {
        assert!(instruction_budget > 0, "instruction budget must be > 0");
        assert!(wall_budget_ms > 0, "wall budget must be > 0");
        assert!(memory_limit > 0, "memory limit must be > 0");
        // Lua::core() loads base, coroutine, math, string, table — no I/O.
        // This matches the restricted stdlib baseline per lua-runtime-rfc:
        // pure computation base only, no `io`/`os.execute`/`debug` ambient authority.
        let lua = Lua::core();
        Self {
            id: id.into(),
            lua,
            status: VmStatus::Ready,
            instruction_budget,
            wall_budget_ms,
            warning_ms,
            memory_limit,
            instructions_used: 0,
            wall_elapsed_ms: 0,
            memory_used: 0,
            warning_triggered: false,
            warning_count: 0,
            suspension_count: 0,
            total_executions: 0,
        }
    }

    /// Plugin id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether suspended (fail-closed).
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        matches!(self.status, VmStatus::Suspended(_))
    }

    /// Suspend reason if suspended.
    #[must_use]
    pub fn suspend_reason(&self) -> Option<&SuspendReason> {
        match &self.status {
            VmStatus::Suspended(r) => Some(r),
            VmStatus::Ready => None,
        }
    }

    /// Total suspensions since creation.
    #[must_use]
    pub fn suspension_count(&self) -> u64 {
        self.suspension_count
    }

    /// Total executions attempted.
    #[must_use]
    pub fn total_executions(&self) -> u64 {
        self.total_executions
    }

    /// Whether warning was triggered in last execution.
    #[must_use]
    pub fn warning_triggered(&self) -> bool {
        self.warning_triggered
    }

    /// Instructions used in last execution (approx).
    #[must_use]
    pub fn instructions_used(&self) -> u64 {
        self.instructions_used
    }

    /// Wall elapsed ms of last execution.
    #[must_use]
    pub fn wall_elapsed_ms(&self) -> u64 {
        self.wall_elapsed_ms
    }

    /// Memory used (total_allocation) after last execution.
    #[must_use]
    pub fn memory_used(&self) -> usize {
        self.memory_used
    }

    /// Instruction budget for this VM.
    #[must_use]
    pub fn instruction_budget(&self) -> u64 {
        self.instruction_budget
    }

    /// Wall budget ms.
    #[must_use]
    pub fn wall_budget_ms(&self) -> u64 {
        self.wall_budget_ms
    }

    /// Warning ms.
    #[must_use]
    pub fn warning_ms(&self) -> u64 {
        self.warning_ms
    }

    /// Memory limit.
    #[must_use]
    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    /// Reset a suspended VM to `Ready` (explicit re-grant required per FS-2).
    ///
    /// Clears suspension state and metrics for the current generation; does not
    /// reset the Lua heap (a new `Lua` would be required for full reclaim per
    /// FS-5 — caller should drop and recreate for generation N+1).
    pub fn reset(&mut self) {
        self.status = VmStatus::Ready;
        self.warning_triggered = false;
        // Keep suspension_count for attribution, but clear per-execution metrics.
        self.instructions_used = 0;
        self.wall_elapsed_ms = 0;
    }

    /// Replace budgets (for tuning without recreation).
    pub fn set_budgets(
        &mut self,
        instruction_budget: u64,
        wall_budget_ms: u64,
        warning_ms: u64,
        memory_limit: usize,
    ) {
        assert!(instruction_budget > 0);
        assert!(wall_budget_ms > 0);
        assert!(memory_limit > 0);
        self.instruction_budget = instruction_budget;
        self.wall_budget_ms = wall_budget_ms;
        self.warning_ms = warning_ms;
        self.memory_limit = memory_limit;
    }

    /// Deterministic budget check helper — headless, no wall/GPU, no Lua.
    ///
    /// Exposed for deterministic harness tests that synthesize elapsed/memory
    /// without real timing jitter. Returns `Some(SuspendReason)` when any
    /// dimension would suspend, else `None`. Warning is returned via the bool.
    #[must_use]
    pub fn check_budgets(
        &self,
        elapsed_ms: u64,
        instructions_used: u64,
        memory_used: usize,
    ) -> (Option<SuspendReason>, bool) {
        let warning = elapsed_ms >= self.warning_ms;
        if memory_used > self.memory_limit {
            return (
                Some(SuspendReason::MemoryExceeded {
                    used: memory_used,
                    limit: self.memory_limit,
                }),
                warning,
            );
        }
        if elapsed_ms >= self.wall_budget_ms {
            return (
                Some(SuspendReason::WallClockExceeded {
                    elapsed_ms,
                    budget_ms: self.wall_budget_ms,
                }),
                warning,
            );
        }
        if instructions_used >= self.instruction_budget {
            return (
                Some(SuspendReason::InstructionBudgetExceeded {
                    used: instructions_used,
                    budget: self.instruction_budget,
                }),
                warning,
            );
        }
        (None, warning)
    }

    /// Headless budget snapshot (perf counters, deterministic).
    #[must_use]
    pub fn budget_snapshot(&self) -> VmBudgetSnapshot {
        let suspended = self.is_suspended();
        let suspend_reason = self.suspend_reason().cloned();
        let instruction_utilization =
            self.instructions_used as f64 / self.instruction_budget as f64;
        let wall_utilization = self.wall_elapsed_ms as f64 / self.wall_budget_ms as f64;
        let memory_utilization = self.memory_used as f64 / self.memory_limit as f64;
        VmBudgetSnapshot {
            instruction_budget: self.instruction_budget,
            wall_budget_ms: self.wall_budget_ms,
            warning_ms: self.warning_ms,
            memory_limit: self.memory_limit,
            instructions_used: self.instructions_used,
            wall_elapsed_ms: self.wall_elapsed_ms,
            memory_used: self.memory_used,
            warning_triggered: self.warning_triggered,
            suspended,
            suspend_reason,
            suspension_count: self.suspension_count,
            warning_count: self.warning_count,
            instruction_utilization,
            wall_utilization,
            memory_utilization,
        }
    }

    /// Total memory currently used by this Lua instance.
    ///
    /// Equivalent to `gc_metrics().total_allocation()` — counts all `Gc`
    /// allocations plus external allocations tracked by `gc-arena`.
    #[must_use]
    pub fn total_memory(&mut self) -> usize {
        self.lua.total_memory()
    }

    /// Shared budget-enforced chunk driver behind [`LuaVm::execute`] and
    /// [`LuaVm::eval_config`](crate::config::ConfigEval).
    ///
    /// Runs `code` through load plus the RC-1/RC-2 stepping loop with the same
    /// fail-closed suspension bookkeeping in both paths, then hands the
    /// finished executor back for result extraction. Final completion checks
    /// (warning/memory/instruction) run inside, so every `Ready` already
    /// passed them; callers only inspect the executor mode and take results.
    pub(crate) fn drive_chunk(&mut self, code: &str) -> Result<DriveOutcome, VmError> {
        if let VmStatus::Suspended(reason) = &self.status {
            return Err(VmError::Suspended {
                reason: reason.clone(),
            });
        }

        self.total_executions = self.total_executions.wrapping_add(1);
        self.warning_triggered = false;
        self.instructions_used = 0;
        self.wall_elapsed_ms = 0;
        self.memory_used = self.lua.total_memory();

        let start = Instant::now();
        // Fuel is i32; clamp budget to i32::MAX for safety (10M fits).
        let fuel_budget: i32 = i32::try_from(self.instruction_budget).unwrap_or(i32::MAX);
        let mut fuel = Fuel::with(fuel_budget);
        let initial_fuel = fuel.remaining();

        // Load closure — deterministic, no I/O beyond source bytes.
        let code_owned = code.to_string();
        let mut load_error: Option<String> = None;
        let stashed_opt =
            self.lua.enter(
                |ctx| match Closure::load(ctx, None, code_owned.as_bytes()) {
                    Ok(closure) => Some(ctx.stash(Executor::start(ctx, closure.into(), ()))),
                    Err(e) => {
                        // Display (not Debug): "parse error at line N: ..."
                        // with 1-based lines and no internal dump.
                        load_error = Some(format!("{e}"));
                        None
                    }
                },
            );
        let stashed = match (stashed_opt, load_error) {
            (Some(s), None) => s,
            (None, Some(msg)) => {
                return Ok(DriveOutcome::Failed { message: msg });
            }
            _ => {
                return Ok(DriveOutcome::Failed {
                    message: "unknown load error".to_string(),
                });
            }
        };

        // Pre-check memory before stepping (fail-closed per FS-7).
        let mem_before = self.lua.total_memory();
        if mem_before > self.memory_limit {
            let reason = SuspendReason::MemoryExceeded {
                used: mem_before,
                limit: self.memory_limit,
            };
            self.status = VmStatus::Suspended(reason.clone());
            self.suspension_count = self.suspension_count.wrapping_add(1);
            self.memory_used = mem_before;
            self.instructions_used = 0;
            self.wall_elapsed_ms = start.elapsed().as_millis() as u64;
            return Ok(DriveOutcome::Suspended {
                reason,
                instructions_used: 0,
                wall_elapsed_ms: self.wall_elapsed_ms,
                memory_used: mem_before,
            });
        }

        // Stepping loop with budget checks at instruction boundaries.
        loop {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            self.wall_elapsed_ms = elapsed_ms;

            // Warning threshold — counted, not suspending.
            if elapsed_ms >= self.warning_ms && !self.warning_triggered {
                self.warning_triggered = true;
                self.warning_count = self.warning_count.wrapping_add(1);
            }

            // Wall hard limit.
            if elapsed_ms >= self.wall_budget_ms {
                let reason = SuspendReason::WallClockExceeded {
                    elapsed_ms,
                    budget_ms: self.wall_budget_ms,
                };
                self.status = VmStatus::Suspended(reason.clone());
                self.suspension_count = self.suspension_count.wrapping_add(1);
                self.instructions_used = (initial_fuel - fuel.remaining().max(0)) as u64;
                self.memory_used = self.lua.total_memory();
                return Ok(DriveOutcome::Suspended {
                    reason,
                    instructions_used: self.instructions_used,
                    wall_elapsed_ms: elapsed_ms,
                    memory_used: self.memory_used,
                });
            }

            // Memory check before next slice.
            let mem = self.lua.total_memory();
            self.memory_used = mem;
            if mem > self.memory_limit {
                let reason = SuspendReason::MemoryExceeded {
                    used: mem,
                    limit: self.memory_limit,
                };
                self.status = VmStatus::Suspended(reason.clone());
                self.suspension_count = self.suspension_count.wrapping_add(1);
                self.instructions_used = (initial_fuel - fuel.remaining().max(0)) as u64;
                return Ok(DriveOutcome::Suspended {
                    reason,
                    instructions_used: self.instructions_used,
                    wall_elapsed_ms: elapsed_ms,
                    memory_used: mem,
                });
            }

            // Instruction budget via fuel.
            if !fuel.should_continue() {
                let still_normal = self.lua.enter(|ctx| {
                    let exec = ctx.fetch(&stashed);
                    exec.mode() == ExecutorMode::Normal
                });
                if still_normal {
                    let used = (initial_fuel - fuel.remaining().max(0)) as u64;
                    let reason = SuspendReason::InstructionBudgetExceeded {
                        used,
                        budget: self.instruction_budget,
                    };
                    self.status = VmStatus::Suspended(reason.clone());
                    self.suspension_count = self.suspension_count.wrapping_add(1);
                    self.instructions_used = used;
                    self.memory_used = self.lua.total_memory();
                    return Ok(DriveOutcome::Suspended {
                        reason,
                        instructions_used: used,
                        wall_elapsed_ms: elapsed_ms,
                        memory_used: self.memory_used,
                    });
                }
                // Executor is done (Result/Stopped) but fuel exhausted — still done.
                break;
            }

            // Step one slice.
            let done = self.lua.enter(|ctx| {
                let exec = ctx.fetch(&stashed);
                exec.step(ctx, &mut fuel)
            });

            // Check fuel consumption after step.
            // If fuel was consumed to <=0 and executor still Normal, next loop
            // will handle suspension. If done, break.

            if done {
                // Step reports the executor finished (Result/Stopped/yielded).
                // Mode inspection and result extraction belong to the caller
                // (`execute` vs `eval_config`), which run after the shared
                // final completion checks below.
                break;
            }

            // If we consumed a lot of fuel in this slice, loop will detect on
            // next iteration. Avoid spinning forever: if elapsed already large,
            // continue to wall check.
            // Also, to keep wall detection granular, we don't batch too many
            // slices without checking. The executor step already limits to ~64
            // instructions per VM_GRANULARITY plus overhead, so loop is fine.

            // Safety: if total elapsed exceeds wall budget *2, break to avoid
            // infinite loop on pathological fuel handling.
            if elapsed_ms > self.wall_budget_ms.saturating_mul(2) {
                let reason = SuspendReason::WallClockExceeded {
                    elapsed_ms,
                    budget_ms: self.wall_budget_ms,
                };
                self.status = VmStatus::Suspended(reason.clone());
                self.suspension_count = self.suspension_count.wrapping_add(1);
                self.instructions_used = (initial_fuel - fuel.remaining().max(0)) as u64;
                self.memory_used = self.lua.total_memory();
                return Ok(DriveOutcome::Suspended {
                    reason,
                    instructions_used: self.instructions_used,
                    wall_elapsed_ms: elapsed_ms,
                    memory_used: self.memory_used,
                });
            }
        }

        // Completed without budget exceed.
        self.instructions_used = (initial_fuel - fuel.remaining().max(0)) as u64;
        self.wall_elapsed_ms = start.elapsed().as_millis() as u64;
        self.memory_used = self.lua.total_memory();

        // Final wall warning check (if warning threshold hit during successful run).
        if self.wall_elapsed_ms >= self.warning_ms && !self.warning_triggered {
            self.warning_triggered = true;
            self.warning_count = self.warning_count.wrapping_add(1);
        }

        // Final memory check after completion (in case last allocation pushed over).
        if self.memory_used > self.memory_limit {
            let reason = SuspendReason::MemoryExceeded {
                used: self.memory_used,
                limit: self.memory_limit,
            };
            self.status = VmStatus::Suspended(reason.clone());
            self.suspension_count = self.suspension_count.wrapping_add(1);
            return Ok(DriveOutcome::Suspended {
                reason,
                instructions_used: self.instructions_used,
                wall_elapsed_ms: self.wall_elapsed_ms,
                memory_used: self.memory_used,
            });
        }

        // Final instruction check: if we consumed >= budget but still completed
        // exactly at budget, treat as suspended per strict fail-closed (RC-1 is
        // hard limit). However if we completed and used < budget, success.
        if self.instructions_used >= self.instruction_budget {
            let reason = SuspendReason::InstructionBudgetExceeded {
                used: self.instructions_used,
                budget: self.instruction_budget,
            };
            // Only suspend if we actually hit the budget; completed work that
            // exactly used the budget is still considered budget exceed per
            // RC-1 (fail-closed). This makes harness deterministic.
            // But if code completed with used < budget, we already returned success.
            // Here used >= budget implies we exhausted budget even though executor
            // reported done — still suspend to be strict.
            // To avoid false positive on tiny scripts that use << budget, we already
            // checked < budget case earlier, so this is genuine exceed.
            self.status = VmStatus::Suspended(reason.clone());
            self.suspension_count = self.suspension_count.wrapping_add(1);
            return Ok(DriveOutcome::Suspended {
                reason,
                instructions_used: self.instructions_used,
                wall_elapsed_ms: self.wall_elapsed_ms,
                memory_used: self.memory_used,
            });
        }

        Ok(DriveOutcome::Ready { stashed })
    }

    /// Execute Lua source `code` with RC-1/RC-2 enforcement.
    ///
    /// Deterministic replay: given identical `code` and identical budgets on a
    /// fresh VM, the outcome (completion, suspension reason, warning flag) is
    /// identical. The VM is headless — no window/GPU, no `mlua` types.
    ///
    /// Fail-closed: if the VM is already `Suspended`, this call returns
    /// `Err(VmError::Suspended)` without touching the Lua heap. On budget
    /// exceed during execution, the VM transitions to `Suspended` at the next
    /// instruction boundary, increments `suspension_count`, and returns
    /// `Ok(ExecuteOutcome::Suspended)`.
    pub fn execute(&mut self, code: &str) -> Result<ExecuteOutcome, VmError> {
        match self.drive_chunk(code)? {
            DriveOutcome::Suspended {
                reason,
                instructions_used,
                wall_elapsed_ms,
                memory_used,
            } => Ok(ExecuteOutcome::Suspended {
                reason,
                instructions_used,
                wall_elapsed_ms,
                memory_used,
            }),
            DriveOutcome::Failed { message } => Ok(ExecuteOutcome::RuntimeError { message }),
            DriveOutcome::Ready { stashed } => {
                // Mode inspection mirrors the pre-refactor loop: Result carries
                // the outcome (or a runtime error), anything else counts as
                // completed (Stopped / yielded / already-done).
                let mode = self.lua.enter(|ctx| ctx.fetch(&stashed).mode());
                if mode == ExecutorMode::Result {
                    let mut runtime_msg: Option<String> = None;
                    let mut ok = false;
                    self.lua.enter(|ctx| {
                        let exec = ctx.fetch(&stashed);
                        match exec.take_result::<()>(ctx) {
                            Ok(Ok(())) => ok = true,
                            Ok(Err(e)) => runtime_msg = Some(format!("{e}")),
                            Err(e) => runtime_msg = Some(format!("{e:?}")),
                        }
                    });
                    if let Some(msg) = runtime_msg {
                        return Ok(ExecuteOutcome::RuntimeError { message: msg });
                    }
                }
                Ok(ExecuteOutcome::Completed {
                    instructions_used: self.instructions_used,
                    wall_elapsed_ms: self.wall_elapsed_ms,
                    memory_used: self.memory_used,
                    warning_triggered: self.warning_triggered,
                })
            }
        }
    }

    /// Execute with synthetic elapsed for deterministic wall tests.
    ///
    /// Same as `execute` but injects `synthetic_elapsed` as the wall time
    /// instead of measuring `Instant`. Used by headless deterministic harness
    /// to prove wall exceed without jitter.
    pub fn execute_with_elapsed(
        &mut self,
        code: &str,
        synthetic_elapsed: Duration,
    ) -> Result<ExecuteOutcome, VmError> {
        if let VmStatus::Suspended(reason) = &self.status {
            return Err(VmError::Suspended {
                reason: reason.clone(),
            });
        }
        // Fast-path: if synthetic elapsed already exceeds budgets, suspend
        // deterministically without touching Lua (proves wall exceed).
        let elapsed_ms = synthetic_elapsed.as_millis() as u64;
        if elapsed_ms >= self.warning_ms {
            self.warning_triggered = true;
            self.warning_count = self.warning_count.wrapping_add(1);
        }
        if elapsed_ms >= self.wall_budget_ms {
            let reason = SuspendReason::WallClockExceeded {
                elapsed_ms,
                budget_ms: self.wall_budget_ms,
            };
            self.status = VmStatus::Suspended(reason.clone());
            self.suspension_count = self.suspension_count.wrapping_add(1);
            self.wall_elapsed_ms = elapsed_ms;
            self.instructions_used = 0;
            self.memory_used = self.lua.total_memory();
            return Ok(ExecuteOutcome::Suspended {
                reason,
                instructions_used: 0,
                wall_elapsed_ms: elapsed_ms,
                memory_used: self.memory_used,
            });
        }
        // Otherwise delegate to real execute (which will measure real wall,
        // but synthetic case already handled).
        self.execute(code)
    }
}

#[cfg(test)]
mod vm_unit_tests {
    use super::*;

    #[test]
    fn new_vm_defaults() {
        let vm = LuaVm::new("xuepoo.test");
        assert_eq!(vm.instruction_budget(), RC1_INSTRUCTION_BUDGET);
        assert_eq!(vm.wall_budget_ms(), RC1_WALL_CLOCK_BUDGET_MS);
        assert_eq!(vm.warning_ms(), RC1_WARNING_MS);
        assert_eq!(vm.memory_limit(), RC2_MEMORY_PER_PLUGIN_BYTES);
        assert!(!vm.is_suspended());
        let snap = vm.budget_snapshot();
        assert!(snap.invariants_hold());
    }

    #[test]
    fn check_budgets_deterministic() {
        let vm = LuaVm::new("xuepoo.check");
        let (s, w) = vm.check_budgets(4, 100, 1024);
        assert!(s.is_none());
        assert!(!w);
        let (s, w) = vm.check_budgets(8, 100, 1024);
        assert!(s.is_none());
        assert!(w);
        let (s, _) = vm.check_budgets(50, 100, 1024);
        assert!(matches!(s, Some(SuspendReason::WallClockExceeded { .. })));
        let (s, _) = vm.check_budgets(0, 10_000_000, 1024);
        assert!(matches!(
            s,
            Some(SuspendReason::InstructionBudgetExceeded { .. })
        ));
        let (s, _) = vm.check_budgets(0, 100, 32 * 1024 * 1024 + 1);
        assert!(matches!(s, Some(SuspendReason::MemoryExceeded { .. })));
    }

    #[test]
    fn simple_execute_completes() {
        let mut vm = LuaVm::new("xuepoo.simple");
        let outcome = vm.execute("return 1+2").unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Completed { .. }));
        assert!(!vm.is_suspended());
        let snap = vm.budget_snapshot();
        assert!(snap.invariants_hold());
        assert_eq!(snap.suspension_count, 0);
    }

    #[test]
    fn restricted_stdlib_denies_ambient_io_and_dynamic_loading() {
        let mut vm = LuaVm::new("xuepoo.sandbox");
        let outcome = vm
            .execute(
                "assert(io == nil and os == nil and package == nil and debug == nil and load == nil and loadfile == nil and dofile == nil)",
            )
            .unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Completed { .. }));
    }

    #[test]
    fn fail_closed_after_suspend() {
        let mut vm = LuaVm::with_budgets("xuepoo.fail", 10, 50, 8, 32 * 1024 * 1024);
        // Infinite loop with tiny instruction budget should suspend.
        let outcome = vm.execute("while true do end").unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Suspended { .. }));
        assert!(vm.is_suspended());
        let err = vm.execute("return 1").unwrap_err();
        assert!(matches!(err, VmError::Suspended { .. }));
    }
}
