//! Continuous daily-driver dogfood session planner (CTX-0643, PERF-10).
//!
//! Headless-safe planner for the Tier 1 dogfood session: it loads the
//! clamped [`SessionConfig`](bitty_perf::dogfood_session::SessionConfig)
//! from the environment, derives the bounded
//! [`SessionPlan`](bitty_perf::dogfood_session::SessionPlan), probes the
//! `hyprctl` + `grim` capture leg (shared with the real-render soak), and
//! prints the human report. Without `BITTY_PERF_DOGFOOD_SESSION=1` (or
//! without a Hyprland session) it prints `UNMEASURED` with reasons and
//! exits 0 — headless CI stays green and never fabricates numbers.
//!
//! The actual long run is `scripts/dogfood-session.sh`, which executes this
//! plan against a live window. `--write-schedule <path>` writes the
//! deterministic schedule JSON (status `"scheduled"`: a promise of future
//! cycles, not past measurements), so planning output stays reviewable.
//!
//! ```text
//! cargo bench -p bitty-perf --bench dogfood_session -- --nocapture
//! cargo bench -p bitty-perf --bench dogfood_session -- --nocapture --write-schedule /tmp/session-plan.json
//! ```
//!
//! `#![forbid(unsafe_code)]`; headless-CI-safe (no display required).

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::dogfood_session::{
    SessionConfig, SessionPlan, format_session_report, plan_cycles, session_gate_reason,
    session_schedule_json,
};
use bitty_perf::real_soak::capture_leg_status;
use bitty_perf::real_window::BaselineMeta;

fn main() {
    let mut write_schedule: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--write-schedule" {
            write_schedule = args.next();
        }
    }

    let config = SessionConfig::from_env();
    let plan: SessionPlan = plan_cycles(config.duration_secs, config.cycle_secs);
    let leg = capture_leg_status();
    let gate = session_gate_reason();
    print!(
        "{}",
        format_session_report(&config, &plan, &leg, gate.as_deref())
    );

    if let Some(path) = write_schedule {
        let meta = meta_from_env();
        let json = session_schedule_json(&config, &plan, &meta);
        if let Err(err) = std::fs::write(&path, json) {
            eprintln!("cannot write {path}: {err}");
            exit(2);
        }
        println!("wrote dogfood session schedule to {path}");
    }
}

/// Build provenance from the environment so the crate never embeds host facts.
fn meta_from_env() -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0643"),
        issues: vec![1064],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "bash scripts/dogfood-session.sh --out-dir recording/dogfood-session",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
