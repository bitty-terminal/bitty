//! Tool/plugin spawns supervised by the extracted `bitty-execution` registry
//! (issue #2249, W-140 integration).
//!
//! Core keeps the permission gate and the process-confinement policy; the
//! extracted supervisor owns job identity, lifecycle, cancellation, and
//! bounded output. This module is the only Core source that names the
//! extension implementation (W-147 fence exception), and it exposes only
//! Core/`bitty-ipc` types to the spawn surface.
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitty_execution::{
    ExecutionOutcome, JobError, JobIo, JobKind, JobLifetime, JobOrigin, JobPrincipal, JobRegistry,
    JobSpec, JobTimeouts, OutputStream, OutputView, ReadOutput, SpawnPreparer,
};
use bitty_ipc::IpcError;
use bitty_ipc::execution::{EffectState, ExecutionRequest, ExecutionStatus, RawExecutionOutput};
use bitty_plugin_host::tools::is_tool_spawn_allowed;

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
pub(crate) struct JobRun {
    /// Real registry identity minted for this spawn.
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

/// Core-owned wrapper around the extracted [`JobRegistry`] for tool spawns.
///
/// Cheap to clone: clones share the same registry and preparer, so a host can
/// hand one clone to a cancellation path while the synchronous dispatch waits.
#[derive(Clone)]
pub(crate) struct ToolJobSupervisor {
    registry: JobRegistry,
}

impl std::fmt::Debug for ToolJobSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolJobSupervisor")
            .field("capacity", &self.registry.capacity())
            .field("tracked", &self.registry.len())
            .finish()
    }
}

impl ToolJobSupervisor {
    /// Registry with the default capacity and the Core confinement preparer.
    pub(crate) fn new() -> Self {
        Self {
            registry: JobRegistry::new().with_spawn_preparer(Arc::new(SandboxPreparer)),
        }
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
        }
    }

    /// Shared registry clone for tests and cancellation paths.
    #[cfg(test)]
    pub(crate) fn registry(&self) -> JobRegistry {
        self.registry.clone()
    }

    /// Spawns a job and waits for its terminal state, returning bounded
    /// output shaped for the synchronous execution service.
    pub(crate) fn spawn_and_wait(
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
