//! In-memory job registry and phase-1 supervisor.
//!
//! The registry owns job identity, the bounded record table, the bounded
//! observation event queue, and one supervisor thread per job. It never
//! blocks a caller on a child's lifetime: [`JobRegistry::spawn`] returns the
//! [`JobId`] as soon as the job is tracked, and completion is published as
//! [`JobEvent`]s.
//!
//! Supervisor threads own their child handle; cancellation is a flag the
//! thread observes (no shared `Child` behind a lock, no cross-thread kill
//! races). Terminating a job kills the direct child only — owned-process-tree
//! kill and typed cancel outcomes are CTX-0512.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitty_ipc::execution::EnvPolicy;
use bitty_pty::{Pty, PtyBuilder, PtyReader};

use super::closed_pipe_command;
use super::delivery::{DeliveryLog, DeliveryState, EventReplay};
use super::model::{
    AttachReceipt, JobCancel, JobError, JobEvent, JobGrant, JobId, JobIo, JobOperation,
    JobPrincipal, JobSignal, JobSnapshot, JobSpec, JobState, JobStop, MAX_GRANTS_PER_JOB,
    MAX_SIGNAL_WINDOW_MS, MAX_SIGNALS_PER_WINDOW, MAX_WRITE_INPUT_BYTES, MAX_WRITE_INPUT_WINDOW_MS,
    MAX_WRITES_PER_WINDOW, SignalOutcome, TransferReceipt,
};
use super::output::{OutputIndex, OutputSink, OutputView, ReadOutput};

/// Default registry capacity.
///
/// Reuses the accepted CTX-0442 tracked-execution bound
/// (`bitty_ipc::execution::MAX_TRACKED_EXECUTIONS`, 64) so the job table has
/// the same hard ceiling as the synchronous surface. Overflow fails closed;
/// finished records become evictable only after their retention elapses.
pub const DEFAULT_MAX_JOBS: usize = bitty_ipc::execution::MAX_TRACKED_EXECUTIONS;

/// Maximum queued lifecycle events before the oldest is dropped.
///
/// Kept for phase-1 API compatibility: it equals the observation-lane bound,
/// and [`JobRegistry::events_dropped`] now reports the sum of both delivery
/// lanes.
pub const MAX_STORED_JOB_EVENTS: usize = super::delivery::MAX_STORED_OBSERVATION_EVENTS;

/// Supervisor poll interval (CTX-0445 spawn poll precedent).
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Pipe drain chunk size (8 KiB, the `bitty-pty` read-chunk precedent).
const DRAIN_CHUNK_BYTES: usize = 8 * 1024;

type Shared = Arc<Mutex<RegistryInner>>;

// ── registry ────────────────────────────────────────────────────────────────

/// Bounded in-memory registry of supervised jobs.
///
/// Cheap to clone: every clone shares the same table, event queue, and
/// supervisor threads. All operations are non-blocking with respect to child
/// lifetimes.
#[derive(Clone)]
pub struct JobRegistry {
    shared: Shared,
}

impl fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = lock_inner(&self.shared);
        f.debug_struct("JobRegistry")
            .field("capacity", &inner.capacity)
            .field("tracked", &inner.jobs.len())
            .field("events_dropped", &inner.events.dropped())
            .finish()
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    /// Registry with [`DEFAULT_MAX_JOBS`] capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_JOBS)
    }

    /// Registry with `capacity` tracked-job slots.
    ///
    /// # Panics
    ///
    /// Panics when `capacity == 0` (a zero-slot registry cannot be used;
    /// [`crate::ColdQueue`] follows the same rule).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "job registry capacity must be > 0");
        Self {
            shared: Arc::new(Mutex::new(RegistryInner {
                capacity,
                next_id: 1,
                jobs: BTreeMap::new(),
                events: DeliveryLog::new(),
            })),
        }
    }

    /// Configured tracked-job capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        lock_inner(&self.shared).capacity
    }

    /// Number of tracked records (live plus retained).
    #[must_use]
    pub fn len(&self) -> usize {
        lock_inner(&self.shared).jobs.len()
    }

    /// Whether no record is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validates `spec`, tracks it as [`JobState::Queued`], and starts its
    /// supervisor thread.
    ///
    /// Expired finished records are reclaimed first (each record becomes
    /// evictable after its retention elapses). When the registry is still at
    /// capacity the call fails closed with [`JobError::RegistryFull`]; live
    /// jobs are never evicted.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::InvalidSpec`] when `spec` fails validation (no
    /// process and no thread is started), and [`JobError::Unavailable`] when
    /// the supervisor thread cannot be created (the record is removed again).
    pub fn spawn(&self, spec: JobSpec) -> Result<JobId, JobError> {
        spec.validate()?;
        let (id, control, output) = {
            let mut inner = lock_inner(&self.shared);
            inner.evict_expired(now_ms());
            if inner.jobs.len() >= inner.capacity {
                return Err(JobError::RegistryFull {
                    limit: inner.capacity,
                });
            }
            let id = inner.allocate_id()?;
            let control = Arc::new(JobControl::new());
            let output = OutputSink::default();
            inner.jobs.insert(
                id,
                JobRecord::unowned(id, spec.clone(), Arc::clone(&control), output.clone()),
            );
            (id, control, output)
        };
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name(format!("bitty-job-{}", id.get()))
            .spawn(move || supervise(shared, id, spec, control, output))
            .map_err(|error| {
                let mut inner = lock_inner(&self.shared);
                inner.jobs.remove(&id);
                JobError::Unavailable {
                    reason: format!("supervisor thread failed to start: {error}"),
                }
            })?;
        Ok(id)
    }

    /// Owned snapshot of one tracked job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] when `id` is not tracked (never
    /// issued, or evicted after its retention elapsed).
    pub fn get(&self, id: JobId) -> Result<JobSnapshot, JobError> {
        let inner = lock_inner(&self.shared);
        inner
            .jobs
            .get(&id)
            .map(JobRecord::snapshot)
            .ok_or(JobError::UnknownJob(id))
    }

    /// Owned snapshots of every tracked job, ordered by [`JobId`].
    #[must_use]
    pub fn list(&self) -> Vec<JobSnapshot> {
        lock_inner(&self.shared)
            .jobs
            .values()
            .map(JobRecord::snapshot)
            .collect()
    }

    /// Requests job termination.
    ///
    /// The request is recorded and the supervisor terminates the job's
    /// direct child and publishes `Stopped`; this returns as soon as the
    /// request is recorded, never after the process is gone. Typed cancel
    /// modes/grace periods and cancel outcomes are CTX-0512.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] when `id` is not tracked.
    pub fn cancel(&self, id: JobId) -> Result<JobCancel, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id).ok_or(JobError::UnknownJob(id))?;
        if let JobState::Done(stop) = record.state {
            return Ok(JobCancel::AlreadyStopped(stop));
        }
        record.control.cancel.store(true, Ordering::Release);
        Ok(JobCancel::Requested)
    }

    // ── capability-scoped operations (CTX-0514) ─────────────────────────────
    //
    // Every `*_as` entry point below enforces one per-principal,
    // per-operation grant at the host before touching job state. The
    // enforcement order is fixed: existence first, then authorization (deny
    // by default, hide existence from unauthorized callers), then validation
    // and rate limiting. A denied caller learns nothing about the job — the
    // same [`JobError::Denied`] shape covers unknown ids for callers without
    // an implicit grant — and no operation ever confers ambient authority:
    // ownership is assigned once at spawn and moves only through `transfer`,
    // and a delegation never widens (a granter can only pass on operations
    // it currently holds).
    //
    // In-process only: no IPC verb is exposed here. The existing IPC
    // scope/auth registry plus the consent/effect gate stays the transport
    // boundary; these methods are what that boundary (or a future wire)
    // calls after authenticating the principal. Role decisions (owner vs.
    // subscriber) stay `bitty-ai` coordination semantics re-authorized here,
    // and self-grant prohibition is the CTX-0524 seam: this layer has no
    // self-grant path (`grant_as`/`transfer_as` both require `transfer`,
    // which only the owner or an explicit delegate holds).

    /// Tracks `spec` as [`JobState::Queued`] owned by `owner`, then starts
    /// its supervisor thread.
    ///
    /// The spawner becomes the job's first owner with the full operation
    /// set; every other principal starts with nothing. All spawn bounds
    /// (capacity, spec validation, supervisor startup) behave exactly like
    /// [`JobRegistry::spawn`].
    ///
    /// # Errors
    ///
    /// Same failure set as [`JobRegistry::spawn`] (no denial: spawning
    /// confers the first ownership rather than exercising one).
    pub fn spawn_as(&self, owner: JobPrincipal, spec: JobSpec) -> Result<JobId, JobError> {
        spec.validate()?;
        // Principals are validated at the boundary: a name that fails the
        // model bounds fails here, before anything is tracked. (`JobPrincipal`
        // construction already enforces the bounds; this re-check keeps the
        // registry path honest even if a future constructor relaxes.)
        if owner.as_str().len() > super::model::MAX_JOB_PRINCIPAL_BYTES {
            return Err(JobError::invalid_principal("job principal exceeds bound"));
        };
        let (id, control, output) = {
            let mut inner = lock_inner(&self.shared);
            inner.evict_expired(now_ms());
            if inner.jobs.len() >= inner.capacity {
                return Err(JobError::RegistryFull {
                    limit: inner.capacity,
                });
            }
            let id = inner.allocate_id()?;
            let control = Arc::new(JobControl::new());
            let output = OutputSink::default();
            inner.jobs.insert(
                id,
                JobRecord::owned(
                    id,
                    spec.clone(),
                    Arc::clone(&control),
                    output.clone(),
                    owner,
                ),
            );
            (id, control, output)
        };
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name(format!("bitty-job-{}", id.get()))
            .spawn(move || supervise(shared, id, spec, control, output))
            .map_err(|error| {
                let mut inner = lock_inner(&self.shared);
                inner.jobs.remove(&id);
                JobError::Unavailable {
                    reason: format!("supervisor thread failed to start: {error}"),
                }
            })?;
        Ok(id)
    }

    /// Owned snapshot of one tracked job, authorized as `principal`.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `observe` when the caller
    /// holds no `observe` grant — including for unknown ids, so a denial
    /// never confirms whether the job exists.
    pub fn get_as(&self, principal: &JobPrincipal, id: JobId) -> Result<JobSnapshot, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id);
        authorize_observe(&inner, principal, record, id, JobOperation::Observe)?;
        // Authorization passed, so the record exists; the `ok_or` below is
        // unreachable except under a lock race that cannot happen (the
        // record is borrowed from the same guard).
        record
            .map(JobRecord::snapshot)
            .ok_or(JobError::UnknownJob(id))
    }

    /// Owned snapshots of every job `principal` may observe, ordered by
    /// [`JobId`].
    ///
    /// Jobs the caller cannot observe are omitted silently (never an
    /// existence oracle); an unauthorized caller gets an empty list. This
    /// never fails: there is no job to deny on, only a filtered view.
    #[must_use]
    pub fn list_as(&self, principal: &JobPrincipal) -> Vec<JobSnapshot> {
        let inner = lock_inner(&self.shared);
        inner
            .jobs
            .values()
            .filter(|record| record.authorized(principal, JobOperation::Observe))
            .map(JobRecord::snapshot)
            .collect()
    }

    /// Requests job termination as `principal`.
    ///
    /// Same direct-child mechanism and return shape as
    /// [`JobRegistry::cancel`]; enforcement only adds the `cancel` grant
    /// check. Denial leaves the job untouched.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `cancel` when the caller
    /// holds no `cancel` grant (unknown ids deny identically).
    pub fn cancel_as(&self, principal: &JobPrincipal, id: JobId) -> Result<JobCancel, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id);
        authorize_strict(&inner, principal, record, id, JobOperation::Cancel)?;
        let record = record.ok_or(JobError::UnknownJob(id))?;
        if let JobState::Done(stop) = record.state {
            return Ok(JobCancel::AlreadyStopped(stop));
        }
        record.control.cancel.store(true, Ordering::Release);
        Ok(JobCancel::Requested)
    }

    /// Reads retained output of one tracked job as `principal` (CTX-0513
    /// shapes, CTX-0514 enforcement).
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `read_output` when the
    /// caller holds no `read_output` grant (unknown ids deny identically),
    /// and [`JobError::InvalidRead`] when the request is over-bound.
    pub fn read_output_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        read: ReadOutput,
    ) -> Result<OutputView, JobError> {
        let validated = read.validate()?;
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id);
        authorize_strict(&inner, principal, record, id, JobOperation::ReadOutput)?;
        let record = record.ok_or(JobError::UnknownJob(id))?;
        Ok(record.output.read(&validated))
    }

    /// Metadata-only output index of one tracked job as `principal`.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `read_output` when the
    /// caller holds no `read_output` grant (unknown ids deny identically).
    pub fn output_index_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
    ) -> Result<OutputIndex, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id);
        authorize_strict(&inner, principal, record, id, JobOperation::ReadOutput)?;
        let record = record.ok_or(JobError::UnknownJob(id))?;
        Ok(record.output.index())
    }

    /// Writes `data` to a live interactive (PTY) job's stdin as `principal`.
    ///
    /// Enforcement order: `write_input` grant, then payload bound, then the
    /// per-principal rate budget, then backend/state gating. Pipe jobs run
    /// with closed stdin by design, so an authorized write there reports
    /// [`JobError::Unsupported`]; a terminal job reports `Unsupported` as
    /// well (never `Denied`: the caller was authorized, the target is gone).
    ///
    /// Writer-half startup race: the supervisor publishes the PTY writer
    /// half just after marking `Running` (same thread, back-to-back). A
    /// claim that lands in that gap fails closed with `Unsupported` — never
    /// a block — so callers retry once `Running` is observable.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `write_input` for callers
    /// without the grant (unknown ids deny identically),
    /// [`JobError::InvalidWrite`] for empty or over-bound payloads,
    /// [`JobError::RateLimited`] once the per-window budget is spent, and
    /// [`JobError::Unsupported`] for pipe backends or terminal jobs.
    pub fn write_input_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        data: &[u8],
    ) -> Result<usize, JobError> {
        let mut inner = lock_inner(&self.shared);
        let authorized = inner
            .jobs
            .get(&id)
            .is_some_and(|record| record.authorized(principal, JobOperation::WriteInput));
        if !authorized {
            return Err(JobError::denied(
                JobOperation::WriteInput,
                "caller holds no grant for this operation",
            ));
        }
        if data.is_empty() {
            return Err(JobError::invalid_write(
                "write_input payload must not be empty",
            ));
        }
        if data.len() > MAX_WRITE_INPUT_BYTES {
            return Err(JobError::invalid_write(format!(
                "write_input payload exceeds {MAX_WRITE_INPUT_BYTES} bytes"
            )));
        }
        let writer = {
            let record = inner.jobs.get_mut(&id).ok_or(JobError::UnknownJob(id))?;
            if record.state.is_terminal() {
                return Err(JobError::unsupported("job is terminal; stdin is closed"));
            }
            if record.spec.io != JobIo::Pty {
                // Pipe jobs run with closed stdin by design (CTX-0511): the
                // caller is authorized, the mechanism truthfully refuses.
                return Err(JobError::unsupported(
                    "pipe jobs run with closed stdin; use a PTY job for input",
                ));
            }
            if !record.write_budget.check() {
                return Err(JobError::write_rate_limited(format!(
                    "write_input budget of {MAX_WRITES_PER_WINDOW} calls per {MAX_WRITE_INPUT_WINDOW_MS} ms spent"
                )));
            }
            match record.pty_writer() {
                Some(writer) => writer,
                None => {
                    // The writer half is gone (taken by a concurrent write or
                    // the backend never published one): fail closed without
                    // spending more budget than the one checked call.
                    return Err(JobError::unsupported(
                        "interactive stdin is unavailable; the job may be starting or stopping",
                    ));
                }
            }
        };
        let mut outcome = write_to_pty_stdin(writer, data);
        {
            let inner = lock_inner(&self.shared);
            if let Some(record) = inner.jobs.get(&id) {
                record.return_pty_writer(outcome.ok_writer());
            }
        }
        outcome.into_result()
    }

    /// Delivers a portable signal request as `principal`.
    ///
    /// Enforcement order: `signal` grant, then the per-principal signal
    /// budget, then liveness. A terminal job observes its stop exactly like
    /// [`JobCancel::AlreadyStopped`] (no budget is spent on a job that is
    /// already gone — the call is an observation, not a delivery). A live
    /// job reaches the CTX-0512 typed-signal seam and reports
    /// [`JobError::Unsupported`]: this layer names the intent, the delivery
    /// mechanism owns kill semantics, and no outcome is invented here.
    ///
    /// Denied callers never consume the budget and never disturb the job:
    /// authorization runs before limiting.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `signal` for callers
    /// without the grant (unknown ids deny identically), and
    /// [`JobError::SignalRateLimited`] once the per-window burst budget is
    /// spent.
    pub fn signal_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        signal: JobSignal,
    ) -> Result<SignalOutcome, JobError> {
        let _ = signal;
        let mut inner = lock_inner(&self.shared);
        let authorized = inner
            .jobs
            .get(&id)
            .is_some_and(|record| record.authorized(principal, JobOperation::Signal));
        if !authorized {
            return Err(JobError::denied(
                JobOperation::Signal,
                "caller holds no grant for this operation",
            ));
        }
        let record = inner.jobs.get_mut(&id).ok_or(JobError::UnknownJob(id))?;
        if let JobState::Done(stop) = record.state {
            return Ok(SignalOutcome::AlreadyStopped(stop));
        }
        if !record.signal_budget.check() {
            return Err(JobError::signal_rate_limited(format!(
                "signal budget of {MAX_SIGNALS_PER_WINDOW} calls per {MAX_SIGNAL_WINDOW_MS} ms spent"
            )));
        }
        Err(JobError::unsupported(
            "live-job signal delivery is the CTX-0512 typed-signal mechanism; intent recorded",
        ))
    }

    /// Subscribes `principal` to a live job's event cursor.
    ///
    /// The cursor is the delivery-log `seq` the caller replays from (0
    /// replays from the origin). The receipt echoes the validated `(job,
    /// from_seq)` pair so a reconnecting consumer can page with
    /// [`JobRegistry::events_since_as`]; no subscription state is stored —
    /// the delivery log stays the only event memory.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `attach` for callers
    /// without the grant (unknown ids deny identically),
    /// [`JobError::InvalidCursor`] when `from_seq` is past the event head,
    /// and [`JobError::Unsupported`] when the job is already terminal (there
    /// is nothing live to subscribe to; replay the retained events instead).
    pub fn attach_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        from_seq: u64,
    ) -> Result<AttachReceipt, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id);
        authorize_strict(&inner, principal, record, id, JobOperation::Attach)?;
        let record = record.ok_or(JobError::UnknownJob(id))?;
        if record.state.is_terminal() {
            return Err(JobError::unsupported(
                "job is terminal; replay retained events instead of attaching",
            ));
        }
        if from_seq > inner.events.head_seq() {
            return Err(JobError::invalid_cursor(format!(
                "cursor {from_seq} is past the event head"
            )));
        }
        Ok(AttachReceipt { job: id, from_seq })
    }

    /// Delegates one operation on `id` to `grant.principal`, authorized as
    /// `principal`.
    ///
    /// Delegation requires `transfer`, and the granter can only pass on
    /// operations it currently holds (ownership counts as holding all
    /// seven): a reviewer holding only `observe` cannot conjure `cancel`
    /// for an accomplice, and holding six of seven never implies the
    /// seventh. Grants are per-job, never global, and idempotent (repeating
    /// a grant reports `false` from [`JobRegistry::revoke_as`]'s mirror
    /// only — `grant_as` itself returns `()` either way).
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `transfer` when the
    /// caller lacks `transfer` or tries to delegate an operation it does not
    /// hold (unknown ids deny identically), and [`JobError::GrantsFull`]
    /// when the job's grant table is at capacity.
    pub fn grant_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        grant: JobGrant,
    ) -> Result<(), JobError> {
        let mut inner = lock_inner(&self.shared);
        let (caller_holds_transfer, granter_holds_delegated) = match inner.jobs.get(&id) {
            Some(record) => (
                record.authorized(principal, JobOperation::Transfer),
                record.authorized(principal, grant.operation),
            ),
            None => (false, false),
        };
        if !caller_holds_transfer {
            return Err(JobError::denied(
                JobOperation::Transfer,
                "caller holds no grant for this operation",
            ));
        }
        if !granter_holds_delegated {
            return Err(JobError::denied(
                JobOperation::Transfer,
                "granter does not hold the delegated operation",
            ));
        }
        let record = inner.jobs.get_mut(&id).ok_or(JobError::UnknownJob(id))?;
        if !record.grants.contains(&grant) && record.grants.len() >= MAX_GRANTS_PER_JOB {
            return Err(JobError::GrantsFull {
                limit: MAX_GRANTS_PER_JOB,
            });
        }
        record.grants.insert(grant);
        Ok(())
    }

    /// Removes one explicit grant, authorized as `principal`.
    ///
    /// Returns `true` when a grant was removed, `false` when none matched
    /// (idempotent: revoking twice is not an error). Removing an ownership
    /// grant is meaningless — ownership is implicit, not a table entry — so
    /// revoking the owner's own pair reports `false`. Transfer of ownership
    /// clears all explicit grants (below): a new owner's authority starts
    /// clean, never inheriting the previous owner's delegations.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `transfer` for callers
    /// without `transfer` (unknown ids deny identically).
    pub fn revoke_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        grant: &JobGrant,
    ) -> Result<bool, JobError> {
        let mut inner = lock_inner(&self.shared);
        let authorized = inner
            .jobs
            .get(&id)
            .is_some_and(|record| record.authorized(principal, JobOperation::Transfer));
        if !authorized {
            return Err(JobError::denied(
                JobOperation::Transfer,
                "caller holds no grant for this operation",
            ));
        }
        let record = inner.jobs.get_mut(&id).ok_or(JobError::UnknownJob(id))?;
        Ok(record.grants.remove(grant))
    }

    /// Moves ownership of `id` to `new_owner`, authorized as `principal`.
    ///
    /// The previous owner is fenced immediately: every operation denies
    /// afterwards unless a fresh grant says otherwise. All explicit grants
    /// clear on transfer, so the successor's authority starts from
    /// ownership alone — a delegation the old owner handed out never
    /// survives the move. Transferring to the current owner is a no-op that
    /// still returns a receipt (and still clears stale grants).
    ///
    /// There is no self-grant path here: `transfer_as` requires `transfer`,
    /// which only the owner (or an explicit `transfer` delegate) holds, and
    /// a delegate can only move ownership onward, never mint authority it
    /// does not hold. Broader self-grant prohibition (effective-capability
    /// intersection across agent/plugin requests) is the CTX-0524 seam and
    /// is not implemented in this task.
    ///
    /// Legacy records (spawned through [`JobRegistry::spawn`]) have no
    /// owner: `transfer_as` denies on them, so a scoped caller can never
    /// seize a legacy job and a legacy job never confers scoped ownership.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `transfer` for callers
    /// without `transfer` (unknown ids deny identically).
    pub fn transfer_as(
        &self,
        principal: &JobPrincipal,
        id: JobId,
        new_owner: JobPrincipal,
    ) -> Result<TransferReceipt, JobError> {
        let mut inner = lock_inner(&self.shared);
        let authorized = inner
            .jobs
            .get(&id)
            .is_some_and(|record| record.authorized(principal, JobOperation::Transfer));
        if !authorized {
            return Err(JobError::denied(
                JobOperation::Transfer,
                "caller holds no grant for this operation",
            ));
        }
        let record = inner.jobs.get_mut(&id).ok_or(JobError::UnknownJob(id))?;
        let previous_owner = record.owner.clone().ok_or_else(|| {
            JobError::denied(
                JobOperation::Transfer,
                "legacy job has no owner to transfer from",
            )
        })?;
        record.owner = Some(new_owner.clone());
        record.grants.clear();
        Ok(TransferReceipt {
            job: id,
            previous_owner,
            new_owner,
        })
    }

    /// Replays retained events newer than `since` that belong to jobs
    /// `principal` may observe (CTX-0513 shapes, CTX-0514 scoping).
    ///
    /// Events for jobs the caller cannot observe are filtered out before
    /// the replay bound applies, so a stranger's replay is empty rather
    /// than an oracle. Cursor validation (`InvalidCursor` past the head)
    /// and the `gap` flag behave exactly like
    /// [`JobRegistry::events_since`]; delivery-state marking only touches
    /// returned events.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::InvalidCursor`] when `since` is past the head.
    /// Unauthorized callers get an empty replay, never a denial: there is no
    /// single job to deny on, only a filtered view (mirroring `list_as`).
    pub fn events_since_as(
        &self,
        principal: &JobPrincipal,
        since: u64,
        limit: usize,
    ) -> Result<EventReplay, JobError> {
        let mut inner = lock_inner(&self.shared);
        let head = inner.events.head_seq();
        if since > head {
            return Err(JobError::invalid_cursor(format!(
                "cursor {since} is past the event head"
            )));
        }
        let visible: BTreeSet<JobId> = inner
            .jobs
            .values()
            .filter(|record| record.authorized(principal, JobOperation::Observe))
            .map(|record| record.id)
            .collect();
        let mut replay = inner.events.replay_since(since, limit)?;
        replay
            .events
            .retain(|stored| visible.contains(&stored.job()));
        Ok(replay)
    }

    /// Marks `seq` acknowledged as `principal` (idempotent at-least-once
    /// close-out).
    ///
    /// The caller must hold `observe` on the event's job: acknowledging
    /// another principal's event denies with operation `observe` (unknown
    /// seqs fail closed as [`JobError::UnknownEvent`], which reveals nothing
    /// — the seq space is registry-global, not per-job).
    ///
    /// # Errors
    ///
    /// Returns [`JobError::Denied`] with operation `observe` when the caller
    /// cannot observe the event's job, and [`JobError::UnknownEvent`] when
    /// `seq` is not retained.
    pub fn acknowledge_as(
        &self,
        principal: &JobPrincipal,
        seq: u64,
    ) -> Result<DeliveryState, JobError> {
        let mut inner = lock_inner(&self.shared);
        let target = inner
            .events
            .find(seq)
            .ok_or_else(|| JobError::unknown_event(seq))?;
        let job = target.job();
        let authorized = inner
            .jobs
            .get(&job)
            .is_some_and(|record| record.authorized(principal, JobOperation::Observe));
        if !authorized {
            return Err(JobError::denied(
                JobOperation::Observe,
                "caller cannot observe this job's events",
            ));
        }
        inner.events.acknowledge(seq)
    }

    /// Drains up to `limit` queued lifecycle events in order.
    ///
    /// This is the phase-1 consumption shape, kept unchanged: it removes the
    /// oldest retained events across both delivery lanes in `seq` order.
    /// Consumers that need reconnect replay should use
    /// [`JobRegistry::events_since`] instead, which retains history and
    /// reports delivery states.
    #[must_use]
    pub fn drain_events(&self, limit: usize) -> Vec<JobEvent> {
        lock_inner(&self.shared).events.drain(limit)
    }

    /// Lifecycle events dropped by the bounded delivery lanes so far (both
    /// lanes summed; see [`JobRegistry::observation_dropped`] and
    /// [`JobRegistry::critical_dropped`] for the split).
    #[must_use]
    pub fn events_dropped(&self) -> u64 {
        lock_inner(&self.shared).events.dropped()
    }

    /// Observation events dropped by their lane so far (UI-only lifecycle
    /// notices; never terminal stops).
    #[must_use]
    pub fn observation_dropped(&self) -> u64 {
        lock_inner(&self.shared).events.observation_dropped()
    }

    /// Critical events dropped by their lane so far (terminal stops; the
    /// lane is sized so ordinary observation pressure never drops one).
    #[must_use]
    pub fn critical_dropped(&self) -> u64 {
        lock_inner(&self.shared).events.critical_dropped()
    }

    /// Replays retained events newer than `since` in `seq` order (CTX-0513).
    ///
    /// `since` is an event `seq` (0 replays from the origin); the returned
    /// [`EventReplay::next_seq`] is the cursor for the next call. Replays
    /// mark returned events delivered; the consumer confirms handling with
    /// [`JobRegistry::acknowledge`]. History older than the retained window
    /// sets [`EventReplay::gap`].
    ///
    /// # Errors
    ///
    /// Returns [`JobError::InvalidCursor`] when `since` is past the head.
    pub fn events_since(&self, since: u64, limit: usize) -> Result<EventReplay, JobError> {
        lock_inner(&self.shared).events.replay_since(since, limit)
    }

    /// Marks `seq` acknowledged (idempotent at-least-once close-out).
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownEvent`] when `seq` is not retained
    /// (unknown, or removed by [`JobRegistry::drain_events`]).
    pub fn acknowledge(&self, seq: u64) -> Result<DeliveryState, JobError> {
        lock_inner(&self.shared).events.acknowledge(seq)
    }

    /// Newest retained event `seq` (0 when the log is empty).
    #[must_use]
    pub fn event_head_seq(&self) -> u64 {
        lock_inner(&self.shared).events.head_seq()
    }

    /// Reads retained output of one tracked job (CTX-0513).
    ///
    /// The request is validated fail-closed before anything is read; the
    /// returned view carries truncation honesty flags so callers can
    /// distinguish "the child wrote this much" from "this much survived".
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] when `id` is not tracked, and
    /// [`JobError::InvalidRead`] when the request is over-bound.
    pub fn read_output(&self, id: JobId, read: ReadOutput) -> Result<OutputView, JobError> {
        let validated = read.validate()?;
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id).ok_or(JobError::UnknownJob(id))?;
        Ok(record.output.read(&validated))
    }

    /// Metadata-only output index of one tracked job (totals, never bytes).
    ///
    /// # Errors
    ///
    /// Returns [`JobError::UnknownJob`] when `id` is not tracked.
    pub fn output_index(&self, id: JobId) -> Result<OutputIndex, JobError> {
        let inner = lock_inner(&self.shared);
        let record = inner.jobs.get(&id).ok_or(JobError::UnknownJob(id))?;
        Ok(record.output.index())
    }

    /// Evicts finished records whose retention elapsed before `now_ms`,
    /// returning how many were removed.
    ///
    /// [`JobRegistry::spawn`] performs the same reclamation automatically
    /// with the system clock; this explicit form exists for deterministic
    /// callers and tests. Clock skew is saturated, never panicking.
    pub fn sweep(&self, now_ms: u64) -> usize {
        lock_inner(&self.shared).evict_expired(now_ms)
    }
}

// ── registry state ──────────────────────────────────────────────────────────

// ── authorization helpers ─────────────────────────────────────────────────────

/// Sends one `write_input` payload through a claimed PTY stdin half.
///
/// The half is returned to the caller either way (inside the outcome), so
/// the call site can hand it back to the record: a live half stays usable
/// for the next write, a broken one is dropped with the outcome.
fn write_to_pty_stdin(mut writer: PtyStdinWriter, data: &[u8]) -> WriteOutcome {
    use std::io::Write as _;
    let result = writer
        .write_all(data)
        .and_then(|()| writer.flush())
        .map(|()| data.len())
        .map_err(|error| {
            JobError::unsupported(format!("interactive stdin refused the write: {error}"))
        });
    WriteOutcome {
        result,
        writer: Some(writer),
    }
}

/// One `write_input_as` attempt: the result plus the claimed writer half.
///
/// A failed write drops the half with the outcome (the child is gone, so
/// the half is useless); a success returns it for the next call.
struct WriteOutcome {
    result: Result<usize, JobError>,
    writer: Option<PtyStdinWriter>,
}

impl WriteOutcome {
    fn ok_writer(&mut self) -> Option<PtyStdinWriter> {
        if self.result.is_ok() {
            self.writer.take()
        } else {
            None
        }
    }

    fn into_result(self) -> Result<usize, JobError> {
        self.result
    }
}

/// Authorizes `principal` for `operation` on an optionally-present record.
///
/// Deny by default with hidden existence: when the record is missing, or the
/// principal holds no grant, the same `Denied` is returned — never
/// `UnknownJob` — so a denied caller cannot probe whether the id is real.
/// Callers that pass authorization then re-resolve the record and report
/// `UnknownJob` on the unreachable path.
fn authorize_strict(
    #[allow(unused_variables)] inner: &RegistryInner,
    principal: &JobPrincipal,
    record: Option<&JobRecord>,
    id: JobId,
    operation: JobOperation,
) -> Result<(), JobError> {
    let _ = id;
    match record {
        Some(record) if record.authorized(principal, operation) => Ok(()),
        _ => Err(JobError::denied(
            operation,
            "caller holds no grant for this operation",
        )),
    }
}

/// Like [`authorize_strict`] but keyed for `observe` reads.
fn authorize_observe(
    inner: &RegistryInner,
    principal: &JobPrincipal,
    record: Option<&JobRecord>,
    id: JobId,
    operation: JobOperation,
) -> Result<(), JobError> {
    authorize_strict(inner, principal, record, id, operation)
}

fn lock_inner(shared: &Shared) -> MutexGuard<'_, RegistryInner> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct RegistryInner {
    capacity: usize,
    next_id: u64,
    jobs: BTreeMap<JobId, JobRecord>,
    events: DeliveryLog,
}

impl RegistryInner {
    /// Issues the next id. Ids start at 1 and are never reused.
    fn allocate_id(&mut self) -> Result<JobId, JobError> {
        let id = JobId::from_raw(self.next_id).ok_or_else(id_space_exhausted)?;
        self.next_id = self.next_id.checked_add(1).ok_or_else(id_space_exhausted)?;
        Ok(id)
    }

    /// Removes finished records whose retention elapsed.
    fn evict_expired(&mut self, now_ms: u64) -> usize {
        let before = self.jobs.len();
        self.jobs.retain(|_, record| {
            let (JobState::Done(_), Some(finished_at_ms)) = (record.state, record.finished_at_ms)
            else {
                return true;
            };
            let age = Duration::from_millis(now_ms.saturating_sub(finished_at_ms));
            age < record.spec.timeouts.effective_retention()
        });
        before - self.jobs.len()
    }
}

fn id_space_exhausted() -> JobError {
    JobError::Unavailable {
        reason: "job id space exhausted".into(),
    }
}

struct JobRecord {
    id: JobId,
    spec: JobSpec,
    state: JobState,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    control: Arc<JobControl>,
    output: OutputSink,
    /// Owning principal: assigned once at spawn, moves only via `transfer`.
    /// Ownership confers every operation implicitly (never a table entry).
    /// `None` marks a legacy phase-1 record (spawned through [`JobRegistry::spawn`]):
    /// scoped calls deny on it, so the legacy path never confers scoped
    /// authority and scoped ownership never leaks into the legacy path.
    owner: Option<JobPrincipal>,
    /// Explicit per-principal delegations on this job (bounded, per-job).
    grants: BTreeSet<JobGrant>,
    /// Per-principal `write_input` call budgets (bounded, per-job).
    write_budget: RateBudget,
    /// Per-principal `signal` call budgets (bounded, per-job).
    signal_budget: RateBudget,
    /// Live PTY stdin half, published by the supervisor once the PTY backend
    /// starts. `Mutex<Option<...>>` because exactly one `write_input_as`
    /// call may hold it at a time; pipe jobs and pre-start jobs hold `None`.
    pty_stdin: Arc<Mutex<Option<PtyStdinWriter>>>,
}

impl JobRecord {
    /// Owned record: `owner` holds the full operation set implicitly.
    fn owned(
        id: JobId,
        spec: JobSpec,
        control: Arc<JobControl>,
        output: OutputSink,
        owner: JobPrincipal,
    ) -> Self {
        Self {
            id,
            spec,
            state: JobState::Queued,
            started_at_ms: None,
            finished_at_ms: None,
            control,
            output,
            owner: Some(owner),
            grants: BTreeSet::new(),
            write_budget: RateBudget::new(
                MAX_WRITES_PER_WINDOW,
                Duration::from_millis(MAX_WRITE_INPUT_WINDOW_MS),
            ),
            signal_budget: RateBudget::new(
                MAX_SIGNALS_PER_WINDOW,
                Duration::from_millis(MAX_SIGNAL_WINDOW_MS),
            ),
            pty_stdin: Arc::new(Mutex::new(None)),
        }
    }

    /// Legacy phase-1 record: no owner, so the ambient `spawn`/`get` path
    /// keeps working. Scoped `*_as` calls deny on these records (there is
    /// no owner to authorize against), which keeps the two APIs from
    /// conferring authority on each other.
    fn unowned(id: JobId, spec: JobSpec, control: Arc<JobControl>, output: OutputSink) -> Self {
        Self {
            id,
            spec,
            state: JobState::Queued,
            started_at_ms: None,
            finished_at_ms: None,
            control,
            output,
            owner: None,
            grants: BTreeSet::new(),
            write_budget: RateBudget::new(
                MAX_WRITES_PER_WINDOW,
                Duration::from_millis(MAX_WRITE_INPUT_WINDOW_MS),
            ),
            signal_budget: RateBudget::new(
                MAX_SIGNALS_PER_WINDOW,
                Duration::from_millis(MAX_SIGNAL_WINDOW_MS),
            ),
            pty_stdin: Arc::new(Mutex::new(None)),
        }
    }

    fn authorized(&self, principal: &JobPrincipal, operation: JobOperation) -> bool {
        if self.owner.as_ref() == Some(principal) {
            return true;
        }
        self.grants
            .contains(&JobGrant::new(principal.clone(), operation))
    }

    /// Claims the live PTY stdin half for one `write_input_as` call.
    ///
    /// Returns `None` for pipe jobs, pre-start jobs, or while another write
    /// holds the half (fail-closed, never blocks).
    fn pty_writer(&self) -> Option<PtyStdinWriter> {
        self.pty_stdin.lock().ok()?.take()
    }

    /// Returns a claimed stdin half after the write (or a failed claim).
    fn return_pty_writer(&self, writer: Option<PtyStdinWriter>) {
        if let Some(writer) = writer
            && let Ok(mut slot) = self.pty_stdin.lock()
        {
            *slot = Some(writer);
        }
    }
}

/// The live PTY writer half, published by the supervisor once the PTY
/// backend starts and claimed by one `write_input_as` call at a time.
type PtyStdinWriter = bitty_pty::PtyWriter;

/// Fixed-window per-(job, operation) call budget.
///
/// Counts authorized calls only (denied callers never reach the budget), and
/// resets the window once it elapses. Bounded: two counters plus one
/// timestamp per budget, never a per-call log.
#[derive(Debug)]
struct RateBudget {
    max_calls: u64,
    window: Duration,
    window_start: Instant,
    used: u64,
}

impl RateBudget {
    fn new(max_calls: u64, window: Duration) -> Self {
        Self {
            max_calls,
            window,
            window_start: Instant::now(),
            used: 0,
        }
    }

    /// Records one authorized call; `false` means the budget is spent.
    fn check(&mut self) -> bool {
        let now = Instant::now();
        if now.saturating_duration_since(self.window_start) >= self.window {
            self.window_start = now;
            self.used = 0;
        }
        if self.used >= self.max_calls {
            return false;
        }
        self.used = self.used.saturating_add(1);
        true
    }
}

impl JobRecord {
    fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            id: self.id,
            state: self.state,
            spec: self.spec.clone(),
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
            output: self.output.index(),
        }
    }
}

/// Shared control surface between the registry and one supervisor thread.
#[derive(Debug)]
struct JobControl {
    cancel: AtomicBool,
    clock: Arc<ActivityClock>,
}

impl JobControl {
    fn new() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            clock: Arc::new(ActivityClock::new()),
        }
    }

    fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
}

/// Monotonic activity clock for the idle deadline.
///
/// Stores the last observed output time as milliseconds since job start;
/// drain threads touch it on every chunk, the supervisor reads it when
/// checking the idle deadline.
#[derive(Debug)]
struct ActivityClock {
    start: Instant,
    last_ms: AtomicU64,
}

impl ActivityClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Records activity at `now`.
    fn touch(&self) {
        self.touch_at(Instant::now());
    }

    /// Records activity at an explicit instant (deterministic tests).
    fn touch_at(&self, now: Instant) {
        self.last_ms.store(self.elapsed_ms(now), Ordering::Relaxed);
    }

    /// Time since the last recorded activity.
    fn idle_for(&self, now: Instant) -> Duration {
        let idle_ms = self
            .elapsed_ms(now)
            .saturating_sub(self.last_ms.load(Ordering::Relaxed));
        Duration::from_millis(idle_ms)
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.start).as_millis()).unwrap_or(u64::MAX)
    }
}

// ── supervision ─────────────────────────────────────────────────────────────

fn supervise(
    shared: Shared,
    id: JobId,
    spec: JobSpec,
    control: Arc<JobControl>,
    output: OutputSink,
) {
    lock_inner(&shared).events.push(JobEvent::Queued {
        id,
        at_ms: now_ms(),
    });
    if control.cancel_requested() {
        finish(&shared, id, JobStop::Cancelled);
        return;
    }
    // The stdin channel is published before the backend starts so a racing
    // `write_input_as` either finds it (PTY) or fails closed (pipes), never
    // a stale half from a previous job: ids are never reused.
    let stdin_writer_slot = stdin_slot_handle(&shared, id);
    let mut backend =
        match Backend::start(&spec, Arc::clone(&control.clock), output, stdin_writer_slot) {
            Ok(backend) => backend,
            Err(_) => {
                // Spawn failures are reported as a terminal state, never as a
                // crash: the caller keeps a job id to observe.
                finish(&shared, id, JobStop::SpawnFailed);
                return;
            }
        };
    mark_running(&shared, id);
    let stop = watch(&spec, &mut backend, &control);
    backend.terminate();
    // The terminal state is published before the detached drain threads
    // finish feeding the store (pipes hold buffered bytes after the process
    // is gone); output quiescence is a read-side concern, and waits must
    // never block a supervisor thread on a drain.
    finish(&shared, id, stop);
}

/// Returns the shared stdin-writer slot for `id` when the job is interactive.
///
/// Pipe jobs (and unknown ids) yield `None`: their `write_input_as` fails
/// closed with `Unsupported`. The slot is empty until the PTY backend takes
/// the writer half and publishes it (see `PtyJob::start`); writers claim it
/// for one call at a time.
fn stdin_slot_handle(shared: &Shared, id: JobId) -> Option<Arc<Mutex<Option<PtyStdinWriter>>>> {
    let inner = lock_inner(shared);
    let record = inner.jobs.get(&id)?;
    (record.spec.io == JobIo::Pty).then(|| Arc::clone(&record.pty_stdin))
}

/// Observes exit, cancel, and deadlines until the job terminates.
///
/// Checks the child first (a process that already exited is `Exited`, never
/// `Cancelled`), then the cancel request, then the deadlines. Sleeping
/// between checks keeps the supervisor thread off the hot path.
fn watch(spec: &JobSpec, backend: &mut Backend, control: &JobControl) -> JobStop {
    let hard_deadline = spec
        .timeouts
        .hard
        .and_then(|hard| Instant::now().checked_add(hard));
    loop {
        if backend.exited() {
            return JobStop::Exited;
        }
        if control.cancel_requested() {
            return JobStop::Cancelled;
        }
        let now = Instant::now();
        if hard_deadline.is_some_and(|deadline| now >= deadline) {
            return JobStop::TimedOut;
        }
        if spec
            .timeouts
            .idle
            .is_some_and(|idle| control.clock.idle_for(now) >= idle)
        {
            return JobStop::TimedOut;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn mark_running(shared: &Shared, id: JobId) {
    let at_ms = now_ms();
    let mut inner = lock_inner(shared);
    if let Some(record) = inner.jobs.get_mut(&id) {
        record.state = JobState::Running;
        record.started_at_ms = Some(at_ms);
    }
    inner.events.push(JobEvent::Started { id, at_ms });
}

fn finish(shared: &Shared, id: JobId, stop: JobStop) {
    let at_ms = now_ms();
    let mut inner = lock_inner(shared);
    if let Some(record) = inner.jobs.get_mut(&id) {
        record.state = JobState::Done(stop);
        record.finished_at_ms = Some(at_ms);
    }
    inner.events.push(JobEvent::Stopped { id, stop, at_ms });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

// ── process backends ────────────────────────────────────────────────────────

enum Backend {
    Pipe(PipeJob),
    Pty(PtyJob),
}

impl Backend {
    fn start(
        spec: &JobSpec,
        clock: Arc<ActivityClock>,
        output: OutputSink,
        stdin_writer_slot: Option<Arc<Mutex<Option<PtyStdinWriter>>>>,
    ) -> Result<Self, String> {
        match spec.io {
            JobIo::Pipes => PipeJob::start(spec, clock, output).map(Self::Pipe),
            JobIo::Pty => PtyJob::start(spec, clock, output, stdin_writer_slot).map(Self::Pty),
        }
    }

    /// Non-blocking exit observation; `true` once the process is reaped.
    fn exited(&mut self) -> bool {
        match self {
            Self::Pipe(job) => job.exited(),
            Self::Pty(job) => job.exited(),
        }
    }

    /// Idempotent kill-and-reap of the direct child.
    fn terminate(&mut self) {
        match self {
            Self::Pipe(job) => job.terminate(),
            Self::Pty(job) => job.terminate(),
        }
    }
}

/// Pipe-backed job: closed stdin, piped stdout/stderr, bounded drain.
///
/// Each drain thread feeds its stream into the job's bounded output store
/// (newest bytes win, oldest evicted first) and timestamps activity for the
/// idle deadline. Drain handles are joined (not detached) once the child is
/// reaped, so the store is quiescent before the supervisor publishes the
/// terminal event. A grandchild that inherits a pipe can still delay that
/// join; owned-process-tree cleanup stays CTX-0512.
struct PipeJob {
    child: Child,
    exited: bool,
    drains: Vec<thread::JoinHandle<()>>,
}

impl PipeJob {
    fn start(
        spec: &JobSpec,
        clock: Arc<ActivityClock>,
        output: OutputSink,
    ) -> Result<Self, String> {
        let mut command =
            closed_pipe_command(&spec.program, &spec.args, spec.cwd.as_deref(), &spec.env);
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let mut drains = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            drains.push(spawn_stdout_drain(
                stdout,
                Arc::clone(&clock),
                output.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            drains.push(spawn_stderr_drain(stderr, clock, output));
        }
        Ok(Self {
            child,
            exited: false,
            drains,
        })
    }

    fn exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                // Reap the child, then join the drains so buffered pipe
                // bytes land in the store before the terminal event. The
                // drains end at EOF once the last writer (the child) is
                // gone; a grandchild holding a pipe open delays this join.
                // To keep the supervisor fail-closed instead of wedged,
                // the join is time-boxed (CTX-0512 owns process-tree
                // cleanup): bytes drained so far stay in the store either
                // way.
                let _ = self.child.wait();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !self.drains.iter().all(|drain| drain.is_finished()) {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                for drain in self.drains.drain(..) {
                    if !drain.is_finished() {
                        continue;
                    }
                    let _ = drain.join();
                }
                true
            }
            Ok(None) => false,
            // The process cannot be observed anymore; stop the watch loop
            // and let `terminate` attempt the kill/reap below.
            Err(_) => true,
        }
    }

    fn terminate(&mut self) {
        if self.exited {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort drain join: never block termination on a grandchild
        // holding a pipe (CTX-0512 owns process-tree cleanup). A thread that
        // has not finished keeps its store clone until EOF, still under the
        // per-stream bound, while the record keeps the bytes drained so far.
        for drain in self.drains.drain(..) {
            if !drain.is_finished() {
                continue;
            }
            let _ = drain.join();
        }
        self.exited = true;
    }
}

/// PTY-backed interactive job: `PtyBuilder` spawn plus a drain thread.
///
/// The PTY primitive inherits the session environment by its accepted
/// terminal contract (DEC-0017) with explicit variables as overrides; the
/// model rejects isolated PTY jobs so no ambient variable can flow in
/// unnoticed.
struct PtyJob {
    pty: Pty,
    exited: bool,
}

impl PtyJob {
    fn start(
        spec: &JobSpec,
        clock: Arc<ActivityClock>,
        output: OutputSink,
        stdin_writer_slot: Option<Arc<Mutex<Option<PtyStdinWriter>>>>,
    ) -> Result<Self, String> {
        let mut builder = PtyBuilder::new(&spec.program);
        builder = builder.args(spec.args.iter().cloned());
        if let Some(cwd) = &spec.cwd {
            builder = builder.cwd(cwd);
        }
        if let EnvPolicy::Explicit { vars } = &spec.env {
            for var in vars {
                builder = builder.env(&var.name, &var.value);
            }
        }
        let mut pty = builder.spawn().map_err(|error| error.to_string())?;
        let reader = pty.take_reader().map_err(|error| error.to_string())?;
        spawn_pty_drain(reader, clock, output);
        // Publish the writer half into the shared slot (when a scoped job
        // asked for one): `write_input_as` claims it for one call at a time
        // and returns it, so concurrent writers serialize on the slot
        // instead of racing on `take_writer` (which the PTY primitive grants
        // only once). No slot (legacy `spawn` path) keeps the previous
        // behavior — the half is never taken. The publish happens here, on
        // the supervisor thread that owns the PTY handle — never under the
        // registry lock — so there is no lock-ordering hazard.
        if let Some(slot) = stdin_writer_slot {
            // A racing `write_input_as` may claim the slot while it is still
            // `None` (backend starting): that call fails closed with
            // `Unsupported` and retries after `Running` is observable.
            if let Ok(writer) = pty.take_writer()
                && let Ok(mut guard) = slot.lock()
            {
                *guard = Some(writer);
            }
        }
        Ok(Self { pty, exited: false })
    }

    fn exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        match self.pty.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                true
            }
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn terminate(&mut self) {
        if self.exited {
            return;
        }
        // `shutdown` is kill-then-reap; if the kill reports the child is
        // already gone it returns before blocking on a wait.
        let _ = self.pty.shutdown();
        self.exited = true;
    }
}

/// Drains one stdout pipe into the bounded store, touching `clock` per
/// chunk (both stdout activity and the drain keep the idle clock honest).
/// The handle is joined after the child is reaped (see [`PipeJob`]); memory
/// stays bounded because the store evicts oldest-first.
fn spawn_stdout_drain(
    mut pipe: impl Read + Send + 'static,
    clock: Arc<ActivityClock>,
    output: OutputSink,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("bitty-job-drain".into())
        .spawn(move || {
            let mut chunk = [0u8; DRAIN_CHUNK_BYTES];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        clock.touch();
                        output.push_stdout(&chunk[..n]);
                    }
                }
            }
        })
        .expect("job drain thread spawns")
}

/// Drains one stderr pipe into the bounded store; joined like
/// [`spawn_stdout_drain`].
fn spawn_stderr_drain(
    mut pipe: impl Read + Send + 'static,
    clock: Arc<ActivityClock>,
    output: OutputSink,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("bitty-job-drain".into())
        .spawn(move || {
            let mut chunk = [0u8; DRAIN_CHUNK_BYTES];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        clock.touch();
                        output.push_stderr(&chunk[..n]);
                    }
                }
            }
        })
        .expect("job drain thread spawns")
}

/// Drains a PTY reader into the bounded stdout store, touching `clock` per
/// chunk. Detached for the same reason as [`spawn_stdout_drain`]; the reader
/// channel is bounded by the `bitty-pty` backpressure contract. PTY output
/// has no separate stderr: the terminal merges both streams.
fn spawn_pty_drain(reader: PtyReader, clock: Arc<ActivityClock>, output: OutputSink) {
    let _ = thread::Builder::new()
        .name("bitty-job-pty-drain".into())
        .spawn(move || {
            while let Ok(Some(chunk)) = reader.recv() {
                if !chunk.is_empty() {
                    clock.touch();
                    output.push_stdout(&chunk);
                }
            }
        });
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::delivery::MAX_STORED_OBSERVATION_EVENTS;
    use super::super::output::OutputStream;
    use super::*;
    use crate::execution::{DEFAULT_RETENTION_TTL, JobLifetime, JobOrigin, JobTimeouts};
    use std::time::Duration;

    /// A program that cannot exist on any host: spawn always fails, so these
    /// tests observe registry bookkeeping without creating processes.
    const MISSING_PROGRAM: &str = "bitty-ct0511-nonexistent-job-program";

    fn missing_spec() -> JobSpec {
        JobSpec::new(MISSING_PROGRAM, vec!["--never".to_owned()])
    }

    fn wait_for(
        registry: &JobRegistry,
        id: JobId,
        what: &str,
        predicate: impl Fn(&JobSnapshot) -> bool,
    ) -> JobSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = registry.get(id).expect("job stays tracked");
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} did not reach {what} in time"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_terminal(registry: &JobRegistry, id: JobId) -> JobSnapshot {
        wait_for(registry, id, "a terminal state", |snapshot| {
            snapshot.state.is_terminal()
        })
    }

    #[test]
    fn registry_starts_empty_and_keeps_its_capacity() {
        let registry = JobRegistry::new();
        assert_eq!(registry.capacity(), DEFAULT_MAX_JOBS);
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.list().is_empty());
        assert_eq!(registry.events_dropped(), 0);
        assert_eq!(registry.sweep(0), 0);
    }

    #[test]
    fn registry_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobRegistry>();
        assert_send_sync::<JobSnapshot>();
        assert_send_sync::<JobSpec>();
    }

    #[test]
    fn unknown_ids_fail_closed() {
        let registry = JobRegistry::new();
        let unknown = JobId::from_raw(9_999).expect("non-zero");
        assert_eq!(registry.get(unknown), Err(JobError::UnknownJob(unknown)));
        assert_eq!(registry.cancel(unknown), Err(JobError::UnknownJob(unknown)));
        assert_eq!(JobId::from_raw(0), None);
    }

    #[test]
    fn quiet_failed_spawn_becomes_a_terminal_observation() {
        let registry = JobRegistry::new();
        let id = registry.spawn(missing_spec()).expect("tracked");
        let snapshot = wait_terminal(&registry, id);
        assert_eq!(snapshot.state, JobState::Done(JobStop::SpawnFailed));
        assert!(snapshot.started_at_ms.is_none());
        assert!(snapshot.finished_at_ms.is_some());
        assert_eq!(snapshot.spec.kind.as_str(), "command");

        let events = registry.drain_events(MAX_STORED_JOB_EVENTS);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[0], JobEvent::Queued { id: first, .. } if first == id));
        assert!(matches!(
            events[1],
            JobEvent::Stopped {
                id: second,
                stop: JobStop::SpawnFailed,
                ..
            } if second == id
        ));
        assert!(registry.drain_events(1).is_empty());
    }

    #[test]
    fn ids_are_unique_and_never_reused_after_eviction() {
        let registry = JobRegistry::new();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let spec = JobSpec::new(MISSING_PROGRAM, Vec::new())
                .with_timeouts(JobTimeouts::default().with_retention(Duration::ZERO));
            let id = registry.spawn(spec).expect("tracked");
            wait_terminal(&registry, id);
            assert!(!seen.contains(&id), "id {id} was reused");
            seen.push(id);
        }
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
        // Every spawn reclaimed the previous expired record, so only the
        // newest record is still tracked; eviction never resurrects an id.
        assert_eq!(registry.sweep(u64::MAX), 1);
        assert!(registry.is_empty());
        assert_eq!(registry.get(seen[0]), Err(JobError::UnknownJob(seen[0])));
    }

    #[test]
    fn capacity_fails_closed_until_retention_reclaims_space() {
        let registry = JobRegistry::with_capacity(1);
        let first = registry.spawn(missing_spec()).expect("first tracked");
        wait_terminal(&registry, first);
        assert_eq!(
            registry.spawn(missing_spec()),
            Err(JobError::RegistryFull { limit: 1 })
        );

        let reclaiming = JobRegistry::with_capacity(1);
        let spec =
            missing_spec().with_timeouts(JobTimeouts::default().with_retention(Duration::ZERO));
        let first = reclaiming.spawn(spec).expect("first tracked");
        let finished = wait_terminal(&reclaiming, first);
        let second = reclaiming
            .spawn(missing_spec())
            .expect("expired record reclaimed at spawn");
        assert!(second > first);
        assert_eq!(reclaiming.len(), 1);
        assert_eq!(finished.state, JobState::Done(JobStop::SpawnFailed));
    }

    #[test]
    fn sweep_respects_the_retention_window() {
        let registry = JobRegistry::new();
        let id = registry.spawn(missing_spec()).expect("tracked");
        let finished = wait_terminal(&registry, id);
        let finished_at = finished.finished_at_ms.expect("terminal time");
        let retention_ms = u64::try_from(DEFAULT_RETENTION_TTL.as_millis()).expect("fits");
        assert_eq!(registry.sweep(finished_at), 0);
        assert_eq!(registry.sweep(finished_at + retention_ms - 1), 0);
        assert_eq!(registry.sweep(finished_at + retention_ms), 1);
        assert!(registry.is_empty());
        // Clock skew saturates instead of panicking.
        let skewed = JobRegistry::new();
        let id = skewed.spawn(missing_spec()).expect("tracked");
        wait_terminal(&skewed, id);
        assert_eq!(skewed.sweep(0), 0);
        assert_eq!(skewed.len(), 1);
    }

    #[test]
    fn cancel_after_a_terminal_state_reports_it() {
        let registry = JobRegistry::new();
        let id = registry.spawn(missing_spec()).expect("tracked");
        wait_terminal(&registry, id);
        assert_eq!(
            registry.cancel(id),
            Ok(JobCancel::AlreadyStopped(JobStop::SpawnFailed))
        );
    }

    #[test]
    fn snapshots_carry_declared_metadata() {
        let registry = JobRegistry::new();
        let spec = missing_spec()
            .with_lifetime(JobLifetime::Workspace)
            .with_origin(JobOrigin::panel("panel-3"));
        let id = registry.spawn(spec).expect("tracked");
        let snapshot = registry.get(id).expect("tracked");
        assert_eq!(snapshot.id, id);
        assert_eq!(snapshot.state, JobState::Queued);
        assert_eq!(snapshot.spec.lifetime, JobLifetime::Workspace);
        assert_eq!(snapshot.spec.origin.panel_id(), Some("panel-3"));
        assert!(snapshot.started_at_ms.is_none());
        assert!(snapshot.finished_at_ms.is_none());
    }

    #[test]
    fn list_never_exceeds_capacity() {
        let registry = JobRegistry::with_capacity(2);
        for _ in 0..2 {
            registry.spawn(missing_spec()).expect("tracked");
        }
        assert_eq!(registry.list().len(), 2);
        assert_eq!(
            registry.spawn(missing_spec()),
            Err(JobError::RegistryFull { limit: 2 })
        );
        assert!(registry.list().len() <= registry.capacity());
    }

    #[test]
    fn observation_lane_drops_oldest_and_counts() {
        let id = JobId::from_raw(1).expect("non-zero");
        let mut log = DeliveryLog::new();
        for at_ms in 1..=(MAX_STORED_OBSERVATION_EVENTS as u64 + 1) {
            log.push(JobEvent::Queued { id, at_ms });
        }
        assert_eq!(log.observation_dropped(), 1);
        assert_eq!(log.critical_dropped(), 0);
        assert_eq!(log.dropped(), 1);
        let drained = log.drain(MAX_STORED_OBSERVATION_EVENTS + 1);
        assert_eq!(drained.len(), MAX_STORED_OBSERVATION_EVENTS);
        assert!(matches!(drained[0], JobEvent::Queued { at_ms: 2, .. }));
        assert!(log.drain(10).is_empty());
    }

    #[test]
    fn snapshots_carry_an_empty_output_index_until_bytes_arrive() {
        let registry = JobRegistry::new();
        let id = registry.spawn(missing_spec()).expect("tracked");
        let snapshot = wait_terminal(&registry, id);
        assert_eq!(snapshot.output, OutputIndex::default());
        assert!(!snapshot.output.is_truncated());
        assert_eq!(
            registry.output_index(id).expect("tracked"),
            OutputIndex::default()
        );
        let read = ReadOutput::new(OutputStream::Stdout)
            .validate()
            .expect("valid");
        let view = registry
            .read_output(id, ReadOutput::new(OutputStream::Stdout))
            .expect("readable");
        assert!(view.text.is_empty());
        assert!(!view.truncated);
        let _ = read;
    }

    #[test]
    fn activity_clock_tracks_idle_gaps() {
        let clock = ActivityClock::new();
        let start = clock.start;
        assert_eq!(clock.idle_for(start), Duration::ZERO);
        clock.touch_at(start + Duration::from_millis(10));
        assert_eq!(
            clock.idle_for(start + Duration::from_millis(30)),
            Duration::from_millis(20)
        );
        clock.touch_at(start + Duration::from_millis(30));
        assert_eq!(
            clock.idle_for(start + Duration::from_millis(30)),
            Duration::ZERO
        );
        assert_eq!(
            clock.idle_for(start + Duration::from_millis(45)),
            Duration::from_millis(15)
        );
    }
}
