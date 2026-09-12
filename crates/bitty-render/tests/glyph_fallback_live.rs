//! Live font-stack coverage tests for the fallback chain (CTX-0368).
//!
//! These tests exercise the real `CrossFontRasterizer` + `FallbackRasterizer`
//! pipeline against the host font stack. They are gated behind
//! `BITTY_RENDER_FONT_TESTS=1` (mirroring the `BITTY_RENDER_GPU_TESTS` gate)
//! and additionally skip when no platform font stack / loadable primary
//! family is available, so default CI stays deterministic on bare runners
//! that may resolve any family through fontconfig substitution.
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test glyph_fallback_live
//! ```
//!
//! Deterministic coverage semantics (primary miss -> fallback hit, unknown ->
//! tofu, bounded walk/cache) are unit-tested headlessly in `src/fallback.rs`
//! and `src/grid/tests.rs`; this file is the live-font evidence layer.

use bitty_render::{
    CellMetrics, CrossFontRasterizer, FallbackRasterizer, FontQuery, FontStyle, GlyphRasterizer,
    GridRenderer, RasterKey, ResolvedGlyph,
};

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const PRIMARY: &str = "JetBrainsMono Nerd Font";
const POINT_SIZE: f32 = 12.0;

fn live_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

fn query() -> FontQuery {
    FontQuery {
        family: PRIMARY.to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

/// Builds the production fallback chain over the host font stack, or `None`
/// when the env gate is off or the stack/primary face is unavailable.
fn live_chain() -> Option<FallbackRasterizer<CrossFontRasterizer>> {
    if !live_tests_enabled() {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run live font tests");
        return None;
    }
    let inner = CrossFontRasterizer::new().ok()?;
    let mut raster = FallbackRasterizer::with_default_chain(inner);
    raster.load_font(&query()).ok()?;
    Some(raster)
}

#[test]
fn host_symbol_stack_reports_coverage_and_tofu() {
    let Some(mut raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    let primary = raster.fonts()[0];
    // Host coverage probe: the fixture symbols and TUI-graph scalars used by
    // the acceptance screenshot. Each must report `covered = true`.
    for c in [
        '✔', '☑', '⚙', '→', '±', '×', '·', '⣿', '─', '│', '┌', '┐', '✅',
    ] {
        let resolved = raster
            .resolve(RasterKey::new(c, primary, POINT_SIZE).unwrap())
            .expect("resolution must not error on a live stack");
        assert!(resolved.covered, "U+{:04X} {c:?} must be covered", c as u32);
        assert!(resolved.bitmap.is_some());
    }
    // U+10FFFF is a noncharacter: no host font may claim coverage, so it is
    // the deterministic tofu probe.
    let unknown = raster
        .resolve(RasterKey::new('\u{10FFFF}', primary, POINT_SIZE).unwrap())
        .expect("unknown resolution must not error");
    assert_eq!(
        unknown,
        ResolvedGlyph {
            font: primary,
            covered: false,
            bitmap: None,
        }
    );
}

#[test]
fn host_pipeline_paints_symbols_and_tofu() {
    let Some(raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    let mut renderer =
        GridRenderer::new(raster, &query(), CellMetrics::new(10, 22).unwrap()).unwrap();
    let symbols = "✔ ☑ ⚙ → ± × · ⣿ ─│┌┐";
    let mut state = bitty_term_state::State::new();
    for ch in symbols.chars() {
        state.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    state.apply(&bitty_term_state::TerminalAction::Print(
        bitty_vt::GraphemeCell::from('\u{10FFFF}'),
    ));
    let snap = state.snapshot();
    let damage = bitty_term_state::Damage {
        generation: snap.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(snap.height as u16, snap.width as u16),
        )]
        .into_boxed_slice(),
    };
    let list = renderer.render(&snap, &damage).expect("render");
    let symbol_count = symbols.chars().filter(|c| *c != ' ').count();
    assert_eq!(
        list.glyphs.len(),
        symbol_count,
        "every symbol must emit a real glyph on a host that covers it"
    );
    // The unknown scalar paints the RFC tofu box (4 outline fills) and is
    // counted; covered symbols are not.
    assert_eq!(renderer.counters().missing_glyphs, 1);
    let tofu_fills = list
        .fills
        .iter()
        .filter(|fill| (fill.rect.width, fill.rect.height) == (10, 1))
        .count();
    assert!(
        tofu_fills >= 2,
        "tofu outline top/bottom edges expected, found {tofu_fills}"
    );
}

#[test]
fn host_chain_is_bounded_and_deterministic() {
    let Some(mut a) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    // The chain is a pinned list: primary plus at most MAX_FALLBACK_FAMILIES
    // tails, all loaded once at startup and walked at most once per scalar.
    assert!(a.fonts().len() <= 1 + bitty_config::types::MAX_FALLBACK_FAMILIES);
    assert!(a.fallback_families().len() <= bitty_config::types::MAX_FALLBACK_FAMILIES);
    let scalars = ['✔', '☑', '⚙', '⣿', 'A', '\u{10FFFF}'];
    let mut first = Vec::new();
    for c in scalars {
        let primary = a.fonts()[0];
        let resolved = a
            .resolve(RasterKey::new(c, primary, POINT_SIZE).unwrap())
            .unwrap();
        let face = a.fonts().iter().position(|f| *f == resolved.font);
        first.push((resolved.covered, face));
    }
    let Some(mut b) = live_chain() else {
        unreachable!("first live chain succeeded");
    };
    let mut second = Vec::new();
    for c in scalars {
        let primary = b.fonts()[0];
        let resolved = b
            .resolve(RasterKey::new(c, primary, POINT_SIZE).unwrap())
            .unwrap();
        let face = b.fonts().iter().position(|f| *f == resolved.font);
        second.push((resolved.covered, face));
    }
    assert_eq!(first, second, "face selection must be deterministic");
}
