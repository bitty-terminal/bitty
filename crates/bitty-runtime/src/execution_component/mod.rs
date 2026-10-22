//! Core host client for the installable `execution` component (bitty#2494).
//!
//! The execution supervisor is being split out of Core (`bitty-execution`):
//! the linked `JobRegistry` stays the fallback for this release, while the
//! same supervisor is also installable as a stdio coprocess named
//! `execution` (`bitty-execution` executable, protocol v1). This module is
//! Core's client for that component:
//!
//! - Resolve the component through the shared two-tier install layout
//!   ([`crate::component::resolve_search`]): `<root>/execution/current` and
//!   `<root>/execution/<version>/bitty-component.toml`, with the executable's
//!   SHA-256 verified before every spawn. `PATH` is never read.
//! - Spawn it with a cleared environment plus the shared component allowlist,
//!   in its own process group on Unix, as a stdio coprocess.
//! - Complete the `Hello`/`HelloAck` handshake within a bounded deadline and
//!   reject a different component, protocol, or version surface.
//! - Speak the length-prefixed execution protocol v1: `Spawn`, `Terminate`,
//!   `ReadOutput`, `Shutdown`, and the `Spawned`/`Started`/`Output`/`Exit`/
//!   `Canceled`/`OutputView`/`Error`/`ShutdownAck` replies. The codec itself
//!   is the pinned extension's authoritative `component::protocol` module, so
//!   host and component cannot drift on framing, bounds, or tags.
//! - Keep all pipe I/O off the caller's thread: one stdout reader, one stdin
//!   writer, and one stderr reader per component, with bounded queues on both
//!   directions. A host that stops reading applies pipe backpressure to the
//!   component instead of growing an unbounded queue.
//! - Map the terminal `Exit` frame to the existing execution-job model. The
//!   `bitty-runtime` `execution_jobs` bridge converts
//!   [`ComponentRun`] into the same `RawExecutionOutput`/`JobRun` shape the
//!   linked registry produces, so callers see no second outcome vocabulary.
//!
//! # Authority boundary
//!
//! The protocol carries no file operations, no descriptor passing, and no
//! ambient command language: program, argv, cwd, environment, and deadlines
//! appear only inside an explicit [`ComponentSpawnSpec`]. The host owns
//! installation, digest verification, safe-mode denial, and the decision to
//! use the component at all; the component executes exactly what it is
//! handed.
//!
//! Safe mode denies the component path before any resolution or spawn
//! ([`ComponentError::SafeMode`]); callers must not fall back to another
//! execution path for that denial.
//!
//! # Bounds
//!
//! Wire bounds are enforced by the pinned codec (`256 KiB` frames, `16 KiB`
//! output chunks, `240 KiB` output reads, `64` jobs, `60 s` cancel grace,
//! argv/env limits). This client adds its own:
//! [`EXECUTION_COMPONENT_INBOUND_FRAMES`],
//! [`EXECUTION_COMPONENT_OUTBOUND_FRAMES`],
//! [`EXECUTION_COMPONENT_RETAINED_BYTES`] per stream,
//! [`EXECUTION_COMPONENT_MAX_JOBS`], a bounded stderr ring, and explicit
//! handshake/shutdown/wait deadlines.
//!
//! # Not in this slice
//!
//! - Production wiring (the gate is default-off; `execution_jobs` uses the
//!   linked registry unless a host injects [`ComponentGateConfig`]).
//! - Removing the linked `bitty-execution` dependency (parity and rollout
//!   follow).
//! - Core exec confinement for admitted `[tools.*]` spawns: protocol v1 has
//!   no confinement field, so the bridge keeps those spawns on the linked
//!   registry (`Preferred` falls back, `Required` refuses).
//! - Windows Job Object containment for the component process itself; the
//!   component is killed directly, and its own supervisor remains responsible
//!   for its job trees.
//! - PTY/interactive jobs: protocol v1 reserves those codes and the component
//!   refuses them. Interactive ownership stays with the terminal primitive.

#![forbid(unsafe_code)]

mod host;

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use bitty_ipc::execution::{
    MAX_EXEC_ARG_BYTES, MAX_EXEC_ARGS, MAX_EXEC_ARGS_TOTAL_BYTES, MAX_EXEC_CWD_BYTES,
    MAX_EXEC_ENV_NAME_BYTES, MAX_EXEC_ENV_VALUE_BYTES, MAX_EXEC_ENV_VARS,
};

pub use host::ComponentExecutionHost;

/// Component name (`[a-z][a-z0-9-]{0,31}`) and install directory name.
pub const EXECUTION_COMPONENT_NAME: &str = "execution";

/// Lowest execution-component wire protocol version this host speaks.
pub const EXECUTION_COMPONENT_PROTOCOL_MIN: u16 = 1;

/// Highest execution-component wire protocol version this host speaks.
pub const EXECUTION_COMPONENT_PROTOCOL_MAX: u16 = 1;

/// Maximum jobs the component tracks (mirrors the wire `MAX_JOBS`).
pub const EXECUTION_COMPONENT_MAX_JOBS: usize = bitty_execution::component::protocol::MAX_JOBS;

/// Inbound frames buffered between the stdout reader and the owner.
///
/// A full queue blocks the reader, which applies pipe backpressure to the
/// component instead of growing memory without bound.
pub const EXECUTION_COMPONENT_INBOUND_FRAMES: usize = 128;

/// Outbound requests buffered for the stdin writer.
pub const EXECUTION_COMPONENT_OUTBOUND_FRAMES: usize = 64;

/// Newest retained bytes per stream on the Core side.
///
/// Matches the supervisor's per-stream output store, so the connected run's
/// text tail is the same shape the linked path returns.
pub const EXECUTION_COMPONENT_RETAINED_BYTES: usize = bitty_execution::MAX_OUTPUT_BYTES_PER_JOB;

/// Bounded stderr ring for diagnostics (never protocol).
pub const EXECUTION_COMPONENT_STDERR_BYTES: usize = crate::component::COMPONENT_STDERR_MAX_BYTES;

/// Maximum combined environment name plus value bytes in one `Spawn`.
pub const EXECUTION_COMPONENT_MAX_ENV_TOTAL_BYTES: usize =
    bitty_execution::component::protocol::MAX_ENV_TOTAL_BYTES;

/// Default hard deadline for a component command (mirrors
/// `bitty-ipc`'s `DEFAULT_EXEC_TIMEOUT_MS`).
pub const EXECUTION_COMPONENT_DEFAULT_TIMEOUT_MS: u32 =
    bitty_ipc::execution::DEFAULT_EXEC_TIMEOUT_MS as u32;

/// Extra time granted after a job's hard deadline for the component to kill
/// and reap the owned tree before the host asks for a typed terminate.
///
/// Mirrors `execution_jobs::JOB_WAIT_GRACE` (the linked path's 5 s drain join
/// plus scheduling slack).
pub const EXECUTION_COMPONENT_WAIT_GRACE: Duration = Duration::from_secs(6);

/// Time granted after a terminate request for the terminal `Exit` frame.
///
/// Mirrors `execution_jobs::JOB_CANCEL_GRACE`.
pub const EXECUTION_COMPONENT_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// One blocking wait slice while polling the component.
pub const EXECUTION_COMPONENT_POLL_SLICE: Duration = Duration::from_millis(10);

/// Frames processed by one `poll` call, so a flooding component cannot make
/// one call unbounded.
pub const EXECUTION_COMPONENT_POLL_MAX_FRAMES: usize = 256;

/// Default `HelloAck` deadline.
pub const EXECUTION_COMPONENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default graceful-shutdown deadline (the component waits up to 5 s for its
/// own jobs to observe the kill before `ShutdownAck`).
pub const EXECUTION_COMPONENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);

/// Finished-record retention sent to the component.
///
/// The linked bridge requests zero retention so finished records are
/// immediately reclaimable. The wire treats `0` as "component default"
/// (60 s), so the closest equivalent is one millisecond.
pub const EXECUTION_COMPONENT_RETENTION_MS: u32 = 1;

/// Which execution path a host prefers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionComponentMode {
    /// Linked `bitty-execution` registry only (default; no component spawn).
    #[default]
    Disabled,
    /// Use the component when it is installed and compatible, otherwise fall
    /// back to the linked registry.
    Preferred,
    /// Require the component. Absent, incompatible, or unusable installs fail
    /// closed with an error; there is no linked fallback.
    Required,
}

impl ExecutionComponentMode {
    /// Stable lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Preferred => "preferred",
            Self::Required => "required",
        }
    }

    /// Parse the stable label; unknown values are rejected.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "disabled" => Some(Self::Disabled),
            "preferred" => Some(Self::Preferred),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

impl fmt::Display for ExecutionComponentMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Gate configuration for the execution component.
///
/// The default is [`ExecutionComponentMode::Disabled`]: the linked registry
/// remains the only path until a host opts in. `safe_mode` is checked before
/// any install resolution or process spawn.
#[derive(Debug, Clone)]
pub struct ComponentGateConfig {
    /// Preferred execution path.
    pub mode: ExecutionComponentMode,
    /// Host-client limits and install roots.
    pub host: ComponentHostConfig,
}

impl Default for ComponentGateConfig {
    fn default() -> Self {
        Self {
            mode: ExecutionComponentMode::Disabled,
            host: ComponentHostConfig::default(),
        }
    }
}

/// Limits and install roots for one component host client.
#[derive(Debug, Clone)]
pub struct ComponentHostConfig {
    /// User-tier component root (`<data_home>/bitty/extensions` in
    /// production, `BITTY_COMPONENTS_DIR` in tests). `None` skips the tier.
    pub user_root: Option<PathBuf>,
    /// System-tier component root. `None` skips the tier.
    pub system_root: Option<PathBuf>,
    /// Environment forwarded to the component process (cleared otherwise).
    pub env: crate::component::ComponentEnv,
    /// Core version announced in `Hello`.
    pub core_version: String,
    /// `HelloAck` deadline.
    pub handshake_timeout: Duration,
    /// Graceful-shutdown deadline.
    pub shutdown_timeout: Duration,
    /// Safe mode: deny the component path before any I/O.
    pub safe_mode: bool,
}

impl Default for ComponentHostConfig {
    fn default() -> Self {
        Self {
            user_root: None,
            system_root: None,
            env: crate::component::ComponentEnv::from_process(),
            core_version: env!("CARGO_PKG_VERSION").to_owned(),
            handshake_timeout: EXECUTION_COMPONENT_HANDSHAKE_TIMEOUT,
            shutdown_timeout: EXECUTION_COMPONENT_SHUTDOWN_TIMEOUT,
            safe_mode: false,
        }
    }
}

/// Environment shape of one spawned job.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ComponentEnvironment {
    /// No inherited variables (the component starts from an empty
    /// environment).
    #[default]
    Isolated,
    /// Exactly the listed variables.
    Explicit(Vec<(String, String)>),
}

impl ComponentEnvironment {
    /// Build a bounded explicit environment.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentError::InvalidRequest`] for a bad name grammar,
    /// a NUL, or an over-bound entry/count.
    pub fn explicit(vars: Vec<(String, String)>) -> Result<Self, ComponentError> {
        if vars.len() > MAX_EXEC_ENV_VARS {
            return Err(ComponentError::InvalidRequest {
                reason: "environment has too many variables".to_owned(),
            });
        }
        let mut total = 0usize;
        for (name, value) in &vars {
            bitty_ipc::execution::EnvVar::new(name.clone(), value.clone()).map_err(|error| {
                ComponentError::InvalidRequest {
                    reason: error.to_string(),
                }
            })?;
            total = total.saturating_add(name.len()).saturating_add(value.len());
        }
        if total > EXECUTION_COMPONENT_MAX_ENV_TOTAL_BYTES {
            return Err(ComponentError::InvalidRequest {
                reason: "environment exceeds the wire total".to_owned(),
            });
        }
        Ok(Self::Explicit(vars))
    }
}

/// Declared lifecycle shape of one spawned job (`Spawn.kind`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ComponentSpawnKind {
    /// Ordinary finite command.
    #[default]
    Command,
    /// Long-running service.
    Service,
    /// Long-running observer.
    Watch,
}

/// One bounded spawn request for the component.
///
/// The program, argv, cwd, environment, and deadline are the only execution
/// input this boundary accepts; nothing is shell-interpreted. Values are
/// validated against the shared `bitty-ipc` execution limits before a frame
/// is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSpawnSpec {
    /// Executable path or name.
    pub program: String,
    /// Argument vector.
    pub args: Vec<String>,
    /// Working directory; `None` means the component's default.
    pub cwd: Option<String>,
    /// Environment shape.
    pub env: ComponentEnvironment,
    /// Lifecycle shape.
    pub kind: ComponentSpawnKind,
    /// Absolute deadline in milliseconds; `0` means none.
    pub hard_timeout_ms: u32,
    /// Output-idle deadline in milliseconds; `0` means none.
    pub idle_timeout_ms: u32,
    /// Finished-record retention in milliseconds; `0` means component default.
    pub retention_ms: u32,
}

impl ComponentSpawnSpec {
    /// Isolated command with the default hard deadline.
    #[must_use]
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env: ComponentEnvironment::Isolated,
            kind: ComponentSpawnKind::Command,
            hard_timeout_ms: EXECUTION_COMPONENT_DEFAULT_TIMEOUT_MS,
            idle_timeout_ms: 0,
            retention_ms: EXECUTION_COMPONENT_RETENTION_MS,
        }
    }

    /// Set the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: Option<String>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Set the environment shape.
    #[must_use]
    pub fn with_env(mut self, env: ComponentEnvironment) -> Self {
        self.env = env;
        self
    }

    /// Set the lifecycle shape.
    #[must_use]
    pub fn with_kind(mut self, kind: ComponentSpawnKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set the hard deadline in milliseconds.
    #[must_use]
    pub fn with_hard_timeout_ms(mut self, hard_timeout_ms: u32) -> Self {
        self.hard_timeout_ms = hard_timeout_ms;
        self
    }

    /// Set the output-idle deadline in milliseconds.
    #[must_use]
    pub fn with_idle_timeout_ms(mut self, idle_timeout_ms: u32) -> Self {
        self.idle_timeout_ms = idle_timeout_ms;
        self
    }

    /// Set the finished-record retention in milliseconds.
    #[must_use]
    pub fn with_retention_ms(mut self, retention_ms: u32) -> Self {
        self.retention_ms = retention_ms;
        self
    }

    /// Validate every field against the shared execution/wire bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentError::InvalidRequest`] for any shape or bound
    /// violation. Validation happens before the `Spawn` frame is queued.
    pub fn validate(&self) -> Result<(), ComponentError> {
        if self.program.is_empty() {
            return Err(invalid("program must not be empty"));
        }
        if self.program.len() > bitty_ipc::execution::MAX_EXECUTABLE_BYTES {
            return Err(invalid("program exceeds the executable bound"));
        }
        if self.program.contains('\0') {
            return Err(invalid("program must not contain NUL"));
        }
        if self.args.len() > MAX_EXEC_ARGS {
            return Err(invalid("argv has too many entries"));
        }
        let mut total = 0usize;
        for arg in &self.args {
            if arg.len() > MAX_EXEC_ARG_BYTES {
                return Err(invalid("an argument exceeds the per-argument bound"));
            }
            if arg.contains('\0') {
                return Err(invalid("arguments must not contain NUL"));
            }
            total = total.saturating_add(arg.len());
        }
        if total > MAX_EXEC_ARGS_TOTAL_BYTES {
            return Err(invalid("argv exceeds the total byte bound"));
        }
        if let Some(cwd) = &self.cwd {
            if cwd.len() > MAX_EXEC_CWD_BYTES {
                return Err(invalid("cwd exceeds the path bound"));
            }
            if cwd.contains('\0') {
                return Err(invalid("cwd must not contain NUL"));
            }
        }
        if let ComponentEnvironment::Explicit(vars) = &self.env {
            if vars.len() > MAX_EXEC_ENV_VARS {
                return Err(invalid("environment has too many variables"));
            }
            let mut env_total = 0usize;
            for (name, value) in vars {
                bitty_ipc::execution::EnvVar::new(name.clone(), value.clone())
                    .map_err(|_| invalid("environment entry is malformed"))?;
                if name.len() > MAX_EXEC_ENV_NAME_BYTES || value.len() > MAX_EXEC_ENV_VALUE_BYTES {
                    return Err(invalid("environment entry exceeds its field bound"));
                }
                if name.is_empty() || name.contains('\0') || value.contains('\0') {
                    return Err(invalid("environment entry is malformed"));
                }
                env_total = env_total
                    .saturating_add(name.len())
                    .saturating_add(value.len());
            }
            if env_total > EXECUTION_COMPONENT_MAX_ENV_TOTAL_BYTES {
                return Err(invalid("environment exceeds the wire total"));
            }
        }
        if self.hard_timeout_ms == 0 && self.kind == ComponentSpawnKind::Command {
            return Err(invalid("a command requires a non-zero hard deadline"));
        }
        if u64::from(self.hard_timeout_ms) > bitty_ipc::execution::MAX_EXEC_TIMEOUT_MS {
            return Err(invalid("hard deadline exceeds the execution timeout bound"));
        }
        Ok(())
    }
}

/// Which child stream a read or output frame addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How a terminate request stops the owned process tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentTerminateMode {
    /// `SIGINT`, then wait up to the grace; never escalates.
    Graceful,
    /// Kill the owned tree now.
    Immediate,
    /// `SIGINT`, wait, `SIGTERM`, wait, then kill.
    GracefulThenKill,
}

/// Typed terminate answer carried by `Canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentTerminateOutcome {
    /// Landed before the process existed.
    CancelledBeforeStart,
    /// Exited within a graceful step.
    CancelledGracefully,
    /// The owned tree was killed.
    Killed,
    /// Already terminal; nothing changed.
    AlreadyExited,
    /// The kernel refused the stop signal.
    PermissionDenied,
    /// The generation does not match the job.
    StaleGeneration,
    /// A graceful-only request left the job running.
    StillRunning,
    /// No graceful stop exists on this platform.
    Unsupported,
    /// The end could not be determined.
    Unknown,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentExitKind {
    /// Exited on its own with status 0.
    Success,
    /// Exited on its own with a non-zero status.
    ExitCode(i32),
    /// A signal the host did not send terminated it.
    Signaled(i32),
    /// No process was created, or a partial start was cleaned up.
    SpawnFailed,
    /// A terminate request ended it.
    Cancelled(ComponentCancelEffect),
    /// A supervision deadline ended it.
    TimedOut(ComponentDeadlineClock),
    /// The kernel OOM-killed it from per-job evidence.
    OomKilled,
    /// The supervisor went away while it ran.
    SupervisorLost,
    /// The end could not be observed.
    Unknown,
}

/// How a terminate request ended a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentCancelEffect {
    /// Landed before the process existed.
    BeforeStart,
    /// Exited within a graceful step.
    Graceful,
    /// The owned tree was killed.
    Killed,
}

/// Which deadline ended a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentDeadlineClock {
    /// Hard wall-clock deadline.
    Hard,
    /// Output-idle deadline.
    Idle,
}

/// How much of the owned tree a kill reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentKillScope {
    /// Direct child only; the orphan gap is surfaced.
    DirectChild,
    /// Whole owned process tree.
    OwnedTree,
}

/// OOM verdict carried by `Exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentOomVerdict {
    /// No completed reading.
    Unknown,
    /// The per-job counter advanced.
    OomKilled,
    /// Both readings exist and the counter did not advance.
    NotOom,
}

/// Terminal `Exit` frame mapped to Core's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentExit {
    /// How the job ended.
    pub kind: ComponentExitKind,
    /// Kill scope reported by the supervisor.
    pub kill_scope: ComponentKillScope,
    /// OOM verdict from per-job evidence.
    pub oom: ComponentOomVerdict,
    /// Every stdout byte the child wrote.
    pub stdout_total: u64,
    /// Stdout bytes evicted by the component's output bound.
    pub stdout_evicted: u64,
    /// Every stderr byte the child wrote.
    pub stderr_total: u64,
    /// Stderr bytes evicted by the component's output bound.
    pub stderr_evicted: u64,
}

/// Point-in-time view of one component-tracked job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentJobSnapshot {
    /// Host-assigned job id.
    pub job: u64,
    /// Execution generation from `Spawned`.
    pub generation: Option<u64>,
    /// The component reported the process exists.
    pub started: bool,
    /// Typed terminate answer, when one arrived.
    pub canceled: Option<ComponentTerminateOutcome>,
    /// Terminal outcome, when one arrived.
    pub exit: Option<ComponentExit>,
    /// Newest retained stdout bytes, lossy UTF-8.
    pub stdout: String,
    /// Newest retained stderr bytes, lossy UTF-8.
    pub stderr: String,
    /// Streamed stdout bytes observed by the host.
    pub stdout_total: u64,
    /// Streamed stderr bytes observed by the host.
    pub stderr_total: u64,
    /// Some stdout byte was lost to the component or host bound.
    pub stdout_truncated: bool,
    /// Some stderr byte was lost to the component or host bound.
    pub stderr_truncated: bool,
}

/// One completed component run shaped for the execution-job bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRun {
    /// Host-assigned job id.
    pub job: u64,
    /// Terminal outcome and byte evidence.
    pub exit: ComponentExit,
    /// Newest retained stdout bytes, lossy UTF-8.
    pub stdout: String,
    /// Newest retained stderr bytes, lossy UTF-8.
    pub stderr: String,
    /// Streamed stdout bytes observed by the host.
    pub stdout_total: u64,
    /// Streamed stderr bytes observed by the host.
    pub stderr_total: u64,
    /// Some stdout byte was lost to the component or host bound.
    pub stdout_truncated: bool,
    /// Some stderr byte was lost to the component or host bound.
    pub stderr_truncated: bool,
}

/// One retained-output read response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentOutput {
    /// Selected lossy UTF-8 bytes (at most 240 KiB).
    pub data: Vec<u8>,
    /// Every byte the child wrote to this stream.
    pub total: u64,
    /// Bytes lost to the component's per-stream bound.
    pub dropped: u64,
    /// Whether any byte of this stream was lost.
    pub truncated: bool,
    /// Lines carried by `data`.
    pub lines: u32,
}

/// Failure from the execution component host client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentError {
    /// The component path is disabled by the gate.
    Disabled,
    /// Safe mode denies the component path before any I/O.
    SafeMode,
    /// No `execution` component is installed in either tier.
    NotInstalled {
        /// Component name.
        name: String,
    },
    /// The installed component does not speak protocol v1.
    Incompatible {
        /// Lowest protocol version the component supports.
        min: u16,
        /// Highest protocol version the component supports.
        max: u16,
    },
    /// The install failed verification (descriptor, digest, layout).
    UnsafeInstall {
        /// Bounded reason.
        reason: String,
    },
    /// The component process could not be spawned or its pipes failed.
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// I/O error kind.
        kind: io::ErrorKind,
    },
    /// A bounded operation deadline elapsed.
    Timeout {
        /// Operation that timed out.
        operation: &'static str,
    },
    /// The component is unavailable for a reason that permits fallback.
    Unavailable {
        /// Bounded reason.
        reason: String,
    },
    /// The component process ended while the host still needed it.
    ComponentLost {
        /// Bounded reason.
        reason: String,
    },
    /// The component violated the wire protocol or a host-side invariant.
    Protocol {
        /// Bounded reason.
        reason: String,
    },
    /// The component already tracks the maximum number of jobs.
    JobLimit {
        /// The bound.
        limit: usize,
    },
    /// A request or reply violated a bound before any execution.
    InvalidRequest {
        /// Bounded reason.
        reason: String,
    },
}

impl fmt::Display for ComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("execution component path is disabled"),
            Self::SafeMode => f.write_str("execution component is unavailable in safe mode"),
            Self::NotInstalled { name } => {
                write!(f, "execution component '{name}' is not installed")
            }
            Self::Incompatible { min, max } => write!(
                f,
                "execution component protocol [{min}, {max}] does not include v{EXECUTION_COMPONENT_PROTOCOL_MIN}"
            ),
            Self::UnsafeInstall { reason } => {
                write!(f, "execution component install rejected: {reason}")
            }
            Self::Io { operation, kind } => {
                write!(f, "execution component {operation} failed: {kind}")
            }
            Self::Timeout { operation } => write!(f, "execution component {operation} timed out"),
            Self::Unavailable { reason } => write!(f, "execution component unavailable: {reason}"),
            Self::ComponentLost { reason } => write!(f, "execution component lost: {reason}"),
            Self::Protocol { reason } => {
                write!(f, "execution component protocol violation: {reason}")
            }
            Self::JobLimit { limit } => {
                write!(f, "execution component already tracks {limit} jobs")
            }
            Self::InvalidRequest { reason } => {
                write!(f, "invalid execution component request: {reason}")
            }
        }
    }
}

impl std::error::Error for ComponentError {}

fn invalid(reason: &str) -> ComponentError {
    ComponentError::InvalidRequest {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_defaults_to_disabled_and_parses_labels() {
        assert_eq!(
            ExecutionComponentMode::default(),
            ExecutionComponentMode::Disabled
        );
        for mode in [
            ExecutionComponentMode::Disabled,
            ExecutionComponentMode::Preferred,
            ExecutionComponentMode::Required,
        ] {
            assert_eq!(ExecutionComponentMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ExecutionComponentMode::parse("on"), None);
        assert_eq!(
            ComponentGateConfig::default().mode,
            ExecutionComponentMode::Disabled
        );
    }

    #[test]
    fn spawn_spec_defaults_and_bounds_fail_closed() {
        let spec = ComponentSpawnSpec::new("/bin/true", Vec::new());
        assert_eq!(spec.hard_timeout_ms, EXECUTION_COMPONENT_DEFAULT_TIMEOUT_MS);
        spec.validate().expect("default spec is valid");

        let mut too_many = spec.clone();
        too_many.args = vec!["x".to_owned(); MAX_EXEC_ARGS + 1];
        assert!(matches!(
            too_many.validate(),
            Err(ComponentError::InvalidRequest { .. })
        ));

        let mut nul = spec.clone();
        nul.program.push('\0');
        assert!(matches!(
            nul.validate(),
            Err(ComponentError::InvalidRequest { .. })
        ));

        let mut no_deadline = spec.clone();
        no_deadline.hard_timeout_ms = 0;
        assert!(matches!(
            no_deadline.validate(),
            Err(ComponentError::InvalidRequest { .. })
        ));

        let mut over_deadline = spec.clone();
        over_deadline.hard_timeout_ms =
            u32::try_from(bitty_ipc::execution::MAX_EXEC_TIMEOUT_MS + 1).expect("fits");
        assert!(matches!(
            over_deadline.validate(),
            Err(ComponentError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn explicit_environment_validates_names_and_total() {
        assert!(
            ComponentEnvironment::explicit(vec![("MODE".to_owned(), "test".to_owned())]).is_ok()
        );
        assert!(
            ComponentEnvironment::explicit(vec![("bad-name".to_owned(), "x".to_owned())]).is_err()
        );
        assert!(
            ComponentEnvironment::explicit(vec![("NUL".to_owned(), "a\0b".to_owned())]).is_err()
        );
    }
}
