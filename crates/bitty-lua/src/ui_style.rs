//! Bounded layout and style primitives for rich scene nodes (bitty#2191).
//!
//! The [Scene Style Primitives (Candidate)](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/scene-style-primitives-candidate.md)
//! record closes the extension styling vocabulary at four primitives and the
//! [Lua UI Composition Reconciliation](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/lua-ui-composition-reconciliation.md)
//! records that the `bitty.ui.mount` bridge shipped none of them. This module
//! is the bridge half of the gap: the `style` key the candidate names, parsed
//! as declarative data and bounded before any host call.
//!
//! Both documents are **draft candidates**. Nothing here is accepted,
//! promoted, or a compatibility promise: the accepted
//! [Rich Presentation RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md)
//! still owns the v1 `SceneNode` schema and the accepted
//! [Plugin API v1 Lua Surface RFC](https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/sdk/plugin-api-v1-lua-surface-rfc.md)
//! still owns the closed Lua surface. The record selects no threshold that
//! moves a requirement between owners: every cap below reuses an accepted
//! range and widens nothing.
//!
//! # Primitive set
//!
//! | Primitive      | Wire key       | Unit        | Default              | Inclusive cap |
//! |----------------|----------------|-------------|----------------------|---------------|
//! | Padding        | `padding`      | cells       | `0`                  | 32            |
//! | Corner radius  | `radius`       | physical px | `0`                  | 16            |
//! | Border width   | `border`       | physical px | `0`                  | 16            |
//! | Border colour  | `border_token` | token       | `content.foreground` | closed set    |
//!
//! Padding is measured in cells because scene layout is cell-based; radius and
//! border width are physical pixels because the scene paint path forwards them
//! unchanged into the accepted `RoundedFill` geometry, which is physical pixel
//! space. A logical-pixel unit with a DPI story is a separate future change.
//!
//! Elevation, shadow, opacity, blur, transitions, transforms, z-order,
//! per-node fonts, and background images are **not** primitives here: each has
//! a separate owner, and none is smuggled in through this table.
//!
//! # Boundaries
//!
//! - **Closed vocabulary.** Only the four keys above are accepted. An unknown
//!   key (CSS-like spellings included) is rejected, never ignored; so is a
//!   duplicate key and an oversized field or token name.
//! - **Theme-resolved colour.** A border names one of six `content.*` /
//!   `state.*` tokens. A colour literal (`#RRGGBB`, `rgb()`, a named colour),
//!   a gradient, an image, and a Core chrome token (`border.*`, `chrome.*`)
//!   are all outside the set and are rejected.
//! - **Bounded, fail closed.** Every geometry value has an inclusive cap;
//!   one past it is a typed rejection naming both values. There is no clamp
//!   and no best-effort default.
//! - **Presentation only.** No input binding, command, capability, renderer
//!   handle, path, image, shader, or function is representable here. The
//!   parser is a pure cold-path function of its input: it enters no plugin VM,
//!   runs no callback, and touches no terminal state.
//! - **No-op when flat.** A resolved flat style (all geometry `0`) emits no
//!   paint operation and is not wrapped into a node, so an unstyled tree keeps
//!   its current rendering byte for byte.
//!
//! # Accounting
//!
//! A style value is not text: it charges no `SCN-3` bytes. `border_token`
//! resolves to an enum, so it retains no string at all under `SCN-4`. A
//! non-flat style is a wrapper node in the scene model, so it costs **one
//! node and one depth level** under `SCN-1`/`SCN-2` on top of the content it
//! styles — which is why a tree at the ceiling is refused, not silently
//! wrapped.

#![forbid(unsafe_code)]

use crate::host::{BridgeError, LuaValue};
use crate::ui::component_invalid;

// ---------------------------------------------------------------------------
// Caps
// ---------------------------------------------------------------------------

// `bitty-lua` has no edge to `bitty-rich` or `bitty-ui` (it is the leaf crate
// of the declarative chain), so the primitive set is mirrored here. The drift
// gate lives where all three crates are visible
// (`crates/bitty-runtime/tests/scene_style_vocabulary.rs`), so the wire
// vocabulary, the extension resolver, and the scene model cannot silently
// disagree.

/// The closed style key set, in canonical declaration order.
pub const UI_STYLE_FIELDS: [&str; 4] = ["padding", "radius", "border", "border_token"];

/// Inclusive maximum padding in cells per side.
///
/// Reuses the accepted `decoration.content_inset` upper bound (`0..=32`).
pub const UI_STYLE_MAX_PADDING_CELLS: u16 = 32;

/// Inclusive maximum corner radius in physical pixels.
///
/// Reuses the accepted `decoration.radius` upper bound (`0..=16`).
pub const UI_STYLE_MAX_RADIUS_PX: u16 = 16;

/// Inclusive maximum border width in physical pixels.
///
/// Reuses the accepted outline `border_width` upper bound (`0..=16`).
pub const UI_STYLE_MAX_BORDER_PX: u16 = 16;

/// Maximum bytes in a style field name or border token name.
///
/// A diagnostic never echoes an unbounded producer string: a longer name is
/// refused by length alone, without being quoted back.
pub const UI_STYLE_MAX_NAME_BYTES: usize = 32;

// ---------------------------------------------------------------------------
// Closed border-token set
// ---------------------------------------------------------------------------

/// A content theme token a rich node may name for its border colour.
///
/// The set is closed and holds only `content.*`/`state.*` tokens: a plugin
/// styles content it owns and never claims Core-owned chrome. There is no
/// colour-literal variant, so an arbitrary colour is not representable.
///
/// The scene model carries the same six names as
/// `bitty_rich::scene::ContentToken` and the resolver as
/// `bitty_ui::scene_style::BorderToken`; the drift gate pins the three
/// name-for-name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum UiBorderToken {
    /// `content.foreground` — the default border colour.
    #[default]
    Foreground,
    /// `content.accent`.
    Accent,
    /// `content.muted`.
    Muted,
    /// `state.error`.
    Error,
    /// `state.warning`.
    Warning,
    /// `state.success`.
    Success,
}

impl UiBorderToken {
    /// Every permitted token, in canonical declaration order.
    pub const ALL: [Self; 6] = [
        Self::Foreground,
        Self::Accent,
        Self::Muted,
        Self::Error,
        Self::Warning,
        Self::Success,
    ];

    /// Wire spelling (the theme token name).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "content.foreground",
            Self::Accent => "content.accent",
            Self::Muted => "content.muted",
            Self::Error => "state.error",
            Self::Warning => "state.warning",
            Self::Success => "state.success",
        }
    }

    /// Parses an exact, case-sensitive token name; `None` outside the set.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|token| token.as_str() == name)
    }
}

impl std::fmt::Display for UiBorderToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Declared style
// ---------------------------------------------------------------------------

/// Typed refusal of a geometry value above its inclusive cap.
///
/// Caps are checked by [`UiNodeStyle::validate`] and by the wire parser, and
/// again by the accepted scene admission boundary when the value is converted.
/// Values are never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiStyleError {
    /// Padding exceeds [`UI_STYLE_MAX_PADDING_CELLS`].
    PaddingOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// Radius exceeds [`UI_STYLE_MAX_RADIUS_PX`].
    RadiusOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// Border width exceeds [`UI_STYLE_MAX_BORDER_PX`].
    BorderOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
}

impl std::fmt::Display for UiStyleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PaddingOverCap { found, cap } => {
                write!(f, "scene style padding exceeds cap: {found} > {cap}")
            }
            Self::RadiusOverCap { found, cap } => {
                write!(f, "scene style radius exceeds cap: {found} > {cap}")
            }
            Self::BorderOverCap { found, cap } => {
                write!(f, "scene style border exceeds cap: {found} > {cap}")
            }
        }
    }
}

impl std::error::Error for UiStyleError {}

/// One validated style declaration for a rich node.
///
/// Construction is plain data (an out-of-bounds value stays representable so
/// that [`Self::validate`] and the scene admission boundary can refuse it);
/// [`parse_node_style`] is the only value that reaches the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UiNodeStyle {
    /// Padding around the child content, in cells per side.
    pub padding_cells: u16,
    /// Corner radius applied to the painted ring, in physical pixels.
    pub radius_px: u16,
    /// Border stroke width inside the frame, in physical pixels.
    pub border_px: u16,
    /// Border colour token from the closed content set.
    pub border_token: UiBorderToken,
}

impl UiNodeStyle {
    /// The flat style: no padding, no radius, no border. Paints nothing and
    /// measures exactly like the unwrapped node.
    pub const FLAT: Self = Self {
        padding_cells: 0,
        radius_px: 0,
        border_px: 0,
        border_token: UiBorderToken::Foreground,
    };

    /// Builds a style declaration; bounds are checked by [`Self::validate`].
    #[must_use]
    pub const fn new(
        padding_cells: u16,
        radius_px: u16,
        border_px: u16,
        border_token: UiBorderToken,
    ) -> Self {
        Self {
            padding_cells,
            radius_px,
            border_px,
            border_token,
        }
    }

    /// Checks every geometry field against its inclusive cap.
    ///
    /// # Errors
    ///
    /// A typed [`UiStyleError`] naming the field, the rejected value, and the
    /// cap.
    pub fn validate(self) -> Result<(), UiStyleError> {
        if self.padding_cells > UI_STYLE_MAX_PADDING_CELLS {
            return Err(UiStyleError::PaddingOverCap {
                found: self.padding_cells,
                cap: UI_STYLE_MAX_PADDING_CELLS,
            });
        }
        if self.radius_px > UI_STYLE_MAX_RADIUS_PX {
            return Err(UiStyleError::RadiusOverCap {
                found: self.radius_px,
                cap: UI_STYLE_MAX_RADIUS_PX,
            });
        }
        if self.border_px > UI_STYLE_MAX_BORDER_PX {
            return Err(UiStyleError::BorderOverCap {
                found: self.border_px,
                cap: UI_STYLE_MAX_BORDER_PX,
            });
        }
        Ok(())
    }

    /// True when no geometry is claimed; the border token is then inert and
    /// the declaration lowers to no wrapper node.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        self.padding_cells == 0 && self.radius_px == 0 && self.border_px == 0
    }
}

// ---------------------------------------------------------------------------
// Wire parser
// ---------------------------------------------------------------------------

/// Parses one `style` table into a bounded [`UiNodeStyle`].
///
/// The table is the closed four-key set. Nothing else is accepted: an unknown
/// key, a duplicate key, an oversized name, a non-integer geometry value, and
/// a token outside the content set all fail with the bridge's typed
/// `E_UI_COMPONENT_INVALID` diagnostic. The input is never echoed past
/// [`UI_STYLE_MAX_NAME_BYTES`].
///
/// # Errors
///
/// [`BridgeError`] with class `validation` and code
/// `E_UI_COMPONENT_INVALID`.
pub fn parse_node_style(value: &LuaValue) -> Result<UiNodeStyle, BridgeError> {
    let LuaValue::Table(pairs) = value else {
        return Err(component_invalid("scene style must be a table"));
    };
    let mut padding_cells = 0u16;
    let mut radius_px = 0u16;
    let mut border_px = 0u16;
    let mut border_token = UiBorderToken::Foreground;
    let mut seen = [false; UI_STYLE_FIELDS.len()];
    for (key, field_value) in pairs {
        let LuaValue::String(name) = key else {
            return Err(component_invalid("scene style keys must be strings"));
        };
        if name.len() > UI_STYLE_MAX_NAME_BYTES {
            return Err(component_invalid(format!(
                "scene style field name exceeds cap: {} > {UI_STYLE_MAX_NAME_BYTES} bytes",
                name.len()
            )));
        }
        let Some(index) = UI_STYLE_FIELDS
            .iter()
            .position(|field| *field == name.as_str())
        else {
            return Err(component_invalid(format!(
                "unsupported scene style field '{name}'"
            )));
        };
        if seen[index] {
            return Err(component_invalid(format!(
                "duplicate scene style field '{name}'"
            )));
        }
        seen[index] = true;
        match name.as_str() {
            "padding" => {
                padding_cells = style_count(field_value, "padding", UI_STYLE_MAX_PADDING_CELLS)?;
            }
            "radius" => {
                radius_px = style_count(field_value, "radius", UI_STYLE_MAX_RADIUS_PX)?;
            }
            "border" => {
                border_px = style_count(field_value, "border", UI_STYLE_MAX_BORDER_PX)?;
            }
            _ => border_token = style_token(field_value)?,
        }
    }
    let style = UiNodeStyle::new(padding_cells, radius_px, border_px, border_token);
    style
        .validate()
        .map_err(|error| component_invalid(error.to_string()))?;
    Ok(style)
}

/// Reads one bounded non-negative integer style value.
///
/// A float (`2.0` included), a boolean, a table, a string, and a negative
/// integer are all the same typed refusal: the wire form is an integer, and
/// reinterpreting a float would be a silent coercion.
fn style_count(value: &LuaValue, field: &str, cap: u16) -> Result<u16, BridgeError> {
    let LuaValue::Integer(count) = value else {
        return Err(component_invalid(format!(
            "invalid scene style value for field '{field}'"
        )));
    };
    if *count < 0 {
        return Err(component_invalid(format!(
            "invalid scene style value for field '{field}'"
        )));
    }
    if *count > i64::from(cap) {
        return Err(component_invalid(format!(
            "scene style {field} exceeds cap: {count} > {cap}"
        )));
    }
    Ok(u16::try_from(*count).unwrap_or(cap))
}

/// Reads the border colour token, bounded and closed.
fn style_token(value: &LuaValue) -> Result<UiBorderToken, BridgeError> {
    let LuaValue::String(token) = value else {
        return Err(component_invalid(
            "invalid scene style value for field 'border_token'",
        ));
    };
    if token.len() > UI_STYLE_MAX_NAME_BYTES {
        return Err(component_invalid(format!(
            "scene border token exceeds cap: {} > {UI_STYLE_MAX_NAME_BYTES} bytes",
            token.len()
        )));
    }
    UiBorderToken::parse(token)
        .ok_or_else(|| component_invalid(format!("unknown scene border token '{token}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(pairs: Vec<(&str, LuaValue)>) -> Result<UiNodeStyle, BridgeError> {
        let table = LuaValue::Table(
            pairs
                .into_iter()
                .map(|(key, value)| (LuaValue::String(key.to_string()), value))
                .collect(),
        );
        parse_node_style(&table)
    }

    #[test]
    fn the_caps_are_the_candidate_numbers() {
        assert_eq!(UI_STYLE_MAX_PADDING_CELLS, 32);
        assert_eq!(UI_STYLE_MAX_RADIUS_PX, 16);
        assert_eq!(UI_STYLE_MAX_BORDER_PX, 16);
        assert_eq!(UI_STYLE_MAX_NAME_BYTES, 32);
        assert_eq!(
            UI_STYLE_FIELDS,
            ["padding", "radius", "border", "border_token"]
        );
    }

    #[test]
    fn an_empty_style_is_flat() {
        let parsed = style(Vec::new()).expect("empty style parses");
        assert_eq!(parsed, UiNodeStyle::FLAT);
        assert!(parsed.is_flat());
        assert_eq!(parsed.border_token, UiBorderToken::Foreground);
    }

    #[test]
    fn every_field_parses_and_the_caps_are_inclusive() {
        let parsed = style(vec![
            ("padding", LuaValue::Integer(32)),
            ("radius", LuaValue::Integer(16)),
            ("border", LuaValue::Integer(16)),
            ("border_token", LuaValue::String("state.error".to_string())),
        ])
        .expect("boundary values are accepted");
        assert_eq!(parsed.padding_cells, 32);
        assert_eq!(parsed.radius_px, 16);
        assert_eq!(parsed.border_px, 16);
        assert_eq!(parsed.border_token, UiBorderToken::Error);
        assert!(!parsed.is_flat());
    }

    #[test]
    fn one_past_every_cap_is_a_typed_rejection_naming_both_values() {
        let cases = [
            ("padding", 33, "scene style padding exceeds cap: 33 > 32"),
            ("radius", 17, "scene style radius exceeds cap: 17 > 16"),
            ("border", 17, "scene style border exceeds cap: 17 > 16"),
        ];
        for (field, value, expected) in cases {
            let error = style(vec![(field, LuaValue::Integer(value))]).expect_err("rejected");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
            assert_eq!(error.class, "validation");
            assert_eq!(error.message, expected, "{field}");
        }
        // A value past `u16` reports the same shape rather than saturating.
        let error = style(vec![("padding", LuaValue::Integer(100_000))]).expect_err("rejected");
        assert_eq!(
            error.message,
            "scene style padding exceeds cap: 100000 > 32"
        );
    }

    #[test]
    fn non_integer_and_negative_values_are_refused() {
        for value in [
            LuaValue::Integer(-1),
            LuaValue::Number(2.5),
            LuaValue::Number(2.0),
            LuaValue::Bool(true),
            LuaValue::table([("cells", LuaValue::Integer(1))]),
            LuaValue::String("2".to_string()),
        ] {
            let error = style(vec![("padding", value)]).expect_err("rejected");
            assert_eq!(
                error.message,
                "invalid scene style value for field 'padding'"
            );
        }
    }

    #[test]
    fn unknown_and_duplicate_keys_fail_closed() {
        let error = style(vec![("width", LuaValue::Integer(8))]).expect_err("rejected");
        assert_eq!(error.message, "unsupported scene style field 'width'");
        for name in ["margin", "gap", "stroke", "colour", "background_color"] {
            assert!(
                style(vec![(name, LuaValue::Integer(1))]).is_err(),
                "{name} is outside the closed set"
            );
        }
        let error = style(vec![
            ("padding", LuaValue::Integer(1)),
            ("padding", LuaValue::Integer(2)),
        ])
        .expect_err("rejected");
        assert_eq!(error.message, "duplicate scene style field 'padding'");
        // A non-string key is refused, not coerced.
        let error = parse_node_style(&LuaValue::Table(vec![(
            LuaValue::Integer(1),
            LuaValue::Integer(2),
        )]))
        .expect_err("rejected");
        assert_eq!(error.message, "scene style keys must be strings");
    }

    #[test]
    fn an_oversized_name_is_refused_without_echoing_it() {
        let long = "p".repeat(UI_STYLE_MAX_NAME_BYTES + 8);
        let error = style(vec![(long.as_str(), LuaValue::Integer(1))]).expect_err("rejected");
        assert_eq!(
            error.message,
            "scene style field name exceeds cap: 40 > 32 bytes"
        );
        assert!(!error.message.contains(&long));
    }

    #[test]
    fn the_token_set_is_closed() {
        assert_eq!(UiBorderToken::ALL.len(), 6);
        for token in UiBorderToken::ALL {
            let parsed = style(vec![(
                "border_token",
                LuaValue::String(token.as_str().to_string()),
            )])
            .expect("closed token accepted");
            assert_eq!(parsed.border_token, token);
            assert_eq!(UiBorderToken::parse(token.as_str()), Some(token));
        }
        // Colour literals, Core chrome tokens, and unknown names are refused.
        for rejected in [
            "#ff0000",
            "rgb(255,0,0)",
            "red",
            "border.idle",
            "chrome.accent",
            "content.background",
            "",
        ] {
            let error = style(vec![(
                "border_token",
                LuaValue::String(rejected.to_string()),
            )])
            .expect_err("rejected");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
            assert!(
                error.message.starts_with("unknown scene border token '"),
                "{rejected}: {}",
                error.message
            );
        }
        // A wrong type is not a token.
        let error = style(vec![("border_token", LuaValue::Integer(1))]).expect_err("rejected");
        assert_eq!(
            error.message,
            "invalid scene style value for field 'border_token'"
        );
        let long = LuaValue::String("c".repeat(UI_STYLE_MAX_NAME_BYTES + 1));
        let error = style(vec![("border_token", long)]).expect_err("rejected");
        assert_eq!(
            error.message,
            "scene border token exceeds cap: 33 > 32 bytes"
        );
    }

    #[test]
    fn a_style_that_is_not_a_table_is_refused() {
        let error = parse_node_style(&LuaValue::Integer(1)).expect_err("rejected");
        assert_eq!(error.message, "scene style must be a table");
    }

    #[test]
    fn a_token_without_geometry_is_inert_not_an_error() {
        let parsed = style(vec![(
            "border_token",
            LuaValue::String("state.success".to_string()),
        )])
        .expect("inert token is accepted");
        assert!(parsed.is_flat());
        assert_eq!(parsed.border_token, UiBorderToken::Success);
    }

    #[test]
    fn the_type_level_validator_agrees_with_the_parser() {
        assert_eq!(UiNodeStyle::FLAT.validate(), Ok(()));
        assert_eq!(
            UiNodeStyle::new(33, 0, 0, UiBorderToken::Foreground).validate(),
            Err(UiStyleError::PaddingOverCap { found: 33, cap: 32 })
        );
        assert_eq!(
            UiNodeStyle::new(0, 17, 0, UiBorderToken::Foreground).validate(),
            Err(UiStyleError::RadiusOverCap { found: 17, cap: 16 })
        );
        assert_eq!(
            UiNodeStyle::new(0, 0, 17, UiBorderToken::Foreground).validate(),
            Err(UiStyleError::BorderOverCap { found: 17, cap: 16 })
        );
        assert_eq!(
            UiStyleError::PaddingOverCap { found: 33, cap: 32 }.to_string(),
            "scene style padding exceeds cap: 33 > 32"
        );
    }
}
