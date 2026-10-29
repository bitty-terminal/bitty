//! Rich-scene bridge: `ui.rich` declarative nodes become admitted `Scene` content.
//!
//! CTX-1628 / bitty#2760. The Lua surface validates declarative tables into
//! [`UiRichNode`] values (`bitty-lua`), the accepted
//! [Rich Presentation RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md)
//! model owns `SceneNode`/`RichBlock` (`bitty-rich`), and the renderer paints
//! a `Scene` (`bitty-render::scene`). This module is the single Core-owned
//! conversion between them: it introduces no third node vocabulary and no
//! renderer handle.
//!
//! # Boundaries
//!
//! - **Mechanism, not policy.** The caller is the `ui.rich` mount path; this
//!   module never reads a capability table, a manifest, or a plugin handle. A
//!   mount that has not passed the `ui.rich` gate must not reach it.
//!
//!   The shipped caller is [`SceneHost`](crate::scene_host::SceneHost)
//!   (CTX-1641 / bitty#2785): `bitty.ui.mount("terminal", …)` and `ui.update`
//!   read through the `ui.rich` reader, and the host attaches the tree this
//!   bridge admits with `PanelRuntime::attach_panel_scene`. The bridge owns
//!   only the conversion; the host owns the panel identity, the placement
//!   bind, and release.
//! - **Admission stays in `bitty-rich`.** [`mount_rich_node`] inserts through
//!   [`Scene::insert`], so `SCN-1..5` are validated at the same trust boundary
//!   as every other rich block. A bridge conversion never bypasses a limit.
//! - **No pixels, no I/O.** This is pure data conversion; `Image` stays a
//!   placement reference, and the admitted image plane keeps owning decode and
//!   blit.
//! - **Core-resolved presentation.** `Block.border` carries a width only; the
//!   border colour is Core-owned (the accepted scene model's producer-supplied
//!   colour string is carried empty and the paint path resolves the Core
//!   decoration role). Fonts, colours, and layout geometry are never taken
//!   from the plugin.
//!
//! # Layout mapping
//!
//! | `UiRichNode` | `SceneNode` |
//! |---|---|
//! | `Text { text, bold }` | `Text(StyledSpan { text, bold, italic: false })` |
//! | `Row` | `Row` |
//! | `Column` | `Column` |
//! | `List` | `Column` (the v1 list container composes vertically) |
//! | `Block { border, child }` | `Block { border: width > 0, child }` |
//! | `Image { placement }` | `Image(PlacementId)` |
//! | `CodeBlock` | `CodeBlock` |
//! | `Table` | `Table` |
//! | `Rule` | `Rule` |

#![forbid(unsafe_code)]

use bitty_lua::UiRichNode;
use bitty_rich::image::PlacementId;
use bitty_rich::scene::{
    BlockAnchor, BlockId, Border, CodeBlockModel, RichBlock, Scene, SceneError, SceneNode,
    ScrollBehavior, StyledSpan, TableModel,
};

/// Typed failure from the rich-scene bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SceneBridgeError {
    /// The accepted scene admission boundary refused the block (`SCN-1..5`).
    Admission(SceneError),
}

impl std::fmt::Display for SceneBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(error) => write!(f, "rich block refused at admission: {error}"),
        }
    }
}

impl std::error::Error for SceneBridgeError {}

impl From<SceneError> for SceneBridgeError {
    fn from(error: SceneError) -> Self {
        Self::Admission(error)
    }
}

/// Converts one validated rich node into the accepted scene model.
///
/// Infallible: every [`UiRichNode`] variant has a `SceneNode` analogue, and
/// the conversion adds no field the model cannot express.
#[must_use]
pub fn scene_node_of(node: &UiRichNode) -> SceneNode {
    match node {
        UiRichNode::Text { text, bold } => SceneNode::Text(StyledSpan {
            text: text.clone(),
            bold: *bold,
            italic: false,
        }),
        UiRichNode::Row(children) => SceneNode::Row(children.iter().map(scene_node_of).collect()),
        UiRichNode::Column(children) => {
            SceneNode::Column(children.iter().map(scene_node_of).collect())
        }
        UiRichNode::List(children) => {
            SceneNode::Column(children.iter().map(scene_node_of).collect())
        }
        UiRichNode::Block { border, child } => SceneNode::Block {
            border: (*border > 0).then(|| Border {
                width: *border,
                // Core owns the colour; the paint path resolves its
                // decoration role and never parses a producer string.
                color: String::new(),
            }),
            child: Box::new(scene_node_of(child)),
        },
        UiRichNode::Image { placement } => SceneNode::Image(PlacementId(*placement)),
        UiRichNode::CodeBlock { lang, content } => SceneNode::CodeBlock(CodeBlockModel {
            lang: lang.clone(),
            content: content.clone(),
        }),
        UiRichNode::Table { rows } => SceneNode::Table(TableModel { rows: rows.clone() }),
        UiRichNode::Rule => SceneNode::Rule,
    }
}

/// Identity and provenance for one rich mount.
///
/// Anchoring and scroll behaviour stay caller-supplied presentation choices:
/// a rich panel leaf has no scrollback line, so the mount path passes the
/// anchor its leaf model uses. Owner and generation are the attribution the
/// accepted RFC requires on every block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichMount {
    /// Stable block identifier.
    pub id: BlockId,
    /// Anchor binding the block to terminal state.
    pub anchor: BlockAnchor,
    /// Scroll behaviour for the block.
    pub scroll: ScrollBehavior,
    /// Owning plugin id (attribution).
    pub owner: u64,
    /// Lifecycle generation (attribution).
    pub generation: u64,
    /// Provenance zone id for diagnostics.
    pub created_at: u64,
}

/// Builds one admitted `RichBlock` from a validated rich node.
///
/// # Errors
///
/// [`SceneBridgeError::Admission`] when the block content exceeds `SCN-1..3`.
pub fn rich_block(mount: &RichMount, node: &UiRichNode) -> Result<RichBlock, SceneBridgeError> {
    Ok(RichBlock::new(
        mount.id,
        mount.anchor.clone(),
        scene_node_of(node),
        mount.scroll,
        mount.owner,
        mount.generation,
        mount.created_at,
    )?)
}

/// Converts and inserts one validated rich node into a scene.
///
/// Admission runs through [`Scene::insert`] (`SCN-1..5`), so an over-budget
/// node is refused with the typed limit and the previous scene is retained.
///
/// # Errors
///
/// [`SceneBridgeError::Admission`] from the accepted scene admission boundary.
pub fn mount_rich_node(
    scene: &mut Scene,
    mount: &RichMount,
    node: &UiRichNode,
) -> Result<(), SceneBridgeError> {
    let block = rich_block(mount, node)?;
    scene.insert(block)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_rich::scene::{SceneNode, StyledSpan};

    fn text(value: &str) -> UiRichNode {
        UiRichNode::Text {
            text: value.to_string(),
            bold: false,
        }
    }

    fn mount_of(id: u64) -> RichMount {
        RichMount {
            id: BlockId(id),
            anchor: BlockAnchor::Grid {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 0,
            },
            scroll: ScrollBehavior::Inline,
            owner: 1,
            generation: 1,
            created_at: id,
        }
    }

    fn mount(scene: &mut Scene, id: u64, node: &UiRichNode) -> Result<(), SceneBridgeError> {
        mount_rich_node(scene, &mount_of(id), node)
    }

    #[test]
    fn every_rich_kind_maps_to_its_scene_node() {
        let cases = [
            (
                UiRichNode::Text {
                    text: "x".to_string(),
                    bold: true,
                },
                "text",
            ),
            (UiRichNode::Row(vec![text("a")]), "row"),
            (UiRichNode::Column(vec![text("a")]), "column"),
            (UiRichNode::List(vec![text("a")]), "column"),
            (
                UiRichNode::Block {
                    border: 1,
                    child: Box::new(text("a")),
                },
                "block",
            ),
            (UiRichNode::Image { placement: 4 }, "image"),
            (
                UiRichNode::CodeBlock {
                    lang: Some("rust".to_string()),
                    content: "fn main() {}".to_string(),
                },
                "codeblock",
            ),
            (
                UiRichNode::Table {
                    rows: vec![vec!["a".to_string()]],
                },
                "table",
            ),
            (UiRichNode::Rule, "rule"),
        ];
        for (node, expected) in cases {
            let mapped = scene_node_of(&node);
            let kind = match &mapped {
                SceneNode::Text(_) => "text",
                SceneNode::Row(_) => "row",
                SceneNode::Column(_) => "column",
                SceneNode::Block { .. } => "block",
                SceneNode::Image(_) => "image",
                SceneNode::CodeBlock(_) => "codeblock",
                SceneNode::Table(_) => "table",
                SceneNode::List(_) => "list",
                SceneNode::Rule => "rule",
                SceneNode::Unknown(_) => "unknown",
            };
            assert_eq!(kind, expected);
        }
        // Bold survives; italic is never synthesized on the rich surface.
        match scene_node_of(&UiRichNode::Text {
            text: "x".to_string(),
            bold: true,
        }) {
            SceneNode::Text(StyledSpan { bold, italic, .. }) => {
                assert!(bold);
                assert!(!italic);
            }
            other => panic!("unexpected node {other:?}"),
        }
    }

    #[test]
    fn borderless_block_maps_to_no_border() {
        let node = UiRichNode::Block {
            border: 0,
            child: Box::new(text("x")),
        };
        match scene_node_of(&node) {
            SceneNode::Block { border, .. } => assert!(border.is_none()),
            other => panic!("unexpected node {other:?}"),
        }
    }

    #[test]
    fn mount_enforces_scene_admission() {
        let mut scene = Scene::new();
        let mut node = text("leaf");
        for _ in 0..16 {
            node = UiRichNode::Column(vec![node]);
        }
        // 17 levels is inside the 32-level scene cap: admitted.
        mount(&mut scene, 1, &node).expect("within SCN-2");
        assert_eq!(scene.len(), 1);

        // A table whose structure exceeds SCN-1 is refused by the accepted
        // admission boundary; the bridge does not bypass it.
        let huge = UiRichNode::Table {
            rows: (0..2048).map(|_| Vec::new()).collect(),
        };
        let error = mount(&mut scene, 2, &huge).expect_err("refused");
        assert!(matches!(
            error,
            SceneBridgeError::Admission(SceneError::NodesTooMany { .. })
        ));
        assert_eq!(scene.len(), 1, "last good scene retained");
    }

    #[test]
    fn mount_keeps_block_identity_and_provenance() {
        let mut scene = Scene::new();
        mount_rich_node(
            &mut scene,
            &RichMount {
                id: BlockId(9),
                anchor: BlockAnchor::Line(3),
                scroll: ScrollBehavior::PinnedBelow,
                owner: 7,
                generation: 5,
                created_at: 11,
            },
            &text("hello"),
        )
        .expect("admitted");
        let block = scene.get(BlockId(9)).expect("retained");
        assert_eq!(block.owner(), 7);
        assert_eq!(block.generation(), 5);
        assert_eq!(block.created_at(), 11);
        assert_eq!(block.anchor(), &BlockAnchor::Line(3));
        assert_eq!(block.scroll(), ScrollBehavior::PinnedBelow);
        assert_eq!(block.text_bytes(), 5);
    }
}
