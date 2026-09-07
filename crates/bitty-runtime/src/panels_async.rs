#![forbid(unsafe_code)]
//! Bounded async workers for panel data collection (CTX-0215).
//!
//! Problem (FIND-0002 triage): `tabs`, `statusline`, `git_panel`, and
//! `file_manager` derive panel data synchronously wherever the tick consumes
//! it. Cheap derivations are harmless, but expensive probes (`git status`
//! process spawns, filesystem directory scans) executed inline can stall the
//! render tick.
//!
//! This module moves probe execution off the tick path behind bounded worker
//! threads built only on `std` (`thread` + `mpsc::sync_channel` + atomics):
//! no new runtime dependencies, no `tokio`, no Unix-only APIs (portable to
//! the `x86_64-pc-windows-gnu` check gate).
//!
//! # Design choice: worker-per-expensive-source
//!
//! One [`PanelWorker`] thread is spawned per panel source instead of a single
//! shared worker with a job queue. Rationale:
//!
//! - Probes have independent cadences and costs. A shared worker would
//!   head-of-line-block a cheap statusline/tab refresh behind a slow
//!   `git status` spawn or a large directory scan, reintroducing the stall
//!   this task removes (only relocated, not fixed).
//! - Per-source threads isolate overload and failure per panel and match the
//!   existing per-panel module ownership (`tabs.rs`, `statusline.rs`,
//!   `git_panel.rs`, `file_manager.rs` each drive their own worker).
//! - Cost is at most a handful of threads parked in `recv_timeout`; there is
//!   no per-tick allocation and no shared job enum, routing, or fairness
//!   scheduler to audit.
//!
//! # Tick contract (last-known-good snapshots)
//!
//! - The tick never runs a probe and never waits for one. [`PanelWorker::latest`]
//!   clones the newest completed snapshot under a brief mutex that the worker
//!   holds only while swapping an `Arc` pointer (never across probe
//!   execution), so a slow probe cannot stall the tick.
//! - [`PanelWorker::request_refresh`] only enqueues a coalescable unit token
//!   via non-blocking `try_send`; it never blocks the caller.
//! - Refresh tokens are interchangeable: when the bounded request queue is
//!   full the new token is shed (counted in [`PanelWorker::dropped`]) because
//!   a refresh is already pending, which is observationally equivalent to
//!   drop-oldest for identical tokens. The snapshot slot itself keeps exactly
//!   one value with overwrite (newest-wins) semantics.
//! - Nothing grows without bound: the request queue is capped at
//!   [`PANEL_WORKER_MAX_QUEUE_CAP`], the snapshot slot holds one `Arc<T>`,
//!   and snapshot payloads `T` must already respect the owning panel's
//!   bounds (e.g. `<=128` entries, `8 KiB` bus payload cap). Counters use
//!   wrapping/saturating arithmetic and never allocate.
//! - Teardown is prompt and joined: [`PanelWorker::shutdown`] signals the
//!   worker, wakes it, and joins the thread. [`Drop`] performs the same
//!   best-effort join without panicking, so an abandoned worker cannot
//!   outlive its owner silently or hang teardown.
//!
//! # Example
//!
//! ```rust
//! use bitty_runtime::panels_async::PanelWorker;
//! use std::time::Duration;
//!
//! let mut worker = PanelWorker::try_spawn(
//!     "statusline",
//!     Some("boot".to_string()),
//!     2,
//!     Duration::from_millis(50),
//!     || "cwd:~/projects".to_string(),
//! )
//! .expect("valid worker config");
//! worker.request_refresh();
//! let snapshot = worker.latest();
//! assert!(snapshot.is_some());
//! worker.shutdown();
//! assert!(!worker.is_alive());
//! ```

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, RecvTimeoutError, SyncSender},
};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::RuntimeError;

/// Default bounded refresh-queue depth per worker (coalescable unit tokens).
pub const PANEL_WORKER_DEFAULT_QUEUE_CAP: usize = 4;

/// Maximum bounded refresh-queue depth per worker. Larger values are rejected
/// fail-closed by [`PanelWorker::try_spawn`].
pub const PANEL_WORKER_MAX_QUEUE_CAP: usize = 16;

/// Default background poll cadence: the worker re-runs its probe at this
/// interval even without an explicit [`PanelWorker::request_refresh`], so
/// snapshots stay fresh without tick involvement.
pub const PANEL_WORKER_DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Latest completed snapshot slot shared between the worker thread (writer)
/// and the tick (reader). The writer swaps an `Arc` pointer under a brief
/// lock; probes always run outside the lock.
struct SnapshotSlot<T> {
    latest: Option<Arc<T>>,
    generation: u64,
    completed_probes: u64,
}

/// Bounded async worker hosting one expensive panel probe off the tick path.
///
/// `T` is the panel's snapshot payload (already bounded by the owning panel's
/// constants). See the module docs for the tick contract.
pub struct PanelWorker<T> {
    tx: SyncSender<()>,
    slot: Arc<Mutex<SnapshotSlot<T>>>,
    shutdown_flag: Arc<AtomicBool>,
    /// Best-effort count of tokens waiting in the request queue (telemetry).
    queued: Arc<AtomicUsize>,
    /// Tokens shed because the bounded queue was full (telemetry).
    dropped: Arc<AtomicU64>,
    queue_cap: usize,
    handle: Option<JoinHandle<()>>,
}

impl<T> PanelWorker<T> {
    /// Spawns a worker thread named `bitty-panel-{name}` running `probe`.
    ///
    /// - `initial` is the last-known-good snapshot served until the first
    ///   probe completes (`None` means "no data yet").
    /// - `queue_cap` must be `1..=PANEL_WORKER_MAX_QUEUE_CAP`; `0` or larger
    ///   fails closed with [`RuntimeError::InvalidQueueCapacity`].
    /// - A zero `poll_interval` falls back to
    ///   [`PANEL_WORKER_DEFAULT_POLL_INTERVAL`] (a zero cadence would
    ///   busy-loop the worker).
    /// - Thread-spawn failure maps to [`RuntimeError::InvalidConfig`] with a
    ///   static message so no upstream/`io` type escapes this crate.
    pub fn try_spawn(
        name: &str,
        initial: Option<T>,
        queue_cap: usize,
        poll_interval: Duration,
        probe: impl Fn() -> T + Send + 'static,
    ) -> Result<Self, RuntimeError>
    where
        T: Clone + Send + Sync + 'static,
    {
        if queue_cap == 0 || queue_cap > PANEL_WORKER_MAX_QUEUE_CAP {
            return Err(RuntimeError::InvalidQueueCapacity);
        }
        let poll_interval = if poll_interval.is_zero() {
            PANEL_WORKER_DEFAULT_POLL_INTERVAL
        } else {
            poll_interval
        };
        let (tx, rx) = mpsc::sync_channel::<()>(queue_cap);
        let slot = Arc::new(Mutex::new(SnapshotSlot {
            latest: initial.map(Arc::new),
            generation: 0,
            completed_probes: 0,
        }));
        let worker = Self {
            tx,
            slot: Arc::clone(&slot),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            queued: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            queue_cap,
            handle: None,
        };
        let thread_name = format!("bitty-panel-{name}");
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn({
                let slot = Arc::clone(&slot);
                let shutdown_flag = Arc::clone(&worker.shutdown_flag);
                let queued = Arc::clone(&worker.queued);
                move || Self::run(rx, slot, shutdown_flag, queued, poll_interval, probe)
            })
            .map_err(|_| RuntimeError::InvalidConfig("panel worker thread spawn failed"))?;
        worker.with_handle(handle)
    }

    fn with_handle(mut self, handle: JoinHandle<()>) -> Result<Self, RuntimeError> {
        self.handle = Some(handle);
        Ok(self)
    }

    /// Worker main loop: serve refresh tokens and periodic polls, one probe
    /// per wake with coalescing drain. Probes run outside the slot lock.
    fn run(
        rx: mpsc::Receiver<()>,
        slot: Arc<Mutex<SnapshotSlot<T>>>,
        shutdown_flag: Arc<AtomicBool>,
        queued: Arc<AtomicUsize>,
        poll_interval: Duration,
        probe: impl Fn() -> T,
    ) where
        T: Send + Sync + 'static,
    {
        loop {
            match rx.recv_timeout(poll_interval) {
                Ok(()) => {
                    dec_saturating(&queued);
                    // Coalesce: pending tokens collapse into this single run.
                    while rx.try_recv().is_ok() {
                        dec_saturating(&queued);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if shutdown_flag.load(Ordering::Relaxed) {
                break;
            }
            let value = probe();
            let fresh = Arc::new(value);
            if let Ok(mut guard) = slot.lock() {
                guard.latest = Some(fresh);
                guard.generation = guard.generation.wrapping_add(1);
                guard.completed_probes = guard.completed_probes.wrapping_add(1);
            }
        }
    }

    /// Requests a refresh without blocking. Always returns immediately; when
    /// the bounded queue is full the request is shed (a refresh is already
    /// pending) and counted in [`Self::dropped`].
    pub fn request_refresh(&self) {
        if self.tx.try_send(()).is_ok() {
            self.queued.fetch_add(1, Ordering::Relaxed);
        } else {
            // Full (shed, coalesced) — or disconnected after teardown; only
            // full-queue sheds count as load shedding.
            if self.is_alive() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Best-effort count of refresh tokens currently queued. Telemetry only;
    /// never exceeds [`Self::queue_cap`] plus in-flight handover.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Refresh requests shed because the bounded queue was full. Telemetry
    /// for `bitty plugin doctor`-style attribution (shed, never lost work:
    /// a refresh was already pending or a newer poll supersedes).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Configured bounded queue depth.
    #[must_use]
    pub const fn queue_cap(&self) -> usize {
        self.queue_cap
    }

    /// Whether the worker thread is still running.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Signals shutdown, wakes the worker, and joins the thread. Prompt:
    /// at most one in-flight probe plus join. Idempotent and panic-free.
    pub fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        // Best-effort wake so join does not wait out the poll interval.
        // The token may be shed or orphaned after exit; the worker's
        // saturating accounting keeps `pending()` bounded regardless.
        let _ = self.tx.try_send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl<T> PanelWorker<T>
where
    T: Clone,
{
    /// Returns the latest completed snapshot (last-known-good), or `None`
    /// when no probe has completed and no `initial` was provided.
    ///
    /// Never runs or waits for a probe: the slot lock is held only while
    /// cloning an `Arc` pointer, so a slow probe cannot stall the caller.
    #[must_use]
    pub fn latest(&self) -> Option<T> {
        self.slot
            .lock()
            .ok()
            .and_then(|guard| guard.latest.clone())
            .map(|arc| (*arc).clone())
    }

    /// Monotonic count of completed probes that produced [`Self::latest`].
    /// The tick can compare generations to detect freshness.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.slot.lock().ok().map_or(0, |guard| guard.generation)
    }

    /// Total probes completed by the worker thread.
    #[must_use]
    pub fn completed(&self) -> u64 {
        self.slot
            .lock()
            .ok()
            .map_or(0, |guard| guard.completed_probes)
    }
}

impl<T> Drop for PanelWorker<T> {
    /// Best-effort [`PanelWorker::shutdown`] without panicking: a dropped
    /// worker never leaks its thread and never hangs teardown on a join
    /// error (e.g. a panicked probe, which is swallowed here).
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Saturating decrement for telemetry counters shared with the worker loop.
fn dec_saturating(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Relaxed);
    while current > 0 {
        match counter.compare_exchange_weak(
            current,
            current.saturating_sub(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    /// Waits up to `deadline` for `cond`, polling briefly. Generous margins:
    /// CI slowness must not flake this; failure means the worker never
    /// delivered, not that it was merely slow.
    fn wait_for(deadline: Duration, cond: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn spawn_rejects_invalid_queue_cap() {
        let bad_zero: Result<PanelWorker<String>, _> =
            PanelWorker::try_spawn("bad", None, 0, Duration::from_millis(50), String::new);
        assert!(matches!(bad_zero, Err(RuntimeError::InvalidQueueCapacity)));
        let bad_huge: Result<PanelWorker<String>, _> = PanelWorker::try_spawn(
            "bad",
            None,
            PANEL_WORKER_MAX_QUEUE_CAP + 1,
            Duration::from_millis(50),
            String::new,
        );
        assert!(matches!(bad_huge, Err(RuntimeError::InvalidQueueCapacity)));
    }

    #[test]
    fn snapshot_freshness_tick_reads_latest_only() {
        let counter = Arc::new(AtomicU64::new(0));
        let probe_counter = Arc::clone(&counter);
        let mut worker = PanelWorker::try_spawn(
            "freshness",
            None,
            PANEL_WORKER_DEFAULT_QUEUE_CAP,
            Duration::from_secs(60),
            move || probe_counter.fetch_add(1, Ordering::Relaxed) + 1,
        )
        .expect("valid worker config");
        // No probe has completed yet: no data, not a stall.
        assert_eq!(worker.latest(), None);
        assert_eq!(worker.generation(), 0);

        worker.request_refresh();
        assert!(
            wait_for(Duration::from_secs(5), || worker.latest().is_some()),
            "worker must deliver first snapshot"
        );
        let first = worker.latest().expect("snapshot present");
        let gen_first = worker.generation();
        assert!(first >= 1);
        assert!(gen_first >= 1);

        // A second refresh advances freshness monotonically.
        worker.request_refresh();
        assert!(
            wait_for(Duration::from_secs(5), || worker.generation() > gen_first),
            "generation must advance after refresh"
        );
        let second = worker.latest().expect("snapshot present");
        assert!(second > first, "probe reran and published newer value");
        worker.shutdown();
    }

    #[test]
    fn full_queue_sheds_with_counter_and_stays_bounded() {
        // Slow probe keeps the worker occupied so the bounded queue fills.
        let mut worker =
            PanelWorker::try_spawn("shed", Some(0_u64), 1, Duration::from_secs(60), || {
                std::thread::sleep(Duration::from_millis(400));
                1_u64
            })
            .expect("valid worker config");
        worker.request_refresh();
        // Generous settle so the worker is parked inside the slow probe and
        // cannot drain the queue while we flood it.
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..32 {
            worker.request_refresh();
        }
        assert!(
            worker.dropped() > 0,
            "full queue must shed load with a counted drop"
        );
        assert!(
            worker.pending() <= worker.queue_cap() + 1,
            "queue must stay bounded (pending={}, cap={})",
            worker.pending(),
            worker.queue_cap()
        );
        worker.shutdown();
    }

    #[test]
    fn tick_never_blocks_on_slow_probe() {
        let mut worker = PanelWorker::try_spawn(
            "no-block",
            Some("last-known-good".to_string()),
            PANEL_WORKER_DEFAULT_QUEUE_CAP,
            Duration::from_secs(60),
            || {
                std::thread::sleep(Duration::from_millis(800));
                "fresh".to_string()
            },
        )
        .expect("valid worker config");
        worker.request_refresh();
        // Let the worker enter the slow probe (generous settle).
        std::thread::sleep(Duration::from_millis(100));
        // Tick reads must stay fast and serve last-known-good while the probe
        // is still running: generous 300 ms bound vs the 800 ms probe.
        for _ in 0..5 {
            let start = Instant::now();
            let snapshot = worker.latest();
            let elapsed = start.elapsed();
            assert_eq!(snapshot.as_deref(), Some("last-known-good"));
            assert!(
                elapsed < Duration::from_millis(300),
                "tick read stalled on probe: {elapsed:?}"
            );
        }
        worker.shutdown();
    }

    #[test]
    fn teardown_joins_worker_promptly() {
        let mut worker = PanelWorker::try_spawn(
            "teardown",
            None,
            PANEL_WORKER_DEFAULT_QUEUE_CAP,
            Duration::from_millis(10),
            || 7_u64,
        )
        .expect("valid worker config");
        worker.request_refresh();
        assert!(
            wait_for(Duration::from_secs(5), || worker.completed() >= 1),
            "worker must complete a probe before teardown"
        );
        let start = Instant::now();
        worker.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown join must be prompt"
        );
        assert!(!worker.is_alive(), "thread must be joined after shutdown");
        // Idempotent: second shutdown is a no-op, never panics.
        worker.shutdown();
    }

    #[test]
    fn drop_joins_worker_without_hang() {
        let start = Instant::now();
        {
            let _worker = PanelWorker::try_spawn(
                "drop-join",
                None,
                PANEL_WORKER_DEFAULT_QUEUE_CAP,
                Duration::from_millis(10),
                || 1_u64,
            )
            .expect("valid worker config");
            // Dropped without explicit shutdown: Drop must join, not leak.
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Drop join must not hang teardown"
        );
    }
}
