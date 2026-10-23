//! Bounded crash and exit diagnostics (issue #2570).
//!
//! The owner's 0.0.27 instance disappeared without a core, without a journal
//! entry, and without an exit-save (graphical launch loses stderr). The
//! opt-in stall watchdog was not enabled, so the next occurrence would have
//! stayed just as invisible. This module installs a process-wide panic hook
//! and writes one bounded, path-redacted report under the XDG state directory
//! so a panic, an abort path, or a clean event-loop exit leaves durable
//! evidence.
//!
//! Design constraints:
//!
//! - **Bounded.** One fixed-size slot (8 KiB crash report, 1 KiB exit line);
//!   the report is truncated at a char boundary and never appended without
//!   bound.
//! - **No hot-path work.** A panic hook and an exit hook only; normal frames
//!   do not touch this module. Lifecycle marks are one relaxed atomic store.
//! - **No new authority.** Writes only inside the user's own XDG state
//!   directory with `0600` permissions on Unix. No host path is retained:
//!   locations keep the file name only, the home directory is replaced with
//!   `~`, and the stall-watchdog secret scrubber runs before the write.
//! - **Default-on, opt-out.** `BITTY_CRASH_REPORT=0|false|off|no` disables it.
//!   It is deliberately not gated behind the stall watchdog: the reported
//!   process showed no stall log because the watchdog is opt-in.
//! - **Signal limitation.** This crate is `#![forbid(unsafe_code)]`, so it
//!   cannot install an async-signal-safe handler for `SIGTERM`/`SIGHUP`.
//!   A terminating signal therefore still leaves only the last exit/phase
//!   record; the follow-up task owns a signal-safe design.
//!
//! The surface counters come from [`bitty_render::telemetry`], the phase from
//! the same coarse vocabulary as the stall watchdog, and the lifecycle marks
//! from the window-event sites in [`crate::terminal_app`].

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::stall_watchdog::{self, Phase, ThreadRow};

/// Environment switch that disables the crash report.
const CRASH_REPORT_ENV: &str = "BITTY_CRASH_REPORT";
/// Durable panic slot name under the XDG state directory.
const CRASH_FILE_NAME: &str = "crash-report.log";
/// Durable exit slot name under the XDG state directory.
const EXIT_FILE_NAME: &str = "exit-report.log";
/// Hard cap for one crash report.
const MAX_CRASH_BYTES: usize = 8 * 1024;
/// Hard cap for one exit line.
const MAX_EXIT_BYTES: usize = 1024;
/// Maximum panic message characters retained.
const MAX_MESSAGE_CHARS: usize = 512;
/// Maximum backtrace frame rows retained.
const MAX_BACKTRACE_FRAMES: usize = 48;

/// Coarse lifecycle event, stored as one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Lifecycle {
    Startup = 0,
    Resumed = 1,
    Suspended = 2,
    WindowCreated = 3,
    GpuAttached = 4,
    GpuAttachFailed = 5,
    Resized = 6,
    ScaleChanged = 7,
    Focused = 8,
    Unfocused = 9,
    CloseRequested = 10,
    Closed = 11,
    SurfaceRetry = 12,
    Exit = 13,
}

impl Lifecycle {
    const COUNT: usize = 14;

    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Resumed,
            2 => Self::Suspended,
            3 => Self::WindowCreated,
            4 => Self::GpuAttached,
            5 => Self::GpuAttachFailed,
            6 => Self::Resized,
            7 => Self::ScaleChanged,
            8 => Self::Focused,
            9 => Self::Unfocused,
            10 => Self::CloseRequested,
            11 => Self::Closed,
            12 => Self::SurfaceRetry,
            13 => Self::Exit,
            _ => Self::Startup,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resumed => "resumed",
            Self::Suspended => "suspended",
            Self::WindowCreated => "window-created",
            Self::GpuAttached => "gpu-attached",
            Self::GpuAttachFailed => "gpu-attach-failed",
            Self::Resized => "resized",
            Self::ScaleChanged => "scale-changed",
            Self::Focused => "focused",
            Self::Unfocused => "unfocused",
            Self::CloseRequested => "close-requested",
            Self::Closed => "closed",
            Self::SurfaceRetry => "surface-retry",
            Self::Exit => "exit",
        }
    }
}

/// Clean exit reason, stored as one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ExitReason {
    ShellExit = 0,
    CloseView = 1,
    MouseAction = 2,
    RouteEvent = 3,
    LoopExiting = 4,
    EventLoopReturned = 5,
    DisplayFallback = 6,
    WindowClose = 7,
    Unknown = 8,
}

impl ExitReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ShellExit => "shell-exit",
            Self::CloseView => "close-view",
            Self::MouseAction => "mouse-action",
            Self::RouteEvent => "route-event",
            Self::LoopExiting => "loop-exiting",
            Self::EventLoopReturned => "event-loop-returned",
            Self::DisplayFallback => "display-fallback",
            Self::WindowClose => "window-close",
            Self::Unknown => "unknown",
        }
    }
}

static INSTALLED: Once = Once::new();
/// Enabled only after [`install`] runs in the real binary; tests and headless
/// runs never write crash/exit slots into the user's state directory.
static ENABLED: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU8 = AtomicU8::new(Phase::Startup as u8);
static LAST_LIFECYCLE: AtomicU8 = AtomicU8::new(Lifecycle::Startup as u8);
static EXIT_RECORDED: AtomicBool = AtomicBool::new(false);
static LIFECYCLE_COUNTS: [AtomicU64; Lifecycle::COUNT] =
    [const { AtomicU64::new(0) }; Lifecycle::COUNT];

/// Parse the opt-out switch. Absent means enabled.
fn enabled_from_values(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") => true,
        Some(value)
            if value.eq_ignore_ascii_case("0")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("no") =>
        {
            false
        }
        Some(_) => true,
    }
}

fn enabled_from_env() -> bool {
    enabled_from_values(std::env::var(CRASH_REPORT_ENV).ok().as_deref())
}

/// Install the panic hook once. Safe to call more than once.
pub(crate) fn install() {
    if !enabled_from_env() {
        ENABLED.store(false, Ordering::Relaxed);
        return;
    }
    INSTALLED.call_once(|| {
        ENABLED.store(true, Ordering::Relaxed);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            write_panic_report(info);
            previous(info);
        }));
    });
}

/// Record the active main-loop phase (one relaxed store).
pub(crate) fn set_phase(phase: Phase) {
    PHASE.store(phase as u8, Ordering::Relaxed);
}

/// Record one window/surface lifecycle event.
pub(crate) fn lifecycle(event: Lifecycle) {
    LAST_LIFECYCLE.store(event as u8, Ordering::Relaxed);
    let slot = event as usize;
    if let Some(counter) = LIFECYCLE_COUNTS.get(slot) {
        let _ = counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record the event-loop exit reason and its bounded durable line.
///
/// Called on every explicit exit path; the panic hook overwrites the crash
/// slot, so a panic and a clean exit stay distinguishable.
pub(crate) fn record_exit(reason: ExitReason) {
    // The first exit path is the specific one (shell-exit, close-view,
    // route-event); the event loop then returns and `main` would otherwise
    // overwrite it with the generic `event-loop-returned`.
    if EXIT_RECORDED.swap(true, Ordering::Relaxed) {
        return;
    }
    if ENABLED.load(Ordering::Relaxed) {
        let line = render_exit_line(reason);
        if let Some(path) = state_path(EXIT_FILE_NAME) {
            let _ = write_slot(&path, &line, MAX_EXIT_BYTES);
        }
    }
    // Mark the exit after the line is rendered: the report's `lifecycle` field
    // then names the last real window event (close-requested/closed) rather
    // than the exit marker itself.
    lifecycle(Lifecycle::Exit);
}

/// The last recorded lifecycle event name (bounded, static).
pub(crate) fn last_lifecycle_name() -> &'static str {
    Lifecycle::from_u8(LAST_LIFECYCLE.load(Ordering::Relaxed)).as_str()
}

/// The last recorded phase (test observability).
#[cfg(test)]
fn phase() -> Phase {
    Phase::from_u8(PHASE.load(Ordering::Relaxed))
}

/// The last recorded lifecycle event (test observability).
#[cfg(test)]
fn last_lifecycle() -> Lifecycle {
    Lifecycle::from_u8(LAST_LIFECYCLE.load(Ordering::Relaxed))
}

fn write_panic_report(info: &std::panic::PanicHookInfo<'_>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(path) = state_path(CRASH_FILE_NAME) else {
        return;
    };
    let report = render_crash_report(info);
    let _ = write_slot(&path, &report, MAX_CRASH_BYTES);
}

/// Render the bounded panic report. Pure apart from the supplied `info`.
fn render_crash_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    let message = panic_message(info);
    let location = panic_location(info);
    let backtrace = render_backtrace();
    render_crash_report_from(&message, location.as_deref(), &backtrace)
}

/// Pure renderer used by the hook and tests.
fn render_crash_report_from(message: &str, location: Option<&str>, backtrace: &str) -> String {
    let phase = Phase::from_u8(PHASE.load(Ordering::Relaxed));
    let lifecycle = Lifecycle::from_u8(LAST_LIFECYCLE.load(Ordering::Relaxed));
    let surface = bitty_render::telemetry::snapshot();
    let (rows, totals) = stall_watchdog::sample_process_totals();

    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "bitty: crash report");
    let _ = writeln!(out, "  kind=panic");
    let _ = writeln!(
        out,
        "  message={}",
        sanitize_text(message, MAX_MESSAGE_CHARS)
    );
    let _ = writeln!(
        out,
        "  location={}",
        location.map_or("unknown", |value| value)
    );
    let _ = writeln!(out, "  phase={}", phase.as_str());
    let _ = writeln!(out, "  lifecycle={}", render_lifecycle(lifecycle));
    let _ = writeln!(out, "  surface={}", render_surface(&surface));
    let _ = writeln!(
        out,
        "  process: threads={} fds={} rss={} KiB",
        totals.thread_count, totals.fd_count, totals.rss_kib,
    );
    render_threads(&mut out, &rows);
    if !backtrace.is_empty() {
        let _ = writeln!(out, "  backtrace:");
        out.push_str(backtrace);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    truncate(out, MAX_CRASH_BYTES)
}

/// Render one bounded exit line.
fn render_exit_line(reason: ExitReason) -> String {
    let phase = Phase::from_u8(PHASE.load(Ordering::Relaxed));
    let lifecycle = Lifecycle::from_u8(LAST_LIFECYCLE.load(Ordering::Relaxed));
    let surface = bitty_render::telemetry::snapshot();
    format!(
        "bitty: exit reason={} phase={} lifecycle={} surface={}\n",
        reason.as_str(),
        phase.as_str(),
        lifecycle.as_str(),
        render_surface(&surface),
    )
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        String::from("<non-string panic payload>")
    }
}

/// Keep only the file name and line/column; never a checkout or home path.
fn panic_location(info: &std::panic::PanicHookInfo<'_>) -> Option<String> {
    let location = info.location()?;
    let file = Path::new(location.file())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    Some(format!("{file}:{}:{}", location.line(), location.column()))
}

/// Render a bounded backtrace without source paths.
///
/// `Backtrace::frames()` is still unstable, so the stable `Display` form is
/// captured and each `at /path/file.rs:line` source path is reduced to its
/// file name. Function names and line numbers stay useful; no checkout path
/// reaches the report.
fn render_backtrace() -> String {
    let captured = std::backtrace::Backtrace::force_capture();
    let rendered = captured.to_string();
    let mut out = String::with_capacity(1024);
    for line in rendered.lines().take(MAX_BACKTRACE_FRAMES) {
        out.push_str(&strip_source_path(&redact_home(line)));
        out.push('\n');
        if out.len() >= MAX_CRASH_BYTES / 2 {
            let _ = writeln!(out, "  ... backtrace truncated");
            break;
        }
    }
    out
}

/// Reduce `... at /checkout/path/file.rs:42` or `... at ./checkout/file.rs:42`
/// to `... at file.rs:42`.
fn strip_source_path(line: &str) -> String {
    let Some(index) = line.rfind(" at ") else {
        return line.to_string();
    };
    let head = &line[..index + 4];
    let tail = &line[index + 4..];
    let file = tail.rsplit('/').next().unwrap_or(tail);
    format!("{head}{file}")
}

fn render_lifecycle(last: Lifecycle) -> String {
    let mut out = format!("last={}", last.as_str());
    for raw in 0u8..u8::try_from(Lifecycle::COUNT).unwrap_or(u8::MAX) {
        let event = Lifecycle::from_u8(raw);
        let count = LIFECYCLE_COUNTS[raw as usize].load(Ordering::Relaxed);
        if count > 0 {
            let _ = write!(out, " {}={count}", event.as_str());
        }
    }
    out
}

fn render_surface(surface: &bitty_render::telemetry::SurfaceTelemetrySnapshot) -> String {
    format!(
        "last={} configure_ok={} configure_err={} acquire_ok={} acquire_timeout={} acquire_outdated={} acquire_lost={} acquire_other={} present_ok={} present_err={} resize={} resize_zero_skipped={} headless_present={}",
        surface.last_event,
        surface.configure_ok,
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
    )
}

fn render_threads(out: &mut String, rows: &[ThreadRow]) {
    if rows.is_empty() {
        let _ = writeln!(out, "  thread-state snapshot unavailable on this platform");
        return;
    }
    let main_tid = u64::from(std::process::id());
    for row in rows {
        let marker = if row.tid == main_tid { "[main] " } else { "" };
        let _ = writeln!(
            out,
            "  {marker}tid={} state={} wchan={} syscall_number={} comm={}",
            row.tid, row.state, row.wchan, row.syscall_number, row.comm,
        );
    }
}

/// One-line, bounded, control-character-free text.
fn sanitize_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(raw.len()));
    for ch in raw.chars().take(max_chars) {
        out.push(if ch.is_control() { '?' } else { ch });
    }
    redact_home(&out)
}

/// Replace the user's home prefix; the report never retains a host path.
fn redact_home(text: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return text.to_string();
    };
    if home.len() <= 1 {
        return text.to_string();
    }
    text.replace(&home, "~")
}

fn truncate(mut text: String, max_bytes: usize) -> String {
    const SUFFIX: &str = "\n  ... report truncated";
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes.saturating_sub(SUFFIX.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(SUFFIX);
    text
}

fn state_path(name: &str) -> Option<PathBuf> {
    stall_watchdog::state_file_path_for(name)
}

/// Atomically replace one report slot, capped at `max_bytes`.
fn write_slot(path: &Path, text: &str, max_bytes: usize) -> std::io::Result<()> {
    let bounded = truncate(text.to_string(), max_bytes);
    let redacted = crate::observability::redact_text_for_stderr(&bounded).into_owned();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    {
        let mut file = options.open(&tmp)?;
        file.write_all(redacted.as_bytes())?;
        file.flush()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_out_switch_defaults_to_enabled() {
        assert!(enabled_from_values(None));
        assert!(enabled_from_values(Some("")));
        assert!(enabled_from_values(Some("1")));
        assert!(!enabled_from_values(Some("0")));
        assert!(!enabled_from_values(Some("off")));
        assert!(!enabled_from_values(Some("FALSE")));
        assert!(!enabled_from_values(Some("no")));
    }

    #[test]
    fn lifecycle_round_trips_and_names_are_stable() {
        for raw in 0u8..u8::try_from(Lifecycle::COUNT).unwrap_or(u8::MAX) {
            let event = Lifecycle::from_u8(raw);
            assert_eq!(event as u8, raw);
            assert!(!event.as_str().is_empty());
        }
        assert_eq!(Lifecycle::from_u8(u8::MAX), Lifecycle::Startup);
    }

    #[test]
    fn exit_reasons_are_stable_and_bounded() {
        for reason in [
            ExitReason::ShellExit,
            ExitReason::CloseView,
            ExitReason::MouseAction,
            ExitReason::RouteEvent,
            ExitReason::LoopExiting,
            ExitReason::EventLoopReturned,
            ExitReason::DisplayFallback,
            ExitReason::WindowClose,
            ExitReason::Unknown,
        ] {
            assert!(!reason.as_str().is_empty());
        }
        let line = render_exit_line(ExitReason::ShellExit);
        assert!(line.contains("reason=shell-exit"));
        assert!(line.len() <= MAX_EXIT_BYTES);
    }

    #[test]
    fn crash_report_is_bounded_and_names_phase_and_surface() {
        let message = "x".repeat(MAX_MESSAGE_CHARS * 2);
        let backtrace = "#0 frame at file.rs:1\n".repeat(MAX_BACKTRACE_FRAMES * 16);
        let report = render_crash_report_from(&message, Some("main.rs:42:7"), &backtrace);
        assert!(report.len() <= MAX_CRASH_BYTES, "{}", report.len());
        assert!(report.contains("kind=panic"));
        assert!(report.contains("location=main.rs:42:7"));
        assert!(report.contains("phase="));
        assert!(report.contains("surface=last="));
        assert!(report.contains("lifecycle=last="));
        assert!(report.contains("backtrace:"));
        assert!(report.contains("report truncated"));
    }

    #[test]
    fn crash_report_redacts_the_home_prefix() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        if home.len() <= 1 {
            return;
        }
        let report = render_crash_report_from(
            &format!("failed to open {home}/secret"),
            Some("main.rs:1:1"),
            "",
        );
        assert!(!report.contains(&home), "{report}");
        assert!(report.contains("~/secret") || report.contains("~"));
    }

    #[test]
    fn backtrace_source_paths_keep_only_the_file_name() {
        let line = "  12: bitty_terminal::main\n             at /example/checkout/src/main.rs:42:5";
        let stripped = strip_source_path(line);
        assert!(stripped.contains("at main.rs:42:5"), "{stripped}");
        assert!(!stripped.contains("/example/checkout"), "{stripped}");
        let relative = "             at ./bitty/.worktrees/ctx-1/src/main.rs:42:5";
        let stripped = strip_source_path(relative);
        assert!(stripped.contains("at main.rs:42:5"), "{stripped}");
        assert!(!stripped.contains("worktrees"), "{stripped}");
        assert_eq!(
            strip_source_path("  no source path here"),
            "  no source path here"
        );
    }

    #[test]
    fn write_slot_replaces_atomically_and_is_user_only() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-crash-report-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("crash-report.log");
        write_slot(&path, "bitty: crash report\n", MAX_CRASH_BYTES).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("crash report"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn phase_and_lifecycle_marks_are_visible() {
        set_phase(Phase::RuntimeTick);
        lifecycle(Lifecycle::Resized);
        assert_eq!(phase(), Phase::RuntimeTick);
        assert_eq!(last_lifecycle(), Lifecycle::Resized);
        set_phase(Phase::Startup);
    }
}
