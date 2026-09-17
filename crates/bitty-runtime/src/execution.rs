//! Phase-2 execution supervisor: async jobs plus bounded output,
//! reliable event delivery, and capability-scoped operations
//! (CTX-0511 + CTX-0513 + CTX-0514).
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
//! - Persistence, restart reconciliation, and a detached supervisor daemon:
//!   CTX-0516 (Phase 2/3). Registry state is in-memory only.
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

mod delivery;
mod model;
mod output;
mod registry;

use std::process::{Command, Stdio};

use bitty_ipc::execution::EnvPolicy;

pub use delivery::{
    DeliveryState, EventClass, EventReplay, MAX_EVENT_REPLAY, MAX_STORED_CRITICAL_EVENTS,
    MAX_STORED_OBSERVATION_EVENTS, StoredEvent,
};
pub use model::{
    AttachReceipt, DEFAULT_RETENTION_TTL, JobCancel, JobError, JobEvent, JobGrant, JobId, JobIo,
    JobKind, JobLifetime, JobOperation, JobOrigin, JobPrincipal, JobSignal, JobSnapshot, JobSpec,
    JobState, JobStop, JobTimeouts, MAX_GRANTS_PER_JOB, MAX_JOB_ORIGIN_BYTES,
    MAX_JOB_PRINCIPAL_BYTES, MAX_SIGNAL_WINDOW_MS, MAX_SIGNALS_PER_WINDOW, MAX_WRITE_INPUT_BYTES,
    MAX_WRITE_INPUT_WINDOW_MS, MAX_WRITES_PER_WINDOW, SignalOutcome, TransferReceipt,
};
pub use output::{
    MAX_OUTPUT_BYTES_PER_JOB, MAX_READ_BYTES, MAX_READ_LINES, OutputFilter, OutputIndex,
    OutputStream, OutputView, ReadOutput,
};
pub use registry::{DEFAULT_MAX_JOBS, JobRegistry, MAX_STORED_JOB_EVENTS};

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
