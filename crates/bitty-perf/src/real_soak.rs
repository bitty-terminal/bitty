//! Long-duration real-render soak automation (CTX-0642, PERF-09).
//!
//! The short real-window harness (`real_window.rs`, CTX-0592) measures PB-1
//! startup and PB-2 idle RSS over seconds. The headless soak
//! (`bitty-runtime/tests/soak.rs`) proves the bytes-to-present plumbing
//! without a display. What was missing between them — and what this module
//! closes — is the **automated, schedulable long-duration real-render leg**:
//! a bounded soak of a real `bitty` window on a Tier 1 Hyprland host that
//! collects periodic evidence through both the pixel path (`hyprctl` +
//! `grim`, previously a manual checklist step) and the DevTools-preferred
//! introspection path (`bitty ctl terminal text`, backed by the
//! `bitty.debug/*` socket contract), plus per-capture RSS samples for the
//! PB-3 typical-session anchor.
//!
//! ## Measurement contract
//!
//! - **Opt-in only.** A run requires `BITTY_PERF_REAL_SOAK=1` *and* a
//!   resolvable `bitty` binary *and* a live Hyprland capture leg
//!   (`hyprctl`, `grim`, `jq` on `PATH` plus a Hyprland session). Without
//!   all three the result is `Unavailable` with a reason string; no number
//!   is ever fabricated. Headless CI therefore reports `Unavailable` by
//!   design and stays green.
//! - **Bounded and deterministic.** Duration and capture interval come from
//!   the environment but are clamped (`MIN_DURATION_SECS`,
//!   `MAX_DURATION_SECS`, `MIN_CAPTURE_INTERVAL_SECS`,
//!   `MAX_CAPTURE_INTERVAL_SECS`, `MAX_CAPTURES`). The capture plan
//!   (count, effective interval, capture offsets) is a pure function of the
//!   clamped config, so `--dry-run` planning needs no display.
//! - **Host context from the environment.** OS, architecture, CPU count, and
//!   total memory are derived from standard interfaces; no checkout path,
//!   username, or hostname is embedded. Evidence artifacts record screenshot
//!   file names only, never absolute host paths.
//! - **Local-only evidence.** Screenshots and grid-text snapshots may show
//!   shell output, so the script drives a fixed synthetic workload and
//!   writes everything under a caller-chosen output directory (default
//!   `recording/real-soak`, gitignored). Nothing is committed by the run;
//!   promoting an artifact to `baselines/` is an explicit, reviewed copy.
//!
//! `#![forbid(unsafe_code)]`; process and display probing is Unix-gated and
//! degrades to `Unavailable` on other platforms.
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`
//! (250 MB) and `#pb-7-idle-cpu`. Runbook:
//! `crates/bitty-perf/baselines/real-soak-evidence.md`. Automation script:
//! `scripts/real-render-soak.sh`.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use super::real_window::{BaselineMeta, HostContext, workspace_root};

/// Env flag that opts a run into long-duration real-render soak measurement.
pub const SOAK_ENV: &str = "BITTY_PERF_REAL_SOAK";

/// Env var overriding the soak duration in seconds.
pub const DURATION_ENV: &str = "BITTY_PERF_SOAK_DURATION_SECS";
/// Env var overriding the capture interval in seconds.
pub const INTERVAL_ENV: &str = "BITTY_PERF_SOAK_INTERVAL_SECS";
/// Env var overriding the Hyprland workspace that hosts the soak window.
pub const WORKSPACE_ENV: &str = "BITTY_PERF_SOAK_WORKSPACE";
/// Env var overriding the synthetic workload profile.
pub const WORKLOAD_ENV: &str = "BITTY_PERF_SOAK_WORKLOAD";
/// Env var overriding the evidence output directory.
pub const OUT_DIR_ENV: &str = "BITTY_PERF_SOAK_OUT_DIR";

/// Default soak duration: 4 h, the PB-3 typical-session window.
pub const DEFAULT_DURATION_SECS: u64 = 14_400;
/// Minimum soak duration: 60 s (smoke runs below this belong in `--dry-run`).
pub const MIN_DURATION_SECS: u64 = 60;
/// Maximum soak duration: 24 h (longer runs are re-scheduled, not stretched).
pub const MAX_DURATION_SECS: u64 = 86_400;
/// Default capture interval: one screenshot + RSS + grid snapshot per 5 min.
pub const DEFAULT_CAPTURE_INTERVAL_SECS: u64 = 300;
/// Minimum capture interval: 30 s (tighter cadence would distort the soak).
pub const MIN_CAPTURE_INTERVAL_SECS: u64 = 30;
/// Maximum capture interval: 1 h.
pub const MAX_CAPTURE_INTERVAL_SECS: u64 = 3_600;
/// Hard cap on captures per run (keeps the plan and the output dir bounded).
pub const MAX_CAPTURES: usize = 512;

/// Default Hyprland workspace hosting the soak window.
pub const DEFAULT_WORKSPACE: &str = "4";
/// Synthetic workload profiles the script knows how to drive.
pub const WORKLOADS: &[&str] = &["idle", "mixed", "input-spam"];
/// Default workload profile.
pub const DEFAULT_WORKLOAD: &str = "mixed";

/// Default evidence output directory, relative to the workspace root
/// (gitignored singular `recording/`, per the scratch-path convention).
pub const DEFAULT_OUT_DIR: &str = "recording/real-soak";

/// Committed soak evidence artifact (relative to the repository root).
pub const REAL_SOAK_BASELINE_REL_PATH: &str = "crates/bitty-perf/baselines/pb-real-soak.json";

/// Schema version of the soak evidence artifact.
pub const REAL_SOAK_SCHEMA_VERSION: u32 = 1;

/// Capture-leg tools required on `PATH` for a real run.
pub const CAPTURE_TOOLS: &[&str] = &["hyprctl", "grim", "jq"];

// ---------------------------------------------------------------------------
// Soak configuration (environment-derived, clamped, no host facts)
// ---------------------------------------------------------------------------

/// Clamped soak configuration parsed from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoakConfig {
    /// Total soak wall time in seconds, clamped to
    /// `MIN_DURATION_SECS..=MAX_DURATION_SECS`.
    pub duration_secs: u64,
    /// Requested capture interval in seconds, clamped to
    /// `MIN_CAPTURE_INTERVAL_SECS..=MAX_CAPTURE_INTERVAL_SECS`.
    pub interval_secs: u64,
    /// Hyprland workspace id hosting the soak window (e.g. `"4"`).
    pub workspace: String,
    /// Synthetic workload profile (one of [`WORKLOADS`]).
    pub workload: String,
    /// Evidence output directory (absolute when derived from a relative
    /// value: resolved against the workspace root).
    pub out_dir: PathBuf,
}

impl SoakConfig {
    /// Parse the configuration from the environment with clamping.
    ///
    /// Unknown workload names fall back to [`DEFAULT_WORKLOAD`]; a
    /// non-numeric workspace falls back to [`DEFAULT_WORKSPACE`]. Both
    /// fallbacks are reported in the human plan so a typo never silently
    /// re-targets a live session.
    #[must_use]
    pub fn from_env() -> Self {
        let duration_secs = env_u64(DURATION_ENV, DEFAULT_DURATION_SECS)
            .clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);
        let interval_secs = env_u64(INTERVAL_ENV, DEFAULT_CAPTURE_INTERVAL_SECS)
            .clamp(MIN_CAPTURE_INTERVAL_SECS, MAX_CAPTURE_INTERVAL_SECS);
        let workspace = bravais_workspace(&std::env::var(WORKSPACE_ENV).unwrap_or_default());
        let workload = validated_workload(&std::env::var(WORKLOAD_ENV).unwrap_or_default());
        let out_dir = resolve_out_dir(&std::env::var(OUT_DIR_ENV).unwrap_or_default());
        Self {
            duration_secs,
            interval_secs,
            workspace,
            workload,
            out_dir,
        }
    }
}

/// Validate a workspace selector: a `1..=10` id stays, anything else falls
/// back to [`DEFAULT_WORKSPACE`].
#[must_use]
pub fn bravais_workspace(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_WORKSPACE.to_string();
    }
    match trimmed.parse::<u32>() {
        Ok(id) if (1..=10).contains(&id) => trimmed.to_string(),
        _ => DEFAULT_WORKSPACE.to_string(),
    }
}

/// Validate a workload profile name against [`WORKLOADS`].
#[must_use]
pub fn validated_workload(raw: &str) -> String {
    let trimmed = raw.trim();
    if WORKLOADS.contains(&trimmed) {
        trimmed.to_string()
    } else {
        DEFAULT_WORKLOAD.to_string()
    }
}

/// Resolve the output directory: empty means the default (relative to the
/// workspace root); a relative value resolves against the workspace root;
/// an absolute value is taken as given (the run stays local and uncommitted).
#[must_use]
pub fn resolve_out_dir(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return workspace_root().join(DEFAULT_OUT_DIR);
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        workspace_root().join(path)
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Capture plan (pure function of the clamped config)
// ---------------------------------------------------------------------------

/// Bounded capture plan derived from a [`SoakConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePlan {
    /// Number of captures in this run (includes the `t = 0` capture).
    pub captures: usize,
    /// Effective interval in seconds (widened when the requested cadence
    /// would exceed [`MAX_CAPTURES`]).
    pub interval_effective_secs: u64,
    /// Soak duration this plan covers.
    pub duration_secs: u64,
    /// Capture offsets in seconds from soak start (`capture_at_secs[0] == 0`).
    pub capture_at_secs: Vec<u64>,
}

impl CapturePlan {
    /// `true` when the requested interval had to be widened to respect
    /// [`MAX_CAPTURES`].
    #[must_use]
    pub fn interval_widened(&self, requested: u64) -> bool {
        self.interval_effective_secs != requested
    }
}

/// Derive the bounded capture plan for a duration/interval pair.
///
/// Capture `0` fires at soak start; further captures follow every effective
/// interval while `offset <= duration`. The count is clamped to
/// [`MAX_CAPTURES`] by widening the effective interval, never by dropping
/// the trailing capture.
#[must_use]
pub fn plan_captures(duration_secs: u64, interval_secs: u64) -> CapturePlan {
    let duration_secs = duration_secs.clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);
    let interval_secs = interval_secs.clamp(MIN_CAPTURE_INTERVAL_SECS, MAX_CAPTURE_INTERVAL_SECS);
    let mut effective = interval_secs;
    let mut count = duration_secs / effective + 1;
    if count > MAX_CAPTURES as u64 {
        effective = duration_secs.div_ceil(MAX_CAPTURES as u64).max(1);
        count = duration_secs / effective + 1;
    }
    let count = (count as usize).clamp(1, MAX_CAPTURES);
    let capture_at_secs = (0..count as u64).map(|i| i * effective).collect();
    CapturePlan {
        captures: count,
        interval_effective_secs: effective,
        duration_secs,
        capture_at_secs,
    }
}

// ---------------------------------------------------------------------------
// Gate and capture-leg probing
// ---------------------------------------------------------------------------

/// The opt-in gate reason when long-duration soak measurement is disabled.
///
/// Returns `Some(reason)` unless `BITTY_PERF_REAL_SOAK=1`; callers turn that
/// into an `Unavailable` result rather than touching a display.
#[must_use]
pub fn soak_gate_reason() -> Option<String> {
    if std::env::var(SOAK_ENV).as_deref() == Ok("1") {
        None
    } else {
        Some(format!(
            "{SOAK_ENV}!=1 (opt-in Tier 1 long-duration real-render soak)"
        ))
    }
}

/// Status of the `hyprctl` + `grim` pixel-capture leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureLegStatus {
    /// `true` when every tool resolves and a Hyprland session is present.
    pub available: bool,
    /// Why the leg is unavailable (`None` when available).
    pub reason: Option<String>,
    /// Required tools missing from `PATH` (empty when all resolve).
    pub missing_tools: Vec<String>,
}

impl CaptureLegStatus {
    /// Build a status from a tool-presence probe and a Hyprland-session flag.
    #[must_use]
    pub fn probe(tools_present: &[(&str, bool)], hyprland_session: bool) -> Self {
        let missing_tools: Vec<String> = tools_present
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| (*name).to_string())
            .collect();
        if !missing_tools.is_empty() {
            return Self {
                available: false,
                reason: Some(format!(
                    "capture leg missing tools: {}",
                    missing_tools.join(", ")
                )),
                missing_tools,
            };
        }
        if !hyprland_session {
            return Self {
                available: false,
                reason: Some("no Hyprland session (HYPRLAND_INSTANCE_SIGNATURE unset)".to_string()),
                missing_tools,
            };
        }
        Self {
            available: true,
            reason: None,
            missing_tools,
        }
    }
}

/// Probe the live capture leg: every [`CAPTURE_TOOLS`] entry must resolve on
/// `PATH` and `HYPRLAND_INSTANCE_SIGNATURE` must be set.
#[must_use]
pub fn capture_leg_status() -> CaptureLegStatus {
    let tools_present: Vec<(&str, bool)> = CAPTURE_TOOLS
        .iter()
        .map(|tool| (*tool, tool_present(tool)))
        .collect();
    CaptureLegStatus::probe(
        &tools_present,
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok_and(|v| !v.trim().is_empty()),
    )
}

/// `true` when `name` resolves to an executable file on a `PATH` entry.
#[must_use]
pub fn tool_present(name: &str) -> bool {
    match std::env::var_os("PATH") {
        Some(path) => tool_present_in_path(name, &std::env::split_paths(&path).collect::<Vec<_>>()),
        None => false,
    }
}

/// Pure `PATH`-entry scan backing [`tool_present`] (unit-testable without
/// touching the process environment).
#[must_use]
pub fn tool_present_in_path(name: &str, entries: &[PathBuf]) -> bool {
    entries.iter().any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

// ---------------------------------------------------------------------------
// Evidence model (measured captures; no absolute host paths)
// ---------------------------------------------------------------------------

/// One periodic capture inside a soak run.
#[derive(Debug, Clone, PartialEq)]
pub struct SoakCapture {
    /// Capture index (`0` is the `t = 0` capture after first-frame settle).
    pub index: usize,
    /// Offset in seconds from soak start.
    pub at_secs: u64,
    /// Screenshot file name only (e.g. `capture-0007.png`); the directory
    /// is the run output dir and is never serialized.
    pub screenshot: String,
    /// Child RSS in MB at capture time (`None` when the sample failed).
    pub rss_mb: Option<f64>,
    /// Grid-text snapshot byte size (`bitty ctl terminal text` leg).
    pub grid_text_bytes: Option<u64>,
    /// Whether the workload driver step succeeded for this interval.
    pub driver_ok: bool,
}

impl SoakCapture {
    /// File name of the newest capture (for progress reporting).
    #[must_use]
    pub fn screenshot_basename(path: &str) -> String {
        basename(path)
    }
}

/// Keep only the final path segment so artifacts never embed host dirs.
#[must_use]
pub fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// RSS trend over a completed run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RssTrend {
    /// First RSS sample in MB.
    pub first_mb: f64,
    /// Last RSS sample in MB.
    pub last_mb: f64,
    /// Maximum RSS sample in MB.
    pub max_mb: f64,
}

/// Summarize an RSS series; `None` when no sample exists.
#[must_use]
pub fn rss_trend(samples_mb: &[f64]) -> Option<RssTrend> {
    let first_mb = *samples_mb.first()?;
    let last_mb = *samples_mb.last()?;
    let max_mb = samples_mb
        .iter()
        .fold(f64::NEG_INFINITY, |acc, v| acc.max(*v));
    Some(RssTrend {
        first_mb,
        last_mb,
        max_mb,
    })
}

/// Growth in percent from the first to the last RSS sample.
#[must_use]
pub fn rss_growth_pct(trend: &RssTrend) -> f64 {
    if trend.first_mb <= 0.0 {
        return 0.0;
    }
    (trend.last_mb - trend.first_mb) / trend.first_mb * 100.0
}

/// `true` when the soak peak RSS stays within the PB-3 typical-session
/// budget (8 tabs after a 4 h mixed session).
#[must_use]
pub fn meets_pb3(max_rss_mb: f64) -> bool {
    max_rss_mb <= crate::PB3_TYPICAL_RSS_MB as f64
}

// ---------------------------------------------------------------------------
// Artifacts: schedule plan JSON and measured evidence JSON
// ---------------------------------------------------------------------------

/// The committed soak evidence artifact, embedded at compile time.
#[must_use]
pub const fn committed_soak_json() -> &'static str {
    include_str!("../baselines/pb-real-soak.json")
}

/// Serialize a dry-run schedule (plan + config + provenance) for
/// `--dry-run` output. Status is always `"scheduled"`: a schedule promises
/// future captures, never past measurements.
#[must_use]
pub fn schedule_json(config: &SoakConfig, plan: &CapturePlan, meta: &BaselineMeta) -> String {
    let host = HostContext::capture();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {REAL_SOAK_SCHEMA_VERSION},\n"
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
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu\",\n",
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
    out.push_str("  \"soak\": {\n");
    out.push_str(&format!(
        "    \"status\": \"scheduled\",\n    \"duration_secs\": {},\n    \"interval_secs\": {},\n",
        config.duration_secs, config.interval_secs
    ));
    out.push_str(&format!(
        "    \"interval_effective_secs\": {},\n    \"workspace\": \"{}\",\n    \"workload\": \"{}\",\n",
        plan.interval_effective_secs,
        escape(&config.workspace),
        escape(&config.workload)
    ));
    out.push_str(&format!(
        "    \"planned_captures\": {}\n  }}\n}}\n",
        plan.captures
    ));
    out
}

/// Serialize completed soak evidence (status `"measured"`) or an honest
/// `"unavailable"` record carrying the reason. Screenshot entries are file
/// names only; no output directory or binary path is serialized.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn evidence_json(
    config: &SoakConfig,
    plan: &CapturePlan,
    captures: &[SoakCapture],
    trend: Option<RssTrend>,
    unavailable_reason: Option<&str>,
    meta: &BaselineMeta,
) -> String {
    let host = HostContext::capture();
    let measured = unavailable_reason.is_none() && !captures.is_empty();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {REAL_SOAK_SCHEMA_VERSION},\n"
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
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory and #pb-7-idle-cpu\",\n",
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
    out.push_str("  \"soak\": {\n");
    out.push_str(&format!("    \"status\": \"{}\",\n", status_of(!measured)));
    out.push_str(&format!(
        "    \"duration_secs\": {},\n    \"interval_secs\": {},\n    \"interval_effective_secs\": {},\n",
        config.duration_secs, config.interval_secs, plan.interval_effective_secs
    ));
    out.push_str(&format!(
        "    \"workspace\": \"{}\",\n    \"workload\": \"{}\",\n    \"completed_captures\": {},\n",
        escape(&config.workspace),
        escape(&config.workload),
        captures.len()
    ));
    match trend {
        Some(t) if measured => {
            out.push_str(&format!(
                "    \"rss_first_mb\": {:.3},\n    \"rss_last_mb\": {:.3},\n    \"rss_max_mb\": {:.3},\n    \"rss_growth_pct\": {:.2},\n    \"budget_mb\": {},\n",
                t.first_mb,
                t.last_mb,
                t.max_mb,
                rss_growth_pct(&t),
                crate::PB3_TYPICAL_RSS_MB
            ));
        }
        _ => {
            out.push_str("    \"rss_first_mb\": null,\n    \"rss_last_mb\": null,\n");
            out.push_str(&format!(
                "    \"budget_mb\": {},\n",
                crate::PB3_TYPICAL_RSS_MB
            ));
        }
    }
    if measured {
        out.push_str("    \"captures\": [\n");
        for (i, capture) in captures.iter().enumerate() {
            out.push_str("      {\n");
            out.push_str(&format!(
                "        \"index\": {},\n        \"at_secs\": {},\n        \"screenshot\": \"{}\",\n",
                capture.index,
                capture.at_secs,
                escape(&basename(&capture.screenshot))
            ));
            match capture.rss_mb {
                Some(mb) => out.push_str(&format!("        \"rss_mb\": {mb:.3},\n")),
                None => out.push_str("        \"rss_mb\": null,\n"),
            }
            match capture.grid_text_bytes {
                Some(bytes) => {
                    out.push_str(&format!("        \"grid_text_bytes\": {bytes},\n"));
                }
                None => out.push_str("        \"grid_text_bytes\": null,\n"),
            }
            out.push_str(&format!("        \"driver_ok\": {}\n", capture.driver_ok));
            out.push_str(if i + 1 == captures.len() {
                "      }\n"
            } else {
                "      },\n"
            });
        }
        out.push_str("    ]\n");
    } else {
        out.push_str(&format!(
            "    \"reason\": \"{}\"\n",
            escape(unavailable_reason.unwrap_or("no captures"))
        ));
    }
    out.push_str("  }\n}\n");
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

// ---------------------------------------------------------------------------
// Human report
// ---------------------------------------------------------------------------

/// Render a stable human-readable soak plan report for the bench and logs.
#[must_use]
pub fn format_soak_report(
    config: &SoakConfig,
    plan: &CapturePlan,
    leg: &CaptureLegStatus,
    gate: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("real_soak — long-duration real-render evidence (opt-in Tier 1 soak)\n");
    out.push_str(&format!(
        "budget: PB-3 peak <= {} MB RSS; PB-7 idle discipline via frame-on-demand\n",
        crate::PB3_TYPICAL_RSS_MB
    ));
    out.push_str(&format!(
        "plan: duration={} s interval={} s effective={} s captures={} workspace={} workload={}\n",
        config.duration_secs,
        config.interval_secs,
        plan.interval_effective_secs,
        plan.captures,
        config.workspace,
        config.workload
    ));
    if plan.interval_widened(config.interval_secs) {
        out.push_str(&format!(
            "note: interval widened from {} s to {} s to respect MAX_CAPTURES={}\n",
            config.interval_secs, plan.interval_effective_secs, MAX_CAPTURES
        ));
    }
    match gate {
        Some(reason) => out.push_str(&format!("gate: UNMEASURED ({reason})\n")),
        None => out.push_str("gate: open (BITTY_PERF_REAL_SOAK=1)\n"),
    }
    match &leg.reason {
        Some(reason) if !leg.available => {
            out.push_str(&format!("capture leg: UNAVAILABLE ({reason})\n"));
        }
        _ => out.push_str("capture leg: available (hyprctl+grim+jq, Hyprland session)\n"),
    }
    if gate.is_none() && leg.available {
        out.push_str("soak verdict: SCHEDULED (run scripts/real-render-soak.sh)\n");
    } else {
        out.push_str("soak verdict: UNMEASURED (opt-in Tier 1 evidence; see reasons above)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_includes_t0_and_trailing_capture() {
        let plan = plan_captures(600, 300);
        assert_eq!(plan.captures, 3);
        assert_eq!(plan.capture_at_secs, vec![0, 300, 600]);
        assert_eq!(plan.interval_effective_secs, 300);
        assert!(!plan.interval_widened(300));
    }

    #[test]
    fn default_four_hour_plan_has_49_captures() {
        let plan = plan_captures(DEFAULT_DURATION_SECS, DEFAULT_CAPTURE_INTERVAL_SECS);
        assert_eq!(plan.captures, 49);
        assert_eq!(plan.capture_at_secs[0], 0);
        assert_eq!(
            plan.capture_at_secs[plan.captures - 1],
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn extreme_cadence_widens_instead_of_overflowing() {
        let plan = plan_captures(MAX_DURATION_SECS, MIN_CAPTURE_INTERVAL_SECS);
        assert_eq!(plan.captures, MAX_CAPTURES);
        assert!(plan.interval_widened(MIN_CAPTURE_INTERVAL_SECS));
        assert!(plan.interval_effective_secs > MIN_CAPTURE_INTERVAL_SECS);
    }

    #[test]
    fn bounds_are_clamped_not_rejected() {
        let plan = plan_captures(1, 1);
        assert_eq!(plan.duration_secs, MIN_DURATION_SECS);
        assert_eq!(plan.interval_effective_secs, MIN_CAPTURE_INTERVAL_SECS);
        let plan = plan_captures(u64::MAX, u64::MAX);
        assert_eq!(plan.duration_secs, MAX_DURATION_SECS);
        assert_eq!(plan.interval_effective_secs, MAX_CAPTURE_INTERVAL_SECS);
    }

    #[test]
    fn workload_and_workspace_fall_back_honestly() {
        assert_eq!(validated_workload("mixed"), "mixed");
        assert_eq!(validated_workload("idle"), "idle");
        assert_eq!(validated_workload("input-spam"), "input-spam");
        assert_eq!(validated_workload("nope"), DEFAULT_WORKLOAD);
        assert_eq!(validated_workload(""), DEFAULT_WORKLOAD);
        assert_eq!(bravais_workspace("4"), "4");
        assert_eq!(bravais_workspace("10"), "10");
        assert_eq!(bravais_workspace("11"), DEFAULT_WORKSPACE);
        assert_eq!(bravais_workspace("studio"), DEFAULT_WORKSPACE);
        assert_eq!(bravais_workspace(""), DEFAULT_WORKSPACE);
    }

    #[test]
    fn leg_probe_reports_each_missing_tool() {
        let leg =
            CaptureLegStatus::probe(&[("hyprctl", true), ("grim", false), ("jq", false)], true);
        assert!(!leg.available);
        assert_eq!(leg.missing_tools, vec!["grim", "jq"]);
        assert!(leg.reason.is_some());
    }

    #[test]
    fn leg_probe_requires_a_hyprland_session() {
        let leg =
            CaptureLegStatus::probe(&[("hyprctl", true), ("grim", true), ("jq", true)], false);
        assert!(!leg.available);
        assert!(leg.missing_tools.is_empty());
        let leg = CaptureLegStatus::probe(&[("hyprctl", true), ("grim", true), ("jq", true)], true);
        assert!(leg.available);
        assert!(leg.reason.is_none());
    }

    #[test]
    fn rss_trend_and_pb3_verdict() {
        assert_eq!(rss_trend(&[]), None);
        let trend = rss_trend(&[100.0, 120.0, 110.0]).expect("trend");
        assert_eq!(trend.first_mb, 100.0);
        assert_eq!(trend.last_mb, 110.0);
        assert_eq!(trend.max_mb, 120.0);
        assert!((rss_growth_pct(&trend) - 10.0).abs() < f64::EPSILON);
        assert!(meets_pb3(crate::PB3_TYPICAL_RSS_MB as f64));
        assert!(!meets_pb3(crate::PB3_TYPICAL_RSS_MB as f64 + 1.0));
    }

    #[test]
    fn basenames_never_carry_host_dirs() {
        assert_eq!(
            SoakCapture::screenshot_basename("recording/real-soak/capture-0007.png"),
            "capture-0007.png"
        );
        assert_eq!(basename("capture-0007.png"), "capture-0007.png");
    }
}
