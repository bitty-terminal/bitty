//! Phase-1 execution supervisor: AI-agnostic async job foundation (CTX-0511).
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
//!   cancel, and deadlines, then publishes [`JobEvent`]s into a bounded
//!   observation queue a runtime can drain.
//! - Origin is provenance only: [`JobOrigin`] records "started from here"
//!   and no lifecycle path is coupled to it.
//!
//! # Deliberate non-goals (sibling tasks own them)
//!
//! - Structured outcomes, owned-process-tree kill, typed cancel with
//!   generation fencing: CTX-0512. [`JobStop`] is an interim observation
//!   only, and cancel terminates the direct child.
//! - Output retention/tail/filter and reliable at-least-once event delivery
//!   with IPC reconnect: CTX-0513. This slice drains pipes without retaining
//!   bytes; it keeps only activity timestamps (which the idle clock needs).
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
//!   delegating to `ExecutionRequest::validate`; registry size, event queue,
//!   provenance, and deadlines are bounded or explicit, and overflow fails
//!   closed.
//! - No shell: specs are argv-first and processes are built with
//!   [`std::process::Command`] directly; nothing routes through `bash -c`.
//! - No zombie or wedged child: every supervisor path kills and reaps its
//!   direct child, and drain threads are detached so a grandchild holding a
//!   pipe cannot pin a job in `Running`.

mod model;
mod registry;

use std::process::{Command, Stdio};

use bitty_ipc::execution::EnvPolicy;

pub use model::{
    DEFAULT_RETENTION_TTL, JobCancel, JobError, JobEvent, JobId, JobIo, JobKind, JobLifetime,
    JobOrigin, JobSnapshot, JobSpec, JobState, JobStop, JobTimeouts, MAX_JOB_ORIGIN_BYTES,
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
