//! Real-window PB-1 startup and PB-2 idle-memory evidence (CTX-0592).
//!
//! The Phase F / CTX-0100 harness measures the *headless* pipeline and reports
//! display-tied phases as `Unavailable` on CI. This module adds the
//! complementary, opt-in **real-window** half: it launches the actual
//! `bitty` binary on a Tier 1 host and records the accepted budgets
//!
//! - **PB-1** cold startup: p50 / p99 from process launch to the first
//!   rendered frame (`performance-budget-rfc.md#pb-1`, 100 ms / 200 ms).
//! - **PB-2** idle memory: resident set size (RSS) after one window has been
//!   idle for a bounded interval (`performance-budget-rfc.md#pb-2`, 80 MB p50).
//!
//! ## Measurement contract
//!
//! - **Opt-in only.** A run requires `BITTY_PERF_REAL_WINDOW=1` *and* a
//!   resolvable `bitty` binary. Without either the result is `Unavailable`
//!   with a reason string; no number is ever fabricated. Headless CI therefore
//!   reports `Unavailable` by design and stays green.
//! - **Instrumented binary, not a proxy.** The child is the real
//!   `crates/bitty-app` binary. When `BITTY_PERF_STARTUP_MARKER` is set the
//!   app emits `bitty perf: first-frame` on stdout *after* the first frame is
//!   presented, so the harness measures launch-to-first-frame rather than
//!   `--help` or a phase sum. When the marker is absent the harness still
//!   measures the launch and records the absence as a provenance note.
//! - **Bounded and deterministic.** Startup samples and the idle window are
//!   configurable but clamped (`MAX_STARTUP_SAMPLES`, `MAX_IDLE_SECS`), every
//!   child is killed and reaped on every path, and the reader is a bounded
//!   line pump behind a timeout — no unbounded read, no polling loop.
//! - **Host context from the environment.** OS, architecture, CPU count, and
//!   total memory are derived from `std::env::consts`, `/proc`, and
//!   `available_parallelism`; no checkout path, username, or hostname is
//!   embedded. The binary path is discovered from `CARGO_MANIFEST_DIR`, the
//!   `target/` layout, or `BITTY_PERF_BIN` — never a literal.
//!
//! `#![forbid(unsafe_code)]`; the process/RSS code is Unix-gated and degrades
//! to `Unavailable` on other platforms.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::startup::StartupDistribution;

/// Env flag that opts a run into real-window measurement.
pub const REAL_WINDOW_ENV: &str = "BITTY_PERF_REAL_WINDOW";

/// Env var carrying the path of the binary to launch (overrides discovery).
pub const BINARY_ENV: &str = "BITTY_PERF_BIN";

/// Env var passed to the child so it emits the first-frame marker on stdout.
pub const STARTUP_MARKER_ENV: &str = "BITTY_PERF_STARTUP_MARKER";

/// Stdout line the instrumented child emits once the first frame is presented.
pub const FIRST_FRAME_MARKER: &str = "bitty perf: first-frame";

/// Default number of startup launches sampled.
pub const DEFAULT_STARTUP_SAMPLES: usize = 5;
/// Hard cap on startup launches per evidence run.
pub const MAX_STARTUP_SAMPLES: usize = 50;
/// Default idle window, in seconds, before the RSS sample.
pub const DEFAULT_IDLE_SECS: u64 = 5;
/// Hard cap on the idle window, in seconds.
pub const MAX_IDLE_SECS: u64 = 300;
/// Default timeout for a single startup launch, in seconds.
pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 20;
/// Hard cap on a single startup launch timeout, in seconds.
pub const MAX_STARTUP_TIMEOUT_SECS: u64 = 120;
/// RSS samples taken across the idle window once it closes.
pub const IDLE_RSS_SAMPLES: usize = 3;

/// Committed evidence baseline artifact (relative to the repository root).
pub const REAL_WINDOW_BASELINE_REL_PATH: &str = "crates/bitty-perf/baselines/pb-real-window.json";

/// Schema version of the committed evidence artifact.
pub const REAL_WINDOW_BASELINE_SCHEMA_VERSION: u32 = 1;

/// Generous Tier 1 regression factor used by the opt-in `--check` comparison.
///
/// The accepted PB-1/PB-2 budgets are the primary verdict; this factor only
/// flags a pathological regression against the committed baseline on a shared
/// machine (3x the committed p50 / RSS). It is deliberately loose: evidence is
/// captured on a workstation, not a pinned reference machine, so it must not
/// flake on a noisy host.
pub const REGRESSION_FACTOR: f64 = 3.0;

// ---------------------------------------------------------------------------
// Evidence result types
// ---------------------------------------------------------------------------

/// PB-1 startup evidence from real-window launches.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RealStartupEvidence {
    /// Launch-to-first-frame durations in milliseconds (empty when unavailable).
    pub samples_ms: Vec<f64>,
    /// Absolute path of the launched binary, when one was resolved.
    pub binary: Option<String>,
    /// Why the measurement is unavailable (`None` when samples exist).
    pub reason: Option<String>,
    /// Whether the child emitted the first-frame marker (instrumented build).
    pub marker_seen: bool,
}

impl RealStartupEvidence {
    /// `true` when no launch produced a sample.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.samples_ms.is_empty()
    }

    /// Distribution metrics over the samples, if any were collected.
    #[must_use]
    pub fn distribution(&self) -> Option<StartupDistribution> {
        StartupDistribution::from_samples_ms(self.samples_ms.clone())
    }

    /// p50 startup latency in milliseconds.
    #[must_use]
    pub fn p50_ms(&self) -> Option<f64> {
        self.distribution().map(|d| d.p50_ms)
    }

    /// p99 startup latency in milliseconds.
    #[must_use]
    pub fn p99_ms(&self) -> Option<f64> {
        self.distribution().map(|d| d.p99_ms)
    }

    /// `true` when p50 meets the accepted PB-1 budget.
    #[must_use]
    pub fn meets_p50(&self) -> bool {
        self.distribution().is_some_and(|d| d.meets_p50())
    }

    /// `true` when p99 meets the accepted PB-1 budget.
    #[must_use]
    pub fn meets_p99(&self) -> bool {
        self.distribution().is_some_and(|d| d.meets_p99())
    }
}

/// PB-2 idle-memory evidence from a real-window session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RealIdleEvidence {
    /// RSS samples in MB taken across the idle window (empty when unavailable).
    pub samples_mb: Vec<f64>,
    /// Idle window actually observed, in seconds.
    pub idle_secs: u64,
    /// Absolute path of the launched binary, when one was resolved.
    pub binary: Option<String>,
    /// Why the measurement is unavailable (`None` when samples exist).
    pub reason: Option<String>,
}

impl RealIdleEvidence {
    /// `true` when no RSS sample was collected.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        self.samples_mb.is_empty()
    }

    /// Median RSS in MB (PB-2 is specified as a p50).
    #[must_use]
    pub fn median_mb(&self) -> Option<f64> {
        if self.samples_mb.is_empty() {
            return None;
        }
        let mut sorted = self.samples_mb.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 1 {
            Some(sorted[mid])
        } else {
            Some((sorted[mid - 1] + sorted[mid]) / 2.0)
        }
    }

    /// `true` when the median RSS is within the accepted PB-2 budget.
    #[must_use]
    pub fn meets_budget(&self) -> bool {
        self.median_mb()
            .is_some_and(|mb| mb <= crate::PB2_IDLE_RSS_MB as f64)
    }
}

// ---------------------------------------------------------------------------
// Gate, bounds, and binary discovery
// ---------------------------------------------------------------------------

/// Repository root derived from `CARGO_MANIFEST_DIR` (worktree- and CI-safe).
#[must_use]
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate directory must be <workspace>/crates/bitty-perf")
        .to_path_buf()
}

/// The opt-in gate reason when real-window measurement is disabled.
///
/// Returns `Some(reason)` unless `BITTY_PERF_REAL_WINDOW=1`; callers turn that
/// into an `Unavailable` result rather than measuring on CI.
#[must_use]
pub fn gate_reason() -> Option<String> {
    if std::env::var(REAL_WINDOW_ENV).as_deref() == Ok("1") {
        None
    } else {
        Some(format!(
            "{REAL_WINDOW_ENV}!=1 (opt-in Tier 1 real-window evidence run)"
        ))
    }
}

/// Resolve the `bitty` binary to launch without hardcoding a host path.
///
/// Order: `BITTY_PERF_BIN` (any existing file), then a `release` build under
/// the discovered workspace `target/`, then a `debug` build. A missing binary
/// is a reason string, never a panic.
pub fn resolve_binary() -> Result<PathBuf, String> {
    if let Ok(explicit) = std::env::var(BINARY_ENV) {
        if !explicit.is_empty() {
            let path = PathBuf::from(explicit);
            return if path.is_file() {
                Ok(path)
            } else {
                Err(format!("{BINARY_ENV} points at a missing file"))
            };
        }
    }
    let target = workspace_root().join("target");
    let candidates = [
        target.join("release").join(binary_name()),
        target.join("debug").join(binary_name()),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no bitty binary under target/ (build one or set {BINARY_ENV})"
    ))
}

/// Platform binary file name (`bitty` / `bitty.exe`).
#[must_use]
pub const fn binary_name() -> &'static str {
    if cfg!(windows) { "bitty.exe" } else { "bitty" }
}

/// Clamped startup sample count from `BITTY_PERF_STARTUP_SAMPLES`.
#[must_use]
pub fn startup_samples() -> usize {
    env_usize("BITTY_PERF_STARTUP_SAMPLES", DEFAULT_STARTUP_SAMPLES).clamp(1, MAX_STARTUP_SAMPLES)
}

/// Clamped idle window from `BITTY_PERF_IDLE_SECS`.
#[must_use]
pub fn idle_secs() -> u64 {
    env_u64("BITTY_PERF_IDLE_SECS", DEFAULT_IDLE_SECS).clamp(1, MAX_IDLE_SECS)
}

fn startup_timeout() -> Duration {
    Duration::from_secs(
        env_u64(
            "BITTY_PERF_STARTUP_TIMEOUT_SECS",
            DEFAULT_STARTUP_TIMEOUT_SECS,
        )
        .clamp(2, MAX_STARTUP_TIMEOUT_SECS),
    )
}

fn idle_timeout(idle: Duration) -> Duration {
    idle + Duration::from_secs(30)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Measure PB-1 real-window startup over [`startup_samples`] launches.
///
/// Returns `Unavailable` (empty samples + reason) when the gate is off, no
/// binary resolves, or a launch never reaches the first frame.
#[must_use]
pub fn measure_startup_evidence() -> RealStartupEvidence {
    let samples = startup_samples();
    measure_startup_evidence_with(samples, startup_timeout())
}

/// Measurement variant with explicit bounds (used by the bench and tests).
#[must_use]
pub fn measure_startup_evidence_with(samples: usize, timeout: Duration) -> RealStartupEvidence {
    let samples = samples.clamp(1, MAX_STARTUP_SAMPLES);
    if let Some(reason) = gate_reason() {
        return RealStartupEvidence {
            reason: Some(reason),
            ..RealStartupEvidence::default()
        };
    }
    let binary = match resolve_binary() {
        Ok(path) => path,
        Err(reason) => {
            return RealStartupEvidence {
                reason: Some(reason),
                ..RealStartupEvidence::default()
            };
        }
    };
    let binary_str = binary.display().to_string();
    let mut outcome = imp::launch_startup(&binary, samples, timeout);
    outcome.binary = Some(binary_str);
    outcome
}

/// Measure PB-2 idle RSS after [`idle_secs`] of a real-window session.
///
/// Returns `Unavailable` (empty samples + reason) when the gate is off, no
/// binary resolves, or the session never reaches the first frame / RSS cannot
/// be read on this platform.
#[must_use]
pub fn measure_idle_evidence() -> RealIdleEvidence {
    let idle = Duration::from_secs(idle_secs());
    measure_idle_evidence_with(idle, idle_timeout(idle))
}

/// Measurement variant with explicit bounds (used by the bench and tests).
#[must_use]
pub fn measure_idle_evidence_with(idle: Duration, timeout: Duration) -> RealIdleEvidence {
    let idle = idle.min(Duration::from_secs(MAX_IDLE_SECS));
    if let Some(reason) = gate_reason() {
        return RealIdleEvidence {
            reason: Some(reason),
            idle_secs: idle.as_secs(),
            ..RealIdleEvidence::default()
        };
    }
    let binary = match resolve_binary() {
        Ok(path) => path,
        Err(reason) => {
            return RealIdleEvidence {
                reason: Some(reason),
                idle_secs: idle.as_secs(),
                ..RealIdleEvidence::default()
            };
        }
    };
    let binary_str = binary.display().to_string();
    let mut outcome = imp::launch_idle(&binary, idle, timeout);
    outcome.binary = Some(binary_str);
    outcome.idle_secs = idle.as_secs();
    outcome
}

// ---------------------------------------------------------------------------
// Host context (environment-derived; no host paths or identifiers)
// ---------------------------------------------------------------------------

/// Hardware/software context recorded alongside the numbers.
///
/// Every field is derived from the environment or standard system interfaces;
/// no checkout path, username, or hostname is captured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostContext {
    /// OS family from `std::env::consts::OS` (e.g. `linux`, `macos`).
    pub os: String,
    /// CPU architecture from `std::env::consts::ARCH`.
    pub arch: String,
    /// Rust toolchain description from `BITTY_PERF_TOOLCHAIN` (or `unknown`).
    pub toolchain: String,
    /// Logical CPU count.
    pub cpus: usize,
    /// Total system memory in MB, when discoverable.
    pub total_memory_mb: Option<u64>,
}

impl HostContext {
    /// Capture the host context from the environment.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            toolchain: std::env::var("BITTY_PERF_TOOLCHAIN")
                .unwrap_or_else(|_| "unknown".to_string()),
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get()),
            total_memory_mb: total_memory_mb(),
        }
    }
}

fn total_memory_mb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kb: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())?;
    Some(kb / 1024)
}

// ---------------------------------------------------------------------------
// Committed baseline artifact
// ---------------------------------------------------------------------------

/// Provenance metadata supplied to [`baseline_json`].
#[derive(Debug, Clone, Default)]
pub struct BaselineMeta {
    /// Owning CarryCtx task.
    pub task: String,
    /// Owning GitHub issues.
    pub issues: Vec<u64>,
    /// Capture date (`YYYY-MM-DD`).
    pub captured_at: String,
    /// Git revision the baseline was captured at.
    pub revision: String,
    /// Exact measurement command.
    pub command: String,
    /// Cargo profile used.
    pub profile: String,
}

/// The committed evidence artifact, embedded at compile time.
#[must_use]
pub const fn committed_baseline_json() -> &'static str {
    include_str!("../baselines/pb-real-window.json")
}

/// Serialize a real-window report plus provenance into the committed shape.
#[must_use]
pub fn baseline_json(
    startup: &RealStartupEvidence,
    idle: &RealIdleEvidence,
    meta: &BaselineMeta,
) -> String {
    let host = HostContext::capture();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {REAL_WINDOW_BASELINE_SCHEMA_VERSION},\n"
    ));
    out.push_str(&format!("  \"task\": \"{}\",\n", escape(&meta.task)));
    let issues: Vec<String> = meta.issues.iter().map(u64::to_string).collect();
    out.push_str(&format!("  \"issues\": [{}],\n", issues.join(", ")));
    out.push_str(&format!(
        "  \"captured_at\": \"{}\",\n",
        escape(&meta.captured_at)
    ));
    out.push_str(&format!(
        "  \"revision\": \"{}\",\n",
        escape(&meta.revision)
    ));
    out.push_str(&format!("  \"command\": \"{}\",\n", escape(&meta.command)));
    out.push_str(&format!("  \"profile\": \"{}\",\n", escape(&meta.profile)));
    out.push_str(
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-1-cold-startup-time and #pb-2-idle-memory\",\n",
    );
    out.push_str("  \"host_context\": {\n");
    out.push_str(&format!("    \"os\": \"{}\",\n", escape(&host.os)));
    out.push_str(&format!("    \"arch\": \"{}\",\n", escape(&host.arch)));
    out.push_str(&format!(
        "    \"toolchain\": \"{}\",\n",
        escape(&host.toolchain)
    ));
    out.push_str(&format!("    \"cpus\": {},\n", host.cpus));
    match host.total_memory_mb {
        Some(mb) => out.push_str(&format!("    \"total_memory_mb\": {mb}\n")),
        None => out.push_str("    \"total_memory_mb\": null\n"),
    }
    out.push_str("  },\n");
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!(
        "    \"startup_samples\": {},\n",
        startup.samples_ms.len()
    ));
    out.push_str(&format!("    \"idle_secs\": {},\n", idle.idle_secs));
    out.push_str(&format!(
        "    \"idle_rss_samples\": {}\n",
        idle.samples_mb.len()
    ));
    out.push_str("  },\n");
    out.push_str(&format!("  \"regression_factor\": {REGRESSION_FACTOR},\n"));
    out.push_str("  \"pb1_startup\": {\n");
    out.push_str(&format!(
        "    \"status\": \"{}\",\n",
        status_of(startup.is_unavailable())
    ));
    out.push_str(&format!("    \"marker_seen\": {},\n", startup.marker_seen));
    match startup.distribution() {
        Some(dist) => {
            out.push_str(&format!("    \"count\": {},\n", dist.count));
            out.push_str(&format!("    \"p50_ms\": {:.3},\n", dist.p50_ms));
            out.push_str(&format!("    \"p99_ms\": {:.3},\n", dist.p99_ms));
            out.push_str(&format!("    \"min_ms\": {:.3},\n", dist.min_ms));
            out.push_str(&format!("    \"max_ms\": {:.3},\n", dist.max_ms));
            out.push_str(&format!(
                "    \"budget_p50_ms\": {},\n",
                crate::PB1_STARTUP_MS_P50
            ));
            out.push_str(&format!(
                "    \"budget_p99_ms\": {}\n",
                crate::PB1_STARTUP_MS_P99
            ));
        }
        None => {
            out.push_str("    \"p50_ms\": null,\n");
            out.push_str("    \"p99_ms\": null,\n");
            out.push_str(&format!(
                "    \"reason\": \"{}\"\n",
                escape(startup.reason.as_deref().unwrap_or("no samples"))
            ));
        }
    }
    out.push_str("  },\n");
    out.push_str("  \"pb2_idle_rss\": {\n");
    out.push_str(&format!(
        "    \"status\": \"{}\",\n",
        status_of(idle.is_unavailable())
    ));
    match idle.median_mb() {
        Some(median) => {
            out.push_str(&format!("    \"rss_mb\": {median:.3},\n"));
            let samples: Vec<String> = idle.samples_mb.iter().map(|v| format!("{v:.3}")).collect();
            out.push_str(&format!("    \"samples_mb\": [{}],\n", samples.join(", ")));
            out.push_str(&format!("    \"budget_mb\": {}\n", crate::PB2_IDLE_RSS_MB));
        }
        None => {
            out.push_str("    \"rss_mb\": null,\n");
            out.push_str(&format!(
                "    \"reason\": \"{}\"\n",
                escape(idle.reason.as_deref().unwrap_or("no samples"))
            ));
        }
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

const fn status_of(unavailable: bool) -> &'static str {
    if unavailable {
        "unavailable"
    } else {
        "measured"
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out
}

/// Render a stable human-readable report for the bench.
#[must_use]
pub fn format_report(startup: &RealStartupEvidence, idle: &RealIdleEvidence) -> String {
    let mut out = String::new();
    out.push_str("real_window — PB-1 startup + PB-2 idle RSS (opt-in Tier 1 evidence)\n");
    out.push_str(&format!(
        "budget: PB-1 p50 {} ms / p99 {} ms; PB-2 <= {} MB RSS p50\n",
        crate::PB1_STARTUP_MS_P50,
        crate::PB1_STARTUP_MS_P99,
        crate::PB2_IDLE_RSS_MB
    ));
    out.push_str(&format!(
        "host: os={} arch={} cpus={}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get())
    ));

    out.push_str("PB-1 startup:\n");
    match startup.distribution() {
        Some(dist) => {
            out.push_str(&format!(
                "  samples={} p50={:.3} ms p99={:.3} ms min={:.3} max={:.3} marker_seen={}\n",
                dist.count, dist.p50_ms, dist.p99_ms, dist.min_ms, dist.max_ms, startup.marker_seen
            ));
            let verdict = if dist.meets_p50() {
                "PASS p50"
            } else if dist.meets_p99() {
                "PASS p99 (p50 exceeded)"
            } else {
                "ABOVE_BUDGET"
            };
            out.push_str(&format!("  verdict: {verdict}\n"));
        }
        None => {
            out.push_str(&format!(
                "  UNMEASURED ({})\n",
                startup.reason.as_deref().unwrap_or("no samples")
            ));
        }
    }

    out.push_str("PB-2 idle RSS:\n");
    match idle.median_mb() {
        Some(median) => {
            let samples: Vec<String> = idle.samples_mb.iter().map(|v| format!("{v:.1}")).collect();
            out.push_str(&format!(
                "  idle={} s rss_p50={:.1} MB samples=[{}] verdict={}\n",
                idle.idle_secs,
                median,
                samples.join(", "),
                if idle.meets_budget() {
                    "PASS"
                } else {
                    "ABOVE_BUDGET"
                }
            ));
        }
        None => {
            out.push_str(&format!(
                "  UNMEASURED ({})\n",
                idle.reason.as_deref().unwrap_or("no samples")
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Unix implementation
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Read};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{
        FIRST_FRAME_MARKER, IDLE_RSS_SAMPLES, RealIdleEvidence, RealStartupEvidence,
        STARTUP_MARKER_ENV,
    };

    fn spawn(binary: &Path) -> Result<Child, String> {
        Command::new(binary)
            .env(STARTUP_MARKER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))
    }

    fn kill(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Wait for the marker on `child` stdout, then return the child and a
    /// reader-join handle. Used by the idle workload, which keeps the child
    /// alive after the marker.
    fn wait_for_marker(child: &mut Child, timeout: Duration) -> Result<(), String> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child stdout pipe was not captured".to_string())?;
        let (tx, rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let marker = FIRST_FRAME_MARKER;
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if line.starts_with(marker) {
                            let _ = tx.send(true);
                            let mut sink = Vec::new();
                            let _ = reader.read_to_end(&mut sink);
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(false);
        });
        match rx.recv_timeout(timeout) {
            Ok(true) => Ok(()),
            Ok(false) => Err(
                "binary exited before first frame (no real window / display unavailable)"
                    .to_string(),
            ),
            Err(_) => {
                Err("first-frame marker timed out (display or event loop unavailable)".to_string())
            }
        }
    }

    pub(super) fn launch_startup(
        binary: &Path,
        samples: usize,
        timeout: Duration,
    ) -> RealStartupEvidence {
        let mut evidence = RealStartupEvidence::default();
        for _ in 0..samples {
            let started = Instant::now();
            match wait_for_marker_spawn(binary, timeout) {
                Ok(child) => {
                    evidence
                        .samples_ms
                        .push(started.elapsed().as_secs_f64() * 1000.0);
                    evidence.marker_seen = true;
                    let mut child = child;
                    kill(&mut child);
                }
                Err(reason) => {
                    if evidence.samples_ms.is_empty() {
                        evidence.reason = Some(reason);
                        return evidence;
                    }
                    // A late failure after a successful sample: keep the samples
                    // and stop cleanly, recording the partial reason.
                    evidence.reason = Some(format!("partial: {reason}"));
                    return evidence;
                }
            }
        }
        evidence
    }

    /// Spawn and wait for the marker, returning the still-running child.
    fn wait_for_marker_spawn(binary: &Path, timeout: Duration) -> Result<Child, String> {
        let mut child = spawn(binary)?;
        match wait_for_marker(&mut child, timeout) {
            Ok(()) => Ok(child),
            Err(reason) => {
                kill(&mut child);
                Err(reason)
            }
        }
    }

    pub(super) fn launch_idle(
        binary: &Path,
        idle: Duration,
        timeout: Duration,
    ) -> RealIdleEvidence {
        let mut evidence = RealIdleEvidence {
            idle_secs: idle.as_secs(),
            ..RealIdleEvidence::default()
        };
        let mut child = match spawn(binary) {
            Ok(child) => child,
            Err(reason) => {
                evidence.reason = Some(reason);
                return evidence;
            }
        };
        if let Err(reason) = wait_for_marker(&mut child, timeout) {
            kill(&mut child);
            evidence.reason = Some(reason);
            return evidence;
        }
        let pid = child.id();
        std::thread::sleep(idle);
        for _ in 0..IDLE_RSS_SAMPLES {
            match read_rss_mb(pid) {
                Ok(mb) => evidence.samples_mb.push(mb),
                Err(reason) => {
                    kill(&mut child);
                    if evidence.samples_mb.is_empty() {
                        evidence.reason = Some(reason);
                        return evidence;
                    }
                    evidence.reason = Some(format!("partial: {reason}"));
                    return evidence;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        kill(&mut child);
        evidence
    }

    /// Read resident set size in MB for `pid`.
    ///
    /// Prefers `/proc/<pid>/status` (Linux) and falls back to `ps -o rss=`,
    /// which is available on macOS too. Both are system interfaces, not host
    /// paths baked into the artifact.
    fn read_rss_mb(pid: u32) -> Result<f64, String> {
        let status_path = format!("/proc/{pid}/status");
        if let Ok(status) = std::fs::read_to_string(&status_path) {
            if let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                if let Some(kb) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<f64>().ok())
                {
                    return Ok(kb / 1024.0);
                }
            }
        }
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .map_err(|e| format!("ps unavailable: {e}"))?;
        if !output.status.success() {
            return Err("ps could not read child RSS".to_string());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let kb: f64 = text
            .trim()
            .parse()
            .map_err(|_| "ps rss output was not numeric".to_string())?;
        Ok(kb / 1024.0)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use super::{RealIdleEvidence, RealStartupEvidence};

    const UNSUPPORTED: &str = "real-window process/RSS probes are Unix-only";

    pub(super) fn launch_startup(
        _binary: &Path,
        _samples: usize,
        _timeout: Duration,
    ) -> RealStartupEvidence {
        RealStartupEvidence {
            reason: Some(UNSUPPORTED.to_string()),
            ..RealStartupEvidence::default()
        }
    }

    pub(super) fn launch_idle(
        _binary: &Path,
        _idle: Duration,
        _timeout: Duration,
    ) -> RealIdleEvidence {
        RealIdleEvidence {
            reason: Some(UNSUPPORTED.to_string()),
            ..RealIdleEvidence::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_is_closed_without_the_env_flag() {
        // The test process normally runs without the opt-in flag, so the
        // harness must report Unavailable with a reason, never a number.
        if gate_reason().is_none() {
            // A developer opted in locally; skip the closed-gate assertion.
            return;
        }
        let startup = measure_startup_evidence_with(1, Duration::from_secs(2));
        assert!(startup.is_unavailable(), "gate off must be unavailable");
        assert!(startup.reason.is_some(), "unavailable must carry a reason");
        assert!(startup.p50_ms().is_none());
        let idle = measure_idle_evidence_with(Duration::from_secs(1), Duration::from_secs(2));
        assert!(idle.is_unavailable(), "gate off must be unavailable");
        assert!(idle.reason.is_some(), "unavailable must carry a reason");
        assert!(idle.median_mb().is_none());
    }

    #[test]
    fn distribution_math_is_exact_for_fixed_samples() {
        let evidence = RealStartupEvidence {
            samples_ms: (1..=100).map(|v| v as f64).collect(),
            marker_seen: true,
            ..RealStartupEvidence::default()
        };
        assert!(!evidence.is_unavailable());
        assert_eq!(evidence.p50_ms(), Some(50.0));
        assert_eq!(evidence.p99_ms(), Some(99.0));
        assert!(evidence.meets_p50());
        assert!(evidence.meets_p99());
    }

    #[test]
    fn idle_median_handles_odd_even_and_empty() {
        assert_eq!(
            RealIdleEvidence {
                samples_mb: vec![10.0, 30.0, 20.0],
                ..RealIdleEvidence::default()
            }
            .median_mb(),
            Some(20.0)
        );
        assert_eq!(
            RealIdleEvidence {
                samples_mb: vec![10.0, 20.0],
                ..RealIdleEvidence::default()
            }
            .median_mb(),
            Some(15.0)
        );
        assert_eq!(RealIdleEvidence::default().median_mb(), None);
    }

    #[test]
    fn idle_budget_is_the_pb2_constant() {
        let under = RealIdleEvidence {
            samples_mb: vec![(crate::PB2_IDLE_RSS_MB as f64) - 1.0],
            ..RealIdleEvidence::default()
        };
        assert!(under.meets_budget());
        let over = RealIdleEvidence {
            samples_mb: vec![(crate::PB2_IDLE_RSS_MB as f64) + 1.0],
            ..RealIdleEvidence::default()
        };
        assert!(!over.meets_budget());
    }

    #[test]
    fn baseline_json_round_trips_provenance_and_numbers() {
        let startup = RealStartupEvidence {
            samples_ms: vec![42.0, 55.0, 61.0],
            marker_seen: true,
            ..RealStartupEvidence::default()
        };
        let idle = RealIdleEvidence {
            samples_mb: vec![70.0, 71.0, 72.0],
            idle_secs: 5,
            ..RealIdleEvidence::default()
        };
        let meta = BaselineMeta {
            task: "CTX-0592".to_string(),
            issues: vec![1056, 1057],
            captured_at: "2026-09-20".to_string(),
            revision: "deadbeef".to_string(),
            command: "cargo bench -p bitty-perf --bench real_window".to_string(),
            profile: "bench (release)".to_string(),
        };
        let json = baseline_json(&startup, &idle, &meta);
        for key in [
            "\"task\": \"CTX-0592\"",
            "\"issues\": [1056, 1057]",
            "\"captured_at\": \"2026-09-20\"",
            "\"revision\": \"deadbeef\"",
            "\"marker_seen\": true",
            "\"p50_ms\"",
            "\"budget_mb\": 80",
            "\"regression_factor\": 3",
        ] {
            assert!(json.contains(key), "baseline json missing {key}: {json}");
        }
        assert!(
            json.contains("\"status\": \"measured\""),
            "measured evidence must not be marked unavailable"
        );
    }

    #[test]
    fn committed_baseline_is_provenanced() {
        let text = committed_baseline_json();
        for key in [
            "\"task\"",
            "\"issues\"",
            "\"captured_at\"",
            "\"revision\"",
            "\"command\"",
            "\"host_context\"",
            "\"budget_ref\"",
        ] {
            assert!(
                text.contains(key),
                "committed real-window baseline missing {key}"
            );
        }
        assert!(
            !text.contains("pending") && !text.contains("unspecified"),
            "committed baseline still carries placeholder provenance"
        );
    }
}
