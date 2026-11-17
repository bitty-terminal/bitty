//! End-to-end evidence for the bounded content-height vocabulary on the
//! `ui.rich` scene style table (bitty#2191, CTX-1867).
//!
//! The chain under test is the shipped one: a declarative Lua table is read by
//! the `ui.rich` reader (`bitty-lua`), converted by the Core bridge into the
//! accepted scene model (`bitty-runtime::scene_bridge`), admitted through the
//! accepted `SCN` boundary (`bitty-rich`), and laid out and painted by the
//! render scene path (`bitty-render`) into a bounded, headless paint list.
//! Nothing here needs a GPU, a compositor, or a terminal.
//!
//! Every layout assertion below reads the **painted** list — cell rows, cell
//! columns, cell runs, and the rounded ring rectangle — not a structure the
//! test built itself, so a mutant that parses the key but never applies it
//! fails here. The suite pins both directions:
//!
//! - **accept:** `min_height` raises the painted frame and the row a following
//!   sibling starts on, `max_height` clips the rows a child paints into and
//!   pulls the next sibling up, and both compose with `padding` as a content
//!   box;
//! - **compatibility:** an unstyled tree and a tree declaring a `0`/`0` height
//!   range paint the same ops, in the same order, at the same coordinates;
//! - **refuse:** an unknown key (including a bare `height`), a wrong value
//!   type, an over-cap height, and an inverted range are typed
//!   `E_UI_COMPONENT_INVALID` refusals that mount nothing and keep the previous
//!   scene.

use std::sync::Arc;

use bitty_lua::ui_rich::UiRichNode;
use bitty_lua::{BridgeError, LuaValue};
use bitty_render::RectPx;
use bitty_render::grid::CellMetrics;
use bitty_render::scene::{ScenePaint, ScenePaintOp, SceneSurface};
use bitty_rich::scene::{
    BlockAnchor, BlockId, RichBlock, Scene, SceneError, SceneNode, ScrollBehavior,
};
use bitty_runtime::cw_present::consume_scene_present_at;
use bitty_runtime::scene_bridge::{RichMount, SceneBridgeError, mount_rich_node, scene_node_of};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// 480x240 physical pixels at 8x16 per cell: a 60-column, 15-row leaf.
fn surface() -> SceneSurface {
    SceneSurface::new(
        RectPx::new(0, 0, 480, 240),
        CellMetrics::new(8, 16).expect("cell metrics"),
    )
}

const SURFACE_ROWS: u16 = 15;

fn text_value(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
    ])
}

fn column_value(children: Vec<LuaValue>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Column".to_string())),
        ("children", LuaValue::array(children)),
    ])
}

fn style_table(style: Vec<(&str, LuaValue)>) -> LuaValue {
    LuaValue::Table(
        style
            .into_iter()
            .map(|(key, value)| (LuaValue::String(key.to_string()), value))
            .collect(),
    )
}

/// One `Text` leaf carrying the given style pairs.
fn styled_text_value(text: &str, style: Vec<(&str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(text.to_string())),
        ("style", style_table(style)),
    ])
}

fn height_range(min: i64, max: i64) -> Vec<(&'static str, LuaValue)> {
    vec![
        ("min_height", LuaValue::Integer(min)),
        ("max_height", LuaValue::Integer(max)),
    ]
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

/// Reads one Lua value through the shipped rich reader and mounts it.
fn read_and_mount(value: &LuaValue) -> Result<Scene, BridgeError> {
    let node = UiRichNode::from_lua_value(value)?;
    let mut scene = Scene::new();
    mount_rich_node(&mut scene, &mount_of(1), &node).expect("admitted");
    Ok(scene)
}

/// The paint list of one mounted tree, as the present path produces it.
fn paint_of(scene: &Scene) -> Arc<ScenePaint> {
    let present = consume_scene_present_at(scene, surface(), 256);
    assert!(
        present.refusal.is_none(),
        "scene refused at present: {:?}",
        present.refusal
    );
    Arc::clone(&present.paint)
}

/// Painted text runs as `(row, col, text)`, in op order.
fn lines(paint: &ScenePaint) -> Vec<(u16, u16, String)> {
    paint
        .text_lines()
        .map(|line| (line.row, line.col, line.text.to_string()))
        .collect()
}

/// Painted rounded rings as `(x px, y px, width px, height px)`.
fn rings(paint: &ScenePaint) -> Vec<(i32, i32, u32, u32)> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Rounded { rect, .. } => Some((rect.x, rect.y, rect.width, rect.height)),
            _ => None,
        })
        .collect()
}

/// Ring heights in physical pixels, in op order.
fn ring_heights(paint: &ScenePaint) -> Vec<u32> {
    rings(paint).into_iter().map(|ring| ring.3).collect()
}

// ---------------------------------------------------------------------------
// Accept path: the height reaches the painted layout
// ---------------------------------------------------------------------------

#[test]
fn max_height_clips_the_painted_rows_and_pulls_the_next_sibling_up() {
    // "a\nb\nc" measures 3 rows; a 1-row cap clips the painted run to "a" and
    // makes the following sibling start on row 1 instead of row 3. Both facts
    // are read from the paint list, so a reader that stores the key without the
    // measure path applying it paints three rows and puts "Z" on row 3.
    let tree = column_value(vec![
        styled_text_value("a\nb\nc", vec![("max_height", LuaValue::Integer(1))]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("a height cap is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (1, 0, "Z".to_string())],
        "the cap narrows both the painted rows and the column advance"
    );

    // The unbounded reading of the same tree: the cap is what moved the
    // sibling, not the tree shape.
    let uncapped = column_value(vec![text_value("a\nb\nc"), text_value("Z")]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "a".to_string()),
            (1, 0, "b".to_string()),
            (2, 0, "c".to_string()),
            (3, 0, "Z".to_string())
        ]
    );
}

#[test]
fn min_height_raises_the_frame_and_pushes_the_next_sibling_down() {
    // "a" measures 1 row; a 3-row floor makes the node three rows tall, so the
    // ring frames three rows and the next sibling starts on row 3. The floor
    // never invents content: the run stays "a".
    let tree = column_value(vec![
        styled_text_value(
            "a",
            vec![
                ("min_height", LuaValue::Integer(3)),
                ("border", LuaValue::Integer(1)),
            ],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("a height floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (3, 0, "Z".to_string())],
        "the floor widens the measured box without inventing content"
    );
    assert_eq!(
        ring_heights(&paint),
        vec![3 * 16],
        "the ring frames the floored content box (3 rows of 16 px)"
    );
}

#[test]
fn a_height_range_is_a_content_box_inside_padding_and_inside_the_ring() {
    // padding = 1 on each side, content floor 4, border 1: the node's extent is
    // 4 + 1 + 1 = 6 rows, the text starts on row 1, the sibling on row 6, and
    // the ring frames exactly that box. A mutant that applies the height to the
    // padded frame (or the padding inside the height) moves one of these.
    let tree = column_value(vec![
        styled_text_value(
            "a",
            vec![
                ("padding", LuaValue::Integer(1)),
                ("border", LuaValue::Integer(1)),
                ("min_height", LuaValue::Integer(4)),
            ],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("padding + floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(1, 1, "a".to_string()), (6, 0, "Z".to_string())],
        "the floor is the content box: padding sits outside it"
    );
    assert_eq!(
        ring_heights(&paint),
        vec![6 * 16],
        "the ring frames the constrained content box plus padding (px)"
    );
}

#[test]
fn a_max_height_cap_paints_only_the_rows_it_allows() {
    // The cap is on the node's content box, so a capped node paints the rows it
    // allows and nothing else — the existing walk's clip, not a truncation of
    // the text the plugin declared (the model still carries all three lines).
    let tree = styled_text_value("a\nb\nc", height_range(0, 2));
    let paint = paint_of(&read_and_mount(&tree).expect("a cap alone is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (1, 0, "b".to_string()),],
        "the third row is clipped at the declared content box"
    );
}

#[test]
fn a_floor_taller_than_the_leaf_is_clamped_to_the_surface() {
    // The declared floor is bounded by the leaf, not by the plugin: 1000 rows on
    // a 15-row surface measures 15, so the sibling after it is placed past the
    // edge and never painted. The declaration can never widen the surface.
    let tree = column_value(vec![
        styled_text_value("a", vec![("min_height", LuaValue::Integer(1000))]),
        text_value("B"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("the cap itself is valid"));
    assert_eq!(SURFACE_ROWS, 15, "fixture surface height in cells");
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string())],
        "the floor clamps to the leaf, so the next sibling falls outside it"
    );
}

#[test]
fn a_height_bound_alone_still_costs_one_wrapper_node() {
    // A height-only declaration sizes the node, so it is not flat and lowers to
    // the additive wrapper: one node and one depth level under `SCN-1`/`SCN-2`,
    // and no text bytes under `SCN-3`.
    let styled = UiRichNode::from_lua_value(&styled_text_value(
        "ab",
        vec![("max_height", LuaValue::Integer(4))],
    ))
    .expect("valid");
    assert_eq!(styled.count_nodes(), 2, "wrapper + content");
    assert_eq!(styled.depth(), 2, "wrapper + content");
    assert_eq!(styled.text_bytes(), 2, "a height is not text");

    let plain = UiRichNode::from_lua_value(&text_value("ab")).expect("valid");
    assert_eq!(plain.count_nodes(), 1, "an unstyled leaf is still one node");
    assert_eq!(
        scene_node_of(&styled).count_nodes(),
        scene_node_of(&plain).count_nodes() + 1,
        "the bridge carries the wrapper, it does not drop the bound"
    );
}

// ---------------------------------------------------------------------------
// Backward compatibility: an unstyled tree is byte-identical
// ---------------------------------------------------------------------------

#[test]
fn an_unstyled_tree_and_a_zero_height_declaration_paint_the_same_list() {
    let unstyled = column_value(vec![text_value("ab"), text_value("cd")]);
    let zero_range = column_value(vec![
        styled_text_value("ab", height_range(0, 0)),
        text_value("cd"),
    ]);
    let unstyled_paint = paint_of(&read_and_mount(&unstyled).expect("valid"));
    let zero_paint = paint_of(&read_and_mount(&zero_range).expect("valid"));
    assert_eq!(
        unstyled_paint.ops(),
        zero_paint.ops(),
        "a 0/0 height range is invisible: same ops, same order, same coordinates"
    );
    assert_eq!(
        lines(&unstyled_paint),
        vec![(0, 0, "ab".to_string()), (1, 0, "cd".to_string())],
        "the pre-change top-to-bottom flow is unchanged"
    );
}

#[test]
fn a_height_declaration_never_moves_a_sibling_on_the_other_axis() {
    // The vertical pair bounds the node's own extent only: it must not touch the
    // horizontal flow, which is the width pair's and `gap`'s business.
    let flat = column_value(vec![
        styled_text_value("ab", height_range(1, 3)),
        text_value("cd"),
    ]);
    let paint = paint_of(&read_and_mount(&flat).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ab".to_string()), (1, 0, "cd".to_string())],
        "a height range at the measured value changes no coordinate"
    );
}

// ---------------------------------------------------------------------------
// Refusal path: loud, typed, and nothing mounted
// ---------------------------------------------------------------------------

#[test]
fn the_grid_dimension_cap_is_inclusive_on_the_wire() {
    let at_cap = styled_text_value("ab", height_range(1000, 1000));
    assert!(
        read_and_mount(&at_cap).is_ok(),
        "1000 cells is the inclusive cap"
    );

    for (field, expected) in [
        (
            "min_height",
            "scene style min_height exceeds cap: 1001 > 1000",
        ),
        (
            "max_height",
            "scene style max_height exceeds cap: 1001 > 1000",
        ),
    ] {
        let node = styled_text_value("ab", vec![(field, LuaValue::Integer(1001))]);
        let error = UiRichNode::from_lua_value(&node).expect_err("one past the cap is refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(error.message, expected);
    }
}

#[test]
fn an_inverted_height_range_is_refused_not_reordered() {
    let node = styled_text_value("ab", height_range(9, 4));
    let error = UiRichNode::from_lua_value(&node).expect_err("an empty range is refused");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "scene style height range is inverted: min_height 9 > max_height 4"
    );

    // Equal bounds are a legal fixed height, not an inverted range.
    assert!(
        read_and_mount(&styled_text_value("ab", height_range(3, 3))).is_ok(),
        "min_height == max_height is a fixed height"
    );
}

#[test]
fn a_wrong_value_type_is_refused() {
    for value in [
        LuaValue::Number(3.0),
        LuaValue::Number(3.5),
        LuaValue::Bool(true),
        LuaValue::String("3".to_string()),
        LuaValue::Integer(-1),
        LuaValue::table([("cells", LuaValue::Integer(3))]),
    ] {
        let node = styled_text_value("ab", vec![("min_height", value)]);
        let error = UiRichNode::from_lua_value(&node).expect_err("refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(
            error.message,
            "invalid scene style value for field 'min_height'"
        );
    }
}

#[test]
fn the_keys_outside_the_set_are_still_refused_loudly() {
    // The vocabulary is the `min_height`/`max_height` pair, so a bare `height`
    // (CSS-like) and a suffixed `min_height_cells` stay outside the closed set:
    // one concept must not have two spellings, and a refusal must not silently
    // accept a declaration Core would never apply.
    for key in ["height", "min_height_cells", "max_height_cells", "size"] {
        let node = styled_text_value("ab", vec![(key, LuaValue::Integer(1))]);
        let error = UiRichNode::from_lua_value(&node).expect_err("refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(
            error.message,
            format!("unsupported scene style field '{key}'")
        );
    }
}

#[test]
fn a_refused_height_declaration_mounts_no_block_and_keeps_the_previous_scene() {
    let mut scene = Scene::new();
    let accepted = UiRichNode::from_lua_value(&text_value("kept")).expect("valid");
    mount_rich_node(&mut scene, &mount_of(1), &accepted).expect("admitted");
    let before = scene.get(BlockId(1)).expect("admitted").content().clone();

    let refused = styled_text_value("ab", height_range(9, 4));
    let error = UiRichNode::from_lua_value(&refused).expect_err("refused at the reader");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(scene.len(), 1, "the refused declaration added no block");
    assert_eq!(
        scene.get(BlockId(1)).expect("admitted").content(),
        &before,
        "the last good scene is untouched"
    );

    // A hand-built over-cap style is refused by the accepted admission boundary
    // too, so the wire parser is not the only gate.
    let over_cap = UiRichNode::Styled {
        style: bitty_lua::UiNodeStyle::new(0, 0, 0, bitty_lua::UiBorderToken::Foreground)
            .with_height_range(0, 1001),
        child: Box::new(accepted),
    };
    let error = mount_rich_node(&mut scene, &mount_of(2), &over_cap).expect_err("refused");
    assert!(matches!(
        error,
        SceneBridgeError::Admission(SceneError::StyleInvalid { .. })
    ));
    assert_eq!(scene.len(), 1, "the refused block was never stored");
}

#[test]
fn an_inverted_height_range_hand_built_for_the_model_is_refused_at_admission() {
    // The model-level validator is the second gate: a producer that bypasses the
    // Lua bridge cannot admit an empty range either.
    let inverted = SceneNode::Styled {
        style: bitty_rich::scene::SceneNodeStyle::new(
            0,
            0,
            0,
            bitty_rich::scene::ContentToken::Foreground,
        )
        .with_height_range(9, 4),
        child: Box::new(SceneNode::Text(bitty_rich::scene::StyledSpan {
            text: "ab".to_string(),
            bold: false,
            italic: false,
        })),
    };
    let error = RichBlock::new(
        BlockId(7),
        BlockAnchor::Zone(7),
        inverted,
        ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .expect_err("an empty range is refused");
    assert!(
        matches!(error, SceneError::StyleInvalid { .. }),
        "admission refuses the inverted range: {error:?}"
    );
}
