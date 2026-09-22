//! Throughput-floor baseline — PB-6 parse-and-render (CTX-0676).
//!
//! Headless sustained measurement over the fixed synthetic corpus: every
//! chunk runs parse (`Parser::advance`) → apply (`State::apply`) → render
//! (`GridRenderer::render`, fake rasterizer, no GPU). Prints per-round and
//! median MiB/s plus the floor verdict and exits 0 either way (the floor
//! gates on Tier 1 reference hardware, not on this host); `--write-baseline`
//! commits the artifact and refuses when the measurement failed (exit 2).
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`.
//! Runbook: `crates/bitty-perf/baselines/throughput-floor-evidence.md`.
//!
//! ```text
//! cargo bench -p bitty-perf --bench throughput_floor -- --nocapture
//! cargo bench -p bitty-perf --bench throughput_floor -- --nocapture --write-baseline crates/bitty-perf/baselines/pb-throughput-floor.json
//! ```

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::real_window::BaselineMeta;
use bitty_perf::throughput_floor::{
    ROUNDS, SAMPLE_BYTES, THROUGHPUT_FLOOR_BASELINE_REL_PATH, baseline_json, measure,
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
        "throughput_floor — PB-6 sustained parse-and-render (CTX-0676, fixed synthetic corpus)"
    );
    println!(
        "budget: docs/specifications/performance-budget-rfc.md#pb-6 (≥{} MiB/s sustained parse-and-render single core)",
        bitty_perf::PB6_THROUGHPUT_MB_S,
    );

    let report = match measure(SAMPLE_BYTES, ROUNDS) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("throughput_floor: measurement failed: {err}");
            exit(2);
        }
    };
    println!("\n{}", report.format_summary());

    if report.meets_budget() {
        println!("PB-6 verdict: PASS sustained floor met on this host");
    } else {
        println!("PB-6 verdict: ABOVE_BUDGET (see samples above)");
    }

    if let Some(path) = write_baseline {
        let json = baseline_json(&report, &meta_from_env(&report));
        let out_path = resolve_out(&path);
        if let Err(err) = std::fs::write(&out_path, json) {
            eprintln!("cannot write {}: {err}", out_path.display());
            exit(2);
        }
        println!("wrote throughput-floor baseline to {}", out_path.display());
        println!("baseline evidence: {THROUGHPUT_FLOOR_BASELINE_REL_PATH}");
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
fn meta_from_env(report: &bitty_perf::throughput_floor::ThroughputFloorReport) -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0676"),
        issues: vec![1061],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: format!(
            "cargo bench -p bitty-perf --bench throughput_floor -- --nocapture (sample_bytes={} rounds={})",
            report.sample_bytes, report.rounds
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
