//! Differential parity: `SwashSingle` vs the captured crossfont baseline
//! (CTX-0957, issue #1666).
//!
//! The baseline (`fixtures/crossfont_baseline.json`) was captured from
//! `CrossFontRasterizer` + `FallbackRasterizer::with_default_chain` on the
//! reference host (JetBrainsMono Nerd Font 12pt) by a since-removed one-shot
//! `tests/capture_baseline.rs`. The crossfont backend itself stays in-tree
//! as the production default (CTX-0957 additive landing, DEC-0095 — see the
//! `Backend selection` section in the crate docs); these tests exercise the
//! explicit shaped opt-in (`SwashSingle::new` + default chain).
//! They are gated behind `BITTY_RENDER_FONT_TESTS=1` like the other
//! live-font tests and additionally skip when the primary face is missing,
//! so default CI stays deterministic.
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test shaped_parity
//! ```
//!
//! Parity contract (structural — the two engines hint differently, so
//! byte-identical bitmaps are NOT expected):
//!
//! - coverage (`covered`) is EQUAL for every corpus scalar: no glyph the old
//!   stack drew may go tofu, and no tofu may become a glyph;
//! - when both sides cover from an outline or monochrome bitmap strike,
//!   bitmap dimensions match within ±1px (hinting rounding) and the bitmap
//!   is non-empty exactly when the baseline is; color-strike glyphs assert
//!   coverage + non-blank only (strike selection legitimately differs from
//!   the old stack there — the design resolves emoji-presentation scalars
//!   to the emoji tail flattened monochrome);
//! - the recorded crossfont metrics triple is NOT usable
//!   (`FontMetrics::is_usable == false`: unrounded advance, zero line and
//!   descent on the FreeType path), and `SwashSingle::font_metrics`
//!   answers `Ok(None)` — so the grid takes the legacy fixed-baseline rule
//!   on both stacks (placement unchanged by construction).
//!
//! The live `shape_run` checks below cover the Phase B skeleton over the
//! same chain: ASCII clusters, tofu flagging, and tail-face fallback.

use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontMetrics, FontQuery, FontStyle, GlyphRasterizer,
    GlyphSource, GridRenderer, RasterKey, RunAttrs, SwashSingle,
};

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const PRIMARY: &str = "JetBrainsMono Nerd Font";
const POINT_SIZE: f32 = 12.0;
const DIM_TOLERANCE_PX: i32 = 1;

/// Scalars the old stack covered through its dynamic per-glyph fontconfig
/// fallback (`face_for_glyph` → any system face) that the pinned chain
/// cannot reach. CTX-0961 owns the fix (CJK/script chain policy plus a
/// dynamic-fallback strategy with per-OS evidence); this allowlist closes
/// when that lands and must not gain new entries without a tracked task.
/// The test asserts the divergent set is EXACTLY this: a fixed gap fails
/// until the allowlist is updated with the fix, and any new divergence
/// fails as a coverage mismatch.
const KNOWN_DIVERGENT: &[u32] = &[0x6F22, 0x5B57];

#[derive(Debug)]
struct FixtureRow {
    ch: u32,
    covered: bool,
    w: i32,
    h: i32,
    len: usize,
}

#[derive(Debug)]
struct Fixture {
    metrics: (f32, f32, f32),
    rows: Vec<FixtureRow>,
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_row(line: &str) -> Option<FixtureRow> {
    let line = line
        .trim()
        .trim_start_matches('{')
        .trim_end_matches(['}', ',']);
    let mut ch = None;
    let mut covered = None;
    let mut w = None;
    let mut h = None;
    let mut len = None;
    for part in line.split(',') {
        let (key, value) = part.split_once(':')?;
        match key.trim().trim_matches('"') {
            "ch" => ch = value.trim().parse().ok(),
            "covered" => covered = parse_bool(value),
            "w" => w = value.trim().parse().ok(),
            "h" => h = value.trim().parse().ok(),
            "len" => len = value.trim().parse().ok(),
            _ => {}
        }
    }
    Some(FixtureRow {
        ch: ch?,
        covered: covered?,
        w: w?,
        h: h?,
        len: len?,
    })
}

fn load_fixture() -> Fixture {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/crossfont_baseline.json"
    );
    let text = std::fs::read_to_string(path).expect("baseline fixture must exist");
    let mut metrics = None;
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("\"metrics\"") {
            let nums: Vec<f32> = line
                .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            assert_eq!(nums.len(), 3, "fixture metrics triple: {line}");
            metrics = Some((nums[0], nums[1], nums[2]));
        } else if line.starts_with('{') {
            if let Some(row) = parse_row(line) {
                rows.push(row);
            }
        }
    }
    assert!(!rows.is_empty(), "fixture must hold the corpus");
    Fixture {
        metrics: metrics.expect("fixture metrics"),
        rows,
    }
}

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

/// Production chain over the new backend, or `None` when the env gate is
/// off or the host stack/primary face is unavailable.
fn live_chain() -> Option<FallbackRasterizer<SwashSingle>> {
    if !live_tests_enabled() {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run live font tests");
        return None;
    }
    let inner = SwashSingle::new().ok()?;
    let mut raster = FallbackRasterizer::with_default_chain(inner);
    raster.load_font(&query()).ok()?;
    Some(raster)
}

#[test]
fn swash_matches_crossfont_coverage_and_dimensions() {
    let Some(mut raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    let fixture = load_fixture();
    assert!(
        fixture.rows.len() >= 100,
        "fixture must cover the ASCII corpus plus TUI/symbol probes"
    );
    let primary = raster.fonts()[0];
    let mut mismatches = 0;
    let mut color_only = 0;
    let mut divergent = 0;
    for row in &fixture.rows {
        let Some(c) = char::from_u32(row.ch) else {
            continue;
        };
        let resolved = raster
            .resolve(RasterKey::new(c, primary, POINT_SIZE).unwrap())
            .expect("resolution must not error on a live stack");
        if KNOWN_DIVERGENT.contains(&row.ch) {
            // Documented gap: old stack covered via dynamic fallback, the
            // pinned chain stays tofu. Fails loudly if the gap ever closes
            // without updating the allowlist (or widens elsewhere).
            assert!(
                !resolved.covered,
                "U+{:04X}: known divergence closed — update KNOWN_DIVERGENT with the fix",
                row.ch
            );
            divergent += 1;
            continue;
        }
        if resolved.covered != row.covered {
            eprintln!(
                "COVERAGE DIVERGENCE U+{:04X}: fixture={} swash={}",
                row.ch, row.covered, resolved.covered
            );
            mismatches += 1;
            continue;
        }
        if !row.covered {
            assert!(
                resolved.bitmap.is_none(),
                "U+{:04X}: uncovered must carry no bitmap",
                row.ch
            );
            continue;
        }
        let bitmap = resolved
            .bitmap
            .as_ref()
            .unwrap_or_else(|| panic!("U+{:04X}: covered must carry a bitmap", row.ch));
        // Source gate: outline/mono-bitmap strikes compare dimensions;
        // color strikes (strike selection differs) compare coverage only.
        let source = raster
            .inner_mut()
            .glyph_source(resolved.font, c, POINT_SIZE)
            .expect("source probe must not error")
            .unwrap_or_else(|| panic!("U+{:04X}: covered must have a source", row.ch));
        if source == GlyphSource::ColorBitmap {
            color_only += 1;
            assert!(
                !bitmap.is_blank(),
                "U+{:04X}: color glyph must be non-blank",
                row.ch
            );
            continue;
        }
        let dw = (bitmap.metrics.width - row.w).abs();
        let dh = (bitmap.metrics.height - row.h).abs();
        assert!(
            dw <= DIM_TOLERANCE_PX && dh <= DIM_TOLERANCE_PX,
            "U+{:04X}: dims {}x{} vs baseline {}x{} (tol ±{DIM_TOLERANCE_PX})",
            row.ch,
            bitmap.metrics.width,
            bitmap.metrics.height,
            row.w,
            row.h
        );
        assert_eq!(
            bitmap.is_blank(),
            row.len == 0,
            "U+{:04X}: blank-ness must match the baseline",
            row.ch
        );
    }
    assert_eq!(mismatches, 0, "coverage must match the baseline exactly");
    assert_eq!(
        divergent,
        KNOWN_DIVERGENT.len(),
        "divergent set must be exactly KNOWN_DIVERGENT"
    );
    eprintln!("color-strike coverage-only glyphs: {color_only}");
}

#[test]
fn swash_metrics_keep_the_legacy_baseline_path() {
    let Some(raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    let fixture = load_fixture();
    // The recorded crossfont triple is degenerate on the FreeType path, so
    // the grid demonstrably took the legacy fixed-baseline rule there.
    let recorded = FontMetrics {
        average_advance_px: fixture.metrics.0,
        line_height_px: fixture.metrics.1,
        descent_px: fixture.metrics.2,
    };
    assert!(
        !recorded.is_usable(),
        "baseline metrics must be unusable (legacy path): {recorded:?}"
    );
    // ... and the new backend answers no measurement, so the grid takes the
    // same legacy path (placement unchanged by construction).
    let answered = raster
        .inner()
        .font_metrics(raster.fonts()[0], POINT_SIZE)
        .expect("metrics must not error");
    assert_eq!(answered, None, "Phase A reports no face measurement");
}

#[test]
fn shape_run_clusters_ascii_and_flags_tofu() {
    let Some(mut raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
    };
    let chain: Vec<FontId> = raster.fonts().to_vec();
    let primary = chain[0];
    let clusters = raster
        .inner_mut()
        .shape_run("->", &chain, &attrs)
        .expect("ascii shapes");
    assert_eq!(clusters.len(), 2, "unligated ascii shapes per scalar");
    for (i, cluster) in clusters.iter().enumerate() {
        assert!(!cluster.uncovered, "ascii must be covered");
        assert_eq!(cluster.face, primary);
        assert_eq!(cluster.cells, (i, 1));
        assert!(
            (cluster.x_advance_px - 9.6).abs() < 0.6,
            "advance near 9.6px at 12pt: {}",
            cluster.x_advance_px
        );
    }
    // U+10FFFF is a noncharacter: no face covers it, so it stays uncovered
    // for the tofu path.
    let unknown = raster
        .inner_mut()
        .shape_run("\u{10FFFF}", &chain, &attrs)
        .expect("unknown shapes");
    assert_eq!(unknown.len(), 1);
    assert!(unknown[0].uncovered);
    // Emoji falls through to the color-emoji tail: covered, but not by the
    // primary face. (Braille resolves at the primary Nerd face on this
    // stack, so it cannot exercise the tail walk.)
    let mixed = raster
        .inner_mut()
        .shape_run("A\u{1F600}B", &chain, &attrs)
        .expect("mixed shapes");
    assert_eq!(mixed.len(), 3);
    assert_eq!(mixed[0].face, primary);
    assert!(!mixed[0].uncovered);
    assert!(mixed[1].covered_by_tail(primary));
    assert_eq!(mixed[2].face, primary);
    assert_eq!(mixed[0].cells, (0, 1));
    assert_eq!(mixed[1].cells, (1, 2));
    assert_eq!(mixed[2].cells, (3, 1));
}

#[test]
fn swash_chain_paints_symbols_through_grid_pipeline() {
    let Some(raster) = live_chain() else {
        eprintln!("skipped: no host font stack or primary family");
        return;
    };
    // Same pipeline shape as the rewired live-fallback test: symbols emit
    // real glyphs, the unknown scalar paints tofu and is counted.
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
    assert_eq!(renderer.counters().missing_glyphs, 1);
}

trait ClusterExt {
    /// True when covered by a fallback tail (not the primary face).
    fn covered_by_tail(&self, primary: FontId) -> bool;
}

impl ClusterExt for bitty_render::ShapedCluster {
    fn covered_by_tail(&self, primary: FontId) -> bool {
        !self.uncovered && self.face != primary
    }
}
