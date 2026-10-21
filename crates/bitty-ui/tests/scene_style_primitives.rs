//! Scene style primitive tests (bitty#2112).
//!
//! Candidate-status honesty: these tests pin the draft candidate primitive
//! set, caps, precedence, wire round-trip, and no-authority boundary from the
//! scene-style-primitives candidate record. They assert nothing beyond the
//! candidate theme-token cascade and the accepted decoration ranges the caps
//! reuse.
//!
//! Headless, deterministic, and bounded: no window, GPU, PTY, filesystem, or
//! plugin VM.

#![forbid(unsafe_code)]

use bitty_ui::decoration::{MAX_CONTENT_INSET_PX, MAX_RADIUS_PX};
use bitty_ui::scene_style::{
    BorderToken, SCENE_STYLE_MAX_BORDER_PX, SCENE_STYLE_MAX_NAME_BYTES,
    SCENE_STYLE_MAX_PADDING_CELLS, SCENE_STYLE_MAX_RADIUS_PX, SceneStyle, SceneStyleError,
    StyleArg, StyleSource, StyleValue, parse, resolve,
};
use bitty_ui::theme::{TokenColor, TokenLayer, resolve as resolve_tokens};

fn tokens() -> bitty_ui::theme::ResolvedTokens {
    resolve_tokens(&[("content.accent", "#112233")], None, &[], false).expect("theme resolves")
}

fn styled_theme() -> SceneStyle {
    SceneStyle::none()
        .with_padding(4)
        .expect("within cap")
        .with_radius(8)
        .expect("within cap")
        .with_border(2)
        .expect("within cap")
        .with_border_token(BorderToken::Accent)
}

#[test]
fn primitives_resolve_from_theme_tokens() {
    let resolved = resolve(&styled_theme(), &SceneStyle::none(), &tokens(), false)
        .expect("theme style resolves");

    assert_eq!(resolved.padding_cells(), 4);
    assert_eq!(resolved.padding_source(), StyleSource::Theme);
    assert_eq!(resolved.radius_px(), 8);
    assert_eq!(resolved.radius_source(), StyleSource::Theme);

    let border = resolved.border().expect("border resolves");
    assert_eq!(border.width_px, 2);
    assert_eq!(border.width_source, StyleSource::Theme);
    assert_eq!(border.token, BorderToken::Accent);
    assert_eq!(border.token.as_str(), "content.accent");
    assert_eq!(border.color, TokenColor::parse("#112233").expect("hex"));
    assert_eq!(border.token_layer, TokenLayer::ThemePreset);
}

#[test]
fn undeclared_border_token_uses_the_content_foreground_default() {
    let tokens = resolve_tokens(&[], None, &[], false).expect("defaults resolve");
    let theme = SceneStyle::none().with_border(1).expect("within cap");
    let resolved = resolve(&theme, &SceneStyle::none(), &tokens, false).expect("resolves");
    let border = resolved.border().expect("border resolves");
    assert_eq!(border.token, BorderToken::Foreground);
    assert_eq!(
        border.color,
        bitty_ui::theme::framework_default("content.foreground").expect("default")
    );
    assert_eq!(border.token_layer, TokenLayer::FrameworkDefault);
}

#[test]
fn node_override_wins_per_field_and_keeps_theme_context() {
    let node = SceneStyle::none().with_padding(6).expect("within cap");
    let resolved = resolve(&styled_theme(), &node, &tokens(), false).expect("resolves");

    // The node claims padding; every other field still comes from the theme.
    assert_eq!(resolved.padding_cells(), 6);
    assert_eq!(resolved.padding_source(), StyleSource::NodeOverride);
    assert_eq!(resolved.radius_px(), 8);
    assert_eq!(resolved.radius_source(), StyleSource::Theme);
    let border = resolved.border().expect("theme border survives");
    assert_eq!(border.width_px, 2);
    assert_eq!(border.width_source, StyleSource::Theme);
    assert_eq!(border.token, BorderToken::Accent);

    // An explicit node width of 0 disables the theme border; it is a claimed
    // value, not "unset". Padding and radius still come from the theme.
    let no_border = SceneStyle::none().with_border(0).expect("zero is valid");
    let resolved = resolve(&styled_theme(), &no_border, &tokens(), false).expect("resolves");
    assert!(resolved.border().is_none());
    assert_eq!(resolved.padding_cells(), 4);
    assert_eq!(resolved.radius_px(), 8);
}

#[test]
fn node_border_token_overrides_theme_token() {
    let tokens = resolve_tokens(
        &[("content.accent", "#112233"), ("state.error", "#445566")],
        None,
        &[],
        false,
    )
    .expect("theme resolves");
    let theme = styled_theme();
    let node = SceneStyle::none().with_border_token(BorderToken::Error);
    let resolved = resolve(&theme, &node, &tokens, false).expect("resolves");

    let border = resolved.border().expect("border resolves");
    assert_eq!(border.token, BorderToken::Error);
    assert_eq!(border.color, TokenColor::parse("#445566").expect("hex"));
    assert_eq!(border.token_layer, TokenLayer::ThemePreset);
    assert_eq!(
        border.width_px, 2,
        "the node token does not claim the theme width"
    );
}

#[test]
fn node_border_width_overrides_theme_width_with_node_source() {
    let node = SceneStyle::none().with_border(5).expect("within cap");
    let resolved = resolve(&styled_theme(), &node, &tokens(), false).expect("resolves");

    let border = resolved.border().expect("border resolves");
    assert_eq!(border.width_px, 5);
    assert_eq!(border.width_source, StyleSource::NodeOverride);
    assert_eq!(
        border.token,
        BorderToken::Accent,
        "the theme token survives a width-only override"
    );
}

#[test]
fn node_radius_overrides_theme_radius() {
    let node = SceneStyle::none().with_radius(3).expect("within cap");
    let resolved = resolve(&styled_theme(), &node, &tokens(), false).expect("resolves");

    assert_eq!(resolved.radius_px(), 3);
    assert_eq!(resolved.radius_source(), StyleSource::NodeOverride);
    assert_eq!(
        resolved.padding_cells(),
        4,
        "unclaimed fields still come from the theme"
    );
}

#[test]
fn safe_policy_ignores_every_declaration() {
    let resolved = resolve(&styled_theme(), &styled_theme(), &tokens(), true)
        .expect("safe resolution cannot fail on a valid declaration");
    assert!(resolved.is_flat());
    assert_eq!(resolved.padding_source(), StyleSource::Safe);
    assert_eq!(resolved.radius_source(), StyleSource::Safe);
    assert!(resolved.border().is_none());
    assert!(resolved.paint_signature().is_empty());
}

#[test]
fn caps_are_inclusive_and_overflow_fails_closed() {
    // Boundary values are accepted.
    let at_cap = SceneStyle::none()
        .with_padding(SCENE_STYLE_MAX_PADDING_CELLS)
        .expect("padding cap inclusive")
        .with_radius(SCENE_STYLE_MAX_RADIUS_PX)
        .expect("radius cap inclusive")
        .with_border(SCENE_STYLE_MAX_BORDER_PX)
        .expect("border cap inclusive");
    assert_eq!(at_cap.padding_cells(), Some(32));
    assert_eq!(at_cap.radius_px(), Some(16));
    assert_eq!(at_cap.border_px(), Some(16));

    // One past the cap is a typed error naming both values; never a clamp.
    let error = SceneStyle::none()
        .with_padding(SCENE_STYLE_MAX_PADDING_CELLS + 1)
        .expect_err("padding overflow fails");
    assert!(matches!(
        error,
        SceneStyleError::PaddingOverCap { found: 33, cap: 32 }
    ));
    assert!(error.to_string().contains("33 > 32"));

    let error = SceneStyle::none()
        .with_radius(SCENE_STYLE_MAX_RADIUS_PX + 1)
        .expect_err("radius overflow fails");
    assert!(matches!(
        error,
        SceneStyleError::RadiusOverCap { found: 17, cap: 16 }
    ));

    let error = SceneStyle::none()
        .with_border(SCENE_STYLE_MAX_BORDER_PX + 1)
        .expect_err("border overflow fails");
    assert!(matches!(
        error,
        SceneStyleError::BorderOverCap { found: 17, cap: 16 }
    ));

    // The wire parser applies the same caps and rejects negatives.
    let error = parse(&[("padding", StyleArg::Int(33))]).expect_err("wire overflow fails");
    assert!(matches!(error, SceneStyleError::PaddingOverCap { .. }));
    let error = parse(&[("radius", StyleArg::Int(-1))]).expect_err("negative fails");
    assert!(matches!(error, SceneStyleError::InvalidValue { .. }));
}

#[test]
fn caps_track_the_accepted_decoration_ranges() {
    // Shared-constant drift check: the candidate caps reuse accepted ranges.
    assert_eq!(
        SCENE_STYLE_MAX_PADDING_CELLS, MAX_CONTENT_INSET_PX,
        "padding cap tracks decoration.content_inset"
    );
    assert_eq!(
        SCENE_STYLE_MAX_RADIUS_PX, MAX_RADIUS_PX,
        "radius cap tracks decoration.radius"
    );
    // A scene border is a stroke, not a View frame: the cap tracks the
    // accepted outline `border_width` range (0..=16), not
    // `decoration.border` (0..=8).
    const ACCEPTED_OUTLINE_BORDER_WIDTH_MAX: u16 = 16;
    assert_eq!(
        SCENE_STYLE_MAX_BORDER_PX, ACCEPTED_OUTLINE_BORDER_WIDTH_MAX,
        "border cap tracks the accepted outline width range"
    );
}

#[test]
fn oversized_names_fail_closed_without_echoing_the_input() {
    let long_key = "x".repeat(SCENE_STYLE_MAX_NAME_BYTES + 1);
    let error = parse(&[(long_key.as_str(), StyleArg::Int(1))]).expect_err("oversized key fails");
    assert!(matches!(
        error,
        SceneStyleError::OversizedField { len: 33, cap: 32 }
    ));
    assert!(
        !error.to_string().contains("xxxx"),
        "diagnostic reports lengths, not the unbounded input"
    );

    let long_token = "t".repeat(SCENE_STYLE_MAX_NAME_BYTES + 1);
    let error = parse(&[("border_token", StyleArg::Text(long_token.as_str()))])
        .expect_err("oversized token fails");
    assert!(matches!(
        error,
        SceneStyleError::OversizedToken { len: 33, cap: 32 }
    ));
    assert!(!error.to_string().contains("tttt"));
}

#[test]
fn extension_api_round_trips_through_the_closed_wire_form() {
    let style = parse(&[
        ("padding", StyleArg::Int(3)),
        ("radius", StyleArg::Int(6)),
        ("border", StyleArg::Int(1)),
        ("border_token", StyleArg::Text("state.error")),
    ])
    .expect("wire form parses");

    assert_eq!(style.padding_cells(), Some(3));
    assert_eq!(style.radius_px(), Some(6));
    assert_eq!(style.border_px(), Some(1));
    assert_eq!(style.border_token(), Some(BorderToken::Error));

    let pairs = style.to_pairs();
    assert_eq!(
        pairs,
        vec![
            ("padding", StyleValue::Int(3)),
            ("radius", StyleValue::Int(6)),
            ("border", StyleValue::Int(1)),
            ("border_token", StyleValue::Text("state.error")),
        ],
        "canonical declaration order"
    );

    let args: Vec<(&str, StyleArg<'_>)> = pairs
        .into_iter()
        .map(|(key, value)| {
            let arg = match value {
                StyleValue::Int(number) => StyleArg::Int(i64::from(number)),
                StyleValue::Text(text) => StyleArg::Text(text),
            };
            (key, arg)
        })
        .collect();
    assert_eq!(parse(&args), Ok(style), "round-trip preserves the style");
}

#[test]
fn mutation_and_style_authority_fields_fail_closed() {
    // A style declaration is presentation data. Keys that would carry input
    // authority, renderer handles, or CSS-like freedom are not part of the
    // closed set and are rejected, never ignored.
    for key in [
        "on_click",
        "on_scroll",
        "command",
        "capability",
        "z_index",
        "opacity",
        "blur",
        "transition",
        "transform",
        "box-shadow",
        "background_image",
        "color",
        "font",
        "pipeline",
        "shader",
    ] {
        let error = parse(&[(key, StyleArg::Int(1))]).expect_err("authority field rejected");
        match error {
            SceneStyleError::UnsupportedField { field } => assert_eq!(field, key),
            other => panic!("expected UnsupportedField for {key}, got {other}"),
        }
    }

    // Duplicate keys are ambiguous declarations, not last-write-wins.
    let error = parse(&[("padding", StyleArg::Int(1)), ("padding", StyleArg::Int(2))])
        .expect_err("duplicate rejected");
    assert!(matches!(
        error,
        SceneStyleError::DuplicateField {
            field: bitty_ui::scene_style::StyleField::Padding
        }
    ));

    // A colour literal is not a token and cannot bypass theme resolution.
    let error =
        parse(&[("border_token", StyleArg::Text("#FF00FF"))]).expect_err("literal colour rejected");
    assert!(matches!(error, SceneStyleError::UnknownToken { .. }));

    // Resolution is pure: the same declaration against the same theme gives
    // the same result, and the declaration itself is unchanged (Copy value).
    let before = styled_theme();
    let first = resolve(&before, &SceneStyle::none(), &tokens(), false).expect("resolves");
    let second = resolve(&before, &SceneStyle::none(), &tokens(), false).expect("resolves");
    assert_eq!(first, second);
    assert_eq!(before, styled_theme());
}

#[test]
fn unused_style_is_byte_identical_to_no_style() {
    let default_tokens = resolve_tokens(&[], None, &[], false).expect("defaults resolve");

    // No declaration at either layer: flat, zero paint ops, zero bytes.
    let unused = resolve(
        &SceneStyle::none(),
        &SceneStyle::none(),
        &default_tokens,
        false,
    )
    .expect("flat resolves");
    assert!(unused.is_flat());
    assert_eq!(unused.paint_signature(), Vec::<u8>::new());
    assert!(SceneStyle::none().to_pairs().is_empty());

    // A styled node has a frozen, deterministic encoding (golden evidence).
    let resolved =
        resolve(&styled_theme(), &SceneStyle::none(), &tokens(), false).expect("styled resolves");
    let golden = vec![
        0x01, 0x04, 0x00, // padding = 4 cells
        0x02, 0x08, 0x00, // radius = 8 px
        0x03, 0x02, 0x00, 0x11, 0x22, 0x33, 0xFF, // border = 2 px, #112233
    ];
    assert_eq!(resolved.paint_signature(), golden);

    // And the consumer contract is explicit: an empty signature means the
    // unstyled path emits nothing at all.
    assert!(unused.paint_ops().is_empty());
    assert_ne!(resolved.paint_ops(), unused.paint_ops());
}
