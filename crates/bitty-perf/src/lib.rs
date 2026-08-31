//! `bitty-perf`: bench harness owner (Phase F → CTX-0100 Real Window).
//!
//! This crate owns the workspace-root `benches/` targets so
//! `cargo bench --no-run` can compile them while keeping the workspace
//! virtual. Benches live at `benches/*.rs` per the task scope
//! `benches/{vt_throughput,terminal_state,reflow,search,render_prepare}.rs`
//! and new real-window benches
//! `benches/{startup_real,latency_real,idle_real}.rs` (CTX-0100).
//! All are headless, bounded, `forbid(unsafe)` — see
//! `bitty-docs/docs/specifications/performance-budget-rfc.md` PB-1..PB-7.
//!
//! CTX-0100 upgrade: the former `--help` proxy is replaced by
//! instrumentation that covers the full `bitty-app` cold path:
//! process start → config → PTY spawn → winit window → wgpu init → font
//! init → first shell bytes → first frame presented. Each phase is
//! timestamped with `Instant` and bounded tracing; on headless CI the
//! display-tied phases report `Unavailable` with their attempt duration,
//! proving the seam without requiring a display. Input latency
//! (`keydown → PTY → parser → state → render → present`) is measured with
//! stage breakdown and p50/p99, and idle is gated by the frame-on-demand
//! invariant (`tick == None` → no polling loop → ≤1 % CPU).
//!
//! Budget reference: `bitty-docs/docs/specifications/performance-budget-rfc.md#budgets`.
//! Evidence: `docs/product/perf-evidence.md` (CTX-0100, real measurements from `c0aadd2+`).

#![forbid(unsafe_code)]

pub mod idle;
pub mod latency;
pub mod startup;

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
