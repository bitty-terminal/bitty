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

fn row_value(children: Vec<LuaValue>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
        ("children", LuaValue::array(children)),
    ])
}

fn bordered_block_value(child: LuaValue, style: Vec<(&str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Block".to_string())),
        ("border", LuaValue::table([("width", LuaValue::Integer(1))])),
        ("child", child),
        ("style", style_table(style)),
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

/// One `Table` kind over a dense rows array; cells are joined by `" | "` when
/// painted.
fn table_value(rows: Vec<Vec<&str>>, style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    let rows = rows
        .into_iter()
        .map(|row| {
            LuaValue::array(
                row.into_iter()
                    .map(|cell| LuaValue::String(cell.to_string()))
                    .collect(),
            )
        })
        .collect();
    LuaValue::table([
        ("kind", LuaValue::String("Table".to_string())),
        ("rows", LuaValue::array(rows)),
        ("style", style_table(style)),
    ])
}

/// One `List` kind over child nodes. The bridge composes a `List` like a
/// `Column` (`scene_bridge::scene_node_of`), so on this surface the "items"
/// are child nodes rather than `ListModel` items.
fn list_value(children: Vec<LuaValue>, style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("List".to_string())),
        ("children", LuaValue::array(children)),
        ("style", style_table(style)),
    ])
}

fn code_block_value(content: &str, style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("CodeBlock".to_string())),
        ("content", LuaValue::String(content.to_string())),
        ("style", style_table(style)),
    ])
}

fn rule_value(style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Rule".to_string())),
        ("style", style_table(style)),
    ])
}

fn image_value(placement: i64, style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Image".to_string())),
        ("placement", LuaValue::Integer(placement)),
        ("style", style_table(style)),
    ])
}

/// Adds a `style` table to any node value, so a container can carry the range
/// too instead of only a leaf.
fn with_style(node: LuaValue, style: Vec<(&'static str, LuaValue)>) -> LuaValue {
    match node {
        LuaValue::Table(mut pairs) => {
            pairs.push((LuaValue::String("style".to_string()), style_table(style)));
            LuaValue::Table(pairs)
        }
        other => other,
    }
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

/// Reserved image rectangles as `(placement id, rect)`, in op order.
fn images(paint: &ScenePaint) -> Vec<(u64, RectPx)> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Image { placement, rect } => Some((placement.0, *rect)),
            _ => None,
        })
        .collect()
}

/// Painted solid fills, in op order.
fn fills(paint: &ScenePaint) -> Vec<RectPx> {
    paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Fill { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect()
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
    let row = row_value(vec![
        styled_text_value("ab", height_range(3, 5)),
        text_value("cd"),
    ]);
    let paint = paint_of(&read_and_mount(&row).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ab".to_string()), (0, 2, "cd".to_string())],
        "a vertical floor taller than the child keeps the horizontal sibling at column 2"
    );

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

#[test]
fn a_capped_bordered_block_never_paints_into_the_next_column_sibling() {
    let tree = column_value(vec![
        bordered_block_value(
            text_value("boxed"),
            vec![("max_height", LuaValue::Integer(1))],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&tree).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(1, 0, "Z".to_string())],
        "sibling starts on row 1 after the 1-row capped Block"
    );
    let cell = surface().cell;
    let capped_alloc = RectPx::new(0, 0, 7 * cell.width, cell.height);
    let sibling_alloc = RectPx::new(
        0,
        i32::try_from(cell.height).unwrap(),
        cell.width,
        cell.height,
    );
    let fills: Vec<RectPx> = paint
        .ops()
        .iter()
        .filter_map(|op| match op {
            ScenePaintOp::Fill { rect, .. } => Some(*rect),
            _ => None,
        })
        .collect();
    assert!(!fills.is_empty(), "top/side border segments still paint");
    for fill in &fills {
        assert!(
            capped_alloc.contains_rect(fill),
            "border fill {fill:?} escaped the 1-row capped Block allocation {capped_alloc:?}"
        );
        assert!(
            fill.intersection(&sibling_alloc).is_none(),
            "border fill {fill:?} overlapped the sibling allocation {sibling_alloc:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-kind coverage evidence: the same range on every remaining kind
// ---------------------------------------------------------------------------
//
// These rows are coverage evidence, not gates. The range is generic through
// `SceneNode::Styled`, so every row below also passes at the base commit; the
// mechanism is discriminated by the sabotage recorded on the PR, not here.
// What they add is the per-kind fact the accepted claim lacked: each kind's
// own measure reaches the painted rows through the same shipped chain.
//
// Two kinds measure differently from the rest and only one direction is
// observable on them: `Rule` and `Image` measure exactly one content row, so a
// `max_height` of 1 is the identity and a `max_height` of 0 is the degenerate
// `0/0` range — only their floor moves a sibling. `List` is composed as a
// `Column` by the bridge, so its items are child nodes on this surface; the
// capped and floored rows are those children's rows.

#[test]
fn a_table_max_height_paints_at_most_the_capped_rows() {
    // Three table rows measure three content rows; a 2-row cap leaves the two
    // header/body rows painted and puts the next sibling on row 2. The
    // unbounded reading of the same table puts it on row 3, so the cap is what
    // moved it.
    let rows = vec![vec!["h1", "h2"], vec!["r1", "r2"], vec!["r3", "r4"]];
    let capped = column_value(vec![
        table_value(rows.clone(), vec![("max_height", LuaValue::Integer(2))]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&capped).expect("a table cap is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "h1 | h2".to_string()),
            (1, 0, "r1 | r2".to_string()),
            (2, 0, "Z".to_string()),
        ],
        "the third table row is clipped and the sibling lands on row 2"
    );

    let uncapped = column_value(vec![table_value(rows, vec![]), text_value("Z")]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "h1 | h2".to_string()),
            (1, 0, "r1 | r2".to_string()),
            (2, 0, "r3 | r4".to_string()),
            (3, 0, "Z".to_string()),
        ],
        "the uncapped table paints all three rows"
    );
}

#[test]
fn a_list_min_height_pushes_the_next_sibling_to_the_floored_row() {
    // One list item measures one row; a 4-row floor holds the box open without
    // inventing items, so the only painted text is the item and the sibling
    // starts on row 4.
    let floored = column_value(vec![
        list_value(
            vec![text_value("i1")],
            vec![("min_height", LuaValue::Integer(4))],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&floored).expect("a list floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "i1".to_string()), (4, 0, "Z".to_string())],
        "the floor widens the list box without inventing items"
    );

    let unfloored = column_value(vec![
        list_value(vec![text_value("i1")], vec![]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&unfloored).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "i1".to_string()), (1, 0, "Z".to_string())],
        "the unfloored list is one row tall"
    );
}

#[test]
fn a_list_max_height_paints_at_most_the_capped_rows() {
    // Three items measure three rows; a 2-row cap paints the first two and puts
    // the sibling on row 2.
    let capped = column_value(vec![
        list_value(
            vec![text_value("i1"), text_value("i2"), text_value("i3")],
            vec![("max_height", LuaValue::Integer(2))],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&capped).expect("a list cap is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "i1".to_string()),
            (1, 0, "i2".to_string()),
            (2, 0, "Z".to_string()),
        ],
        "the third item is clipped and the sibling lands on row 2"
    );

    let uncapped = column_value(vec![
        list_value(
            vec![text_value("i1"), text_value("i2"), text_value("i3")],
            vec![],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "i1".to_string()),
            (1, 0, "i2".to_string()),
            (2, 0, "i3".to_string()),
            (3, 0, "Z".to_string()),
        ],
        "the uncapped list paints all three items"
    );
}

#[test]
fn a_code_block_max_height_paints_at_most_the_capped_lines() {
    // A lang-free code block measures one row per content line: three lines, a
    // 2-line cap, so "l3" is never painted and the sibling lands on row 2.
    let capped = column_value(vec![
        code_block_value("l1\nl2\nl3", vec![("max_height", LuaValue::Integer(2))]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&capped).expect("a code block cap is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "l1".to_string()),
            (1, 0, "l2".to_string()),
            (2, 0, "Z".to_string()),
        ],
        "the third line is clipped and the sibling lands on row 2"
    );

    let uncapped = column_value(vec![
        code_block_value("l1\nl2\nl3", vec![]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "l1".to_string()),
            (1, 0, "l2".to_string()),
            (2, 0, "l3".to_string()),
            (3, 0, "Z".to_string()),
        ],
        "the uncapped code block paints all three lines"
    );
}

#[test]
fn a_rule_min_height_pushes_the_next_sibling_to_the_floored_row() {
    // A rule measures one content row, so a 3-row floor is the only observable
    // direction: the rule itself stays on its own row (the stroke is one pixel
    // inside row 0) and the sibling starts on row 3.
    let floored = column_value(vec![
        rule_value(vec![("min_height", LuaValue::Integer(3))]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&floored).expect("a rule floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(3, 0, "Z".to_string())],
        "the sibling lands on the floored row"
    );
    let cell = surface().cell;
    let stroke = fills(&paint);
    assert_eq!(stroke.len(), 1, "one rule paints exactly one stroke");
    assert_eq!(
        stroke[0],
        RectPx::new(
            0,
            i32::try_from(cell.height / 2).unwrap(),
            cell.width * u32::from(surface().cols()),
            1
        ),
        "the stroke stays on row 0: the floor moved the sibling, not the rule"
    );

    let unfloored = column_value(vec![rule_value(vec![]), text_value("Z")]);
    let paint = paint_of(&read_and_mount(&unfloored).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(1, 0, "Z".to_string())],
        "an unfloored rule is one row tall"
    );
}

#[test]
fn an_image_min_height_pushes_the_next_sibling_to_the_floored_row() {
    // An image measures one content row, so a 3-row floor is the only
    // observable direction: the placement keeps its one-row reservation on
    // row 0 and the sibling starts on row 3.
    let floored = column_value(vec![
        image_value(7, vec![("min_height", LuaValue::Integer(3))]),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&floored).expect("an image floor is valid"));
    assert_eq!(
        lines(&paint),
        vec![(3, 0, "Z".to_string())],
        "the sibling lands on the floored row"
    );
    let cell = surface().cell;
    assert_eq!(
        images(&paint),
        vec![(
            7,
            RectPx::new(0, 0, cell.width * u32::from(surface().cols()), cell.height)
        )],
        "the placement keeps its one-row reservation on row 0"
    );

    let unfloored = column_value(vec![image_value(7, vec![]), text_value("Z")]);
    let paint = paint_of(&read_and_mount(&unfloored).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(1, 0, "Z".to_string())],
        "an unfloored image is one row tall"
    );
}

#[test]
fn a_styled_floor_around_a_styled_cap_keeps_the_outer_floor() {
    // A 2-row cap inside a 5-row floor: the cap clips the three text rows to
    // two and the floor still holds the box open, so the sibling lands on row
    // 5 either way. Both numbers are painted facts, and the pair shows the two
    // directions composing rather than one overriding the other.
    let capped_inside_floor = column_value(vec![
        with_style(
            column_value(vec![styled_text_value(
                "a\nb\nc",
                vec![("max_height", LuaValue::Integer(2))],
            )]),
            vec![("min_height", LuaValue::Integer(5))],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&capped_inside_floor).expect("nested range is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "a".to_string()),
            (1, 0, "b".to_string()),
            (5, 0, "Z".to_string()),
        ],
        "the inner cap clips to two rows and the outer floor still holds row 5"
    );

    let floor_only = column_value(vec![
        with_style(
            column_value(vec![text_value("a\nb\nc")]),
            vec![("min_height", LuaValue::Integer(5))],
        ),
        text_value("Z"),
    ]);
    let paint = paint_of(&read_and_mount(&floor_only).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "a".to_string()),
            (1, 0, "b".to_string()),
            (2, 0, "c".to_string()),
            (5, 0, "Z".to_string()),
        ],
        "without the cap the floor alone never clips a row"
    );
}

#[test]
fn a_row_holding_a_capped_table_shrinks_to_the_cap_for_the_next_sibling() {
    // A row is as tall as its tallest child, so a 1-row capped table makes the
    // row one row tall: the table paints one row, the horizontal sibling keeps
    // its column, and the vertical sibling lands on row 1 instead of row 3.
    let rows = vec![vec!["t1", "u1"], vec!["t2", "u2"], vec!["t3", "u3"]];
    let capped = column_value(vec![
        row_value(vec![
            table_value(rows.clone(), vec![("max_height", LuaValue::Integer(1))]),
            text_value("Z"),
        ]),
        text_value("after"),
    ]);
    let paint = paint_of(&read_and_mount(&capped).expect("a capped table in a row is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "t1 | u1".to_string()),
            (0, 7, "Z".to_string()),
            (1, 0, "after".to_string()),
        ],
        "the row shrinks to the capped table and the next sibling lands on row 1"
    );

    let uncapped = column_value(vec![
        row_value(vec![table_value(rows, vec![]), text_value("Z")]),
        text_value("after"),
    ]);
    let paint = paint_of(&read_and_mount(&uncapped).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (0, 0, "t1 | u1".to_string()),
            (1, 0, "t2 | u2".to_string()),
            (2, 0, "t3 | u3".to_string()),
            (0, 7, "Z".to_string()),
            (3, 0, "after".to_string()),
        ],
        "the uncapped row is three rows tall"
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
