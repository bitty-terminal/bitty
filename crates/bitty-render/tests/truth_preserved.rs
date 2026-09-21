//! SEC-05 (R-008 / P0-AC-016): presentation never rewrites terminal truth.
//!
//! The grid pipeline reads only the public `Snapshot`/`Damage` surface of
//! `bitty-term-state` (ADR-0003 dependency rule 3). These tests prove the
//! direction holds at runtime: rendering a snapshot — including repeated
//! frames — leaves the canonical hash and the snapshot bytes untouched, and
//! tampering with a handed-out snapshot copy cannot propagate back into
//! live state.

use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, GridRenderer};

#[derive(Debug)]
struct FakeR {
    next: u64,
}

impl GlyphRasterizer for FakeR {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, k: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if k.character == ' ' {
            return Ok(None);
        }
        let side = (u32::from(k.character) % 3 + 6) as i32;
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side,
                    height: side,
                    advance: [side, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn fake_renderer() -> GridRenderer<FakeR> {
    let query = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(FakeR { next: 0 }, &query, CellMetrics::new(8, 16).unwrap()).unwrap()
}

fn scripted_state() -> bitty_term_state::State {
    let mut state = bitty_term_state::State::new();
    for ch in "hello, truth".chars() {
        state.apply(&bitty_term_state::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(ch),
        ));
    }
    state
}

fn damage_for(state: &bitty_term_state::State) -> bitty_term_state::Damage {
    let snapshot = state.snapshot();
    bitty_term_state::Damage {
        generation: snapshot.generation,
        regions: state.damage_since(0).into_boxed_slice(),
    }
}

#[test]
fn render_leaves_state_hash_and_snapshot_untouched() {
    let state = scripted_state();
    let hash_before = state.state_hash();
    let snapshot_before = state.snapshot();

    let mut renderer = fake_renderer();
    let damage = damage_for(&state);
    let list = renderer
        .render(&snapshot_before, &damage)
        .expect("render succeeds");
    assert!(list.needs_draw(), "scripted frame must plan work");

    // A second frame (atlas warm) must be equally side-effect free.
    let list2 = renderer
        .render(&snapshot_before, &damage)
        .expect("second render succeeds");
    assert_eq!(
        list.plan.dirty_rects, list2.plan.dirty_rects,
        "rendering is deterministic across frames"
    );

    assert_eq!(
        state.state_hash(),
        hash_before,
        "SEC-05: rendering must not rewrite terminal truth"
    );
    assert_eq!(
        state.snapshot(),
        snapshot_before,
        "SEC-05: rendering must not alter snapshot bytes"
    );
}

#[test]
fn tampered_snapshot_copy_cannot_reach_live_truth() {
    let state = scripted_state();
    let hash_before = state.state_hash();
    let pristine = state.snapshot();

    // Tamper with the handed-out copy the way hostile presentation code
    // could: swap cells and bump the generation.
    let mut tampered = pristine.clone();
    assert!(tampered.cells.len() >= 2, "grid must hold cells");
    tampered.cells.swap(0, 1);
    tampered.generation = tampered.generation.wrapping_add(1);

    assert_eq!(
        state.state_hash(),
        hash_before,
        "SEC-05: snapshot tampering must not reach terminal truth"
    );
    assert_eq!(
        state.snapshot(),
        pristine,
        "SEC-05: live snapshots must not observe copy tampering"
    );
}
