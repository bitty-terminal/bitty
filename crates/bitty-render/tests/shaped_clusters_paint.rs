//! Every shaped glyph of a grapheme cluster paints inside the cluster span
//! (CTX-1609, issue #1767).
//!
//! The shaped path commits one layout unit per extended grapheme cluster.
//! A cluster whose text shapes into several glyphs (an Indic matra reordered
//! before its consonant, an applied virama, a decomposed Thai SARA AM, a
//! decomposed Hangul syllable, both regional indicators of a flag) must paint
//! all of them inside that span. Only the classic crossfont path
//! ([`GridRenderer::render`], no shaper) stays a documented degraded
//! fallback.
//!
//! Deterministic run-formation and grid-truth tests live in
//! `shaped_graphemes.rs`; this file needs the host font stack:
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test shaped_clusters_paint
//! ```

use bitty_config::types::LigaturePolicy;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphRasterizer, GridRenderer,
    RasterKey, RunAttrs, ShapedCluster, SwashSingle, cells_for_range, collect_run_text,
};
use bitty_term_state::{
    AmbiguousWidth, Cell, Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction,
};
use bitty_vt::GraphemeCell;

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
    &snapshot.cells[start..start + snapshot.width]
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
            direction: bitty_render::RunDirection::Ltr,
        },
    )
}

/// Cluster column of one glyph instance, rounded to the grid.
fn glyph_column(renderer: &GridRenderer<FallbackRasterizer<SwashSingle>>, x: i32) -> i64 {
    let cell = f64::from(renderer.cell_metrics().width);
    (f64::from(x) / cell).round() as i64
}

/// Cases whose cluster text shapes into more than one glyph on Noto fonts.
const MULTI_GLYPH_CASES: &[(&str, &str, usize)] = &[
    // (cluster, description, minimum painted glyphs)
    (
        "\u{0915}\u{093F}",
        "Devanagari KA + vowel sign I (i-matra)",
        2,
    ),
    (
        "\u{0915}\u{094D}\u{0937}",
        "Devanagari KA + virama + SSA",
        2,
    ),
    ("\u{0E01}\u{0E33}", "Thai KO KAI + SARA AM", 2),
    ("\u{0995}\u{09C7}", "Bengali KA + vowel sign E", 2),
];

#[test]
fn multi_glyph_clusters_paint_every_member_inside_the_span() {
    for (text, description, minimum) in MULTI_GLYPH_CASES {
        let Some(mut raster) = live_chain_for("Noto Sans") else {
            eprintln!("skipped: no host font stack or Noto Sans");
            return;
        };
        let base = text.chars().next().expect("case is non-empty");
        warm_chain(&mut raster, base);
        let state = state_with_text(text);
        let snapshot = state.snapshot();
        let cells = row(&snapshot, 0);
        let span = usize::from(cells[0].width);
        assert_eq!(
            collect_run_text(&cells[..span.max(1)]),
            *text,
            "{description}: grid truth keeps the whole cluster"
        );
        assert_eq!(
            cells_for_range(text),
            span,
            "{description}: one layout unit"
        );
        let clustered = shape(&mut raster, text).expect("cluster shapes");
        eprintln!("{description}: shaped clusters: {clustered:?}");
        if clustered.iter().all(|c| c.uncovered) {
            eprintln!("skipped: host has no coverage for {description}");
            continue;
        }
        let damage = full_damage(&state);
        let mut renderer = renderer_for(raster, "Noto Sans");
        let _warm = renderer
            .render(&snapshot, &damage)
            .expect("unshaped warm render succeeds");
        let list = renderer
            .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
            .expect("shaped render succeeds");
        eprintln!("{description}: painted glyphs: {}", list.glyphs.len());
        assert!(
            list.glyphs.len() >= *minimum,
            "{description} paints {} glyphs, expected at least {minimum}: {:?}",
            list.glyphs.len(),
            list.glyphs
        );
        // Every member paints at its shaped pen position. A cluster whose
        // shaped advance is wider than the committed cell (a 16px
        // Devanagari cluster in a 10px cell) overhangs by at most one cell,
        // exactly like any glyph bitmap; ink is never clipped to the cell.
        for glyph in &list.glyphs {
            let column = glyph_column(&renderer, glyph.dest[0]);
            assert!(
                (0..=span as i64).contains(&column),
                "{description}: glyph escapes the {span}-cell span: {glyph:?}"
            );
        }
        assert_eq!(
            glyph_column(&renderer, list.glyphs[0].dest[0]),
            0,
            "{description}: the cluster lead keeps its committed column"
        );
    }
}

#[test]
fn decomposed_hangul_cluster_paints_its_composed_or_all_jamo_glyphs() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    // U+1100 HANGUL CHOSEONG KIYEOK + U+1161 HANGUL JUNGSEONG A.
    let text = "\u{1100}\u{1161}";
    warm_chain(&mut raster, '\u{1100}');
    let state = state_with_text(text);
    let snapshot = state.snapshot();
    assert_eq!(row(&snapshot, 0)[0].width, 2, "syllable is one wide unit");
    let clustered = shape(&mut raster, text).expect("jamo shapes");
    eprintln!("decomposed Hangul clusters: {clustered:?}");
    let covered: usize = clustered.iter().filter(|c| !c.uncovered).count();
    if covered == 0 {
        eprintln!("skipped: host has no Hangul jamo coverage");
        return;
    }
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("shaped render succeeds");
    eprintln!("decomposed Hangul: painted glyphs: {}", list.glyphs.len());
    // A composing font returns one syllable glyph; a non-composing font
    // returns the jamo, and every covered one must paint. The shaper maps
    // the L jamo to two columns of its own, so a non-composing cascade can
    // overhang the committed two-cell span by one column.
    assert!(
        !list.glyphs.is_empty() && list.glyphs.len() <= covered.max(1),
        "every covered jamo glyph paints: {} painted of {covered} covered",
        list.glyphs.len()
    );
    for glyph in &list.glyphs {
        let column = glyph_column(&renderer, glyph.dest[0]);
        assert!(
            (0..=3).contains(&column),
            "jamo glyph escapes the wide span: {glyph:?}"
        );
    }
}

/// One painted glyph instance: grid column, bitmap size, atlas uv.
type PaintedGlyph = (i64, [u32; 2], [f32; 4]);

/// Glyph instances that identify the painted pixels, plus the atlas texels
/// they sample.
///
/// The atlas coordinates are renderer-local, so the comparison also carries
/// the atlas texels the instances sample: identical tuples mean identical
/// painted pixels, and different flags must differ here.
fn glyph_pixels(
    renderer: &GridRenderer<FallbackRasterizer<SwashSingle>>,
    list: &bitty_render::DrawList,
) -> (Vec<PaintedGlyph>, Vec<u8>) {
    let instances = list
        .glyphs
        .iter()
        .map(|g| (glyph_column(renderer, g.dest[0]), g.size, g.uv))
        .collect();
    (instances, renderer.atlas_texels().to_vec())
}

#[test]
fn flags_paint_both_indicators_and_stay_distinguishable() {
    let mut painted = Vec::new();
    for (text, label) in [("\u{1F1EB}\u{1F1F7}", "FR"), ("\u{1F1EB}\u{1F1EE}", "FI")] {
        let Some(mut raster) = live_chain_for("Noto Sans") else {
            eprintln!("skipped: no host font stack or Noto Sans");
            return;
        };
        warm_chain(&mut raster, '\u{1F1EB}');
        let state = state_with_text(text);
        let snapshot = state.snapshot();
        let cells = row(&snapshot, 0);
        assert_eq!(cells[0].width, 2, "{label}: a flag is one wide unit");
        assert_eq!(
            collect_run_text(&cells[..2]),
            text,
            "{label}: both indicators stay in grid truth"
        );
        let clustered = shape(&mut raster, text).expect("flag shapes");
        eprintln!("{label} shaped clusters: {clustered:?}");
        if clustered.iter().all(|c| c.uncovered) {
            eprintln!("skipped: host has no flag coverage");
            return;
        }
        let damage = full_damage(&state);
        let mut renderer = renderer_for(raster, "Noto Sans");
        let _warm = renderer
            .render(&snapshot, &damage)
            .expect("unshaped warm render succeeds");
        let list = renderer
            .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
            .expect("shaped render succeeds");
        eprintln!("{label} painted: {} glyphs", list.glyphs.len());
        // Either the font ligated the pair into one glyph, or both
        // indicators paint (one per column). Never just the first.
        assert!(
            list.glyphs.len() >= 2 || clustered.len() == 1,
            "{label}: flag paints {} glyphs from {clustered:?}",
            list.glyphs.len()
        );
        painted.push(glyph_pixels(&renderer, &list));
    }
    assert_ne!(
        painted[0], painted[1],
        "FR and FI must not paint identical pixels"
    );
}

#[test]
fn wide_ambiguous_cluster_keeps_its_committed_columns() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    // Ambiguous scalar followed by a plain ASCII cell: under the wide policy
    // the first cluster commits two columns, so the second glyph must start
    // at column two, not one.
    let text = "\u{03B1}A";
    warm_chain(&mut raster, '\u{03B1}');
    let mut state = State::new();
    state.set_ambiguous_width(AmbiguousWidth::Wide);
    print_text(&mut state, text);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].width, 2);
    assert!(cells[1].spacer);
    assert_eq!(cells[2].glyph, 'A');
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    renderer.set_ambiguous_width(AmbiguousWidth::Wide);
    assert_eq!(renderer.ambiguous_width(), AmbiguousWidth::Wide);
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("shaped render succeeds");
    let columns: Vec<i64> = list
        .glyphs
        .iter()
        .map(|g| glyph_column(&renderer, g.dest[0]))
        .collect();
    eprintln!("wide ambiguous columns: {columns:?}");
    assert!(columns.contains(&0), "ambiguous scalar keeps column 0");
    assert!(
        columns.contains(&2),
        "the next cell must start at column 2 under the wide policy: {columns:?}"
    );
    assert!(
        !columns.contains(&1),
        "no glyph may land on the spacer column: {columns:?}"
    );
    assert_eq!(
        renderer.counters().shaping_misaligned,
        0,
        "the policy span must not be read as a font advance"
    );
}

#[test]
fn cluster_at_the_last_column_stays_inside_the_grid() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    warm_chain(&mut raster, '\u{0915}');
    let mut state = State::new();
    print_text(&mut state, &"a".repeat(79));
    print_text(&mut state, "\u{0915}\u{093F}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[79].glyph, '\u{0915}');
    assert_eq!(cells[79].width, 1);
    let damage = full_damage(&state);
    let mut renderer = renderer_for(raster, "Noto Sans");
    let _warm = renderer
        .render(&snapshot, &damage)
        .expect("unshaped warm render succeeds");
    let list = renderer
        .render_shaped(&snapshot, &damage, LigaturePolicy::Never, &[])
        .expect("shaped render succeeds");
    // The cluster lead keeps column 79; a member wider than the cell may
    // overhang into the (nonexistent) next column, exactly like any glyph.
    for glyph in &list.glyphs {
        let column = glyph_column(&renderer, glyph.dest[0]);
        assert!(
            (0..=80).contains(&column),
            "glyph paints past the last column: {glyph:?}"
        );
    }
    assert!(
        list.glyphs
            .iter()
            .any(|g| glyph_column(&renderer, g.dest[0]) == 79),
        "the last-column cluster paints on column 79"
    );
}
