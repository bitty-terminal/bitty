#![forbid(unsafe_code)]
//! End-to-end evidence for the band-root distribution field (bitty#2191,
//! CTX-1853).
//!
//! The chain under test is the shipped one: a declarative Lua table is read by
//! the v1 band reader (`bitty_lua::ui`), and the band host lays it out at the
//! presented width (`bitty_runtime::flatten_band_runs_in_width`). Every
//! assertion below reads the **cell columns the walk emits** — the same runs
//! the band paint path and the hit-test consume — so a reader that stores the
//! key without the walk applying it, or a walk that offsets the group without
//! clipping it, fails here rather than in a passing parse test.
//!
//! Pinned directions:
//!
//! - **distribute:** `center`, `end`, and `space-between` move the real cells
//!   at a fixed width, including the integer-division remainder rule for
//!   `space-between` (leftmost gaps take it);
//! - **compatibility:** an absent `align` and a declared `start` emit the same
//!   runs, so an unstyled tree is byte-identical;
//! - **no escape:** an over-subscribed group keeps the plain flow, no run ends
//!   past the band edge, and no two runs overlap;
//! - **refuse:** an unknown value, a wrong type, a non-`Row` kind, a nested
//!   `Row`, an `Unanchored` (overlay / column band) mount, and `align` beside a
//!   child `anchor` are typed `E_UI_COMPONENT_INVALID` refusals that mount
//!   nothing — plus the rich/terminal surface refusing the band-only field.

use bitty_lua::ui::{ClickArg, UiNode, UiRegionKind, UiSceneScope, UiSlot};
use bitty_lua::{BridgeError, LuaValue, UiAlign, UiRichNode};
use bitty_platform::{CursorPosition, MouseButton, PhysicalSize, PressState};
use bitty_runtime::{BandContent, BandEdge, BandMount, ChromeBands, Runtime};

// ---------------------------------------------------------------------------
// Lua fixtures (the wire shape a plugin actually writes)
// ---------------------------------------------------------------------------

fn text_value(value: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(value.to_string())),
    ])
}

/// One `Text` leaf with an `on_click` binding and its region identity.
fn clickable_text_value(label: &str, region: &str) -> LuaValue {
    LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String(label.to_string())),
        (
            "on_click",
            LuaValue::table([
                // The band routes only a command the owner plugin declares, so the
                // fixture uses the owner-qualified spelling the host splits.
                ("command", LuaValue::String(format!("{PLUGIN}:focus"))),
                (
                    "args",
                    LuaValue::table([("id", LuaValue::String(region.to_string()))]),
                ),
                (
                    "region",
                    LuaValue::table([("id", LuaValue::String(region.to_string()))]),
                ),
            ]),
        ),
    ])
}

/// A container node of `kind`, optionally carrying an `align` value and/or a
/// child `anchor`.
fn container_value(
    kind: &str,
    children: Vec<LuaValue>,
    align: Option<LuaValue>,
    anchor: Option<&str>,
) -> LuaValue {
    let mut fields = vec![
        ("kind", LuaValue::String(kind.to_string())),
        ("children", LuaValue::array(children)),
    ];
    if let Some(align) = align {
        fields.push(("align", align));
    }
    if let Some(anchor) = anchor {
        fields.push(("anchor", LuaValue::String(anchor.to_string())));
    }
    LuaValue::Table(
        fields
            .into_iter()
            .map(|(key, value)| (LuaValue::String(key.to_string()), value))
            .collect(),
    )
}

fn align_value(name: &str) -> Option<LuaValue> {
    Some(LuaValue::String(name.to_string()))
}

/// Three siblings totalling 10 cells, so a 40-cell band has 30 cells of slack.
fn three_siblings(align: Option<LuaValue>) -> LuaValue {
    container_value(
        "Row",
        vec![text_value("aaa"), text_value("bb"), text_value("ccccc")],
        align,
        None,
    )
}

fn parse_band(value: &LuaValue) -> Result<UiNode, BridgeError> {
    UiNode::from_lua_value_in(value, UiSceneScope::Band)
}

fn parse_overlay(value: &LuaValue) -> Result<UiNode, BridgeError> {
    UiNode::from_lua_value_in(value, UiSceneScope::Unanchored)
}

/// `(start_col, end_col)` of every emitted run, in walk order.
fn spans(root: &UiNode, width: usize) -> Vec<(usize, usize)> {
    let (_, runs) = bitty_runtime::flatten_band_runs_in_width(root, width);
    runs.iter()
        .map(|run| (run.start_col, run.end_col))
        .collect()
}

fn band_spans(align: Option<LuaValue>, width: usize) -> Vec<(usize, usize)> {
    let root = parse_band(&three_siblings(align)).expect("band tree parses");
    spans(&root, width)
}

// ---------------------------------------------------------------------------
// Behaviour: the four distributions move real cells
// ---------------------------------------------------------------------------

#[test]
fn the_default_distribution_is_todays_left_to_right_flow() {
    // 40 cells, 10 of content: absent and `start` must both pack from 0 and be
    // indistinguishable, which is the compatibility promise of the field.
    assert_eq!(band_spans(None, 40), [(0, 3), (3, 5), (5, 10)]);
    assert_eq!(
        band_spans(align_value("start"), 40),
        band_spans(None, 40),
        "a declared start renders like an absent align"
    );
}

#[test]
fn center_offsets_the_group_by_half_the_slack() {
    // slack 30 -> origin 15.
    assert_eq!(
        band_spans(align_value("center"), 40),
        [(15, 18), (18, 20), (20, 25)]
    );
    // An odd slack takes the smaller half on the left (integer division).
    // 41 cells: slack 31 -> origin 15.
    assert_eq!(
        band_spans(align_value("center"), 41),
        [(15, 18), (18, 20), (20, 25)]
    );
}

#[test]
fn end_right_aligns_the_group_flush_to_the_edge() {
    // slack 30 -> the last child ends exactly at column 40.
    assert_eq!(
        band_spans(align_value("end"), 40),
        [(30, 33), (33, 35), (35, 40)]
    );
    assert_eq!(
        band_spans(align_value("end"), 41),
        [(31, 34), (34, 36), (36, 41)]
    );
}

#[test]
fn space_between_pins_the_edges_and_splits_the_slack_between_gaps() {
    // 40 cells, slack 30, two inter-child gaps -> 15 each: first flush left,
    // last flush right.
    assert_eq!(
        band_spans(align_value("space-between"), 40),
        [(0, 3), (18, 20), (35, 40)]
    );
    // 41 cells, slack 31, two gaps: 15 each with the remainder (1) to the
    // leftmost gap -> 16 then 15. The last child still ends at the edge.
    assert_eq!(
        band_spans(align_value("space-between"), 41),
        [(0, 3), (19, 21), (36, 41)]
    );
    // Three gaps and a remainder of 2: the two leftmost gaps take the extra
    // cell, which is the documented integer-division rule.
    let four = container_value(
        "Row",
        vec![
            text_value("a"),
            text_value("b"),
            text_value("c"),
            text_value("d"),
        ],
        align_value("space-between"),
        None,
    );
    let root = parse_band(&four).expect("band tree parses");
    // content 4, width 20 -> slack 16 over 3 gaps -> 6,5,5.
    assert_eq!(spans(&root, 20), [(0, 1), (7, 8), (13, 14), (19, 20)]);
}

#[test]
fn space_between_is_inert_without_a_pair_and_start_is_inert_without_children() {
    // A single child has no inter-child gap to spread: it stays flush left
    // rather than drifting to the edge.
    let single = container_value(
        "Row",
        vec![text_value("only")],
        align_value("space-between"),
        None,
    );
    let root = parse_band(&single).expect("band tree parses");
    assert_eq!(spans(&root, 40), [(0, 4)]);
    // A childless row emits no text and no runs at any alignment.
    for align in UiAlign::ALL {
        let empty = container_value(
            "Row",
            Vec::new(),
            Some(LuaValue::String(align.as_str().to_string())),
            None,
        );
        let root = parse_band(&empty).expect("band tree parses");
        let (text, runs) = bitty_runtime::flatten_band_runs_in_width(&root, 40);
        assert!(text.is_empty(), "{align} childless text");
        assert!(runs.is_empty(), "{align} childless runs");
    }
}

#[test]
fn an_over_subscribed_group_clips_inside_the_band_and_never_overlaps() {
    // 10 cells of content in a 8-cell band: no slack exists, so every
    // distribution degrades to the plain flow and the walk clips at the edge.
    let wide = container_value(
        "Row",
        vec![text_value("aaaaa"), text_value("bbbbb")],
        None,
        None,
    );
    let base = spans(&parse_band(&wide).expect("band tree parses"), 8);
    assert_eq!(base, [(0, 5), (5, 8)], "the walk clips at the band edge");
    for align in UiAlign::ALL {
        let value = container_value(
            "Row",
            vec![text_value("aaaaa"), text_value("bbbbb")],
            Some(LuaValue::String(align.as_str().to_string())),
            None,
        );
        let root = parse_band(&value).expect("band tree parses");
        for width in [0usize, 1, 4, 8, 9] {
            let emitted = spans(&root, width);
            let mut previous_end = 0usize;
            for (start, end) in &emitted {
                assert!(
                    end <= &width,
                    "{align} at {width}: run {start}..{end} escapes"
                );
                assert!(start >= &previous_end, "{align} at {width}: runs overlap");
                previous_end = *end;
            }
            for (start, _) in &emitted {
                assert!(start < &width, "{align} at {width}: run starts outside");
            }
        }
        // A group that does not fit keeps the plain flow, never a negative
        // offset: the first child starts at 0 and the tail is clipped.
        assert_eq!(spans(&root, 8), base, "{align} over-subscription");
    }
}

// ---------------------------------------------------------------------------
// Behaviour: one geometry for hit-test and paint
// ---------------------------------------------------------------------------

const PLUGIN: &str = "test-align";

fn cell_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cw, ch) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cw),
        y: pad + (f64::from(row) + 0.5) * f64::from(ch),
    }
}

fn runtime_width(cols: u16) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("runtime must build");
    let (cw, ch) = rt.live_cell_size();
    let pad = rt.window_padding_physical();
    rt.handle_resize(PhysicalSize::new(
        cw.saturating_mul(u32::from(cols))
            .saturating_add(pad.saturating_mul(2)),
        ch.saturating_mul(24).saturating_add(pad.saturating_mul(2)),
    ))
    .expect("resize");
    rt.tick().expect("tick");
    assert_eq!(rt.window_cells().width, cols, "window width seam");
    rt
}

fn mount_top(rt: &mut Runtime, root: UiNode) -> u16 {
    rt.set_chrome_bands_with_mounts(
        ChromeBands {
            top: vec![BandContent {
                plugin_id: PLUGIN.to_string(),
                slot: UiSlot::Top,
                root,
                version: 1,
            }],
            ..Default::default()
        },
        vec![BandMount {
            plugin_id: PLUGIN.to_string(),
            slot: UiSlot::Top,
            version: 1,
            generation: 1,
            handle: 1,
        }],
    );
    rt.tick().expect("tick");
    rt.plugin_band_row(BandEdge::Top, 0)
        .expect("top band occupies a row")
}

fn click_cell(rt: &mut Runtime, row: u16, col: u16) {
    let pos = cell_pixels(rt, row, col);
    rt.handle_cursor_moved(pos);
    let button = MouseButton::Left;
    rt.handle_mouse_input_at(
        bitty_platform::MouseEvent::new(button, PressState::Pressed),
        std::time::Instant::now(),
    );
    rt.handle_mouse_input_at(
        bitty_platform::MouseEvent::new(button, PressState::Released),
        std::time::Instant::now(),
    );
}

#[test]
fn the_hit_test_reads_the_aligned_runs_not_a_second_geometry() {
    // A clickable item at `end`: the region the host routes from must be the
    // cells the align walk placed, so a cell inside the aligned span routes and
    // a cell in the vacated slack does not.
    let tree = container_value(
        "Row",
        vec![
            clickable_text_value("aaa", "first"),
            clickable_text_value("bb", "second"),
            clickable_text_value("ccccc", "third"),
        ],
        align_value("end"),
        None,
    );
    let root = parse_band(&tree).expect("band tree parses");
    let runs = spans(&root, 40);
    assert_eq!(runs, [(30, 33), (33, 35), (35, 40)]);

    let mut rt = runtime_width(40);
    let row = mount_top(&mut rt, root);
    let regions = rt.band_item_regions(BandEdge::Top, 0).expect("regions");
    let mut rects: Vec<(usize, usize, String)> = regions
        .iter()
        .map(|region| (region.start_col, region.end_col, region.region.id.clone()))
        .collect();
    rects.sort_by_key(|(start, _, _)| *start);
    assert_eq!(
        rects,
        [
            (30, 33, "first".to_string()),
            (33, 35, "second".to_string()),
            (35, 40, "third".to_string())
        ],
        "the regions are the aligned cells"
    );

    // A cell inside the last aligned item routes to it.
    click_cell(&mut rt, row, 37);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "the aligned cell routes");
    assert_eq!(clicks[0].region.as_ref().expect("region").id, "third");
    // A cell in the slack the group left behind routes nowhere.
    click_cell(&mut rt, row, 5);
    assert!(
        rt.drain_band_clicks().is_empty(),
        "the vacated slack is not a second geometry"
    );
}

// ---------------------------------------------------------------------------
// Refusals: one per rule in the contract
// ---------------------------------------------------------------------------

fn assert_invalid(result: Result<UiNode, BridgeError>, expected: &str) {
    let error = result.expect_err("refused");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID", "{expected}");
    assert_eq!(error.class, "validation", "{expected}");
    assert_eq!(error.message, expected);
}

#[test]
fn an_unknown_or_non_string_value_is_refused_closed_set_named() {
    // Rule: the vocabulary is closed — `justify`, `middle`, `left`, and a
    // case variant are refused, never clamped to `start`.
    for rejected in ["justify", "middle", "left", "Center", ""] {
        assert_invalid(
            parse_band(&three_siblings(align_value(rejected))),
            &format!(
                "unknown align value '{rejected}': expected 'start', 'center', 'end', or 'space-between'"
            ),
        );
    }
    // Rule: the wire type is a string; an integer, float, or boolean is the
    // same typed refusal instead of a coercion.
    for value in [
        LuaValue::Integer(1),
        LuaValue::Number(1.0),
        LuaValue::Bool(true),
    ] {
        assert_invalid(
            parse_band(&three_siblings(Some(value))),
            "align must be a string",
        );
    }
}

#[test]
fn align_is_refused_on_every_kind_without_a_horizontal_child_group() {
    // Rule: `Row` only. `Text` has no children, `Column`/`List` compose on the
    // other axis; each names the kind it refused.
    let text = LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("leaf".to_string())),
        ("align", LuaValue::String("center".to_string())),
    ]);
    assert_invalid(
        parse_band(&text),
        "align is not valid on a Text node: only a Row composes children horizontally",
    );
    for kind in ["Column", "List"] {
        let container = container_value(kind, vec![text_value("a")], align_value("center"), None);
        assert_invalid(
            parse_band(&container),
            &format!(
                "align is not valid on a {kind} node: only a Row composes children horizontally"
            ),
        );
    }
    // A declared `start` is refused too: the field is position-invalid, not a
    // magnitude, so there is no "0 is inert" carve-out here.
    let leaf_start = LuaValue::table([
        ("kind", LuaValue::String("Text".to_string())),
        ("text", LuaValue::String("leaf".to_string())),
        ("align", LuaValue::String("start".to_string())),
    ]);
    assert_invalid(
        parse_band(&leaf_start),
        "align is not valid on a Text node: only a Row composes children horizontally",
    );
}

#[test]
fn align_is_refused_on_a_nested_row() {
    // Rule: only the band root has the presented width, so a nested `Row` has
    // nothing to distribute inside.
    let nested = container_value(
        "Row",
        vec![container_value(
            "Row",
            vec![text_value("a"), text_value("b")],
            align_value("center"),
            None,
        )],
        None,
        None,
    );
    assert_invalid(
        parse_band(&nested),
        "align is only valid on the scene root Row: a nested Row is sized by its own children",
    );
}

#[test]
fn align_is_refused_outside_a_band_scene() {
    // Rule: an overlay (and a left/right column band) paints content-sized
    // pills, so there is no bounded row to align against.
    assert_invalid(
        parse_overlay(&three_siblings(align_value("center"))),
        "align is only valid in a band scene (top/bottom slot): an overlay pill is content-sized",
    );
    // The same tree is accepted in a band scope, so the refusal is the scope
    // and not the tree.
    assert!(parse_band(&three_siblings(align_value("center"))).is_ok());
}

#[test]
fn align_beside_a_child_anchor_is_refused_independent_of_value() {
    // Rule: one row is placed by one mechanism, so the combination is refused
    // rather than given a precedence. `start` is no exception: declaring both
    // mechanisms is the authoring mistake, not the value.
    for align in UiAlign::ALL {
        let mixed = container_value(
            "Row",
            vec![
                container_value("Row", vec![text_value("left")], None, Some("left")),
                text_value("rest"),
            ],
            Some(LuaValue::String(align.as_str().to_string())),
            None,
        );
        assert_invalid(
            parse_band(&mixed),
            "align and a child anchor are mutually exclusive in the same root Row",
        );
    }
    // An anchored child alone still parses: the mix is what is refused.
    let anchored = container_value(
        "Row",
        vec![container_value(
            "Row",
            vec![text_value("left")],
            None,
            Some("left"),
        )],
        None,
        None,
    );
    assert!(parse_band(&anchored).is_ok());
}

#[test]
fn the_rich_surface_refuses_the_band_only_field() {
    // Rule: scope is band only. The `style`-hosting terminal surface has no
    // band root, so a rich tree that declares `align` is told so instead of
    // silently dropping it.
    let rich = LuaValue::table([
        ("kind", LuaValue::String("Row".to_string())),
        (
            "children",
            LuaValue::array(vec![LuaValue::table([
                ("kind", LuaValue::String("Text".to_string())),
                ("text", LuaValue::String("a".to_string())),
            ])]),
        ),
        ("align", LuaValue::String("center".to_string())),
    ]);
    let error = UiRichNode::from_lua_value(&rich).expect_err("refused");
    assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    assert_eq!(
        error.message,
        "field 'align' is not part of the rich scene surface"
    );
}

#[test]
fn the_read_tree_carries_the_declared_value_and_no_retained_bytes() {
    // A parsed band keeps the value as an enum on the root row, costs no
    // wrapper node, and retains no string (SCN-3/SCN-4 unchanged).
    let root = parse_band(&three_siblings(align_value("space-between"))).expect("band tree parses");
    assert_eq!(root.align(), Some(UiAlign::SpaceBetween));
    assert_eq!(root.kind(), "Row");
    assert_eq!(root.count_nodes(), 4, "the field adds no wrapper node");
    assert_eq!(root.text_bytes(), 10);
    // The value resolves to an enum, so it retains no string: the same tree
    // without `align` retains exactly as many bytes.
    let plain_bytes = parse_band(&three_siblings(None))
        .expect("band tree parses")
        .retained_string_bytes();
    assert_eq!(
        root.retained_string_bytes(),
        plain_bytes,
        "align retains no string"
    );
    // An absent field reports `None`, and a non-Row kind never reports one.
    let plain = parse_band(&three_siblings(None)).expect("band tree parses");
    assert_eq!(plain.align(), None);
    let column = container_value("Column", vec![text_value("a")], None, None);
    let column = parse_band(&column).expect("band tree parses");
    assert_eq!(column.align(), None);
    // A click binding on an aligned child still crosses the walk unchanged:
    // `align` never drops or rewrites a binding.
    let clickable = container_value(
        "Row",
        vec![clickable_text_value("aaa", "first")],
        align_value("end"),
        None,
    );
    let root = parse_band(&clickable).expect("band tree parses");
    let (_, runs) = bitty_runtime::flatten_band_runs_in_width(&root, 20);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].start_col, 17, "the item is end-aligned");
    assert_eq!(
        runs[0].on_click.as_ref().expect("binding").command,
        format!("{PLUGIN}:focus")
    );
    assert_eq!(
        runs[0].on_click.as_ref().expect("binding").args,
        vec![("id".to_string(), ClickArg::String("first".to_string()))]
    );
    assert_eq!(
        runs[0].region.as_ref().expect("region").kind,
        UiRegionKind::Module
    );
}
