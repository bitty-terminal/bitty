//! bitty#3021: a mixed-script fallback gap must resolve per cluster.
//!
//! Live report (0.0.36, every TUI): a line containing an emoji rendered
//! blank — the text and the block cursor vanished while the cells kept their
//! space. The line's shaped run was dropped wholesale, not one glyph: the
//! primary face covers neither the Han text nor the emoji, so every cluster
//! was uncovered and the fallback coalesced them into one gap. No single
//! face covers Han *and* emoji, so the whole gap stayed uncovered and the
//! grid emitted a missing-glyph box per cluster instead of a glyph.
//!
//! The reproduction needs the shaped stack: the deterministic headless
//! rasterizer has no shaping, and a fake rasterizer that covers every cell
//! cannot produce a mixed-coverage gap. These tests therefore drive
//! `SwashSingle` with the embedded symbols face as the primary (always
//! present, covers neither Han nor emoji) and skip cleanly when the host has
//! no face for one of the two scripts, so CI stays deterministic.
//!
//! ```text
//! cargo test -p bitty-render --test shaped_mixed_script_fallback
//! ```

use bitty_render::embedded::EMBEDDED_SYMBOLS_FAMILY;
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontQuery, FontStyle, GlyphRasterizer, GridRenderer, RunAttrs,
    RunDirection, ShapedRunSource, SwashSingle,
};
use bitty_term_state::{Damage, State, TerminalAction};
use bitty_vt::GraphemeCell;

const POINT_SIZE: f32 = 12.0;

fn query(family: &str) -> FontQuery {
    FontQuery {
        family: family.to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

/// Loads the embedded symbols face as the primary and returns the shaper
/// plus its font handle. `None` when the embedded asset cannot be loaded
/// (it is compiled in, so this is a hard failure of the crate, not a host
/// property).
fn embedded_primary() -> Option<(SwashSingle, bitty_render::FontId)> {
    let mut shaper = SwashSingle::new().ok()?;
    let id = shaper.load_font(&query(EMBEDDED_SYMBOLS_FAMILY)).ok()?;
    Some((shaper, id))
}

/// True when the host has a dynamic face for both `scalars`, i.e. the
/// per-cluster fallback this test asserts can actually resolve them.
///
/// `dynamic_face_for` only answers a face that carries a renderable source
/// for the scalar (the shaper's own coverage probe), so its `Some` is the
/// host-availability gate.
fn host_covers(shaper: &mut SwashSingle, scalars: &[char]) -> bool {
    scalars
        .iter()
        .all(|c| shaper.dynamic_face_for(*c, POINT_SIZE).is_some())
}

#[test]
fn mixed_han_and_emoji_gap_resolves_each_cluster() {
    let Some((mut shaper, primary)) = embedded_primary() else {
        eprintln!("skipped: embedded symbols face unavailable");
        return;
    };
    if !host_covers(&mut shaper, &['\u{4E2D}', '\u{1F600}']) {
        eprintln!("skipped: host has no dynamic face for Han + emoji");
        return;
    }
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
        direction: RunDirection::Ltr,
    };
    let clusters = ShapedRunSource::shape_run(&mut shaper, "\u{4E2D}\u{1F600}", &[primary], &attrs)
        .expect("shaping must not fail");
    assert_eq!(clusters.len(), 2, "one cluster per scalar");
    assert!(
        clusters.iter().all(|cluster| !cluster.uncovered),
        "a mixed-coverage gap resolves each cluster on its own face, got {:?}",
        clusters
            .iter()
            .map(|c| (c.byte_offset, c.face, c.uncovered))
            .collect::<Vec<_>>()
    );
    assert_ne!(
        clusters[0].face, clusters[1].face,
        "Han and emoji must come from different fallback faces"
    );
}

/// The reported line, end to end through the grid emitter: the Han text must
/// keep its glyphs when an emoji joins the same run. Before the fix every
/// cluster of the run was `uncovered`, so only the `>` prefix painted.
#[test]
fn han_row_with_a_trailing_emoji_keeps_its_glyphs() {
    // Calibration: the regression is "adding an emoji must not drop the Han
    // glyphs", so it only means something on a host whose font stack covers
    // the Han line at all. A host without a CJK face paints neither line and
    // must skip rather than fail (CI images vary).
    let Some((control, _)) = render_line("> 请问饿七五二二头痛医头用") else {
        return;
    };
    if control.glyphs_emitted < 13 {
        eprintln!(
            "skipped: host has no CJK face for the reported line ({} glyphs)",
            control.glyphs_emitted
        );
        return;
    }
    let Some((counters, list)) = render_line("> 请问饿七五二二头痛医头用\u{1F600}")
    else {
        return;
    };
    // 12 Han clusters (width 2 each) plus the `>`; the emoji is the only
    // cluster a host may legitimately lack a face for.
    assert!(
        counters.glyphs_emitted >= 13,
        "the Han text keeps its glyphs next to an emoji, got {}",
        counters.glyphs_emitted
    );
    assert!(
        counters.missing_glyphs <= 1,
        "only the emoji may be missing, got {} missing clusters",
        counters.missing_glyphs
    );
    assert_eq!(
        list.glyphs.len(),
        counters.glyphs_emitted as usize,
        "every emitted glyph reaches the draw list"
    );
    // The GPU path consumes the same draw list: the atlas plan must carry
    // every Han/ASCII quad, and the emoji must ride the color-atlas blits.
    let atlas_quads: usize = bitty_render::batch::chunk_atlas_glyphs(&list.glyphs, 800, 456, 1.0)
        .iter()
        .map(|chunk| chunk.quad_count)
        .sum();
    assert!(
        atlas_quads >= 13,
        "the GPU atlas plan carries every Han glyph, got {atlas_quads}"
    );
    assert!(
        list.glyphs.iter().any(|glyph| matches!(
            glyph.source,
            bitty_render::grid::GlyphSource::ColorAtlas { .. }
        )),
        "the emoji rides the color-atlas blit plan"
    );
}

/// A ZWJ sequence shapes as one multi-glyph cluster, which the coalescing
/// never merged into a mixed gap — it painted before the fix and must keep
/// painting after the per-cluster retry.
#[test]
fn han_row_with_a_zwj_emoji_keeps_its_glyphs() {
    let Some((counters, list)) =
        render_line("> 请问饿七五二二头痛医头用\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}")
    else {
        return;
    };
    assert!(
        counters.glyphs_emitted >= 13,
        "the Han text keeps its glyphs next to a ZWJ sequence, got {}",
        counters.glyphs_emitted
    );
    assert!(
        counters.missing_glyphs <= 1,
        "only the ZWJ sequence may be missing, got {} missing clusters",
        counters.missing_glyphs
    );
    assert_eq!(
        list.glyphs.len(),
        counters.glyphs_emitted as usize,
        "every emitted glyph reaches the draw list"
    );
}

/// Renders one prompt line through the production shaped grid emitter.
/// `None` when the host has no primary face (the test skips).
fn render_line(
    line: &str,
) -> Option<(
    bitty_render::grid::RenderCounters,
    bitty_render::grid::DrawList,
)> {
    let mut raster =
        FallbackRasterizer::with_default_chain(SwashSingle::new().expect("shaped stack"));
    let Ok(_) = raster.load_font(&query("JetBrainsMono Nerd Font")) else {
        eprintln!("skipped: JetBrainsMono Nerd Font unavailable");
        return None;
    };
    let cell = CellMetrics::new(9, 19).expect("cell metrics");
    let mut renderer =
        GridRenderer::new(raster, &query("JetBrainsMono Nerd Font"), cell).expect("renderer");
    let mut state = State::new();
    for ch in line.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
    }
    let snapshot = state.snapshot();
    let damage = Damage {
        generation: snapshot.generation,
        regions: state.damage_since(0).into_boxed_slice(),
    };
    let list = renderer
        .render_shaped_with_bidi(
            &snapshot,
            &damage,
            bitty_config::types::LigaturePolicy::Never,
            &[],
            None,
            false,
        )
        .expect("render");
    Some((renderer.counters(), list))
}
