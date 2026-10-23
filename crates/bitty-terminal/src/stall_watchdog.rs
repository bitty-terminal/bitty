//! Bounded main-loop stall watchdog (issue #2483).
//!
//! Long-running sessions can stop answering while the window stays mapped:
//! the main (winit) thread is parked inside one phase, no further events are
//! consumed, and the compositor reports `Application Not Responding`. The
//! owner's report left no process behind, so this module exists to make the
//! next occurrence self-describing.
//!
//! Design constraints:
//!
//! - **Observability only.** The watchdog never changes routing, timing, or
//!   state. It marks phases and, on a stall, writes a bounded report.
//! - **Opt-in.** `BITTY_STALL_WATCHDOG` is the required enable switch for the
//!   monitor thread; CI, headless smoke runs, and normal sessions stay quiet
//!   by default. A threshold variable never enables the watchdog on its own.
//! - **No hot-path work.** A phase mark is one relaxed atomic store plus a
//!   timestamp. The app publishes its bounded context sample at most twice a
//!   second, and the monitor thread touches `/proc` only after a stall.
//! - **Bounded.** A fixed thread-row cap, a fixed report-byte cap, and a
//!   report count/interval limit per stall; repeated stalls cannot grow the
//!   log without bound. The latest stall report also lands in one truncated
//!   XDG state slot (`bitty/stall-watchdog.log`), so a terminated session
//!   still leaves evidence behind. The state slot is persisted *before* the
//!   stderr write; a closed stderr pipe must never take the evidence with it.
//! - **Shutdown-clean.** Dropping the handle wakes and joins the monitor, so
//!   no detached thread survives app shutdown and no report is written once
//!   shutdown completed.
//! - **No host paths.** Reports name phases, ids, counts, thread states, and
//!   kernel wait channels; never a checkout, home, or config path.
//!
//! Configuration:
//!
//! - `BITTY_STALL_WATCHDOG=1` (or `true`/`on`) enables the watchdog with the
//!   default 2000 ms threshold.
//! - `BITTY_STALL_WATCHDOG=<milliseconds>` enables it with that threshold.
//! - `BITTY_STALL_WATCHDOG_MS=<milliseconds>` overrides the threshold
//!   (clamped to `50..=600000`) but does not enable the watchdog by itself.
//! - `0`/`false`/`off` leaves it disabled; invalid values stay disabled.
//!
//! The idle state is explicit: after the event loop arms `ControlFlow::Wait`
//! the app calls [`StallWatchdog::idle`], so a quiet window is never reported
//! as a stall. The monitor only fires while a phase is active and the
//! heartbeat has not advanced past the threshold.

use std::fmt::Write as _;
use std::path::PathBuf;
#[cfg(not(test))]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::logging;

/// Environment switch for the watchdog.
pub(crate) const WATCHDOG_ENV: &str = "BITTY_STALL_WATCHDOG";
/// Environment override for the stall threshold in milliseconds.
pub(crate) const WATCHDOG_THRESHOLD_ENV: &str = "BITTY_STALL_WATCHDOG_MS";

/// Default no-progress threshold (#2446 asks for a >2 s stall report).
pub(crate) const DEFAULT_STALL_THRESHOLD: Duration = Duration::from_secs(2);
/// Smallest accepted threshold; below this normal cold ticks look like stalls.
const MIN_STALL_THRESHOLD: Duration = Duration::from_millis(50);
/// Largest accepted threshold; keeps the monitor useful after long idles.
const MAX_STALL_THRESHOLD: Duration = Duration::from_secs(600);
/// Monitor sampling period at the default threshold.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Minimum sampling period; small thresholds still get several samples.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Minimum time between repeated reports for one continuous stall.
const REPORT_REPEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Minimum time between app-context samples pushed by the main thread.
///
/// The sampler allocates the bounded plugin/band id lists, so it must not run
/// on every phase mark; a context at most this stale still names the right
/// window when a stall is reported.
const CONTEXT_REFRESH_NANOS: u64 = 500_000_000;
/// Single-slot state file name under the XDG state directory.
const STATE_FILE_NAME: &str = "stall-watchdog.log";
/// Prefix of a stall report line; only these overwrite the state slot.
const STALL_REPORT_PREFIX: &str = "bitty: main-loop stall detected";
/// Maximum reports for one continuous stall (bounded log growth).
const MAX_REPORTS_PER_STALL: u32 = 5;
/// Maximum thread rows included in one report.
///
/// Linux-only: the sampler reads `/proc/self/task`; other targets render an
/// empty thread list, so the row cap has no use there.
#[cfg(target_os = "linux")]
const MAX_THREAD_ROWS: usize = 64;
/// Maximum bytes in one report (hard cap after rendering).
const MAX_REPORT_BYTES: usize = 8 * 1024;
/// Deadline for one report sink, including partial pipe writes.
const REPORT_WRITE_TIMEOUT: Duration = Duration::from_millis(25);
/// Deadline for the isolated state-file writer.
#[cfg(not(test))]
const STATE_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// Private argv used by the bounded state-file helper.
pub(crate) const STATE_WRITER_ARG: &str = "--internal-stall-watchdog-state";
/// Maximum identifier characters rendered per plugin id.
const MAX_IDENTIFIER_CHARS: usize = 48;
/// Maximum plugin ids rendered per list.
const MAX_IDS: usize = 8;

/// Coarse main-loop phase, stored as one byte on the hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Phase {
    /// Application construction before the first event.
    Startup = 0,
    /// Event loop parked in `ControlFlow::Wait`.
    Idle = 1,
    /// Between event stages (the next stage sets itself).
    Event = 2,
    /// Window creation, IME opt-in, and GPU attach.
    WindowCreate = 3,
    /// Draining the bounded PTY pump.
    PtyPump = 4,
    /// Reaping exited shells and closing panes.
    Reap = 5,
    /// Chrome-key intercept and overlay capture routing.
    ChromeIntercept = 6,
    /// Routing the event into `Runtime::handle_platform_event`.
    RouteEvent = 7,
    /// Draining queued `bitty ctl` control actions.
    CtlDrain = 8,
    /// Polling the live config file.
    ConfigPoll = 9,
    /// Applying queued plugin workspace/pin/fx/theme/submit requests.
    PluginWorkspace = 10,
    /// Firing due plugin timers (bounded, but a callback can still hang).
    PluginTimers = 11,
    /// Projecting plugin UI blocks into chrome bands.
    ChromeBands = 12,
    /// Draining plugin notifications.
    PluginNotifications = 13,
    /// Overlay, targeting, and capture bookkeeping.
    Overlays = 14,
    /// Applying cold-title/exit events.
    ColdEvents = 15,
    /// `Runtime::tick` (compose and present the frame).
    RuntimeTick = 16,
    /// Publishing live plugin snapshots.
    SnapshotPublish = 17,
    /// Sampling scrollback into the transcript ring.
    HistoryCapture = 18,
    /// IME/cursor OS synchronization.
    CursorSync = 19,
    /// Post-present PTY reply flush.
    ReplyFlush = 20,
    /// Delivering coalesced runtime events to plugin VMs.
    RuntimeEvents = 21,
    /// Cold plugin command pump (`AboutToWait` only).
    ColdCommands = 22,
    /// Save/session shutdown.
    Shutdown = 23,
    /// Unknown phase (defensive fallback).
    Unknown = 24,
}

impl Phase {
    /// Decode a stored phase byte.
    fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Startup,
            1 => Self::Idle,
            2 => Self::Event,
            3 => Self::WindowCreate,
            4 => Self::PtyPump,
            5 => Self::Reap,
            6 => Self::ChromeIntercept,
            7 => Self::RouteEvent,
            8 => Self::CtlDrain,
            9 => Self::ConfigPoll,
            10 => Self::PluginWorkspace,
            11 => Self::PluginTimers,
            12 => Self::ChromeBands,
            13 => Self::PluginNotifications,
            14 => Self::Overlays,
            15 => Self::ColdEvents,
            16 => Self::RuntimeTick,
            17 => Self::SnapshotPublish,
            18 => Self::HistoryCapture,
            19 => Self::CursorSync,
            20 => Self::ReplyFlush,
            21 => Self::RuntimeEvents,
            22 => Self::ColdCommands,
            23 => Self::Shutdown,
            _ => Self::Unknown,
        }
    }

    /// Stable lowercase name used in reports.
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Idle => "idle",
            Self::Event => "event",
            Self::WindowCreate => "window-create",
            Self::PtyPump => "pty-pump",
            Self::Reap => "reap",
            Self::ChromeIntercept => "chrome-intercept",
            Self::RouteEvent => "route-event",
            Self::CtlDrain => "ctl-drain",
            Self::ConfigPoll => "config-poll",
            Self::PluginWorkspace => "plugin-workspace",
            Self::PluginTimers => "plugin-timers",
            Self::ChromeBands => "chrome-bands",
            Self::PluginNotifications => "plugin-notifications",
            Self::Overlays => "overlays",
            Self::ColdEvents => "cold-events",
            Self::RuntimeTick => "runtime-tick",
            Self::SnapshotPublish => "snapshot-publish",
            Self::HistoryCapture => "history-capture",
            Self::CursorSync => "cursor-sync",
            Self::ReplyFlush => "reply-flush",
            Self::RuntimeEvents => "runtime-events",
            Self::ColdCommands => "cold-commands",
            Self::Shutdown => "shutdown",
            Self::Unknown => "unknown",
        }
    }
}

/// Identifiers sampled from the app on the cold path for a stall report.
///
/// All values are plain data; no runtime/plugin handle or host path is
/// retained. The list fields are pre-bounded before they reach the report.
#[derive(Clone, Debug, Default)]
pub(crate) struct WatchdogContext {
    /// Focused view id, when a leaf exists.
    pub(crate) focused_view: Option<u64>,
    /// Live leaf count.
    pub(crate) leaf_count: usize,
    /// Live split-pane session count.
    pub(crate) pane_count: usize,
    /// Focused pane's retained scrollback lines.
    pub(crate) scrollback_lines: usize,
    /// Discovered plugin count.
    pub(crate) plugin_count: usize,
    /// Discovered plugin ids (bounded by the sampler).
    pub(crate) plugin_ids: Vec<String>,
    /// Mounted chrome bands across all edges.
    pub(crate) band_count: usize,
    /// Plugin ids that own a mounted band (bounded by the sampler).
    pub(crate) band_plugin_ids: Vec<String>,
    /// Presented frames at sample time.
    pub(crate) presented_frames: u64,
}

/// Parsed watchdog configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchdogConfig {
    threshold: Duration,
}

impl WatchdogConfig {
    /// Default opt-in configuration (2 s threshold).
    const fn default_threshold() -> Self {
        Self {
            threshold: DEFAULT_STALL_THRESHOLD,
        }
    }

    /// Clamp a candidate threshold into the accepted range.
    fn with_threshold_ms(milliseconds: u64) -> Self {
        let candidate = Duration::from_millis(milliseconds);
        Self {
            threshold: candidate.clamp(MIN_STALL_THRESHOLD, MAX_STALL_THRESHOLD),
        }
    }
}

/// Parse the opt-in switch and optional threshold.
///
/// Pure so the environment contract is testable without touching the process
/// environment. Returns `None` for an absent switch, an explicit off value,
/// or an unparseable value (fail-quiet: diagnostics never surprise CI).
pub(crate) fn config_from_values(
    enable: Option<&str>,
    threshold: Option<&str>,
) -> Option<WatchdogConfig> {
    let enabled = match enable.map(str::trim) {
        None | Some("") => false,
        Some(value)
            if value.eq_ignore_ascii_case("0")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("no") =>
        {
            false
        }
        Some(value)
            if value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("yes") =>
        {
            true
        }
        Some(value) => value.parse::<u64>().is_ok(),
    };
    if !enabled {
        return None;
    }
    let threshold_ms = threshold
        .and_then(|value| value.trim().parse::<u64>().ok())
        .or_else(|| {
            // `1` is the boolean opt-in, not a one-millisecond threshold;
            // larger numeric values enable with that many milliseconds.
            enable
                .map(str::trim)
                .filter(|value| *value != "1")
                .and_then(|value| value.parse::<u64>().ok())
        });
    Some(match threshold_ms {
        Some(milliseconds) => WatchdogConfig::with_threshold_ms(milliseconds),
        None => WatchdogConfig::default_threshold(),
    })
}

/// Read the opt-in configuration from the process environment.
fn config_from_env() -> Option<WatchdogConfig> {
    let enable = std::env::var(WATCHDOG_ENV).ok();
    let threshold = std::env::var(WATCHDOG_THRESHOLD_ENV).ok();
    config_from_values(enable.as_deref(), threshold.as_deref())
}

/// One sampled thread's kernel-visible state.
#[derive(Clone, Debug)]
struct ThreadRow {
    tid: u64,
    comm: String,
    state: char,
    wchan: String,
    syscall_number: String,
}

/// Process totals sampled alongside the thread rows.
#[derive(Clone, Debug, Default)]
struct ProcessTotals {
    thread_count: usize,
    fd_count: usize,
    rss_kib: u64,
}

/// One rendered stall report, built on the monitor thread only.
struct StallReport {
    threshold: Duration,
    idle: Duration,
    phase: Phase,
    app_uptime: Duration,
    progress: u64,
    context: Option<WatchdogContext>,
    threads: Vec<ThreadRow>,
    totals: ProcessTotals,
}

/// Monitor state machine, kept separate from the thread for deterministic
/// tests of the rate limiter.
#[derive(Debug, Default)]
struct MonitorState {
    last_progress: u64,
    stall_started: Option<Instant>,
    last_report: Option<Instant>,
    reports: u32,
    suppression_logged: bool,
}

/// What the monitor decided after one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorAction {
    /// Nothing to emit.
    None,
    /// Emit a stall report for `idle` of no progress.
    Report { idle: Duration },
    /// The loop made progress again after a reported stall.
    Recovered { stalled_for: Duration },
    /// The per-stall report cap was reached; emit one suppression line.
    Suppressed,
}

impl MonitorState {
    /// Evaluate one sample.
    fn evaluate(
        &mut self,
        now: Instant,
        progress: u64,
        active: bool,
        idle: Duration,
        threshold: Duration,
    ) -> MonitorAction {
        if progress != self.last_progress {
            self.last_progress = progress;
            let recovered = self
                .stall_started
                .take()
                .map(|started| now.saturating_duration_since(started));
            let had_report = self.reports > 0;
            self.reports = 0;
            self.last_report = None;
            self.suppression_logged = false;
            return match recovered {
                Some(stalled_for) if had_report => MonitorAction::Recovered { stalled_for },
                _ => MonitorAction::None,
            };
        }
        if !active {
            // Idle is expected: no stall, and a threshold crossing that ended
            // without a report leaves no recovery line.
            self.stall_started = None;
            self.reports = 0;
            self.last_report = None;
            self.suppression_logged = false;
            return MonitorAction::None;
        }
        if idle < threshold {
            return MonitorAction::None;
        }
        let _ = self.stall_started.get_or_insert(now);
        let due = match self.last_report {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= REPORT_REPEAT_INTERVAL,
        };
        if due && self.reports < MAX_REPORTS_PER_STALL {
            self.reports = self.reports.saturating_add(1);
            self.last_report = Some(now);
            return MonitorAction::Report { idle };
        }
        if !due {
            return MonitorAction::None;
        }
        if !self.suppression_logged {
            self.suppression_logged = true;
            return MonitorAction::Suppressed;
        }
        MonitorAction::None
    }
}

/// Shared state written by the main thread and sampled by the monitor.
struct Shared {
    start: Instant,
    last_beat_ns: AtomicU64,
    last_context_ns: AtomicU64,
    phase: AtomicU8,
    active: AtomicBool,
    progress: AtomicU64,
    context: Mutex<WatchdogContext>,
}

impl Shared {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_beat_ns: AtomicU64::new(0),
            last_context_ns: AtomicU64::new(0),
            phase: AtomicU8::new(Phase::Startup as u8),
            active: AtomicBool::new(true),
            progress: AtomicU64::new(0),
            context: Mutex::new(WatchdogContext::default()),
        }
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Whether the app may spend one sample publishing a fresh context.
    fn context_due(&self) -> bool {
        let last = self.last_context_ns.load(Ordering::Relaxed);
        last == 0 || self.elapsed_ns().saturating_sub(last) >= CONTEXT_REFRESH_NANOS
    }

    fn mark(&self, phase: Phase, active: bool) {
        self.phase.store(phase as u8, Ordering::Relaxed);
        self.active.store(active, Ordering::Relaxed);
        self.last_beat_ns
            .store(self.elapsed_ns(), Ordering::Relaxed);
        self.progress.fetch_add(1, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        Duration::from_nanos(
            self.elapsed_ns()
                .saturating_sub(self.last_beat_ns.load(Ordering::Relaxed)),
        )
    }
}

/// Monitor wake-up signal.
///
/// A plain sleep would make shutdown wait up to one poll interval; the
/// condition variable wakes the monitor as soon as the stop flag is set.
struct StopSignal {
    stopped: AtomicBool,
    waiter: Mutex<()>,
    wake: Condvar,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            waiter: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    fn request(&self) {
        // Hold the waiter lock so a monitor between the check and the wait
        // cannot miss the notification.
        let _guard = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Wait up to `timeout` for a stop request; `true` means stop now.
    fn wait(&self, timeout: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let guard = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (guard, _) = self
            .wake
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(guard);
        self.is_stopped()
    }
}

/// Opt-in main-loop watchdog handle.
///
/// `Drop` requests a stop and joins the monitor: shutdown leaves no detached
/// thread behind and no report is written once `Drop` returns. Sink writes
/// ignore errors, so a closed stderr pipe cannot panic the monitor or strand
/// the join. Owned production sinks have deadlines; injected test sinks retain
/// normal join semantics so no arbitrary closure is ever detached.
pub(crate) struct StallWatchdog {
    shared: Arc<Shared>,
    stop: Arc<StopSignal>,
    monitor: Option<JoinHandle<()>>,
    /// Set by the monitor thread right before it exits (test observability).
    #[cfg(test)]
    finished: Arc<AtomicBool>,
}

impl StallWatchdog {
    /// Start the watchdog when the environment opts in.
    pub(crate) fn from_env() -> Option<Self> {
        config_from_env().map(|config| {
            let mut sink = ReportSink {
                state_path: state_file_path(),
                stderr: BoundedStderr::new(std::io::stderr()),
            };
            Self::start(config, Box::new(move |line| sink.emit(line)))
        })
    }

    /// Start a watchdog with an explicit configuration and sink.
    fn start(config: WatchdogConfig, mut sink: Box<dyn FnMut(&str) + Send>) -> Self {
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(StopSignal::new());
        #[cfg(test)]
        let finished = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        #[cfg(test)]
        let thread_finished = Arc::clone(&finished);
        let threshold = config.threshold;
        let poll_interval = std::cmp::max(
            MIN_POLL_INTERVAL,
            std::cmp::min(DEFAULT_POLL_INTERVAL, threshold / 4),
        );
        let spawn = std::thread::Builder::new()
            .name(String::from("bitty-stall-watchdog"))
            .spawn(move || {
                monitor_loop(
                    &thread_shared,
                    &thread_stop,
                    threshold,
                    poll_interval,
                    &mut sink,
                );
                #[cfg(test)]
                thread_finished.store(true, Ordering::SeqCst);
            });
        let monitor = match spawn {
            Ok(handle) => Some(handle),
            Err(error) => {
                logging::warn(|| {
                    format!(
                        "bitty: stall watchdog thread unavailable ({error}); diagnostics stay off"
                    )
                });
                #[cfg(test)]
                finished.store(true, Ordering::SeqCst);
                None
            }
        };
        Self {
            shared,
            stop,
            monitor,
            #[cfg(test)]
            finished,
        }
    }

    /// Mark the loop active in `phase` and advance the heartbeat.
    pub(crate) fn enter(&self, phase: Phase) {
        self.shared.mark(phase, true);
    }

    /// Mark the loop idle (armed `ControlFlow::Wait`): not a stall.
    pub(crate) fn idle(&self) {
        self.shared.mark(Phase::Idle, false);
    }

    /// Whether the app may publish a fresh context sample now.
    ///
    /// The sampler builds two bounded id lists, so it runs at most once per
    /// [`CONTEXT_REFRESH_NANOS`]; a disabled watchdog never asks.
    pub(crate) fn context_due(&self) -> bool {
        self.shared.context_due()
    }

    /// Replace the sampled app identifiers shown in the next report.
    pub(crate) fn update_context(&self, context: WatchdogContext) {
        if let Ok(mut slot) = self.shared.context.lock() {
            *slot = context;
            self.shared
                .last_context_ns
                .store(self.shared.elapsed_ns(), Ordering::Relaxed);
        }
    }

    /// Request monitor shutdown and wait for the thread to exit.
    fn shutdown(&mut self) -> std::thread::Result<()> {
        self.stop.request();
        match self.monitor.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        }
    }

    /// Stop the monitor and assert the thread actually exited (tests).
    #[cfg(test)]
    fn stop_and_join(mut self) -> std::thread::Result<()> {
        self.shutdown()
    }
}

impl Drop for StallWatchdog {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Monitor thread body.
fn monitor_loop(
    shared: &Shared,
    stop: &StopSignal,
    threshold: Duration,
    poll_interval: Duration,
    sink: &mut (dyn FnMut(&str) + Send),
) {
    let mut state = MonitorState {
        last_progress: shared.progress.load(Ordering::Relaxed),
        ..MonitorState::default()
    };
    loop {
        if stop.wait(poll_interval) {
            return;
        }
        let now = Instant::now();
        let progress = shared.progress.load(Ordering::Relaxed);
        let phase = Phase::from_u8(shared.phase.load(Ordering::Relaxed));
        let active = shared.active.load(Ordering::Relaxed);
        let idle = shared.idle_for();
        match state.evaluate(now, progress, active, idle, threshold) {
            MonitorAction::None => {}
            MonitorAction::Report { idle } => {
                let context = shared.context.try_lock().ok().map(|slot| slot.clone());
                let totals_snapshot = sample_process_totals();
                let report = StallReport {
                    threshold,
                    idle,
                    phase,
                    app_uptime: now.saturating_duration_since(shared.start),
                    progress,
                    context,
                    threads: totals_snapshot.0,
                    totals: totals_snapshot.1,
                };
                let rendered = render_report(&report);
                // Sampling /proc and rendering takes time; never emit once
                // shutdown was requested.
                if stop.is_stopped() {
                    return;
                }
                sink(&rendered);
            }
            MonitorAction::Recovered { stalled_for } => {
                if stop.is_stopped() {
                    return;
                }
                sink(&format!(
                    "bitty: main-loop stall recovered after {} ms (phase={})",
                    stalled_for.as_millis(),
                    phase.as_str()
                ));
            }
            MonitorAction::Suppressed => {
                if stop.is_stopped() {
                    return;
                }
                sink(&format!(
                    "bitty: main-loop stall still active after {} reports; further reports suppressed",
                    MAX_REPORTS_PER_STALL
                ));
            }
        }
    }
}

/// Sample all threads and process totals (Linux); other targets return empty.
fn sample_process_totals() -> (Vec<ThreadRow>, ProcessTotals) {
    #[cfg(target_os = "linux")]
    {
        linux_thread_snapshot()
    }
    #[cfg(not(target_os = "linux"))]
    {
        (Vec::new(), ProcessTotals::default())
    }
}

/// Read `/proc/self/task/*` into bounded rows plus fd/RSS totals.
#[cfg(target_os = "linux")]
fn linux_thread_snapshot() -> (Vec<ThreadRow>, ProcessTotals) {
    let mut rows = Vec::new();
    let main_tid = u64::from(std::process::id());
    let mut totals = ProcessTotals::default();
    if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
        for entry in entries.flatten() {
            if rows.len() >= MAX_THREAD_ROWS {
                // Count the remainder without retaining rows.
                totals.thread_count = totals.thread_count.saturating_add(1);
                continue;
            }
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u64>().ok())
            else {
                continue;
            };
            totals.thread_count = totals.thread_count.saturating_add(1);
            let base = format!("/proc/self/task/{tid}");
            let comm = read_proc_text(&format!("{base}/comm"), 64)
                .map(|text| sanitize_identifier(&text, 32))
                .unwrap_or_default();
            let state = read_proc_text(&format!("{base}/stat"), 4096)
                .map(|text| stat_state(&text))
                .unwrap_or('?');
            let wchan = read_proc_text(&format!("{base}/wchan"), 128)
                .map(|text| sanitize_kernel_token(&text))
                .unwrap_or_else(|| String::from("?"));
            let syscall_number = read_proc_text(&format!("{base}/syscall"), 128)
                .map(|text| syscall_number(&text))
                .unwrap_or_else(|| String::from("?"));
            rows.push(ThreadRow {
                tid,
                comm,
                state,
                wchan,
                syscall_number,
            });
        }
    }
    rows.sort_by_key(|row| (row.tid != main_tid, row.tid));
    totals.fd_count = std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    totals.rss_kib = read_proc_text("/proc/self/status", 8192)
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("VmRSS:")?;
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .unwrap_or(0);
    (rows, totals)
}

/// Read a proc file, capping bytes before UTF-8 lossy conversion.
#[cfg(target_os = "linux")]
fn read_proc_text(path: &str, max_bytes: usize) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::with_capacity(max_bytes.min(4096));
    file.take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).trim().to_string())
}

/// Extract the state character from `/proc/<pid>/stat` (field 3).
#[cfg(target_os = "linux")]
fn stat_state(stat: &str) -> char {
    let Some(close) = stat.rfind(')') else {
        return '?';
    };
    stat.get(close + 1..)
        .and_then(|rest| rest.trim_start().chars().next())
        .unwrap_or('?')
}

/// Keep only the syscall number (never its argument registers/pointers).
#[cfg(target_os = "linux")]
fn syscall_number(raw: &str) -> String {
    let name = raw.split_whitespace().next().unwrap_or_default();
    if name.is_empty() || name == "running" || name.starts_with('-') {
        String::from("-")
    } else {
        sanitize_kernel_token(name)
    }
}

/// Sanitize a wchan/syscall token to printable, bounded text.
#[cfg(target_os = "linux")]
fn sanitize_kernel_token(raw: &str) -> String {
    let token = raw.trim();
    if token.is_empty() || token == "0" {
        return String::from("-");
    }
    sanitize_identifier(token, 64)
}

/// Bound and strip control characters from an identifier.
fn sanitize_identifier(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(raw.len()));
    for ch in raw.chars().take(max_chars) {
        out.push(if ch.is_control() { '?' } else { ch });
    }
    out
}

/// Render one bounded report.
fn render_report(report: &StallReport) -> String {
    let mut out = String::with_capacity(1024);
    let _ = writeln!(
        out,
        "bitty: main-loop stall detected: no progress for {} ms (threshold {} ms, phase={}, app_uptime {} ms, progress={})",
        report.idle.as_millis(),
        report.threshold.as_millis(),
        report.phase.as_str(),
        report.app_uptime.as_millis(),
        report.progress,
    );
    match &report.context {
        Some(context) => {
            let focus = context
                .focused_view
                .map_or_else(|| String::from("none"), |id| id.to_string());
            let _ = writeln!(
                out,
                "  focus=view-{focus} leaves={} panes={} scrollback={} lines presented={}",
                context.leaf_count,
                context.pane_count,
                context.scrollback_lines,
                context.presented_frames,
            );
            let _ = writeln!(
                out,
                "  plugins={} [{}] bands={} [{}]",
                context.plugin_count,
                format_ids(&context.plugin_ids),
                context.band_count,
                format_ids(&context.band_plugin_ids),
            );
        }
        None => {
            let _ = writeln!(
                out,
                "  app state unavailable (context lock held or poisoned)"
            );
        }
    }
    let _ = writeln!(
        out,
        "  process: threads={} fds={} rss={} KiB",
        report.totals.thread_count, report.totals.fd_count, report.totals.rss_kib,
    );
    if report.threads.is_empty() {
        let _ = writeln!(out, "  thread-state snapshot unavailable on this platform");
    } else {
        let main_tid = u64::from(std::process::id());
        for row in &report.threads {
            let marker = if row.tid == main_tid { "[main] " } else { "" };
            let _ = writeln!(
                out,
                "  {marker}tid={} state={} wchan={} syscall_number={} comm={}",
                row.tid, row.state, row.wchan, row.syscall_number, row.comm,
            );
        }
    }
    truncate_report(out)
}

/// Join at most [`MAX_IDS`] identifiers into one bounded display field.
fn format_ids(ids: &[String]) -> String {
    let mut out = String::new();
    for id in ids.iter().take(MAX_IDS) {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&sanitize_identifier(id, MAX_IDENTIFIER_CHARS));
    }
    if ids.len() > MAX_IDS {
        let _ = write!(out, ",+{}", ids.len().saturating_sub(MAX_IDS));
    }
    out
}

/// Hard-cap one report at a char boundary.
fn truncate_report(mut report: String) -> String {
    const SUFFIX: &str = "\n  ... report truncated";
    if report.len() <= MAX_REPORT_BYTES {
        return report;
    }
    let mut boundary = MAX_REPORT_BYTES.saturating_sub(SUFFIX.len());
    while boundary > 0 && !report.is_char_boundary(boundary) {
        boundary -= 1;
    }
    report.truncate(boundary);
    report.push_str(SUFFIX);
    report
}

/// Report sink used in production: one bounded state slot, then stderr.
///
/// The owner's report had no captured log because the graphical launcher's
/// stderr was lost and the process was terminated. Persisting the latest stall
/// report under the XDG state directory makes the next occurrence readable
/// after the fact; it is a single truncated slot, never an append log.
///
/// Ordering is deliberate: the state slot is written first, so a broken stderr
/// pipe cannot lose the evidence. The stderr write runs through the same
/// emission-time redaction gate as every other app diagnostic and drops every
/// write error instead of panicking the monitor thread.
struct ReportSink<W: std::io::Write> {
    state_path: Option<PathBuf>,
    stderr: W,
}

impl<W: std::io::Write> ReportSink<W> {
    fn emit(&mut self, line: &str) {
        let redacted = crate::observability::redact_text_for_stderr(line);
        persist_state_file_at(self.state_path.as_deref(), &redacted);
        // `writeln!` on stderr returns an error for EPIPE/EBADF instead of
        // panicking (`eprintln!` would panic and kill the monitor thread).
        let _ = writeln!(self.stderr, "{redacted}");
        let _ = self.stderr.flush();
    }
}

/// A duplicated, nonblocking stderr fd. The duplicate prevents changing the
/// process-global blocking mode while allowing poll to bound pipe backpressure.
#[cfg(unix)]
struct BoundedStderr {
    fd: std::os::fd::OwnedFd,
    deadline: Option<Instant>,
}

#[cfg(unix)]
impl BoundedStderr {
    fn new(stderr: std::io::Stderr) -> Self {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use nix::unistd::dup;
        let fd = dup(&stderr).expect("duplicate stderr for watchdog");
        let _ = fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
        Self { fd, deadline: None }
    }
}

#[cfg(unix)]
impl std::io::Write for BoundedStderr {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + REPORT_WRITE_TIMEOUT);
        let result = write_fd_with_deadline(&self.fd, bytes, deadline);
        if result.is_err() {
            self.deadline = None;
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.deadline = None;
        Ok(())
    }
}

#[cfg(unix)]
fn write_fd_with_deadline(
    fd: &impl std::os::fd::AsFd,
    bytes: &[u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::unistd::write;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
        if poll(
            &mut fds,
            PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX),
        )
        .map_err(std::io::Error::other)?
            == 0
        {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match write(fd, bytes) {
            Ok(count) => return Ok(count),
            Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => continue,
            Err(error) => return Err(std::io::Error::other(error)),
        }
    }
}

#[cfg(not(unix))]
type BoundedStderr = std::io::Stderr;

#[cfg(not(unix))]
impl BoundedStderr {
    fn new(stderr: std::io::Stderr) -> Self {
        stderr
    }
}

/// Persist a stall report to `path` before it reaches stderr.
fn persist_state_file_at(path: Option<&std::path::Path>, line: &str) {
    if !line.starts_with(STALL_REPORT_PREFIX) {
        return;
    }
    let Some(path) = path else {
        return;
    };
    write_state_slot(path, line);
}

/// Write (truncate) the single state slot, best-effort.
fn write_state_slot(path: &std::path::Path, line: &str) {
    #[cfg(test)]
    {
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let _ = std::fs::write(path, line);
    }
    #[cfg(not(test))]
    {
        use std::io::Write as _;
        let Ok(mut child) = Command::new(std::env::current_exe().unwrap_or_default())
            .arg(STATE_WRITER_ARG)
            .env("BITTY_WATCHDOG_STATE_PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let write_result = stdin.write_all(line.as_bytes());
        drop(stdin);
        if write_result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        let deadline = Instant::now() + STATE_WRITE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => std::thread::yield_now(),
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

/// Private state writer invoked before normal CLI parsing.
pub(crate) fn handle_state_writer(raw: &[String]) -> bool {
    use std::io::Read as _;
    if raw.get(1).map(String::as_str) != Some(STATE_WRITER_ARG) {
        return false;
    }
    let Some(path) = std::env::var_os("BITTY_WATCHDOG_STATE_PATH") else {
        std::process::exit(1);
    };
    let Some(parent) = std::path::Path::new(&path).parent() else {
        std::process::exit(1);
    };
    let mut line = String::new();
    if std::io::stdin().read_to_string(&mut line).is_err()
        || std::fs::create_dir_all(parent)
            .and_then(|_| std::fs::write(&path, line))
            .is_err()
    {
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// Resolve the single-slot state file path from XDG state conventions.
fn state_file_path() -> Option<PathBuf> {
    state_file_path_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure path resolution for [`state_file_path`].
fn state_file_path_from(
    xdg_state: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let base = xdg_state
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local").join("state")))?;
    Some(base.join("bitty").join(STATE_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn config_is_opt_in_and_threshold_is_clamped() {
        assert_eq!(config_from_values(None, None), None);
        assert_eq!(config_from_values(Some("0"), None), None);
        assert_eq!(config_from_values(Some("off"), None), None);
        assert_eq!(config_from_values(Some("nonsense"), None), None);
        assert_eq!(
            config_from_values(Some("1"), None),
            Some(WatchdogConfig::default_threshold())
        );
        assert_eq!(
            config_from_values(Some("750"), None),
            Some(WatchdogConfig::with_threshold_ms(750))
        );
        assert_eq!(
            config_from_values(Some("1"), Some("250")),
            Some(WatchdogConfig::with_threshold_ms(250))
        );
        assert_eq!(
            config_from_values(Some("1"), Some("1")).map(|c| c.threshold),
            Some(MIN_STALL_THRESHOLD)
        );
        assert_eq!(
            config_from_values(Some("1"), Some("9999999")).map(|c| c.threshold),
            Some(MAX_STALL_THRESHOLD)
        );
    }

    #[test]
    fn phase_round_trips_every_byte() {
        for raw in 0u8..=24 {
            let phase = Phase::from_u8(raw);
            assert_eq!(phase as u8, raw);
            assert!(!phase.as_str().is_empty());
        }
        assert_eq!(Phase::from_u8(u8::MAX), Phase::Unknown);
    }

    #[test]
    fn monitor_ignores_idle_and_sub_threshold_samples() {
        let mut state = MonitorState::default();
        let now = Instant::now();
        assert_eq!(
            state.evaluate(
                now,
                1,
                false,
                Duration::from_secs(60),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::None
        );
        assert_eq!(
            state.evaluate(
                now,
                1,
                true,
                Duration::from_millis(1999),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::None
        );
    }

    #[test]
    fn monitor_reports_then_rate_limits_and_recovers() {
        let mut state = MonitorState {
            last_progress: 7,
            ..MonitorState::default()
        };
        let start = Instant::now();
        assert_eq!(
            state.evaluate(
                start,
                7,
                true,
                Duration::from_millis(2000),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::Report {
                idle: Duration::from_millis(2000)
            }
        );
        // Within the repeat interval: silent.
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(5),
                7,
                true,
                Duration::from_secs(7),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::None
        );
        // At the repeat interval: report again.
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(30),
                7,
                true,
                Duration::from_secs(32),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::Report {
                idle: Duration::from_secs(32)
            }
        );
        // Progress after a report emits one recovery line.
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(31),
                8,
                true,
                Duration::from_millis(0),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::Recovered {
                stalled_for: Duration::from_secs(31)
            }
        );
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(32),
                8,
                true,
                Duration::from_millis(0),
                DEFAULT_STALL_THRESHOLD
            ),
            MonitorAction::None
        );
    }

    #[test]
    fn monitor_caps_reports_per_stall() {
        let mut state = MonitorState {
            last_progress: 9,
            ..MonitorState::default()
        };
        let start = Instant::now();
        let mut reports = 0;
        let mut suppressed = 0;
        for step in 0..20 {
            let now = start + Duration::from_secs(30 * step);
            match state.evaluate(
                now,
                9,
                true,
                Duration::from_secs(3600),
                DEFAULT_STALL_THRESHOLD,
            ) {
                MonitorAction::Report { .. } => reports += 1,
                MonitorAction::Suppressed => suppressed += 1,
                _ => {}
            }
        }
        assert_eq!(reports, MAX_REPORTS_PER_STALL);
        assert_eq!(suppressed, 1);
    }

    #[test]
    fn identifier_and_report_bounds_hold() {
        assert_eq!(sanitize_identifier("a\nb\tc", 16), "a?b?c");
        let long = "x".repeat(1000);
        assert_eq!(sanitize_identifier(&long, 8).chars().count(), 8);
        let ids: Vec<String> = (0..20).map(|index| format!("plugin-{index}")).collect();
        let rendered = format_ids(&ids);
        assert!(rendered.contains("+12"), "{rendered}");
        assert!(rendered.len() < 1024, "{rendered}");
    }

    #[test]
    fn context_samples_are_throttled() {
        let shared = Shared::new();
        assert!(shared.context_due());
        shared
            .last_context_ns
            .store(shared.elapsed_ns(), Ordering::Relaxed);
        assert!(!shared.context_due());
        shared.last_context_ns.store(
            shared.elapsed_ns().saturating_sub(CONTEXT_REFRESH_NANOS),
            Ordering::Relaxed,
        );
        assert!(shared.context_due());
    }

    #[test]
    fn state_slot_is_a_single_bounded_file() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-watchdog-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join(STATE_FILE_NAME);
        write_state_slot(&path, "first");
        write_state_slot(&path, "second");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_path_follows_xdg_then_home() {
        use std::ffi::OsStr;
        assert_eq!(
            state_file_path_from(Some(OsStr::new("/example/state")), None),
            Some(PathBuf::from("/example/state/bitty/stall-watchdog.log"))
        );
        assert_eq!(
            state_file_path_from(None, Some(OsStr::new("/example/home"))),
            Some(PathBuf::from(
                "/example/home/.local/state/bitty/stall-watchdog.log"
            ))
        );
        assert_eq!(state_file_path_from(None, None), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_state_handles_parentheses_in_comm() {
        assert_eq!(stat_state("123 (a (b) c) S 456 789"), 'S');
        assert_eq!(stat_state("123 (x) R 1"), 'R');
        assert_eq!(stat_state("not a stat line"), '?');
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn syscall_number_never_keeps_arguments() {
        assert_eq!(syscall_number("202 0x7f 0x81 0 0"), "202");
        assert_eq!(syscall_number("running"), "-");
        assert_eq!(syscall_number(""), "-");
        assert_eq!(syscall_number("-1 0 0 0"), "-");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rendered_report_is_bounded_and_marks_main_thread() {
        let (threads, totals) = linux_thread_snapshot();
        let mut rows = threads;
        for index in 0..500 {
            rows.push(ThreadRow {
                tid: 10_000 + index,
                comm: "worker".to_string(),
                state: 'S',
                wchan: "futex_wait_queue".to_string(),
                syscall_number: "202".to_string(),
            });
        }
        rows.truncate(MAX_THREAD_ROWS);
        let report = StallReport {
            threshold: DEFAULT_STALL_THRESHOLD,
            idle: Duration::from_secs(3),
            phase: Phase::RuntimeTick,
            app_uptime: Duration::from_secs(7200),
            progress: 42,
            context: Some(WatchdogContext {
                focused_view: Some(3),
                leaf_count: 2,
                pane_count: 2,
                scrollback_lines: 10_000,
                plugin_count: 1,
                plugin_ids: vec![String::from("bitty-terminal.bar")],
                band_count: 1,
                band_plugin_ids: vec![String::from("bitty-terminal.bar")],
                presented_frames: 41,
            }),
            threads: rows,
            totals,
        };
        let rendered = render_report(&report);
        assert!(rendered.contains("phase=runtime-tick"), "{rendered}");
        assert!(rendered.contains("[main]"), "{rendered}");
        assert!(rendered.len() <= MAX_REPORT_BYTES, "{}", rendered.len());
        assert!(!rendered.contains("/home/"), "{rendered}");
    }

    #[test]
    fn watchdog_thread_reports_a_stalled_heartbeat() {
        let (tx, rx) = mpsc::channel::<String>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(40),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
            }),
        );
        watchdog.enter(Phase::PluginTimers);
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("stall report must arrive");
        assert!(line.contains("stall detected"), "{line}");
        assert!(line.contains("phase=plugin-timers"), "{line}");
        watchdog.idle();
        let recovered = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recovery line must arrive");
        assert!(recovered.contains("stall recovered"), "{recovered}");
        watchdog
            .stop_and_join()
            .expect("monitor thread must exit cleanly");
    }

    /// Deterministic temp directory for one hermetic monitor test.
    fn test_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-watchdog-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Stderr stand-in that fails exactly like a closed pipe.
    struct BrokenStderr {
        state_path: PathBuf,
        saw_state_before_write: Arc<AtomicBool>,
    }

    impl std::io::Write for BrokenStderr {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            // Record whether the state evidence already existed when this
            // write arrived; that is the ordering the fix guarantees.
            if self.state_path.exists() {
                self.saw_state_before_write.store(true, Ordering::SeqCst);
            }
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn report_sink_persists_state_before_a_broken_stderr_write() {
        let dir = test_dir("sink-order");
        let path = dir.join(STATE_FILE_NAME);
        let saw_state = Arc::new(AtomicBool::new(false));
        let mut sink = ReportSink {
            state_path: Some(path.clone()),
            stderr: BrokenStderr {
                state_path: path.clone(),
                saw_state_before_write: Arc::clone(&saw_state),
            },
        };
        let line = concat!(
            "bitty: main-loop stall detected: no progress for 2000 ms ",
            "(threshold 2000 ms, phase=plugin-timers, app_uptime 10 ms, progress=3)\n",
            "  thread-state snapshot\n"
        );
        sink.emit(line);

        assert!(
            saw_state.load(Ordering::SeqCst),
            "the state slot must exist before stderr is written"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("state slot"), line);
        // Recovery lines share the sink but never overwrite the evidence.
        sink.emit("bitty: main-loop stall recovered after 2000 ms (phase=idle)");
        assert_eq!(std::fs::read_to_string(&path).expect("state slot"), line);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn monitor_soak_with_broken_stderr_keeps_phase_and_thread_evidence() {
        let dir = test_dir("soak");
        let state_path = dir.join(STATE_FILE_NAME);
        let writes = Arc::new(AtomicU64::new(0));
        let saw_state = Arc::new(AtomicBool::new(false));
        let sink_path = state_path.clone();
        let sink_writes = Arc::clone(&writes);
        let sink_saw_state = Arc::clone(&saw_state);
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(45),
            Box::new(move |line| {
                let mut sink = ReportSink {
                    state_path: Some(sink_path.clone()),
                    stderr: BrokenStderr {
                        state_path: sink_path.clone(),
                        saw_state_before_write: Arc::clone(&sink_saw_state),
                    },
                };
                sink.emit(line);
                sink_writes.fetch_add(1, Ordering::SeqCst);
            }),
        );
        watchdog.enter(Phase::PluginTimers);

        let deadline = Instant::now() + Duration::from_secs(10);
        let evidence = loop {
            if let Ok(text) = std::fs::read_to_string(&state_path) {
                if text.contains("phase=plugin-timers") {
                    break text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "monitor never persisted a stall report"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        assert!(evidence.starts_with(STALL_REPORT_PREFIX), "{evidence}");
        assert!(evidence.contains("phase=plugin-timers"), "{evidence}");
        assert!(evidence.contains("threads="), "{evidence}");
        #[cfg(target_os = "linux")]
        {
            assert!(evidence.contains("tid="), "{evidence}");
            assert!(evidence.contains("[main]"), "{evidence}");
            assert!(evidence.contains("state="), "{evidence}");
        }
        assert!(
            saw_state.load(Ordering::SeqCst),
            "evidence must be on disk before the failing stderr write"
        );

        // The broken stderr must not panic the monitor: join succeeds and the
        // thread records its own exit.
        let finished = Arc::clone(&watchdog.finished);
        let writes_deadline = Instant::now() + Duration::from_secs(5);
        while writes.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < writes_deadline,
                "sink never observed the persisted report"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let writes_at_stop = writes.load(Ordering::SeqCst);
        watchdog
            .stop_and_join()
            .expect("broken stderr must not panic the monitor");
        assert!(finished.load(Ordering::SeqCst), "monitor must exit");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            writes.load(Ordering::SeqCst),
            writes_at_stop,
            "no report may be written after shutdown"
        );
        assert_eq!(
            std::fs::read_to_string(&state_path).expect("state slot"),
            evidence,
            "state evidence must survive shutdown unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_joins_the_monitor_and_stops_writing() {
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (drop_started_tx, drop_started_rx) = mpsc::channel::<()>();
        let (drop_done_tx, drop_done_rx) = mpsc::channel::<()>();
        let writes = Arc::new(AtomicU64::new(0));
        let sink_writes = Arc::clone(&writes);
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(30),
            Box::new(move |_line| {
                let _ = entered_tx.send(());
                // Hold the monitor inside the sink until the test releases it.
                let _ = release_rx.recv();
                sink_writes.fetch_add(1, Ordering::SeqCst);
            }),
        );
        watchdog.enter(Phase::RuntimeTick);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first stall report must enter the sink");

        let finished = Arc::clone(&watchdog.finished);
        let dropper = std::thread::spawn(move || {
            let _ = drop_started_tx.send(());
            drop(watchdog);
            let _ = drop_done_tx.send(());
        });
        drop_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("drop thread must start");
        // The sink is still blocked. A detached monitor returns from Drop
        // immediately; a joining Drop must stay blocked here.
        assert!(
            drop_done_rx
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "Drop must join the monitor thread, not detach it"
        );
        release_tx.send(()).expect("release the sink");
        drop_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Drop must return after the monitor exits");
        dropper.join().expect("drop thread");
        assert!(finished.load(Ordering::SeqCst), "monitor must exit");

        let writes_at_drop = writes.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            writes.load(Ordering::SeqCst),
            writes_at_drop,
            "a stopped monitor must not write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saturated_os_pipe_report_write_is_deadline_bounded() {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use nix::unistd::{pipe, write};

        let (_reader, writer) = pipe().expect("OS pipe");
        fcntl(&writer, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("nonblocking pipe");
        let fill = [0_u8; 4096];
        loop {
            match write(&writer, &fill) {
                Ok(_) => {}
                Err(nix::errno::Errno::EAGAIN) => break,
                Err(error) => panic!("fill pipe: {error}"),
            }
        }
        let started = Instant::now();
        let result =
            write_fd_with_deadline(&writer, b"stall report\n", started + REPORT_WRITE_TIMEOUT);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() <= REPORT_WRITE_TIMEOUT + Duration::from_millis(100),
            "pipe report write exceeded deadline"
        );
    }
}
