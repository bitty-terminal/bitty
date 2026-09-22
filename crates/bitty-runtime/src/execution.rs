//! Phase-2 execution supervisor: async jobs plus bounded output,
//! reliable event delivery, capability-scoped operations, persistent
//! metadata with file-held logs, and the detached-supervisor contract
//! (CTX-0511 + CTX-0513 + CTX-0514 + CTX-0516).
//!
//! This module is the Core-side foundation of the execution-host boundary
//! (research record 044, captured as DIR-026 in the `bitty-docs` draft
//! `docs/development/execution-host-boundary.md`): a **job** is a
//! first-class, panel-independent object above the PTY/process primitives,
//! and the runtime waits for it — never a model, never a panel, never a
//! polling loop.
//!
//! # What this slice implements
//!
//! - An in-memory, capacity-bounded [`JobRegistry`] with spawn/get/list/
//!   cancel handles and a monotonic [`JobId`] that is never reused.
//! - The spawn-time job model: declared [`JobLifetime`], [`JobKind`],
//!   argv-first execution, closed stdin by default ([`JobIo::Pipes`]) with a
//!   PTY as the explicit interactive opt-in, and separated `hard`/`idle`/
//!   `retention` clocks ([`JobTimeouts`]). No implicit deadline exists for
//!   any kind, so long `Service`/`Watch` jobs are never killed by an
//!   ordinary timeout.
//! - Event-driven lifecycle: one supervisor thread per job observes exit,
//!   cancel, and deadlines, then publishes [`JobEvent`]s into the bounded
//!   two-lane delivery log a runtime can drain or replay across an IPC
//!   disconnect.
//! - Bounded output: per-stream newest-wins byte stores with tail/filter
//!   reads ([`ReadOutput`]) and metadata-only [`OutputIndex`] snapshots, so
//!   raw stdout never grows memory or a database.
//! - Critical/observation delivery: terminal `Stopped` events are
//!   [`EventClass::Critical`] (at-least-once, replayable, acknowledged) and
//!   queue/start notices are [`EventClass::Observation`] (drop-oldest,
//!   UI-only, never a model wake-up).
//! - Capability-scoped operations (CTX-0514): per-principal, per-operation
//!   grants ([`JobPrincipal`], [`JobOperation`]) enforced in-process over
//!   the supervisor handles (`spawn_as`/`get_as`/`list_as`/`read_output_as`/
//!   `output_index_as`/`write_input_as`/`signal_as`/`cancel_as`/`attach_as`/
//!   `grant_as`/`revoke_as`/`transfer_as`/`events_since_as`/
//!   `acknowledge_as`). Deny by default with hidden existence, no
//!   observe/control bundle, no ambient authority; owner/subscriber roles
//!   stay `bitty-ai` coordination re-authorized here, and self-grant
//!   prohibition is the CTX-0524 seam.
//! - Origin is provenance only: [`JobOrigin`] records "started from here"
//!   and no lifecycle path is coupled to it.
//!
//! # Deliberate non-goals (sibling tasks own them)
//!
//! - Structured outcomes, owned-process-tree kill, typed cancel with
//!   generation fencing: CTX-0512. [`JobStop`] is an interim observation
//!   only, and cancel terminates the direct child.
//! - Capability enforcement transport: this task is in-process only, with no
//!   new IPC verbs. The existing IPC scope/auth registry plus the
//!   consent/effect gate stays the transport boundary; these `*_as` methods
//!   are what that boundary calls after authenticating the principal.
//!   Effective-capability intersection and self-grant prohibition across
//!   agent/plugin requests is the CTX-0524 seam (noted, not implemented).
//! - Persistence, restart reconciliation, and the detached supervisor
//!   contract: CTX-0516. [`JobStore`] checkpoints metadata plus the
//!   metadata-only [`OutputIndex`] into a versioned manifest with retained
//!   output spilled to per-job log files held by reference (never raw bytes
//!   in the manifest); [`reconcile`] maps live-at-crash rows onto
//!   [`ResumeDecision::UnknownOutcome`]; [`SupervisorDaemon`] owns one
//!   supervised directory at a time with handoff/adoption and
//!   [`SchedulePolicy`] admission. Grants are re-issued after a restart
//!   (never persisted) and adoption never respawns by itself.
//!
//! # Invariants
//!
//! - AI-agnostic vocabulary: no `AgentId`/`TaskId`/LLM/prompt symbol.
//! - Bounds: program/args/cwd/env reuse the accepted CTX-0442 limits by
//!   delegating to `ExecutionRequest::validate`; registry size, delivery
//!   lanes, output bytes, read shapes, provenance, and deadlines are bounded
//!   or explicit, and overflow fails closed. Critical terminal events have
//!   their own lane so observation pressure can never mask a `Stopped`.
//! - No shell: specs are argv-first and processes are built with
//!   [`std::process::Command`] directly; nothing routes through `bash -c`.
//! - No zombie or wedged child: every supervisor path kills and reaps its
//!   direct child, and pipe drain threads are joined after the reap so the
//!   output store is quiescent before the terminal event (a grandchild
//!   holding a pipe open delays the join; owned-process-tree cleanup stays
//!   CTX-0512). A job's own network failure is recorded as the observed stop
//!   plus retained stderr facts; no separate `network_error` classification
//!   is invented.
//!
//! # Candidate signals (CTX-0678, RUN-20..RUN-23 analysis batch)
//!
//! - Panel lease kernel ([`PanelLease`]): idle/occupied transitions with
//!   acquire/release/handoff events for OQ-083; lease vocabulary stays a UX
//!   metaphor and owns no bus, clock, or agent ontology.
//! - Sensitive-input gate ([`automated_input_allowed`]): the observed PTY
//!   echo state is the only signal for OQ-086; no-echo denies automated
//!   input with a typed denial and excludes capture.
//! - Command-risk kernel ([`classify_argv`]): structural argv tiers and
//!   hard-deny classes for OQ-087; shell-AST resolution stays open work.
//! - Detached-supervisor trust boundary: analysis only
//!   (`specifications/run-20-detached-supervisor-trust-boundary.md`); no
//!   daemon code, per the accepted headless/daemon decision.

mod command_risk;
mod delivery;
mod lease;
mod model;
mod oom;
mod output;
mod persistence;
mod process_tree;
mod registry;
mod retention;
mod sensitive_input;
mod supervisor;

use std::process::{Command, Stdio};

use bitty_ipc::execution::EnvPolicy;

pub use command_risk::{HardDeny, OperationIntent, RiskTier, RiskVerdict, classify_argv};
pub use delivery::{
    DeliveryState, EventClass, EventReplay, MAX_EVENT_REPLAY, MAX_STORED_CRITICAL_EVENTS,
    MAX_STORED_OBSERVATION_EVENTS, StoredEvent,
};
pub use lease::{
    LeaseError, LeaseEvent, LeaseHolder, LeaseState, MAX_PANEL_DESCRIPTION_CHARS,
    MAX_PANEL_TITLE_CHARS, PanelLease, validate_description, validate_title,
};
pub use model::{
    AttachReceipt, DEFAULT_RETENTION_TTL, JobCancel, JobError, JobEvent, JobGrant, JobId, JobIo,
    JobKind, JobLifetime, JobOperation, JobOrigin, JobPrincipal, JobSignal, JobSnapshot, JobSpec,
    JobState, JobStop, JobTimeouts, MAX_GRANTS_PER_JOB, MAX_JOB_ORIGIN_BYTES,
    MAX_JOB_PRINCIPAL_BYTES, MAX_SIGNAL_WINDOW_MS, MAX_SIGNALS_PER_WINDOW, MAX_WRITE_INPUT_BYTES,
    MAX_WRITE_INPUT_WINDOW_MS, MAX_WRITES_PER_WINDOW, SignalOutcome, TransferReceipt,
};
pub use oom::{MAX_MEMORY_EVENTS_BYTES, OomVerdict, classify_oom, parse_oom_kill_count};
pub use output::{
    MAX_OUTPUT_BYTES_PER_JOB, MAX_READ_BYTES, MAX_READ_LINES, OutputFilter, OutputIndex,
    OutputStream, OutputView, ReadOutput,
};
pub use persistence::{
    CheckpointSummary, IdAllocator, JobStore, PersistError, PersistedJob, PersistedStore,
    ReconciledJob, ResumeCursor, ResumeDecision, reconcile,
};
pub use persistence::{
    LOGS_DIR_NAME, MANIFEST_FILE_NAME, MAX_LOG_FILE_BYTES, MAX_MANIFEST_BYTES, MAX_PERSISTED_JOBS,
    PERSIST_FORMAT_VERSION,
};
pub use process_tree::{KillScope, ProcessTreeBackend};
pub use registry::{DEFAULT_MAX_JOBS, JobRegistry, MAX_STORED_JOB_EVENTS};
pub use retention::{MAX_RETENTION_TTL, RetentionError, RetentionPolicy, RetentionTier};
pub use sensitive_input::{
    EchoState, InteractionClass, SecureInputDenial, automated_input_allowed, may_capture,
};
pub use supervisor::{
    AdoptedJob, AdoptionKind, DaemonError, HandoffOffer, ScheduleDecision, SchedulePolicy,
    SupervisorDaemon, adoption_plan, clear_handoff, read_handoff, write_handoff,
};
pub use supervisor::{
    DEFAULT_MAX_RUNNING, HANDOFF_FILE_NAME, MAX_HANDOFF_BYTES, MAX_HANDOFF_JOBS,
    MAX_HEARTBEAT_BYTES, MAX_SCHEDULE_RUNNING, STALE_HEARTBEAT_MS, SUPERVISOR_FORMAT_VERSION,
    SUPERVISOR_HEARTBEAT_NAME, SUPERVISOR_LOCK_NAME,
};

/// Builds the closed-environment pipe command shared by the CTX-0442
/// synchronous provider and the job supervisor.
///
/// Argv-only (no shell), ambient environment cleared, explicit variables
/// only, stdin closed, stdout/stderr piped. Reusing one constructor keeps
/// the two execution surfaces from drifting apart.
#[must_use]
pub(crate) fn closed_pipe_command(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &EnvPolicy,
) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.env_clear();
    if let EnvPolicy::Explicit { vars } = env {
        for var in vars {
            command.env(&var.name, &var.value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
