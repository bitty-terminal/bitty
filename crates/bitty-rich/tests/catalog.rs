//! Extension component catalog guards (bitty#2114, CTX-1416).
//!
//! The catalog lowers typed descriptors onto the accepted Plugin API v1 node
//! set (`Text | Row | Column | List`). These tests pin the four properties the
//! contract promises, and each one is written so that breaking the property
//! fails the test rather than degrading quietly:
//!
//! 1. **Shape.** Every composition lowers to the node kind and child count its
//!    documentation names, and nothing outside the four v1 kinds.
//! 2. **Determinism.** Declaration order is carried, never derived: identical
//!    declarations rebuild byte-identically, permuted input permutes the output,
//!    and duplicates survive. A sort, a dedup, or a hashed collection anywhere
//!    in the lowering breaks at least one of these.
//! 3. **Caps.** Each composition's collection cap and label cap are inclusive,
//!    and `cap + 1` is refused with a typed [`CatalogError`] — never truncated
//!    and never clamped. Boundary and boundary+1 are both exercised for every
//!    composition.
//! 4. **No borrowed authority.** No style, border, colour, emphasis, role, or
//!    input binding reaches the output tree; a catalog tree is a v1 tree and
//!    admits at [`bitty_rich::Scene::insert`] under `SCN-1..5`.
//!
//! The last test is a source guard: the module itself must not name a hashed
//! collection, a sort, or an excluded `SceneNode` kind, so the properties above
//! cannot be reintroduced behind a passing shape test.

use std::fs;
use std::path::Path;

use bitty_rich::scene::RICH_BLOCK_VERSION;
use bitty_rich::{
    BandSpec, BlockAnchor, CATALOG_MAX_BAND_SLOTS, CATALOG_MAX_DETAIL_LINES,
    CATALOG_MAX_LABEL_BYTES, CATALOG_MAX_PILL_LABEL_BYTES, CATALOG_MAX_PILLS,
    CATALOG_MAX_SHEET_LINES, CATALOG_MAX_STATUS_SEGMENTS, CatalogComponent, CatalogError,
    CompositionKind, DetailSpec, PillRowSpec, SCENE_MAX_DEPTH, SCENE_MAX_NODES_PER_BLOCK,
    SCENE_MAX_TEXT_BYTES_PER_BLOCK, Scene, SceneNode, ScrollBehavior, SheetSpec, StatusSegment,
    StatusStripSpec,
};

// The pill cap must stay strictly tighter than the general label cap: equal
// caps would leave a pill row and a band with no contract difference at all,
// since the v1 node set gives both the same `Row` shape.
const _: () = assert!(CATALOG_MAX_PILL_LABEL_BYTES < CATALOG_MAX_LABEL_BYTES);

// ---------------------------------------------------------------------------
// Walkers
// ---------------------------------------------------------------------------

/// Walks a catalog tree, returning its node kinds in pre-order.
///
/// Panics on any kind outside the four v1 nodes, on a non-plain text span, or
/// on an ordered list: the catalog promises all three, so a tree that trips
/// this cannot have come from the catalog's contract.
fn assert_catalog_tree(node: &SceneNode) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    walk(node, &mut kinds);
    kinds
}

fn walk(node: &SceneNode, kinds: &mut Vec<&'static str>) {
    match node {
        SceneNode::Text(span) => {
            assert!(
                !span.bold,
                "catalog text {span:?} claims bold; presentation is a separate change"
            );
            assert!(
                !span.italic,
                "catalog text {span:?} claims italic; presentation is a separate change"
            );
            kinds.push("text");
        }
        SceneNode::Row(children) => {
            kinds.push("row");
            for child in children {
                walk(child, kinds);
            }
        }
        SceneNode::Column(children) => {
            kinds.push("column");
            for child in children {
                walk(child, kinds);
            }
        }
        SceneNode::List(model) => {
            assert!(
                !model.ordered,
                "a detail fallback is an unordered list, not an ordinal one"
            );
            kinds.push("list");
        }
        other => panic!("catalog emitted {other:?}, outside the four v1 kinds"),
    }
}

/// Every composition at its documented cap, for budget and determinism sweeps.
fn at_cap_components() -> Vec<CatalogComponent> {
    vec![
        CatalogComponent::Band(
            BandSpec::new("band", vec!["slot".to_string(); CATALOG_MAX_BAND_SLOTS])
                .expect("a band at its slot cap is accepted"),
        ),
        CatalogComponent::PillRow(
            PillRowSpec::new(vec!["pill".to_string(); CATALOG_MAX_PILLS])
                .expect("a pill row at its cap is accepted"),
        ),
        CatalogComponent::StatusStrip(
            StatusStripSpec::new(
                (0..CATALOG_MAX_STATUS_SEGMENTS)
                    .map(|index| StatusSegment::new("label", format!("value {index}")))
                    .collect(),
            )
            .expect("a status strip at its cap is accepted"),
        ),
        CatalogComponent::Detail(
            DetailSpec::new(vec!["line".to_string(); CATALOG_MAX_DETAIL_LINES])
                .expect("a detail block at its cap is accepted"),
        ),
        CatalogComponent::Sheet(
            SheetSpec::new("sheet", vec!["body".to_string(); CATALOG_MAX_SHEET_LINES])
                .expect("a sheet at its cap is accepted"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// 1. Shape
// ---------------------------------------------------------------------------

#[test]
fn band_lowers_to_a_row_with_the_title_then_the_trailing_slots() {
    let band = BandSpec::new(
        "main",
        vec!["2".to_string(), "zsh".to_string(), "80x24".to_string()],
    )
    .expect("three trailing slots is within the cap");

    let tree = band.build();
    assert_eq!(
        assert_catalog_tree(&tree),
        vec!["row", "text", "text", "text", "text"],
        "band is Row[ Text(title), Text(slot) x 3 ]"
    );

    let SceneNode::Row(children) = &tree else {
        panic!("a band must lower to a Row, got {tree:?}");
    };
    assert_eq!(children.len(), 4, "one title plus three trailing slots");
    assert_eq!(band.title(), "main");
    assert_eq!(band.trailing().len(), 3);
    let SceneNode::Text(title) = &children[0] else {
        panic!("the band's first child must be the title text");
    };
    assert_eq!(title.text, "main", "the title stays first");
}

#[test]
fn pill_row_lowers_to_a_row_of_one_text_per_pill() {
    let pills = PillRowSpec::new(vec!["NEW".to_string(), "LSP".to_string()]).expect("within caps");

    let tree = pills.build();
    assert_eq!(assert_catalog_tree(&tree), vec!["row", "text", "text"]);

    let SceneNode::Row(children) = &tree else {
        panic!("a pill row must lower to a Row, got {tree:?}");
    };
    assert_eq!(
        children.len(),
        2,
        "one text per pill, no separators invented"
    );
    assert_eq!(pills.pills().len(), 2);
}

#[test]
fn status_strip_groups_each_label_with_its_value() {
    let strip = StatusStripSpec::new(vec![
        StatusSegment::new("cpu", "12%"),
        StatusSegment::new("mem", "1.2G"),
    ])
    .expect("within caps");

    let tree = strip.build();
    assert_eq!(
        assert_catalog_tree(&tree),
        vec!["row", "row", "text", "text", "row", "text", "text"],
        "status strip is Row[ Row[ Text(label), Text(value) ] x 2 ]"
    );

    let SceneNode::Row(segments) = &tree else {
        panic!("a status strip must lower to a Row, got {tree:?}");
    };
    assert_eq!(segments.len(), 2);
    for segment in segments {
        assert_eq!(
            segment.count_nodes(),
            3,
            "each segment is a Row of exactly two text leaves"
        );
    }
    assert_eq!(strip.segments()[0].label(), "cpu");
    assert_eq!(strip.segments()[0].value(), "12%");
}

#[test]
fn detail_lowers_to_an_unordered_list_of_its_lines() {
    let detail = DetailSpec::new(vec![
        "scope: one".to_string(),
        "owner: extension".to_string(),
    ])
    .expect("within caps");

    let tree = detail.build();
    assert_eq!(
        assert_catalog_tree(&tree),
        vec!["list"],
        "the detail fallback is a single List node, not a hand-built Column"
    );

    let SceneNode::List(model) = &tree else {
        panic!("a detail block must lower to a List, got {tree:?}");
    };
    assert_eq!(model.items.len(), 2);
    assert_eq!(model.items[0], "scope: one");
    assert_eq!(tree.count_nodes(), 3, "one list node plus its two items");
    assert_eq!(detail.lines().len(), 2);
}

#[test]
fn sheet_lowers_to_a_column_with_the_title_then_the_body() {
    let sheet = SheetSpec::new("Confirm", vec!["paste 4 lines?".to_string()]).expect("within caps");

    let tree = sheet.build();
    assert_eq!(
        assert_catalog_tree(&tree),
        vec!["column", "text", "text"],
        "sheet is Column[ Text(title), Text(line) x 1 ]"
    );

    let SceneNode::Column(children) = &tree else {
        panic!("a sheet must lower to a Column, got {tree:?}");
    };
    assert_eq!(children.len(), 2, "one title plus one body line");
    assert_eq!(sheet.title(), "Confirm");
}

#[test]
fn the_component_wrapper_reports_the_kind_and_builds_the_same_tree() {
    let cases = at_cap_components();
    let expected = [
        CompositionKind::Band,
        CompositionKind::PillRow,
        CompositionKind::StatusStrip,
        CompositionKind::Detail,
        CompositionKind::Sheet,
    ];
    assert_eq!(cases.len(), expected.len());

    for (component, kind) in cases.iter().zip(expected) {
        assert_eq!(component.kind(), kind);
        assert_eq!(
            component.kind().as_str(),
            kind.as_str(),
            "the wrapper kind and the descriptor kind agree"
        );
        let direct = match component {
            CatalogComponent::Band(spec) => spec.build(),
            CatalogComponent::PillRow(spec) => spec.build(),
            CatalogComponent::StatusStrip(spec) => spec.build(),
            CatalogComponent::Detail(spec) => spec.build(),
            CatalogComponent::Sheet(spec) => spec.build(),
        };
        assert_eq!(
            component.build(),
            direct,
            "the wrapper adds no behaviour to the lowering"
        );
        assert_catalog_tree(&component.build());
    }
}

// ---------------------------------------------------------------------------
// 2. Determinism
// ---------------------------------------------------------------------------

#[test]
fn equal_declarations_lower_to_byte_identical_trees() {
    for component in at_cap_components() {
        let kind = component.kind();
        let first = format!("{:?}", component.build());
        let second = format!("{:?}", component.build());
        let rebuilt = format!("{:?}", component.clone().build());
        assert_eq!(first, second, "{kind} is not stable across two builds");
        assert_eq!(
            first, rebuilt,
            "{kind} depends on descriptor identity rather than its value"
        );
    }
}

#[test]
fn declaration_order_is_carried_not_normalised() {
    let forward =
        PillRowSpec::new(vec!["a".to_string(), "b".to_string(), "c".to_string()]).expect("ok");
    let reversed =
        PillRowSpec::new(vec!["c".to_string(), "b".to_string(), "a".to_string()]).expect("ok");
    assert_ne!(
        forward.build(),
        reversed.build(),
        "the lowering must not sort or reorder its input"
    );

    let band_a = BandSpec::new("t", vec!["1".to_string(), "2".to_string()]).expect("ok");
    let band_b = BandSpec::new("t", vec!["2".to_string(), "1".to_string()]).expect("ok");
    assert_ne!(
        band_a.build(),
        band_b.build(),
        "trailing slots keep their declared order"
    );

    let duplicated = PillRowSpec::new(vec!["a".to_string(), "a".to_string()]).expect("ok");
    assert_eq!(
        duplicated.build().count_nodes(),
        3,
        "a repeated label must not be deduplicated away"
    );
}

// ---------------------------------------------------------------------------
// 3. Caps: boundary accepted, boundary + 1 refused, nothing truncated
// ---------------------------------------------------------------------------

fn band_with_trailing_slots(slots: usize) -> Result<BandSpec, CatalogError> {
    BandSpec::new("band", vec!["slot".to_string(); slots])
}

fn pill_row_with_pills(pills: usize) -> Result<PillRowSpec, CatalogError> {
    PillRowSpec::new(vec!["pill".to_string(); pills])
}

fn status_strip_with_segments(segments: usize) -> Result<StatusStripSpec, CatalogError> {
    StatusStripSpec::new(
        (0..segments)
            .map(|index| StatusSegment::new("label", format!("value {index}")))
            .collect(),
    )
}

fn detail_with_lines(lines: usize) -> Result<DetailSpec, CatalogError> {
    DetailSpec::new(vec!["line".to_string(); lines])
}

fn sheet_with_body_lines(lines: usize) -> Result<SheetSpec, CatalogError> {
    SheetSpec::new("sheet", vec!["body".to_string(); lines])
}

#[test]
fn band_trailing_slots_accept_the_cap_and_refuse_the_next() {
    assert!(
        band_with_trailing_slots(CATALOG_MAX_BAND_SLOTS).is_ok(),
        "the cap itself is inclusive"
    );
    assert_eq!(
        band_with_trailing_slots(CATALOG_MAX_BAND_SLOTS + 1).unwrap_err(),
        CatalogError::TooManyItems {
            composition: CompositionKind::Band,
            found: CATALOG_MAX_BAND_SLOTS + 1,
            cap: CATALOG_MAX_BAND_SLOTS,
        },
        "one slot past the cap is refused, not dropped"
    );
}

#[test]
fn pill_row_accepts_the_cap_and_refuses_the_next_pill() {
    assert!(
        pill_row_with_pills(CATALOG_MAX_PILLS).is_ok(),
        "the cap itself is inclusive"
    );
    assert_eq!(
        pill_row_with_pills(CATALOG_MAX_PILLS + 1).unwrap_err(),
        CatalogError::TooManyItems {
            composition: CompositionKind::PillRow,
            found: CATALOG_MAX_PILLS + 1,
            cap: CATALOG_MAX_PILLS,
        }
    );
}

#[test]
fn status_strip_accepts_the_cap_and_refuses_the_next_segment() {
    assert!(
        status_strip_with_segments(CATALOG_MAX_STATUS_SEGMENTS).is_ok(),
        "the cap itself is inclusive"
    );
    assert_eq!(
        status_strip_with_segments(CATALOG_MAX_STATUS_SEGMENTS + 1).unwrap_err(),
        CatalogError::TooManyItems {
            composition: CompositionKind::StatusStrip,
            found: CATALOG_MAX_STATUS_SEGMENTS + 1,
            cap: CATALOG_MAX_STATUS_SEGMENTS,
        }
    );
}

#[test]
fn detail_accepts_the_cap_and_refuses_the_next_line() {
    assert!(
        detail_with_lines(CATALOG_MAX_DETAIL_LINES).is_ok(),
        "the cap itself is inclusive"
    );
    assert_eq!(
        detail_with_lines(CATALOG_MAX_DETAIL_LINES + 1).unwrap_err(),
        CatalogError::TooManyItems {
            composition: CompositionKind::Detail,
            found: CATALOG_MAX_DETAIL_LINES + 1,
            cap: CATALOG_MAX_DETAIL_LINES,
        }
    );
}

#[test]
fn sheet_accepts_the_cap_and_refuses_the_next_body_line() {
    assert!(
        sheet_with_body_lines(CATALOG_MAX_SHEET_LINES).is_ok(),
        "the cap itself is inclusive"
    );
    assert_eq!(
        sheet_with_body_lines(CATALOG_MAX_SHEET_LINES + 1).unwrap_err(),
        CatalogError::TooManyItems {
            composition: CompositionKind::Sheet,
            found: CATALOG_MAX_SHEET_LINES + 1,
            cap: CATALOG_MAX_SHEET_LINES,
        }
    );
}

#[test]
fn a_label_may_fill_its_cap_and_one_byte_more_is_refused() {
    let at_cap = "a".repeat(CATALOG_MAX_LABEL_BYTES);
    assert_eq!(at_cap.len(), CATALOG_MAX_LABEL_BYTES);
    assert!(
        BandSpec::new(at_cap.clone(), Vec::new()).is_ok(),
        "a label of exactly the cap is accepted"
    );

    let over_cap = "a".repeat(CATALOG_MAX_LABEL_BYTES + 1);
    assert_eq!(
        BandSpec::new(over_cap, Vec::new()).unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::Band,
            slot: 0,
            found_bytes: CATALOG_MAX_LABEL_BYTES + 1,
            cap_bytes: CATALOG_MAX_LABEL_BYTES,
        },
        "one byte past the cap is refused, not truncated"
    );
}

#[test]
fn trailing_slots_are_numbered_after_the_title() {
    let over_cap = "a".repeat(CATALOG_MAX_LABEL_BYTES + 1);
    assert_eq!(
        BandSpec::new("ok", vec!["fine".to_string(), over_cap]).unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::Band,
            slot: 2,
            found_bytes: CATALOG_MAX_LABEL_BYTES + 1,
            cap_bytes: CATALOG_MAX_LABEL_BYTES,
        },
        "the title is slot 0, so the second trailing slot is slot 2"
    );
}

#[test]
fn a_pill_label_has_a_tighter_cap_than_a_band_label() {
    let at_pill_cap = "p".repeat(CATALOG_MAX_PILL_LABEL_BYTES);
    assert!(
        PillRowSpec::new(vec![at_pill_cap.clone()]).is_ok(),
        "a pill at its own cap is accepted"
    );
    assert!(
        BandSpec::new(at_pill_cap, Vec::new()).is_ok(),
        "the same label is legal on a band, which is what separates the two"
    );

    let over_cap = "p".repeat(CATALOG_MAX_PILL_LABEL_BYTES + 1);
    assert_eq!(
        PillRowSpec::new(vec![over_cap]).unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::PillRow,
            slot: 0,
            found_bytes: CATALOG_MAX_PILL_LABEL_BYTES + 1,
            cap_bytes: CATALOG_MAX_PILL_LABEL_BYTES,
        }
    );
}

#[test]
fn status_strip_caps_both_halves_of_a_segment() {
    let over_cap = "a".repeat(CATALOG_MAX_LABEL_BYTES + 1);
    assert_eq!(
        StatusStripSpec::new(vec![
            StatusSegment::new("cpu", "12%"),
            StatusSegment::new("mem", over_cap.clone()),
        ])
        .unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::StatusStrip,
            slot: 1,
            found_bytes: CATALOG_MAX_LABEL_BYTES + 1,
            cap_bytes: CATALOG_MAX_LABEL_BYTES,
        },
        "a runaway value is refused just like a runaway label"
    );
    assert_eq!(
        StatusStripSpec::new(vec![StatusSegment::new(over_cap, "12%")]).unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::StatusStrip,
            slot: 0,
            found_bytes: CATALOG_MAX_LABEL_BYTES + 1,
            cap_bytes: CATALOG_MAX_LABEL_BYTES,
        }
    );
}

#[test]
fn label_caps_count_bytes_rather_than_characters() {
    // U+00E9 is two bytes, so 128 of them fill the 256-byte cap exactly.
    let accented = "\u{e9}";
    let at_cap = accented.repeat(CATALOG_MAX_LABEL_BYTES / 2);
    assert_eq!(at_cap.len(), CATALOG_MAX_LABEL_BYTES);
    assert_eq!(at_cap.chars().count(), CATALOG_MAX_LABEL_BYTES / 2);
    assert!(
        DetailSpec::new(vec![at_cap]).is_ok(),
        "the cap is a byte budget, and this label fills it"
    );

    let over_cap = accented.repeat(CATALOG_MAX_LABEL_BYTES / 2 + 1);
    assert_eq!(
        DetailSpec::new(vec![over_cap]).unwrap_err(),
        CatalogError::LabelTooLong {
            composition: CompositionKind::Detail,
            slot: 0,
            found_bytes: CATALOG_MAX_LABEL_BYTES + 2,
            cap_bytes: CATALOG_MAX_LABEL_BYTES,
        },
        "a 129-character label is 258 bytes and is refused"
    );
}

#[test]
fn every_composition_refuses_an_empty_declaration() {
    assert_eq!(
        BandSpec::new("", Vec::new()).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::Band,
        },
        "a band without a title is not a band"
    );
    assert_eq!(
        PillRowSpec::new(Vec::new()).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::PillRow,
        }
    );
    assert_eq!(
        StatusStripSpec::new(Vec::new()).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::StatusStrip,
        }
    );
    assert_eq!(
        DetailSpec::new(Vec::new()).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::Detail,
        }
    );
    assert_eq!(
        SheetSpec::new("", vec!["body".to_string()]).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::Sheet,
        }
    );
    assert_eq!(
        SheetSpec::new("title", Vec::new()).unwrap_err(),
        CatalogError::Empty {
            composition: CompositionKind::Sheet,
        },
        "a sheet without a body is not a sheet"
    );
}

#[test]
fn refusals_name_the_composition_the_value_and_the_cap() {
    let error = band_with_trailing_slots(CATALOG_MAX_BAND_SLOTS + 1).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("band"),
        "message names the composition: {message}"
    );
    assert!(
        message.contains(&(CATALOG_MAX_BAND_SLOTS + 1).to_string()),
        "message names the rejected value: {message}"
    );
    assert!(
        message.contains(&CATALOG_MAX_BAND_SLOTS.to_string()),
        "message names the cap: {message}"
    );

    let empty = DetailSpec::new(Vec::new()).unwrap_err().to_string();
    assert!(
        empty.contains("detail"),
        "message names the composition: {empty}"
    );
}

// ---------------------------------------------------------------------------
// 4. No borrowed authority, and the block budget stays out of reach
// ---------------------------------------------------------------------------

#[test]
fn a_catalog_tree_is_a_v1_tree_with_no_style_or_role_claim() {
    for component in at_cap_components() {
        let tree = component.build();
        // The walker is the assertion: it rejects every kind outside the four
        // v1 nodes, an ordered list, and any bold or italic span.
        let kinds = assert_catalog_tree(&tree);
        assert!(
            matches!(kinds[0], "row" | "column" | "list"),
            "{} lowered to {} at the root; a catalog composition is a bare layout node, \
             never a style or border wrapper",
            component.kind(),
            kinds[0]
        );
        assert_eq!(
            tree.schema_version(),
            RICH_BLOCK_VERSION,
            "a catalog tree requires no newer scene schema than v1"
        );
    }
}

#[test]
fn no_composition_can_spend_a_block_budget() {
    for component in at_cap_components() {
        let kind = component.kind();
        let tree = component.build();
        assert!(
            tree.count_nodes() <= SCENE_MAX_NODES_PER_BLOCK,
            "{kind} at cap uses {} nodes, over SCN-1's {SCENE_MAX_NODES_PER_BLOCK}",
            tree.count_nodes()
        );
        assert!(
            tree.depth() <= SCENE_MAX_DEPTH,
            "{kind} at cap is {} deep, over SCN-2's {SCENE_MAX_DEPTH}",
            tree.depth()
        );
        assert!(
            tree.text_bytes() <= SCENE_MAX_TEXT_BYTES_PER_BLOCK,
            "{kind} at cap holds {} text bytes, over SCN-3's {SCENE_MAX_TEXT_BYTES_PER_BLOCK}",
            tree.text_bytes()
        );
    }
}

#[test]
fn every_worst_case_component_admits_into_one_scene() {
    let mut scene = Scene::new();
    for (index, component) in at_cap_components().iter().enumerate() {
        let block = bitty_rich::RichBlock::new(
            scene.alloc_id(),
            BlockAnchor::Zone(u64::try_from(index).expect("small index")),
            component.build(),
            ScrollBehavior::Overlay,
            1,
            1,
            0,
        )
        .expect("a worst-case component admits under SCN-1..3");
        scene
            .insert(block)
            .expect("a worst-case component admits under SCN-4/5");
    }
    assert_eq!(
        scene.len(),
        5,
        "all five worst-case compositions fit in one terminal's scene"
    );
}

// ---------------------------------------------------------------------------
// Source guard
// ---------------------------------------------------------------------------

/// Tokens the catalog's own source must never name.
///
/// Each one would defeat a property the tests above can only observe from the
/// outside: a hashed collection or a sort would make order derived rather than
/// carried, and an excluded `SceneNode` kind or a style token would let
/// presentation or authority ride in behind a passing shape test.
const FORBIDDEN_SOURCE_TOKENS: [&str; 13] = [
    "HashMap",
    "HashSet",
    "BTreeMap",
    "BTreeSet",
    ".sort(",
    ".sort_by(",
    ".sort_unstable",
    ".dedup(",
    "SceneNode::Styled",
    "SceneNode::Block",
    "SceneNode::Rule",
    "SceneNode::Image",
    "SceneNode::Unknown",
];

#[test]
fn the_catalog_module_names_no_hashed_order_no_sort_and_no_excluded_kind() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/catalog.rs");
    let source = fs::read_to_string(&path).expect("the catalog source is readable");
    assert!(
        source.contains("SceneNode::Row"),
        "the guard must be reading the real catalog source"
    );
    for token in FORBIDDEN_SOURCE_TOKENS {
        assert!(
            !source.contains(token),
            "src/catalog.rs names {token}, which the catalog contract excludes"
        );
    }
}
