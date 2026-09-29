//! `bitty-perf`: bench harness owner (Phase F → CTX-0100 Real Window).
//!
//! This crate owns the workspace-root `benches/` targets so
//! `cargo bench --no-run` can compile them while keeping the workspace
//! virtual. Benches live at `benches/*.rs` per the task scope
//! `benches/{vt_throughput,terminal_state,reflow,search,render_prepare}.rs`
//! and new real-window benches
//! `benches/{startup_real,latency_real,idle_real}.rs` (CTX-0100).
//! All are headless, bounded, `forbid(unsafe)` — see
//! `docs/specifications/performance-budget-rfc.md` PB-1..PB-7.
//!
//! CTX-0100 upgrade: the former `--help` proxy is replaced by
//! instrumentation that covers the full `bitty-terminal` cold path:
//! process start → config → PTY spawn → winit window → wgpu init → font
//! init → first shell bytes → first frame presented. Each phase is
//! timestamped with `Instant` and bounded tracing; on headless CI the
//! display-tied phases report `Unavailable` with their attempt duration,
//! proving the seam without requiring a display. Input latency
//! (`keydown → PTY → parser → state → render → present`) is measured with
//! stage breakdown and p50/p99, and idle is gated by the frame-on-demand
//! invariant (`tick == None` → no polling loop → ≤1 % CPU).
//!
//! Budget reference: `docs/specifications/performance-budget-rfc.md#budgets`.
//! Evidence: `docs/product/perf-evidence.md` (CTX-0100, real measurements from `c0aadd2+`).

#![forbid(unsafe_code)]

pub mod dogfood_session;
pub mod idle;
pub mod latency;
pub mod parser_throughput;
pub mod real_soak;
pub mod real_window;
pub mod startup;
pub mod throughput_floor;
pub mod typical_session;

/// How a `harness = false` bench binary was invoked (CTX-0854).
///
/// `cargo bench` passes `--bench` to every bench binary; `cargo test
/// --benches` (the CI compile-and-run gate) does not. Measurement loops sized
/// for an optimized `bench` build run for minutes in the unoptimized `test`
/// profile without producing a meaningful number, so the smoke invocation
/// scales them down while every code path and invariant check still runs.
/// Budget verdicts come only from [`BenchInvocation::Measure`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchInvocation {
    /// `cargo bench`: full measurement workload.
    Measure,
    /// `cargo test --benches`: reduced workload, same code paths.
    Smoke,
}

/// Divisor applied to measurement workloads in [`BenchInvocation::Smoke`].
pub const SMOKE_WORKLOAD_DIVISOR: usize = 100;

impl BenchInvocation {
    /// Classify a bench binary's arguments (program name excluded).
    #[must_use]
    pub fn from_args<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if args.into_iter().any(|arg| arg.as_ref() == "--bench") {
            Self::Measure
        } else {
            Self::Smoke
        }
    }

    /// Classify the running bench binary from its process arguments.
    #[must_use]
    pub fn current() -> Self {
        Self::from_args(std::env::args().skip(1))
    }

    /// Scale a measurement workload (iterations, bytes) for this invocation,
    /// never below `floor`.
    #[must_use]
    pub fn workload(self, full: usize, floor: usize) -> usize {
        match self {
            Self::Measure => full,
            Self::Smoke => (full / SMOKE_WORKLOAD_DIVISOR).max(floor),
        }
    }
}

/// PB-1 cold startup budget — p50 / p99 (ms).
pub const PB1_STARTUP_MS_P50: u64 = 100;
/// PB-1 p99.
pub const PB1_STARTUP_MS_P99: u64 = 200;

/// PB-2 idle RSS budget — MB RSS p50, one window 60 s idle, bundled plugins only.
pub const PB2_IDLE_RSS_MB: u64 = 80;

/// PB-3 typical-session budget — 8 tabs after 4 h mixed session.
pub const PB3_TYPICAL_RSS_MB: u64 = 250;
/// PB-3 reclaim budget — within 15 % of pre-open baseline after close+GC.
pub const PB3_RECLAIM_PCT: u64 = 15;

/// PB-4 input latency — key-to-screen p50 / p99 (ms).
pub const PB4_LATENCY_MS_P50: u64 = 8;
/// PB-4 p99.
pub const PB4_LATENCY_MS_P99: u64 = 15;

/// PB-5 package size — release binary ≤ 25 MB, dist ≤ 40 MB.
pub const PB5_BINARY_MB: u64 = 25;
/// PB-5 dist.
pub const PB5_DIST_MB: u64 = 40;

/// PB-6 throughput floor — MB/s sustained VT parse-and-render.
pub const PB6_THROUGHPUT_MB_S: u64 = 40;

/// PB-7 idle CPU — ≤ 1 % average over 10 min, zero wakeups when idle.
pub const PB7_IDLE_CPU_PCT: u64 = 1;

/// Correlated bounds reused across benches and `tools/perf/*`.
pub const MAX_CORPUS_BYTES: usize = 8 * 1024;
/// Correlated actions bound.
pub const MAX_ACTIONS: usize = 4096;

/// Returns `true` when no window or GPU types leak into the bench harness
/// (grep guard for CI). This is a compile-time witness: the crate never
/// `use`s `winit` or `wgpu` surface types outside `bitty-render`'s fake seam.
///
/// CTX-0100 note: this witness remains `true` for the headless baseline.
/// The new `startup::probe_winit_availability` / `probe_wgpu_availability`
/// do name `winit::EventLoop` and `GpuContext::initialize` behind a bounded
/// probe seam, but never construct a live `Window` or `wgpu::Surface` in
/// `cargo bench --no-run` without `BITTY_PERF_REAL_WINDOW=1`. A grep for
/// `winit::Window` / `wgpu::Surface` in `benches/` remains 0 except this
/// forbid-list and the `probe_*` impls which never leak handles.
#[must_use]
pub const fn is_headless_witness() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{BenchInvocation, SMOKE_WORKLOAD_DIVISOR};

    #[test]
    fn cargo_bench_flag_selects_the_full_measurement() {
        assert_eq!(
            BenchInvocation::from_args(["--bench", "--nocapture"]),
            BenchInvocation::Measure
        );
        assert_eq!(
            BenchInvocation::from_args(Vec::<String>::new()),
            BenchInvocation::Smoke
        );
        assert_eq!(
            BenchInvocation::from_args(["--nocapture"]),
            BenchInvocation::Smoke
        );
    }

    #[test]
    fn smoke_workload_is_scaled_but_never_below_its_floor() {
        assert_eq!(BenchInvocation::Measure.workload(5_000, 1), 5_000);
        assert_eq!(
            BenchInvocation::Smoke.workload(5_000, 1),
            5_000 / SMOKE_WORKLOAD_DIVISOR
        );
        assert_eq!(BenchInvocation::Smoke.workload(3, 1), 1);
        assert_eq!(BenchInvocation::Smoke.workload(10_000, 8_192), 8_192);
    }
}
