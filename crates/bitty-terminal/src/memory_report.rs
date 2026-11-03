//! Opt-in periodic memory attribution dump (bitty#2859).
//!
//! The idle baseline is a whole-process figure, so the attribution report in
//! `bitty_runtime::runtime::memory` is only useful if a real session writes it
//! out at a known cadence. This module owns that policy: the env knob, the
//! state-directory target, and the per-tick check.
//!
//! # Opt-in
//!
//! `BITTY_MEMORY_REPORT_SECS=<n>` (clamped to [`MIN_INTERVAL_SECS`]..
//! [`MAX_INTERVAL_SECS`]) makes the session write
//! `<state dir>/bitty/memory-report-<pid>.txt` every `n` seconds. Anything
//! else — unset, empty, `0`, malformed — leaves the dump off, matching the
//! other opt-in diagnostic knobs (`BITTY_DEMO_PUMP`, `BITTY_PERF_STARTUP_MARKER`).
//! The file name carries the pid so a worktree full of concurrent sessions does
//! not overwrite one report with another.
//!
//! # Cost
//!
//! The tick hook is one `Option` check plus one `Instant` compare. Nothing is
//! allocated, nothing is read from `/proc`, and no string is built unless a
//! report is actually due: the interval is resolved once at startup, and the
//! report itself (which reads `/proc/self/smaps*` and renders a bounded
//! string) runs at most once per interval.
//!
//! # Failure policy
//!
//! A diagnostic that cannot be written must not take the session down. The
//! first failure is reported once on stderr through
//! [`crate::logging::warn`]; later failures are silent, and the next interval
//! still tries again.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_runtime::plugin_runtime::PluginRuntime;
use bitty_runtime::runtime::Runtime;
use bitty_runtime::runtime::memory::{MemoryReport, PluginMemoryUsage};

/// Environment knob carrying the dump interval in seconds.
pub(crate) const MEMORY_REPORT_ENV: &str = "BITTY_MEMORY_REPORT_SECS";

/// Smallest accepted interval: one report per second is already far faster
/// than any human-scale soak mark.
pub(crate) const MIN_INTERVAL_SECS: u64 = 1;

/// Largest accepted interval (one day), so a typo cannot park a report forever.
pub(crate) const MAX_INTERVAL_SECS: u64 = 86_400;

/// File name under `<state dir>/bitty/`; the writer appends the pid.
pub(crate) const MEMORY_REPORT_FILE_HINT: &str = "memory-report.txt";

/// One armed periodic dump.
#[derive(Debug)]
pub(crate) struct MemoryReportDump {
    /// Seconds between reports.
    interval: Duration,
    /// Directory the report is written to.
    dir: PathBuf,
    /// Next permitted write.
    next_at: Instant,
}

impl MemoryReportDump {
    /// Resolves the dump from the process environment, or `None` when off.
    #[must_use]
    pub(crate) fn from_env() -> Option<Self> {
        Self::from_parts(
            std::env::var(MEMORY_REPORT_ENV).ok().as_deref(),
            crate::stall_watchdog::state_file_path_for(MEMORY_REPORT_FILE_HINT)
                .and_then(|path| path.parent().map(Path::to_path_buf)),
            Instant::now(),
        )
    }

    /// Pure resolution used by [`Self::from_env`] and the unit tests.
    ///
    /// `state_path` is the directory the state convention resolves to (the
    /// parent of the crash/exit report slot), so no host path is embedded
    /// here; `raw` is the knob's value.
    #[must_use]
    pub(crate) fn from_parts(
        raw: Option<&str>,
        state_path: Option<PathBuf>,
        now: Instant,
    ) -> Option<Self> {
        let interval = interval_from_value(raw)?;
        let dir = state_path?;
        Some(Self {
            interval,
            dir,
            // The first report is written as soon as the hook runs, so an
            // owner who starts a session and waits one interval already has a
            // baseline; the cadence then continues from there.
            next_at: now,
        })
    }

    /// The directory reports land in.
    #[must_use]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a report is due at `now`.
    #[must_use]
    pub(crate) fn due(&self, now: Instant) -> bool {
        now >= self.next_at
    }

    /// Records a successful write at `now`, arming the next interval.
    pub(crate) fn written(&mut self, now: Instant) {
        self.next_at = now + self.interval;
    }
}

/// Parses the knob value.
///
/// `None`, an empty value, `0`, and anything unparsable disable the dump; a
/// valid value is clamped into [`MIN_INTERVAL_SECS`]..[`MAX_INTERVAL_SECS`].
#[must_use]
pub(crate) fn interval_from_value(raw: Option<&str>) -> Option<Duration> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let secs = raw.parse::<u64>().ok()?;
    if secs == 0 {
        return None;
    }
    Some(Duration::from_secs(
        secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS),
    ))
}

/// Plugin Lua heap figures for the report, or nothing when no plugin runtime
/// is installed.
#[must_use]
pub(crate) fn plugin_usage(plugins: Option<&PluginRuntime>) -> PluginMemoryUsage {
    plugins.map_or_else(PluginMemoryUsage::default, PluginRuntime::lua_memory_usage)
}

/// Writes one report into `dir`, returning the path on success.
///
/// Split from [`MemoryReportDump`] so callers that already have a report (a
/// soak mark, a test) share the exact write path the session uses.
pub(crate) fn write_report(report: &MemoryReport, dir: &Path) -> std::io::Result<PathBuf> {
    report.write_to_dir(dir)
}

/// Whether one write failure has already been reported.
///
/// A broken state directory would otherwise print a warning every interval for
/// the life of the session.
pub(crate) fn note_failure(failed_once: &mut bool, dir: &Path, error: &std::io::Error) {
    if *failed_once {
        return;
    }
    *failed_once = true;
    let dir = dir.display();
    crate::logging::warn(|| format!("bitty: memory report could not be written to {dir}: {error}"));
}

/// Builds the report for the current tick.
///
/// Kept as a free function so the tick hook stays a single statement and the
/// report shape has one owner.
#[must_use]
pub(crate) fn current_report(runtime: &Runtime, plugins: Option<&PluginRuntime>) -> MemoryReport {
    runtime.memory_report_with_plugins(plugin_usage(plugins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_knob_is_off_unless_it_names_a_positive_interval() {
        assert_eq!(interval_from_value(None), None);
        assert_eq!(interval_from_value(Some("")), None);
        assert_eq!(interval_from_value(Some("   ")), None);
        assert_eq!(interval_from_value(Some("0")), None);
        assert_eq!(interval_from_value(Some("soon")), None);
        assert_eq!(interval_from_value(Some("-5")), None);
        assert_eq!(
            interval_from_value(Some("30")),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            interval_from_value(Some(" 30 ")),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn the_interval_is_clamped_to_its_accepted_range() {
        assert_eq!(
            interval_from_value(Some("1")),
            Some(Duration::from_secs(MIN_INTERVAL_SECS))
        );
        assert_eq!(
            interval_from_value(Some(&u64::MAX.to_string())),
            Some(Duration::from_secs(MAX_INTERVAL_SECS))
        );
    }

    #[test]
    fn a_configured_dump_is_due_immediately_and_then_on_cadence() {
        let now = Instant::now();
        let dir = std::env::temp_dir().join("bitty-memory-report-cadence");
        let mut dump = MemoryReportDump::from_parts(Some("10"), Some(dir.clone()), now)
            .expect("configured dump");
        assert_eq!(dump.dir(), dir.as_path());
        assert_eq!(dump.interval, Duration::from_secs(10));
        assert!(dump.due(now), "the first report is due immediately");
        dump.written(now);
        assert!(!dump.due(now));
        assert!(!dump.due(now + Duration::from_secs(9)));
        assert!(dump.due(now + Duration::from_secs(10)));
    }

    #[test]
    fn without_a_state_directory_the_dump_stays_off() {
        assert!(
            MemoryReportDump::from_parts(Some("30"), None, Instant::now()).is_none(),
            "a report with nowhere to land must not arm"
        );
        assert!(
            MemoryReportDump::from_parts(None, Some(PathBuf::from("/tmp")), Instant::now())
                .is_none(),
            "an unset knob must not arm"
        );
    }

    #[test]
    fn a_failure_is_reported_once() {
        let mut failed = false;
        note_failure(
            &mut failed,
            Path::new("/nonexistent"),
            &std::io::Error::other("x"),
        );
        assert!(failed);
        // The second call must be a no-op; `failed` stays true either way, so
        // the observable contract is the flag itself.
        note_failure(
            &mut failed,
            Path::new("/nonexistent"),
            &std::io::Error::other("x"),
        );
        assert!(failed);
    }

    #[test]
    fn no_plugin_runtime_means_no_lua_rows() {
        let usage = plugin_usage(None);
        assert_eq!(usage.lua_bytes, None);
        assert_eq!(usage.vms, 0);
    }
}
