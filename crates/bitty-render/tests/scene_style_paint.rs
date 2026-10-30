//! Scene style primitives (v2): paint output, bounds, v1 identity, a11y.
//!
//! Evidence for the scene-model + paint half of bitty#2112 (CTX-1659). A
//! `SceneNode::Styled` wrapper carries bounded style data only; this suite
//! pins that Core measures and paints it, that a flat style is byte-identical
//! to an unwrapped child, that a v1 scene's paint list is unchanged, and that
//! the text projection (the accessibility parity source) is never changed by
//! styling.

use bitty_render::error::RenderError;
use bitty_render::frame::plan_frame;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, DrawList, GridRenderer, SnapshotDamage};
use bitty_render::scene::{
    ContentPalette, ScenePaint, ScenePaintBudget, ScenePaintError, ScenePaintOp, ScenePaintStyle,
    SceneSurface, paint_scene, paint_scene_with_style,
};
use bitty_render::{RectPx, grid::Rgba8};
use bitty_rich::scene::{
    BlockAnchor, BlockId, Border, ContentToken, RichBlock, SCENE_STYLE_MAX_BORDER_PX, Scene,
    SceneNode, SceneNodeStyle, ScrollBehavior, StyledSpan,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeRasterizer {
    next: u64,
}

impl GlyphRasterizer for FakeRasterizer {
    fn load_font(&mut self, _: &FontQuery) -> Result<FontId, RenderError> {
        Ok(FontId::next(&mut self.next))
    }

    fn rasterize(&mut self, key: RasterKey) -> Result<Option<GlyphBitmap>, RenderError> {
        if key.character == ' ' || key.character == '\u{FFFD}' {
            return Ok(None);
        }
        let side = u32::from(key.character) % 3 + 6;
        Ok(Some(
            GlyphBitmap::try_new(
                GlyphMetrics {
                    left: 0,
                    top: 6,
                    width: side as i32,
                    height: side as i32,
                    advance: [side as i32, 0],
                },
                BitmapFormat::Rgb,
                vec![0xAA; side as usize * side as usize * 3],
            )
            .unwrap(),
        ))
    }
}

fn renderer() -> GridRenderer<FakeRasterizer> {
    let query = FontQuery {
        family: "Fake".into(),
        style: FontStyle::Normal,
        point_size: 12.0,
    };
    GridRenderer::new(
        FakeRasterizer { next: 0 },
        &query,
        CellMetrics::new(8, 16).unwrap(),
    )
    .unwrap()
}

fn cell() -> CellMetrics {
    CellMetrics::new(8, 16).unwrap()
}

/// Leaf content rect at a non-zero origin: 40 columns x 12 rows.
fn surface() -> SceneSurface {
    SceneSurface::new(RectPx::new(16, 32, 320, 192), cell())
}

fn text(value: &str) -> SceneNode {
    SceneNode::Text(StyledSpan {
        text: value.to_string(),
        bold: false,
        italic: false,
    })
}

fn styled(style: SceneNodeStyle, child: SceneNode) -> SceneNode {
    SceneNode::Styled {
        style,
        child: Box::new(child),
    }
}

fn block(id: u64, content: SceneNode) -> RichBlock {
    RichBlock::new(
        BlockId(id),
        BlockAnchor::Zone(id),
        content,
        ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .expect("fixture fits SCN-1..3 and style caps")
}

fn scene_of(content: SceneNode) -> Scene {
    let mut scene = Scene::new();
    scene
        .insert(block(1, content))
        .expect("fixture fits SCN-1..5");
    scene
}

fn paint(content: SceneNode) -> ScenePaint {
    paint_scene(&scene_of(content), surface(), ScenePaintBudget::default()).expect("paints")
}

fn empty_draw_list() -> DrawList {
    DrawList {
        generation: 0,
        atlas_epoch: 0,
        plan: plan_frame(&SnapshotDamage::new(
            &bitty_term_state::State::new().snapshot(),
            &bitty_term_state::Damage {
                generation: 0,
                regions: Vec::new().into_boxed_slice(),
            },
            cell(),
        )),
        fills: Vec::new(),
        rounded_fills: Vec::new(),
        backgrounds: Vec::new(),
        overlay_fills: Vec::new(),
        overlay_card_rounded_fills: Vec::new(),
        overlay_content_fills: Vec::new(),
        overlay_rounded_fills: Vec::new(),
        glyphs: Vec::new(),
        images: Vec::new(),
    }
}

fn rings(paint: &ScenePaint) -> Vec<(RectPx, u16, u16, Rgba8)> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Rounded {
                rect,
                border,
                radius,
                color,
            } => Some((*rect, *border, *radius, *color)),
            _ => None,
        })
        .collect()
}

fn text_ops(paint: &ScenePaint) -> Vec<(u16, u16, String)> {
    paint
        .text_lines()
        .map(|line| (line.row, line.col, line.text.to_string()))
        .collect()
}

/// Deterministic one-line encoding of a paint list (golden evidence).
fn encode(paint: &ScenePaint) -> String {
    let mut out = String::new();
    for op in paint.ops() {
        match op {
            ScenePaintOp::Fill { rect, color } => out.push_str(&format!(
                "fill {} {} {} {} {:02x}{:02x}{:02x}{:02x}\n",
                rect.x, rect.y, rect.width, rect.height, color[0], color[1], color[2], color[3]
            )),
            ScenePaintOp::Text {
                row,
                col,
                rect,
                text,
                color,
                bold,
                ..
            } => out.push_str(&format!(
                "text {row} {col} {} {} {} {:02x}{:02x}{:02x}{:02x} {bold} {text}\n",
                rect.x, rect.y, rect.width, color[0], color[1], color[2], color[3]
            )),
            ScenePaintOp::Rounded {
                rect,
                border,
                radius,
                color,
            } => out.push_str(&format!(
                "rounded {} {} {} {} {border} {radius} {:02x}{:02x}{:02x}{:02x}\n",
                rect.x, rect.y, rect.width, rect.height, color[0], color[1], color[2], color[3]
            )),
            ScenePaintOp::Image { placement, rect } => out.push_str(&format!(
                "image {} {} {} {} {}\n",
                rect.x, rect.y, rect.width, rect.height, placement.0
            )),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Flat style and v1 identity
// ---------------------------------------------------------------------------

#[test]
fn flat_style_paints_exactly_like_the_unwrapped_child() {
    let plain = text("hello");
    let wrapped = styled(SceneNodeStyle::FLAT, plain.clone());
    let bare = paint(plain);
    let flat = paint(wrapped);

    assert_eq!(
        encode(&flat),
        encode(&bare),
        "a flat style must emit the same ops, colours, and text as no wrapper"
    );
    assert!(rings(&flat).is_empty(), "a flat style paints no ring");
    assert_eq!(flat.damage(), bare.damage());
    assert_eq!(flat.text_lines().count(), bare.text_lines().count());
}

#[test]
fn unstyled_v1_paint_list_is_golden_pinned() {
    // Accepted "unstyled nodes render exactly as today" evidence: this list
    // is frozen. Any drift in v1 layout, colour, or op order fails here.
    let scene = scene_of(SceneNode::Column(vec![
        text("hi"),
        SceneNode::Block {
            border: Some(Border {
                width: 1,
                color: String::new(),
            }),
            child: Box::new(text("boxed")),
        },
        SceneNode::Rule,
    ]));
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(
        encode(&paint),
        "text 0 0 16 32 16 cdd6f4ff false hi\n\
         fill 16 48 56 16 585b70ff\n\
         fill 16 48 8 48 585b70ff\n\
         fill 16 80 56 16 585b70ff\n\
         fill 64 48 8 48 585b70ff\n\
         text 2 1 24 64 40 cdd6f4ff false boxed\n\
         fill 16 104 320 1 585b70ff\n"
    );
}

#[test]
fn a_styled_node_does_not_change_a_v1_siblings_paint() {
    let plain = paint(SceneNode::Row(vec![text("aa"), text("bb")]));
    let with_style = paint(SceneNode::Row(vec![
        styled(
            SceneNodeStyle::new(2, 4, 1, ContentToken::Accent),
            text("aa"),
        ),
        text("bb"),
    ]));
    let sibling = |paint: &ScenePaint| {
        paint
            .text_lines()
            .find(|line| line.text == "bb")
            .map(|line| (line.row, line.col))
    };
    // The styled box measures child + padding on each side, exactly like a
    // declared frame, so the sibling follows it instead of overlapping it.
    assert_eq!(sibling(&plain), Some((0, 2)));
    assert_eq!(sibling(&with_style), Some((0, 6)));
}

// ---------------------------------------------------------------------------
// Padding (spacing)
// ---------------------------------------------------------------------------

#[test]
fn padding_insets_the_child_by_cells_and_emits_no_op() {
    let paint = paint(styled(
        SceneNodeStyle::new(2, 0, 0, ContentToken::Foreground),
        text("hi"),
    ));
    assert!(rings(&paint).is_empty(), "padding alone paints nothing");
    assert_eq!(text_ops(&paint), vec![(2, 2, "hi".to_string())]);
    let line = paint
        .ops()
        .iter()
        .find_map(|op| match op {
            ScenePaintOp::Text { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("text op");
    // The line keeps the rest of the allocation (like an unstyled root line),
    // shifted by the padding cells on both axes.
    let expected = RectPx::new(
        surface().rect.x + 2 * i32::try_from(cell().width).unwrap(),
        surface().rect.y + 2 * i32::try_from(cell().height).unwrap(),
        surface().rect.width - 4 * cell().width,
        cell().height,
    );
    assert_eq!(line, expected);
}

#[test]
fn padding_is_bounded_by_the_frame_not_the_leaf() {
    // 40 columns of leaf, a 1-column child, 32 cells of padding per side:
    // the measured frame wants 65 columns and is clamped to the leaf, but the
    // child still starts at its honest inset (clipped away by the leaf).
    let paint = paint(styled(
        SceneNodeStyle::new(32, 0, 0, ContentToken::Foreground),
        text("x"),
    ));
    assert_eq!(text_ops(&paint), Vec::<(u16, u16, String)>::new());
    assert_eq!(paint.damage().len(), 0, "nothing visible paints");
}

// ---------------------------------------------------------------------------
// Border + token (borders)
// ---------------------------------------------------------------------------

#[test]
fn border_paints_one_rounded_ring_with_the_token_colour() {
    let style = SceneNodeStyle::new(0, 6, 2, ContentToken::Error);
    let paint = paint(styled(style, text("hi")));
    let rings = rings(&paint);
    assert_eq!(rings.len(), 1, "one ring per bordered node");
    let (rect, border, radius, color) = rings[0];
    assert_eq!(border, 2);
    assert_eq!(radius, 6);
    assert_eq!(color, ContentPalette::default().color(ContentToken::Error));
    // Frame = child (2 cols x 1 row), no padding.
    assert_eq!(
        rect,
        RectPx::new(
            surface().rect.x,
            surface().rect.y,
            2 * cell().width,
            cell().height
        )
    );
    // Paint order: ring before the text glyph op for the same node.
    let ring_at = paint
        .ops()
        .iter()
        .position(|op| matches!(op, ScenePaintOp::Rounded { .. }))
        .expect("ring");
    let text_at = paint
        .ops()
        .iter()
        .position(|op| matches!(op, ScenePaintOp::Text { .. }))
        .expect("text");
    assert!(ring_at < text_at, "the ring is painted under the text");
}

#[test]
fn the_default_content_palette_is_the_reference_theme_roles() {
    use bitty_render::ThemePalette;
    use bitty_render::grid::DEFAULT_FG;

    let palette = ContentPalette::default();
    assert_eq!(palette.foreground, DEFAULT_FG);
    assert_eq!(
        palette,
        ContentPalette::from_theme_palette(&ThemePalette::bitty_dark())
    );
    for token in ContentToken::ALL {
        // Every closed token resolves to an opaque colour.
        assert_eq!(palette.color(token)[3], 0xFF, "{token} must be opaque");
    }
}

#[test]
fn every_content_token_maps_through_the_closed_palette() {
    let palette = ContentPalette {
        foreground: [1, 2, 3, 0xFF],
        accent: [4, 5, 6, 0xFF],
        muted: [7, 8, 9, 0xFF],
        error: [10, 11, 12, 0xFF],
        warning: [13, 14, 15, 0xFF],
        success: [16, 17, 18, 0xFF],
    };
    for token in ContentToken::ALL {
        let scene = scene_of(styled(SceneNodeStyle::new(0, 0, 1, token), text("x")));
        let paint = paint_scene_with_style(
            &scene,
            surface(),
            ScenePaintBudget::default(),
            ScenePaintStyle {
                text: [0, 0, 0, 0xFF],
                rule: [0, 0, 0, 0xFF],
                content: palette,
            },
        )
        .expect("paints");
        let rings = rings(&paint);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].3, palette.color(token), "{token} colour");
    }
}

#[test]
fn border_width_and_radius_are_physical_pixels_inside_the_frame() {
    let style = SceneNodeStyle::new(
        1,
        SCENE_STYLE_MAX_BORDER_PX,
        SCENE_STYLE_MAX_BORDER_PX,
        ContentToken::Warning,
    );
    let paint = paint(styled(style, text("ok")));
    let (rect, border, radius, _) = rings(&paint)[0];
    assert_eq!(border, SCENE_STYLE_MAX_BORDER_PX);
    assert_eq!(radius, SCENE_STYLE_MAX_BORDER_PX);
    // Ring geometry equals the padded frame, not the leaf.
    assert_eq!(
        rect,
        RectPx::new(
            surface().rect.x,
            surface().rect.y,
            4 * cell().width,
            3 * cell().height
        )
    );
}

#[test]
fn border_is_a_stroke_inside_the_frame_and_never_expands_it() {
    // No padding: the border must not reserve layout cells.
    let bare = paint(text("abcd"));
    let ringed = paint(styled(
        SceneNodeStyle::new(0, 0, 9, ContentToken::Accent),
        text("abcd"),
    ));
    assert_eq!(text_ops(&bare), text_ops(&ringed));
    let (rect, _, _, _) = rings(&ringed)[0];
    assert_eq!(
        rect,
        RectPx::new(surface().rect.x, surface().rect.y, 32, 16)
    );
}

#[test]
fn radius_without_a_border_is_inert() {
    let paint = paint(styled(
        SceneNodeStyle::new(0, 12, 0, ContentToken::Accent),
        text("hi"),
    ));
    assert!(rings(&paint).is_empty());
    assert_eq!(text_ops(&paint), vec![(0, 0, "hi".to_string())]);
}

#[test]
fn a_clipped_frame_paints_its_ring_inside_the_leaf() {
    // 60 columns of text in a 40-column leaf: layout clamps the measured frame
    // to the leaf, so the ring is the clamped frame and its op never leaves
    // the leaf even though the producer's line is wider.
    let long = text(&"z".repeat(60));
    let paint = paint(styled(
        SceneNodeStyle::new(0, 4, 1, ContentToken::Accent),
        long,
    ));
    let rings = rings(&paint);
    assert_eq!(rings.len(), 1);
    assert_eq!(
        rings[0].0,
        RectPx::new(surface().rect.x, surface().rect.y, 320, cell().height)
    );
    assert!(surface().aligned_rect().contains_rect(&rings[0].0));
    let line = paint.text_lines().next().expect("clipped text paints");
    assert_eq!(line.text.len(), 40);
}

// ---------------------------------------------------------------------------
// Bounds and budgets
// ---------------------------------------------------------------------------

#[test]
fn style_ops_are_counted_by_the_op_budget() {
    let content = SceneNode::Column(vec![
        styled(
            SceneNodeStyle::new(0, 4, 1, ContentToken::Accent),
            text("a"),
        ),
        styled(
            SceneNodeStyle::new(0, 4, 1, ContentToken::Accent),
            text("b"),
        ),
        styled(
            SceneNodeStyle::new(0, 4, 1, ContentToken::Accent),
            text("c"),
        ),
    ]);
    let scene = scene_of(content);
    let budget = ScenePaintBudget {
        max_ops: 4,
        ..ScenePaintBudget::default()
    };
    let error = paint_scene(&scene, surface(), budget).expect_err("ops cap refuses");
    assert!(matches!(error, ScenePaintError::OpsTooMany { .. }));

    // One ring per bordered node: three rings plus three text ops fit a
    // budget of six exactly.
    let budget = ScenePaintBudget {
        max_ops: 6,
        ..ScenePaintBudget::default()
    };
    let paint = paint_scene(&scene, surface(), budget).expect("fits exactly");
    assert_eq!(rings(&paint).len(), 3);
    assert_eq!(paint.ops().len(), 6);
}

#[test]
fn over_cap_style_is_rejected_at_admission_not_clamped() {
    let style = SceneNodeStyle::new(0, 0, SCENE_STYLE_MAX_BORDER_PX + 1, ContentToken::Accent);
    let error = RichBlock::new(
        BlockId(1),
        BlockAnchor::Zone(1),
        styled(style, text("x")),
        ScrollBehavior::Inline,
        1,
        0,
        1,
    )
    .expect_err("the parent block fails closed");
    assert!(matches!(
        error,
        bitty_rich::scene::SceneError::StyleInvalid { .. }
    ));
}

#[test]
fn style_geometry_is_bounded_and_never_leaves_the_leaf() {
    let paint = paint(SceneNode::Column(vec![
        styled(
            SceneNodeStyle::new(3, 8, 2, ContentToken::Accent),
            text("one"),
        ),
        styled(
            SceneNodeStyle::new(0, 16, 16, ContentToken::Success),
            text("two"),
        ),
    ]));
    let aligned = surface().aligned_rect();
    for op in paint.ops() {
        assert!(
            aligned.contains_rect(&op.rect()),
            "op {:?} left the leaf",
            op.rect()
        );
    }
    let total: u64 = rings(&paint)
        .iter()
        .map(|(rect, ..)| u64::from(rect.width) * u64::from(rect.height))
        .sum();
    assert!(total > 0);
}

// ---------------------------------------------------------------------------
// Accessibility projection
// ---------------------------------------------------------------------------

#[test]
fn style_never_changes_the_text_projection_or_its_order() {
    let plain = paint(SceneNode::Column(vec![text("head"), text("body")]));
    let styled_paint = paint(SceneNode::Column(vec![
        styled(
            SceneNodeStyle::new(0, 4, 1, ContentToken::Accent),
            text("head"),
        ),
        styled(
            SceneNodeStyle::new(1, 0, 0, ContentToken::Foreground),
            text("body"),
        ),
    ]));
    // Same lines, same order. Padding may shift a row/column, but the text
    // itself is identical and no line is hidden by a border or radius.
    let texts = |paint: &ScenePaint| -> Vec<String> {
        paint
            .text_lines()
            .map(|line| line.text.to_string())
            .collect()
    };
    assert_eq!(texts(&plain), vec!["head".to_string(), "body".to_string()]);
    assert_eq!(texts(&styled_paint), texts(&plain));
    assert_eq!(styled_paint.plain_text(), plain.plain_text());
    assert_eq!(text_ops(&styled_paint)[1].2, "body");
}

#[test]
fn style_on_an_unknown_kind_keeps_the_plain_text_fallback() {
    let paint = paint(styled(
        SceneNodeStyle::new(1, 4, 1, ContentToken::Accent),
        SceneNode::Unknown("fallback".to_string()),
    ));
    assert_eq!(text_ops(&paint), vec![(1, 1, "fallback".to_string())]);
    assert_eq!(paint.plain_text(), "fallback");
    assert_eq!(rings(&paint).len(), 1);
}

// ---------------------------------------------------------------------------
// Frame seam
// ---------------------------------------------------------------------------

#[test]
fn rounded_rings_reach_the_draw_list_rounded_pass() {
    let scene = scene_of(styled(
        SceneNodeStyle::new(0, 5, 1, ContentToken::Accent),
        text("x"),
    ));
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let mut renderer = renderer();
    let mut list = empty_draw_list();
    assert_eq!(paint.append_to(&mut renderer, &mut list), 0);
    assert_eq!(
        list.rounded_fills.len(),
        1,
        "the ring joins the compositor's rounded pass"
    );
    let ring = &list.rounded_fills[0];
    assert_eq!(ring.border, 1);
    assert_eq!(ring.radius, 5);
    assert!(!list.glyphs.is_empty(), "text still paints");
}

#[test]
fn append_into_splits_rounded_rings_from_plain_fills() {
    let scene = scene_of(styled(
        SceneNodeStyle::new(0, 5, 1, ContentToken::Accent),
        text("x"),
    ));
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let mut renderer = renderer();
    let mut fills = Vec::new();
    let mut rounded = Vec::new();
    let mut glyphs = Vec::new();
    let images = paint.append_into(&mut renderer, &mut fills, &mut rounded, &mut glyphs);
    assert_eq!(images, 0);
    assert!(fills.is_empty(), "a style ring is not a plain fill");
    assert_eq!(rounded.len(), 1);
    assert!(!glyphs.is_empty());
}
