//! Bounded layout and style primitives for rich scene nodes (bitty#2191).
//!
//! The [Scene Style Primitives (Candidate)](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/scene-style-primitives-candidate.md)
//! record closes the extension styling vocabulary at four paint primitives,
//! and the [Lua UI Composition Reconciliation](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/lua-ui-composition-reconciliation.md)
//! records that the `bitty.ui.mount` bridge shipped none of them. This module
//! is the bridge half of the gap: the `style` key the candidate names, parsed
//! as declarative data and bounded before any host call. The record's
//! residual-`gap` row ("no `gap`, size-constraint, or cross-axis alignment
//! field exists on either bridge") is the reason the two width bounds below
//! are the only additions: sibling spacing and cross-axis alignment stay
//! unimplemented rather than half-declared.
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
//! | Width floor    | `min_width`    | cells       | `0` (no floor)       | 1000          |
//! | Width cap      | `max_width`    | cells       | `0` (no cap)         | 1000          |
//!
//! Padding is measured in cells because scene layout is cell-based; radius and
//! border width are physical pixels because the scene paint path forwards them
//! unchanged into the accepted `RoundedFill` geometry, which is physical pixel
//! space. A logical-pixel unit with a DPI story is a separate future change.
//!
//! # Size constraints (bitty#2191)
//!
//! `min_width` and `max_width` bound the node's **content box** in cells — the
//! width inside `padding`, not the padded outer frame — so a declaration
//! composes with an inset instead of fighting it. `0` means "no constraint",
//! the same inert idiom `border = 0` uses, so an undeclared width is
//! indistinguishable from an absent one and an unstyled tree keeps its exact
//! layout.
//!
//! The cell width is bounded by [`UI_STYLE_MAX_WIDTH_CELLS`], and a declaration
//! whose floor exceeds its cap is refused rather than reordered. Both values
//! are data: the Core measure path
//! (`bitty_render::scene`, `SceneNodeStyle::constrain_width`) applies them, so a
//! plugin never runs layout. Nothing here measures, wraps, or reads a terminal:
//! the plugin declares an intent, Core owns the geometry.
//!
//! A width-only declaration is **not** flat: it sizes the node, so it must lower
//! to the additive wrapper to have any effect.
//!
//! Elevation, shadow, opacity, blur, transitions, transforms, z-order,
//! per-node fonts, and background images are **not** primitives here: each has
//! a separate owner, and none is smuggled in through this table.
//!
//! # Boundaries
//!
//! - **Closed vocabulary.** Only the six keys above are accepted. An unknown
//!   key (CSS-like spellings included) is rejected, never ignored; so is an
//!   oversized field or token name. A repeated key is rejected as well, but
//!   only a marshalled table can carry one: the Lua VM collapses duplicate
//!   keys in a table literal before the bridge sees the value, so that arm is
//!   a defence-in-depth guard for a hand-built value, not a path a plugin can
//!   reach with a literal (bitty#3176).
//! - **Theme-resolved colour.** A border names one of six `content.*` /
//!   `state.*` tokens. A colour literal (`#RRGGBB`, `rgb()`, a named colour),
//!   a gradient, an image, and a Core chrome token (`border.*`, `chrome.*`)
//!   are all outside the set and are rejected.
//! - **Bounded, fail closed.** Every geometry value has an inclusive cap;
//!   one past it is a typed rejection naming both values. There is no clamp
//!   and no best-effort default.
//! - **One stroke per node.** `style.border` (physical px) and a `Block`'s
//!   own `border` (cells) are two spellings for one stroke, exactly like the
//!   `stroke` key the candidate record refuses: two non-zero widths on one
//!   node would paint a rounded ring and a four-op cell frame over the same
//!   box. A `Block` that declares both is refused with a typed
//!   `E_UI_COMPONENT_INVALID` naming both widths; a zero width paints
//!   nothing, so it never collides (bitty#3176).
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
pub const UI_STYLE_FIELDS: [&str; 6] = [
    "padding",
    "radius",
    "border",
    "border_token",
    "min_width",
    "max_width",
];

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

/// Inclusive maximum content width in cells for `min_width`/`max_width`.
///
/// Reuses the accepted grid-dimension bound: `bitty_term_state::MAX_GRID_DIM`
/// (mirrored by `bitty_ui::view::MAX_FIXED_SIZE_DIM` and the
/// `panel_rules` dimension cap), so a width constraint can never name a grid
/// the terminal cannot represent. The bound is a cap on the declaration, not a
/// clamp: one past it is refused.
pub const UI_STYLE_MAX_WIDTH_CELLS: u16 = 1000;

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
    /// Minimum content width exceeds [`UI_STYLE_MAX_WIDTH_CELLS`].
    MinWidthOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// Maximum content width exceeds [`UI_STYLE_MAX_WIDTH_CELLS`].
    MaxWidthOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// A width floor above the width cap would describe an empty range.
    WidthRangeInverted {
        /// Declared floor, in cells.
        min: u16,
        /// Declared cap, in cells.
        max: u16,
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
            Self::MinWidthOverCap { found, cap } => {
                write!(f, "scene style min_width exceeds cap: {found} > {cap}")
            }
            Self::MaxWidthOverCap { found, cap } => {
                write!(f, "scene style max_width exceeds cap: {found} > {cap}")
            }
            Self::WidthRangeInverted { min, max } => {
                write!(
                    f,
                    "scene style width range is inverted: min_width {min} > max_width {max}"
                )
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
    /// Minimum content width in cells; `0` declares no floor.
    pub min_width_cells: u16,
    /// Maximum content width in cells; `0` declares no cap.
    pub max_width_cells: u16,
}

impl UiNodeStyle {
    /// The flat style: no padding, no radius, no border, no width constraint.
    /// Paints nothing and measures exactly like the unwrapped node.
    pub const FLAT: Self = Self {
        padding_cells: 0,
        radius_px: 0,
        border_px: 0,
        border_token: UiBorderToken::Foreground,
        min_width_cells: 0,
        max_width_cells: 0,
    };

    /// Builds a style declaration; bounds are checked by [`Self::validate`].
    ///
    /// The width range is left unconstrained (`0`/`0`) so the four-primitive
    /// shape this constructor existed for keeps its meaning; declare a range
    /// with [`Self::with_width_range`].
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
            min_width_cells: 0,
            max_width_cells: 0,
        }
    }

    /// Declares a content-box width range in cells (`0` = unbounded on that
    /// side); bounds are checked by [`Self::validate`].
    #[must_use]
    pub const fn with_width_range(mut self, min_width_cells: u16, max_width_cells: u16) -> Self {
        self.min_width_cells = min_width_cells;
        self.max_width_cells = max_width_cells;
        self
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
        if self.min_width_cells > UI_STYLE_MAX_WIDTH_CELLS {
            return Err(UiStyleError::MinWidthOverCap {
                found: self.min_width_cells,
                cap: UI_STYLE_MAX_WIDTH_CELLS,
            });
        }
        if self.max_width_cells > UI_STYLE_MAX_WIDTH_CELLS {
            return Err(UiStyleError::MaxWidthOverCap {
                found: self.max_width_cells,
                cap: UI_STYLE_MAX_WIDTH_CELLS,
            });
        }
        // `0` is "unbounded", so only two declared bounds can invert. A floor
        // above the cap describes an empty range; Core never reorders the two
        // to make it fit, because a silent swap would paint a width the plugin
        // did not ask for.
        if self.min_width_cells > 0
            && self.max_width_cells > 0
            && self.min_width_cells > self.max_width_cells
        {
            return Err(UiStyleError::WidthRangeInverted {
                min: self.min_width_cells,
                max: self.max_width_cells,
            });
        }
        Ok(())
    }

    /// True when no geometry is claimed; the border token is then inert and
    /// the declaration lowers to no wrapper node.
    ///
    /// A declared width range counts as geometry: it sizes the node, so it is
    /// never flat.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        self.padding_cells == 0
            && self.radius_px == 0
            && self.border_px == 0
            && self.min_width_cells == 0
            && self.max_width_cells == 0
    }

    /// Whether this style's pixel stroke collides with a node's own cell
    /// frame (bitty#3176).
    ///
    /// A `Block` paints a cell frame with `Block.border` (cells, cap
    /// [`crate::ui_rich::UI_RICH_MAX_BORDER_CELLS`]), and a `style` declares
    /// a pixel stroke with `border` (physical px, cap
    /// [`UI_STYLE_MAX_BORDER_PX`]). One node paints one stroke: both non-zero
    /// at once would draw a rounded ring *and* the four-op cell frame over the
    /// same box, which is the double painting the candidate record refuses
    /// when it rejects a separate `stroke` spelling for one width. A width of
    /// `0` paints nothing, so it never collides.
    #[must_use]
    pub const fn collides_with_cell_frame(self, block_border_cells: u16) -> bool {
        self.border_px > 0 && block_border_cells > 0
    }
}

// ---------------------------------------------------------------------------
// Wire parser
// ---------------------------------------------------------------------------

/// Parses one `style` table into a bounded [`UiNodeStyle`].
///
/// The table is the closed six-key set. Nothing else is accepted: an unknown
/// key, a repeated key, an oversized name, a non-integer geometry value, an
/// inverted width range, and a token outside the content set all fail with the
/// bridge's typed `E_UI_COMPONENT_INVALID` diagnostic. The input is never
/// echoed past [`UI_STYLE_MAX_NAME_BYTES`].
///
/// A repeated key is only reachable from a hand-built marshalled table: the
/// Lua VM collapses duplicate keys inside a table literal, so a plugin cannot
/// produce one and the arm exists to keep the closed set closed at the type
/// boundary (bitty#3176).
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
    let mut min_width_cells = 0u16;
    let mut max_width_cells = 0u16;
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
                "duplicate scene style field '{name}' (only a marshalled table can repeat a key)"
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
            "min_width" => {
                min_width_cells = style_count(field_value, "min_width", UI_STYLE_MAX_WIDTH_CELLS)?;
            }
            "max_width" => {
                max_width_cells = style_count(field_value, "max_width", UI_STYLE_MAX_WIDTH_CELLS)?;
            }
            _ => border_token = style_token(field_value)?,
        }
    }
    let style = UiNodeStyle::new(padding_cells, radius_px, border_px, border_token)
        .with_width_range(min_width_cells, max_width_cells);
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
///
/// The conversion is the check, not a fallible step after it (bitty#3176):
/// a value that does not fit `u16` and a value past the field's cap are the
/// same typed rejection naming the found value and the cap, so there is no
/// length-preserving fallback for a future guard bug to fall into. A silent
/// clamp here would be exactly what the caps exist to prevent.
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
    let Ok(bounded) = u16::try_from(*count) else {
        return Err(style_over_cap(field, *count, cap));
    };
    if bounded > cap {
        return Err(style_over_cap(field, i64::from(bounded), cap));
    }
    Ok(bounded)
}

/// The one over-cap refusal shape: field, found value, and inclusive cap.
fn style_over_cap(field: &str, found: i64, cap: u16) -> BridgeError {
    component_invalid(format!("scene style {field} exceeds cap: {found} > {cap}"))
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
        assert_eq!(UI_STYLE_MAX_WIDTH_CELLS, 1000);
        assert_eq!(
            UI_STYLE_FIELDS,
            [
                "padding",
                "radius",
                "border",
                "border_token",
                "min_width",
                "max_width"
            ]
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
        // The conversion is the check (bitty#3176 item 2): the whole `i64`
        // range above the cap is refused with the found value, so no input can
        // reach a clamp and none is clamped to the cap.
        for found in [i64::from(u16::MAX) + 1, 1_000_000_000, i64::MAX] {
            let error = style(vec![("radius", LuaValue::Integer(found))]).expect_err("rejected");
            assert_eq!(
                error.message,
                format!("scene style radius exceeds cap: {found} > 16"),
                "no clamp: the refused value is reported verbatim"
            );
        }
        // The far negative side is the invalid-value refusal, never a
        // conversion fallback.
        let error = style(vec![("radius", LuaValue::Integer(i64::MIN))]).expect_err("rejected");
        assert_eq!(
            error.message,
            "invalid scene style value for field 'radius'"
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
        // `width` is not a key: the size vocabulary is the two-sided
        // `min_width`/`max_width` pair, and a bare CSS-like `width` is refused
        // so a plugin cannot believe it declared a constraint that Core never
        // applies.
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
        // bitty#3176 item 5: a Lua table literal cannot repeat a key (the VM
        // collapses it before the bridge sees the value), so the message says
        // what the arm actually guards instead of implying a plugin can
        // declare the same field twice.
        assert_eq!(
            error.message,
            "duplicate scene style field 'padding' (only a marshalled table can repeat a key)"
        );
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
    fn the_width_bounds_parse_and_their_cap_is_inclusive() {
        let parsed = style(vec![
            ("min_width", LuaValue::Integer(4)),
            ("max_width", LuaValue::Integer(12)),
        ])
        .expect("a width range parses");
        assert_eq!(parsed.min_width_cells, 4);
        assert_eq!(parsed.max_width_cells, 12);
        assert!(
            !parsed.is_flat(),
            "a width range sizes the node, so it can never be flat"
        );

        // Both bounds are independently optional: `0` is "unbounded".
        let floor_only =
            style(vec![("min_width", LuaValue::Integer(9))]).expect("a floor alone parses");
        assert_eq!(floor_only.max_width_cells, 0);
        assert!(!floor_only.is_flat());
        let cap_only =
            style(vec![("max_width", LuaValue::Integer(9))]).expect("a cap alone parses");
        assert_eq!(cap_only.min_width_cells, 0);
        assert!(!cap_only.is_flat());

        // The grid-dimension cap is inclusive on both sides.
        let at_cap = style(vec![
            (
                "min_width",
                LuaValue::Integer(i64::from(UI_STYLE_MAX_WIDTH_CELLS)),
            ),
            (
                "max_width",
                LuaValue::Integer(i64::from(UI_STYLE_MAX_WIDTH_CELLS)),
            ),
        ])
        .expect("the cap itself is accepted");
        assert_eq!(at_cap.min_width_cells, UI_STYLE_MAX_WIDTH_CELLS);
        assert_eq!(at_cap.max_width_cells, UI_STYLE_MAX_WIDTH_CELLS);

        // Equal bounds are a legal fixed width, not an inverted range.
        let fixed = style(vec![
            ("min_width", LuaValue::Integer(6)),
            ("max_width", LuaValue::Integer(6)),
        ])
        .expect("min == max is a fixed width");
        assert_eq!((fixed.min_width_cells, fixed.max_width_cells), (6, 6));
    }

    #[test]
    fn one_past_the_width_cap_is_a_typed_rejection() {
        let over = i64::from(UI_STYLE_MAX_WIDTH_CELLS) + 1;
        for field in ["min_width", "max_width"] {
            let error = style(vec![(field, LuaValue::Integer(over))]).expect_err("rejected");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
            assert_eq!(error.class, "validation");
            assert_eq!(
                error.message,
                format!("scene style {field} exceeds cap: {over} > {UI_STYLE_MAX_WIDTH_CELLS}"),
                "{field}"
            );
            // No clamp: a value past `u16` reports the found value verbatim.
            let error = style(vec![(field, LuaValue::Integer(1_000_000))]).expect_err("rejected");
            assert_eq!(
                error.message,
                format!("scene style {field} exceeds cap: 1000000 > {UI_STYLE_MAX_WIDTH_CELLS}")
            );
        }
    }

    #[test]
    fn an_inverted_width_range_is_refused_instead_of_reordered() {
        let error = style(vec![
            ("min_width", LuaValue::Integer(9)),
            ("max_width", LuaValue::Integer(4)),
        ])
        .expect_err("an empty range is refused");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert_eq!(
            error.message,
            "scene style width range is inverted: min_width 9 > max_width 4"
        );

        // A zero (unbounded) side never inverts: `min_width = 9` alone means
        // "at least 9", not "9..0".
        assert!(style(vec![("min_width", LuaValue::Integer(9))]).is_ok());
        assert!(style(vec![("max_width", LuaValue::Integer(1))]).is_ok());
    }

    #[test]
    fn a_width_value_follows_the_same_integer_rule_as_the_other_fields() {
        for value in [
            LuaValue::Integer(-1),
            LuaValue::Number(4.0),
            LuaValue::Number(4.5),
            LuaValue::Bool(true),
            LuaValue::String("4".to_string()),
            LuaValue::table([("cells", LuaValue::Integer(4))]),
        ] {
            let error = style(vec![("max_width", value)]).expect_err("rejected");
            assert_eq!(
                error.message,
                "invalid scene style value for field 'max_width'"
            );
        }
    }

    #[test]
    fn a_pixel_stroke_collides_with_a_cell_frame_only_when_both_paint() {
        // bitty#3176 item 1: the rule is a pure function of the two widths, so
        // the parser applies one behaviour instead of inventing a precedence.
        let stroke = UiNodeStyle::new(0, 0, 2, UiBorderToken::Foreground);
        assert!(stroke.collides_with_cell_frame(1));
        assert!(stroke.collides_with_cell_frame(8));
        assert!(
            !stroke.collides_with_cell_frame(0),
            "a zero-width cell frame paints nothing"
        );
        let no_stroke = UiNodeStyle::new(4, 6, 0, UiBorderToken::Accent);
        assert!(!no_stroke.collides_with_cell_frame(8));
        assert!(!UiNodeStyle::FLAT.collides_with_cell_frame(8));
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
            UiNodeStyle::new(0, 0, 0, UiBorderToken::Foreground)
                .with_width_range(0, UI_STYLE_MAX_WIDTH_CELLS + 1)
                .validate(),
            Err(UiStyleError::MaxWidthOverCap {
                found: 1001,
                cap: 1000
            })
        );
        assert_eq!(
            UiNodeStyle::new(0, 0, 0, UiBorderToken::Foreground)
                .with_width_range(UI_STYLE_MAX_WIDTH_CELLS + 1, 0)
                .validate(),
            Err(UiStyleError::MinWidthOverCap {
                found: 1001,
                cap: 1000
            })
        );
        assert_eq!(
            UiNodeStyle::new(0, 0, 0, UiBorderToken::Foreground)
                .with_width_range(7, 3)
                .validate(),
            Err(UiStyleError::WidthRangeInverted { min: 7, max: 3 })
        );
        assert_eq!(
            UiStyleError::PaddingOverCap { found: 33, cap: 32 }.to_string(),
            "scene style padding exceeds cap: 33 > 32"
        );
    }
}
