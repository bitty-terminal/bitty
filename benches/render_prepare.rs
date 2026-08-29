//! Render prepare baseline — PB-4 / PB-7 frame-on-demand.
//!
//! Headless, bounded, `#![forbid(unsafe_code)]` harness for
//! `bitty-docs/docs/specifications/performance-budget-rfc.md`:
//! - PB-4 input latency: snapshot→frame plan→draw list must stay well
//!   under 8 ms p50 (this bench isolates `GridRenderer::render` with a
//!   fake `GlyphRasterizer`, no GPU, no `SurfaceTarget`).
//! - PB-7 idle: clean `Damage` must yield `FrameMode::Clean` with no draws.
//!
//! The bench exercises the owned grid pipeline (`Snapshot`/`Damage` →
//! `DrawList`/`Atlas`) headlessly via the same fake rasterizer unit tests
//! use (`src/cache.rs` `FakeRasterizer` and `src/gpu.rs` headless seam).
//! No `winit::Window`, no `wgpu::Surface` or adapter, no display server.
//! Bounded via `State` invariants, `Damage::regions` ≤ 256 (coalesced), and
//! atlas `MAX_FRAME_REGIONS` 256.
//!
//! Budget reference: `bitty-docs/docs/specifications/performance-budget-rfc.md#pb-4-input-latency`
//! and `#pb-7-idle-resource-usage` plus `crates/bitty-render/src/grid.rs` docs.
//!
//! Run headlessly:
//! ```text
//! cargo bench -p bitty-perf --bench render_prepare -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::Instant;

use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, GridRenderer};
use bitty_term_state::{Damage, State, TerminalAction};
use bitty_vt::{GraphemeCell, Parser};

#[derive(Debug)]
struct FakeR {
    next: u64,
}

impl GlyphRasterizer for FakeR {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }
    fn rasterize(&mut self, k: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if k.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(k.character) % 3 + 6) as i32;
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

fn fake_renderer() -> GridRenderer<FakeR> {
    let q = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(FakeR { next: 0 }, &q, CellMetrics::new(8, 16).unwrap())
        .expect("fake renderer")
}

fn state_with_content(lines: usize, paint_every: usize) -> (State, Damage) {
    let mut s = State::new();
    let mut parser = Parser::new();
    // Fill with SGR + printable so cells have varied styles.
    let line = b"\x1b[31mhello\x1b[0m world \x1b[1mBOLD\x1b[22m 0123456789\n";
    for i in 0..lines {
        let mut acts = Vec::new();
        parser.advance(line, |a| acts.push(a));
        for a in acts.drain(..) {
            s.apply(&a);
        }
        if i % paint_every == 0 {
            // Move cursor and print a marker to dirty a specific cell.
            s.apply(&TerminalAction::Print(GraphemeCell::from('X')));
        }
    }
    let snap = s.snapshot();
    let dmg = Damage {
        generation: snap.generation,
        regions: s.damage_since(0).into_boxed_slice(),
    };
    // Ensure damage is bounded (coalesced ≤ 256).
    assert!(dmg.regions.len() <= 256, "damage bound");
    (s, dmg)
}

fn bench_render_once(
    renderer: &mut GridRenderer<FakeR>,
    state: &State,
    dmg: &Damage,
    iters: usize,
) -> f64 {
    let snap = state.snapshot();
    // Warmup.
    let _ = renderer.render(&snap, dmg).expect("warmup render");
    let start = Instant::now();
    for _ in 0..iters {
        let snap = black_box(state.snapshot());
        let out = renderer
            .render(black_box(&snap), black_box(dmg))
            .expect("render");
        black_box(out);
    }
    let elapsed = start.elapsed().as_secs_f64().max(1e-9);
    (elapsed * 1_000_000.0) / iters as f64
}

fn main() {
    println!("render_prepare — GridRenderer::render headless fake (PB-4 8 ms p50, PB-7 Clean)");
    println!("  cell metrics 8×16, atlas default, bounded MAX_FRAME_REGIONS 256, forbid(unsafe)");

    // Small full damage: typical prompt line dirty.
    let (small_state, small_dmg) = state_with_content(2, 99);
    let snap = small_state.snapshot();
    let mut r = fake_renderer();
    let mean_small = bench_render_once(&mut r, &small_state, &small_dmg, 1_500);
    let list = r.render(&snap, &small_dmg).expect("list");
    println!(
        "  small_2_lines_full: {mean_small:.2} µs mean (cells {}, fills {}, glyphs {}, plan {:?})",
        snap.cells.len(),
        list.fills.len(),
        list.glyphs.len(),
        list.plan.mode
    );

    // Large full damage: 80×24 full screen + scrollback pressure.
    let (big_state, big_dmg) = state_with_content(30, 5);
    let snap_big = big_state.snapshot();
    let mut r2 = fake_renderer();
    let mean_big = bench_render_once(&mut r2, &big_state, &big_dmg, 800);
    let list_big = r2.render(&snap_big, &big_dmg).expect("big list");
    println!(
        "  big_30_lines_full: {mean_big:.2} µs mean (cells {}, fills {}, glyphs {}, plan {:?})",
        snap_big.cells.len(),
        list_big.fills.len(),
        list_big.glyphs.len(),
        list_big.plan.mode
    );

    // Partial damage: one cell dirty after clean frame.
    let mut partial_state = State::new();
    partial_state.apply(&TerminalAction::Print(GraphemeCell::from('A')));
    let mut r3 = fake_renderer();
    let snap0 = partial_state.snapshot();
    let dmg0 = Damage {
        generation: snap0.generation,
        regions: partial_state.damage_since(0).into_boxed_slice(),
    };
    let _ = r3.render(&snap0, &dmg0).expect("prime");
    let generation = snap0.generation;
    // No further mutation → damage_since(generation) should yield empty or Clean.
    let snap1 = partial_state.snapshot();
    let dmg_clean = Damage {
        generation: snap1.generation,
        regions: partial_state.damage_since(generation).into_boxed_slice(),
    };
    let list_clean = r3.render(&snap1, &dmg_clean).expect("clean");
    println!(
        "  clean_frame: mode {:?} dirty {} (PB-7 idle: expect Clean / 0 draws)",
        list_clean.plan.mode,
        list_clean.plan.dirty_rects.len()
    );
    assert!(
        !list_clean.plan.needs_draw()
            || list_clean.plan.mode == bitty_render::frame::FrameMode::Clean,
        "idle clean should not draw"
    );
    let mean_clean = bench_render_once(&mut r3, &partial_state, &dmg_clean, 3_000);
    println!("  clean (idle) mean: {mean_clean:.2} µs (PB-7 zero-wakeup path; should be << 8 ms)");

    // One-cell dirty (partial) after idle.
    partial_state.apply(&TerminalAction::Print(GraphemeCell::from('B')));
    let snap2 = partial_state.snapshot();
    let dmg_partial = Damage {
        generation: snap2.generation,
        regions: partial_state.damage_since(generation).into_boxed_slice(),
    };
    let mut r4 = fake_renderer();
    let mean_partial = bench_render_once(&mut r4, &partial_state, &dmg_partial, 2_000);
    let list_partial = r4.render(&snap2, &dmg_partial).expect("partial");
    println!(
        "  partial_1_cell: {mean_partial:.2} µs mean (mode {:?}, dirty {})",
        list_partial.plan.mode,
        list_partial.plan.dirty_rects.len()
    );

    // Tail gate: full-frame prepare should stay well under 8 ms p50.
    for (label, us) in [
        ("small", mean_small),
        ("big", mean_big),
        ("clean", mean_clean),
        ("partial", mean_partial),
    ] {
        if us > 8_000.0 {
            eprintln!("warning: render_prepare {label} {us:.0} µs exceeds PB-4 p50 8 ms headroom");
        }
    }
}
