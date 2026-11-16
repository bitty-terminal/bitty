//! End-to-end evidence for the bounded content-width vocabulary on the
//! `ui.rich` scene style table (bitty#2191, CTX-1848).
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
//! - **accept:** `max_width` clips the painted run and the `Row` advance,
//!   `min_width` widens the measured box and pushes the next sibling, and the
//!   range composes with `padding` as a content box;
//! - **compatibility:** an unstyled tree and a tree declaring a `0`/`0` range
//!   paint the same ops, in the same order, at the same coordinates;
//! - **refuse:** an unknown key, a wrong value type, an over-cap width, and an
//!   inverted range are typed `E_UI_COMPONENT_INVALID` refusals that mount
//!   nothing and keep the previous scene.

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

const SURFACE_COLS: u16 = 60;

fn text_value(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
    ])
}

fn row_value(children: Vec<LuaValue>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
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

fn width_range(min: i64, max: i64) -> Vec<(&'static str, LuaValue)> {
    vec![
        ("min_width", LuaValue::Integer(min)),
        ("max_width", LuaValue::Integer(max)),
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

// ---------------------------------------------------------------------------
// Accept path: the bound reaches the painted layout
// ---------------------------------------------------------------------------

#[test]
fn max_width_clips_the_painted_run_and_the_row_advance() {
    // "ready" is 5 cells; a 3-cell cap clips the painted run to "rea" and makes
    // the following sibling start at column 3 instead of 5. Both facts are read
    // from the paint list, so a reader that stores the key without the measure
    // path applying it produces (0, 0, "ready") and (0, 5, "X") instead.
    let tree = row_value(vec![
        styled_text_value("ready", vec![("max_width", LuaValue::Integer(3))]),
        text_value("X"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("a width cap is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "rea".to_string()), (0, 3, "X".to_string()),],
        "the cap narrows both the painted run and the row advance"
    );

    // The unbounded reading of the same tree: the cap is what moved the
    // sibling, not the tree shape.
    let uncapped = row_value(vec![text_value("ready"), text_value("X")]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ready".to_string()), (0, 5, "X".to_string()),]
    );
}

#[test]
fn min_width_widens_the_box_and_pushes_the_next_sibling() {
    // "ab" measures 2 cells; a 6-cell floor widens the node so the next sibling
    // starts at column 6. The floor never adds glyphs: the run stays "ab".
    let tree = row_value(vec![
        styled_text_value("ab", vec![("min_width", LuaValue::Integer(6))]),
        text_value("cd"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("a width floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ab".to_string()), (0, 6, "cd".to_string())],
        "the floor widens the measured box without inventing content"
    );
    assert!(
        rings(&paint).is_empty(),
        "a width bound paints no ring on its own"
    );
}

#[test]
fn a_width_range_is_a_content_box_inside_padding_and_inside_the_ring() {
    // padding = 2 on each side, content floor 6: the node's extent is 6 + 2 + 2
    // = 10 cells, the text starts at column 2, the sibling at column 10, and
    // the ring frames exactly that box. A mutant that applies the width to the
    // padded frame (or the padding inside the width) moves one of these.
    let tree = row_value(vec![
        styled_text_value(
            "ab",
            vec![
                ("padding", LuaValue::Integer(2)),
                ("border", LuaValue::Integer(1)),
                ("min_width", LuaValue::Integer(6)),
            ],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("padding + floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(2, 2, "ab".to_string()), (0, 10, "Z".to_string())],
        "the floor is the content box: padding sits outside it"
    );
    assert_eq!(
        rings(&paint),
        vec![(0, 0, 10 * 8, 5 * 16)],
        "the ring frames the constrained content box plus padding (px)"
    );
}

#[test]
fn a_floor_wider_than_the_leaf_is_clamped_to_the_surface() {
    // The declared floor is bounded by the leaf, not by the plugin: 1000 cells
    // on a 60-column surface measures 60, so the sibling after it is placed
    // past the edge and never painted. The declaration can never widen the
    // surface.
    let tree = row_value(vec![
        styled_text_value("a", vec![("min_width", LuaValue::Integer(1000))]),
        text_value("B"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("the cap itself is valid"));
    assert_eq!(SURFACE_COLS, 60, "fixture surface width in cells");
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string())],
        "the floor clamps to the leaf, so the next sibling falls outside it"
    );
}

#[test]
fn a_width_bound_alone_still_costs_one_wrapper_node() {
    // A width-only declaration sizes the node, so it is not flat and lowers to
    // the additive wrapper: one node and one depth level under `SCN-1`/`SCN-2`,
    // and no text bytes under `SCN-3`. The rendered result proves the wrapper
    // is not a paint op (nothing extra is drawn).
    let styled = UiRichNode::from_lua_value(&styled_text_value(
        "ab",
        vec![("max_width", LuaValue::Integer(4))],
    ))
    .expect("valid");
    assert_eq!(styled.count_nodes(), 2, "wrapper + content");
    assert_eq!(styled.depth(), 2, "wrapper + content");
    assert_eq!(styled.text_bytes(), 2, "a width is not text");

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
fn an_unstyled_tree_and_a_zero_width_declaration_paint_the_same_list() {
    let unstyled = row_value(vec![text_value("ab"), text_value("cd")]);
    let zero_range = row_value(vec![
        styled_text_value("ab", width_range(0, 0)),
        text_value("cd"),
    ]);
    let unstyled_paint = paint_of(&read_and_mount(&unstyled).expect("valid"));
    let zero_paint = paint_of(&read_and_mount(&zero_range).expect("valid"));
    assert_eq!(
        unstyled_paint.ops(),
        zero_paint.ops(),
        "a 0/0 range is invisible: same ops, same order, same coordinates"
    );
    assert_eq!(
        lines(&unstyled_paint),
        vec![(0, 0, "ab".to_string()), (0, 2, "cd".to_string())],
        "the pre-change left-to-right flow is unchanged"
    );
}

#[test]
fn the_padding_only_layout_is_unchanged_by_this_vocabulary() {
    // The shipped four-primitive behaviour is a regression surface for this
    // change: padding still insets by whole cells on both axes and does not
    // touch the sibling flow it did not touch before.
    let tree = row_value(vec![
        styled_text_value("ab", vec![("padding", LuaValue::Integer(1))]),
        text_value("cd"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(1, 1, "ab".to_string()), (0, 4, "cd".to_string())],
        "padding insets the content and the padded box advances the row"
    );
}

// ---------------------------------------------------------------------------
// Refusal path: loud, typed, and nothing mounted
// ---------------------------------------------------------------------------

#[test]
fn the_grid_dimension_cap_is_inclusive_on_the_wire() {
    let at_cap = styled_text_value("ab", width_range(1000, 1000));
    assert!(
        read_and_mount(&at_cap).is_ok(),
        "1000 cells is the inclusive cap"
    );

    for (pair, expected) in [
        (
            ("min_width", 1001),
            "scene style min_width exceeds cap: 1001 > 1000",
        ),
        (
            ("max_width", 1001),
            "scene style max_width exceeds cap: 1001 > 1000",
        ),
    ] {
        let (field, value) = pair;
        let node = styled_text_value("ab", vec![(field, LuaValue::Integer(value))]);
        let error = UiRichNode::from_lua_value(&node).expect_err("one past the cap is refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(error.message, expected);
    }
}

#[test]
fn an_inverted_width_range_is_refused_not_reordered() {
    let node = styled_text_value("ab", width_range(9, 4));
    let error = UiRichNode::from_lua_value(&node).expect_err("an empty range is refused");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "scene style width range is inverted: min_width 9 > max_width 4"
    );
}

#[test]
fn a_wrong_value_type_is_refused() {
    for value in [
        LuaValue::Number(3.0),
        LuaValue::Bool(true),
        LuaValue::String("3".to_string()),
        LuaValue::Integer(-1),
    ] {
        let node = styled_text_value("ab", vec![("max_width", value)]);
        let error = UiRichNode::from_lua_value(&node).expect_err("refused");
        assert_eq!(
            error.message,
            "invalid scene style value for field 'max_width'"
        );
    }
}

#[test]
fn the_keys_outside_the_set_are_still_refused_loudly() {
    // `gap` and `align` are the two layout vocabularies this lane deliberately
    // did not land: they are outside the closed key set, and the refusal names
    // them instead of accepting a declaration that would paint nothing.
    for key in ["gap", "align", "spacing", "width", "min_width_cells"] {
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
fn a_refused_width_declaration_mounts_no_block_and_keeps_the_previous_scene() {
    let mut scene = Scene::new();
    let accepted = UiRichNode::from_lua_value(&text_value("kept")).expect("valid");
    mount_rich_node(&mut scene, &mount_of(1), &accepted).expect("admitted");
    let before = scene.get(BlockId(1)).expect("admitted").content().clone();

    let refused = styled_text_value("ab", width_range(9, 4));
    let error = UiRichNode::from_lua_value(&refused).expect_err("refused at the reader");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(scene.len(), 1, "the refused declaration added no block");
    assert_eq!(
        scene.get(BlockId(1)).expect("admitted").content(),
        &before,
        "the last good scene is untouched"
    );

    // A hand-built over-cap style is refused by the accepted admission
    // boundary too, so the wire parser is not the only gate.
    let over_cap = UiRichNode::Styled {
        style: bitty_lua::UiNodeStyle::new(0, 0, 0, bitty_lua::UiBorderToken::Foreground)
            .with_width_range(0, 1001),
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
fn an_inverted_range_hand_built_for_the_model_is_refused_at_admission() {
    // The model-level validator is the second gate: a producer that bypasses the
    // Lua bridge cannot admit an empty range either.
    let inverted = SceneNode::Styled {
        style: bitty_rich::scene::SceneNodeStyle::new(
            0,
            0,
            0,
            bitty_rich::scene::ContentToken::Foreground,
        )
        .with_width_range(9, 4),
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
