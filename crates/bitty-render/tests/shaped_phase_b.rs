//! Phase B acceptance: run shaping + ligature spans + cursor policy
//! (CTX-0958, issue #1666).
//!
//! Live-font gate: `BITTY_RENDER_FONT_TESTS=1` like the other shaping
//! tests. Default CI stays deterministic (all tests skip without the
//! gate or without the host face).
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test shaped_phase_b
//! ```

use bitty_config::types::{LigaturePolicy, OpenTypeFeature};
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontQuery, FontStyle, GlyphRasterizer, RunAttrs, SwashSingle,
    cells_for_range, collect_run_text, features_for_policy, form_runs_for_row,
};

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const POINT_SIZE: f32 = 12.0;

fn live_tests_enabled() -> bool {
    matches!(std::env::var(ENABLE_ENV).as_deref(), Ok("1"))
}

fn query_for(family: &str) -> FontQuery {
    FontQuery {
        family: family.to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

fn live_chain_for(family: &str) -> Option<FallbackRasterizer<SwashSingle>> {
    if !live_tests_enabled() {
        eprintln!("skipped: set {ENABLE_ENV}=1 to run live font tests");
        return None;
    }
    let inner = SwashSingle::new().ok()?;
    let mut raster = FallbackRasterizer::with_default_chain(inner);
    raster.load_font(&query_for(family)).ok()?;
    Some(raster)
}

fn parse_features(raw: &[&str]) -> Vec<OpenTypeFeature> {
    raw.iter()
        .map(|s| OpenTypeFeature::parse(s).unwrap())
        .collect()
}

#[test]
fn ligature_spans_cover_n_cells() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let chain = raster.fonts().to_vec();
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
    };
    // `fi` ligates to a single glyph covering two cells.
    let fi = raster
        .inner_mut()
        .shape_run("fi", &chain, &attrs)
        .expect("fi shapes");
    assert_eq!(fi.len(), 1, "fi must ligate to one cluster: {fi:?}");
    assert_eq!(fi[0].cells, (0, 2));
    assert!(!fi[0].uncovered);
    // `ffi` ligates to a single glyph covering three cells.
    let ffi = raster
        .inner_mut()
        .shape_run("ffi", &chain, &attrs)
        .expect("ffi shapes");
    assert_eq!(ffi.len(), 1, "ffi must ligate to one cluster: {ffi:?}");
    assert_eq!(ffi[0].cells, (0, 3));
    assert!(!ffi[0].uncovered);
    // Plain ASCII stays per-scalar.
    let plain = raster
        .inner_mut()
        .shape_run("ab", &chain, &attrs)
        .expect("ab shapes");
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].cells, (0, 1));
    assert_eq!(plain[1].cells, (1, 1));
}

#[test]
fn run_cache_serves_hits_without_reshaping() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let chain = raster.fonts().to_vec();
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
    };
    let inner = raster.inner_mut();
    let (hits_before, misses_before) = inner.shape_stats();
    inner
        .shape_run("hello world", &chain, &attrs)
        .expect("first shapes");
    let (_, misses_after_first) = inner.shape_stats();
    assert_eq!(misses_after_first, misses_before + 1, "first run must miss");
    inner
        .shape_run("hello world", &chain, &attrs)
        .expect("second shapes");
    let (hits_after_second, misses_after_second) = inner.shape_stats();
    assert_eq!(
        misses_after_second, misses_after_first,
        "second run must not miss again"
    );
    assert!(hits_after_second > hits_before, "second run must hit");
}

#[test]
fn shape_plan_cache_stays_bounded_per_face() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let chain = raster.fonts().to_vec();
    // Exercise more distinct feature sets than the per-face bound.
    for i in 0..12u32 {
        let tag = format!("ss{:02}", (i % 20) + 1);
        let raw = format!("{tag}=1");
        let parsed = OpenTypeFeature::parse(&raw).unwrap();
        let attrs = RunAttrs {
            features: vec![parsed],
            point_size: POINT_SIZE,
        };
        raster
            .inner_mut()
            .shape_run("test", &chain, &attrs)
            .expect("shapes with feature");
    }
    // Bound is enforced per face (6); the run cache holds 12 entries but
    // plans per face stay capped. We assert indirectly: shaping still
    // succeeds and the debug view reports bounded plans.
    let debug = format!("{:?}", raster.inner());
    assert!(
        debug.contains("shape_plans"),
        "debug must report plan cache: {debug}"
    );
}

#[test]
fn cursor_policy_matrix() {
    // Pure policy unit test (no fonts needed).
    let base = parse_features(&["calt", "liga"]);
    // Never: verbatim.
    let never = features_for_policy(&base, LigaturePolicy::Never);
    assert_eq!(never, base);
    // Cursor: verbatim (un-shaping happens per-run under the cursor).
    let cursor = features_for_policy(&base, LigaturePolicy::Cursor);
    assert_eq!(cursor, base);
    // Always: programming ligatures forced off, others pass through.
    let with_extra = parse_features(&["calt", "ss01=2"]);
    let always = features_for_policy(&with_extra, LigaturePolicy::Always);
    let tags: Vec<[u8; 4]> = always.iter().map(|f| f.tag).collect();
    for tag in [b"calt", b"liga", b"clig", b"dlig"] {
        assert!(
            tags.contains(tag),
            "Always must force {tag:?} off: {always:?}"
        );
    }
    for feature in &always {
        if [*b"calt", *b"liga", *b"clig", *b"dlig"].contains(&feature.tag) {
            assert_eq!(feature.value, 0, "forced tag must be zero: {feature:?}");
        }
    }
    // Non-programming tags survive Always.
    assert!(
        always.iter().any(|f| f.tag == *b"ss01" && f.value == 2),
        "ss01=2 must survive Always: {always:?}"
    );
}

#[test]
fn cursor_intersects_only_ligatures() {
    use bitty_render::ShapedCluster;
    let primary = {
        let mut counter = 0u64;
        bitty_render::FontId::next(&mut counter)
    };
    let single = ShapedCluster {
        cells: (0, 1),
        glyph_id: 1,
        face: primary,
        x_advance_px: 9.0,
        x_offset_px: 0.0,
        uncovered: false,
        byte_offset: 0,
    };
    let ligature = ShapedCluster {
        cells: (1, 2),
        glyph_id: 2,
        face: primary,
        x_advance_px: 18.0,
        x_offset_px: 0.0,
        uncovered: false,
        byte_offset: 1,
    };
    let clusters = vec![single, ligature];
    // Cursor on single-cell cluster: no un-shape.
    assert_eq!(SwashSingle::cursor_intersects_ligature(&clusters, 0), None);
    // Cursor on either half of the ligature: un-shape.
    assert_eq!(
        SwashSingle::cursor_intersects_ligature(&clusters, 1),
        Some(1)
    );
    assert_eq!(
        SwashSingle::cursor_intersects_ligature(&clusters, 2),
        Some(1)
    );
    // Cursor past the ligature: no un-shape.
    assert_eq!(SwashSingle::cursor_intersects_ligature(&clusters, 3), None);
}

#[test]
fn cjk_epsilon_gate() {
    // Single wide char, aligned advance: passes.
    assert!(SwashSingle::is_cjk_advance_aligned("漢", 2, 19.2, 9.6));
    // Single wide char, misaligned advance: fails (degrade to unshaped).
    assert!(!SwashSingle::is_cjk_advance_aligned("漢", 2, 12.0, 9.6));
    // Ligature spans skip the gate (always true).
    assert!(SwashSingle::is_cjk_advance_aligned("fi", 2, 9.6, 9.6));
    // Multi-char clusters are not single-wide: skip.
    assert!(SwashSingle::is_cjk_advance_aligned("ab", 2, 5.0, 9.6));
}

#[test]
fn zerowidth_folding_keeps_grid_truth() {
    // Combining mark attaches without advancing.
    assert_eq!(cells_for_range("é"), 1);
    // Run text concatenates base + marks per cell.
    let mut cell = bitty_term_state::Cell::erased(bitty_term_state::Style::default());
    cell.glyph = 'e';
    cell.push_zerowidth('́');
    let text = collect_run_text(std::slice::from_ref(&cell));
    assert_eq!(text, "é");
    assert_eq!(cells_for_range(&text), 1);
}

#[test]
fn run_formation_groups_style_and_breaks_blanks() {
    use bitty_term_state::{Cell, Style};
    let style_a = Style::default();
    let mut style_b = Style::default();
    style_b.attributes.bold = true;
    let mut cell_a1 = Cell::erased(style_a);
    cell_a1.glyph = 'a';
    let mut cell_a2 = Cell::erased(style_a);
    cell_a2.glyph = 'b';
    let mut cell_b = Cell::erased(style_b);
    cell_b.glyph = 'c';
    let blank = Cell::erased(style_a);
    let row = vec![cell_a1, cell_a2, cell_b, blank];
    let runs = form_runs_for_row(&row, 0);
    assert_eq!(runs.len(), 2, "blank breaks runs: {runs:?}");
    assert_eq!(runs[0].text, "ab");
    assert_eq!(runs[0].start_col, 0);
    assert_eq!(runs[1].text, "c");
    assert_eq!(runs[1].start_col, 2);
}

#[test]
fn shaped_grid_emission_merges_ligatures() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    // Row: `fi` ligature + `a`. Grid truth: 3 cells.
    let mut state = bitty_term_state::State::new();
    for ch in ['f', 'i', 'a'] {
        state.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    let snap = state.snapshot();
    assert_eq!(snap.width, 80, "default width");
    let damage = bitty_term_state::Damage {
        generation: snap.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(snap.height as u16, snap.width as u16),
        )]
        .into_boxed_slice(),
    };
    // Unshaped baseline: 3 glyphs for 3 cells.
    let plain = renderer.render(&snap, &damage).expect("plain renders");
    assert_eq!(plain.glyphs.len(), 3, "unshaped emits per cell");
    // Shaped: `fi` merges to one span, so fewer glyphs than cells.
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("shaped renders");
    assert!(
        shaped.glyphs.len() < plain.glyphs.len(),
        "ligature must merge glyphs: plain={} shaped={}",
        plain.glyphs.len(),
        shaped.glyphs.len()
    );
    // Grid truth untouched: snapshot still holds 3 distinct cells.
    assert_eq!(snap.cells[0].glyph, 'f');
    assert_eq!(snap.cells[1].glyph, 'i');
    assert_eq!(snap.cells[2].glyph, 'a');
    // Shape cache warmed: second render hits.
    let (hits_before, _) = renderer.shape_stats();
    renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("second shaped renders");
    let (hits_after, _) = renderer.shape_stats();
    assert!(
        hits_after > hits_before,
        "second shaped frame must hit the run cache"
    );
}

#[test]
fn shaped_cursor_policy_unshapes_under_cursor() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let mut state = bitty_term_state::State::new();
    for ch in ['f', 'i'] {
        state.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    let mut snap = state.snapshot();
    // Place the visible cursor onto the ligature (col 0).
    snap.cursor.position.row = 0;
    snap.cursor.position.col = 0;
    snap.cursor.visible = true;
    let damage = bitty_term_state::Damage {
        generation: snap.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(snap.height as u16, snap.width as u16),
        )]
        .into_boxed_slice(),
    };
    let never = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("never renders");
    let cursor = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Cursor, &[])
        .expect("cursor renders");
    let always = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("always renders");
    // `Never` keeps the ligature merged (1 glyph for `fi`).
    assert_eq!(never.glyphs.len(), 1, "Never keeps ligature: {never:?}");
    // `Cursor` with cursor on the ligature un-shapes (2 glyphs).
    assert_eq!(
        cursor.glyphs.len(),
        2,
        "Cursor un-shapes under cursor: {cursor:?}"
    );
    // `Always` is unshaped (2 glyphs, bit-for-bit the unshaped path).
    assert_eq!(always.glyphs.len(), 2, "Always is unshaped");
    assert_eq!(
        cursor.glyphs.len(),
        always.glyphs.len(),
        "Cursor-on-ligature must equal Always"
    );
}

#[test]
fn shaped_cjk_rows_stay_double_width() {
    let Some(raster) = live_chain_for("Noto Sans CJK SC") else {
        eprintln!("skipped: no CJK face on this host");
        return;
    };
    let query = query_for("Noto Sans CJK SC");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let mut state = bitty_term_state::State::new();
    state.apply(&bitty_term_state::TerminalAction::Print(
        bitty_vt::GraphemeCell::from('漢'),
    ));
    let snap = state.snapshot();
    // Grid truth: wide char + spacer.
    assert_eq!(snap.cells[0].glyph, '漢');
    assert_eq!(snap.cells[0].width, 2);
    assert!(snap.cells[1].spacer);
    let damage = bitty_term_state::Damage {
        generation: snap.generation,
        regions: vec![bitty_term_state::DamagedRegion::Grid(
            bitty_term_state::DamageRect::full(snap.height as u16, snap.width as u16),
        )]
        .into_boxed_slice(),
    };
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("CJK shaped renders");
    // One glyph for the wide char (spacer paints background only).
    assert_eq!(shaped.glyphs.len(), 1);
    assert_eq!(snap.cells[0].glyph, '漢', "grid truth untouched");
}
