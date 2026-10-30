//! One closed style vocabulary across the resolver and the scene model.
//!
//! `bitty-ui` owns the extension-facing resolver and `bitty-rich` owns the
//! admitted scene data; `bitty-ui` has no production edge to `bitty-rich`
//! (the scene crate is a dev-dependency there), so the caps and token names
//! are mirrored. This suite is the drift gate: it compiles only where both
//! crates are visible (`bitty-runtime` depends on both), and it fails the
//! moment a cap or a token spelling moves on one side.

use bitty_rich::scene as scene_model;
use bitty_ui as resolver;

#[test]
fn caps_are_identical_on_both_sides() {
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
}

#[test]
fn the_default_border_token_is_content_foreground_on_both_sides() {
    assert_eq!(
        scene_model::SceneNodeStyle::default().border_token,
        scene_model::ContentToken::Foreground
    );
    assert_eq!(
        scene_model::SceneNodeStyle::default().border_token.as_str(),
        "content.foreground"
    );
}
