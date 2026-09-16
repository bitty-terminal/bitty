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

use std::collections::{BTreeMap, VecDeque};
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
use super::model::{
    JobCancel, JobError, JobEvent, JobId, JobIo, JobSnapshot, JobSpec, JobState, JobStop,
};

/// Default registry capacity.
///
/// Reuses the accepted CTX-0442 tracked-execution bound
/// (`bitty_ipc::execution::MAX_TRACKED_EXECUTIONS`, 64) so the job table has
/// the same hard ceiling as the synchronous surface. Overflow fails closed;
/// finished records become evictable only after their retention elapses.
pub const DEFAULT_MAX_JOBS: usize = bitty_ipc::execution::MAX_TRACKED_EXECUTIONS;

/// Maximum queued lifecycle events before the oldest is dropped.
pub const MAX_STORED_JOB_EVENTS: usize = 256;

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
                events: EventQueue::new(MAX_STORED_JOB_EVENTS),
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
        let (id, control) = {
            let mut inner = lock_inner(&self.shared);
            inner.evict_expired(now_ms());
            if inner.jobs.len() >= inner.capacity {
                return Err(JobError::RegistryFull {
                    limit: inner.capacity,
                });
            }
            let id = inner.allocate_id()?;
            let control = Arc::new(JobControl::new());
            inner.jobs.insert(
                id,
                JobRecord {
                    id,
                    spec: spec.clone(),
                    state: JobState::Queued,
                    started_at_ms: None,
                    finished_at_ms: None,
                    control: Arc::clone(&control),
                },
            );
            (id, control)
        };
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name(format!("bitty-job-{}", id.get()))
            .spawn(move || supervise(shared, id, spec, control))
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
    #[must_use]
    pub fn drain_events(&self, limit: usize) -> Vec<JobEvent> {
        lock_inner(&self.shared).events.drain(limit)
    }

    /// Lifecycle events dropped by the bounded observation queue so far.
    #[must_use]
    pub fn events_dropped(&self) -> u64 {
        lock_inner(&self.shared).events.dropped()
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
    events: EventQueue,
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
}

impl JobRecord {
    fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            id: self.id,
            state: self.state,
            spec: self.spec.clone(),
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
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

/// Bounded FIFO of lifecycle events; the oldest is dropped at capacity
/// (reliable delivery is CTX-0513).
#[derive(Debug)]
struct EventQueue {
    events: VecDeque<JobEvent>,
    capacity: usize,
    dropped: u64,
}

impl EventQueue {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "event queue capacity must be > 0");
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    fn push(&mut self, event: JobEvent) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.events.push_back(event);
    }

    fn drain(&mut self, limit: usize) -> Vec<JobEvent> {
        let take = limit.min(self.events.len());
        self.events.drain(..take).collect()
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ── supervision ─────────────────────────────────────────────────────────────

fn supervise(shared: Shared, id: JobId, spec: JobSpec, control: Arc<JobControl>) {
    lock_inner(&shared).events.push(JobEvent::Queued {
        id,
        at_ms: now_ms(),
    });
    if control.cancel_requested() {
        finish(&shared, id, JobStop::Cancelled);
        return;
    }
    let mut backend = match Backend::start(&spec, Arc::clone(&control.clock)) {
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
    fn start(spec: &JobSpec, clock: Arc<ActivityClock>) -> Result<Self, String> {
        match spec.io {
            JobIo::Pipes => PipeJob::start(spec, clock).map(Self::Pipe),
            JobIo::Pty => PtyJob::start(spec, clock).map(Self::Pty),
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
/// Phase 1 retains no output bytes (the store is CTX-0513); the drain
/// threads exist to keep the child unblocked and to timestamp activity for
/// the idle deadline.
struct PipeJob {
    child: Child,
    exited: bool,
}

impl PipeJob {
    fn start(spec: &JobSpec, clock: Arc<ActivityClock>) -> Result<Self, String> {
        let mut command =
            closed_pipe_command(&spec.program, &spec.args, spec.cwd.as_deref(), &spec.env);
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        if let Some(stdout) = child.stdout.take() {
            spawn_drain(stdout, Arc::clone(&clock));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_drain(stderr, clock);
        }
        Ok(Self {
            child,
            exited: false,
        })
    }

    fn exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.exited = true;
                let _ = self.child.wait();
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
    fn start(spec: &JobSpec, clock: Arc<ActivityClock>) -> Result<Self, String> {
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
        spawn_pty_drain(reader, clock);
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

/// Drains one pipe to EOF without retaining bytes, touching `clock` per
/// chunk. The thread is detached on purpose: a grandchild that inherits the
/// pipe must not be able to keep the job alive (owned-process-tree cleanup
/// is CTX-0512). Memory stays bounded because nothing is retained.
fn spawn_drain(mut pipe: impl Read + Send + 'static, clock: Arc<ActivityClock>) {
    let _ = thread::Builder::new()
        .name("bitty-job-drain".into())
        .spawn(move || {
            let mut chunk = [0u8; DRAIN_CHUNK_BYTES];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => clock.touch(),
                }
            }
        });
}

/// Drains a PTY reader until EOF, touching `clock` per chunk. Detached for
/// the same reason as [`spawn_drain`]; the reader channel is bounded by the
/// `bitty-pty` backpressure contract.
fn spawn_pty_drain(reader: PtyReader, clock: Arc<ActivityClock>) {
    let _ = thread::Builder::new()
        .name("bitty-job-pty-drain".into())
        .spawn(move || {
            while let Ok(Some(chunk)) = reader.recv() {
                if !chunk.is_empty() {
                    clock.touch();
                }
            }
        });
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
    fn event_queue_drops_oldest_and_counts() {
        let id = JobId::from_raw(1).expect("non-zero");
        let mut queue = EventQueue::new(2);
        queue.push(JobEvent::Queued { id, at_ms: 1 });
        queue.push(JobEvent::Started { id, at_ms: 2 });
        queue.push(JobEvent::Stopped {
            id,
            stop: JobStop::Exited,
            at_ms: 3,
        });
        assert_eq!(queue.dropped(), 1);
        let drained = queue.drain(10);
        assert_eq!(drained.len(), 2);
        assert!(matches!(drained[0], JobEvent::Started { at_ms: 2, .. }));
        assert!(matches!(drained[1], JobEvent::Stopped { at_ms: 3, .. }));
        assert_eq!(queue.drain(10).len(), 0);
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
