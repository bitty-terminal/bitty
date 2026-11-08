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
//! present, covers neither Han nor emoji) and supply the Han side from the
//! vendored [`HAN_TEST_BYTES`] subset, so no assertion depends on the host
//! having a CJK face: a CI image without one must still fail the test when
//! the per-cluster retry regresses, not skip it. Only the emoji face stays a
//! host property, and only the emoji-specific assertions are gated on it.
//!
//! ```text
//! cargo test -p bitty-render --test shaped_mixed_script_fallback
//! ```

use bitty_render::embedded::{EMBEDDED_SYMBOLS_FAMILY, HAN_TEST_BYTES, HAN_TEST_FAMILY};
use bitty_render::grid::{DrawList, RenderCounters};
use bitty_render::{
    CellMetrics, FallbackRasterizer, FontId, FontQuery, FontStyle, GlyphRasterizer, GridRenderer,
    RunAttrs, RunDirection, ShapedRunSource, SwashSingle,
};
use bitty_term_state::{Damage, State, TerminalAction};
use bitty_vt::GraphemeCell;

const POINT_SIZE: f32 = 12.0;

/// The reported line: `>` plus 12 Han clusters (width 2 each), so a fully
/// painted row is 13 emitted glyphs.
const HAN_LINE: &str = "> 请问饿七五二二头痛医头用";

fn query(family: &str) -> FontQuery {
    FontQuery {
        family: family.to_string(),
        style: FontStyle::Normal,
        point_size: POINT_SIZE,
    }
}

/// Builds a shaper over the host stack plus the vendored Han subset
/// (bitty#3021), so every Han scalar of the reported line resolves without a
/// host CJK face. The fixture is only reachable through the per-scalar
/// fallback walk, exactly like a real CJK face.
///
/// `None` only when the host exposes no text face at all (a bare image with
/// no font stack); the fixture itself is compiled in.
fn shaper_with_han_fixture() -> Option<SwashSingle> {
    SwashSingle::with_extra_faces(&[HAN_TEST_BYTES]).ok()
}

/// Loads the embedded symbols face as the primary and returns the shaper
/// plus its font handle. `None` when the embedded asset cannot be loaded
/// (it is compiled in, so this is a hard failure of the crate, not a host
/// property).
fn embedded_primary() -> Option<(SwashSingle, FontId)> {
    let mut shaper = shaper_with_han_fixture()?;
    let id = shaper.load_font(&query(EMBEDDED_SYMBOLS_FAMILY)).ok()?;
    Some((shaper, id))
}

/// True when the host has a dynamic face for `c`, i.e. the per-cluster
/// fallback can resolve it beyond the pinned chain and the fixture.
///
/// Used only for the emoji scalar: Han coverage is vendored, emoji coverage
/// is not, and the emoji half may legitimately be absent.
fn host_covers(shaper: &mut SwashSingle, c: char) -> bool {
    shaper.dynamic_face_for(c, POINT_SIZE).is_some()
}

#[test]
fn mixed_han_and_emoji_gap_resolves_each_cluster() {
    let Some((mut shaper, primary)) = embedded_primary() else {
        panic!("the shaped stack must build on any host (fixture is compiled in)");
    };
    let attrs = RunAttrs {
        features: Vec::new(),
        point_size: POINT_SIZE,
        direction: RunDirection::Ltr,
    };
    // `请` comes from the vendored subset, `😀` only from a host emoji face:
    // a gap the primary covers neither side of.
    let clusters = ShapedRunSource::shape_run(&mut shaper, "请\u{1F600}", &[primary], &attrs)
        .expect("shaping must not fail");
    assert_eq!(clusters.len(), 2, "one cluster per scalar");
    // Host-independent: the Han cluster resolves from the fixture, on every
    // image, because that is the reported bug.
    assert!(
        !clusters[0].uncovered,
        "the Han cluster resolves even with no host CJK face, got {:?}",
        (
            clusters[0].byte_offset,
            clusters[0].face,
            clusters[0].uncovered
        )
    );
    assert_eq!(
        shaper.loaded_face_family(clusters[0].face).as_deref(),
        Some(HAN_TEST_FAMILY),
        "the Han cluster must come from the vendored subset"
    );
    if !host_covers(&mut shaper, '\u{1F600}') {
        eprintln!(
            "note: host has no emoji face; the Han cluster still resolved from \
             {HAN_TEST_FAMILY}, only the cross-script pairing is unchecked"
        );
        return;
    }
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
    // No host calibration: the Han face is the vendored subset, so a CI
    // image without a CJK face must produce the same 13 glyphs as a
    // font-rich developer seat.
    let rendered =
        render_line(&format!("{HAN_LINE}\u{1F600}")).expect("the shaped renderer builds");
    // 12 Han clusters (width 2 each) plus the `>`; the emoji is the only
    // cluster a host may legitimately lack a face for.
    assert!(
        rendered.counters.glyphs_emitted >= 13,
        "the Han text keeps its glyphs next to an emoji, got {}",
        rendered.counters.glyphs_emitted
    );
    assert!(
        rendered.counters.missing_glyphs <= 1,
        "only the emoji may be missing, got {} missing clusters",
        rendered.counters.missing_glyphs
    );
    assert_eq!(
        rendered.list.glyphs.len(),
        rendered.counters.glyphs_emitted as usize,
        "every emitted glyph reaches the draw list"
    );
    // The GPU path consumes the same draw list: the atlas plan must carry
    // every Han/ASCII quad, and the emoji must ride the color-atlas blits.
    let atlas_quads: usize =
        bitty_render::batch::chunk_atlas_glyphs(&rendered.list.glyphs, 800, 456, 1.0)
            .iter()
            .map(|chunk| chunk.quad_count)
            .sum();
    assert!(
        atlas_quads >= 13,
        "the GPU atlas plan carries every Han glyph, got {atlas_quads}"
    );
    if !rendered.emoji_face {
        eprintln!("note: host has no emoji face; the Han quads are still asserted");
        return;
    }
    assert!(
        rendered.list.glyphs.iter().any(|glyph| matches!(
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
    let rendered = render_line(&format!(
        "{HAN_LINE}\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"
    ))
    .expect("the shaped renderer builds");
    assert!(
        rendered.counters.glyphs_emitted >= 13,
        "the Han text keeps its glyphs next to a ZWJ sequence, got {}",
        rendered.counters.glyphs_emitted
    );
    assert!(
        rendered.counters.missing_glyphs <= 1,
        "only the ZWJ sequence may be missing, got {} missing clusters",
        rendered.counters.missing_glyphs
    );
    assert_eq!(
        rendered.list.glyphs.len(),
        rendered.counters.glyphs_emitted as usize,
        "every emitted glyph reaches the draw list"
    );
}

/// One rendered prompt line plus the host facts the assertions may gate on.
struct Rendered {
    counters: RenderCounters,
    list: DrawList,
    /// True when the host resolved the emoji scalar to a real face. Han
    /// coverage never appears here: it is vendored.
    emoji_face: bool,
}

/// Renders one prompt line through the production shaped grid emitter.
///
/// The primary is the embedded symbols face, which covers neither Han nor
/// emoji, so the run always contains a mixed-coverage gap and the per-cluster
/// retry is what the counters measure. `None` only when the host has no text
/// face at all.
fn render_line(line: &str) -> Option<Rendered> {
    let mut raster = FallbackRasterizer::with_default_chain(
        SwashSingle::with_extra_faces(&[HAN_TEST_BYTES]).ok()?,
    );
    // Primary: covers neither Han nor emoji, so the run always carries a
    // mixed-coverage gap and the per-cluster retry is what the counters
    // measure.
    if raster.load_font(&query(EMBEDDED_SYMBOLS_FAMILY)).is_err() {
        eprintln!("skipped: embedded symbols face unavailable");
        return None;
    }
    // Probe the emoji before the renderer takes the rasterizer: it is the
    // one host property these assertions may be gated on.
    let emoji_face = raster.dynamic_face_for('\u{1F600}', POINT_SIZE).is_some();
    let cell = CellMetrics::new(9, 19).expect("cell metrics");
    let mut renderer =
        GridRenderer::new(raster, &query(EMBEDDED_SYMBOLS_FAMILY), cell).expect("renderer");
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
    Some(Rendered {
        counters: renderer.counters(),
        list,
        emoji_face,
    })
}
