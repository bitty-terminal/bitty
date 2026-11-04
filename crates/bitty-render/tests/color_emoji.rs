//! Color emoji painting (CTX-1691, issue #2858).
//!
//! Acceptance evidence for the issue: a terminal row containing U+2705 paints
//! the composited color glyph — not the monochrome outline flattened to
//! coverage — on a face that resolves to a color emoji face, and the same row
//! on a face without one keeps the previous monochrome behavior with unchanged
//! layout.
//!
//! The deterministic half of the evidence runs on every CI host: a fake
//! rasterizer serves RGBA for emoji presentation and three-channel coverage
//! for text presentation, so the assertions can tell color from monochrome by
//! inspecting the emitted instance source and the composited pixels. The live
//! half (`BITTY_RENDER_FONT_TESTS=1`) proves the real `SwashSingle` +
//! fallback chain resolves U+2705 to a COLR/CPAL layer stack or a color
//! bitmap strike on a host that has such a face, and skips cleanly otherwise.

use bitty_platform::PhysicalSize;
use bitty_render::glyph::{BitmapFormat, GlyphMetrics};
use bitty_render::gpu::Surface;
use bitty_render::grid::GlyphSource;
use bitty_render::{
    CellMetrics, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphRasterizer, GridRenderer,
    RasterKey,
};
use bitty_term_state::{Damage, EmojiPresentation, State, TerminalAction};
use bitty_vt::GraphemeCell;

const CELL: (u16, u16) = (8, 16);
/// Renderer default foreground (the palette's `DEFAULT_FG`).
const DEFAULT_FG: [u8; 4] = [0xCD, 0xD6, 0xF4, 0xFF];
const COLOR_RGBA: [u8; 4] = [0xE0, 0x30, 0x20, 0xFF];

/// Rasterizer that serves a color bitmap for emoji presentation and
/// monochrome coverage otherwise, with identical metrics so a layout
/// difference between the two presentations is a bug, not a glyph-size
/// difference.
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
        let metrics = GlyphMetrics {
            left: 0,
            top: 6,
            width: 6,
            height: 6,
            advance: [8, 0],
        };
        if key.wants_color() {
            return Ok(Some(
                GlyphBitmap::try_new(metrics, BitmapFormat::Rgba, COLOR_RGBA.repeat(36))
                    .expect("rgba bitmap"),
            ));
        }
        Ok(Some(
            GlyphBitmap::try_new(metrics, BitmapFormat::Rgb, vec![0xFF; 6 * 6 * 3])
                .expect("coverage bitmap"),
        ))
    }
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

/// One frame's draw list plus the atlas payloads it addresses.
struct Row {
    list: bitty_render::grid::DrawList,
    atlas: Vec<u8>,
    dims: bitty_render::atlas::AtlasDims,
    color: Option<(Vec<u8>, bitty_render::atlas::AtlasDims)>,
}

/// Renders one printed row through the shaped emitter and returns the draw
/// list plus the two atlas payloads.
fn render_row(cells: &[char]) -> Row {
    let mut renderer = renderer();
    let mut state = State::new();
    for &scalar in cells {
        state.apply(&TerminalAction::Print(GraphemeCell::from(scalar)));
    }
    let snapshot = state.snapshot();
    let damage = Damage {
        generation: snapshot.generation,
        regions: state.damage_since(0).into_boxed_slice(),
    };
    let draw_list = renderer.render(&snapshot, &damage).expect("render");
    let atlas = renderer.atlas_texels().to_vec();
    let dims = renderer.atlas_dims();
    let color = renderer
        .color_atlas_texels()
        .map(|(texels, color_dims)| (texels.to_vec(), color_dims));
    Row {
        list: draw_list,
        atlas,
        dims,
        color,
    }
}

fn cell_scalar(cell: &bitty_term_state::Cell) -> char {
    cell.glyph
}

#[test]
fn u2705_without_selector_paints_the_color_glyph() {
    let Row { list, color, .. } = render_row(&['\u{2705}']);
    assert_eq!(
        list.glyphs.len(),
        1,
        "one wide cluster must emit one instance"
    );
    assert!(
        matches!(list.glyphs[0].source, GlyphSource::ColorAtlas { .. }),
        "U+2705 is Emoji_Presentation=Yes and must paint the color glyph, got {:?}",
        list.glyphs[0].source
    );
    let instance = &list.glyphs[0];
    assert_eq!(
        instance.color, [0xFF; 4],
        "a color bitmap is never tinted by the cell foreground"
    );
    assert_eq!(
        instance.size,
        [6, 6],
        "the color bitmap keeps its texel size"
    );
    let (texels, dims) = color.expect("color atlas payload");
    assert_eq!(
        texels.len(),
        usize::from(dims.width) * usize::from(dims.height) * 4
    );
    // The atlas slot must carry the rasterizer's color, not coverage.
    let slot = match list.glyphs[0].source {
        GlyphSource::ColorAtlas { slot } => slot,
        _ => unreachable!(),
    };
    let stride = usize::from(dims.width) * 4;
    for row in 0..usize::from(slot.height) {
        for col in 0..usize::from(slot.width) {
            let index = (usize::from(slot.y) + row) * stride + (usize::from(slot.x) + col) * 4;
            assert_eq!(
                &texels[index..index + 4],
                &COLOR_RGBA,
                "color atlas texel {col},{row} is the rasterized color"
            );
        }
    }
}

#[test]
fn u2705_with_vs16_paints_the_color_glyph() {
    let Row { list, color, .. } = render_row(&['\u{2705}', '\u{FE0F}']);
    assert_eq!(
        list.glyphs.len(),
        1,
        "VS16 is zero-width and adds no instance"
    );
    assert!(
        matches!(list.glyphs[0].source, GlyphSource::ColorAtlas { .. }),
        "VS16 requests emoji presentation: {:?}",
        list.glyphs[0].source
    );
    assert!(color.is_some(), "the color atlas holds the placed glyph");
}

#[test]
fn u2705_with_vs15_stays_monochrome() {
    let Row { list, color, .. } = render_row(&['\u{2705}', '\u{FE0E}']);
    assert_eq!(
        list.glyphs.len(),
        1,
        "VS15 is zero-width and adds no instance"
    );
    assert!(
        matches!(list.glyphs[0].source, GlyphSource::Atlas { .. }),
        "VS15 keeps text presentation: {:?}",
        list.glyphs[0].source
    );
    assert!(
        color.is_none(),
        "no color atlas placement is made for a monochrome cluster"
    );
    assert_eq!(
        list.glyphs[0].color, DEFAULT_FG,
        "the monochrome control keeps the cell foreground tint"
    );
}

#[test]
fn text_presentation_scalar_stays_monochrome_without_vs16() {
    // U+2611 is emoji-capable but Emoji_Presentation=No: without a selector it
    // must not paint color.
    let Row { list, color, .. } = render_row(&['\u{2611}']);
    assert_eq!(list.glyphs.len(), 1);
    assert!(
        matches!(list.glyphs[0].source, GlyphSource::Atlas { .. }),
        "U+2611 defaults to text presentation: {:?}",
        list.glyphs[0].source
    );
    assert!(color.is_none());
}

#[test]
fn headless_present_paints_the_color_pixels() {
    let Row {
        list,
        atlas,
        dims,
        color,
    } = render_row(&['\u{2705}']);
    let (color_texels, color_dims) = color.expect("color atlas payload");
    let surface = Surface::headless(PhysicalSize::new(u32::from(CELL.0), u32::from(CELL.1)))
        .expect("headless surface");
    surface
        .headless_present_color(
            &list,
            Some((&atlas, dims)),
            Some((&color_texels, color_dims)),
        )
        .expect("headless present");
    let rgba = surface.headless_rgba().expect("rgba buffer");
    // The instance is placed at the baseline (3/4 of the 16px cell row = 12)
    // minus the bitmap's top bearing (6): row 6, column 0.
    let index = (6 * usize::from(CELL.0)) * 4;
    assert_eq!(
        &rgba[index..index + 4],
        &COLOR_RGBA,
        "the composited pixel is the color glyph, not a coverage tint"
    );
}

#[test]
fn color_row_keeps_the_double_width_cell_contract() {
    // The wide cluster occupies two cells; the following cell keeps its
    // column, so a color glyph never shifts the grid.
    let Row { list, .. } = render_row(&['\u{2705}', 'A']);
    assert_eq!(list.glyphs.len(), 2, "wide glyph plus the following ASCII");
    // 'A' starts at the third column (two cells for U+2705).
    assert_eq!(
        list.glyphs[1].dest[0],
        2 * i32::from(CELL.0),
        "the ASCII cell keeps the grid column after a double-width color glyph"
    );
    assert!(matches!(list.glyphs[1].source, GlyphSource::Atlas { .. }));
}

#[test]
fn cell_zero_is_the_wide_glyph() {
    // Guards the fixture itself: the row really holds U+2705 in cell 0.
    let mut state = State::new();
    state.apply(&TerminalAction::Print(GraphemeCell::from('\u{2705}')));
    let snapshot = state.snapshot();
    assert_eq!(cell_scalar(&snapshot.cells[0]), '\u{2705}');
}

// ---------------------------------------------------------------------------
// Live font evidence (BITTY_RENDER_FONT_TESTS=1)
// ---------------------------------------------------------------------------

/// True when a swash-backed session can serve a COLR/CPAL or color-bitmap
/// glyph for U+2705 through the configured emoji fallback family.
#[test]
fn live_color_face_serves_a_color_u2705() {
    if std::env::var("BITTY_RENDER_FONT_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: BITTY_RENDER_FONT_TESTS != 1");
        return;
    }
    use bitty_render::SwashSingle;

    let mut rasterizer = match SwashSingle::new() {
        Ok(rasterizer) => rasterizer,
        Err(_) => {
            eprintln!("skipped: no system font database available");
            return;
        }
    };
    let family = bitty_config::types::EMOJI_FALLBACK_FAMILY;
    let query = FontQuery {
        family: family.to_string(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    let Ok(font) = rasterizer.load_font(&query) else {
        eprintln!("skipped: emoji fallback family {family} is not installed");
        return;
    };
    let source = rasterizer
        .glyph_source_for(font, '\u{2705}', 12.0, EmojiPresentation::Emoji)
        .expect("probe")
        .expect("U+2705 is covered by the emoji face");
    assert!(
        matches!(
            source,
            bitty_render::GlyphSource::ColorOutline | bitty_render::GlyphSource::ColorBitmap
        ),
        "the emoji face must serve a color source, got {source:?}"
    );
    // The rasterized bitmap for emoji presentation is RGBA with more than one
    // color; text presentation stays a coverage bitmap.
    let key = RasterKey::new('\u{2705}', font, 12.0)
        .expect("key")
        .with_presentation(EmojiPresentation::Emoji);
    let bitmap = rasterizer
        .rasterize(key)
        .expect("rasterize")
        .expect("drawable U+2705");
    assert_eq!(
        bitmap.format,
        BitmapFormat::Rgba,
        "color presentation is RGBA"
    );
    let distinct: std::collections::BTreeSet<[u8; 3]> = bitmap
        .data
        .chunks_exact(4)
        .filter(|px| px[3] > 0)
        .map(|px| [px[0], px[1], px[2]])
        .collect();
    assert!(
        distinct.len() > 1,
        "a color glyph paints more than one color: {distinct:?}"
    );

    let text_key = RasterKey::new('\u{2705}', font, 12.0).expect("key");
    let monochrome = rasterizer
        .rasterize(text_key)
        .expect("rasterize")
        .expect("drawable U+2705");
    assert_eq!(
        monochrome.format,
        BitmapFormat::Rgb,
        "text presentation keeps the monochrome coverage bitmap"
    );
}
