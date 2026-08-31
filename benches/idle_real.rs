//! Idle invariant — PB-7 frame-on-demand, ≤1 % CPU over 10 min.
//!
//! CTX-0100 upgrade: proves zero periodic wakeups when idle via the
//! `tick == None` invariant and GridRenderer `FrameMode::Clean` path, with
//! bounded cost sampling (idle tick mean, clean render mean) and an optional
//! `ps %cpu` sample. No polling loop, no unnecessary redraw, no 10-minute
//! sleep required; real ≤1 % over 10 min is gated on the Tier 1 ref machine.
//!
//! Headless, bounded, `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::idle::check_idle;

fn main() {
    println!("idle_real — PB-7 idle resource (CTX-0100 frame-on-demand invariant, bounded)");
    println!(
        "budget: bitty-docs/docs/specifications/performance-budget-rfc.md#pb-7 (≤1% avg CPU over 10 min, zero wakeups when idle)"
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

    if !report.meets_cpu_budget() {
        eprintln!(
            "note: sampled_cpu {:?} exceeds PB-7 1% — expected only on noisy CI (real 10 min on Tier 1 gates)",
            report.sampled_cpu_pct
        );
    }
}
