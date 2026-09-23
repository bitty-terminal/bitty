//! Idle invariant — PB-7 frame-on-demand, ≤1 % CPU over 10 min.
//!
//! CTX-0100 upgrade: proves zero periodic wakeups when idle via the
//! `tick == None` invariant and GridRenderer `FrameMode::Clean` path, with
//! bounded cost sampling (idle tick mean, clean render mean) and an optional
//! `ps %cpu` sample. No polling loop, no unnecessary redraw, no 10-minute
//! sleep required; real ≤1 % over 10 min is gated on the Tier 1 ref machine.
//!
//! CTX-0636 (PERF-08) addition: an extended idle-CPU/wakeup measurement that
//! re-execs this binary as a parked-`Runtime` child (`--idle-child`) and
//! samples its `/proc` CPU and context-switch counters across a bounded
//! window (`--idle-window`, default 60 s, max 600 s — the full PB-7
//! 10-minute acceptance window). `--write-baseline`
//! commits the numbers plus provenance to `pb-idle.json`; it refuses to
//! write when the frame-on-demand checks fail or the window is unmeasured.
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-7-idle-resource-usage`.
//! Runbook: `crates/bitty-perf/baselines/idle-evidence.md`.
//!
//! Headless, bounded, `forbid(unsafe)`.

#![forbid(unsafe_code)]

use std::process::exit;

use bitty_perf::idle::{
    IDLE_BASELINE_REL_PATH, IDLE_CHILD_ARG, IdleBaselineMeta, baseline_json, check_idle,
    idle_window_secs, measure_idle_cpu_with, run_idle_child,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Idle-subject child mode: park a proven-idle Runtime, then exit.
    if let Some(pos) = args.iter().position(|arg| arg == IDLE_CHILD_ARG) {
        let secs = args
            .get(pos + 1)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(idle_window_secs());
        run_idle_child(secs);
    }

    let mut write_baseline: Option<String> = None;
    let mut window_override: Option<u64> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--write-baseline" {
            write_baseline = rest.next().cloned();
        } else if arg == "--idle-window" {
            window_override = rest.next().and_then(|value| value.parse::<u64>().ok());
        }
    }

    println!("idle_real — PB-7 idle resource (CTX-0100 frame-on-demand invariant, bounded)");
    println!(
        "budget: docs/specifications/performance-budget-rfc.md#pb-7 (≤1% avg CPU over 10 min, zero wakeups when idle)"
    );
    println!(
        "invariant: tick returns None when no new generation and no pending_full_redraw → ControlFlow::Wait → zero periodic wakeups"
    );

    let report = check_idle();
    println!("\n{}", report.format_summary());

    assert!(
        report.all_passed(),
        "PB-7 frame-on-demand: all idle checks must PASS ({} failed)",
        report.checks.iter().filter(|c| !c.passed).count()
    );
    assert!(
        report.clean_is_clean,
        "PB-7 clean render must be FrameMode::Clean with no draws"
    );

    // Cost sanity: idle path must stay well under PB-4 p50 headroom (<< 8 ms).
    assert!(
        report.idle_tick_mean_us < 5_000.0,
        "idle tick mean {:.2} µs must be << 8 ms p50 headroom",
        report.idle_tick_mean_us
    );
    assert!(
        report.clean_render_mean_us < 5_000.0,
        "clean render mean {:.2} µs must be << 8 ms",
        report.clean_render_mean_us
    );

    if let Some(path) = write_baseline {
        let window = window_override.unwrap_or_else(idle_window_secs);
        let cpu = measure_idle_cpu_with(window);
        println!("\n{}", cpu.format_summary());
        if !report.all_passed() || cpu.is_unavailable() {
            eprintln!(
                "refusing to write baseline: frame-on-demand must pass and the idle window must be measured (no fabricated numbers)"
            );
            exit(2);
        }
        let meta = meta_from_env();
        let json = baseline_json(&report, &cpu, &meta);
        let out_path = resolve_out(&path);
        if let Err(err) = std::fs::write(&out_path, json) {
            eprintln!("cannot write {}: {err}", out_path.display());
            exit(2);
        }
        println!("wrote idle baseline to {}", out_path.display());
        println!("baseline evidence: {IDLE_BASELINE_REL_PATH}");
        if cpu.meets_budget() {
            println!("PB-7 verdict: PASS idle CPU within budget");
        } else {
            println!("PB-7 verdict: ABOVE_BUDGET (see samples above)");
        }
        return;
    }

    if let Some(window) = window_override {
        let cpu = measure_idle_cpu_with(window);
        println!("\n{}", cpu.format_summary());
        if cpu.meets_budget() {
            println!("PB-7 verdict: PASS idle CPU within budget");
        } else if cpu.is_unavailable() {
            println!("PB-7 verdict: UNMEASURED (see reason above)");
        } else {
            println!("PB-7 verdict: ABOVE_BUDGET (see samples above)");
        }
        return;
    }

    if report.cpu_budget_verdict() != bitty_perf::idle::CpuBudgetVerdict::Met {
        eprintln!(
            "note: PB-7 CPU sample {:?} is {:?} — the real 10 min ≤1 % measurement is gated on the Tier 1 reference machine",
            report.sampled_cpu_pct,
            report.cpu_budget_verdict()
        );
    }
    println!(
        "note: extended idle-CPU/wakeup sample runs with --idle-window <secs> (or --write-baseline); see {IDLE_BASELINE_REL_PATH}"
    );
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
fn meta_from_env() -> IdleBaselineMeta {
    let env = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.to_string());
    IdleBaselineMeta {
        task: env("BITTY_PERF_TASK", "CTX-0636"),
        issues: vec![1062],
        captured_at: env("BITTY_PERF_DATE", "unspecified-date"),
        revision: env("BITTY_PERF_REVISION", "unspecified-revision"),
        command: env(
            "BITTY_PERF_COMMAND",
            "cargo bench -p bitty-perf --bench idle_real -- --nocapture --write-baseline",
        ),
        profile: env("BITTY_PERF_PROFILE", "bench (release)"),
    }
}
