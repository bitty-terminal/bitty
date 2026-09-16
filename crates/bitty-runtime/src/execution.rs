//! Phase-2 execution supervisor: async jobs plus bounded output and
//! reliable event delivery (CTX-0511 + CTX-0513).
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
//! - Origin is provenance only: [`JobOrigin`] records "started from here"
//!   and no lifecycle path is coupled to it.
//!
//! # Deliberate non-goals (sibling tasks own them)
//!
//! - Structured outcomes, owned-process-tree kill, typed cancel with
//!   generation fencing: CTX-0512. [`JobStop`] is an interim observation
//!   only, and cancel terminates the direct child.
//! - Capability-scoped job operations (`observe`/`read_output`/`write_input`/
//!   `signal`/`cancel`/`attach`/`transfer`): CTX-0514. Only in-process
//!   callers exist today and no IPC verb is exposed here.
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
    DEFAULT_RETENTION_TTL, JobCancel, JobError, JobEvent, JobId, JobIo, JobKind, JobLifetime,
    JobOrigin, JobSnapshot, JobSpec, JobState, JobStop, JobTimeouts, MAX_JOB_ORIGIN_BYTES,
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
