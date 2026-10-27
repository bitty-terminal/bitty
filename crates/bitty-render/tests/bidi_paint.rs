//! Paint-time visual order for BiDi rows (CTX-1612, #1766).
//!
//! Terminal Truth stays logical: these tests render the same snapshot with
//! reordering off and on and prove that (a) the off path is byte-identical
//! to the historical output, (b) an RTL row paints its clusters at the
//! columns `BidiGrid` assigns, and (c) the snapshot/hash never change.
//! The rasterizer is deterministic and identifies a glyph by its bitmap
//! height, so assertions can name the scalar that landed at a column.

use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, DrawList, GridRenderer};
use bitty_render::{
    FallbackRasterizer, RunAttrs, ShapedCluster, ShapedRunSource, glyph::ShapedBitmap,
};
use bitty_term_state::{Damage, DamageRect, DamagedRegion, Snapshot, State, TerminalAction};
use bitty_vt::GraphemeCell;

/// Every non-space scalar rasterizes to a 1x(codepoint) bitmap so
/// `GlyphInstance::size[1]` identifies the painted scalar.
#[derive(Debug, Default)]
struct TagRasterizer {
    next: u64,
}

impl ShapedRunSource for TagRasterizer {
    fn shaping_supported(&self) -> bool {
        false
    }

    fn shape_run(
        &mut self,
        _text: &str,
        _chain: &[FontId],
        _attrs: &RunAttrs,
    ) -> Result<Vec<ShapedCluster>, RenderError> {
        Err(RenderError::InvalidInput {
            reason: "deterministic test rasterizer does not shape",
        })
    }

    fn rasterize_shaped_cached_ref(
        &mut self,
        _face: FontId,
        _glyph_id: u16,
        _point_size: f32,
        _features: &[bitty_config::types::OpenTypeFeature],
    ) -> Result<Option<ShapedBitmap<'_>>, RenderError> {
        Ok(None)
    }

    fn note_misaligned(&mut self) {}

    fn shape_stats(&self) -> (u64, u64) {
        (0, 0)
    }

    fn face_is_monospaced(&self, _face: FontId) -> bool {
        false
    }
}

impl GlyphRasterizer for TagRasterizer {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        let height = u32::from(key.character).max(1);
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: height as i32,
                    width: 1,
                    height: height as i32,
                    advance: [8, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; height as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn renderer() -> GridRenderer<TagRasterizer> {
    let query = FontQuery {
        family: "Tag".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(
        TagRasterizer::default(),
        &query,
        CellMetrics::new(8, 32).unwrap(),
    )
    .unwrap()
}

fn shaped_renderer() -> GridRenderer<FallbackRasterizer<TagRasterizer>> {
    let query = FontQuery {
        family: "Tag".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(
        FallbackRasterizer::with_default_chain(TagRasterizer::default()),
        &query,
        CellMetrics::new(8, 32).unwrap(),
    )
    .unwrap()
}

fn full_damage(snapshot: &Snapshot) -> Damage {
    Damage {
        generation: snapshot.generation,
        regions: vec![DamagedRegion::Grid(DamageRect::full(
            snapshot.height as u16,
            snapshot.width as u16,
        ))]
        .into_boxed_slice(),
    }
}

fn state_with(text: &str) -> State {
    let mut state = State::new();
    for scalar in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(scalar)));
    }
    state
}

/// Painted `(scalar, cell column)` pairs for glyphs whose scalar is tagged.
fn painted_cells(list: &DrawList) -> Vec<(char, usize)> {
    list.glyphs
        .iter()
        .filter_map(|glyph| {
            let scalar = char::from_u32(glyph.size[1])?;
            let col = usize::try_from(glyph.dest[0]).ok()? / 8;
            Some((scalar, col))
        })
        .collect()
}

#[test]
fn bidi_off_paints_logical_columns_and_is_byte_identical() {
    let state = state_with("abc אבג");
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = renderer();
    let baseline = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    renderer.set_bidi(false);
    let again = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    assert_eq!(baseline.glyphs, again.glyphs);
    assert_eq!(baseline.fills, again.fills);
    let painted = painted_cells(&baseline);
    assert!(painted.contains(&('א', 4)), "{painted:?}");
    assert!(painted.contains(&('ב', 5)), "{painted:?}");
    assert!(painted.contains(&('ג', 6)), "{painted:?}");
}

#[test]
fn bidi_on_paints_the_rtl_run_in_visual_order() {
    let state = state_with("abc אבג");
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = renderer();
    renderer.set_bidi(true);
    assert!(renderer.bidi_enabled());
    let list = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    let painted = painted_cells(&list);
    // Latin stays logical; the Hebrew run reverses in place.
    assert!(painted.contains(&('a', 0)), "{painted:?}");
    assert!(painted.contains(&('b', 1)), "{painted:?}");
    assert!(painted.contains(&('c', 2)), "{painted:?}");
    assert!(painted.contains(&('א', 6)), "{painted:?}");
    assert!(painted.contains(&('ב', 5)), "{painted:?}");
    assert!(painted.contains(&('ג', 4)), "{painted:?}");
    // The snapshot itself is untouched.
    let after = state.snapshot();
    assert_eq!(snapshot.cells, after.cells);
}

#[test]
fn bidi_on_pure_ltr_screen_changes_nothing() {
    let state = state_with("plain ascii");
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = renderer();
    renderer.set_bidi(false);
    let logical = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    renderer.set_bidi(true);
    let visual = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    assert_eq!(logical.glyphs, visual.glyphs);
    assert_eq!(logical.fills, visual.fills);
    assert_eq!(logical.backgrounds, visual.backgrounds);
}

#[test]
fn wide_rtl_pair_keeps_its_spacer_next_to_the_lead() {
    let state = state_with("א中ב");
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = renderer();
    renderer.set_bidi(true);
    let list = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    let painted = painted_cells(&list);
    assert!(painted.contains(&('א', 3)), "{painted:?}");
    assert!(painted.contains(&('中', 1)), "{painted:?}");
    assert!(painted.contains(&('ב', 0)), "{painted:?}");
    // The spacer never paints a glyph; the lead's background run is merged
    // with neighbouring same-color cells, so the atomicity assertion lives
    // in the term-state map test. Here the painted lead column is what
    // proves the pair moved together.
    assert!(
        painted.iter().filter(|(scalar, _)| *scalar == '中').count() == 1,
        "one wide glyph: {painted:?}"
    );
}

#[test]
fn rendering_leaves_the_canonical_hash_unchanged() {
    let state = state_with("אבג abc");
    let hash = state.state_hash();
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = renderer();
    renderer.set_bidi(true);
    let _ = renderer
        .render_with_wraps(&snapshot, &damage, None)
        .expect("render");
    assert_eq!(hash, state.state_hash());
    assert_eq!(
        bitty_term_state::canonical_public::CANONICAL_HASH_VERSION,
        13
    );
}

#[test]
fn shaped_entry_point_falls_back_to_the_visual_per_cell_path() {
    // CTX-1612: the deterministic rasterizer cannot shape, so the shaped
    // entry point must delegate to the per-cell path with the same BiDi
    // decision. This is the shipped RTL fallback (shaped RTL pen placement
    // is deferred), and it must still paint the Hebrew run in visual order.
    let state = state_with("abc אבג");
    let snapshot = state.snapshot();
    let damage = full_damage(&snapshot);
    let mut renderer = shaped_renderer();
    renderer.set_bidi(true);
    let list = renderer
        .render_shaped(
            &snapshot,
            &damage,
            bitty_config::types::LigaturePolicy::Cursor,
            &[],
        )
        .expect("shaped entry point");
    let painted = painted_cells(&list);
    assert!(painted.contains(&('א', 6)), "{painted:?}");
    assert!(painted.contains(&('ב', 5)), "{painted:?}");
    assert!(painted.contains(&('ג', 4)), "{painted:?}");
}
