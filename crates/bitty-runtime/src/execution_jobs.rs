//! Tool/plugin spawns supervised by the extracted `bitty-execution` registry
//! (issue #2249, W-140 integration).
//!
//! Core keeps the permission gate and the process-confinement policy; the
//! extracted supervisor owns job identity, lifecycle, cancellation, and
//! bounded output. This module and the #2494 host client
//! (`crate::execution_component`) are the only Core sources that name the
//! extension implementation (W-147 fence exception), and this module exposes
//! only Core/`bitty-ipc` types to the spawn surface.
//!
//! # Execution-component gate (bitty#2494)
//!
//! The installable `execution` component can replace the linked registry for
//! the same spawn surface. The gate is **default-off**
//! ([`ExecutionComponentMode::Disabled`]); a host injects
//! [`ComponentGateConfig`] through `SpawnService::with_execution_component`.
//! `Preferred` uses the component when it is installed and compatible, and
//! falls back to the linked registry only when the component was never
//! entered (nothing executed). `Required` fails closed instead. Safe mode
//! denies the component path before any install resolution. A component that
//! was entered and then failed never falls back, so no request can execute
//! twice.
//!
//! # What a tool spawn gets
//!
//! - A real [`JobId`] minted by the registry, never a caller-chosen id.
//! - The registry lifecycle (`Queued` -> `Running` -> `Done(outcome)`) and
//!   the bounded critical/observation event log.
//! - Owned-tree cancellation: a cancel goes through
//!   [`JobRegistry::cancel_as`] and the supervisor kills the recorded job's
//!   tree. No code here signals or kills a pid.
//! - Newest-wins bounded output read through the registry, with the tail and
//!   truncation flags taken from the registry's metadata.
//!
//! # Preserved Core controls
//!
//! - The caller still passes `process.spawn:<family>` scope, explicit effect
//!   opt-in, and the consent ledger before this module is reached; a denial
//!   never registers a job.
//! - CTX-1151 Landlock `EXECUTE` confinement is applied by
//!   [`SandboxPreparer`] to the supervisor-built command. A confinement setup
//!   failure refuses the spawn before the registry is asked to track it, and
//!   the preparer re-checks the same tool shape before every pipe spawn.
//! - The supervisor's own refusal set (`RegistryFull`, `InvalidSpec`,
//!   `Denied`, `CommandRisk*`, `Unavailable`, ...) is mapped to the existing
//!   `bitty-ipc` failure set; nothing falls back to a direct
//!   `std::process::Command`.
//!
//! PTY shells are deliberately excluded: interactive PTY ownership stays
//! with the terminal primitive (DEC-0017), and the preparer is never applied
//! to a PTY job.

use std::any::Any;
use std::cell::RefCell;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bitty_execution::{
    ExecutionOutcome, JobError, JobIo, JobKind, JobLifetime, JobOrigin, JobPrincipal, JobRegistry,
    JobSpec, JobTimeouts, OutputStream, OutputView, ReadOutput, SpawnPreparer,
};
use bitty_ipc::IpcError;
use bitty_ipc::execution::{
    EffectState, EnvPolicy, ExecutionRequest, ExecutionStatus, RawExecutionOutput,
};
use bitty_plugin_host::tools::is_tool_spawn_allowed;

use crate::execution_component::{
    ComponentError, ComponentExecutionHost, ComponentExitKind, ComponentGateConfig, ComponentRun,
    ComponentSpawnKind, ComponentSpawnSpec, EXECUTION_COMPONENT_RETENTION_MS,
    ExecutionComponentMode,
};

/// Extra time the synchronous bridge grants the supervisor after the job's
/// hard deadline so it can kill the owned tree, reap the leader, and join the
/// bounded drains before the terminal event. Matches the registry's 5 s
/// drain-join bound plus scheduling slack.
const JOB_WAIT_GRACE: Duration = Duration::from_secs(6);

/// Extra time granted after a fallback cancel request when the supervisor did
/// not publish a terminal event within [`JOB_WAIT_GRACE`].
const JOB_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Principal used by the direct IPC provider when no plugin dispatch context
/// is active (the host-bridge path). Plugin dispatches carry their own
/// authenticated client identity instead.
const IPC_PRINCIPAL: &str = "ipc-execution";

/// Re-exported job identity for the Core spawn surface.
///
/// The type is upstream, but keeping the import here confines the extension
/// identifier to this seam module (W-147 source fence).
pub(crate) use bitty_execution::JobId;

/// Test-only re-exports for the spawn surface's registry assertions.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) use bitty_execution::{
        CancelOutcome, CancelReceipt, CancelRequest, ExecutionGeneration, ExecutionHandle,
        ExecutionOutcome, JobCancel, JobError, JobEvent, JobIo, JobKind, JobLifetime, JobOrigin,
        JobPrincipal, JobRegistry, JobSnapshot, JobSpec, JobState, JobTimeouts, OutputStream,
        ReadOutput, SidecarPolicy,
    };
}

/// Builds the registry principal for one authenticated plugin caller.
pub(crate) fn job_principal(name: &str) -> Result<JobPrincipal, IpcError> {
    JobPrincipal::new(name).map_err(map_job_error)
}

/// One completed (or terminally failed) supervised tool run.
#[derive(Debug)]
pub(crate) struct JobRun {
    /// Real registry or component identity minted for this spawn.
    pub job: JobId,
    /// Outcome and retained (not yet caller-budget-truncated) output shaped
    /// for the existing `bitty-ipc` synchronous surface.
    pub raw: RawExecutionOutput,
    /// Whether any output byte was lost to the registry store or the caller's
    /// stream budget. Computed from byte totals, not from the line-joined
    /// text length, so newline-heavy output still reports truncation.
    pub truncated: bool,
    /// Per-stream lost-byte signal for the `ExecutionService` provider path.
    pub stdout_truncated: bool,
    /// Per-stream lost-byte signal for the `ExecutionService` provider path.
    pub stderr_truncated: bool,
}

/// Why an execution component attempt did not produce a run.
///
/// `NotUsable` is the only variant a `Preferred` gate may answer with the
/// linked registry: the component never received a `Spawn`, so falling back
/// cannot execute anything twice. A denial and every post-submission failure
/// fail closed.
#[derive(Debug)]
pub(crate) enum ComponentRunError {
    /// No usable component install or connection; nothing was submitted.
    NotUsable(String),
    /// The host denied the component path (for example safe mode).
    Denied(IpcError),
    /// The component path was entered and failed; never fall back.
    Failed(IpcError),
}

/// Injectable execution-component backend for the gate.
///
/// The production implementation is [`ExecutionComponentGate`]; tests use a
/// deterministic fake so the fallback/denial policy is provable without a
/// process.
pub(crate) trait ComponentRunner: Send {
    /// Configured mode (used to decide whether `NotUsable` may fall back).
    fn mode(&self) -> ExecutionComponentMode;

    /// Run one request through the component.
    fn spawn_and_wait(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<ComponentRun, ComponentRunError>;
}

/// Lazy execution-component gate.
///
/// Resolution and the handshake happen on the first spawn. A failed
/// connection is never cached as a permanent verdict; the next attempt
/// re-resolves, so installing or repairing the component takes effect
/// without restarting Core. A component that was already entered is dropped
/// on any error and the error is returned without a linked fallback.
struct ExecutionComponentGate {
    config: ComponentGateConfig,
    host: Option<ComponentExecutionHost>,
}

impl ExecutionComponentGate {
    fn new(config: ComponentGateConfig) -> Self {
        Self { config, host: None }
    }

    fn connect(&mut self) -> Result<&mut ComponentExecutionHost, ComponentRunError> {
        if self.host.is_none() {
            match ComponentExecutionHost::connect(self.config.host.clone()) {
                Ok(host) => self.host = Some(host),
                Err(ComponentError::SafeMode) => {
                    return Err(ComponentRunError::Denied(IpcError::Denied {
                        code: "SafeMode".to_owned(),
                        reason: ComponentError::SafeMode.to_string(),
                    }));
                }
                Err(error) => {
                    return Err(ComponentRunError::NotUsable(component_failure_class(
                        &error,
                    )));
                }
            }
        }
        self.host
            .as_mut()
            .ok_or_else(|| ComponentRunError::NotUsable("component host vanished".to_owned()))
    }
}

impl ComponentRunner for ExecutionComponentGate {
    fn mode(&self) -> ExecutionComponentMode {
        self.config.mode
    }

    fn spawn_and_wait(
        &mut self,
        request: &ExecutionRequest,
    ) -> Result<ComponentRun, ComponentRunError> {
        if self.config.host.safe_mode {
            return Err(ComponentRunError::Denied(IpcError::Denied {
                code: "SafeMode".to_owned(),
                reason: ComponentError::SafeMode.to_string(),
            }));
        }
        if self.config.mode == ExecutionComponentMode::Disabled {
            return Err(ComponentRunError::NotUsable(
                "execution component path is disabled".to_owned(),
            ));
        }
        let spec = component_spec(request)?;
        let host = self.connect()?;
        match host.spawn_and_wait(&spec) {
            Ok(run) => Ok(run),
            Err(error) => {
                // The host is single-connection: any post-submission failure
                // poisons it. Reconnect on the next request only; this request
                // never falls back.
                self.host = None;
                Err(ComponentRunError::Failed(map_component_error(error)))
            }
        }
    }
}

/// Converts one `bitty-ipc` request into the component's bounded spawn shape.
fn component_spec(request: &ExecutionRequest) -> Result<ComponentSpawnSpec, ComponentRunError> {
    request.validate().map_err(ComponentRunError::Failed)?;
    // CTX-1151 confinement is applied by `SandboxPreparer` to the linked
    // registry's child. The component is policy-free and has no wire field
    // for a Landlock ruleset, so an admitted `[tools.*]` spawn cannot carry
    // Core's exec confinement across this boundary. Never run one
    // unconfined: `Preferred` falls back to the linked path and `Required`
    // fails closed.
    if is_tool_spawn_allowed(&request.executable, &request.args) {
        return Err(ComponentRunError::NotUsable(
            "admitted tool spawns require Core exec confinement; the execution component path cannot apply it"
                .to_owned(),
        ));
    }
    let env = match &request.env_policy {
        EnvPolicy::Isolated => crate::execution_component::ComponentEnvironment::Isolated,
        EnvPolicy::Explicit { vars } => crate::execution_component::ComponentEnvironment::Explicit(
            vars.iter()
                .map(|var| (var.name.clone(), var.value.clone()))
                .collect(),
        ),
    };
    let timeout_ms = u32::try_from(request.timeout_ms).map_err(|_| {
        ComponentRunError::Failed(IpcError::LimitExceeded {
            field: "timeout_ms".to_owned(),
            limit: u32::MAX as usize,
            actual: usize::try_from(request.timeout_ms).unwrap_or(usize::MAX),
        })
    })?;
    let spec = ComponentSpawnSpec::new(request.executable.clone(), request.args.clone())
        .with_cwd(request.cwd.clone())
        .with_env(env)
        .with_kind(ComponentSpawnKind::Command)
        .with_hard_timeout_ms(timeout_ms)
        .with_retention_ms(EXECUTION_COMPONENT_RETENTION_MS);
    spec.validate()
        .map_err(|error| ComponentRunError::Failed(map_component_error(error)))?;
    Ok(spec)
}

/// Stable, path-free class for one component failure.
///
/// Install and I/O errors can carry host paths in their `Display` form; the
/// plugin-facing synchronous error set never needs them, so the bridge maps
/// to a bounded class and leaves full diagnostics to the host client.
fn component_failure_class(error: &ComponentError) -> String {
    match error {
        ComponentError::Disabled => "execution component path is disabled".to_owned(),
        ComponentError::SafeMode => "execution component is unavailable in safe mode".to_owned(),
        ComponentError::NotInstalled { .. } => "execution component is not installed".to_owned(),
        ComponentError::Incompatible { .. } => {
            "execution component protocol is incompatible".to_owned()
        }
        ComponentError::UnsafeInstall { .. } => {
            "execution component install verification failed".to_owned()
        }
        ComponentError::Io { .. } => "execution component process I/O failed".to_owned(),
        ComponentError::Timeout { .. } => "execution component operation timed out".to_owned(),
        ComponentError::Unavailable { reason } => reason.clone(),
        ComponentError::ComponentLost { .. } => "execution component was lost".to_owned(),
        ComponentError::Protocol { .. } => "execution component protocol violation".to_owned(),
        ComponentError::JobLimit { limit } => {
            format!("execution component job capacity reached ({limit})")
        }
        ComponentError::InvalidRequest { reason } => reason.clone(),
    }
}

/// Maps every component-client failure into the existing synchronous set.
///
/// Nothing here falls back to a second spawn; a denial stays a denial and a
/// lost/protocol/timeout failure stays `Unavailable`.
pub(crate) fn map_component_error(error: ComponentError) -> IpcError {
    match error {
        ComponentError::SafeMode | ComponentError::Disabled => IpcError::Denied {
            code: "ExecutionComponentUnavailable".to_owned(),
            reason: error.to_string(),
        },
        ComponentError::InvalidRequest { reason } => IpcError::InvalidRequest { reason },
        ComponentError::JobLimit { limit } => IpcError::LimitExceeded {
            field: "execution jobs".to_owned(),
            limit,
            actual: limit.saturating_add(1),
        },
        ComponentError::Unavailable { .. }
        | ComponentError::NotInstalled { .. }
        | ComponentError::Incompatible { .. }
        | ComponentError::UnsafeInstall { .. }
        | ComponentError::Io { .. }
        | ComponentError::Timeout { .. }
        | ComponentError::ComponentLost { .. }
        | ComponentError::Protocol { .. } => IpcError::Unavailable {
            reason: component_failure_class(&error),
        },
    }
}

/// Maps a component terminal outcome into the supervisor's vocabulary.
fn component_outcome(kind: ComponentExitKind) -> ExecutionOutcome {
    match kind {
        ComponentExitKind::Success => ExecutionOutcome::Success,
        ComponentExitKind::ExitCode(code) => ExecutionOutcome::ExitCode(code),
        ComponentExitKind::Signaled(signal) => ExecutionOutcome::Signaled(signal),
        ComponentExitKind::SpawnFailed => ExecutionOutcome::SpawnFailed,
        ComponentExitKind::Cancelled(effect) => ExecutionOutcome::Cancelled(match effect {
            crate::execution_component::ComponentCancelEffect::BeforeStart => {
                bitty_execution::CancelEffect::BeforeStart
            }
            crate::execution_component::ComponentCancelEffect::Graceful => {
                bitty_execution::CancelEffect::Graceful
            }
            crate::execution_component::ComponentCancelEffect::Killed => {
                bitty_execution::CancelEffect::Killed
            }
        }),
        ComponentExitKind::TimedOut(clock) => ExecutionOutcome::TimedOut(match clock {
            crate::execution_component::ComponentDeadlineClock::Hard => {
                bitty_execution::DeadlineClock::Hard
            }
            crate::execution_component::ComponentDeadlineClock::Idle => {
                bitty_execution::DeadlineClock::Idle
            }
        }),
        ComponentExitKind::OomKilled => ExecutionOutcome::OomKilled,
        ComponentExitKind::SupervisorLost => ExecutionOutcome::SupervisorLost,
        ComponentExitKind::Unknown => ExecutionOutcome::Unknown,
    }
}

/// Core-owned wrapper around the extracted [`JobRegistry`] for tool spawns.
///
/// Cheap to clone: clones share the same registry and preparer, so a host can
/// hand one clone to a cancellation path while the synchronous dispatch waits.
#[derive(Clone)]
pub(crate) struct ToolJobSupervisor {
    registry: JobRegistry,
    /// Optional execution-component gate. `None` is the shipped default: the
    /// linked registry is the only path.
    component: Option<Arc<Mutex<Box<dyn ComponentRunner>>>>,
}

impl std::fmt::Debug for ToolJobSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolJobSupervisor")
            .field("capacity", &self.registry.capacity())
            .field("tracked", &self.registry.len())
            .field("component", &self.component.is_some())
            .finish()
    }
}

impl ToolJobSupervisor {
    /// Registry with the default capacity and the Core confinement preparer.
    pub(crate) fn new() -> Self {
        Self {
            registry: JobRegistry::new().with_spawn_preparer(Arc::new(SandboxPreparer)),
            component: None,
        }
    }

    /// Registry plus an execution-component gate.
    ///
    /// `Disabled` is equivalent to [`Self::new`]. `Preferred` falls back to
    /// the linked registry only when the component was never entered;
    /// `Required` fails closed instead.
    pub(crate) fn with_component(config: ComponentGateConfig) -> Self {
        let mut supervisor = Self::new();
        if config.mode != ExecutionComponentMode::Disabled {
            supervisor.component = Some(Arc::new(Mutex::new(Box::new(
                ExecutionComponentGate::new(config),
            ))));
        }
        supervisor
    }

    /// Test seam: an explicit runner, including fake gates.
    #[cfg(test)]
    pub(crate) fn with_component_runner(runner: Box<dyn ComponentRunner>) -> Self {
        let mut supervisor = Self::new();
        supervisor.component = Some(Arc::new(Mutex::new(runner)));
        supervisor
    }

    /// Registry with an explicit capacity (tests and bounded hosts).
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0`, matching [`JobRegistry::with_capacity`].
    #[cfg(test)]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            registry: JobRegistry::with_capacity(capacity)
                .with_spawn_preparer(Arc::new(SandboxPreparer)),
            component: None,
        }
    }

    /// Shared registry clone for tests and cancellation paths.
    #[cfg(test)]
    pub(crate) fn registry(&self) -> JobRegistry {
        self.registry.clone()
    }

    /// Spawns a job and waits for its terminal state, returning bounded
    /// output shaped for the synchronous execution service.
    ///
    /// The component gate runs first when configured. A `Preferred` gate may
    /// use the linked path only for a `NotUsable` component (nothing was
    /// submitted); every other component failure is returned as-is.
    pub(crate) fn spawn_and_wait(
        &self,
        request: &ExecutionRequest,
        principal: &JobPrincipal,
        poll_interval: Duration,
    ) -> Result<JobRun, IpcError> {
        if let Some(component) = &self.component {
            let mut runner = lock_runner(component);
            let mode = runner.mode();
            match runner.spawn_and_wait(request) {
                Ok(run) => return finish_component_run(run, request),
                Err(ComponentRunError::NotUsable(reason)) => {
                    if mode == ExecutionComponentMode::Preferred {
                        // Nothing was submitted; fall through to the linked
                        // registry for this release.
                    } else {
                        return Err(IpcError::Unavailable { reason });
                    }
                }
                Err(ComponentRunError::Denied(error)) | Err(ComponentRunError::Failed(error)) => {
                    return Err(error);
                }
            }
        }
        self.spawn_linked(request, principal, poll_interval)
    }

    /// Linked-registry path (the shipped fallback for this release).
    fn spawn_linked(
        &self,
        request: &ExecutionRequest,
        principal: &JobPrincipal,
        poll_interval: Duration,
    ) -> Result<JobRun, IpcError> {
        // Preserve CTX-1151 fail-closed behavior for admitted tools: resolve
        // the executable closure and build the Landlock ruleset once before
        // anything is tracked. The registry's preparer repeats the setup on
        // the real command; a repeated failure still fails closed as
        // `SpawnFailed`.
        validate_tool_confinement(request)?;
        let spec = JobSpec::new(request.executable.clone(), request.args.clone())
            .with_cwd(request.cwd.clone())
            .with_env(request.env_policy.clone())
            .with_kind(JobKind::Command)
            .with_lifetime(JobLifetime::Detached)
            .with_io(JobIo::Pipes)
            .with_timeouts(
                JobTimeouts::default()
                    .with_hard(Duration::from_millis(request.timeout_ms))
                    // The synchronous bridge reads both streams before it
                    // returns. Zero retention lets the next spawn on this
                    // registry evict the finished record instead of keeping
                    // 60 s of completed jobs that would exhaust
                    // `JobRegistry` capacity (`RegistryFull`). The caller's
                    // result/handle maps remain the durable synchronous view.
                    .with_retention(Duration::ZERO),
            )
            .with_origin(JobOrigin::none());
        let job = self
            .registry
            .spawn_as(principal.clone(), spec)
            .map_err(map_job_error)?;
        let snapshot = self.wait_terminal(principal, job, request.timeout_ms, poll_interval)?;
        let outcome = snapshot.state.outcome().ok_or_else(|| IpcError::Internal {
            reason: format!("job {job} reported a terminal snapshot without an outcome"),
        })?;
        let stdout = self.read_stream(principal, job, OutputStream::Stdout)?;
        let stderr = self.read_stream(principal, job, OutputStream::Stderr)?;
        let stdout_total = stdout.total_bytes;
        let stderr_total = stderr.total_bytes;
        let stdout_registry_truncated = stdout.truncated;
        let stderr_registry_truncated = stderr.truncated;
        let (status, effect_state, exit_code, mut evidence) =
            classify_outcome(job, outcome, request, stdout_total, stderr_total);
        if matches!(outcome, ExecutionOutcome::SpawnFailed) {
            return Err(IpcError::Unavailable {
                reason: format!("job {job} failed to spawn under the execution supervisor"),
            });
        }
        let budget = request.effective_stream_budget() as u64;
        let stdout_truncated = stdout_registry_truncated || stdout_total > budget;
        let stderr_truncated = stderr_registry_truncated || stderr_total > budget;
        let truncated = stdout_truncated || stderr_truncated;
        if truncated {
            evidence.push(format!(
                "job {job} output was bounded; newest bytes preserved"
            ));
        }
        Ok(JobRun {
            job,
            raw: RawExecutionOutput {
                target_id: request.target.clone(),
                status,
                exit_code,
                stdout: restore_trailing_newline(stdout),
                stderr: restore_trailing_newline(stderr),
                evidence_refs: evidence,
                effect_state,
            },
            truncated,
            stdout_truncated,
            stderr_truncated,
        })
    }

    /// Reads one bounded stream from a terminal job.
    fn read_stream(
        &self,
        principal: &JobPrincipal,
        job: JobId,
        stream: OutputStream,
    ) -> Result<OutputView, IpcError> {
        // Read the whole retained tail (bounded by the registry's 256 KiB
        // per-stream store) and let the synchronous `ExecutionService` apply
        // the caller's stream budget. Reading only `budget + 1` here would
        // lose the truncation signal when line-joining shortens the tail
        // below the budget even though the child emitted more bytes.
        self.registry
            .read_output_as(principal, job, ReadOutput::new(stream))
            .map_err(map_job_error)
    }

    /// Waits for the terminal snapshot without blocking the supervisor.
    ///
    /// The registry's own hard deadline ends the job; this wait only bounds
    /// how long the synchronous caller may block on a wedged supervisor. If
    /// the grace elapses, a typed cancel is requested through the registry
    /// (never a pid kill) and the wait continues for the cancel grace.
    fn wait_terminal(
        &self,
        principal: &JobPrincipal,
        job: JobId,
        timeout_ms: u64,
        poll_interval: Duration,
    ) -> Result<bitty_execution::JobSnapshot, IpcError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms).saturating_add(JOB_WAIT_GRACE))
            .unwrap_or_else(Instant::now);
        loop {
            let snapshot = self
                .registry
                .get_as(principal, job)
                .map_err(map_job_error)?;
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(poll_interval.max(Duration::from_millis(1)));
        }
        self.registry
            .cancel_as(principal, job)
            .map_err(map_job_error)?;
        let cancel_deadline = Instant::now()
            .checked_add(JOB_CANCEL_GRACE)
            .unwrap_or_else(Instant::now);
        loop {
            let snapshot = self
                .registry
                .get_as(principal, job)
                .map_err(map_job_error)?;
            if snapshot.state.is_terminal() {
                return Ok(snapshot);
            }
            if Instant::now() >= cancel_deadline {
                return Err(IpcError::Unavailable {
                    reason: format!(
                        "job {job} did not reach a terminal state after the supervisor cancel"
                    ),
                });
            }
            std::thread::sleep(poll_interval.max(Duration::from_millis(1)));
        }
    }
}

/// Locks a component runner, recovering from a poisoned mutex.
fn lock_runner(
    component: &Arc<Mutex<Box<dyn ComponentRunner>>>,
) -> MutexGuard<'_, Box<dyn ComponentRunner>> {
    component.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shapes one completed component run exactly like the linked registry path.
///
/// The outcome, evidence, status/effect classification, and caller stream
/// budget are the same functions the linked path uses, so parity is in the
/// mapping rather than in a second vocabulary.
fn finish_component_run(run: ComponentRun, request: &ExecutionRequest) -> Result<JobRun, IpcError> {
    let job = JobId::from_raw(run.job).ok_or_else(|| IpcError::Internal {
        reason: "execution component minted a zero job id".to_owned(),
    })?;
    let outcome = component_outcome(run.exit.kind);
    let (status, effect_state, exit_code, mut evidence) =
        classify_outcome(job, outcome, request, run.stdout_total, run.stderr_total);
    if matches!(outcome, ExecutionOutcome::SpawnFailed) {
        return Err(IpcError::Unavailable {
            reason: format!("job {job} failed to spawn under the execution component"),
        });
    }
    let budget = request.effective_stream_budget() as u64;
    let stdout_truncated = run.stdout_truncated || run.stdout_total > budget;
    let stderr_truncated = run.stderr_truncated || run.stderr_total > budget;
    let truncated = stdout_truncated || stderr_truncated;
    if truncated {
        evidence.push(format!(
            "job {job} output was bounded; newest bytes preserved"
        ));
    }
    if let Some(label) = component_evidence_label(&run) {
        evidence.push(label);
    }
    Ok(JobRun {
        job,
        raw: RawExecutionOutput {
            target_id: request.target.clone(),
            status,
            exit_code,
            stdout: run.stdout,
            stderr: run.stderr,
            evidence_refs: evidence,
            effect_state,
        },
        truncated,
        stdout_truncated,
        stderr_truncated,
    })
}

/// Per-stream truncation evidence for a component run.
fn component_evidence_label(run: &ComponentRun) -> Option<String> {
    if run.stdout_truncated || run.stderr_truncated {
        Some(format!(
            "job {} component retained stdout {} of {} bytes, stderr {} of {} bytes",
            run.job,
            run.stdout.len(),
            run.stdout_total,
            run.stderr.len(),
            run.stderr_total
        ))
    } else {
        None
    }
}

/// Maps every registry refusal into the existing synchronous failure set.
///
/// There is no catch-all success path: an unrecognized registry error fails
/// closed as `Unavailable`, and nothing spawns a second time.
pub(crate) fn map_job_error(error: JobError) -> IpcError {
    match error {
        JobError::RegistryFull { limit } => IpcError::LimitExceeded {
            field: "execution jobs".into(),
            limit,
            actual: limit.saturating_add(1),
        },
        JobError::InvalidSpec { reason }
        | JobError::InvalidPrincipal { reason }
        | JobError::InvalidRead { reason }
        | JobError::InvalidWrite { reason }
        | JobError::InvalidCancel { reason }
        | JobError::InvalidCursor { reason } => IpcError::InvalidRequest { reason },
        JobError::Denied { operation, reason } => IpcError::Denied {
            code: format!("ExecutionJobDenied({operation})"),
            reason,
        },
        JobError::CommandRiskDenied { deny, reason } => IpcError::Denied {
            code: format!("CommandRiskDenied({deny})"),
            reason,
        },
        JobError::CommandRiskNeedsConsent { tier, reason } => IpcError::Denied {
            code: format!("CommandRiskNeedsConsent({tier})"),
            reason,
        },
        JobError::SecureInputDenied { denial, reason } => IpcError::Denied {
            code: format!("SecureInputDenied({denial})"),
            reason,
        },
        JobError::UnknownJob(id) => IpcError::NotFound {
            reason: format!("unknown job {id}"),
        },
        JobError::UnknownEvent { seq } => IpcError::NotFound {
            reason: format!("unknown job event {seq}"),
        },
        JobError::Unavailable { reason }
        | JobError::Unsupported { reason }
        | JobError::RateLimited { reason, .. }
        | JobError::SignalRateLimited { reason } => IpcError::Unavailable { reason },
        JobError::GrantsFull { limit } => IpcError::LimitExceeded {
            field: "job grants".into(),
            limit,
            actual: limit.saturating_add(1),
        },
    }
}

/// Classifies one terminal registry outcome for the synchronous surface.
///
/// Timeout stays `Unknown` (reconcile, never a clean failure) exactly like
/// the previous one-shot provider; a cancel stays `Canceled`; a signal and
/// OOM evidence stay failures. The registry's job id is always in evidence
/// so the caller can correlate the real identity.
fn classify_outcome(
    job: JobId,
    outcome: ExecutionOutcome,
    request: &ExecutionRequest,
    stdout_total: u64,
    stderr_total: u64,
) -> (ExecutionStatus, EffectState, Option<i32>, Vec<String>) {
    match outcome {
        ExecutionOutcome::Success => (
            ExecutionStatus::Completed,
            EffectState::Completed,
            Some(0),
            vec![format!("job {job} completed")],
        ),
        ExecutionOutcome::ExitCode(code) => (
            ExecutionStatus::Failed,
            EffectState::Failed,
            Some(code),
            vec![format!("job {job} exited with code {code}")],
        ),
        ExecutionOutcome::Signaled(signal) => (
            ExecutionStatus::Failed,
            EffectState::Failed,
            None,
            vec![format!("job {job} ended by signal {signal}")],
        ),
        ExecutionOutcome::Cancelled(effect) => (
            ExecutionStatus::Canceled,
            EffectState::Canceled,
            None,
            vec![format!("job {job} canceled ({})", effect.as_str())],
        ),
        ExecutionOutcome::TimedOut(clock) => (
            ExecutionStatus::Unknown,
            EffectState::Unknown,
            None,
            vec![format!(
                "spawn timeout after {} ms; job {job} timed out ({}) and its owned tree was killed and reaped; stdout {} bytes, stderr {} bytes preserved",
                request.timeout_ms,
                clock.as_str(),
                stdout_total,
                stderr_total
            )],
        ),
        ExecutionOutcome::SpawnFailed => (
            ExecutionStatus::Failed,
            EffectState::Failed,
            None,
            vec![format!("job {job} failed to spawn")],
        ),
        ExecutionOutcome::OomKilled => (
            ExecutionStatus::Failed,
            EffectState::Failed,
            None,
            vec![format!("job {job} was oom-killed")],
        ),
        ExecutionOutcome::SupervisorLost => (
            ExecutionStatus::Unknown,
            EffectState::Unknown,
            None,
            vec![format!("job {job} lost its supervisor")],
        ),
        ExecutionOutcome::Unknown => (
            ExecutionStatus::Unknown,
            EffectState::Unknown,
            None,
            vec![format!("job {job} outcome is unknown")],
        ),
    }
}

/// Host-wide spawn context for one synchronous plugin dispatch.
///
/// The `bitty-ipc` provider slot is a bare `fn`, so the authenticated plugin
/// identity and registry clone for one dispatch travel through a thread-local
/// scope around [`ExecutionService::dispatch`](bitty_ipc::ExecutionService).
/// The plugin VM and the provider run on the same thread; a nested dispatch
/// restores the enclosing scope on drop.
struct DispatchContext {
    supervisor: ToolJobSupervisor,
    principal: JobPrincipal,
}

thread_local! {
    static ACTIVE_DISPATCH: RefCell<Option<DispatchContext>> = const { RefCell::new(None) };
    static DEFAULT_SUPERVISOR: RefCell<Option<ToolJobSupervisor>> = const { RefCell::new(None) };
}

/// Scoped plugin dispatch context; restores the previous scope on drop.
pub(crate) struct DispatchScope {
    previous: Option<DispatchContext>,
}

impl DispatchScope {
    /// Enters a dispatch scope for `principal` over `supervisor`.
    pub(crate) fn enter(supervisor: ToolJobSupervisor, principal: JobPrincipal) -> Self {
        let context = DispatchContext {
            supervisor,
            principal,
        };
        let previous = ACTIVE_DISPATCH.with(|slot| slot.replace(Some(context)));
        Self { previous }
    }
}

impl Drop for DispatchScope {
    fn drop(&mut self) {
        ACTIVE_DISPATCH.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

/// Returns the active dispatch supervisor/principal pair, if any.
fn active_dispatch() -> Option<(ToolJobSupervisor, JobPrincipal)> {
    ACTIVE_DISPATCH.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| (context.supervisor.clone(), context.principal.clone()))
    })
}

/// Default supervisor for the direct IPC provider (no plugin scope).
fn default_supervisor() -> ToolJobSupervisor {
    DEFAULT_SUPERVISOR.with(|slot| {
        slot.borrow_mut()
            .get_or_insert_with(ToolJobSupervisor::new)
            .clone()
    })
}

/// Runs one tool spawn through the active plugin supervisor, or the direct
/// IPC default supervisor when no plugin scope is active.
pub(crate) fn spawn_supervised(
    request: &ExecutionRequest,
    poll_interval: Duration,
) -> Result<JobRun, IpcError> {
    let (supervisor, principal) = match active_dispatch() {
        Some(context) => context,
        None => (
            default_supervisor(),
            JobPrincipal::new(IPC_PRINCIPAL).map_err(map_job_error)?,
        ),
    };
    supervisor.spawn_and_wait(request, &principal, poll_interval)
}

/// Fail-closed CTX-1151 confinement pre-validation for admitted tools.
///
/// Builds the same ruleset the preparer will attach, then drops it without
/// spawning anything. This keeps the previous synchronous `Unavailable`
/// diagnostic for a host that cannot confine an admitted tool (non-Linux,
/// unresolvable executable closure) instead of hiding it behind a tracked
/// `SpawnFailed` job.
fn validate_tool_confinement(request: &ExecutionRequest) -> Result<(), IpcError> {
    if !is_tool_spawn_allowed(&request.executable, &request.args) {
        return Ok(());
    }
    let exec_paths = bitty_sandbox::resolve_tool_execs(&request.executable).map_err(|error| {
        IpcError::Unavailable {
            reason: format!(
                "tool spawn confinement refused '{}': {}",
                request.executable,
                truncate_message(error.to_string(), 96)
            ),
        }
    })?;
    let mut probe = Command::new(&request.executable);
    probe.args(&request.args);
    let guard =
        bitty_sandbox::confine_command_to_execs(&mut probe, &exec_paths).map_err(|error| {
            IpcError::Unavailable {
                reason: format!(
                    "tool spawn confinement refused '{}': {}",
                    request.executable,
                    truncate_message(error.to_string(), 96)
                ),
            }
        })?;
    drop(guard);
    Ok(())
}

/// Host-owned preparer that applies CTX-1151 exec confinement to every
/// admitted tool command the supervisor builds.
struct SandboxPreparer;

impl SpawnPreparer for SandboxPreparer {
    fn prepare(
        &self,
        spec: &JobSpec,
        command: &mut Command,
    ) -> Result<Box<dyn Any + Send>, String> {
        if !is_tool_spawn_allowed(&spec.program, &spec.args) {
            return Ok(Box::new(()));
        }
        let exec_paths = bitty_sandbox::resolve_tool_execs(&spec.program)
            .map_err(|error| truncate_message(error.to_string(), 96))?;
        let guard = bitty_sandbox::confine_command_to_execs(command, &exec_paths)
            .map_err(|error| truncate_message(error.to_string(), 96))?;
        Ok(Box::new(guard))
    }
}

/// Keeps the newest `budget` bytes of `text` at a char boundary.
///
/// The registry's retained store is newest-wins, so the synchronous summary
/// must take the **end** of that tail rather than its prefix; a prefix of a
/// tail larger than the 256 KiB store would deliver a middle chunk of the
/// child's output. Returns the bounded tail and whether any byte was cut.
pub(crate) fn truncate_to_budget_tail(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_owned(), false);
    }
    let mut start = text.len() - budget;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_owned(), true)
}

/// Restores the trailing newline the registry's line-joined view drops.
///
/// The registry decodes retained bytes and joins the split lines without a
/// trailing newline. When the stored payload ended in `\n`, the joined text is
/// exactly one byte shorter than the stored bytes; append that byte so the
/// synchronous summary preserves the child's exact line semantics (and the
/// byte-length truncation signal stays honest). Lossy invalid UTF-8 may not
/// satisfy the one-byte relation; that output is rejected downstream by the
/// NUL/shape validation.
fn restore_trailing_newline(view: OutputView) -> String {
    let mut text = view.text;
    if view.stored_bytes > 0 && text.len().saturating_add(1) == view.stored_bytes {
        text.push('\n');
    }
    text
}

/// Bounded diagnostic truncation (char-boundary safe).
fn truncate_message(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::execution_component::{
        ComponentCancelEffect, ComponentDeadlineClock, ComponentExit, ComponentExitKind,
        ComponentKillScope, ComponentOomVerdict,
    };

    #[derive(Debug, Clone)]
    enum FakeOutcome {
        NotUsable,
        Denied,
        Success,
        Failed,
    }

    struct FakeRunner {
        mode: ExecutionComponentMode,
        outcome: FakeOutcome,
        calls: Arc<AtomicUsize>,
    }

    impl ComponentRunner for FakeRunner {
        fn mode(&self) -> ExecutionComponentMode {
            self.mode
        }

        fn spawn_and_wait(
            &mut self,
            _request: &ExecutionRequest,
        ) -> Result<ComponentRun, ComponentRunError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                FakeOutcome::NotUsable => Err(ComponentRunError::NotUsable(
                    "fixture component is not usable".to_owned(),
                )),
                FakeOutcome::Denied => Err(ComponentRunError::Denied(IpcError::Denied {
                    code: "SafeMode".to_owned(),
                    reason: "fixture safe mode".to_owned(),
                })),
                FakeOutcome::Failed => Err(ComponentRunError::Failed(IpcError::Unavailable {
                    reason: "fixture post-submission failure".to_owned(),
                })),
                FakeOutcome::Success => Ok(ComponentRun {
                    job: 41,
                    exit: ComponentExit {
                        kind: ComponentExitKind::Success,
                        kill_scope: ComponentKillScope::OwnedTree,
                        oom: ComponentOomVerdict::NotOom,
                        stdout_total: 6,
                        stdout_evicted: 0,
                        stderr_total: 0,
                        stderr_evicted: 0,
                    },
                    stdout: "hello\n".to_owned(),
                    stderr: String::new(),
                    stdout_total: 6,
                    stderr_total: 0,
                    stdout_truncated: false,
                    stderr_truncated: false,
                }),
            }
        }
    }

    fn fake(
        mode: ExecutionComponentMode,
        outcome: FakeOutcome,
    ) -> (ToolJobSupervisor, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            mode,
            outcome,
            calls: Arc::clone(&calls),
        };
        (
            ToolJobSupervisor::with_component_runner(Box::new(runner)),
            calls,
        )
    }

    /// The test binary itself is a portable child; `--list` prints tests and
    /// exits without running them, so the linked fallback is cheap.
    fn linked_probe_request() -> ExecutionRequest {
        ExecutionRequest::new(
            std::env::current_exe()
                .expect("test executable")
                .display()
                .to_string(),
            vec!["--list".to_owned()],
        )
        .with_timeout_ms(30_000)
        .with_allow_effects(true)
    }

    fn principal() -> JobPrincipal {
        JobPrincipal::new("gate-tests").expect("principal")
    }

    #[test]
    fn preferred_gate_falls_back_to_the_linked_registry() {
        let (supervisor, calls) = fake(ExecutionComponentMode::Preferred, FakeOutcome::NotUsable);
        let run = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect("linked fallback must serve the probe");
        assert_eq!(run.raw.status, ExecutionStatus::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn required_gate_fails_closed_without_fallback() {
        let (supervisor, calls) = fake(ExecutionComponentMode::Required, FakeOutcome::NotUsable);
        let error = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect_err("required component must fail closed");
        match error {
            IpcError::Unavailable { reason } => {
                assert!(reason.contains("not usable"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn denied_gate_never_falls_back() {
        let (supervisor, calls) = fake(ExecutionComponentMode::Preferred, FakeOutcome::Denied);
        let error = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect_err("denied component path must not fall back");
        assert!(matches!(error, IpcError::Denied { .. }), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn post_submission_failure_never_falls_back() {
        let (supervisor, calls) = fake(ExecutionComponentMode::Preferred, FakeOutcome::Failed);
        let error = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect_err("a submitted component run must not be retried");
        assert!(matches!(error, IpcError::Unavailable { .. }), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preferred_failure_after_mutation_submission_is_not_replayed() {
        // `Failed` models a component that has accepted the Spawn and then
        // lost its result. The linked registry must not receive the same
        // mutating request as a fallback.
        let (supervisor, calls) = fake(ExecutionComponentMode::Preferred, FakeOutcome::Failed);
        let error = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect_err("a submitted mutation must fail closed");
        assert!(matches!(error, IpcError::Unavailable { .. }), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn component_success_maps_to_the_existing_job_run() {
        let (supervisor, calls) = fake(ExecutionComponentMode::Preferred, FakeOutcome::Success);
        let run = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect("component run");
        assert_eq!(run.job.get(), 41);
        assert_eq!(run.raw.status, ExecutionStatus::Completed);
        assert_eq!(run.raw.exit_code, Some(0));
        assert_eq!(run.raw.stdout, "hello\n");
        assert_eq!(run.raw.effect_state, EffectState::Completed);
        assert!(!run.truncated);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn safe_mode_denies_before_any_install_resolution() {
        let supervisor = ToolJobSupervisor::with_component(ComponentGateConfig {
            mode: ExecutionComponentMode::Preferred,
            host: crate::execution_component::ComponentHostConfig {
                safe_mode: true,
                user_root: Some(std::path::PathBuf::from("/nonexistent-component-root")),
                system_root: None,
                ..crate::execution_component::ComponentHostConfig::default()
            },
        });
        let error = supervisor
            .spawn_and_wait(
                &linked_probe_request(),
                &principal(),
                Duration::from_millis(10),
            )
            .expect_err("safe mode must deny the component path");
        match error {
            IpcError::Denied { code, .. } => assert_eq!(code, "SafeMode"),
            other => panic!("expected SafeMode denial, got {other:?}"),
        }
    }

    #[test]
    fn admitted_tool_spawns_never_enter_the_component_unconfined() {
        let request = ExecutionRequest::new("git", vec!["status".to_owned()])
            .with_timeout_ms(5_000)
            .with_allow_effects(true);
        match component_spec(&request) {
            Err(ComponentRunError::NotUsable(reason)) => {
                assert!(reason.contains("confinement"), "{reason}");
            }
            other => panic!("admitted tool must not reach the component: {other:?}"),
        }
        assert!(
            component_spec(&linked_probe_request()).is_ok(),
            "non-tool commands remain eligible for the component"
        );
    }

    #[test]
    fn component_outcome_mapping_preserves_timeout_and_cancel() {
        assert_eq!(
            component_outcome(ComponentExitKind::TimedOut(ComponentDeadlineClock::Hard)),
            ExecutionOutcome::TimedOut(bitty_execution::DeadlineClock::Hard)
        );
        assert_eq!(
            component_outcome(ComponentExitKind::Cancelled(ComponentCancelEffect::Killed)),
            ExecutionOutcome::Cancelled(bitty_execution::CancelEffect::Killed)
        );
        assert_eq!(
            component_outcome(ComponentExitKind::ExitCode(9)),
            ExecutionOutcome::ExitCode(9)
        );
        assert_eq!(
            component_outcome(ComponentExitKind::Success),
            ExecutionOutcome::Success
        );
    }
}
