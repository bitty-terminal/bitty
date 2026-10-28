//! Scene paint path: a bounded `Scene` becomes a clipped paint list (OQ-051).
//!
//! The [Rich Presentation RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md)
//! accepts a declarative `SceneNode` model and states that the renderer
//! consumes a snapshot carrying `scene` and `damage`. This module is that
//! consumer: it lays out the admitted `Scene` inside the leaf content
//! rectangle the
//! [Workspace Compositor Specification](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/workspace-compositor.md)
//! owns (`content_rect = frame inset by border + content_inset`), emits a
//! bounded paint list, and reports the damage rectangle a scene revision
//! invalidates.
//!
//! # What this module is
//!
//! - **A composed layer inside the leaf rectangle**, never a compositor
//!   sub-surface: every emitted rectangle is clipped to the supplied leaf
//!   content rect, so terminal cells outside it are never painted.
//! - **Presentation only.** Nothing here reads or writes terminal truth; the
//!   input is an immutable `&Scene` and the output is owned paint data.
//! - **Headless.** No GPU, window, or adapter is touched. A caller turns the
//!   paint list into glyphs with [`GridRenderer::overlay_text_glyphs`] (the
//!   same cell-accurate text seam the pending-paste banner and IME preedit
//!   use), which works with the deterministic fake rasterizers used in CI.
//! - **Bounded.** Layout refuses (typed error, no partial paint) scenes that
//!   exceed the accepted `SCN` admission bounds or the paint budgets below.
//!   Nothing is silently clamped, truncated, or substituted.
//!
//! # Budgets
//!
//! | Budget | Value | Policy |
//! |---|---|---|
//! | [`SCENE_PAINT_MAX_OPS`] paint ops per frame | 16,384 | typed refusal |
//! | [`SCENE_PAINT_MAX_BORDER_CELLS`] block border width | 8 | typed refusal |
//! | blocks per frame | `SCENE_MAX_BLOCKS_PER_TERMINAL` (64) | typed refusal |
//! | nodes per block | `SCENE_MAX_NODES_PER_BLOCK` (2048) | typed refusal |
//! | depth per block | `SCENE_MAX_DEPTH` (32) | typed refusal |
//! | text bytes per frame | `SCENE_MAX_RICH_BYTES_PER_TERMINAL` (2 MiB) | typed refusal |
//!
//! The paint list is derived once per `(scene generation, surface)` pair by
//! [`ScenePaintCache`], so an unchanged scene costs no layout work and no
//! text copy on the next frame.
//!
//! # Composition and paint order
//!
//! A `Scene` is a bounded store of `RichBlock` values. Inside one leaf its
//! blocks compose top to bottom in scene iteration order; each block's root
//! subtree lays out with the rules below:
//!
//! | Node | Layout |
//! |---|---|
//! | `Text` / `Unknown` | one content row per `\n`-separated line, width = widest line |
//! | `Row` | children left to right; height = tallest child |
//! | `Column` | children top to bottom; width = widest child |
//! | `Block` | `border.width` cells of inset on every side around the child |
//! | `CodeBlock` | one row per content line, plus one header row when `lang` is set |
//! | `Table` | one row per table row, cells joined with `" \| "` |
//! | `List` | one row per item, prefixed `"- "` or `"N. "` when ordered |
//! | `Rule` | one content row holding a one-pixel horizontal line |
//! | `Image` | one content row reserved for the admitted placement |
//!
//! Ops are emitted in declaration order. Fills land in [`DrawList::fills`]
//! (below Core decoration rings, like cell backgrounds) and text glyphs land
//! in [`DrawList::glyphs`] (above the rings, like grid text), so a decorated
//! leaf keeps the accepted z-order. Image nodes emit a
//! [`ScenePaintOp::Image`] reference and no pixels: the admitted image plane
//! keeps owning image painting, and this path adds no decode seam.
//!
//! [`ScenePaint::damage`] is the painted extent intersected with the leaf
//! rect — a bounded invalidation rectangle. A scene revision never damages
//! outside its leaf, and a partial revision never forces a full-screen
//! repaint.

#![forbid(unsafe_code)]

use bitty_rich::image::PlacementId;
use bitty_rich::scene::{
    Border, CodeBlockModel, ListModel, RichBlock, SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH,
    SCENE_MAX_NODES_PER_BLOCK, SCENE_MAX_RICH_BYTES_PER_TERMINAL, Scene, SceneNode, TableModel,
};
use bitty_term_state::char_cell_width;

use crate::geometry::RectPx;
use crate::glyph::GlyphRasterizer;
use crate::grid::{
    CellMetrics, DECORATION_BORDER, DEFAULT_FG, DrawList, FillRect, GridRenderer, Rgba8,
};

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum paint ops one scene may emit into a frame.
///
/// Far above any admitted scene's practical shape (`SCN-1` allows 2048 nodes
/// per block) and low enough that one frame's paint walk stays bounded. A
/// scene that needs more is refused with a typed error.
pub const SCENE_PAINT_MAX_OPS: usize = 16_384;

/// Maximum block border width the paint path renders, in cells.
///
/// `Border::width` is producer-supplied and otherwise unbounded; a border
/// wider than this would consume the whole leaf. Wider borders are refused,
/// never clamped.
pub const SCENE_PAINT_MAX_BORDER_CELLS: u16 = 8;

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

/// The leaf content rectangle a scene paints into.
///
/// `rect` is the compositor-owned content rect in logical pixels (the frame
/// inset by `border + content_inset`); `cell` is the live cell geometry.
/// Cell spans are derived by flooring, so a partially visible last cell is
/// painted only to the pixel edge of `rect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneSurface {
    /// Leaf content rectangle in logical pixels.
    pub rect: RectPx,
    /// Live cell geometry.
    pub cell: CellMetrics,
}

impl SceneSurface {
    /// Builds a surface for one leaf content rect.
    #[must_use]
    pub const fn new(rect: RectPx, cell: CellMetrics) -> Self {
        Self { rect, cell }
    }

    /// Whole content columns that fit the rect.
    #[must_use]
    pub fn cols(&self) -> u16 {
        if self.rect.is_empty() {
            return 0;
        }
        u16::try_from(self.rect.width / self.cell.width).unwrap_or(u16::MAX)
    }

    /// Whole content rows that fit the rect.
    #[must_use]
    pub fn rows(&self) -> u16 {
        if self.rect.is_empty() {
            return 0;
        }
        u16::try_from(self.rect.height / self.cell.height).unwrap_or(u16::MAX)
    }

    /// The cell-aligned pixel area this surface paints.
    ///
    /// The remainder below/right of the last whole cell is left untouched,
    /// exactly like the grid path.
    #[must_use]
    pub fn aligned_rect(&self) -> RectPx {
        let aligned = RectPx::new(
            self.rect.x,
            self.rect.y,
            u32::from(self.cols()).saturating_mul(self.cell.width),
            u32::from(self.rows()).saturating_mul(self.cell.height),
        );
        aligned
            .intersection(&self.rect)
            .unwrap_or(RectPx::new(self.rect.x, self.rect.y, 0, 0))
    }
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// Per-frame paint budget, defaulting to the accepted scene ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenePaintBudget {
    /// Blocks admitted into one paint list (`SCN-5`).
    pub max_blocks: usize,
    /// Total paint ops emitted ([`SCENE_PAINT_MAX_OPS`]).
    pub max_ops: usize,
    /// Total text bytes emitted (`SCN-4`).
    pub max_text_bytes: usize,
}

impl Default for ScenePaintBudget {
    fn default() -> Self {
        Self {
            max_blocks: SCENE_MAX_BLOCKS_PER_TERMINAL,
            max_ops: SCENE_PAINT_MAX_OPS,
            max_text_bytes: SCENE_MAX_RICH_BYTES_PER_TERMINAL,
        }
    }
}

impl ScenePaintBudget {
    /// Empty budget: paints nothing and refuses any non-empty scene.
    #[must_use]
    pub const fn nothing() -> Self {
        Self {
            max_blocks: 0,
            max_ops: 0,
            max_text_bytes: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed refusal from the scene paint path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenePaintError {
    /// `SCN-5`: more blocks than the paint budget admits.
    BlocksTooMany {
        /// Blocks presented.
        count: usize,
        /// Budget.
        cap: usize,
    },
    /// `SCN-1`: a block exceeds the accepted node ceiling.
    NodesTooMany {
        /// Nodes counted.
        count: usize,
        /// Cap.
        cap: usize,
    },
    /// `SCN-2`: a block exceeds the accepted depth ceiling.
    DepthTooDeep {
        /// Depth measured.
        depth: usize,
        /// Cap.
        cap: usize,
    },
    /// `SCN-4`: the frame's text exceeds the paint text budget.
    TextTooLarge {
        /// Bytes presented.
        bytes: usize,
        /// Budget.
        cap: usize,
    },
    /// The paint list would exceed [`SCENE_PAINT_MAX_OPS`].
    OpsTooMany {
        /// Ops emitted.
        count: usize,
        /// Budget.
        cap: usize,
    },
    /// `Border::width` exceeds [`SCENE_PAINT_MAX_BORDER_CELLS`].
    BorderTooWide {
        /// Declared cells.
        width: u16,
        /// Cap.
        cap: u16,
    },
}

impl std::fmt::Display for ScenePaintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlocksTooMany { count, cap } => {
                write!(f, "scene paint blocks too many: {count} > {cap}")
            }
            Self::NodesTooMany { count, cap } => {
                write!(f, "scene paint nodes too many: {count} > {cap}")
            }
            Self::DepthTooDeep { depth, cap } => {
                write!(f, "scene paint depth too deep: {depth} > {cap}")
            }
            Self::TextTooLarge { bytes, cap } => {
                write!(f, "scene paint text too large: {bytes} > {cap}")
            }
            Self::OpsTooMany { count, cap } => {
                write!(f, "scene paint ops too many: {count} > {cap}")
            }
            Self::BorderTooWide { width, cap } => {
                write!(f, "scene paint border too wide: {width} > {cap}")
            }
        }
    }
}

impl std::error::Error for ScenePaintError {}

// ---------------------------------------------------------------------------
// Paint list
// ---------------------------------------------------------------------------

/// One paint op in the scene list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenePaintOp {
    /// Opaque rectangle (border segment or rule line).
    Fill {
        /// Cell-aligned rectangle in logical pixels.
        rect: RectPx,
        /// Straight-alpha fill color.
        color: Rgba8,
    },
    /// One text line painted left-to-right inside `rect`.
    ///
    /// `rect` is cell-aligned; `cols` is the number of whole cells the line
    /// may occupy. Rasterization reuses the grid renderer's overlay text
    /// seam, so the glyphs match cell text exactly.
    Text {
        /// Zero-based content row inside the leaf.
        row: u16,
        /// Zero-based content column of the line's first cell.
        col: u16,
        /// Cell-aligned line rectangle.
        rect: RectPx,
        /// Whole cells available to the line.
        cols: u16,
        /// Line text (bounded by the scene text budget).
        text: String,
        /// Text color.
        color: Rgba8,
        /// Painted with the deterministic one-pixel faux-bold pass.
        bold: bool,
    },
    /// Reference to an admitted image placement.
    ///
    /// No pixels are produced here: the existing image plane owns decoding,
    /// rasterization, and blitting, and this op tells the caller where the
    /// scene reserves room for it.
    Image {
        /// Placement the scene refers to.
        placement: PlacementId,
        /// Reserved destination rectangle (one content row).
        rect: RectPx,
    },
}

impl ScenePaintOp {
    /// The pixel rectangle this op covers (the reserved rect for images).
    #[must_use]
    pub const fn rect(&self) -> RectPx {
        match self {
            Self::Fill { rect, .. } | Self::Text { rect, .. } | Self::Image { rect, .. } => *rect,
        }
    }

    /// The op's text, when it paints text.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::Fill { .. } | Self::Image { .. } => None,
        }
    }
}

/// One painted text line, in paint order.
///
/// The accessibility projection and the plain-text export consume exactly
/// these lines, so painted and projected text cannot diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneTextLine<'a> {
    /// Zero-based content row inside the leaf.
    pub row: u16,
    /// Zero-based content column of the line's first cell.
    pub col: u16,
    /// Painted line text.
    pub text: &'a str,
}

/// Deterministic paint list for one scene and one leaf content rect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScenePaint {
    generation: u64,
    ops: Vec<ScenePaintOp>,
    damage: Vec<RectPx>,
    text_bytes: usize,
    node_count: usize,
    block_count: usize,
    image_count: usize,
}

impl ScenePaint {
    /// Scene generation this list was derived from.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Ordered paint ops.
    #[must_use]
    pub fn ops(&self) -> &[ScenePaintOp] {
        &self.ops
    }

    /// True when the list paints nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Bounded invalidation rectangles for this list.
    ///
    /// Empty when nothing paints; otherwise one rectangle: the painted
    /// extent intersected with the leaf content rect.
    #[must_use]
    pub fn damage(&self) -> &[RectPx] {
        &self.damage
    }

    /// Text bytes carried by the paint list.
    #[must_use]
    pub const fn text_bytes(&self) -> usize {
        self.text_bytes
    }

    /// Nodes visited while building the list.
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.node_count
    }

    /// Blocks visited while building the list.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    /// Image placement references in the list.
    #[must_use]
    pub const fn image_count(&self) -> usize {
        self.image_count
    }

    /// Painted text lines in paint order.
    ///
    /// This is the raw-text projection the accepted interface document keeps
    /// always available (`CopyAsRaw` fallback): the same bytes the frame
    /// paints, in the same order.
    pub fn text_lines(&self) -> impl Iterator<Item = SceneTextLine<'_>> + '_ {
        self.ops.iter().filter_map(|op| match op {
            ScenePaintOp::Text { row, col, text, .. } => Some(SceneTextLine {
                row: *row,
                col: *col,
                text: text.as_str(),
            }),
            ScenePaintOp::Fill { .. } | ScenePaintOp::Image { .. } => None,
        })
    }

    /// Plain-text fallback: painted lines joined with `\n`, in paint order.
    ///
    /// Bounded by [`Self::text_bytes`] plus one newline per line. This is the
    /// `export --plain` / `copy --raw` projection of a scene; it never
    /// executes, fetches, or interprets content.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let mut out = String::with_capacity(self.text_bytes);
        let mut first = true;
        for line in self.text_lines() {
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(line.text);
        }
        out
    }

    /// Pushes this list's fills and glyphs into a frame [`DrawList`].
    ///
    /// Image ops are skipped: the admitted image plane paints them, and this
    /// call must never fabricate pixels. Returns the number of image
    /// references skipped so the caller can attribute them.
    pub fn append_to<R: GlyphRasterizer>(
        &self,
        renderer: &mut GridRenderer<R>,
        list: &mut DrawList,
    ) -> usize {
        let mut images = 0usize;
        for op in &self.ops {
            match op {
                ScenePaintOp::Fill { rect, color } => {
                    list.fills.push(FillRect {
                        rect: *rect,
                        color: *color,
                    });
                }
                ScenePaintOp::Text {
                    rect,
                    cols,
                    text,
                    color,
                    bold,
                    ..
                } => {
                    list.glyphs.extend(renderer.overlay_text_glyphs(
                        text,
                        (rect.x, rect.y),
                        usize::from(*cols),
                        *color,
                    ));
                    if *bold {
                        // Deterministic faux bold: one extra pass one pixel to
                        // the right, bounded by the same `cols` cap.
                        list.glyphs.extend(renderer.overlay_text_glyphs(
                            text,
                            (rect.x.saturating_add(1), rect.y),
                            usize::from(*cols),
                            *color,
                        ));
                    }
                }
                ScenePaintOp::Image { .. } => images += 1,
            }
        }
        images
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

/// Style used while painting a scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenePaintStyle {
    /// Text color for scene text.
    pub text: Rgba8,
    /// Rule and border color (Core-owned decoration role).
    pub rule: Rgba8,
}

impl Default for ScenePaintStyle {
    fn default() -> Self {
        Self {
            text: DEFAULT_FG,
            rule: DECORATION_BORDER,
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Lays out `scene` inside `surface` and returns its bounded paint list.
///
/// # Errors
///
/// A [`ScenePaintError`] when the scene exceeds the accepted `SCN` bounds or
/// the paint budgets. The returned error is the only output: no partial
/// paint list escapes, so a refused scene paints nothing and the caller
/// keeps the previous frame.
pub fn paint_scene(
    scene: &Scene,
    surface: SceneSurface,
    budget: ScenePaintBudget,
) -> Result<ScenePaint, ScenePaintError> {
    paint_scene_with_style(scene, surface, budget, ScenePaintStyle::default())
}

/// [`paint_scene`] with an explicit Core-resolved style.
///
/// # Errors
///
/// Same as [`paint_scene`].
pub fn paint_scene_with_style(
    scene: &Scene,
    surface: SceneSurface,
    budget: ScenePaintBudget,
    style: ScenePaintStyle,
) -> Result<ScenePaint, ScenePaintError> {
    if scene.len() > budget.max_blocks {
        return Err(ScenePaintError::BlocksTooMany {
            count: scene.len(),
            cap: budget.max_blocks,
        });
    }
    if scene.aggregated_bytes() > budget.max_text_bytes {
        return Err(ScenePaintError::TextTooLarge {
            bytes: scene.aggregated_bytes(),
            cap: budget.max_text_bytes,
        });
    }

    let mut painter = Painter {
        surface,
        budget,
        style,
        ops: Vec::new(),
        text_bytes: 0,
        node_count: 0,
        block_count: 0,
        image_count: 0,
    };
    if surface.cols() == 0 || surface.rows() == 0 {
        // A zero-area leaf paints nothing; this is a valid empty frame, not
        // an error, and it still carries the scene generation.
        return Ok(ScenePaint {
            generation: scene.generation(),
            ..ScenePaint::default()
        });
    }
    let cols = surface.cols();
    let rows = surface.rows();
    let mut y = 0u16;
    for block in scene.iter() {
        if y >= rows {
            break;
        }
        let size = painter.measure(block.content(), 1)?;
        painter.block(block, y, cols, rows - y)?;
        y = y.saturating_add(size.rows);
    }
    let damage = painter
        .ops
        .iter()
        .map(ScenePaintOp::rect)
        .reduce(|a, b| RectPx::union_bounding(&a, &b))
        .and_then(|rect| rect.intersection(&surface.aligned_rect()))
        .into_iter()
        .collect();
    Ok(ScenePaint {
        generation: scene.generation(),
        ops: painter.ops,
        damage,
        text_bytes: painter.text_bytes,
        node_count: painter.node_count,
        block_count: painter.block_count,
        image_count: painter.image_count,
    })
}

/// Paint cache: derives one paint list per `(generation, surface)` pair.
///
/// The present path caches this per panel slot, so an unchanged scene costs
/// no layout walk and no text copy. A refusal is cached too: re-presenting
/// the same over-budget scene keeps failing closed without re-walking it.
#[derive(Debug, Clone, Default)]
pub struct ScenePaintCache {
    key: Option<(u64, RectPx, CellMetrics)>,
    paint: ScenePaint,
    refused: Option<ScenePaintError>,
}

impl ScenePaintCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the paint list for `scene`/`surface`, rebuilding only when the
    /// scene generation or the surface changed.
    ///
    /// # Errors
    ///
    /// Same as [`paint_scene`]; a refused build is cached and reported until
    /// the key changes.
    pub fn get_or_paint(
        &mut self,
        scene: &Scene,
        surface: SceneSurface,
        budget: ScenePaintBudget,
    ) -> Result<&ScenePaint, ScenePaintError> {
        let key = (scene.generation(), surface.rect, surface.cell);
        if self.key != Some(key) {
            self.key = Some(key);
            match paint_scene(scene, surface, budget) {
                Ok(paint) => {
                    self.paint = paint;
                    self.refused = None;
                }
                Err(error) => {
                    self.paint = ScenePaint::default();
                    self.refused = Some(error);
                }
            }
        }
        match &self.refused {
            Some(error) => Err(error.clone()),
            None => Ok(&self.paint),
        }
    }

    /// The cached paint list, if the current key produced one.
    #[must_use]
    pub fn paint(&self) -> Option<&ScenePaint> {
        if self.refused.is_some() || self.key.is_none() {
            None
        } else {
            Some(&self.paint)
        }
    }

    /// Drops the cached entry (panel detach or an explicit invalidation).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Requested size of one node in content cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Size {
    cols: u16,
    rows: u16,
}

struct Painter {
    surface: SceneSurface,
    budget: ScenePaintBudget,
    style: ScenePaintStyle,
    ops: Vec<ScenePaintOp>,
    text_bytes: usize,
    node_count: usize,
    block_count: usize,
    image_count: usize,
}

impl Painter {
    fn block(
        &mut self,
        block: &RichBlock,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<(), ScenePaintError> {
        self.block_count = self.block_count.saturating_add(1);
        let nodes = block.node_count();
        if nodes > SCENE_MAX_NODES_PER_BLOCK {
            return Err(ScenePaintError::NodesTooMany {
                count: nodes,
                cap: SCENE_MAX_NODES_PER_BLOCK,
            });
        }
        let depth = block.depth();
        if depth > SCENE_MAX_DEPTH {
            return Err(ScenePaintError::DepthTooDeep {
                depth,
                cap: SCENE_MAX_DEPTH,
            });
        }
        self.node_count = self.node_count.saturating_add(nodes);
        let remaining = self.budget.max_text_bytes.saturating_sub(self.text_bytes);
        if block.text_bytes() > remaining {
            return Err(ScenePaintError::TextTooLarge {
                bytes: self.text_bytes.saturating_add(block.text_bytes()),
                cap: self.budget.max_text_bytes,
            });
        }
        self.paint_node(block.content(), 0, y, width, height, 1)
    }

    fn measure(&self, node: &SceneNode, depth: usize) -> Result<Size, ScenePaintError> {
        if depth > SCENE_MAX_DEPTH {
            return Err(ScenePaintError::DepthTooDeep {
                depth,
                cap: SCENE_MAX_DEPTH,
            });
        }
        let cols = self.surface.cols();
        let rows = self.surface.rows();
        let size = match node {
            SceneNode::Text(span) => {
                let (width, height) = text_extent(&span.text);
                Size {
                    cols: width.min(cols),
                    rows: height.min(rows),
                }
            }
            SceneNode::Unknown(text) => {
                let (width, height) = text_extent(text);
                Size {
                    cols: width.min(cols),
                    rows: height.min(rows),
                }
            }
            SceneNode::Row(children) => {
                let mut width = 0u16;
                let mut height = 0u16;
                for child in children {
                    let size = self.measure(child, depth + 1)?;
                    width = width.saturating_add(size.cols).min(cols);
                    height = height.max(size.rows);
                }
                Size {
                    cols: width,
                    rows: height.min(rows),
                }
            }
            SceneNode::Column(children) => {
                let mut width = 0u16;
                let mut height = 0u16;
                for child in children {
                    let size = self.measure(child, depth + 1)?;
                    width = width.max(size.cols);
                    height = height.saturating_add(size.rows).min(rows);
                }
                Size {
                    cols: width.min(cols),
                    rows: height,
                }
            }
            SceneNode::Block { border, child } => {
                let inset = border_cells(border.as_ref())?;
                let inner = self.measure(child, depth + 1)?;
                Size {
                    cols: inner.cols.saturating_add(inset.saturating_mul(2)).min(cols),
                    rows: inner.rows.saturating_add(inset.saturating_mul(2)).min(rows),
                }
            }
            SceneNode::CodeBlock(model) => {
                let (width, height) = code_block_extent(model);
                Size {
                    cols: width.min(cols),
                    rows: height.min(rows),
                }
            }
            SceneNode::Table(model) => {
                let (width, height) = table_extent(model);
                Size {
                    cols: width.min(cols),
                    rows: height.min(rows),
                }
            }
            SceneNode::List(model) => {
                let (width, height) = list_extent(model);
                Size {
                    cols: width.min(cols),
                    rows: height.min(rows),
                }
            }
            SceneNode::Rule => Size { cols, rows: 1 },
            SceneNode::Image(_) => Size { cols, rows: 1 },
        };
        Ok(size)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_node(
        &mut self,
        node: &SceneNode,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        depth: usize,
    ) -> Result<(), ScenePaintError> {
        if depth > SCENE_MAX_DEPTH {
            return Err(ScenePaintError::DepthTooDeep {
                depth,
                cap: SCENE_MAX_DEPTH,
            });
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        match node {
            SceneNode::Text(span) => self.paint_lines(&span.text, x, y, width, height, span.bold),
            SceneNode::Unknown(text) => self.paint_lines(text, x, y, width, height, false),
            SceneNode::Row(children) => {
                let mut cursor = 0u16;
                for child in children {
                    if cursor >= width {
                        break;
                    }
                    let size = self.measure(child, depth + 1)?;
                    self.paint_node(
                        child,
                        x.saturating_add(cursor),
                        y,
                        width - cursor,
                        height,
                        depth + 1,
                    )?;
                    cursor = cursor.saturating_add(size.cols);
                }
                Ok(())
            }
            SceneNode::Column(children) => {
                let mut cursor = 0u16;
                for child in children {
                    if cursor >= height {
                        break;
                    }
                    self.paint_node(
                        child,
                        x,
                        y.saturating_add(cursor),
                        width,
                        height - cursor,
                        depth + 1,
                    )?;
                    cursor = cursor.saturating_add(self.measure(child, depth + 1)?.rows);
                }
                Ok(())
            }
            SceneNode::Block { border, child } => {
                let inset = border_cells(border.as_ref())?;
                if inset > 0 {
                    self.paint_border(x, y, width, height, inset)?;
                }
                let double = inset.saturating_mul(2);
                if width > double && height > double {
                    self.paint_node(
                        child,
                        x.saturating_add(inset),
                        y.saturating_add(inset),
                        width - double,
                        height - double,
                        depth + 1,
                    )?;
                }
                Ok(())
            }
            SceneNode::CodeBlock(model) => {
                let mut row = 0u16;
                if let Some(lang) = &model.lang {
                    if row < height {
                        self.push_text_line(lang, x, y, width, false)?;
                        row = row.saturating_add(1);
                    }
                }
                for line in model.content.lines() {
                    if row >= height {
                        break;
                    }
                    self.push_text_line(line, x, y.saturating_add(row), width, false)?;
                    row = row.saturating_add(1);
                }
                Ok(())
            }
            SceneNode::Table(model) => {
                for (index, table_row) in model.rows.iter().enumerate() {
                    let Ok(index) = u16::try_from(index) else {
                        break;
                    };
                    if index >= height {
                        break;
                    }
                    self.push_text_line(
                        &table_row.join(" | "),
                        x,
                        y.saturating_add(index),
                        width,
                        false,
                    )?;
                }
                Ok(())
            }
            SceneNode::List(model) => {
                for (index, item) in model.items.iter().enumerate() {
                    let Ok(index) = u16::try_from(index) else {
                        break;
                    };
                    if index >= height {
                        break;
                    }
                    let prefix = if model.ordered {
                        format!("{}. ", index.saturating_add(1))
                    } else {
                        "- ".to_string()
                    };
                    self.push_text_line(
                        &format!("{prefix}{item}"),
                        x,
                        y.saturating_add(index),
                        width,
                        false,
                    )?;
                }
                Ok(())
            }
            SceneNode::Rule => {
                let rect = self.cell_rect(x, y, width, 1);
                let line = RectPx::new(
                    rect.x,
                    rect.y
                        .saturating_add(saturating_i32_from_u32(self.surface.cell.height / 2)),
                    rect.width,
                    1,
                )
                .intersection(&self.surface.aligned_rect());
                if let Some(rect) = line {
                    self.push_op(ScenePaintOp::Fill {
                        rect,
                        color: self.style.rule,
                    })?;
                }
                Ok(())
            }
            SceneNode::Image(placement) => {
                let rect = self.cell_rect(x, y, width, 1);
                let Some(rect) = rect.intersection(&self.surface.aligned_rect()) else {
                    return Ok(());
                };
                self.image_count = self.image_count.saturating_add(1);
                self.push_op(ScenePaintOp::Image {
                    placement: *placement,
                    rect,
                })
            }
        }
    }

    fn paint_lines(
        &mut self,
        text: &str,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        bold: bool,
    ) -> Result<(), ScenePaintError> {
        for (index, line) in text.split('\n').enumerate() {
            let Ok(index) = u16::try_from(index) else {
                break;
            };
            if index >= height {
                break;
            }
            self.push_text_line(line, x, y.saturating_add(index), width, bold)?;
        }
        Ok(())
    }

    fn push_text_line(
        &mut self,
        text: &str,
        x: u16,
        y: u16,
        width: u16,
        bold: bool,
    ) -> Result<(), ScenePaintError> {
        if width == 0 {
            return Ok(());
        }
        let rect = self.cell_rect(x, y, width, 1);
        let Some(rect) = rect.intersection(&self.surface.aligned_rect()) else {
            return Ok(());
        };
        let cols = u16::try_from(rect.width / self.surface.cell.width).unwrap_or(u16::MAX);
        if cols == 0 {
            return Ok(());
        }
        self.text_bytes = self.text_bytes.saturating_add(text.len());
        if self.text_bytes > self.budget.max_text_bytes {
            return Err(ScenePaintError::TextTooLarge {
                bytes: self.text_bytes,
                cap: self.budget.max_text_bytes,
            });
        }
        self.push_op(ScenePaintOp::Text {
            row: y,
            col: x,
            rect,
            cols,
            text: text.to_string(),
            color: self.style.text,
            bold,
        })
    }

    fn paint_border(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        inset: u16,
    ) -> Result<(), ScenePaintError> {
        let rect = self.cell_rect(x, y, width, height);
        let Some(rect) = rect.intersection(&self.surface.aligned_rect()) else {
            return Ok(());
        };
        let horizontal = u32::from(inset)
            .saturating_mul(self.surface.cell.height)
            .min(rect.height);
        let vertical = u32::from(inset)
            .saturating_mul(self.surface.cell.width)
            .min(rect.width);
        let segments = [
            RectPx::new(rect.x, rect.y, rect.width, horizontal),
            RectPx::new(
                rect.x,
                rect.y.saturating_add(saturating_i32_from_u32(
                    rect.height.saturating_sub(horizontal),
                )),
                rect.width,
                horizontal,
            ),
            RectPx::new(rect.x, rect.y, vertical, rect.height),
            RectPx::new(
                rect.x
                    .saturating_add(saturating_i32_from_u32(rect.width.saturating_sub(vertical))),
                rect.y,
                vertical,
                rect.height,
            ),
        ];
        for segment in segments {
            if !segment.is_empty() {
                self.push_op(ScenePaintOp::Fill {
                    rect: segment,
                    color: self.style.rule,
                })?;
            }
        }
        Ok(())
    }

    fn cell_rect(&self, x: u16, y: u16, cols: u16, rows: u16) -> RectPx {
        RectPx::new(
            self.surface.rect.x.saturating_add(saturating_i32_from_u32(
                u32::from(x).saturating_mul(self.surface.cell.width),
            )),
            self.surface.rect.y.saturating_add(saturating_i32_from_u32(
                u32::from(y).saturating_mul(self.surface.cell.height),
            )),
            u32::from(cols).saturating_mul(self.surface.cell.width),
            u32::from(rows).saturating_mul(self.surface.cell.height),
        )
    }

    fn push_op(&mut self, op: ScenePaintOp) -> Result<(), ScenePaintError> {
        if self.ops.len() >= self.budget.max_ops {
            return Err(ScenePaintError::OpsTooMany {
                count: self.ops.len().saturating_add(1),
                cap: self.budget.max_ops,
            });
        }
        self.ops.push(op);
        Ok(())
    }
}

/// Resolved block border inset in cells (`0` for no border or `width == 0`).
fn border_cells(border: Option<&Border>) -> Result<u16, ScenePaintError> {
    let Some(border) = border else {
        return Ok(0);
    };
    if border.width == 0 {
        return Ok(0);
    }
    if border.width > SCENE_PAINT_MAX_BORDER_CELLS {
        return Err(ScenePaintError::BorderTooWide {
            width: border.width,
            cap: SCENE_PAINT_MAX_BORDER_CELLS,
        });
    }
    Ok(border.width)
}

fn saturating_i32_from_u32(value: u32) -> i32 {
    if value > i32::MAX as u32 {
        i32::MAX
    } else {
        value as i32
    }
}

// ---------------------------------------------------------------------------
// Model extent helpers
// ---------------------------------------------------------------------------

fn text_extent(text: &str) -> (u16, u16) {
    let mut width = 0usize;
    let mut height = 0u16;
    for line in text.split('\n') {
        width = width.max(line_cells(line));
        height = height.saturating_add(1);
    }
    (u16::try_from(width).unwrap_or(u16::MAX), height)
}

fn code_block_extent(model: &CodeBlockModel) -> (u16, u16) {
    let mut width = 0usize;
    let mut height = 0u16;
    if let Some(lang) = &model.lang {
        width = line_cells(lang);
        height = 1;
    }
    for line in model.content.lines() {
        width = width.max(line_cells(line));
        height = height.saturating_add(1);
    }
    (u16::try_from(width).unwrap_or(u16::MAX), height)
}

fn table_extent(model: &TableModel) -> (u16, u16) {
    let mut width = 0usize;
    for row in &model.rows {
        width = width.max(line_cells(&row.join(" | ")));
    }
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(model.rows.len()).unwrap_or(u16::MAX),
    )
}

fn list_extent(model: &ListModel) -> (u16, u16) {
    let mut width = 0usize;
    for (index, item) in model.items.iter().enumerate() {
        let prefix = if model.ordered {
            format!("{}. ", index.saturating_add(1))
        } else {
            "- ".to_string()
        };
        width = width.max(line_cells(&prefix) + line_cells(item));
    }
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(model.items.len()).unwrap_or(u16::MAX),
    )
}

fn line_cells(line: &str) -> usize {
    line.chars()
        .map(|ch| usize::from(char_cell_width(ch)))
        .sum::<usize>()
}
