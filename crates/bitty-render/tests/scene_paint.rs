//! Scene paint path: bounded layout, leaf clipping, damage, and headless glyphs.
//!
//! Evidence for the OQ-051 mechanism half (CTX-1628 / bitty#2760): an admitted
//! `Scene` becomes a paint list clipped to the leaf content rect, a scene
//! revision contributes a bounded invalidation rectangle, and over-budget
//! scenes are refused atomically instead of being silently clamped.

use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::{CellMetrics, DrawList, GridRenderer, SnapshotDamage};
use bitty_render::scene::{
    SCENE_PAINT_MAX_BORDER_CELLS, SCENE_PAINT_MAX_OPS, ScenePaintBudget, ScenePaintCache,
    ScenePaintError, ScenePaintOp, SceneSurface, paint_scene,
};
use bitty_render::{ExtentPx, RectPx, frame::plan_frame};
use bitty_rich::image::PlacementId;
use bitty_rich::scene::{
    BlockAnchor, BlockId, Border, CodeBlockModel, ListModel, RichBlock,
    SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH, Scene, SceneNode, ScrollBehavior, StyledSpan,
    TableModel,
};

/// Accepted per-terminal block ceiling (`SCN-5`).
const SCENE_ACCEPTED_BLOCKS: usize = SCENE_MAX_BLOCKS_PER_TERMINAL;

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
        if key.character == ' ' {
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
    .expect("fixture fits SCN-1..3")
}

fn scene_of(blocks: Vec<RichBlock>) -> Scene {
    let mut scene = Scene::new();
    for block in blocks {
        scene.insert(block).expect("fixture fits SCN-1..5");
    }
    scene
}

fn inside(rect: &RectPx, bound: &RectPx) -> bool {
    bound.contains_rect(rect) && !rect.is_empty()
}

// ---------------------------------------------------------------------------
// Per-node-kind paint
// ---------------------------------------------------------------------------

#[test]
fn every_v1_node_kind_paints_inside_the_leaf() {
    let content = SceneNode::Column(vec![
        text("heading"),
        SceneNode::Row(vec![text("left"), text("right")]),
        SceneNode::Block {
            border: Some(Border {
                width: 1,
                color: String::new(),
            }),
            child: Box::new(text("boxed")),
        },
        SceneNode::CodeBlock(CodeBlockModel {
            lang: Some("rust".to_string()),
            content: "fn main() {}".to_string(),
        }),
        SceneNode::Table(TableModel {
            rows: vec![
                vec!["name".to_string(), "value".to_string()],
                vec!["alpha".to_string(), "1".to_string()],
            ],
        }),
        SceneNode::List(ListModel {
            items: vec!["first".to_string(), "second".to_string()],
            ordered: true,
        }),
        SceneNode::Rule,
    ]);
    let scene = scene_of(vec![block(1, content)]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");

    let bound = surface().aligned_rect();
    for op in paint.ops() {
        assert!(
            inside(&op.rect(), &bound),
            "op {:?} must stay inside the leaf rect {bound:?}",
            op.rect()
        );
    }
    for damage in paint.damage() {
        assert!(inside(damage, &bound), "damage must stay inside the leaf");
    }

    let lines: Vec<&str> = paint.text_lines().map(|line| line.text).collect();
    assert_eq!(
        lines,
        vec![
            "heading",
            "left",
            "right",
            "boxed",
            "rust",
            "fn main() {}",
            "name | value",
            "alpha | 1",
            "1. first",
            "2. second",
        ],
        "each kind contributes its declared text in declaration order"
    );

    // Border segments and the rule are fills; text nodes are text ops.
    assert!(
        paint
            .ops()
            .iter()
            .any(|op| matches!(op, ScenePaintOp::Fill { .. }))
    );
    assert_eq!(paint.node_count(), count_nodes(&scene));
    assert_eq!(paint.block_count(), 1);
    assert!(!paint.is_empty());
}

fn count_nodes(scene: &Scene) -> usize {
    scene.iter().map(RichBlock::node_count).sum()
}

#[test]
fn image_node_reserves_a_row_without_painting_pixels() {
    let mut scene = Scene::new();
    scene
        .insert(block(1, SceneNode::Image(PlacementId(7))))
        .expect("fixture fits");
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(paint.image_count(), 1);
    assert!(
        paint
            .ops()
            .iter()
            .all(|op| !matches!(op, ScenePaintOp::Fill { .. } | ScenePaintOp::Text { .. }))
    );
    let rect = paint.ops()[0].rect();
    assert_eq!(rect.height, cell().height, "one reserved content row");
    assert!(inside(&rect, &surface().aligned_rect()));
}

#[test]
fn unknown_variant_paints_bounded_plain_text() {
    let scene = scene_of(vec![block(1, SceneNode::Unknown("fallback".to_string()))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(paint.plain_text(), "fallback");
}

#[test]
fn blocks_compose_top_to_bottom_in_scene_order() {
    let scene = scene_of(vec![
        block(1, text("one")),
        block(2, text("two")),
        block(3, text("three")),
    ]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let rows: Vec<u16> = paint.text_lines().map(|line| line.row).collect();
    assert_eq!(rows, vec![0, 1, 2]);
    assert_eq!(paint.plain_text(), "one\ntwo\nthree");
}

// ---------------------------------------------------------------------------
// Clipping
// ---------------------------------------------------------------------------

#[test]
fn oversized_scene_clips_without_writing_outside_the_leaf() {
    let wide = "x".repeat(400);
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column((0..64).map(|_| text(&wide)).collect()),
    )]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let bound = surface().aligned_rect();
    assert!(!paint.ops().is_empty(), "visible part still paints");
    let mut rows = 0u16;
    for op in paint.ops() {
        assert!(inside(&op.rect(), &bound), "clipped op left the leaf");
        if let ScenePaintOp::Text { cols, .. } = op {
            assert!(
                *cols <= surface().cols(),
                "line never exceeds the leaf column span"
            );
            rows += 1;
        }
    }
    assert_eq!(rows, surface().rows(), "row overflow is clipped");
    for damage in paint.damage() {
        assert!(inside(damage, &bound));
    }
}

#[test]
fn surface_outside_the_cell_lattice_is_untouched() {
    // 319x191 px at 8x16 cells: 39 full columns, 11 full rows, with a
    // remainder the scene path must never paint.
    let ragged = SceneSurface::new(RectPx::new(16, 32, 319, 191), cell());
    let scene = scene_of(vec![block(1, text("hello"))]);
    let paint = paint_scene(&scene, ragged, ScenePaintBudget::default()).expect("paints");
    let aligned = ragged.aligned_rect();
    assert_eq!(aligned.width, 39 * 8);
    assert_eq!(aligned.height, 11 * 16);
    for op in paint.ops() {
        assert!(inside(&op.rect(), &aligned));
    }
}

// ---------------------------------------------------------------------------
// Budget refusal
// ---------------------------------------------------------------------------

#[test]
fn over_budget_scenes_are_refused_atomically() {
    let scene = scene_of(vec![block(1, text("hello"))]);

    for (budget, expected) in [
        (
            ScenePaintBudget {
                max_blocks: 0,
                ..ScenePaintBudget::default()
            },
            "blocks",
        ),
        (
            ScenePaintBudget {
                max_ops: 0,
                ..ScenePaintBudget::default()
            },
            "ops",
        ),
        (
            ScenePaintBudget {
                max_text_bytes: 1,
                ..ScenePaintBudget::default()
            },
            "text",
        ),
    ] {
        let error = paint_scene(&scene, surface(), budget).expect_err("refused");
        match (expected, &error) {
            ("blocks", ScenePaintError::BlocksTooMany { .. })
            | ("ops", ScenePaintError::OpsTooMany { .. })
            | ("text", ScenePaintError::TextTooLarge { .. }) => {}
            other => panic!("unexpected refusal: {other:?}"),
        }
    }
}

#[test]
fn repeated_refusals_do_not_grow_paint_or_scene_state() {
    let scene = scene_of(vec![block(1, text("hello"))]);
    let generation = scene.generation();
    let blocks = scene.len();
    let bytes = scene.aggregated_bytes();
    let good = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    for _ in 0..64 {
        assert!(paint_scene(&scene, surface(), ScenePaintBudget::nothing()).is_err());
    }
    assert_eq!(scene.generation(), generation);
    assert_eq!(scene.len(), blocks);
    assert_eq!(scene.aggregated_bytes(), bytes);
    let again = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(good, again, "a refusal never changes the next good paint");
}

#[test]
fn border_width_over_the_paint_cap_is_refused_not_clamped() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Block {
            border: Some(Border {
                width: SCENE_PAINT_MAX_BORDER_CELLS + 1,
                color: String::new(),
            }),
            child: Box::new(text("inside")),
        },
    )]);
    assert_eq!(
        paint_scene(&scene, surface(), ScenePaintBudget::default()),
        Err(ScenePaintError::BorderTooWide {
            width: SCENE_PAINT_MAX_BORDER_CELLS + 1,
            cap: SCENE_PAINT_MAX_BORDER_CELLS,
        })
    );
}

#[test]
fn maximum_accepted_depth_paints_without_recursion_failure() {
    let mut node = text("leaf");
    for _ in 0..(SCENE_MAX_DEPTH - 1) {
        node = SceneNode::Column(vec![node]);
    }
    assert_eq!(node.depth(), SCENE_MAX_DEPTH);
    let scene = scene_of(vec![block(1, node)]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(paint.plain_text(), "leaf");
}

// ---------------------------------------------------------------------------
// Damage integration
// ---------------------------------------------------------------------------

#[test]
fn scene_damage_enters_the_frame_planner_without_leaving_the_leaf() {
    let scene = scene_of(vec![block(1, text("hello"))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(paint.damage().len(), 1, "one bounded invalidation rect");

    let extent = ExtentPx::new(800, 600);
    let damage = SnapshotDamage::new(
        &bitty_term_state::State::new().snapshot(),
        &bitty_term_state::Damage {
            generation: 0,
            regions: Vec::new().into_boxed_slice(),
        },
        cell(),
    )
    .with_extra_regions(paint.damage().iter().copied());
    let plan = plan_frame(&damage);
    assert!(plan.needs_draw());
    assert_eq!(
        plan.dirty_rects,
        paint.damage(),
        "same rect reaches the plan"
    );
    for rect in &plan.dirty_rects {
        assert!(extent.contains_rect(rect));
    }
}

#[test]
fn outside_cells_are_not_in_the_damage_region() {
    let scene = scene_of(vec![block(1, text("x"))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let bound = surface().aligned_rect();
    let damage = paint.damage()[0];
    // Well outside every leaf edge.
    for probe in [
        RectPx::new(0, 0, 8, 8),
        RectPx::new(400, 32, 8, 16),
        RectPx::new(16, 300, 8, 16),
        RectPx::new(400, 300, 8, 16),
    ] {
        assert!(
            damage.intersection(&probe).is_none(),
            "damage must not cover cells outside the leaf"
        );
        assert!(!bound.contains_rect(&probe));
    }
}

// ---------------------------------------------------------------------------
// Headless / safe determinism
// ---------------------------------------------------------------------------

#[test]
fn painting_is_deterministic_and_needs_no_gpu() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column(vec![text("alpha"), text("beta")]),
    )]);
    let first = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let second = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(first, second);

    // The whole paint list renders through the CPU text seam with a
    // deterministic fake rasterizer: no adapter, device, or surface exists.
    let mut first_renderer = renderer();
    let mut list = DrawList {
        generation: scene.generation(),
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
    };
    let images = first.append_to(&mut first_renderer, &mut list);
    assert_eq!(images, 0);
    assert!(!list.glyphs.is_empty(), "headless text rasterizes");

    let mut second_list = list.clone();
    second_list.glyphs.clear();
    let mut second_renderer = renderer();
    let images = second.append_to(&mut second_renderer, &mut second_list);
    assert_eq!(images, 0);
    assert_eq!(list.glyphs, second_list.glyphs, "glyph output is stable");

    // Every glyph origin lands inside the leaf content rect.
    let bound = surface().rect;
    for glyph in &list.glyphs {
        let dest = RectPx::new(
            glyph.dest[0],
            glyph.dest[1],
            glyph.size[0].max(1),
            glyph.size[1].max(1),
        );
        assert!(
            bound.intersection(&dest).is_some(),
            "glyph at {dest:?} left the leaf rect"
        );
    }
}

#[test]
fn paint_cache_rebuilds_only_on_generation_or_surface_change() {
    let mut scene = scene_of(vec![block(1, text("one"))]);
    let mut cache = ScenePaintCache::new();
    let first = cache
        .get_or_paint(&scene, surface(), ScenePaintBudget::default())
        .expect("paints")
        .clone();

    scene
        .replace_content(BlockId(1), text("two"))
        .expect("replace fits");
    let second = cache
        .get_or_paint(&scene, surface(), ScenePaintBudget::default())
        .expect("paints after revision")
        .clone();
    assert_ne!(first.plain_text(), second.plain_text());
    assert_eq!(second.plain_text(), "two");

    let unchanged = cache
        .get_or_paint(&scene, surface(), ScenePaintBudget::default())
        .expect("cached")
        .clone();
    assert_eq!(unchanged, second, "same generation reuses the list");

    let moved = SceneSurface::new(RectPx::new(0, 0, 320, 192), cell());
    let repainted = cache
        .get_or_paint(&scene, moved, ScenePaintBudget::default())
        .expect("surface change repaints")
        .clone();
    assert_ne!(repainted.damage(), second.damage());
}

#[test]
fn plain_text_is_the_painted_line_projection() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column(vec![
            text("first"),
            SceneNode::Table(TableModel {
                rows: vec![vec!["a".to_string(), "b".to_string()]],
            }),
        ]),
    )]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let painted: Vec<&str> = paint.text_lines().map(|line| line.text).collect();
    assert_eq!(painted.join("\n"), paint.plain_text());
    assert_eq!(paint.plain_text(), "first\na | b");
}

#[test]
fn empty_scene_paints_nothing_and_damages_nothing() {
    let paint = paint_scene(&Scene::new(), surface(), ScenePaintBudget::default()).expect("empty");
    assert!(paint.is_empty());
    assert!(paint.damage().is_empty());
    assert_eq!(paint.plain_text(), "");
}

// ---------------------------------------------------------------------------
// Sibling layout (M1): a bordered child keeps its measured extent
// ---------------------------------------------------------------------------

fn bordered(child: SceneNode) -> SceneNode {
    SceneNode::Block {
        border: Some(Border {
            width: 1,
            color: String::new(),
        }),
        child: Box::new(child),
    }
}

fn text_ops(paint: &bitty_render::scene::ScenePaint) -> Vec<(u16, u16, String)> {
    paint
        .text_lines()
        .map(|line| (line.row, line.col, line.text.to_string()))
        .collect()
}

fn fill_rects(paint: &bitty_render::scene::ScenePaint) -> Vec<RectPx> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Fill { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect()
}

#[test]
fn block_in_row_does_not_overlap_its_sibling() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Row(vec![bordered(text("aa")), text("bb")]),
    )]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(
        text_ops(&paint),
        vec![(1, 1, "aa".to_string()), (0, 4, "bb".to_string())],
        "the boxed child stays at its measured 4-column frame; the sibling starts after it"
    );

    let fills = fill_rects(&paint);
    let x0 = surface().rect.x;
    let y0 = surface().rect.y;
    let frame = RectPx::new(x0, y0, 4 * cell().width, 3 * cell().height);
    for fill in &fills {
        assert!(
            frame.contains_rect(fill),
            "border segment {fill:?} left the measured frame {frame:?}"
        );
    }
    let sibling = RectPx::new(
        x0 + i32::try_from(4 * cell().width).expect("test px"),
        y0,
        2 * cell().width,
        cell().height,
    );
    for fill in &fills {
        assert!(
            fill.intersection(&sibling).is_none(),
            "border segment {fill:?} overlaps the sibling text cell {sibling:?}"
        );
    }
    // The sibling text starts exactly where the border frame ends: adjacent,
    // not overlapping and not separated.
    let text = paint
        .ops()
        .iter()
        .find_map(|op| match op {
            ScenePaintOp::Text { rect, text, .. } if text == "bb" => Some(*rect),
            _ => None,
        })
        .expect("sibling text op");
    assert_eq!(
        text.x,
        x0 + i32::try_from(4 * cell().width).expect("test px")
    );
}

#[test]
fn block_in_column_does_not_overlap_its_sibling() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column(vec![bordered(text("aa")), text("bb")]),
    )]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(
        text_ops(&paint),
        vec![(1, 1, "aa".to_string()), (3, 0, "bb".to_string())],
        "the boxed child stays at its measured 3-row frame; the sibling starts on row 3"
    );

    let x0 = surface().rect.x;
    let y0 = surface().rect.y;
    let frame = RectPx::new(x0, y0, 4 * cell().width, 3 * cell().height);
    let sibling = RectPx::new(
        x0,
        y0 + i32::try_from(3 * cell().height).expect("test px"),
        2 * cell().width,
        cell().height,
    );
    for fill in fill_rects(&paint) {
        assert!(
            frame.contains_rect(&fill),
            "border segment {fill:?} left the measured frame {frame:?}"
        );
        assert!(
            fill.intersection(&sibling).is_none(),
            "border segment {fill:?} overlaps the sibling row {sibling:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Text clipping (M2): the op stores the painted cells only
// ---------------------------------------------------------------------------

#[test]
fn clipped_text_lines_store_only_the_painted_cells() {
    let narrow = "x".repeat(100);
    let wide = "字".repeat(30);
    let mixed = format!("a{}", "字".repeat(30));
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column(vec![text(&narrow), text(&wide), text(&mixed), text("tail")]),
    )]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let cols = surface().cols();
    assert_eq!(cols, 40);
    assert_eq!(
        text_ops(&paint),
        vec![
            (0, 0, "x".repeat(40)),
            (1, 0, "字".repeat(20)),
            (2, 0, format!("a{}", "字".repeat(19))),
            (3, 0, "tail".to_string()),
        ],
        "narrow, wide, and mixed lines clip to the visible cells"
    );
    for op in paint.ops() {
        if let ScenePaintOp::Text { text, cols, .. } = op {
            let cells: usize = text
                .chars()
                .map(|ch| usize::from(bitty_term_state::char_cell_width(ch)))
                .sum();
            assert!(
                cells <= usize::from(*cols),
                "stored line '{text}' needs {cells} cells but the op reserves {cols}"
            );
        }
    }
    // The accessible/plain-text projection reads the same clipped bytes: a
    // producer cannot append invisible text past the clip edge.
    assert_eq!(
        paint.plain_text(),
        [
            "x".repeat(40),
            "字".repeat(20),
            format!("a{}", "字".repeat(19)),
            "tail".to_string(),
        ]
        .join("\n")
    );
}

#[test]
fn clipped_line_keeps_glyph_seam_parity() {
    let line = format!("abcdef{}", "字".repeat(40));
    let scene = scene_of(vec![block(1, text(&line))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let painted = paint.text_lines().next().expect("one line");
    assert!(
        painted.text.len() < line.len(),
        "the fixture must actually clip"
    );

    let mut paint_list = empty_draw_list();
    let mut paint_renderer = renderer();
    paint.append_to(&mut paint_renderer, &mut paint_list);
    let op = paint
        .ops()
        .iter()
        .find_map(|op| match op {
            ScenePaintOp::Text {
                rect, cols, color, ..
            } => Some((*rect, *cols, *color)),
            _ => None,
        })
        .expect("text op");
    let mut direct_renderer = renderer();
    let direct = direct_renderer.overlay_text_glyphs(
        painted.text,
        (op.0.x, op.0.y),
        usize::from(op.1),
        op.2,
    );
    assert_eq!(
        paint_list.glyphs, direct,
        "append_to feeds the seam exactly the stored (clipped) line"
    );
    assert!(!direct.is_empty(), "the clipped line still paints");
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

// ---------------------------------------------------------------------------
// Cache key (M5)
// ---------------------------------------------------------------------------

#[test]
fn cache_rekeys_on_budget_flip() {
    let scene = scene_of(vec![block(1, text("hello"))]);
    let mut cache = ScenePaintCache::new();
    cache
        .get_or_paint(&scene, surface(), ScenePaintBudget::default())
        .expect("default budget paints");
    assert!(
        cache
            .get_or_paint(&scene, surface(), ScenePaintBudget::nothing())
            .is_err(),
        "a narrowed budget must not reuse the cached success"
    );
    assert!(cache.paint().is_none());
    let repainted = cache
        .get_or_paint(&scene, surface(), ScenePaintBudget::default())
        .expect("the widened budget must not reuse the cached refusal");
    assert_eq!(repainted.plain_text(), "hello");

    // The reverse order: a cached refusal must not survive a widening.
    let mut cache = ScenePaintCache::new();
    assert!(
        cache
            .get_or_paint(&scene, surface(), ScenePaintBudget::nothing())
            .is_err()
    );
    assert_eq!(
        cache
            .get_or_paint(&scene, surface(), ScenePaintBudget::default())
            .expect("widened budget paints")
            .plain_text(),
        "hello"
    );
}

#[test]
fn cache_distinguishes_scenes_at_the_same_generation() {
    let first = scene_of(vec![block(1, text("first"))]);
    let second = scene_of(vec![block(1, text("second"))]);
    assert_eq!(
        first.generation(),
        second.generation(),
        "the fixture proves generation alone cannot separate the scenes"
    );
    assert_ne!(
        first.identity(),
        second.identity(),
        "each scene carries its own identity token"
    );
    let mut cache = ScenePaintCache::new();
    assert_eq!(
        cache
            .get_or_paint(&first, surface(), ScenePaintBudget::default())
            .expect("paints")
            .plain_text(),
        "first"
    );
    assert_eq!(
        cache
            .get_or_paint(&second, surface(), ScenePaintBudget::default())
            .expect("paints the other scene")
            .plain_text(),
        "second",
        "a different scene at the same generation repaints"
    );
}

// ---------------------------------------------------------------------------
// Surface validation (M6)
// ---------------------------------------------------------------------------

#[test]
fn zero_cell_metrics_paint_nothing_instead_of_panicking() {
    let scene = scene_of(vec![block(1, text("hello"))]);
    for (cell, cols, rows) in [
        (
            CellMetrics {
                width: 0,
                height: 0,
            },
            0,
            0,
        ),
        (
            CellMetrics {
                width: 8,
                height: 0,
            },
            40,
            0,
        ),
        (
            CellMetrics {
                width: 0,
                height: 16,
            },
            0,
            12,
        ),
    ] {
        let surface = SceneSurface::new(surface().rect, cell);
        assert_eq!(surface.cols(), cols, "cols for {cell:?}");
        assert_eq!(surface.rows(), rows, "rows for {cell:?}");
        assert_eq!(surface.aligned_rect().width, 0, "no aligned columns");
        let paint = paint_scene(&scene, surface, ScenePaintBudget::default())
            .expect("a degenerate surface is an empty frame, not a refusal");
        assert!(paint.is_empty());
        assert!(paint.damage().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Border clipping (minor)
// ---------------------------------------------------------------------------

#[test]
fn block_edges_beyond_the_leaf_are_dropped_not_clamped() {
    // A bordered block taller than the leaf: its top edge is real, its bottom
    // edge is outside and must not be drawn on the last visible row.
    let tall = SceneNode::Column((0..20).map(|_| text("row")).collect());
    let scene = scene_of(vec![block(1, bordered(tall))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let aligned = surface().aligned_rect();
    let last_row = RectPx::new(
        aligned.x,
        aligned.y + i32::try_from(aligned.height).expect("test px")
            - i32::try_from(cell().height).expect("test px"),
        aligned.width,
        cell().height,
    );
    let fills = fill_rects(&paint);
    assert!(!fills.is_empty(), "the top edge still paints");
    assert!(
        fills.iter().any(|fill| fill.y == aligned.y
            && fill.height == cell().height
            && fill.width > cell().width),
        "the top edge is a real edge and still paints: {fills:?}"
    );
    assert!(
        fills
            .iter()
            .filter(|fill| fill.intersection(&last_row).is_some())
            .all(|fill| fill.width <= cell().width),
        "only the continuing vertical sides reach the clip boundary; no horizontal edge is drawn there: {fills:?}"
    );
}

// ---------------------------------------------------------------------------
// Ops budget, images, and hostile origins
// ---------------------------------------------------------------------------

#[test]
fn tall_text_block_is_refused_at_the_op_cap() {
    let lines = SCENE_PAINT_MAX_OPS + 1;
    let content = "\n".repeat(SCENE_PAINT_MAX_OPS);
    let surface = SceneSurface::new(
        RectPx::new(
            0,
            0,
            cell().width,
            cell()
                .height
                .saturating_mul(u32::try_from(lines).expect("cap fits u32")),
        ),
        cell(),
    );
    let scene = scene_of(vec![block(1, text(&content))]);
    let error = paint_scene(&scene, surface, ScenePaintBudget::default()).expect_err("refused");
    match error {
        ScenePaintError::OpsTooMany { count, cap } => {
            assert_eq!(cap, SCENE_PAINT_MAX_OPS);
            assert_eq!(count, SCENE_PAINT_MAX_OPS + 1);
        }
        other => panic!("unexpected refusal: {other:?}"),
    }
}

#[test]
fn many_small_blocks_are_refused_at_the_op_cap() {
    // Many admitted blocks that each emit a few ops: the cap holds on the
    // composed frame, not only on one huge text node.
    let blocks = SCENE_ACCEPTED_BLOCKS;
    let lines_per_block = 300usize;
    let content = (0..lines_per_block)
        .map(|_| "x")
        .collect::<Vec<_>>()
        .join("\n");
    let scene = scene_of(
        (1..=blocks as u64)
            .map(|id| block(id, text(&content)))
            .collect(),
    );
    assert_eq!(scene.len(), blocks);
    let surface = SceneSurface::new(
        RectPx::new(
            0,
            0,
            cell().width,
            cell()
                .height
                .saturating_mul(u32::try_from(blocks * lines_per_block).expect("fits")),
        ),
        cell(),
    );
    let error = paint_scene(&scene, surface, ScenePaintBudget::default()).expect_err("refused");
    match error {
        ScenePaintError::OpsTooMany { count, cap } => {
            assert_eq!(cap, SCENE_PAINT_MAX_OPS);
            assert_eq!(count, SCENE_PAINT_MAX_OPS + 1);
        }
        other => panic!("unexpected refusal: {other:?}"),
    }
}

#[test]
fn image_ops_are_counted_by_append_to_and_paint_no_pixels() {
    let scene = scene_of(vec![block(1, SceneNode::Image(PlacementId(9)))]);
    let paint = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let mut list = empty_draw_list();
    let mut renderer = renderer();
    assert_eq!(paint.append_to(&mut renderer, &mut list), 1);
    assert!(
        list.fills.is_empty() && list.glyphs.is_empty() && list.images.is_empty(),
        "an image op reserves a row and adds no pixels"
    );
}

#[test]
fn negative_and_saturating_origins_stay_inside_the_leaf() {
    let scene = scene_of(vec![block(
        1,
        SceneNode::Column(vec![text("above"), text("left")]),
    )]);
    for rect in [
        RectPx::new(-40, -32, 320, 192),
        RectPx::new(i32::MAX - 8, i32::MAX - 16, 320, 192),
        RectPx::new(i32::MIN, i32::MIN, 320, 192),
    ] {
        let surface = SceneSurface::new(rect, cell());
        let paint = paint_scene(&scene, surface, ScenePaintBudget::default())
            .expect("a hostile origin is not a refusal");
        let aligned = surface.aligned_rect();
        for op in paint.ops() {
            assert!(
                op.rect().intersection(&aligned).is_some(),
                "op {:?} must intersect the leaf {aligned:?}",
                op.rect()
            );
        }
    }
}
