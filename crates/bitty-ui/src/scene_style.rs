//! Scene style primitives for extension-owned scene nodes (bitty#2112).
//!
//! Candidate implementation of the
//! [Scene Style Primitives (Candidate)](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/scene-style-primitives-candidate.md)
//! record. Nothing here is normative, accepted, or verified: the accepted
//! [Rich Presentation RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md)
//! fixes the v1 `SceneNode` schema (including `Block`'s `Border`) and the
//! accepted [Workspace Compositor Specification](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/workspace-compositor.md)
//! owns Core decoration geometry. This module resolves a **candidate**,
//! closed primitive set for content an extension owns; the UI Runtime RFC
//! accepts or rejects every spelling below, never this module.
//!
//! # Primitive set
//!
//! | Primitive      | Wire key       | Unit  | Cap | Accepted analogue |
//! |----------------|----------------|-------|-----|-------------------|
//! | Padding        | `padding`      | cells | 32  | `content_inset` range |
//! | Corner radius  | `radius`       | px    | 16  | `decoration.radius` range |
//! | Border width   | `border`       | px    | 16  | `border_width` range |
//! | Border color   | `border_token` | token | —   | theme token contract |
//!
//! Elevation is deliberately absent: no accepted contract defines a shadow or
//! elevation model, so the candidate record keeps it an open point rather than
//! inventing a token and a compositing budget here.
//!
//! # Boundaries this module keeps
//!
//! - **Closed vocabulary.** There is no arbitrary CSS-like surface: unknown
//!   keys, color literals, shader/pipeline handles, transforms, opacity,
//!   blur, transitions, and z-order are rejected fail-closed.
//! - **Theme-resolved colour.** A border names one of a closed set of content
//!   theme tokens; the colour comes from the resolved theme cascade
//!   ([`crate::theme::resolve`]), never from a plugin-supplied hex value.
//!   Core chrome tokens (`border.*`, `chrome.*`) are not in the set.
//! - **Bounded, fail closed.** Every geometry value has a documented cap;
//!   overflow is a typed error, never a silent clamp or default.
//! - **Presentation only.** No input, terminal state, renderer handle, or
//!   plugin VM participates. [`SceneStyle::resolve`] is a pure function of
//!   its inputs, safe to run on the cold path.
//! - **No-op when unused.** [`ResolvedSceneStyle::paint_signature`] is empty
//!   for an unstyled node, so a consumer that only emits style paint ops for
//!   a non-empty signature produces byte-identical output to today.
//!
//! All types are bounded, deterministic, and English-only.

#![forbid(unsafe_code)]

use std::fmt;

use crate::theme::{ResolvedTokens, TokenColor, TokenLayer};

// ---------------------------------------------------------------------------
// Caps (accepted analogue ranges, candidate application)
// ---------------------------------------------------------------------------

/// Maximum padding in cells.
///
/// Reuses the accepted `decoration.content_inset` upper bound (`32`).
pub const SCENE_STYLE_MAX_PADDING_CELLS: u16 = 32;

/// Maximum corner radius in logical pixels.
///
/// Reuses the accepted `decoration.radius` upper bound (`16`).
pub const SCENE_STYLE_MAX_RADIUS_PX: u16 = 16;

/// Maximum border width in logical pixels.
///
/// Reuses the accepted outline `border_width` upper bound (`16`).
pub const SCENE_STYLE_MAX_BORDER_PX: u16 = 16;

/// Maximum bytes in a style field name or token name.
///
/// Reuses the bridge-side theme-token length cap (`32`): a diagnostic never
/// clones an unbounded input string.
pub const SCENE_STYLE_MAX_NAME_BYTES: usize = 32;

// ---------------------------------------------------------------------------
// Closed border-token set
// ---------------------------------------------------------------------------

/// A content theme token an extension node may name for its border colour.
///
/// The set is closed and contains only `content.*`/`state.*` tokens, never
/// Core chrome tokens: a plugin contributes appearance for content it owns and
/// cannot claim Core-owned chrome. Adding a token is a contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorderToken {
    /// `content.foreground` — the default border colour.
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

impl BorderToken {
    /// Every permitted token, in canonical declaration order.
    pub const ALL: [Self; 6] = [
        Self::Foreground,
        Self::Accent,
        Self::Muted,
        Self::Error,
        Self::Warning,
        Self::Success,
    ];

    /// Canonical wire spelling (the theme token name).
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

impl fmt::Display for BorderToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Fields and sources
// ---------------------------------------------------------------------------

/// One writable style field, for diagnostics and duplicate detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StyleField {
    /// `padding`
    Padding,
    /// `radius`
    Radius,
    /// `border`
    Border,
    /// `border_token`
    BorderToken,
}

impl StyleField {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Padding => "padding",
            Self::Radius => "radius",
            Self::Border => "border",
            Self::BorderToken => "border_token",
        }
    }
}

impl fmt::Display for StyleField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which layer supplied a resolved field. Higher rank wins.
///
/// This is the candidate per-field order for extension content: the resolved
/// theme content style is a baseline, the extension's own node declaration
/// overrides it for content the extension owns, and `bitty --safe` overrides
/// both. Core chrome tokens and user chrome rules stay outside this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StyleSource {
    /// Built-in framework default (flat: `0`, no border).
    FrameworkDefault,
    /// Resolved theme content style.
    Theme,
    /// The extension node's own declaration.
    NodeOverride,
    /// `bitty --safe` forced flat style.
    Safe,
}

impl StyleSource {
    /// Stable lowercase label for diagnostics and attribution traces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FrameworkDefault => "framework-default",
            Self::Theme => "theme",
            Self::NodeOverride => "node-override",
            Self::Safe => "safe",
        }
    }
}

impl fmt::Display for StyleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build, parse, or resolve a scene style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneStyleError {
    /// Padding exceeds [`SCENE_STYLE_MAX_PADDING_CELLS`].
    PaddingOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// Radius exceeds [`SCENE_STYLE_MAX_RADIUS_PX`].
    RadiusOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// Border width exceeds [`SCENE_STYLE_MAX_BORDER_PX`].
    BorderOverCap {
        /// Rejected value.
        found: u16,
        /// The cap that was exceeded.
        cap: u16,
    },
    /// A value has the wrong type or is negative.
    InvalidValue {
        /// The rejected field.
        field: StyleField,
    },
    /// A field was supplied twice.
    DuplicateField {
        /// The repeated field.
        field: StyleField,
    },
    /// A key outside the closed primitive set (CSS-like input included).
    UnsupportedField {
        /// The rejected key.
        field: String,
    },
    /// A field name longer than [`SCENE_STYLE_MAX_NAME_BYTES`].
    OversizedField {
        /// Length in bytes.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A border token outside the closed content-token set.
    UnknownToken {
        /// The rejected token.
        token: String,
    },
    /// A token name longer than [`SCENE_STYLE_MAX_NAME_BYTES`].
    OversizedToken {
        /// Length in bytes.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A named token is absent from the supplied resolved theme.
    UnresolvedToken {
        /// The token that did not resolve.
        token: &'static str,
    },
}

impl fmt::Display for SceneStyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            Self::InvalidValue { field } => {
                write!(f, "invalid scene style value for field '{field}'")
            }
            Self::DuplicateField { field } => {
                write!(f, "duplicate scene style field '{field}'")
            }
            Self::UnsupportedField { field } => {
                write!(f, "unsupported scene style field '{field}'")
            }
            Self::OversizedField { len, cap } => {
                write!(f, "scene style field name exceeds cap: {len} > {cap} bytes")
            }
            Self::UnknownToken { token } => {
                write!(f, "unknown scene border token '{token}'")
            }
            Self::OversizedToken { len, cap } => {
                write!(f, "scene border token exceeds cap: {len} > {cap} bytes")
            }
            Self::UnresolvedToken { token } => {
                write!(f, "scene border token did not resolve: '{token}'")
            }
        }
    }
}

impl std::error::Error for SceneStyleError {}

// ---------------------------------------------------------------------------
// Declared style (validated intent)
// ---------------------------------------------------------------------------

/// A validated style declaration for one extension node.
///
/// Every field is optional: `None` means "not declared at this layer", which
/// lets a node override one field while inheriting the rest from the theme
/// content style. Values are range-checked by the constructors, so a value
/// that exists here is always within its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SceneStyle {
    padding_cells: Option<u16>,
    radius_px: Option<u16>,
    border_px: Option<u16>,
    border_token: Option<BorderToken>,
}

impl SceneStyle {
    /// An empty declaration (no field claimed).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            padding_cells: None,
            radius_px: None,
            border_px: None,
            border_token: None,
        }
    }

    /// Declares padding in cells, subject to [`SCENE_STYLE_MAX_PADDING_CELLS`].
    pub fn with_padding(mut self, cells: u16) -> Result<Self, SceneStyleError> {
        if cells > SCENE_STYLE_MAX_PADDING_CELLS {
            return Err(SceneStyleError::PaddingOverCap {
                found: cells,
                cap: SCENE_STYLE_MAX_PADDING_CELLS,
            });
        }
        self.padding_cells = Some(cells);
        Ok(self)
    }

    /// Declares a corner radius in logical pixels, subject to
    /// [`SCENE_STYLE_MAX_RADIUS_PX`].
    pub fn with_radius(mut self, px: u16) -> Result<Self, SceneStyleError> {
        if px > SCENE_STYLE_MAX_RADIUS_PX {
            return Err(SceneStyleError::RadiusOverCap {
                found: px,
                cap: SCENE_STYLE_MAX_RADIUS_PX,
            });
        }
        self.radius_px = Some(px);
        Ok(self)
    }

    /// Declares a border width in logical pixels, subject to
    /// [`SCENE_STYLE_MAX_BORDER_PX`].
    pub fn with_border(mut self, px: u16) -> Result<Self, SceneStyleError> {
        if px > SCENE_STYLE_MAX_BORDER_PX {
            return Err(SceneStyleError::BorderOverCap {
                found: px,
                cap: SCENE_STYLE_MAX_BORDER_PX,
            });
        }
        self.border_px = Some(px);
        Ok(self)
    }

    /// Declares the border colour token.
    #[must_use]
    pub const fn with_border_token(mut self, token: BorderToken) -> Self {
        self.border_token = Some(token);
        self
    }

    /// Declared padding in cells, if any.
    #[must_use]
    pub const fn padding_cells(self) -> Option<u16> {
        self.padding_cells
    }

    /// Declared radius in logical pixels, if any.
    #[must_use]
    pub const fn radius_px(self) -> Option<u16> {
        self.radius_px
    }

    /// Declared border width in logical pixels, if any.
    #[must_use]
    pub const fn border_px(self) -> Option<u16> {
        self.border_px
    }

    /// Declared border colour token, if any.
    #[must_use]
    pub const fn border_token(self) -> Option<BorderToken> {
        self.border_token
    }

    /// True when no field is claimed.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.padding_cells.is_none()
            && self.radius_px.is_none()
            && self.border_px.is_none()
            && self.border_token.is_none()
    }
}

// ---------------------------------------------------------------------------
// Closed wire form
// ---------------------------------------------------------------------------

/// One typed argument in the closed style wire form.
///
/// Mirrors the two value shapes a host bridge can carry without exposing
/// arbitrary expressions: an integer and a token name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleArg<'a> {
    /// Integer argument (negative values are rejected by [`parse`]).
    Int(i64),
    /// Token-name argument.
    Text(&'a str),
}

/// One typed value in the canonical style wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleValue {
    /// Integer value within its field cap.
    Int(u16),
    /// Closed token name.
    Text(&'static str),
}

/// Parses a closed list of style fields.
///
/// Unknown keys, duplicate keys, wrong value types, negative integers,
/// over-cap integers, and tokens outside [`BorderToken::ALL`] all fail
/// closed with a typed error. The parser never clamps and never ignores a
/// key.
pub fn parse(pairs: &[(&str, StyleArg<'_>)]) -> Result<SceneStyle, SceneStyleError> {
    let mut style = SceneStyle::none();
    let mut seen = [false; 4];

    for (key, arg) in pairs {
        if key.len() > SCENE_STYLE_MAX_NAME_BYTES {
            return Err(SceneStyleError::OversizedField {
                len: key.len(),
                cap: SCENE_STYLE_MAX_NAME_BYTES,
            });
        }
        let field = match *key {
            "padding" => StyleField::Padding,
            "radius" => StyleField::Radius,
            "border" => StyleField::Border,
            "border_token" => StyleField::BorderToken,
            other => {
                return Err(SceneStyleError::UnsupportedField {
                    field: other.to_owned(),
                });
            }
        };
        let index = match field {
            StyleField::Padding => 0,
            StyleField::Radius => 1,
            StyleField::Border => 2,
            StyleField::BorderToken => 3,
        };
        if seen[index] {
            return Err(SceneStyleError::DuplicateField { field });
        }
        seen[index] = true;

        match (field, arg) {
            (StyleField::Padding, StyleArg::Int(value)) => {
                style = style.with_padding(int_to_u16(*value, field)?)?;
            }
            (StyleField::Radius, StyleArg::Int(value)) => {
                style = style.with_radius(int_to_u16(*value, field)?)?;
            }
            (StyleField::Border, StyleArg::Int(value)) => {
                style = style.with_border(int_to_u16(*value, field)?)?;
            }
            (StyleField::BorderToken, StyleArg::Text(name)) => {
                if name.len() > SCENE_STYLE_MAX_NAME_BYTES {
                    return Err(SceneStyleError::OversizedToken {
                        len: name.len(),
                        cap: SCENE_STYLE_MAX_NAME_BYTES,
                    });
                }
                let token =
                    BorderToken::parse(name).ok_or_else(|| SceneStyleError::UnknownToken {
                        token: (*name).to_owned(),
                    })?;
                style = style.with_border_token(token);
            }
            _ => return Err(SceneStyleError::InvalidValue { field }),
        }
    }

    Ok(style)
}

/// Converts a wire integer to `u16`, rejecting negatives and overflow.
fn int_to_u16(value: i64, field: StyleField) -> Result<u16, SceneStyleError> {
    u16::try_from(value).map_err(|_| SceneStyleError::InvalidValue { field })
}

impl SceneStyle {
    /// Canonical closed wire form, in declaration order.
    ///
    /// Round-trips through [`parse`]: `parse(&to_pairs(style)) == style` for
    /// every constructible value.
    #[must_use]
    pub fn to_pairs(self) -> Vec<(&'static str, StyleValue)> {
        let mut pairs = Vec::new();
        if let Some(cells) = self.padding_cells {
            pairs.push(("padding", StyleValue::Int(cells)));
        }
        if let Some(px) = self.radius_px {
            pairs.push(("radius", StyleValue::Int(px)));
        }
        if let Some(px) = self.border_px {
            pairs.push(("border", StyleValue::Int(px)));
        }
        if let Some(token) = self.border_token {
            pairs.push(("border_token", StyleValue::Text(token.as_str())));
        }
        pairs
    }
}

// ---------------------------------------------------------------------------
// Resolved style
// ---------------------------------------------------------------------------

/// One resolved border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedBorder {
    /// Effective width in logical pixels (non-zero).
    pub width_px: u16,
    /// The named content token.
    pub token: BorderToken,
    /// The theme-resolved colour.
    pub color: TokenColor,
    /// Which theme layer supplied the colour.
    pub token_layer: TokenLayer,
    /// Which layer supplied the width.
    pub width_source: StyleSource,
}

/// A resolved style: concrete values plus per-field attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedSceneStyle {
    padding_cells: u16,
    padding_source: StyleSource,
    radius_px: u16,
    radius_source: StyleSource,
    border: Option<ResolvedBorder>,
}

impl ResolvedSceneStyle {
    /// Effective padding in cells.
    #[must_use]
    pub const fn padding_cells(self) -> u16 {
        self.padding_cells
    }

    /// Which layer supplied the padding.
    #[must_use]
    pub const fn padding_source(self) -> StyleSource {
        self.padding_source
    }

    /// Effective corner radius in logical pixels.
    #[must_use]
    pub const fn radius_px(self) -> u16 {
        self.radius_px
    }

    /// Which layer supplied the radius.
    #[must_use]
    pub const fn radius_source(self) -> StyleSource {
        self.radius_source
    }

    /// Effective border, or `None` for a zero/undeclared width.
    #[must_use]
    pub const fn border(self) -> Option<ResolvedBorder> {
        self.border
    }

    /// True when nothing would be painted.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        self.padding_cells == 0 && self.radius_px == 0 && self.border.is_none()
    }

    /// Typed paint operations in canonical order (padding, radius, border).
    ///
    /// Empty for a flat style: an unused style contributes no operation and
    /// therefore no rendering change.
    #[must_use]
    pub fn paint_ops(self) -> Vec<StylePaintOp> {
        let mut ops = Vec::new();
        if self.padding_cells > 0 {
            ops.push(StylePaintOp::Padding {
                cells: self.padding_cells,
            });
        }
        if self.radius_px > 0 {
            ops.push(StylePaintOp::RadiusClip { px: self.radius_px });
        }
        if let Some(border) = self.border {
            ops.push(StylePaintOp::Border {
                width_px: border.width_px,
                color: border.color,
            });
        }
        ops
    }

    /// Canonical byte encoding of [`Self::paint_ops`].
    ///
    /// This is deterministic evidence encoding, not a render protocol:
    /// `padding` = `0x01` + `u16` LE, `radius` = `0x02` + `u16` LE, `border`
    /// = `0x03` + `u16` LE + RGBA. A flat style encodes to zero bytes.
    #[must_use]
    pub fn paint_signature(self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for op in self.paint_ops() {
            match op {
                StylePaintOp::Padding { cells } => {
                    bytes.push(0x01);
                    bytes.extend_from_slice(&cells.to_le_bytes());
                }
                StylePaintOp::RadiusClip { px } => {
                    bytes.push(0x02);
                    bytes.extend_from_slice(&px.to_le_bytes());
                }
                StylePaintOp::Border { width_px, color } => {
                    bytes.push(0x03);
                    bytes.extend_from_slice(&width_px.to_le_bytes());
                    bytes.extend_from_slice(&color.0);
                }
            }
        }
        bytes
    }
}

/// One typed paint operation derived from a resolved style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StylePaintOp {
    /// Inset content by `cells` cells.
    Padding {
        /// Padding in cells.
        cells: u16,
    },
    /// Clip the painted frame with a corner radius.
    RadiusClip {
        /// Radius in logical pixels.
        px: u16,
    },
    /// Stroke a border.
    Border {
        /// Width in logical pixels.
        width_px: u16,
        /// Theme-resolved colour.
        color: TokenColor,
    },
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolves a node declaration over a theme content style.
///
/// Per field the order is `node` > `theme` > framework default (`0`). A
/// border colour comes from `tokens` (the accepted theme-token resolution
/// cascade) and inherits its winning [`TokenLayer`] as attribution. When the
/// effective border width is `0`, no border is resolved and the colour token
/// is inert.
///
/// `safe` implements the `bitty --safe` posture for extension content: every
/// geometry primitive is forced flat (`0`, no border), regardless of either
/// declaration. Nothing is clamped; declarations are validated before they
/// reach this function.
///
/// Fails closed with [`SceneStyleError::UnresolvedToken`] when a named token
/// is absent from `tokens`; it never substitutes an unthemed colour.
pub fn resolve(
    theme: &SceneStyle,
    node: &SceneStyle,
    tokens: &ResolvedTokens,
    safe: bool,
) -> Result<ResolvedSceneStyle, SceneStyleError> {
    if safe {
        return Ok(ResolvedSceneStyle {
            padding_cells: 0,
            padding_source: StyleSource::Safe,
            radius_px: 0,
            radius_source: StyleSource::Safe,
            border: None,
        });
    }

    let (padding_cells, padding_source) = pick(theme.padding_cells, node.padding_cells, 0);
    let (radius_px, radius_source) = pick(theme.radius_px, node.radius_px, 0);
    let (border_px, width_source) = pick(theme.border_px, node.border_px, 0);

    let border = if border_px == 0 {
        None
    } else {
        let token = node
            .border_token
            .or(theme.border_token)
            .unwrap_or(BorderToken::Foreground);
        let resolved = tokens
            .get(token.as_str())
            .ok_or(SceneStyleError::UnresolvedToken {
                token: token.as_str(),
            })?;
        Some(ResolvedBorder {
            width_px: border_px,
            token,
            color: resolved.value,
            token_layer: resolved.layer,
            width_source,
        })
    };

    Ok(ResolvedSceneStyle {
        padding_cells,
        padding_source,
        radius_px,
        radius_source,
        border,
    })
}

/// Picks a field value and its winning source.
fn pick(theme: Option<u16>, node: Option<u16>, default: u16) -> (u16, StyleSource) {
    if let Some(value) = node {
        (value, StyleSource::NodeOverride)
    } else if let Some(value) = theme {
        (value, StyleSource::Theme)
    } else {
        (default, StyleSource::FrameworkDefault)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::resolve as resolve_tokens;

    #[test]
    fn default_resolution_is_flat_and_emits_no_bytes() {
        let tokens = resolve_tokens(&[], None, &[], false).expect("defaults resolve");
        let resolved = resolve(&SceneStyle::none(), &SceneStyle::none(), &tokens, false)
            .expect("flat style resolves");
        assert!(resolved.is_flat());
        assert_eq!(resolved.padding_source(), StyleSource::FrameworkDefault);
        assert_eq!(resolved.radius_source(), StyleSource::FrameworkDefault);
        assert!(resolved.border().is_none());
        assert!(resolved.paint_ops().is_empty());
        assert!(resolved.paint_signature().is_empty());
    }

    #[test]
    fn caps_reject_overflow_without_clamping() {
        assert!(matches!(
            SceneStyle::none().with_padding(SCENE_STYLE_MAX_PADDING_CELLS + 1),
            Err(SceneStyleError::PaddingOverCap { .. })
        ));
        assert!(matches!(
            SceneStyle::none().with_radius(SCENE_STYLE_MAX_RADIUS_PX + 1),
            Err(SceneStyleError::RadiusOverCap { .. })
        ));
        assert!(matches!(
            SceneStyle::none().with_border(SCENE_STYLE_MAX_BORDER_PX + 1),
            Err(SceneStyleError::BorderOverCap { .. })
        ));
    }
}
