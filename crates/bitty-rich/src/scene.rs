//! Scene model (OQ-015, headless, bounded, forbid unsafe).
//!
//! Implements the accepted scene limits from `rich-presentation-rfc.md`:
//!
//! | ID | Dimension | Accepted default |
//! |---|---|---|
//! | SCN-1 | Max nodes per `RichBlock` | 2048 |
//! | SCN-2 | Max tree depth per block | 32 |
//! | SCN-3 | Max text bytes per block | 256 KiB |
//! | SCN-4 | Max aggregated rich bytes per terminal | 2 MiB |
//! | SCN-5 | Max blocks per terminal | 64 |
//!
//! All collections are bounded and deterministic. No GPU, no I/O, no unsafe.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Limits (accepted, parameterized)
// ---------------------------------------------------------------------------

/// Max nodes per `RichBlock` (SCN-1).
pub const SCENE_MAX_NODES_PER_BLOCK: usize = 2048;

/// Max tree depth per block (SCN-2).
pub const SCENE_MAX_DEPTH: usize = 32;

/// Max text bytes per block (SCN-3).
pub const SCENE_MAX_TEXT_BYTES_PER_BLOCK: usize = 256 * 1024;

/// Max aggregated rich bytes per terminal (SCN-4).
pub const SCENE_MAX_RICH_BYTES_PER_TERMINAL: usize = 2 * 1024 * 1024;

/// Max blocks per terminal (SCN-5).
pub const SCENE_MAX_BLOCKS_PER_TERMINAL: usize = 64;

/// Current `RichBlock` version (RFC: version = 1, monotonic).
pub const RICH_BLOCK_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable block identifier (content-addressed or ULID in production; here u64 for headless determinism).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

impl BlockId {
    /// Numeric value for diagnostics.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Anchors and scroll
// ---------------------------------------------------------------------------

/// Stable binding of a `RichBlock` to terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BlockAnchor {
    /// Preferred: bound to a `SemanticZone` id.
    Zone(u64),
    /// Fallback: bound to a scrollback line id.
    Line(u64),
    /// Discouraged: grid coordinate range.
    Grid {
        /// Start row.
        start_row: u16,
        /// Start col.
        start_col: u16,
        /// End row.
        end_row: u16,
        /// End col.
        end_col: u16,
    },
}

/// Scroll semantics for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollBehavior {
    /// Scrolls with terminal content.
    Inline,
    /// Stays below its zone during scroll.
    PinnedBelow,
    /// Transient overlay; does not affect layout.
    Overlay,
}

// ---------------------------------------------------------------------------
// SceneNode (declarative, layout-only)
// ---------------------------------------------------------------------------

/// Styled span for `SceneNode::Text`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StyledSpan {
    /// Text content (bounded via block text budget).
    pub text: String,
    /// Bold flag.
    pub bold: bool,
    /// Italic flag.
    pub italic: bool,
}

/// Border description for `SceneNode::Block`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Border {
    /// Width in cells.
    pub width: u16,
    /// Color as hex string (bounded).
    pub color: String,
}

/// Code block model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodeBlockModel {
    /// Language hint (e.g. "rust").
    pub lang: Option<String>,
    /// Code content (bounded via text budget).
    pub content: String,
}

/// Table model (headless, bounded).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableModel {
    /// Rows of cells; each cell is plain text (bounded).
    pub rows: Vec<Vec<String>>,
}

/// List model (headless, bounded).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListModel {
    /// Items (plain text, bounded).
    pub items: Vec<String>,
    /// Ordered?
    pub ordered: bool,
}

// ---------------------------------------------------------------------------
// Style primitives (v2, additive)
// ---------------------------------------------------------------------------

/// Additive scene-schema revision that introduced [`SceneNode::Styled`].
///
/// A block whose content only uses v1 kinds reports [`RICH_BLOCK_VERSION`];
/// a block that carries a styled subtree reports this revision through
/// [`RichBlock::schema_version`]. The block's `version` field stays the
/// monotonic revision counter, so nothing about v1 blocks moves.
pub const SCENE_STYLE_SCHEMA_VERSION: u32 = 2;

// The style schema is an additive revision after v1; a smaller value would
// make `schema_version()` lie about a v1-only block.
const _: () = assert!(SCENE_STYLE_SCHEMA_VERSION > RICH_BLOCK_VERSION);

/// Inclusive maximum style padding in cells.
///
/// Reuses the accepted `decoration.content_inset` upper bound (`0..=32`) and
/// matches `bitty_ui::scene_style::SCENE_STYLE_MAX_PADDING_CELLS`; the value
/// is owned here so the scene model and the extension resolver cannot drift.
pub const SCENE_STYLE_MAX_PADDING_CELLS: u16 = 32;

/// Inclusive maximum style corner radius in physical pixels.
///
/// Reuses the accepted `decoration.radius` upper bound (`0..=16`) and matches
/// `bitty_ui::scene_style::SCENE_STYLE_MAX_RADIUS_PX`.
pub const SCENE_STYLE_MAX_RADIUS_PX: u16 = 16;

/// Inclusive maximum style border width in physical pixels.
///
/// Reuses the accepted outline `border_width` upper bound (`0..=16`) and
/// matches `bitty_ui::scene_style::SCENE_STYLE_MAX_BORDER_PX`.
pub const SCENE_STYLE_MAX_BORDER_PX: u16 = 16;

/// Closed content-token set a style colour may name.
///
/// Only `content.*`/`state.*` tokens are in the set: an extension styles
/// content it owns and never claims Core-owned chrome (`border.*`,
/// `chrome.*`). There is no colour-literal variant, so a producer cannot
/// smuggle an arbitrary colour through a style. Adding a token is a contract
/// change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ContentToken {
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

impl ContentToken {
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

impl std::fmt::Display for ContentToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed refusal of an out-of-bounds style value.
///
/// Caps are inclusive; one past the cap is rejected and never clamped. The
/// extension resolver (`bitty_ui::scene_style::SceneStyleError`) carries the
/// same three cap variants plus its wire-parsing variants, so an authoring
/// diagnostic and the admission refusal describe a cap the same way.
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
}

impl std::fmt::Display for SceneStyleError {
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

impl std::error::Error for SceneStyleError {}

/// Bounded, data-only style for one styled scene subtree (v2).
///
/// The values are declarations, not paint: Core owns measurement and
/// painting, so a producer never runs a layout algorithm. Every field has a
/// documented default, an inclusive cap, and a unit:
///
/// | Field           | Unit        | Default               | Cap  |
/// |-----------------|-------------|-----------------------|------|
/// | `padding_cells` | cells       | `0`                   | `32` |
/// | `radius_px`     | physical px | `0`                   | `16` |
/// | `border_px`     | physical px | `0`                   | `16` |
/// | `border_token`  | closed set  | `content.foreground`  | —    |
///
/// `radius_px` and `border_px` are **physical pixels**: the paint path passes
/// them unchanged into the accepted `RoundedFill` geometry, which is physical
/// pixel space, so the scene layer applies no DPI scale. A logical-pixel unit
/// with a DPI story is a separate future change; declaring one here would make
/// the documented unit disagree with the painted frame.
///
/// Construction is plain data: an out-of-bounds value is representable so a
/// test (or a hostile producer that bypasses a typed constructor) can prove
/// admission rejects it, and [`Self::validate`] is the single check every
/// entry point applies. A token is inert while `border_px == 0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SceneNodeStyle {
    /// Padding around the child content, in cells per side.
    pub padding_cells: u16,
    /// Corner radius applied to the painted ring, in physical pixels.
    pub radius_px: u16,
    /// Border stroke width inside the frame, in physical pixels.
    pub border_px: u16,
    /// Border colour token from the closed content set.
    pub border_token: ContentToken,
}

impl SceneNodeStyle {
    /// The flat style: no padding, no radius, no border. A flat style paints
    /// no operation and measures exactly like an unstyled node.
    pub const FLAT: Self = Self {
        padding_cells: 0,
        radius_px: 0,
        border_px: 0,
        border_token: ContentToken::Foreground,
    };

    /// Builds a style declaration; bounds are checked by [`Self::validate`].
    #[must_use]
    pub const fn new(
        padding_cells: u16,
        radius_px: u16,
        border_px: u16,
        border_token: ContentToken,
    ) -> Self {
        Self {
            padding_cells,
            radius_px,
            border_px,
            border_token,
        }
    }

    /// Checks every field against its inclusive cap.
    ///
    /// # Errors
    ///
    /// A typed [`SceneStyleError`] naming the field, the rejected value, and
    /// the cap. Values are never clamped.
    pub fn validate(self) -> Result<(), SceneStyleError> {
        if self.padding_cells > SCENE_STYLE_MAX_PADDING_CELLS {
            return Err(SceneStyleError::PaddingOverCap {
                found: self.padding_cells,
                cap: SCENE_STYLE_MAX_PADDING_CELLS,
            });
        }
        if self.radius_px > SCENE_STYLE_MAX_RADIUS_PX {
            return Err(SceneStyleError::RadiusOverCap {
                found: self.radius_px,
                cap: SCENE_STYLE_MAX_RADIUS_PX,
            });
        }
        if self.border_px > SCENE_STYLE_MAX_BORDER_PX {
            return Err(SceneStyleError::BorderOverCap {
                found: self.border_px,
                cap: SCENE_STYLE_MAX_BORDER_PX,
            });
        }
        Ok(())
    }

    /// True when no geometry is claimed; the border token is then inert.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        self.padding_cells == 0 && self.radius_px == 0 && self.border_px == 0
    }
}

/// Declarative layout and paint primitive (v1, with the additive v2
/// [`SceneNode::Styled`] kind).
///
/// Text, rows, columns, blocks, images, tables, lists, and rules. No shaders,
/// pipelines, native windows, or global-coordinate drawing. Unknown variants
/// received from a newer producer are rendered as bounded plain-text fallback
/// (`Unknown`) per compatibility requirement.
///
/// The enum is intentionally not `#[non_exhaustive]`: during the `0.0.x`
/// series a new variant is accepted as a breaking change for a downstream
/// exhaustive match. Adding `#[non_exhaustive]` is a separate compatibility
/// decision, not part of the additive [`SceneNode::Styled`] kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SceneNode {
    /// Text span.
    Text(StyledSpan),
    /// Horizontal row of children.
    Row(Vec<SceneNode>),
    /// Vertical column of children.
    Column(Vec<SceneNode>),
    /// Bordered block with a single child.
    Block {
        /// Optional border.
        border: Option<Border>,
        /// Child.
        child: Box<SceneNode>,
    },
    /// Styled subtree (additive v2 kind).
    ///
    /// Carries bounded style data only. Core measures the child and paints
    /// the padding inset, the rounded border ring, and the token colour; the
    /// producer expresses no layout and holds no render handle. A v1
    /// producer never constructs this variant, so v1 scenes are unchanged,
    /// and an `Unknown` child still falls back to bounded plain text.
    Styled {
        /// Bounded style declaration; validated at block admission.
        style: SceneNodeStyle,
        /// Child subtree.
        child: Box<SceneNode>,
    },
    /// Image placement reference.
    Image(crate::image::PlacementId),
    /// Code block.
    CodeBlock(CodeBlockModel),
    /// Table.
    Table(TableModel),
    /// List.
    List(ListModel),
    /// Horizontal rule.
    Rule,
    /// Forward-compatible unknown variant (bounded plain-text fallback).
    Unknown(String),
}

impl SceneNode {
    /// Counts nodes in subtree (including self).
    ///
    /// Structural elements count, not just enum nesting: every table row
    /// and cell and every list item is a separately allocated `Vec`/`String`
    /// slot, so a table of empty strings still presses against SCN-1.
    /// (SEC-18/R-021: without structural counting, a table with hundreds of
    /// thousands of empty rows measured 1 node and 0 text bytes and sailed
    /// through admission.) `CodeBlock` stays 1: its content is a single
    /// `String` already budgeted by [`Self::text_bytes`].
    #[must_use]
    pub fn count_nodes(&self) -> usize {
        match self {
            Self::Text(_) | Self::Image(_) | Self::Rule | Self::Unknown(_) => 1,
            Self::CodeBlock(_) => 1,
            Self::Table(model) => {
                1 + model.rows.len() + model.rows.iter().map(Vec::len).sum::<usize>()
            }
            Self::List(model) => 1 + model.items.len(),
            Self::Row(children) | Self::Column(children) => {
                1 + children.iter().map(Self::count_nodes).sum::<usize>()
            }
            Self::Block { child, .. } | Self::Styled { child, .. } => 1 + child.count_nodes(),
        }
    }

    /// Maximum depth of subtree (leaf = 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Text(_) | Self::Image(_) | Self::Rule | Self::Unknown(_) => 1,
            Self::CodeBlock(_) | Self::Table(_) | Self::List(_) => 1,
            Self::Row(children) | Self::Column(children) => {
                if children.is_empty() {
                    1
                } else {
                    1 + children.iter().map(Self::depth).max().unwrap_or(0)
                }
            }
            Self::Block { child, .. } | Self::Styled { child, .. } => 1 + child.depth(),
        }
    }

    /// Text bytes in subtree (sum of all string payloads).
    ///
    /// Includes the `Block` border color: it is a producer-supplied `String`
    /// with no other budget, so excluding it would let an unbounded payload
    /// bypass SCN-3 (SEC-18/R-021).
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text(span) => span.text.len(),
            Self::CodeBlock(model) => {
                model.content.len() + model.lang.as_ref().map_or(0, |s| s.len())
            }
            Self::Table(model) => model.rows.iter().flatten().map(|s| s.len()).sum(),
            Self::List(model) => model.items.iter().map(|s| s.len()).sum(),
            Self::Unknown(s) => s.len(),
            Self::Row(children) | Self::Column(children) => {
                children.iter().map(Self::text_bytes).sum()
            }
            Self::Block { border, child } => {
                child.text_bytes() + border.as_ref().map_or(0, |b| b.color.len())
            }
            // Style is not text: a closed token enum and integer fields carry
            // no string payload, so styling never consumes the SCN-3 budget.
            Self::Styled { child, .. } => child.text_bytes(),
            Self::Image(_) | Self::Rule => 0,
        }
    }

    /// Scene-schema revision this subtree requires.
    ///
    /// Returns [`RICH_BLOCK_VERSION`] for a v1-only subtree and
    /// [`SCENE_STYLE_SCHEMA_VERSION`] once any [`SceneNode::Styled`] kind is
    /// present. Consumers that only understand v1 can use this to degrade a
    /// newer producer's block instead of misreading it.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::Styled { child, .. } => SCENE_STYLE_SCHEMA_VERSION.max(child.schema_version()),
            Self::Row(children) | Self::Column(children) => children
                .iter()
                .map(Self::schema_version)
                .max()
                .unwrap_or(RICH_BLOCK_VERSION),
            Self::Block { child, .. } => child.schema_version(),
            Self::Text(_)
            | Self::Image(_)
            | Self::CodeBlock(_)
            | Self::Table(_)
            | Self::List(_)
            | Self::Rule
            | Self::Unknown(_) => RICH_BLOCK_VERSION,
        }
    }

    /// Validates every style declaration in the subtree.
    ///
    /// Called by block admission so an out-of-bounds style is rejected with a
    /// typed diagnostic instead of being clamped during layout.
    fn validate_style_bounds(&self) -> Result<(), SceneStyleError> {
        match self {
            Self::Styled { style, child } => {
                style.validate()?;
                child.validate_style_bounds()
            }
            Self::Row(children) | Self::Column(children) => {
                children.iter().try_for_each(Self::validate_style_bounds)
            }
            Self::Block { child, .. } => child.validate_style_bounds(),
            Self::Text(_)
            | Self::Image(_)
            | Self::CodeBlock(_)
            | Self::Table(_)
            | Self::List(_)
            | Self::Rule
            | Self::Unknown(_) => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// RichBlock
// ---------------------------------------------------------------------------

/// Versioned, plugin-owned rich region anchored to terminal state.
///
/// Fields are private so per-block AST limits (SCN-1..3) cannot be bypassed
/// by direct struct construction (CR-RICH-01). Build via [`RichBlock::new`];
/// admit via [`Scene::insert`] / [`Scene::replace_content`], which re-validate
/// SCN-1..3 at the trust boundary. Read via accessors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichBlock {
    id: BlockId,
    version: u32,
    anchor: BlockAnchor,
    content: SceneNode,
    scroll: ScrollBehavior,
    owner: u64,
    generation: u64,
    created_at: u64,
}

/// Shared per-block AST limit check (SCN-1..3) over block content.
///
/// Single choke point used by [`RichBlock::new`], [`Scene::insert`], and
/// [`Scene::replace_content`] so a block that reaches the scene through any
/// path is bounded the same way.
///
/// Style bounds are checked first: an out-of-cap style is a producer defect
/// that must fail the whole block (the parent), never be clamped later by the
/// paint path. The check runs at every entry point, so a value that bypassed
/// the typed constructor still cannot reach the renderer.
fn check_block_content_limits(content: &SceneNode) -> Result<(), SceneError> {
    content
        .validate_style_bounds()
        .map_err(|error| SceneError::StyleInvalid { error })?;
    let nodes = content.count_nodes();
    if nodes > SCENE_MAX_NODES_PER_BLOCK {
        return Err(SceneError::NodesTooMany {
            count: nodes,
            cap: SCENE_MAX_NODES_PER_BLOCK,
        });
    }
    let depth = content.depth();
    if depth > SCENE_MAX_DEPTH {
        return Err(SceneError::DepthTooDeep {
            depth,
            cap: SCENE_MAX_DEPTH,
        });
    }
    let bytes = content.text_bytes();
    if bytes > SCENE_MAX_TEXT_BYTES_PER_BLOCK {
        return Err(SceneError::TextTooLarge {
            bytes,
            cap: SCENE_MAX_TEXT_BYTES_PER_BLOCK,
        });
    }
    Ok(())
}

impl RichBlock {
    /// Creates a block with validation (SCN-1..3 plus style bounds).
    ///
    /// Returns `Err` with a typed diagnostic when any per-block limit or a
    /// style value is out of bounds; the last good scene is retained by the
    /// caller (RFC). A block that carries a [`SceneNode::Styled`] subtree
    /// still reports [`RICH_BLOCK_VERSION`] here; read
    /// [`Self::schema_version`] for the additive scene revision.
    pub fn new(
        id: BlockId,
        anchor: BlockAnchor,
        content: SceneNode,
        scroll: ScrollBehavior,
        owner: u64,
        generation: u64,
        created_at: u64,
    ) -> Result<Self, SceneError> {
        check_block_content_limits(&content)?;
        Ok(Self {
            id,
            version: RICH_BLOCK_VERSION,
            anchor,
            content,
            scroll,
            owner,
            generation,
            created_at,
        })
    }

    /// Test-only bypass constructor (CR-RICH-01 regression harness).
    ///
    /// Builds a block without SCN-1..3 validation so tests can prove the
    /// scene admission boundary ([`Scene::insert`]) rejects oversized ASTs
    /// even if such a value ever reaches it. Never available in non-test
    /// builds.
    #[cfg(test)]
    pub(crate) fn new_unchecked(
        id: BlockId,
        anchor: BlockAnchor,
        content: SceneNode,
        scroll: ScrollBehavior,
        owner: u64,
        generation: u64,
        created_at: u64,
    ) -> Self {
        Self {
            id,
            version: RICH_BLOCK_VERSION,
            anchor,
            content,
            scroll,
            owner,
            generation,
            created_at,
        }
    }

    /// Stable identifier distinct from its anchor.
    #[must_use]
    pub fn id(&self) -> BlockId {
        self.id
    }

    /// Monotonic version (1 for v1, incremented by `replace_content`).
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Anchor binding this block to terminal state.
    #[must_use]
    pub fn anchor(&self) -> &BlockAnchor {
        &self.anchor
    }

    /// Root of this block's subtree.
    #[must_use]
    pub fn content(&self) -> &SceneNode {
        &self.content
    }

    /// Scroll behavior.
    #[must_use]
    pub fn scroll(&self) -> ScrollBehavior {
        self.scroll
    }

    /// Owner plugin id (attributable).
    #[must_use]
    pub fn owner(&self) -> u64 {
        self.owner
    }

    /// Lifecycle generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Provenance zone id for diagnostics.
    #[must_use]
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    /// Node count for this block.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.content.count_nodes()
    }

    /// Depth for this block.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.content.depth()
    }

    /// Scene-schema revision this block requires.
    ///
    /// [`RICH_BLOCK_VERSION`] for a v1 block, [`SCENE_STYLE_SCHEMA_VERSION`]
    /// once the content carries a styled subtree. The block's [`Self::version`]
    /// stays the monotonic revision counter.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.content.schema_version()
    }

    /// Text bytes for this block.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        self.content.text_bytes()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed scene admission failure (SCN-1..5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneError {
    /// SCN-1: nodes per block exceeded.
    NodesTooMany {
        /// Provided.
        count: usize,
        /// Cap.
        cap: usize,
    },
    /// SCN-2: depth per block exceeded.
    DepthTooDeep {
        /// Provided.
        depth: usize,
        /// Cap.
        cap: usize,
    },
    /// SCN-3: text bytes per block exceeded.
    TextTooLarge {
        /// Provided.
        bytes: usize,
        /// Cap.
        cap: usize,
    },
    /// SCN-4: aggregated rich bytes per terminal exceeded.
    AggregatedTooLarge {
        /// Would-be total.
        total: usize,
        /// Cap.
        cap: usize,
    },
    /// SCN-5: blocks per terminal exceeded.
    BlocksTooMany {
        /// Provided.
        count: usize,
        /// Cap.
        cap: usize,
    },
    /// Duplicate `BlockId` (deterministic conflict, RFC).
    DuplicateBlockId(BlockId),
    /// Block not found for replacement.
    BlockNotFound(BlockId),
    /// A style declaration in the block content is out of bounds.
    StyleInvalid {
        /// The typed style refusal (never clamped).
        error: SceneStyleError,
    },
}

impl std::fmt::Display for SceneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodesTooMany { count, cap } => write!(f, "nodes too many: {count} > {cap}"),
            Self::DepthTooDeep { depth, cap } => write!(f, "depth too deep: {depth} > {cap}"),
            Self::TextTooLarge { bytes, cap } => write!(f, "text too large: {bytes} > {cap}"),
            Self::AggregatedTooLarge { total, cap } => {
                write!(f, "aggregated rich bytes too large: {total} > {cap}")
            }
            Self::BlocksTooMany { count, cap } => write!(f, "blocks too many: {count} > {cap}"),
            Self::DuplicateBlockId(id) => write!(f, "duplicate block id: {}", id.0),
            Self::BlockNotFound(id) => write!(f, "block not found: {}", id.0),
            Self::StyleInvalid { error } => write!(f, "invalid scene style: {error}"),
        }
    }
}

impl std::error::Error for SceneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StyleInvalid { error } => Some(error),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Scene (composed)
// ---------------------------------------------------------------------------

/// Composed tree of `SceneNode` values with damage for one frame (headless).
///
/// Bounded: node count, depth, text bytes, and total layout size are
/// validated at insertion (SCN-1..5). Incremental updates diff the previous
/// tree; here we model the store as a bounded map of `RichBlock`s keyed by
/// `BlockId`. Renderer consumes a snapshot of this store plus damage.
#[derive(Debug)]
pub struct Scene {
    blocks: VecDeque<RichBlock>,
    aggregated_bytes: usize,
    generation: u64,
    next_id: u64,
    identity: u64,
}

/// Process-local counter behind [`Scene::identity`].
///
/// The value never enters paint output or admission accounting: it exists
/// only so a per-leaf paint cache can tell two scenes apart when both sit at
/// the same generation (two fresh scenes that each admitted one block are
/// both generation `1`).
static NEXT_SCENE_IDENTITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocates the next scene identity (wrapping; collisions require 2^64
/// allocations and only cost a redundant repaint, never a wrong frame).
fn next_scene_identity() -> u64 {
    NEXT_SCENE_IDENTITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Clone for Scene {
    /// A clone is an independent scene: it keeps the blocks, byte
    /// accounting, generation, and next id, but takes a fresh
    /// [`Scene::identity`] so a cache keyed on identity cannot serve one
    /// scene's paint list for the other.
    fn clone(&self) -> Self {
        Self {
            blocks: self.blocks.clone(),
            aggregated_bytes: self.aggregated_bytes,
            generation: self.generation,
            next_id: self.next_id,
            identity: next_scene_identity(),
        }
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene {
    /// An empty scene.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: VecDeque::new(),
            aggregated_bytes: 0,
            generation: 0,
            next_id: 1,
            identity: next_scene_identity(),
        }
    }

    /// Stable, process-local identity of this scene instance.
    ///
    /// Two scenes can share a generation while holding different content
    /// (both admitted one block, for example). A paint cache that keys only
    /// on `(generation, geometry)` would serve one scene's list for the
    /// other; the identity token makes that impossible. It is never painted,
    /// serialized, or compared for ordering.
    #[must_use]
    pub const fn identity(&self) -> u64 {
        self.identity
    }

    /// Number of retained blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Aggregated text bytes across all blocks (SCN-4 budget).
    #[must_use]
    pub fn aggregated_bytes(&self) -> usize {
        self.aggregated_bytes
    }

    /// Current generation (increments on each mutation, for damage).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Capacity bound (SCN-5).
    #[must_use]
    pub const fn max_blocks(&self) -> usize {
        SCENE_MAX_BLOCKS_PER_TERMINAL
    }

    /// Allocates a fresh `BlockId` (deterministic).
    pub fn alloc_id(&mut self) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Inserts a new block, validating SCN-1..5.
    ///
    /// Re-validates per-block SCN-1..3 at admission (CR-RICH-01 defense in
    /// depth: construction-time checks alone are insufficient), then checks
    /// SCN-4 and SCN-5. On failure returns a typed diagnostic and retains
    /// the last good scene (no partial insertion).
    pub fn insert(&mut self, block: RichBlock) -> Result<(), SceneError> {
        check_block_content_limits(&block.content)?;
        if self.blocks.iter().any(|b| b.id == block.id) {
            return Err(SceneError::DuplicateBlockId(block.id));
        }
        if self.blocks.len() >= SCENE_MAX_BLOCKS_PER_TERMINAL {
            return Err(SceneError::BlocksTooMany {
                count: self.blocks.len() + 1,
                cap: SCENE_MAX_BLOCKS_PER_TERMINAL,
            });
        }
        let block_bytes = block.text_bytes();
        let new_total = self.aggregated_bytes.saturating_add(block_bytes);
        if new_total > SCENE_MAX_RICH_BYTES_PER_TERMINAL {
            return Err(SceneError::AggregatedTooLarge {
                total: new_total,
                cap: SCENE_MAX_RICH_BYTES_PER_TERMINAL,
            });
        }
        self.aggregated_bytes = new_total;
        self.blocks.push_back(block);
        self.generation = self.generation.wrapping_add(1).max(1);
        Ok(())
    }

    /// Replaces content of an existing block with an incremented version.
    ///
    /// Validates the new content against SCN-1..3 and re-checks SCN-4
    /// (aggregated) against the new total. The `BlockId` stays the same;
    /// version is incremented deterministically.
    pub fn replace_content(
        &mut self,
        id: BlockId,
        new_content: SceneNode,
    ) -> Result<(), SceneError> {
        let pos = self
            .blocks
            .iter()
            .position(|b| b.id == id)
            .ok_or(SceneError::BlockNotFound(id))?;

        // Validate new content per-block limits before mutating aggregated.
        check_block_content_limits(&new_content)?;
        let new_bytes = new_content.text_bytes();

        let old_bytes = self.blocks[pos].text_bytes();
        let new_total = self
            .aggregated_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if new_total > SCENE_MAX_RICH_BYTES_PER_TERMINAL {
            return Err(SceneError::AggregatedTooLarge {
                total: new_total,
                cap: SCENE_MAX_RICH_BYTES_PER_TERMINAL,
            });
        }

        let block = &mut self.blocks[pos];
        let old_version = block.version;
        block.content = new_content;
        block.version = old_version.wrapping_add(1).max(1);
        self.aggregated_bytes = new_total;
        self.generation = self.generation.wrapping_add(1).max(1);
        Ok(())
    }

    /// Removes the block with `id`; `true` when removed.
    pub fn remove(&mut self, id: BlockId) -> bool {
        let before = self.blocks.len();
        let mut removed_bytes = 0usize;
        self.blocks.retain(|b| {
            if b.id == id {
                removed_bytes = removed_bytes.saturating_add(b.text_bytes());
                false
            } else {
                true
            }
        });
        let removed = self.blocks.len() != before;
        if removed {
            self.aggregated_bytes = self.aggregated_bytes.saturating_sub(removed_bytes);
            self.generation = self.generation.wrapping_add(1).max(1);
        }
        removed
    }

    /// Looks up a block by id.
    #[must_use]
    pub fn get(&self, id: BlockId) -> Option<&RichBlock> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// Iterates blocks oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &RichBlock> {
        self.blocks.iter()
    }

    /// Clears all blocks.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.aggregated_bytes = 0;
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    /// Detaches blocks whose anchor `Zone` id is no longer live.
    ///
    /// Per RFC anchoring rule: scrollback pruning detaches blocks whose
    /// anchor line range was pruned. Here we model the Zone variant: given
    /// the set of live `ZoneId`s, drop detached blocks and emit a drain
    /// diagnostic count (returned). This is deterministic and bounded.
    pub fn detach_pruned_zones(&mut self, live_zones: &[u64]) -> usize {
        let before = self.blocks.len();
        let mut detached_bytes = 0usize;
        self.blocks.retain(|b| match &b.anchor {
            BlockAnchor::Zone(zid) => {
                if live_zones.contains(zid) {
                    true
                } else {
                    detached_bytes = detached_bytes.saturating_add(b.text_bytes());
                    false
                }
            }
            _ => true,
        });
        let detached = before - self.blocks.len();
        if detached > 0 {
            self.aggregated_bytes = self.aggregated_bytes.saturating_sub(detached_bytes);
            self.generation = self.generation.wrapping_add(1).max(1);
        }
        detached
    }

    /// Damage for the current generation (headless stub: one region per block).
    ///
    /// Incremental damage is bounded by block count (≤64). Full-scene repaint
    /// occurs only on resize or store eviction (not modeled here).
    #[must_use]
    pub fn damage_len(&self) -> usize {
        self.blocks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::PlacementId;

    fn tiny_text(s: &str) -> SceneNode {
        SceneNode::Text(StyledSpan {
            text: s.to_string(),
            bold: false,
            italic: false,
        })
    }

    fn many_nodes(n: usize) -> SceneNode {
        SceneNode::Row((0..n).map(|i| tiny_text(&format!("{i}"))).collect())
    }

    fn deep_nodes(depth: usize) -> SceneNode {
        let mut node = tiny_text("leaf");
        for _ in 0..depth - 1 {
            node = SceneNode::Column(vec![node]);
        }
        node
    }

    #[test]
    fn new_is_empty() {
        let scene = Scene::new();
        assert!(scene.is_empty());
        assert_eq!(scene.len(), 0);
        assert_eq!(scene.aggregated_bytes(), 0);
    }

    #[test]
    fn insert_and_lookup() {
        let mut scene = Scene::new();
        let id = BlockId(1);
        let block = RichBlock::new(
            id,
            BlockAnchor::Zone(42),
            tiny_text("hello"),
            ScrollBehavior::Inline,
            1,
            0,
            42,
        )
        .unwrap();
        scene.insert(block).unwrap();
        assert_eq!(scene.len(), 1);
        assert_eq!(scene.get(id).unwrap().text_bytes(), 5);
        assert_eq!(scene.aggregated_bytes(), 5);
    }

    #[test]
    fn clone_is_an_independent_scene_with_its_own_identity() {
        let mut scene = Scene::new();
        scene
            .insert(
                RichBlock::new(
                    BlockId(1),
                    BlockAnchor::Zone(1),
                    tiny_text("hello"),
                    ScrollBehavior::Inline,
                    1,
                    0,
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let clone = scene.clone();
        // The clone keeps the content and generation it copied...
        assert_eq!(clone.len(), scene.len());
        assert_eq!(clone.generation(), scene.generation());
        assert_eq!(clone.aggregated_bytes(), scene.aggregated_bytes());
        // ...but it is a distinct scene instance, so a paint cache keyed on
        // identity can never serve one scene's list for the other.
        assert_ne!(clone.identity(), scene.identity());
        // Two `Scene::new()` values are distinct even before any mutation.
        assert_ne!(Scene::new().identity(), Scene::new().identity());
    }

    #[test]
    fn scn1_nodes_per_block_enforced() {
        // Build a block with 2049 nodes (exceeds 2048)
        let nodes = SCENE_MAX_NODES_PER_BLOCK + 1;
        // Row with n children = 1 (Row) + n (each Text) = n+1 nodes
        let content = many_nodes(nodes);
        let count = content.count_nodes();
        assert!(count > SCENE_MAX_NODES_PER_BLOCK);
        let id = BlockId(1);
        let err = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            content,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::NodesTooMany { .. }));
    }

    #[test]
    fn scn2_depth_enforced() {
        let content = deep_nodes(SCENE_MAX_DEPTH + 1);
        assert!(content.depth() > SCENE_MAX_DEPTH);
        let id = BlockId(1);
        let err = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            content,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::DepthTooDeep { .. }));
    }

    #[test]
    fn scn3_text_bytes_per_block_enforced() {
        // Value pin: the SCN-3 budget is 256 KiB and must not drift while the
        // unwired projection helper that used to exercise it is retired
        // (CTX-1412 / bitty#2117).
        assert_eq!(
            SCENE_MAX_TEXT_BYTES_PER_BLOCK,
            256 * 1024,
            "SCN-3 per-block text budget must stay 256 KiB"
        );
        let large = "a".repeat(SCENE_MAX_TEXT_BYTES_PER_BLOCK + 1);
        let content = tiny_text(&large);
        assert!(content.text_bytes() > SCENE_MAX_TEXT_BYTES_PER_BLOCK);
        let id = BlockId(1);
        let err = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            content,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::TextTooLarge { .. }));
    }

    #[test]
    fn scn5_blocks_per_terminal_enforced() {
        let mut scene = Scene::new();
        for i in 0..SCENE_MAX_BLOCKS_PER_TERMINAL {
            let block = RichBlock::new(
                BlockId(i as u64 + 1),
                BlockAnchor::Zone(i as u64),
                tiny_text("x"),
                ScrollBehavior::Inline,
                1,
                0,
                i as u64,
            )
            .unwrap();
            scene.insert(block).unwrap();
        }
        assert_eq!(scene.len(), SCENE_MAX_BLOCKS_PER_TERMINAL);
        let extra = RichBlock::new(
            BlockId(9999),
            BlockAnchor::Zone(9999),
            tiny_text("y"),
            ScrollBehavior::Inline,
            1,
            0,
            9999,
        )
        .unwrap();
        let err = scene.insert(extra).unwrap_err();
        assert!(matches!(err, SceneError::BlocksTooMany { .. }));
    }

    #[test]
    fn scn4_aggregated_bytes_enforced() {
        let mut scene = Scene::new();
        // Each block with 256 KiB (max per block) — how many fit before 2 MiB?
        // 2 MiB / 256 KiB = 8 blocks.
        let per_block = SCENE_MAX_TEXT_BYTES_PER_BLOCK;
        let text = "a".repeat(per_block);
        for i in 0..8 {
            let block = RichBlock::new(
                BlockId(i as u64 + 1),
                BlockAnchor::Zone(i as u64),
                tiny_text(&text),
                ScrollBehavior::Inline,
                1,
                0,
                i as u64,
            )
            .unwrap();
            scene.insert(block).unwrap();
        }
        assert_eq!(scene.aggregated_bytes(), 8 * per_block);
        assert_eq!(scene.aggregated_bytes(), SCENE_MAX_RICH_BYTES_PER_TERMINAL);
        let extra = RichBlock::new(
            BlockId(99),
            BlockAnchor::Zone(99),
            tiny_text("x"),
            ScrollBehavior::Inline,
            1,
            0,
            99,
        )
        .unwrap();
        let err = scene.insert(extra).unwrap_err();
        assert!(matches!(err, SceneError::AggregatedTooLarge { .. }));
    }

    #[test]
    fn replace_content_increments_version_and_checks_aggregated() {
        let mut scene = Scene::new();
        let id = BlockId(1);
        let block = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            tiny_text("hi"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        let v1 = block.version();
        scene.insert(block).unwrap();
        // Replace with larger but within block cap
        scene
            .replace_content(id, tiny_text(&"b".repeat(100)))
            .unwrap();
        assert_eq!(scene.get(id).unwrap().version(), v1 + 1);
        assert_eq!(scene.get(id).unwrap().text_bytes(), 100);
    }

    #[test]
    fn replace_rejected_retains_last_good() {
        let mut scene = Scene::new();
        let id = BlockId(1);
        let block = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            tiny_text("ok"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        scene.insert(block).unwrap();
        let before = scene.get(id).unwrap().content().clone();
        // Try to replace with over-depth content
        let deep = deep_nodes(SCENE_MAX_DEPTH + 5);
        let err = scene.replace_content(id, deep).unwrap_err();
        assert!(matches!(err, SceneError::DepthTooDeep { .. }));
        // Still old content
        assert_eq!(scene.get(id).unwrap().content(), &before);
    }

    #[test]
    fn duplicate_block_id_rejected() {
        let mut scene = Scene::new();
        let id = BlockId(7);
        let b1 = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            tiny_text("a"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        let b2 = RichBlock::new(
            id,
            BlockAnchor::Zone(2),
            tiny_text("b"),
            ScrollBehavior::Inline,
            1,
            0,
            2,
        )
        .unwrap();
        scene.insert(b1).unwrap();
        let err = scene.insert(b2).unwrap_err();
        assert!(matches!(err, SceneError::DuplicateBlockId(_)));
    }

    #[test]
    fn detach_pruned_zones() {
        let mut scene = Scene::new();
        for i in 1..=3 {
            let block = RichBlock::new(
                BlockId(i),
                BlockAnchor::Zone(i),
                tiny_text("x"),
                ScrollBehavior::Inline,
                1,
                0,
                i,
            )
            .unwrap();
            scene.insert(block).unwrap();
        }
        // Also a Line-anchored block that should survive pruning
        let line_block = RichBlock::new(
            BlockId(99),
            BlockAnchor::Line(999),
            tiny_text("keep"),
            ScrollBehavior::Inline,
            1,
            0,
            0,
        )
        .unwrap();
        scene.insert(line_block).unwrap();
        assert_eq!(scene.len(), 4);
        let detached = scene.detach_pruned_zones(&[1, 3]);
        assert_eq!(detached, 1); // Zone 2 detached
        assert_eq!(scene.len(), 3);
        assert!(scene.get(BlockId(2)).is_none());
        assert!(scene.get(BlockId(99)).is_some());
    }

    #[test]
    fn unknown_variant_is_bounded_fallback() {
        let node = SceneNode::Unknown("fallback text".to_string());
        assert_eq!(node.count_nodes(), 1);
        assert_eq!(node.depth(), 1);
        assert_eq!(node.text_bytes(), 13);
        let block = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            node,
            ScrollBehavior::Overlay,
            1,
            0,
            1,
        )
        .unwrap();
        assert_eq!(block.text_bytes(), 13);
    }

    #[test]
    fn image_node_zero_text() {
        let node = SceneNode::Image(PlacementId(42));
        assert_eq!(node.text_bytes(), 0);
        assert_eq!(node.count_nodes(), 1);
    }

    #[test]
    fn block_anchor_variants_survive_pruning() {
        let mut scene = Scene::new();
        let id = BlockId(1);
        let block = RichBlock::new(
            id,
            BlockAnchor::Grid {
                start_row: 0,
                start_col: 0,
                end_row: 10,
                end_col: 10,
            },
            tiny_text("grid"),
            ScrollBehavior::Inline,
            1,
            0,
            0,
        )
        .unwrap();
        scene.insert(block).unwrap();
        // Grid anchor is not detached by zone pruning
        let detached = scene.detach_pruned_zones(&[]);
        assert_eq!(detached, 0);
        assert_eq!(scene.len(), 1);
    }

    #[test]
    fn deterministic_ids() {
        let mut s1 = Scene::new();
        let mut s2 = Scene::new();
        assert_eq!(s1.alloc_id(), s2.alloc_id());
        assert_eq!(s1.alloc_id(), s2.alloc_id());
    }

    #[test]
    fn remove_and_clear() {
        let mut scene = Scene::new();
        let id = BlockId(1);
        let block = RichBlock::new(
            id,
            BlockAnchor::Zone(1),
            tiny_text("x"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        scene.insert(block).unwrap();
        assert!(scene.remove(id));
        assert!(scene.is_empty());
        assert_eq!(scene.aggregated_bytes(), 0);
        scene
            .insert(
                RichBlock::new(
                    BlockId(2),
                    BlockAnchor::Zone(2),
                    tiny_text("y"),
                    ScrollBehavior::Inline,
                    1,
                    0,
                    2,
                )
                .unwrap(),
            )
            .unwrap();
        scene.clear();
        assert!(scene.is_empty());
    }

    #[test]
    fn accessors_expose_block_metadata() {
        let block = RichBlock::new(
            BlockId(7),
            BlockAnchor::Zone(9),
            tiny_text("hi"),
            ScrollBehavior::PinnedBelow,
            11,
            12,
            13,
        )
        .unwrap();
        assert_eq!(block.id(), BlockId(7));
        assert_eq!(block.version(), RICH_BLOCK_VERSION);
        assert_eq!(block.anchor(), &BlockAnchor::Zone(9));
        assert_eq!(block.content(), &tiny_text("hi"));
        assert_eq!(block.scroll(), ScrollBehavior::PinnedBelow);
        assert_eq!(block.owner(), 11);
        assert_eq!(block.generation(), 12);
        assert_eq!(block.created_at(), 13);
    }

    /// CR-RICH-01: admission must reject oversized ASTs even if a block
    /// value reaches `insert` without going through `RichBlock::new`.
    #[test]
    fn cr_rich_01_insert_rejects_unchecked_oversized_nodes() {
        let mut scene = Scene::new();
        let content = many_nodes(SCENE_MAX_NODES_PER_BLOCK + 1);
        assert!(content.count_nodes() > SCENE_MAX_NODES_PER_BLOCK);
        let block = RichBlock::new_unchecked(
            BlockId(1),
            BlockAnchor::Zone(1),
            content,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        );
        let err = scene.insert(block).unwrap_err();
        assert!(matches!(err, SceneError::NodesTooMany { .. }));
        assert!(scene.is_empty());
        assert_eq!(scene.aggregated_bytes(), 0);
    }

    /// CR-RICH-01: admission must reject over-deep ASTs.
    #[test]
    fn cr_rich_01_insert_rejects_unchecked_deep_ast() {
        let mut scene = Scene::new();
        let content = deep_nodes(SCENE_MAX_DEPTH + 1);
        assert!(content.depth() > SCENE_MAX_DEPTH);
        let block = RichBlock::new_unchecked(
            BlockId(1),
            BlockAnchor::Zone(1),
            content,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        );
        let err = scene.insert(block).unwrap_err();
        assert!(matches!(err, SceneError::DepthTooDeep { .. }));
        assert!(scene.is_empty());
    }

    /// CR-RICH-01: admission must reject over-byte ASTs.
    #[test]
    fn cr_rich_01_insert_rejects_unchecked_large_text() {
        let mut scene = Scene::new();
        let large = "a".repeat(SCENE_MAX_TEXT_BYTES_PER_BLOCK + 1);
        let block = RichBlock::new_unchecked(
            BlockId(1),
            BlockAnchor::Zone(1),
            tiny_text(&large),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        );
        let err = scene.insert(block).unwrap_err();
        assert!(matches!(err, SceneError::TextTooLarge { .. }));
        assert!(scene.is_empty());
    }

    /// CR-RICH-01: legitimate blocks are unaffected by admission checks.
    #[test]
    fn cr_rich_01_legit_block_still_admitted() {
        let mut scene = Scene::new();
        let block = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            tiny_text("hello"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        scene.insert(block).unwrap();
        assert_eq!(scene.len(), 1);
        assert_eq!(scene.get(BlockId(1)).unwrap().text_bytes(), 5);
    }

    /// CTX-1628 / bitty#2760: a rejected mount must leave the scene byte- and
    /// allocation-identical, for every `SCN` dimension, across repeated
    /// attempts. A retrying host therefore cannot grow the store by failing.
    #[test]
    fn repeated_over_budget_inserts_do_not_grow_scene_state() {
        fn observe(scene: &Scene) -> (usize, usize, usize, usize, u64) {
            (
                scene.len(),
                scene.aggregated_bytes(),
                scene.iter().map(RichBlock::node_count).sum(),
                scene.iter().map(RichBlock::text_bytes).sum(),
                scene.generation(),
            )
        }

        // SCN-1: node count.
        let mut scene = Scene::new();
        scene
            .insert(
                RichBlock::new(
                    BlockId(1),
                    BlockAnchor::Zone(1),
                    tiny_text("kept"),
                    ScrollBehavior::Inline,
                    1,
                    0,
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let baseline = observe(&scene);
        let too_many = RichBlock::new_unchecked(
            BlockId(2),
            BlockAnchor::Zone(2),
            many_nodes(SCENE_MAX_NODES_PER_BLOCK + 1),
            ScrollBehavior::Inline,
            1,
            0,
            2,
        );
        for _ in 0..64 {
            assert!(matches!(
                scene.insert(too_many.clone()),
                Err(SceneError::NodesTooMany { .. })
            ));
        }
        assert_eq!(observe(&scene), baseline, "SCN-1 refusal changes nothing");

        // SCN-2: depth.
        let too_deep = RichBlock::new_unchecked(
            BlockId(3),
            BlockAnchor::Zone(3),
            deep_nodes(SCENE_MAX_DEPTH + 1),
            ScrollBehavior::Inline,
            1,
            0,
            3,
        );
        for _ in 0..64 {
            assert!(matches!(
                scene.insert(too_deep.clone()),
                Err(SceneError::DepthTooDeep { .. })
            ));
        }
        assert_eq!(observe(&scene), baseline, "SCN-2 refusal changes nothing");

        // SCN-3: per-block text bytes.
        let too_large = RichBlock::new_unchecked(
            BlockId(4),
            BlockAnchor::Zone(4),
            tiny_text(&"a".repeat(SCENE_MAX_TEXT_BYTES_PER_BLOCK + 1)),
            ScrollBehavior::Inline,
            1,
            0,
            4,
        );
        for _ in 0..64 {
            assert!(matches!(
                scene.insert(too_large.clone()),
                Err(SceneError::TextTooLarge { .. })
            ));
        }
        assert_eq!(observe(&scene), baseline, "SCN-3 refusal changes nothing");

        // SCN-5: block count, then SCN-4: aggregated bytes.
        let mut full = Scene::new();
        for index in 0..SCENE_MAX_BLOCKS_PER_TERMINAL {
            full.insert(
                RichBlock::new(
                    BlockId(index as u64 + 1),
                    BlockAnchor::Zone(index as u64),
                    tiny_text("x"),
                    ScrollBehavior::Inline,
                    1,
                    0,
                    index as u64,
                )
                .unwrap(),
            )
            .unwrap();
        }
        let full_baseline = observe(&full);
        let extra = RichBlock::new(
            BlockId(999),
            BlockAnchor::Zone(999),
            tiny_text("y"),
            ScrollBehavior::Inline,
            1,
            0,
            999,
        )
        .unwrap();
        for _ in 0..64 {
            assert!(matches!(
                full.insert(extra.clone()),
                Err(SceneError::BlocksTooMany { .. })
            ));
        }
        assert_eq!(
            observe(&full),
            full_baseline,
            "SCN-5 refusal changes nothing"
        );

        let mut aggregate = Scene::new();
        let text = "a".repeat(SCENE_MAX_TEXT_BYTES_PER_BLOCK);
        for index in 0..8 {
            aggregate
                .insert(
                    RichBlock::new(
                        BlockId(index as u64 + 1),
                        BlockAnchor::Zone(index as u64),
                        tiny_text(&text),
                        ScrollBehavior::Inline,
                        1,
                        0,
                        index as u64,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let aggregate_baseline = observe(&aggregate);
        let extra_small = RichBlock::new(
            BlockId(99),
            BlockAnchor::Zone(99),
            tiny_text("x"),
            ScrollBehavior::Inline,
            1,
            0,
            99,
        )
        .unwrap();
        for _ in 0..64 {
            assert!(matches!(
                aggregate.insert(extra_small.clone()),
                Err(SceneError::AggregatedTooLarge { .. })
            ));
        }
        assert_eq!(
            observe(&aggregate),
            aggregate_baseline,
            "SCN-4 refusal changes nothing"
        );
    }

    /// SEC-18/R-021: a table of empty strings must press against SCN-1.
    /// Structural rows/cells count as nodes; string bytes alone (here: 0)
    /// would admit an arbitrarily large allocation.
    #[test]
    fn sec18_table_structure_counts_against_scn1() {
        let small = SceneNode::Table(TableModel {
            rows: vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string()],
            ],
        });
        // 1 (table) + 2 (rows) + 3 (cells).
        assert_eq!(small.count_nodes(), 6);
        assert_eq!(small.text_bytes(), 3);

        let rows = SCENE_MAX_NODES_PER_BLOCK + 1;
        let big = SceneNode::Table(TableModel {
            rows: (0..rows).map(|_| vec![String::new()]).collect(),
        });
        assert_eq!(big.text_bytes(), 0);
        assert!(big.count_nodes() > SCENE_MAX_NODES_PER_BLOCK);
        let err = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            big,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::NodesTooMany { .. }));
    }

    /// SEC-18/R-021: list items count as nodes against SCN-1.
    #[test]
    fn sec18_list_items_count_against_scn1() {
        let small = SceneNode::List(ListModel {
            items: vec!["a".to_string(), "b".to_string()],
            ordered: false,
        });
        assert_eq!(small.count_nodes(), 3);
        assert_eq!(small.text_bytes(), 2);

        let big = SceneNode::List(ListModel {
            items: vec![String::new(); SCENE_MAX_NODES_PER_BLOCK + 1],
            ordered: true,
        });
        assert_eq!(big.text_bytes(), 0);
        let err = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            big,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::NodesTooMany { .. }));
    }

    /// SEC-18/R-021: the border color string is producer-supplied and has
    /// no other budget, so it counts against SCN-3.
    #[test]
    fn sec18_border_color_counts_against_scn3() {
        let bordered = SceneNode::Block {
            border: Some(Border {
                width: 1,
                color: "#ff0000".to_string(),
            }),
            child: Box::new(tiny_text("hi")),
        };
        // 2 (child text) + 7 (color).
        assert_eq!(bordered.text_bytes(), 9);

        let oversized = SceneNode::Block {
            border: Some(Border {
                width: 1,
                color: "c".repeat(SCENE_MAX_TEXT_BYTES_PER_BLOCK + 1),
            }),
            child: Box::new(tiny_text("")),
        };
        let err = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            oversized,
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(err, SceneError::TextTooLarge { .. }));
    }

    /// SEC-18/R-021 pin: the constrained AST admits no script, executable,
    /// or resource-loading variant. This exhaustive match forces the test
    /// (and the audit) to change when a variant is added.
    #[test]
    fn sec18_constrained_variant_set_is_pinned() {
        fn kind(node: &SceneNode) -> &'static str {
            match node {
                SceneNode::Text(_) => "text",
                SceneNode::Row(_) => "row",
                SceneNode::Column(_) => "column",
                SceneNode::Block { .. } => "block",
                SceneNode::Styled { .. } => "styled",
                SceneNode::Image(_) => "image",
                SceneNode::CodeBlock(_) => "codeblock",
                SceneNode::Table(_) => "table",
                SceneNode::List(_) => "list",
                SceneNode::Rule => "rule",
                SceneNode::Unknown(_) => "unknown",
            }
        }
        let span = StyledSpan {
            text: String::new(),
            bold: false,
            italic: false,
        };
        let cases: Vec<(SceneNode, &str)> = vec![
            (SceneNode::Text(span.clone()), "text"),
            (SceneNode::Row(vec![]), "row"),
            (SceneNode::Column(vec![]), "column"),
            (
                SceneNode::Block {
                    border: None,
                    child: Box::new(SceneNode::Rule),
                },
                "block",
            ),
            (
                SceneNode::Styled {
                    style: SceneNodeStyle::FLAT,
                    child: Box::new(SceneNode::Rule),
                },
                "styled",
            ),
            (SceneNode::Image(PlacementId(0)), "image"),
            (
                SceneNode::CodeBlock(CodeBlockModel {
                    lang: None,
                    content: String::new(),
                }),
                "codeblock",
            ),
            (SceneNode::Table(TableModel { rows: vec![] }), "table"),
            (
                SceneNode::List(ListModel {
                    items: vec![],
                    ordered: false,
                }),
                "list",
            ),
            (SceneNode::Rule, "rule"),
            (SceneNode::Unknown(String::new()), "unknown"),
        ];
        assert_eq!(cases.len(), 11);
        for (node, expected) in &cases {
            assert_eq!(kind(node), *expected);
            // Every variant is measurable and bounded; leaves sit at depth
            // 1, a block or styled wrapper around one child at depth 2 with
            // 2 nodes.
            let (expected_depth, expected_nodes) = if *expected == "block" || *expected == "styled"
            {
                (2, 2)
            } else {
                (1, 1)
            };
            assert_eq!(node.depth(), expected_depth);
            assert_eq!(node.count_nodes(), expected_nodes);
        }
    }

    // -- v2 style primitives (bitty#2112) -----------------------------------

    fn styled(style: SceneNodeStyle) -> SceneNode {
        SceneNode::Styled {
            style,
            child: Box::new(tiny_text("hi")),
        }
    }

    #[test]
    fn content_token_set_is_closed_and_stable() {
        assert_eq!(ContentToken::ALL.len(), 6);
        for token in ContentToken::ALL {
            assert_eq!(ContentToken::parse(token.as_str()), Some(token));
            assert!(!token.as_str().is_empty());
        }
        assert_eq!(
            ContentToken::parse("content.accent"),
            Some(ContentToken::Accent)
        );
        assert_eq!(ContentToken::parse("content.accent "), None);
        assert_eq!(ContentToken::parse("#ff0000"), None);
        assert_eq!(ContentToken::parse("border.idle"), None);
        assert_eq!(ContentToken::parse("chrome.accent"), None);
        assert_eq!(ContentToken::default(), ContentToken::Foreground);
    }

    #[test]
    fn style_caps_are_inclusive_and_one_past_fails() {
        assert_eq!(
            (
                SCENE_STYLE_MAX_PADDING_CELLS,
                SCENE_STYLE_MAX_RADIUS_PX,
                SCENE_STYLE_MAX_BORDER_PX
            ),
            (32, 16, 16),
            "the public caps are the candidate record's literal values"
        );
        let at_cap = SceneNodeStyle::new(32, 16, 16, ContentToken::Accent);
        assert_eq!(at_cap.validate(), Ok(()));

        for style in [
            SceneNodeStyle::new(33, 0, 0, ContentToken::Accent),
            SceneNodeStyle::new(0, 17, 0, ContentToken::Accent),
            SceneNodeStyle::new(0, 0, 17, ContentToken::Accent),
        ] {
            assert!(style.validate().is_err());
        }
    }

    /// Parent fails: `RichBlock::new` (the block that owns the subtree) must
    /// reject an out-of-cap style for every primitive instead of admitting it
    /// and letting the paint path clamp.
    #[test]
    fn parent_block_rejects_each_over_cap_style() {
        let cases = [
            SceneNodeStyle::new(
                SCENE_STYLE_MAX_PADDING_CELLS + 1,
                0,
                0,
                ContentToken::Accent,
            ),
            SceneNodeStyle::new(0, SCENE_STYLE_MAX_RADIUS_PX + 1, 0, ContentToken::Accent),
            SceneNodeStyle::new(0, 0, SCENE_STYLE_MAX_BORDER_PX + 1, ContentToken::Accent),
        ];
        for style in cases {
            let err = RichBlock::new(
                BlockId(1),
                BlockAnchor::Zone(1),
                styled(style),
                ScrollBehavior::Inline,
                1,
                0,
                1,
            )
            .unwrap_err();
            assert!(
                matches!(err, SceneError::StyleInvalid { .. }),
                "out-of-cap style must fail the block, got {err:?}"
            );
        }
    }

    /// Parent fails at the scene boundary too: an unchecked block that
    /// bypassed `RichBlock::new` is rejected by `Scene::insert` before it can
    /// paint, and the last good scene is retained.
    #[test]
    fn parent_scene_insert_rejects_unchecked_over_cap_style() {
        let mut scene = Scene::new();
        let good = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            tiny_text("ok"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        scene.insert(good).unwrap();
        let before = scene.aggregated_bytes();

        let bad = RichBlock::new_unchecked(
            BlockId(2),
            BlockAnchor::Zone(2),
            styled(SceneNodeStyle::new(
                0,
                0,
                SCENE_STYLE_MAX_BORDER_PX + 1,
                ContentToken::Accent,
            )),
            ScrollBehavior::Inline,
            1,
            0,
            2,
        );
        let err = scene.insert(bad).unwrap_err();
        assert!(matches!(err, SceneError::StyleInvalid { .. }));
        assert_eq!(scene.len(), 1, "last good scene is retained");
        assert_eq!(scene.aggregated_bytes(), before);
    }

    /// The replace path is a separate admission entry point: out-of-cap style
    /// content must fail there too, leaving the previous content and version
    /// untouched.
    #[test]
    fn scene_replace_content_rejects_unchecked_over_cap_style() {
        let mut scene = Scene::new();
        let good = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            tiny_text("ok"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        scene.insert(good).unwrap();
        let before = scene.get(BlockId(1)).unwrap().content().clone();
        let before_version = scene.get(BlockId(1)).unwrap().version();

        let err = scene
            .replace_content(
                BlockId(1),
                styled(SceneNodeStyle::new(0, 0, 17, ContentToken::Accent)),
            )
            .unwrap_err();
        assert!(matches!(err, SceneError::StyleInvalid { .. }));
        assert_eq!(
            scene.get(BlockId(1)).unwrap().content(),
            &before,
            "the rejected replacement never lands"
        );
        assert_eq!(scene.get(BlockId(1)).unwrap().version(), before_version);
    }

    #[test]
    fn style_is_not_text_and_keeps_the_v1_budgets() {
        let plain = tiny_text("hello");
        let styled_node = SceneNode::Styled {
            style: SceneNodeStyle::new(
                SCENE_STYLE_MAX_PADDING_CELLS,
                SCENE_STYLE_MAX_RADIUS_PX,
                SCENE_STYLE_MAX_BORDER_PX,
                ContentToken::Success,
            ),
            child: Box::new(plain.clone()),
        };
        assert_eq!(styled_node.text_bytes(), plain.text_bytes());
        // The wrapper is one node and one level, so SCN-1..2 stay bounded.
        assert_eq!(styled_node.count_nodes(), plain.count_nodes() + 1);
        assert_eq!(styled_node.depth(), plain.depth() + 1);
    }

    #[test]
    fn v1_content_reports_the_v1_schema_and_styled_content_reports_v2() {
        assert_eq!(RICH_BLOCK_VERSION, 1, "v1 is the accepted literal");
        assert_eq!(
            SCENE_STYLE_SCHEMA_VERSION, 2,
            "the additive style revision is the literal 2"
        );
        let v1 = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            tiny_text("v1"),
            ScrollBehavior::Inline,
            1,
            0,
            1,
        )
        .unwrap();
        assert_eq!(v1.schema_version(), 1);
        assert_eq!(v1.version(), 1);

        let v2 = RichBlock::new(
            BlockId(2),
            BlockAnchor::Zone(2),
            SceneNode::Column(vec![
                tiny_text("a"),
                styled(SceneNodeStyle::new(2, 4, 1, ContentToken::Accent)),
            ]),
            ScrollBehavior::Inline,
            1,
            0,
            2,
        )
        .unwrap();
        assert_eq!(v2.schema_version(), 2);
        // The block revision counter is unchanged: schema and revision are
        // distinct, so v1 decoding paths are unaffected.
        assert_eq!(v2.version(), 1);
    }

    #[test]
    fn flat_style_is_a_pure_wrapper() {
        assert_eq!(SceneNodeStyle::default(), SceneNodeStyle::FLAT);
        assert!(SceneNodeStyle::FLAT.is_flat());
        assert!(
            SceneNodeStyle::new(0, 0, 0, ContentToken::Error).is_flat(),
            "an inert token does not make a style non-flat"
        );
        assert!(!SceneNodeStyle::new(1, 0, 0, ContentToken::Foreground).is_flat());
        assert!(!SceneNodeStyle::new(0, 1, 0, ContentToken::Foreground).is_flat());
        assert!(!SceneNodeStyle::new(0, 0, 1, ContentToken::Foreground).is_flat());
    }

    #[test]
    fn unknown_kind_fallback_survives_a_styled_wrapper() {
        let node = styled_default(SceneNode::Unknown("fallback".to_string()));
        assert_eq!(node.text_bytes(), "fallback".len());
        assert_eq!(node.count_nodes(), 2);
        let block = RichBlock::new(
            BlockId(1),
            BlockAnchor::Zone(1),
            node,
            ScrollBehavior::Overlay,
            1,
            0,
            1,
        )
        .unwrap();
        assert_eq!(block.text_bytes(), "fallback".len());
    }

    fn styled_default(child: SceneNode) -> SceneNode {
        SceneNode::Styled {
            style: SceneNodeStyle::default(),
            child: Box::new(child),
        }
    }
}
