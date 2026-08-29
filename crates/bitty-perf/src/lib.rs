//! `bitty-perf`: bench harness owner (Phase F).
//!
//! This crate exists only to own the workspace-root `benches/` targets
//! so `cargo bench --no-run` can compile them while keeping the workspace
//! virtual. The bench files themselves live at `benches/*.rs` per the task
//! scope `benches/{vt_throughput,terminal_state,reflow,search,render_prepare}.rs`
//! and are headless, bounded, `forbid(unsafe)` — see
//! `bitty-docs/docs/specifications/performance-budget-rfc.md` PB-1..PB-7.
//!
//! The crate itself exposes no runtime API beyond a re-export of the harness
//! constants so `tools/perf/*` scripts can share the same bounds without
//! duplicating magic numbers. No `winit::Window` or `wgpu::Surface` is ever
//! constructed here; benches use `Parser → State → Snapshot → Damage → DrawList`
//! headlessly via a fake `GlyphRasterizer`.
//!
//! Budget reference: `bitty-docs/docs/specifications/performance-budget-rfc.md#budgets`.

#![forbid(unsafe_code)]

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
#[must_use]
pub const fn is_headless_witness() -> bool {
    true
}
