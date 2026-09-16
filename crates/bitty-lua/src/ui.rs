//! Plugin API v1 declarative UI scenes for `bitty.ui.mount` / `bitty.ui.update`
//! (accepted: ADR-0009 `LUA-OQ-7`, Plugin API v1 Lua Surface RFC).
//!
//! The Lua surface accepts only the reduced v1 node vocabulary — `Text`, `Row`,
//! `Column`, and `List` — shaped as declarative tables. `Image`, `CodeBlock`,
//! `Table`, `Rule`, and bordered `Block` nodes are excluded from v1. This
//! module owns the shape validation and the bounded `UiNode` representation
//! that crosses the host bridge.
//!
//! # Budgets
//!
//! | ID     | Dimension                          | Bound  | Source |
//! |--------|------------------------------------|--------|--------|
//! | SCN-1  | nodes per block                    | 2048   | accepted Rich Presentation RFC |
//! | SCN-2  | tree depth per block               | 32     | accepted Rich Presentation RFC |
//! | SCN-3  | text bytes per block               | 256 KiB| accepted Rich Presentation RFC |
//! | SCN-4  | aggregated text bytes per terminal | 2 MiB  | accepted Rich Presentation RFC |
//! | SCN-5  | blocks per terminal                | 64     | accepted Rich Presentation RFC |
//! | v1     | Lua bridge depth                   | 16     | CTX-0428 (stricter bridge budget) |
//!
//! `UI_MAX_DEPTH = 16` is a deliberate, stricter bridge-side budget than the
//! accepted `SCN-2 = 32`: it matches the Plugin SDK reference model
//! (`UI_MAX_DEPTH` in the cross-repository conformance mock host), keeps the
//! bounded value conversion inside the RC-1 host-call slice, and stays under
//! the scene-level bound the presentation layer enforces for host-built
//! scenes. A component deeper than 16 fails closed with
//! `E_UI_COMPONENT_INVALID`; nothing is mounted.
//!
//! `SCN-4`/`SCN-5` are accepted per terminal (all plugins); this host-bridge
//! slice enforces the same numbers per plugin generation, which is a strict
//! subset of the terminal-wide budget. The terminal-wide aggregation across
//! generations is owned by the host composer and is not part of this slice.

use piccolo::Value;

use crate::host::{BridgeError, LuaValue, MarshallingLimits};

/// Accepted closed slot set for `bitty.ui.mount` (ADR-0009 `LUA-OQ-7`).
pub const UI_SLOTS: [&str; 8] = [
    "terminal",
    "top",
    "bottom",
    "left",
    "right",
    "tabline",
    "statusline",
    "overlay",
];

/// v1 declarative node kinds accepted by the Lua bridge.
pub const UI_V1_NODE_KINDS: [&str; 4] = ["Text", "Row", "Column", "List"];

/// `SceneNode` variants excluded from v1 (typed rejection).
pub const UI_V1_EXCLUDED_NODE_KINDS: [&str; 5] = ["Block", "Image", "CodeBlock", "Table", "Rule"];

/// Maximum Lua-v1 scene depth (`CTX-0428`; stricter than accepted `SCN-2`).
pub const UI_MAX_DEPTH: usize = 16;

/// Maximum nodes per mounted component (`SCN-1`).
pub const UI_MAX_NODES: usize = 2048;

/// Maximum text bytes per mounted component (`SCN-3`).
pub const UI_MAX_TEXT_BYTES: usize = 256 * 1024;

/// Maximum blocks retained per plugin generation (`SCN-5` per terminal,
/// enforced here per generation as a strict subset).
pub const UI_MAX_BLOCKS: usize = 64;

/// Maximum aggregated text bytes per plugin generation (`SCN-4` per terminal,
/// enforced here per generation as a strict subset).
pub const UI_MAX_AGGREGATED_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Marshalling bounds for one raw component value, evaluated before shape
/// validation. Generous enough for the v1 vocabulary (the scene-node walk
/// enforces the exact `SCN-1`/`SCN-3` numbers) and tight enough that a
/// hostile table cannot make the conversion unbounded: a cyclic or deep table
/// fails closed with `E_UI_COMPONENT_INVALID` before any host call.
pub(crate) const UI_MARSHAL_LIMITS: MarshallingLimits = MarshallingLimits {
    max_depth: 2 * UI_MAX_DEPTH + 4,
    max_nodes: 8 * UI_MAX_NODES,
    max_bytes: UI_MAX_TEXT_BYTES + 64 * 1024,
};

/// One validated v1 declarative node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiNode {
    /// Plain text leaf.
    Text {
        /// Bounded text content (`SCN-3`).
        text: String,
    },
    /// Horizontal composition of children.
    Row {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
    },
    /// Vertical composition of children.
    Column {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
    },
    /// List composition of children.
    List {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
    },
}

impl UiNode {
    /// A text leaf.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// A `Row` node.
    #[must_use]
    pub fn row(children: Vec<UiNode>) -> Self {
        Self::Row { children }
    }

    /// A `Column` node.
    #[must_use]
    pub fn column(children: Vec<UiNode>) -> Self {
        Self::Column { children }
    }

    /// A `List` node.
    #[must_use]
    pub fn list(children: Vec<UiNode>) -> Self {
        Self::List { children }
    }

    /// v1 kind name.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "Text",
            Self::Row { .. } => "Row",
            Self::Column { .. } => "Column",
            Self::List { .. } => "List",
        }
    }

    /// Node count of the subtree (self included, `SCN-1`).
    #[must_use]
    pub fn count_nodes(&self) -> usize {
        match self {
            Self::Text { .. } => 1,
            Self::Row { children } | Self::Column { children } | Self::List { children } => {
                1 + children.iter().map(Self::count_nodes).sum::<usize>()
            }
        }
    }

    /// Maximum depth of the subtree (leaf = 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Text { .. } => 1,
            Self::Row { children } | Self::Column { children } | Self::List { children } => {
                if children.is_empty() {
                    1
                } else {
                    1 + children.iter().map(Self::depth).max().unwrap_or(0)
                }
            }
        }
    }

    /// Text bytes in the subtree (`SCN-3`).
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Row { children } | Self::Column { children } | Self::List { children } => {
                children.iter().map(Self::text_bytes).sum()
            }
        }
    }

    /// Validate an already-marshalled bounded value against the v1 contract.
    ///
    /// # Errors
    ///
    /// [`BridgeError`] with class `validation` and code
    /// `E_UI_COMPONENT_INVALID` when the value is not a v1 node table, carries
    /// an unknown or excluded kind, nests deeper than [`UI_MAX_DEPTH`], or
    /// exceeds [`UI_MAX_NODES`] / [`UI_MAX_TEXT_BYTES`].
    pub fn from_lua_value(value: &LuaValue) -> Result<Self, BridgeError> {
        let node = Self::from_value(value, 1)?;
        let nodes = node.count_nodes();
        if nodes > UI_MAX_NODES {
            return Err(component_invalid(format!(
                "component node count exceeds {UI_MAX_NODES}"
            )));
        }
        let bytes = node.text_bytes();
        if bytes > UI_MAX_TEXT_BYTES {
            return Err(component_invalid(format!(
                "component text exceeds {UI_MAX_TEXT_BYTES} bytes"
            )));
        }
        Ok(node)
    }

    fn from_value(value: &LuaValue, depth: usize) -> Result<Self, BridgeError> {
        if depth > UI_MAX_DEPTH {
            return Err(component_invalid(format!(
                "component depth exceeds {UI_MAX_DEPTH}"
            )));
        }
        if !matches!(value, LuaValue::Table(_)) {
            return Err(component_invalid("component must be a table"));
        }
        let kind = match value.get("kind") {
            Some(LuaValue::String(kind)) => kind.as_str(),
            _ => return Err(component_invalid("component.kind must be a string")),
        };
        if UI_V1_EXCLUDED_NODE_KINDS.contains(&kind) {
            return Err(component_invalid(format!(
                "node kind '{kind}' is excluded from Plugin API v1"
            )));
        }
        match kind {
            "Text" => match value.get("text") {
                Some(LuaValue::String(text)) => Ok(Self::Text { text: text.clone() }),
                _ => Err(component_invalid(
                    "Text components require a string text field",
                )),
            },
            "Row" | "Column" | "List" => {
                let Some(children) = value.get("children") else {
                    return Err(component_invalid(format!(
                        "{kind} components require a children array"
                    )));
                };
                let nodes = Self::children_array(children, depth)?;
                Ok(match kind {
                    "Row" => Self::Row { children: nodes },
                    "Column" => Self::Column { children: nodes },
                    _ => Self::List { children: nodes },
                })
            }
            _ => Err(component_invalid(format!("unknown node kind '{kind}'"))),
        }
    }

    fn children_array(value: &LuaValue, depth: usize) -> Result<Vec<Self>, BridgeError> {
        let LuaValue::Table(pairs) = value else {
            return Err(component_invalid(
                "component children must be an array table",
            ));
        };
        let mut indexed: Vec<(i64, &LuaValue)> = Vec::with_capacity(pairs.len());
        for (key, child) in pairs {
            match key {
                LuaValue::Integer(index) => indexed.push((*index, child)),
                // A mixed table (array entries plus named fields) or any other
                // key shape is malformed input, not an array; fail closed
                // rather than silently dropping the named entries.
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
            nodes.push(Self::from_value(child, depth + 1)?);
        }
        Ok(nodes)
    }
}

/// Whether `slot` is a member of the accepted closed slot set.
#[must_use]
pub fn is_ui_slot(slot: &str) -> bool {
    UI_SLOTS.contains(&slot)
}

/// Typed `E_UI_COMPONENT_INVALID` diagnostic (accepted ui.mount/update code).
#[must_use]
pub fn component_invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::new("validation", "E_UI_COMPONENT_INVALID", message)
}

/// Read one raw Lua component value into the bounded v1 scene model.
///
/// Marshalling failures (depth, node, byte ceilings, and non-data values such
/// as functions or cyclic tables) are surfaced as
/// `E_UI_COMPONENT_INVALID`, the only component-error code the accepted
/// `bitty.ui.mount` / `bitty.ui.update` surface fixes.
///
/// # Errors
///
/// [`BridgeError`] with class `validation` and code
/// `E_UI_COMPONENT_INVALID` when the value cannot be marshalled or violates
/// the v1 scene contract.
pub fn read_component(value: Value<'_>) -> Result<UiNode, BridgeError> {
    let marshalled = LuaValue::from_lua(value, UI_MARSHAL_LIMITS)
        .map_err(|error| component_invalid(error.message))?;
    UiNode::from_lua_value(&marshalled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> LuaValue {
        LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String(value.to_string())),
        ])
    }

    fn row(children: Vec<LuaValue>) -> LuaValue {
        LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            ("children", LuaValue::array(children)),
        ])
    }

    #[test]
    fn accepts_v1_vocabulary() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Column".to_string())),
            (
                "children",
                LuaValue::array(vec![
                    row(vec![text("a"), text("b")]),
                    LuaValue::table([
                        ("kind", LuaValue::String("List".to_string())),
                        ("children", LuaValue::array(vec![text("item")])),
                    ]),
                ]),
            ),
        ]);
        let node = UiNode::from_lua_value(&value).expect("valid v1 component");
        assert_eq!(node.kind(), "Column");
        assert_eq!(node.count_nodes(), 6);
        assert_eq!(node.depth(), 3);
        assert_eq!(node.text_bytes(), 6);
    }

    #[test]
    fn rejects_excluded_and_unknown_kinds() {
        for kind in UI_V1_EXCLUDED_NODE_KINDS {
            let value = LuaValue::table([("kind", LuaValue::String(kind.to_string()))]);
            let error = UiNode::from_lua_value(&value).expect_err("excluded kind");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        }
        let value = LuaValue::table([("kind", LuaValue::String("Sparkle".to_string()))]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("unknown kind")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    #[test]
    fn depth_sixteen_accepted_seventeen_rejected() {
        let mut value = text("leaf");
        for _ in 0..(UI_MAX_DEPTH - 1) {
            value = row(vec![value]);
        }
        let node = UiNode::from_lua_value(&value).expect("depth 16 accepted");
        assert_eq!(node.depth(), UI_MAX_DEPTH);
        let deeper = row(vec![value]);
        let error = UiNode::from_lua_value(&deeper).expect_err("depth 17 rejected");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
    }

    #[test]
    fn node_and_text_budgets_are_enforced() {
        let many = row((0..=UI_MAX_NODES).map(|_| text("")).collect());
        assert_eq!(
            UiNode::from_lua_value(&many).expect_err("node cap").code,
            "E_UI_COMPONENT_INVALID"
        );
        let big = text(&"x".repeat(UI_MAX_TEXT_BYTES + 1));
        assert_eq!(
            UiNode::from_lua_value(&big).expect_err("text cap").code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    #[test]
    fn sparse_children_rejected_as_dense_array() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            (
                "children",
                LuaValue::Table(vec![(LuaValue::Integer(2), text("gap"))]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("sparse children")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    #[test]
    fn mixed_children_keys_rejected_as_dense_array() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Row".to_string())),
            (
                "children",
                LuaValue::Table(vec![
                    (LuaValue::Integer(1), text("dense")),
                    (LuaValue::String("note".to_string()), text("named")),
                ]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("mixed children keys")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
    }
}
