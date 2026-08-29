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

/// Declarative layout and paint primitive (v1).
///
/// Text, rows, columns, blocks, images, tables, lists, and rules. No shaders,
/// pipelines, native windows, or global-coordinate drawing. Unknown variants
/// received from a newer producer are rendered as bounded plain-text fallback
/// (`Unknown`) per compatibility requirement.
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
    #[must_use]
    pub fn count_nodes(&self) -> usize {
        match self {
            Self::Text(_) | Self::Image(_) | Self::Rule | Self::Unknown(_) => 1,
            Self::CodeBlock(_) | Self::Table(_) | Self::List(_) => 1,
            Self::Row(children) | Self::Column(children) => {
                1 + children.iter().map(Self::count_nodes).sum::<usize>()
            }
            Self::Block { child, .. } => 1 + child.count_nodes(),
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
            Self::Block { child, .. } => 1 + child.depth(),
        }
    }

    /// Text bytes in subtree (sum of all string payloads).
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
            Self::Block { child, .. } => child.text_bytes(),
            Self::Image(_) | Self::Rule => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// RichBlock
// ---------------------------------------------------------------------------

/// Versioned, plugin-owned rich region anchored to terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichBlock {
    /// Stable identifier distinct from its anchor.
    pub id: BlockId,
    /// Monotonic version (1 for v1).
    pub version: u32,
    /// Anchor.
    pub anchor: BlockAnchor,
    /// Root of this block's subtree.
    pub content: SceneNode,
    /// Scroll behavior.
    pub scroll: ScrollBehavior,
    /// Owner plugin id (attributable).
    pub owner: u64,
    /// Lifecycle generation.
    pub generation: u64,
    /// Provenance zone id for diagnostics.
    pub created_at: u64,
}

impl RichBlock {
    /// Creates a v1 block with validation (SCN-1..3).
    ///
    /// Returns `Err` with a typed diagnostic when any per-block limit is
    /// exceeded; the last good scene is retained by the caller (RFC).
    pub fn new(
        id: BlockId,
        anchor: BlockAnchor,
        content: SceneNode,
        scroll: ScrollBehavior,
        owner: u64,
        generation: u64,
        created_at: u64,
    ) -> Result<Self, SceneError> {
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
        }
    }
}

impl std::error::Error for SceneError {}

// ---------------------------------------------------------------------------
// Scene (composed)
// ---------------------------------------------------------------------------

/// Composed tree of `SceneNode` values with damage for one frame (headless).
///
/// Bounded: node count, depth, text bytes, and total layout size are
/// validated at insertion (SCN-1..5). Incremental updates diff the previous
/// tree; here we model the store as a bounded map of `RichBlock`s keyed by
/// `BlockId`. Renderer consumes a snapshot of this store plus damage.
#[derive(Debug, Clone)]
pub struct Scene {
    blocks: VecDeque<RichBlock>,
    aggregated_bytes: usize,
    generation: u64,
    next_id: u64,
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
        }
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
    /// Validates per-block SCN-1..3 via `RichBlock::new` (caller should have
    /// constructed the block via that helper), then checks SCN-4 and SCN-5
    /// at admission. On failure returns a typed diagnostic and retains the
    /// last good scene (no partial insertion).
    pub fn insert(&mut self, block: RichBlock) -> Result<(), SceneError> {
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
        let nodes = new_content.count_nodes();
        if nodes > SCENE_MAX_NODES_PER_BLOCK {
            return Err(SceneError::NodesTooMany {
                count: nodes,
                cap: SCENE_MAX_NODES_PER_BLOCK,
            });
        }
        let depth = new_content.depth();
        if depth > SCENE_MAX_DEPTH {
            return Err(SceneError::DepthTooDeep {
                depth,
                cap: SCENE_MAX_DEPTH,
            });
        }
        let new_bytes = new_content.text_bytes();
        if new_bytes > SCENE_MAX_TEXT_BYTES_PER_BLOCK {
            return Err(SceneError::TextTooLarge {
                bytes: new_bytes,
                cap: SCENE_MAX_TEXT_BYTES_PER_BLOCK,
            });
        }

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
        let v1 = block.version;
        scene.insert(block).unwrap();
        // Replace with larger but within block cap
        scene
            .replace_content(id, tiny_text(&"b".repeat(100)))
            .unwrap();
        assert_eq!(scene.get(id).unwrap().version, v1 + 1);
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
        let before = scene.get(id).unwrap().content.clone();
        // Try to replace with over-depth content
        let deep = deep_nodes(SCENE_MAX_DEPTH + 5);
        let err = scene.replace_content(id, deep).unwrap_err();
        assert!(matches!(err, SceneError::DepthTooDeep { .. }));
        // Still old content
        assert_eq!(scene.get(id).unwrap().content, before);
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
}
