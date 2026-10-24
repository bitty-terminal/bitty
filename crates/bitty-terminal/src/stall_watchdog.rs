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
//!   log without bound. The latest stall report atomically replaces one XDG
//!   state slot (`bitty/stall-watchdog.log`) inside a disposable helper
//!   process that also owns the stderr copy: the previous slot stays intact
//!   unless a complete new report is renamed in, and a blocked stderr pipe is
//!   bounded by killing the helper, never by changing the application's
//!   descriptor flags.
//! - **Observable delivery.** Every delivery returns a [`DeliveryOutcome`].
//!   The sink mirrors a bounded ledger (`attempts`, `failures`, last outcome,
//!   up to four recent failures) to a companion slot
//!   (`bitty/stall-watchdog-delivery.log`), and the next stall report carries
//!   the same summary in-band before the thread table. `SpawnFailed`,
//!   `TimedOut`, `HelperFailed`, and `EvidenceFailed` therefore stay visible
//!   even when the report itself could not be stored and stderr is gone. An
//!   evidence-path failure still retries through a no-slot helper so stderr
//!   receives the report; both attempts are recorded.
//! - **Generation-safe evidence.** Each delivery gets a monotonic generation
//!   from a locked sequence file and stages its bytes in a temp sibling named
//!   after that generation (never a shared `.tmp`). The helper re-checks the
//!   generation under the same lock immediately before renaming, and the
//!   supervisor removes a timed-out attempt's temp; a late helper can
//!   therefore never replace newer evidence. The lock is `flock` on Unix and a
//!   bounded token-checked gate on Windows (no advisory lock exists there in
//!   this crate's safe API surface): stale leases are reclaimed by a
//!   single-owner recovery claim plus an atomic quarantine rename, release is
//!   ownership-checked, and a displaced holder cannot enter the critical
//!   section. Ownership is re-verified immediately before the publish rename,
//!   so a holder displaced while staging cannot overwrite newer evidence.
//! - **Confined paths.** The slot is derived from `XDG_STATE_HOME`,
//!   `HOME/.local/state`, or `LOCALAPPDATA`;
//!   `BITTY_WATCHDOG_STATE_PATH` is only honored when it repeats that derived
//!   path, and symlinked, hard-linked, foreign-owned, group/world-writable, or
//!   special-file slots are rejected. Temp files are created with `O_EXCL`
//!   (no-follow) semantics.
//! - **Shutdown-clean.** Dropping the handle wakes and joins the monitor, so
//!   no detached monitor survives app shutdown and no report is written once
//!   shutdown completed. Report delivery is deadline-bounded (spawn, stdin,
//!   helper work, wait, and a bounded reap window), so shutdown does not wait
//!   on an unread stderr pipe. Helper resources are single-flight: at most one
//!   spawn thread, one stdin writer, and a fixed abandoned-child set are live
//!   across repeated stalls, and finished ones are joined on the next
//!   delivery. A spawn thread owns every child it creates and kills+reaps one
//!   the supervisor no longer accepts. A pinned stdin writer is retained (the
//!   next delivery deliberately hands its helper an empty stdin, which the
//!   helper rejects as `HelperFailed`) rather than spawning a second writer. A
//!   helper stuck in uninterruptible I/O is killed and reaped with the same
//!   bounded window; a survivor is retained only up to the fixed abandoned
//!   cap and is killed+reaped again before any handle is dropped. Process
//!   cancellation is best-effort, not a proven kill guarantee.
//! - **No process-wide stderr mutation.** The monitor never duplicates or
//!   re-flags fd 2; the helper inherits it as a plain blocking descriptor and
//!   is killed at the deadline if the reader is gone.
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

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
/// Single-slot state file name under the platform state directory.
const STATE_FILE_NAME: &str = "stall-watchdog.log";
/// Companion single-slot file recording delivery outcomes.
///
/// Written directly by the monitor (local file I/O only) so a helper that could
/// not be spawned, timed out, or failed is still visible after the fact.
const DELIVERY_FILE_NAME: &str = "stall-watchdog-delivery.log";
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
/// Deadline for the complete isolated report delivery.
///
/// Covers helper spawn, the bounded stdin transfer, helper work (atomic state
/// slot, then stderr), and the wait. The monitor never blocks on a report for
/// longer than this deadline plus [`REPORT_HELPER_REAP_GRACE`]; spawn and
/// stdin are off-loaded to helper threads so neither can pin the monitor.
const REPORT_HELPER_TIMEOUT: Duration = Duration::from_millis(250);
/// Extra bounded window to reap a killed helper and collect its writer.
///
/// A helper stuck in uninterruptible I/O can outlive this window; the monitor
/// then abandons it (reaped by process exit) instead of blocking shutdown.
const REPORT_HELPER_REAP_GRACE: Duration = Duration::from_millis(100);
/// Supervision poll cadence (short sleep, never a busy spin).
const REPORT_HELPER_POLL: Duration = Duration::from_millis(5);
/// Private argv used by the bounded report helper.
pub(crate) const STATE_WRITER_ARG: &str = "--internal-stall-watchdog-state";
/// Environment variable carrying the optional state-slot path to the helper.
///
/// The helper only honors a value that repeats the platform-derived slot path;
/// other value is rejected, so the variable cannot redirect evidence.
const STATE_WRITER_PATH_ENV: &str = "BITTY_WATCHDOG_STATE_PATH";
/// Environment variable carrying the evidence generation to the helper.
const ATTEMPT_ENV: &str = "BITTY_WATCHDOG_ATTEMPT";
/// Maximum recent failure records kept in the companion delivery slot.
const MAX_DELIVERY_FAILURES: usize = 4;
/// Hard cap for the companion delivery slot.
const MAX_DELIVERY_BYTES: usize = 1024;
/// Bounded monitor-side wait for the evidence generation lock.
///
/// A late helper stuck mid-commit holds the lock; the monitor then records a
/// skipped delivery instead of writing unordered evidence.
const SLOT_LOCK_WAIT: Duration = Duration::from_millis(25);
/// Bounded helper-side wait for the evidence generation lock.
///
/// The supervisor's delivery deadline is shorter, so this only bounds a
/// directly invoked helper; it is never a path to an unbounded wait.
const HELPER_LOCK_WAIT: Duration = Duration::from_secs(1);
/// Lease lifetime of the portable generation gate.
///
/// Platforms without an advisory file lock in this crate's safe API surface
/// (Windows) use an atomically created gate file. A holder that dies leaves the
/// entry behind, so a lease older than this window is reclaimed; the window is
/// far longer than the bounded stage+rename critical section, so a live holder
/// is never displaced by a normal delivery. Both bounded lock waits above are
/// shorter than this lease, so no delivery reclaims another delivery's gate.
#[cfg(any(windows, test))]
const PORTABLE_GATE_STALE: Duration = Duration::from_secs(5);
/// Hard cap when reading a gate owner token.
///
/// Tokens are a fixed-shape process/sequence/timestamp string; a larger file
/// at the gate path is not ours and is rejected instead of read unbounded.
#[cfg(any(windows, test))]
const MAX_GATE_TOKEN_BYTES: usize = 128;
/// Maximum killed-but-unreaped helper children retained across stalls.
const MAX_ABANDONED_CHILDREN: usize = 4;
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
    pub(crate) fn from_u8(raw: u8) -> Self {
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
    pub(crate) fn as_str(self) -> &'static str {
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
    /// Surface-present counters from [`bitty_render::telemetry`] (issue #2570).
    pub(crate) surface: bitty_render::telemetry::SurfaceTelemetrySnapshot,
    /// Last window/surface lifecycle event name (issue #2570).
    pub(crate) lifecycle: &'static str,
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
pub(crate) struct ThreadRow {
    pub(crate) tid: u64,
    pub(crate) comm: String,
    pub(crate) state: char,
    pub(crate) wchan: String,
    pub(crate) syscall_number: String,
}

/// Process totals sampled alongside the thread rows.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProcessTotals {
    pub(crate) thread_count: usize,
    pub(crate) fd_count: usize,
    pub(crate) rss_kib: u64,
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
    /// Outcomes of earlier report deliveries, shown in-band so a lost evidence
    /// write is visible in the next report that does get through.
    delivery: DeliverySummary,
}

/// Bounded in-band delivery summary carried by the monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeliverySummary {
    attempts: u64,
    failures: u64,
    last: Option<DeliveryOutcome>,
}

impl DeliverySummary {
    fn record(&mut self, outcome: DeliveryOutcome) {
        self.attempts = self.attempts.saturating_add(1);
        if outcome.is_failure() {
            self.failures = self.failures.saturating_add(1);
        }
        self.last = Some(outcome);
    }

    /// One bounded report line; `None` before the first delivery.
    fn render(&self) -> Option<String> {
        let last = self.last?;
        Some(format!(
            "  watchdog-delivery: attempts={} failures={} last={}",
            self.attempts,
            self.failures,
            last.as_str(),
        ))
    }
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
    ///
    /// The stop flag is read *after* taking `waiter`: `request` holds the same
    /// lock while setting the flag and notifying, so a request that lands
    /// between the check and the wait cannot be missed.
    fn wait(&self, timeout: Duration) -> bool {
        let guard = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_stopped() {
            return true;
        }
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
/// monitor behind and no report is written once `Drop` returns. The production
/// sink never writes to the process's own stderr; it hands the report to a
/// killable helper under a hard deadline, so a closed or unread stderr pipe
/// cannot panic the monitor and cannot pin the join past that deadline plus
/// the bounded reap window. Injected test sinks retain normal join semantics
/// so no arbitrary closure is ever detached.
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
            let slot = StateSlot::from_env();
            let mut sink = ReportSink::new(slot, Box::new(HelperDelivery::current()));
            Self::start(config, Box::new(move |line| sink.emit(line)))
        })
    }

    /// Start a watchdog with an explicit configuration and sink.
    fn start(
        config: WatchdogConfig,
        mut sink: Box<dyn FnMut(&str) -> DeliveryOutcome + Send>,
    ) -> Self {
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
    sink: &mut (dyn FnMut(&str) -> DeliveryOutcome + Send),
) {
    let mut state = MonitorState {
        last_progress: shared.progress.load(Ordering::Relaxed),
        ..MonitorState::default()
    };
    let mut delivery = DeliverySummary::default();
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
                    delivery,
                };
                let rendered = render_report(&report);
                // Sampling /proc and rendering takes time; never emit once
                // shutdown was requested.
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&rendered));
            }
            MonitorAction::Recovered { stalled_for } => {
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&format!(
                    "bitty: main-loop stall recovered after {} ms (phase={})",
                    stalled_for.as_millis(),
                    phase.as_str()
                )));
            }
            MonitorAction::Suppressed => {
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&format!(
                    "bitty: main-loop stall still active after {} reports; further reports suppressed",
                    MAX_REPORTS_PER_STALL
                )));
            }
        }
    }
}

/// Sample all threads and process totals (Linux); other targets return empty.
pub(crate) fn sample_process_totals() -> (Vec<ThreadRow>, ProcessTotals) {
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
            let surface = &context.surface;
            let _ = writeln!(
                out,
                "  surface: last={} configure_err={} acquire_ok={} acquire_timeout={} acquire_outdated={} acquire_lost={} acquire_other={} present_ok={} present_err={} resize={} resize_zero_skipped={} headless_present={} retry_failures={} retry_attempts={} retry_recoveries={} retry_last_delay_ms={} lifecycle={}",
                surface.last_event,
                surface.configure_err,
                surface.acquire_ok,
                surface.acquire_timeout,
                surface.acquire_outdated,
                surface.acquire_lost,
                surface.acquire_other,
                surface.present_ok,
                surface.present_err,
                surface.resize,
                surface.resize_zero_skipped,
                surface.headless_present,
                surface.retry_failures,
                surface.retry_attempts,
                surface.retry_recoveries,
                surface.retry_last_delay_ms,
                context.lifecycle,
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
    // The delivery summary goes before the thread table: the row list is the
    // only unbounded-ish section, so truncation must drop rows, never the
    // evidence about earlier failed deliveries.
    if let Some(delivery) = report.delivery.render() {
        let _ = writeln!(out, "{delivery}");
    }
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
fn truncate_report(report: String) -> String {
    truncate_text(report, MAX_REPORT_BYTES, "\n  ... report truncated")
}

/// Hard-cap arbitrary evidence text at a char boundary with a marker.
fn truncate_text(mut text: String, max_bytes: usize, suffix: &str) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes.saturating_sub(suffix.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(suffix);
    text
}

/// Report delivery seam.
///
/// Production always goes through [`HelperDelivery`]: a separate helper
/// process owns the atomic state write and the stderr write, so the monitor
/// never touches the application's stderr descriptor or its blocking mode.
/// Tests inject deterministic deliveries; the production supervisor itself is
/// exercised with real child processes.
trait ReportDelivery: Send {
    /// Deliver one already-redacted report and report what happened to it.
    fn deliver(&mut self, request: DeliveryRequest<'_>) -> DeliveryResult;
}

/// One report handed to a [`ReportDelivery`].
struct DeliveryRequest<'a> {
    /// Redacted report text.
    line: &'a str,
    /// Confined evidence slot; `Some` only for a stall report (recovery and
    /// suppression lines never replace the persisted evidence).
    slot: Option<&'a StateSlot>,
}

/// Outcomes of one report delivery.
///
/// The primary outcome is the evidence path (or stderr-only when the report is
/// not persisted). When the evidence path cannot even allocate a generation,
/// [`HelperDelivery`] retries through a no-slot helper so the report still
/// reaches stderr; that second attempt is reported here and recorded as its
/// own ledger entry. Both outcomes therefore stay visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeliveryResult {
    /// The evidence (or first-attempt) outcome for the report.
    primary: DeliveryOutcome,
    /// Outcome of the no-slot stderr retry, present only when the evidence
    /// path failed before a helper could carry the report.
    fallback: Option<DeliveryOutcome>,
}

impl DeliveryResult {
    /// One attempt with no fallback.
    fn single(outcome: DeliveryOutcome) -> Self {
        Self {
            primary: outcome,
            fallback: None,
        }
    }

    /// An evidence failure followed by a no-slot stderr retry.
    fn with_fallback(primary: DeliveryOutcome, fallback: DeliveryOutcome) -> Self {
        Self {
            primary,
            fallback: Some(fallback),
        }
    }
}

/// Outcome of one helper delivery.
///
/// Every variant is recorded: [`ReportSink`] keeps a bounded ledger, mirrors it
/// to the companion delivery slot, and the next stall report carries the
/// summary. A failed delivery is therefore never silent even when both the
/// state slot and stderr are unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// Helper exited 0: the state slot (when requested) and stderr were
    /// handled by the helper.
    Delivered,
    /// Helper exited non-zero (state write failed) or the wait itself failed.
    HelperFailed,
    /// Deadline elapsed; the helper was killed and reaped within the grace
    /// window, or abandoned when it could not be reaped.
    TimedOut,
    /// The helper could not be spawned (EMFILE/ENFILE, missing exe).
    SpawnFailed,
    /// The confined evidence slot could not be prepared, locked, or sequenced.
    ///
    /// The report is still retried through a no-slot helper so stderr receives
    /// it; the sink records this outcome and the fallback's outcome separately.
    EvidenceFailed,
}

impl DeliveryOutcome {
    /// Stable lowercase name used in durable evidence.
    fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::HelperFailed => "helper-failed",
            Self::TimedOut => "timed-out",
            Self::SpawnFailed => "spawn-failed",
            Self::EvidenceFailed => "evidence-failed",
        }
    }

    /// Whether this delivery left no evidence.
    fn is_failure(self) -> bool {
        self != Self::Delivered
    }
}

/// Confined single-slot evidence file plus its directory.
///
/// The path is derived from `XDG_STATE_HOME`, `HOME/.local/state`, or
/// `LOCALAPPDATA`; the helper additionally
/// requires `BITTY_WATCHDOG_STATE_PATH` to repeat that derived path, so a
/// hostile or stale value can never redirect evidence. Every write is staged
/// in a fresh `O_EXCL` temp file and renamed into place, and symlinked,
/// hard-linked, foreign-owned, group/world-writable, or special-file slots are
/// rejected before anything is replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StateSlot {
    path: PathBuf,
}

impl StateSlot {
    /// Monitor-side resolution: use the shared platform resolver.
    fn from_env() -> Option<Self> {
        Some(Self {
            path: state_file_path()?,
        })
    }

    /// Pure monitor-side resolution (tests and fixtures).
    #[cfg(test)]
    fn from_values(
        xdg_state: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
        local_app_data: Option<&std::ffi::OsStr>,
    ) -> Option<Self> {
        Some(Self {
            path: state_file_path_from(xdg_state, home, local_app_data, STATE_FILE_NAME)?,
        })
    }

    /// Helper-side resolution: the inherited path must repeat the derived one.
    fn from_helper_env() -> Option<Self> {
        Self::from_helper_values(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
            std::env::var_os("LOCALAPPDATA").as_deref(),
            std::env::var_os(STATE_WRITER_PATH_ENV).as_deref(),
        )
    }

    /// Pure helper-side confinement.
    fn from_helper_values(
        xdg_state: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
        local_app_data: Option<&std::ffi::OsStr>,
        override_path: Option<&std::ffi::OsStr>,
    ) -> Option<Self> {
        let derived = state_file_path_from(xdg_state, home, local_app_data, STATE_FILE_NAME)?;
        let raw = override_path?;
        let candidate = normalize_absolute(Path::new(raw))?;
        if candidate != derived {
            // Fail closed: never honor a path outside the derived slot.
            return None;
        }
        Some(Self { path: candidate })
    }

    fn dir(&self) -> &Path {
        match self.path.parent() {
            Some(dir) => dir,
            None => Path::new("."),
        }
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from(STATE_FILE_NAME))
    }

    /// Companion single-slot file recording delivery outcomes.
    fn delivery_path(&self) -> PathBuf {
        self.dir().join(DELIVERY_FILE_NAME)
    }

    /// Deterministic temp sibling for one helper attempt.
    ///
    /// The attempt sequence makes the name unique per delivery, so a timed-out
    /// helper can never truncate or rename the staged bytes of a newer attempt.
    fn attempt_temp(&self, attempt: u64) -> PathBuf {
        self.dir()
            .join(format!(".{}.{attempt}.tmp", self.file_name()))
    }

    /// Validate the confined directory and the existing evidence files without
    /// following symlinks; create the state directory with private permissions.
    fn prepare(&self) -> std::io::Result<()> {
        create_private_dir(self.dir())?;
        let dir_meta = std::fs::symlink_metadata(self.dir())?;
        if dir_meta.file_type().is_symlink() {
            return Err(invalid_input("state directory is a symlink"));
        }
        if !dir_meta.is_dir() {
            return Err(invalid_input("state directory is not a directory"));
        }
        check_owned_directory(&dir_meta)?;
        check_existing_slot(&self.path)?;
        check_existing_slot(&self.lock_path())?;
        check_existing_slot(&self.gate_path())?;
        Ok(())
    }

    /// Stage bytes in a fresh, no-follow temp file (`O_EXCL` semantics).
    ///
    /// `create_new` never follows an existing symlink and never truncates a
    /// file another attempt owns; a pre-planted entry fails closed.
    fn stage(&self, temp: &Path, contents: &str) -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    }

    /// Validate the target and atomically rename the staged file over it.
    ///
    /// The generation and ownership check immediately before the rename is the
    /// last fail-closed guard: a holder displaced while staging (for example
    /// by a stale reclaim after a long `fsync`) must not replace evidence a
    /// newer helper already published.
    fn publish(&self, temp: &Path, lock: &SlotLock) -> std::io::Result<()> {
        self.prepare()?;
        if !lock.verify() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "evidence lock was reclaimed before publish",
            ));
        }
        std::fs::rename(temp, &self.path)
    }

    /// Remove the temp sibling owned by one attempt (timeout cleanup).
    fn remove_attempt_temp(&self, attempt: u64) {
        let _ = std::fs::remove_file(self.attempt_temp(attempt));
    }

    /// Lock/sequence file used for generation-ordered publication.
    fn lock_path(&self) -> PathBuf {
        self.dir().join(format!(".{}.lock", self.file_name()))
    }

    /// Portable gate entry used where no advisory lock exists (Windows).
    ///
    /// Distinct from [`Self::lock_path`] because the sequence file must keep
    /// its generation across attempts; the gate is created and removed per
    /// critical section.
    fn gate_path(&self) -> PathBuf {
        self.dir().join(format!(".{}.gate", self.file_name()))
    }

    fn open_lock(&self) -> std::io::Result<File> {
        check_existing_slot(&self.lock_path())?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
        }
        let file = options.open(self.lock_path())?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid_input("evidence lock is not a regular file"));
        }
        check_owned_regular_file(&meta)?;
        Ok(file)
    }

    /// Allocate the next evidence generation under the slot lock.
    ///
    /// `Ok(None)` means the bounded lock window expired: the caller skips this
    /// delivery instead of writing unordered evidence. Skipping is recorded as
    /// a [`DeliveryOutcome`], never silently ignored.
    fn next_attempt(&self, deadline: Instant) -> std::io::Result<Option<u64>> {
        self.prepare()?;
        let mut file = self.open_lock()?;
        let gate = self.gate_path();
        let mut lock = loop {
            match try_lock_slot(file, &gate) {
                Ok(lock) => break lock,
                Err(returned) => {
                    file = returned;
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(
                        REPORT_HELPER_POLL.min(deadline.saturating_duration_since(now)),
                    );
                }
            }
        };
        if !lock.verify() {
            // The portable gate was reclaimed as stale between acquisition
            // and entry; never allocate a generation out of order.
            lock.release();
            return Ok(None);
        }
        let next = match read_attempt(lock.file_mut()) {
            Ok(current) => current.saturating_add(1),
            Err(error) => {
                lock.release();
                return Err(error);
            }
        };
        let written = write_attempt(lock.file_mut(), next);
        lock.release();
        written?;
        Ok(Some(next))
    }

    /// Stage and publish one helper report with generation ordering.
    ///
    /// The generation check, staging, and rename all happen under the lock the
    /// monitor uses to allocate attempts; an attempt older than the current
    /// generation is discarded before it writes anything, so a late helper can
    /// never replace newer evidence. Ownership is verified again immediately
    /// before the rename, so a holder displaced during staging (for example by
    /// a stale reclaim after a long `fsync`) fails closed. Temp removal by the
    /// supervisor after a timeout is a second, independent guard. Concurrent
    /// app instances share the slot: the newest allocated generation wins and
    /// a superseded attempt is discarded rather than overwriting it.
    fn commit(&self, attempt: u64, contents: &str) -> std::io::Result<CommitOutcome> {
        self.prepare()?;
        let mut file = self.open_lock()?;
        let gate = self.gate_path();
        let deadline = Instant::now() + HELPER_LOCK_WAIT;
        let mut lock = loop {
            match try_lock_slot(file, &gate) {
                Ok(lock) => break lock,
                Err(returned) => {
                    file = returned;
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "evidence lock unavailable",
                        ));
                    }
                    std::thread::sleep(
                        REPORT_HELPER_POLL.min(deadline.saturating_duration_since(now)),
                    );
                }
            }
        };
        let result = (|| {
            if !lock.verify() {
                // A stale reclaim displaced this holder; refuse to publish
                // under a gate another process now owns.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "evidence lock was reclaimed",
                ));
            }
            if read_attempt(lock.file_mut())? > attempt {
                return Ok(CommitOutcome::Stale);
            }
            let temp = self.attempt_temp(attempt);
            self.stage(&temp, contents)?;
            self.publish(&temp, &lock)?;
            Ok(CommitOutcome::Published)
        })();
        if !matches!(result, Ok(CommitOutcome::Published)) {
            self.remove_attempt_temp(attempt);
        }
        lock.release();
        result
    }

    /// Atomically replace the companion delivery slot (best effort, bounded).
    fn write_delivery(&self, contents: &str) -> std::io::Result<()> {
        self.prepare()?;
        let temp = self.dir().join(format!(
            ".{DELIVERY_FILE_NAME}.{}-{}.tmp",
            std::process::id(),
            next_sidecar_sequence(),
        ));
        self.stage(&temp, contents)?;
        let target = self.delivery_path();
        let checked = check_existing_slot(&target);
        let result = checked.and_then(|()| std::fs::rename(&temp, &target));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

/// Result of a generation-ordered helper commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitOutcome {
    /// The staged report replaced the slot.
    Published,
    /// A newer attempt already holds the slot; the staged report was dropped.
    Stale,
}

/// Per-process sequence for sidecar temp names (unique across attempts).
fn next_sidecar_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

/// Lexically normalize an absolute path; `None` for relative or escaping paths.
fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            Component::ParentDir => return None,
        }
    }
    Some(normalized)
}

/// Create or harden the state directory with owner-only permissions.
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;

        // Open the directory itself without following a final symlink.  Using
        // fchmod on this handle avoids a path-based permission update race.
        let mut options = OpenOptions::new();
        options.read(true);
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
        let directory = options.open(dir)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(invalid_input("state directory is not a directory"));
        }
        check_owned_directory_owner(&metadata)?;
        nix::sys::stat::fchmod(&directory, nix::sys::stat::Mode::from_bits_truncate(0o700))
            .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
        check_owned_directory(&directory.metadata()?)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Reject a symlink, special file, hard link, or file another user owns.
fn check_existing_slot(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => check_owned_regular_file(&meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_owned_regular_file(meta: &std::fs::Metadata) -> std::io::Result<()> {
    if meta.file_type().is_symlink() {
        return Err(invalid_input("evidence file is a symlink"));
    }
    if !meta.is_file() {
        return Err(invalid_input("evidence file is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(permission_denied("evidence file is owned by another user"));
        }
        if meta.nlink() != 1 {
            return Err(invalid_input("evidence file is hard-linked"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(permission_denied("evidence file is group/world writable"));
        }
    }
    Ok(())
}

fn check_owned_directory(meta: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        check_owned_directory_owner(meta)?;
        if meta.mode() & 0o022 != 0 {
            return Err(permission_denied("state directory is group/world writable"));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
    }
    Ok(())
}

#[cfg(unix)]
fn check_owned_directory_owner(meta: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(permission_denied(
            "state directory is owned by another user",
        ));
    }
    Ok(())
}

fn invalid_input(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

#[cfg(unix)]
fn permission_denied(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

/// Held slot lock; explicitly released to avoid a panicking third-party drop.
struct SlotLock {
    #[cfg(unix)]
    inner: nix::fcntl::Flock<File>,
    #[cfg(windows)]
    inner: File,
    /// Portable generation gate held alongside the sequence file (Windows).
    #[cfg(windows)]
    gate: Option<PortableGate>,
    /// Test-only gate attached to a real Unix lock so the publish-time
    /// ownership check can be exercised without a Windows host.
    #[cfg(all(test, unix))]
    test_gate: Option<PortableGate>,
    #[cfg(not(any(unix, windows)))]
    inner: File,
}

impl SlotLock {
    fn file_mut(&mut self) -> &mut File {
        &mut self.inner
    }

    /// Whether this lock still holds the generation gate.
    ///
    /// Unix advisory locks cannot be stolen, so this is true there (except for
    /// the test-only attached gate). On Windows a token-checked gate can be
    /// reclaimed as stale; a displaced holder must not enter (or finish) the
    /// critical section.
    fn verify(&self) -> bool {
        #[cfg(windows)]
        let owned = self.gate.as_ref().is_some_and(PortableGate::still_owned);
        #[cfg(all(test, unix))]
        let owned = self
            .test_gate
            .as_ref()
            .is_none_or(PortableGate::still_owned);
        #[cfg(all(unix, not(test)))]
        let owned = true;
        #[cfg(not(any(unix, windows)))]
        let owned = true;
        owned
    }

    #[cfg(all(test, unix))]
    fn with_test_gate(mut self, gate: PortableGate) -> Self {
        self.test_gate = Some(gate);
        self
    }

    /// Release the lock without ever panicking on an unlock error.
    fn release(self) {
        #[cfg(unix)]
        {
            match self.inner.unlock() {
                Ok(file) => drop(file),
                // Unlock failure cannot be surfaced from `Drop` and must not
                // panic inside the helper; leak the descriptor until exit.
                Err((lock, _)) => std::mem::forget(lock),
            }
        }
        #[cfg(windows)]
        {
            // Remove the gate entry, then close the sequence file. No explicit
            // unlock exists for the portable gate; the release itself is
            // token-checked, and a displaced holder does not touch the path.
            if let Some(gate) = self.gate {
                if gate.still_owned() {
                    gate.release();
                }
            }
            drop(self.inner);
        }
        #[cfg(not(any(unix, windows)))]
        {
            drop(self.inner);
        }
    }
}

/// Portable generation gate for platforms without an advisory file lock in
/// this crate's safe API surface.
///
/// The entry is created atomically (`create_new`) with a unique owner token
/// written inside. Stale reclaims are serialized by a single-owner recovery
/// claim, then the stale entry is renamed atomically to a token-unique
/// quarantine path: only one recoverer can move the source, so only one can
/// win the steal. The winner verifies the quarantined token before publishing
/// its own gate, and release (or a displaced holder) only removes an entry
/// whose token matches the one this gate wrote.
///
/// A holder killed between create and remove leaves a lease, which
/// [`PORTABLE_GATE_STALE`] reclaims after a window far longer than any bounded
/// critical section. The reclaim is a bounded fallback, not a normal path:
/// both the monitor and the helper give up long before the lease expires, so a
/// live holder is only displaced if it truly stalls for seconds inside
/// stage+rename.
#[cfg(any(windows, test))]
#[derive(Clone, Debug)]
struct PortableGate {
    path: PathBuf,
    /// Unique owner token written into the gate entry.
    token: String,
}

#[cfg(any(windows, test))]
impl PortableGate {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            token: fresh_gate_token(),
        }
    }

    /// Try to take the gate without blocking.
    ///
    /// `Ok(true)` means this caller owns it; `Ok(false)` means another holder
    /// (or an unexpired lease) owns it and the caller should retry.
    fn try_acquire(&self) -> std::io::Result<bool> {
        match self.create() {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Quick rejection: a fresh gate cannot be stolen.
        match self.expired() {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        // Serialize stealers: only the single recovery-claim owner may move a
        // stale gate. Without this, two waiters could each rename a gate the
        // other just published, and both would believe they had won.
        let Some(claim) = RecoveryClaim::take(self.recovery_path())? else {
            return Ok(false);
        };
        // Re-inspect under the claim: the holder may have released or a new
        // gate may have replaced the lease while the claim was taken.
        let observed = match read_gate_token(&self.path) {
            Ok(token) => token,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match self.expired() {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        if !claim.still_owned() {
            // The recovery claim itself was reclaimed while we inspected the
            // gate; another recoverer owns the steal now.
            return Ok(false);
        }
        // The rename is the single-winner claim: a unique destination plus an
        // atomic rename means exactly one waiter can move this source entry.
        let quarantine = self.quarantine_path();
        match std::fs::rename(&self.path, &quarantine) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let moved = read_gate_token(&quarantine).unwrap_or_default();
        if moved != observed {
            // The stale entry was replaced between inspection and the rename:
            // this is not the lease we judged stale. Put the displaced entry
            // back when nobody took the slot, and never claim ownership.
            restore_or_remove(&quarantine, &self.path);
            return Ok(false);
        }
        let _ = std::fs::remove_file(&quarantine);
        match self.create() {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Create the gate entry atomically with this gate's owner token.
    fn create(&self) -> std::io::Result<()> {
        create_token_file(&self.path, &self.token)
    }

    /// Whether an existing gate entry is a stale lease.
    fn expired(&self) -> std::io::Result<bool> {
        lease_expired(&self.path)
    }

    /// Unique quarantine destination for one steal attempt.
    fn quarantine_path(&self) -> PathBuf {
        self.sibling_path(&format!(".stale.{}", self.token))
    }

    /// Single-owner recovery claim used to serialize stale reclaims.
    fn recovery_path(&self) -> PathBuf {
        self.sibling_path(".recover")
    }

    /// Path next to the gate with `suffix` appended to its file name.
    fn sibling_path(&self, suffix: &str) -> PathBuf {
        sibling_path(&self.path, suffix)
    }

    /// Whether this gate's token is still the entry at the gate path.
    fn still_owned(&self) -> bool {
        read_gate_token(&self.path)
            .ok()
            .is_some_and(|token| token == self.token)
    }

    /// Remove the gate entry only if it still carries this owner's token.
    ///
    /// The rename makes the check and the removal refer to one moved entry;
    /// a displaced holder that moves a newer gate restores it instead of
    /// deleting it.
    fn release(&self) {
        // Cheap pre-check: never touch the path when this holder was already
        // displaced. The post-move token check below stays authoritative for
        // the check-to-rename window.
        if !self.still_owned() {
            return;
        }
        let quarantine = self.quarantine_path();
        if std::fs::rename(&self.path, &quarantine).is_err() {
            return;
        }
        match read_gate_token(&quarantine) {
            Ok(token) if token == self.token => {
                let _ = std::fs::remove_file(&quarantine);
            }
            _ => restore_or_remove(&quarantine, &self.path),
        }
    }
}

/// Age of one gate-era entry; `None` when the filesystem cannot report it.
///
/// Rejects a symlinked or special entry before it can be judged stale, so a
/// planted path never gets reclaimed or followed.
#[cfg(any(windows, test))]
fn lease_age(path: &Path) -> std::io::Result<Option<Duration>> {
    let meta = std::fs::symlink_metadata(path)?;
    check_owned_regular_file(&meta)?;
    let Ok(modified) = meta.modified() else {
        return Ok(None);
    };
    Ok(modified.elapsed().ok())
}

/// Single-owner claim that serializes stale reclaim attempts.
///
/// Only the process that creates `.recover` (atomically) may move a stale
/// gate. The claim is removed on drop on every path, and a claim left by a
/// crashed recoverer is itself reclaimed only after the lease window.
#[cfg(any(windows, test))]
struct RecoveryClaim {
    path: PathBuf,
    token: String,
}

#[cfg(any(windows, test))]
impl RecoveryClaim {
    /// Take the recovery claim, or `None` when another recoverer owns it.
    fn take(path: PathBuf) -> std::io::Result<Option<Self>> {
        let token = fresh_gate_token();
        match create_token_file(&path, &token) {
            Ok(()) => return Ok(Some(Self { path, token })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if !lease_expired(&path)? {
            return Ok(None);
        }
        // The rename is the single-winner claim for a crashed recoverer too.
        let quarantine = sibling_path(&path, &format!(".stale.{token}"));
        match std::fs::rename(&path, &quarantine) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        if !lease_expired(&quarantine)? {
            // A fresh claim replaced the stale one in the window; put it back.
            restore_or_remove(&quarantine, &path);
            return Ok(None);
        }
        let _ = std::fs::remove_file(&quarantine);
        match create_token_file(&path, &token) {
            Ok(()) => Ok(Some(Self { path, token })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether this claim's token is still the entry at the claim path.
    fn still_owned(&self) -> bool {
        read_gate_token(&self.path)
            .ok()
            .is_some_and(|token| token == self.token)
    }
}

#[cfg(any(windows, test))]
impl Drop for RecoveryClaim {
    fn drop(&mut self) {
        // Ownership-checked: a displaced recoverer must not delete a newer
        // claim.
        if read_gate_token(&self.path).ok().as_deref() == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create one atomically exclusive file carrying `token`.
#[cfg(any(windows, test))]
fn create_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(error) = file.write_all(token.as_bytes()) {
        drop(file);
        // We created this entry and own it; do not leave a tokenless marker.
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    drop(file);
    Ok(())
}

/// Path next to `path` with `suffix` appended to its file name.
#[cfg(any(windows, test))]
fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().map_or_else(
        || String::from("gate"),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}{suffix}"))
}

/// Whether a gate-era entry is older than the reclaim lease.
#[cfg(any(windows, test))]
fn lease_expired(path: &Path) -> std::io::Result<bool> {
    match lease_age(path) {
        Ok(Some(age)) => Ok(age >= PORTABLE_GATE_STALE),
        Ok(None) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Put a quarantined entry back when the gate slot is free, else drop it.
///
/// Used when a holder discovers it moved an entry it does not own. The restore
/// is best-effort: if another owner already created a gate, the moved entry
/// must not replace it.
#[cfg(any(windows, test))]
fn restore_or_remove(quarantine: &Path, gate: &Path) {
    if std::fs::symlink_metadata(gate).is_err() {
        let _ = std::fs::rename(quarantine, gate);
    } else {
        let _ = std::fs::remove_file(quarantine);
    }
}

/// Generate a unique gate owner token (process, sequence, wall clock).
#[cfg(any(windows, test))]
fn fresh_gate_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{nanos}", std::process::id(), next_sidecar_sequence())
}

/// Read a bounded gate owner token without following a symlink.
#[cfg(any(windows, test))]
fn read_gate_token(path: &Path) -> std::io::Result<String> {
    let meta = std::fs::symlink_metadata(path)?;
    check_owned_regular_file(&meta)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    let file = options.open(path)?;
    let limit = u64::try_from(MAX_GATE_TOKEN_BYTES).unwrap_or(u64::MAX) + 1;
    let mut buffer = Vec::new();
    file.take(limit).read_to_end(&mut buffer)?;
    if buffer.len() > MAX_GATE_TOKEN_BYTES {
        return Err(invalid_input("gate token is too large"));
    }
    String::from_utf8(buffer).map_err(|_| invalid_input("gate token is not UTF-8"))
}

/// Acquire the slot lock without blocking; `Err` returns the file to retry.
#[cfg(unix)]
fn try_lock_slot(file: File, _gate: &Path) -> Result<SlotLock, File> {
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(inner) => Ok(SlotLock {
            inner,
            #[cfg(all(test, unix))]
            test_gate: None,
        }),
        Err((file, _errno)) => Err(file),
    }
}

/// Acquire the sequence file and the portable generation gate.
///
/// Windows has no `flock` in this crate's safe API surface. The gate preserves
/// the generation ordering guarantee: the sequence is read/written and the
/// evidence renamed while the gate is held, so an older late helper either
/// observes the newer generation and discards its attempt or cannot enter the
/// critical section at all.
#[cfg(windows)]
fn try_lock_slot(file: File, gate_path: &Path) -> Result<SlotLock, File> {
    let gate = PortableGate::new(gate_path.to_path_buf());
    match gate.try_acquire() {
        Ok(true) => Ok(SlotLock {
            inner: file,
            gate: Some(gate),
        }),
        Ok(false) | Err(_) => Err(file),
    }
}

/// Platforms with neither `flock` nor the Windows gate keep the sequence
/// check only; the crate does not ship a target in this class.
#[cfg(not(any(unix, windows)))]
fn try_lock_slot(file: File, _gate: &Path) -> Result<SlotLock, File> {
    Ok(SlotLock { inner: file })
}

/// Read the current evidence generation (missing or unparsable reads as 0).
fn read_attempt(file: &mut File) -> std::io::Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0u8; 32];
    let mut filled = 0;
    loop {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if filled == buffer.len() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(std::str::from_utf8(&buffer[..filled])
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0))
}

/// Persist the current evidence generation.
fn write_attempt(file: &mut File, attempt: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(format!("{attempt}\n").as_bytes())?;
    file.sync_data()
}

/// Bounded delivery ledger mirrored to the companion evidence slot.
#[derive(Debug, Default)]
struct DeliveryLedger {
    attempts: u64,
    failures: u64,
    last: Option<DeliveryOutcome>,
    recent_failures: VecDeque<(u64, DeliveryOutcome)>,
}

impl DeliveryLedger {
    fn record(&mut self, attempt: u64, outcome: DeliveryOutcome) {
        self.attempts = self.attempts.saturating_add(1);
        self.last = Some(outcome);
        if outcome.is_failure() {
            self.failures = self.failures.saturating_add(1);
            self.recent_failures.push_back((attempt, outcome));
            while self.recent_failures.len() > MAX_DELIVERY_FAILURES {
                self.recent_failures.pop_front();
            }
        }
    }

    /// Render the bounded companion slot contents.
    fn render(&self) -> String {
        let mut out = String::with_capacity(256);
        let last = self.last.map_or("none", DeliveryOutcome::as_str);
        let _ = writeln!(
            out,
            "bitty: stall watchdog delivery: attempts={} failures={} last={last}",
            self.attempts, self.failures,
        );
        for (attempt, outcome) in &self.recent_failures {
            let _ = writeln!(
                out,
                "bitty: stall watchdog delivery failed: attempt={attempt} outcome={}",
                outcome.as_str(),
            );
        }
        truncate_text(
            out,
            MAX_DELIVERY_BYTES,
            "\n  ... delivery evidence truncated",
        )
    }
}

/// Report sink used in production: one bounded state slot, then stderr.
///
/// The owner's report had no captured log because the graphical launcher's
/// stderr was lost and the process was terminated. Persisting the latest stall
/// report under the XDG state directory makes the next occurrence readable
/// after the fact; it is a single truncated slot, never an append log.
///
/// Ordering is deliberate and enforced inside the helper: the state slot is
/// atomically replaced first, so a blocked or broken stderr pipe cannot lose
/// the evidence. The monitor never writes to its own stderr: it hands the
/// redacted report to the killable helper and abandons it at the deadline.
/// The delivery outcome is returned to the monitor, kept in a bounded ledger,
/// and mirrored to a companion slot so `SpawnFailed`, `TimedOut`,
/// `HelperFailed`, and `EvidenceFailed` stay observable when the report itself
/// could not be stored. An evidence failure triggers a no-slot stderr retry
/// whose outcome is recorded as a second ledger entry.
struct ReportSink {
    slot: Option<StateSlot>,
    delivery: Box<dyn ReportDelivery>,
    ledger: DeliveryLedger,
}

impl ReportSink {
    fn new(slot: Option<StateSlot>, delivery: Box<dyn ReportDelivery>) -> Self {
        Self {
            slot,
            delivery,
            ledger: DeliveryLedger::default(),
        }
    }

    /// Emit one line, record what happened, and return the outcome.
    fn emit(&mut self, line: &str) -> DeliveryOutcome {
        let redacted = crate::observability::redact_text_for_stderr(line);
        // Recovery/suppression lines share the sink but never overwrite the
        // stall evidence.
        let slot = if line.starts_with(STALL_REPORT_PREFIX) {
            self.slot.as_ref()
        } else {
            None
        };
        let attempt = self.ledger.attempts.saturating_add(1);
        let result = self.delivery.deliver(DeliveryRequest {
            line: redacted.as_ref(),
            slot,
        });
        self.ledger.record(attempt, result.primary);
        if let Some(fallback) = result.fallback {
            // The evidence path failed, but a no-slot helper still tried
            // stderr. Record that second attempt instead of hiding it behind
            // the primary failure.
            self.ledger.record(attempt.saturating_add(1), fallback);
        }
        self.persist_ledger();
        result.primary
    }

    /// Mirror the bounded ledger to the companion slot.
    ///
    /// Bounded local file I/O only (one path under the confined state
    /// directory, at most [`MAX_DELIVERY_BYTES`]): never stderr, never a wait
    /// on the helper, and every error is contained so a read-only or full state
    /// directory cannot panic a stalled monitor.
    fn persist_ledger(&mut self) {
        let Some(slot) = self.slot.as_ref() else {
            return;
        };
        let rendered = self.ledger.render();
        let _ = slot.write_delivery(&rendered);
    }
}

/// Production delivery through the private report helper.
struct HelperDelivery {
    /// Current executable; `None` when it cannot be resolved (fail closed).
    program: Option<PathBuf>,
    /// Bounded helper resource tracking across repeated stalls.
    runtime: HelperRuntime,
}

impl HelperDelivery {
    fn current() -> Self {
        Self {
            program: std::env::current_exe().ok(),
            runtime: HelperRuntime::default(),
        }
    }
}

impl ReportDelivery for HelperDelivery {
    fn deliver(&mut self, request: DeliveryRequest<'_>) -> DeliveryResult {
        self.runtime.reap_finished();
        let Some(program) = self.program.as_deref() else {
            return DeliveryResult::single(DeliveryOutcome::SpawnFailed);
        };
        let deadline = Instant::now() + REPORT_HELPER_TIMEOUT;
        let attempt = match request.slot {
            Some(slot) => {
                let lock_deadline = deadline.min(Instant::now() + SLOT_LOCK_WAIT);
                match slot.next_attempt(lock_deadline) {
                    Ok(Some(attempt)) => Some(attempt),
                    // A stuck lock or unusable state directory: the evidence
                    // slot is gone, but the report must still reach stderr.
                    // Retry through a no-slot helper and record both outcomes.
                    Ok(None) | Err(_) => {
                        let fallback =
                            run_helper(program, request.line, None, deadline, &mut self.runtime);
                        return DeliveryResult::with_fallback(
                            DeliveryOutcome::EvidenceFailed,
                            fallback,
                        );
                    }
                }
            }
            None => None,
        };
        let target = request.slot.zip(attempt);
        DeliveryResult::single(run_helper(
            program,
            request.line,
            target,
            deadline,
            &mut self.runtime,
        ))
    }
}

/// Build and supervise one helper invocation.
///
/// `target` carries the state slot and generation; `None` is the no-slot
/// stderr-only helper used for recovery lines and the evidence-failure
/// fallback. Both inherited variables are removed in that case so an
/// environment value can never redirect evidence.
fn run_helper(
    program: &Path,
    line: &str,
    target: Option<(&StateSlot, u64)>,
    deadline: Instant,
    runtime: &mut HelperRuntime,
) -> DeliveryOutcome {
    let mut command = Command::new(program);
    command.arg(STATE_WRITER_ARG);
    match target {
        Some((slot, attempt)) => {
            command.env(STATE_WRITER_PATH_ENV, &slot.path);
            command.env(ATTEMPT_ENV, attempt.to_string());
        }
        None => {
            command.env_remove(STATE_WRITER_PATH_ENV);
            command.env_remove(ATTEMPT_ENV);
        }
    }
    supervise_helper(HelperRun {
        command,
        line,
        deadline,
        stderr: Stdio::inherit(),
        runtime,
        cleanup: target,
    })
}

/// Bounded lifetimes of helper resources across repeated stalls.
///
/// At most one spawn thread and one stdin writer are live at a time; a previous
/// resource that an unkillable helper still pins makes the next delivery skip
/// its own resource instead of accumulating threads. At most
/// [`MAX_ABANDONED_CHILDREN`] unreaped children are retained and every delivery
/// first tries to reap them.
#[derive(Default)]
struct HelperRuntime {
    spawn: Option<JoinHandle<()>>,
    stdin: Option<JoinHandle<()>>,
    abandoned: Vec<Child>,
    #[cfg(test)]
    spawns: u32,
    #[cfg(test)]
    writers: u32,
    /// Deliveries that kept the pinned writer and handed the helper an empty
    /// stdin instead of spawning a second writer.
    #[cfg(test)]
    writers_skipped: u32,
}

/// Disposition of one killed-but-unreaped helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbandonedOutcome {
    /// Added to the fixed abandoned set for a later reap.
    Retained,
    /// Killed and reaped immediately because the set was at its cap.
    Reaped,
    /// Killed, but still alive after the bounded window; the handle is dropped
    /// as the last resort (process exit reaps it).
    Detached,
}

impl HelperRuntime {
    /// Join finished threads and reap exited abandoned children (never blocks
    /// on a live child).
    ///
    /// A child whose state cannot be polled is cancelled and reaped within the
    /// bounded grace window instead of being dropped while possibly alive.
    #[allow(clippy::zombie_processes)] // `try_wait` observed the exit; drop is safe.
    fn reap_finished(&mut self) {
        for slot in [&mut self.spawn, &mut self.stdin] {
            if slot.as_ref().is_some_and(JoinHandle::is_finished) {
                if let Some(handle) = slot.take() {
                    let _ = handle.join();
                }
            }
        }
        let mut index = 0;
        while index < self.abandoned.len() {
            match self.abandoned[index].try_wait() {
                Ok(Some(_)) => {
                    let _ = self.abandoned.swap_remove(index);
                }
                Ok(None) => index += 1,
                Err(_) => {
                    // The child's state is unknown: contain it here rather
                    // than dropping a handle that may still own a live child.
                    let mut child = self.abandoned.swap_remove(index);
                    let _ = child.kill();
                    let _ = wait_child_bounded(child, REPORT_HELPER_REAP_GRACE);
                }
            }
        }
    }

    fn spawn_in_flight(&self) -> bool {
        self.spawn
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    fn writer_in_flight(&self) -> bool {
        self.stdin
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// Retain a killed-but-unreaped child within the fixed bound.
    ///
    /// At the cap the existing set is swept once for exited children; a child
    /// that still cannot be retained is killed and reaped before its handle is
    /// dropped, so the cap never turns into an unreaped zombie.
    fn track_abandoned(&mut self, child: Child) -> AbandonedOutcome {
        if self.abandoned.len() >= MAX_ABANDONED_CHILDREN {
            self.sweep_abandoned();
        }
        if self.abandoned.len() < MAX_ABANDONED_CHILDREN {
            self.abandoned.push(child);
            return AbandonedOutcome::Retained;
        }
        let mut child = child;
        let _ = child.kill();
        match wait_child_bounded(child, REPORT_HELPER_REAP_GRACE) {
            None => AbandonedOutcome::Reaped,
            Some(_) => AbandonedOutcome::Detached,
        }
    }

    /// Non-blocking sweep of already-exited abandoned children.
    #[allow(clippy::zombie_processes)] // `try_wait` observed the exit; drop is safe.
    fn sweep_abandoned(&mut self) {
        let mut index = 0;
        while index < self.abandoned.len() {
            match self.abandoned[index].try_wait() {
                Ok(Some(_)) => {
                    let _ = self.abandoned.swap_remove(index);
                }
                Ok(None) => index += 1,
                Err(_) => {
                    let mut child = self.abandoned.swap_remove(index);
                    let _ = child.kill();
                    let _ = wait_child_bounded(child, REPORT_HELPER_REAP_GRACE);
                }
            }
        }
    }

    /// Collect the stdin writer within the bounded grace window.
    ///
    /// A writer pinned by a helper (or a descendant that still holds the pipe)
    /// is retained as the single in-flight writer; the next delivery then
    /// closes its helper's stdin immediately and the helper rejects the empty
    /// report (`HelperFailed`). That is the shipped single-flight policy, not
    /// an unbounded writer set.
    fn finish_stdin(&mut self) {
        let Some(handle) = self.stdin.take() else {
            return;
        };
        let deadline = Instant::now() + REPORT_HELPER_REAP_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(REPORT_HELPER_POLL);
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            // Keep the single in-flight writer tracked; the next delivery will
            // not spawn a second one while it is alive.
            self.stdin = Some(handle);
        }
    }

    #[cfg(all(test, unix))]
    fn live_resources(&self) -> (bool, bool, usize) {
        (
            self.spawn_in_flight(),
            self.writer_in_flight(),
            self.abandoned.len(),
        )
    }
}

impl Drop for HelperRuntime {
    fn drop(&mut self) {
        // Non-blocking: a writer pinned by an unkillable helper is detached and
        // exits with the process; every finished thread is joined here.
        self.reap_finished();
        // Abandoned children were already signalled by the supervisor. Collect
        // a final exit without blocking so shutdown leaves no reapable zombie;
        // an unkillable survivor is left to process exit.
        for mut child in self.abandoned.drain(..) {
            let _ = child.kill();
            let _ = child.try_wait();
        }
    }
}

/// One supervised helper invocation.
struct HelperRun<'a> {
    command: Command,
    line: &'a str,
    deadline: Instant,
    stderr: Stdio,
    runtime: &'a mut HelperRuntime,
    /// State slot and attempt whose temp sibling must not outlive a failure.
    cleanup: Option<(&'a StateSlot, u64)>,
}

/// Run one helper to completion or the deadline, whichever comes first.
///
/// The full operation is bounded: spawn runs on its own thread (a hung exec
/// cannot pin the monitor and a late child is contained), the stdin transfer
/// runs on at most one writer thread, the wait polls until the deadline, and a
/// timed-out child is killed with a bounded reap window. A timed-out attempt's
/// temp file is removed so a late helper cannot publish stale evidence; the
/// helper-side generation check is the second guard. A child that survives the
/// reap window is abandoned (process exit reaps it); cancellation remains
/// best-effort and is measured by the repeated-stall test.
fn supervise_helper(run: HelperRun<'_>) -> DeliveryOutcome {
    let HelperRun {
        command,
        line,
        deadline,
        stderr,
        runtime,
        cleanup,
    } = run;
    runtime.reap_finished();
    let mut child = match spawn_helper_bounded(command, deadline, stderr, runtime) {
        Ok(child) => child,
        Err(outcome) => {
            remove_attempt_temp(cleanup);
            return outcome;
        }
    };
    start_stdin_writer(&mut child, line, runtime);
    let mut outcome = DeliveryOutcome::TimedOut;
    let mut exited = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                outcome = if status.success() {
                    DeliveryOutcome::Delivered
                } else {
                    DeliveryOutcome::HelperFailed
                };
                exited = true;
                break;
            }
            Ok(None) => {}
            Err(_) => {
                // Polling failed: the child's state is unknown, so cancel and
                // reap it below instead of dropping a possibly-live handle.
                outcome = DeliveryOutcome::HelperFailed;
                break;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep(REPORT_HELPER_POLL.min(deadline.saturating_duration_since(now)));
    }
    if !exited {
        // Timed out or the status could not be observed: cancel the helper and
        // reap it within the bounded grace window. A survivor is retained in
        // the fixed abandoned set (killed and reaped again at the cap).
        let _ = child.kill();
        if let Some(abandoned) = wait_child_bounded(child, REPORT_HELPER_REAP_GRACE) {
            runtime.track_abandoned(abandoned);
        }
    }
    if outcome.is_failure() {
        remove_attempt_temp(cleanup);
    }
    runtime.finish_stdin();
    outcome
}

/// Remove a timed-out or failed attempt's staged bytes.
fn remove_attempt_temp(cleanup: Option<(&StateSlot, u64)>) {
    if let Some((slot, attempt)) = cleanup {
        slot.remove_attempt_temp(attempt);
    }
}

/// Spawn the helper on a dedicated thread so a hung `exec` cannot hold the
/// monitor past the deadline.
///
/// Only one spawn thread is ever live: if the previous exec never returned,
/// the next delivery is recorded as timed out instead of accumulating threads.
/// The handoff is a rendezvous channel, so a child that arrives after the
/// supervisor gave up is still owned by the spawn thread: its `send` fails
/// with the child in hand, and the thread kills and reaps it within the same
/// bounded grace window instead of letting `Child::drop` leak a live process.
fn spawn_helper_bounded(
    mut command: Command,
    deadline: Instant,
    stderr: Stdio,
    runtime: &mut HelperRuntime,
) -> Result<Child, DeliveryOutcome> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr);
    let budget = deadline.saturating_duration_since(Instant::now());
    if budget.is_zero() {
        return Err(DeliveryOutcome::TimedOut);
    }
    if runtime.spawn_in_flight() {
        return Err(DeliveryOutcome::TimedOut);
    }
    // A rendezvous channel makes the handoff synchronous: `send` completes
    // only when this thread accepts the child, and a rejected child comes back
    // to the spawn thread for containment. An unbounded channel would queue
    // the child for a receiver that may already be gone.
    let (tx, rx) = mpsc::sync_channel(0);
    let spawned = std::thread::Builder::new()
        .name(String::from("bitty-stall-helper-spawn"))
        .spawn(move || {
            let _ = spawn_and_handoff(command, tx);
        });
    let Ok(handle) = spawned else {
        return Err(DeliveryOutcome::SpawnFailed);
    };
    runtime.spawn = Some(handle);
    #[cfg(test)]
    {
        runtime.spawns = runtime.spawns.saturating_add(1);
    }
    let outcome = match rx.recv_timeout(budget) {
        Ok(Ok(child)) => Ok(child),
        Ok(Err(_)) => Err(DeliveryOutcome::SpawnFailed),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DeliveryOutcome::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(DeliveryOutcome::SpawnFailed),
    };
    if !matches!(outcome, Err(DeliveryOutcome::TimedOut)) {
        // The thread already sent its result; collecting it now keeps at most
        // one spawn thread tracked instead of relying on the next delivery.
        if let Some(handle) = runtime.spawn.take() {
            let _ = handle.join();
        }
    }
    outcome
}

/// Spawn one helper and hand it to the supervisor, or contain it alone.
///
/// The thread owns every `Child` it creates: on a successful handoff the
/// supervisor owns it, and on a rejected handoff (the receiver timed out or was
/// dropped) this call kills and reaps it. `Child::drop` never sees a live
/// process on this path. Returns the contained child's pid for tests and
/// diagnostics.
fn spawn_and_handoff(
    mut command: Command,
    tx: mpsc::SyncSender<std::io::Result<Child>>,
) -> Option<u32> {
    match command.spawn() {
        Ok(child) => {
            let pid = child.id();
            if let Err(mpsc::SendError(Ok(mut child))) = tx.send(Ok(child)) {
                let _ = child.kill();
                let _ = wait_child_bounded(child, REPORT_HELPER_REAP_GRACE);
                return Some(pid);
            }
            None
        }
        Err(error) => {
            let _ = tx.send(Err(error));
            None
        }
    }
}

/// Write the report into the helper's stdin from a single bounded writer.
///
/// A report is capped at [`MAX_REPORT_BYTES`]; the writer thread exists only so
/// a helper that never reads cannot pin the supervisor. If a previous writer is
/// still blocked on an unkillable helper, this delivery closes its stdin
/// immediately (the helper rejects the empty report) instead of spawning a
/// second writer.
fn start_stdin_writer(child: &mut Child, line: &str, runtime: &mut HelperRuntime) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    if runtime.writer_in_flight() {
        drop(stdin);
        #[cfg(test)]
        {
            runtime.writers_skipped = runtime.writers_skipped.saturating_add(1);
        }
        return;
    }
    let bytes = line.as_bytes().to_vec();
    let spawned = std::thread::Builder::new()
        .name(String::from("bitty-stall-helper-stdin"))
        .spawn(move || {
            let _ = stdin.write_all(&bytes);
            drop(stdin);
        });
    if let Ok(handle) = spawned {
        runtime.stdin = Some(handle);
        #[cfg(test)]
        {
            runtime.writers = runtime.writers.saturating_add(1);
        }
    }
}

/// Wait for a child up to `grace`; `Some(child)` means it survived the window.
fn wait_child_bounded(mut child: Child, grace: Duration) -> Option<Child> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Some(child);
        }
        std::thread::sleep(REPORT_HELPER_POLL.min(deadline.saturating_duration_since(now)));
    }
}

/// Write one report to the optional atomic state slot, then stderr.
///
/// The state slot is replaced before the stderr write, so a failure or kill
/// during the stderr write cannot destroy the previous report. A state-slot
/// failure fails the helper (non-zero exit) without touching the existing
/// slot; stderr errors (EPIPE/EBADF) are tolerated.
fn write_report_to(
    target: Option<(&StateSlot, u64)>,
    report: &str,
    stderr: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let persisted = match target {
        Some((slot, attempt)) => slot.commit(attempt, report).map(|_| ()),
        None => Ok(()),
    };
    // Stderr is attempted even when the state slot failed: the report is the
    // diagnostic of last resort, and the helper's non-zero status still
    // records the persistence failure for the supervisor.
    let _ = stderr.write_all(report.as_bytes());
    let _ = stderr.write_all(b"\n");
    let _ = stderr.flush();
    persisted
}

/// Read the helper's report from stdin, capped at [`MAX_REPORT_BYTES`].
fn read_helper_report(input: &mut dyn std::io::Read) -> Option<String> {
    let mut buffer = Vec::new();
    input
        .take(u64::try_from(MAX_REPORT_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() > MAX_REPORT_BYTES {
        return None;
    }
    String::from_utf8(buffer).ok()
}

/// Private report helper invoked before normal CLI parsing.
///
/// Reads one bounded report, atomically replaces the optional state slot, then
/// writes the report to its inherited stderr. The helper is disposable: the
/// monitor kills it at the delivery deadline, which is how a blocked stderr
/// pipe stays bounded without ever changing the application's fd 2 flags.
/// Every state decision is fail-closed: a redirected path, a symlinked or
/// special-file slot, a missing generation, or an empty report exits non-zero
/// without replacing the previous evidence.
pub(crate) fn handle_state_writer(raw: &[String]) -> bool {
    if raw.get(1).map(String::as_str) != Some(STATE_WRITER_ARG) {
        return false;
    }
    let code = run_state_writer(&mut std::io::stdin(), &mut std::io::stderr());
    std::process::exit(code);
}

/// Helper body split from the process exit so tests can drive the exact path.
fn run_state_writer(input: &mut dyn std::io::Read, stderr: &mut dyn std::io::Write) -> i32 {
    let state_requested = std::env::var_os(STATE_WRITER_PATH_ENV).is_some();
    let slot = if state_requested {
        StateSlot::from_helper_env()
    } else {
        None
    };
    let attempt = std::env::var(ATTEMPT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let Some(report) = read_helper_report(input) else {
        return 1;
    };
    if report.trim().is_empty() {
        // An empty report means the supervisor could not hand over its bytes;
        // never replace good evidence with nothing.
        return 1;
    }
    let target = match (state_requested, slot.as_ref(), attempt) {
        (false, _, _) => None,
        (true, Some(slot), Some(attempt)) => Some((slot, attempt)),
        // A redirected or unresolvable path, or a missing generation: fail
        // closed without touching evidence, still emitting stderr.
        (true, _, _) => {
            let _ = stderr.write_all(report.as_bytes());
            let _ = stderr.write_all(b"\n");
            let _ = stderr.flush();
            return 1;
        }
    };
    match write_report_to(target, &report, stderr) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Resolve the single-slot state file path from platform state conventions.
fn state_file_path() -> Option<PathBuf> {
    state_file_path_for(STATE_FILE_NAME)
}

/// Resolve a named state file under the platform state directory.
///
/// Order: `XDG_STATE_HOME`, then `HOME/.local/state`, then `LOCALAPPDATA`
/// (Windows has no XDG/HOME by default, so the crash/exit report would
/// otherwise have no durable location).
pub(crate) fn state_file_path_for(name: &str) -> Option<PathBuf> {
    state_file_path_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        name,
    )
}

/// Pure path resolution for [`state_file_path`].
///
/// A relative base directory is invalid per the XDG base-directory spec and
/// would resolve against the process working directory; it fails closed.
fn state_file_path_from(
    xdg_state: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    local_app_data: Option<&std::ffi::OsStr>,
    name: &str,
) -> Option<PathBuf> {
    let base = xdg_state
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local").join("state")))
        .or_else(|| local_app_data.map(PathBuf::from))?;
    if !base.is_absolute() {
        return None;
    }
    Some(base.join("bitty").join(name))
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

    /// Confined slot rooted at a test directory.
    fn state_slot_in(dir: &Path) -> StateSlot {
        StateSlot::from_values(Some(dir.as_os_str()), None, None).expect("state slot")
    }

    /// Quote a path for a POSIX `sh` single-quoted context (test fixtures).
    #[cfg(unix)]
    fn quote_path(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[test]
    fn state_slot_is_a_single_bounded_file() {
        let dir = test_dir("state-slot");
        let slot = state_slot_in(&dir);
        slot.commit(1, "first").expect("initial slot");
        slot.commit(2, "second").expect("replace slot");
        assert_eq!(std::fs::read_to_string(&slot.path).unwrap(), "second");
        assert!(
            !slot.attempt_temp(2).exists(),
            "published temp must be gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_replacement_is_atomic_and_keeps_the_previous_report_on_failure() {
        let dir = test_dir("state-atomic");
        let slot = state_slot_in(&dir);
        slot.commit(1, "first").expect("initial slot");
        // Block the attempt temp with a directory: the new report cannot be
        // staged, so the helper fails before the rename and the previous
        // evidence stays byte-identical.
        std::fs::create_dir(slot.attempt_temp(2)).expect("block temp sibling");
        assert!(slot.commit(2, "second").is_err());
        assert_eq!(
            std::fs::read_to_string(&slot.path).expect("previous slot"),
            "first"
        );
        std::fs::remove_dir(slot.attempt_temp(2)).expect("unblock temp sibling");
        slot.commit(2, "second").expect("replacement after unblock");
        assert_eq!(std::fs::read_to_string(&slot.path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_generation_cannot_replace_newer_evidence() {
        let dir = test_dir("stale-generation");
        let slot = state_slot_in(&dir);
        let first = slot
            .next_attempt(Instant::now() + Duration::from_millis(100))
            .expect("lock")
            .expect("generation");
        let stale_temp = slot.attempt_temp(first);
        slot.stage(&stale_temp, "stale\n").expect("stage stale");
        let second = slot
            .next_attempt(Instant::now() + Duration::from_millis(100))
            .expect("lock")
            .expect("generation");
        assert!(second > first);
        slot.commit(second, "newer\n").expect("newer evidence");
        // A late helper republishes its staged bytes: rejected as stale.
        assert_eq!(
            slot.commit(first, "stale\n").expect("commit outcome"),
            CommitOutcome::Stale
        );
        assert_eq!(std::fs::read_to_string(&slot.path).unwrap(), "newer\n");
        assert!(!stale_temp.exists(), "stale staged bytes must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_slot_is_rejected_and_target_untouched() {
        let dir = test_dir("symlink-slot");
        let slot = state_slot_in(&dir);
        std::fs::create_dir_all(slot.dir()).expect("state dir");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "untouched").expect("victim");
        std::os::unix::fs::symlink(&victim, &slot.path).expect("symlink slot");
        assert!(slot.prepare().is_err(), "a symlinked slot must fail closed");
        assert!(slot.commit(1, "evidence").is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "untouched",
            "no write may follow the symlink"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_state_slot_is_rejected() {
        let dir = test_dir("hardlink-slot");
        let slot = state_slot_in(&dir);
        slot.commit(1, "evidence").expect("slot");
        std::fs::hard_link(&slot.path, dir.join("elsewhere")).expect("hard link");
        assert!(
            slot.prepare().is_err(),
            "a hard-linked slot can redirect evidence and must fail closed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn special_file_state_slot_is_rejected() {
        let dir = test_dir("fifo-slot");
        let slot = state_slot_in(&dir);
        slot.prepare().expect("state dir");
        nix::unistd::mkfifo(
            &slot.path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("fifo");
        assert!(slot.prepare().is_err(), "a FIFO slot must fail closed");
        assert!(slot.commit(1, "evidence").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn temp_staging_never_follows_a_preexisting_symlink() {
        let dir = test_dir("temp-symlink");
        let slot = state_slot_in(&dir);
        slot.prepare().expect("state dir");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "untouched").expect("victim");
        let temp = slot.attempt_temp(7);
        std::os::unix::fs::symlink(&victim, &temp).expect("symlink temp");
        assert!(
            slot.commit(7, "leak").is_err(),
            "O_EXCL staging must not follow a pre-planted symlink"
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_path_override_is_confined_to_the_derived_slot() {
        // Platform-derived absolute fixtures: Unix literals like
        // `/example/...` are not absolute on Windows, where the confinement
        // check would reject the *derived* path as well.
        let base = std::env::temp_dir().join("bitty-watchdog-confined-fixture");
        let derived = base.join("bitty").join(STATE_FILE_NAME);
        let elsewhere = std::env::temp_dir()
            .join("bitty-watchdog-confined-elsewhere")
            .join(STATE_FILE_NAME);
        let parent = base.join("bitty");
        let parent_escape = base
            .join("bitty")
            .join("..")
            .join("bitty")
            .join(STATE_FILE_NAME);
        assert_eq!(
            StateSlot::from_helper_values(
                Some(base.as_os_str()),
                None,
                None,
                Some(derived.as_os_str())
            ),
            Some(StateSlot {
                path: derived.clone()
            })
        );
        for hostile in [
            elsewhere.as_path(),
            parent.as_path(),
            parent_escape.as_path(),
            Path::new("relative/stall-watchdog.log"),
        ] {
            assert_eq!(
                StateSlot::from_helper_values(
                    Some(base.as_os_str()),
                    None,
                    None,
                    Some(hostile.as_os_str())
                ),
                None,
                "hostile override must be rejected: {}",
                hostile.display()
            );
        }
        assert_eq!(
            StateSlot::from_helper_values(Some(base.as_os_str()), None, None, None),
            None,
            "the helper only writes state when a path is requested"
        );
    }

    #[test]
    fn helper_input_is_capped_at_the_report_bound() {
        let exact = vec![b'x'; MAX_REPORT_BYTES];
        assert!(read_helper_report(&mut exact.as_slice()).is_some());
        let over = vec![b'x'; MAX_REPORT_BYTES + 1];
        assert!(
            read_helper_report(&mut over.as_slice()).is_none(),
            "an over-cap helper input must fail closed"
        );
        assert!(read_helper_report(&mut b"short report".as_slice()).is_some());
    }

    #[test]
    fn state_path_follows_xdg_then_home() {
        // Platform-derived absolute roots (Windows drive paths included).
        let base = std::env::temp_dir().join("bitty-watchdog-xdg-fixture");
        let home = std::env::temp_dir().join("bitty-watchdog-home-fixture");
        let local_app_data = std::env::temp_dir().join("bitty-watchdog-localappdata-fixture");
        assert_eq!(
            state_file_path_from(Some(base.as_os_str()), None, None, STATE_FILE_NAME),
            Some(base.join("bitty").join(STATE_FILE_NAME))
        );
        assert_eq!(
            state_file_path_from(None, Some(home.as_os_str()), None, STATE_FILE_NAME),
            Some(
                home.join(".local")
                    .join("state")
                    .join("bitty")
                    .join(STATE_FILE_NAME)
            )
        );
        assert_eq!(
            state_file_path_from(
                None,
                None,
                Some(local_app_data.as_os_str()),
                STATE_FILE_NAME
            ),
            Some(local_app_data.join("bitty").join(STATE_FILE_NAME))
        );
        assert_eq!(
            state_file_path_from(None, None, None, STATE_FILE_NAME),
            None
        );
        assert_eq!(
            state_file_path_from(
                Some(std::ffi::OsStr::new("relative/state")),
                None,
                None,
                STATE_FILE_NAME
            ),
            None,
            "a relative XDG state home must not resolve against the cwd"
        );
    }

    /// Age one gate entry into the stale-lease window (test fixture).
    #[cfg(any(windows, test))]
    fn age_gate(path: &Path) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("gate file");
        file.set_modified(
            std::time::SystemTime::now() - PORTABLE_GATE_STALE - Duration::from_secs(1),
        )
        .expect("age the lease");
    }

    #[test]
    fn portable_gate_is_exclusive_and_reusable() {
        let dir = test_dir("portable-gate");
        let gate = PortableGate::new(dir.join(".stall-watchdog.log.gate"));
        assert!(gate.try_acquire().expect("first acquire"));
        assert!(
            !gate.try_acquire().expect("second acquire"),
            "a live gate must exclude a second holder"
        );
        gate.release();
        assert!(
            gate.try_acquire().expect("reacquire"),
            "a released gate must be reusable"
        );
        gate.release();
        assert!(
            !dir.join(".stall-watchdog.log.gate").exists(),
            "release must remove the gate entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_gate_reclaims_only_an_expired_lease() {
        let dir = test_dir("portable-gate-stale");
        let path = dir.join(".stall-watchdog.log.gate");
        let gate = PortableGate::new(path.clone());
        assert!(gate.try_acquire().expect("acquire"));
        assert!(
            !gate.try_acquire().expect("fresh lease"),
            "a fresh lease must not be reclaimed"
        );
        age_gate(&path);
        assert!(
            gate.try_acquire().expect("reclaim"),
            "an expired lease must be reclaimed"
        );
        gate.release();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn portable_gate_never_reclaims_a_symlink() {
        let dir = test_dir("portable-gate-symlink");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "untouched").expect("victim");
        let gate_path = dir.join(".stall-watchdog.log.gate");
        std::os::unix::fs::symlink(&victim, &gate_path).expect("symlink gate");
        let gate = PortableGate::new(gate_path);
        assert!(
            gate.try_acquire().is_err(),
            "a symlinked gate must fail closed, never be removed or followed"
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_gate_release_is_ownership_checked() {
        // A holder displaced by a newer reclaim must not delete the newer
        // gate: release verifies the owner token of the entry it moves.
        let dir = test_dir("portable-gate-owner");
        let path = dir.join(".stall-watchdog.log.gate");
        let owner = PortableGate::new(path.clone());
        assert!(owner.try_acquire().expect("owner"));
        assert!(owner.still_owned());
        age_gate(&path);
        let newer = PortableGate::new(path.clone());
        assert!(newer.try_acquire().expect("reclaim"), "stale lease");
        assert!(!owner.still_owned(), "the displaced holder lost the gate");
        assert!(newer.still_owned());
        // A third waiter must see the fresh winner, not the displaced holder.
        let third = PortableGate::new(path.clone());
        assert!(
            !third.try_acquire().expect("third acquire"),
            "a fresh winner must exclude a third holder"
        );
        owner.release();
        assert!(path.exists(), "a displaced holder must not delete the gate");
        assert_eq!(
            read_gate_token(&path).expect("newer token"),
            newer.token,
            "the newer owner must still hold the gate"
        );
        newer.release();
        assert!(!path.exists(), "the owner's release must remove its gate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_stale_reclaim_has_one_winner() {
        // Race several waiters against one stale lease. The quarantine rename
        // is the single-winner claim: exactly one thread may acquire, and the
        // displaced/losing holders' releases must not delete the winner.
        use std::sync::{Arc, Barrier};
        const RACERS: usize = 6;
        let dir = test_dir("portable-gate-race");
        let path = dir.join(".stall-watchdog.log.gate");
        let seed = PortableGate::new(path.clone());
        assert!(seed.try_acquire().expect("seed gate"));
        age_gate(&path);
        let barrier = Arc::new(Barrier::new(RACERS));
        let handles: Vec<_> = (0..RACERS)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let gate = PortableGate::new(path);
                    barrier.wait();
                    let acquired = gate.try_acquire().expect("reclaim race");
                    (gate, acquired)
                })
            })
            .collect();
        let mut winner = None;
        let mut losers = Vec::new();
        for handle in handles {
            let (gate, acquired) = handle.join().expect("race thread");
            if acquired {
                assert!(winner.is_none(), "only one racer may win the reclaim");
                winner = Some(gate);
            } else {
                losers.push(gate);
            }
        }
        let winner = winner.expect("exactly one stale reclaim must win");
        // Every losing/displaced holder releases: none may remove the winner.
        for gate in &losers {
            gate.release();
        }
        seed.release();
        assert!(path.exists(), "a loser must not delete the winner's gate");
        assert_eq!(
            read_gate_token(&path).expect("winner token"),
            winner.token,
            "the winner must still own the gate"
        );
        winner.release();
        assert!(!path.exists(), "the winner's release must remove its gate");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("state dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".stale."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "quarantine entries leaked: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn displaced_stale_holder_cannot_publish_after_newer_evidence() {
        // Acquire the old lock, displace its gate, stage stale bytes through
        // that lock, let a newer helper publish, then attempt the stale
        // publish. The ownership check at publish time (not just at entry)
        // must fail closed and leave the newer slot intact.
        let dir = test_dir("displaced-publish");
        let slot = state_slot_in(&dir);
        slot.prepare().expect("state dir");
        let stale = slot
            .next_attempt(Instant::now() + Duration::from_millis(100))
            .expect("lock")
            .expect("generation");

        let file = slot.open_lock().expect("sequence file");
        let lock = try_lock_slot(file, &slot.gate_path()).expect("old lock");
        #[cfg(unix)]
        let mut lock = lock;
        #[cfg(unix)]
        {
            // Unix flock cannot be stolen; attach a portable gate to the real
            // lock so the publish-time token check is exercised there too.
            let old_gate = PortableGate::new(slot.gate_path());
            assert!(old_gate.try_acquire().expect("old gate"));
            lock = lock.with_test_gate(old_gate);
        }
        // On Windows `try_lock_slot` already created and owns the gate.
        age_gate(&slot.gate_path());
        let new_gate = PortableGate::new(slot.gate_path());
        assert!(
            new_gate.try_acquire().expect("reclaim"),
            "the stale holder must be displaceable"
        );
        assert!(
            !lock.verify(),
            "the reclaim must have displaced the old lock"
        );

        // The displaced holder stages its older bytes, then a newer helper
        // publishes while it is about to rename.
        let temp = slot.attempt_temp(stale);
        slot.stage(&temp, "stale\n").expect("stage stale bytes");
        std::fs::write(&slot.path, "newer\n").expect("newer evidence");
        assert!(
            slot.publish(&temp, &lock).is_err(),
            "the pre-publish ownership check must fail closed"
        );
        assert_eq!(
            std::fs::read_to_string(&slot.path).expect("state slot"),
            "newer\n",
            "a stale holder must never replace newer evidence"
        );
        assert!(temp.exists(), "a failed publish must not move the temp");

        lock.release();
        assert!(
            slot.gate_path().exists(),
            "a displaced holder must not delete the newer gate"
        );
        assert_eq!(
            read_gate_token(&slot.gate_path()).expect("newer gate token"),
            new_gate.token
        );
        new_gate.release();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delivery_summary_is_rendered_in_band() {
        let mut summary = DeliverySummary::default();
        assert!(summary.render().is_none());
        summary.record(DeliveryOutcome::TimedOut);
        summary.record(DeliveryOutcome::Delivered);
        let rendered = summary.render().expect("summary line");
        assert!(rendered.contains("attempts=2"), "{rendered}");
        assert!(rendered.contains("failures=1"), "{rendered}");
        assert!(rendered.contains("last=delivered"), "{rendered}");
    }

    #[test]
    fn delivery_ledger_is_bounded_and_names_every_failure() {
        let mut ledger = DeliveryLedger::default();
        let outcomes = [
            DeliveryOutcome::SpawnFailed,
            DeliveryOutcome::TimedOut,
            DeliveryOutcome::HelperFailed,
            DeliveryOutcome::EvidenceFailed,
            DeliveryOutcome::Delivered,
            DeliveryOutcome::SpawnFailed,
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            ledger.record(index as u64 + 1, outcome);
        }
        let rendered = ledger.render();
        assert!(
            rendered.starts_with("bitty: stall watchdog delivery: attempts=6 failures=5"),
            "{rendered}"
        );
        assert!(rendered.contains("last=spawn-failed"), "{rendered}");
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("delivery failed"))
                .count(),
            MAX_DELIVERY_FAILURES,
            "recent failures must stay bounded: {rendered}"
        );
        assert!(rendered.len() <= MAX_DELIVERY_BYTES, "{rendered}");
    }

    /// Delivery seam that returns a scripted result with no side effects.
    struct ScriptedDelivery {
        results: VecDeque<DeliveryResult>,
    }

    impl ReportDelivery for ScriptedDelivery {
        fn deliver(&mut self, _request: DeliveryRequest<'_>) -> DeliveryResult {
            self.results
                .pop_front()
                .unwrap_or_else(|| DeliveryResult::single(DeliveryOutcome::Delivered))
        }
    }

    #[test]
    fn every_delivery_failure_reaches_the_bounded_companion_slot() {
        let dir = test_dir("delivery-slot");
        let slot = state_slot_in(&dir);
        let mut sink = ReportSink::new(
            Some(slot.clone()),
            Box::new(ScriptedDelivery {
                results: VecDeque::from([
                    DeliveryResult::single(DeliveryOutcome::SpawnFailed),
                    DeliveryResult::single(DeliveryOutcome::TimedOut),
                    DeliveryResult::single(DeliveryOutcome::HelperFailed),
                ]),
            }),
        );
        let line = concat!(
            "bitty: main-loop stall detected: no progress for 2000 ms ",
            "(threshold 2000 ms, phase=plugin-timers, app_uptime 10 ms, progress=3)\n"
        );
        assert_eq!(sink.emit(line), DeliveryOutcome::SpawnFailed);
        assert_eq!(sink.emit(line), DeliveryOutcome::TimedOut);
        assert_eq!(sink.emit(line), DeliveryOutcome::HelperFailed);
        assert_eq!(sink.emit(line), DeliveryOutcome::Delivered);
        let evidence = std::fs::read_to_string(slot.delivery_path()).expect("delivery slot");
        assert!(
            evidence.starts_with("bitty: stall watchdog delivery: attempts=4 failures=3"),
            "{evidence}"
        );
        assert!(evidence.contains("last=delivered"), "{evidence}");
        for expected in ["spawn-failed", "timed-out", "helper-failed"] {
            assert!(
                evidence.contains(&format!("outcome={expected}")),
                "missing {expected}: {evidence}"
            );
        }
        assert!(evidence.len() <= MAX_DELIVERY_BYTES, "{evidence}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn production_spawn_failure_is_recorded_without_panicking() {
        let dir = test_dir("spawn-failure");
        let slot = state_slot_in(&dir);
        let mut sink = ReportSink::new(
            Some(slot.clone()),
            Box::new(HelperDelivery {
                program: None,
                runtime: HelperRuntime::default(),
            }),
        );
        let outcome = sink.emit(concat!(
            "bitty: main-loop stall detected: no progress for 2000 ms ",
            "(threshold 2000 ms, phase=runtime-tick, app_uptime 10 ms, progress=1)\n"
        ));
        assert_eq!(outcome, DeliveryOutcome::SpawnFailed);
        let evidence = std::fs::read_to_string(slot.delivery_path()).expect("delivery slot");
        assert!(evidence.contains("last=spawn-failed"), "{evidence}");
        assert!(evidence.contains("outcome=spawn-failed"), "{evidence}");
        assert!(!slot.path.exists(), "no stall slot may be written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn evidence_failure_records_both_outcomes_and_still_delivers_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = test_dir("evidence-fallback");
        let slot = state_slot_in(&dir);
        // Make the confined state directory unusable before any helper runs:
        // `prepare()`/`next_attempt()` fail, so the evidence path reports
        // `EvidenceFailed` without consuming the report.
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("bitty"), "not a directory").expect("blocker file");
        let report = "bitty: main-loop stall detected: no progress for 2000 ms\n";
        let capture = dir.join("captured-report.txt");
        let env_capture = dir.join("captured-env.txt");
        let script = dir.join("stderr-helper.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > {capture}\nprintf '%s|%s\\n' \"${{BITTY_WATCHDOG_STATE_PATH-absent}}\" \"${{BITTY_WATCHDOG_ATTEMPT-absent}}\" > {env_capture}\nexit 0\n",
                capture = quote_path(&capture),
                env_capture = quote_path(&env_capture),
            ),
        )
        .expect("helper script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("executable helper script");
        let mut sink = ReportSink::new(
            Some(slot),
            Box::new(HelperDelivery {
                program: Some(script),
                runtime: HelperRuntime::default(),
            }),
        );

        assert_eq!(sink.emit(report), DeliveryOutcome::EvidenceFailed);
        // Both outcomes are recorded: the evidence failure and the no-slot
        // stderr retry.
        assert_eq!(sink.ledger.attempts, 2, "{:?}", sink.ledger);
        assert_eq!(sink.ledger.failures, 1, "{:?}", sink.ledger);
        assert_eq!(sink.ledger.last, Some(DeliveryOutcome::Delivered));
        assert_eq!(
            sink.ledger.recent_failures.front(),
            Some(&(1, DeliveryOutcome::EvidenceFailed))
        );
        // The report reached the helper's stdin even though no state slot was
        // available, and the fallback carried no state-redirect variables.
        assert_eq!(std::fs::read_to_string(&capture).expect("capture"), report);
        assert_eq!(
            std::fs::read_to_string(&env_capture).expect("env capture"),
            "absent|absent\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_failure_records_a_persisted_ledger_with_both_outcomes() {
        let dir = test_dir("evidence-fallback-ledger");
        let slot = state_slot_in(&dir);
        let mut sink = ReportSink::new(
            Some(slot.clone()),
            Box::new(ScriptedDelivery {
                results: VecDeque::from([DeliveryResult::with_fallback(
                    DeliveryOutcome::EvidenceFailed,
                    DeliveryOutcome::Delivered,
                )]),
            }),
        );
        assert_eq!(
            sink.emit(concat!(
                "bitty: main-loop stall detected: no progress for 2000 ms ",
                "(threshold 2000 ms, phase=runtime-tick, app_uptime 10 ms, progress=1)\n"
            )),
            DeliveryOutcome::EvidenceFailed
        );
        assert_eq!(sink.ledger.attempts, 2);
        assert_eq!(sink.ledger.failures, 1);
        assert_eq!(sink.ledger.last, Some(DeliveryOutcome::Delivered));
        let evidence = std::fs::read_to_string(slot.delivery_path()).expect("delivery slot");
        assert!(
            evidence.starts_with("bitty: stall watchdog delivery: attempts=2 failures=1"),
            "{evidence}"
        );
        assert!(evidence.contains("outcome=evidence-failed"), "{evidence}");
        assert!(evidence.contains("last=delivered"), "{evidence}");
        let _ = std::fs::remove_dir_all(&dir);
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
                ..Default::default()
            }),
            threads: rows,
            totals,
            delivery: DeliverySummary {
                attempts: 2,
                failures: 1,
                last: Some(DeliveryOutcome::TimedOut),
            },
        };
        let rendered = render_report(&report);
        assert!(rendered.contains("phase=runtime-tick"), "{rendered}");
        assert!(rendered.contains("[main]"), "{rendered}");
        assert!(
            rendered.contains("watchdog-delivery: attempts=2 failures=1 last=timed-out"),
            "{rendered}"
        );
        assert!(rendered.len() <= MAX_REPORT_BYTES, "{}", rendered.len());
        assert!(rendered.contains("surface: last=none"), "{rendered}");
        assert!(!rendered.contains("/home/"), "{rendered}");
    }

    #[test]
    fn delivery_summary_precedes_thread_rows_and_survives_truncation() {
        // Long synthetic names saturate the 8 KiB report cap; the delivery
        // summary must still be present because it is rendered before the
        // thread table.
        const SATURATING_ROWS: usize = 64;
        let rows = (0..SATURATING_ROWS)
            .map(|index| ThreadRow {
                tid: 20_000 + index as u64,
                comm: "worker-with-a-long-name".repeat(8),
                state: 'S',
                wchan: "futex_wait_queue".repeat(8),
                syscall_number: "202".to_string(),
            })
            .collect();
        let report = StallReport {
            threshold: DEFAULT_STALL_THRESHOLD,
            idle: Duration::from_secs(3),
            phase: Phase::RuntimeTick,
            app_uptime: Duration::from_secs(7200),
            progress: 42,
            context: None,
            threads: rows,
            totals: ProcessTotals::default(),
            delivery: DeliverySummary {
                attempts: 3,
                failures: 2,
                last: Some(DeliveryOutcome::EvidenceFailed),
            },
        };
        let rendered = render_report(&report);
        assert!(rendered.len() <= MAX_REPORT_BYTES, "{}", rendered.len());
        assert!(
            rendered.contains("report truncated"),
            "the fixture must force truncation: {}",
            rendered.len()
        );
        let summary = rendered
            .find("watchdog-delivery: attempts=3 failures=2 last=evidence-failed")
            .unwrap_or_else(|| panic!("delivery summary must survive truncation: {rendered}"));
        let first_row = rendered
            .find("tid=")
            .unwrap_or_else(|| panic!("expected at least one thread row: {rendered}"));
        assert!(
            summary < first_row,
            "the delivery summary must precede the thread table"
        );
    }

    #[test]
    fn watchdog_thread_reports_a_stalled_heartbeat() {
        let (tx, rx) = mpsc::channel::<String>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(40),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
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

    /// Stderr stand-in that fails exactly like a closed pipe.
    struct BrokenStderr {
        state_path: PathBuf,
        saw_state_before_write: Arc<AtomicBool>,
        /// Set by the first stderr write of the first delivery.
        first_write_seen: Arc<AtomicBool>,
        /// Whether the state slot existed at that first write.
        first_write_saw_state: Arc<AtomicBool>,
    }

    impl std::io::Write for BrokenStderr {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            // Record whether the state evidence already existed when this
            // write arrived; that is the ordering the helper guarantees. The
            // first write is recorded separately so a later report cannot mask
            // an ordering violation in the first delivery.
            let first = !self.first_write_seen.swap(true, Ordering::SeqCst);
            if first {
                self.first_write_saw_state
                    .store(self.state_path.exists(), Ordering::SeqCst);
            }
            if self.state_path.exists() {
                self.saw_state_before_write.store(true, Ordering::SeqCst);
            }
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    /// Delivery that runs the helper's real state+stderr body in-process with
    /// an injected failing stderr, so the sink wiring is covered without
    /// depending on the test harness running as the CLI binary.
    struct OrderingDelivery {
        state_path: PathBuf,
        saw_state_before_write: Arc<AtomicBool>,
        first_write_seen: Arc<AtomicBool>,
        first_write_saw_state: Arc<AtomicBool>,
    }

    impl ReportDelivery for OrderingDelivery {
        fn deliver(&mut self, request: DeliveryRequest<'_>) -> DeliveryResult {
            let Some(slot) = request.slot else {
                return DeliveryResult::single(DeliveryOutcome::Delivered);
            };
            let attempt = slot
                .next_attempt(Instant::now() + Duration::from_millis(100))
                .ok()
                .flatten()
                .unwrap_or(0);
            let _ = write_report_to(
                Some((slot, attempt)),
                request.line,
                &mut BrokenStderr {
                    state_path: self.state_path.clone(),
                    saw_state_before_write: Arc::clone(&self.saw_state_before_write),
                    first_write_seen: Arc::clone(&self.first_write_seen),
                    first_write_saw_state: Arc::clone(&self.first_write_saw_state),
                },
            );
            DeliveryResult::single(DeliveryOutcome::Delivered)
        }
    }

    #[test]
    fn report_sink_persists_state_before_a_broken_stderr_write() {
        let dir = test_dir("sink-order");
        let slot = state_slot_in(&dir);
        let saw_state = Arc::new(AtomicBool::new(false));
        let mut sink = ReportSink::new(
            Some(slot.clone()),
            Box::new(OrderingDelivery {
                state_path: slot.path.clone(),
                saw_state_before_write: Arc::clone(&saw_state),
                first_write_seen: Arc::new(AtomicBool::new(false)),
                first_write_saw_state: Arc::new(AtomicBool::new(false)),
            }),
        );
        let line = concat!(
            "bitty: main-loop stall detected: no progress for 2000 ms ",
            "(threshold 2000 ms, phase=plugin-timers, app_uptime 10 ms, progress=3)\n",
            "  thread-state snapshot\n"
        );
        assert_eq!(sink.emit(line), DeliveryOutcome::Delivered);

        assert!(
            saw_state.load(Ordering::SeqCst),
            "the state slot must exist before stderr is written"
        );
        assert_eq!(
            std::fs::read_to_string(&slot.path).expect("state slot"),
            line
        );
        // Recovery lines share the sink but never overwrite the evidence.
        sink.emit("bitty: main-loop stall recovered after 2000 ms (phase=idle)");
        assert_eq!(
            std::fs::read_to_string(&slot.path).expect("state slot"),
            line
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn monitor_soak_with_broken_stderr_keeps_phase_and_thread_evidence() {
        let dir = test_dir("soak");
        let slot = state_slot_in(&dir);
        let writes = Arc::new(AtomicU64::new(0));
        let saw_state = Arc::new(AtomicBool::new(false));
        let first_write = Arc::new(AtomicBool::new(false));
        let first_write_state = Arc::new(AtomicBool::new(false));
        let mut sink = ReportSink::new(
            Some(slot.clone()),
            Box::new(OrderingDelivery {
                state_path: slot.path.clone(),
                saw_state_before_write: Arc::clone(&saw_state),
                first_write_seen: Arc::clone(&first_write),
                first_write_saw_state: Arc::clone(&first_write_state),
            }),
        );
        let sink_writes = Arc::clone(&writes);
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(45),
            Box::new(move |line| {
                let outcome = sink.emit(line);
                sink_writes.fetch_add(1, Ordering::SeqCst);
                outcome
            }),
        );
        watchdog.enter(Phase::PluginTimers);

        let deadline = Instant::now() + Duration::from_secs(10);
        let evidence = loop {
            if let Ok(text) = std::fs::read_to_string(&slot.path) {
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
        // Wait for the monitor's first stderr write instead of sampling a
        // snapshot: the main thread can observe the renamed slot before the
        // monitor reaches the stderr attempt. The per-first-write probe then
        // asserts the real contract (evidence exists before that write).
        let first_write_deadline = Instant::now() + Duration::from_secs(5);
        while !first_write.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < first_write_deadline,
                "sink never attempted the failing stderr write"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            first_write_state.load(Ordering::SeqCst),
            "evidence must be on disk before the first failing stderr write"
        );
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
            std::fs::read_to_string(&slot.path).expect("state slot"),
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
                DeliveryOutcome::Delivered
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
    fn shell_helper(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[cfg(unix)]
    fn run_helper(
        runtime: &mut HelperRuntime,
        command: Command,
        line: &str,
        timeout: Duration,
    ) -> DeliveryOutcome {
        supervise_helper(HelperRun {
            command,
            line,
            deadline: Instant::now() + timeout,
            stderr: Stdio::null(),
            runtime,
            cleanup: None,
        })
    }

    #[test]
    fn helper_spawn_failure_is_fail_closed() {
        // EMFILE/ENFILE or a missing executable must not panic or block: the
        // monitor simply loses the stderr copy while the state slot remains
        // best-effort.
        let mut runtime = HelperRuntime::default();
        let command = Command::new("bitty-watchdog-helper-that-does-not-exist-0123456789");
        let outcome = supervise_helper(HelperRun {
            command,
            line: "report\n",
            deadline: Instant::now() + Duration::from_millis(200),
            stderr: Stdio::null(),
            runtime: &mut runtime,
            cleanup: None,
        });
        assert_eq!(outcome, DeliveryOutcome::SpawnFailed);
    }

    #[cfg(unix)]
    #[test]
    fn helper_with_an_unread_stderr_pipe_times_out_and_is_reaped() {
        // Production shape: the helper inherits a pipe nobody reads. It blocks
        // writing to stderr; the supervisor must kill it at the deadline
        // instead of pinning the monitor.
        let (reader, writer) = nix::unistd::pipe().expect("OS pipe");
        let mut runtime = HelperRuntime::default();
        let started = Instant::now();
        let outcome = supervise_helper(HelperRun {
            command: shell_helper("head -c 1000000 /dev/zero >&2"),
            line: "stall report\n",
            deadline: started + Duration::from_millis(120),
            stderr: Stdio::from(writer),
            runtime: &mut runtime,
            cleanup: None,
        });
        assert_eq!(outcome, DeliveryOutcome::TimedOut);
        assert!(
            started.elapsed()
                <= Duration::from_millis(120)
                    + REPORT_HELPER_REAP_GRACE
                    + Duration::from_millis(500),
            "saturated stderr pipe must stay deadline-bounded, got {:?}",
            started.elapsed()
        );
        drop(reader);
    }

    #[cfg(unix)]
    #[test]
    fn helper_status_is_observed() {
        let mut runtime = HelperRuntime::default();
        let outcome = run_helper(
            &mut runtime,
            shell_helper("cat >/dev/null; exit 7"),
            "report\n",
            Duration::from_secs(2),
        );
        assert_eq!(outcome, DeliveryOutcome::HelperFailed);
        let delivered = run_helper(
            &mut runtime,
            shell_helper("cat >/dev/null"),
            "report\n",
            Duration::from_secs(2),
        );
        assert_eq!(delivered, DeliveryOutcome::Delivered);
    }

    #[cfg(unix)]
    #[test]
    fn helper_that_never_reads_stdin_cannot_pin_the_supervisor() {
        // A helper that never reads stdin leaves the writer thread blocked on
        // the pipe. The supervisor still returns at the deadline and only
        // drains the writer within the bounded grace window.
        let line = "x".repeat(1024 * 1024);
        let mut runtime = HelperRuntime::default();
        let started = Instant::now();
        let outcome = run_helper(
            &mut runtime,
            shell_helper("sleep 5"),
            &line,
            Duration::from_millis(120),
        );
        assert_eq!(outcome, DeliveryOutcome::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a blocked stdin write must not pin the supervisor, got {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn repeated_stalls_keep_helper_resources_bounded() {
        // Six consecutive stalls with a helper that never reads stdin, an
        // unread stderr pipe, and a 1 MiB report. The shipped policy is
        // single-flight, not "no pinned writer": a writer blocked on a helper
        // pipe (or on a descendant that still holds it) is retained, and the
        // next delivery deliberately gives its helper an empty stdin instead
        // of spawning a second writer. Whether the pipe closes when the direct
        // child is killed depends on the platform and on whether `sh` forked
        // `sleep`, so `TimedOut` and `HelperFailed` are both valid; the
        // invariants are the bounds, never a fixed outcome or a sleep-derived
        // order.
        const ATTEMPTS: u32 = 6;
        let line = "x".repeat(1024 * 1024);
        let mut runtime = HelperRuntime::default();
        let started = Instant::now();
        for attempt in 0..ATTEMPTS {
            let (reader, writer) = nix::unistd::pipe().expect("OS pipe");
            let writers_before = runtime.writers;
            let skips_before = runtime.writers_skipped;
            let outcome = supervise_helper(HelperRun {
                command: shell_helper("sleep 5"),
                line: &line,
                deadline: Instant::now() + Duration::from_millis(150),
                stderr: Stdio::from(writer),
                runtime: &mut runtime,
                cleanup: None,
            });
            assert!(
                matches!(
                    outcome,
                    DeliveryOutcome::TimedOut | DeliveryOutcome::HelperFailed
                ),
                "attempt {attempt}: unexpected outcome {outcome:?}"
            );
            let (spawn_live, _writer_live, abandoned) = runtime.live_resources();
            assert!(
                !spawn_live,
                "attempt {attempt}: no spawn thread may survive a delivery"
            );
            // `stdin` is a single optional handle, so at most one writer can
            // survive. Use the decisions `start_stdin_writer` actually made
            // during this delivery: exactly one spawn or one skip, never both
            // and never neither. This is race-free even when `reap_finished`
            // joins a previously pinned writer first.
            let spawned = runtime.writers == writers_before + 1;
            let skipped = runtime.writers_skipped == skips_before + 1;
            assert!(
                spawned ^ skipped,
                "attempt {attempt}: exactly one stdin decision expected \
                 (writers {}->{}, skipped {}->{})",
                writers_before,
                runtime.writers,
                skips_before,
                runtime.writers_skipped,
            );
            if skipped {
                assert!(
                    runtime.stdin.is_some(),
                    "attempt {attempt}: a skip implies the pinned writer handle"
                );
            }
            assert!(
                abandoned <= MAX_ABANDONED_CHILDREN,
                "attempt {attempt}: abandoned={abandoned}"
            );
            drop(reader);
        }
        assert!(
            runtime.spawns <= ATTEMPTS,
            "spawn threads must not grow past one per delivery: {}",
            runtime.spawns
        );
        assert!(
            runtime.writers <= ATTEMPTS,
            "stdin writers must not grow past one per delivery: {}",
            runtime.writers
        );
        assert!(
            runtime.writers_skipped <= ATTEMPTS,
            "stdin skips must not grow past one per delivery: {}",
            runtime.writers_skipped
        );
        assert!(
            runtime.abandoned.len() <= MAX_ABANDONED_CHILDREN,
            "{}",
            runtime.abandoned.len()
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "repeated stalls must stay bounded, got {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdin_writer_is_single_flight_when_a_previous_writer_is_pinned() {
        let mut runtime = HelperRuntime::default();
        runtime.stdin = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
        }));
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("cat >/dev/null")
            .stdin(Stdio::piped())
            .spawn()
            .expect("child");
        start_stdin_writer(&mut child, "report\n", &mut runtime);
        assert_eq!(
            runtime.writers, 0,
            "a second writer must not be spawned while one is pinned"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn spawn_is_single_flight_when_a_previous_exec_is_stuck() {
        let mut runtime = HelperRuntime::default();
        runtime.spawn = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
        }));
        let outcome = spawn_helper_bounded(
            Command::new("/bin/sh"),
            Instant::now() + Duration::from_millis(100),
            Stdio::null(),
            &mut runtime,
        );
        assert_eq!(outcome.unwrap_err(), DeliveryOutcome::TimedOut);
        assert_eq!(runtime.spawns, 0, "no second spawn thread may exist");
    }

    #[cfg(unix)]
    #[test]
    fn late_rename_after_newer_evidence_is_rejected() {
        // Deterministic replacement for the old timing-based test: that test
        // SIGKILLed the shell before its `mv` could ever run, so it proved
        // nothing. Here the supervisor's deterministic cleanup is checked
        // first, then the test itself performs the late rename, and finally
        // the helper's own generation-checked commit rejects the stale
        // attempt. The real helper process path is covered by
        // `stall_watchdog_helper::helper_rejects_a_stale_generation`.
        let dir = test_dir("late-helper");
        let slot = state_slot_in(&dir);
        slot.prepare().expect("state dir");
        let mut runtime = HelperRuntime::default();
        let stale = slot
            .next_attempt(Instant::now() + Duration::from_millis(100))
            .expect("lock")
            .expect("generation");
        let temp = slot.attempt_temp(stale);
        slot.stage(&temp, "stale\n").expect("stage stale bytes");
        let outcome = supervise_helper(HelperRun {
            command: shell_helper("exec sleep 5"),
            line: "short report\n",
            deadline: Instant::now() + Duration::from_millis(120),
            stderr: Stdio::null(),
            runtime: &mut runtime,
            cleanup: Some((&slot, stale)),
        });
        assert_eq!(outcome, DeliveryOutcome::TimedOut);
        assert!(!temp.exists(), "the timed-out temp must be removed");

        let newer = slot
            .next_attempt(Instant::now() + Duration::from_millis(100))
            .expect("lock")
            .expect("generation");
        assert!(newer > stale);
        slot.commit(newer, "newer\n").expect("newer evidence");

        // An actual late rename of the removed attempt temp cannot replace
        // the newer evidence.
        let late = std::fs::rename(&temp, &slot.path);
        assert!(late.is_err(), "a removed attempt temp cannot be renamed");
        assert_eq!(std::fs::read_to_string(&slot.path).unwrap(), "newer\n");

        // Even when the stale bytes reappear, the generation check (the same
        // lock/check the helper uses) discards the stale commit.
        slot.stage(&temp, "stale\n").expect("re-stage stale bytes");
        assert_eq!(
            slot.commit(stale, "stale\n").expect("commit outcome"),
            CommitOutcome::Stale
        );
        assert_eq!(std::fs::read_to_string(&slot.path).unwrap(), "newer\n");
        assert!(!temp.exists(), "a stale commit must remove its staged temp");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejected_spawn_handoff_kills_and_reaps_the_late_child() {
        // The supervisor gave up before the child existed: the rendezvous
        // handoff fails with the child in the spawn thread, which must kill
        // and reap it instead of letting `Child::drop` leak a live process.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exec sleep 30");
        let (tx, rx) = mpsc::sync_channel(0);
        drop(rx);
        let contained = spawn_and_handoff(command, tx);
        let pid = contained.expect("the rejected child must be contained");
        assert!(
            !process_exists(pid),
            "the rejected late child must be killed and reaped (pid {pid})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_cap_kills_and_reaps_instead_of_dropping() {
        let mut runtime = HelperRuntime::default();
        for _ in 0..MAX_ABANDONED_CHILDREN {
            let child = Command::new("/bin/sh")
                .arg("-c")
                .arg("exec sleep 30")
                .spawn()
                .expect("abandoned child");
            assert_eq!(
                runtime.track_abandoned(child),
                AbandonedOutcome::Retained,
                "the set must fill up to its fixed cap"
            );
        }
        let extra = Command::new("/bin/sh")
            .arg("-c")
            .arg("exec sleep 30")
            .spawn()
            .expect("extra child");
        assert_eq!(
            runtime.track_abandoned(extra),
            AbandonedOutcome::Reaped,
            "a child past the cap must be killed and reaped, never dropped live"
        );
        assert_eq!(runtime.abandoned.len(), MAX_ABANDONED_CHILDREN);
        for mut child in runtime.abandoned.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Whether a recorded pid is still visible to the OS.
    #[cfg(target_os = "linux")]
    fn process_exists(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_exists(pid: u32) -> bool {
        Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[test]
    fn drop_is_bounded_while_a_helper_is_hung_on_an_unread_pipe() {
        // Production-path bounded Drop: the monitor is inside the helper
        // supervisor when the handle is dropped. Drop joins the monitor but
        // must return within the deadline plus the bounded reap grace.
        let (reader, writer) = nix::unistd::pipe().expect("OS pipe");
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(30),
            Box::new(move |line| {
                let _ = entered_tx.send(());
                let started = Instant::now();
                let cloned = writer.try_clone().expect("clone pipe writer");
                let mut runtime = HelperRuntime::default();
                supervise_helper(HelperRun {
                    command: shell_helper("head -c 1000000 /dev/zero >&2"),
                    line,
                    deadline: started + Duration::from_millis(120),
                    stderr: Stdio::from(cloned),
                    runtime: &mut runtime,
                    cleanup: None,
                })
            }),
        );
        watchdog.enter(Phase::RuntimeTick);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("monitor must enter the helper supervisor");
        let started = Instant::now();
        drop(watchdog);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Drop must be bounded under a hung helper, got {:?}",
            started.elapsed()
        );
        drop(reader);
    }
}
