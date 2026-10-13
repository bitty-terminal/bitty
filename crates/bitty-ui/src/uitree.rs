//! Retained declarative `UiTree` model with schema, revision, and identity rules
//! (UX-13/UX-14, CTX-0662).
//!
//! Candidate implementation of U-1
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every kind name, bound, and rule below is a
//! candidate spelling that the UI Runtime RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`UiNodeKind`] — the Level-1 primitive set from the candidate
//!   (`Box`, `Text`, `Image`, `ScrollView`, `VirtualList`, `TextInput`,
//!   `Canvas`, `Terminal`, `Overlay`). Complex widgets stay Rust mechanisms
//!   with a Lua appearance: Lua emits the tree, Rust owns layout, shaping,
//!   scroll physics, hit-testing, and paint.
//! - [`UiNode`] / [`UiTree`] — a retained tree with stable [`UiNodeId`]
//!   diffing identities. Lua emits a tree only on state change; Rust
//!   reconciles revisions. There is no per-frame Lua draw loop: a tree
//!   revision schedules paint only when something changed
//!   (frame-on-demand).
//! - [`UiTree::apply_revision`] — revision/identity rules: the revision
//!   bumps only on structural change, and the report carries the
//!   invalidation set ([`UiChange`]) for layout, hit-test, and paint.
//! - [`a11y_role_of`] — the accessibility mapping for every primitive,
//!   reusing the crate [`A11yRole`](crate::a11y::A11yRole) vocabulary so no
//!   second role table is introduced.
//!
//! Relationship to the accepted Rich `Scene`/`SceneNode` model: this tree
//! composes **beside** it and subsumes nothing. The [`UiNodeKind::Terminal`]
//! primitive is a presentation attachment over terminal content, and rich
//! panel content continues on the `bitty-rich` scene path until the UI
//! Runtime RFC decides otherwise. A `UiTree` revision is presentation data:
//! it cannot mutate grid, cursor, modes, or scrollback.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, or platform handle participates; diff output is sorted by
//! node id so reports are deterministic.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::a11y::A11yRole;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on nodes per tree (including the root).
///
/// Rejected with [`UiTreeError::TooManyNodes`], never silently pruned:
/// pruning would present a partial surface as complete.
pub const MAX_UI_TREE_NODES: usize = 1024;

/// Hard cap on nesting depth (root counts as depth 1).
///
/// Rejected with [`UiTreeError::TooDeep`]: unbounded depth is an
/// unbounded recursion budget on the reconciler.
pub const MAX_UI_TREE_DEPTH: usize = 16;

/// Hard cap on children per node.
///
/// Rejected with [`UiTreeError::TooManyChildren`].
pub const MAX_UI_CHILDREN: usize = 128;

/// Hard cap in characters for inline text payloads (`Text`, `TextInput`
/// value, `TextInput` placeholder).
///
/// Rejected with [`UiTreeError::TextTooLong`], never silently truncated:
/// truncation would mislabel the surface.
pub const MAX_UI_TEXT_LEN: usize = 4096;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build or revise a [`UiTree`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiTreeError {
    /// The tree holds more than [`MAX_UI_TREE_NODES`] nodes.
    TooManyNodes {
        /// Nodes counted in the submitted tree.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Nesting exceeds [`MAX_UI_TREE_DEPTH`].
    TooDeep {
        /// Depth measured in the submitted tree (root is 1).
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A node carries more than [`MAX_UI_CHILDREN`] children.
    TooManyChildren {
        /// Children counted on the offending node.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A text payload exceeds [`MAX_UI_TEXT_LEN`] characters.
    TextTooLong {
        /// Length in characters of the rejected payload.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Two nodes share one [`UiNodeId`].
    ///
    /// Fail-closed: diffing identity must be unique, and the reconciler
    /// must not guess which node wins.
    DuplicateId {
        /// The repeated identifier.
        id: UiNodeId,
    },
    /// A leaf primitive carries children.
    ///
    /// Only container kinds (`Box`, `ScrollView`, `Overlay`) may carry
    /// children; anything else fails closed so a future container cannot
    /// appear by accident.
    ChildrenOnLeaf {
        /// The offending node.
        id: UiNodeId,
    },
}

impl fmt::Display for UiTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyNodes { found, cap } => {
                write!(f, "ui tree too large: {found} nodes exceeds cap {cap}")
            }
            Self::TooDeep { found, cap } => {
                write!(f, "ui tree too deep: depth {found} exceeds cap {cap}")
            }
            Self::TooManyChildren { found, cap } => {
                write!(
                    f,
                    "ui node has too many children: {found} exceeds cap {cap}"
                )
            }
            Self::TextTooLong { len, cap } => {
                write!(f, "ui text too long: {len} chars exceeds cap {cap}")
            }
            Self::DuplicateId { id } => write!(f, "duplicate ui node id: {id}"),
            Self::ChildrenOnLeaf { id } => {
                write!(f, "leaf ui node carries children: {id}")
            }
        }
    }
}

impl std::error::Error for UiTreeError {}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Stable diffing identity for a [`UiNode`].
///
/// Lua assigns the id and keeps it stable across revisions so Rust can
/// reconcile by identity instead of by position. Distinct newtype: it never
/// aliases `PanelId`, `ViewId`, or `TerminalId`, and no `From` bridge
/// exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UiNodeId(pub u64);

impl UiNodeId {
    /// Creates an id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UiNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UiNodeId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Schema: Level-1 primitives
// ---------------------------------------------------------------------------

/// Retained-tree node primitives (candidate Level-1 set from U-3).
///
/// Containers (`Box`, `ScrollView`, `Overlay`) carry children on the node;
/// every other variant is a leaf and must carry none
/// ([`UiTreeError::ChildrenOnLeaf`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiNodeKind {
    /// Generic container.
    Box,
    /// Static text run (bounded by [`MAX_UI_TEXT_LEN`]).
    Text(String),
    /// Image placement reference. Decode and rasterization stay
    /// Rust-owned; the tree carries the reference only.
    Image,
    /// Scrollable container (Rust owns the scroll physics).
    ScrollView,
    /// Virtualized list (Rust instantiates only visible rows; Lua defines
    /// per-item appearance and action dispatch).
    VirtualList,
    /// Text input composing with the host IME path.
    TextInput {
        /// Current value (bounded by [`MAX_UI_TEXT_LEN`]).
        value: String,
        /// Placeholder hint (bounded by [`MAX_UI_TEXT_LEN`]).
        placeholder: String,
    },
    /// Bounded display list surface.
    Canvas,
    /// Presentation attachment over terminal content.
    ///
    /// The binding to an accepted `TerminalId` is runtime routing state,
    /// not tree content: the tree carries no terminal handle, and the
    /// attachment never grants PTY ownership or terminal management
    /// authority to Lua.
    Terminal,
    /// Transient overlay container.
    Overlay,
}

impl UiNodeKind {
    /// Whether this kind may carry children.
    #[must_use]
    pub fn is_container(&self) -> bool {
        match self {
            Self::Box | Self::ScrollView | Self::Overlay => true,
            Self::Text(_)
            | Self::Image
            | Self::VirtualList
            | Self::TextInput { .. }
            | Self::Canvas
            | Self::Terminal => false,
        }
    }

    /// Candidate vocabulary spelling for this kind.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Box => "box",
            Self::Text(_) => "text",
            Self::Image => "image",
            Self::ScrollView => "scroll-view",
            Self::VirtualList => "virtual-list",
            Self::TextInput { .. } => "text-input",
            Self::Canvas => "canvas",
            Self::Terminal => "terminal",
            Self::Overlay => "overlay",
        }
    }
}

impl fmt::Display for UiNodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One retained node: stable identity, a primitive, and children for
/// container kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiNode {
    id: UiNodeId,
    kind: UiNodeKind,
    children: Vec<UiNode>,
}

impl UiNode {
    /// Builds a node.
    ///
    /// Validation (uniqueness, bounds, leaf rule) runs at
    /// [`UiTree::new`] / [`UiTree::apply_revision`], not here, so partial
    /// subtrees can be assembled before submission.
    #[must_use]
    pub fn new(id: UiNodeId, kind: UiNodeKind, children: Vec<UiNode>) -> Self {
        Self { id, kind, children }
    }

    /// Builds a leaf node.
    #[must_use]
    pub fn leaf(id: UiNodeId, kind: UiNodeKind) -> Self {
        Self {
            id,
            kind,
            children: Vec::new(),
        }
    }

    /// The stable diffing identity.
    #[must_use]
    pub const fn id(&self) -> UiNodeId {
        self.id
    }

    /// The primitive of this node.
    #[must_use]
    pub const fn kind(&self) -> &UiNodeKind {
        &self.kind
    }

    /// Child nodes (empty for leaves).
    #[must_use]
    pub fn children(&self) -> &[UiNode] {
        &self.children
    }

    /// Counts nodes in the subtree (including self).
    #[must_use]
    pub fn count_nodes(&self) -> usize {
        let mut count = 1usize;
        let mut stack: Vec<&UiNode> = self.children.iter().collect();
        while let Some(node) = stack.pop() {
            count = count.saturating_add(1);
            stack.extend(node.children.iter());
        }
        count
    }

    /// Depth of the subtree (self alone is 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut depth = 1usize;
        let mut stack: Vec<(&UiNode, usize)> =
            self.children.iter().map(|child| (child, 2usize)).collect();
        while let Some((node, node_depth)) = stack.pop() {
            depth = depth.max(node_depth);
            stack.extend(
                node.children
                    .iter()
                    .map(|child| (child, node_depth.saturating_add(1))),
            );
        }
        depth
    }

    /// Deterministic structural fingerprint of this node.
    ///
    /// Covers the kind, its payloads, and the child id sequence — but not
    /// descendants. A reorder of children therefore marks the parent
    /// [`UiChangeKind::Updated`], while an unchanged subtree keeps its
    /// fingerprint and reconciles silently.
    fn fingerprint(&self) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        let mut mix = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        };
        let (discriminant, payloads): (u64, &[&str]) = match &self.kind {
            UiNodeKind::Box => (0, &[]),
            UiNodeKind::Text(text) => (1, &[text]),
            UiNodeKind::Image => (2, &[]),
            UiNodeKind::ScrollView => (3, &[]),
            UiNodeKind::VirtualList => (4, &[]),
            UiNodeKind::TextInput { value, placeholder } => (5, &[value, placeholder]),
            UiNodeKind::Canvas => (6, &[]),
            UiNodeKind::Terminal => (7, &[]),
            UiNodeKind::Overlay => (8, &[]),
        };
        for byte in discriminant.to_le_bytes() {
            mix(byte);
        }
        for payload in payloads {
            for byte in payload.as_bytes().iter().copied() {
                mix(byte);
            }
            mix(0xff);
        }
        for child in &self.children {
            for byte in child.id.0.to_le_bytes() {
                mix(byte);
            }
        }
        hash
    }

    /// Flattens the subtree into `id -> node` (fails on duplicates).
    fn flatten<'a>(&'a self, map: &mut BTreeMap<UiNodeId, &'a UiNode>) -> Result<(), UiTreeError> {
        let mut stack: Vec<&'a UiNode> = vec![self];
        while let Some(node) = stack.pop() {
            if map.insert(node.id, node).is_some() {
                return Err(UiTreeError::DuplicateId { id: node.id });
            }
            stack.extend(node.children.iter());
        }
        Ok(())
    }

    /// Validates bounds, depth, children caps, text caps, and the leaf
    /// rule over the subtree.
    fn validate(&self) -> Result<(), UiTreeError> {
        let total = self.count_nodes();
        if total > MAX_UI_TREE_NODES {
            return Err(UiTreeError::TooManyNodes {
                found: total,
                cap: MAX_UI_TREE_NODES,
            });
        }
        let depth = self.depth();
        if depth > MAX_UI_TREE_DEPTH {
            return Err(UiTreeError::TooDeep {
                found: depth,
                cap: MAX_UI_TREE_DEPTH,
            });
        }
        let mut stack: Vec<&UiNode> = vec![self];
        while let Some(node) = stack.pop() {
            if node.children.len() > MAX_UI_CHILDREN {
                return Err(UiTreeError::TooManyChildren {
                    found: node.children.len(),
                    cap: MAX_UI_CHILDREN,
                });
            }
            if !node.kind.is_container() && !node.children.is_empty() {
                return Err(UiTreeError::ChildrenOnLeaf { id: node.id });
            }
            match &node.kind {
                UiNodeKind::Text(text) => check_text_len(text)?,
                UiNodeKind::TextInput { value, placeholder } => {
                    check_text_len(value)?;
                    check_text_len(placeholder)?;
                }
                UiNodeKind::Box
                | UiNodeKind::Image
                | UiNodeKind::ScrollView
                | UiNodeKind::VirtualList
                | UiNodeKind::Canvas
                | UiNodeKind::Terminal
                | UiNodeKind::Overlay => {}
            }
            stack.extend(node.children.iter());
        }
        Ok(())
    }
}

fn check_text_len(text: &str) -> Result<(), UiTreeError> {
    let len = text.chars().count();
    if len > MAX_UI_TEXT_LEN {
        return Err(UiTreeError::TextTooLong {
            len,
            cap: MAX_UI_TEXT_LEN,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff / invalidation
// ---------------------------------------------------------------------------

/// How one node changed between two revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiChangeKind {
    /// Present in the new tree only.
    Added,
    /// Present in the old tree only.
    Removed,
    /// Present in both with a different fingerprint (payload or child
    /// sequence changed).
    Updated,
}

impl UiChangeKind {
    /// Candidate vocabulary spelling for this change.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Updated => "updated",
        }
    }
}

impl fmt::Display for UiChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the invalidation set: the node and how it changed.
///
/// Layout, hit-test, accessibility, and paint consume this set; nodes
/// absent from it reconcile silently. Entries are sorted by node id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiChange {
    /// The changed node.
    pub id: UiNodeId,
    /// How it changed.
    pub kind: UiChangeKind,
}

/// Computes the invalidation set between two validated trees.
///
/// Identity-set diff over [`UiNodeId`]: added, removed, and
/// fingerprint-changed nodes. A pure subtree move without payload or
/// child-sequence change is not an update; deeper structural
/// reconciliation (order-preserving patch plans) belongs to the
/// owner-pending UI Runtime RFC and is not claimed here.
#[must_use]
pub fn diff_trees(old: &UiNode, new: &UiNode) -> Vec<UiChange> {
    let mut old_map = BTreeMap::new();
    let mut new_map = BTreeMap::new();
    // Both trees validated at construction; duplicates are unreachable.
    // A best-effort flatten still yields a deterministic (possibly
    // partial) diff rather than panicking.
    let _ = old.flatten(&mut old_map);
    let _ = new.flatten(&mut new_map);
    let mut changes = Vec::new();
    for (id, old_node) in &old_map {
        match new_map.get(id) {
            None => changes.push(UiChange {
                id: *id,
                kind: UiChangeKind::Removed,
            }),
            Some(new_node) => {
                if old_node.fingerprint() != new_node.fingerprint() {
                    changes.push(UiChange {
                        id: *id,
                        kind: UiChangeKind::Updated,
                    });
                }
            }
        }
    }
    for id in new_map.keys() {
        if !old_map.contains_key(id) {
            changes.push(UiChange {
                id: *id,
                kind: UiChangeKind::Added,
            });
        }
    }
    changes.sort_by_key(|change| (change.id, change.kind as u8));
    changes
}

// ---------------------------------------------------------------------------
// Retained tree with revision
// ---------------------------------------------------------------------------

/// Report of [`UiTree::apply_revision`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// Revision after the call (bumped only when `changed`).
    pub revision: u64,
    /// Whether the submitted tree differs structurally.
    pub changed: bool,
    /// Invalidation set (empty when unchanged).
    pub changes: Vec<UiChange>,
}

/// Retained declarative tree with a monotonic revision.
///
/// The revision starts at `0`, bumps by one per structural change, and
/// never bumps for an identical resubmission: paint schedules only on
/// change (frame-on-demand). The tree holds validated content only —
/// construction and every revision run the full validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTree {
    root: UiNode,
    revision: u64,
}

impl UiTree {
    /// Builds a retained tree from a root node.
    ///
    /// # Errors
    ///
    /// Returns [`UiTreeError`] when the tree violates any bound, repeats
    /// an id, or puts children on a leaf.
    pub fn new(root: UiNode) -> Result<Self, UiTreeError> {
        root.validate()?;
        let mut ids = BTreeMap::new();
        root.flatten(&mut ids)?;
        Ok(Self { root, revision: 0 })
    }

    /// Current revision (starts at `0`).
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Current root.
    #[must_use]
    pub const fn root(&self) -> &UiNode {
        &self.root
    }

    /// Applies a new revision.
    ///
    /// The submission is validated first: a rejected tree never touches
    /// the retained state. An identical tree returns `changed: false`
    /// with the revision untouched; a different tree replaces the root,
    /// bumps the revision by one (wrapping on `u64::MAX`; tree equality —
    /// not the counter — drives paint), and reports the invalidation
    /// set.
    ///
    /// # Errors
    ///
    /// Returns [`UiTreeError`] for an invalid submission; the retained
    /// tree is unchanged.
    pub fn apply_revision(&mut self, root: UiNode) -> Result<ApplyReport, UiTreeError> {
        root.validate()?;
        let mut ids = BTreeMap::new();
        root.flatten(&mut ids)?;
        let changes = diff_trees(&self.root, &root);
        if changes.is_empty() {
            return Ok(ApplyReport {
                revision: self.revision,
                changed: false,
                changes,
            });
        }
        self.root = root;
        self.revision = self.revision.wrapping_add(1);
        Ok(ApplyReport {
            revision: self.revision,
            changed: true,
            changes,
        })
    }
}

// ---------------------------------------------------------------------------
// Accessibility mapping
// ---------------------------------------------------------------------------

/// Maps a primitive to its accessible role.
///
/// Total: every [`UiNodeKind`] maps, reusing the crate [`A11yRole`]
/// vocabulary. Interactive semantics stay disciplined — `TextInput`
/// exposes `input`; no primitive guesses a button role. Visual styling is
/// free; this mapping is the fixed semantic half.
#[must_use]
pub fn a11y_role_of(kind: &UiNodeKind) -> A11yRole {
    match kind {
        UiNodeKind::Box | UiNodeKind::ScrollView | UiNodeKind::Canvas | UiNodeKind::Overlay => {
            A11yRole::Group
        }
        UiNodeKind::Text(_) | UiNodeKind::Terminal => A11yRole::Text,
        UiNodeKind::Image => A11yRole::Image,
        UiNodeKind::VirtualList => A11yRole::List,
        UiNodeKind::TextInput { .. } => A11yRole::Input,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text(id: u64, body: &str) -> UiNode {
        UiNode::leaf(UiNodeId::new(id), UiNodeKind::Text(body.to_string()))
    }

    fn container(id: u64, kind: UiNodeKind, children: Vec<UiNode>) -> UiNode {
        UiNode::new(UiNodeId::new(id), kind, children)
    }

    #[test]
    fn new_accepts_valid_tree_at_revision_zero() {
        let tree = UiTree::new(container(
            1,
            UiNodeKind::Box,
            vec![
                text(2, "hello"),
                UiNode::leaf(UiNodeId::new(3), UiNodeKind::Terminal),
            ],
        ))
        .expect("valid tree");
        assert_eq!(tree.revision(), 0);
        assert_eq!(tree.root().count_nodes(), 3);
    }

    #[test]
    fn duplicate_ids_fail_closed() {
        let err = UiTree::new(container(
            1,
            UiNodeKind::Box,
            vec![text(2, "a"), text(2, "b")],
        ))
        .expect_err("duplicate ids must fail");
        assert_eq!(
            err,
            UiTreeError::DuplicateId {
                id: UiNodeId::new(2)
            }
        );
    }

    #[test]
    fn children_on_leaf_fail_closed() {
        let err = UiTree::new(UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Text("x".to_string()),
            vec![text(2, "y")],
        ))
        .expect_err("children on a leaf must fail");
        assert_eq!(
            err,
            UiTreeError::ChildrenOnLeaf {
                id: UiNodeId::new(1)
            }
        );
    }

    #[test]
    fn oversized_text_fails_closed() {
        let big = "x".repeat(MAX_UI_TEXT_LEN + 1);
        let err = UiTree::new(text(1, &big)).expect_err("oversized text must fail");
        assert_eq!(
            err,
            UiTreeError::TextTooLong {
                len: MAX_UI_TEXT_LEN + 1,
                cap: MAX_UI_TEXT_LEN
            }
        );
    }

    #[test]
    fn identical_resubmission_does_not_bump_revision() {
        let root = || container(1, UiNodeKind::Box, vec![text(2, "same")]);
        let mut tree = UiTree::new(root()).expect("valid tree");
        let report = tree.apply_revision(root()).expect("valid revision");
        assert!(!report.changed);
        assert!(report.changes.is_empty());
        assert_eq!(report.revision, 0);
        assert_eq!(tree.revision(), 0);
    }

    #[test]
    fn payload_change_bumps_revision_with_updated_entry() {
        let mut tree =
            UiTree::new(container(1, UiNodeKind::Box, vec![text(2, "old")])).expect("valid tree");
        let report = tree
            .apply_revision(container(1, UiNodeKind::Box, vec![text(2, "new")]))
            .expect("valid revision");
        assert!(report.changed);
        assert_eq!(report.revision, 1);
        assert_eq!(tree.revision(), 1);
        assert!(
            report.changes.contains(&UiChange {
                id: UiNodeId::new(2),
                kind: UiChangeKind::Updated
            }),
            "payload change must invalidate the node: {report:?}"
        );
    }

    #[test]
    fn added_and_removed_nodes_appear_in_order() {
        let mut tree =
            UiTree::new(container(1, UiNodeKind::Box, vec![text(2, "keep")])).expect("valid tree");
        let report = tree
            .apply_revision(container(
                1,
                UiNodeKind::Box,
                vec![text(2, "keep"), text(9, "new")],
            ))
            .expect("valid revision");
        assert!(report.changed);
        let kinds: Vec<(u64, UiChangeKind)> = report
            .changes
            .iter()
            .map(|change| (change.id.get(), change.kind))
            .collect();
        assert!(
            kinds.contains(&(1, UiChangeKind::Updated)),
            "parent child-sequence change must invalidate the parent: {kinds:?}"
        );
        assert!(kinds.contains(&(9, UiChangeKind::Added)), "{kinds:?}");
    }

    #[test]
    fn rejected_revision_keeps_retained_state() {
        let mut tree =
            UiTree::new(container(1, UiNodeKind::Box, vec![text(2, "kept")])).expect("valid tree");
        let bad = UiNode::new(
            UiNodeId::new(1),
            UiNodeKind::Text("bad".to_string()),
            vec![text(2, "kept")],
        );
        let err = tree
            .apply_revision(bad)
            .expect_err("leaf children must fail");
        assert_eq!(
            err,
            UiTreeError::ChildrenOnLeaf {
                id: UiNodeId::new(1)
            }
        );
        assert_eq!(tree.revision(), 0);
        assert_eq!(tree.root().count_nodes(), 2);
    }

    #[test]
    fn a11y_mapping_is_total() {
        let kinds = [
            UiNodeKind::Box,
            UiNodeKind::Text("t".to_string()),
            UiNodeKind::Image,
            UiNodeKind::ScrollView,
            UiNodeKind::VirtualList,
            UiNodeKind::TextInput {
                value: String::new(),
                placeholder: String::new(),
            },
            UiNodeKind::Canvas,
            UiNodeKind::Terminal,
            UiNodeKind::Overlay,
        ];
        for kind in &kinds {
            let _ = a11y_role_of(kind);
        }
        assert_eq!(a11y_role_of(&UiNodeKind::Terminal), A11yRole::Text);
        assert_eq!(
            a11y_role_of(&UiNodeKind::TextInput {
                value: String::new(),
                placeholder: String::new(),
            }),
            A11yRole::Input
        );
        assert_eq!(a11y_role_of(&UiNodeKind::VirtualList), A11yRole::List);
    }

    #[test]
    fn error_display_is_human_readable() {
        let err = UiTreeError::DuplicateId {
            id: UiNodeId::new(7),
        };
        assert_eq!(err.to_string(), "duplicate ui node id: UiNodeId(7)");
    }
}
