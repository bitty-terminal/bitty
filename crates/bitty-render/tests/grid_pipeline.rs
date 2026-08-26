//! End-to-end grid-pipeline tests under the `sw-fallback` feature.
//!
//! These drive the SAME plan/place/cache pipeline the GPU backend consumes
//! (`GridRenderer::render`) and composite its [`DrawList`] through
//! [`bitty_render::software::draw_list_onto`] onto an RGBA surface,
//! proving the terminal-state-rfc requirement that the software fallback
//! exercises the production pipeline headlessly: snapshot bytes in, RGBA
//! bytes out.
//!
//! Compiled and tested locally with
//! `cargo test -p bitty-render --features sw-fallback`; the default CI
//! matrix does not enable the feature (see crate docs).

#![cfg(feature = "sw-fallback")]

use bitty_render::grid::Rgba8;
use bitty_render::software::{SurfaceRgba, draw_list_onto};
use bitty_render::{CellMetrics, DrawList, GridRenderer, RenderError};
use bitty_term_state::{Damage, State, TerminalAction};
use bitty_vt::GraphemeCell;

use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};

/// Deterministic fake rasterizer (same contract shape as the unit-test
/// one): bitmap width 6..=8 by character code, height 6, `' '` blank.
struct FakeRasterizer {
    next_id: u64,
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next_id))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        let code = u32::from(key.character) as usize;
        // Wide characters produce a two-cell-wide bitmap so the trailing
        // half of the cell pair carries ink, like real monospace fonts.
        let width: i32 = if bitty_term_state::char_cell_width(key.character) == 2 {
            14
        } else {
            i32::try_from(code % 3 + 6).unwrap()
        };
        let height: i32 = 6;
        let data: Vec<u8> = (0..(width as usize) * (height as usize) * 3)
            .map(|i| (0x30 + ((code + i) % 0x50)) as u8)
            .collect();
        Ok(Some(GlyphBitmap::try_new(
            GlyphMetrics {
                left: 0,
                top: 1,
                width,
                height,
                advance: [width, 0],
            },
            BitmapFormat::Rgb,
            data,
        )?))
    }
}

fn font_query() -> FontQuery {
    FontQuery {
        family: "Fake Mono".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    }
}

fn renderer() -> GridRenderer<FakeRasterizer> {
    GridRenderer::new(
        FakeRasterizer { next_id: 0 },
        &font_query(),
        CellMetrics::new(8, 16).unwrap(),
    )
    .unwrap()
}

fn print(c: char) -> TerminalAction {
    TerminalAction::Print(GraphemeCell::from(c))
}

fn whole_history(state: &State) -> Damage {
    Damage {
        generation: state.generation(),
        regions: state.damage_since(0).into_boxed_slice(),
    }
}

/// Pixel index of the center of cell `(row, col)` on an 8x16 surface grid.
fn cell_center_pixel(row: usize, col: usize, stride_cols: usize) -> usize {
    let x = col * 8 + 4;
    let y = row * 16 + 6;
    (y * stride_cols * 8 + x) * 4
}

#[test]
fn snapshot_bytes_in_rgba_bytes_out() {
    // A tiny script so the surface stays small: two chars, one wide.
    let mut state = State::new();
    for action in [print('O'), print('K'), print('\u{6F22}')] {
        state.apply(&action);
    }
    let snapshot = state.snapshot();

    let mut grid = renderer();
    let surface = bitty_render::grid::render_snapshot_to_surface(
        &mut grid,
        &snapshot,
        &whole_history(&state),
        bitty_render::grid::DEFAULT_BG,
    )
    .unwrap();

    // The three printed cells carry glyph ink; untouched areas stay pure
    // background.
    let bg: Rgba8 = bitty_render::grid::DEFAULT_BG;
    let px = |row: usize, col: usize| {
        let i = cell_center_pixel(row, col, usize::try_from(surface.width()).unwrap());
        [
            surface.as_bytes()[i],
            surface.as_bytes()[i + 1],
            surface.as_bytes()[i + 2],
            surface.as_bytes()[i + 3],
        ]
    };

    // Cells 0,1,2 hold O,K,漢 — their centers may or may not hit ink given
    // the fake coverage pattern, so assert against a small neighborhood:
    // at least one non-background pixel must exist in each cell.
    let has_ink = |row: usize, col: usize| {
        for dy in 0..16u32 {
            for dx in 0..8u32 {
                let x = col * 8 + dx as usize;
                let y = row * 16 + dy as usize;
                let i = (y * surface.width() as usize + x) * 4;
                let p = [
                    surface.as_bytes()[i],
                    surface.as_bytes()[i + 1],
                    surface.as_bytes()[i + 2],
                ];
                if p != [bg[0], bg[1], bg[2]] {
                    return true;
                }
            }
        }
        false
    };
    assert!(has_ink(0, 0), "cell(0,0) must show glyph ink");
    assert!(has_ink(0, 1), "cell(0,1) must show glyph ink");
    assert!(has_ink(0, 2), "wide leading half must show glyph ink");
    assert!(has_ink(0, 3), "wide spacer half shows the wide glyph");
    assert!(!has_ink(5, 40), "far background must stay untouched");
    let _ = px(0, 0);
}

#[test]
fn incremental_frames_accumulate_exactly_to_a_full_redraw() {
    // Batch 1: plain text. Batch 2: styled text on the next cells. The
    // persistent surface receives ONLY each batch's damage; the reference
    // surface receives ONE full redraw of the final state. Byte equality
    // proves the damage-driven partial redraw semantics end to end.
    let mut state = State::new();
    let mut batches: Vec<Damage> = Vec::new();
    for action in [print('O'), print('K')] {
        batches.push(state.apply(&action));
    }
    for action in [
        bitty_term_state::TerminalAction::SetAttributes {
            attrs: bitty_term_state::AttributeDiff {
                changes: Box::new([bitty_term_state::AttributeChange::Background(
                    bitty_term_state::Color::Indexed(4),
                )]),
            },
        },
        print('\u{6F22}'),
        print('!'),
    ] {
        batches.push(state.apply(&action));
    }

    let snapshot = state.snapshot();
    let extent = CellMetrics::new(8, 16)
        .unwrap()
        .extent_for(snapshot.width, snapshot.height);

    // Incremental path.
    let mut grid = renderer();
    let mut surface = SurfaceRgba::try_new(extent.width, extent.height.min(64)).unwrap();
    surface.clear(bitty_render::grid::DEFAULT_BG);
    for batch in &batches {
        let list = grid.render(&snapshot, batch).unwrap();
        bitty_render::grid::composite_frame(&grid, &list, &mut surface).unwrap();
        let _drained = grid.take_atlas_uploads();
    }

    // Reference path: one full redraw onto a fresh surface.
    let mut reference_renderer = renderer();
    let list = reference_renderer
        .render(&snapshot, &whole_history(&state))
        .unwrap();
    let mut reference = SurfaceRgba::try_new(extent.width, extent.height.min(64)).unwrap();
    reference.clear(bitty_render::grid::DEFAULT_BG);
    draw_list_onto(
        &list,
        Some((
            reference_renderer.atlas_texels(),
            reference_renderer.atlas_dims(),
        )),
        &mut reference,
    )
    .unwrap();

    assert_eq!(surface.as_bytes(), reference.as_bytes());
}

#[test]
fn software_output_is_byte_deterministic_across_runs() {
    let run = || {
        let mut state = State::new();
        for action in [print('A'), print('\u{6F22}'), print('z')] {
            state.apply(&action);
        }
        let mut grid = renderer();
        let surface = bitty_render::grid::render_snapshot_to_surface(
            &mut grid,
            &state.snapshot(),
            &whole_history(&state),
            bitty_render::grid::DEFAULT_BG,
        )
        .unwrap();
        (surface.as_bytes().to_vec(), grid.atlas_texels().to_vec())
    };

    let (a, ta) = run();
    let (b, tb) = run();
    assert_eq!(a, b, "RGBA output must be byte-identical across runs");
    assert_eq!(ta, tb, "atlas textures must be byte-identical across runs");
}

#[test]
fn clean_frame_composites_nothing() {
    let state = State::new();
    let mut grid = renderer();
    let list: DrawList = grid
        .render(&state.snapshot(), &Damage::empty(state.generation()))
        .unwrap();
    assert!(!list.needs_draw());

    let mut surface = SurfaceRgba::try_new(64, 16).unwrap();
    surface.clear(bitty_render::grid::DEFAULT_BG);
    let before = surface.as_bytes().to_vec();
    bitty_render::grid::composite_frame(&grid, &list, &mut surface).unwrap();
    assert_eq!(surface.as_bytes(), &before[..]);
}
