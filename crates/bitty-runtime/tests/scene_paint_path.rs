//! End-to-end evidence for the OQ-051 mechanism half (CTX-1628 / bitty#2760).
//!
//! `ui.rich` declarative nodes (bitty-lua) -> admitted `Scene` (bitty-rich) ->
//! bounded paint list clipped to the leaf (bitty-render) -> accessibility
//! projection of the same text runs (bitty-ui). The test also proves the
//! over-budget refusal at insertion holds across repeated mounts and that the
//! whole path is deterministic without a GPU.

use bitty_lua::host::LuaValue;
use bitty_lua::ui_rich::UiRichNode;
use bitty_render::error::RenderError;
use bitty_render::glyph::{
    BitmapFormat, FontId, FontQuery, FontStyle, GlyphBitmap, GlyphMetrics, GlyphRasterizer,
    RasterKey,
};
use bitty_render::grid::SnapshotDamage;
use bitty_render::grid::{CellMetrics, DrawList, GridRenderer};
use bitty_render::scene::{
    ScenePaintBudget, ScenePaintStyle, SceneSurface, paint_scene, paint_scene_with_style,
};
use bitty_render::{ExtentPx, RectPx, frame::plan_frame};
use bitty_rich::scene::{BlockAnchor, BlockId, SceneError, ScrollBehavior};
use bitty_runtime::cw_present::consume_scene_present_at;
use bitty_runtime::scene_bridge::{RichMount, SceneBridgeError, mount_rich_node};
use bitty_ui::a11y_host::{SceneKind, UiSnapshotHost};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn cell() -> CellMetrics {
    CellMetrics::new(8, 16).expect("cell metrics")
}

fn surface() -> SceneSurface {
    SceneSurface::new(RectPx::new(16, 32, 320, 192), cell())
}

fn lua_text(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
    ])
}

fn lua_array(values: Vec<LuaValue>) -> LuaValue {
    LuaValue::array(values)
}

/// One `ui.rich` scene exercising every accepted node kind.
fn rich_scene_value() -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Column".to_string())),
        (
            "children",
            lua_array(vec![
                lua_text("status"),
                LuaValue::table([
                    ("kind", LuaValue::String("Row".to_string())),
                    (
                        "children",
                        lua_array(vec![lua_text("cpu"), lua_text("42%")]),
                    ),
                ]),
                LuaValue::table([
                    ("kind", LuaValue::String("Block".to_string())),
                    ("border", LuaValue::table([("width", LuaValue::Integer(1))])),
                    ("child", lua_text("boxed")),
                ]),
                LuaValue::table([
                    ("kind", LuaValue::String("CodeBlock".to_string())),
                    ("lang", LuaValue::String("rust".to_string())),
                    ("content", LuaValue::String("fn main() {}".to_string())),
                ]),
                LuaValue::table([
                    ("kind", LuaValue::String("Table".to_string())),
                    (
                        "rows",
                        lua_array(vec![
                            lua_array(vec![
                                LuaValue::String("name".to_string()),
                                LuaValue::String("value".to_string()),
                            ]),
                            lua_array(vec![
                                LuaValue::String("alpha".to_string()),
                                LuaValue::String("1".to_string()),
                            ]),
                        ]),
                    ),
                ]),
                LuaValue::table([
                    ("kind", LuaValue::String("List".to_string())),
                    (
                        "children",
                        lua_array(vec![lua_text("first"), lua_text("second")]),
                    ),
                ]),
                LuaValue::table([("kind", LuaValue::String("Rule".to_string()))]),
            ]),
        ),
    ])
}

fn mount_of(id: u64) -> RichMount {
    RichMount {
        id: BlockId(id),
        anchor: BlockAnchor::Grid {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 0,
        },
        scroll: ScrollBehavior::Inline,
        owner: 1,
        generation: 1,
        created_at: id,
    }
}

fn mount(scene: &mut bitty_rich::scene::Scene, id: u64, node: &UiRichNode) {
    mount_rich_node(scene, &mount_of(id), node).expect("rich node admitted");
}

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
    GridRenderer::new(FakeRasterizer { next: 0 }, &query, cell()).unwrap()
}

fn empty_list() -> DrawList {
    let snapshot = bitty_term_state::State::new().snapshot();
    let damage = bitty_term_state::Damage {
        generation: 0,
        regions: Vec::new().into_boxed_slice(),
    };
    DrawList {
        generation: 0,
        atlas_epoch: 0,
        plan: plan_frame(&SnapshotDamage::new(&snapshot, &damage, cell())),
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
// Tests
// ---------------------------------------------------------------------------

#[test]
fn rich_mount_paints_bounded_scene_and_projects_the_same_text() {
    let node = UiRichNode::from_lua_value(&rich_scene_value()).expect("rich node validates");
    let mut scene = bitty_rich::scene::Scene::new();
    mount(&mut scene, 1, &node);

    let present = consume_scene_present_at(&scene, surface(), 64);
    assert!(present.refusal.is_none(), "{:?}", present.refusal);
    assert_eq!(present.block_ids, vec![BlockId(1)]);

    let bound = surface().aligned_rect();
    for op in present.paint.ops() {
        assert!(
            bound.contains_rect(&op.rect()),
            "paint op left the leaf content rect"
        );
    }
    for damage in present.paint.damage() {
        assert!(bound.contains_rect(damage));
    }

    // Paint projection: the plain-text and accessibility views of the scene
    // are exactly the painted lines, in paint order.
    let painted: Vec<(u32, String)> = present
        .paint
        .text_lines()
        .map(|line| (u32::from(line.row), line.text.to_string()))
        .collect();
    assert!(!painted.is_empty());

    let mut host = UiSnapshotHost::new("window", "panel");
    let borrowed: Vec<(u32, &str)> = painted
        .iter()
        .map(|(row, text)| (*row, text.as_str()))
        .collect();
    host.push_scene_text(SceneKind::Column, 1, &borrowed)
        .expect("bounded scene text");
    let projected: Vec<(u32, String)> = host.scene_text().to_vec();
    assert_eq!(projected, painted, "painted text == projected text");
    assert_eq!(
        present.paint.plain_text(),
        painted
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        "the plain-text export is the same projection"
    );

    // Headless frame: the paint list becomes fills and glyphs through the CPU
    // text seam, with every glyph inside the leaf.
    let mut list = empty_list();
    let mut renderer = renderer();
    assert_eq!(present.paint.append_to(&mut renderer, &mut list), 0);
    assert!(!list.glyphs.is_empty());
    for glyph in &list.glyphs {
        let dest = RectPx::new(
            glyph.dest[0],
            glyph.dest[1],
            glyph.size[0].max(1),
            glyph.size[1].max(1),
        );
        assert!(surface().rect.intersection(&dest).is_some());
    }
}

#[test]
fn over_budget_rich_mount_is_refused_at_insertion_without_growth() {
    // A table whose structural accounting exceeds SCN-1 (a table row and a
    // cell each allocate, so an empty-cell table still presses the bound).
    let huge = UiRichNode::Table {
        rows: (0..2048).map(|_| Vec::new()).collect(),
    };
    let mut scene = bitty_rich::scene::Scene::new();
    mount(
        &mut scene,
        1,
        &UiRichNode::Text {
            text: "kept".to_string(),
            bold: false,
        },
    );
    let generation = scene.generation();
    let blocks = scene.len();
    let bytes = scene.aggregated_bytes();
    for _ in 0..64 {
        let error = mount_rich_node(&mut scene, &mount_of(2), &huge).expect_err("refused");
        assert!(matches!(
            error,
            SceneBridgeError::Admission(SceneError::NodesTooMany { .. })
        ));
    }
    assert_eq!(scene.generation(), generation);
    assert_eq!(scene.len(), blocks);
    assert_eq!(scene.aggregated_bytes(), bytes);

    // The retained scene still paints exactly as before.
    let present = consume_scene_present_at(&scene, surface(), 64);
    assert!(present.refusal.is_none());
    assert_eq!(present.paint.plain_text(), "kept");
}

#[test]
fn scene_paint_is_deterministic_and_has_no_theme_or_gpu_input() {
    let node = UiRichNode::from_lua_value(&rich_scene_value()).expect("rich node validates");
    let mut scene = bitty_rich::scene::Scene::new();
    mount(&mut scene, 1, &node);

    let first = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    let second = paint_scene(&scene, surface(), ScenePaintBudget::default()).expect("paints");
    assert_eq!(first, second, "pure function of scene and surface");

    // The default style is the explicit one: nothing reads a live theme, a
    // config layer, or `bitty --safe` decoration state on this path.
    let styled = paint_scene_with_style(
        &scene,
        surface(),
        ScenePaintBudget::default(),
        ScenePaintStyle::default(),
    )
    .expect("paints");
    assert_eq!(first, styled);

    // Damage stays inside the leaf and inside the surface extent the frame
    // planner will clip against.
    let extent = ExtentPx::new(640, 480);
    for damage in first.damage() {
        assert!(surface().rect.contains_rect(damage));
        assert!(extent.contains_rect(damage));
    }
}

#[test]
fn leaf_clipping_holds_for_a_scene_much_larger_than_the_leaf() {
    let wide = "very-wide-line-".repeat(64);
    let node = UiRichNode::Column(
        (0..64)
            .map(|_| UiRichNode::Text {
                text: wide.clone(),
                bold: false,
            })
            .collect(),
    );
    let mut scene = bitty_rich::scene::Scene::new();
    mount(&mut scene, 1, &node);
    let present = consume_scene_present_at(&scene, surface(), 64);
    assert!(present.refusal.is_none());
    let bound = surface().aligned_rect();
    let mut rows = 0u16;
    for op in present.paint.ops() {
        assert!(bound.contains_rect(&op.rect()));
        if let bitty_render::scene::ScenePaintOp::Text { cols, .. } = op {
            assert!(*cols <= surface().cols());
            rows += 1;
        }
    }
    assert_eq!(
        rows,
        surface().rows(),
        "row overflow is clipped, not painted"
    );
}

#[test]
fn clipped_rich_line_projects_only_the_painted_cells() {
    // A plugin can mount far more text than the leaf holds; the a11y
    // projection must read the clipped op, never the producer's full string.
    let full = format!("visible{}{}", "x".repeat(50), "SECRET");
    let node = UiRichNode::Text {
        text: full.clone(),
        bold: false,
    };
    let mut scene = bitty_rich::scene::Scene::new();
    mount(&mut scene, 1, &node);
    let present = consume_scene_present_at(&scene, surface(), 64);
    assert!(present.refusal.is_none());
    let line = present.paint.text_lines().next().expect("one painted line");
    assert_eq!(line.text, &full[..40], "clipped to the 40-cell leaf width");
    assert!(!line.text.contains("SECRET"));

    let mut host = UiSnapshotHost::new("window", "panel");
    host.push_scene_text(SceneKind::Text, 1, &[(u32::from(line.row), line.text)])
        .expect("bounded scene text");
    assert_eq!(
        host.scene_text(),
        &[(0, full[..40].to_string())],
        "the accessibility projection carries only painted bytes"
    );
}

#[test]
fn paint_cap_sheds_the_tail_from_the_paint_list_too() {
    let mut scene = bitty_rich::scene::Scene::new();
    for id in 1..=8u64 {
        mount(
            &mut scene,
            id,
            &UiRichNode::Text {
                text: format!("line {id}"),
                bold: false,
            },
        );
    }
    let present = consume_scene_present_at(&scene, surface(), 3);
    assert!(present.refusal.is_none());
    assert_eq!(present.block_ids.len(), 3);
    assert_eq!(present.shed, 5);
    assert_eq!(present.paint.plain_text(), "line 1\nline 2\nline 3");
    assert_eq!(present.paint.block_count(), 3);
}
