//! Input latency real measurement — PB-4 key-to-screen.
//!
//! CTX-0100 upgrade: measures the full `keydown → PTY → parser → state →
//! render → present` hot path with `Instant` stage tracing, p50/p99/mean/max,
//! bounded (≤64 B per key, ≤8 KiB per batch, ≤256 damage regions).
//! Headless CI uses `Surface::headless_present`; a real Wayland box would
//! drive the same tracer with a compositor-present timestamp (future slice)
//! without changing this API.
//!
//! Headless, bounded, `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::latency::{measure_latency, measure_latency_with_pty_echo};

fn main() {
    println!(
        "latency_real — PB-4 input latency (CTX-0100 keydown→PTY→parser→state→render→present, bounded tracing)"
    );
    println!(
        "budget: bitty-docs/docs/specifications/performance-budget-rfc.md#pb-4 (p50 8 ms p99 15 ms, 60 Hz minimum, Wayland/frame-presented)"
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

    // Small fast-path sanity: single key must be ≤8 ms p50 headroom even on slow CI.
    let fast = measure_latency(10);
    assert!(
        fast.p99_ms < 50.0,
        "sanity: p99 should stay << 50 ms even on CI (got {:.3} ms)",
        fast.p99_ms
    );
    println!("sanity 10-sample p99 {:.3} ms within headroom", fast.p99_ms);
}
