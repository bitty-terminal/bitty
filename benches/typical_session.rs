//! Typical-session memory + growth — PB-3 bounded evidence (CTX-0676).
//!
//! Headless synthetic anchor: 8 real `State` tabs fed a fixed workload,
//! self-RSS before/open/close, reclaim check. Prints the verdict and exits 0
//! either way (budgets gate on Tier 1, not on this host); `--write-baseline`
//! commits the artifact and refuses to write when unmeasured (exit 2, no
//! fabricated numbers).
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`.
//! Runbook: `crates/bitty-perf/baselines/typical-session-evidence.md`.
//!
//! ```text
//! cargo bench -p bitty-perf --bench typical_session -- --nocapture
//! cargo bench -p bitty-perf --bench typical_session -- --nocapture --write-baseline crates/bitty-perf/baselines/pb-typical-session.json
//! ```

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::real_window::BaselineMeta;
use bitty_perf::typical_session::{
    TYPICAL_SESSION_BASELINE_REL_PATH, baseline_json, measure_typical_session,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write_baseline: Option<String> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--write-baseline" {
            write_baseline = rest.next().cloned();
        }
    }

    println!(
        "typical_session — PB-3 typical-session memory + growth (CTX-0676, bounded synthetic)"
    );
    println!(
        "budget: docs/specifications/performance-budget-rfc.md#pb-3 ({} MB 8 tabs; reclaim within {}% after close)",
        bitty_perf::PB3_TYPICAL_RSS_MB,
        bitty_perf::PB3_RECLAIM_PCT,
    );

    let report = measure_typical_session();
    println!("\n{}", report.format_summary());

    if report.is_unavailable() {
        println!("PB-3 verdict: UNMEASURED (see reason above)");
    } else {
        if report.meets_open_budget() {
            println!("PB-3 verdict: PASS 8-tab RSS within budget");
        } else {
            println!("PB-3 verdict: ABOVE_BUDGET (see samples above)");
        }
        if report.meets_reclaim() {
            println!("PB-3 reclaim verdict: PASS within reclaim budget");
        } else {
            println!("PB-3 reclaim verdict: ABOVE_BUDGET (see samples above)");
        }
    }

    if let Some(path) = write_baseline {
        match baseline_json(&report, &meta_from_env()) {
            Ok(json) => {
                let out_path = resolve_out(&path);
                if let Err(err) = std::fs::write(&out_path, json) {
                    eprintln!("cannot write {}: {err}", out_path.display());
                    exit(2);
                }
                println!("wrote typical-session baseline to {}", out_path.display());
                println!("baseline evidence: {TYPICAL_SESSION_BASELINE_REL_PATH}");
            }
            Err(err) => {
                eprintln!("refusing to write baseline: {err}");
                exit(2);
            }
        }
    }
}

/// Resolve a `--write-baseline` path: absolute paths are used as-is, relative
/// paths resolve against the workspace root (bench executables run with the
/// package directory as CWD, not the repository root).
fn resolve_out(path: &str) -> std::path::PathBuf {
    let candidate = std::path::Path::new(path);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|dir| dir.parent())
        .expect("bench must live at <workspace>/benches")
        .join(candidate)
}

/// Build provenance from the environment so the crate never embeds host facts.
fn meta_from_env() -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0676"),
        issues: vec![1058],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "cargo bench -p bitty-perf --bench typical_session -- --nocapture --write-baseline",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
