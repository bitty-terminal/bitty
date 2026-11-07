//! bitty#2994: a glyph the compositor cannot paint costs that glyph only.
//!
//! The live report was a TUI line with an emoji rendering blank — the text in
//! that region and the block cursor gone while the cells kept their space.
//! The class is a fail-closed drop scoped too widely: one unaddressable
//! placement must never abort the rest of the line.
//!
//! The headless production compositor (`emoji_line_paint.rs`) already drops a
//! bad slot alone. The software backend did not: a color slot outside its
//! atlas returned `InvalidInput` for the whole list, so every glyph after it
//! never painted. This file pins that contract on the backend that was wrong.
//!
//! Compiled with `cargo test -p bitty-render --features sw-fallback` (the
//! default CI matrix does not enable the feature; see `grid_pipeline.rs`).

#![cfg(feature = "sw-fallback")]

use bitty_render::atlas::{AtlasDims, AtlasSlot};
use bitty_render::frame::{FrameMode, FramePlan};
use bitty_render::geometry::{ExtentPx, RectPx};
use bitty_render::grid::{DrawList, FillRect, GlyphInstance, GlyphSource};
use bitty_render::software::{SurfaceRgba, draw_list_onto_color};

const CELL: (u32, u32) = (8, 16);
const BG: [u8; 4] = [0x10, 0x10, 0x10, 0xFF];
const INK: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const EMOJI: [u8; 4] = [0xE0, 0x30, 0x20, 0xFF];

fn glyph(dest: [i32; 2], size: [u32; 2], source: GlyphSource) -> GlyphInstance {
    GlyphInstance {
        dest,
        size,
        uv: [0.0; 4],
        color: INK,
        clip: None,
        source,
    }
}

/// One row: "ab😀cd" where the emoji's color slot is one row past its atlas —
/// the shape a placement takes after a wholesale atlas reset.
fn stale_slot_row() -> DrawList {
    let coverage = AtlasSlot {
        x: 0,
        y: 0,
        width: 8,
        height: 16,
    };
    DrawList {
        generation: 1,
        atlas_epoch: 0,
        plan: FramePlan {
            extent: ExtentPx::new(6 * CELL.0, CELL.1),
            mode: FrameMode::Full,
            dirty_rects: vec![RectPx::new(0, 0, 6 * CELL.0, CELL.1)],
        },
        fills: vec![FillRect {
            rect: RectPx::new(0, 0, 6 * CELL.0, CELL.1),
            color: BG,
        }],
        rounded_fills: Vec::new(),
        backgrounds: Vec::new(),
        overlay_fills: Vec::new(),
        overlay_card_rounded_fills: Vec::new(),
        overlay_content_fills: Vec::new(),
        overlay_rounded_fills: Vec::new(),
        glyphs: vec![
            glyph(
                [0, 0],
                [CELL.0, CELL.1],
                GlyphSource::Atlas { slot: coverage },
            ),
            glyph(
                [CELL.0 as i32, 0],
                [CELL.0, CELL.1],
                GlyphSource::Atlas { slot: coverage },
            ),
            glyph(
                [2 * CELL.0 as i32, 0],
                [2 * CELL.0, CELL.1],
                GlyphSource::ColorAtlas {
                    slot: AtlasSlot {
                        x: 0,
                        y: 16,
                        width: 16,
                        height: 16,
                    },
                },
            ),
            glyph(
                [4 * CELL.0 as i32, 0],
                [CELL.0, CELL.1],
                GlyphSource::Atlas { slot: coverage },
            ),
            glyph(
                [5 * CELL.0 as i32, 0],
                [CELL.0, CELL.1],
                GlyphSource::Atlas { slot: coverage },
            ),
        ],
        images: Vec::new(),
    }
}

fn surface() -> SurfaceRgba {
    let mut surface = SurfaceRgba::try_new(6 * CELL.0, CELL.1).expect("surface");
    surface.clear(BG);
    surface
}

fn cell_is_painted(surface: &SurfaceRgba, col: u32) -> bool {
    let width = surface.width();
    for y in 0..CELL.1 {
        for x in col * CELL.0..(col + 1) * CELL.0 {
            let index = ((y * width + x) * 4) as usize;
            let px = &surface.as_bytes()[index..index + 4];
            if [px[0], px[1], px[2], px[3]] != BG {
                return true;
            }
        }
    }
    false
}

/// The regression: the whole-list abort dropped every glyph after the bad
/// color slot, which is exactly "the rest of the line went blank".
#[test]
fn unaddressable_color_slot_does_not_abort_the_line() {
    let list = stale_slot_row();
    let coverage = vec![0xFFu8; 8 * 16];
    let dims = AtlasDims {
        width: 8,
        height: 16,
    };
    // The color atlas is a 16x16 tile of opaque emoji texels; the placement
    // addresses row 16, which the atlas does not have.
    let color = EMOJI.repeat(16 * 16);
    let color_dims = AtlasDims {
        width: 16,
        height: 16,
    };
    let mut surface = surface();
    draw_list_onto_color(
        &list,
        Some((&coverage, dims)),
        Some((&color, color_dims)),
        &mut surface,
    )
    .expect("an unaddressable color slot must not refuse the whole draw list");

    assert!(cell_is_painted(&surface, 0), "glyph before the bad slot");
    assert!(cell_is_painted(&surface, 1), "glyph before the bad slot");
    assert!(
        !cell_is_painted(&surface, 2) && !cell_is_painted(&surface, 3),
        "only the unaddressable emoji is dropped"
    );
    assert!(cell_is_painted(&surface, 4), "glyph after the bad slot");
    assert!(cell_is_painted(&surface, 5), "glyph after the bad slot");
}

/// An inline mask that disagrees with its own dimensions is that instance's
/// defect, not the line's.
#[test]
fn malformed_inline_mask_does_not_abort_the_line() {
    let mut list = stale_slot_row();
    // Replace the emoji with a malformed inline mask and keep the neighbors.
    list.glyphs[2] = glyph(
        [2 * CELL.0 as i32, 0],
        [2 * CELL.0, CELL.1],
        GlyphSource::Inline {
            mask: vec![0xFF; 7],
            width: 16,
            height: 16,
        },
    );
    let coverage = vec![0xFFu8; 8 * 16];
    let dims = AtlasDims {
        width: 8,
        height: 16,
    };
    let mut surface = surface();
    draw_list_onto_color(&list, Some((&coverage, dims)), None, &mut surface)
        .expect("a malformed inline mask must not refuse the whole draw list");
    for col in [0u32, 1, 4, 5] {
        assert!(
            cell_is_painted(&surface, col),
            "cell {col} must still paint"
        );
    }
    assert!(
        !cell_is_painted(&surface, 2),
        "only the malformed instance is dropped"
    );
}

/// A missing atlas payload stays a wiring error: that is a caller contract,
/// not a glyph defect, and silently painting a frame without it would hide the
/// bug the guard exists for.
#[test]
fn missing_color_atlas_stays_an_error() {
    let list = stale_slot_row();
    let coverage = vec![0xFFu8; 8 * 16];
    let dims = AtlasDims {
        width: 8,
        height: 16,
    };
    let mut surface = surface();
    let error = draw_list_onto_color(&list, Some((&coverage, dims)), None, &mut surface)
        .expect_err("a color instance without a color atlas is a wiring bug");
    assert!(matches!(
        error,
        bitty_render::RenderError::InvalidInput { .. }
    ));
}
