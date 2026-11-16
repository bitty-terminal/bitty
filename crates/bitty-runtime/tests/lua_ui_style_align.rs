//! End-to-end evidence for the cross-axis child-alignment primitive on the
//! `ui.rich` scene style table (bitty#2191).
//!
//! The chain under test is the shipped one: a declarative Lua table is read by
//! the `ui.rich` reader (`bitty-lua`), converted by the Core bridge into the
//! accepted scene model (`bitty-runtime::scene_bridge`), admitted through the
//! accepted `SCN` boundary (`bitty-rich`), and laid out and painted by the
//! render scene path (`bitty-render`) into a bounded, headless paint list.
//! Nothing here needs a GPU, a compositor, or a terminal.
//!
//! Every assertion below reads the **painted** list — cell rows, cell columns,
//! text runs, and ring rectangles — not a structure the test built itself, so a
//! mutant that parses the key but never offsets a child fails here. The suite
//! pins both directions:
//!
//! - **accept:** `center` and `end` move a child on the axis its siblings do not
//!   advance — horizontally inside a `Column`/`List`, vertically inside a
//!   `Row` — inside the box the node declared, composing with `padding`,
//!   `gap`, `min_width`, `max_width`, and the border ring;
//! - **compatibility:** a declared `start` and an absent `align` paint the same
//!   ops, in the same order, at the same coordinates, and cost no wrapper node;
//! - **no overflow:** slack is saturating, so a child as large as its box stays
//!   at offset zero and clips at the box edge exactly as it does today;
//! - **refuse:** a non-`start` value on a leaf or single-child kind, the band
//!   distribution spelling `space-between`, a wrong value type, an unknown
//!   spelling, and an oversized value are typed `E_UI_COMPONENT_INVALID`
//!   refusals that mount nothing and keep the previous scene.

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
use bitty_runtime::scene_bridge::{RichMount, SceneBridgeError, mount_rich_node};

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

fn text_value(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
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

/// One container node of the given kind carrying the given style pairs.
fn container_value(kind: &str, children: Vec<LuaValue>, style: Vec<(&str, LuaValue)>) -> LuaValue {
    let mut fields = vec![
        ("kind", LuaValue::String(kind.to_string())),
        ("children", LuaValue::array(children)),
    ];
    if !style.is_empty() {
        fields.push(("style", style_table(style)));
    }
    LuaValue::Table(
        fields
            .into_iter()
            .map(|(key, value)| (LuaValue::String(key.to_string()), value))
            .collect(),
    )
}

/// A `Column` style declaration: `align` plus any extra pairs.
fn column_style(
    align: &'static str,
    extra: Vec<(&'static str, LuaValue)>,
) -> Vec<(&'static str, LuaValue)> {
    let mut style = vec![("align", LuaValue::String(align.to_string()))];
    style.extend(extra);
    style
}

fn align_of(align: &str) -> LuaValue {
    LuaValue::String(align.to_string())
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

/// Every painted border ring as `(x, y, width, height)` in physical pixels.
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

fn text_run(row: u16, col: u16, text: &str) -> (u16, u16, String) {
    (row, col, text.to_string())
}

// ---------------------------------------------------------------------------
// Accept path: the placement reaches the painted layout
// ---------------------------------------------------------------------------

#[test]
fn an_aligned_column_centres_its_children_inside_its_content_box() {
    // `min_width = 9` is what makes horizontal slack exist on a Column: the
    // children measure 1 and 3 cells, so the box the plugin declared is 9 cells
    // and the offsets are 4 and 3. A reader that stores the key without the
    // walk applying it paints both runs at column 0.
    let column = container_value(
        "Column",
        vec![text_value("a"), text_value("abc")],
        column_style("center", vec![("min_width", LuaValue::Integer(9))]),
    );
    let paint = paint_of(&read_and_mount(&column).expect("an aligned Column is valid"));
    assert_eq!(
        lines(&paint),
        vec![text_run(0, 4, "a"), text_run(1, 3, "abc")],
        "each child is centred in the 9-cell content box, not packed left"
    );

    // The unwrapped tree is the pre-change left-to-right/zero-offset flow.
    let plain = container_value(
        "Column",
        vec![text_value("a"), text_value("abc")],
        Vec::new(),
    );
    let paint = paint_of(&read_and_mount(&plain).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![text_run(0, 0, "a"), text_run(1, 0, "abc")]
    );
}

#[test]
fn an_aligned_column_pushes_its_children_to_the_far_edge() {
    let column = container_value(
        "Column",
        vec![text_value("a"), text_value("abc")],
        column_style("end", vec![("min_width", LuaValue::Integer(9))]),
    );
    let paint = paint_of(&read_and_mount(&column).expect("an aligned Column is valid"));
    assert_eq!(
        lines(&paint),
        vec![text_run(0, 8, "a"), text_run(1, 6, "abc")],
        "every child ends flush with the far edge of the content box"
    );
}

#[test]
fn an_aligned_list_composes_like_a_column() {
    // The rich `List` is the v1 spelling of a vertical container (the bridge
    // maps it to `SceneNode::Column`), so it takes the same placement.
    let list = container_value(
        "List",
        vec![text_value("a"), text_value("abc")],
        column_style("end", vec![("min_width", LuaValue::Integer(5))]),
    );
    let paint = paint_of(&read_and_mount(&list).expect("an aligned List is valid"));
    assert_eq!(
        lines(&paint),
        vec![text_run(0, 4, "a"), text_run(1, 2, "abc")],
        "the List places its items on the same axis a Column does"
    );
}

#[test]
fn an_aligned_row_places_its_children_on_the_vertical_axis() {
    // A Row walks left to right, so its placement axis is vertical: the
    // three-row run sets the row height and the one-row run is centred or
    // pushed to the bottom of it. A walk that applied the placement on the
    // horizontal axis would move the second run's column instead.
    let plain = container_value(
        "Row",
        vec![text_value("a\nb\nc"), text_value("x")],
        Vec::new(),
    );
    let paint = paint_of(&read_and_mount(&plain).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            text_run(0, 0, "a"),
            text_run(1, 0, "b"),
            text_run(2, 0, "c"),
            text_run(0, 1, "x"),
        ],
        "the pre-change flow tops every sibling out"
    );

    let centered = container_value(
        "Row",
        vec![text_value("a\nb\nc"), text_value("x")],
        vec![("align", align_of("center"))],
    );
    let paint = paint_of(&read_and_mount(&centered).expect("an aligned Row is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            text_run(0, 0, "a"),
            text_run(1, 0, "b"),
            text_run(2, 0, "c"),
            text_run(1, 1, "x"),
        ],
        "the one-row run is centred in the three-row content box"
    );

    let ended = container_value(
        "Row",
        vec![text_value("a\nb\nc"), text_value("x")],
        vec![("align", align_of("end"))],
    );
    let paint = paint_of(&read_and_mount(&ended).expect("an aligned Row is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            text_run(0, 0, "a"),
            text_run(1, 0, "b"),
            text_run(2, 0, "c"),
            text_run(2, 1, "x"),
        ],
        "the one-row run ends flush with the bottom of the content box"
    );
}

#[test]
fn alignment_composes_with_padding_and_sibling_gap() {
    // padding = 1, gap = 1, min_width = 5, align = end: the content box is 5
    // cells starting at column 1. "a" (1 cell) is offset 4 and "bb" (2 cells)
    // offset 3, and the gap puts the second row on row 3. The container's own
    // extent is 1 + 5 + 1 = 7 cells, so the ring around it is 7 cells wide and
    // the placement stays inside it.
    let column = container_value(
        "Column",
        vec![text_value("a"), text_value("bb")],
        column_style(
            "end",
            vec![
                ("padding", LuaValue::Integer(1)),
                ("gap", LuaValue::Integer(1)),
                ("min_width", LuaValue::Integer(5)),
                ("border", LuaValue::Integer(1)),
            ],
        ),
    );
    let paint = paint_of(&read_and_mount(&column).expect("padding + gap + align is valid"));
    assert_eq!(
        lines(&paint),
        vec![text_run(1, 5, "a"), text_run(3, 4, "bb")],
        "the placement is measured inside the padded content box"
    );
    assert_eq!(
        rings(&paint),
        vec![(0, 0, 56, 80)],
        "the ring frames the 7-cell by 5-row box the placement lives in"
    );
}

#[test]
fn an_aligned_child_is_never_pushed_past_its_box() {
    // `max_width = 4` gives the 6-cell run a 4-cell box, so there is no slack:
    // every placement collapses to offset zero and the pre-existing clip is the
    // only failure mode. `end` must not push the run out of the frame to keep
    // its right edge aligned.
    for align in ["start", "center", "end"] {
        let column = container_value(
            "Column",
            vec![text_value("abcdef")],
            column_style(align, vec![("max_width", LuaValue::Integer(4))]),
        );
        let paint = paint_of(&read_and_mount(&column).expect("a bounded Column is valid"));
        assert_eq!(
            lines(&paint),
            vec![text_run(0, 0, "abcd")],
            "align = {align}: a child as wide as its box clips, it never overflows"
        );
    }
}

// ---------------------------------------------------------------------------
// Backward compatibility: a declared start is byte-identical to an absent align
// ---------------------------------------------------------------------------

#[test]
fn a_declared_start_paints_the_same_list_as_an_absent_align() {
    let children = || vec![text_value("a"), text_value("abc")];
    let absent = container_value("Column", children(), Vec::new());
    let declared = container_value("Column", children(), vec![("align", align_of("start"))]);
    let absent_paint = paint_of(&read_and_mount(&absent).expect("valid"));
    let declared_paint = paint_of(&read_and_mount(&declared).expect("valid"));
    assert_eq!(
        absent_paint.ops(),
        declared_paint.ops(),
        "start is invisible: same ops, same order, same coordinates"
    );
    assert_eq!(absent_paint.damage(), declared_paint.damage());

    // And it costs no wrapper: `start` is flat, so the node count is the
    // unstyled one.
    let absent_node = UiRichNode::from_lua_value(&absent).expect("valid");
    let declared_node = UiRichNode::from_lua_value(&declared).expect("valid");
    assert_eq!(
        declared_node.count_nodes(),
        absent_node.count_nodes(),
        "a declared start lowers to no wrapper node"
    );
    assert_eq!(declared_node.depth(), absent_node.depth());
}

#[test]
fn a_non_start_align_costs_exactly_one_wrapper_node_and_no_bytes() {
    let column = container_value(
        "Column",
        vec![text_value("a"), text_value("abc")],
        column_style("center", vec![("min_width", LuaValue::Integer(9))]),
    );
    let node = UiRichNode::from_lua_value(&column).expect("valid");
    assert_eq!(node.count_nodes(), 4, "wrapper + column + two leaves");
    assert_eq!(node.depth(), 3, "wrapper + column + leaf");
    assert_eq!(node.text_bytes(), 4, "an align is not text");
    assert_eq!(
        node.retained_string_bytes(),
        4,
        "an align retains no string at all"
    );
}

// ---------------------------------------------------------------------------
// The scene model is the same vocabulary: no drift between wire and model
// ---------------------------------------------------------------------------

#[test]
fn the_bridged_style_is_the_hand_built_scene_style() {
    let from_lua = UiRichNode::from_lua_value(&container_value(
        "Column",
        vec![text_value("a")],
        column_style("center", vec![("min_width", LuaValue::Integer(9))]),
    ))
    .expect("valid");
    let bridged = bitty_runtime::scene_bridge::scene_node_of(&from_lua);
    let hand_built = SceneNode::Styled {
        style: bitty_rich::scene::SceneNodeStyle::new(
            0,
            0,
            0,
            bitty_rich::scene::ContentToken::Foreground,
        )
        .with_width_range(9, 0)
        .with_align(bitty_rich::scene::SceneStyleAlign::Center),
        child: Box::new(SceneNode::Column(vec![SceneNode::Text(
            bitty_rich::scene::StyledSpan {
                text: "a".to_string(),
                bold: false,
                italic: false,
            },
        )])),
    };
    assert_eq!(bridged, hand_built);
}

// ---------------------------------------------------------------------------
// Refusal path: loud, typed, and nothing mounted
// ---------------------------------------------------------------------------

#[test]
fn an_align_on_a_text_leaf_is_refused() {
    // A leaf has no adjacent child to place. The refusal names the kind instead
    // of accepting a declaration that could never paint anything.
    let leaf = LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("ab".to_string())),
        ("style", style_table(vec![("align", align_of("center"))])),
    ]);
    let error = UiRichNode::from_lua_value(&leaf).expect_err("a leaf align is refused");
    assert_eq!(error.class, "validation");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "style.align is not valid on a Text node: only Row, Column, and List place \
         adjacent children, and a declared start is the only align such a node accepts"
    );

    // A declared `start` asks for nothing, so it is accepted and inert.
    let zero = LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("ab".to_string())),
        ("style", style_table(vec![("align", align_of("start"))])),
    ]);
    assert!(
        UiRichNode::from_lua_value(&zero).is_ok(),
        "a declared start renders like an absent align, so it is not a kind violation"
    );
}

#[test]
fn the_band_distribution_spelling_is_refused_in_the_style_table() {
    // `space-between` spreads slack between siblings: that is the band root
    // Row's distribution, not a cross-axis placement, so the style table says
    // so instead of handing back a distribution it cannot apply.
    let column = container_value(
        "Column",
        vec![text_value("a")],
        vec![("align", align_of("space-between"))],
    );
    let error = UiRichNode::from_lua_value(&column).expect_err("not a placement");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "unknown scene style align 'space-between': expected 'start', 'center', or 'end'"
    );
}

#[test]
fn a_malformed_align_is_refused_and_never_clamped() {
    // A wrong type is the field's invalid-value refusal: the wire form is a
    // string, so nothing is coerced from an integer or a boolean.
    for value in [
        LuaValue::Integer(1),
        LuaValue::Bool(true),
        LuaValue::Number(2.0),
        LuaValue::table([("value", LuaValue::String("center".to_string()))]),
        LuaValue::Nil,
    ] {
        let column = container_value("Column", vec![text_value("a")], vec![("align", value)]);
        let error = UiRichNode::from_lua_value(&column).expect_err("wrong type refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(error.message, "invalid scene style value for field 'align'");
    }

    // An unlisted spelling (including a case variant) is refused, not clamped
    // to `start`.
    for spelling in ["middle", "Center", "END", "centre"] {
        let column = container_value(
            "Column",
            vec![text_value("a")],
            vec![("align", align_of(spelling))],
        );
        let error = UiRichNode::from_lua_value(&column).expect_err("unknown spelling refused");
        assert_eq!(
            error.message,
            format!("unknown scene style align '{spelling}': expected 'start', 'center', or 'end'")
        );
    }

    // An oversized value is refused by length without being echoed.
    let long = "s".repeat(33);
    let column = container_value(
        "Column",
        vec![text_value("a")],
        vec![("align", align_of(&long))],
    );
    let error = UiRichNode::from_lua_value(&column).expect_err("oversized value refused");
    assert_eq!(
        error.message,
        "scene style align exceeds cap: 33 > 32 bytes"
    );
    assert!(!error.message.contains(&long));
}

#[test]
fn a_repeated_align_key_is_refused() {
    // Only a hand-built marshalled table can repeat a key (the Lua VM collapses
    // duplicates in a literal), so this arm is the closed-set defence in depth.
    let table = LuaValue::Table(vec![
        (
            LuaValue::String("kind".to_string()),
            LuaValue::String("Column".to_string()),
        ),
        (
            LuaValue::String("children".to_string()),
            LuaValue::array(vec![text_value("a")]),
        ),
        (
            LuaValue::String("style".to_string()),
            LuaValue::Table(vec![
                (
                    LuaValue::String("align".to_string()),
                    LuaValue::String("center".to_string()),
                ),
                (
                    LuaValue::String("align".to_string()),
                    LuaValue::String("end".to_string()),
                ),
            ]),
        ),
    ]);
    let error = UiRichNode::from_lua_value(&table).expect_err("a repeated key is refused");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "duplicate scene style field 'align' (only a marshalled table can repeat a key)"
    );
}

#[test]
fn a_refused_align_mounts_no_block_and_keeps_the_previous_scene() {
    let mut scene = Scene::new();
    let accepted = UiRichNode::from_lua_value(&text_value("kept")).expect("valid");
    mount_rich_node(&mut scene, &mount_of(1), &accepted).expect("admitted");
    let before = scene.get(BlockId(1)).expect("admitted").content().clone();

    // The reader refuses before the bridge runs, so the last good scene is
    // untouched: there is no partial admission to roll back.
    let refused = LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("ab".to_string())),
        ("style", style_table(vec![("align", align_of("end"))])),
    ]);
    let error = UiRichNode::from_lua_value(&refused).expect_err("refused at the reader");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(scene.len(), 1, "the refused declaration added no block");
    assert_eq!(
        scene.get(BlockId(1)).expect("admitted").content(),
        &before,
        "the last good scene is untouched"
    );

    // A hand-built over-cap declaration is refused by the accepted admission
    // boundary too, so the wire parser is not the only gate.
    let over_cap = UiRichNode::Styled {
        style: bitty_lua::UiNodeStyle::new(0, 0, 0, bitty_lua::UiBorderToken::Foreground)
            .with_gap(33),
        child: Box::new(UiRichNode::Row(vec![accepted.clone(), accepted])),
    };
    let error = mount_rich_node(&mut scene, &mount_of(2), &over_cap).expect_err("refused");
    assert!(matches!(
        error,
        SceneBridgeError::Admission(SceneError::StyleInvalid { .. })
    ));
    assert_eq!(scene.len(), 1, "the refused block was never stored");
}

#[test]
fn a_hand_built_aligned_scene_node_is_admitted_and_painted() {
    // The scene model carries the placement independently of the Lua bridge, so
    // a Core producer gets the same layout without a second vocabulary.
    let node = SceneNode::Styled {
        style: bitty_rich::scene::SceneNodeStyle::new(
            0,
            0,
            0,
            bitty_rich::scene::ContentToken::Foreground,
        )
        .with_width_range(6, 0)
        .with_align(bitty_rich::scene::SceneStyleAlign::End),
        child: Box::new(SceneNode::Column(vec![SceneNode::Text(
            bitty_rich::scene::StyledSpan {
                text: "ab".to_string(),
                bold: false,
                italic: false,
            },
        )])),
    };
    let block = RichBlock::new(
        BlockId(3),
        BlockAnchor::Zone(3),
        node,
        ScrollBehavior::Inline,
        1,
        1,
        3,
    )
    .expect("admitted");
    let mut scene = Scene::new();
    scene.insert(block).expect("admitted");
    let paint = paint_of(&scene);
    assert_eq!(
        lines(&paint),
        vec![text_run(0, 4, "ab")],
        "a hand-built placement offsets the child inside the 6-cell box"
    );
}
