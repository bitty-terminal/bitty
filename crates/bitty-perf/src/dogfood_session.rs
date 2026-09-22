//! Continuous daily-driver dogfood session automation (CTX-0643, PERF-10).
//!
//! The bounded headless harness (`bitty-runtime/tests/dogfooding.rs`,
//! `scripts/dogfood.sh`) proves each daily-driver surface — shell, cargo,
//! git, nvim, tmux, ssh — in isolation, and the long-duration real-render
//! soak (`real_soak.rs`, PERF-09) automates periodic pixel + grid + RSS
//! evidence for a synthetic workload. What was missing between them — and
//! what this module closes — is the **automated, schedulable continuous
//! daily-driver session**: a bounded run of a real `bitty` window on a
//! Tier 1 Hyprland host that cycles through all six app surfaces every
//! cycle (the way a daily driver actually works) and collects per-cycle
//! evidence through both the pixel path (`hyprctl` + `grim`) and the
//! DevTools-preferred introspection path (`bitty ctl terminal text`),
//! plus per-cycle RSS samples for the PB-3 typical-session anchor.
//!
//! ## Measurement contract
//!
//! - **Opt-in only.** A run requires `BITTY_PERF_DOGFOOD_SESSION=1` *and* a
//!   resolvable `bitty` binary *and* a live Hyprland capture leg
//!   (`hyprctl`, `grim`, `jq` on `PATH` plus a Hyprland session — the same
//!   leg `real_soak::capture_leg_status` probes). Without all three the
//!   result is `Unavailable` with a reason string; no number is ever
//!   fabricated. Headless CI therefore reports `Unavailable` by design and
//!   stays green.
//! - **Bounded and deterministic.** Duration and cycle cadence come from the
//!   environment but are clamped (`MIN_DURATION_SECS`,
//!   `MAX_DURATION_SECS`, `MIN_CYCLE_SECS`, `MAX_CYCLE_SECS`,
//!   `MAX_CYCLES`). The session plan (cycle count, effective cadence,
//!   cycle offsets) is a pure function of the clamped config, so
//!   `--dry-run` planning needs no display. The app set is validated
//!   against [`SESSION_APPS`]; unknown names never re-target the driver.
//! - **Host context from the environment.** OS, architecture, CPU count, and
//!   total memory are derived from standard interfaces; no checkout path,
//!   username, or hostname is embedded. Evidence artifacts record screenshot
//!   file names only, never absolute host paths.
//! - **Local-only evidence.** Cycle captures may show shell output, so the
//!   script drives a fixed synthetic per-app workload and writes everything
//!   under a caller-chosen output directory (default
//!   `recording/dogfood-session`, gitignored). Nothing is committed by the
//!   run; promoting an artifact to `baselines/` is an explicit, reviewed
//!   copy.
//!
//! `#![forbid(unsafe_code)]`; process and display probing is Unix-gated and
//! degrades to `Unavailable` on other platforms.
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`
//! (250 MB) and `#pb-7-idle-cpu`. Runbook:
//! `crates/bitty-perf/baselines/dogfood-session-evidence.md`. Automation
//! script: `scripts/dogfood-session.sh`.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use super::real_soak::{
    CaptureLegStatus, RssTrend, basename, bravais_workspace, resolve_out_dir, rss_growth_pct,
};
use super::real_window::{BaselineMeta, HostContext};

/// Env flag that opts a run into the continuous daily-driver session.
pub const SESSION_ENV: &str = "BITTY_PERF_DOGFOOD_SESSION";

/// Env var overriding the session duration in seconds.
pub const DURATION_ENV: &str = "BITTY_PERF_SESSION_DURATION_SECS";
/// Env var overriding the per-cycle cadence in seconds.
pub const CYCLE_ENV: &str = "BITTY_PERF_SESSION_CYCLE_SECS";
/// Env var overriding the Hyprland workspace that hosts the session window.
pub const WORKSPACE_ENV: &str = "BITTY_PERF_SESSION_WORKSPACE";
/// Env var overriding the driven app set (comma-separated, see
/// [`SESSION_APPS`]).
pub const APPS_ENV: &str = "BITTY_PERF_SESSION_APPS";
/// Env var overriding the evidence output directory.
pub const OUT_DIR_ENV: &str = "BITTY_PERF_SESSION_OUT_DIR";

/// Default session duration: 4 h, the PB-3 typical-session window.
pub const DEFAULT_DURATION_SECS: u64 = 14_400;
/// Minimum session duration: 60 s (shorter runs belong in `--dry-run`).
pub const MIN_DURATION_SECS: u64 = 60;
/// Maximum session duration: 24 h (longer runs are re-scheduled, not stretched).
pub const MAX_DURATION_SECS: u64 = 86_400;
/// Default per-cycle cadence: one full app rotation every 10 min.
pub const DEFAULT_CYCLE_SECS: u64 = 600;
/// Minimum per-cycle cadence: 60 s (tighter cadence would distort the session).
pub const MIN_CYCLE_SECS: u64 = 60;
/// Maximum per-cycle cadence: 1 h.
pub const MAX_CYCLE_SECS: u64 = 3_600;
/// Hard cap on cycles per run (keeps the plan and the output dir bounded).
pub const MAX_CYCLES: usize = 256;

/// Daily-driver app surfaces, in canonical drive order. Mirrors the
/// synthetic corpora in `bitty-runtime/tests/dogfooding.rs`.
pub const SESSION_APPS: &[&str] = &["shell", "cargo", "git", "nvim", "tmux", "ssh"];

/// Default evidence output directory, relative to the workspace root
/// (gitignored singular `recording/`, per the scratch-path convention).
pub const DEFAULT_OUT_DIR: &str = "recording/dogfood-session";

/// Committed session evidence artifact (relative to the repository root).
pub const DOGFOOD_SESSION_BASELINE_REL_PATH: &str =
    "crates/bitty-perf/baselines/pb-dogfood-session.json";

/// Schema version of the session evidence artifact.
pub const DOGFOOD_SESSION_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Session configuration (environment-derived, clamped, no host facts)
// ---------------------------------------------------------------------------

/// Clamped dogfood session configuration parsed from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// Total session wall time in seconds, clamped to
    /// `MIN_DURATION_SECS..=MAX_DURATION_SECS`.
    pub duration_secs: u64,
    /// Requested per-cycle cadence in seconds, clamped to
    /// `MIN_CYCLE_SECS..=MAX_CYCLE_SECS`.
    pub cycle_secs: u64,
    /// Hyprland workspace id hosting the session window (e.g. `"4"`).
    pub workspace: String,
    /// Validated app subset in canonical [`SESSION_APPS`] order.
    pub apps: Vec<String>,
    /// Evidence output directory (absolute when derived from a relative
    /// value: resolved against the workspace root).
    pub out_dir: PathBuf,
}

impl SessionConfig {
    /// Parse the configuration from the environment with clamping.
    ///
    /// Unknown app names are dropped (empty selection falls back to all of
    /// [`SESSION_APPS`]); a non-numeric workspace falls back to the shared
    /// default. Both fallbacks are reported in the human plan so a typo
    /// never silently re-targets a live session.
    #[must_use]
    pub fn from_env() -> Self {
        let duration_secs = env_u64(DURATION_ENV, DEFAULT_DURATION_SECS)
            .clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);
        let cycle_secs =
            env_u64(CYCLE_ENV, DEFAULT_CYCLE_SECS).clamp(MIN_CYCLE_SECS, MAX_CYCLE_SECS);
        let workspace = bravais_workspace(&std::env::var(WORKSPACE_ENV).unwrap_or_default());
        let apps = validated_session_apps(&std::env::var(APPS_ENV).unwrap_or_default());
        let out_dir = resolve_out_dir(&std::env::var(OUT_DIR_ENV).unwrap_or_default());
        Self {
            duration_secs,
            cycle_secs,
            workspace,
            apps,
            out_dir,
        }
    }
}

/// Validate a comma-separated app selection against [`SESSION_APPS`].
///
/// Selection is matched case-insensitively and returned in canonical drive
/// order with duplicates removed. An empty selection — or one with no
/// known app — falls back to the full set, so the driver never runs empty.
#[must_use]
pub fn validated_session_apps(raw: &str) -> Vec<String> {
    let selected: Vec<String> = raw
        .split(',')
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    let mut apps: Vec<String> = SESSION_APPS
        .iter()
        .filter(|app| selected.iter().any(|part| part == *app))
        .map(|app| (*app).to_string())
        .collect();
    if apps.is_empty() {
        apps = SESSION_APPS.iter().map(|app| (*app).to_string()).collect();
    }
    apps
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Session plan (pure function of the clamped config)
// ---------------------------------------------------------------------------

/// Bounded session plan derived from a [`SessionConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// Number of app-rotation cycles in this run (includes the `t = 0` cycle).
    pub cycles: usize,
    /// Effective cycle cadence in seconds (widened when the requested
    /// cadence would exceed [`MAX_CYCLES`]).
    pub cycle_effective_secs: u64,
    /// Session duration this plan covers.
    pub duration_secs: u64,
    /// Cycle offsets in seconds from session start (`cycle_at_secs[0] == 0`).
    pub cycle_at_secs: Vec<u64>,
}

impl SessionPlan {
    /// `true` when the requested cadence had to be widened to respect
    /// [`MAX_CYCLES`].
    #[must_use]
    pub fn cycle_widened(&self, requested: u64) -> bool {
        self.cycle_effective_secs != requested
    }
}

/// Derive the bounded session plan for a duration/cycle pair.
///
/// Cycle `0` fires at session start; further cycles follow every effective
/// cadence while `offset <= duration`. The count is clamped to
/// [`MAX_CYCLES`] by widening the effective cadence, never by dropping the
/// trailing cycle.
#[must_use]
pub fn plan_cycles(duration_secs: u64, cycle_secs: u64) -> SessionPlan {
    let duration_secs = duration_secs.clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);
    let cycle_secs = cycle_secs.clamp(MIN_CYCLE_SECS, MAX_CYCLE_SECS);
    let mut effective = cycle_secs;
    let mut count = duration_secs / effective + 1;
    if count > MAX_CYCLES as u64 {
        effective = duration_secs.div_ceil(MAX_CYCLES as u64).max(1);
        count = duration_secs / effective + 1;
    }
    let count = (count as usize).clamp(1, MAX_CYCLES);
    let cycle_at_secs = (0..count as u64).map(|i| i * effective).collect();
    SessionPlan {
        cycles: count,
        cycle_effective_secs: effective,
        duration_secs,
        cycle_at_secs,
    }
}

// ---------------------------------------------------------------------------
// Gate (the pixel leg is shared with the real-render soak)
// ---------------------------------------------------------------------------

/// The opt-in gate reason when the daily-driver session is disabled.
///
/// Returns `Some(reason)` unless `BITTY_PERF_DOGFOOD_SESSION=1`; callers
/// turn that into an `Unavailable` result rather than touching a display.
#[must_use]
pub fn session_gate_reason() -> Option<String> {
    if std::env::var(SESSION_ENV).as_deref() == Ok("1") {
        None
    } else {
        Some(format!(
            "{SESSION_ENV}!=1 (opt-in Tier 1 continuous daily-driver session)"
        ))
    }
}

// ---------------------------------------------------------------------------
// Per-cycle evidence rows
// ---------------------------------------------------------------------------

/// One completed app-rotation cycle of a daily-driver session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCycle {
    /// Zero-based cycle index.
    pub index: usize,
    /// Cycle offset in seconds from session start.
    pub at_secs: u64,
    /// Screenshot file reference (a file name; the serializer keeps the
    /// basename only).
    pub screenshot: String,
    /// Bitty RSS in MB at capture time (`None` when unreadable).
    pub rss_mb: Option<f64>,
    /// Grid-text snapshot size in bytes (`None` when the introspection
    /// leg was unavailable).
    pub grid_text_bytes: Option<u64>,
    /// How many of the configured apps the driver reached this cycle.
    pub apps_driven: usize,
    /// `false` when any leg (pixel, grid, or driver) missed this cycle.
    pub driver_ok: bool,
}

impl SessionCycle {
    /// Keep file names only: evidence must never embed the output dir.
    #[must_use]
    pub fn screenshot_basename(path: &str) -> String {
        basename(path)
    }
}

// ---------------------------------------------------------------------------
// Artifacts: schedule plan JSON and measured evidence JSON
// ---------------------------------------------------------------------------

/// The committed session evidence artifact, embedded at compile time.
#[must_use]
pub const fn committed_session_json() -> &'static str {
    include_str!("../baselines/pb-dogfood-session.json")
}

/// Serialize a dry-run schedule (plan + config + provenance) for
/// `--dry-run` output. Status is always `"scheduled"`: a schedule promises
/// future cycles, never past measurements.
#[must_use]
pub fn session_schedule_json(
    config: &SessionConfig,
    plan: &SessionPlan,
    meta: &BaselineMeta,
) -> String {
    let host = HostContext::capture();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {DOGFOOD_SESSION_SCHEMA_VERSION},\n"
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
    out.push_str("  \"session\": {\n");
    out.push_str(&format!(
        "    \"status\": \"scheduled\",\n    \"duration_secs\": {},\n    \"cycle_secs\": {},\n",
        config.duration_secs, config.cycle_secs
    ));
    out.push_str(&format!(
        "    \"cycle_effective_secs\": {},\n    \"workspace\": \"{}\",\n",
        plan.cycle_effective_secs,
        escape(&config.workspace),
    ));
    let apps: Vec<String> = config
        .apps
        .iter()
        .map(|app| format!("\"{}\"", escape(app)))
        .collect();
    out.push_str(&format!("    \"apps\": [{}],\n", apps.join(", ")));
    out.push_str(&format!(
        "    \"planned_cycles\": {}\n  }}\n}}\n",
        plan.cycles
    ));
    out
}

/// Serialize completed session evidence (status `"measured"`) or an honest
/// `"unavailable"` record carrying the reason. Screenshot entries are file
/// names only; no output directory or binary path is serialized.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn session_evidence_json(
    config: &SessionConfig,
    plan: &SessionPlan,
    cycles: &[SessionCycle],
    trend: Option<RssTrend>,
    unavailable_reason: Option<&str>,
    meta: &BaselineMeta,
) -> String {
    let host = HostContext::capture();
    let measured = unavailable_reason.is_none() && !cycles.is_empty();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {DOGFOOD_SESSION_SCHEMA_VERSION},\n"
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
    out.push_str("  \"session\": {\n");
    out.push_str(&format!(
        "    \"status\": \"{}\",\n",
        if measured { "measured" } else { "unavailable" }
    ));
    out.push_str(&format!(
        "    \"duration_secs\": {},\n    \"cycle_secs\": {},\n    \"cycle_effective_secs\": {},\n",
        config.duration_secs, config.cycle_secs, plan.cycle_effective_secs,
    ));
    out.push_str(&format!(
        "    \"workspace\": \"{}\",\n",
        escape(&config.workspace),
    ));
    let apps: Vec<String> = config
        .apps
        .iter()
        .map(|app| format!("\"{}\"", escape(app)))
        .collect();
    out.push_str(&format!("    \"apps\": [{}],\n", apps.join(", ")));
    out.push_str(&format!(
        "    \"completed_cycles\": {},\n",
        if measured { cycles.len() } else { 0 }
    ));
    match trend {
        Some(ref trend) if measured => {
            out.push_str(&format!(
                "    \"rss_first_mb\": {},\n    \"rss_last_mb\": {},\n    \"rss_max_mb\": {},\n    \"rss_growth_pct\": {},\n",
                trend.first_mb,
                trend.last_mb,
                trend.max_mb,
                rss_growth_pct(trend),
            ));
        }
        _ => {
            out.push_str(
                "    \"rss_first_mb\": null,\n    \"rss_last_mb\": null,\n    \"rss_max_mb\": null,\n",
            );
        }
    }
    out.push_str("    \"budget_mb\": 250,\n");
    if let Some(reason) = unavailable_reason {
        out.push_str(&format!("    \"reason\": \"{}\",\n", escape(reason)));
    }
    out.push_str("    \"cycles\": [");
    for (i, cycle) in cycles.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\n      {{\"index\": {}, \"at_secs\": {}, \"screenshot\": \"{}\", ",
            cycle.index,
            cycle.at_secs,
            escape(&SessionCycle::screenshot_basename(&cycle.screenshot)),
        ));
        match cycle.rss_mb {
            Some(rss) => out.push_str(&format!("\"rss_mb\": {rss}, ")),
            None => out.push_str("\"rss_mb\": null, "),
        }
        match cycle.grid_text_bytes {
            Some(bytes) => out.push_str(&format!("\"grid_text_bytes\": {bytes}, ")),
            None => out.push_str("\"grid_text_bytes\": null, "),
        }
        out.push_str(&format!(
            "\"apps_driven\": {}, \"driver_ok\": {}}}",
            cycle.apps_driven, cycle.driver_ok,
        ));
    }
    if cycles.is_empty() {
        out.push_str("]\n  }\n}\n");
    } else {
        out.push_str("\n    ]\n  }\n}\n");
    }
    out
}

/// Human-readable session plan report for the planner bench.
///
/// Without the opt-in gate (or without the capture leg) the report says
/// `UNMEASURED` with reasons and never prints a fabricated number.
#[must_use]
pub fn format_session_report(
    config: &SessionConfig,
    plan: &SessionPlan,
    leg: &CaptureLegStatus,
    gate: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("dogfood session plan:\n");
    out.push_str(&format!(
        "  duration={}s cycle={}s effective={}s cycles={} workspace={} apps={}\n",
        config.duration_secs,
        config.cycle_secs,
        plan.cycle_effective_secs,
        plan.cycles,
        config.workspace,
        config.apps.join(","),
    ));
    if plan.cycle_widened(config.cycle_secs) {
        out.push_str(&format!(
            "  note: cadence widened to {}s to respect MAX_CYCLES={}\n",
            plan.cycle_effective_secs, MAX_CYCLES,
        ));
    }
    out.push_str(&format!(
        "  capture leg: available={} {}\n",
        leg.available,
        leg.reason.as_deref().unwrap_or("(all tools present)"),
    ));
    match gate {
        Some(reason) => {
            out.push_str(&format!("  gate: UNMEASURED ({reason})\n"));
        }
        None if !leg.available => {
            out.push_str("  gate: UNMEASURED (capture leg unavailable)\n");
        }
        None => {
            out.push_str("  gate: open (Tier 1 live session)\n");
        }
    }
    out
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
