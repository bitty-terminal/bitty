//! One closed style vocabulary across the wire parser, the resolver, and the
//! scene model.
//!
//! `bitty-lua` parses the Lua `style` table (bitty#2191), `bitty-ui` owns the
//! extension-facing resolver, and `bitty-rich` owns the admitted scene data.
//! None of the three has a production edge to another: `bitty-lua` is the leaf
//! of the declarative chain and `bitty-ui` has no edge to `bitty-rich` (the
//! scene crate is a dev-dependency there), so the caps and token names are
//! mirrored. This suite is the drift gate: it compiles only where all three
//! crates are visible (`bitty-runtime` depends on all of them), and it fails
//! the moment a cap or a token spelling moves on one side.

use bitty_lua as wire;
use bitty_rich::scene as scene_model;
use bitty_ui as resolver;

#[test]
fn caps_are_identical_on_every_side() {
    assert_eq!(
        scene_model::SCENE_STYLE_MAX_PADDING_CELLS,
        resolver::SCENE_STYLE_MAX_PADDING_CELLS
    );
    assert_eq!(
        scene_model::SCENE_STYLE_MAX_RADIUS_PX,
        resolver::SCENE_STYLE_MAX_RADIUS_PX
    );
    assert_eq!(
        scene_model::SCENE_STYLE_MAX_BORDER_PX,
        resolver::SCENE_STYLE_MAX_BORDER_PX
    );

    // The Lua wire parser mirrors the same three numbers, so a cap cannot move
    // on one side of the chain and leave the other refusing a legal value (or
    // admitting an illegal one).
    assert_eq!(
        wire::UI_STYLE_MAX_PADDING_CELLS,
        scene_model::SCENE_STYLE_MAX_PADDING_CELLS
    );
    assert_eq!(
        wire::UI_STYLE_MAX_RADIUS_PX,
        scene_model::SCENE_STYLE_MAX_RADIUS_PX
    );
    assert_eq!(
        wire::UI_STYLE_MAX_BORDER_PX,
        scene_model::SCENE_STYLE_MAX_BORDER_PX
    );
    assert_eq!(
        wire::UI_STYLE_MAX_NAME_BYTES,
        resolver::SCENE_STYLE_MAX_NAME_BYTES
    );
    // The width bound is a grid-dimension cap, and it mirrors the accepted
    // `bitty_term_state::MAX_GRID_DIM` on both sides of the chain. The
    // extension resolver keeps its four primitives (it has no measure path), so
    // it carries no width cap and no drift is possible there.
    assert_eq!(
        wire::UI_STYLE_MAX_WIDTH_CELLS,
        scene_model::SCENE_STYLE_MAX_WIDTH_CELLS
    );
    assert_eq!(wire::UI_STYLE_MAX_WIDTH_CELLS, 1000);

    // The sibling gap reuses the padding cap on both sides of the chain, so a
    // value the wire parser accepts can never be refused a layer later.
    assert_eq!(
        wire::UI_STYLE_MAX_GAP_CELLS,
        scene_model::SCENE_STYLE_MAX_GAP_CELLS
    );
    assert_eq!(wire::UI_STYLE_MAX_GAP_CELLS, 32);
}

#[test]
fn the_closed_token_set_is_name_for_name_identical() {
    assert_eq!(
        scene_model::ContentToken::ALL.len(),
        resolver::BorderToken::ALL.len()
    );
    for (model, resolver) in scene_model::ContentToken::ALL
        .into_iter()
        .zip(resolver::BorderToken::ALL)
    {
        assert_eq!(model.as_str(), resolver.as_str());
        assert_eq!(
            scene_model::ContentToken::parse(resolver.as_str()),
            Some(model),
            "a token the resolver accepts must be a scene token"
        );
        assert_eq!(
            resolver::BorderToken::parse(model.as_str()),
            Some(resolver),
            "a token the scene accepts must be a resolver token"
        );
    }

    // And the Lua wire set is the same six names, in the same order.
    assert_eq!(
        wire::UiBorderToken::ALL.len(),
        scene_model::ContentToken::ALL.len()
    );
    for (wire, model) in wire::UiBorderToken::ALL
        .into_iter()
        .zip(scene_model::ContentToken::ALL)
    {
        assert_eq!(wire.as_str(), model.as_str());
        assert_eq!(
            wire::UiBorderToken::parse(model.as_str()),
            Some(wire),
            "a token the scene accepts must be a wire token"
        );
        assert_eq!(
            scene_model::ContentToken::parse(wire.as_str()),
            Some(model),
            "a token the wire accepts must be a scene token"
        );
    }
}

#[test]
fn the_wire_key_set_is_the_candidate_set() {
    // The closed style key set: the four paint primitives from the candidate
    // record, the two content-width bounds, the sibling gap, and the cross-axis
    // child placement (bitty#2191). No more.
    assert_eq!(
        wire::UI_STYLE_FIELDS,
        [
            "padding",
            "radius",
            "border",
            "border_token",
            "min_width",
            "max_width",
            "gap",
            "align"
        ]
    );
}

#[test]
fn the_two_align_vocabularies_are_closed_and_do_not_mix() {
    // bitty#2191 / CTX-1853: the band root Row's `align` is a four-value
    // **distribution** of the whole group across the band width.
    assert_eq!(
        wire::UiAlign::ALL.map(wire::UiAlign::as_str),
        ["start", "center", "end", "space-between"]
    );
    assert_eq!(wire::UiAlign::default(), wire::UiAlign::Start);
    for align in wire::UiAlign::ALL {
        assert_eq!(wire::UiAlign::parse(align.as_str()), Some(align));
    }

    // The `style.align` key is a three-value **placement** of each child on the
    // axis its siblings do not advance, and it mirrors the scene model
    // name-for-name. `space-between` spreads slack between siblings, which is a
    // distribution and not a placement, so the style set refuses it.
    assert_eq!(
        wire::UiStyleAlign::ALL.map(wire::UiStyleAlign::as_str),
        ["start", "center", "end"]
    );
    assert_eq!(
        wire::UiStyleAlign::ALL.map(wire::UiStyleAlign::as_str),
        scene_model::SceneStyleAlign::ALL.map(scene_model::SceneStyleAlign::as_str)
    );
    for (style_align, model) in wire::UiStyleAlign::ALL
        .into_iter()
        .zip(scene_model::SceneStyleAlign::ALL)
    {
        assert_eq!(
            wire::UiStyleAlign::parse(style_align.as_str()),
            Some(style_align)
        );
        assert_eq!(
            scene_model::SceneStyleAlign::parse(style_align.as_str()),
            Some(model),
            "the scene model accepts every wire spelling"
        );
    }
    assert_eq!(wire::UiStyleAlign::parse("space-between"), None);
    assert_eq!(scene_model::SceneStyleAlign::parse("space-between"), None);

    // The two surfaces can never meet on one node: the `style` table is hosted
    // on `ui.rich`'s terminal slot, where no node is the band root, so neither
    // vocabulary needs a precedence rule against the other.
    assert_eq!(wire::UI_STYLE_FIELDS.len(), 8);
    assert!(
        wire::UI_STYLE_FIELDS.contains(&"align"),
        "the placement key is the style table's"
    );

    // The anchor vocabulary stays distinct: the only overlap is the spelling
    // `center`, which means "centred on the row" for an anchor, "centred group"
    // for a distribution, and "centred child" for a placement.
    for anchor in wire::ui::UiAnchor::ALL {
        if anchor == wire::ui::UiAnchor::Center {
            continue;
        }
        assert_eq!(
            wire::UiAlign::parse(anchor.as_str()),
            None,
            "anchor '{}' is not an align value",
            anchor.as_str()
        );
        assert_eq!(
            wire::UiStyleAlign::parse(anchor.as_str()),
            None,
            "anchor '{}' is not a style align value",
            anchor.as_str()
        );
    }
    for align in wire::UiAlign::ALL {
        assert_eq!(
            wire::ui::UiAnchor::parse(align.as_str()),
            if align == wire::UiAlign::Center {
                Some(wire::ui::UiAnchor::Center)
            } else {
                None
            },
            "align '{}' is not an anchor value",
            align.as_str()
        );
    }
}

#[test]
fn the_default_border_token_is_content_foreground_on_every_side() {
    assert_eq!(
        scene_model::SceneNodeStyle::default().border_token,
        scene_model::ContentToken::Foreground
    );
    assert_eq!(
        scene_model::SceneNodeStyle::default().border_token.as_str(),
        "content.foreground"
    );
    assert_eq!(
        wire::UiNodeStyle::FLAT.border_token,
        wire::UiBorderToken::Foreground
    );
    assert_eq!(
        wire::UiNodeStyle::FLAT.border_token.as_str(),
        scene_model::SceneNodeStyle::default().border_token.as_str()
    );
}

#[test]
fn the_wire_flat_style_is_the_scene_flat_style() {
    let scene_flat = scene_model::SceneNodeStyle::default();
    assert!(scene_flat.is_flat());
    assert!(wire::UiNodeStyle::FLAT.is_flat());
    assert_eq!(
        wire::UiNodeStyle::FLAT.padding_cells,
        scene_flat.padding_cells
    );
    assert_eq!(wire::UiNodeStyle::FLAT.radius_px, scene_flat.radius_px);
    assert_eq!(wire::UiNodeStyle::FLAT.border_px, scene_flat.border_px);
    assert_eq!(
        wire::UiNodeStyle::FLAT.min_width_cells,
        scene_flat.min_width_cells
    );
    assert_eq!(
        wire::UiNodeStyle::FLAT.max_width_cells,
        scene_flat.max_width_cells
    );
    assert_eq!(wire::UiNodeStyle::FLAT.gap_cells, scene_flat.gap_cells);
    assert_eq!(
        wire::UiNodeStyle::FLAT.align.as_str(),
        scene_flat.align.as_str()
    );
    assert_eq!(scene_flat.min_width_cells, 0, "0 is the unbounded default");
    assert_eq!(scene_flat.max_width_cells, 0, "0 is the unbounded default");
    assert_eq!(scene_flat.gap_cells, 0, "0 is the no-spacing default");
    assert_eq!(
        scene_flat.align,
        scene_model::SceneStyleAlign::Start,
        "start is the offset-zero default"
    );
}
