//! Phase C hardening acceptance: partial-damage repaint, cluster grouping,
//! GPOS offsets, concealment, kill-switch, negative inputs, atlas probe
//! (CTX-0959, issue #1666).
//!
//! Deterministic tests run everywhere; shaped-emission tests carry the
//! live-font gate `BITTY_RENDER_FONT_TESTS=1` like the Phase B suite.
//!
//! ```text
//! BITTY_RENDER_FONT_TESTS=1 cargo test -p bitty-render --test shaped_phase_c
//! ```

use bitty_config::types::LigaturePolicy;
use bitty_render::glyph::{BitmapFormat, GlyphMetrics};
use bitty_render::grid::GlyphSource;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphAtlas, GlyphBitmap,
    GlyphRasterizer, MAX_SHAPED_GLYPH_CACHE_ENTRIES, RunAttrs, ShapedGlyphKey, SwashSingle,
};
use bitty_term_state::{
    Attribute, AttributeChange, AttributeDiff, Damage, DamageRect, DamagedRegion, State,
    TerminalAction,
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

fn print_action(c: char) -> TerminalAction {
    TerminalAction::Print(GraphemeCell::from(c))
}

fn state_from(script: &[TerminalAction]) -> State {
    let mut state = State::new();
    for action in script {
        state.apply(action);
    }
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

// ---------------------------------------------------------------------------
// Negative inputs: fail-closed, never a panic or frame error (no fonts).
// ---------------------------------------------------------------------------

#[test]
fn shape_run_rejects_invalid_point_size() {
    let mut shaper = SwashSingle::new().expect("shaper builds headless");
    let empty_chain: Vec<FontId> = Vec::new();
    for bad in [0.0, -12.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let attrs = RunAttrs {
            features: Vec::new(),
            point_size: bad,
        };
        let err = shaper
            .shape_run("hello", &empty_chain, &attrs)
            .expect_err("non-positive/non-finite size must fail");
        assert!(
            matches!(err, bitty_render::RenderError::InvalidInput { .. }),
            "size {bad:?} must report InvalidInput, got {err:?}"
        );
    }
}

#[test]
fn shape_run_rejects_empty_chain_without_panicking() {
    let mut shaper = SwashSingle::new().expect("shaper builds headless");
    let empty_chain: Vec<FontId> = Vec::new();
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
    };
    // Adversarial corpus: empty, NUL, lone mark, Zalgo, mixed scripts,
    // regional indicators, overlong rows. Every input fails closed with
    // UnknownFontHandle — the grid degrades these runs to unshaped.
    let corpus = [
        "",
        "\0",
        "\u{301}",
        "e\u{301}\u{302}\u{303}\u{304}\u{305}",
        "ﬁ\0🇺🇸",
        "مرحبا",
        "漢字",
        "\u{202E}reversed",
        &"a".repeat(100_000),
    ];
    for text in corpus {
        let err = shaper
            .shape_run(text, &empty_chain, &attrs)
            .expect_err("empty chain must fail without panicking");
        assert!(
            matches!(err, bitty_render::RenderError::UnknownFontHandle),
            "empty chain must report UnknownFontHandle, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Atlas occupancy probe at the shaped-slot bound (Phase B risk item).
// ---------------------------------------------------------------------------

fn probe_bitmap() -> GlyphBitmap {
    GlyphBitmap::try_new(
        GlyphMetrics {
            left: 0,
            top: 6,
            width: 8,
            height: 8,
            advance: [8, 0],
        },
        BitmapFormat::Rgb,
        vec![0xAA; 8 * 8 * 3],
    )
    .expect("probe bitmap builds")
}

#[test]
fn atlas_probe_holds_2048_shaped_slots() {
    // The probe count tracks the rasterizer bound exactly: if
    // MAX_SHAPED_GLYPH_CACHE_ENTRIES moves, this test moves with it.
    assert_eq!(
        MAX_SHAPED_GLYPH_CACHE_ENTRIES, 2048,
        "probe assumes the 2048 shaped-glyph bound"
    );
    let mut atlas = GlyphAtlas::new(2048).expect("atlas builds");
    let face = FontId::next(&mut 0);
    let mut placed = 0usize;
    for id in 0..MAX_SHAPED_GLYPH_CACHE_ENTRIES {
        let key = ShapedGlyphKey::new(face, id as u16, POINT_SIZE, &[]);
        match atlas.ensure_shaped(key, &probe_bitmap()) {
            GlyphSource::Atlas { .. } => placed += 1,
            GlyphSource::Inline { .. } => {}
        }
    }
    assert_eq!(
        placed, MAX_SHAPED_GLYPH_CACHE_ENTRIES,
        "2048 small shaped glyphs must all place without exhaustion"
    );
    assert_eq!(atlas.len(), MAX_SHAPED_GLYPH_CACHE_ENTRIES);
    assert!(!atlas.is_exhausted());
    let occupancy = atlas.occupancy();
    assert!(
        occupancy > 0.0 && occupancy < 1.0,
        "occupancy must stay interior, got {occupancy}"
    );
    // Re-placing served keys hits without growing the placement set.
    let hits_before = atlas.hits();
    for id in 0..10u16 {
        let key = ShapedGlyphKey::new(face, id, POINT_SIZE, &[]);
        assert!(matches!(
            atlas.ensure_shaped(key, &probe_bitmap()),
            GlyphSource::Atlas { .. }
        ));
    }
    assert_eq!(atlas.hits(), hits_before + 10);
    assert_eq!(atlas.len(), MAX_SHAPED_GLYPH_CACHE_ENTRIES);
}

#[test]
fn atlas_shaped_paths_degrade_without_exhaustion_or_inline_confusion() {
    // Oversized bitmaps fall back inline *without* flagging exhaustion (a
    // reset could never fit them).
    let mut atlas = GlyphAtlas::new(2048).expect("atlas builds");
    let face = FontId::next(&mut 0);
    let huge = GlyphBitmap::try_new(
        GlyphMetrics {
            left: 0,
            top: 0,
            width: 4096,
            height: 8,
            advance: [8, 0],
        },
        BitmapFormat::Rgb,
        vec![0xAA; 4096 * 8 * 3],
    )
    .expect("oversized bitmap builds");
    let key = ShapedGlyphKey::new(face, 1, POINT_SIZE, &[]);
    assert!(matches!(
        atlas.ensure_shaped(key, &huge),
        GlyphSource::Inline { .. }
    ));
    assert!(!atlas.is_exhausted(), "oversize must not flag exhaustion");
    // A saturated tiny atlas flags exhaustion and serves inline instead.
    let mut tiny = GlyphAtlas::new(8).expect("tiny atlas builds");
    let first = ShapedGlyphKey::new(face, 1, POINT_SIZE, &[]);
    assert!(matches!(
        tiny.ensure_shaped(first, &probe_bitmap()),
        GlyphSource::Atlas { .. }
    ));
    let second = ShapedGlyphKey::new(face, 2, POINT_SIZE, &[]);
    assert!(matches!(
        tiny.ensure_shaped(second, &probe_bitmap()),
        GlyphSource::Inline { .. }
    ));
    assert!(tiny.is_exhausted(), "saturated atlas must flag reset");
}

// ---------------------------------------------------------------------------
// Live-font Phase C acceptance.
// ---------------------------------------------------------------------------

#[test]
fn grouped_emission_keeps_ligature_spans_and_truth() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let state = state_from(&[print_action('f'), print_action('i'), print_action('a')]);
    let snap_before = state.snapshot();
    let damage = full_damage(&state);
    let shaped = renderer
        .render_shaped(&snap_before, &damage, LigaturePolicy::Never, &[])
        .expect("shaped renders");
    // Grouped emission preserves the Phase B merge: `fi` is one span.
    assert!(
        shaped.glyphs.len() < 3,
        "ligature must still merge: {}",
        shaped.glyphs.len()
    );
    assert_eq!(
        state.snapshot(),
        snap_before,
        "grid truth untouched by grouped emission"
    );
}

#[test]
fn partial_damage_repaints_emitted_union() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let cell = CellMetrics::new(10, 22).unwrap();
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, cell).expect("renderer builds");
    let state = state_from(&[print_action('f'), print_action('i'), print_action('a')]);
    let snap = state.snapshot();
    // Damage covers columns 1.. (the ligature's second cell onward) but
    // not column 0: the `fi` span at columns 0..2 crosses the dirty edge.
    let damage = Damage {
        generation: snap.generation,
        regions: vec![DamagedRegion::Grid(DamageRect {
            top: 0,
            left: 1,
            bottom: 0,
            right: snap.width as u16 - 1,
        })]
        .into_boxed_slice(),
    };
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("partial shaped renders");
    // The ligature glyph still emits at column 0 (full-row runs).
    assert!(
        shaped.glyphs.iter().any(|g| g.dest[0] == 0),
        "ligature must emit at its span origin: {:?}",
        shaped.glyphs.iter().map(|g| g.dest).collect::<Vec<_>>()
    );
    // And the background repaint covers column 0 despite the dirty edge.
    assert!(
        shaped.fills.iter().any(|f| f.rect.x == 0 && f.rect.y == 0),
        "union repaint must cover column 0: {:?}",
        shaped
            .fills
            .iter()
            .map(|f| (f.rect.x, f.rect.y, f.rect.width))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        state.snapshot(),
        snap,
        "grid truth untouched by partial repaint"
    );
}

#[test]
fn invisible_runs_emit_no_glyphs_on_either_path() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let state = state_from(&[
        TerminalAction::SetAttributes {
            attrs: AttributeDiff {
                changes: [AttributeChange::Enable(Attribute::Invisible)]
                    .into_iter()
                    .collect(),
            },
        },
        print_action('x'),
        print_action('y'),
    ]);
    let snap = state.snapshot();
    let damage = full_damage(&state);
    let plain = renderer.render(&snap, &damage).expect("plain renders");
    assert!(
        plain.glyphs.is_empty(),
        "unshaped path conceals invisible cells"
    );
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("shaped renders");
    assert!(
        shaped.glyphs.is_empty(),
        "shaped path must conceal invisible cells, got {:?}",
        shaped.glyphs.len()
    );
    assert!(
        !shaped.fills.is_empty(),
        "backgrounds stay painted under concealment"
    );
    assert_eq!(
        state.snapshot(),
        snap,
        "grid truth untouched by concealment"
    );
}

#[test]
fn always_kill_switch_matches_unshaped_placement() {
    let Some(raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let state = state_from(&[print_action('a'), print_action('b'), print_action('c')]);
    let snap = state.snapshot();
    let damage = full_damage(&state);
    let plain = renderer.render(&snap, &damage).expect("plain renders");
    let always = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Always, &[])
        .expect("Always renders");
    assert_eq!(
        always.glyphs.len(),
        plain.glyphs.len(),
        "Always must emit per-cell like the unshaped path"
    );
    for (shaped_glyph, plain_glyph) in always.glyphs.iter().zip(plain.glyphs.iter()) {
        assert_eq!(
            shaped_glyph.dest, plain_glyph.dest,
            "Always dest must match unshaped"
        );
        assert_eq!(
            shaped_glyph.size, plain_glyph.size,
            "Always size must match unshaped"
        );
        assert_eq!(
            shaped_glyph.color, plain_glyph.color,
            "Always tint must match unshaped"
        );
    }
    assert_eq!(
        always.fills.len(),
        plain.fills.len(),
        "Always backgrounds must match unshaped"
    );
    for (shaped_fill, plain_fill) in always.fills.iter().zip(plain.fills.iter()) {
        assert_eq!(shaped_fill.rect, plain_fill.rect);
        assert_eq!(shaped_fill.color, plain_fill.color);
    }
    assert_eq!(
        state.snapshot(),
        snap,
        "grid truth untouched by the kill-switch path"
    );
}

#[test]
fn combining_sequence_renders_without_splitting_truth() {
    let Some(mut raster) = live_chain_for("Noto Sans") else {
        eprintln!("skipped: no host font stack or Noto Sans");
        return;
    };
    // Base + mark shapes without panicking; structural invariants hold for
    // one-glyph and multi-glyph clusters alike.
    let chain = raster.fonts().to_vec();
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
    };
    let text = "e\u{301}";
    let clusters = raster
        .inner_mut()
        .shape_run(text, &chain, &attrs)
        .expect("combining sequence shapes");
    assert!(!clusters.is_empty(), "combining sequence must shape");
    for cluster in &clusters {
        assert!(
            cluster.byte_offset <= text.len(),
            "cluster offset in range: {cluster:?}"
        );
        assert!(cluster.cells.1 >= 1, "cluster covers a cell: {cluster:?}");
    }
    // Grid row with a precomposed mark character renders and keeps truth.
    let query = query_for("Noto Sans");
    let mut renderer =
        bitty_render::GridRenderer::new(raster, &query, CellMetrics::new(10, 22).unwrap())
            .expect("renderer builds");
    let state = state_from(&[print_action('\u{e9}')]);
    let snap = state.snapshot();
    let damage = full_damage(&state);
    let shaped = renderer
        .render_shaped(&snap, &damage, LigaturePolicy::Never, &[])
        .expect("precomposed mark renders");
    assert!(
        shaped.glyphs.len() <= 2,
        "one cell emits at most its group: {}",
        shaped.glyphs.len()
    );
    assert_eq!(
        state.snapshot(),
        snap,
        "grid truth untouched by mark rendering"
    );
}
