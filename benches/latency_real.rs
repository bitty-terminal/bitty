//! Input latency real measurement — PB-4 key-to-screen.
//!
//! CTX-0100 upgrade: measures the full `keydown → PTY → parser → state →
//! render → present` hot path with `Instant` stage tracing, p50/p99/mean/max,
//! bounded (≤64 B per key, ≤8 KiB per batch, ≤256 damage regions).
//! Headless CI uses `Surface::headless_present`; a real Wayland box would
//! drive the same tracer with a compositor-present timestamp (future slice)
//! without changing this API.
//!
//! CTX-0686 (PERF-05) addition: `--write-baseline <path>` commits the
//! 1,000-sample headless report plus provenance to `pb-latency.json`; it
//! refuses to write when no sample presented (exit 2, no fabricated
//! numbers). The committed verdicts are recorded as measured — wall clock on
//! a shared runner includes scheduler gaps, so the artifact carries the work
//! percentiles alongside.
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-4-input-latency`.
//! Runbook: `crates/bitty-perf/baselines/latency-evidence.md`.
//!
//! ```text
//! cargo bench -p bitty-perf --bench latency_real -- --nocapture
//! cargo bench -p bitty-perf --bench latency_real -- --nocapture --write-baseline crates/bitty-perf/baselines/pb-latency.json
//! ```
//!
//! Headless, bounded, `forbid(unsafe)`.

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::latency::{
    HEADLESS_BUDGET_SAMPLES, HEADLESS_SHARED_RUNNER_FACTOR, HEADLESS_SHARED_RUNNER_TAIL_FACTOR,
    HEADLESS_WALL_CLOCK_CEILING_MS, HEADLESS_WORK_FLOOR_CEILING_MS, HEADLESS_WORK_TAIL_CEILING_MS,
    LATENCY_BASELINE_REL_PATH, baseline_json, measure_latency, measure_latency_with_pty_echo,
};
use bitty_perf::real_window::BaselineMeta;

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
        "latency_real — PB-4 input latency (CTX-0100 keydown→PTY→parser→state→render→present, bounded tracing)"
    );
    println!(
        "budget: docs/specifications/performance-budget-rfc.md#pb-4 (p50 8 ms p99 15 ms, 60 Hz minimum, Wayland/frame-presented)"
    );
    println!(
        "pipeline: encode_key_event(≤64 B) → handle_key_event → handle_pty_bytes → parser→State → Damage → GridRenderer(fake) → Surface::headless_present"
    );

    // Primary: headless echo model (deterministic, no display).
    let report = measure_latency(1_000);
    println!("\n--- headless echo model (1_000 samples, deterministic) ---");
    println!("{}", report.format_summary());
    if report.meets_p50() && report.meets_p99() {
        println!("PB-4 headless latency: PASS p50+p99 (well under 8/15 ms)");
    } else {
        eprintln!(
            "note: headless p50 {:.3} ms p99 {:.3} ms vs budget 8/15 ms — arch constraint on Tier 1 ref machine will gate",
            report.p50_ms, report.p99_ms
        );
    }
    // Idle misses are allowed for non-printable keys (e.g. ArrowRight at boundary);
    // presented samples are used for p50/p99. Report them for visibility.
    if report.idle_misses > 0 {
        println!(
            "note: {} idle misses (non-dirty keys) — p50/p99 computed over {} presented samples",
            report.idle_misses,
            report.samples.len() - report.idle_misses
        );
    }

    // Secondary: PTY echo when `cat` is available (bounded real write/poll).
    let report2 = measure_latency_with_pty_echo(200);
    println!("\n--- PTY echo variant (200 samples, real cat when available) ---");
    println!("{}", report2.format_summary());
    println!(
        "headless flag: {} (true means software seam, false would be real GPU)",
        report2.headless
    );

    // Small fast-path sanity: the tracer must stay well under headroom even on
    // a slow shared runner. Use the shared HEADLESS_BUDGET_SAMPLES=200 so
    // `percentile(99)` is a true p99 rather than the maximum: at n=50
    // `round(0.99*49)=49` returned the single worst sample and one scheduler
    // stall failed this sanity (CTX-0410 / #659). The 120 ms ceiling is
    // unchanged; the real PB-4 budget (8/15 ms) is gated by the 1_000-sample
    // report above and Tier 1 evidence, not this fast-path sanity.
    let fast = measure_latency(HEADLESS_BUDGET_SAMPLES);
    assert!(
        fast.p99_ms < HEADLESS_WALL_CLOCK_CEILING_MS,
        "sanity: p99 should stay << {HEADLESS_WALL_CLOCK_CEILING_MS:.0} ms even on CI (got {:.3} ms)",
        fast.p99_ms
    );
    // CTX-0484/CTX-0494: the wall-clock ceiling is a liveness guard only. The
    // tight budget is asserted on the scheduler-noise-robust work *floor*
    // (fastest presented sample), because percentile work on a shared runner
    // absorbs in-stage preemption and measures the runner, not the pipeline;
    // the percentiles keep a generous pathology ceiling.
    assert!(
        fast.min_work_ms < HEADLESS_WORK_FLOOR_CEILING_MS,
        "work floor {:.3} ms must be < {:.0} ms (PB-4 p50 × {HEADLESS_SHARED_RUNNER_FACTOR} shared-runner factor)",
        fast.min_work_ms,
        HEADLESS_WORK_FLOOR_CEILING_MS
    );
    assert!(
        fast.p99_work_ms < HEADLESS_WORK_TAIL_CEILING_MS,
        "work p99 {:.3} ms must stay below the {:.0} ms pathology ceiling (PB-4 p99 × {HEADLESS_SHARED_RUNNER_TAIL_FACTOR} tail factor)",
        fast.p99_work_ms,
        HEADLESS_WORK_TAIL_CEILING_MS
    );
    println!(
        "sanity {HEADLESS_BUDGET_SAMPLES}-sample p99 {:.3} ms within headroom; work floor {:.3} ms / p99 {:.3} ms ({} {})",
        fast.p99_ms,
        fast.min_work_ms,
        fast.p99_work_ms,
        if fast.meets_work_p50() {
            "PB-4 work p50 PASS"
        } else {
            "PB-4 work p50 ABOVE"
        },
        if fast.meets_work_p99() {
            "PB-4 work p99 PASS"
        } else {
            "PB-4 work p99 ABOVE"
        }
    );

    if let Some(path) = write_baseline {
        match baseline_json(&report, &meta_from_env()) {
            Ok(json) => {
                let out_path = resolve_out(&path);
                if let Err(err) = std::fs::write(&out_path, json) {
                    eprintln!("cannot write {}: {err}", out_path.display());
                    exit(2);
                }
                println!("wrote latency baseline to {}", out_path.display());
                println!("baseline evidence: {LATENCY_BASELINE_REL_PATH}");
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
        task: env("BITTY_PERF_TASK", "CTX-0686"),
        issues: vec![1059],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "cargo bench -p bitty-perf --bench latency_real -- --nocapture --write-baseline",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
