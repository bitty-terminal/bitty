//! Grapheme clusters across wide-char spacers (issue #2262).
//!
//! The grid stores a wide lead plus its trailing spacer per base, so a ZWJ
//! family sequence is eight columns of cells with four logical bases. Run
//! formation must keep those spacer halves inside one run; otherwise HarfBuzz
//! never sees the full cluster and paints four separate glyphs.
//!
//! Deterministic tests run everywhere. Font-dependent cluster checks carry
//! the usual live-font gate:
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test shaped_graphemes
//! ```

use bitty_config::types::LigaturePolicy;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphRasterizer, GridRenderer,
    RasterKey, RunAttrs, ShapedCluster, SwashSingle, cells_for_range, collect_run_text,
    form_runs_for_row,
};
use bitty_term_state::{Cell, Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction};
use bitty_vt::GraphemeCell;

const ENABLE_ENV: &str = "BITTY_RENDER_FONT_TESTS";
const POINT_SIZE: f32 = 12.0;

/// ZWJ family: man, woman, girl, boy.
const ZWJ_FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
/// Regional indicator pair for JP.
const REGIONAL_PAIR: &str = "\u{1F1EF}\u{1F1F5}";
/// Red heart with emoji presentation selector.
const VARIATION_SELECTOR: &str = "\u{2764}\u{FE0F}";

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

/// Resolves one scalar through the fallback walk so lazy tails (emoji, CJK,
/// symbols) join the chain before `shape_run` reads `fonts()`.
fn warm_chain(raster: &mut FallbackRasterizer<SwashSingle>, scalar: char) {
    let primary = raster
        .fonts()
        .first()
        .copied()
        .expect("live chain always loads a primary");
    let key = RasterKey::new(scalar, primary, POINT_SIZE).expect("valid raster key");
    let _ = raster.rasterize(key);
}

fn print_text(state: &mut State, text: &str) {
    for ch in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
    }
}

fn state_with_text(text: &str) -> State {
    let mut state = State::new();
    print_text(&mut state, text);
    state
}

fn full_damage(state: &State) -> Damage {
    let snapshot = state.snapshot();
    Damage {
        generation: snapshot.generation,
        regions: vec![DamagedRegion::Grid(DamageRect::full(
            snapshot.height as u16,
            snapshot.width as u16,
        ))]
        .into_boxed_slice(),
    }
}

fn row(snapshot: &Snapshot, row: usize) -> &[Cell] {
    let start = row * snapshot.width;
    let end = start + snapshot.width;
    &snapshot.cells[start..end]
}

fn renderer_for(
    raster: FallbackRasterizer<SwashSingle>,
    family: &str,
) -> GridRenderer<FallbackRasterizer<SwashSingle>> {
    GridRenderer::new(
        raster,
        &query_for(family),
        CellMetrics::new(10, 22).unwrap(),
    )
    .expect("renderer builds")
}

fn shape(
    raster: &mut FallbackRasterizer<SwashSingle>,
    text: &str,
) -> Result<Vec<ShapedCluster>, bitty_render::RenderError> {
    let chain: Vec<FontId> = raster.fonts().to_vec();
    raster.inner_mut().shape_run(
        text,
        &chain,
        &RunAttrs {
            features: Vec::new(),
            point_size: POINT_SIZE,
        },
    )
}

#[test]
fn run_formation_spans_zwj_family_spacers() {
    let state = state_with_text(ZWJ_FAMILY);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    // Grid truth: four wide leads carrying the joins, four spacers.
    for (index, base) in ['\u{1F468}', '\u{1F469}', '\u{1F467}', '\u{1F466}']
        .into_iter()
        .enumerate()
    {
        let lead = cells[index * 2];
        assert_eq!(lead.glyph, base);
        assert_eq!(lead.width, 2);
        assert!(!lead.spacer);
        if index < 3 {
            assert_eq!(lead.zerowidth.as_slice(), &['\u{200D}']);
        }
        assert!(cells[index * 2 + 1].spacer, "trailing half is a spacer");
    }
    assert_eq!(collect_run_text(&cells[..8]), ZWJ_FAMILY);
    let runs = form_runs_for_row(cells, 0);
    assert_eq!(
        runs.len(),
        1,
        "family must reach the shaper as one run: {runs:?}"
    );
    assert_eq!(runs[0].text, ZWJ_FAMILY);
    assert_eq!(runs[0].start_col, 0);
    assert_eq!(runs[0].col_count, 8);
    assert_eq!(
        cells_for_range(&runs[0].text),
        8,
        "column accounting stays exact"
    );
}

#[test]
fn run_formation_keeps_regional_pair_and_variation_selector_clusters() {
    let flag_state = state_with_text(REGIONAL_PAIR);
    let flag_snapshot = flag_state.snapshot();
    let flag = row(&flag_snapshot, 0);
    // Regional indicators are width-1 cells, so no spacers are involved.
    assert_eq!(flag[0].glyph, '\u{1F1EF}');
    assert_eq!(flag[1].glyph, '\u{1F1F5}');
    assert!(!flag[0].spacer && !flag[1].spacer);
    let flag_runs = form_runs_for_row(flag, 0);
    assert_eq!(flag_runs.len(), 1);
    assert_eq!(flag_runs[0].text, REGIONAL_PAIR);
    assert_eq!(flag_runs[0].col_count, 2);

    let heart_state = state_with_text(VARIATION_SELECTOR);
    let heart_snapshot = heart_state.snapshot();
    let heart = row(&heart_snapshot, 0);
    assert_eq!(heart[0].glyph, '\u{2764}');
    assert_eq!(heart[0].zerowidth.as_slice(), &['\u{FE0F}']);
    let heart_runs = form_runs_for_row(heart, 0);
    assert_eq!(heart_runs.len(), 1);
    assert_eq!(heart_runs[0].text, VARIATION_SELECTOR);
    assert_eq!(heart_runs[0].col_count, 1);
}

#[test]
fn shaped_zwj_family_cluster_when_font_supports_it() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{1F468}');
    let clusters = shape(&mut raster, ZWJ_FAMILY).expect("family shapes without error");
    eprintln!("family clusters: {clusters:?}");
    let covered = clusters.iter().any(|c| !c.uncovered);
    if !covered {
        eprintln!("skipped: host has no emoji coverage for the family sequence");
        return;
    }
    // A font with the ZWJ ligature returns one glyph over the whole eight
    // columns; a font without it returns per-base glyphs that still start on
    // wide leads (never on a spacer or joiner column). Multi-glyph clusters
    // share one byte offset, so per-member cell counts are placeholders; only
    // the fully-covered distinct-offset case sums to the eight columns.
    if clusters.iter().all(|cluster| !cluster.uncovered) {
        if clusters.iter().all(|cluster| cluster.byte_offset == 0) {
            assert_eq!(cells_for_range(ZWJ_FAMILY), 8);
        } else {
            let covered_cells: usize = clusters.iter().map(|cluster| cluster.cells.1).sum();
            assert_eq!(
                covered_cells, 8,
                "covered clusters must span every grid column: {clusters:?}"
            );
        }
    }
    if clusters.len() == 1 {
        assert_eq!(
            clusters[0].cells,
            (0, 8),
            "full family cluster spans eight columns"
        );
    } else {
        for cluster in &clusters {
            assert!(
                cluster.cells.0 % 2 == 0,
                "per-base fallback starts on wide leads, never spacers: {clusters:?}"
            );
        }
    }
    // The shaped frame emits no glyph at a spacer-only column: every glyph
    // starts on an even (lead) column for this all-wide row. The unshaped
    // render first warms the lazy fallback chain, as production frames do.
    let state = state_with_text(ZWJ_FAMILY);
    let snapshot = state.snapshot();
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let before = renderer.counters();
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("family shaped render succeeds");
    assert!(
        !list.glyphs.is_empty(),
        "family must emit at least one glyph"
    );
    let cell_width = f64::from(renderer.cell_metrics().width);
    for glyph in &list.glyphs {
        let column = (f64::from(glyph.dest[0]) / cell_width).round() as i64;
        assert!(
            column % 2 == 0,
            "glyph starts on a spacer column: {glyph:?}"
        );
    }
    assert!(
        renderer.counters().spacer_cells_skipped > before.spacer_cells_skipped,
        "spacers still paint backgrounds only"
    );
    assert_eq!(snapshot.cells[0].glyph, '\u{1F468}', "grid truth untouched");
}

#[test]
fn shaped_zwj_family_without_ligature_falls_back_per_cell() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{1F468}');
    // `ccmp=0` disables the required ZWJ ligature, reproducing a font that
    // covers the individual bases but not the family cluster.
    let features = vec![bitty_config::types::OpenTypeFeature::parse("ccmp=0").unwrap()];
    let attrs = RunAttrs {
        // The chain was warmed above; shape() uses an empty feature list, so
        // repeat the call with the disabling feature explicitly.
        features: features.clone(),
        point_size: POINT_SIZE,
    };
    let chain: Vec<FontId> = raster.fonts().to_vec();
    let clusters = raster
        .inner_mut()
        .shape_run(ZWJ_FAMILY, &chain, &attrs)
        .expect("family shapes without the ligature feature");
    eprintln!("no-ligature clusters: {clusters:?}");
    if clusters.len() <= 1 {
        eprintln!("skipped: host font ignored ccmp=0 for the family sequence");
        return;
    }
    let state = state_with_text(ZWJ_FAMILY);
    let snapshot = state.snapshot();
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let before = renderer.counters();
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &features)
        .expect("per-base fallback renders");
    // Four bases, one glyph each, on their own grid columns; join controls
    // and spacers never emit. No stacking at the cluster origin.
    assert_eq!(list.glyphs.len(), 4, "one glyph per emoji base: {list:?}");
    let cell_width = f64::from(renderer.cell_metrics().width);
    let mut columns: Vec<i64> = list
        .glyphs
        .iter()
        .map(|glyph| (f64::from(glyph.dest[0]) / cell_width).round() as i64)
        .collect();
    columns.sort_unstable();
    assert_eq!(columns, vec![0, 2, 4, 6], "bases keep their lead columns");
    assert_eq!(
        renderer.counters().spacer_cells_skipped,
        before.spacer_cells_skipped + 4,
        "four spacers paint background only"
    );
    assert_eq!(snapshot.cells[0].glyph, '\u{1F468}', "grid truth untouched");
}

#[test]
fn shaped_fallback_keeps_combining_mark_on_wide_base() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{1F468}');
    // A combining mark on the final wide base must survive the per-cell
    // fallback, matching the #2279 base-pen mark path.
    let text = format!("{ZWJ_FAMILY}\u{0301}");
    let state = state_with_text(&text);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.cells[6].glyph, '\u{1F466}');
    assert_eq!(
        snapshot.cells[6].zerowidth.as_slice(),
        &['\u{0301}'],
        "grid truth keeps the mark on the final base"
    );
    let features = vec![bitty_config::types::OpenTypeFeature::parse("ccmp=0").unwrap()];
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &features)
        .expect("per-base fallback renders");
    assert_eq!(
        list.glyphs.len(),
        5,
        "four bases plus the combining mark: {list:?}"
    );
    let cell_width = f64::from(renderer.cell_metrics().width);
    let mut columns: Vec<i64> = list
        .glyphs
        .iter()
        .map(|glyph| (f64::from(glyph.dest[0]) / cell_width).round() as i64)
        .collect();
    columns.sort_unstable();
    // The mark follows the final base's advance; it may cross into the last
    // spacer column but must never precede its base or land on columns 1/3/5.
    assert_eq!(columns[..4], [0, 2, 4, 6], "bases keep lead columns");
    assert!(columns[4] >= 6, "mark never precedes its base: {columns:?}");
    assert_eq!(snapshot.cells[6].glyph, '\u{1F466}', "grid truth untouched");
}

#[test]
fn shaped_ligatures_survive_wide_spacers() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{6F22}');
    let text = "fi\u{6F22}fi";
    let clusters = shape(&mut raster, text).expect("mixed ligature run shapes");
    eprintln!("ligature clusters: {clusters:?}");
    let ligatures: Vec<usize> = clusters
        .iter()
        .filter(|c| !c.uncovered && c.cells.1 == 2 && c.cells.0 != 2)
        .map(|c| c.cells.0)
        .collect();
    assert!(
        ligatures.contains(&0) && ligatures.contains(&4),
        "fi ligatures on both sides of the wide spacer must survive: {clusters:?}"
    );
}

#[test]
fn shaped_cjk_and_box_drawing_keep_one_glyph_per_cell() {
    let Some(raster) = live_chain_for("Noto Sans CJK SC") else {
        eprintln!("skipped: no CJK face on this host");
        return;
    };
    let text = "\u{6F22}A\u{2502}\u{2500}";
    let state = state_with_text(text);
    let snapshot = state.snapshot();
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans CJK SC");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let before = renderer.counters();
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("CJK/box shaped render succeeds");
    assert_eq!(
        list.glyphs.len(),
        4,
        "wide CJK plus three narrow cells: {list:?}"
    );
    assert_eq!(snapshot.cells[0].glyph, '\u{6F22}');
    assert!(snapshot.cells[1].spacer);
    assert_eq!(snapshot.cells[2].glyph, 'A');
    assert_eq!(snapshot.cells[3].glyph, '\u{2502}');
    assert_eq!(snapshot.cells[4].glyph, '\u{2500}');
    assert_eq!(
        renderer.counters().spacer_cells_skipped,
        before.spacer_cells_skipped + 1
    );
}
