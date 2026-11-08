//! A line that contains an emoji must paint its emoji *and* every other cell
//! of that line (bitty#2994).
//!
//! The live report: any TUI line containing an emoji renders blank — the text
//! in that region and the block cursor disappear while the cells keep their
//! space. The class behind it is a *fail-closed drop scoped too widely*: a
//! glyph the compositor cannot paint must cost that glyph only, never the
//! surrounding run.
//!
//! These cases drive the production paint order through the headless surface
//! ([`Surface::headless_present_color`], the same compositor the runtime uses
//! when no adapter is attached) with a fake rasterizer that serves an RGBA
//! color bitmap for emoji presentation, so the colour-glyph path runs on every
//! host without a font stack or a GPU.
//!
//! **Scope:** symptom canaries, not regression tests. The headless GPU path
//! already refused a stale or overrunning color slot before this fix, so every
//! case here also passes on the parent commit. The regression proof is
//! `emoji_fail_closed.rs` (the software backend that painted nothing and
//! returned `Ok(())`) and the `pipeline::tests` pair
//! `horizontally_overrunning_color_slot_is_skipped` /
//! `horizontal_overflow_skips_only_the_offending_glyph`.

use bitty_platform::PhysicalSize;
use bitty_render::glyph::{AlphaMode, BitmapFormat, GlyphMetrics};
use bitty_render::gpu::Surface;
use bitty_render::grid::{DrawList, FillRect, GlyphSource, ThemePalette};
use bitty_render::{
    CellMetrics, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, GridRenderer,
    RasterKey,
};
use bitty_term_state::{Damage, State, TerminalAction};
use bitty_vt::GraphemeCell;

const CELL: (u16, u16) = (8, 16);
/// The color bitmap every emoji-presentation glyph serves.
const EMOJI_RGBA: [u8; 4] = [0xE0, 0x30, 0x20, 0xFF];
/// A block-cursor overlay fill (the runtime pushes this into `overlay_fills`).
const CURSOR: [u8; 4] = [0xCD, 0xD6, 0xF4, 0xFF];

/// Serves an opaque RGBA bitmap for emoji presentation and full coverage
/// otherwise, with metrics that fill the committed cell span so a painted cell
/// is unambiguous.
struct ColorFake {
    next_id: u64,
}

impl GlyphRasterizer for ColorFake {
    fn load_font(&mut self, _query: &FontQuery) -> Result<FontId, bitty_render::RenderError> {
        Ok(FontId::next(&mut self.next_id))
    }

    fn rasterize(
        &mut self,
        key: RasterKey,
    ) -> Result<Option<GlyphBitmap>, bitty_render::RenderError> {
        if key.character == ' ' {
            return Ok(None);
        }
        // The baseline sits 3/4 down a 16px cell, so `top == 12` places the
        // bitmap flush with the cell box: a painted cell is unambiguous.
        let metrics = if key.wants_color() {
            GlyphMetrics {
                left: 0,
                top: 12,
                width: i32::from(CELL.0) * 2,
                height: i32::from(CELL.1),
                advance: [i32::from(CELL.0) * 2, 0],
            }
        } else {
            GlyphMetrics {
                left: 0,
                top: 12,
                width: i32::from(CELL.0),
                height: i32::from(CELL.1),
                advance: [i32::from(CELL.0), 0],
            }
        };
        if key.wants_color() {
            let pixels = (metrics.width * metrics.height) as usize;
            return Ok(Some(
                GlyphBitmap::try_new(metrics, BitmapFormat::Rgba, EMOJI_RGBA.repeat(pixels))
                    .expect("rgba bitmap")
                    .with_alpha(AlphaMode::Straight),
            ));
        }
        let pixels = (metrics.width * metrics.height) as usize;
        Ok(Some(
            GlyphBitmap::try_new(metrics, BitmapFormat::Rgb, vec![0xFF; pixels * 3])
                .expect("coverage bitmap"),
        ))
    }
}

/// One frame's draw list plus the atlas payloads it addresses.
struct Row {
    list: DrawList,
    atlas: Vec<u8>,
    dims: bitty_render::atlas::AtlasDims,
    color: Option<(Vec<u8>, bitty_render::atlas::AtlasDims)>,
}

fn renderer() -> GridRenderer<ColorFake> {
    let query = FontQuery {
        family: "Fake Color".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(
        ColorFake { next_id: 0 },
        &query,
        CellMetrics::new(u32::from(CELL.0), u32::from(CELL.1)).unwrap(),
    )
    .expect("renderer")
}

/// Renders `lines` through the grid emitter and returns the draw list plus the
/// two atlas payloads.
fn render_lines(lines: &[&str]) -> Row {
    let mut renderer = renderer();
    let mut state = State::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            state.apply(&TerminalAction::PrintControl(bitty_vt::ControlChar(b'\r')));
            state.apply(&TerminalAction::PrintControl(bitty_vt::ControlChar(b'\n')));
        }
        for ch in line.chars() {
            state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
        }
    }
    let snapshot = state.snapshot();
    let damage = Damage {
        generation: snapshot.generation,
        regions: state.damage_since(0).into_boxed_slice(),
    };
    let list = renderer.render(&snapshot, &damage).expect("render");
    Row {
        atlas: renderer.atlas_texels().to_vec(),
        dims: renderer.atlas_dims(),
        color: renderer
            .color_atlas_texels()
            .map(|(texels, dims)| (texels.to_vec(), dims)),
        list,
    }
}

/// Presents `row` on a `cells_x` x `cells_y` headless surface and returns its
/// premultiplied RGBA buffer.
fn present(row: &Row, cells_x: u32, cells_y: u32) -> Vec<u8> {
    let surface = Surface::headless(PhysicalSize::new(
        cells_x * u32::from(CELL.0),
        cells_y * u32::from(CELL.1),
    ))
    .expect("headless surface");
    surface
        .headless_present_color(
            &row.list,
            Some((&row.atlas, row.dims)),
            row.color
                .as_ref()
                .map(|(texels, dims)| (texels.as_slice(), *dims)),
        )
        .expect("headless present");
    surface.headless_rgba().expect("rgba buffer")
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let index = ((y * width + x) * 4) as usize;
    [
        rgba[index],
        rgba[index + 1],
        rgba[index + 2],
        rgba[index + 3],
    ]
}

/// A cell is painted when at least one of its pixels left the clear colour.
fn cell_is_painted(rgba: &[u8], width: u32, col: u32, row: u32) -> bool {
    let bg = ThemePalette::bitty_dark().background;
    for y in row * u32::from(CELL.1)..(row + 1) * u32::from(CELL.1) {
        for x in col * u32::from(CELL.0)..(col + 1) * u32::from(CELL.0) {
            if pixel(rgba, width, x, y) != bg {
                return true;
            }
        }
    }
    false
}

#[test]
fn emoji_alone_paints_the_color_glyph() {
    let row = render_lines(&["\u{1F600}"]);
    assert!(
        row.color.is_some(),
        "the emoji must ride the color atlas, not the coverage atlas"
    );
    assert!(matches!(
        row.list.glyphs[0].source,
        GlyphSource::ColorAtlas { .. }
    ));
    let width = 4 * u32::from(CELL.0);
    let rgba = present(&row, 4, 1);
    assert!(
        cell_is_painted(&rgba, width, 0, 0),
        "emoji lead cell paints"
    );
    assert!(
        cell_is_painted(&rgba, width, 1, 0),
        "emoji spacer cell paints"
    );
    assert_eq!(
        pixel(&rgba, width, 0, 0),
        EMOJI_RGBA,
        "the composited pixel is the color bitmap, not a coverage tint"
    );
    assert!(
        !cell_is_painted(&rgba, width, 2, 0),
        "cells after the emoji stay clear"
    );
}

#[test]
fn emoji_with_text_paints_every_cell_on_the_line() {
    let row = render_lines(&["ab\u{1F600}cd"]);
    let width = 6 * u32::from(CELL.0);
    let rgba = present(&row, 6, 1);
    // 'a' 'b' occupy cells 0-1, the wide emoji owns 2-3, 'c' 'd' sit at 4-5.
    for col in 0..6 {
        assert!(
            cell_is_painted(&rgba, width, col, 0),
            "cell {col} of an emoji line must paint"
        );
    }
    assert_eq!(
        pixel(&rgba, width, 2 * u32::from(CELL.0), 0),
        EMOJI_RGBA,
        "the emoji itself keeps its colour"
    );
}

#[test]
fn emoji_line_with_a_visible_cursor_paints_both() {
    // The runtime pushes the focused pane's block cursor as an overlay fill
    // painted before the glyph pass; a color glyph must not take it with it.
    let mut row = render_lines(&["ab\u{1F600}cd"]);
    row.list.overlay_fills.push(FillRect {
        rect: bitty_render::geometry::RectPx::new(
            5 * i32::from(CELL.0),
            0,
            u32::from(CELL.0),
            u32::from(CELL.1),
        ),
        color: CURSOR,
    });
    let width = 6 * u32::from(CELL.0);
    let rgba = present(&row, 6, 1);
    assert!(
        cell_is_painted(&rgba, width, 2, 0),
        "the emoji paints with the cursor present"
    );
    assert_eq!(
        pixel(&rgba, width, 5 * u32::from(CELL.0), 0),
        CURSOR,
        "the cursor fill still paints over its own cell"
    );
    for col in 0..6 {
        assert!(
            cell_is_painted(&rgba, width, col, 0),
            "cell {col} must paint"
        );
    }
}

#[test]
fn color_glyph_in_a_paged_pane_paints() {
    // A paged (scrolled) pane composites history rows into the viewport, so
    // the emoji line arrives as a non-first snapshot row — the shape rendered
    // here. The emoji row must paint like any other.
    let row = render_lines(&["scrollback one", "scrollback two", "abc \u{1F600} def"]);
    let width = 14 * u32::from(CELL.0);
    let rgba = present(&row, 14, 3);
    // "abc 😀 def": cells 3 and 6 are spaces, every other cell carries ink.
    for col in [0u32, 1, 2, 4, 5, 7, 8, 9] {
        assert!(
            cell_is_painted(&rgba, width, col, 2),
            "paged emoji row cell {col} must paint"
        );
    }
    assert_eq!(
        pixel(&rgba, width, 4 * u32::from(CELL.0), 2 * u32::from(CELL.1)),
        EMOJI_RGBA,
        "the paged color glyph keeps its colour"
    );
}

/// A color slot the compositor cannot address — the shape a placement takes
/// after a wholesale atlas reset — must cost that glyph only. The compositors
/// share this contract; `emoji_fail_closed.rs` pins the software backend that
/// used to refuse the whole list instead.
#[test]
fn stale_color_slot_drops_only_its_glyph() {
    let mut row = render_lines(&["ab\u{1F600}cd"]);
    let (_, color_dims) = row.color.as_ref().expect("color atlas payload");
    let color_dims = *color_dims;
    let emoji = row
        .list
        .glyphs
        .iter_mut()
        .find(|glyph| matches!(glyph.source, GlyphSource::ColorAtlas { .. }))
        .expect("one color glyph");
    let GlyphSource::ColorAtlas { slot } = &mut emoji.source else {
        unreachable!()
    };
    // One row past the atlas: a stale placement after `evict_now`.
    slot.y = color_dims.height;

    let width = 6 * u32::from(CELL.0);
    let rgba = present(&row, 6, 1);
    for col in [0u32, 1, 4, 5] {
        assert!(
            cell_is_painted(&rgba, width, col, 0),
            "cell {col} must still paint when one color slot is unaddressable"
        );
    }
    assert!(
        !cell_is_painted(&rgba, width, 2, 0) && !cell_is_painted(&rgba, width, 3, 0),
        "only the unaddressable emoji itself is dropped"
    );
}
