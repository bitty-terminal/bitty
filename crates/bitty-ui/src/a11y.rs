//! Accessibility baseline projection (UX-42, CTX-0614).
//!
//! Candidate implementation of
//! `bitty-terminal-docs/specifications/accessibility-baseline-candidate.md`
//! (**Draft**, CTX-0046). Nothing here is normative, accepted, or verified:
//! every role name, key spelling, and bound below is a candidate spelling
//! that the owner-pending UI Runtime RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`SceneKind`] / [`A11yRole`] — role mapping for every presentable
//!   [`SceneKind`], mirroring the `SceneNode` variants of `bitty-rich`
//!   (`Text`, `Row`, `Column`, `Block`, `Image`, `CodeBlock`, `Table`,
//!   `List`, `Rule`). [`role_of`] maps each of them; the forward-compatible
//!   fallback (`SceneNode::Unknown`, mirrored as [`SceneKind::Unknown`])
//!   fails closed with [`A11yError::UnmappedSceneKind`] instead of
//!   presenting silently. Interactive roles ([`A11yRole::Button`] /
//!   [`A11yRole::Input`]) resolve from the declared purpose via
//!   [`interactive_role`]; `bitty-rich` v1 carries no interactive
//!   `SceneNode` variant yet, so any undeclared purpose also fails closed.
//! - Terminal exposure ([`expose_terminal`]) — readable text runs plus
//!   cursor position, with the stated [`FIDELITY_BOUNDARY`]. Styling,
//!   graphics protocols, and images are never promised as structure.
//! - Chrome exposure ([`ChromeNode`], [`chrome_tab_order`]) — read-only
//!   structure with state announced, excluded from the tab order by
//!   default, never a focus target.
//! - Accessibility tree ([`A11yTree`], [`A11yTreeBuilder`],
//!   [`build_a11y_tree`]) — the M1 v1 snapshot joining the terminal leaf,
//!   chrome surfaces, and scene roles under one window root. Owned by
//!   `bitty-ui`; Panel Scene producers and platform adapters consume it
//!   read-only. Structure, names, and state only; actions and live events
//!   are post-1.0.
//!
//! The projection is derived and read-only: every constructor takes shared
//! references and returns owned data. It never mutates terminal state, is
//! never persisted, and is never consulted for routing or dispatch.

#![forbid(unsafe_code)]

use bitty_term_state::{CursorPosition, Snapshot};
use std::fmt;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap in characters for an accessible name or active-item label.
///
/// Longer input is rejected with [`A11yError::NameTooLong`], never
/// silently truncated: truncation would misname the surface to assistive
/// technology.
pub const MAX_ACCESSIBLE_NAME_LEN: usize = 256;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build an accessibility projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yError {
    /// A scene kind (or interactive purpose) has no mapped role.
    ///
    /// Fail-closed: the caller must not present the node until a mapping
    /// exists. Mirrors the candidate rule that a scene path without a
    /// semantics mapping is non-conformant.
    UnmappedSceneKind,
    /// An accessible name or label exceeds [`MAX_ACCESSIBLE_NAME_LEN`].
    NameTooLong {
        /// Length in characters of the rejected value.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A tree build would exceed [`MAX_A11Y_TREE_NODES`].
    ///
    /// Fail-closed: no partial tree is returned.
    TooManyNodes {
        /// Nodes the build attempted to hold.
        count: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A node was pushed under a handle from another builder (or out of
    /// range): handles are builder-scoped and never forged.
    UnknownParent,
}

impl fmt::Display for A11yError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnmappedSceneKind => f.write_str("unmapped scene kind: no role mapping"),
            Self::NameTooLong { len, cap } => {
                write!(f, "accessible name too long: {len} chars exceeds cap {cap}")
            }
            Self::TooManyNodes { count, cap } => {
                write!(
                    f,
                    "accessibility tree too large: {count} nodes exceeds cap {cap}"
                )
            }
            Self::UnknownParent => f.write_str("unknown parent node: not part of this tree"),
        }
    }
}

impl std::error::Error for A11yError {}

// ---------------------------------------------------------------------------
// Scene role mapping
// ---------------------------------------------------------------------------

/// Presentable scene node kinds.
///
/// Mirrors the `SceneNode` variants of `bitty-rich` v1 (`Text`, `Row`,
/// `Column`, `Block`, `Image`, `CodeBlock`, `Table`, `List`, `Rule`) plus
/// the forward-compatible fallback (`SceneNode::Unknown`, mirrored here as
/// [`SceneKind::Unknown`]). The kinds are mirrored rather than imported so
/// the projection stays decoupled from the producer at runtime; the mirror
/// cannot drift silently — `scene_kind_mirror_tracks_bitty_rich` (integration
/// test, `bitty-rich` dev-dependency) maps every `SceneNode` variant and
/// fails to compile if `bitty-rich` gains one. If it does, a matching
/// [`SceneKind`] plus a [`role_of`] mapping must land here first, otherwise
/// validation fails closed.
///
/// `bitty-rich` v1 carries no interactive node variant. The candidate
/// requires interactive nodes to expose a role from their declared purpose
/// (`button`, `input`) with name, state, and activation; that requirement
/// is served by [`interactive_role`] / [`InteractiveNode`], and any
/// undeclared purpose fails closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SceneKind {
    /// `SceneNode::Text` — styled span.
    Text,
    /// `SceneNode::Row` — horizontal row of children.
    Row,
    /// `SceneNode::Column` — vertical column of children.
    Column,
    /// `SceneNode::Block` — bordered block with a single child.
    Block,
    /// `SceneNode::Image` — image placement reference.
    Image,
    /// `SceneNode::CodeBlock` — code block with language hint.
    CodeBlock,
    /// `SceneNode::Table` — headless bounded table model.
    Table,
    /// `SceneNode::List` — headless bounded list model.
    List,
    /// `SceneNode::Rule` — horizontal rule.
    Rule,
    /// `SceneNode::Unknown` — forward-compatible bounded plain-text
    /// fallback for variants from a newer producer. Never mapped: it
    /// fails closed at validation.
    Unknown,
}

impl SceneKind {
    /// Every kind this module knows about, including [`SceneKind::Unknown`].
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Text,
            Self::Row,
            Self::Column,
            Self::Block,
            Self::Image,
            Self::CodeBlock,
            Self::Table,
            Self::List,
            Self::Rule,
            Self::Unknown,
        ]
    }

    /// Every kind that currently has a role mapping (all of [`SceneKind::all`]
    /// except [`SceneKind::Unknown`]).
    #[must_use]
    pub const fn mapped() -> &'static [Self] {
        &[
            Self::Text,
            Self::Row,
            Self::Column,
            Self::Block,
            Self::Image,
            Self::CodeBlock,
            Self::Table,
            Self::List,
            Self::Rule,
        ]
    }
}

/// Candidate accessible roles (candidate vocabulary from the baseline
/// record; not a platform convention claim).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yRole {
    /// Running text.
    Text,
    /// Grouping container (row, column, bordered block).
    Group,
    /// Code block.
    Code,
    /// Table; row/column counts and declared headers exposed by the caller.
    Table,
    /// List; item count exposed by the caller, items as children.
    List,
    /// Activatable button (declared purpose `button`).
    Button,
    /// Text input (declared purpose `input`).
    Input,
    /// Separator (horizontal rule).
    Separator,
    /// Image placement. Exposure names the boundary: no pixel description
    /// is promised (see [`FIDELITY_BOUNDARY`]).
    Image,
}

impl A11yRole {
    /// Candidate vocabulary spelling for this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Group => "group",
            Self::Code => "code",
            Self::Table => "table",
            Self::List => "list",
            Self::Button => "button",
            Self::Input => "input",
            Self::Separator => "separator",
            Self::Image => "image",
        }
    }
}

impl fmt::Display for A11yRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maps a scene kind to its accessible role.
///
/// Every [`SceneKind`] except [`SceneKind::Unknown`] maps; `Unknown`
/// returns [`A11yError::UnmappedSceneKind`] (fail closed).
///
/// # Errors
///
/// Returns [`A11yError::UnmappedSceneKind`] for [`SceneKind::Unknown`].
pub fn role_of(kind: SceneKind) -> Result<A11yRole, A11yError> {
    match kind {
        SceneKind::Text => Ok(A11yRole::Text),
        SceneKind::Row | SceneKind::Column | SceneKind::Block => Ok(A11yRole::Group),
        SceneKind::CodeBlock => Ok(A11yRole::Code),
        SceneKind::Table => Ok(A11yRole::Table),
        SceneKind::List => Ok(A11yRole::List),
        SceneKind::Rule => Ok(A11yRole::Separator),
        SceneKind::Image => Ok(A11yRole::Image),
        SceneKind::Unknown => Err(A11yError::UnmappedSceneKind),
    }
}

/// Resolves an interactive role from its declared purpose.
///
/// The candidate requires interactive nodes to expose the role of their
/// declared purpose (`button`, `input`); anything else fails closed so a
/// future purpose cannot present under a guessed role.
///
/// # Errors
///
/// Returns [`A11yError::UnmappedSceneKind`] for any purpose other than
/// `button` or `input`.
pub fn interactive_role(purpose: &str) -> Result<A11yRole, A11yError> {
    match purpose {
        "button" => Ok(A11yRole::Button),
        "input" => Ok(A11yRole::Input),
        _ => Err(A11yError::UnmappedSceneKind),
    }
}

/// Validates that every kind in a presented scene has a mapped role.
///
/// Fail-closed: the first unmapped kind aborts with
/// [`A11yError::UnmappedSceneKind`] and the scene must not present.
///
/// # Errors
///
/// Returns [`A11yError::UnmappedSceneKind`] when any entry is unmapped.
pub fn validate_scene(kinds: &[SceneKind]) -> Result<(), A11yError> {
    for kind in kinds {
        role_of(*kind)?;
    }
    Ok(())
}

/// An interactive node with accessible name, state, and activation.
///
/// Built from a declared purpose via [`interactive_role`], so the role is
/// never guessed. `enabled` is the exposed state; [`InteractiveNode::activation_label`]
/// is the exposed activation, present only while enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveNode {
    role: A11yRole,
    name: String,
    enabled: bool,
}

impl InteractiveNode {
    /// Builds an interactive node for `purpose` (`button` or `input`)
    /// with the given accessible name.
    ///
    /// # Errors
    ///
    /// Returns [`A11yError::UnmappedSceneKind`] for an undeclared purpose,
    /// [`A11yError::NameTooLong`] when `name` exceeds
    /// [`MAX_ACCESSIBLE_NAME_LEN`] characters.
    pub fn new(purpose: &str, name: &str, enabled: bool) -> Result<Self, A11yError> {
        let role = interactive_role(purpose)?;
        check_name_len(name)?;
        Ok(Self {
            role,
            name: name.to_string(),
            enabled,
        })
    }

    /// The resolved role (`button` or `input`).
    #[must_use]
    pub const fn role(&self) -> A11yRole {
        self.role
    }

    /// The accessible name from content.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The exposed state: whether the control can be activated.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The exposed activation. Present (`"activate"`) only while enabled;
    /// a disabled control exposes state with no activation.
    #[must_use]
    pub const fn activation_label(&self) -> Option<&'static str> {
        if self.enabled { Some("activate") } else { None }
    }
}

fn check_name_len(name: &str) -> Result<(), A11yError> {
    let len = name.chars().count();
    if len > MAX_ACCESSIBLE_NAME_LEN {
        return Err(A11yError::NameTooLong {
            len,
            cap: MAX_ACCESSIBLE_NAME_LEN,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal exposure
// ---------------------------------------------------------------------------

/// Stated fidelity boundary for terminal exposure.
///
/// The projection exposes readable text and the cursor position; it does
/// not promise faithful representation of absolute cursor addressing,
/// full SGR styling as a semantic structure, graphics protocols, or
/// images. The boundary is stated rather than implied, so no reviewer
/// mistakes partial exposure for a missing feature.
pub const FIDELITY_BOUNDARY: &str = "terminal exposure covers readable text runs and cursor position only; \
absolute cursor addressing, SGR styling as semantic structure, graphics protocols, and images are not represented";

/// One contiguous span of readable terminal cell content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRun {
    /// Zero-based grid row this run was read from.
    pub row: usize,
    /// Row content with trailing blanks trimmed. Wide-character trailing
    /// halves (spacers) are skipped, never split; combining marks stay
    /// attached to their base cell. Styling is deliberately absent (see
    /// [`FIDELITY_BOUNDARY`]).
    pub text: String,
}

/// Read-only accessibility exposure of a terminal leaf.
///
/// Derived from a [`Snapshot`]: role `text` region named by the window
/// title, text runs, and cursor position. Built from a shared reference
/// and fully owned, so exposure can never mutate grid, cursor, modes, or
/// scrollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalExposure {
    /// Accessible name of the region: the window/icon title.
    pub title: String,
    /// Readable text runs, one per non-blank row in row order.
    pub runs: Vec<TextRun>,
    /// Live cursor position on the active grid.
    pub cursor: CursorPosition,
    /// Whether the cursor is rendered (`DECTCEM`).
    pub cursor_visible: bool,
}

/// Reads the text runs of a snapshot: one [`TextRun`] per non-blank row,
/// in row order, with trailing blanks trimmed.
///
/// Read-only: the snapshot is only borrowed.
#[must_use]
pub fn terminal_text_runs(snapshot: &Snapshot) -> Vec<TextRun> {
    let mut runs = Vec::new();
    if snapshot.width == 0 || snapshot.height == 0 {
        return runs;
    }
    for row in 0..snapshot.height {
        let start = row.saturating_mul(snapshot.width);
        let end = start
            .saturating_add(snapshot.width)
            .min(snapshot.cells.len());
        let Some(cells) = snapshot.cells.get(start..end) else {
            continue;
        };
        let mut text = String::new();
        for cell in cells {
            if cell.spacer {
                continue;
            }
            text.push(cell.glyph);
            text.extend(cell.zerowidth.iter().copied());
        }
        let trimmed = text.trim_end().to_string();
        if !trimmed.is_empty() {
            runs.push(TextRun { row, text: trimmed });
        }
    }
    runs
}

/// Builds the full read-only exposure of a terminal leaf.
///
/// The region is named by the snapshot title; runs come from
/// [`terminal_text_runs`]; cursor position and visibility come from the
/// snapshot cursor. Nothing is promised beyond [`FIDELITY_BOUNDARY`].
///
/// Read-only: the snapshot is only borrowed.
#[must_use]
pub fn expose_terminal(snapshot: &Snapshot) -> TerminalExposure {
    TerminalExposure {
        title: snapshot.title.as_str().to_string(),
        runs: terminal_text_runs(snapshot),
        cursor: snapshot.cursor.position,
        cursor_visible: snapshot.cursor.visible,
    }
}

// ---------------------------------------------------------------------------
// Chrome exposure
// ---------------------------------------------------------------------------

/// Chrome surface kinds with accessibility exposure.
///
/// Bars, rails, tab strips, notifications, and overlays: read-only
/// structure with state announced, per the candidate required-mappings
/// table (bar/rail: active item exposed; tab strip: `tablist` with the
/// active tab exposed; notification: announcement on transition with no
/// focus capture).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChromeKind {
    /// Status bar.
    Bar,
    /// Side rail.
    Rail,
    /// Tab strip (role `tablist`).
    TabStrip,
    /// Notification surface (announces on transition, never captures focus).
    Notification,
    /// Ephemeral overlay surface.
    Overlay,
}

impl ChromeKind {
    /// Candidate vocabulary spelling for this chrome kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bar => "bar",
            Self::Rail => "rail",
            Self::TabStrip => "tablist",
            Self::Notification => "notification",
            Self::Overlay => "overlay",
        }
    }
}

impl fmt::Display for ChromeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A chrome surface's read-only accessibility exposure.
///
/// Carries the accessible name and the announced state (the active item or
/// status text). Chrome never receives keyboard or IME input, so it is
/// never a focus target: [`ChromeNode::is_tab_stop`] is always false and
/// [`chrome_tab_order`] always yields an empty order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChromeNode {
    kind: ChromeKind,
    name: String,
    active_item: Option<String>,
}

impl ChromeNode {
    /// Describes a chrome surface with its accessible name and optional
    /// active-item / status text (the announced state).
    ///
    /// # Errors
    ///
    /// Returns [`A11yError::NameTooLong`] when `name` or `active_item`
    /// exceeds [`MAX_ACCESSIBLE_NAME_LEN`] characters.
    pub fn new(kind: ChromeKind, name: &str, active_item: Option<&str>) -> Result<Self, A11yError> {
        check_name_len(name)?;
        if let Some(active) = active_item {
            check_name_len(active)?;
        }
        Ok(Self {
            kind,
            name: name.to_string(),
            active_item: active_item.map(str::to_string),
        })
    }

    /// The chrome surface kind.
    #[must_use]
    pub const fn kind(&self) -> ChromeKind {
        self.kind
    }

    /// The accessible name (always exposed, read-only).
    #[must_use]
    pub fn accessible_name(&self) -> &str {
        &self.name
    }

    /// The announced state: the active item or status text, if any
    /// (always exposed, read-only).
    #[must_use]
    pub fn active_item(&self) -> Option<&str> {
        self.active_item.as_deref()
    }

    /// Whether this surface is a tab stop. Always false: chrome is
    /// excluded from the tab order by default and never a focus target.
    #[must_use]
    pub const fn is_tab_stop(&self) -> bool {
        false
    }
}

/// Computes the tab order over chrome surfaces.
///
/// Filter-based: only tab stops are listed, and no chrome surface is a
/// tab stop, so the order is empty while names and states stay exposed
/// through [`ChromeNode::accessible_name`] / [`ChromeNode::active_item`].
/// If a future kind ever becomes focusable, only
/// [`ChromeNode::is_tab_stop`] changes; the exclusion rule stays in this
/// one place.
#[must_use]
pub fn chrome_tab_order(nodes: &[ChromeNode]) -> Vec<&ChromeNode> {
    nodes.iter().filter(|node| node.is_tab_stop()).collect()
}

// ---------------------------------------------------------------------------
// Accessibility tree (M1 decision, #1149)
// ---------------------------------------------------------------------------
//
// Ownership: the tree is owned by `bitty-ui`. Panel Scene producers
// (`bitty-panels`, `bitty-rich`) and platform adapters (window, screen-reader
// bridges) are consumers — they read [`A11yTree`] snapshots and never
// construct or mutate nodes. v1 exposes structure, names, and state only;
// action routing, activation dispatch, and live-event streams are post-1.0.
//
// Shape (stable, pinned by integration tests): the root names the window;
// its children are the terminal leaf first (run rows nested under it in row
// order), then chrome surfaces in caller order, then scene roles in caller
// order. [`build_a11y_tree`] validates every scene kind before attaching
// anything, so an unmapped kind fails the whole build instead of presenting
// a partial tree.

/// Hard cap on nodes in one tree.
///
/// Bounds untrusted input (window titles, grid text, chrome labels): a
/// `MAX_GRID_DIM` × `MAX_GRID_DIM` grid contributes at most its row count
/// plus two structural nodes, so real grids land far below this cap and
/// only adversarial or programming-error inputs trip it — fail-closed, via
/// [`A11yError::TooManyNodes`].
pub const MAX_A11Y_TREE_NODES: usize = 4096;

/// Opaque node handle, scoped to the [`A11yTreeBuilder`] that issued it.
///
/// There is no public constructor: handles cannot be forged, and passing a
/// handle from another builder fails with [`A11yError::UnknownParent`]
/// instead of attaching to the wrong tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct A11yNodeId(u32);

impl A11yNodeId {
    /// Numeric handle, stable for the lifetime of the owning tree.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }

    /// Handle as a node-table index.
    #[must_use]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// What one tree node exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum A11yNodeKind {
    /// Window root; names the window.
    Root,
    /// Terminal leaf; names the region, states the cursor.
    Terminal,
    /// One readable text row; carries content (see [`A11yTreeBuilder::push_row_text`]).
    TerminalRow,
    /// Mapped scene kind; the role resolves via [`role_of`].
    Scene(SceneKind),
    /// Chrome surface; identified by [`ChromeKind`], never a tab stop.
    Chrome(ChromeKind),
    /// Interactive control with a declared-purpose role.
    Interactive(A11yRole),
}

/// One node of the accessibility tree: kind, accessible name, announced
/// state, and ordered children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A11yNode {
    id: A11yNodeId,
    kind: A11yNodeKind,
    name: String,
    state: Option<String>,
    children: Vec<A11yNodeId>,
}

impl A11yNode {
    /// Node handle.
    #[must_use]
    pub const fn id(&self) -> A11yNodeId {
        self.id
    }

    /// What this node exposes.
    #[must_use]
    pub const fn kind(&self) -> A11yNodeKind {
        self.kind
    }

    /// Accessible name (row nodes carry row content instead; see
    /// [`A11yTreeBuilder::push_row_text`]).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Announced state (cursor summary, active item, control state), if any.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.state.as_deref()
    }

    /// Ordered child handles.
    #[must_use]
    pub fn children(&self) -> &[A11yNodeId] {
        &self.children
    }

    /// Accessible role, when the kind carries one. Chrome surfaces have no
    /// [`A11yRole`]: they are identified by [`ChromeKind`] instead, so this
    /// returns `None` for them rather than inventing a mapping.
    #[must_use]
    pub fn role(&self) -> Option<A11yRole> {
        match self.kind {
            A11yNodeKind::Root => Some(A11yRole::Group),
            A11yNodeKind::Terminal | A11yNodeKind::TerminalRow => Some(A11yRole::Text),
            A11yNodeKind::Scene(kind) => role_of(kind).ok(),
            A11yNodeKind::Chrome(_) => None,
            A11yNodeKind::Interactive(role) => Some(role),
        }
    }
}

/// Read-only accessibility tree snapshot: nodes plus the root handle.
///
/// Derived and immutable like the rest of this module — built once by
/// [`A11yTreeBuilder`], then only borrowed. Node handles stay stable for
/// the lifetime of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A11yTree {
    nodes: Vec<A11yNode>,
    root: A11yNodeId,
}

impl A11yTree {
    /// Root handle (always id `0`).
    #[must_use]
    pub const fn root(&self) -> A11yNodeId {
        self.root
    }

    /// Node count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree holds no nodes (never true for built trees: the
    /// root always exists).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Resolves a handle to its node; foreign handles fail closed (`None`).
    #[must_use]
    pub fn get(&self, id: A11yNodeId) -> Option<&A11yNode> {
        self.nodes.get(id.as_usize())
    }

    /// Depth-first pre-order traversal from the root (documented tree order:
    /// terminal subtree, then chrome, then scene).
    pub fn preorder(&self) -> impl Iterator<Item = &A11yNode> + '_ {
        let mut stack = vec![self.root];
        std::iter::from_fn(move || {
            let id = stack.pop()?;
            let node = self.nodes.get(id.as_usize())?;
            stack.extend(node.children.iter().rev());
            Some(node)
        })
    }
}

/// Builds [`A11yTree`] snapshots, fail-closed.
///
/// Handles are builder-scoped: pushing under a foreign or out-of-range
/// handle returns [`A11yError::UnknownParent`]. Exceeding
/// [`MAX_A11Y_TREE_NODES`] returns [`A11yError::TooManyNodes`]; overlong
/// names or states return [`A11yError::NameTooLong`], except row content
/// (see [`Self::push_row_text`]).
#[derive(Debug)]
pub struct A11yTreeBuilder {
    nodes: Vec<A11yNode>,
}

impl A11yTreeBuilder {
    /// Starts a tree whose root carries `root_name`.
    ///
    /// # Errors
    ///
    /// Returns [`A11yError::NameTooLong`] when `root_name` exceeds
    /// [`MAX_ACCESSIBLE_NAME_LEN`] characters.
    pub fn new(root_name: &str) -> Result<Self, A11yError> {
        let mut builder = Self { nodes: Vec::new() };
        builder.alloc_raw(None, A11yNodeKind::Root, root_name, None, true)?;
        Ok(builder)
    }

    /// Root handle (always id `0`).
    #[must_use]
    pub fn root(&self) -> A11yNodeId {
        A11yNodeId(0)
    }

    /// Node count so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no nodes exist yet (never true after [`Self::new`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Attaches a child node under `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`A11yError::UnknownParent`] for a foreign handle,
    /// [`A11yError::UnmappedSceneKind`] for [`A11yNodeKind::Scene`] with an
    /// unmapped kind, [`A11yError::NameTooLong`] for overlong `name` or
    /// `state`, [`A11yError::TooManyNodes`] past [`MAX_A11Y_TREE_NODES`].
    pub fn push(
        &mut self,
        parent: A11yNodeId,
        kind: A11yNodeKind,
        name: &str,
        state: Option<&str>,
    ) -> Result<A11yNodeId, A11yError> {
        self.alloc_raw(Some(parent), kind, name, state, true)
    }

    /// Attaches a terminal-row content node under `parent`.
    ///
    /// Row text is content, not an accessible name: it is never
    /// length-checked, so grids wider than [`MAX_ACCESSIBLE_NAME_LEN`]
    /// cannot fail the build. Total text stays bounded by the grid itself
    /// (at most `MAX_GRID_DIM` rows of content). Every other bound —
    /// parent validity, scene mapping, node cap — still applies.
    ///
    /// # Errors
    ///
    /// Returns [`A11yError::UnknownParent`] for a foreign handle,
    /// [`A11yError::TooManyNodes`] past [`MAX_A11Y_TREE_NODES`].
    pub fn push_row_text(
        &mut self,
        parent: A11yNodeId,
        text: &str,
    ) -> Result<A11yNodeId, A11yError> {
        self.alloc_raw(Some(parent), A11yNodeKind::TerminalRow, text, None, false)
    }

    /// Freezes the builder into an immutable [`A11yTree`].
    #[must_use]
    pub fn finish(self) -> A11yTree {
        A11yTree {
            nodes: self.nodes,
            root: A11yNodeId(0),
        }
    }

    fn alloc_raw(
        &mut self,
        parent: Option<A11yNodeId>,
        kind: A11yNodeKind,
        name: &str,
        state: Option<&str>,
        check_len: bool,
    ) -> Result<A11yNodeId, A11yError> {
        if check_len {
            check_name_len(name)?;
            if let Some(active) = state {
                check_name_len(active)?;
            }
        }
        if let A11yNodeKind::Scene(scene) = kind {
            role_of(scene)?;
        }
        if self.nodes.len() >= MAX_A11Y_TREE_NODES {
            return Err(A11yError::TooManyNodes {
                count: self.nodes.len() + 1,
                cap: MAX_A11Y_TREE_NODES,
            });
        }
        if let Some(handle) = parent {
            if handle.as_usize() >= self.nodes.len() {
                return Err(A11yError::UnknownParent);
            }
        }
        let id = A11yNodeId(self.nodes.len() as u32);
        self.nodes.push(A11yNode {
            id,
            kind,
            name: name.to_string(),
            state: state.map(str::to_string),
            children: Vec::new(),
        });
        if let Some(handle) = parent {
            self.nodes[handle.as_usize()].children.push(id);
        }
        Ok(id)
    }
}

/// Builds the v1 accessibility tree for a terminal snapshot.
///
/// The root and the terminal leaf are named by the snapshot title; the
/// terminal leaf states the cursor as `cursor <row>,<col> visible|hidden`;
/// run rows nest under the terminal leaf in row order; chrome surfaces
/// follow in caller order; scene roles attach last in caller order.
///
/// Read-only: the snapshot and chrome nodes are only borrowed.
///
/// # Errors
///
/// Returns [`A11yError::NameTooLong`] for an overlong title or chrome
/// label, [`A11yError::UnmappedSceneKind`] when `scene` holds an unmapped
/// kind (validated before anything attaches), [`A11yError::TooManyNodes`]
/// past [`MAX_A11Y_TREE_NODES`].
pub fn build_a11y_tree(
    snapshot: &Snapshot,
    chrome: &[ChromeNode],
    scene: &[SceneKind],
) -> Result<A11yTree, A11yError> {
    validate_scene(scene)?;
    let title = snapshot.title.as_str();
    let mut builder = A11yTreeBuilder::new(title)?;
    let root = builder.root();
    let cursor = &snapshot.cursor;
    let cursor_state = format!(
        "cursor {},{} {}",
        cursor.position.row,
        cursor.position.col,
        if cursor.visible { "visible" } else { "hidden" }
    );
    let terminal = builder.push(root, A11yNodeKind::Terminal, title, Some(&cursor_state))?;
    for run in terminal_text_runs(snapshot) {
        builder.push_row_text(terminal, &run.text)?;
    }
    for node in chrome {
        builder.push(
            root,
            A11yNodeKind::Chrome(node.kind()),
            node.accessible_name(),
            node.active_item(),
        )?;
    }
    for kind in scene {
        let role = role_of(*kind)?;
        builder.push(root, A11yNodeKind::Scene(*kind), role.as_str(), None)?;
    }
    Ok(builder.finish())
}
