//! Pointer drag family: Mod+drag move, edge/corner resize, and
//! cross-workspace drops (PW-1/PW-2/PW-7; issues #1007, #1008, #1015).
//!
//! Builds on the CW-09 tree primitives
//! ([`LayoutNode::reparent_leaf`](crate::layout::LayoutNode::reparent_leaf),
//! [`LayoutNode::resize_split_by_drag`](crate::layout::LayoutNode::resize_split_by_drag),
//! hit testing) and the CW-08 command-registry path: every mutating step
//! validates a registered workspace command before touching the tree, pushes
//! a [`DragHistory`] snapshot first, and fails with the tree untouched
//! otherwise. All operations are deterministic and headless; this module
//! adds no new crate dependency.
//!
//! # Open-item resolutions
//!
//! - Move (UX-01): the gesture requires Mod held (`mod_held`); drops without
//!   Mod fail with [`DragMoveError::ModNotHeld`]. The drop target resolves
//!   via [`LayoutNode::hit_test_leaf`](crate::layout::LayoutNode::hit_test_leaf)
//!   (live preview is advisory: [`DragMoveSession::preview`] records the
//!   hovered leaf, [`DragMoveSession::commit`] revalidates everything).
//! - Resize (UX-02): tiled leaves adjust the adjacent split ratio through
//!   [`LayoutNode::resize_split_by_drag`](crate::layout::LayoutNode::resize_split_by_drag)
//!   (clamped to `[MIN_RATIO, MAX_RATIO]`); floating leaves adjust their
//!   rect via [`resize_floating_rect`] (clamped to [`View`](crate::view::View)
//!   minima and the container). Edge/corner detection is
//!   [`detect_resize_edge`] with a [`RESIZE_HANDLE_CELLS`]-cell hit area.
//! - Cross-workspace (UX-09): [`move_leaf_to_workspace`] detaches a leaf
//!   from the source tree and docks it beside an anchor in the destination
//!   tree. Both snapshots are recorded (source and destination histories),
//!   so either side undoes independently. Dwell is not gated: the drop
//!   commits on command round-trip like the same-tree move.

#![forbid(unsafe_code)]

use crate::geometry::{Gaps, Point, Rect, SplitAxis};
use crate::layout::LayoutNode;
use crate::panel::CommandRegistry;
use crate::view::ViewId;

/// Maximum undo snapshots retained per drag session.
pub const DRAG_HISTORY_CAP: usize = 32;

/// Workspace command carrying a Mod+drag panel move (UX-01).
///
/// `<owner>.<name>:<command>` grammar (see
/// [`QualifiedCommand`](crate::panel::QualifiedCommand)); routed by
/// [`DragMoveSession::commit`].
pub const DRAG_MOVE_CMD: &str = "bitty.workspace:drag-move";

/// Workspace command carrying a drag resize step (UX-02).
pub const DRAG_RESIZE_CMD: &str = "bitty.workspace:drag-resize";

/// Workspace command carrying a cross-workspace drop (UX-09).
pub const WORKSPACE_DROP_CMD: &str = "bitty.workspace:drop";

/// Maximum leaves per destination tree on a cross-workspace drop.
///
/// Mirrors the accepted `32`-view workspace bound (`MAX_VIEWS_PER_WORKSPACE`
/// in `bitty-runtime`); drops that would exceed it fail with
/// [`CrossWorkspaceError::DestinationFull`].
pub const MAX_VIEWS_PER_WORKSPACE_TREE: usize = 32;

/// Edge/corner hit area for free resize, in cells from the leaf border
/// (UX-02). One cell keeps the grab zone reachable on the cell grid without
/// stealing interior clicks.
pub const RESIZE_HANDLE_CELLS: u16 = 1;

/// Bounded LIFO undo history over [`LayoutNode`] snapshots (CW-09).
#[derive(Clone, Debug, Default)]
pub struct DragHistory {
    snapshots: Vec<LayoutNode>,
}

impl DragHistory {
    /// Creates an empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
        }
    }

    /// Number of retained snapshots (`<= [`DRAG_HISTORY_CAP`]`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// True when no snapshot is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Records `tree` as the pre-mutation snapshot; evicts the oldest when
    /// the history is at [`DRAG_HISTORY_CAP`] (`DropOldest`).
    pub fn push(&mut self, tree: &LayoutNode) {
        if self.snapshots.len() >= DRAG_HISTORY_CAP {
            self.snapshots.remove(0);
        }
        self.snapshots.push(tree.clone());
    }

    /// Restores the most recent snapshot into `tree`, returning `true`.
    /// Returns `false` with `tree` untouched when the history is empty.
    pub fn undo(&mut self, tree: &mut LayoutNode) -> bool {
        let Some(snapshot) = self.snapshots.pop() else {
            return false;
        };
        *tree = snapshot;
        true
    }

    /// Drops all retained snapshots.
    pub fn clear(&mut self) {
        self.snapshots.clear();
    }
}

// ---------------------------------------------------------------------------
// UX-01: Mod+drag panel move
// ---------------------------------------------------------------------------

/// Where a dragged leaf docks: the anchor leaf, the split axis/ratio for
/// the new split, and which side the dragged leaf lands on (`after` puts
/// it second). Shared by same-tree moves and cross-workspace drops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropSpec {
    /// Anchor leaf the dragged leaf docks beside.
    pub target: ViewId,
    /// Axis of the split created at the anchor.
    pub axis: SplitAxis,
    /// Split ratio (clamped to `[MIN_RATIO, MAX_RATIO]`).
    pub ratio: f32,
    /// True puts the dragged leaf second, false first.
    pub after: bool,
}

impl DropSpec {
    /// Creates a drop spec.
    #[must_use]
    pub const fn new(target: ViewId, axis: SplitAxis, ratio: f32, after: bool) -> Self {
        Self {
            target,
            axis,
            ratio,
            after,
        }
    }
}

/// Cross-workspace drop plan (UX-09): which leaf moves and where it docks
/// in the destination tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossWorkspaceDrop {
    /// Leaf id detached from the source tree.
    pub id: ViewId,
    /// Docking plan in the destination tree (`target` is the anchor).
    pub anchor: DropSpec,
}

impl CrossWorkspaceDrop {
    /// Creates a cross-workspace drop plan.
    #[must_use]
    pub const fn new(id: ViewId, anchor: DropSpec) -> Self {
        Self { id, anchor }
    }
}

/// Error for [`DragMoveSession`] start/preview/commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragMoveError {
    /// The gesture started without Mod held; nothing was recorded.
    ModNotHeld,
    /// `command` is not [`DRAG_MOVE_CMD`]; the tree is untouched.
    UnknownCommand(String),
    /// The move command is not registered in the workspace
    /// [`CommandRegistry`]; the tree is untouched.
    Unregistered(String),
    /// No leaf with this id exists in the tree; the tree is untouched.
    SourceNotFound(ViewId),
    /// No leaf with this id exists in the tree; the tree is untouched.
    TargetNotFound(ViewId),
    /// Source and target are the same leaf (self-drop no-op); untouched.
    SelfDrop,
}

impl std::fmt::Display for DragMoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModNotHeld => f.write_str("drag move requires Mod held"),
            Self::UnknownCommand(cmd) => write!(f, "unknown drag move command: {cmd}"),
            Self::Unregistered(cmd) => write!(f, "drag move command not registered: {cmd}"),
            Self::SourceNotFound(id) => write!(f, "drag move source not found: {id}"),
            Self::TargetNotFound(id) => write!(f, "drag move target not found: {id}"),
            Self::SelfDrop => f.write_str("drag move source and target are the same leaf"),
        }
    }
}

impl std::error::Error for DragMoveError {}

/// In-progress Mod+drag move gesture (UX-01).
///
/// `start` gates on Mod; `preview` hit-tests the hovered drop target
/// (advisory, recorded for the caller); `commit` validates the registered
/// workspace command, records a [`DragHistory`] snapshot, and re-parents
/// the source leaf beside the target via
/// [`LayoutNode::reparent_leaf`](crate::layout::LayoutNode::reparent_leaf).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragMoveSession {
    source: ViewId,
    preview: Option<ViewId>,
}

impl DragMoveSession {
    /// Begins a move gesture for `source`. Fails with
    /// [`DragMoveError::ModNotHeld`] when Mod is not held.
    pub fn start(source: ViewId, mod_held: bool) -> Result<Self, DragMoveError> {
        if !mod_held {
            return Err(DragMoveError::ModNotHeld);
        }
        Ok(Self {
            source,
            preview: None,
        })
    }

    /// The dragged leaf.
    #[must_use]
    pub fn source(&self) -> ViewId {
        self.source
    }

    /// The last hovered drop target from [`Self::preview`], if any.
    #[must_use]
    pub fn preview_target(&self) -> Option<ViewId> {
        self.preview
    }

    /// Advisory live preview: hit-tests `point` against `tree` allocations
    /// and records the hovered leaf. Returns the hovered leaf, or `None`
    /// when the point lands on background. Never mutates the tree.
    pub fn preview(
        &mut self,
        tree: &LayoutNode,
        bounds: Rect,
        gaps: Gaps,
        point: Point,
    ) -> Option<ViewId> {
        let hit = tree.hit_test_leaf(bounds, gaps, point);
        self.preview = hit;
        hit
    }

    /// Commits the move: validates `command` against [`DRAG_MOVE_CMD`] and
    /// the workspace [`CommandRegistry`], records a `history` snapshot, and
    /// re-parents the source leaf beside `drop.target`.
    ///
    /// Validation order (tree untouched on every failure): command name,
    /// registry ownership, self-drop, source lookup, target lookup. The
    /// recorded `preview` (if any) is advisory only and not re-checked.
    pub fn commit(
        self,
        tree: &mut LayoutNode,
        history: &mut DragHistory,
        registry: &CommandRegistry,
        command: &str,
        drop: DropSpec,
    ) -> Result<(), DragMoveError> {
        if command != DRAG_MOVE_CMD {
            return Err(DragMoveError::UnknownCommand(command.to_owned()));
        }
        if registry.owner_of(command).is_none() {
            return Err(DragMoveError::Unregistered(command.to_owned()));
        }
        if self.source == drop.target {
            return Err(DragMoveError::SelfDrop);
        }
        if tree.find_leaf(self.source).is_none() {
            return Err(DragMoveError::SourceNotFound(self.source));
        }
        if tree.find_leaf(drop.target).is_none() {
            return Err(DragMoveError::TargetNotFound(drop.target));
        }
        history.push(tree);
        if !tree.reparent_leaf(self.source, drop.target, drop.axis, drop.ratio, drop.after) {
            // Unreachable after the lookups above, but never lose the
            // snapshot accounting: roll the snapshot back.
            history.undo(tree);
            return Err(DragMoveError::TargetNotFound(drop.target));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// UX-02: edge/corner free resize
// ---------------------------------------------------------------------------

/// Error for tiled drag-resize steps (UX-02).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragResizeError {
    /// `command` is not [`DRAG_RESIZE_CMD`]; the tree is untouched.
    UnknownCommand(String),
    /// The resize command is not registered in the workspace
    /// [`CommandRegistry`]; the tree is untouched.
    Unregistered(String),
    /// `path` addresses no split, or `total_cells == 0`; untouched.
    BadHandle,
}

impl std::fmt::Display for DragResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(cmd) => write!(f, "unknown drag resize command: {cmd}"),
            Self::Unregistered(cmd) => {
                write!(f, "drag resize command not registered: {cmd}")
            }
            Self::BadHandle => f.write_str("drag resize handle addresses no split"),
        }
    }
}

impl std::error::Error for DragResizeError {}

/// Which border of a leaf allocation the resize grabs (UX-02).
///
/// Tiled leaves resolve the edge to the adjacent split handle (axis +
/// direction); floating leaves adjust that edge of their rect via
/// [`resize_floating_rect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeEdge {
    /// True for corner grabs (both axes move).
    #[must_use]
    pub fn is_corner(self) -> bool {
        matches!(
            self,
            Self::TopLeft | Self::TopRight | Self::BottomLeft | Self::BottomRight
        )
    }
}

/// Detects which border of `alloc` a resize grabs (UX-02 hit area).
///
/// Returns `None` when `point` lies outside `alloc` or farther than
/// `handle` cells from every border. Corners win over edges when both axes
/// are within the band. Total and deterministic.
#[must_use]
pub fn detect_resize_edge(alloc: Rect, point: Point, handle: u16) -> Option<ResizeEdge> {
    if !alloc.contains_point(point) {
        return None;
    }
    let band = u32::from(handle);
    let left_dist = u32::from(point.x.saturating_sub(alloc.x));
    let top_dist = u32::from(point.y.saturating_sub(alloc.y));
    let right_dist = alloc.right().saturating_sub(u32::from(point.x) + 1);
    let bottom_dist = alloc.bottom().saturating_sub(u32::from(point.y) + 1);
    let near_left = left_dist < band;
    let near_right = right_dist < band;
    let near_top = top_dist < band;
    let near_bottom = bottom_dist < band;
    match (near_left || near_right, near_top || near_bottom) {
        (true, true) => Some(match (near_left, near_top) {
            (true, true) => ResizeEdge::TopLeft,
            (true, false) => ResizeEdge::BottomLeft,
            (false, true) => ResizeEdge::TopRight,
            (false, false) => ResizeEdge::BottomRight,
        }),
        (true, false) => Some(if near_left {
            ResizeEdge::Left
        } else {
            ResizeEdge::Right
        }),
        (false, true) => Some(if near_top {
            ResizeEdge::Top
        } else {
            ResizeEdge::Bottom
        }),
        (false, false) => None,
    }
}

/// Applies one tiled resize step through the workspace command registry
/// (UX-02): validates `command` against [`DRAG_RESIZE_CMD`] and registry
/// ownership, records a `history` snapshot, then adjusts the split ratio at
/// `path` via
/// [`LayoutNode::resize_split_by_drag`](crate::layout::LayoutNode::resize_split_by_drag)
/// (clamped to `[MIN_RATIO, MAX_RATIO]`). Fails with the tree untouched on
/// every error.
pub fn apply_tiled_resize(
    tree: &mut LayoutNode,
    history: &mut DragHistory,
    registry: &CommandRegistry,
    command: &str,
    path: &[usize],
    delta_cells: i32,
    total_cells: u16,
) -> Result<(), DragResizeError> {
    if command != DRAG_RESIZE_CMD {
        return Err(DragResizeError::UnknownCommand(command.to_owned()));
    }
    if registry.owner_of(command).is_none() {
        return Err(DragResizeError::Unregistered(command.to_owned()));
    }
    if total_cells == 0 || tree.split_ratio_at(path).is_none() {
        return Err(DragResizeError::BadHandle);
    }
    history.push(tree);
    if !tree.resize_split_by_drag(path, delta_cells, total_cells) {
        history.undo(tree);
        return Err(DragResizeError::BadHandle);
    }
    Ok(())
}

/// Adjusts a floating leaf rect by dragging `edge` (UX-02).
///
/// `dx`/`dy` are signed cell deltas (positive grows right/down for
/// `Right`/`Bottom` edges, shrinks for `Left`/`Top`). The result keeps at
/// least [`View`](crate::view::View) minima in each dimension and is
/// clipped to `container`. Pure, total, deterministic.
#[must_use]
pub fn resize_floating_rect(
    current: Rect,
    edge: ResizeEdge,
    dx: i16,
    dy: i16,
    container: Rect,
) -> Rect {
    let min_w = u32::from(crate::view::View::MIN_COLS);
    let min_h = u32::from(crate::view::View::MIN_ROWS);
    let mut x0 = i32::from(current.x);
    let mut y0 = i32::from(current.y);
    let mut x1 = x0 + i32::from(current.width);
    let mut y1 = y0 + i32::from(current.height);
    match edge {
        ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
            x0 += i32::from(dx);
        }
        ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
            x1 += i32::from(dx);
        }
        ResizeEdge::Top | ResizeEdge::Bottom => {}
    }
    match edge {
        ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
            y0 += i32::from(dy);
        }
        ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
            y1 += i32::from(dy);
        }
        ResizeEdge::Left | ResizeEdge::Right => {}
    }
    if x1 - x0 < min_w as i32 {
        match edge {
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
                x0 = x1 - min_w as i32;
            }
            _ => x1 = x0 + min_w as i32,
        }
    }
    if y1 - y0 < min_h as i32 {
        match edge {
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
                y0 = y1 - min_h as i32;
            }
            _ => y1 = y0 + min_h as i32,
        }
    }
    let cx0 = i32::from(container.x);
    let cy0 = i32::from(container.y);
    let cx1 = cx0 + i32::from(container.width);
    let cy1 = cy0 + i32::from(container.height);
    x0 = x0.clamp(cx0, cx1);
    y0 = y0.clamp(cy0, cy1);
    x1 = x1.clamp(cx0, cx1);
    y1 = y1.clamp(cy0, cy1);
    if x1 < x0 {
        x1 = x0;
    }
    if y1 < y0 {
        y1 = y0;
    }
    Rect::new(x0 as u16, y0 as u16, (x1 - x0) as u16, (y1 - y0) as u16)
}

// ---------------------------------------------------------------------------
// UX-09: drag-across-workspace
// ---------------------------------------------------------------------------

/// Error for [`move_leaf_to_workspace`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrossWorkspaceError {
    /// `command` is not [`WORKSPACE_DROP_CMD`]; both trees untouched.
    UnknownCommand(String),
    /// The drop command is not registered; both trees untouched.
    Unregistered(String),
    /// No leaf with this id exists in the source tree; untouched.
    SourceNotFound(ViewId),
    /// No leaf with this id exists in the destination tree; untouched.
    AnchorNotFound(ViewId),
    /// The destination tree already holds the maximum leaves; untouched.
    DestinationFull {
        /// The enforced bound ([`MAX_VIEWS_PER_WORKSPACE_TREE`]).
        max: usize,
    },
}

impl std::fmt::Display for CrossWorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCommand(cmd) => write!(f, "unknown workspace drop command: {cmd}"),
            Self::Unregistered(cmd) => {
                write!(f, "workspace drop command not registered: {cmd}")
            }
            Self::SourceNotFound(id) => write!(f, "workspace drop source not found: {id}"),
            Self::AnchorNotFound(id) => write!(f, "workspace drop anchor not found: {id}"),
            Self::DestinationFull { max } => {
                write!(f, "workspace drop destination full (max {max})")
            }
        }
    }
}

impl std::error::Error for CrossWorkspaceError {}

/// Advisory drop-target preview in the destination workspace (UX-09):
/// hit-tests `point` against `dst` allocations. Never mutates either tree.
#[must_use]
pub fn workspace_drop_target(
    dst: &LayoutNode,
    bounds: Rect,
    gaps: Gaps,
    point: Point,
) -> Option<ViewId> {
    dst.hit_test_leaf(bounds, gaps, point)
}

/// Moves leaf `drop.id` from `src` into `dst` beside
/// `drop.anchor.target` (UX-09 drag-across-workspace).
///
/// Validates `command` against [`WORKSPACE_DROP_CMD`] and registry
/// ownership, source/anchor presence, and the destination bound
/// ([`MAX_VIEWS_PER_WORKSPACE_TREE`]) before mutating; every failure leaves
/// both trees untouched. On success both `src_history` and `dst_history`
/// hold their pre-drop snapshots, so each side undoes independently.
pub fn move_leaf_to_workspace(
    src: &mut LayoutNode,
    src_history: &mut DragHistory,
    dst: &mut LayoutNode,
    dst_history: &mut DragHistory,
    registry: &CommandRegistry,
    command: &str,
    drop: CrossWorkspaceDrop,
) -> Result<(), CrossWorkspaceError> {
    if command != WORKSPACE_DROP_CMD {
        return Err(CrossWorkspaceError::UnknownCommand(command.to_owned()));
    }
    if registry.owner_of(command).is_none() {
        return Err(CrossWorkspaceError::Unregistered(command.to_owned()));
    }
    if src.find_leaf(drop.id).is_none() {
        return Err(CrossWorkspaceError::SourceNotFound(drop.id));
    }
    if dst.find_leaf(drop.anchor.target).is_none() {
        return Err(CrossWorkspaceError::AnchorNotFound(drop.anchor.target));
    }
    if dst.leaf_count() + 1 > MAX_VIEWS_PER_WORKSPACE_TREE {
        return Err(CrossWorkspaceError::DestinationFull {
            max: MAX_VIEWS_PER_WORKSPACE_TREE,
        });
    }
    src_history.push(src);
    dst_history.push(dst);
    let Some(view) = src.remove_leaf(drop.id) else {
        src_history.undo(src);
        dst_history.undo(dst);
        return Err(CrossWorkspaceError::SourceNotFound(drop.id));
    };
    let anchor = drop.anchor;
    if !dst.insert_beside(
        anchor.target,
        &view,
        anchor.axis,
        anchor.ratio,
        anchor.after,
    ) {
        // Anchor was verified above; restore both sides rather than lose
        // the detached view.
        dst_history.undo(dst);
        restore_detached(src, view);
        return Err(CrossWorkspaceError::AnchorNotFound(anchor.target));
    }
    Ok(())
}

/// Returns a detached view to `tree` beside its first live leaf (or as the
/// sole leaf of an empty tree). Last-resort recovery; never drops the view.
fn restore_detached(tree: &mut LayoutNode, view: crate::view::View) {
    if tree.leaf_count() == 0 {
        *tree = LayoutNode::leaf(view);
        return;
    }
    let Some(first) = tree.leaf_ids().into_iter().next() else {
        *tree = LayoutNode::leaf(view);
        return;
    };
    if !tree.insert_beside(first, &view, SplitAxis::Horizontal, 0.5, true) {
        let old = std::mem::replace(tree, LayoutNode::stack(Vec::new()));
        *tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, old, LayoutNode::leaf(view));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::SplitAxis;
    use crate::view::{View, ViewId};

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    fn registry_with(cmds: &[&str]) -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        let owner = crate::panel::PanelId::new(1);
        for cmd in cmds {
            registry.register(owner, cmd).expect("command registers");
        }
        registry
    }

    #[test]
    fn undo_restores_pre_drag_snapshot() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        assert!(history.is_empty());
        history.push(&tree);
        assert_eq!(history.len(), 1);
        assert!(tree.reparent_leaf(
            ViewId::new(2),
            ViewId::new(1),
            SplitAxis::Vertical,
            0.5,
            true
        ));
        assert_ne!(tree, pair());
        assert!(history.undo(&mut tree));
        assert_eq!(tree, pair());
        assert!(history.is_empty());
        // Empty history: fail-soft false, tree untouched.
        let before = tree.clone();
        assert!(!history.undo(&mut tree));
        assert_eq!(tree, before);
    }

    #[test]
    fn history_drops_oldest_at_cap() {
        let mut history = DragHistory::new();
        for i in 0..(DRAG_HISTORY_CAP + 5) {
            let tree = LayoutNode::leaf(View::new(ViewId::new(i as u64), 80, 24));
            history.push(&tree);
        }
        assert_eq!(history.len(), DRAG_HISTORY_CAP);
        // The five oldest snapshots were evicted: the newest surviving
        // undo target is the tree pushed at index 5.
        let mut probe = pair();
        assert!(history.undo(&mut probe));
        assert_eq!(
            probe.leaf_ids(),
            vec![ViewId::new((DRAG_HISTORY_CAP + 5 - 1) as u64)]
        );
        history.clear();
        assert!(history.is_empty());
    }

    #[test]
    fn undo_resize_step_returns_prior_ratio() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        history.push(&tree);
        assert!(tree.resize_split_by_drag(&[], 8, 80));
        assert!((tree.split_ratio_at(&[]).expect("split") - 0.6).abs() < 1e-6);
        assert!(history.undo(&mut tree));
        assert!((tree.split_ratio_at(&[]).expect("split") - 0.5).abs() < 1e-6);
    }

    // -- UX-01 move -------------------------------------------------------

    #[test]
    fn move_requires_mod_held() {
        assert_eq!(
            DragMoveSession::start(ViewId::new(1), false),
            Err(DragMoveError::ModNotHeld)
        );
        let session = DragMoveSession::start(ViewId::new(1), true).expect("mod held");
        assert_eq!(session.source(), ViewId::new(1));
        assert_eq!(session.preview_target(), None);
    }

    #[test]
    fn move_preview_hit_tests_drop_target_without_mutating() {
        let tree = pair();
        let bounds = Rect::new(0, 0, 80, 24);
        let mut session = DragMoveSession::start(ViewId::new(1), true).expect("start");
        // Left half holds leaf 1, right half leaf 2.
        assert_eq!(
            session.preview(&tree, bounds, Gaps::ZERO, Point::new(60, 12)),
            Some(ViewId::new(2))
        );
        assert_eq!(session.preview_target(), Some(ViewId::new(2)));
        assert_eq!(
            session.preview(&tree, bounds, Gaps::ZERO, Point::new(200, 200)),
            None,
            "background hover clears the advisory preview"
        );
        assert_eq!(session.preview_target(), None);
    }

    #[test]
    fn move_commit_validates_registry_then_reparents_with_undo() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        let registry = registry_with(&[DRAG_MOVE_CMD]);
        DragMoveSession::start(ViewId::new(2), true)
            .expect("start")
            .commit(
                &mut tree,
                &mut history,
                &registry,
                DRAG_MOVE_CMD,
                DropSpec::new(ViewId::new(1), SplitAxis::Vertical, 0.5, true),
            )
            .expect("commit routes through the registry");
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert!(history.undo(&mut tree));
        assert_eq!(tree, pair());
    }

    #[test]
    fn move_commit_failures_leave_tree_and_history_untouched() {
        let no_registry = CommandRegistry::new();
        let registered = registry_with(&[DRAG_MOVE_CMD]);
        for (registry, command, source, target, want) in [
            (
                &no_registry,
                DRAG_MOVE_CMD,
                ViewId::new(2),
                ViewId::new(1),
                DragMoveError::Unregistered(DRAG_MOVE_CMD.to_string()),
            ),
            (
                &registered,
                "bitty.workspace:nope",
                ViewId::new(2),
                ViewId::new(1),
                DragMoveError::UnknownCommand("bitty.workspace:nope".to_string()),
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(1),
                ViewId::new(1),
                DragMoveError::SelfDrop,
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(404),
                ViewId::new(1),
                DragMoveError::SourceNotFound(ViewId::new(404)),
            ),
            (
                &registered,
                DRAG_MOVE_CMD,
                ViewId::new(2),
                ViewId::new(404),
                DragMoveError::TargetNotFound(ViewId::new(404)),
            ),
        ] {
            let mut tree = pair();
            let mut history = DragHistory::new();
            let err = DragMoveSession::start(source, true)
                .expect("start")
                .commit(
                    &mut tree,
                    &mut history,
                    registry,
                    command,
                    DropSpec::new(target, SplitAxis::Horizontal, 0.5, true),
                )
                .expect_err("commit must fail");
            assert_eq!(err, want);
            assert_eq!(tree, pair(), "tree untouched on {err}");
            assert!(history.is_empty(), "no snapshot on {err}");
        }
    }

    // -- UX-02 resize ------------------------------------------------------

    #[test]
    fn tiled_resize_adjusts_ratio_through_registry_with_undo() {
        let mut tree = pair();
        let mut history = DragHistory::new();
        let registry = registry_with(&[DRAG_RESIZE_CMD]);
        apply_tiled_resize(
            &mut tree,
            &mut history,
            &registry,
            DRAG_RESIZE_CMD,
            &[],
            8,
            80,
        )
        .expect("resize routes");
        assert!((tree.split_ratio_at(&[]).expect("split") - 0.6).abs() < 1e-6);
        assert!(history.undo(&mut tree));
        assert_eq!(tree, pair());
    }

    #[test]
    fn tiled_resize_failures_leave_tree_untouched() {
        let registered = registry_with(&[DRAG_RESIZE_CMD]);
        let empty = CommandRegistry::new();
        // Unknown command, unregistered command, bad path, zero total.
        let cases: [(&CommandRegistry, &str, &[usize], i32, u16); 4] = [
            (&registered, "bitty.workspace:nope", &[], 8, 80),
            (&empty, DRAG_RESIZE_CMD, &[], 8, 80),
            (&registered, DRAG_RESIZE_CMD, &[7], 8, 80),
            (&registered, DRAG_RESIZE_CMD, &[], 8, 0),
        ];
        for (registry, command, path, delta, total) in cases {
            let mut tree = pair();
            let mut history = DragHistory::new();
            assert!(
                apply_tiled_resize(
                    &mut tree,
                    &mut history,
                    registry,
                    command,
                    path,
                    delta,
                    total
                )
                .is_err()
            );
            assert_eq!(tree, pair());
            assert!(history.is_empty());
        }
        // Clamp, not failure: oversized deltas pin to the ratio bounds.
        let mut tree = pair();
        let mut history = DragHistory::new();
        apply_tiled_resize(
            &mut tree,
            &mut history,
            &registered,
            DRAG_RESIZE_CMD,
            &[],
            400,
            80,
        )
        .expect("clamped resize succeeds");
        assert!((tree.split_ratio_at(&[]).expect("split") - LayoutNode::MAX_RATIO).abs() < 1e-6);
    }

    #[test]
    fn detect_resize_edge_corners_edges_and_misses() {
        let alloc = Rect::new(10, 5, 20, 10);
        // Corners win when both axes are in the band.
        assert_eq!(
            detect_resize_edge(alloc, Point::new(10, 5), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(
            detect_resize_edge(alloc, Point::new(29, 14), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::BottomRight)
        );
        // Edges.
        assert_eq!(
            detect_resize_edge(alloc, Point::new(10, 8), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::Left)
        );
        assert_eq!(
            detect_resize_edge(alloc, Point::new(29, 8), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::Right)
        );
        assert_eq!(
            detect_resize_edge(alloc, Point::new(15, 5), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::Top)
        );
        assert_eq!(
            detect_resize_edge(alloc, Point::new(15, 14), RESIZE_HANDLE_CELLS),
            Some(ResizeEdge::Bottom)
        );
        // Interior and exterior miss.
        assert_eq!(
            detect_resize_edge(alloc, Point::new(15, 8), RESIZE_HANDLE_CELLS),
            None
        );
        assert_eq!(
            detect_resize_edge(alloc, Point::new(0, 0), RESIZE_HANDLE_CELLS),
            None
        );
        assert!(ResizeEdge::TopLeft.is_corner());
        assert!(!ResizeEdge::Left.is_corner());
    }

    #[test]
    fn floating_resize_grows_shrinks_and_clamps() {
        let container = Rect::new(0, 0, 80, 24);
        let current = Rect::new(10, 5, 20, 10);
        // Grow right/down.
        assert_eq!(
            resize_floating_rect(current, ResizeEdge::Right, 5, 0, container),
            Rect::new(10, 5, 25, 10)
        );
        assert_eq!(
            resize_floating_rect(current, ResizeEdge::BottomRight, 5, 3, container),
            Rect::new(10, 5, 25, 13)
        );
        // Shrink from the left/top moves the origin.
        assert_eq!(
            resize_floating_rect(current, ResizeEdge::Left, 4, 0, container),
            Rect::new(14, 5, 16, 10)
        );
        assert_eq!(
            resize_floating_rect(current, ResizeEdge::TopLeft, 4, 2, container),
            Rect::new(14, 7, 16, 8)
        );
        // Collapse pins to the view minima instead of inverting.
        let tiny = resize_floating_rect(current, ResizeEdge::Left, 100, 0, container);
        assert_eq!(tiny.width, View::MIN_COLS);
        assert_eq!(tiny.x + tiny.width, current.x + current.width);
        // Growth past the container clips.
        let clipped = resize_floating_rect(current, ResizeEdge::Right, 100, 0, container);
        assert_eq!(clipped.right(), container.right());
        assert_eq!(clipped.x, current.x);
    }

    // -- UX-09 cross-workspace ---------------------------------------------

    #[test]
    fn cross_workspace_move_transfers_leaf_with_both_undos() {
        let mut src = pair();
        let mut dst = leaf(9);
        let mut src_history = DragHistory::new();
        let mut dst_history = DragHistory::new();
        let registry = registry_with(&[WORKSPACE_DROP_CMD]);
        // Advisory preview resolves the anchor in the destination tree.
        let bounds = Rect::new(0, 0, 40, 24);
        assert_eq!(
            workspace_drop_target(&dst, bounds, Gaps::ZERO, Point::new(5, 5)),
            Some(ViewId::new(9))
        );
        move_leaf_to_workspace(
            &mut src,
            &mut src_history,
            &mut dst,
            &mut dst_history,
            &registry,
            WORKSPACE_DROP_CMD,
            CrossWorkspaceDrop::new(
                ViewId::new(2),
                DropSpec::new(ViewId::new(9), SplitAxis::Horizontal, 0.5, true),
            ),
        )
        .expect("cross-workspace drop routes");
        assert_eq!(src.leaf_ids(), vec![ViewId::new(1)]);
        assert_eq!(dst.leaf_ids(), vec![ViewId::new(9), ViewId::new(2)]);
        assert!(dst_history.undo(&mut dst));
        assert_eq!(dst, leaf(9));
        assert!(src_history.undo(&mut src));
        assert_eq!(src, pair());
    }

    #[test]
    fn cross_workspace_failures_leave_both_trees_untouched() {
        let registered = registry_with(&[WORKSPACE_DROP_CMD]);
        let empty = CommandRegistry::new();
        let cases = [
            (&empty, WORKSPACE_DROP_CMD, ViewId::new(2), ViewId::new(9)),
            (
                &registered,
                "bitty.workspace:nope",
                ViewId::new(2),
                ViewId::new(9),
            ),
            (
                &registered,
                WORKSPACE_DROP_CMD,
                ViewId::new(404),
                ViewId::new(9),
            ),
            (
                &registered,
                WORKSPACE_DROP_CMD,
                ViewId::new(2),
                ViewId::new(404),
            ),
        ];
        for (registry, command, id, anchor) in cases {
            let mut src = pair();
            let mut dst = leaf(9);
            let mut src_history = DragHistory::new();
            let mut dst_history = DragHistory::new();
            assert!(
                move_leaf_to_workspace(
                    &mut src,
                    &mut src_history,
                    &mut dst,
                    &mut dst_history,
                    registry,
                    command,
                    CrossWorkspaceDrop::new(
                        id,
                        DropSpec::new(anchor, SplitAxis::Horizontal, 0.5, true),
                    ),
                )
                .is_err()
            );
            assert_eq!(src, pair());
            assert_eq!(dst, leaf(9));
            assert!(src_history.is_empty());
            assert!(dst_history.is_empty());
        }
    }

    #[test]
    fn cross_workspace_destination_bound_is_fail_closed() {
        let many: Vec<LayoutNode> = (0..MAX_VIEWS_PER_WORKSPACE_TREE as u64)
            .map(|i| leaf(100 + i))
            .collect();
        let mut dst = LayoutNode::stack(many);
        let mut src = pair();
        let mut src_history = DragHistory::new();
        let mut dst_history = DragHistory::new();
        let registry = registry_with(&[WORKSPACE_DROP_CMD]);
        assert_eq!(
            move_leaf_to_workspace(
                &mut src,
                &mut src_history,
                &mut dst,
                &mut dst_history,
                &registry,
                WORKSPACE_DROP_CMD,
                CrossWorkspaceDrop::new(
                    ViewId::new(2),
                    DropSpec::new(ViewId::new(100), SplitAxis::Horizontal, 0.5, true),
                ),
            ),
            Err(CrossWorkspaceError::DestinationFull {
                max: MAX_VIEWS_PER_WORKSPACE_TREE
            })
        );
        assert_eq!(src, pair());
        assert_eq!(dst.leaf_count(), MAX_VIEWS_PER_WORKSPACE_TREE);
    }
}
