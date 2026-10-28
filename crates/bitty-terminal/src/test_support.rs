//! Test-only helpers for the resumable RC-1 dispatch path (bitty #2409).
//!
//! bitty #2314 made app-dispatched plugin commands resumable: a command that
//! reaches the 50 ms charged RC-1 wall budget returns
//! [`PluginRuntimeError::Parked`] and the next cold tick resumes the same
//! executor. Probe tests that call [`PluginRuntime::dispatch_command`]
//! directly therefore have to drive that recovery instead of treating a park
//! as a refusal; re-dispatching instead would run the command body twice.
//!
//! [`PluginRuntimeError::Parked`]:
//! bitty_runtime::plugin_runtime::PluginRuntimeError::Parked

use bitty_lua::{RC1_CALLBACK_CEILING_MS, RC1_WALL_CLOCK_BUDGET_MS};
use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{LuaValue, PluginRuntime, PluginRuntimeError};

/// Cold ticks one legitimate park may need to complete (bitty #2409).
///
/// Each cold tick advances at most one RC-1 wall window
/// ([`RC1_WALL_CLOCK_BUDGET_MS`]) and the cumulative active time is bounded by
/// [`RC1_CALLBACK_CEILING_MS`], so no legitimate park needs more windows than
/// the ceiling divided by the window plus the window that completes it. The
/// bound only turns a stuck park into a test failure instead of a hang; it is
/// derived from the production constants, never tuned to a runner.
pub(crate) const PARK_RESUME_TICKS: usize =
    (RC1_CALLBACK_CEILING_MS / RC1_WALL_CLOCK_BUDGET_MS) as usize + 1;

/// One host-mediated dispatch after cold-tick settling (bitty #2409).
pub(crate) struct SettledDispatch {
    /// Value or terminal error the command produced, immediately or on the
    /// resuming tick. A park itself is never reported here.
    pub(crate) outcome: Result<LuaValue, PluginRuntimeError>,
    /// Whether the dispatch parked at the RC-1 wall budget.
    pub(crate) parked: bool,
    /// Completed parked resumes observed while settling (`0` or `1`).
    pub(crate) resumes: usize,
}

impl SettledDispatch {
    /// Consume the settlement, requiring the probe's expected string shape.
    pub(crate) fn into_string(self, command: &str) -> String {
        match self.outcome {
            Ok(LuaValue::String(value)) => value,
            Ok(other) => panic!("{command}: expected a string result, got {other:?}"),
            Err(error) => panic!("{command}: dispatch failed: {error:?}"),
        }
    }

    /// Consume the settlement, panicking on a terminal dispatch error.
    pub(crate) fn expect_value(self, command: &str) -> LuaValue {
        match self.outcome {
            Ok(value) => value,
            Err(error) => panic!("{command}: dispatch failed: {error:?}"),
        }
    }
}

/// Dispatch one command and settle an RC-1 park on the cold ticks.
///
/// Drives the documented recovery, the same
/// [`PluginRuntime::pump_parked_commands`] step `TerminalApp` runs inside its
/// cold phase, and proves the parked path explicitly: the park is recorded
/// once, exactly one completion resumes it, the parked set returns to empty,
/// and an immediately following pump delivers nothing (no replay). The
/// dispatch itself is never retried, so the body runs exactly once on either
/// path; a park is replaced by the resumed outcome, never reported as one.
pub(crate) fn settle_dispatch(
    runtime: &mut PluginRuntime,
    id: &str,
    command: &str,
    args: &[LuaValue],
) -> SettledDispatch {
    let pid = PluginId::new(id).expect("plugin id");
    match runtime.dispatch_command(&pid, command, args) {
        Ok(value) => SettledDispatch {
            outcome: Ok(value),
            parked: false,
            resumes: 0,
        },
        Err(error) if error.is_parked() => {
            assert_eq!(
                runtime.parked_command_count(),
                1,
                "{command}: the park must be recorded exactly once"
            );
            for _ in 0..PARK_RESUME_TICKS {
                let completions = runtime.pump_parked_commands();
                if completions.is_empty() {
                    // Still parked: the next cold tick opens the next RC-1
                    // wall window.
                    continue;
                }
                assert_eq!(
                    completions.len(),
                    1,
                    "{command}: one parked invocation completes per cold tick"
                );
                let completion = completions
                    .into_iter()
                    .next()
                    .expect("checked exactly one completion");
                assert_eq!(completion.plugin.as_str(), id);
                assert_eq!(completion.command, command);
                assert_eq!(
                    runtime.parked_command_count(),
                    0,
                    "{command}: a completed park must be dropped"
                );
                assert!(
                    runtime.pump_parked_commands().is_empty(),
                    "{command}: a completed command must never replay"
                );
                return SettledDispatch {
                    outcome: completion.outcome,
                    parked: true,
                    resumes: 1,
                };
            }
            panic!(
                "{command}: the parked invocation did not complete within \
                 {PARK_RESUME_TICKS} cold ticks"
            )
        }
        Err(error) => SettledDispatch {
            outcome: Err(error),
            parked: false,
            resumes: 0,
        },
    }
}

/// Activation wall window for composition-root probes (bitty#2781).
///
/// Activation callbacks are slice-only: they do not park, they suspend
/// fail-closed, so a loaded runner that deschedules the thread for longer than
/// one RC-1 slice makes `init.lua` fail with `WallClockExceeded` and the plugin
/// never activates. No resume signal can recover that, so composition-root
/// probes that assert capability wiring install the documented test-only wall
/// seam before activation. The window is the documented callback envelope
/// ([`RC1_CALLBACK_CEILING_MS`]); instruction, memory, and cumulative-callback
/// budgets keep their production values.
pub(crate) const HARNESS_ACTIVATION_WALL_MS: u64 = RC1_CALLBACK_CEILING_MS;

thread_local! {
    /// Scoped activation wall override, read by `discover_with_roots` under
    /// `#[cfg(test)]` only (production never installs it).
    static ACTIVATION_WALL_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Reads the scoped harness activation budget (bitty#2781).
pub(crate) fn harness_activation_wall_ms() -> Option<u64> {
    ACTIVATION_WALL_MS.with(std::cell::Cell::get)
}

/// Run `f` with the composition-root activation window widened for the
/// duration of the call (bitty#2781).
///
/// Scoped on purpose: other `discover_with_roots` probes pin park and budget
/// semantics at the production slice and must not inherit the widened window.
pub(crate) fn with_harness_activation_wall<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);

    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVATION_WALL_MS.with(|cell| cell.set(self.0));
        }
    }

    let _restore =
        Restore(ACTIVATION_WALL_MS.with(|cell| cell.replace(Some(HARNESS_ACTIVATION_WALL_MS))));
    f()
}
