#![forbid(unsafe_code)]
//! Lua UI composition parity matrix (issue #2191, CTX-1419).
//!
//! The bridge (`bitty_lua::ui`) is the source of truth for what a plugin may
//! declare. This module observes the *host* for every accepted v1 kind and
//! property, every typed rejection, the bounds, and the click/scroll path. It
//! is layer-crossing by construction: [`UiNode::from_lua_value`] is the exact
//! validator `read_component` calls for a real `bitty.ui.mount` / `ui.update`
//! argument, so an assertion here is a statement about a Lua table a plugin can
//! send.
//!
//! Coverage map (issue #2191 acceptance):
//!
//! | Concern                        | Test |
//! |--------------------------------|------|
//! | accepted kinds render          | [`every_v1_kind_accepts_and_renders`] |
//! | property inheritance/parity    | [`property_matrix_drop_one_fails`] |
//! | unsupported components typed   | [`unsupported_components_fail_typed`] |
//! | bounds fail closed             | [`bounds_fail_closed`] |
//! | click/scroll parity            | [`click_and_scroll_route_through_commands`] |
//! | slot placement parity          | [`slot_placement_is_exhaustive`] |
//! | no plugin VM on the input path | [`hot_path_is_pure_data`] |
//! | no input authority             | [`chrome_gestures_keep_terminal_authority`] |
//! | forward-compatible tolerance   | [`unknown_properties_are_tolerated`] |
//!
//! Host paint (token resolution, bold double-strike, overlay style parity) is
//! pinned by the `plugin_overlay_paint_tests` and `host_chrome_gaps` modules in
//! the runtime crate; this file covers the declarative boundary and routing.

use bitty_lua::host::LuaValue;
use bitty_lua::ui::{
    ClickArg, UI_MAX_DEPTH, UI_MAX_NODES, UI_MAX_TEXT_BYTES, UiNode, UiRegion, UiRegionKind, UiSlot,
};
use bitty_platform::{CursorPosition, MouseButton, PressState, ScrollDelta};
use bitty_runtime::{
    BandButton, BandContent, BandEdge, ChromeBands, Runtime, UiSlotPlacement,
    band_click_args_table, band_regions_for, band_scroll_args_table, flatten_band_runs,
    resolve_click_in_band, resolve_scroll_in_band, ui_slot_placement, unsupported_slot_error,
};

// ---------------------------------------------------------------------------
// Lua-table fixtures (what a plugin actually sends)
// ---------------------------------------------------------------------------

fn string(value: &str) -> LuaValue {
    LuaValue::String(value.to_string())
}

fn command_binding(command: &str, with_region: bool) -> LuaValue {
    let mut pairs = vec![
        (string("command"), string(command)),
        (
            string("args"),
            LuaValue::table([("id", LuaValue::Integer(2))]),
        ),
    ];
    if with_region {
        pairs.push((
            string("region"),
            LuaValue::table([("id", string("ws:2")), ("kind", string("item"))]),
        ));
    }
    LuaValue::Table(pairs)
}

/// `{ kind = "Text", text = <text> }`, the leaf every fixture starts from.
fn plain_text(text: &str) -> LuaValue {
    LuaValue::table([("kind", string("Text")), ("text", string(text))])
}

/// `{ kind = "Text", text = <text> }` with every accepted property declared.
fn styled_text(text: &str) -> LuaValue {
    LuaValue::table([
        ("kind", string("Text")),
        ("text", string(text)),
        ("fg", string("accent")),
        ("bg", string("selection")),
        ("bold", LuaValue::Bool(true)),
        ("on_click", command_binding("test-bar:focus", true)),
        ("on_scroll", command_binding("test-bar:step", true)),
    ])
}

/// The accepted component for one v1 kind.
///
/// `Text` carries the properties itself; `Row`/`Column`/`List` carry them on
/// the container so the host's nearest-declaration-wins inheritance is proven
/// for every kind, not only for leaves.
fn accepted_component(kind: &str) -> LuaValue {
    if kind == "Text" {
        return styled_text("ws2");
    }
    LuaValue::table([
        ("kind", string(kind)),
        ("fg", string("accent")),
        ("bg", string("selection")),
        ("bold", LuaValue::Bool(true)),
        ("on_click", command_binding("test-bar:focus", true)),
        ("on_scroll", command_binding("test-bar:step", true)),
        (
            "children",
            LuaValue::array(vec![plain_text("ws"), plain_text("2")]),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Host observations
// ---------------------------------------------------------------------------

/// One style/command observation of the span that carries the declared
/// properties (the last leaf of the accepted tree).
#[derive(Debug, Clone, PartialEq)]
struct Observed {
    text: String,
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    click: Option<(String, Vec<(String, ClickArg)>)>,
    scroll: Option<(String, Vec<(String, ClickArg)>)>,
    region: Option<UiRegion>,
}

fn observed(root: &UiNode) -> Observed {
    let (text, runs) = flatten_band_runs(root);
    let run = runs.last().expect("every accepted tree paints one run");
    Observed {
        text,
        fg: run.fg.clone(),
        bg: run.bg.clone(),
        bold: run.bold,
        click: run
            .on_click
            .as_ref()
            .map(|binding| (binding.command.clone(), binding.args.clone())),
        scroll: run
            .on_scroll
            .as_ref()
            .map(|binding| (binding.command.clone(), binding.args.clone())),
        region: run.region.clone(),
    }
}

fn region_item() -> UiRegion {
    UiRegion {
        id: "ws:2".to_string(),
        kind: UiRegionKind::Item,
    }
}

fn component(value: LuaValue) -> UiNode {
    UiNode::from_lua_value(&value).expect("accepted Lua component")
}

fn is_component_invalid(error: &bitty_lua::BridgeError) -> bool {
    error.class == "validation" && error.code == "E_UI_COMPONENT_INVALID"
}

// ---------------------------------------------------------------------------
// Accepted kinds
// ---------------------------------------------------------------------------

#[test]
fn every_v1_kind_accepts_and_renders() {
    // Old-tree compatibility plus the additive property path: each v1 kind
    // accepts a tree and the host sees the declared text, style, and bindings
    // on the effective span. `Column`/`List` join horizontally in a one-row
    // band today (documented degradation, design note); the assertion pins the
    // host fact rather than claiming general vertical layout.
    let cases = [
        ("Text", "ws2"),
        ("Row", "ws2"),
        ("Column", "ws2"),
        ("List", "ws2"),
    ];
    for (kind, expected_tail) in cases {
        let root = component(accepted_component(kind));
        let seen = observed(&root);
        assert!(
            seen.text.ends_with(expected_tail),
            "{kind}: host paints the declared text ({:?})",
            seen.text
        );
        assert_eq!(seen.fg.as_deref(), Some("accent"), "{kind}: fg");
        assert_eq!(seen.bg.as_deref(), Some("selection"), "{kind}: bg");
        assert!(seen.bold, "{kind}: bold");
        assert_eq!(
            seen.click,
            Some((
                "test-bar:focus".to_string(),
                vec![("id".to_string(), ClickArg::Number(2.0))]
            )),
            "{kind}: click binding survives to the span"
        );
        assert_eq!(
            seen.scroll,
            Some((
                "test-bar:step".to_string(),
                vec![("id".to_string(), ClickArg::Number(2.0))]
            )),
            "{kind}: scroll binding survives to the span"
        );
        assert_eq!(seen.region, Some(region_item()), "{kind}: region identity");
    }
}

/// One mutation-probe row: property name, mutated Lua table, and the
/// observation the mutation must produce.
type MutationProbe = (&'static str, LuaValue, fn(&Observed) -> bool);

#[test]
fn property_matrix_drop_one_fails() {
    // Mutation check: the matrix above must fail when one accepted property is
    // dropped from the same tree, so it cannot pass vacuously. Each case drops
    // exactly one declaration and asserts the observation loses exactly that
    // signal.
    let baseline = observed(&component(accepted_component("Text")));
    assert_eq!(baseline.fg.as_deref(), Some("accent"));

    let mutations: [MutationProbe; 6] = [
        (
            "fg",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("bg", string("selection")),
                ("bold", LuaValue::Bool(true)),
                ("on_click", command_binding("test-bar:focus", true)),
                ("on_scroll", command_binding("test-bar:step", true)),
            ]),
            |seen| seen.fg.is_none(),
        ),
        (
            "bg",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("fg", string("accent")),
                ("bold", LuaValue::Bool(true)),
                ("on_click", command_binding("test-bar:focus", true)),
                ("on_scroll", command_binding("test-bar:step", true)),
            ]),
            |seen| seen.bg.is_none(),
        ),
        (
            "bold",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("fg", string("accent")),
                ("bg", string("selection")),
                ("on_click", command_binding("test-bar:focus", true)),
                ("on_scroll", command_binding("test-bar:step", true)),
            ]),
            |seen| !seen.bold,
        ),
        (
            "on_click",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("fg", string("accent")),
                ("bg", string("selection")),
                ("bold", LuaValue::Bool(true)),
                ("on_scroll", command_binding("test-bar:step", true)),
            ]),
            |seen| seen.click.is_none(),
        ),
        (
            "on_scroll",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("fg", string("accent")),
                ("bg", string("selection")),
                ("bold", LuaValue::Bool(true)),
                ("on_click", command_binding("test-bar:focus", true)),
            ]),
            |seen| seen.scroll.is_none(),
        ),
        (
            "region",
            LuaValue::table([
                ("kind", string("Text")),
                ("text", string("ws2")),
                ("fg", string("accent")),
                ("bg", string("selection")),
                ("bold", LuaValue::Bool(true)),
                ("on_click", command_binding("test-bar:focus", false)),
                ("on_scroll", command_binding("test-bar:step", false)),
            ]),
            |seen| seen.region.is_none(),
        ),
    ];
    for (property, mutated, changed) in mutations {
        let seen = observed(&component(mutated));
        assert!(
            changed(&seen),
            "dropping {property} must change the host observation"
        );
        assert_ne!(
            seen, baseline,
            "dropping {property} must differ from the accepted tree"
        );
        // The other accepted signals are untouched, so the mutation isolates
        // one property rather than corrupting the fixture.
        assert_eq!(seen.text, baseline.text);
        assert_eq!(seen.fg.is_some(), baseline.fg.is_some() && property != "fg");
        assert_eq!(seen.bg.is_some(), baseline.bg.is_some() && property != "bg");
        assert_eq!(seen.bold, baseline.bold && property != "bold");
        assert_eq!(
            seen.click.is_some(),
            baseline.click.is_some() && property != "on_click"
        );
        assert_eq!(
            seen.scroll.is_some(),
            baseline.scroll.is_some() && property != "on_scroll"
        );
        assert_eq!(
            seen.region.is_some(),
            baseline.region.is_some() && property != "region"
        );
    }
}

// ---------------------------------------------------------------------------
// Unsupported input fails closed with the accepted typed error
// ---------------------------------------------------------------------------

#[test]
fn unsupported_components_fail_typed() {
    let mut cases: Vec<(&str, LuaValue)> = Vec::new();
    // Excluded node kinds and unknown kinds: typed, never ignored.
    for kind in ["Block", "Image", "CodeBlock", "Table", "Rule", "Sparkle"] {
        cases.push((kind, LuaValue::table([("kind", string(kind))])));
    }
    cases.push(("non-table component", string("Text")));
    cases.push(("missing kind", LuaValue::table([("text", string("x"))])));
    cases.push((
        "kind not a string",
        LuaValue::table([("kind", LuaValue::Integer(1))]),
    ));
    cases.push((
        "Text without text",
        LuaValue::table([("kind", string("Text"))]),
    ));
    cases.push((
        "Text with non-string text",
        LuaValue::table([("kind", string("Text")), ("text", LuaValue::Integer(1))]),
    ));
    cases.push((
        "Row without children",
        LuaValue::table([("kind", string("Row"))]),
    ));
    cases.push((
        "children not an array",
        LuaValue::table([("kind", string("Row")), ("children", string("x"))]),
    ));
    // Malformed style fields.
    cases.push((
        "fg not a string",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("fg", LuaValue::Integer(1)),
        ]),
    ));
    cases.push((
        "fg invalid token",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("fg", string("Accent")),
        ]),
    ));
    cases.push((
        "bg invalid token",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("bg", string("9surface")),
        ]),
    ));
    cases.push((
        "bold not a boolean",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("bold", string("true")),
        ]),
    ));
    // Malformed command bindings.
    cases.push((
        "on_click not a table",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("on_click", string("test-bar:focus")),
        ]),
    ));
    cases.push((
        "on_click missing command",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([("args", LuaValue::table([("id", LuaValue::Integer(1))]))]),
            ),
        ]),
    ));
    cases.push((
        "on_click empty command",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("on_click", command_binding("", false)),
        ]),
    ));
    cases.push((
        "on_click overlong command",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            ("on_click", command_binding(&"c".repeat(65), false)),
        ]),
    ));
    cases.push((
        "on_click args not a table",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    ("args", string("id")),
                ]),
            ),
        ]),
    ));
    cases.push((
        "on_click arg value not scalar",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    (
                        "args",
                        LuaValue::table([(
                            "nested",
                            LuaValue::table([("id", LuaValue::Integer(1))]),
                        )]),
                    ),
                ]),
            ),
        ]),
    ));
    cases.push((
        "on_scroll overlong arg value",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", string("test-bar:step")),
                    (
                        "args",
                        LuaValue::table([("step", string(&"v".repeat(257)))]),
                    ),
                ]),
            ),
        ]),
    ));
    // Region identity: closed alphabet, required id, closed kind, agreement.
    cases.push((
        "region not a table",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    ("region", string("clock")),
                ]),
            ),
        ]),
    ));
    cases.push((
        "region missing id",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    ("region", LuaValue::table([("kind", string("module"))])),
                ]),
            ),
        ]),
    ));
    cases.push((
        "region invalid id",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    ("region", LuaValue::table([("id", string("Clock"))])),
                ]),
            ),
        ]),
    ));
    cases.push((
        "region unknown kind",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    (
                        "region",
                        LuaValue::table([("id", string("clock")), ("kind", string("widget"))]),
                    ),
                ]),
            ),
        ]),
    ));
    cases.push((
        "click and scroll regions disagree",
        LuaValue::table([
            ("kind", string("Text")),
            ("text", string("x")),
            (
                "on_click",
                LuaValue::table([
                    ("command", string("test-bar:focus")),
                    (
                        "region",
                        LuaValue::table([("id", string("clock")), ("kind", string("module"))]),
                    ),
                ]),
            ),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", string("test-bar:step")),
                    (
                        "region",
                        LuaValue::table([("id", string("volume")), ("kind", string("module"))]),
                    ),
                ]),
            ),
        ]),
    ));

    for (label, value) in cases {
        let error = UiNode::from_lua_value(&value)
            .err()
            .unwrap_or_else(|| panic!("{label} must fail closed"));
        assert!(
            is_component_invalid(&error),
            "{label}: expected validation/E_UI_COMPONENT_INVALID, got {}/{}",
            error.class,
            error.code
        );
    }
}

#[test]
fn unknown_properties_are_tolerated() {
    // Forward compatibility (accepted Rich Presentation RFC, "Compatibility
    // and versioning": minor versions add optional fields). A field this build
    // does not know — including the not-yet-accepted `tooltip` spelling — is
    // ignored, never guessed and never silently repurposed. Rejecting unknown
    // fields would break the accepted additive-minor rule, so the design note
    // records this as intentional tolerance rather than a parity defect.
    let value = LuaValue::table([
        ("kind", string("Text")),
        ("text", string("ws1")),
        ("tooltip", string("workspace one")),
        ("width", LuaValue::Integer(8)),
        (
            "style",
            LuaValue::table([("padding", LuaValue::Integer(1))]),
        ),
    ]);
    let seen = observed(&component(value));
    assert_eq!(seen.text, "ws1");
    assert_eq!(seen.fg, None);
    assert_eq!(seen.bg, None);
    assert!(!seen.bold);
    assert_eq!(seen.click, None);
    assert_eq!(seen.scroll, None);
    assert_eq!(seen.region, None);
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

fn nested_row(depth: usize) -> LuaValue {
    let mut value = plain_text("leaf");
    for _ in 1..depth {
        value = LuaValue::table([
            ("kind", string("Row")),
            ("children", LuaValue::array(vec![value])),
        ]);
    }
    value
}

#[test]
fn bounds_fail_closed() {
    // Each accepted bound is checked at its exact edge: the limit itself is
    // accepted and one over is a typed rejection, so a later refactor cannot
    // silently change a documented number.
    let depth_limit = component(nested_row(UI_MAX_DEPTH));
    assert_eq!(depth_limit.depth(), UI_MAX_DEPTH);
    let depth_over = UiNode::from_lua_value(&nested_row(UI_MAX_DEPTH + 1))
        .expect_err("depth over the bridge budget");
    assert!(is_component_invalid(&depth_over));

    let nodes_limit = component(LuaValue::table([
        ("kind", string("Row")),
        (
            "children",
            LuaValue::array((1..UI_MAX_NODES).map(|_| plain_text("")).collect()),
        ),
    ]));
    assert_eq!(nodes_limit.count_nodes(), UI_MAX_NODES);
    let nodes_over = UiNode::from_lua_value(&LuaValue::table([
        ("kind", string("Row")),
        (
            "children",
            LuaValue::array((0..UI_MAX_NODES).map(|_| plain_text("")).collect()),
        ),
    ]))
    .expect_err("node over the SCN-1 budget");
    assert!(is_component_invalid(&nodes_over));

    let text_limit = component(plain_text(&"x".repeat(UI_MAX_TEXT_BYTES)));
    assert_eq!(text_limit.text_bytes(), UI_MAX_TEXT_BYTES);
    let text_over =
        UiNode::from_lua_value(&plain_text(&"x".repeat(UI_MAX_TEXT_BYTES + 1))).expect_err("SCN-3");
    assert!(is_component_invalid(&text_over));
}

// ---------------------------------------------------------------------------
// Slots and typed host errors
// ---------------------------------------------------------------------------

#[test]
fn slot_placement_is_exhaustive() {
    // The bridge slot set and the host placement policy agree for every
    // accepted name; an unhosted slot fails with the frozen v1 code instead of
    // returning a handle the renderer would never paint.
    assert_eq!(UiSlot::ALL.len(), 7);
    for slot in UiSlot::ALL {
        match (slot, ui_slot_placement(slot)) {
            (UiSlot::Top, UiSlotPlacement::Band(BandEdge::Top))
            | (UiSlot::Bottom, UiSlotPlacement::Band(BandEdge::Bottom))
            | (UiSlot::Overlay, UiSlotPlacement::Overlay) => {}
            (
                UiSlot::Left | UiSlot::Right | UiSlot::Tabline | UiSlot::Terminal,
                UiSlotPlacement::Unsupported(_),
            ) => {}
            (other, placement) => {
                panic!("{other:?} placement changed without a parity decision: {placement:?}")
            }
        }
    }
    for slot in [
        UiSlot::Left,
        UiSlot::Right,
        UiSlot::Tabline,
        UiSlot::Terminal,
    ] {
        let error = unsupported_slot_error(slot, "parity probe");
        assert_eq!(error.class, "runtime");
        assert_eq!(error.code, bitty_lua::E_UI_UNAVAILABLE);
        assert!(error.message.contains(slot.as_str()));
    }
    // Removed/unknown spellings are not slots at all.
    for name in ["statusline", "Statusline", "workspaces", " top"] {
        assert_eq!(UiSlot::parse(name), None, "{name:?} must not parse");
    }
}

// ---------------------------------------------------------------------------
// Click and scroll parity
// ---------------------------------------------------------------------------

fn minimal_runtime() -> Runtime {
    Runtime::with_defaults().expect("runtime builds")
}

fn mount_bottom(rt: &mut Runtime, root: UiNode) {
    rt.set_chrome_bands(ChromeBands {
        bottom: vec![BandContent {
            plugin_id: "test-bar".to_string(),
            slot: UiSlot::Bottom,
            root,
            version: 1,
        }],
        ..Default::default()
    });
}

fn band_row(rt: &Runtime) -> u16 {
    rt.plugin_band_row(BandEdge::Bottom, 0)
        .expect("bottom band fits")
}

fn cell_pixels(rt: &Runtime, row: u16, col: u16) -> CursorPosition {
    let (cell_w, cell_h) = rt.live_cell_size();
    let pad = f64::from(rt.window_padding_physical());
    CursorPosition {
        x: pad + (f64::from(col) + 0.5) * f64::from(cell_w),
        y: pad + (f64::from(row) + 0.5) * f64::from(cell_h),
    }
}

fn press() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Pressed)
}

fn release() -> bitty_platform::MouseEvent {
    bitty_platform::MouseEvent::new(MouseButton::Left, PressState::Released)
}

fn click_at(rt: &mut Runtime, row: u16, col: u16) {
    rt.handle_cursor_moved(cell_pixels(rt, row, col));
    rt.handle_mouse_input(press());
    rt.handle_mouse_input(release());
}

#[test]
fn click_and_scroll_route_through_commands() {
    // A Lua-authored tree reaches the host as a plain `UiNode`, and a real
    // frame's geometry turns pointer input into a declared command. The click
    // carries the region id and button; the wheel carries the host-computed
    // delta. Nothing here constructs a plugin VM or dispatches a Lua function.
    let mut rt = minimal_runtime();
    mount_bottom(&mut rt, component(accepted_component("Text")));
    rt.tick();
    let row = band_row(&rt);

    // "ws2" starts at column 0; column 1 is inside it (and inside the region).
    click_at(&mut rt, row, 1);
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1, "one declared click routes once");
    assert_eq!(clicks[0].plugin_id, "test-bar");
    assert_eq!(clicks[0].command, "focus", "owner qualifier is stripped");
    assert_eq!(
        clicks[0].args,
        vec![("id".to_string(), ClickArg::Number(2.0))],
        "declared args survive unchanged"
    );
    assert_eq!(clicks[0].region, Some(region_item()));
    assert_eq!(clicks[0].button, BandButton::Left);
    // The host-injected values are what dispatch sees, merged last.
    let click_table =
        band_click_args_table(&clicks[0].args, clicks[0].region.as_ref(), clicks[0].button);
    assert_eq!(click_table.get("id"), Some(&LuaValue::Number(2.0)));
    assert_eq!(click_table.get("region"), Some(&string("ws:2")));
    assert_eq!(click_table.get("button"), Some(&string("left")));

    // Wheel over the same span: the declared scroll command plus `delta`.
    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    let scrolls = rt.drain_band_scrolls();
    assert_eq!(scrolls.len(), 1, "one declared scroll routes once");
    assert_eq!(scrolls[0].command, "step");
    assert_eq!(scrolls[0].region, Some(region_item()));
    assert!(scrolls[0].delta > 0, "positive delta for wheel-up");
    let scroll_table = band_scroll_args_table(
        &scrolls[0].args,
        scrolls[0].delta,
        scrolls[0].region.as_ref(),
    );
    assert_eq!(scroll_table.get("id"), Some(&LuaValue::Number(2.0)));
    assert_eq!(scroll_table.get("region"), Some(&string("ws:2")));
    assert_eq!(
        scroll_table.get("delta"),
        Some(&LuaValue::Number(f64::from(scrolls[0].delta)))
    );
}

#[test]
fn legacy_span_keeps_primary_button_and_args() {
    // Backward compatibility: a region-less binding routes only the primary
    // button and its payload is byte-for-byte the declared args (no `button`,
    // no `region`), so strict command schemas keep working.
    let value = LuaValue::table([
        ("kind", string("Text")),
        ("text", string("ws1")),
        (
            "on_click",
            LuaValue::table([
                ("command", string("test-bar:focus")),
                ("args", LuaValue::table([("id", LuaValue::Integer(1))])),
            ]),
        ),
    ]);
    let runs = flatten_band_runs(&component(value)).1;
    let hit = resolve_click_in_band("test-bar", &runs, 0, BandButton::Left);
    let request = match hit {
        bitty_runtime::BandColumnOutcome::Hit(request) => request,
        other => panic!("expected a hit, got {other:?}"),
    };
    assert_eq!(request.region, None);
    assert_eq!(
        band_click_args_table(&request.args, request.region.as_ref(), request.button),
        LuaValue::table([("id", LuaValue::Number(1.0))])
    );
    // Right/middle clicks on a legacy span are unclaimed chrome.
    assert_eq!(
        resolve_click_in_band("test-bar", &runs, 0, BandButton::Right),
        bitty_runtime::BandColumnOutcome::NoClaim
    );
    // A foreign qualifier is denied, never routed across plugins.
    let foreign = LuaValue::table([
        ("kind", string("Text")),
        ("text", string("ws1")),
        ("on_click", command_binding("other-plugin:focus", false)),
        ("on_scroll", command_binding("other-plugin:step", false)),
    ]);
    let foreign_runs = flatten_band_runs(&component(foreign)).1;
    assert_eq!(
        resolve_click_in_band("test-bar", &foreign_runs, 0, BandButton::Left),
        bitty_runtime::BandColumnOutcome::Denied
    );
    assert_eq!(
        resolve_scroll_in_band("test-bar", &foreign_runs, 0),
        bitty_runtime::BandScrollOutcome::Denied
    );
}

#[test]
fn regions_are_derived_from_painted_spans() {
    // The plugin never supplies coordinates: the emitted rect comes from the
    // same flattened runs the paint path uses, clipped to the window width.
    let band = BandContent {
        plugin_id: "test-bar".to_string(),
        slot: UiSlot::Bottom,
        root: component(accepted_component("Row")),
        version: 4,
    };
    let regions = band_regions_for(&band, BandEdge::Bottom, 0, 23, 80).expect("region table");
    assert_eq!(regions.len(), 1, "one declared item, merged across leaves");
    assert_eq!(regions[0].region, region_item());
    assert_eq!((regions[0].start_col, regions[0].end_col), (0, 3));
    assert_eq!(regions[0].version, 4);
    // A narrower window clips the same geometry rather than rejecting it.
    let clipped = band_regions_for(&band, BandEdge::Bottom, 0, 23, 2).expect("clipped table");
    assert_eq!((clipped[0].start_col, clipped[0].end_col), (0, 2));
}

// ---------------------------------------------------------------------------
// Hot path and authority
// ---------------------------------------------------------------------------

#[test]
fn hot_path_is_pure_data() {
    // Rendering, hit-testing, and region emission are plain walks over the
    // mounted `UiNode`: no VM, no Lua callback, no host object. This test runs
    // without ever constructing a `PluginRuntime`, which is the structural
    // proof that the input/render path cannot enter plugin code.
    let root = component(accepted_component("Row"));
    let (text, runs) = flatten_band_runs(&root);
    assert_eq!(text, "ws2");
    let region_table = band_regions_for(
        &BandContent {
            plugin_id: "test-bar".to_string(),
            slot: UiSlot::Bottom,
            root: root.clone(),
            version: 1,
        },
        BandEdge::Bottom,
        0,
        0,
        80,
    )
    .expect("region table");
    assert_eq!(region_table.len(), 1);
    let click = resolve_click_in_band("test-bar", &runs, 1, BandButton::Left);
    let scroll = resolve_scroll_in_band("test-bar", &runs, 1);
    assert!(matches!(click, bitty_runtime::BandColumnOutcome::Hit(_)));
    assert!(matches!(scroll, bitty_runtime::BandScrollOutcome::Hit(_)));
}

#[test]
fn chrome_gestures_keep_terminal_authority() {
    // Chrome consumes its own gestures: no focus move, no selection, no PTY
    // input, and no plugin VM. The routed request carries only the declared
    // command triple plus host-owned region/button/delta — never pointer
    // coordinates, so presentation data cannot become input authority.
    let mut rt = minimal_runtime();
    mount_bottom(&mut rt, component(accepted_component("Text")));
    rt.tick();
    let row = band_row(&rt);
    let focused = rt.focused_view();
    let primary = rt.primary_view();

    click_at(&mut rt, row, 1);
    assert_eq!(rt.focused_view(), focused, "chrome click moves no focus");
    assert_eq!(rt.primary_view(), primary, "chrome click changes no layout");
    assert!(!rt.has_selection(), "chrome click starts no selection");
    let clicks = rt.drain_band_clicks();
    assert_eq!(clicks.len(), 1);
    assert_eq!(
        clicks[0].args,
        vec![("id".to_string(), ClickArg::Number(2.0))],
        "the routed payload is the declared args only (no coordinates)"
    );

    rt.handle_wheel(ScrollDelta::Lines(0.0, 1.0));
    assert!(!rt.has_selection(), "chrome scroll starts no selection");
    assert_eq!(rt.drain_band_scrolls().len(), 1);
    assert!(
        rt.drain_band_clicks().is_empty(),
        "a wheel never double-routes as a click"
    );
}
