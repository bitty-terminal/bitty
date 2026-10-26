//! Grapheme clusters as one layout unit (issues #2262, #1767).
//!
//! CTX-1609 stores an extended grapheme cluster on one wide lead cell
//! (`Cell::glyph` plus the bounded `zerowidth` continuation buffer) with a
//! trailing spacer for the second column. Run formation must keep that
//! spacer inside one run and feed the full cluster text to HarfBuzz;
//! otherwise a ZWJ family or flag pair never shapes as one cluster.
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
fn run_formation_keeps_zwj_family_in_one_cell() {
    let state = state_with_text(ZWJ_FAMILY);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    // CTX-1609: grid truth is one wide lead carrying the whole cluster plus
    // one spacer, not one lead per family member.
    let lead = cells[0];
    assert_eq!(lead.glyph, '\u{1F468}');
    assert_eq!(lead.width, 2);
    assert!(!lead.spacer);
    assert_eq!(
        lead.zerowidth.as_slice(),
        &[
            '\u{200D}',
            '\u{1F469}',
            '\u{200D}',
            '\u{1F467}',
            '\u{200D}',
            '\u{1F466}'
        ]
    );
    assert!(cells[1].spacer, "trailing half is a spacer");
    assert!(!cells[2].spacer);
    assert_eq!(collect_run_text(&cells[..2]), ZWJ_FAMILY);
    let runs = form_runs_for_row(cells, 0);
    assert_eq!(
        runs.len(),
        1,
        "family must reach the shaper as one run: {runs:?}"
    );
    assert_eq!(runs[0].text, ZWJ_FAMILY);
    assert_eq!(runs[0].start_col, 0);
    assert_eq!(runs[0].col_count, 2, "one layout unit, two columns");
    assert_eq!(
        cells_for_range(&runs[0].text),
        2,
        "column accounting stays exact"
    );
}

#[test]
fn run_formation_keeps_regional_pair_and_variation_selector_clusters() {
    let flag_state = state_with_text(REGIONAL_PAIR);
    let flag_snapshot = flag_state.snapshot();
    let flag = row(&flag_snapshot, 0);
    // CTX-1609: a regional-indicator pair grows its lead to two cells.
    assert_eq!(flag[0].glyph, '\u{1F1EF}');
    assert_eq!(flag[0].width, 2);
    assert_eq!(flag[0].zerowidth.as_slice(), &['\u{1F1F5}']);
    assert!(flag[1].spacer);
    let flag_runs = form_runs_for_row(flag, 0);
    assert_eq!(flag_runs.len(), 1);
    assert_eq!(flag_runs[0].text, REGIONAL_PAIR);
    assert_eq!(flag_runs[0].col_count, 2);

    let heart_state = state_with_text(VARIATION_SELECTOR);
    let heart_snapshot = heart_state.snapshot();
    let heart = row(&heart_snapshot, 0);
    assert_eq!(heart[0].glyph, '\u{2764}');
    assert_eq!(heart[0].zerowidth.as_slice(), &['\u{FE0F}']);
    // Selector-16 emoji presentation widens the text-default base.
    assert_eq!(heart[0].width, 2);
    assert!(heart[1].spacer);
    let heart_runs = form_runs_for_row(heart, 0);
    assert_eq!(heart_runs.len(), 1);
    assert_eq!(heart_runs[0].text, VARIATION_SELECTOR);
    assert_eq!(heart_runs[0].col_count, 2);
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
    // CTX-1609: the grid commits one two-column cell for the whole family.
    // A font with the ZWJ ligature returns one glyph over those two columns;
    // a font without it returns one cluster per base, which the shaped
    // emitter only paints for the leading committed column.
    assert_eq!(cells_for_range(ZWJ_FAMILY), 2);
    if clusters.len() == 1 {
        assert_eq!(
            clusters[0].cells,
            (0, 2),
            "full family cluster spans two columns"
        );
    } else {
        assert_eq!(clusters[0].cells.0, 0, "the lead starts at column zero");
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
fn shaped_zwj_family_without_ligature_paints_one_cell() {
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
    // CTX-1609: the cluster owns one two-column cell, so a per-base shaper
    // result paints only the leading base; the remaining bases stay in
    // Terminal Truth (copy/search/shaping with a ligating font).
    assert_eq!(
        list.glyphs.len(),
        1,
        "one glyph for the committed cell: {list:?}"
    );
    let cell_width = f64::from(renderer.cell_metrics().width);
    let column = (f64::from(list.glyphs[0].dest[0]) / cell_width).round() as i64;
    assert_eq!(column, 0, "the lead keeps its lead column");
    assert_eq!(
        renderer.counters().spacer_cells_skipped,
        before.spacer_cells_skipped + 1,
        "one spacer paints background only"
    );
    assert_eq!(snapshot.cells[0].glyph, '\u{1F468}', "grid truth untouched");
}

#[test]
fn shaped_fallback_keeps_combining_mark_in_grid_truth() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{1F468}');
    // CTX-1609: use a three-person family (five tail scalars) so the
    // combining mark fits the six-scalar cluster cap. A full four-person
    // family plus a mark is the documented over-cap degradation.
    let text = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}\u{0301}";
    let state = state_with_text(text);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.cells[0].glyph, '\u{1F468}');
    assert_eq!(snapshot.cells[0].width, 2);
    assert_eq!(
        snapshot.cells[0].zerowidth.as_slice(),
        &['\u{200D}', '\u{1F469}', '\u{200D}', '\u{1F466}', '\u{0301}'],
        "grid truth keeps the mark on the cluster cell"
    );
    assert!(snapshot.cells[1].spacer);
    let features = vec![bitty_config::types::OpenTypeFeature::parse("ccmp=0").unwrap()];
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &features)
        .expect("per-base fallback renders");
    // Only the leading committed column paints; the family members and the
    // mark remain in Terminal Truth for the shaped path with a ligating
    // font and for copy/search/accessibility.
    assert!(
        !list.glyphs.is_empty(),
        "the lead base must paint: {list:?}"
    );
    let cell_width = f64::from(renderer.cell_metrics().width);
    let column = (f64::from(list.glyphs[0].dest[0]) / cell_width).round() as i64;
    assert_eq!(column, 0, "the lead base keeps column zero");
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
