//! Shared W-146 stub backends for `bitty-runtime` integration tests.
//!
//! Seam-discipline only, never byte parity: the memory stub proves call
//! order, fail-closed behavior, and state intactness through the Core-owned
//! traits. Byte parity with the pre-rewire mechanics is proven in the
//! wiring crate (`bitty-terminal`) against the real `bitty-storage`
//! backend and the committed golden fixtures.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    KvCommitBackend, KvCommitError, LuaValue, PluginRuntime, PluginRuntimeError,
    STORE_FILE_MAX_BYTES,
};

#[derive(Debug, Default)]
struct MemoryState {
    files: HashMap<PathBuf, Vec<u8>>,
    fail_commits: bool,
    fail_loads: bool,
    slow_commit: Option<Duration>,
    commits: u64,
    loads: u64,
}

/// In-memory [`KvCommitBackend`] stub with failure and latency injection.
///
/// Enforces the Core file ceiling on both directions, mirroring the
/// production requirement; every other behavior (ordering, intactness,
/// fail-closed) is what the tests pin.
///
/// Shared across integration test binaries; each binary uses a subset, so
/// unused methods are allowed here rather than per call site.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct MemoryKvBackend {
    state: Arc<Mutex<MemoryState>>,
}

#[allow(dead_code)]
impl MemoryKvBackend {
    /// Builds an empty stub backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds one stored image (stands in for a file written earlier).
    pub fn seed(&self, path: &Path, bytes: Vec<u8>) {
        self.state
            .lock()
            .expect("stub lock")
            .files
            .insert(path.to_path_buf(), bytes);
    }

    /// Reads back one stored image, if any.
    #[must_use]
    pub fn stored(&self, path: &Path) -> Option<Vec<u8>> {
        self.state
            .lock()
            .expect("stub lock")
            .files
            .get(path)
            .cloned()
    }

    /// Makes every commit fail (durability fault injection).
    pub fn set_fail_commits(&self, fail: bool) {
        self.state.lock().expect("stub lock").fail_commits = fail;
    }

    /// Makes every load fail (read fault injection).
    pub fn set_fail_loads(&self, fail: bool) {
        self.state.lock().expect("stub lock").fail_loads = fail;
    }

    /// Slows every commit (slow-disk regression stand-in).
    pub fn set_slow_commit(&self, delay: Option<Duration>) {
        self.state.lock().expect("stub lock").slow_commit = delay;
    }

    /// Commits attempted so far.
    #[must_use]
    pub fn commits(&self) -> u64 {
        self.state.lock().expect("stub lock").commits
    }

    /// Loads attempted so far.
    #[must_use]
    pub fn loads(&self) -> u64 {
        self.state.lock().expect("stub lock").loads
    }
}

impl KvCommitBackend for MemoryKvBackend {
    fn commit_store_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), KvCommitError> {
        let mut state = self.state.lock().expect("stub lock");
        if state.fail_commits {
            return Err(KvCommitError::new("stub commit refused"));
        }
        if bytes.len() > STORE_FILE_MAX_BYTES {
            return Err(KvCommitError::new("stub payload exceeds ceiling"));
        }
        if let Some(delay) = state.slow_commit {
            std::thread::sleep(delay);
        }
        state.commits += 1;
        state.files.insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn load_store_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, KvCommitError> {
        let mut state = self.state.lock().expect("stub lock");
        if state.fail_loads {
            return Err(KvCommitError::new("stub load refused"));
        }
        state.loads += 1;
        let image = state.files.get(path).cloned();
        if let Some(bytes) = &image {
            if bytes.len() > STORE_FILE_MAX_BYTES {
                return Err(KvCommitError::new("stub image exceeds ceiling"));
            }
        }
        Ok(image)
    }
}

/// Installs a fresh stub backend on `rt` and returns the handle so tests
/// can seed storage and inject faults.
#[allow(dead_code)]
pub fn install_stub_backend(
    rt: &mut bitty_runtime::plugin_runtime::PluginRuntime,
) -> MemoryKvBackend {
    let backend = MemoryKvBackend::new();
    rt.set_store_backend(Some(Arc::new(backend.clone())));
    backend
}

// ── RC-1 resumable dispatch (bitty #2314, bitty #2781) ──────────────────

/// Cold ticks one legitimate RC-1 park may need to complete.
///
/// Each cold tick opens at most one RC-1 wall window
/// ([`bitty_lua::RC1_WALL_CLOCK_BUDGET_MS`]) and the cumulative active time of
/// one callback is bounded by [`bitty_lua::RC1_CALLBACK_CEILING_MS`], so no
/// legitimate park needs more resume ticks than the ceiling divided by the
/// window, plus the tick that completes it. The bound exists only to turn a
/// stuck park into a clear test failure instead of a hang; it is derived from
/// the production constants and is never tuned to a runner.
#[allow(dead_code)]
pub const RC1_RESUME_TICKS: usize =
    (bitty_lua::RC1_CALLBACK_CEILING_MS / bitty_lua::RC1_WALL_CLOCK_BUDGET_MS) as usize + 1;

/// Host-call deadline for capability probes (bitty#2781).
///
/// Production bounds every `bitty.*` call at [`bitty_lua::DEFAULT_HOST_DEADLINE_MS`]
/// (the RC-1 50 ms slice) on a raw wall clock measured inside the call, so a
/// runner that deschedules the probe thread mid-call is charged the whole
/// stall and the read fails closed as `E_TIMEOUT` (the bitty#2432 class,
/// observed on `runtime.inspect` in bitty#2781). Probe assertions are about
/// capability semantics, never deadlines, so the documented test-only seam
/// raises that one dimension to the documented callback envelope
/// ([`bitty_lua::RC1_CALLBACK_CEILING_MS`]). Instruction, memory, and
/// cumulative-callback limits remain in force, and deadline enforcement stays
/// pinned by `bitty-lua/tests/host_bridge.rs` and
/// `bitty-lua/tests/host_budget_accounting.rs`.
#[allow(dead_code)]
pub const HARNESS_HOST_CALL_DEADLINE_MS: u64 = bitty_lua::RC1_CALLBACK_CEILING_MS;

/// Activation and dispatch wall window for capability probes (bitty#2781).
///
/// Activation callbacks are slice-only: they do not park, they suspend
/// fail-closed (`init.lua suspended: WallClockExceeded`), so a loaded runner
/// that deschedules the thread for 95 ms before `init.lua` finishes leaves the
/// plugin inactive and no resume signal can recover it. Probe assertions are
/// about capability wiring, not the activation window, so they install the
/// documented test-only wall seam before activation. The window is the
/// documented callback envelope ([`bitty_lua::RC1_CALLBACK_CEILING_MS`]);
/// instruction and memory budgets keep their production values, the derived
/// cumulative ceiling stays bounded, and a park is still settled on the
/// documented cold-tick resume.
#[allow(dead_code)]
pub const HARNESS_VM_WALL_BUDGET_MS: u64 = bitty_lua::RC1_CALLBACK_CEILING_MS;

/// One app-dispatched command invoked once and settled on the cold ticks.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SettledDispatch {
    /// Terminal outcome: the value the command produced, or the non-park
    /// error it failed with. A park itself is never surfaced here.
    pub outcome: Result<LuaValue, PluginRuntimeError>,
    /// Whether the first dispatch parked at the RC-1 wall budget.
    pub parked: bool,
    /// Cold ticks the resume consumed before the command completed.
    pub resume_ticks: usize,
}

#[allow(dead_code)]
impl SettledDispatch {
    /// Consume the settlement, panicking on a terminal dispatch error.
    pub fn expect_value(self, command: &str) -> LuaValue {
        self.outcome
            .unwrap_or_else(|error| panic!("{command}: dispatch failed: {error}"))
    }

    /// Consume the settlement, requiring a string result.
    pub fn expect_string(self, command: &str) -> String {
        match self.expect_value(command) {
            LuaValue::String(value) => value,
            other => panic!("{command}: expected a string result, got {other:?}"),
        }
    }
}

/// Dispatch one command and settle an RC-1 park on the documented resume
/// signal (bitty #2781).
///
/// The RC-1 wall budget is charged against wall-clock time, so a loaded
/// runner can park a command body that used microseconds of CPU: the thread
/// was simply off-CPU for the window. Parking is the documented recovery
/// path, not a refusal, so this helper drives
/// [`PluginRuntime::pump_parked_commands`] — exactly the step `TerminalApp`
/// runs inside its cold phase — until the invocation completes. The dispatch
/// is never retried, so the body runs exactly once; the parked set must be
/// empty and must never replay after the completion.
///
/// Test probes therefore assert the command's *result*, never "the command
/// finished inside one default wall window", which is a property of the
/// machine rather than of the code under test.
#[allow(dead_code)]
pub fn settle_dispatch(
    runtime: &mut PluginRuntime,
    id: &PluginId,
    command: &str,
    args: &[LuaValue],
) -> SettledDispatch {
    match runtime.dispatch_command(id, command, args) {
        Ok(value) => SettledDispatch {
            outcome: Ok(value),
            parked: false,
            resume_ticks: 0,
        },
        Err(error) if error.is_parked() => {
            assert_eq!(
                runtime.parked_command_count(),
                1,
                "{command}: the park must be recorded exactly once"
            );
            for tick in 0..RC1_RESUME_TICKS {
                let mut completions = runtime.pump_parked_commands();
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
                let completion = completions.pop().expect("checked exactly one completion");
                assert_eq!(completion.plugin, *id, "{command}: completion owner");
                assert_eq!(completion.command, command, "{command}: completion id");
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
                    resume_ticks: tick + 1,
                };
            }
            panic!(
                "{command}: the parked invocation did not complete within \
                 {RC1_RESUME_TICKS} cold ticks"
            )
        }
        Err(error) => SettledDispatch {
            outcome: Err(error),
            parked: false,
            resume_ticks: 0,
        },
    }
}
