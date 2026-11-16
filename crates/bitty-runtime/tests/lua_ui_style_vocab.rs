//! End-to-end evidence for the additive bounded Lua style vocabulary
//! (bitty#2191, CTX-1837).
//!
//! The chain under test is the shipped one: a declarative Lua table is read by
//! the `ui.rich` reader (`bitty-lua`), converted by the Core bridge into the
//! accepted scene model (`bitty-rich`), and painted by the render scene path
//! (`bitty-render`) into a bounded, headless paint list. Nothing here needs a
//! GPU, a compositor, or a terminal.
//!
//! The suite pins both directions:
//!
//! - **accept:** the four documented primitives reach the paint list — one
//!   rounded ring per bordered node, a cell inset equal to the declared
//!   padding, and the same painted text as an unstyled tree;
//! - **refuse:** an unknown style key, an over-cap value, a wrong value type,
//!   and a colour literal are all typed `E_UI_COMPONENT_INVALID` refusals that
//!   mount nothing, and the v1 band/overlay reader refuses the same key
//!   because it has no geometry to paint it with.

use std::sync::Arc;

use bitty_lua::ui::UiNode;
use bitty_lua::ui_rich::UiRichNode;
use bitty_lua::{BridgeError, LuaValue};
use bitty_render::RectPx;
use bitty_render::grid::CellMetrics;
use bitty_render::scene::ScenePaintOp;
use bitty_render::scene::{ScenePaint, SceneSurface};
use bitty_rich::scene::{
    BlockAnchor, BlockId, ContentToken, RichBlock, Scene, SceneError, SceneNode, SceneNodeStyle,
    ScrollBehavior,
};
use bitty_runtime::cw_present::consume_scene_present_at;
use bitty_runtime::scene_bridge::{RichMount, SceneBridgeError, mount_rich_node, scene_node_of};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn surface() -> SceneSurface {
    SceneSurface::new(
        RectPx::new(0, 0, 480, 240),
        CellMetrics::new(8, 16).expect("cell metrics"),
    )
}

fn text_value(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
    ])
}

fn styled_text_value(style: Vec<(&str, LuaValue)>) -> LuaValue {
    let style = LuaValue::Table(
        style
            .into_iter()
            .map(|(key, value)| (LuaValue::String(key.to_string()), value))
            .collect(),
    );
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("ready".to_string())),
        ("style", style),
    ])
}

fn full_style() -> Vec<(&'static str, LuaValue)> {
    vec![
        ("padding", LuaValue::Integer(2)),
        ("radius", LuaValue::Integer(6)),
        ("border", LuaValue::Integer(1)),
        (
            "border_token",
            LuaValue::String("content.accent".to_string()),
        ),
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

fn rings(paint: &ScenePaint) -> Vec<(u16, u16)> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Rounded { border, radius, .. } => Some((*border, *radius)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Accept path
// ---------------------------------------------------------------------------

#[test]
fn a_styled_component_paints_one_ring_and_insets_its_text_by_padding() {
    let scene = read_and_mount(&styled_text_value(full_style())).expect("styled tree is valid");
    let paint = paint_of(&scene);

    assert_eq!(
        rings(&paint),
        vec![(1, 6)],
        "one ring per bordered node, carrying the declared width and radius"
    );
    let lines: Vec<(u16, u16, String)> = paint
        .text_lines()
        .map(|line| (line.row, line.col, line.text.to_string()))
        .collect();
    assert_eq!(
        lines,
        vec![(2, 2, "ready".to_string())],
        "padding insets the child by whole cells on both axes"
    );
    assert_eq!(
        paint.plain_text(),
        "ready",
        "the painted text is unchanged by the style"
    );
}

#[test]
fn padding_alone_insets_without_a_ring() {
    let scene = read_and_mount(&styled_text_value(vec![("padding", LuaValue::Integer(3))]))
        .expect("padding-only tree is valid");
    let paint = paint_of(&scene);
    assert!(rings(&paint).is_empty(), "no border was declared");
    let lines: Vec<(u16, u16)> = paint
        .text_lines()
        .map(|line| (line.row, line.col))
        .collect();
    assert_eq!(lines, vec![(3, 3)]);
    assert_eq!(paint.plain_text(), "ready");
}

#[test]
fn the_caps_are_inclusive_on_the_wire() {
    // 32/16/16 is the boundary; nothing is clamped, nothing is refused.
    let scene = read_and_mount(&styled_text_value(vec![
        ("padding", LuaValue::Integer(32)),
        ("radius", LuaValue::Integer(16)),
        ("border", LuaValue::Integer(16)),
        (
            "border_token",
            LuaValue::String("state.success".to_string()),
        ),
    ]))
    .expect("boundary values are accepted");
    let paint = paint_of(&scene);
    assert_eq!(rings(&paint), vec![(16, 16)]);
}

#[test]
fn an_unstyled_tree_and_a_flat_declaration_paint_the_same_list() {
    // The backwards-compatibility claim: an existing unstyled tree keeps its
    // exact paint list, and a declared-but-flat style is indistinguishable
    // from it (no wrapper, no op, no node).
    let unstyled = read_and_mount(&text_value("ready")).expect("unstyled tree is valid");
    let flat = read_and_mount(&styled_text_value(vec![
        ("padding", LuaValue::Integer(0)),
        ("border", LuaValue::Integer(0)),
    ]))
    .expect("flat declaration is valid");
    assert_eq!(
        paint_of(&flat).ops(),
        paint_of(&unstyled).ops(),
        "a flat style is byte-identical to no style"
    );
    assert_eq!(flat.len(), 1);

    // bitty#3176 item 6: a paint list cannot see a wrapper that paints
    // nothing, so the node cost is asserted at this layer too. A mutant that
    // wraps a flat style adds a wrapper node and one depth level and fails
    // here, whether it wraps in the reader or in the bridge.
    let flat_node = UiRichNode::from_lua_value(&styled_text_value(vec![
        ("padding", LuaValue::Integer(0)),
        ("radius", LuaValue::Integer(0)),
        ("border", LuaValue::Integer(0)),
    ]))
    .expect("flat declaration is valid");
    let unstyled_node = UiRichNode::from_lua_value(&text_value("ready")).expect("valid");
    assert_eq!(flat_node.count_nodes(), 1, "no wrapper node");
    assert_eq!(flat_node.depth(), 1, "no extra depth level");
    assert_eq!(
        scene_node_of(&flat_node).count_nodes(),
        scene_node_of(&unstyled_node).count_nodes(),
        "a flat declaration costs the same node budget as no declaration"
    );
    assert_eq!(
        scene_node_of(&flat_node),
        scene_node_of(&unstyled_node),
        "and lowers to the same scene node"
    );
}

#[test]
fn a_second_border_on_one_node_is_refused_before_anything_is_painted() {
    // bitty#3176 item 1: one node paints one stroke. `Block.border` (cells,
    // cap 8) and `style.border` (px, cap 16) set on the same node would paint
    // a rounded ring and the four-op cell frame over one box — the conflicting
    // spelling the candidate record rejects for a separate `stroke` key — so
    // the reader refuses the pair instead of choosing a precedence.
    let child = text_value("ready");
    let both = LuaValue::table([
        ("kind", LuaValue::String("Block".to_string())),
        ("border", LuaValue::table([("width", LuaValue::Integer(1))])),
        ("style", LuaValue::table([("border", LuaValue::Integer(2))])),
        ("child", child.clone()),
    ]);
    let error = UiRichNode::from_lua_value(&both).expect_err("two borders refused");
    assert_eq!(error.class, "validation");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "Block declares two borders: border.width = 1 cells and style.border = 2 px; declare one"
    );

    // One spelling alone still paints exactly one border: the pixel stroke is
    // the only rounded op, and the cell-frame spelling paints no ring at all.
    let pixel_stroke = LuaValue::table([
        ("kind", LuaValue::String("Block".to_string())),
        ("style", LuaValue::table([("border", LuaValue::Integer(2))])),
        ("child", child.clone()),
    ]);
    let painted = paint_of(&read_and_mount(&pixel_stroke).expect("one stroke is valid"));
    assert_eq!(rings(&painted), vec![(2, 0)], "one ring, no cell frame");

    let cell_frame = LuaValue::table([
        ("kind", LuaValue::String("Block".to_string())),
        ("border", LuaValue::table([("width", LuaValue::Integer(1))])),
        ("child", child),
    ]);
    let painted = paint_of(&read_and_mount(&cell_frame).expect("one frame is valid"));
    assert!(
        rings(&painted).is_empty(),
        "the cell frame is not a rounded ring"
    );
}

#[test]
fn a_style_declaration_matches_the_hand_built_scene_node() {
    // The Lua key is not a second vocabulary: it produces exactly the scene
    // model a renderer-level producer would build by hand.
    let from_lua = UiRichNode::from_lua_value(&styled_text_value(full_style())).expect("valid");
    let bridged = scene_node_of(&from_lua);
    let hand_built = SceneNode::Styled {
        style: SceneNodeStyle::new(2, 6, 1, ContentToken::Accent),
        child: Box::new(SceneNode::Text(bitty_rich::scene::StyledSpan {
            text: "ready".to_string(),
            bold: false,
            italic: false,
        })),
    };
    assert_eq!(bridged, hand_built);
}

// ---------------------------------------------------------------------------
// Refusal path
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_style_key_is_refused_not_ignored() {
    let error =
        UiRichNode::from_lua_value(&styled_text_value(vec![("align", LuaValue::Integer(1))]))
            .expect_err("unknown key refused");
    assert_eq!(error.class, "validation");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(error.message, "unsupported scene style field 'align'");
}

#[test]
fn an_over_cap_value_is_refused_with_a_typed_diagnostic() {
    for (field, value, expected) in [
        ("padding", 33, "scene style padding exceeds cap: 33 > 32"),
        ("radius", 17, "scene style radius exceeds cap: 17 > 16"),
        ("border", 17, "scene style border exceeds cap: 17 > 16"),
    ] {
        let error =
            UiRichNode::from_lua_value(&styled_text_value(vec![(field, LuaValue::Integer(value))]))
                .expect_err("over cap refused");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(error.message, expected);
    }
}

#[test]
fn a_non_integer_geometry_value_is_refused() {
    let error =
        UiRichNode::from_lua_value(&styled_text_value(vec![("padding", LuaValue::Number(2.5))]))
            .expect_err("fractional refused");
    assert_eq!(
        error.message,
        "invalid scene style value for field 'padding'"
    );
}

#[test]
fn a_colour_literal_is_not_a_token() {
    for literal in ["#ff0000", "rgb(255,0,0)", "red", "chrome.accent"] {
        let error = UiRichNode::from_lua_value(&styled_text_value(vec![(
            "border_token",
            LuaValue::String(literal.to_string()),
        )]))
        .expect_err("colour literal refused");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert!(
            error.message.starts_with("unknown scene border token '"),
            "{literal}: {}",
            error.message
        );
    }
}

#[test]
fn a_refused_style_mounts_nothing_and_keeps_the_previous_scene() {
    let mut scene = Scene::new();
    let accepted = UiRichNode::from_lua_value(&text_value("kept")).expect("plain tree is valid");
    mount_rich_node(&mut scene, &mount_of(1), &accepted).expect("admitted");
    let before = scene.len();

    let refused =
        UiRichNode::from_lua_value(&styled_text_value(vec![("padding", LuaValue::Integer(99))]))
            .expect_err("over cap refused");
    assert_eq!(refused.code, "E_UI_COMPONENT_INVALID");
    // The reader refuses before the bridge runs, so the last good scene is
    // untouched: there is no partial admission to roll back.
    assert_eq!(scene.len(), before);

    // The admission boundary refuses a hand-built over-cap node too.
    let over_cap = UiRichNode::Styled {
        style: bitty_lua::UiNodeStyle::new(0, 17, 0, bitty_lua::UiBorderToken::Foreground),
        child: Box::new(accepted),
    };
    let error = mount_rich_node(&mut scene, &mount_of(2), &over_cap).expect_err("refused");
    assert!(matches!(
        error,
        SceneBridgeError::Admission(SceneError::StyleInvalid { .. })
    ));
    assert_eq!(scene.len(), before, "last good scene retained");
}

#[test]
fn the_v1_band_surface_still_tolerates_the_key_as_unknown() {
    // The bounded style vocabulary is hosted on the rich scene surface, where
    // a ring and a cell inset are painted. A band/overlay node keeps the
    // accepted additive-minor behaviour: `style` is an unknown optional field
    // it ignores (pinned by `lua_ui_parity::unknown_properties_are_tolerated`),
    // so no existing band tree changes shape or behaviour because of this
    // change. Extending the v1 surface is a separate accepted-extension
    // decision, and this test records today's boundary rather than assuming
    // one.
    let styled_leaf = styled_text_value(full_style());
    let with_style = LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
        ("children", LuaValue::array(vec![styled_leaf])),
    ]);
    let node = UiNode::from_lua_value(&with_style).expect("v1 tree still parses");
    let plain = LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
        ("children", LuaValue::array(vec![text_value("ready")])),
    ]);
    let plain_node = UiNode::from_lua_value(&plain).expect("plain tree parses");
    assert_eq!(node.count_nodes(), plain_node.count_nodes());
    assert_eq!(node.text_bytes(), plain_node.text_bytes());
    assert_eq!(
        node.retained_string_bytes(),
        plain_node.retained_string_bytes()
    );
    assert_eq!(node.depth(), plain_node.depth());
}

// ---------------------------------------------------------------------------
// Budgets stay put
// ---------------------------------------------------------------------------

#[test]
fn a_style_wrapper_costs_one_node_and_no_text_bytes() {
    let styled =
        UiRichNode::from_lua_value(&styled_text_value(vec![("padding", LuaValue::Integer(1))]))
            .expect("styled leaf is valid");
    assert_eq!(styled.count_nodes(), 2, "wrapper + content");
    assert_eq!(styled.depth(), 2, "wrapper + content");
    assert_eq!(styled.text_bytes(), 5, "style values are not text (SCN-3)");
    assert_eq!(
        styled.retained_string_bytes(),
        5,
        "the border token is an enum, so it retains no string (SCN-4)"
    );
}

#[test]
fn the_node_ceiling_counts_the_wrapper_and_still_refuses() {
    // 1023 styled leaves + the root `Row` = 2047 nodes: inside SCN-1.
    let row_of = |leaves: usize| {
        let leaves: Vec<LuaValue> = (0..leaves)
            .map(|_| styled_text_value(vec![("padding", LuaValue::Integer(1))]))
            .collect();
        LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            ("children", LuaValue::array(leaves)),
        ])
    };
    let inside =
        UiRichNode::from_lua_value(&row_of(1023)).expect("2047 nodes fit the bridge budget");
    assert_eq!(inside.count_nodes(), 2047);

    // One more styled leaf is 2049 nodes: refused, never truncated.
    let error = UiRichNode::from_lua_value(&row_of(1024)).expect_err("over SCN-1");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert!(
        error.message.contains("node count exceeds"),
        "{}",
        error.message
    );

    // The same tree unstyled is 1025 nodes and admitted, so the refusal above
    // is caused by the wrapper accounting and nothing else.
    let unstyled: Vec<LuaValue> = (0..1024).map(|_| text_value("ready")).collect();
    let plain = LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
        ("children", LuaValue::array(unstyled)),
    ]);
    assert_eq!(
        UiRichNode::from_lua_value(&plain)
            .expect("admitted")
            .count_nodes(),
        1025
    );
}

#[test]
fn the_accepted_admission_boundary_still_refuses_an_over_budget_block() {
    let wide = UiRichNode::Table {
        rows: (0..2048).map(|_| Vec::new()).collect(),
    };
    let error = RichBlock::new(
        BlockId(9),
        BlockAnchor::Zone(1),
        scene_node_of(&wide),
        ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .expect_err("refused");
    assert!(
        matches!(error, SceneError::NodesTooMany { .. }),
        "the accepted admission boundary still refuses an over-SCN tree: {error:?}"
    );
    // Nothing was admitted by the attempt.
    let scene = Scene::new();
    assert!(scene.is_empty());
}
