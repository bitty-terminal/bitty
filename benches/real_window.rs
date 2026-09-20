//! Real-window PB-1 startup + PB-2 idle-memory evidence (CTX-0592).
//!
//! Opt-in Tier 1 evidence bench: it launches the real `bitty` binary and
//! records launch-to-first-frame startup (PB-1) and post-idle RSS (PB-2).
//! Without `BITTY_PERF_REAL_WINDOW=1` (and a resolvable `bitty` binary) it
//! prints `UNMEASURED` with a reason and exits 0 — headless CI stays green and
//! never fabricates numbers.
//!
//! Bounded and deterministic: sample count, idle window, and per-launch
//! timeout are clamped constants (`MAX_STARTUP_SAMPLES`, `MAX_IDLE_SECS`), and
//! every spawned child is killed and reaped on every path.
//!
//! `#![forbid(unsafe_code)]`; headless-CI-safe (no display required).
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-1-cold-startup-time` and
//! `#pb-2-idle-memory`. Runbook:
//! `crates/bitty-perf/baselines/real-window-evidence.md`.
//!
//! Run on a Tier 1 host:
//! ```text
//! BITTY_PERF_REAL_WINDOW=1 cargo bench -p bitty-perf --bench real_window -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::real_window::{
    BaselineMeta, REAL_WINDOW_BASELINE_REL_PATH, REGRESSION_FACTOR, baseline_json, format_report,
    measure_idle_evidence, measure_startup_evidence,
};

fn main() {
    let mut write_baseline: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--write-baseline" {
            write_baseline = args.next();
        }
    }

    let startup = measure_startup_evidence();
    let idle = measure_idle_evidence();
    print!("{}", format_report(&startup, &idle));

    if let Some(path) = write_baseline {
        if startup.is_unavailable() || idle.is_unavailable() {
            eprintln!(
                "refusing to write baseline: PB-1 and PB-2 must both be measured (no fabricated numbers)"
            );
            exit(2);
        }
        let meta = meta_from_env();
        let json = baseline_json(&startup, &idle, &meta);
        if let Err(err) = std::fs::write(&path, json) {
            eprintln!("cannot write {path}: {err}");
            exit(2);
        }
        println!("wrote real-window baseline to {path}");
        println!("baseline evidence: {REAL_WINDOW_BASELINE_REL_PATH}");
        return;
    }

    // Opt-in regression comparison against the committed baseline, only when
    // both measurements are available (Tier 1 runs).
    if !startup.is_unavailable() && !idle.is_unavailable() {
        println!(
            "note: committed baseline at {REAL_WINDOW_BASELINE_REL_PATH}; regression factor {REGRESSION_FACTOR:.1} (opt-in check is informational)"
        );
        if startup.meets_p50() && startup.meets_p99() {
            println!("PB-1 verdict: PASS p50+p99");
        } else {
            println!("PB-1 verdict: ABOVE_BUDGET (see samples above)");
        }
        if idle.meets_budget() {
            println!("PB-2 verdict: PASS");
        } else {
            println!("PB-2 verdict: ABOVE_BUDGET");
        }
    } else {
        println!("real_window verdict: UNMEASURED (opt-in Tier 1 evidence; see reason above)");
    }
}

/// Build provenance from the environment so the crate never embeds host facts.
fn meta_from_env() -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0592"),
        issues: vec![1056, 1057],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "BITTY_PERF_REAL_WINDOW=1 cargo bench -p bitty-perf --bench real_window -- --nocapture",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
