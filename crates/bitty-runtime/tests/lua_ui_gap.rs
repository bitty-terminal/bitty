//! End-to-end evidence for the sibling-spacing primitive on the `ui.rich`
//! scene style table (bitty#2191, CTX-1852).
//!
//! The chain under test is the shipped one: a declarative Lua table is read by
//! the `ui.rich` reader (`bitty-lua`), converted by the Core bridge into the
//! accepted scene model (`bitty-runtime::scene_bridge`), admitted through the
//! accepted `SCN` boundary (`bitty-rich`), and laid out and painted by the
//! render scene path (`bitty-render`) into a bounded, headless paint list.
//! Nothing here needs a GPU, a compositor, or a terminal.
//!
//! Every layout assertion below reads the **painted** list — cell rows, cell
//! columns, and text runs — not a structure the test built itself, so a mutant
//! that parses the key but never applies it fails here. The suite pins both
//! directions:
//!
//! - **accept:** a `gap` on a `Row`/`Column`/`List` moves the following sibling
//!   on the composed axis and advances the container's measured width, at the
//!   inclusive `32` cap too, and composes with `padding`;
//! - **compatibility:** a declared `0` and an absent `gap` paint the same ops,
//!   in the same order, at the same coordinates, and cost no wrapper node;
//! - **no clamp:** a sibling group whose gaps overflow the slot clips at the
//!   boundary under the existing walk — a node is never truncated to make room
//!   for spacing, nothing wraps, and nothing scrolls;
//! - **refuse:** a non-zero `gap` on a leaf, an unknown key, a wrong value type,
//!   a negative value, and an over-cap value are typed
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

const SURFACE_COLS: u16 = 60;

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

fn row_value(children: Vec<LuaValue>) -> LuaValue {
    container_value("Row", children, Vec::new())
}

fn styled_row_value(children: Vec<LuaValue>, style: Vec<(&str, LuaValue)>) -> LuaValue {
    container_value("Row", children, style)
}

fn styled_column_value(children: Vec<LuaValue>, style: Vec<(&str, LuaValue)>) -> LuaValue {
    container_value("Column", children, style)
}

fn styled_text_value(text: &str, style: Vec<(&str, LuaValue)>) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(text.to_string())),
        ("style", style_table(style)),
    ])
}

fn gap(cells: i64) -> Vec<(&'static str, LuaValue)> {
    vec![("gap", LuaValue::Integer(cells))]
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

/// Painted text runs as `(row, col, cells)`, in op order.
fn columns(paint: &ScenePaint) -> Vec<(u16, u16, usize)> {
    lines(paint)
        .into_iter()
        .map(|(row, col, text)| (row, col, text.chars().count()))
        .collect()
}

// ---------------------------------------------------------------------------
// Accept path: the gap reaches the painted layout
// ---------------------------------------------------------------------------

#[test]
fn a_gap_on_a_row_moves_every_following_sibling() {
    // "ab" is 2 cells, so without a gap the second run starts at column 2. A
    // 3-cell gap moves it to column 5. Both facts are read from the paint list,
    // so a reader that stores the key without the walk applying it produces
    // (0, 0, "ab"), (0, 2, "cd") instead.
    let unstyled = row_value(vec![text_value("ab"), text_value("cd")]);
    let paint = paint_of(&read_and_mount(&unstyled).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ab".to_string()), (0, 2, "cd".to_string())],
        "the pre-change left-to-right flow"
    );

    let gapped = styled_row_value(vec![text_value("ab"), text_value("cd")], gap(3));
    let paint = paint_of(&read_and_mount(&gapped).expect("a gap on a Row is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "ab".to_string()), (0, 5, "cd".to_string())],
        "the gap advances the walk cursor between siblings"
    );
}

#[test]
fn a_gap_on_a_column_stacks_with_spacing() {
    // A Column spends the gap vertically: the second one-row child lands on row
    // 3 instead of row 1. A mutant that applies the gap on the horizontal axis
    // for every container leaves it at (1, 0).
    let unstyled = container_value("Column", vec![text_value("a"), text_value("b")], Vec::new());
    let paint = paint_of(&read_and_mount(&unstyled).expect("valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (1, 0, "b".to_string())]
    );

    let gapped = styled_column_value(vec![text_value("a"), text_value("b")], gap(2));
    let paint = paint_of(&read_and_mount(&gapped).expect("a gap on a Column is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (3, 0, "b".to_string())],
        "a Column spends the gap on the vertical axis"
    );
}

#[test]
fn a_gap_on_a_list_composes_like_a_column() {
    // The rich `List` is the v1 spelling of a vertical container (the bridge
    // maps it to `SceneNode::Column`), so it takes the same gap.
    let gapped = container_value("List", vec![text_value("a"), text_value("b")], gap(1));
    let paint = paint_of(&read_and_mount(&gapped).expect("a gap on a List is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (2, 0, "b".to_string())],
        "the List gap stacks the items"
    );
}

#[test]
fn the_cap_is_inclusive_and_the_gap_still_advances_the_walk() {
    let gapped = styled_row_value(
        vec![text_value("a"), text_value("b")],
        gap(i64::from(bitty_lua::UI_STYLE_MAX_GAP_CELLS)),
    );
    let paint = paint_of(&read_and_mount(&gapped).expect("32 cells is the inclusive cap"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 33, "b".to_string())],
        "the cap itself is a usable gap"
    );
}

#[test]
fn a_gap_composes_with_padding_and_reaches_a_containing_sibling() {
    // padding = 1 on each side and gap = 2: the content starts at column 1, the
    // first run is one cell, so the second run starts at 1 + 1 + 2 = 4. The
    // container's own extent is 1 + 1 + 2 + 1 + 1 = 6 cells, which is what the
    // outer `Row` advances by, so "Z" lands at column 6.
    let inner = styled_row_value(
        vec![text_value("a"), text_value("b")],
        vec![
            ("padding", LuaValue::Integer(1)),
            ("gap", LuaValue::Integer(2)),
        ],
    );
    let outer = row_value(vec![inner, text_value("Z")]);
    let paint = paint_of(&read_and_mount(&outer).expect("padding + gap is valid"));
    assert_eq!(
        lines(&paint),
        vec![
            (1, 1, "a".to_string()),
            (1, 4, "b".to_string()),
            (0, 6, "Z".to_string()),
        ],
        "the gap is spent inside the padded content box, and the box carries it out"
    );
}

#[test]
fn a_gap_on_a_single_child_or_a_childless_container_is_inert() {
    // No adjacent pair, no spacing: the measured extent must not gain a
    // trailing gap, so a sibling after the container still starts immediately.
    let single = styled_row_value(vec![text_value("a")], gap(5));
    let outer = row_value(vec![single, text_value("Z")]);
    let paint = paint_of(&read_and_mount(&outer).expect("a single-child gap is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 1, "Z".to_string())],
        "no trailing gap widens a single-child container"
    );

    let empty = styled_row_value(Vec::new(), gap(5));
    let paint = paint_of(&read_and_mount(&empty).expect("a childless gap is valid"));
    assert!(
        lines(&paint).is_empty(),
        "a childless container paints nothing, with or without a gap"
    );
}

// ---------------------------------------------------------------------------
// Backward compatibility: a declared 0 is byte-identical to an absent gap
// ---------------------------------------------------------------------------

#[test]
fn a_declared_zero_paints_the_same_list_as_an_absent_gap() {
    let absent = row_value(vec![text_value("ab"), text_value("cd")]);
    let declared = styled_row_value(vec![text_value("ab"), text_value("cd")], gap(0));
    let absent_paint = paint_of(&read_and_mount(&absent).expect("valid"));
    let declared_paint = paint_of(&read_and_mount(&declared).expect("valid"));
    assert_eq!(
        absent_paint.ops(),
        declared_paint.ops(),
        "a 0 gap is invisible: same ops, same order, same coordinates"
    );

    // And it costs no wrapper: a declared 0 is flat, so the node count is the
    // unstyled one.
    let absent_node = UiRichNode::from_lua_value(&absent).expect("valid");
    let declared_node = UiRichNode::from_lua_value(&declared).expect("valid");
    assert_eq!(
        declared_node.count_nodes(),
        absent_node.count_nodes(),
        "a declared 0 lowers to no wrapper node"
    );
    assert_eq!(declared_node.depth(), absent_node.depth());
}

#[test]
fn a_non_zero_gap_costs_exactly_one_wrapper_node() {
    let gapped = UiRichNode::from_lua_value(&styled_row_value(
        vec![text_value("ab"), text_value("cd")],
        gap(2),
    ))
    .expect("valid");
    assert_eq!(gapped.count_nodes(), 4, "wrapper + row + two leaves");
    assert_eq!(gapped.depth(), 3, "wrapper + row + leaf");
    assert_eq!(gapped.text_bytes(), 4, "a gap is not text");
    assert_eq!(
        gapped.retained_string_bytes(),
        4,
        "a gap retains no string at all"
    );
}

// ---------------------------------------------------------------------------
// No clamp: a group that does not fit clips, it never truncates a node
// ---------------------------------------------------------------------------

#[test]
fn an_over_subscribed_group_clips_without_truncating_a_node() {
    // Two 40-cell runs plus a 5-cell gap measure 85 cells on a 60-column leaf,
    // so the group cannot fit. The first run is painted in full — the gap is
    // spent after it, never taken out of it — and the second is clipped at the
    // slot boundary (columns 45..60 = 15 cells), which is the pre-existing
    // degradation: a clip, not a truncation to make room and not a refusal.
    let long = "x".repeat(40);
    let also_long = "y".repeat(40);
    let gapped = styled_row_value(vec![text_value(&long), text_value(&also_long)], gap(5));
    let paint = paint_of(&read_and_mount(&gapped).expect("an over-subscribed gap is valid"));
    assert_eq!(SURFACE_COLS, 60, "fixture surface width in cells");
    assert_eq!(
        columns(&paint),
        vec![(0, 0, 40), (0, 45, 15)],
        "the first node keeps every cell it measured; the second clips at the slot"
    );
    let painted = lines(&paint);
    assert_eq!(painted[0].2, long, "no node is truncated for the gap");
    assert_eq!(painted[1].2, "y".repeat(15), "the overflow is a clip");
}

#[test]
fn a_gap_that_pushes_a_sibling_past_the_slot_drops_it() {
    // 3 one-cell children with a 30-cell gap: the third sibling would start at
    // column 62 on a 60-column leaf, so the existing walk breaks before it. The
    // declaration is still valid and still mounts; only the painted extent is
    // clipped.
    let gapped = styled_row_value(
        vec![text_value("a"), text_value("b"), text_value("c")],
        gap(30),
    );
    let paint = paint_of(&read_and_mount(&gapped).expect("a wide gap is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 31, "b".to_string())],
        "the sibling past the slot boundary is clipped, not clamped into it"
    );
}

// ---------------------------------------------------------------------------
// Empty siblings: the gap sits only between two non-empty spans
// ---------------------------------------------------------------------------

/// Every painted border ring as `(x, y, width, height)` in physical pixels, in
/// op order.
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

#[test]
fn a_trailing_empty_sibling_adds_no_gap_to_the_measure() {
    // `gap = 2` next to an empty trailing span: the empty span has no start for
    // the spacing to reach, so the pair takes none and the container measures 1
    // cell for its 1 cell of content. A measure that only inspects the
    // *previous* span charges the 2 cells anyway and moves the sibling after the
    // container to column 3 — three cells of width sold for one cell of text,
    // which is then what a following frame draws around.
    let trailing = styled_row_value(vec![text_value("a"), text_value("")], gap(2));
    let outer = row_value(vec![trailing, text_value("Z")]);
    let paint = paint_of(&read_and_mount(&outer).expect("an empty sibling is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 1, "Z".to_string())],
        "an empty trailing sibling is inert, so the container stays 1 cell wide"
    );

    // Baseline: the same tree with no gap paints the identical coordinates, so
    // the inert declaration contributed nothing to the measure.
    let baseline = row_value(vec![
        row_value(vec![text_value("a"), text_value("")]),
        text_value("Z"),
    ]);
    let baseline_paint = paint_of(&read_and_mount(&baseline).expect("valid"));
    assert_eq!(
        lines(&baseline_paint),
        lines(&paint),
        "a gap that has no non-empty pair to separate paints like an absent one"
    );
}

#[test]
fn an_empty_span_between_two_runs_leaves_them_touching() {
    // The rule is between *adjacent* siblings: `a`, ``, `b` has no two adjacent
    // non-empty spans, so no cell of spacing is charged and both runs are
    // contiguous. Paint walks the same rule, so the two runs stay one cell
    // apart-free and the container measures 2 cells, not 4.
    let gapped = styled_row_value(
        vec![text_value("a"), text_value(""), text_value("b")],
        gap(2),
    );
    let paint = paint_of(&read_and_mount(&gapped).expect("an empty sibling is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 1, "b".to_string())],
        "an empty sibling separates nothing, so it earns no spacing"
    );
}

#[test]
fn a_trailing_empty_sibling_adds_no_gap_on_the_column_axis() {
    // A text span is one row tall even when it is empty, so the empty span on
    // this axis is a childless container: 0 rows and 0 columns.
    let empty = || container_value("Column", Vec::new(), Vec::new());

    // The defect on the vertical axis: 1 row of content must not measure 3 rows,
    // so the sibling after the column lands on row 1 instead of row 3.
    let trailing = styled_column_value(vec![text_value("a"), empty()], gap(2));
    let outer = container_value("Column", vec![trailing, text_value("Z")], Vec::new());
    let paint = paint_of(&read_and_mount(&outer).expect("an empty sibling is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (1, 0, "Z".to_string())],
        "an empty trailing sibling is inert on the vertical axis too"
    );

    // The symmetric boundary: a *leading* empty sibling takes no gap before the
    // first non-empty span either, so the content still starts at row 0 and the
    // column still measures 1 row.
    let leading = styled_column_value(vec![empty(), text_value("a")], gap(2));
    let outer = container_value("Column", vec![leading, text_value("Z")], Vec::new());
    let paint = paint_of(&read_and_mount(&outer).expect("an empty sibling is valid"));
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (1, 0, "Z".to_string())],
        "an empty leading sibling opens no gap at the top of the column"
    );
}

#[test]
fn a_bordered_container_around_the_group_frames_one_content_cell() {
    // A border is a stroke inside the node's own measured frame, so the frame is
    // exactly the content box: 1 cell here, never the 3 a trailing-empty span
    // used to buy. The ring op carries that frame, so its rect is 8 px wide (one
    // 8-px cell) and the sibling after the block still starts at column 1. On
    // the pre-fix rule the same declaration draws a 24-px ring around one
    // character and moves "Z" to column 3.
    let bordered = styled_row_value(
        vec![text_value("a"), text_value("")],
        vec![
            ("gap", LuaValue::Integer(2)),
            ("border", LuaValue::Integer(1)),
        ],
    );
    let outer = row_value(vec![bordered, text_value("Z")]);
    let paint = paint_of(&read_and_mount(&outer).expect("a bordered gap is valid"));
    assert_eq!(
        rings(&paint),
        vec![(0, 0, 8, 16)],
        "the ring frames one 8x16 content cell, not three cells of nothing"
    );
    assert_eq!(
        lines(&paint),
        vec![(0, 0, "a".to_string()), (0, 1, "Z".to_string())],
        "the frame and the container measure agree on the 1-cell width"
    );
}

// ---------------------------------------------------------------------------
// Refusal path: loud, typed, and nothing mounted
// ---------------------------------------------------------------------------

#[test]
fn a_gap_on_a_text_leaf_is_refused() {
    // A leaf has no siblings to separate. The refusal names the kind instead of
    // accepting a declaration that could never paint anything.
    let node = styled_text_value("ab", gap(1));
    let error = UiRichNode::from_lua_value(&node).expect_err("a leaf gap is refused");
    assert_eq!(error.class, "validation");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "style.gap is not valid on a Text node: only Row, Column, and List separate \
         adjacent siblings, and a declared 0 is the only gap such a node accepts"
    );

    // A declared 0 asks for nothing, so it is accepted and inert.
    let zero = styled_text_value("ab", gap(0));
    assert!(
        UiRichNode::from_lua_value(&zero).is_ok(),
        "a declared 0 renders like an absent gap, so it is not a kind violation"
    );
}

#[test]
fn the_keys_outside_the_set_are_still_refused_loudly() {
    // `align`, `spacing`, and `width` remain outside the closed key set: the
    // gap lane added no second spelling and no cross-axis distribution.
    for key in ["align", "spacing", "width", "min_width_cells", "margin"] {
        let node = styled_row_value(vec![text_value("a")], vec![(key, LuaValue::Integer(1))]);
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
fn a_non_integer_negative_or_over_cap_gap_is_refused() {
    // One past the cap: typed, naming both values, no clamp.
    for over in [33, 1000] {
        let node = styled_row_value(vec![text_value("a"), text_value("b")], gap(over));
        let error = UiRichNode::from_lua_value(&node).expect_err("over cap refused");
        assert_eq!(error.class, "validation");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(
            error.message,
            format!("scene style gap exceeds cap: {over} > 32")
        );
    }

    // A float, a boolean, a string, a table, and a negative integer are all the
    // same typed refusal: the wire form is an integer cell count.
    for value in [
        LuaValue::Number(2.0),
        LuaValue::Number(2.5),
        LuaValue::Bool(true),
        LuaValue::String("2".to_string()),
        LuaValue::table([("cells", LuaValue::Integer(2))]),
        LuaValue::Integer(-1),
    ] {
        let node = styled_row_value(vec![text_value("a"), text_value("b")], vec![("gap", value)]);
        let error = UiRichNode::from_lua_value(&node).expect_err("refused");
        assert_eq!(error.class, "validation");
        assert_eq!(
            error.message, "invalid scene style value for field 'gap'",
            "no coercion: the gap is an integer"
        );
    }
}

#[test]
fn a_refused_gap_mounts_no_block_and_keeps_the_previous_scene() {
    let mut scene = Scene::new();
    let accepted = UiRichNode::from_lua_value(&text_value("kept")).expect("valid");
    mount_rich_node(&mut scene, &mount_of(1), &accepted).expect("admitted");
    let before = scene.get(BlockId(1)).expect("admitted").content().clone();

    // The reader refuses before the bridge runs, so the last good scene is
    // untouched: there is no partial admission to roll back.
    let refused = styled_text_value("ab", gap(2));
    let error = UiRichNode::from_lua_value(&refused).expect_err("refused at the reader");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(scene.len(), 1, "the refused declaration added no block");
    assert_eq!(
        scene.get(BlockId(1)).expect("admitted").content(),
        &before,
        "the last good scene is untouched"
    );

    // A hand-built over-cap gap is refused by the accepted admission boundary
    // too, so the wire parser is not the only gate.
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
fn a_hand_built_over_cap_gap_is_refused_at_the_model_boundary() {
    // The model-level validator is the second gate: a producer that bypasses the
    // Lua bridge cannot admit an out-of-range gap either.
    let over_cap = SceneNode::Styled {
        style: bitty_rich::scene::SceneNodeStyle::new(
            0,
            0,
            0,
            bitty_rich::scene::ContentToken::Foreground,
        )
        .with_gap(33),
        child: Box::new(SceneNode::Row(vec![
            SceneNode::Text(bitty_rich::scene::StyledSpan {
                text: "a".to_string(),
                bold: false,
                italic: false,
            }),
            SceneNode::Text(bitty_rich::scene::StyledSpan {
                text: "b".to_string(),
                bold: false,
                italic: false,
            }),
        ])),
    };
    let error = RichBlock::new(
        BlockId(7),
        BlockAnchor::Zone(7),
        over_cap,
        ScrollBehavior::Inline,
        1,
        1,
        1,
    )
    .expect_err("an over-cap gap is refused");
    assert!(
        matches!(error, SceneError::StyleInvalid { .. }),
        "admission refuses the over-cap gap: {error:?}"
    );
}
