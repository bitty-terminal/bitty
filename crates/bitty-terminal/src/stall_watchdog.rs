//! Bounded main-loop hang watchdog (issues #2483, #2830).
//!
//! Long-running sessions can stop answering while the window stays mapped:
//! the main (winit) thread is parked inside one phase, no further events are
//! consumed, and the compositor reports `Application Not Responding`. The
//! owner's report left no process behind, so this module exists to make the
//! next occurrence self-describing.
//!
//! bitty#2830 made the diagnostic built in instead of opt-in: release builds
//! arm the monitor by default, a single blocked tick or command writes one
//! report, and `SIGUSR1` captures the same evidence from a process that still
//! looks idle because winit is parked in `ControlFlow::Wait`.
//!
//! Design constraints:
//!
//! - **Observability only.** The watchdog never changes routing, timing, or
//!   state. It marks phases and, on a stall, writes a bounded report.
//! - **Default-on in release, opt-in in debug.** `BITTY_HANG_WATCHDOG=0`
//!   disables it. Debug builds default to off so unit/CI runs never write
//!   diagnostics unless a developer asks for them; `BITTY_HANG_WATCHDOG=1`
//!   enables the same monitor there. The legacy `BITTY_STALL_WATCHDOG` name
//!   remains an accepted alias. A threshold variable only selects the bound;
//!   it never overrides an explicit off switch or the debug-build default.
//! - **Named bounds.** [`HANG_WATCHDOG_TICK_BOUND`] and
//!   [`HANG_WATCHDOG_COMMAND_BOUND`] cap the generic bound for the two phases
//!   that have caused the reported freezes: `Runtime::tick` and the cold
//!   command pump. Every other phase uses the configured
//!   [`HANG_WATCHDOG_DEFAULT_BOUND`] (or the operator's threshold) unchanged,
//!   so raising `BITTY_HANG_WATCHDOG_MS` really raises the generic bound.
//! - **One-shot.** One continuous stall writes exactly one report; after the
//!   loop advances, one recovery line records how long it was wedged. A later
//!   stall reports again. While the monitor is running, `SIGUSR1` writes on
//!   demand (rate-limited to one per second) because the owner explicitly
//!   asked for that sample.
//! - **Every retained completed tick counts.** A `Runtime::tick` that
//!   outlived [`HANG_WATCHDOG_TICK_BOUND`] is a stall whether the monitor
//!   sampled it while blocked or only after it returned. The runtime keeps
//!   the last [`TICK_RING_CAPACITY`] `(sequence, duration)` records in a
//!   lock-free ring; the monitor drains them oldest first and writes one
//!   `phase=runtime-tick` report per over-bound tick with that measured
//!   duration, in bounded per-poll batches so a drained burst cannot delay
//!   the next in-flight sample; the remainder carries to the next poll.
//!   Pending records survive progress changes and are reported later if a
//!   poll cannot deliver them yet. A block the in-flight detector already
//!   reported is closed by its one recovery line and never reported twice.
//!   Records overwritten before a drain are named once by a
//!   `tick-ring: capacity=… missed_at_most=… seq=…` line in the next
//!   report, so
//!   overflow is visible instead of silent.
//! - **No hot-path work.** A phase mark is one relaxed atomic store plus a
//!   timestamp. The app publishes its bounded context sample at most twice a
//!   second, the runtime publishes lock-free gauges on every tick, and the
//!   monitor thread touches `/proc` only after a stall.
//! - **Bounded.** A fixed thread-row cap, a fixed report-byte cap, and one
//!   report per stall; repeated stalls cannot grow the log without bound. The
//!   latest report atomically replaces one XDG state slot
//!   (`bitty/stall-watchdog.log`) inside a disposable helper process that
//!   also owns the stderr copy: the previous slot stays intact unless a
//!   complete new report is renamed in, and a blocked stderr pipe is bounded
//!   by killing the helper, never by changing the application's descriptor
//!   flags.
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
//! - Release builds arm the monitor with the default 10 s bound. Debug builds
//!   start no monitor unless the switch asks for one.
//! - `BITTY_HANG_WATCHDOG=0` (or `false`/`off`/`no`) disables it; `1`
//!   (or `true`/`on`/`yes`) enables it (including in debug builds).
//! - `BITTY_HANG_WATCHDOG=<milliseconds>` enables it with that generic bound.
//! - `BITTY_HANG_WATCHDOG_MS=<milliseconds>` overrides the generic bound
//!   (clamped to `50..=600000`) but does not override an explicit off switch
//!   or the debug-build default.
//! - The legacy `BITTY_STALL_WATCHDOG` / `BITTY_STALL_WATCHDOG_MS` names are
//!   accepted as aliases for existing runbooks.
//! - `BITTY_HANG_WATCHDOG_ABORT=1` additionally aborts a debug build after a
//!   stall or no-present report (with a backtrace). Release builds never
//!   abort, and a `SIGUSR1` capture never aborts: the operator is inspecting
//!   a live process.
//! - `SIGUSR1` (Unix) asks a running monitor to write one report without
//!   exiting. The report action is installed only after the monitor thread
//!   spawned and released on shutdown, so an invocation that never starts a
//!   monitor (headless/test-mode, an explicit off switch, the debug-build
//!   default, or a failed thread spawn) installs nothing and keeps the
//!   kernel's default process-terminating action. `signal_hook` cannot
//!   restore `SIG_DFL` when the last action is removed - its registry
//!   dispatcher stays installed and would silently swallow the signal - so
//!   the first monitor also registers one bounded conditional-default action.
//!   It does nothing while a monitor is registered, and with no monitor it
//!   emulates the signal's default action, so `SIGUSR1` terminates the
//!   process again after the last monitor stopped. (Should that registration
//!   itself fail, one warning is logged and a later `SIGUSR1` is ignored.)
//! - The latest report atomically replaces `bitty/stall-watchdog.log` under
//!   the platform state directory (`XDG_STATE_HOME`, `HOME/.local/state`, or
//!   `LOCALAPPDATA`). Delivery outcomes are mirrored to
//!   `bitty/stall-watchdog-delivery.log` in the same directory.
//!
//! The idle state is explicit: after the event loop arms `ControlFlow::Wait`
//! the app calls [`StallWatchdog::idle`], so a quiet window is never reported
//! as a stall. The monitor only fires while a phase is active and the
//! heartbeat has not advanced past that phase's bound.
//!
//! ## Parked work and the no-present detector (bitty#2830)
//!
//! bitty#2830 has two shapes that a phase heartbeat cannot see. First, a park
//! with a backlog: the loop sits in `ControlFlow::Wait` while bytes are still
//! queued in the production PTY writer FIFO, query replies are unwritten, or
//! the surface-visibility gate holds retained damage. [`PendingWork`] reads
//! those three bits from the lock-free runtime gauges, and a park with any of
//! them stays stall-eligible instead of counting as a quiet window. (The
//! `pty` bit names the queued writer FIFO; the headless observation buffer is
//! a test-only seam and is not reported as production backlog.) Second, a
//! surface that keeps parsing output but never paints (`trigger=no-present`):
//! the presented-frame counter is independent of the heartbeat, so a hidden
//! surface with retained damage that stops presenting is reported even though
//! progress keeps advancing.
//!
//! Every report carries `  parked: pending=<bits> present_stale_ms=<ms>
//! trigger=<reason>`, where `pending` is one of `hidden-damage`, `pty`,
//! `replies`, a `|`-joined subset, or `none`.
//!
//! A long workspace switch-away with a pane that keeps printing looks the same
//! to this detector as the reported shape, so repeats are bounded to one
//! report per [`REPORT_REPEAT_INTERVAL`]; the bounded repair wake is what keeps
//! the loop alive in both cases.
//!
//! ## One trigger: `SIGUSR1`
//!
//! The freeze campaign that produced this diagnostic used an out-of-band
//! dump-request file (`bitty/stall-watchdog.dump.req` polled by the monitor)
//! because it was a separate patch. This crate is `#![forbid(unsafe_code)]`,
//! so it cannot install an async-signal-safe handler itself; `signal_hook`
//! wraps the one unsafe registration, and the monitor thread owns the flag.
//! Given that, this build keeps the single `SIGUSR1` capture: it needs no new
//! file or watcher, cannot be spoofed by another local process writing a slot,
//! and works from a process that looks idle. The report action is registered
//! by [`StallWatchdog::start`] only after the monitor thread spawned and
//! released on shutdown, so the capture exists exactly while a monitor exists:
//! release builds by default, debug builds only with an explicit enable value,
//! and never after `BITTY_HANG_WATCHDOG=0` or in headless/test-mode
//! invocations, which never construct the app. Because the registry dispatcher
//! cannot be uninstalled, the first monitor also keeps one conditional-default
//! action registered: while a monitor runs it does nothing, and once none is
//! registered it emulates the default action, so an operator's `SIGUSR1`
//! terminates the process instead of being silently swallowed. The campaign's
//! parked-work bitset and no-present semantics are kept; only the trigger
//! differs.

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
use bitty_runtime::hang_watchdog::{
    HangGaugeSnapshot, HangGauges, TICK_RING_CAPACITY, TickGap, TickRingDrain,
};

/// Environment switch for the watchdog (bitty#2830).
pub(crate) const HANG_WATCHDOG_ENV: &str = "BITTY_HANG_WATCHDOG";
/// Environment override for the generic hang bound in milliseconds.
pub(crate) const HANG_WATCHDOG_THRESHOLD_ENV: &str = "BITTY_HANG_WATCHDOG_MS";
/// Legacy (pre-#2830) environment switch, kept as an alias for runbooks.
pub(crate) const LEGACY_WATCHDOG_ENV: &str = "BITTY_STALL_WATCHDOG";
/// Legacy (pre-#2830) threshold override, kept as an alias.
pub(crate) const LEGACY_WATCHDOG_THRESHOLD_ENV: &str = "BITTY_STALL_WATCHDOG_MS";
/// Opt-in debug abort after a report (with a backtrace).
pub(crate) const HANG_WATCHDOG_ABORT_ENV: &str = "BITTY_HANG_WATCHDOG_ABORT";

/// Generic no-progress bound for an arbitrary main-loop phase.
///
/// Ten seconds is long enough that a loaded but healthy desktop never trips it
/// and short enough that the owner still has a live process to inspect.
pub(crate) const HANG_WATCHDOG_DEFAULT_BOUND: Duration = Duration::from_secs(10);
/// Tighter bound for one `Runtime::tick` (the reported freeze stage).
///
/// A healthy frame is milliseconds; three seconds means the compositor has
/// already shown the window as unresponsive.
pub(crate) const HANG_WATCHDOG_TICK_BOUND: Duration = Duration::from_secs(3);
/// Bound for the cold plugin-command pump.
///
/// A single command may legitimately run for several seconds under its own
/// RC-1 budget; ten seconds names a callback that outlived its executor
/// deadline.
pub(crate) const HANG_WATCHDOG_COMMAND_BOUND: Duration = Duration::from_secs(10);
/// Default no-progress threshold before bitty#2830 tightened single phases.
pub(crate) const DEFAULT_STALL_THRESHOLD: Duration = HANG_WATCHDOG_DEFAULT_BOUND;
/// Smallest accepted threshold; below this normal cold ticks look like stalls.
const MIN_STALL_THRESHOLD: Duration = Duration::from_millis(50);
/// Largest accepted threshold; keeps the monitor useful after long idles.
const MAX_STALL_THRESHOLD: Duration = Duration::from_secs(600);
/// Monitor sampling period at the default threshold.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Minimum sampling period; small thresholds still get several samples.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Maximum completed-tick reports delivered per monitor poll.
///
/// One delivery can spend the helper's bounded 250 ms deadline (plus a 1 s
/// evidence-lock wait on the fallback path). Draining a whole ring of 16
/// serially could keep the monitor away from the next in-flight sample for
/// roughly twenty seconds, so the rest stay pending for the next poll. One
/// report is still written per completed episode.
const MAX_COMPLETED_REPORTS_PER_POLL: usize = 4;
/// Minimum interval between two `SIGUSR1`-requested dumps.
#[cfg(unix)]
const SIGNAL_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum interval between two parked-work reports for one continuous
/// backlog, and between two `no-present` reports for one continuous
/// hidden-with-retained-damage span (bitty#2830).
///
/// Both states are otherwise invisible, so they must be reported; the bound
/// keeps a legitimately long workspace switch-away (a hidden pane still
/// printing) or a slow-but-live child from writing a report on every poll.
const REPORT_REPEAT_INTERVAL: Duration = Duration::from_secs(60);
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
pub(crate) const STALL_REPORT_PREFIX: &str = "bitty: main-loop stall detected";
/// Prefix of a `SIGUSR1` capture line; also overwrites the state slot.
const SIGNAL_REPORT_PREFIX: &str = "bitty: main-loop hang dump (SIGUSR1)";
/// Maximum thread rows included in one report.
///
/// Linux-only: the sampler reads `/proc/self/task`; other targets render an
/// empty thread list, so the row cap has no use there.
#[cfg(target_os = "linux")]
const MAX_THREAD_ROWS: usize = 64;
/// Maximum bytes in one report (hard cap after rendering).
const MAX_REPORT_BYTES: usize = 16 * 1024;
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
    /// Whether the embedder's surface-visibility gate currently allows
    /// presentation (issues #2589/#2570). A stall with `true` names the
    /// commit-while-occluded path the gate exists to close; `false` names a
    /// stall behind the gate, where the reveal edge never arrived.
    pub(crate) surface_visible: bool,
    /// Age of the oldest unanswered `request_redraw` at sample time (issue
    /// #2570). Large spans name a compositor that stopped answering the
    /// redraw probe; `None` means no request was outstanding.
    pub(crate) redraw_pending_ms: Option<u64>,
    /// Whether the redraw probe has ever observed a served frame callback.
    pub(crate) probe_redraw_seen: bool,
    /// Whether the synthetic/native occlusion edge is currently delivered.
    pub(crate) probe_occluded: bool,
}

/// Parsed watchdog configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchdogConfig {
    threshold: Duration,
}

impl WatchdogConfig {
    /// Default configuration (10 s generic bound).
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

    /// Effective no-progress bound for one phase.
    ///
    /// The tighter per-phase bounds are the bitty#2830 contract: a tick or a
    /// cold command that outlives them is a hang even when the generic bound
    /// has not elapsed. They are caps, not a second default: every other
    /// phase uses the configured threshold unchanged, so an operator can
    /// raise the generic bound (`BITTY_HANG_WATCHDOG_MS`) as well as tighten
    /// it. A shorter operator threshold still wins over a per-phase cap.
    fn bound_for(&self, phase: Phase) -> Duration {
        match phase {
            Phase::RuntimeTick => self.threshold.min(HANG_WATCHDOG_TICK_BOUND),
            Phase::ColdCommands => self.threshold.min(HANG_WATCHDOG_COMMAND_BOUND),
            _ => self.threshold,
        }
    }
}

/// Parse the enable switch and optional threshold.
///
/// Pure so the environment contract is testable without touching the process
/// environment. `default_on` is the build default the caller selected (see
/// [`default_enabled`]); it is an explicit input so tests can pin both build
/// profiles without re-deriving `cfg!(debug_assertions)`. `enable = None`
/// means the build default; an explicit off value always wins. A numeric
/// enable value selects the generic bound. An unparseable value leaves the
/// watchdog disabled (fail-quiet: diagnostics never surprise CI).
pub(crate) fn config_from_values(
    enable: Option<&str>,
    threshold: Option<&str>,
    default_on: bool,
) -> Option<WatchdogConfig> {
    let enabled = match enable.map(str::trim) {
        None | Some("") => default_on,
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

/// Product policy: release builds arm the watchdog without any switch.
///
/// One named constant instead of a literal inside [`default_enabled`], so the
/// release decision is pinned by a test that the debug profile CI run also
/// executes: silently flipping the release default to off fails CI.
pub(crate) const RELEASE_DEFAULT_ON: bool = true;

/// Whether this build arms the watchdog without an environment switch.
///
/// Release is the owner-facing product build and must capture the next hang
/// without anyone predicting it. Debug is a developer/CI build where a
/// deliberately slow test could otherwise write a state slot; `BITTY_HANG_WATCHDOG=1`
/// opts in there for reproduction. This is the only place the build profile is
/// read; the decision itself is [`RELEASE_DEFAULT_ON`].
const fn default_enabled() -> bool {
    if cfg!(debug_assertions) {
        false
    } else {
        RELEASE_DEFAULT_ON
    }
}

/// Raw enable switch from either accepted variable.
fn environment_enable_value() -> Option<String> {
    std::env::var(HANG_WATCHDOG_ENV)
        .ok()
        .or_else(|| std::env::var(LEGACY_WATCHDOG_ENV).ok())
}

/// Raw threshold override from either accepted variable.
fn environment_threshold_value() -> Option<String> {
    std::env::var(HANG_WATCHDOG_THRESHOLD_ENV)
        .ok()
        .or_else(|| std::env::var(LEGACY_WATCHDOG_THRESHOLD_ENV).ok())
}

/// Whether a debug build should abort after writing the report.
fn abort_after_report() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    std::env::var(HANG_WATCHDOG_ABORT_ENV).is_ok_and(|value| {
        !(value.eq_ignore_ascii_case("0")
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("off")
            || value.eq_ignore_ascii_case("no"))
    })
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

/// The sample a diagnostic report is rendered for.
#[derive(Clone, Copy, Debug)]
struct ReportSample {
    /// Phase the report must name; the detector's phase, not a later read.
    phase: Phase,
    /// Age of the heartbeat the detector decided on (a completed tick names
    /// its measured duration).
    idle: Duration,
    /// Completed-tick ring span lost to overwrite since the previous report.
    tick_gap: Option<TickGap>,
}

/// One rendered stall report, built on the monitor thread only.
struct StallReport {
    reason: DiagnosticReason,
    threshold: Duration,
    idle: Duration,
    phase: Phase,
    app_uptime: Duration,
    progress: u64,
    context: Option<WatchdogContext>,
    /// Lock-free runtime gauges sampled at dump time (bitty#2830).
    gauges: HangGaugeSnapshot,
    threads: Vec<ThreadRow>,
    totals: ProcessTotals,
    /// Outcomes of earlier report deliveries, shown in-band so a lost evidence
    /// write is visible in the next report that does get through.
    delivery: DeliverySummary,
    /// Parked work that was outstanding at report time (bitty#2830).
    pending: PendingWork,
    /// How long the presented-frame counter had been stuck at report time.
    present_stale_ms: Option<u64>,
    /// Completed-tick ring span lost before it could be reported.
    tick_gap: Option<TickGap>,
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
/// tests of the one-shot contract.
#[derive(Debug, Default)]
struct MonitorState {
    last_progress: u64,
    stall_started: Option<Instant>,
    reported: bool,
    /// Highest completed-tick sequence consumed from the runtime ring.
    last_tick_seq: u64,
    /// Over-bound completed ticks drained but not yet reported, oldest first.
    ///
    /// Bounded by [`TICK_RING_CAPACITY`]: one ring drain can add at most that
    /// many entries, and they are reported on the same poll. Any overflow is
    /// named by `tick_gap`, never queued.
    pending_ticks: VecDeque<CompletedTick>,
    /// Sequence of the tick that completes the active in-flight report.
    ///
    /// Set when an in-flight `runtime-tick` report is emitted. The completing
    /// tick is the same stall and must not be reported again.
    completion_seq: Option<u64>,
    /// Completed-tick ring span lost to overwrite since the last report.
    tick_gap: Option<TickGap>,
    /// Last sampled presented-frame counter (bitty#2830).
    last_presented: u64,
    /// When `last_presented` last moved; the anchor for `present_stale_ms`.
    presented_at: Option<Instant>,
    /// Last `no-present` report, for the repeat bound.
    no_present_at: Option<Instant>,
    /// Last stall report caused by parked work, for the repeat bound. A
    /// parked loop keeps marking progress (`AboutToWait`), so the one-shot
    /// contract alone would re-report every threshold.
    parked_at: Option<Instant>,
    /// Last stall/no-present report, so the two detectors do not describe the
    /// same wedge twice. A phase stall also freezes the presented counter;
    /// reporting both in one poll (or one poll apart) is duplicate evidence.
    reported_at: Option<Instant>,
}

/// One completed tick waiting for its own report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletedTick {
    /// Runtime sequence number (unique per process run).
    seq: u64,
    /// Measured duration of that tick.
    duration: Duration,
}

/// Widens `slot` to cover `gap`; the next report names one missing span.
fn merge_tick_gap(slot: &mut Option<TickGap>, gap: TickGap) {
    *slot = Some(match *slot {
        Some(existing) => TickGap {
            from: existing.from.min(gap.from),
            to: existing.to.max(gap.to),
        },
        None => gap,
    });
}

/// One phase-heartbeat sample: the phase, how long its heartbeat has been
/// still, and the bound that applies to it.
#[derive(Clone, Copy, Debug)]
struct StallSample {
    phase: Phase,
    idle: Duration,
    threshold: Duration,
}

/// What the monitor decided after one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorAction {
    /// Nothing to emit.
    None,
    /// Emit the one report for this continuous in-flight stall (`idle` old).
    ///
    /// `phase` is the sampled phase that stalled.
    Report { idle: Duration, phase: Phase },
    /// The loop made progress again after a reported stall.
    Recovered { stalled_for: Duration },
}

impl MonitorState {
    /// Starts the detector with the first sample already taken.
    fn new(now: Instant, progress: u64, presented: u64, tick_seq: u64) -> Self {
        Self {
            last_progress: progress,
            stall_started: None,
            reported: false,
            last_tick_seq: tick_seq,
            pending_ticks: VecDeque::with_capacity(TICK_RING_CAPACITY),
            completion_seq: None,
            tick_gap: None,
            last_presented: presented,
            presented_at: Some(now),
            no_present_at: None,
            parked_at: None,
            reported_at: None,
        }
    }

    /// Drains completed ticks above `bound` into the pending report queue.
    ///
    /// Called once per poll before [`Self::evaluate`]. Every retained ring
    /// record is consumed; only an over-bound duration becomes a pending
    /// report. Pending entries are kept oldest first and reported on this or a
    /// later poll, independent of the phase heartbeat, so a progress change
    /// cannot discard them. The tick that completes an in-flight report is
    /// matched and dropped here: the in-flight report is already its evidence.
    fn accept_tick_drain(&mut self, drain: TickRingDrain, bound: Duration) {
        if let Some(gap) = drain.gap() {
            merge_tick_gap(&mut self.tick_gap, gap);
        }
        for record in drain.records() {
            if self.completion_seq == Some(record.seq) {
                self.completion_seq = None;
                continue;
            }
            let duration = Duration::from_nanos(record.duration_ns);
            if duration < bound {
                continue;
            }
            if self.pending_ticks.len() >= TICK_RING_CAPACITY {
                // One drain cannot exceed the ring capacity; this guard only
                // keeps a pathological backlog bounded and names the dropped
                // evidence instead of growing the monitor.
                merge_tick_gap(
                    &mut self.tick_gap,
                    TickGap {
                        from: record.seq,
                        to: record.seq,
                    },
                );
                continue;
            }
            self.pending_ticks.push_back(CompletedTick {
                seq: record.seq,
                duration,
            });
        }
        self.last_tick_seq = drain.last_seq();
        // The completion of a reported stall can be overwritten before it is
        // drained; the gap line is the evidence then, so stop waiting for it.
        if self
            .completion_seq
            .is_some_and(|seq| seq <= self.last_tick_seq)
        {
            self.completion_seq = None;
        }
    }

    /// Marks a just-emitted in-flight report so its completing tick is not
    /// reported a second time.
    fn note_in_flight_report(&mut self, phase: Phase) {
        if phase == Phase::RuntimeTick {
            self.completion_seq = Some(self.last_tick_seq.saturating_add(1));
        }
    }

    /// Takes the oldest completed tick still waiting for its report.
    ///
    /// A completed-tick report names the same stopped frame as the no-present
    /// detector, so it suppresses that duplicate line for one repeat interval.
    fn next_completed_report(&mut self, now: Instant) -> Option<CompletedTick> {
        let tick = self.pending_ticks.pop_front()?;
        self.reported_at = Some(now);
        Some(tick)
    }

    /// Pops up to [`MAX_COMPLETED_REPORTS_PER_POLL`] completed ticks, oldest
    /// first.
    ///
    /// The remainder stays queued so a drained burst cannot keep the monitor
    /// from sampling the next in-flight stall for the whole delivery budget.
    fn take_completed_batch(
        &mut self,
        now: Instant,
    ) -> [Option<CompletedTick>; MAX_COMPLETED_REPORTS_PER_POLL] {
        let mut batch = [None; MAX_COMPLETED_REPORTS_PER_POLL];
        for slot in &mut batch {
            *slot = self.next_completed_report(now);
            if slot.is_none() {
                break;
            }
        }
        batch
    }

    /// Takes the ring-overflow span for the next report.
    fn take_tick_gap(&mut self) -> Option<TickGap> {
        self.tick_gap.take()
    }

    /// Tracks the presented-frame counter and returns how long it has been
    /// stuck (bitty#2830 no-present detector).
    fn sample_presented(&mut self, now: Instant, presented: u64) -> Duration {
        if presented != self.last_presented || self.presented_at.is_none() {
            self.last_presented = presented;
            self.presented_at = Some(now);
            // A presented frame proves the no-present shape ended, so a later
            // wedge may report immediately instead of waiting out the repeat
            // bound meant for the previous one.
            self.reported_at = None;
            return Duration::ZERO;
        }
        now.saturating_duration_since(self.presented_at.unwrap_or(now))
    }

    /// Decides whether the stuck frame counter is a reportable state.
    ///
    /// Only a hidden surface that retains damage qualifies: an idle terminal
    /// legitimately presents nothing, and a visible surface with damage has
    /// its own bounded retry wake. The repeat bound keeps a legitimately long
    /// workspace switch-away from writing more than one line per interval.
    /// A stall report already covers the same sample (a wedged phase also
    /// freezes the frame counter), so the no-present line is suppressed for
    /// one repeat interval after any report.
    fn no_present_due(
        &mut self,
        now: Instant,
        stale: Duration,
        hidden_damage: bool,
        threshold: Duration,
    ) -> Option<Duration> {
        if !hidden_damage || stale < threshold {
            return None;
        }
        if self
            .reported_at
            .is_some_and(|last| now.saturating_duration_since(last) < REPORT_REPEAT_INTERVAL)
        {
            return None;
        }
        if self
            .no_present_at
            .is_some_and(|last| now.saturating_duration_since(last) < REPORT_REPEAT_INTERVAL)
        {
            return None;
        }
        self.no_present_at = Some(now);
        self.reported_at = Some(now);
        Some(stale)
    }

    /// Evaluate one phase-heartbeat sample.
    ///
    /// `parked` marks outstanding work published at `ControlFlow::Wait`: a
    /// park with a backlog is not a quiet window, so it stays stall-eligible
    /// even though the app called [`StallWatchdog::idle`] (bitty#2830).
    ///
    /// Completed ticks are not decided here. [`Self::accept_tick_drain`]
    /// queues them independently, so a progress change cannot discard an
    /// over-bound tick (bitty#2830 review F1/F4).
    fn evaluate(
        &mut self,
        now: Instant,
        progress: u64,
        active: bool,
        sample: StallSample,
        parked: bool,
    ) -> MonitorAction {
        let StallSample {
            phase,
            idle,
            threshold,
        } = sample;
        let active = active || parked;
        if progress != self.last_progress {
            self.last_progress = progress;
            let recovered = self
                .stall_started
                .take()
                .map(|started| now.saturating_duration_since(started));
            let had_report = std::mem::replace(&mut self.reported, false);
            return match recovered {
                Some(stalled_for) if had_report => MonitorAction::Recovered { stalled_for },
                _ => MonitorAction::None,
            };
        }
        if !active {
            // Idle is expected: no stall, and a threshold crossing that ended
            // without a report leaves no recovery line.
            self.stall_started = None;
            self.reported = false;
            return MonitorAction::None;
        }
        if idle < threshold {
            return MonitorAction::None;
        }
        let _ = self.stall_started.get_or_insert(now);
        if self.reported {
            // One-shot: a continuous stall never writes a second report.
            return MonitorAction::None;
        }
        if parked
            && self
                .parked_at
                .is_some_and(|last| now.saturating_duration_since(last) < REPORT_REPEAT_INTERVAL)
        {
            // A parked loop keeps marking progress, which resets the one-shot
            // flag; bound the repeats for the same backlog instead.
            self.reported = true;
            return MonitorAction::None;
        }
        self.reported = true;
        if parked {
            self.parked_at = Some(now);
        }
        // Suppress the no-present detector for this sample and the next
        // repeat interval: a wedged phase already named the same stopped
        // frame, so two lines for one wedge would be duplicate evidence.
        self.reported_at = Some(now);
        MonitorAction::Report { idle, phase }
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
    /// Lock-free runtime gauges published on every tick (bitty#2830).
    gauges: Arc<HangGauges>,
    /// Frames the app has presented (bitty#2830 no-present detector).
    ///
    /// The phase heartbeat advances on every parsed byte, so a hidden surface
    /// that never paints looks healthy to the stall detector. The presented
    /// counter is the independent signal: it only moves when a frame actually
    /// committed.
    presented_frames: AtomicU64,
    /// Completed monitor-loop polls (test observability for PB-7 0 Hz idle).
    #[cfg(test)]
    polls: AtomicU64,
    /// Set when the monitor computes `idle_park == true` and parks at 0 Hz.
    #[cfg(test)]
    idle_parked: AtomicBool,
}

impl Shared {
    fn new(gauges: Arc<HangGauges>) -> Self {
        Self {
            start: Instant::now(),
            last_beat_ns: AtomicU64::new(0),
            last_context_ns: AtomicU64::new(0),
            phase: AtomicU8::new(Phase::Startup as u8),
            active: AtomicBool::new(true),
            progress: AtomicU64::new(0),
            context: Mutex::new(WatchdogContext::default()),
            gauges,
            presented_frames: AtomicU64::new(0),
            #[cfg(test)]
            polls: AtomicU64::new(0),
            #[cfg(test)]
            idle_parked: AtomicBool::new(false),
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
/// Supports both bounded periodic polling (`Some(timeout)`) and 0 Hz idle
/// parking (`None`) per `PB-7`. On Unix, a non-blocking stream pair allows
/// both thread notifications (`notify`/`request`) and async-signal-safe
/// `SIGUSR1` delivery (`signal_hook::low_level::pipe`) to wake the monitor
/// from `poll(..., PollTimeout::NONE)`.
struct StopSignal {
    stopped: AtomicBool,
    stop_waiter: Mutex<()>,
    stop_wake: Condvar,
    waiter: Mutex<bool>,
    wake: Condvar,
    #[cfg(unix)]
    pipe: Option<(
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    )>,
}

impl StopSignal {
    fn new() -> Self {
        #[cfg(unix)]
        let pipe = std::os::unix::net::UnixStream::pair()
            .ok()
            .and_then(|(r, w)| {
                r.set_nonblocking(true).ok()?;
                w.set_nonblocking(true).ok()?;
                Some((r, w))
            });
        Self {
            stopped: AtomicBool::new(false),
            stop_waiter: Mutex::new(()),
            stop_wake: Condvar::new(),
            waiter: Mutex::new(false),
            wake: Condvar::new(),
            #[cfg(unix)]
            pipe,
        }
    }

    #[cfg(unix)]
    fn clone_wake_writer(&self) -> Option<std::os::unix::net::UnixStream> {
        self.pipe.as_ref().and_then(|(_, w)| w.try_clone().ok())
    }

    fn notify(&self) {
        let mut notified = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(unix)]
        if let Some((_, mut w)) = self.pipe.as_ref().map(|(r, w)| (r, w)) {
            let _ = std::io::Write::write(&mut w, &[1]);
        }
        *notified = true;
        self.wake.notify_all();
    }

    fn request(&self) {
        let stop_guard = self
            .stop_waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut notified = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stopped.store(true, Ordering::SeqCst);
        *notified = true;
        self.wake.notify_all();
        self.stop_wake.notify_all();
        drop(notified);
        drop(stop_guard);
        #[cfg(unix)]
        if let Some((_, mut w)) = self.pipe.as_ref().map(|(r, w)| (r, w)) {
            let _ = std::io::Write::write(&mut w, &[1]);
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn clear_wake_notifications(&self) {
        #[cfg(unix)]
        if let Some((r, _)) = &self.pipe {
            let mut buf = [0u8; 64];
            let mut reader = r;
            while std::io::Read::read(&mut reader, &mut buf).is_ok_and(|n| n > 0) {}
        }
        let mut notified = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *notified = false;
    }

    /// Periodic poll sleep: waits `timeout`, waking early only on `request()`.
    fn wait_poll(&self, timeout: Duration) -> bool {
        let guard = self
            .stop_waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_stopped() {
            return true;
        }
        let (guard, _) = self
            .stop_wake
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(guard);
        self.is_stopped()
    }

    /// 0 Hz idle park (`PB-7`): sleeps indefinitely until `notify()`, `SIGUSR1`,
    /// or `request()` arrives; returns `true` when stopped.
    fn wait_idle(&self) -> bool {
        #[cfg(unix)]
        if let Some((r, _)) = &self.pipe {
            use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
            use std::os::fd::AsFd;
            if self.is_stopped() {
                return true;
            }
            let mut fds = [PollFd::new(r.as_fd(), PollFlags::POLLIN)];
            let _ = poll(&mut fds, PollTimeout::NONE);
            self.clear_wake_notifications();
            return self.is_stopped();
        }
        let mut notified = self
            .waiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_stopped() {
            return true;
        }
        if !*notified {
            notified = self
                .wake
                .wait(notified)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *notified = false;
        drop(notified);
        self.is_stopped()
    }
}

/// Process-wide `SIGUSR1` wiring shared by every monitor instance (Unix).
///
/// `signal_hook`'s registry keeps its dispatcher installed after the last
/// action for a signal is removed, so `unregister` alone leaves `SIGUSR1`
/// silently ignored; it does not restore `SIG_DFL`. Restoring the kernel
/// disposition is not possible through the registry, so the first monitor also
/// registers one conditional-default action. That action reads a single shared
/// flag (no allocation, no lock) and emulates the signal's default action -
/// process termination by `SIGUSR1` - whenever no monitor is registered, and
/// does nothing at all while one is. The observable contract is therefore:
/// a capture while a monitor runs, termination otherwise, including after the
/// last monitor stopped.
///
/// The bookkeeping is serialized by one process-global mutex so concurrent
/// monitors cannot race the flag that decides between capture and
/// termination. The registry action itself never takes that mutex.
#[cfg(unix)]
mod sigusr1 {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use signal_hook::SigId;
    use signal_hook::consts::SIGUSR1;
    use signal_hook::flag as signal_flag;

    use crate::logging;

    /// Registry state for one process.
    struct Wiring {
        /// Monitors whose report action is registered; guarded by [`WIRING`].
        active: usize,
        /// `true` while no monitor is registered (read in signal context).
        idle: Arc<AtomicBool>,
        /// Whether the conditional-default action is registered.
        fallback: bool,
    }

    impl Wiring {
        fn new() -> Self {
            Self {
                active: 0,
                idle: Arc::new(AtomicBool::new(true)),
                fallback: false,
            }
        }
    }

    static WIRING: Mutex<Option<Wiring>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<Wiring>> {
        WIRING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) struct Registration {
        flag_id: SigId,
        pipe_id: Option<SigId>,
    }

    /// Register the report action after its monitor thread spawned and
    /// publish that a monitor is active.
    ///
    /// Returns `None` when the action could not be registered; the caller then
    /// keeps the monitor but loses only the capture, never the periodic
    /// reports.
    pub(super) fn install(
        flag: Arc<AtomicBool>,
        wake_pipe: Option<std::os::unix::net::UnixStream>,
    ) -> Option<Registration> {
        let mut guard = lock();
        let wiring = guard.get_or_insert_with(Wiring::new);
        // Publish "a monitor is starting" before the terminating action can
        // observe the signal: a `SIGUSR1` that races registration must never
        // kill a process whose monitor is being armed.
        wiring.idle.store(false, Ordering::SeqCst);
        if !wiring.fallback {
            match signal_flag::register_conditional_default(SIGUSR1, Arc::clone(&wiring.idle)) {
                Ok(_) => wiring.fallback = true,
                Err(error) => logging::warn(|| {
                    format!(
                        "bitty: SIGUSR1 default action unavailable after shutdown ({error}); \
                         a later SIGUSR1 would be ignored"
                    )
                }),
            }
        }
        match signal_flag::register(SIGUSR1, flag) {
            Ok(flag_id) => {
                let pipe_id = wake_pipe.and_then(|stream| {
                    signal_hook::low_level::pipe::register(SIGUSR1, stream).ok()
                });
                wiring.active = wiring.active.saturating_add(1);
                Some(Registration { flag_id, pipe_id })
            }
            Err(error) => {
                if wiring.active == 0 {
                    // No capture exists, so keep the default action armed.
                    wiring.idle.store(true, Ordering::SeqCst);
                }
                logging::warn(|| {
                    format!(
                        "bitty: SIGUSR1 hang capture unavailable ({error}); periodic watchdog remains armed"
                    )
                });
                None
            }
        }
    }

    /// Remove the report action and re-arm the default action when it was the
    /// last one.
    pub(super) fn release(reg: Registration) {
        let _ = signal_hook::low_level::unregister(reg.flag_id);
        if let Some(pipe_id) = reg.pipe_id {
            let _ = signal_hook::low_level::unregister(pipe_id);
        }
        let mut guard = lock();
        let Some(wiring) = guard.as_mut() else {
            return;
        };
        wiring.active = wiring.active.saturating_sub(1);
        if wiring.active == 0 {
            // Arm termination only after the last report action is gone:
            // while any monitor is registered, `SIGUSR1` must never terminate.
            wiring.idle.store(true, Ordering::SeqCst);
        }
    }

    /// Whether the bounded conditional-default action is registered.
    ///
    /// Test observability only: once installed it stays installed for the life
    /// of the process, so this is monotonic and safe to assert under a
    /// parallel harness.
    #[cfg(test)]
    pub(super) fn fallback_installed() -> bool {
        lock().as_ref().is_some_and(|wiring| wiring.fallback)
    }
}

/// Main-loop hang watchdog handle (issue #2483, built in by bitty#2830).
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
    /// `SIGUSR1` report registration; released on shutdown.
    ///
    /// The flag the action sets lives on the monitor thread. `None` means no
    /// action was registered (the monitor failed to spawn, or registration
    /// itself failed), so this handle owns nothing to release.
    #[cfg(unix)]
    signal_id: Option<sigusr1::Registration>,
    /// The monitor's `SIGUSR1` flag, retained for hermetic tests.
    ///
    /// Production keeps the flag inside the monitor thread; tests set it
    /// directly instead of raising a process signal, which would terminate
    /// the harness whenever no handler is installed.
    #[cfg(all(test, unix))]
    signal_flag: Arc<AtomicBool>,
    /// Set by the monitor thread right before it exits (test observability).
    #[cfg(test)]
    finished: Arc<AtomicBool>,
}

impl StallWatchdog {
    /// Start the watchdog from the environment.
    ///
    /// Release builds start it by default. Debug builds start it only when
    /// `BITTY_HANG_WATCHDOG=1` (or another enable value); an explicit
    /// `BITTY_HANG_WATCHDOG=0` disables it everywhere. The `SIGUSR1` capture is
    /// registered by [`Self::start`] only after the monitor thread spawned, so
    /// it exists only while a monitor runs. Unit-test binaries return `None`
    /// unless the switch opts in, so CI runs spawn no monitor per app
    /// instance.
    pub(crate) fn from_env(gauges: Arc<HangGauges>) -> Option<Self> {
        // Unit-test binaries construct many apps; without an explicit opt-in
        // they neither need periodic reports nor a `SIGUSR1` action per
        // instance. The guard runs before any thread or registration exists.
        let enable = environment_enable_value();
        if cfg!(test) && enable.is_none() {
            return None;
        }
        Self::from_values(
            enable.as_deref(),
            environment_threshold_value().as_deref(),
            default_enabled(),
            gauges,
        )
    }

    /// Start from explicit inputs instead of the process environment.
    ///
    /// This is the decision core of [`Self::from_env`] with the build default
    /// passed in, so tests exercise the debug and release defaults in the same
    /// profile without reading or mutating the environment. `None` means no
    /// monitor exists: no thread, no registration.
    fn from_values(
        enable: Option<&str>,
        threshold: Option<&str>,
        default_on: bool,
        gauges: Arc<HangGauges>,
    ) -> Option<Self> {
        // `None` covers an explicit off switch and an off build default:
        // neither installs a monitor thread nor a `SIGUSR1` action.
        let config = config_from_values(enable, threshold, default_on)?;
        let slot = StateSlot::from_env();
        let mut sink = ReportSink::new(slot, Box::new(HelperDelivery::current()));
        Some(Self::start(
            config,
            gauges,
            Box::new(move |line| sink.emit(line)),
        ))
    }

    /// Start a watchdog with an explicit configuration and sink.
    fn start(
        config: WatchdogConfig,
        gauges: Arc<HangGauges>,
        mut sink: Box<dyn FnMut(&str) -> DeliveryOutcome + Send>,
    ) -> Self {
        let shared = Arc::new(Shared::new(gauges));
        let stop = Arc::new(StopSignal::new());
        let wake_stop = Arc::clone(&stop);
        shared.gauges.set_waker(Some(Arc::new(move || {
            wake_stop.notify();
        })));
        #[cfg(unix)]
        let signal_flag = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let finished = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        #[cfg(unix)]
        let thread_signal = Arc::clone(&signal_flag);
        #[cfg(all(test, unix))]
        let test_signal = Arc::clone(&signal_flag);
        #[cfg(test)]
        let thread_finished = Arc::clone(&finished);
        let poll_interval = std::cmp::max(
            MIN_POLL_INTERVAL,
            std::cmp::min(DEFAULT_POLL_INTERVAL, config.threshold / 4),
        );
        let spawn = std::thread::Builder::new()
            .name(String::from("bitty-stall-watchdog"))
            .spawn(move || {
                monitor_loop(
                    &thread_shared,
                    &thread_stop,
                    config,
                    #[cfg(unix)]
                    &thread_signal,
                    poll_interval,
                    &mut sink,
                );
                #[cfg(test)]
                thread_finished.store(true, Ordering::SeqCst);
            });
        let monitor = match spawn {
            Ok(handle) => Some(handle),
            Err(error) => {
                // Nothing is registered yet, so a failed spawn leaves
                // `SIGUSR1` exactly as it was: the kernel default when no
                // monitor ever ran, or the conditional default owned by an
                // earlier monitor. No cleanup is needed here by design.
                shared.gauges.set_waker(None);
                logging::warn(|| {
                    format!(
                        "bitty: hang watchdog thread unavailable ({error}); diagnostics stay off"
                    )
                });
                #[cfg(test)]
                finished.store(true, Ordering::SeqCst);
                None
            }
        };
        // bitty#2862: register only once a monitor thread exists to consume the
        // flag. A capture without a monitor would be a dead registration, and
        // removing one does not restore the default action.
        #[cfg(unix)]
        let signal_id = if monitor.is_some() {
            sigusr1::install(Arc::clone(&signal_flag), stop.clone_wake_writer())
        } else {
            None
        };
        Self {
            shared,
            stop,
            monitor,
            #[cfg(unix)]
            signal_id,
            #[cfg(all(test, unix))]
            signal_flag: test_signal,
            #[cfg(test)]
            finished,
        }
    }

    /// Mark the loop active in `phase` and advance the heartbeat.
    pub(crate) fn enter(&self, phase: Phase) {
        let was_active = self.shared.active.load(Ordering::Relaxed);
        self.shared.mark(phase, true);
        if !was_active {
            self.stop.notify();
        }
    }

    /// Publish the app's presented-frame counter (bitty#2830).
    ///
    /// One relaxed store per present; the monitor's no-present detector reads
    /// it without touching app state. `presented` is monotonic, so a stale
    /// value is exactly the diagnostic: the loop kept running while no frame
    /// committed.
    pub(crate) fn presented(&self, frames: u64) {
        self.shared
            .presented_frames
            .store(frames, Ordering::Relaxed);
    }

    /// Mark the loop idle (armed `ControlFlow::Wait`): not a stall.
    pub(crate) fn idle(&self) {
        let was_active = self.shared.active.load(Ordering::Relaxed);
        self.shared.mark(Phase::Idle, false);
        if was_active {
            self.stop.notify();
        }
    }

    /// Simulate one `SIGUSR1` receipt for hermetic monitor tests.
    ///
    /// The production handler sets the same flag through `signal-hook`; this
    /// seam never raises a process signal, which would terminate the harness
    /// whenever no handler is installed.
    #[cfg(all(test, unix))]
    fn simulate_signal_receipt(&self) {
        self.signal_flag.store(true, Ordering::SeqCst);
        self.stop.notify();
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
        self.shared.gauges.set_waker(None);
        self.stop.request();
        // Join before releasing: while the monitor is still winding down, a
        // `SIGUSR1` must continue to reach the handler instead of taking the
        // terminating path.
        let result = match self.monitor.take() {
            Some(handle) => handle.join(),
            None => Ok(()),
        };
        #[cfg(unix)]
        if let Some(id) = self.signal_id.take() {
            sigusr1::release(id);
        }
        result
    }

    /// Start the production monitor with an injected sink (app wiring tests).
    ///
    /// The app-level regressions replace the delivery sink so a report is
    /// observed directly; the monitor thread, phase shared state, and tick
    /// gauges stay exactly the production path.
    #[cfg(test)]
    pub(crate) fn start_for_test(
        threshold_ms: u64,
        gauges: Arc<HangGauges>,
        sink: Box<dyn FnMut(&str) -> DeliveryOutcome + Send>,
    ) -> Self {
        Self::start(
            WatchdogConfig::with_threshold_ms(threshold_ms),
            gauges,
            sink,
        )
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
    config: WatchdogConfig,
    #[cfg(unix)] signal_flag: &AtomicBool,
    poll_interval: Duration,
    sink: &mut (dyn FnMut(&str) -> DeliveryOutcome + Send),
) {
    let initial_gauges = shared.gauges.snapshot();
    let mut state = MonitorState::new(
        Instant::now(),
        shared.progress.load(Ordering::Relaxed),
        shared.presented_frames.load(Ordering::Relaxed),
        initial_gauges.tick_count,
    );
    let mut delivery = DeliverySummary::default();
    #[cfg(unix)]
    let mut last_signal_at: Option<Instant> = None;
    let mut idle_park = false;
    loop {
        if idle_park {
            #[cfg(test)]
            shared.idle_parked.store(true, Ordering::Release);
            if stop.wait_idle() {
                return;
            }
            #[cfg(test)]
            shared.idle_parked.store(false, Ordering::Release);
            #[cfg(unix)]
            let urgent_signal = signal_flag.load(Ordering::SeqCst);
            #[cfg(not(unix))]
            let urgent_signal = false;
            if !urgent_signal && stop.wait_poll(poll_interval) {
                return;
            }
        } else if stop.wait_poll(poll_interval) {
            return;
        }
        stop.clear_wake_notifications();
        #[cfg(test)]
        shared.polls.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        #[cfg(unix)]
        if signal_flag.swap(false, Ordering::SeqCst) {
            let due = last_signal_at
                .is_none_or(|last| now.saturating_duration_since(last) >= SIGNAL_REPORT_INTERVAL);
            if due && !stop.is_stopped() {
                last_signal_at = Some(now);
                let stale =
                    state.sample_presented(now, shared.presented_frames.load(Ordering::Relaxed));
                let sampled_phase = Phase::from_u8(shared.phase.load(Ordering::Relaxed));
                let report = build_report(
                    shared,
                    config,
                    DiagnosticReason::Signal,
                    ReportSample {
                        phase: sampled_phase,
                        idle: shared.idle_for(),
                        tick_gap: state.take_tick_gap(),
                    },
                    now,
                    delivery,
                    Some(stale),
                );
                let rendered = render_report(&report);
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&rendered));
                // Deliberately no `maybe_abort_after_report()` here: a
                // `SIGUSR1` capture must never terminate the process the
                // operator is inspecting, even with the debug abort armed.
            }
        }
        let progress = shared.progress.load(Ordering::Relaxed);
        let phase = Phase::from_u8(shared.phase.load(Ordering::Relaxed));
        let active = shared.active.load(Ordering::Relaxed);
        let idle = shared.idle_for();
        let bound = config.bound_for(phase);
        // One gauge sample per poll drives the parked-work bitset and the
        // no-present detector; the completed-tick ring is drained separately
        // so every retained tick is retained for its own report (bitty#2830).
        let gauges = shared.gauges.snapshot();
        let pending = PendingWork::from_gauges(&gauges);
        let tick_seq_before = state.last_tick_seq;
        let drain = shared.gauges.drain_ticks_since(state.last_tick_seq);
        state.accept_tick_drain(drain, config.bound_for(Phase::RuntimeTick));
        let drained_completion = state.last_tick_seq > tick_seq_before;
        let stale = state.sample_presented(now, shared.presented_frames.load(Ordering::Relaxed));
        // A `runtime-tick` phase mark outlives the tick by a few instructions:
        // the tick publishes its ring record before the app marks the next
        // phase. If that completion was drained on this poll, the ring owns
        // the evidence and a phase report would duplicate it; the pending
        // completed tick is reported below. A completion not yet visible to
        // this poll leaves the in-flight detector authoritative.
        let tick_phase_stale = phase == Phase::RuntimeTick && drained_completion;
        let action = if tick_phase_stale {
            MonitorAction::None
        } else {
            state.evaluate(
                now,
                progress,
                active,
                StallSample {
                    phase,
                    idle,
                    threshold: bound,
                },
                pending.any(),
            )
        };
        match action {
            MonitorAction::None => {}
            MonitorAction::Report {
                idle: stalled_for,
                phase: report_phase,
            } => {
                let report = build_report(
                    shared,
                    config,
                    DiagnosticReason::Stall,
                    ReportSample {
                        phase: report_phase,
                        idle: stalled_for,
                        tick_gap: state.take_tick_gap(),
                    },
                    now,
                    delivery,
                    Some(stale),
                );
                let rendered = render_report(&report);
                // Sampling /proc and rendering takes time; never emit once
                // shutdown was requested.
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&rendered));
                state.note_in_flight_report(report_phase);
                maybe_abort_after_report();
            }
            MonitorAction::Recovered { stalled_for } => {
                if stop.is_stopped() {
                    return;
                }
                delivery.record(sink(&format!(
                    "bitty: main-loop hang recovered after {} ms (phase={})",
                    stalled_for.as_millis(),
                    phase.as_str()
                )));
            }
        }
        // Completed over-bound ticks are independent evidence: a progress
        // change on this poll must not discard them, and each one gets its own
        // report oldest first. The tick that completed a reported in-flight
        // stall was already dropped by `accept_tick_drain`. The batch is
        // capped so a burst cannot delay the next in-flight sample for the
        // whole delivery budget; the rest carry to the next poll.
        for tick in state.take_completed_batch(now).into_iter().flatten() {
            let report = build_report(
                shared,
                config,
                DiagnosticReason::Stall,
                ReportSample {
                    phase: Phase::RuntimeTick,
                    idle: tick.duration,
                    tick_gap: state.take_tick_gap(),
                },
                now,
                delivery,
                Some(stale),
            );
            let rendered = render_report(&report);
            if stop.is_stopped() {
                return;
            }
            delivery.record(sink(&rendered));
        }
        // bitty#2830 hypothesis 2: the phase heartbeat keeps advancing while a
        // hidden surface parses output but never paints. The presented-frame
        // counter is the independent signal that catches that shape.
        let no_present_threshold = config.bound_for(Phase::Idle);
        if let Some(stale) =
            state.no_present_due(now, stale, pending.hidden_damage, no_present_threshold)
        {
            let report = build_report(
                shared,
                config,
                DiagnosticReason::NoPresent,
                ReportSample {
                    phase,
                    idle: shared.idle_for(),
                    tick_gap: state.take_tick_gap(),
                },
                now,
                delivery,
                Some(stale),
            );
            let rendered = render_report(&report);
            if stop.is_stopped() {
                return;
            }
            delivery.record(sink(&rendered));
            maybe_abort_after_report();
        }
        // A ring gap observed by this poll may not have found a report: every
        // retained tick was under the bound and no phase/no-present stall
        // fired. Write a minimal bounded line through the same sink and slot
        // so overflow is never silent (bitty#2830 review N2). A report built
        // above already took and rendered the gap.
        if let Some(gap) = state.take_tick_gap() {
            if stop.is_stopped() {
                return;
            }
            let line = format!(
                "{STALL_REPORT_PREFIX}: completed-tick ring gap (capacity={} missed_at_most={} seq={}..{}, phase={}, app_uptime={} ms, pid={})",
                TICK_RING_CAPACITY,
                gap.missed(),
                gap.from,
                gap.to,
                phase.as_str(),
                shared.start.elapsed().as_millis(),
                std::process::id(),
            );
            delivery.record(sink(&line));
        }
        idle_park = !active
            && !shared.active.load(Ordering::Acquire)
            && !pending.any()
            && state.pending_ticks.is_empty()
            && state.tick_gap.is_none()
            && !state.reported
            && shared.gauges.snapshot().tick_count == state.last_tick_seq;
    }
}

/// Why one diagnostic is being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticReason {
    /// The monitored phase exceeded its bound.
    Stall,
    /// The loop keeps advancing (output is parsed) but no frame is presented
    /// while the surface is hidden with retained damage (bitty#2830
    /// hypothesis 2, the occlusion/reveal stall).
    NoPresent,
    /// The operator sent `SIGUSR1` (Unix only).
    #[cfg(unix)]
    Signal,
}

impl DiagnosticReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stall => "stall",
            Self::NoPresent => "no-present",
            #[cfg(unix)]
            Self::Signal => "sigusr1",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Stall | Self::NoPresent => STALL_REPORT_PREFIX,
            #[cfg(unix)]
            Self::Signal => SIGNAL_REPORT_PREFIX,
        }
    }
}

/// Parked work the loop still owes while it sits in `ControlFlow::Wait`.
///
/// bitty#2830: a park is only a quiet window when nothing is outstanding. The
/// bitset is published as `pending=` in every report so a lost wakeup names
/// its backlog instead of looking idle. The three sources are the ones the
/// freeze campaign proved matter: queued PTY bytes, queued query replies, and
/// the occlusion gate holding retained damage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PendingWork {
    /// Bytes queued for the PTY writer and not yet written.
    pty: bool,
    /// Query replies queued and not yet answered.
    replies: bool,
    /// The surface-visibility gate is closed while retained work is owed.
    hidden_damage: bool,
}

impl PendingWork {
    /// Derives the bitset from one lock-free gauge sample.
    fn from_gauges(gauges: &HangGaugeSnapshot) -> Self {
        Self {
            pty: gauges.pending_input_bytes > 0,
            replies: gauges.pending_replies > 0,
            hidden_damage: gauges.hidden_damage > 0,
        }
    }

    fn any(self) -> bool {
        self.pty || self.replies || self.hidden_damage
    }

    /// One stable `|`-separated field, `none` when nothing is outstanding.
    fn render(self) -> String {
        if !self.any() {
            return String::from("none");
        }
        let mut out = String::new();
        for (set, name) in [
            (self.hidden_damage, "hidden-damage"),
            (self.pty, "pty"),
            (self.replies, "replies"),
        ] {
            if !set {
                continue;
            }
            if !out.is_empty() {
                out.push('|');
            }
            out.push_str(name);
        }
        out
    }
}

/// Assemble one report from the shared heartbeat, context, gauges, and `/proc`.
///
/// `sample` is the detector's decision, not a fresh read: a completed
/// over-bound tick is named `runtime-tick` with its measured duration even
/// though the app has already moved on to the next phase.
fn build_report(
    shared: &Shared,
    config: WatchdogConfig,
    reason: DiagnosticReason,
    sample: ReportSample,
    now: Instant,
    delivery: DeliverySummary,
    present_stale: Option<Duration>,
) -> StallReport {
    let totals_snapshot = sample_process_totals();
    let gauges = shared.gauges.snapshot();
    StallReport {
        reason,
        threshold: config.bound_for(sample.phase),
        idle: sample.idle,
        phase: sample.phase,
        app_uptime: now.saturating_duration_since(shared.start),
        progress: shared.progress.load(Ordering::Relaxed),
        context: shared.context.try_lock().ok().map(|slot| slot.clone()),
        pending: PendingWork::from_gauges(&gauges),
        present_stale_ms: present_stale
            .map(|stale| u64::try_from(stale.as_millis()).unwrap_or(u64::MAX)),
        gauges,
        threads: totals_snapshot.0,
        totals: totals_snapshot.1,
        delivery,
        tick_gap: sample.tick_gap,
    }
}

/// Debug-only opt-in abort after a report.
fn maybe_abort_after_report() {
    if !abort_after_report() {
        return;
    }
    let backtrace = std::backtrace::Backtrace::force_capture();
    eprintln!("bitty: hang watchdog debug abort requested\n{backtrace}");
    std::process::abort();
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
    // bitty#2830: RSS comes from `VmRSS` in `/proc/self/status`. That read is
    // lock-free with respect to the process `mmap` lock (unlike a `smaps`
    // traversal, which the stalled path may itself hold) and the value is
    // already in KiB, so no page-size assumption is needed.
    totals.rss_kib = rss_kib_from_status().unwrap_or(0);
    (rows, totals)
}

/// Resident set size in KiB from `/proc/self/status`.
#[cfg(target_os = "linux")]
fn rss_kib_from_status() -> Option<u64> {
    let status = read_proc_text("/proc/self/status", 8192)?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS:")?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
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
    let mut out = String::with_capacity(2048);
    match report.reason {
        DiagnosticReason::Stall => {
            let _ = writeln!(
                out,
                "{}: no progress for {} ms (threshold {} ms, phase={}, app_uptime {} ms, progress={}, pid={})",
                report.reason.prefix(),
                report.idle.as_millis(),
                report.threshold.as_millis(),
                report.phase.as_str(),
                report.app_uptime.as_millis(),
                report.progress,
                std::process::id(),
            );
        }
        DiagnosticReason::NoPresent => {
            let _ = writeln!(
                out,
                "{}: no frame presented for {} ms while the surface is hidden with retained damage (threshold {} ms, phase={}, app_uptime {} ms, progress={}, pid={})",
                report.reason.prefix(),
                report.present_stale_ms.unwrap_or_default(),
                report.threshold.as_millis(),
                report.phase.as_str(),
                report.app_uptime.as_millis(),
                report.progress,
                std::process::id(),
            );
        }
        #[cfg(unix)]
        DiagnosticReason::Signal => {
            let _ = writeln!(
                out,
                "{}: operator requested a capture (phase={}, app_uptime {} ms, progress={}, pid={}, idle {} ms)",
                report.reason.prefix(),
                report.phase.as_str(),
                report.app_uptime.as_millis(),
                report.progress,
                std::process::id(),
                report.idle.as_millis(),
            );
        }
    }
    let _ = writeln!(out, "  reason={}", report.reason.as_str());
    // bitty#2830: the parked-work bitset and the stuck-frame span. A lost
    // wakeup names its backlog here instead of looking idle; `trigger=` names
    // what wrote this report (`stall`, `no-present`, or `sigusr1`).
    let _ = writeln!(
        out,
        "  parked: pending={} present_stale_ms={} trigger={}",
        report.pending.render(),
        report
            .present_stale_ms
            .map_or_else(|| String::from("none"), |ms| ms.to_string()),
        report.reason.as_str(),
    );
    // bitty#2830 review F1: a ring overwrite is explicit evidence, never a
    // silent loss. One line names the ring size and the missing sequence span.
    if let Some(gap) = report.tick_gap {
        let _ = writeln!(
            out,
            "  tick-ring: capacity={} missed_at_most={} seq={}..{}",
            TICK_RING_CAPACITY,
            gap.missed(),
            gap.from,
            gap.to,
        );
    }
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
            let redraw_pending = context
                .redraw_pending_ms
                .map_or_else(|| String::from("none"), |ms| ms.to_string());
            // bitty#2830: the off-thread presenter gauges. A stuck present is
            // visible here while the main thread is healthy: `in_flight=1`
            // with a growing `in_flight_ms` names the compositor wait that no
            // longer blocks the event loop.
            let _ = writeln!(
                out,
                "  presenter: in_flight={} in_flight_since_ms={} in_flight_ms={} stuck_presents={} recoveries={} coalesced={} acks_dropped={} unconfirmed={}",
                surface.presenter_in_flight,
                surface.presenter_in_flight_since_ms,
                surface.presenter_in_flight_ms,
                surface.presenter_stuck_presents,
                surface.presenter_recoveries,
                surface.presenter_coalesced,
                surface.presenter_acks_dropped,
                surface.present_unconfirmed,
            );
            let _ = writeln!(
                out,
                "  surface: last={} visible={} redraw_pending_ms={} probe_seen={} probe_occluded={} configure_err={} acquire_ok={} acquire_timeout={} acquire_outdated={} acquire_lost={} acquire_other={} present_ok={} present_err={} resize={} resize_zero_skipped={} headless_present={} retry_failures={} retry_attempts={} retry_recoveries={} retry_last_delay_ms={} lifecycle={}",
                surface.last_event,
                context.surface_visible,
                redraw_pending,
                context.probe_redraw_seen,
                context.probe_occluded,
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
        "  process: threads={} fds={} rss={} KiB (VmRSS)",
        report.totals.thread_count, report.totals.fd_count, report.totals.rss_kib,
    );
    // bitty#2830: the runtime gauges make an accumulation or deadlock visible
    // without a second tool: queue depths, content bytes, plugin state.
    let _ = writeln!(out, "{}", report.gauges.render());
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
        // Recovery lines share the sink but never overwrite the diagnostic;
        // both the periodic stall and the SIGUSR1 capture own the slot.
        let slot =
            if line.starts_with(STALL_REPORT_PREFIX) || line.starts_with(SIGNAL_REPORT_PREFIX) {
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
    fn config_is_default_on_and_threshold_is_clamped() {
        // The build default is an explicit input, so this pins both profiles
        // in every CI run instead of re-deriving `cfg!(debug_assertions)`.
        assert_eq!(
            config_from_values(None, None, true),
            Some(WatchdogConfig::default_threshold())
        );
        assert_eq!(config_from_values(None, None, false), None);
        assert_eq!(config_from_values(Some("0"), None, true), None);
        assert_eq!(config_from_values(Some("off"), None, true), None);
        assert_eq!(config_from_values(Some("nonsense"), None, true), None);
        assert_eq!(
            config_from_values(Some("1"), None, false),
            Some(WatchdogConfig::default_threshold())
        );
        assert_eq!(
            config_from_values(Some("750"), None, false),
            Some(WatchdogConfig::with_threshold_ms(750))
        );
        assert_eq!(
            config_from_values(Some("1"), Some("250"), false),
            Some(WatchdogConfig::with_threshold_ms(250))
        );
        assert_eq!(
            config_from_values(Some("1"), Some("1"), false).map(|c| c.threshold),
            Some(MIN_STALL_THRESHOLD)
        );
        assert_eq!(
            config_from_values(Some("1"), Some("9999999"), false).map(|c| c.threshold),
            Some(MAX_STALL_THRESHOLD)
        );
    }

    /// The enable contract for both build defaults: unset follows the
    /// injected default, an explicit off value always wins, and a numeric
    /// value selects the bound.
    ///
    /// Both values run in every profile, so the debug-profile CI run pins the
    /// release default instead of only pinning debug-off.
    #[test]
    fn watchdog_default_follows_the_build_and_explicit_off_always_wins() {
        for default_on in [true, false] {
            assert_eq!(
                config_from_values(None, None, default_on).is_some(),
                default_on,
                "unset must follow the injected build default"
            );
            assert_eq!(
                config_from_values(Some(""), None, default_on).is_some(),
                default_on,
                "an empty switch means unset"
            );
            for off in ["0", "false", "off", "no", "OFF", " No "] {
                assert_eq!(
                    config_from_values(Some(off), None, default_on),
                    None,
                    "explicit off must win: {off:?}"
                );
                assert_eq!(
                    config_from_values(Some(off), Some("250"), default_on),
                    None,
                    "a threshold must not override explicit off: {off:?}"
                );
            }
            for on in ["1", "true", "on", "yes", "TRUE"] {
                assert_eq!(
                    config_from_values(Some(on), None, default_on),
                    Some(WatchdogConfig::default_threshold()),
                    "explicit on must work: {on:?}"
                );
            }
            // The threshold variable follows the build default: it cannot
            // enable a build whose default is off, and with the default on it
            // selects the bound of the monitor that is already armed.
            assert_eq!(
                config_from_values(None, Some("250"), default_on),
                default_on.then(|| WatchdogConfig::with_threshold_ms(250))
            );
        }
    }

    /// The release policy is expressed through the config decision, so this
    /// assertion runs in the debug-profile CI job too: silently turning the
    /// release watchdog off fails CI instead of only failing in a release run.
    #[test]
    fn release_default_on_is_pinned() {
        assert_eq!(
            config_from_values(None, None, RELEASE_DEFAULT_ON),
            Some(WatchdogConfig::default_threshold()),
            "release builds must arm the watchdog without an environment switch"
        );
        // The debug build must not inherit the release policy.
        #[cfg(debug_assertions)]
        assert!(
            config_from_values(None, None, default_enabled()).is_none(),
            "debug builds must not arm the watchdog without an explicit switch"
        );
    }

    /// `from_env`'s decision core with the build default injected: unset in
    /// debug starts nothing, unset in release starts a monitor, and an
    /// explicit off value wins over the release default.
    ///
    /// The monitor threads started here are joined immediately; no report is
    /// delivered because no phase stalls and no signal is raised.
    #[test]
    fn from_env_starts_a_monitor_only_when_the_default_or_switch_asks() {
        let gauges = || Arc::new(HangGauges::new());
        assert!(
            StallWatchdog::from_values(None, None, false, gauges()).is_none(),
            "unset in a debug build must not start a monitor"
        );
        let started = StallWatchdog::from_values(None, None, true, gauges())
            .expect("unset in a release build must start a monitor");
        #[cfg(unix)]
        assert!(
            started.signal_id.is_some(),
            "a monitor started by the release default must own the SIGUSR1 action"
        );
        started
            .stop_and_join()
            .expect("the monitor thread must exit");
        assert!(
            StallWatchdog::from_values(Some("0"), None, true, gauges()).is_none(),
            "an explicit off switch must win over the release default"
        );
        assert!(
            StallWatchdog::from_values(Some("off"), Some("250"), true, gauges()).is_none(),
            "a threshold must not override an explicit off switch"
        );
        let started = StallWatchdog::from_values(Some("1"), None, false, gauges())
            .expect("an explicit on switch must start a monitor in a debug build");
        started
            .stop_and_join()
            .expect("the monitor thread must exit");
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
                StallSample {
                    phase: Phase::Idle,
                    idle: Duration::from_secs(60),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::None
        );
        assert_eq!(
            state.evaluate(
                now,
                1,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_millis(1999),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::None
        );
    }

    #[test]
    fn monitor_reports_once_and_recovers() {
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
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: DEFAULT_STALL_THRESHOLD,
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::Report {
                idle: DEFAULT_STALL_THRESHOLD,
                phase: Phase::RuntimeTick,
            }
        );
        // One-shot: a continuous stall never writes a second report.
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(5),
                7,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_secs(7),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::None
        );
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(30),
                7,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_secs(32),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::None
        );
        // Progress after a report emits one recovery line.
        assert_eq!(
            state.evaluate(
                start + Duration::from_secs(31),
                8,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_millis(0),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
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
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_millis(0),
                    threshold: DEFAULT_STALL_THRESHOLD,
                },
                false,
            ),
            MonitorAction::None
        );
    }

    #[test]
    fn no_present_requires_hidden_damage_and_is_repeat_bounded() {
        let start = Instant::now();
        let mut state = MonitorState::new(start, 1, 7, 0);
        let threshold = DEFAULT_STALL_THRESHOLD;
        // A stuck counter on a visible surface is an idle app, not a stall:
        // the gate holds no retained damage, so nothing is reported.
        assert_eq!(
            state.no_present_due(start + threshold, threshold, false, threshold),
            None
        );
        // Hidden with retained damage past the threshold reports the span...
        assert_eq!(
            state.no_present_due(start + threshold, threshold, true, threshold),
            Some(threshold)
        );
        // ...and the same continuous span is not re-reported immediately.
        assert_eq!(
            state.no_present_due(
                start + threshold + Duration::from_secs(1),
                threshold + Duration::from_secs(1),
                true,
                threshold,
            ),
            None
        );
        // One interval later the still-stuck state reports again.
        assert_eq!(
            state.no_present_due(
                start + threshold + REPORT_REPEAT_INTERVAL,
                threshold + REPORT_REPEAT_INTERVAL,
                true,
                threshold,
            ),
            Some(threshold + REPORT_REPEAT_INTERVAL)
        );
        // A presented frame resets the anchor, so the next stall is measured
        // from the frame that did commit.
        let later = start + threshold + REPORT_REPEAT_INTERVAL + Duration::from_secs(5);
        assert_eq!(state.sample_presented(later, 8), Duration::ZERO);
        assert_eq!(
            state.sample_presented(later + Duration::from_millis(250), 8),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn stall_report_suppresses_the_duplicate_no_present_line() {
        let start = Instant::now();
        let mut state = MonitorState::new(start, 1, 7, 0);
        let threshold = DEFAULT_STALL_THRESHOLD;
        assert_eq!(
            state.evaluate(
                start + threshold,
                1,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: threshold,
                    threshold,
                },
                false,
            ),
            MonitorAction::Report {
                idle: threshold,
                phase: Phase::RuntimeTick,
            }
        );
        // Same poll: a wedged phase also froze the frame counter, so the
        // no-present detector must not add a second line for one wedge.
        assert_eq!(
            state.no_present_due(start + threshold, threshold, true, threshold),
            None
        );
        // Still suppressed one poll later, while the frame counter is stuck.
        assert_eq!(
            state.no_present_due(
                start + threshold + Duration::from_millis(100),
                threshold + Duration::from_millis(100),
                true,
                threshold
            ),
            None
        );
        // A frame that presents ends the wedge; a later one may report again.
        assert_eq!(
            state.sample_presented(start + threshold + Duration::from_secs(1), 8),
            Duration::ZERO
        );
        let next = start + threshold + Duration::from_secs(2);
        let finished_stale = threshold;
        assert_eq!(
            state.no_present_due(next, finished_stale, true, threshold),
            Some(finished_stale)
        );
    }

    #[test]
    fn monitor_is_one_shot_per_stall() {
        let mut state = MonitorState {
            last_progress: 9,
            ..MonitorState::default()
        };
        let start = Instant::now();
        let mut reports = 0;
        for step in 0..20 {
            let now = start + Duration::from_secs(30 * step);
            if matches!(
                state.evaluate(
                    now,
                    9,
                    true,
                    StallSample {
                        phase: Phase::RuntimeTick,
                        idle: Duration::from_secs(3600),
                        threshold: DEFAULT_STALL_THRESHOLD,
                    },
                    false,
                ),
                MonitorAction::Report { .. }
            ) {
                reports += 1;
            }
        }
        assert_eq!(reports, 1, "a continuous stall writes exactly one report");
    }

    /// bitty#2830 review F3: the exact old-bug case. The sample that observes
    /// a completed tick also sees a progress change (the tick's own phase
    /// marks), and the old early return discarded the measured duration. The
    /// completed-tick queue must report it anyway.
    #[test]
    fn completed_tick_reports_survive_a_progress_change() {
        let start = Instant::now();
        let gauges = Arc::new(HangGauges::new());
        let mut state = MonitorState::new(start, 1, 0, 0);
        let bound = HANG_WATCHDOG_TICK_BOUND;
        let long = Duration::from_millis(17_490);
        gauges.record_tick_for_test(long);
        state.accept_tick_drain(gauges.drain_ticks_since(state.last_tick_seq), bound);
        // Healthy phase heartbeat: progress advanced past the tick.
        assert_eq!(
            state.evaluate(
                start + Duration::from_millis(200),
                2,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_millis(200),
                    threshold: bound,
                },
                false,
            ),
            MonitorAction::None
        );
        let report = state
            .next_completed_report(Instant::now())
            .expect("a progress change must not discard the completed tick");
        assert_eq!(report.duration, long);
        assert!(state.next_completed_report(Instant::now()).is_none());

        let shared = Shared::new(Arc::clone(&gauges));
        let rendered = render_report(&build_report(
            &shared,
            WatchdogConfig::with_threshold_ms(10_000),
            DiagnosticReason::Stall,
            ReportSample {
                phase: Phase::RuntimeTick,
                idle: report.duration,
                tick_gap: None,
            },
            Instant::now(),
            DeliverySummary::default(),
            None,
        ));
        assert!(
            rendered.contains("no progress for 17490 ms (threshold 3000 ms, phase=runtime-tick"),
            "{rendered}"
        );
    }

    /// bitty#2830 review F4: a completed tick sampled between `runtime.tick()`
    /// returning and the next phase mark is consumed by the ring drain but
    /// retained by the monitor until it is reported. A poll with unchanged
    /// progress must not drop it.
    #[test]
    fn completed_tick_without_a_progress_change_is_retained() {
        let start = Instant::now();
        let gauges = Arc::new(HangGauges::new());
        let mut state = MonitorState::new(start, 1, 0, 0);
        let bound = HANG_WATCHDOG_TICK_BOUND;
        let long = Duration::from_millis(13_090);
        gauges.record_tick_for_test(long);
        state.accept_tick_drain(gauges.drain_ticks_since(state.last_tick_seq), bound);
        assert_eq!(
            state.evaluate(
                start + Duration::from_millis(10),
                1,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::from_millis(10),
                    threshold: bound,
                },
                false,
            ),
            MonitorAction::None
        );
        let report = state
            .next_completed_report(Instant::now())
            .expect("an unchanged progress sample must retain the tick");
        assert_eq!(report.duration, long);
    }

    /// bitty#2830 review F1: every over-bound tick retained by the ring is
    /// reported, oldest first, and a tick inside the bound is skipped without
    /// consuming the ones around it.
    #[test]
    fn every_over_bound_completed_tick_reports_oldest_first() {
        let gauges = Arc::new(HangGauges::new());
        let mut state = MonitorState::new(Instant::now(), 0, 0, 0);
        let bound = HANG_WATCHDOG_TICK_BOUND;
        let first = Duration::from_millis(17_490);
        let second = Duration::from_millis(13_090);
        gauges.record_tick_for_test(first);
        gauges.record_tick_for_test(Duration::from_millis(180));
        gauges.record_tick_for_test(second);
        state.accept_tick_drain(gauges.drain_ticks_since(state.last_tick_seq), bound);
        assert_eq!(
            state.next_completed_report(Instant::now()),
            Some(CompletedTick {
                seq: 1,
                duration: first,
            })
        );
        assert_eq!(
            state.next_completed_report(Instant::now()),
            Some(CompletedTick {
                seq: 3,
                duration: second,
            })
        );
        assert!(state.next_completed_report(Instant::now()).is_none());
    }

    /// bitty#2830 review F1: a ring overwrite is named by the next report
    /// with the ring size and the missing sequence span, once.
    #[test]
    fn tick_ring_gap_is_named_once_in_the_next_report() {
        let gauges = Arc::new(HangGauges::new());
        for _ in 0..(TICK_RING_CAPACITY + 4) {
            gauges.record_tick_for_test(Duration::from_millis(1));
        }
        let mut state = MonitorState::new(Instant::now(), 0, 0, 0);
        state.accept_tick_drain(
            gauges.drain_ticks_since(state.last_tick_seq),
            HANG_WATCHDOG_TICK_BOUND,
        );
        let gap = state.take_tick_gap().expect("overflow must be named");
        assert_eq!(gap, TickGap { from: 1, to: 4 });
        assert_eq!(gap.missed(), 4);
        assert!(
            state.take_tick_gap().is_none(),
            "the gap belongs to exactly one report"
        );

        let shared = Shared::new(Arc::clone(&gauges));
        let report = build_report(
            &shared,
            WatchdogConfig::with_threshold_ms(10_000),
            DiagnosticReason::Stall,
            ReportSample {
                phase: Phase::RuntimeTick,
                idle: Duration::from_millis(1),
                tick_gap: Some(gap),
            },
            Instant::now(),
            DeliverySummary::default(),
            None,
        );
        let rendered = render_report(&report);
        assert!(
            rendered.contains("tick-ring: capacity=256 missed_at_most=4 seq=1..4"),
            "{rendered}"
        );
    }

    /// bitty#2830 review N3: merged spans include retained sequences between
    /// them, so the rendered count is explicitly an upper bound.
    #[test]
    fn merged_tick_gaps_render_an_upper_bound() {
        let mut gap = None;
        merge_tick_gap(&mut gap, TickGap { from: 3, to: 4 });
        merge_tick_gap(&mut gap, TickGap { from: 10, to: 12 });
        let gap = gap.expect("merged gap");
        assert_eq!(gap, TickGap { from: 3, to: 12 });
        // Sequences 5..=9 are retained between the two gaps; the count is the
        // span length, not an exact lost-entry count.
        assert_eq!(gap.missed(), 10);

        let gauges = Arc::new(HangGauges::new());
        let shared = Shared::new(Arc::clone(&gauges));
        let report = build_report(
            &shared,
            WatchdogConfig::with_threshold_ms(10_000),
            DiagnosticReason::Stall,
            ReportSample {
                phase: Phase::RuntimeTick,
                idle: Duration::from_millis(1),
                tick_gap: Some(gap),
            },
            Instant::now(),
            DeliverySummary::default(),
            None,
        );
        let rendered = render_report(&report);
        assert!(
            rendered.contains("tick-ring: capacity=256 missed_at_most=10 seq=3..12"),
            "{rendered}"
        );
    }

    /// The completed-tick report names the phase and duration the detector
    /// decided on, not the phase the app had reached by rendering time.
    #[test]
    fn completed_tick_report_names_runtime_tick_and_its_measured_duration() {
        let gauges = Arc::new(HangGauges::new());
        let shared = Shared::new(Arc::clone(&gauges));
        // The app has already moved past the tick when the report is built.
        shared.mark(Phase::SnapshotPublish, true);
        let measured = Duration::from_millis(17_490);
        let report = build_report(
            &shared,
            WatchdogConfig::with_threshold_ms(10_000),
            DiagnosticReason::Stall,
            ReportSample {
                phase: Phase::RuntimeTick,
                idle: measured,
                tick_gap: None,
            },
            Instant::now(),
            DeliverySummary::default(),
            None,
        );
        assert_eq!(report.phase, Phase::RuntimeTick);
        assert_eq!(report.threshold, HANG_WATCHDOG_TICK_BOUND);
        assert_eq!(report.idle, measured);
        let rendered = render_report(&report);
        assert!(
            rendered.contains("no progress for 17490 ms (threshold 3000 ms, phase=runtime-tick"),
            "{rendered}"
        );
    }

    /// One report per continuous stall still holds when the in-flight
    /// detector caught the block: the ring's completion record is matched and
    /// dropped, and the progress change is the single recovery line.
    #[test]
    fn in_flight_tick_completion_is_not_reported_twice() {
        let start = Instant::now();
        let gauges = Arc::new(HangGauges::new());
        let mut state = MonitorState::new(start, 1, 0, 0);
        let bound = HANG_WATCHDOG_TICK_BOUND;
        assert_eq!(
            state.evaluate(
                start + bound,
                1,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: bound,
                    threshold: bound,
                },
                false,
            ),
            MonitorAction::Report {
                idle: bound,
                phase: Phase::RuntimeTick,
            }
        );
        state.note_in_flight_report(Phase::RuntimeTick);
        assert_eq!(state.completion_seq, Some(1));
        let long = Duration::from_millis(17_490);
        gauges.record_tick_for_test(long);
        state.accept_tick_drain(gauges.drain_ticks_since(state.last_tick_seq), bound);
        assert!(
            state.completion_seq.is_none(),
            "the completing tick must be matched"
        );
        assert!(
            state.next_completed_report(Instant::now()).is_none(),
            "the in-flight report already covers this tick"
        );
        assert_eq!(
            state.evaluate(
                start + long,
                2,
                true,
                StallSample {
                    phase: Phase::RuntimeTick,
                    idle: Duration::ZERO,
                    threshold: bound,
                },
                false,
            ),
            MonitorAction::Recovered {
                stalled_for: long - bound,
            }
        );
    }

    /// bitty#2830 review N2: a ring gap with no other report must still be
    /// delivered. Twenty fast ticks cannot produce a pending over-bound
    /// report, so only the fallback line can carry the overflow evidence.
    #[test]
    fn tick_ring_gap_is_delivered_without_any_stall_report() {
        let (tx, rx) = mpsc::channel::<String>();
        let gauges = Arc::new(HangGauges::new());
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(600_000),
            Arc::clone(&gauges),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        watchdog.idle();
        // Let the monitor settle into `wait_idle` on the empty ring so the
        // whole burst is one observed overflow without racing startup poll.
        let settle_deadline = Instant::now() + Duration::from_secs(2);
        while !watchdog.shared.idle_parked.load(Ordering::Acquire)
            && Instant::now() < settle_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..(TICK_RING_CAPACITY + 4) {
            gauges.record_tick_for_test(Duration::from_millis(1));
        }
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a ring gap must be delivered without a stall report");
        assert!(line.starts_with(STALL_REPORT_PREFIX), "{line}");
        assert!(line.contains("completed-tick ring gap"), "{line}");
        assert!(line.contains("capacity=256"), "{line}");
        assert!(line.contains("missed_at_most=4"), "{line}");
        assert!(line.contains("seq=1..4"), "{line}");
        assert!(line.contains("phase=idle"), "{line}");
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "the gap is delivered exactly once"
        );
        watchdog.stop_and_join().expect("monitor exits");
    }

    /// bitty#2830 review N4: the completed-tick ring path through the real
    /// monitor loop. The phase is idle and the generic bound is 600 s, so the
    /// in-flight detector cannot produce this report; only the ring can.
    #[test]
    fn completed_tick_ring_reports_through_the_monitor_loop() {
        let (tx, rx) = mpsc::channel::<String>();
        let gauges = Arc::new(HangGauges::new());
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(600_000),
            Arc::clone(&gauges),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        watchdog.idle();
        // Let the monitor capture its initial tick sequence and park in
        // `wait_idle` before the seeded tick so the drain sees it.
        let settle_deadline = Instant::now() + Duration::from_secs(2);
        while !watchdog.shared.idle_parked.load(Ordering::Acquire)
            && Instant::now() < settle_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let measured = Duration::from_millis(4_500);
        gauges.record_tick_for_test(measured);
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a completed over-bound tick must report");
        assert!(line.starts_with(STALL_REPORT_PREFIX), "{line}");
        assert!(line.contains("phase=runtime-tick"), "{line}");
        assert!(line.contains("reason=stall"), "{line}");
        assert!(line.contains("no progress for 4500 ms"), "{line}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "one report per completed tick"
        );
        watchdog.stop_and_join().expect("monitor exits");
    }

    /// bitty#2830 review N5: a drained burst is delivered in bounded batches
    /// and the remainder carries to the next poll.
    #[test]
    fn completed_report_batch_is_capped_and_carried() {
        let gauges = Arc::new(HangGauges::new());
        for _ in 0..(MAX_COMPLETED_REPORTS_PER_POLL + 2) {
            gauges.record_tick_for_test(Duration::from_millis(4_000));
        }
        let mut state = MonitorState::new(Instant::now(), 0, 0, 0);
        state.accept_tick_drain(gauges.drain_ticks_since(0), HANG_WATCHDOG_TICK_BOUND);
        let now = Instant::now();
        let first: Vec<u64> = state
            .take_completed_batch(now)
            .iter()
            .flatten()
            .map(|tick| tick.seq)
            .collect();
        assert_eq!(
            first,
            (1..=MAX_COMPLETED_REPORTS_PER_POLL as u64).collect::<Vec<_>>()
        );
        let second: Vec<u64> = state
            .take_completed_batch(now)
            .iter()
            .flatten()
            .map(|tick| tick.seq)
            .collect();
        assert_eq!(
            second,
            vec![
                MAX_COMPLETED_REPORTS_PER_POLL as u64 + 1,
                MAX_COMPLETED_REPORTS_PER_POLL as u64 + 2,
            ]
        );
        assert!(
            state
                .take_completed_batch(now)
                .iter()
                .flatten()
                .next()
                .is_none()
        );
    }

    /// bitty#2830: a `SIGUSR1` capture writes one full report while the loop
    /// is parked, and the monitor keeps running afterwards.
    ///
    /// The receipt is simulated on the monitor's own flag instead of raising a
    /// real signal: the test binary's handler is a process-wide registration,
    /// and a real signal can arrive after it is removed, which would terminate
    /// the whole harness.
    #[cfg(unix)]
    #[test]
    fn signal_receipt_writes_a_report_without_stopping_the_monitor() {
        let (tx, rx) = mpsc::channel::<String>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(150),
            Arc::new(HangGauges::new()),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        // The reported freeze shape: the loop is parked, not in an active
        // phase, so only the signal can produce this sample.
        watchdog.idle();
        watchdog.simulate_signal_receipt();
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("SIGUSR1 capture must arrive while idle");
        assert!(line.starts_with(SIGNAL_REPORT_PREFIX), "{line}");
        assert!(line.contains("reason=sigusr1"), "{line}");
        assert!(line.contains("trigger=sigusr1"), "{line}");
        assert!(line.contains("phase=idle"), "{line}");
        assert!(line.contains("progress="), "{line}");
        assert!(line.contains("  parked: pending="), "{line}");
        assert!(line.contains("  surface: "), "{line}");
        assert!(line.contains("  process: threads="), "{line}");
        #[cfg(target_os = "linux")]
        assert!(line.contains("[main] tid="), "{line}");
        #[cfg(not(target_os = "linux"))]
        assert!(line.contains("thread-state snapshot unavailable"), "{line}");
        // The process did not exit or abort: a real stall still reports.
        watchdog.enter(Phase::RuntimeTick);
        let stall = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the monitor must keep reporting after a signal capture");
        assert!(stall.starts_with(STALL_REPORT_PREFIX), "{stall}");
        assert!(stall.contains("reason=stall"), "{stall}");
        watchdog
            .stop_and_join()
            .expect("the monitor thread must exit cleanly");
    }

    /// Rapid `SIGUSR1` receipts coalesce into one atomic flag, and receipts
    /// inside [`SIGNAL_REPORT_INTERVAL`] are dropped: they never queue up or
    /// grow the report log.
    #[cfg(unix)]
    #[test]
    fn signal_receipts_are_rate_limited_and_do_not_queue() {
        let (tx, rx) = mpsc::channel::<String>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(60_000),
            Arc::new(HangGauges::new()),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        watchdog.idle();
        let first_at = Instant::now();
        watchdog.simulate_signal_receipt();
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first receipt must write");
        assert!(first.starts_with(SIGNAL_REPORT_PREFIX), "{first}");
        // Far more receipts than the monitor can service: the flag is one bit,
        // so they collapse instead of growing a queue.
        for _ in 0..256 {
            watchdog.simulate_signal_receipt();
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "receipts inside the interval must be dropped"
        );
        // Once the interval elapses, one more receipt is accepted and brings
        // exactly one report, not 256.
        let mut accepted = None;
        while first_at.elapsed() < Duration::from_secs(5) {
            watchdog.simulate_signal_receipt();
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(line) => {
                    accepted = Some(line);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("report channel closed: {error}"),
            }
        }
        let accepted = accepted.expect("one receipt after the interval must write");
        assert!(accepted.starts_with(SIGNAL_REPORT_PREFIX), "{accepted}");
        assert!(
            first_at.elapsed() >= SIGNAL_REPORT_INTERVAL,
            "the second dump must respect the interval: {:?}",
            first_at.elapsed()
        );
        // The limiter re-arms: an immediate third receipt is dropped.
        watchdog.simulate_signal_receipt();
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "an immediate receipt after a dump must be dropped"
        );
        watchdog
            .stop_and_join()
            .expect("the monitor thread must exit cleanly");
    }

    /// Structural coverage for the real wiring: starting a monitor owns a
    /// `SIGUSR1` report action, and the first monitor also installs the
    /// bounded post-shutdown default action. Real-signal delivery is covered
    /// in child processes by [`sigusr1_reaches_the_monitor_in_a_child_process`]
    /// and [`sigusr1_terminates_a_child_after_the_monitor_stopped`]; the
    /// harness itself never raises the signal.
    #[cfg(unix)]
    #[test]
    fn monitor_start_owns_the_sigusr1_registration() {
        let watchdog = StallWatchdog::start(
            WatchdogConfig::default_threshold(),
            Arc::new(HangGauges::new()),
            Box::new(|_line| DeliveryOutcome::Delivered),
        );
        assert!(
            watchdog.signal_id.is_some(),
            "a started monitor must own a SIGUSR1 registration"
        );
        assert!(
            sigusr1::fallback_installed(),
            "a started monitor must install the bounded post-shutdown default action"
        );
        watchdog
            .stop_and_join()
            .expect("the monitor thread must exit cleanly");
    }

    /// Environment switch that turns the re-executed test binary into the
    /// `SIGUSR1` child helper; absent in a normal harness run.
    #[cfg(unix)]
    const SIGUSR1_CHILD_ENV: &str = "BITTY_TEST_SIGUSR1_CHILD";
    /// Directory the child helper publishes its ready marker and report in.
    #[cfg(unix)]
    const SIGUSR1_CHILD_DIR_ENV: &str = "BITTY_TEST_SIGUSR1_DIR";
    /// Exact libtest name of the child helper.
    #[cfg(unix)]
    const SIGUSR1_CHILD_TEST: &str = "stall_watchdog::tests::sigusr1_child_process_helper";
    /// Bound for every parent-side wait on the child helper.
    #[cfg(unix)]
    const SIGUSR1_CHILD_TIMEOUT: Duration = Duration::from_secs(10);

    /// Fresh scratch directory for one child-process signal test.
    #[cfg(unix)]
    fn sigusr1_child_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bitty-sigusr1-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("child scratch directory");
        dir
    }

    /// Re-execute this test binary so it runs only the child helper in `mode`.
    #[cfg(unix)]
    fn spawn_sigusr1_child(mode: &str, dir: &Path) -> Option<Child> {
        let exe = std::env::current_exe().ok()?;
        Command::new(exe)
            .args([
                "--exact",
                SIGUSR1_CHILD_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SIGUSR1_CHILD_ENV, mode)
            .env(SIGUSR1_CHILD_DIR_ENV, dir)
            .env_remove(HANG_WATCHDOG_ENV)
            .env_remove(LEGACY_WATCHDOG_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()
    }

    /// Wait for the child's ready marker, failing if it exits first.
    #[cfg(unix)]
    fn wait_for_child_marker(marker: &Path, child: &mut Child, deadline: Instant) {
        loop {
            if marker.exists() {
                return;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!("the child exited before it was ready: {status:?}");
            }
            assert!(
                Instant::now() < deadline,
                "the child did not become ready in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Deliver a real `SIGUSR1` to a child this test spawned.
    ///
    /// `kill(1)` is used because the crate is `#![forbid(unsafe_code)]` and the
    /// standard library cannot name a signal. `None` means `kill` is not
    /// available on this host, so the caller skips; `Some(false)` means the
    /// signal could not be delivered to a child that was just alive, which is
    /// a real failure rather than a skip.
    #[cfg(unix)]
    fn send_sigusr1(child: &Child) -> Option<bool> {
        match Command::new("kill")
            .args(["-USR1", &child.id().to_string()])
            .status()
        {
            Ok(status) => Some(status.success()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot run kill(1): {error}"),
        }
    }

    /// Bounded wait for the child to exit; `None` means it outlived the bound.
    #[cfg(unix)]
    fn wait_for_child_exit(
        child: &mut Child,
        deadline: Instant,
    ) -> Option<std::process::ExitStatus> {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(error) => panic!("cannot observe the child: {error}"),
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Kill and reap a child this test spawned, then report its stderr.
    #[cfg(unix)]
    fn reap_child(child: &mut Child) -> String {
        let _ = child.kill();
        let _ = child.wait();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        stderr
    }

    /// Child-process half of the real-signal tests.
    ///
    /// The parent re-executes this test binary with [`SIGUSR1_CHILD_ENV`]; a
    /// normal harness run does nothing. Modes:
    ///
    /// - `report`: start a monitor, publish readiness, wait for a real
    ///   `SIGUSR1`, write the captured report, and return so libtest exits 0.
    /// - `stopped`: start and stop a monitor, publish readiness, then stay
    ///   alive. The documented contract is that `SIGUSR1` terminates the
    ///   process once no monitor is registered, so the harness fails if this
    ///   function ever returns.
    #[cfg(unix)]
    #[test]
    fn sigusr1_child_process_helper() {
        let Ok(mode) = std::env::var(SIGUSR1_CHILD_ENV) else {
            return;
        };
        let dir = std::env::var_os(SIGUSR1_CHILD_DIR_ENV)
            .map(PathBuf::from)
            .expect("the parent passes the child directory");
        let ready = dir.join("ready");
        let (tx, rx) = mpsc::channel::<String>();
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(60_000),
            Arc::new(HangGauges::new()),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        watchdog.idle();
        if mode == "stopped" {
            watchdog
                .stop_and_join()
                .expect("the monitor thread must exit cleanly");
            std::fs::write(&ready, b"stopped\n").expect("ready marker");
            std::thread::sleep(SIGUSR1_CHILD_TIMEOUT);
            panic!("SIGUSR1 was ignored after the monitor stopped");
        }
        std::fs::write(&ready, b"report\n").expect("ready marker");
        let line = rx
            .recv_timeout(SIGUSR1_CHILD_TIMEOUT)
            .expect("a real SIGUSR1 must reach the running monitor");
        assert!(line.starts_with(SIGNAL_REPORT_PREFIX), "{line}");
        std::fs::write(dir.join("report.log"), line.as_bytes()).expect("report file");
    }

    /// End-to-end real-signal coverage of the capture path: a child process
    /// runs a monitor, receives a real `SIGUSR1`, writes the report, and exits
    /// 0. The signal is never raised in the test harness itself.
    #[cfg(unix)]
    #[test]
    fn sigusr1_reaches_the_monitor_in_a_child_process() {
        let dir = sigusr1_child_dir("report");
        let Some(mut child) = spawn_sigusr1_child("report", &dir) else {
            eprintln!("skipping SIGUSR1 delivery test: cannot re-exec the test binary");
            return;
        };
        let deadline = Instant::now() + SIGUSR1_CHILD_TIMEOUT;
        wait_for_child_marker(&dir.join("ready"), &mut child, deadline);
        match send_sigusr1(&child) {
            Some(true) => {}
            Some(false) => {
                let stderr = reap_child(&mut child);
                panic!("kill -USR1 failed for a live child\n{stderr}");
            }
            None => {
                let stderr = reap_child(&mut child);
                let _ = std::fs::remove_dir_all(&dir);
                eprintln!("skipping SIGUSR1 delivery test: kill(1) unavailable\n{stderr}");
                return;
            }
        }
        let status = match wait_for_child_exit(&mut child, deadline) {
            Some(status) => status,
            None => {
                let stderr = reap_child(&mut child);
                panic!("the child outlived the SIGUSR1 deadline\n{stderr}");
            }
        };
        let stderr = reap_child(&mut child);
        assert!(status.success(), "the child failed: {status:?}\n{stderr}");
        let report = std::fs::read_to_string(dir.join("report.log"))
            .expect("the child must write the captured report");
        assert!(report.starts_with(SIGNAL_REPORT_PREFIX), "{report}");
        assert!(report.contains("reason=sigusr1"), "{report}");
        assert!(report.contains("trigger=sigusr1"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end real-signal coverage of the post-shutdown contract: with no
    /// monitor registered, a real `SIGUSR1` terminates the process by the
    /// signal instead of being swallowed by the registry dispatcher.
    #[cfg(unix)]
    #[test]
    fn sigusr1_terminates_a_child_after_the_monitor_stopped() {
        use signal_hook::consts::SIGUSR1;
        use std::os::unix::process::ExitStatusExt;

        let dir = sigusr1_child_dir("stopped");
        let Some(mut child) = spawn_sigusr1_child("stopped", &dir) else {
            eprintln!("skipping post-shutdown SIGUSR1 test: cannot re-exec the test binary");
            return;
        };
        let deadline = Instant::now() + SIGUSR1_CHILD_TIMEOUT;
        wait_for_child_marker(&dir.join("ready"), &mut child, deadline);
        match send_sigusr1(&child) {
            Some(true) => {}
            Some(false) => {
                let stderr = reap_child(&mut child);
                panic!("kill -USR1 failed for a live child\n{stderr}");
            }
            None => {
                let stderr = reap_child(&mut child);
                let _ = std::fs::remove_dir_all(&dir);
                eprintln!("skipping post-shutdown SIGUSR1 test: kill(1) unavailable\n{stderr}");
                return;
            }
        }
        let status = match wait_for_child_exit(&mut child, deadline) {
            Some(status) => status,
            None => {
                let stderr = reap_child(&mut child);
                panic!("the child swallowed SIGUSR1 after the monitor stopped\n{stderr}");
            }
        };
        let stderr = reap_child(&mut child);
        assert_eq!(
            status.signal(),
            Some(SIGUSR1),
            "the documented post-shutdown action is termination by SIGUSR1: {status:?}\n{stderr}"
        );
        assert!(
            status.code().is_none(),
            "termination must not look like a normal exit: {status:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_phase_bounds_tighten_tick_and_command() {
        let config = WatchdogConfig::default_threshold();
        assert_eq!(
            config.bound_for(Phase::RuntimeTick),
            HANG_WATCHDOG_TICK_BOUND
        );
        assert_eq!(
            config.bound_for(Phase::ColdCommands),
            HANG_WATCHDOG_COMMAND_BOUND
        );
        assert_eq!(
            config.bound_for(Phase::PluginTimers),
            HANG_WATCHDOG_DEFAULT_BOUND
        );
        // A shorter operator threshold still wins.
        let tight = WatchdogConfig::with_threshold_ms(500);
        assert_eq!(
            tight.bound_for(Phase::RuntimeTick),
            Duration::from_millis(500)
        );
        // ...and raising the threshold raises the generic bound: the named
        // per-phase bounds are caps, not a second default (CodeRabbit #2834).
        let raised = WatchdogConfig::with_threshold_ms(60_000);
        assert_eq!(
            raised.bound_for(Phase::PluginTimers),
            Duration::from_secs(60)
        );
        assert_eq!(
            raised.bound_for(Phase::RuntimeTick),
            HANG_WATCHDOG_TICK_BOUND
        );
        assert_eq!(
            raised.bound_for(Phase::ColdCommands),
            HANG_WATCHDOG_COMMAND_BOUND
        );
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
        let shared = Shared::new(Arc::new(HangGauges::new()));
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
            reason: DiagnosticReason::Stall,
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
                surface_visible: true,
                redraw_pending_ms: Some(750),
                probe_redraw_seen: true,
                ..Default::default()
            }),
            gauges: HangGaugeSnapshot {
                tick_count: 41,
                last_tick_ns: 4_000_000,
                plugin_parked_commands: 1,
                plugin_active_commands: 1,
                ..HangGaugeSnapshot::default()
            },
            threads: rows,
            totals,
            delivery: DeliverySummary {
                attempts: 2,
                failures: 1,
                last: Some(DeliveryOutcome::TimedOut),
            },
            pending: PendingWork {
                pty: true,
                replies: true,
                hidden_damage: true,
            },
            present_stale_ms: Some(12_345),
            tick_gap: None,
        };
        let rendered = render_report(&report);
        assert!(rendered.contains("phase=runtime-tick"), "{rendered}");
        // bitty#2830: the parked-work line names the bitset, the stuck-frame
        // span, and what fired.
        assert!(
            rendered.contains(
                "parked: pending=hidden-damage|pty|replies present_stale_ms=12345 trigger=stall"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("[main]"), "{rendered}");
        assert!(
            rendered.contains("watchdog-delivery: attempts=2 failures=1 last=timed-out"),
            "{rendered}"
        );
        assert!(rendered.len() <= MAX_REPORT_BYTES, "{}", rendered.len());
        assert!(rendered.contains("surface: last=none"), "{rendered}");
        assert!(
            rendered.contains(
                "visible=true redraw_pending_ms=750 probe_seen=true probe_occluded=false"
            ),
            "{rendered}"
        );
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
            reason: DiagnosticReason::Stall,
            threshold: DEFAULT_STALL_THRESHOLD,
            idle: Duration::from_secs(3),
            phase: Phase::RuntimeTick,
            app_uptime: Duration::from_secs(7200),
            progress: 42,
            context: None,
            gauges: HangGaugeSnapshot::default(),
            threads: rows,
            totals: ProcessTotals::default(),
            delivery: DeliverySummary {
                attempts: 3,
                failures: 2,
                last: Some(DeliveryOutcome::EvidenceFailed),
            },
            pending: PendingWork::default(),
            present_stale_ms: None,
            tick_gap: None,
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
            Arc::new(HangGauges::new()),
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
        assert!(recovered.contains("hang recovered"), "{recovered}");
        watchdog
            .stop_and_join()
            .expect("monitor thread must exit cleanly");
    }

    /// bitty#2830 acceptance: a blocked stage writes exactly one report, and
    /// that report names every field the issue requires.
    #[test]
    fn blocked_stage_writes_one_complete_dump() {
        let (tx, rx) = mpsc::channel::<String>();
        let gauges = Arc::new(HangGauges::new());
        gauges.set_input_commands(2, 1);
        gauges.set_pending_exits(3);
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(100),
            Arc::clone(&gauges),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        // Simulate the runtime tick that never returns: the main thread stops
        // advancing the phase while the monitor keeps sampling.
        watchdog.enter(Phase::RuntimeTick);
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked tick must produce a dump");
        assert!(first.starts_with(STALL_REPORT_PREFIX), "{first}");
        assert!(first.contains("phase=runtime-tick"), "{first}");
        assert!(first.contains("reason=stall"), "{first}");
        // Runtime gauges (bitty#2830 field set).
        for key in [
            "runtime: tick_count=",
            "last_tick_ms=",
            "pending_input_bytes=",
            "cold_queue=",
            "plugin_events=",
            "plugin_parked=",
            "plugin_active=",
            "component_events=",
            "panes=",
            "content_bytes=",
            "event_ring=",
            "pending_deliveries=",
            "workers=",
            "leases=",
            "bindings=",
            "input_commands=2",
            "input_commands_dropped=1",
            "pending_exits=3",
        ] {
            assert!(first.contains(key), "missing {key}: {first}");
        }
        // Process evidence.
        assert!(first.contains("fds="), "{first}");
        assert!(first.contains("rss="), "{first}");
        #[cfg(target_os = "linux")]
        {
            assert!(first.contains("tid="), "{first}");
            assert!(first.contains("[main]"), "{first}");
            assert!(first.contains("state="), "{first}");
        }
        // Still blocked: the one-shot contract forbids a second report.
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "a continuous stall must not write a second dump"
        );
        watchdog.idle();
        let recovered = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recovery line");
        assert!(recovered.contains("hang recovered"), "{recovered}");
        watchdog
            .stop_and_join()
            .expect("monitor thread must exit cleanly");
    }

    /// bitty#2830: each parked-work source renders its own bit, in one stable
    /// order, and yields `none` when nothing is outstanding.
    #[test]
    fn pending_work_bitset_names_each_source() {
        assert_eq!(PendingWork::default().render(), "none");
        assert!(!PendingWork::default().any());
        let cases = [
            (
                PendingWork {
                    pty: true,
                    ..PendingWork::default()
                },
                "pty",
            ),
            (
                PendingWork {
                    replies: true,
                    ..PendingWork::default()
                },
                "replies",
            ),
            (
                PendingWork {
                    hidden_damage: true,
                    ..PendingWork::default()
                },
                "hidden-damage",
            ),
        ];
        for (work, expected) in cases {
            assert!(work.any());
            assert_eq!(work.render(), expected);
        }
        let all = PendingWork::from_gauges(&HangGaugeSnapshot {
            pending_input_bytes: 64,
            pending_replies: 12,
            hidden_damage: 1,
            ..HangGaugeSnapshot::default()
        });
        assert_eq!(all.render(), "hidden-damage|pty|replies");
    }

    /// bitty#2830: a park is only quiet when nothing is outstanding. A queue
    /// reply still waiting to be written keeps the sample stall-eligible, and
    /// the report names the `replies` bit.
    #[test]
    fn parked_replies_are_not_a_quiet_window() {
        let (tx, rx) = mpsc::channel::<String>();
        let gauges = Arc::new(HangGauges::new());
        gauges.set_pending_replies(24);
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(60),
            Arc::clone(&gauges),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        // The loop is parked in `ControlFlow::Wait` with an unwritten reply.
        watchdog.idle();
        let line = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("parked work must report even while idle");
        assert!(line.starts_with(STALL_REPORT_PREFIX), "{line}");
        assert!(line.contains("reason=stall"), "{line}");
        assert!(line.contains("pending=replies"), "{line}");
        assert!(line.contains("trigger=stall"), "{line}");
        watchdog
            .stop_and_join()
            .expect("monitor thread must exit cleanly");
    }

    /// bitty#2830 hypothesis 2: the phase heartbeat keeps advancing (output is
    /// parsed on every tick) while a hidden surface never presents. The
    /// presented-frame counter is the only signal that catches it, and the
    /// report must name `trigger=no-present` with the stuck span.
    #[test]
    fn hidden_surface_with_stuck_frames_reports_no_present_while_progress_advances() {
        let (tx, rx) = mpsc::channel::<String>();
        let gauges = Arc::new(HangGauges::new());
        gauges.set_hidden_damage(true);
        // A generous threshold keeps a loaded runner from turning a scheduling
        // gap between heartbeat marks into a phase stall: this test is about
        // the no-present detector, not about the phase bound.
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(1_000),
            Arc::clone(&gauges),
            Box::new(move |line| {
                let _ = tx.send(line.to_string());
                DeliveryOutcome::Delivered
            }),
        );
        // Healthy heartbeat: the hidden loop keeps advancing (parsed output
        // and the repair wake both mark progress). Only the frame counter is
        // stuck, so the stall detector alone would never fire.
        let mut line = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            watchdog.enter(Phase::PluginTimers);
            if let Ok(report) = rx.recv_timeout(Duration::from_millis(20)) {
                line = Some(report);
                break;
            }
        }
        let line = line.unwrap_or_else(|| panic!("a hidden surface with stuck frames must report"));
        // Visible with `--nocapture`: the exact dump an operator gets.
        eprintln!("{line}");
        assert!(line.contains("reason=no-present"), "{line}");
        assert!(line.contains("trigger=no-present"), "{line}");
        assert!(line.contains("pending=hidden-damage"), "{line}");
        assert!(line.contains("present_stale_ms="), "{line}");
        assert!(!line.contains("present_stale_ms=none"), "{line}");
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
            // Observe the slot exactly once per write: on Windows the monitor
            // publishes the next report with `MoveFileEx`, which is not an
            // atomic replacement, so a second `exists()` can fall into the
            // window where the destination name is unbound and report a
            // violation that never happened.
            let state_present = self.state_path.exists();
            let first = !self.first_write_seen.swap(true, Ordering::SeqCst);
            if first {
                self.first_write_saw_state
                    .store(state_present, Ordering::SeqCst);
            }
            if state_present {
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
            Arc::new(HangGauges::new()),
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
        // `write_report_to` deliberately attempts stderr even when the state
        // commit failed, so the per-write invariant is the ordering of that
        // attempt, not the presence of a report on every soak pass: a soak
        // pass can legitimately find no freshly published slot (a failed
        // commit, or - on Windows - the window while the monitor replaces the
        // slot with `MoveFileEx`). The first-write probe above is the
        // non-vacuous ordering assertion; the per-emit ordering is covered
        // deterministically by `report_sink_persists_state_before_a_broken_stderr_write`.
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
            Arc::new(HangGauges::new()),
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
            Arc::new(HangGauges::new()),
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

    #[test]
    fn idle_without_pending_work_parks_monitor_at_zero_hz() {
        let gauges = Arc::new(HangGauges::new());
        let watchdog = StallWatchdog::start(
            WatchdogConfig::with_threshold_ms(50),
            Arc::clone(&gauges),
            Box::new(|_| DeliveryOutcome::Delivered),
        );
        watchdog.idle();
        // Wait until the monitor computes `idle_park == true` and parks at 0 Hz.
        let settle_deadline = Instant::now() + Duration::from_secs(1);
        while !watchdog.shared.idle_parked.load(Ordering::Acquire)
            && Instant::now() < settle_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            watchdog.shared.idle_parked.load(Ordering::Acquire),
            "monitor must reach idle_park state"
        );
        let parked_polls = watchdog.shared.polls.load(Ordering::Relaxed);

        // Over 10x the 12 ms poll interval, a parked monitor must perform 0 wakeups.
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            watchdog.shared.polls.load(Ordering::Relaxed),
            parked_polls,
            "idle monitor with no pending work must park at 0 Hz (PB-7)"
        );

        // Entering an active phase wakes the monitor immediately.
        watchdog.enter(Phase::RuntimeTick);
        let wake_deadline = Instant::now() + Duration::from_secs(1);
        while watchdog.shared.polls.load(Ordering::Relaxed) == parked_polls
            && Instant::now() < wake_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            watchdog.shared.polls.load(Ordering::Relaxed) > parked_polls,
            "entering an active phase must wake the parked monitor"
        );
        watchdog.stop_and_join().expect("monitor thread joined");
    }
}
