//! LayoutProvider plugin path (CW-07, issue #986).
//!
//! Contract source: the workspace-compositor specification, section
//! "Layout algorithms as plugin via LayoutProvider" (accepted via CTX-0118).
//! Layout algorithms are not Core built-ins: Core owns the `H`/`V`
//! primitives ([`LayoutNode`]), decoration, and commit validation, while
//! `LayoutProvider` plugins contribute only geometry proposals.
//!
//! What this module provides:
//!
//! - [`LayoutProvider`] — the provider trait: `id`, `name`, and a pure
//!   `propose` function of snapshot, view set, and available area.
//! - [`ProviderName`] — validated provider names: bare `dwindle`, `master`,
//!   and `grid` are reserved for the canonical providers; anything else
//!   registers under a qualified `owner.name:algorithm` name.
//! - [`validate_proposal`] — Core-side validation of every returned tree
//!   (well-formedness, view-set consistency, `ratio` bounds) before commit.
//!   Invalid proposals are rejected; the previous tree is retained by the
//!   caller.
//! - [`ProviderRegistry`] — name-keyed provider set with the
//!   `layout.provider` capability gate ([`LAYOUT_PROVIDER_CAPABILITY`]).
//!   The built-in no-op tiler ([`NoopTiler`]) needs no capability.
//! - Canonical providers: [`DwindleProvider`], [`MasterProvider`],
//!   [`GridProvider`]. They build trees from `H`/`V` splits and `View`
//!   leaves only, reuse the snapshot's `View`s when available (so per-leaf
//!   presentation modes survive a recompose), and are deterministic.
//!
//! # Role and dependency rule
//!
//! This module lives in `bitty-ui` because `bitty-ui` owns `View` and
//! `LayoutNode`. It performs no filesystem, network, or PTY access, holds
//! no mutable `Workspace` handle, and proposes no decoration values.
//! Capability *enforcement* stays with the host: registration carries an
//! explicit grant flag so `bitty-ui` never depends on `bitty-plugin-host`.
//!
//! # Determinism
//!
//! Every provider is a pure function of its inputs: no wall-clock time,
//! randomness, or platform variance participates. Ratios are fixed at
//! [`DEFAULT_PROVIDER_RATIO`]; leaf order follows the input view order.

#![forbid(unsafe_code)]

use crate::geometry::{Rect, SplitAxis};
use crate::layout::LayoutNode;
use crate::view::{View, ViewId};

/// Capability that gates `LayoutProvider` registration.
///
/// Mirrors the closed identifier `layout.provider` in `bitty-package` and
/// `bitty-plugin-host`. The registry takes an explicit grant flag instead
/// of depending on the host crate.
pub const LAYOUT_PROVIDER_CAPABILITY: &str = "layout.provider";

/// Name of the built-in no-op tiler that preserves the current tree.
/// Needs no capability.
pub const NOOP_PROVIDER_NAME: &str = "noop";

/// Bare names reserved for the canonical providers.
pub const RESERVED_PROVIDER_NAMES: &[&str] = &["dwindle", "master", "grid"];

/// Maximum provider name length (the contract's `BoundedString<32>`).
pub const MAX_PROVIDER_NAME_LEN: usize = 32;

/// Fixed split ratio used by every canonical provider (always valid).
pub const DEFAULT_PROVIDER_RATIO: f32 = 0.5;

/// Built-in provider id for the no-op tiler.
pub const NOOP_PROVIDER_ID: ProviderId = ProviderId(0);
/// Built-in provider id for dwindle.
pub const DWINDLE_PROVIDER_ID: ProviderId = ProviderId(1);
/// Built-in provider id for master.
pub const MASTER_PROVIDER_ID: ProviderId = ProviderId(2);
/// Built-in provider id for grid.
pub const GRID_PROVIDER_ID: ProviderId = ProviderId(3);

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Opaque identifier for a registered layout provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(pub u64);

impl ProviderId {
    /// Creates a provider id from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProviderId({})", self.0)
    }
}

/// Validated provider name.
///
/// Grammar (all lowercase, total length `1..=32`):
///
/// - bare: `[a-z][a-z0-9_-]*` — `dwindle`, `master`, and `grid` are
///   reserved for the canonical providers;
/// - qualified: `owner.name:algorithm` — the head is dot-separated with at
///   least two segments (e.g. `acme.tiling`), each a bare segment, and the
///   algorithm is a bare segment.
///
/// Spelling validation accepts any well-formed name; *registration*
/// membership (unknown names fail) is enforced by [`ProviderRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderName {
    raw: String,
    canonical: bool,
}

impl ProviderName {
    /// Parses and validates a provider name.
    ///
    /// # Errors
    ///
    /// [`LayoutError::InvalidName`] when the name is empty, too long, or
    /// violates the grammar above.
    pub fn parse(raw: &str) -> Result<Self, LayoutError> {
        if raw.is_empty() {
            return Err(LayoutError::InvalidName {
                name: raw.to_string(),
                reason: "provider name must not be empty",
            });
        }
        if raw.len() > MAX_PROVIDER_NAME_LEN {
            return Err(LayoutError::InvalidName {
                name: raw.to_string(),
                reason: "provider name exceeds 32 bytes",
            });
        }
        if let Some((head, algo)) = raw.split_once(':') {
            if raw.chars().filter(|&c| c == ':').count() > 1 {
                return Err(LayoutError::InvalidName {
                    name: raw.to_string(),
                    reason: "qualified name holds a single ':'",
                });
            }
            if algo.is_empty() || !is_bare_segment(algo) {
                return Err(LayoutError::InvalidName {
                    name: raw.to_string(),
                    reason: "algorithm after ':' must be [a-z][a-z0-9_-]*",
                });
            }
            let segments: Vec<&str> = head.split('.').collect();
            if segments.len() < 2 || segments.iter().any(|s| !is_bare_segment(s)) {
                return Err(LayoutError::InvalidName {
                    name: raw.to_string(),
                    reason: "owner head must be dot-separated (owner.name)",
                });
            }
            return Ok(Self {
                raw: raw.to_string(),
                canonical: false,
            });
        }
        if !is_bare_segment(raw) {
            return Err(LayoutError::InvalidName {
                name: raw.to_string(),
                reason: "bare name must be [a-z][a-z0-9_-]*",
            });
        }
        Ok(Self {
            raw: raw.to_string(),
            canonical: RESERVED_PROVIDER_NAMES.contains(&raw),
        })
    }

    /// Raw name string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether this is a reserved canonical name (`dwindle`/`master`/`grid`).
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        self.canonical
    }
}

impl std::fmt::Display for ProviderName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

fn is_bare_segment(seg: &str) -> bool {
    if seg.is_empty() || seg.len() > MAX_PROVIDER_NAME_LEN {
        return false;
    }
    let mut chars = seg.bytes();
    let first = chars.next().unwrap_or(b' ');
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed failure for the provider path.
///
/// Every rejection carries an attributed diagnostic; Core never falls back
/// to a silent default and never commits a rejected proposal.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// Provider name violates the [`ProviderName`] grammar.
    InvalidName { name: String, reason: &'static str },
    /// No provider is registered under this name.
    UnknownProvider { name: String },
    /// Registration without the `layout.provider` capability grant.
    CapabilityDenied { name: String },
    /// Bare reserved name claimed by a non-canonical registration.
    ReservedName { name: String },
    /// A provider under this name is already registered.
    DuplicateName { name: String },
    /// A provider with this id is already registered.
    DuplicateId { id: u64 },
    /// `propose` or validation received an empty view set.
    EmptyViewSet,
    /// Proposal contains no leaves.
    EmptyProposal,
    /// Proposal repeats a `ViewId`.
    DuplicateView { id: u64 },
    /// Proposal drops a requested `ViewId`.
    MissingView { id: u64 },
    /// Proposal invents a `ViewId` outside the requested set.
    UnexpectedView { id: u64 },
    /// A split `ratio` falls outside `[0.1, 0.9]` (rejected, never clamped).
    RatioOutOfBounds { ratio: f32 },
    /// Proposal uses a non-`H`/`V` interior (`Stack` or `Overlay`).
    UnsupportedNode { kind: &'static str },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName { name, reason } => {
                write!(f, "invalid layout provider name '{name}': {reason}")
            }
            Self::UnknownProvider { name } => {
                write!(f, "unknown layout provider '{name}'")
            }
            Self::CapabilityDenied { name } => {
                write!(
                    f,
                    "registering layout provider '{name}' requires the '{LAYOUT_PROVIDER_CAPABILITY}' capability"
                )
            }
            Self::ReservedName { name } => {
                write!(
                    f,
                    "layout provider name '{name}' is reserved for the canonical provider"
                )
            }
            Self::DuplicateName { name } => {
                write!(f, "layout provider '{name}' is already registered")
            }
            Self::DuplicateId { id } => {
                write!(f, "layout provider id {id} is already registered")
            }
            Self::EmptyViewSet => write!(f, "layout proposal needs at least one view"),
            Self::EmptyProposal => write!(f, "layout proposal contains no views"),
            Self::DuplicateView { id } => {
                write!(f, "layout proposal repeats ViewId({id})")
            }
            Self::MissingView { id } => {
                write!(f, "layout proposal drops requested ViewId({id})")
            }
            Self::UnexpectedView { id } => {
                write!(f, "layout proposal invents unrequested ViewId({id})")
            }
            Self::RatioOutOfBounds { ratio } => {
                write!(f, "layout proposal ratio {ratio} is outside [0.1, 0.9]")
            }
            Self::UnsupportedNode { kind } => {
                write!(
                    f,
                    "layout proposal uses '{kind}' (only H and V splits are allowed)"
                )
            }
        }
    }
}

impl std::error::Error for LayoutError {}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Available area for a proposal, in logical pixels.
///
/// Providers use it only for axis decisions (e.g. dwindle's opening axis);
/// decoration stays Core-owned and no proposal carries decoration values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    /// Left edge in logical px.
    pub x: f64,
    /// Top edge in logical px.
    pub y: f64,
    /// Width in logical px.
    pub width: f64,
    /// Height in logical px.
    pub height: f64,
}

impl LogicalRect {
    /// Builds an area. Total over all inputs: non-finite or non-positive
    /// dimensions are legal values and simply read as "no usable area"
    /// (providers fall back to deterministic defaults).
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Converts a cell `Rect` to a logical area (1 cell = 1 logical unit
    /// for axis decisions; never a pixel-metric claim).
    #[must_use]
    pub fn from_rect(rect: Rect) -> Self {
        Self::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.width),
            f64::from(rect.height),
        )
    }

    /// Whether this area is empty or unusable for axis decisions.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
    }
}

/// Read-only workspace state handed to [`LayoutProvider::propose`].
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSnapshot {
    /// Views the proposal must lay out, in deterministic order.
    pub views: Vec<ViewId>,
    /// Current tree (lets the no-op tiler preserve it and lets providers
    /// reuse `View` values so per-leaf modes survive a recompose).
    pub current: LayoutNode,
}

impl WorkspaceSnapshot {
    /// Builds a snapshot.
    #[must_use]
    pub fn new(views: Vec<ViewId>, current: LayoutNode) -> Self {
        Self { views, current }
    }
}

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

/// Plugin-supplied layout algorithm.
///
/// `propose` is a pure function of the snapshot, view set, and available
/// area: it holds no mutable `Workspace` handle and performs no
/// filesystem, network, or PTY access. Same inputs yield the same tree;
/// nondeterminism is a conformance violation.
pub trait LayoutProvider: Send + Sync {
    /// Stable provider id.
    fn id(&self) -> ProviderId;
    /// Validated provider name.
    fn name(&self) -> &ProviderName;
    /// Proposes a tree for `views` in `area`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::EmptyViewSet`] when `views` is empty; providers may
    /// also surface validation-style errors instead of building an
    /// invalid tree. Core always runs [`validate_proposal`] before commit.
    fn propose(
        &self,
        workspace: &WorkspaceSnapshot,
        views: &[ViewId],
        area: LogicalRect,
    ) -> Result<LayoutNode, LayoutError>;
}

// ---------------------------------------------------------------------------
// Core validation
// ---------------------------------------------------------------------------

/// Validates a provider proposal before commit.
///
/// Checks, in order: non-empty view set, `H`/`V`-only interiors, `ratio`
/// within `[0.1, 0.9]` (rejected, never clamped), at least one leaf, no
/// duplicate leaves, and an exact leaf-set match with `views` (missing or
/// invented `ViewId`s are rejected — providers cannot authorize
/// view creation or removal through `propose`).
///
/// # Errors
///
/// [`LayoutError::EmptyViewSet`], [`LayoutError::UnsupportedNode`],
/// [`LayoutError::RatioOutOfBounds`], [`LayoutError::EmptyProposal`],
/// [`LayoutError::DuplicateView`], [`LayoutError::MissingView`], or
/// [`LayoutError::UnexpectedView`].
pub fn validate_proposal(tree: &LayoutNode, views: &[ViewId]) -> Result<(), LayoutError> {
    if views.is_empty() {
        return Err(LayoutError::EmptyViewSet);
    }
    let mut leaves: Vec<ViewId> = Vec::new();
    collect_proposal_leaves(tree, &mut leaves)?;
    if leaves.is_empty() {
        return Err(LayoutError::EmptyProposal);
    }
    let mut seen: Vec<ViewId> = Vec::with_capacity(leaves.len());
    for id in &leaves {
        if seen.contains(id) {
            return Err(LayoutError::DuplicateView { id: id.0 });
        }
        seen.push(*id);
    }
    for id in views {
        if !seen.contains(id) {
            return Err(LayoutError::MissingView { id: id.0 });
        }
    }
    for id in &seen {
        if !views.contains(id) {
            return Err(LayoutError::UnexpectedView { id: id.0 });
        }
    }
    Ok(())
}

fn collect_proposal_leaves(tree: &LayoutNode, out: &mut Vec<ViewId>) -> Result<(), LayoutError> {
    match tree {
        LayoutNode::Leaf(view) => {
            out.push(view.id());
            Ok(())
        }
        LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !ratio.is_finite()
                || *ratio < LayoutNode::MIN_RATIO
                || *ratio > LayoutNode::MAX_RATIO
            {
                return Err(LayoutError::RatioOutOfBounds { ratio: *ratio });
            }
            collect_proposal_leaves(first, out)?;
            collect_proposal_leaves(second, out)
        }
        LayoutNode::Stack(_) => Err(LayoutError::UnsupportedNode { kind: "Stack" }),
        LayoutNode::Overlay { .. } => Err(LayoutError::UnsupportedNode { kind: "Overlay" }),
    }
}

/// Leaf `View` for `id`: reuses the snapshot's value when present (so
/// per-leaf presentation modes survive a recompose), else a minimal
/// placeholder Core reflows after commit.
fn leaf_for(snapshot: &WorkspaceSnapshot, id: ViewId) -> LayoutNode {
    match snapshot.current.find_leaf(id) {
        Some(view) => LayoutNode::leaf(view.clone()),
        None => LayoutNode::leaf(View::new(id, 1, 1)),
    }
}

/// Chains `ids` into nested splits along `axis` at a fixed ratio.
fn chain(axis: SplitAxis, ids: &[ViewId], snapshot: &WorkspaceSnapshot) -> LayoutNode {
    let mut iter = ids.iter().rev();
    let last = iter.next().expect("chain needs at least one view");
    let mut tree = leaf_for(snapshot, *last);
    for id in iter {
        tree = LayoutNode::split(axis, DEFAULT_PROVIDER_RATIO, leaf_for(snapshot, *id), tree);
    }
    tree
}

/// Opening axis from the available area, mirroring the `smart_split`
/// heuristic: tall areas stack first, wide and square areas start
/// side-by-side. Empty areas fall back to side-by-side.
fn opening_axis(area: LogicalRect) -> SplitAxis {
    if !area.is_empty() && area.height > area.width {
        SplitAxis::Vertical
    } else {
        SplitAxis::Horizontal
    }
}

// ---------------------------------------------------------------------------
// Canonical providers
// ---------------------------------------------------------------------------

/// Built-in no-op tiler: preserves the current tree.
///
/// Needs no capability. View-set consistency is still enforced by Core
/// validation after `propose` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoopTiler {
    name: ProviderName,
}

impl NoopTiler {
    /// Creates the no-op tiler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: ProviderName::parse(NOOP_PROVIDER_NAME).expect("noop is a valid name"),
        }
    }
}

impl Default for NoopTiler {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutProvider for NoopTiler {
    fn id(&self) -> ProviderId {
        NOOP_PROVIDER_ID
    }

    fn name(&self) -> &ProviderName {
        &self.name
    }

    fn propose(
        &self,
        workspace: &WorkspaceSnapshot,
        views: &[ViewId],
        _area: LogicalRect,
    ) -> Result<LayoutNode, LayoutError> {
        if views.is_empty() {
            return Err(LayoutError::EmptyViewSet);
        }
        Ok(workspace.current.clone())
    }
}

/// Dwindle provider: recursive `H`/`V` splits that spiral inward.
///
/// The first view is outermost; each remaining view splits the remainder,
/// with the axis alternating per depth. The opening axis follows the area
/// aspect (tall stacks first, wide/square starts side-by-side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DwindleProvider {
    name: ProviderName,
}

impl DwindleProvider {
    /// Creates the canonical dwindle provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: ProviderName::parse("dwindle").expect("dwindle is a reserved name"),
        }
    }
}

impl Default for DwindleProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutProvider for DwindleProvider {
    fn id(&self) -> ProviderId {
        DWINDLE_PROVIDER_ID
    }

    fn name(&self) -> &ProviderName {
        &self.name
    }

    fn propose(
        &self,
        workspace: &WorkspaceSnapshot,
        views: &[ViewId],
        area: LogicalRect,
    ) -> Result<LayoutNode, LayoutError> {
        if views.is_empty() {
            return Err(LayoutError::EmptyViewSet);
        }
        // Outermost first: views[0] takes the opening axis, each deeper
        // level flips it, so the spiral alternates per depth from the root.
        let mut axes = Vec::with_capacity(views.len().saturating_sub(1));
        let mut axis = opening_axis(area);
        for _ in 0..views.len().saturating_sub(1) {
            axes.push(axis);
            axis = match axis {
                SplitAxis::Horizontal => SplitAxis::Vertical,
                SplitAxis::Vertical => SplitAxis::Horizontal,
            };
        }
        let mut iter = views.iter().rev();
        let last = iter.next().expect("views are non-empty");
        let mut tree = leaf_for(workspace, *last);
        for (id, axis) in views[..views.len() - 1].iter().zip(axes.iter()).rev() {
            tree = LayoutNode::split(
                *axis,
                DEFAULT_PROVIDER_RATIO,
                leaf_for(workspace, *id),
                tree,
            );
        }
        Ok(tree)
    }
}

/// Master provider: one master `View` on the left at a fixed ratio with
/// the remaining `View`s stacked vertically on the right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterProvider {
    name: ProviderName,
}

impl MasterProvider {
    /// Creates the canonical master provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: ProviderName::parse("master").expect("master is a reserved name"),
        }
    }
}

impl Default for MasterProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutProvider for MasterProvider {
    fn id(&self) -> ProviderId {
        MASTER_PROVIDER_ID
    }

    fn name(&self) -> &ProviderName {
        &self.name
    }

    fn propose(
        &self,
        workspace: &WorkspaceSnapshot,
        views: &[ViewId],
        _area: LogicalRect,
    ) -> Result<LayoutNode, LayoutError> {
        let Some((master, rest)) = views.split_first() else {
            return Err(LayoutError::EmptyViewSet);
        };
        let master_leaf = leaf_for(workspace, *master);
        if rest.is_empty() {
            return Ok(master_leaf);
        }
        let stack = chain(SplitAxis::Vertical, rest, workspace);
        Ok(LayoutNode::split(
            SplitAxis::Horizontal,
            DEFAULT_PROVIDER_RATIO,
            master_leaf,
            stack,
        ))
    }
}

/// Grid provider: views arranged in a near-square grid using only `H`
/// and `V` splits.
///
/// Rows are balanced (lengths differ by at most one, longer rows first);
/// a wide area favors more columns, a tall area more rows. A ragged last
/// row simply holds fewer leaves: the tree never invents `ViewId`s for
/// empty cells (that would fail view-set validation), so Core renders the
/// ragged edge and any empty-cell placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridProvider {
    name: ProviderName,
}

impl GridProvider {
    /// Creates the canonical grid provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            name: ProviderName::parse("grid").expect("grid is a reserved name"),
        }
    }
}

impl Default for GridProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutProvider for GridProvider {
    fn id(&self) -> ProviderId {
        GRID_PROVIDER_ID
    }

    fn name(&self) -> &ProviderName {
        &self.name
    }

    fn propose(
        &self,
        workspace: &WorkspaceSnapshot,
        views: &[ViewId],
        area: LogicalRect,
    ) -> Result<LayoutNode, LayoutError> {
        if views.is_empty() {
            return Err(LayoutError::EmptyViewSet);
        }
        if views.len() == 1 {
            return Ok(leaf_for(workspace, views[0]));
        }
        let n = views.len();
        let base = n.isqrt().max(1);
        // Wide areas favor columns (fewer rows), tall areas favor rows.
        let rows = if area.is_empty() || area.width >= area.height {
            base
        } else {
            n.div_ceil(base)
        };
        // Balanced rows: first `extra` rows hold `per_row + 1` views.
        let per_row = n / rows;
        let extra = n % rows;
        let mut start = 0;
        let mut row_trees: Vec<LayoutNode> = Vec::with_capacity(rows);
        for r in 0..rows {
            let len = per_row + usize::from(r < extra);
            let row = &views[start..start + len];
            start += len;
            row_trees.push(chain(SplitAxis::Horizontal, row, workspace));
        }
        let mut iter = row_trees.into_iter().rev();
        let mut tree = iter.next().expect("at least one row");
        for row in iter {
            tree = LayoutNode::split(SplitAxis::Vertical, DEFAULT_PROVIDER_RATIO, row, tree);
        }
        Ok(tree)
    }
}

// ---------------------------------------------------------------------------
// Registry with capability gate
// ---------------------------------------------------------------------------

/// Name-keyed provider set with the `layout.provider` capability gate.
///
/// Registering a provider requires the capability grant, except for the
/// built-in no-op tiler. The caller passes the grant explicitly
/// (`has_layout_provider_capability`) so this crate never depends on the
/// plugin host. Bare `dwindle`/`master`/`grid` names stay reserved for the
/// canonical providers; unknown names fail [`Self::require`].
pub struct ProviderRegistry {
    providers: Vec<Box<dyn LayoutProvider>>,
}

impl ProviderRegistry {
    /// Creates an empty registry (no providers, not even canonical ones).
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Creates a registry with the canonical providers pre-registered
    /// (`noop` without a capability, `dwindle`/`master`/`grid` as Core).
    #[must_use]
    pub fn with_canonical() -> Self {
        let mut registry = Self::new();
        // Infallible by construction: distinct names and ids, no-op exempt
        // from the capability gate, canonical names carry canonical ids.
        registry
            .register(Box::new(NoopTiler::new()), false)
            .expect("canonical noop registers");
        registry
            .register(Box::new(DwindleProvider::new()), true)
            .expect("canonical dwindle registers");
        registry
            .register(Box::new(MasterProvider::new()), true)
            .expect("canonical master registers");
        registry
            .register(Box::new(GridProvider::new()), true)
            .expect("canonical grid registers");
        registry
    }

    /// Registers a provider.
    ///
    /// Requires `has_layout_provider_capability` unless the provider is
    /// the built-in no-op tiler. Bare reserved names are rejected for
    /// non-canonical registrations.
    ///
    /// # Errors
    ///
    /// [`LayoutError::CapabilityDenied`], [`LayoutError::ReservedName`],
    /// [`LayoutError::DuplicateName`], or [`LayoutError::DuplicateId`].
    pub fn register(
        &mut self,
        provider: Box<dyn LayoutProvider>,
        has_layout_provider_capability: bool,
    ) -> Result<(), LayoutError> {
        let name = provider.name().as_str().to_string();
        if name != NOOP_PROVIDER_NAME && !has_layout_provider_capability {
            return Err(LayoutError::CapabilityDenied { name });
        }
        if provider.name().is_canonical() && provider.id() != canonical_id(&name) {
            return Err(LayoutError::ReservedName { name });
        }
        if self.providers.iter().any(|p| p.name().as_str() == name) {
            return Err(LayoutError::DuplicateName { name });
        }
        if self.providers.iter().any(|p| p.id() == provider.id()) {
            return Err(LayoutError::DuplicateId {
                id: provider.id().0,
            });
        }
        self.providers.push(provider);
        Ok(())
    }

    /// Looks up a provider by name (spelling unchecked).
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&dyn LayoutProvider> {
        self.providers
            .iter()
            .find(|p| p.name().as_str() == name)
            .map(AsRef::as_ref)
    }

    /// Looks up a provider by name with validation.
    ///
    /// # Errors
    ///
    /// [`LayoutError::InvalidName`] for malformed names,
    /// [`LayoutError::UnknownProvider`] for well-formed but unregistered ones.
    pub fn require(&self, name: &str) -> Result<&dyn LayoutProvider, LayoutError> {
        let parsed = ProviderName::parse(name)?;
        self.resolve(parsed.as_str())
            .ok_or_else(|| LayoutError::UnknownProvider {
                name: parsed.as_str().to_string(),
            })
    }

    /// Registered names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.providers
            .iter()
            .map(|p| p.name().as_str().to_string())
            .collect()
    }

    /// Number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no provider is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.names())
            .finish()
    }
}

fn canonical_id(name: &str) -> ProviderId {
    match name {
        "dwindle" => DWINDLE_PROVIDER_ID,
        "master" => MASTER_PROVIDER_ID,
        "grid" => GRID_PROVIDER_ID,
        _ => ProviderId(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_ids(n: u64) -> Vec<ViewId> {
        (1..=n).map(ViewId::new).collect()
    }

    fn snapshot(n: u64) -> WorkspaceSnapshot {
        let views = view_ids(n);
        let current = if n == 0 {
            LayoutNode::stack(Vec::new())
        } else {
            chain(
                SplitAxis::Horizontal,
                &views,
                &WorkspaceSnapshot::new(views.clone(), LayoutNode::stack(Vec::new())),
            )
        };
        WorkspaceSnapshot::new(views, current)
    }

    fn area() -> LogicalRect {
        LogicalRect::new(0.0, 0.0, 120.0, 40.0)
    }

    // -- names ----------------------------------------------------------

    #[test]
    fn bare_and_reserved_names_parse() {
        assert!(ProviderName::parse("dwindle").unwrap().is_canonical());
        assert!(ProviderName::parse("master").unwrap().is_canonical());
        assert!(ProviderName::parse("grid").unwrap().is_canonical());
        let custom = ProviderName::parse("spiral").unwrap();
        assert!(!custom.is_canonical());
        assert_eq!(custom.as_str(), "spiral");
    }

    #[test]
    fn qualified_names_parse() {
        let name = ProviderName::parse("acme.tiling:spiral").unwrap();
        assert!(!name.is_canonical());
        assert_eq!(name.as_str(), "acme.tiling:spiral");
    }

    #[test]
    fn malformed_names_fail() {
        for bad in [
            "",
            "Dwindle",
            "dwindle!",
            "has space",
            "owner:algo",
            ":algo",
            "owner:",
            "a:b:c",
            "owner.name:",
            ".name:algo",
        ] {
            assert!(ProviderName::parse(bad).is_err(), "must reject '{bad}'");
        }
        let long = "a".repeat(MAX_PROVIDER_NAME_LEN + 1);
        assert!(ProviderName::parse(&long).is_err());
        assert!(ProviderName::parse(&"a".repeat(MAX_PROVIDER_NAME_LEN)).is_ok());
    }

    // -- validation -----------------------------------------------------

    #[test]
    fn valid_tree_passes() {
        let snap = snapshot(3);
        let tree = DwindleProvider::new()
            .propose(&snap, &snap.views.clone(), area())
            .unwrap();
        assert!(validate_proposal(&tree, &snap.views).is_ok());
    }

    #[test]
    fn empty_view_set_fails() {
        let tree = LayoutNode::leaf(View::new(ViewId::new(1), 1, 1));
        assert_eq!(
            validate_proposal(&tree, &[]),
            Err(LayoutError::EmptyViewSet)
        );
    }

    #[test]
    fn duplicate_missing_and_unexpected_views_fail() {
        let views = view_ids(2);
        let dup = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(views[0], 1, 1)),
            LayoutNode::leaf(View::new(views[0], 1, 1)),
        );
        assert_eq!(
            validate_proposal(&dup, &views),
            Err(LayoutError::DuplicateView { id: 1 })
        );
        let missing = LayoutNode::leaf(View::new(views[0], 1, 1));
        assert_eq!(
            validate_proposal(&missing, &views),
            Err(LayoutError::MissingView { id: 2 })
        );
        let extra = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::new(views[0], 1, 1)),
            LayoutNode::split(
                SplitAxis::Vertical,
                0.5,
                LayoutNode::leaf(View::new(views[1], 1, 1)),
                LayoutNode::leaf(View::new(ViewId::new(9), 1, 1)),
            ),
        );
        assert_eq!(
            validate_proposal(&extra, &views),
            Err(LayoutError::UnexpectedView { id: 9 })
        );
    }

    #[test]
    fn out_of_range_ratio_is_rejected_not_clamped() {
        let views = view_ids(2);
        for bad in [0.09, 0.91, f32::INFINITY, f32::NEG_INFINITY] {
            let tree = LayoutNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: bad,
                first: Box::new(LayoutNode::leaf(View::new(views[0], 1, 1))),
                second: Box::new(LayoutNode::leaf(View::new(views[1], 1, 1))),
            };
            assert_eq!(
                validate_proposal(&tree, &views),
                Err(LayoutError::RatioOutOfBounds { ratio: bad }),
                "ratio {bad} must be rejected"
            );
        }
        // NaN compares unequal to itself, so it needs a structural match.
        let nan_tree = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: f32::NAN,
            first: Box::new(LayoutNode::leaf(View::new(views[0], 1, 1))),
            second: Box::new(LayoutNode::leaf(View::new(views[1], 1, 1))),
        };
        assert!(
            matches!(
                validate_proposal(&nan_tree, &views),
                Err(LayoutError::RatioOutOfBounds { .. })
            ),
            "NaN ratio must be rejected"
        );
        // Boundary ratios are accepted.
        for ok in [0.1, 0.9] {
            let tree = LayoutNode::split(
                SplitAxis::Horizontal,
                ok,
                LayoutNode::leaf(View::new(views[0], 1, 1)),
                LayoutNode::leaf(View::new(views[1], 1, 1)),
            );
            assert!(validate_proposal(&tree, &views).is_ok());
        }
    }

    #[test]
    fn stack_and_overlay_proposals_fail() {
        let views = view_ids(2);
        let stack = LayoutNode::stack(vec![
            LayoutNode::leaf(View::new(views[0], 1, 1)),
            LayoutNode::leaf(View::new(views[1], 1, 1)),
        ]);
        assert_eq!(
            validate_proposal(&stack, &views),
            Err(LayoutError::UnsupportedNode { kind: "Stack" })
        );
        let overlay = LayoutNode::overlay(
            LayoutNode::leaf(View::new(views[0], 1, 1)),
            LayoutNode::leaf(View::new(views[1], 1, 1)),
            Rect::new(0, 0, 5, 5),
        );
        assert_eq!(
            validate_proposal(&overlay, &views),
            Err(LayoutError::UnsupportedNode { kind: "Overlay" })
        );
    }

    // -- providers ------------------------------------------------------

    #[test]
    fn noop_preserves_current_tree_without_capability() {
        let registry = ProviderRegistry::with_canonical();
        assert!(registry.resolve("noop").is_some());
        let snap = snapshot(3);
        let tree = registry
            .require("noop")
            .unwrap()
            .propose(&snap, &snap.views.clone(), area())
            .unwrap();
        assert_eq!(tree, snap.current);
    }

    #[test]
    fn canonical_providers_cover_1_to_16_views() {
        let providers: Vec<Box<dyn LayoutProvider>> = vec![
            Box::new(DwindleProvider::new()),
            Box::new(MasterProvider::new()),
            Box::new(GridProvider::new()),
        ];
        for provider in &providers {
            for n in 1..=16 {
                let snap = snapshot(n);
                let views = snap.views.clone();
                let first = provider.propose(&snap, &views, area()).unwrap();
                assert!(
                    validate_proposal(&first, &views).is_ok(),
                    "{} must validate for {n} views",
                    provider.name()
                );
                // Deterministic: same inputs yield the same tree.
                let second = provider.propose(&snap, &views, area()).unwrap();
                assert_eq!(first, second);
                // Exact view set, no inventions.
                let mut leaves = first.leaf_ids();
                leaves.sort_unstable();
                let mut expected = views.clone();
                expected.sort_unstable();
                assert_eq!(leaves, expected);
            }
        }
    }

    #[test]
    fn empty_views_fail_on_every_provider() {
        let snap = snapshot(0);
        let providers: Vec<Box<dyn LayoutProvider>> = vec![
            Box::new(NoopTiler::new()),
            Box::new(DwindleProvider::new()),
            Box::new(MasterProvider::new()),
            Box::new(GridProvider::new()),
        ];
        for provider in &providers {
            assert_eq!(
                provider.propose(&snap, &[], area()),
                Err(LayoutError::EmptyViewSet),
                "{} must reject empty views",
                provider.name()
            );
        }
    }

    #[test]
    fn dwindle_spirals_with_alternating_axes() {
        let snap = snapshot(3);
        let views = snap.views.clone();
        let tree = DwindleProvider::new()
            .propose(&snap, &views, LogicalRect::new(0.0, 0.0, 80.0, 80.0))
            .unwrap();
        // Square area opens side-by-side, then alternates.
        let LayoutNode::Split {
            axis: outer,
            first,
            second,
            ..
        } = &tree
        else {
            panic!("dwindle of 3 must split");
        };
        assert_eq!(*outer, SplitAxis::Horizontal);
        assert_eq!(first.leaf_ids(), vec![views[0]]);
        let LayoutNode::Split { axis: inner, .. } = second.as_ref() else {
            panic!("dwindle remainder must split");
        };
        assert_eq!(*inner, SplitAxis::Vertical);
        // Tall area opens stacked.
        let tall = DwindleProvider::new()
            .propose(&snap, &views, LogicalRect::new(0.0, 0.0, 24.0, 80.0))
            .unwrap();
        let LayoutNode::Split {
            axis: tall_outer, ..
        } = &tall
        else {
            panic!("dwindle of 3 must split");
        };
        assert_eq!(*tall_outer, SplitAxis::Vertical);
    }

    #[test]
    fn master_pins_first_view_left() {
        let snap = snapshot(4);
        let views = snap.views.clone();
        let tree = MasterProvider::new()
            .propose(&snap, &views, area())
            .unwrap();
        let LayoutNode::Split {
            axis,
            first,
            second,
            ratio,
        } = &tree
        else {
            panic!("master of 4 must split");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert_eq!(*ratio, DEFAULT_PROVIDER_RATIO);
        assert_eq!(first.leaf_ids(), vec![views[0]]);
        assert_eq!(second.leaf_ids(), views[1..]);
        // A single view stays a bare leaf.
        let solo = snapshot(1);
        let one = MasterProvider::new()
            .propose(&solo, &solo.views.clone(), area())
            .unwrap();
        assert!(one.is_leaf());
    }

    #[test]
    fn grid_balances_rows() {
        let snap = snapshot(5);
        let views = snap.views.clone();
        let tree = GridProvider::new().propose(&snap, &views, area()).unwrap();
        // 5 views on a wide area: 2 rows of 3 + 2 (ragged, no invented ids).
        let LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } = &tree
        else {
            panic!("grid of 5 must split rows");
        };
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_eq!(first.leaf_count(), 3);
        assert_eq!(second.leaf_count(), 2);
        assert!(validate_proposal(&tree, &views).is_ok());
        // 4 views form an exact 2x2.
        let four = snapshot(4);
        let square = GridProvider::new()
            .propose(&four, &four.views.clone(), area())
            .unwrap();
        assert!(validate_proposal(&square, &four.views).is_ok());
        assert_eq!(square.leaf_count(), 4);
    }

    // -- registry gate --------------------------------------------------

    #[test]
    fn registration_requires_capability_except_noop() {
        #[derive(Debug)]
        struct ThirdParty {
            name: ProviderName,
        }
        impl LayoutProvider for ThirdParty {
            fn id(&self) -> ProviderId {
                ProviderId::new(100)
            }
            fn name(&self) -> &ProviderName {
                &self.name
            }
            fn propose(
                &self,
                workspace: &WorkspaceSnapshot,
                views: &[ViewId],
                _area: LogicalRect,
            ) -> Result<LayoutNode, LayoutError> {
                Ok(chain(SplitAxis::Horizontal, views, workspace))
            }
        }

        let mut registry = ProviderRegistry::new();
        // No-op registers without a grant.
        assert!(registry.register(Box::new(NoopTiler::new()), false).is_ok());
        // Third-party without a grant is denied.
        let denied = registry.register(
            Box::new(ThirdParty {
                name: ProviderName::parse("acme.tiling:spiral").unwrap(),
            }),
            false,
        );
        assert_eq!(
            denied,
            Err(LayoutError::CapabilityDenied {
                name: "acme.tiling:spiral".to_string()
            })
        );
        // With a grant it registers and resolves.
        assert!(
            registry
                .register(
                    Box::new(ThirdParty {
                        name: ProviderName::parse("acme.tiling:spiral").unwrap(),
                    }),
                    true
                )
                .is_ok()
        );
        assert!(registry.resolve("acme.tiling:spiral").is_some());
    }

    #[test]
    fn reserved_and_duplicate_registrations_fail() {
        #[derive(Debug)]
        struct Impostor {
            name: ProviderName,
            id: ProviderId,
        }
        impl LayoutProvider for Impostor {
            fn id(&self) -> ProviderId {
                self.id
            }
            fn name(&self) -> &ProviderName {
                &self.name
            }
            fn propose(
                &self,
                _workspace: &WorkspaceSnapshot,
                _views: &[ViewId],
                _area: LogicalRect,
            ) -> Result<LayoutNode, LayoutError> {
                Err(LayoutError::EmptyViewSet)
            }
        }
        let mut registry = ProviderRegistry::with_canonical();
        // Reserved bare names cannot be shadowed, even with a grant.
        assert_eq!(
            registry.register(
                Box::new(Impostor {
                    name: ProviderName::parse("dwindle").unwrap(),
                    id: ProviderId::new(50),
                }),
                true
            ),
            Err(LayoutError::ReservedName {
                name: "dwindle".to_string()
            })
        );
        // Duplicate names and ids are rejected.
        assert_eq!(
            registry.register(Box::new(NoopTiler::new()), false),
            Err(LayoutError::DuplicateName {
                name: "noop".to_string()
            })
        );
        assert_eq!(
            registry.register(
                Box::new(Impostor {
                    name: ProviderName::parse("acme.tiling:other").unwrap(),
                    id: DWINDLE_PROVIDER_ID,
                }),
                true
            ),
            Err(LayoutError::DuplicateId { id: 1 })
        );
    }

    #[test]
    fn unknown_and_malformed_names_fail_require() {
        let registry = ProviderRegistry::with_canonical();
        assert_eq!(
            registry
                .require("acme.tiling:ghost")
                .err()
                .expect("unregistered name must fail"),
            LayoutError::UnknownProvider {
                name: "acme.tiling:ghost".to_string()
            }
        );
        assert!(matches!(
            registry.require("Has Space"),
            Err(LayoutError::InvalidName { .. })
        ));
        assert_eq!(
            registry.names(),
            vec![
                "noop".to_string(),
                "dwindle".to_string(),
                "master".to_string(),
                "grid".to_string(),
            ]
        );
    }
}
