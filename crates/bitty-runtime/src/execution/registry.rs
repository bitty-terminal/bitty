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

use std::collections::BTreeMap;
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
    JobCancel, JobError, JobEvent, JobId, JobIo, JobSnapshot, JobSpec, JobState, JobStop,
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
                JobRecord {
                    id,
                    spec: spec.clone(),
                    state: JobState::Queued,
                    started_at_ms: None,
                    finished_at_ms: None,
                    control: Arc::clone(&control),
                    output: output.clone(),
                },
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
    let mut backend = match Backend::start(&spec, Arc::clone(&control.clock), output) {
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
    ) -> Result<Self, String> {
        match spec.io {
            JobIo::Pipes => PipeJob::start(spec, clock, output).map(Self::Pipe),
            JobIo::Pty => PtyJob::start(spec, clock, output).map(Self::Pty),
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
