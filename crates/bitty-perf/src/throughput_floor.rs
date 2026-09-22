//! Throughput-floor baseline — PB-6 parse-and-render (CTX-0676, PERF-07).
//!
//! Bounded, headless, committed measurement of sustained VT
//! **parse-and-render** throughput over a fixed synthetic corpus, closing
//! issue #1061. The existing `parser_throughput.rs` (M1-11) isolates the
//! parser (`Parser::advance` only); PB-6 budgets the full single-core path,
//! so this module feeds each chunk through parse (`Parser::advance`) → apply
//! (`State::apply`) → render (`GridRenderer::render` with a fake
//! `GlyphRasterizer`, no GPU), timing the whole path as one MB/s number.
//!
//! ## Measurement contract
//!
//! - **Fixed synthetic corpus.** [`corpus_segment`] builds a deterministic
//!   64 KiB segment in code (plain runs + SGR color + cursor motion +
//!   erase + OSC title); the measured buffer repeats it to [`SAMPLE_BYTES`].
//!   No file discovery, no network, no RNG, no clock-dependent assertion.
//! - **Single core, median of rounds.** [`measure`] runs [`ROUNDS`] measured
//!   rounds (fresh `State`/`Parser`/renderer each round) and reports the
//!   median MB/s; every round renders once per [`CHUNK_BYTES`] chunk, like a
//!   terminal presenting per batch.
//! - **Honest floor.** The PB-6 floor is 40 MB/s sustained on the slowest
//!   Tier 1 reference machine; this capture is one workstation data point —
//!   a regression anchor, not a compliance claim. The bench prints the
//!   verdict and exits 0 either way; `--write-baseline` only refuses when
//!   the measurement itself failed (exit 2, no fabricated numbers).
//! - **Host context from the environment.** Provenance via
//!   [`real_window::BaselineMeta`] and [`real_window::HostContext`]; no
//!   checkout path, username, or hostname is embedded.
//!
//! `#![forbid(unsafe_code)]`, headless (no `winit::Window`, no
//! `wgpu::Surface`).
//!
//! Budget reference:
//! `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`.
//! Runbook: `crates/bitty-perf/baselines/throughput-floor-evidence.md`.

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::Instant;

use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, GridRenderer};
use bitty_term_state::{Damage, State};
use bitty_vt::Parser;

use super::real_window::{BaselineMeta, HostContext};

/// Schema version of the committed `pb-throughput-floor.json` artifact.
pub const THROUGHPUT_FLOOR_SCHEMA_VERSION: u32 = 1;
/// Committed PB-6 evidence artifact, relative to the repository root.
pub const THROUGHPUT_FLOOR_BASELINE_REL_PATH: &str =
    "crates/bitty-perf/baselines/pb-throughput-floor.json";

/// Chunk size fed to `Parser::advance` (matches `bitty-pty::READ_CHUNK_SIZE`).
pub const CHUNK_BYTES: usize = 8 * 1024;
/// Corpus segment size; the measured buffer repeats it to [`SAMPLE_BYTES`].
pub const SEGMENT_BYTES: usize = 64 * 1024;
/// Bytes parsed+rendered per measured round.
pub const SAMPLE_BYTES: usize = 1024 * 1024;
/// Measured rounds per run (median reported).
pub const ROUNDS: usize = 3;

// ---------------------------------------------------------------------------
// Fake rasterizer (same headless seam as idle.rs / render_prepare.rs)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeRasterizer {
    next: u64,
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, bitty_render::error::RenderError> {
        Ok(FontId::next(&mut self.next))
    }
    fn rasterize(
        &mut self,
        key: RasterKey,
    ) -> Result<Option<GlyphBitmap>, bitty_render::error::RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(key.character) % 3 + 6) as i32;
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side,
                    height: side,
                    advance: [side, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn fake_renderer() -> GridRenderer<FakeRasterizer> {
    let query = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(
        FakeRasterizer { next: 0 },
        &query,
        CellMetrics::new(8, 16).unwrap(),
    )
    .expect("fake renderer")
}

// ---------------------------------------------------------------------------
// Fixed synthetic corpus (deterministic, built in code)
// ---------------------------------------------------------------------------

/// One deterministic corpus line: plain run + SGR + cursor + erase + OSC.
const CORPUS_LINE: &[u8] = b"\x1b[38;5;196mERROR\x1b[0m \x1b[1mbuild failed\x1b[22m at \x1b[4msrc/main.rs:42\x1b[24m\n\x1b[2K\x1b[1G\x1b[32m$\x1b[0m cargo nextest run --workspace --status-level slow \x1b[33m128 passed\x1b[0m\nplain log output 0123456789 abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ\n\x1b]0;bitty-perf-corpus\x07\x1b[3A\x1b[2B\x1b[1C\x1b[1D\x1b[Kdone\n";

/// Build the fixed [`SEGMENT_BYTES`] corpus segment (line-repeated).
#[must_use]
pub fn corpus_segment() -> Vec<u8> {
    let mut out = Vec::with_capacity(SEGMENT_BYTES);
    while out.len() < SEGMENT_BYTES {
        let take = (SEGMENT_BYTES - out.len()).min(CORPUS_LINE.len());
        out.extend_from_slice(&CORPUS_LINE[..take]);
    }
    out
}

// ---------------------------------------------------------------------------
// Pure metric math (unit-testable, no display)
// ---------------------------------------------------------------------------

/// Throughput in MiB/s for `bytes` over `secs` (`None` when `secs <= 0`).
#[must_use]
pub fn mb_per_s(bytes: usize, secs: f64) -> Option<f64> {
    if secs <= 0.0 {
        return None;
    }
    Some(bytes as f64 / secs / (1024.0 * 1024.0))
}

/// Median of a non-empty sample (`None` when empty).
#[must_use]
pub fn median(mut samples: Vec<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = samples.len() / 2;
    if samples.len() % 2 == 1 {
        Some(samples[mid])
    } else {
        Some((samples[mid - 1] + samples[mid]) / 2.0)
    }
}

/// `true` when `mb_s` meets the PB-6 sustained floor.
#[must_use]
pub fn meets_pb6(mb_s: f64) -> bool {
    mb_s >= crate::PB6_THROUGHPUT_MB_S as f64
}

// ---------------------------------------------------------------------------
// Parse-and-render measurement
// ---------------------------------------------------------------------------

/// Sustained parse-and-render throughput report.
#[derive(Debug, Clone)]
pub struct ThroughputFloorReport {
    /// Bytes parsed+rendered per measured round.
    pub sample_bytes: usize,
    /// Measured rounds.
    pub rounds: usize,
    /// Per-round throughput in MiB/s (median is the headline).
    pub round_mb_s: Vec<f64>,
    /// Median throughput in MiB/s.
    pub median_mb_s: f64,
    /// Total decoded actions in the final round.
    pub actions_final_round: u64,
    /// Renders performed in the final round (one per chunk).
    pub renders_final_round: usize,
    /// Wall seconds across all measured rounds.
    pub elapsed_secs: f64,
}

impl ThroughputFloorReport {
    /// `true` when the median meets the PB-6 floor.
    #[must_use]
    pub fn meets_budget(&self) -> bool {
        meets_pb6(self.median_mb_s)
    }

    /// Human summary for bench output and evidence docs.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let verdict = if self.meets_budget() {
            "PASS"
        } else {
            "ABOVE_BUDGET"
        };
        let rounds: Vec<String> = self.round_mb_s.iter().map(|v| format!("{v:.2}")).collect();
        format!(
            "throughput-floor — {verdict} median {:.2} MiB/s vs PB-6 floor {} MiB/s (rounds [{}]; {} bytes/round x {} rounds, {} actions + {} renders final round, {:.2}s wall)",
            self.median_mb_s,
            crate::PB6_THROUGHPUT_MB_S,
            rounds.join(", "),
            self.sample_bytes,
            self.rounds,
            self.actions_final_round,
            self.renders_final_round,
            self.elapsed_secs,
        )
    }
}

/// Run the measurement with explicit bounds. `sample_bytes` is parsed and
/// rendered per round; `rounds` measured rounds reduce to a median.
pub fn measure(sample_bytes: usize, rounds: usize) -> Result<ThroughputFloorReport, String> {
    if rounds == 0 {
        return Err("rounds must be > 0".to_string());
    }
    if sample_bytes < CHUNK_BYTES {
        return Err(format!("sample_bytes must be >= {CHUNK_BYTES}"));
    }
    let segment = corpus_segment();
    let mut buffer = Vec::with_capacity(sample_bytes);
    while buffer.len() < sample_bytes {
        let take = (sample_bytes - buffer.len()).min(segment.len());
        buffer.extend_from_slice(&segment[..take]);
    }

    let mut round_mb_s = Vec::with_capacity(rounds);
    let mut elapsed_secs = 0.0;
    let mut actions_final_round = 0u64;
    let mut renders_final_round = 0usize;

    for _ in 0..rounds {
        let mut state = State::new();
        let mut parser = Parser::new();
        let mut renderer = fake_renderer();
        let mut actions: u64 = 0;
        let mut renders = 0usize;
        // Last presented generation: each batch renders only its own delta,
        // like a terminal presenting per batch (never re-renders history).
        let mut presented: u64 = 0;
        let t0 = Instant::now();
        for chunk in buffer.chunks(CHUNK_BYTES) {
            let mut pending = Vec::new();
            parser.advance(black_box(chunk), |a| pending.push(a));
            actions += pending.len() as u64;
            for action in pending.drain(..) {
                black_box(state.apply(black_box(&action)));
            }
            // Present per batch, like a terminal rendering each OT batch.
            let snap = black_box(state.snapshot());
            let damage = Damage {
                generation: snap.generation,
                regions: black_box(state.damage_since(presented)).into_boxed_slice(),
            };
            let list = renderer
                .render(black_box(&snap), black_box(&damage))
                .map_err(|e| format!("render failed: {e:?}"))?;
            black_box(list.plan.needs_draw() || list.glyphs.is_empty());
            presented = snap.generation;
            renders += 1;
        }
        let secs = t0.elapsed().as_secs_f64();
        elapsed_secs += secs;
        let Some(mb_s) = mb_per_s(sample_bytes, secs) else {
            return Err("non-positive round time".to_string());
        };
        round_mb_s.push(mb_s);
        actions_final_round = actions;
        renders_final_round = renders;
    }

    let Some(median_mb_s) = median(round_mb_s.clone()) else {
        return Err("no rounds measured".to_string());
    };
    Ok(ThroughputFloorReport {
        sample_bytes,
        rounds,
        round_mb_s,
        median_mb_s,
        actions_final_round,
        renders_final_round,
        elapsed_secs,
    })
}

/// Default-bounds measurement ([`SAMPLE_BYTES`] x [`ROUNDS`]).
pub fn measure_default() -> Result<ThroughputFloorReport, String> {
    measure(SAMPLE_BYTES, ROUNDS)
}

// ---------------------------------------------------------------------------
// Committed baseline artifact
// ---------------------------------------------------------------------------

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The committed evidence artifact, embedded at compile time.
#[must_use]
pub const fn committed_baseline_json() -> &'static str {
    include_str!("../baselines/pb-throughput-floor.json")
}

/// Serialize a measured report plus provenance into the committed shape.
#[must_use]
pub fn baseline_json(report: &ThroughputFloorReport, meta: &BaselineMeta) -> String {
    let host = HostContext::capture();
    let issues: Vec<String> = meta.issues.iter().map(u64::to_string).collect();
    let rounds: Vec<String> = report
        .round_mb_s
        .iter()
        .map(|v| format!("{v:.2}"))
        .collect();
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"schema_version\": {THROUGHPUT_FLOOR_SCHEMA_VERSION},\n"
    ));
    out.push_str(&format!("  \"task\": \"{}\",\n", escape(&meta.task)));
    out.push_str(&format!("  \"issues\": [{}],\n", issues.join(", ")));
    out.push_str(&format!(
        "  \"captured_at\": \"{}\",\n",
        escape(&meta.captured_at)
    ));
    out.push_str(&format!(
        "  \"revision\": \"{}\",\n",
        escape(&meta.revision)
    ));
    out.push_str(&format!("  \"command\": \"{}\",\n", escape(&meta.command)));
    out.push_str(&format!("  \"profile\": \"{}\",\n", escape(&meta.profile)));
    out.push_str(
        "  \"budget_ref\": \"docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor\",\n",
    );
    out.push_str("  \"host_context\": {\n");
    out.push_str(&format!("    \"os\": \"{}\",\n", escape(&host.os)));
    out.push_str(&format!("    \"arch\": \"{}\",\n", escape(&host.arch)));
    out.push_str(&format!(
        "    \"toolchain\": \"{}\",\n",
        escape(&host.toolchain)
    ));
    out.push_str(&format!("    \"cpus\": {},\n", host.cpus));
    match host.total_memory_mb {
        Some(mb) => out.push_str(&format!("    \"total_memory_mb\": {mb}\n")),
        None => out.push_str("    \"total_memory_mb\": null\n"),
    }
    out.push_str("  },\n");
    out.push_str("  \"bounds\": {\n");
    out.push_str(&format!("    \"chunk_bytes\": {CHUNK_BYTES},\n"));
    out.push_str(&format!("    \"segment_bytes\": {SEGMENT_BYTES},\n"));
    out.push_str(&format!("    \"sample_bytes\": {},\n", report.sample_bytes));
    out.push_str(&format!("    \"rounds\": {}\n", report.rounds));
    out.push_str("  },\n");
    out.push_str("  \"throughput\": {\n");
    out.push_str("    \"status\": \"measured\",\n");
    out.push_str(&format!(
        "    \"median_mb_s\": {:.2},\n",
        report.median_mb_s
    ));
    out.push_str(&format!("    \"round_mb_s\": [{}],\n", rounds.join(", ")));
    out.push_str(&format!(
        "    \"actions_final_round\": {},\n",
        report.actions_final_round
    ));
    out.push_str(&format!(
        "    \"renders_final_round\": {},\n",
        report.renders_final_round
    ));
    out.push_str(&format!(
        "    \"floor_mb_s\": {},\n",
        crate::PB6_THROUGHPUT_MB_S
    ));
    out.push_str(&format!("    \"meets_floor\": {}\n", report.meets_budget()));
    out.push_str("  }\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_deterministic_and_bounded() {
        let a = corpus_segment();
        let b = corpus_segment();
        assert_eq!(a, b);
        assert_eq!(a.len(), SEGMENT_BYTES);
    }

    #[test]
    fn throughput_math_holds() {
        assert!((mb_per_s(1024 * 1024, 1.0).unwrap() - 1.0).abs() < 1e-9);
        assert!(mb_per_s(1024, 0.0).is_none());
        assert!((median(vec![3.0, 1.0, 2.0]).unwrap() - 2.0).abs() < 1e-9);
        assert!((median(vec![4.0, 1.0, 2.0, 3.0]).unwrap() - 2.5).abs() < 1e-9);
        assert!(median(vec![]).is_none());
        assert!(meets_pb6(40.0));
        assert!(!meets_pb6(39.99));
    }

    #[test]
    fn rejects_degenerate_bounds() {
        assert!(measure(1024 * 1024, 0).is_err());
        assert!(measure(1024, 1).is_err());
    }

    #[test]
    fn small_sample_measures_headlessly() {
        let report = measure(64 * 1024, 1).expect("small sample must measure");
        assert_eq!(report.rounds, 1);
        assert!(report.median_mb_s > 0.0);
        assert!(report.actions_final_round > 0);
        assert!(report.renders_final_round > 0);
    }
}
