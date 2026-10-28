//! Rich declarative scene nodes for the `ui.rich` surface (CTX-1628).
//!
//! The accepted [Rich Presentation RFC](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/rich-presentation-rfc.md)
//! fixes the `SceneNode` gallery (text, rows, columns, blocks, images, code
//! blocks, tables, lists, rules) and its `SCN-1..5` bounds. The band surface
//! (`crate::ui::UiNode`) stays the closed v1 vocabulary; this module is the
//! **rich** vocabulary a `ui.rich` mount may use, so a plugin that owns rich
//! content never has to express it as a character grid.
//!
//! # Shape
//!
//! Every node is a declarative table with a `kind` field, exactly like the v1
//! band nodes. Content fields are additive per kind:
//!
//! | Kind | Fields | Layout analogue |
//! |---|---|---|
//! | `Text` | `text`, optional `bold` | `SceneNode::Text` |
//! | `Row` | `children` | `SceneNode::Row` |
//! | `Column` | `children` | `SceneNode::Column` |
//! | `List` | `children` | `SceneNode::Column` (the v1 list container composes vertically) |
//! | `Block` | `child`, optional `border = { width = 0..=8 }` | `SceneNode::Block` |
//! | `CodeBlock` | optional `lang`, `content` | `SceneNode::CodeBlock` |
//! | `Table` | `rows` (array of string arrays) | `SceneNode::Table` |
//! | `Image` | `placement` (positive integer placement id) | `SceneNode::Image` |
//! | `Rule` | none | `SceneNode::Rule` |
//!
//! Presentation fields the accepted `SceneNode` schema cannot express are
//! **rejected**, never silently dropped: `fg`, `bg`, `anchor`, `on_click`,
//! `on_scroll`, and `on_hover` belong to the band surface, and a rich node
//! carrying one fails with `E_UI_COMPONENT_INVALID`. `bold` is accepted on
//! `Text` because `StyledSpan` carries it.
//!
//! # Bounds
//!
//! | Budget | Cap | Source |
//! |---|---|---|
//! | nodes per scene | [`UI_MAX_NODES`] (2048) | `SCN-1` |
//! | tree depth | [`UI_MAX_DEPTH`] (16) | bridge budget, stricter than `SCN-2` |
//! | text bytes per scene | [`UI_MAX_TEXT_BYTES`] (256 KiB) | `SCN-3` |
//! | aggregated scene bytes (all blocks) | 2 MiB | `SCN-4`, enforced at `Scene::insert` |
//! | table rows | [`UI_RICH_MAX_TABLE_ROWS`] (256) | paint budget |
//! | table cells | [`UI_RICH_MAX_TABLE_CELLS`] (4096) | paint budget |
//! | border width | [`UI_RICH_MAX_BORDER_CELLS`] (8) | paint budget |
//! | language hint | [`UI_RICH_MAX_LANG_BYTES`] (32) | bridge budget |
//!
//! Structural elements count against the node budget exactly like the
//! accepted scene accounting: a table row and a table cell are separate
//! allocations, so a table of empty cells still presses against `SCN-1`
//! (`SEC-18`/`R-021`).
//!
//! Nothing here allocates beyond the parsed value, executes a callback, or
//! reaches a renderer, GPU, filesystem, or terminal state. The per-scene
//! `SCN-4` aggregate is a property of the composed `Scene`, not of one node,
//! so it stays with the scene admission boundary.

#![forbid(unsafe_code)]

use phodopus::Value;

use crate::host::{BridgeError, LuaValue};
use crate::ui::{
    UI_MARSHAL_LIMITS, UI_MAX_DEPTH, UI_MAX_NODES, UI_MAX_TEXT_BYTES, component_invalid,
};

/// Maximum table rows accepted by one `Table` node.
pub const UI_RICH_MAX_TABLE_ROWS: usize = 256;

/// Maximum table cells accepted by one `Table` node.
pub const UI_RICH_MAX_TABLE_CELLS: usize = 4096;

/// Maximum block border width in cells.
///
/// Matches the render paint cap (`SCENE_PAINT_MAX_BORDER_CELLS`); a wider
/// declaration is rejected here instead of being refused later at paint time.
pub const UI_RICH_MAX_BORDER_CELLS: u16 = 8;

/// Maximum bytes in a code-block language hint.
pub const UI_RICH_MAX_LANG_BYTES: usize = 32;

/// Closed rich node kind set, in the accepted `SceneNode` declaration order.
pub const UI_RICH_NODE_KINDS: [&str; 9] = [
    "Text",
    "Row",
    "Column",
    "Block",
    "Image",
    "CodeBlock",
    "Table",
    "List",
    "Rule",
];

/// One validated rich declarative node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRichNode {
    /// Plain text leaf.
    Text {
        /// Bounded text content (`SCN-3`).
        text: String,
        /// Bold attribute (`StyledSpan.bold`).
        bold: bool,
    },
    /// Horizontal composition of children.
    Row(Vec<UiRichNode>),
    /// Vertical composition of children.
    Column(Vec<UiRichNode>),
    /// Bordered block around one child.
    Block {
        /// Border width in cells; `0` means no border.
        border: u16,
        /// Child node.
        child: Box<UiRichNode>,
    },
    /// Reference to an admitted image placement.
    Image {
        /// Producer placement id (positive).
        placement: u64,
    },
    /// Code block with an optional language hint.
    CodeBlock {
        /// Language hint (`None` when the producer declared none).
        lang: Option<String>,
        /// Code content (`SCN-3`).
        content: String,
    },
    /// Table of plain-text cells.
    Table {
        /// Rows of cells, in declaration order.
        rows: Vec<Vec<String>>,
    },
    /// Vertical composition declared with the v1 `List` container spelling.
    List(Vec<UiRichNode>),
    /// Horizontal rule.
    Rule,
}

impl UiRichNode {
    /// Kind name in the closed rich vocabulary.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "Text",
            Self::Row(_) => "Row",
            Self::Column(_) => "Column",
            Self::Block { .. } => "Block",
            Self::Image { .. } => "Image",
            Self::CodeBlock { .. } => "CodeBlock",
            Self::Table { .. } => "Table",
            Self::List(_) => "List",
            Self::Rule => "Rule",
        }
    }

    /// Node count of the subtree (`SCN-1`, structural elements included).
    #[must_use]
    pub fn count_nodes(&self) -> usize {
        match self {
            Self::Text { .. } | Self::Image { .. } | Self::Rule | Self::CodeBlock { .. } => 1,
            Self::Table { rows } => 1 + rows.len() + rows.iter().map(Vec::len).sum::<usize>(),
            Self::Row(children) | Self::Column(children) | Self::List(children) => {
                1 + children.iter().map(Self::count_nodes).sum::<usize>()
            }
            Self::Block { child, .. } => 1 + child.count_nodes(),
        }
    }

    /// Maximum depth of the subtree (leaf = 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Text { .. } | Self::Image { .. } | Self::Rule | Self::CodeBlock { .. } => 1,
            Self::Table { .. } => 1,
            Self::Row(children) | Self::Column(children) | Self::List(children) => {
                if children.is_empty() {
                    1
                } else {
                    1 + children.iter().map(Self::depth).max().unwrap_or(0)
                }
            }
            Self::Block { child, .. } => 1 + child.depth(),
        }
    }

    /// Text bytes in the subtree (`SCN-3`).
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.len(),
            Self::Image { .. } | Self::Rule => 0,
            Self::CodeBlock { lang, content } => {
                content.len() + lang.as_ref().map_or(0, String::len)
            }
            Self::Table { rows } => rows.iter().flatten().map(String::len).sum(),
            Self::Row(children) | Self::Column(children) | Self::List(children) => {
                children.iter().map(Self::text_bytes).sum()
            }
            Self::Block { child, .. } => child.text_bytes(),
        }
    }

    /// Every retained string byte in the subtree (`SCN-4` local accounting).
    ///
    /// Rich nodes retain only content strings (no theme tokens, command
    /// bindings, or region ids: those fields are rejected on this surface),
    /// so this equals [`Self::text_bytes`] and exists to keep one accounting
    /// shape across both declarative surfaces.
    #[must_use]
    pub fn retained_string_bytes(&self) -> usize {
        self.text_bytes()
    }

    /// Validate an already-marshalled bounded value against the rich contract.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with class `validation` and code
    /// `E_UI_COMPONENT_INVALID` when the value is not a rich node table,
    /// carries an unknown kind or a field the scene schema cannot express,
    /// nests deeper than [`UI_MAX_DEPTH`], or exceeds [`UI_MAX_NODES`] /
    /// [`UI_MAX_TEXT_BYTES`] or a kind-specific bound.
    pub fn from_lua_value(value: &LuaValue) -> Result<Self, BridgeError> {
        let node = Self::from_value(value, 1)?;
        let nodes = node.count_nodes();
        if nodes > UI_MAX_NODES {
            return Err(component_invalid(format!(
                "rich component node count exceeds {UI_MAX_NODES}"
            )));
        }
        let bytes = node.text_bytes();
        if bytes > UI_MAX_TEXT_BYTES {
            return Err(component_invalid(format!(
                "rich component text exceeds {UI_MAX_TEXT_BYTES} bytes"
            )));
        }
        Ok(node)
    }

    fn from_value(value: &LuaValue, depth: usize) -> Result<Self, BridgeError> {
        if depth > UI_MAX_DEPTH {
            return Err(component_invalid(format!(
                "rich component depth exceeds {UI_MAX_DEPTH}"
            )));
        }
        if !matches!(value, LuaValue::Table(_)) {
            return Err(component_invalid("rich component must be a table"));
        }
        let kind = match value.get("kind") {
            Some(LuaValue::String(kind)) => kind.as_str(),
            _ => return Err(component_invalid("rich component.kind must be a string")),
        };
        reject_band_only_fields(value)?;

        match kind {
            "Text" => {
                let text = string_field(value, "text")?.to_string();
                let bold = match value.get("bold") {
                    Some(LuaValue::Bool(bold)) => *bold,
                    Some(_) => return Err(component_invalid("bold must be a boolean")),
                    None => false,
                };
                Ok(Self::Text { text, bold })
            }
            "Row" | "Column" | "List" => {
                let children = children_array(value, depth)?;
                Ok(match kind {
                    "Row" => Self::Row(children),
                    "Column" => Self::Column(children),
                    _ => Self::List(children),
                })
            }
            "Block" => {
                let child = match value.get("child") {
                    Some(child) => Self::from_value(child, depth + 1)?,
                    None => {
                        return Err(component_invalid("Block components require a child node"));
                    }
                };
                let border = match value.get("border") {
                    None => 0,
                    Some(LuaValue::Table(_)) => border_width(value.get("border"))?,
                    Some(_) => {
                        return Err(component_invalid("Block.border must be a table"));
                    }
                };
                Ok(Self::Block {
                    border,
                    child: Box::new(child),
                })
            }
            "Image" => {
                let placement = match value.get("placement") {
                    Some(LuaValue::Integer(id)) if *id > 0 => *id as u64,
                    Some(_) => {
                        return Err(component_invalid(
                            "Image.placement must be a positive integer placement id",
                        ));
                    }
                    None => {
                        return Err(component_invalid(
                            "Image components require a placement field",
                        ));
                    }
                };
                Ok(Self::Image { placement })
            }
            "CodeBlock" => {
                let content = string_field(value, "content")?.to_string();
                let lang = match value.get("lang") {
                    Some(LuaValue::String(lang)) => {
                        if lang.len() > UI_RICH_MAX_LANG_BYTES {
                            return Err(component_invalid(format!(
                                "CodeBlock.lang exceeds {UI_RICH_MAX_LANG_BYTES} bytes"
                            )));
                        }
                        if lang.is_empty()
                            || !lang.chars().all(|ch| {
                                ch.is_ascii_alphanumeric()
                                    || matches!(ch, '.' | '_' | '+' | '#' | '-')
                            })
                        {
                            return Err(component_invalid(
                                "CodeBlock.lang must be 1-32 bytes of [A-Za-z0-9._+#-]",
                            ));
                        }
                        Some(lang.clone())
                    }
                    Some(_) => return Err(component_invalid("CodeBlock.lang must be a string")),
                    None => None,
                };
                Ok(Self::CodeBlock { lang, content })
            }
            "Table" => {
                let Some(rows_value) = value.get("rows") else {
                    return Err(component_invalid("Table components require a rows array"));
                };
                let rows = table_rows(rows_value)?;
                Ok(Self::Table { rows })
            }
            "Rule" => Ok(Self::Rule),
            other => Err(component_invalid(format!(
                "unknown rich node kind '{other}'"
            ))),
        }
    }
}

/// Reads one raw Lua value into the bounded rich scene model.
///
/// Marshalling failures (depth, node, byte ceilings, non-data values such as
/// functions, and cyclic tables) surface as the same
/// `E_UI_COMPONENT_INVALID` diagnostic the accepted `bitty.ui.mount` /
/// `bitty.ui.update` surface uses. The `ui.rich` capability gate is the
/// caller's: this function is the mechanism and grants no authority.
///
/// # Errors
///
/// [`BridgeError`] with class `validation` and code
/// `E_UI_COMPONENT_INVALID` when the value cannot be marshalled or violates
/// the rich scene contract.
pub fn read_rich_component(value: Value<'_>) -> Result<UiRichNode, BridgeError> {
    let marshalled = LuaValue::from_lua(value, UI_MARSHAL_LIMITS)
        .map_err(|error| component_invalid(error.message))?;
    UiRichNode::from_lua_value(&marshalled)
}

/// Rejects band-surface presentation fields the scene schema cannot express.
fn reject_band_only_fields(value: &LuaValue) -> Result<(), BridgeError> {
    for key in [
        "fg",
        "bg",
        "anchor",
        "on_click",
        "on_scroll",
        "on_hover",
        "region",
    ] {
        if value.get(key).is_some() {
            return Err(component_invalid(format!(
                "field '{key}' is not part of the rich scene surface"
            )));
        }
    }
    Ok(())
}

fn string_field<'a>(value: &'a LuaValue, key: &str) -> Result<&'a str, BridgeError> {
    match value.get(key) {
        Some(LuaValue::String(text)) => Ok(text.as_str()),
        Some(_) => Err(component_invalid(format!("{key} must be a string"))),
        None => Err(component_invalid(format!(
            "component requires a string {key} field"
        ))),
    }
}

fn children_array(value: &LuaValue, depth: usize) -> Result<Vec<UiRichNode>, BridgeError> {
    let Some(children) = value.get("children") else {
        return Err(component_invalid(
            "container components require a children array",
        ));
    };
    let LuaValue::Table(pairs) = children else {
        return Err(component_invalid(
            "component children must be an array table",
        ));
    };
    let mut indexed: Vec<(i64, &LuaValue)> = Vec::with_capacity(pairs.len());
    for (key, child) in pairs {
        match key {
            LuaValue::Integer(index) => indexed.push((*index, child)),
            _ => {
                return Err(component_invalid(
                    "component children must be a dense 1-based array",
                ));
            }
        }
    }
    indexed.sort_by_key(|(index, _)| *index);
    let mut nodes = Vec::with_capacity(indexed.len());
    for (position, (index, child)) in indexed.into_iter().enumerate() {
        if index != position as i64 + 1 {
            return Err(component_invalid(
                "component children must be a dense 1-based array",
            ));
        }
        nodes.push(UiRichNode::from_value(child, depth + 1)?);
    }
    Ok(nodes)
}

fn table_rows(value: &LuaValue) -> Result<Vec<Vec<String>>, BridgeError> {
    let LuaValue::Table(pairs) = value else {
        return Err(component_invalid("Table.rows must be an array table"));
    };
    let mut indexed: Vec<(i64, &LuaValue)> = Vec::with_capacity(pairs.len());
    for (key, row) in pairs {
        match key {
            LuaValue::Integer(index) => indexed.push((*index, row)),
            _ => {
                return Err(component_invalid(
                    "Table.rows must be a dense 1-based array",
                ));
            }
        }
    }
    indexed.sort_by_key(|(index, _)| *index);
    if indexed.len() > UI_RICH_MAX_TABLE_ROWS {
        return Err(component_invalid(format!(
            "Table.rows exceeds {UI_RICH_MAX_TABLE_ROWS} rows"
        )));
    }
    let mut rows = Vec::with_capacity(indexed.len());
    let mut cells = 0usize;
    for (position, (index, row)) in indexed.into_iter().enumerate() {
        if index != position as i64 + 1 {
            return Err(component_invalid(
                "Table.rows must be a dense 1-based array",
            ));
        }
        let LuaValue::Table(cell_pairs) = row else {
            return Err(component_invalid("each Table row must be an array table"));
        };
        let mut row_cells: Vec<(i64, &LuaValue)> = Vec::with_capacity(cell_pairs.len());
        for (key, cell) in cell_pairs {
            match key {
                LuaValue::Integer(index) => row_cells.push((*index, cell)),
                _ => {
                    return Err(component_invalid(
                        "each Table row must be a dense 1-based array",
                    ));
                }
            }
        }
        row_cells.sort_by_key(|(index, _)| *index);
        let mut cells_out = Vec::with_capacity(row_cells.len());
        for (cell_position, (cell_index, cell)) in row_cells.into_iter().enumerate() {
            if cell_index != cell_position as i64 + 1 {
                return Err(component_invalid(
                    "each Table row must be a dense 1-based array",
                ));
            }
            match cell {
                LuaValue::String(text) => {
                    cells = cells.saturating_add(1);
                    cells_out.push(text.clone());
                }
                _ => return Err(component_invalid("Table cells must be strings")),
            }
        }
        rows.push(cells_out);
    }
    if cells > UI_RICH_MAX_TABLE_CELLS {
        return Err(component_invalid(format!(
            "Table cells exceed {UI_RICH_MAX_TABLE_CELLS}"
        )));
    }
    Ok(rows)
}

fn border_width(value: Option<&LuaValue>) -> Result<u16, BridgeError> {
    let Some(table) = value else {
        return Ok(0);
    };
    let LuaValue::Table(pairs) = table else {
        return Err(component_invalid("Block.border must be a table"));
    };
    for (key, _) in pairs {
        match key {
            LuaValue::String(name) if name == "width" => {}
            LuaValue::String(name) => {
                return Err(component_invalid(format!(
                    "field '{name}' is not part of the rich block border"
                )));
            }
            _ => {
                return Err(component_invalid("Block.border must use width = <integer>"));
            }
        }
    }
    let width = match table.get("width") {
        Some(LuaValue::Integer(width)) if *width >= 0 => u16::try_from(*width)
            .map_err(|_| component_invalid("Block.border.width is out of range"))?,
        Some(_) => {
            return Err(component_invalid(
                "Block.border.width must be a non-negative integer",
            ));
        }
        None => {
            return Err(component_invalid("Block.border requires a width field"));
        }
    };
    if width > UI_RICH_MAX_BORDER_CELLS {
        return Err(component_invalid(format!(
            "Block.border.width exceeds {UI_RICH_MAX_BORDER_CELLS} cells"
        )));
    }
    Ok(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UI_MAX_AGGREGATED_TEXT_BYTES;

    fn text(value: &str) -> LuaValue {
        LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String(value.to_string())),
        ])
    }

    #[test]
    fn v1_kinds_parse_on_the_rich_surface() {
        let node = UiRichNode::from_lua_value(&LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            ("children", LuaValue::array(vec![text("a"), text("b")])),
        ]))
        .expect("row parses");
        assert_eq!(node.kind(), "Row");
        assert_eq!(node.count_nodes(), 3);
        assert_eq!(node.depth(), 2);
        assert_eq!(node.text_bytes(), 2);
    }

    #[test]
    fn every_rich_kind_parses_and_measures() {
        let cases: Vec<(LuaValue, &str, usize, usize)> = vec![
            (text("hello"), "Text", 1, 5),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("Column".to_string())),
                    ("children", LuaValue::array(vec![text("x")])),
                ]),
                "Column",
                2,
                1,
            ),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("List".to_string())),
                    ("children", LuaValue::array(vec![text("x")])),
                ]),
                "List",
                2,
                1,
            ),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("Block".to_string())),
                    ("border", LuaValue::table([("width", LuaValue::Integer(1))])),
                    ("child", text("inside")),
                ]),
                "Block",
                2,
                6,
            ),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("Image".to_string())),
                    ("placement", LuaValue::Integer(7)),
                ]),
                "Image",
                1,
                0,
            ),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("CodeBlock".to_string())),
                    ("lang", LuaValue::String("rust".to_string())),
                    ("content", LuaValue::String("fn main() {}".to_string())),
                ]),
                "CodeBlock",
                1,
                16,
            ),
            (
                LuaValue::table([
                    ("kind", LuaValue::String("Table".to_string())),
                    (
                        "rows",
                        LuaValue::array(vec![LuaValue::array(vec![
                            LuaValue::String("a".to_string()),
                            LuaValue::String("b".to_string()),
                        ])]),
                    ),
                ]),
                "Table",
                4,
                2,
            ),
            (
                LuaValue::table([("kind", LuaValue::String("Rule".to_string()))]),
                "Rule",
                1,
                0,
            ),
        ];
        for (value, kind, nodes, bytes) in cases {
            let node = UiRichNode::from_lua_value(&value).expect("parses");
            assert_eq!(node.kind(), kind);
            assert_eq!(node.count_nodes(), nodes, "{kind} node count");
            assert_eq!(node.text_bytes(), bytes, "{kind} text bytes");
            assert_eq!(node.retained_string_bytes(), bytes);
        }
    }

    #[test]
    fn band_only_fields_are_rejected_not_dropped() {
        for key in ["fg", "bg", "anchor", "on_click", "on_scroll", "on_hover"] {
            let value = LuaValue::table([
                ("kind", LuaValue::String("Text".to_string())),
                ("text", LuaValue::String("x".to_string())),
                (key, LuaValue::String("ignored".to_string())),
            ]);
            let error = UiRichNode::from_lua_value(&value).expect_err("rejected");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        }
    }

    #[test]
    fn bounds_fail_closed() {
        // Unknown kind.
        assert!(
            UiRichNode::from_lua_value(&LuaValue::table([(
                "kind",
                LuaValue::String("Canvas".to_string())
            )]))
            .is_err()
        );
        // Depth.
        let mut value = text("leaf");
        for _ in 0..=UI_MAX_DEPTH {
            value = LuaValue::table([
                ("kind", LuaValue::String("Column".to_string())),
                ("children", LuaValue::array(vec![value])),
            ]);
        }
        assert!(UiRichNode::from_lua_value(&value).is_err());
        // Nodes.
        let wide = LuaValue::array((0..=UI_MAX_NODES).map(|_| text("")).collect());
        let value = LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            ("children", wide),
        ]);
        assert!(UiRichNode::from_lua_value(&value).is_err());
        // Text bytes.
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("x".repeat(UI_MAX_TEXT_BYTES + 1))),
        ]);
        assert!(UiRichNode::from_lua_value(&value).is_err());
    }

    #[test]
    fn table_and_border_bounds_fail_closed() {
        let rows = (0..=UI_RICH_MAX_TABLE_ROWS)
            .map(|_| LuaValue::array(Vec::new()))
            .collect();
        let value = LuaValue::table([
            ("kind", LuaValue::String("Table".to_string())),
            ("rows", LuaValue::array(rows)),
        ]);
        assert!(UiRichNode::from_lua_value(&value).is_err());

        let wide_border = LuaValue::table([
            ("kind", LuaValue::String("Block".to_string())),
            (
                "border",
                LuaValue::table([(
                    "width",
                    LuaValue::Integer(i64::from(UI_RICH_MAX_BORDER_CELLS) + 1),
                )]),
            ),
            ("child", text("x")),
        ]);
        assert!(UiRichNode::from_lua_value(&wide_border).is_err());

        let border_colour = LuaValue::table([
            ("kind", LuaValue::String("Block".to_string())),
            (
                "border",
                LuaValue::table([
                    ("width", LuaValue::Integer(1)),
                    ("color", LuaValue::String("#ff0000".to_string())),
                ]),
            ),
            ("child", text("x")),
        ]);
        assert!(
            UiRichNode::from_lua_value(&border_colour).is_err(),
            "Core owns the border colour; a raw colour is refused"
        );
    }

    #[test]
    fn image_requires_a_positive_placement() {
        for placement in [
            LuaValue::Integer(0),
            LuaValue::Integer(-1),
            LuaValue::String("7".to_string()),
        ] {
            let value = LuaValue::table([
                ("kind", LuaValue::String("Image".to_string())),
                ("placement", placement),
            ]);
            assert!(UiRichNode::from_lua_value(&value).is_err());
        }
    }

    #[test]
    fn aggregated_accounting_matches_text_bytes() {
        let node = UiRichNode::from_lua_value(&LuaValue::table([
            ("kind", LuaValue::String("Table".to_string())),
            (
                "rows",
                LuaValue::array(vec![LuaValue::array(vec![LuaValue::String(
                    "cell".to_string(),
                )])]),
            ),
        ]))
        .expect("parses");
        assert!(node.retained_string_bytes() <= UI_MAX_AGGREGATED_TEXT_BYTES);
        assert_eq!(node.retained_string_bytes(), 4);
    }
}
