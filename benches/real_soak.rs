//! Long-duration real-render soak planner (CTX-0642, PERF-09).
//!
//! Headless-safe planner for the Tier 1 soak: it loads the clamped
//! [`SoakConfig`](bitty_perf::real_soak::SoakConfig) from the environment,
//! derives the bounded [`CapturePlan`](bitty_perf::real_soak::CapturePlan),
//! probes the `hyprctl` + `grim` capture leg, and prints the human report.
//! Without `BITTY_PERF_REAL_SOAK=1` (or without a Hyprland session) it
//! prints `UNMEASURED` with reasons and exits 0 — headless CI stays green
//! and never fabricates numbers.
//!
//! The actual long run is `scripts/real-render-soak.sh`, which executes this
//! plan against a live window. `--write-schedule <path>` writes the
//! deterministic schedule JSON (status `"scheduled"`: a promise of future
//! captures, not past measurements), so planning output stays reviewable.
//!
//! ```text
//! cargo bench -p bitty-perf --bench real_soak -- --nocapture
//! cargo bench -p bitty-perf --bench real_soak -- --nocapture --write-schedule /tmp/soak-plan.json
//! ```
//!
//! `#![forbid(unsafe_code)]`; headless-CI-safe (no display required).

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::real_soak::{
    CapturePlan, SoakConfig, capture_leg_status, format_soak_report, plan_captures, schedule_json,
    soak_gate_reason,
};
use bitty_perf::real_window::BaselineMeta;

fn main() {
    let mut write_schedule: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--write-schedule" {
            write_schedule = args.next();
        }
    }

    let config = SoakConfig::from_env();
    let plan: CapturePlan = plan_captures(config.duration_secs, config.interval_secs);
    let leg = capture_leg_status();
    let gate = soak_gate_reason();
    print!(
        "{}",
        format_soak_report(&config, &plan, &leg, gate.as_deref())
    );

    if let Some(path) = write_schedule {
        let meta = meta_from_env();
        let json = schedule_json(&config, &plan, &meta);
        if let Err(err) = std::fs::write(&path, json) {
            eprintln!("cannot write {path}: {err}");
            exit(2);
        }
        println!("wrote soak schedule to {path}");
    }
}

/// Build provenance from the environment so the crate never embeds host facts.
fn meta_from_env() -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0642"),
        issues: vec![1063],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "bash scripts/real-render-soak.sh --out-dir recording/real-soak",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
