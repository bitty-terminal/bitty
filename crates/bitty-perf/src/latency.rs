//! Input latency path measurement — PB-4 key-to-screen ≤8 ms p50 / ≤15 ms p99.
//!
//! Covers the full `keydown → PTY → parser → state → render → present`
//! pipeline per task scope, with bounded tracing (≤64 B per key, ≤8 KiB per
//! batch, ≤256 damage regions). Each sample timestamps the hot-path stages
//! via `Instant`; statistics are p50/p99/mean/max, not single-point.
//!
//! The tracer never touches Lua, plugins, or the cold queue beyond pushing
//! bounded observations — proving the plugin pipeline stays off the hot path
//! (core-boundaries.md). On headless CI the present seam is
//! `Surface::headless_present` (deterministic, no display server); on a real
//! Wayland box the same tracer can be driven with a frame-presented timestamp
//! (future slice) without changing this API.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::Runtime;

// ---------------------------------------------------------------------------
// Bounded helpers
// ---------------------------------------------------------------------------

/// Maximum key bytes per event (matches `Runtime::encode_key_event` bound).
const MAX_KEY_BYTES: usize = 64;

/// Maximum samples per report (bounded, avoids unbounded Vec growth).
const MAX_SAMPLES: usize = 10_000;

/// Sample count for the shared-runner headless budget checks (the unit test
/// and the `benches/latency_real.rs` sanity).
///
/// At n=50 `percentile(99)` returns `round(0.99 * 49) = 49`, i.e. the single
/// maximum, so the "p99" assertion was really a max assertion and one
/// scheduler-stalled sample (204.432 ms on Windows CI, issue #659) decided the
/// budget. At n=200 the estimator is a true p99 (`round(0.99 * 199) = 197`
/// excludes the two worst presented samples), matching the PTY-echo tracer
/// (CTX-0342) and `benches/latency_real.rs`.
pub const HEADLESS_BUDGET_SAMPLES: usize = 200;

/// Wall-clock ceiling (ms) for the shared-runner headless budget checks.
///
/// Deliberately loose: the real PB-4 budget (p50 8 ms / p99 15 ms) is gated by
/// `benches/latency_real.rs` and Tier 1 evidence, never by this bound. It only
/// proves the tracer is not pathologically slow under a loaded shared runner,
/// where the test thread can be descheduled for a whole quantum.
pub const HEADLESS_WALL_CLOCK_CEILING_MS: f64 = 120.0;

/// Shared-runner allowance applied to the PB-4 p50 budget for the tight
/// headless *work-floor* ceiling (the sum of measured stage durations).
///
/// The exact PB-4 budgets are gated by `benches/latency_real.rs` and Tier 1
/// evidence. This factor only covers cache/CPU contention on a shared runner;
/// a real regression of the pipeline itself still trips the ceiling. The tight
/// gate applies it to the *fastest* presented sample's work (`min_work_ms`),
/// because scheduler preemption only ever adds time: the floor is the
/// noise-robust estimator of the pipeline's own cost, while the median and
/// percentiles of a contended runner measure the runner (CTX-0494).
pub const HEADLESS_SHARED_RUNNER_FACTOR: f64 = 4.0;

/// Shared-runner allowance applied to the PB-4 p99 budget for the generous
/// headless *work-tail* ceiling.
///
/// Percentile work on a shared runner is dominated by in-stage preemption:
/// CTX-0494 observed work p50 33.4 ms on Linux Wayland (#794) and work p99
/// 62.103 ms on Windows (PR #805 evidence) while the uncontended CI floor is
/// ~16 ms (benches, runs 35049612299 / 35056876992). The tail ceiling is
/// therefore only a pathology guard, not the PB-4 budget: a real pipeline
/// regression is caught by the work floor above and by the deterministic
/// `pb4_work_budget_classification_is_exact` verdicts.
pub const HEADLESS_SHARED_RUNNER_TAIL_FACTOR: f64 = 8.0;

/// Tight headless work-floor ceiling (ms): PB-4 p50 ×
/// [`HEADLESS_SHARED_RUNNER_FACTOR`] (4 × 8 ms = 32 ms). Applied to
/// [`LatencyReport::min_work_ms`].
pub const HEADLESS_WORK_FLOOR_CEILING_MS: f64 =
    super::PB4_LATENCY_MS_P50 as f64 * HEADLESS_SHARED_RUNNER_FACTOR;

/// Generous headless work-tail ceiling (ms): PB-4 p99 ×
/// [`HEADLESS_SHARED_RUNNER_TAIL_FACTOR`] (8 × 15 ms = 120 ms). Applied to the
/// work percentiles as a pathology guard only.
pub const HEADLESS_WORK_TAIL_CEILING_MS: f64 =
    super::PB4_LATENCY_MS_P99 as f64 * HEADLESS_SHARED_RUNNER_TAIL_FACTOR;

/// Creates a deterministic `KeyEvent` for a printable character `c`.
///
/// Pure, headless, bounded — no window required.
fn char_key_event(c: char, text: &str) -> KeyEvent {
    let logical = if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() {
        LogicalKey::Character(c.to_string())
    } else {
        LogicalKey::Named(NamedKey::Other)
    };
    KeyEvent {
        logical_key: logical,
        text: Some(text.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn named_key_event(key: NamedKey) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(key),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

// ---------------------------------------------------------------------------
// Sample
// ---------------------------------------------------------------------------

/// Which measurement path produced a [`LatencyReport`].
///
/// Reported explicitly so a silent fallback to the synthetic echo model is
/// never read as real-PTY evidence (CTX-0484).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyMode {
    /// Key bytes injected straight into `handle_pty_bytes` (no child process).
    InjectedEcho,
    /// A real `cat` child echoed the bytes through the PTY (`poll_pty`).
    RealPtyEcho,
}

impl LatencyMode {
    /// Stable lower-case label used in summaries and evidence.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InjectedEcho => "injected-echo",
            Self::RealPtyEcho => "real-pty-echo",
        }
    }
}

/// One key-to-screen latency sample with stage breakdown (all bounded tracing).
#[derive(Debug, Clone, Copy)]
pub struct LatencySample {
    /// Total keydown → present (wall clock for this sample).
    pub total: Duration,
    /// `keydown` → `encode_key_event` duration.
    pub encode: Duration,
    /// `encode` → `Runtime::handle_key_event` (PTY push) duration.
    pub handle_key: Duration,
    /// `handle_key` → `handle_pty_bytes` (PTY → parser → state) duration.
    pub pty_to_state: Duration,
    /// `state` → `tick` → `present` (render + composite) duration.
    pub render_present: Duration,
    /// Whether the sample presented a frame (vs idle no-damage).
    pub presented: bool,
    /// Whether this sample used synthetic echo injection rather than real PTY output.
    pub is_synthetic: bool,
}

impl LatencySample {
    /// Total in microseconds.
    #[must_use]
    pub fn total_us(&self) -> f64 {
        self.total.as_secs_f64() * 1_000_000.0
    }
    /// Total in milliseconds.
    #[must_use]
    pub fn total_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0
    }

    /// Sum of the measured stage durations (pipeline work).
    ///
    /// Unlike [`total`](Self::total), this excludes the scheduler gaps between
    /// stages, so it is the cost the pipeline itself controls (CTX-0484).
    #[must_use]
    pub fn work(&self) -> Duration {
        self.encode + self.handle_key + self.pty_to_state + self.render_present
    }

    /// Pipeline work in milliseconds.
    #[must_use]
    pub fn work_ms(&self) -> f64 {
        self.work().as_secs_f64() * 1000.0
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Aggregated latency report for PB-4.
#[derive(Debug, Clone)]
pub struct LatencyReport {
    /// All samples (bounded ≤ `MAX_SAMPLES`).
    pub samples: Vec<LatencySample>,
    /// p50 (median) total ms.
    pub p50_ms: f64,
    /// p99 total ms.
    pub p99_ms: f64,
    /// Mean total ms.
    pub mean_ms: f64,
    /// Max total ms.
    pub max_ms: f64,
    /// p50 of measured pipeline work (stage-sum) ms.
    pub p50_work_ms: f64,
    /// p99 of measured pipeline work (stage-sum) ms.
    pub p99_work_ms: f64,
    /// Fastest presented sample's pipeline work (stage-sum) ms.
    pub min_work_ms: f64,
    /// Which measurement path produced the samples (never inferred).
    pub mode: LatencyMode,
    /// Whether headless software seam was used (no real compositor).
    pub headless: bool,
    /// Number of samples that failed to present (should be 0 for this tracer).
    pub idle_misses: usize,
    /// Number of samples where synthetic echo bytes were injected.
    pub synthetic_samples: usize,
}

fn percentile(sorted_ms: &[f64], pct: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let rank = (pct / 100.0 * (sorted_ms.len() as f64 - 1.0)).round() as usize;
    sorted_ms[rank.min(sorted_ms.len() - 1)]
}

fn mean(sorted_ms: &[f64]) -> f64 {
    if sorted_ms.is_empty() {
        0.0
    } else {
        sorted_ms.iter().sum::<f64>() / sorted_ms.len() as f64
    }
}

/// Presented wall-clock totals and pipeline work, sorted ascending.
fn presented_series(samples: &[LatencySample]) -> (Vec<f64>, Vec<f64>) {
    let mut totals = Vec::with_capacity(samples.len());
    let mut work = Vec::with_capacity(samples.len());
    for s in samples.iter().filter(|s| s.presented) {
        totals.push(s.total_ms());
        work.push(s.work_ms());
    }
    let cmp = |a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
    totals.sort_by(cmp);
    work.sort_by(cmp);
    (totals, work)
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Scheduler-noise injection site for the CTX-0494 shared-runner probe.
///
/// [`measure_latency_with_hook`] calls the hook at these points so a test can
/// simulate, deterministically, what a loaded shared runner does to the tracer:
///
/// * [`NoiseSite::BetweenStages`] — between `keydown` and the first stage
///   timer. A deschedule here inflates wall clock but no measured stage, so it
///   must not move the work floor.
/// * [`NoiseSite::InsideRender`] — after the render/present timer starts and
///   before `Runtime::tick`. A deschedule here inflates wall clock *and*
///   measured work, which is exactly why percentile work is not a robust gate
///   while the work floor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoiseSite {
    BetweenStages,
    InsideRender,
}

/// Measures key-to-screen latency over `iterations` synthetic key events.
///
/// Each iteration:
/// 1. `Instant::now` at simulated `keydown`.
/// 2. `encode_key_event` → bounded bytes (≤64 B).
/// 3. `Runtime::handle_key_event` (push to PTY / pending_input).
/// 4. `Runtime::handle_pty_bytes` with the same bytes (echo model — deterministic
///    without a live child; real PTY echo is `cat` bounded via `poll_pty`).
/// 5. `Runtime::tick` → `Surface::headless_present` (or real GPU when attached).
///
/// The tracer is bounded: each iteration touches ≤64 B, ≤256 damage regions,
/// and the whole run touches ≤`MAX_SAMPLES` samples. No `unsafe`, no window.
#[must_use]
pub fn measure_latency(iterations: usize) -> LatencyReport {
    measure_latency_with_hook(iterations, |_, _| {})
}

/// [`measure_latency`] with a scheduler-noise injection hook for tests.
///
/// Production callers pass a no-op closure, which monomorphizes away; the
/// CTX-0494 probe passes a closure that sleeps at chosen [`NoiseSite`]s to
/// reproduce shared-runner descheduling deterministically. The hook receives
/// the 0-based iteration index as its first argument so a probe can interleave
/// independently measured legs in one run (CTX-0500) instead of comparing legs
/// measured at different times, when shared-runner load is not comparable.
fn measure_latency_with_hook(
    iterations: usize,
    mut noise: impl FnMut(usize, NoiseSite),
) -> LatencyReport {
    let iterations = iterations.clamp(1, MAX_SAMPLES);
    let mut rt = Runtime::with_defaults().expect("headless runtime must build for latency tracer");
    // Prime: first tick must present full redraw so idle baseline is clean.
    let _ = rt.tick();
    // Warmup: run a few untimed iterations to settle caches/allocators and
    // reduce the first-sample outlier that caused p99 flakiness on CI
    // (observed 16.6 ms across 5/5 legs, 52 ms on macOS ARM64).
    for _ in 0..3 {
        let key = char_key_event('w', "w");
        let bytes = Runtime::encode_key_event(&key).unwrap_or_else(|| vec![b'a']);
        let encoded = rt.handle_key_event(key);
        let effective = encoded.unwrap_or(bytes);
        rt.handle_pty_bytes(&effective);
        let _ = rt.drain_cold_events();
        let _ = rt.tick();
        let _ = rt.tick();
    }

    let mut samples = Vec::with_capacity(iterations);
    let mut idle_misses = 0usize;

    // Deterministic key sequence: printable + control mix, bounded.
    let keys: Vec<KeyEvent> = vec![
        char_key_event('a', "a"),
        char_key_event('b', "b"),
        char_key_event('c', "c"),
        named_key_event(NamedKey::Enter),
        char_key_event('x', "x"),
        named_key_event(NamedKey::Backspace),
        char_key_event('1', "1"),
        named_key_event(NamedKey::ArrowRight),
    ];

    for i in 0..iterations {
        let key = keys[i % keys.len()].clone();
        let t0 = Instant::now();
        // CTX-0494 probe site: a deschedule here is a pure scheduler gap.
        noise(i, NoiseSite::BetweenStages);

        // Stage 1: encode (keydown → bytes).
        let t_encode = Instant::now();
        let bytes = Runtime::encode_key_event(&key).unwrap_or_else(|| vec![b'a']);
        assert!(bytes.len() <= MAX_KEY_BYTES, "key bytes bound");
        let encode_dur = t_encode.elapsed();

        // Stage 2: handle_key (bytes → PTY pending_input).
        let t_handle = Instant::now();
        let encoded = rt.handle_key_event(key);
        let handle_dur = t_handle.elapsed();
        let effective_bytes = encoded.unwrap_or(bytes);

        // Stage 3: PTY → parser → state (echo model: feed same bytes via handle_pty_bytes).
        // In a real PTY run this would be `poll_pty` echo; headless we inject directly.
        let t_pty = Instant::now();
        rt.handle_pty_bytes(&effective_bytes);
        // Also drain cold queue boundedly (proves hot path stays off plugin queue).
        let _ = rt.drain_cold_events();
        let pty_dur = t_pty.elapsed();

        // Stage 4: state → render → present (tick).
        let t_render = Instant::now();
        // CTX-0494 probe site: a deschedule here is charged to measured work.
        noise(i, NoiseSite::InsideRender);
        let presented = rt.tick().is_some();
        let render_dur = t_render.elapsed();
        if !presented {
            idle_misses += 1;
        }

        let total = t0.elapsed();
        samples.push(LatencySample {
            total,
            encode: encode_dur,
            handle_key: handle_dur,
            pty_to_state: pty_dur,
            render_present: render_dur,
            presented,
            is_synthetic: true,
        });

        // Keep frame-on-demand invariant: after each present, next tick without
        // new bytes must be idle (checked lazily each iteration; not a hard fail).
        if presented {
            debug_assert!(
                rt.tick().is_none(),
                "frame-on-demand: post-present tick must be idle"
            );
        }
    }

    // Percentiles over presented samples only (idle no-damage ticks are not latency).
    let (presented_ms, presented_work) = presented_series(&samples);
    let headless = samples.first().is_some_and(|_| {
        // Runtime is headless by construction in this tracer (is_headless true)
        // unless a real GPU was attached externally.
        rt.is_headless()
    });
    let synthetic_samples = samples.len();

    LatencyReport {
        samples,
        p50_ms: percentile(&presented_ms, 50.0),
        p99_ms: percentile(&presented_ms, 99.0),
        mean_ms: mean(&presented_ms),
        max_ms: presented_ms.last().copied().unwrap_or(0.0),
        p50_work_ms: percentile(&presented_work, 50.0),
        p99_work_ms: percentile(&presented_work, 99.0),
        min_work_ms: presented_work.first().copied().unwrap_or(0.0),
        mode: LatencyMode::InjectedEcho,
        headless,
        idle_misses,
        synthetic_samples,
    }
}

/// Measures synthetic PTY-echo latency (real `cat` child when PTY is available).
///
/// On headless CI without a live PTY child this falls back to the echo model
/// (same as [`measure_latency`]); on a Tier 1 box where `Runtime::spawn_shell`
/// succeeds, the tracer drives a real `cat` and `poll_pty` to measure
/// `keydown → PTY write → shell echo → parser → state → render → present`
/// with a bounded 8 KiB read window.
#[must_use]
pub fn measure_latency_with_pty_echo(iterations: usize) -> LatencyReport {
    // Cheap headless check: if we can spawn `cat`, use real PTY path; otherwise echo model.
    let mut rt = Runtime::with_defaults().expect("runtime for latency pty echo");
    let _ = rt.tick();
    let can_pty = rt.spawn_shell("cat").is_ok();
    if !can_pty {
        return measure_latency(iterations);
    }

    // Warmup: mirror `measure_latency` and run a few untimed iterations so
    // allocator/cache/PTY-forwarder setup and the first `cat` echo are not
    // charged to the first measured sample. The real-PTY branch was the only
    // measured path without warmup, so its first sample was consistently the
    // max (3–6 ms locally, 151.8 ms under CI parallelism — CTX-0342), while
    // the warmed headless path stayed green in the same run. This removes the
    // deterministic cold-start bias without relaxing any bound.
    for _ in 0..3 {
        let key = char_key_event('w', "w");
        let bytes = Runtime::encode_key_event(&key).unwrap_or_else(|| vec![b'a']);
        let encoded = rt.handle_key_event(key);
        let effective = encoded.unwrap_or(bytes);
        let _ = rt.write_replies();
        let _ = rt.poll_pty();
        rt.handle_pty_bytes(&effective);
        let _ = rt.drain_cold_events();
        let _ = rt.tick();
        let _ = rt.tick();
    }

    let iterations = iterations.clamp(1, MAX_SAMPLES);
    let keys: Vec<KeyEvent> = vec![
        char_key_event('a', "a"),
        char_key_event('b', "b"),
        char_key_event('c', "c"),
    ];

    let mut samples = Vec::with_capacity(iterations);
    let mut idle_misses = 0usize;
    let mut synthetic_samples = 0usize;

    for i in 0..iterations {
        let key = keys[i % keys.len()].clone();
        let t0 = Instant::now();

        let t_encode = Instant::now();
        let bytes = Runtime::encode_key_event(&key).unwrap_or_else(|| vec![b'a']);
        let encode_dur = t_encode.elapsed();

        let t_handle = Instant::now();
        let encoded = rt.handle_key_event(key);
        let handle_dur = t_handle.elapsed();
        let _ = encoded.unwrap_or(bytes);

        // For real PTY we need to write_replies and poll_pty boundedly.
        let t_pty = Instant::now();
        let _ = rt.write_replies();
        // Bounded drain: poll_pty returns at most 128 KiB (CHANNEL_CAPACITY*READ_CHUNK)
        let _drained = rt.poll_pty();
        // Fallback echo if poll returned 0 (child hasn't echoed yet — inject bounded synthetic)
        let is_synthetic = _drained == 0;
        if is_synthetic {
            synthetic_samples += 1;
            rt.handle_pty_bytes(b"a");
        }
        let pty_dur = t_pty.elapsed();

        let t_render = Instant::now();
        let presented = rt.tick().is_some();
        let render_dur = t_render.elapsed();
        if !presented {
            idle_misses += 1;
        }
        let total = t0.elapsed();
        samples.push(LatencySample {
            total,
            encode: encode_dur,
            handle_key: handle_dur,
            pty_to_state: pty_dur,
            render_present: render_dur,
            presented,
            is_synthetic,
        });
        if presented {
            let _ = rt.tick();
        }
    }

    let (presented_ms, presented_work) = presented_series(&samples);

    let mode = if synthetic_samples == samples.len() {
        LatencyMode::InjectedEcho
    } else {
        LatencyMode::RealPtyEcho
    };

    LatencyReport {
        samples,
        p50_ms: percentile(&presented_ms, 50.0),
        p99_ms: percentile(&presented_ms, 99.0),
        mean_ms: mean(&presented_ms),
        max_ms: presented_ms.last().copied().unwrap_or(0.0),
        p50_work_ms: percentile(&presented_work, 50.0),
        p99_work_ms: percentile(&presented_work, 99.0),
        min_work_ms: presented_work.first().copied().unwrap_or(0.0),
        mode,
        headless: rt.is_headless(),
        idle_misses,
        synthetic_samples,
    }
}

impl LatencyReport {
    /// Returns `true` when PB-4 p50 (8 ms) is met on the wall clock.
    #[must_use]
    pub fn meets_p50(&self) -> bool {
        self.p50_ms <= super::PB4_LATENCY_MS_P50 as f64
    }
    /// Returns `true` when PB-4 p99 (15 ms) is met on the wall clock.
    #[must_use]
    pub fn meets_p99(&self) -> bool {
        self.p99_ms <= super::PB4_LATENCY_MS_P99 as f64
    }
    /// Returns `true` when the measured pipeline work meets PB-4 p50.
    ///
    /// This is the budget verdict that is not diluted by scheduler gaps: a
    /// wall-clock percentile can only be missed because of them, never met
    /// because of them.
    #[must_use]
    pub fn meets_work_p50(&self) -> bool {
        self.p50_work_ms <= super::PB4_LATENCY_MS_P50 as f64
    }
    /// Returns `true` when the measured pipeline work meets PB-4 p99.
    #[must_use]
    pub fn meets_work_p99(&self) -> bool {
        self.p99_work_ms <= super::PB4_LATENCY_MS_P99 as f64
    }

    /// Formats a human-readable summary for bench output and evidence docs.
    ///
    /// Discloses the measurement [`mode`](Self::mode) and the work cost so a
    /// fallback run is never mistaken for real-PTY evidence and the PB-4
    /// verdict can be read off the reported work percentiles.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let verdict = if self.meets_p50() && self.meets_p99() {
            "PASS p50+p99"
        } else if self.meets_p50() {
            "PASS p50 (p99 exceeded)"
        } else {
            "ABOVE_BUDGET"
        };
        let work_verdict = if self.meets_work_p50() && self.meets_work_p99() {
            "work PASS p50+p99"
        } else if self.meets_work_p50() {
            "work PASS p50 (p99 exceeded)"
        } else {
            "work ABOVE_BUDGET"
        };
        let mut out = String::new();
        out.push_str(&format!(
            "latency — mode={} p50 {:.3} ms / p99 {:.3} ms / mean {:.3} ms / max {:.3} ms (budget p50 {} ms p99 {} ms) work p50 {:.3} ms / p99 {:.3} ms / min {:.3} ms headless={} idle_misses={} synthetic_samples={} [{verdict}; {work_verdict}]\n",
            self.mode.label(),
            self.p50_ms,
            self.p99_ms,
            self.mean_ms,
            self.max_ms,
            super::PB4_LATENCY_MS_P50,
            super::PB4_LATENCY_MS_P99,
            self.p50_work_ms,
            self.p99_work_ms,
            self.min_work_ms,
            self.headless,
            self.idle_misses,
            self.synthetic_samples
        ));
        // Stage breakdown for first few samples (bounded tracing evidence).
        for (i, s) in self.samples.iter().take(5).enumerate() {
            out.push_str(&format!(
                "  sample {i}: total {:.3} ms (encode {:.1} µs handle {:.1} µs pty {:.1} µs render {:.1} µs) presented={} synthetic={}\n",
                s.total_ms(),
                s.encode.as_secs_f64() * 1_000_000.0,
                s.handle_key.as_secs_f64() * 1_000_000.0,
                s.pty_to_state.as_secs_f64() * 1_000_000.0,
                s.render_present.as_secs_f64() * 1_000_000.0,
                s.presented,
                s.is_synthetic
            ));
        }
        if self.samples.len() > 5 {
            out.push_str(&format!("  ... {} total samples\n", self.samples.len()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tight headless work-budget gate, robust to shared-runner scheduler noise.
    ///
    /// CTX-0494: the stage-sum percentiles still absorb preemption that lands
    /// *inside* a stage timer, so on a loaded runner they measure the runner
    /// (observed work p50 33.4 ms on Linux Wayland, #794; work p99 62.103 ms on
    /// Windows, PR #805) rather than the pipeline. The floor (`min_work_ms`,
    /// the fastest presented sample) is the noise-robust estimator of the
    /// pipeline's own cost: preemption only ever adds time, while a genuine
    /// regression moves every sample, so the floor crosses the ceiling first.
    /// The exact PB-4 8/15 ms verdicts are owned by
    /// `pb4_work_budget_classification_is_exact` and the bench/Tier 1 gate.
    fn assert_headless_work_budget(report: &LatencyReport) {
        assert!(
            report.min_work_ms < HEADLESS_WORK_FLOOR_CEILING_MS,
            "work floor {:.3} ms must be < {:.0} ms (PB-4 p50 {} ms × {} shared-runner factor)",
            report.min_work_ms,
            HEADLESS_WORK_FLOOR_CEILING_MS,
            crate::PB4_LATENCY_MS_P50,
            HEADLESS_SHARED_RUNNER_FACTOR
        );
        assert!(
            report.p99_work_ms < HEADLESS_WORK_TAIL_CEILING_MS,
            "work p99 {:.3} ms must stay below the {:.0} ms pathology ceiling (PB-4 p99 {} ms × {} tail factor)",
            report.p99_work_ms,
            HEADLESS_WORK_TAIL_CEILING_MS,
            crate::PB4_LATENCY_MS_P99,
            HEADLESS_SHARED_RUNNER_TAIL_FACTOR
        );
    }

    #[test]
    fn latency_tracer_is_bounded_and_meets_budget_headless() {
        let report = measure_latency(HEADLESS_BUDGET_SAMPLES);
        assert_eq!(
            report.samples.len(),
            HEADLESS_BUDGET_SAMPLES,
            "bounded samples"
        );
        // Wall clock on a shared runner includes scheduler gaps between and
        // inside stages, so it is only a liveness/pathology guard. p50 ≤ p99 by
        // construction, so the single p99 bound covers both; the tight budget
        // is asserted on measured work by `assert_headless_work_budget`, and
        // the exact PB-4 8/15 ms verdicts are pinned deterministically by
        // `pb4_work_budget_classification_is_exact` and the bench/Tier 1 gate.
        assert!(
            report.p99_ms < HEADLESS_WALL_CLOCK_CEILING_MS,
            "wall p99 {:.3} ms must be < {:.0} ms headless liveness ceiling (budget gated by work floor + bench)",
            report.p99_ms,
            HEADLESS_WALL_CLOCK_CEILING_MS
        );
        assert_headless_work_budget(&report);
        // Bounded stage tracing: encode is the hot-path stage whose work must
        // stay sub-millisecond; the pathological guard stays.
        for s in &report.samples {
            assert!(s.encode.as_secs_f64() < 1.0, "encode bound");
        }
        // Bounded invariant: at least half the samples must have presented
        // (non-idle), otherwise the tracer is not exercising the hot path.
        let presented = report.samples.iter().filter(|s| s.presented).count();
        assert!(
            presented >= report.samples.len() / 2,
            "presented {presented}/{} must be >= half",
            report.samples.len()
        );
    }

    // CTX-0500 differential probe constants.
    //
    // The CTX-0494 probe compared a noisy leg's work floor against the absolute
    // PB-4 × 4 ceiling (32 ms) and false-red on a loaded shared runner, where a
    // benign leg's *own* floor reached 33.699 ms (PR #821 Quality gates). The
    // fix measures one clean leg and two noisy legs interleaved in the *same*
    // run and applies relative bounds, so shared-runner load cancels from the
    // comparison; the exact PB-4 8/15 ms verdicts stay pinned by
    // `pb4_work_budget_classification_is_exact`.

    /// Samples per interleaved leg of the differential probe.
    ///
    /// The pre-fix probe measured its between-stage leg with only 6 samples, so
    /// one loaded burst decided the floor. Three legs of 32 samples keep the
    /// per-leg floor (the minimum of the leg) comparable across legs, and the
    /// whole probe runs in a few seconds.
    const HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG: usize = 32;

    /// Interleaved legs per probe run: clean (0), benign noise (1), regression
    /// (2). `iteration % HEADLESS_NOISE_PROBE_LEGS` selects the leg.
    const HEADLESS_NOISE_PROBE_LEGS: usize = 3;

    /// Benign leg: between-stage scheduler gaps plus occasional in-stage tails.
    const HEADLESS_NOISE_PROBE_BENIGN_LEG: usize = 1;

    /// Regression leg: uniform in-stage work on every sample.
    const HEADLESS_NOISE_PROBE_REGRESSION_LEG: usize = 2;

    /// Scheduler gap (ms) injected *between* stages of the benign leg.
    ///
    /// Past [`HEADLESS_WALL_CLOCK_CEILING_MS`] (120 ms) so the probe proves a
    /// deschedule between stages inflates wall clock only, never measured work.
    const HEADLESS_NOISE_GAP_MS: u64 = 150;

    /// In-stage tail (ms) injected every [`HEADLESS_NOISE_TAIL_EVERY`] samples
    /// of the benign leg.
    ///
    /// Past the old 4× percentile ceiling (PB-4 p99 × 4 = 60 ms) so the probe
    /// keeps reproducing the CTX-0494 false-red mechanism, while staying below
    /// the tail pathology guard (PB-4 p99 × 8 = 120 ms).
    const HEADLESS_NOISE_TAIL_MS: u64 = 64;

    /// One in-stage tail every N benign samples: 8 of 32, so the p99 estimator
    /// at n=32 (the second-highest sample) samples a tail even if a few benign
    /// samples fail to present.
    const HEADLESS_NOISE_TAIL_EVERY: usize = 4;

    /// Uniform in-stage work (ms) injected on *every* sample of the regression
    /// leg: 5 × PB-4 p50, deliberately well past
    /// [`HEADLESS_NOISE_REGRESSION_DELTA_MS`] so the additive verdict keeps
    /// margin when shared-runner load lands unevenly on the two legs.
    const HEADLESS_NOISE_REGRESSION_MS: u64 = 5 * crate::PB4_LATENCY_MS_P50;

    /// Multiplicative tolerance applied to the same-run clean work floor when
    /// deciding whether a benign leg's floor is explained by scheduler noise.
    ///
    /// Only covers per-leg sampling jitter: shared-runner load cancels because
    /// the legs are interleaved in one run. It must stay well under
    /// `HEADLESS_NOISE_GAP_MS / clean_floor` so a between-stage gap wrongly
    /// charged as work still fails the benign verdict.
    const HEADLESS_NOISE_FLOOR_RATIO: f64 = 3.0;

    /// Additive slack (ms) added on top of the scaled clean floor, covering the
    /// small-floor case where the ratio alone would be sub-millisecond.
    const HEADLESS_NOISE_FLOOR_SLACK_MS: f64 = 4.0;

    /// Minimum additive lift (ms) a uniform in-stage regression must produce on
    /// the work floor: 2 × PB-4 p50.
    ///
    /// Additive, not a ratio: shared-runner load inflates the clean and
    /// regressed floors together, so their difference isolates the pipeline
    /// work the probe injected.
    const HEADLESS_NOISE_REGRESSION_DELTA_MS: f64 = 2.0 * crate::PB4_LATENCY_MS_P50 as f64;

    /// Benign-leg verdict (CTX-0500): the candidate work floor is consistent
    /// with the clean floor measured in the same interleaved run.
    fn work_floor_follows_clean(clean: &LatencyReport, candidate: &LatencyReport) -> bool {
        candidate.min_work_ms
            <= clean.min_work_ms * HEADLESS_NOISE_FLOOR_RATIO + HEADLESS_NOISE_FLOOR_SLACK_MS
    }

    /// Regression verdict (CTX-0500): the candidate work floor exceeds the
    /// clean floor of the same run by at least the documented additive delta.
    fn work_floor_regressed(clean: &LatencyReport, candidate: &LatencyReport) -> bool {
        candidate.min_work_ms >= clean.min_work_ms + HEADLESS_NOISE_REGRESSION_DELTA_MS
    }

    /// Presented per-sample work (ms) of one interleaved leg, in iteration
    /// order, for building a deterministic per-leg report.
    fn leg_work_ms(report: &LatencyReport, leg: usize) -> Vec<f64> {
        report
            .samples
            .iter()
            .enumerate()
            .filter(|(i, s)| s.presented && i % HEADLESS_NOISE_PROBE_LEGS == leg)
            .map(|(_, s)| s.work_ms())
            .collect()
    }

    #[test]
    fn headless_work_budget_discriminates_scheduler_noise_from_work() {
        // CTX-0500 red-before-fix probe. The shipped percentile work ceilings
        // (PB-4 × 4 = 32/60 ms) false-red when a shared runner deschedules the
        // tracer *inside* a stage timer, because that preemption is charged to
        // the stage sum; the CTX-0494 floor (`min_work_ms`) fixed that. But the
        // CTX-0494 probe then compared the floor to the same *absolute* 32 ms
        // ceiling and false-red again when the runner loaded the benign leg's
        // own clean floor to 33.699 ms (PR #821). This probe keeps the floor
        // discriminator but makes its bounds relative to a clean leg measured
        // in the same interleaved run, so runner load cancels.
        use std::thread;

        let old_p99_work_ceiling = crate::PB4_LATENCY_MS_P99 as f64 * HEADLESS_SHARED_RUNNER_FACTOR;

        // ---- Deterministic shape pins (no timing; runner-independent) ----

        // The CI-observed CTX-0494 shape (floor 16 ms, median 33.4 ms,
        // p99 62.103 ms) is a false red for the old percentile gate and is
        // accepted by the fixed floor gate. Pin it through `report_from_work`
        // so the contract cannot depend on runner timing.
        let mut ci_shape = vec![16.0; 100];
        ci_shape.extend(vec![33.4; 97]);
        ci_shape.extend([62.103; 3]);
        let ci_shape = report_from_work(&ci_shape);
        assert!(
            ci_shape.p50_work_ms > HEADLESS_SHARED_RUNNER_FACTOR * crate::PB4_LATENCY_MS_P50 as f64,
            "observed CI median 33.4 ms must trip the old p50 gate"
        );
        assert!(
            ci_shape.p99_work_ms > old_p99_work_ceiling,
            "observed CI p99 62.103 ms must trip the old p99 gate"
        );
        assert_headless_work_budget(&ci_shape);
        // (c) A benign in-stage tail inflates the work percentiles past the old
        // 4× ceiling but must not trip the tail pathology guard.
        assert!(
            ci_shape.p99_work_ms < HEADLESS_WORK_TAIL_CEILING_MS,
            "benign tail p99 {:.3} ms must stay under the {:.0} ms tail guard",
            ci_shape.p99_work_ms,
            HEADLESS_WORK_TAIL_CEILING_MS
        );

        // The PR #821 false-red, deterministic: a loaded runner put a benign
        // leg's own floor at 33.699 ms, past the absolute 32 ms ceiling. The
        // differential verdict accepts a benign floor that tracks the clean
        // floor of the same run...
        let loaded_clean = report_from_work(&[33.699; HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG]);
        let loaded_benign = report_from_work(&[33.699; HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG]);
        assert!(
            work_floor_follows_clean(&loaded_clean, &loaded_benign),
            "a loaded-runner clean floor {:.3} ms must not reject a benign floor {:.3} ms (PR #821 false red)",
            loaded_clean.min_work_ms,
            loaded_benign.min_work_ms
        );
        // ... while a uniform shift of the same shape is still a regression.
        let loaded_regressed = report_from_work(
            &[33.699 + HEADLESS_NOISE_REGRESSION_MS as f64; HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG],
        );
        assert!(
            work_floor_regressed(&loaded_clean, &loaded_regressed),
            "uniform work inflation must register above the clean floor + {:.0} ms delta",
            HEADLESS_NOISE_REGRESSION_DELTA_MS
        );
        // (b) A between-stage gap wrongly charged as work has a floor at least
        // the injected gap high and must fail the benign verdict.
        let clean_shape = report_from_work(&[16.0; HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG]);
        let mis_charged = report_from_work(
            &[16.0 + HEADLESS_NOISE_GAP_MS as f64; HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG],
        );
        assert!(
            !work_floor_follows_clean(&clean_shape, &mis_charged),
            "a between-stage gap charged as work must exceed the benign floor band"
        );

        // ---- Real-timing differential: clean vs benign vs regression ----

        // One interleaved run: every third sample is clean, benign-noised, or
        // uniformly regressed, so all three legs sample the same runner load
        // and the floors are comparable.
        let report = measure_latency_with_hook(
            HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG * HEADLESS_NOISE_PROBE_LEGS,
            |iteration, site| match (iteration % HEADLESS_NOISE_PROBE_LEGS, site) {
                (HEADLESS_NOISE_PROBE_BENIGN_LEG, NoiseSite::BetweenStages) => {
                    thread::sleep(Duration::from_millis(HEADLESS_NOISE_GAP_MS));
                }
                (HEADLESS_NOISE_PROBE_BENIGN_LEG, NoiseSite::InsideRender)
                    if (iteration / HEADLESS_NOISE_PROBE_LEGS) % HEADLESS_NOISE_TAIL_EVERY == 0 =>
                {
                    thread::sleep(Duration::from_millis(HEADLESS_NOISE_TAIL_MS));
                }
                (HEADLESS_NOISE_PROBE_REGRESSION_LEG, NoiseSite::InsideRender) => {
                    thread::sleep(Duration::from_millis(HEADLESS_NOISE_REGRESSION_MS));
                }
                _ => {}
            },
        );
        assert_eq!(
            report.samples.len(),
            HEADLESS_NOISE_PROBE_SAMPLES_PER_LEG * HEADLESS_NOISE_PROBE_LEGS
        );

        let clean = report_from_work(&leg_work_ms(&report, 0));
        let benign = report_from_work(&leg_work_ms(&report, HEADLESS_NOISE_PROBE_BENIGN_LEG));
        let regressed =
            report_from_work(&leg_work_ms(&report, HEADLESS_NOISE_PROBE_REGRESSION_LEG));
        eprintln!(
            "CTX-0500 differential work floors (ms): clean={:.4} benign={:.4} regressed={:.4} \
             (benign bound {:.4}, regression bound {:.4}; ratio {:.1}, slack {:.0}, delta {:.0})",
            clean.min_work_ms,
            benign.min_work_ms,
            regressed.min_work_ms,
            clean.min_work_ms * HEADLESS_NOISE_FLOOR_RATIO + HEADLESS_NOISE_FLOOR_SLACK_MS,
            clean.min_work_ms + HEADLESS_NOISE_REGRESSION_DELTA_MS,
            HEADLESS_NOISE_FLOOR_RATIO,
            HEADLESS_NOISE_FLOOR_SLACK_MS,
            HEADLESS_NOISE_REGRESSION_DELTA_MS
        );
        // The gap is real: every benign sample carries the between-stage sleep
        // in wall clock, so even the leg's minimum total clears the liveness
        // ceiling while its work floor does not move.
        let benign_min_total_ms = report
            .samples
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                s.presented && i % HEADLESS_NOISE_PROBE_LEGS == HEADLESS_NOISE_PROBE_BENIGN_LEG
            })
            .map(|(_, s)| s.total_ms())
            .fold(f64::INFINITY, f64::min);
        assert!(
            benign_min_total_ms > HEADLESS_WALL_CLOCK_CEILING_MS,
            "injected between-stage gaps must inflate wall clock past the liveness ceiling (min {:.3} ms)",
            benign_min_total_ms
        );

        // (b) Between-stage gaps are not work: the benign floor tracks the clean
        // floor measured in the same run (relative; the absolute ceiling is not
        // consulted, so runner load cannot false-red it).
        assert!(
            work_floor_follows_clean(&clean, &benign),
            "between-stage gaps are not work: benign floor {:.3} ms must track clean floor {:.3} ms",
            benign.min_work_ms,
            clean.min_work_ms
        );

        // (c) The benign in-stage tails lift the work percentiles past the old
        // 4× ceiling (the CTX-0494 false red) without moving the floor — the
        // tail is real preemption, not a pipeline regression.
        assert!(
            benign.p99_work_ms > old_p99_work_ceiling,
            "in-stage tail preemption must exceed the old 4× percentile ceiling (got {:.3} ms)",
            benign.p99_work_ms
        );

        // (a) Uniform in-stage work inflation raises every regressed sample,
        // floor included, so the additive verdict must register it.
        assert!(
            work_floor_regressed(&clean, &regressed),
            "uniform work inflation must raise the floor above clean + {:.0} ms (clean {:.3} ms, regressed {:.3} ms)",
            HEADLESS_NOISE_REGRESSION_DELTA_MS,
            clean.min_work_ms,
            regressed.min_work_ms
        );
    }

    /// Builds a deterministic report from per-sample work values (no timing),
    /// so budget-shape contracts can be pinned without runner dependence.
    fn report_from_work(work_ms: &[f64]) -> LatencyReport {
        let samples: Vec<LatencySample> = work_ms
            .iter()
            .copied()
            .map(|work_ms| LatencySample {
                total: Duration::from_secs_f64(work_ms / 1000.0),
                encode: Duration::ZERO,
                handle_key: Duration::ZERO,
                pty_to_state: Duration::ZERO,
                render_present: Duration::from_secs_f64(work_ms / 1000.0),
                presented: true,
                is_synthetic: true,
            })
            .collect();
        let (totals, work) = presented_series(&samples);
        let synthetic_samples = samples.len();
        LatencyReport {
            samples,
            p50_ms: percentile(&totals, 50.0),
            p99_ms: percentile(&totals, 99.0),
            mean_ms: mean(&totals),
            max_ms: totals.last().copied().unwrap_or(0.0),
            p50_work_ms: percentile(&work, 50.0),
            p99_work_ms: percentile(&work, 99.0),
            min_work_ms: work.first().copied().unwrap_or(0.0),
            mode: LatencyMode::InjectedEcho,
            headless: true,
            idle_misses: 0,
            synthetic_samples,
        }
    }

    #[test]
    fn pb4_work_budget_classification_is_exact() {
        // CTX-0484: the real PB-4 ceilings (p50 8 ms / p99 15 ms) are
        // classified on measured work, deterministically and independently of
        // the runner. A synthetic distribution above the budget must be
        // reported as above budget, not masked by loose wall-clock ceilings.
        let within = report_from_work(&[1.0; 200]);
        assert!(within.meets_work_p50(), "1 ms work meets the 8 ms p50");
        assert!(within.meets_work_p99(), "1 ms work meets the 15 ms p99");

        let above_p50 = report_from_work(&[9.0; 200]);
        assert!(
            !above_p50.meets_work_p50(),
            "9 ms work must fail the 8 ms p50"
        );
        assert!(
            above_p50.meets_work_p99(),
            "9 ms work is still inside the 15 ms p99"
        );

        // A stalled tail misses p99 while the median stays inside p50: the two
        // verdicts are independent and each compares its own percentile.
        let mut skewed = vec![1.0; 197];
        skewed.extend([100.0, 100.0, 100.0]);
        let skewed = report_from_work(&skewed);
        assert!(
            skewed.meets_work_p50(),
            "median work {:.3} ms is inside the 8 ms p50",
            skewed.p50_work_ms
        );
        assert!(
            !skewed.meets_work_p99(),
            "p99 work {:.3} ms must fail the 15 ms p99",
            skewed.p99_work_ms
        );
    }

    #[test]
    fn headless_p99_tolerates_one_scheduler_stall_but_not_a_regression() {
        // #659: a shared Windows runner descheduled the tracer for 204.432 ms
        // in a single sample. A p99 budget must be decided by the distribution,
        // not by one stalled sample, so pin the exact estimator contract.
        let ceiling = HEADLESS_WALL_CLOCK_CEILING_MS;
        let sorted = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v
        };

        // One descheduled sample among clean ones: within the 1% allowance.
        let mut one_stall = vec![0.6; HEADLESS_BUDGET_SAMPLES - 1];
        one_stall.push(204.432);
        assert!(
            percentile(&sorted(one_stall), 99.0) < ceiling,
            "one 204.432 ms scheduler stall must not fail the p99 budget"
        );

        // Two stalled samples are exactly the 1% allowance at n=200.
        let mut two_stalls = vec![0.6; HEADLESS_BUDGET_SAMPLES - 2];
        two_stalls.extend([204.432, 205.0]);
        assert!(
            percentile(&sorted(two_stalls), 99.0) < ceiling,
            "two stalls are within the documented 1% p99 allowance"
        );

        // Three stalled samples are a distribution regression (1.5%) and must
        // fail: the bound still catches real regressions, not only hangs.
        let mut three_stalls = vec![0.6; HEADLESS_BUDGET_SAMPLES - 3];
        three_stalls.extend([204.432, 205.0, 206.0]);
        assert!(
            percentile(&sorted(three_stalls), 99.0) >= ceiling,
            "three stalls (1.5%) must fail the p99 budget"
        );
    }

    #[test]
    fn latency_with_pty_echo_falls_back_when_no_pty() {
        // 5-sample median was brittle on Windows (run 33391058194 fell at
        // 39.276 ms p50 with 15.6 ms timer granularity + CI parallelism,
        // while the 20-sample tracer stayed <30). Use 50 samples for a stable
        // median and gate loosely; real PB-4 p50 8 ms / p99 15 ms is
        // bench-gated (benches/latency_real.rs) and Tier 1 evidence, not this
        // unit test.
        //
        // CTX-0342: use 200 samples (the same count as benches/latency_real.rs
        // uses for this tracer), not 50. At n=50 `percentile(99)` returns rank
        // `round(0.99*49)=49`, i.e. the single worst sample, so the "p99"
        // assertion was really a max assertion and one CI scheduler stall
        // (151.8 ms on run 34629980293) failed the gate while the rerun passed.
        // At n=200 p99 excludes the two worst samples, which is what a p99
        // statistic means; the 150 ms ceiling is unchanged, so the meaningful
        // guard (real budget 8/15 ms, bench-gated) is not weakened. The
        // real-PTY path is also warmed by `measure_latency_with_pty_echo`
        // itself, removing the cold first-sample bias that made the max
        // systematic rather than a random stall.
        let report = measure_latency_with_pty_echo(200);
        assert!(!report.samples.is_empty());
        let p50_limit = if std::env::var("CI").is_ok() {
            80.0
        } else {
            50.0
        };
        let p99_limit = 150.0;
        assert!(
            report.p50_ms < p50_limit,
            "fallback p50 {:.3} ms must be < {:.0} ms (relaxed for Windows timer/parallelism; budget 8 ms gated by bench)",
            report.p50_ms,
            p50_limit
        );
        assert!(
            report.p99_ms < p99_limit,
            "fallback p99 {:.3} ms must be < {:.0} ms (relaxed for CI parallelism)",
            report.p99_ms,
            p99_limit
        );
        // CTX-0543: verify synthetic sample tracking and honest mode labeling
        assert_eq!(
            report.synthetic_samples,
            report.samples.iter().filter(|s| s.is_synthetic).count(),
            "synthetic_samples must match the count of samples marked is_synthetic"
        );
        if report.synthetic_samples == report.samples.len() {
            assert_eq!(
                report.mode,
                LatencyMode::InjectedEcho,
                "when all samples are synthetic, mode must be InjectedEcho"
            );
        } else {
            assert_eq!(
                report.mode,
                LatencyMode::RealPtyEcho,
                "when real PTY echo is received, mode must be RealPtyEcho"
            );
        }
    }

    #[test]
    fn probe_report_discloses_measurement_mode_and_work_cost() {
        // CTX-0484 probe: a report must disclose which path produced it
        // (injected echo vs a real `cat` PTY) and how much pipeline work it
        // measured, so a silent fallback is never read as real-PTY evidence.
        // Fails before the fix because `format_summary` discloses neither.
        let report = measure_latency(HEADLESS_BUDGET_SAMPLES);
        let mut work: Vec<f64> = report
            .samples
            .iter()
            .filter(|s| s.presented)
            .map(LatencySample::work_ms)
            .collect();
        work.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        eprintln!(
            "CTX-0484 probe work_ms min={:.4} p50={:.4} p99={:.4} max={:.4} n={}",
            work.first().copied().unwrap_or(0.0),
            report.p50_work_ms,
            report.p99_work_ms,
            work.last().copied().unwrap_or(0.0),
            work.len()
        );
        let summary = report.format_summary();
        assert!(
            summary.contains("mode="),
            "summary must disclose the measurement mode: {summary}"
        );
        assert!(
            summary.contains("work p50"),
            "summary must disclose measured pipeline work: {summary}"
        );
        assert!(
            summary.contains("synthetic_samples="),
            "summary must disclose synthetic_samples: {summary}"
        );
        assert_eq!(report.mode, LatencyMode::InjectedEcho);
        assert_eq!(report.synthetic_samples, report.samples.len());
        assert!(report.samples.iter().all(|s| s.is_synthetic));
        assert_eq!(report.min_work_ms, work.first().copied().unwrap_or(0.0));

        // The real-PTY variant must never be mislabeled: whatever path actually
        // ran, the report names it and the summary agrees.
        let pty = measure_latency_with_pty_echo(HEADLESS_BUDGET_SAMPLES);
        let pty_summary = pty.format_summary();
        assert!(
            pty_summary.contains(&format!("mode={}", pty.mode.label())),
            "PTY-echo summary must match its mode: {pty_summary}"
        );
        if pty.mode == LatencyMode::InjectedEcho {
            assert_eq!(
                pty.mode.label(),
                "injected-echo",
                "fallback must be labeled as injected, not as real PTY echo"
            );
            assert_eq!(
                pty.synthetic_samples,
                pty.samples.len(),
                "all-synthetic fallback must report synthetic_samples == iterations"
            );
        } else {
            assert_eq!(pty.mode, LatencyMode::RealPtyEcho);
            assert!(
                pty.synthetic_samples < pty.samples.len(),
                "real PTY echo report must not have 100% synthetic samples"
            );
        }
    }

    #[test]
    fn synthetic_latency_labeling_and_accounting_contract() {
        // CTX-0543: verify synthetic sample tracking and honest mode labeling.
        // 1. measure_latency always marks all samples synthetic and sets InjectedEcho.
        let report = measure_latency(50);
        assert_eq!(report.samples.len(), 50);
        assert_eq!(report.synthetic_samples, 50);
        assert!(report.samples.iter().all(|s| s.is_synthetic));
        assert_eq!(report.mode, LatencyMode::InjectedEcho);
        assert!(report.meets_p50());
        assert!(report.meets_p99());
        assert!(report.meets_work_p50());
        assert!(report.meets_work_p99());

        let summary = report.format_summary();
        assert!(
            summary.contains("synthetic_samples=50"),
            "summary must disclose synthetic_samples: {summary}"
        );

        // 2. measure_latency_with_pty_echo must accurately track synthetic samples.
        let pty_report = measure_latency_with_pty_echo(50);
        assert_eq!(pty_report.samples.len(), 50);
        let counted_synthetic = pty_report.samples.iter().filter(|s| s.is_synthetic).count();
        assert_eq!(
            pty_report.synthetic_samples, counted_synthetic,
            "report.synthetic_samples must equal the number of is_synthetic samples"
        );

        if pty_report.synthetic_samples == pty_report.samples.len() {
            assert_eq!(
                pty_report.mode,
                LatencyMode::InjectedEcho,
                "report mode must be InjectedEcho when all samples are synthetic"
            );
            assert_ne!(
                pty_report.mode,
                LatencyMode::RealPtyEcho,
                "synthetic fallback must NOT be labeled RealPtyEcho"
            );
        } else {
            assert_eq!(
                pty_report.mode,
                LatencyMode::RealPtyEcho,
                "report mode must be RealPtyEcho when real PTY echo arrived"
            );
            assert!(
                pty_report.synthetic_samples < pty_report.samples.len(),
                "RealPtyEcho must have at least one non-synthetic sample"
            );
        }

        // 3. Verify deterministic mode switching based on synthetic count.
        let make_report = |synthetic_count: usize, total: usize| -> LatencyReport {
            let samples: Vec<LatencySample> = (0..total)
                .map(|i| LatencySample {
                    total: Duration::from_millis(2),
                    encode: Duration::from_micros(100),
                    handle_key: Duration::from_micros(100),
                    pty_to_state: Duration::from_micros(500),
                    render_present: Duration::from_millis(1),
                    presented: true,
                    is_synthetic: i < synthetic_count,
                })
                .collect();
            let mode = if synthetic_count == total {
                LatencyMode::InjectedEcho
            } else {
                LatencyMode::RealPtyEcho
            };
            LatencyReport {
                samples,
                p50_ms: 2.0,
                p99_ms: 2.0,
                mean_ms: 2.0,
                max_ms: 2.0,
                p50_work_ms: 1.7,
                p99_work_ms: 1.7,
                min_work_ms: 1.7,
                mode,
                headless: true,
                idle_misses: 0,
                synthetic_samples: synthetic_count,
            }
        };

        // All synthetic -> InjectedEcho
        let all_synthetic = make_report(50, 50);
        assert_eq!(all_synthetic.mode, LatencyMode::InjectedEcho);
        assert_eq!(all_synthetic.synthetic_samples, 50);
        assert_ne!(all_synthetic.mode, LatencyMode::RealPtyEcho);

        // Partially synthetic -> RealPtyEcho with accurate synthetic_samples count
        let partial_synthetic = make_report(10, 50);
        assert_eq!(partial_synthetic.mode, LatencyMode::RealPtyEcho);
        assert_eq!(partial_synthetic.synthetic_samples, 10);

        // Zero synthetic -> RealPtyEcho with 0 synthetic_samples
        let zero_synthetic = make_report(0, 50);
        assert_eq!(zero_synthetic.mode, LatencyMode::RealPtyEcho);
        assert_eq!(zero_synthetic.synthetic_samples, 0);

        // Existing latency calculations must continue to work without regression
        assert!(all_synthetic.meets_p50());
        assert!(all_synthetic.meets_p99());
        assert!(all_synthetic.meets_work_p50());
        assert!(all_synthetic.meets_work_p99());
        assert!(partial_synthetic.meets_p50());
        assert!(zero_synthetic.meets_p50());
    }
}
