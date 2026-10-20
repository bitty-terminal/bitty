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
//!
//! The local `SCN-4` aggregate charges every retained string in a mounted
//! subtree, not only `Text` content: theme tokens, click/scroll command
//! identifiers, argument keys, and string argument values are all retained
//! host-side and count against the same 2 MiB number (CTX-1340). This is a
//! strict superset of the old text-only measurement and invents no new
//! ceiling.

use phodopus::Value;

use crate::host::{BridgeError, LuaValue, MarshallingLimits};

/// Accepted closed slot set for `bitty.ui.mount` (ADR-0009 `LUA-OQ-7`,
/// frozen for v1 by OQ-056).
///
/// This enum is the single source of truth for slot names: the Lua bridge,
/// the host `ui_mount` gate, and the host band routing all parse through
/// [`UiSlot::parse`], so validation and rendering cannot disagree about which
/// names exist. Whether a host actually presents a slot is host policy (see
/// `bitty_runtime::ui_slot_placement`), never a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UiSlot {
    /// `terminal` — content attached to a terminal view.
    Terminal,
    /// `top` — top edge band.
    Top,
    /// `bottom` — bottom edge band.
    Bottom,
    /// `left` — left edge band slot (no vertical band host yet; the mount
    /// gate fails closed with `E_UI_UNAVAILABLE`).
    Left,
    /// `right` — right edge band slot (no vertical band host yet; the mount
    /// gate fails closed with `E_UI_UNAVAILABLE`).
    Right,
    /// `tabline` — exclusive-claim tab strip (reserved for panel tabs).
    Tabline,
    /// `overlay` — Core-hosted focusable overlay (needs `ui.overlay`).
    ///
    /// The v1 slot was presentation-only; the Core host now retains the block
    /// as a focusable-overlay surface that can own the transient input capture
    /// through `bitty.ui.overlay.*` (CTX-0941, OQ-056 v2 scope).
    Overlay,
}

impl UiSlot {
    /// Every accepted slot, in the canonical declaration order.
    pub const ALL: [Self; 7] = [
        Self::Terminal,
        Self::Top,
        Self::Bottom,
        Self::Left,
        Self::Right,
        Self::Tabline,
        Self::Overlay,
    ];

    /// Parse a Lua slot name; `None` for any name outside the closed set
    /// (exact, case-sensitive match — no aliases).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.as_str() == name)
    }

    /// Canonical Lua spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
            Self::Tabline => "tabline",
            Self::Overlay => "overlay",
        }
    }
}

impl std::fmt::Display for UiSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Accepted closed slot set for `bitty.ui.mount` as strings, in
/// [`UiSlot::ALL`] order (kept for callers that list names).
pub const UI_SLOTS: [&str; 7] = [
    UiSlot::Terminal.as_str(),
    UiSlot::Top.as_str(),
    UiSlot::Bottom.as_str(),
    UiSlot::Left.as_str(),
    UiSlot::Right.as_str(),
    UiSlot::Tabline.as_str(),
    UiSlot::Overlay.as_str(),
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

/// Maximum aggregated retained string bytes per plugin generation (`SCN-4`
/// per terminal, enforced here per generation as a strict subset).
///
/// The name keeps the accepted `SCN-4` "text" dimension, but the local
/// accounting charges every retained string (see
/// [`UiNode::retained_string_bytes`]); the 2 MiB number is unchanged.
pub const UI_MAX_AGGREGATED_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Maximum click command identifier length (CTX-0890).
pub const UI_CLICK_MAX_COMMAND_BYTES: usize = 64;

/// Maximum click command arguments count (CTX-0890).
pub const UI_CLICK_MAX_ARGS: usize = 16;

/// Maximum click command argument value length (CTX-0890).
pub const UI_CLICK_MAX_ARG_BYTES: usize = 256;

/// Maximum click/scroll command argument key length (CTX-1340).
///
/// Argument keys are identifiers on the same command surface as the command
/// identifier, so they reuse [`UI_CLICK_MAX_COMMAND_BYTES`] instead of being
/// uncapped: this introduces no new numeric ceiling. Values keep their
/// separate [`UI_CLICK_MAX_ARG_BYTES`] bound, and both bounds feed the existing
/// aggregate rather than adding a dimension to it.
pub const UI_CLICK_MAX_ARG_KEY_BYTES: usize = UI_CLICK_MAX_COMMAND_BYTES;

/// Maximum theme token name length (CTX-0890).
pub const UI_MAX_THEME_TOKEN_BYTES: usize = 32;

/// Maximum declared hit-region id length (issue #2248).
///
/// Region ids are identifiers on the same command surface as command
/// identifiers and argument keys, so they reuse
/// [`UI_CLICK_MAX_COMMAND_BYTES`] instead of introducing a new ceiling. The
/// id feeds the existing per-generation aggregate through
/// [`ClickCommand::retained_string_bytes`].
pub const UI_MAX_REGION_ID_BYTES: usize = UI_CLICK_MAX_COMMAND_BYTES;

/// Argument name the host injects for a declared hit region (issue #2248).
///
/// A click or scroll on a span carrying `region = { id, kind }` dispatches
/// the declared command with the region id under this name. A declared entry
/// of the same name is overwritten, never duplicated.
pub const UI_REGION_ARG: &str = "region";

/// Argument name the host injects for a band click button (issue #2248).
///
/// One of [`UI_BUTTON_LEFT`], [`UI_BUTTON_RIGHT`], or [`UI_BUTTON_MIDDLE`].
/// A declared entry of the same name is overwritten, never duplicated.
pub const UI_BUTTON_ARG: &str = "button";

/// Left mouse button spelling carried under [`UI_BUTTON_ARG`].
pub const UI_BUTTON_LEFT: &str = "left";
/// Right mouse button spelling carried under [`UI_BUTTON_ARG`].
pub const UI_BUTTON_RIGHT: &str = "right";
/// Middle mouse button spelling carried under [`UI_BUTTON_ARG`].
pub const UI_BUTTON_MIDDLE: &str = "middle";

/// Argument name the host injects beside the declared scroll args at
/// dispatch (CTX-1190, issue #2008).
///
/// A wheel over an `on_scroll` node invokes the declared command with the
/// declared args plus `delta`: the host-computed signed line step
/// (direction x lines, positive for wheel-up), so the plugin only ever sees
/// a bounded number. A declared `delta` entry is overwritten, never merged.
pub const UI_SCROLL_DELTA_ARG: &str = "delta";

/// Saturation bound for a coalesced host-computed scroll delta (CTX-1190).
///
/// Wheel bursts sum relative ticks; the sum saturates here so a burst that
/// outlives one poll window can never bank unbounded drift into a single
/// dispatch. Large enough to never clip legitimate gestures.
pub const UI_SCROLL_DELTA_SATURATE: f64 = 4096.0;

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

/// Kind of a declared per-item hit region (issue #2248).
///
/// `Module` covers a whole status module (clock, volume, a window item);
/// `Item` covers one item inside a module (one workspace capsule). The kind
/// is presentation metadata carried back with the dispatch; it grants no
/// authority and selects no code path in Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UiRegionKind {
    /// A whole module row or capsule strip entry.
    Module,
    /// One item inside a module.
    Item,
}

impl UiRegionKind {
    /// Every accepted kind, in canonical declaration order.
    pub const ALL: [Self; 2] = [Self::Module, Self::Item];

    /// Lua spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Item => "item",
        }
    }

    /// Parse the closed kind set; `None` for any other string.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

impl std::fmt::Display for UiRegionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Declared identity of one per-item hit region (issue #2248).
///
/// A region is declared beside a click or scroll binding:
/// `on_click = { command = "...", region = { id = "clock", kind = "module" } }`.
/// Core owns the rectangle: the host derives it from the painted spans, so a
/// plugin never supplies coordinates. The id is bounded and stable for one
/// band version; the kind labels what the item represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiRegion {
    /// Bounded region identifier (1..=[`UI_MAX_REGION_ID_BYTES`] bytes).
    pub id: String,
    /// Region kind (`module` or `item`).
    pub kind: UiRegionKind,
}

/// Click command binding for interactive UiNode elements (CTX-0890).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickCommand {
    /// Command identifier (max 128 bytes).
    pub command: String,
    /// Command arguments (max 8 args, each max 256 bytes; only scalar values).
    pub args: Vec<(String, ClickArg)>,
    /// Optional per-item hit region this binding belongs to (issue #2248).
    ///
    /// A span with a region is an explicit item in the band's emitted region
    /// set; dispatch carries the region id under [`UI_REGION_ARG`]. `None`
    /// keeps the pre-2248 span behaviour (the span is clickable, but no
    /// region identity is published).
    pub region: Option<UiRegion>,
}

/// Click command argument value (CTX-0890).
#[derive(Debug, Clone, PartialEq)]
pub enum ClickArg {
    /// String argument.
    String(String),
    /// Number argument.
    Number(f64),
    /// Boolean argument.
    Bool(bool),
}

impl Eq for ClickArg {}

impl ClickCommand {
    /// Bytes of the retained command identifier, argument keys, and string
    /// argument values. Numeric and boolean argument values retain no string
    /// bytes, but their keys are still charged.
    fn retained_string_bytes(&self) -> usize {
        self.command.len()
            + self.region.as_ref().map_or(0, |region| region.id.len())
            + self
                .args
                .iter()
                .map(|(key, value)| {
                    key.len()
                        + match value {
                            ClickArg::String(text) => text.len(),
                            ClickArg::Number(_) | ClickArg::Bool(_) => 0,
                        }
                })
                .sum::<usize>()
    }
}

/// One validated v1 declarative node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiNode {
    /// Plain text leaf.
    Text {
        /// Bounded text content (`SCN-3`).
        text: String,
        /// Foreground theme token name (e.g. "accent", "surface.2").
        fg: Option<String>,
        /// Background theme token name.
        bg: Option<String>,
        /// Bold attribute.
        bold: Option<bool>,
        /// Click command binding.
        on_click: Option<ClickCommand>,
        /// Scroll command binding (CTX-1190, issue #2008).
        ///
        /// Same [`ClickCommand`] shape as `on_click`: at dispatch the host
        /// merges the declared args with the computed
        /// [`UI_SCROLL_DELTA_ARG`] number (signed lines, positive for
        /// wheel-up), overwriting a declared entry of the same name. No new
        /// authority: the binding invokes a registered command through the
        /// normal dispatch path.
        on_scroll: Option<ClickCommand>,
    },
    /// Horizontal composition of children.
    Row {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
        /// Foreground theme token name (e.g. "accent", "surface.2").
        fg: Option<String>,
        /// Background theme token name.
        bg: Option<String>,
        /// Bold attribute.
        bold: Option<bool>,
        /// Click command binding.
        on_click: Option<ClickCommand>,
        /// Scroll command binding (CTX-1190, issue #2008; see `Text`).
        on_scroll: Option<ClickCommand>,
    },
    /// Vertical composition of children.
    Column {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
        /// Foreground theme token name (e.g. "accent", "surface.2").
        fg: Option<String>,
        /// Background theme token name.
        bg: Option<String>,
        /// Bold attribute.
        bold: Option<bool>,
        /// Click command binding.
        on_click: Option<ClickCommand>,
        /// Scroll command binding (CTX-1190, issue #2008; see `Text`).
        on_scroll: Option<ClickCommand>,
    },
    /// List composition of children.
    List {
        /// Child nodes in declared order.
        children: Vec<UiNode>,
        /// Foreground theme token name (e.g. "accent", "surface.2").
        fg: Option<String>,
        /// Background theme token name.
        bg: Option<String>,
        /// Bold attribute.
        bold: Option<bool>,
        /// Click command binding.
        on_click: Option<ClickCommand>,
        /// Scroll command binding (CTX-1190, issue #2008; see `Text`).
        on_scroll: Option<ClickCommand>,
    },
}

impl UiNode {
    /// A text leaf.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
        }
    }

    /// A `Row` node.
    #[must_use]
    pub fn row(children: Vec<UiNode>) -> Self {
        Self::Row {
            children,
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
        }
    }

    /// A `Column` node.
    #[must_use]
    pub fn column(children: Vec<UiNode>) -> Self {
        Self::Column {
            children,
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
        }
    }

    /// A `List` node.
    #[must_use]
    pub fn list(children: Vec<UiNode>) -> Self {
        Self::List {
            children,
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
        }
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
            Self::Row { children, .. }
            | Self::Column { children, .. }
            | Self::List { children, .. } => {
                1 + children.iter().map(Self::count_nodes).sum::<usize>()
            }
        }
    }

    /// Maximum depth of the subtree (leaf = 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Text { .. } => 1,
            Self::Row { children, .. }
            | Self::Column { children, .. }
            | Self::List { children, .. } => {
                if children.is_empty() {
                    1
                } else {
                    1 + children.iter().map(Self::depth).max().unwrap_or(0)
                }
            }
        }
    }

    /// Text bytes in the subtree (`SCN-3`).
    ///
    /// This stays the per-component text measure; the per-generation aggregate
    /// charges [`Self::retained_string_bytes`] instead.
    #[must_use]
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.len(),
            Self::Row { children, .. }
            | Self::Column { children, .. }
            | Self::List { children, .. } => children.iter().map(Self::text_bytes).sum(),
        }
    }

    /// Bytes of every retained string in the subtree (`SCN-4` local subset).
    ///
    /// Counts `Text` content, `fg`/`bg` theme tokens, command identifiers, and
    /// click/scroll argument keys plus string argument values. This is the
    /// accounting unit for the per-generation 2 MiB aggregate: identifiers are
    /// retained host-side exactly like text, so charging only text would leave
    /// a budget hole (CTX-1340).
    #[must_use]
    pub fn retained_string_bytes(&self) -> usize {
        match self {
            Self::Text {
                text,
                fg,
                bg,
                on_click,
                on_scroll,
                ..
            } => text.len() + retained_style_bytes(fg, bg, on_click, on_scroll),
            Self::Row {
                children,
                fg,
                bg,
                on_click,
                on_scroll,
                ..
            }
            | Self::Column {
                children,
                fg,
                bg,
                on_click,
                on_scroll,
                ..
            }
            | Self::List {
                children,
                fg,
                bg,
                on_click,
                on_scroll,
                ..
            } => {
                children
                    .iter()
                    .map(Self::retained_string_bytes)
                    .sum::<usize>()
                    + retained_style_bytes(fg, bg, on_click, on_scroll)
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

        // Parse style attributes (CTX-0890).
        let fg = extract_theme_token(value, "fg")?;
        let bg = extract_theme_token(value, "bg")?;
        let bold = extract_bool_opt(value, "bold")?;
        let on_click = extract_click_command(value)?;
        let on_scroll = extract_scroll_command(value)?;
        // Issue #2248: one span has one region identity. Both bindings may
        // name it (a clickable scroll item), but they must agree; a mismatch
        // fails closed instead of choosing one silently.
        if let (Some(click), Some(scroll)) = (on_click.as_ref(), on_scroll.as_ref()) {
            if (click.region.is_some() || scroll.region.is_some()) && click.region != scroll.region
            {
                return Err(component_invalid(
                    "on_click.region and on_scroll.region must declare the same region",
                ));
            }
        }

        match kind {
            "Text" => match value.get("text") {
                Some(LuaValue::String(text)) => Ok(Self::Text {
                    text: text.clone(),
                    fg,
                    bg,
                    bold,
                    on_click,
                    on_scroll,
                }),
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
                    "Row" => Self::Row {
                        children: nodes,
                        fg,
                        bg,
                        bold,
                        on_click,
                        on_scroll,
                    },
                    "Column" => Self::Column {
                        children: nodes,
                        fg,
                        bg,
                        bold,
                        on_click,
                        on_scroll,
                    },
                    _ => Self::List {
                        children: nodes,
                        fg,
                        bg,
                        bold,
                        on_click,
                        on_scroll,
                    },
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

/// Bytes of the strings retained by one node's style and bindings (CTX-1340).
fn retained_style_bytes(
    fg: &Option<String>,
    bg: &Option<String>,
    on_click: &Option<ClickCommand>,
    on_scroll: &Option<ClickCommand>,
) -> usize {
    fg.as_ref().map_or(0, String::len)
        + bg.as_ref().map_or(0, String::len)
        + on_click
            .as_ref()
            .map_or(0, ClickCommand::retained_string_bytes)
        + on_scroll
            .as_ref()
            .map_or(0, ClickCommand::retained_string_bytes)
}

/// Whether `slot` is a member of the accepted closed slot set.
#[must_use]
pub fn is_ui_slot(slot: &str) -> bool {
    UiSlot::parse(slot).is_some()
}

/// Validates a theme token name (CTX-0890).
///
/// Valid tokens start with a lowercase letter and contain only `[a-z0-9._-]`,
/// max 32 bytes.
#[must_use]
pub fn is_valid_theme_token(token: &str) -> bool {
    if token.is_empty() || token.len() > UI_MAX_THEME_TOKEN_BYTES {
        return false;
    }
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-'
    })
}

/// Extract optional theme token from a Lua table (CTX-0890).
fn extract_theme_token(value: &LuaValue, key: &str) -> Result<Option<String>, BridgeError> {
    match value.get(key) {
        Some(LuaValue::String(token)) => {
            if !is_valid_theme_token(token) {
                return Err(component_invalid(format!(
                    "invalid theme token '{token}': must start with lowercase letter and contain only [a-z0-9._-], max {} bytes",
                    UI_MAX_THEME_TOKEN_BYTES
                )));
            }
            Ok(Some(token.clone()))
        }
        Some(_) => Err(component_invalid(format!("{key} must be a string"))),
        None => Ok(None),
    }
}

/// Extract optional boolean from a Lua table (CTX-0890).
fn extract_bool_opt(value: &LuaValue, key: &str) -> Result<Option<bool>, BridgeError> {
    match value.get(key) {
        Some(LuaValue::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(component_invalid(format!("{key} must be a boolean"))),
        None => Ok(None),
    }
}

/// Extract optional click command from a Lua table (CTX-0890).
fn extract_click_command(value: &LuaValue) -> Result<Option<ClickCommand>, BridgeError> {
    extract_command_binding(value, "on_click", &[], true)
}

/// Extract optional scroll command from a Lua table (CTX-1190, issue #2008).
///
/// Same [`ClickCommand`] shape as `on_click`. The declared args reserve one
/// slot for the host-computed [`UI_SCROLL_DELTA_ARG`] and, when a region is
/// declared, one more for [`UI_REGION_ARG`]. A table that is already full
/// without naming them fails closed here so dispatch can never exceed
/// [`UI_CLICK_MAX_ARGS`].
fn extract_scroll_command(value: &LuaValue) -> Result<Option<ClickCommand>, BridgeError> {
    extract_command_binding(value, "on_scroll", &[UI_SCROLL_DELTA_ARG], false)
}

/// Extract an optional `{ command, args, region? }` command binding under
/// `key`.
///
/// `base_host_args` names arguments the host always injects at dispatch;
/// `region_click` adds a `button` slot on top of [`UI_REGION_ARG`] when the
/// binding declares a region (region-less bindings keep their pre-2248 arg
/// budget exactly). The declared table reserves one slot per host argument
/// it does not already name (a declared entry is overwritten, so it needs
/// no extra slot). Malformed bindings fail closed with
/// `E_UI_COMPONENT_INVALID`.
fn extract_command_binding(
    value: &LuaValue,
    key: &str,
    base_host_args: &[&str],
    region_click: bool,
) -> Result<Option<ClickCommand>, BridgeError> {
    let Some(binding) = value.get(key) else {
        return Ok(None);
    };
    let LuaValue::Table(_) = binding else {
        return Err(component_invalid(format!("{key} must be a table")));
    };

    let command = match binding.get("command") {
        Some(LuaValue::String(cmd)) => {
            if cmd.is_empty() || cmd.len() > UI_CLICK_MAX_COMMAND_BYTES {
                return Err(component_invalid(format!(
                    "{key}.command must be 1-{UI_CLICK_MAX_COMMAND_BYTES} bytes",
                )));
            }
            cmd.clone()
        }
        Some(_) => return Err(component_invalid(format!("{key}.command must be a string"))),
        None => return Err(component_invalid(format!("{key}.command is required"))),
    };

    let region = extract_region(binding, key)?;
    // The host injects the region and button arguments only when the
    // binding declares a region; legacy bindings keep their pre-2248 arg
    // budget and payload exactly.
    let mut host_args = base_host_args.to_vec();
    if region.is_some() {
        host_args.push(UI_REGION_ARG);
        if region_click {
            host_args.push(UI_BUTTON_ARG);
        }
    }

    let args = match binding.get("args") {
        Some(LuaValue::Table(pairs)) => {
            let undeclared_host_args = host_args
                .iter()
                .filter(|injected| {
                    !pairs.iter().any(|(arg_key, _)| {
                        matches!(arg_key, LuaValue::String(name) if name == *injected)
                    })
                })
                .count();
            let max_args = UI_CLICK_MAX_ARGS.saturating_sub(undeclared_host_args);
            if pairs.len() > max_args {
                return Err(component_invalid(format!(
                    "{key}.args must have at most {max_args} entries",
                )));
            }
            let mut result = Vec::with_capacity(pairs.len());
            for (arg_key, val) in pairs {
                let key_str = match arg_key {
                    LuaValue::String(s) => {
                        if s.len() > UI_CLICK_MAX_ARG_KEY_BYTES {
                            return Err(component_invalid(format!(
                                "{key}.args keys must be at most {UI_CLICK_MAX_ARG_KEY_BYTES} bytes",
                            )));
                        }
                        s.clone()
                    }
                    _ => {
                        return Err(component_invalid(format!(
                            "{key}.args keys must be strings"
                        )));
                    }
                };
                let arg = match val {
                    LuaValue::String(s) => {
                        if s.len() > UI_CLICK_MAX_ARG_BYTES {
                            return Err(component_invalid(format!(
                                "{key}.args string values must be at most {UI_CLICK_MAX_ARG_BYTES} bytes",
                            )));
                        }
                        ClickArg::String(s.clone())
                    }
                    LuaValue::Integer(i) => ClickArg::Number(*i as f64),
                    LuaValue::Number(n) => ClickArg::Number(*n),
                    LuaValue::Bool(b) => ClickArg::Bool(*b),
                    _ => {
                        return Err(component_invalid(format!(
                            "{key}.args values must be string, number, or boolean",
                        )));
                    }
                };
                result.push((key_str, arg));
            }
            result
        }
        Some(_) => return Err(component_invalid(format!("{key}.args must be a table"))),
        None => Vec::new(),
    };

    Ok(Some(ClickCommand {
        command,
        args,
        region,
    }))
}

/// Extract the optional `region = { id, kind? }` identity from one binding
/// (issue #2248).
///
/// `id` is required when the table is present, is bounded by
/// [`UI_MAX_REGION_ID_BYTES`], and uses the lowercase identifier alphabet
/// `[a-z0-9._:-]`. `kind` is optional and defaults to
/// [`UiRegionKind::Module`]; an unknown kind fails closed.
fn extract_region(binding: &LuaValue, key: &str) -> Result<Option<UiRegion>, BridgeError> {
    let Some(region) = binding.get(UI_REGION_ARG) else {
        return Ok(None);
    };
    let LuaValue::Table(_) = region else {
        return Err(component_invalid(format!(
            "{key}.{UI_REGION_ARG} must be a table"
        )));
    };
    let id = match region.get("id") {
        Some(LuaValue::String(id)) if is_valid_region_id(id) => id.clone(),
        Some(LuaValue::String(id)) => {
            return Err(component_invalid(format!(
                "invalid region id '{id}': must be 1-{UI_MAX_REGION_ID_BYTES} bytes of [a-z0-9._:-]"
            )));
        }
        Some(_) => {
            return Err(component_invalid(format!(
                "{key}.{UI_REGION_ARG}.id must be a string"
            )));
        }
        None => {
            return Err(component_invalid(format!(
                "{key}.{UI_REGION_ARG}.id is required"
            )));
        }
    };
    let kind = match region.get("kind") {
        Some(LuaValue::String(kind)) => UiRegionKind::parse(kind).ok_or_else(|| {
            component_invalid(format!(
                "{key}.{UI_REGION_ARG}.kind must be 'module' or 'item'"
            ))
        })?,
        Some(_) => {
            return Err(component_invalid(format!(
                "{key}.{UI_REGION_ARG}.kind must be a string"
            )));
        }
        None => UiRegionKind::Module,
    };
    Ok(Some(UiRegion { id, kind }))
}

/// Whether `id` is a valid bounded region identifier (issue #2248).
///
/// Lowercase ASCII alphanumerics plus `.`, `_`, `:`, and `-`, so ids can
/// name a module (`clock`) or a namespaced item (`ws:2`).
#[must_use]
pub fn is_valid_region_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= UI_MAX_REGION_ID_BYTES
        && id.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | ':' | '-')
        })
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

    #[test]
    fn ui_slot_enum_is_the_closed_v1_set() {
        // CTX-0923 / OQ-056: the enum, the string list, and the predicate
        // agree, and parsing is exact (no case folding, no aliases).
        assert_eq!(UiSlot::ALL.map(UiSlot::as_str), UI_SLOTS);
        for slot in UiSlot::ALL {
            assert_eq!(UiSlot::parse(slot.as_str()), Some(slot));
            assert!(is_ui_slot(slot.as_str()));
        }
        for bad in [
            "",
            "Statusline",
            "statusline",
            "status",
            "workspaceline",
            "tabs",
            " top",
        ] {
            assert_eq!(UiSlot::parse(bad), None, "{bad:?}");
            assert!(!is_ui_slot(bad));
        }
    }

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
        assert_eq!(node.retained_string_bytes(), 6);
    }

    #[test]
    fn retained_string_bytes_charges_identifiers_not_only_text() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Column".to_string())),
            ("fg", LuaValue::String("accent".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("workspace.switch".to_string())),
                    (
                        "args",
                        LuaValue::table([
                            ("id", LuaValue::String("dev".to_string())),
                            ("focus", LuaValue::Bool(true)),
                            ("index", LuaValue::Integer(2)),
                        ]),
                    ),
                ]),
            ),
            (
                "children",
                LuaValue::array(vec![LuaValue::table([
                    ("kind", LuaValue::String("Text".to_string())),
                    ("text", LuaValue::String("body".to_string())),
                    ("bg", LuaValue::String("surface.1".to_string())),
                    (
                        "on_scroll",
                        LuaValue::table([
                            ("command", LuaValue::String("bar.scroll".to_string())),
                            ("args", LuaValue::table([("step", LuaValue::Integer(1))])),
                        ]),
                    ),
                ])]),
            ),
        ]);
        let node = UiNode::from_lua_value(&value).expect("valid component");
        let expected = "accent".len()
            + "workspace.switch".len()
            + "id".len()
            + "dev".len()
            + "focus".len()
            + "index".len()
            + "body".len()
            + "surface.1".len()
            + "bar.scroll".len()
            + "step".len();
        assert_eq!(node.retained_string_bytes(), expected);
        // The SCN-3 per-component measure stays text-only.
        assert_eq!(node.text_bytes(), "body".len());
    }

    #[test]
    fn click_arg_keys_are_capped_and_charged() {
        let with_key = |key: &str| {
            LuaValue::table([
                ("kind", LuaValue::String("Text".to_string())),
                ("text", LuaValue::String("t".to_string())),
                (
                    "on_click",
                    LuaValue::table([
                        ("command", LuaValue::String("cmd".to_string())),
                        (
                            "args",
                            LuaValue::table([(key, LuaValue::String("v".to_string()))]),
                        ),
                    ]),
                ),
            ])
        };
        let at_cap = "k".repeat(UI_CLICK_MAX_ARG_KEY_BYTES);
        assert_eq!(
            UI_CLICK_MAX_ARG_KEY_BYTES, UI_CLICK_MAX_COMMAND_BYTES,
            "the key cap reuses the command-identifier bound; no new ceiling"
        );
        let node = UiNode::from_lua_value(&with_key(&at_cap)).expect("64-byte key accepted");
        assert_eq!(
            node.retained_string_bytes(),
            "t".len() + "cmd".len() + at_cap.len() + "v".len()
        );

        let over = "k".repeat(UI_CLICK_MAX_ARG_KEY_BYTES + 1);
        let error = UiNode::from_lua_value(&with_key(&over)).expect_err("65-byte key rejected");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert!(error.message.contains("args keys must be at most"));
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

    #[test]
    fn theme_token_validation() {
        // Valid tokens
        assert!(is_valid_theme_token("accent"));
        assert!(is_valid_theme_token("surface.2"));
        assert!(is_valid_theme_token("bg-primary"));
        assert!(is_valid_theme_token("text_muted"));
        assert!(is_valid_theme_token("a1.b2-c3_d4"));

        // Invalid tokens
        assert!(!is_valid_theme_token("")); // empty
        assert!(!is_valid_theme_token("Accent")); // uppercase start
        assert!(!is_valid_theme_token("1accent")); // digit start
        assert!(!is_valid_theme_token("_accent")); // underscore start
        assert!(!is_valid_theme_token("accent!")); // invalid char
        assert!(!is_valid_theme_token(
            &"a".repeat(UI_MAX_THEME_TOKEN_BYTES + 1)
        )); // too long
    }

    #[test]
    fn style_attributes_parsed() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("styled".to_string())),
            ("fg", LuaValue::String("accent".to_string())),
            ("bg", LuaValue::String("surface.1".to_string())),
            ("bold", LuaValue::Bool(true)),
        ]);
        let node = UiNode::from_lua_value(&value).expect("valid styled text");
        match node {
            UiNode::Text { fg, bg, bold, .. } => {
                assert_eq!(fg, Some("accent".to_string()));
                assert_eq!(bg, Some("surface.1".to_string()));
                assert_eq!(bold, Some(true));
            }
            _ => panic!("expected Text node"),
        }
    }

    #[test]
    fn invalid_theme_token_rejected() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            ("fg", LuaValue::String("Invalid!Token".to_string())),
        ]);
        let error = UiNode::from_lua_value(&value).expect_err("invalid token");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert!(error.message.contains("invalid theme token"));
    }

    #[test]
    fn click_command_parsed() {
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("click me".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("workspace.switch".to_string())),
                    (
                        "args",
                        LuaValue::table([
                            ("id", LuaValue::String("dev".to_string())),
                            ("focus", LuaValue::Bool(true)),
                            ("index", LuaValue::Integer(2)),
                        ]),
                    ),
                ]),
            ),
        ]);
        let node = UiNode::from_lua_value(&value).expect("valid click command");
        match node {
            UiNode::Text { on_click, .. } => {
                let cmd = on_click.expect("on_click present");
                assert_eq!(cmd.command, "workspace.switch");
                assert_eq!(cmd.args.len(), 3);
                assert!(
                    cmd.args
                        .iter()
                        .any(|(k, v)| k == "id" && matches!(v, ClickArg::String(s) if s == "dev"))
                );
                assert!(
                    cmd.args
                        .iter()
                        .any(|(k, v)| k == "focus" && matches!(v, ClickArg::Bool(true)))
                );
                assert!(cmd.args.iter().any(|(k, v)| k == "index"
                    && matches!(v, ClickArg::Number(n) if (*n - 2.0).abs() < 0.01)));
            }
            _ => panic!("expected Text node"),
        }
    }

    fn region_table(id: &str, kind: Option<&str>) -> LuaValue {
        let mut pairs = vec![(
            LuaValue::String("id".to_string()),
            LuaValue::String(id.to_string()),
        )];
        if let Some(kind) = kind {
            pairs.push((
                LuaValue::String("kind".to_string()),
                LuaValue::String(kind.to_string()),
            ));
        }
        LuaValue::Table(pairs)
    }

    fn click_with_region(region: LuaValue) -> LuaValue {
        LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("clock".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("clock.open".to_string())),
                    ("region", region),
                ]),
            ),
        ])
    }

    #[test]
    fn region_round_trips_from_binding() {
        // Issue #2248: a binding may declare one bounded region identity;
        // the parser retains it verbatim for Core geometry emission.
        let node =
            UiNode::from_lua_value(&click_with_region(region_table("clock", Some("module"))))
                .expect("region parses");
        let UiNode::Text { on_click, .. } = node else {
            panic!("expected Text node");
        };
        assert_eq!(
            on_click.expect("binding").region,
            Some(UiRegion {
                id: "clock".to_string(),
                kind: UiRegionKind::Module,
            })
        );

        // The kind is optional and defaults to `module`.
        let node = UiNode::from_lua_value(&click_with_region(region_table("ws:2", None)))
            .expect("region without kind parses");
        let UiNode::Text { on_click, .. } = node else {
            panic!("expected Text node");
        };
        assert_eq!(
            on_click.expect("binding").region,
            Some(UiRegion {
                id: "ws:2".to_string(),
                kind: UiRegionKind::Module,
            })
        );

        // An explicit `item` kind survives the round trip.
        let node = UiNode::from_lua_value(&click_with_region(region_table("ws:2", Some("item"))))
            .expect("item kind parses");
        let UiNode::Text { on_click, .. } = node else {
            panic!("expected Text node");
        };
        assert_eq!(
            on_click.expect("binding").region.expect("region").kind,
            UiRegionKind::Item
        );
    }

    #[test]
    fn region_validation_fails_closed() {
        let too_long = "a".repeat(UI_MAX_REGION_ID_BYTES + 1);
        let bad_ids = ["", "Clock", "with space", "bang!", too_long.as_str()];
        for bad in bad_ids {
            let error = UiNode::from_lua_value(&click_with_region(region_table(bad, None)))
                .expect_err("invalid region id must fail");
            assert_eq!(error.code, "E_UI_COMPONENT_INVALID", "id {bad:?}");
        }
        // Unknown kind fails; missing id fails.
        assert_eq!(
            UiNode::from_lua_value(&click_with_region(region_table("clock", Some("pixel"))))
                .expect_err("unknown kind")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        assert_eq!(
            UiNode::from_lua_value(&click_with_region(LuaValue::table([(
                "kind",
                LuaValue::String("module".to_string()),
            )])))
            .expect_err("missing id")
            .code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    #[test]
    fn click_and_scroll_regions_must_agree() {
        // One span has one item identity: mismatched declarations reject the
        // mount instead of silently choosing a binding's region.
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("volume".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("volume.open".to_string())),
                    ("region", region_table("volume", Some("module"))),
                ]),
            ),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", LuaValue::String("volume.step".to_string())),
                    ("region", region_table("speaker", Some("module"))),
                ]),
            ),
        ]);
        let error = UiNode::from_lua_value(&value).expect_err("mismatched regions");
        assert_eq!(error.code, "E_UI_COMPONENT_INVALID");
        assert!(error.message.contains("same region"));

        // Identical regions are accepted.
        let mut value = value;
        if let LuaValue::Table(pairs) = &mut value {
            for (key, binding) in pairs.iter_mut() {
                if matches!(key, LuaValue::String(name) if name == "on_scroll") {
                    if let LuaValue::Table(binding_pairs) = binding {
                        for (binding_key, region) in binding_pairs.iter_mut() {
                            if matches!(binding_key, LuaValue::String(name) if name == "region") {
                                *region = region_table("volume", Some("module"));
                            }
                        }
                    }
                }
            }
        }
        UiNode::from_lua_value(&value).expect("matching regions parse");
    }

    #[test]
    fn click_args_reserve_region_and_button_slots() {
        let args = |count: usize| {
            LuaValue::Table(
                (0..count)
                    .map(|index| {
                        (
                            LuaValue::String(format!("arg{index}")),
                            LuaValue::String("v".to_string()),
                        )
                    })
                    .collect(),
            )
        };
        let value = |args: LuaValue, region: Option<LuaValue>| {
            let mut binding = vec![
                (
                    LuaValue::String("command".to_string()),
                    LuaValue::String("cmd".to_string()),
                ),
                (LuaValue::String("args".to_string()), args),
            ];
            if let Some(region) = region {
                binding.push((LuaValue::String(UI_REGION_ARG.to_string()), region));
            }
            LuaValue::table([
                ("kind", LuaValue::String("Text".to_string())),
                ("text", LuaValue::String("t".to_string())),
                ("on_click", LuaValue::Table(binding)),
            ])
        };
        // No region: no host args are injected, so the pre-2248 budget of
        // 16 declared args is unchanged.
        UiNode::from_lua_value(&value(args(UI_CLICK_MAX_ARGS), None))
            .expect("16 declared args fit without a region");
        assert_eq!(
            UiNode::from_lua_value(&value(args(UI_CLICK_MAX_ARGS + 1), None))
                .expect_err("17 declared args exceed the dispatch bound")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        // With a region: `region` and `button` are injected, so 14 fit.
        UiNode::from_lua_value(&value(
            args(UI_CLICK_MAX_ARGS - 2),
            Some(region_table("clock", None)),
        ))
        .expect("14 declared args fit with a region");
        assert_eq!(
            UiNode::from_lua_value(&value(
                args(UI_CLICK_MAX_ARGS - 1),
                Some(region_table("clock", None)),
            ))
            .expect_err("15 declared args exceed the dispatch bound with a region")
            .code,
            "E_UI_COMPONENT_INVALID"
        );
        // Declaring the host names reserves no extra slot (overwritten).
        let mut declared = vec![
            (
                LuaValue::String(UI_REGION_ARG.to_string()),
                LuaValue::String("r".to_string()),
            ),
            (
                LuaValue::String(UI_BUTTON_ARG.to_string()),
                LuaValue::String("left".to_string()),
            ),
        ];
        declared.extend((0..(UI_CLICK_MAX_ARGS - 2)).map(|index| {
            (
                LuaValue::String(format!("arg{index}")),
                LuaValue::String("v".to_string()),
            )
        }));
        UiNode::from_lua_value(&value(
            LuaValue::Table(declared),
            Some(region_table("clock", None)),
        ))
        .expect("declared host names reuse their slots");
    }

    #[test]
    fn click_command_validation() {
        // Command too long
        let long_cmd = "a".repeat(UI_CLICK_MAX_COMMAND_BYTES + 1);
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            (
                "on_click",
                LuaValue::table([("command", LuaValue::String(long_cmd))]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("command too long")
                .code,
            "E_UI_COMPONENT_INVALID"
        );

        // Too many args
        let mut args_vec = Vec::new();
        for i in 0..=UI_CLICK_MAX_ARGS {
            args_vec.push((
                LuaValue::String(format!("arg{i}")),
                LuaValue::String("val".to_string()),
            ));
        }
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("cmd".to_string())),
                    ("args", LuaValue::Table(args_vec)),
                ]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("too many args")
                .code,
            "E_UI_COMPONENT_INVALID"
        );

        // Arg value too long
        let long_val = "a".repeat(UI_CLICK_MAX_ARG_BYTES + 1);
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("cmd".to_string())),
                    (
                        "args",
                        LuaValue::table([("key", LuaValue::String(long_val))]),
                    ),
                ]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&value)
                .expect_err("arg too long")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    fn scroll_table(command: &str) -> LuaValue {
        LuaValue::table([("command", LuaValue::String(command.to_string()))])
    }

    #[test]
    fn scroll_binding_parsed_alongside_click() {
        // CTX-1190 (#2008): on_scroll mirrors on_click with the same
        // ClickCommand shape. The repeated-click workaround (on_click
        // stepping state) keeps parsing unchanged beside it.
        let value = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("volume".to_string())),
            (
                "on_click",
                LuaValue::table([
                    ("command", LuaValue::String("volume.step".to_string())),
                    ("args", LuaValue::table([("step", LuaValue::Integer(2))])),
                ]),
            ),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", LuaValue::String("volume.scroll".to_string())),
                    ("args", LuaValue::table([("step", LuaValue::Integer(2))])),
                ]),
            ),
        ]);
        let node = UiNode::from_lua_value(&value).expect("click and scroll bindings parse");
        match node {
            UiNode::Text {
                on_click,
                on_scroll,
                ..
            } => {
                let click = on_click.expect("on_click present");
                assert_eq!(click.command, "volume.step");
                let scroll = on_scroll.expect("on_scroll present");
                assert_eq!(scroll.command, "volume.scroll");
                assert!(scroll.args.iter().any(|(key, value)| key == "step"
                    && matches!(value, ClickArg::Number(n) if (*n - 2.0).abs() < 0.01)));
            }
            _ => panic!("expected Text node"),
        }
    }

    #[test]
    fn scroll_binding_parsed_on_every_container_kind() {
        for kind in ["Row", "Column", "List"] {
            let value = LuaValue::table([
                ("kind", LuaValue::String(kind.to_string())),
                ("children", LuaValue::array(vec![text("x")])),
                ("on_scroll", scroll_table("bar.scroll")),
            ]);
            let node = UiNode::from_lua_value(&value).expect("container scroll binding parses");
            let scroll = match &node {
                UiNode::Row { on_scroll, .. }
                | UiNode::Column { on_scroll, .. }
                | UiNode::List { on_scroll, .. } => on_scroll.clone(),
                UiNode::Text { .. } => panic!("expected container node"),
            };
            assert_eq!(
                scroll.expect("on_scroll present").command,
                "bar.scroll",
                "kind {kind}"
            );
        }
    }

    #[test]
    fn scroll_binding_validation_fails_closed() {
        let text_with = |scroll: LuaValue| {
            LuaValue::table([
                ("kind", LuaValue::String("Text".to_string())),
                ("text", LuaValue::String("text".to_string())),
                ("on_scroll", scroll),
            ])
        };
        // Non-table binding.
        assert_eq!(
            UiNode::from_lua_value(&text_with(LuaValue::String("nope".to_string())))
                .expect_err("non-table scroll")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        // Missing command.
        assert_eq!(
            UiNode::from_lua_value(&text_with(LuaValue::table([])))
                .expect_err("missing command")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        // Over-long command.
        let long_cmd = "a".repeat(UI_CLICK_MAX_COMMAND_BYTES + 1);
        assert_eq!(
            UiNode::from_lua_value(&text_with(scroll_table(&long_cmd)))
                .expect_err("command too long")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        // Non-scalar arg value.
        assert_eq!(
            UiNode::from_lua_value(&text_with(LuaValue::table([
                ("command", LuaValue::String("cmd".to_string())),
                (
                    "args",
                    LuaValue::table([("key", LuaValue::array(vec![text("nested")]),)]),
                ),
            ])))
            .expect_err("table arg value")
            .code,
            "E_UI_COMPONENT_INVALID"
        );
        // Oversize string arg.
        let long_val = "a".repeat(UI_CLICK_MAX_ARG_BYTES + 1);
        assert_eq!(
            UiNode::from_lua_value(&text_with(LuaValue::table([
                ("command", LuaValue::String("cmd".to_string())),
                (
                    "args",
                    LuaValue::table([("key", LuaValue::String(long_val))]),
                ),
            ])))
            .expect_err("arg too long")
            .code,
            "E_UI_COMPONENT_INVALID"
        );
    }

    #[test]
    fn scroll_args_reserve_one_slot_for_the_host_delta() {
        // CTX-1190 (#2008): dispatch merges the host-computed delta beside
        // the declared args, so a full table without a delta key fails at
        // parse; one that already names delta fits exactly.
        let full_args: Vec<(LuaValue, LuaValue)> = (0..UI_CLICK_MAX_ARGS)
            .map(|index| {
                (
                    LuaValue::String(format!("arg{index}")),
                    LuaValue::String("val".to_string()),
                )
            })
            .collect();
        let full = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", LuaValue::String("cmd".to_string())),
                    ("args", LuaValue::Table(full_args)),
                ]),
            ),
        ]);
        assert_eq!(
            UiNode::from_lua_value(&full)
                .expect_err("full args without delta key")
                .code,
            "E_UI_COMPONENT_INVALID"
        );
        let mut with_delta: Vec<(LuaValue, LuaValue)> = (0..UI_CLICK_MAX_ARGS - 1)
            .map(|index| {
                (
                    LuaValue::String(format!("arg{index}")),
                    LuaValue::String("val".to_string()),
                )
            })
            .collect();
        with_delta.push((LuaValue::String("delta".to_string()), LuaValue::Integer(1)));
        let fitting = LuaValue::table([
            ("kind", LuaValue::String("Text".to_string())),
            ("text", LuaValue::String("text".to_string())),
            (
                "on_scroll",
                LuaValue::table([
                    ("command", LuaValue::String("cmd".to_string())),
                    ("args", LuaValue::Table(with_delta)),
                ]),
            ),
        ]);
        let node = UiNode::from_lua_value(&fitting).expect("delta-named full table fits");
        match node {
            UiNode::Text { on_scroll, .. } => {
                assert_eq!(
                    on_scroll.expect("on_scroll present").args.len(),
                    UI_CLICK_MAX_ARGS
                );
            }
            _ => panic!("expected Text node"),
        }
    }
}
