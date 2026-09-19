//! Parser-throughput baseline, M1-11 (CTX-0576).
//!
//! Deterministic, headless, bounded measurement of `bitty-vt::Parser::advance`
//! over representative VT/escape corpora, plus the ratio gate against the
//! committed baseline in `crates/bitty-perf/baselines/parser-throughput.json`.
//! The milestone "Performance guardrail" requires a recorded baseline number
//! and no pathological regression versus plain-text throughput; this bench is
//! the human-facing half of that guard, and
//! `crates/bitty-perf/tests/parser_throughput_regression.rs` is the CI half.
//!
//! The parser is isolated from terminal state and rendering, so a change that
//! only slows `State::apply` or the renderer does not move these numbers.
//!
//! `#![forbid(unsafe_code)]`, headless (no `winit::Window`, no `wgpu::Surface`),
//! bounded (`MAX_CHUNK_BYTES` 8 KiB, `MAX_SEGMENT_BYTES` 64 KiB,
//! `MAX_ACTIONS` 4096), deterministic corpora.
//!
//! Budget reference: `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`.
//! Evidence: `crates/bitty-perf/baselines/README.md` (repo-owned runbook).
//!
//! Run headlessly:
//! ```text
//! cargo bench -p bitty-perf --bench parser_throughput -- --nocapture
//! cargo bench -p bitty-perf --bench parser_throughput -- --write-baseline /tmp/parser-throughput.json
//! ```

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::parser_throughput::{
    ABSOLUTE_FLOOR_MB_S, BaselineMeta, PARSER_BASELINE_REL_PATH, REGRESSION_FACTOR, RELEASE_ROUNDS,
    RELEASE_SAMPLE_BYTES, baseline_json, committed_baseline, format_report, measure,
    regression_check,
};

fn main() {
    let mut write_baseline: Option<String> = None;
    let mut iters = 1usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write-baseline" => {
                write_baseline = args.next();
            }
            "--iters" => {
                iters = args
                    .next()
                    .and_then(|v| v.parse::<usize>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or_else(|| {
                        eprintln!("--iters requires a positive integer");
                        exit(2);
                    });
            }
            other => {
                // Ignore libtest-style flags cargo passes through (`--nocapture`).
                let _ = other;
            }
        }
    }

    let sample_bytes = RELEASE_SAMPLE_BYTES * iters;
    let report = match measure(sample_bytes, RELEASE_ROUNDS) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("parser_throughput: measurement failed: {err}");
            exit(2);
        }
    };

    print!("{}", format_report(&report));

    let baseline = match committed_baseline() {
        Ok(baseline) => baseline,
        Err(err) => {
            eprintln!("parser_throughput: committed baseline invalid: {err}");
            exit(2);
        }
    };
    let regression = regression_check(&report, &baseline);
    println!(
        "regression gate (ratio >= committed/{REGRESSION_FACTOR:.0}, optimized floor {ABSOLUTE_FLOOR_MB_S:.0} MiB/s):"
    );
    print!("{}", regression.format_summary());
    let verdict = if regression.all_passed() {
        "PASS"
    } else {
        "FAIL"
    };
    println!("baseline evidence: {PARSER_BASELINE_REL_PATH}");

    if let Some(path) = write_baseline {
        let meta = meta_from_env(&report);
        let json = baseline_json(&report, &meta);
        if let Err(err) = std::fs::write(&path, json) {
            eprintln!("parser_throughput: cannot write {path}: {err}");
            exit(2);
        }
        println!("wrote baseline candidate to {path}");
    }

    println!("parser_throughput verdict: {verdict}");
    if !regression.all_passed() {
        exit(1);
    }
}

/// Build provenance from the environment. Values are captured by the caller
/// (the `just` recipe fills them) so the crate itself never invents host facts
/// or embeds a host path.
fn meta_from_env(report: &bitty_perf::parser_throughput::ParserThroughputReport) -> BaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    BaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0576"),
        issue: env("BITTY_PERF_ISSUE", "1137")
            .parse::<u64>()
            .unwrap_or(1137),
        milestone_item: env("BITTY_PERF_MILESTONE_ITEM", "M1-11"),
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: format!(
            "cargo bench -p bitty-perf --bench parser_throughput -- --nocapture (sample_bytes={} rounds={})",
            report.sample_bytes, report.rounds
        ),
        profile: "bench (release)".to_string(),
        toolchain: env("BITTY_PERF_TOOLCHAIN", "unspecified-toolchain"),
        os: env("BITTY_PERF_OS", "unspecified-os"),
        machine_class: env("BITTY_PERF_MACHINE_CLASS", "unspecified-machine-class"),
        budget_ref: "docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor"
            .to_string(),
    }
}
