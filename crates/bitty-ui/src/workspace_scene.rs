//! Four-layer spatial and identity model: `WorkspaceScene` -> `View` ->
//! `Panel` -> `Activity` (UX-15, CTX-0662).
//!
//! Candidate implementation of U-2
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, `RFC-OQ-3` placement Open, owner-pending Workspace Scene
//! and Panel & Activity RFCs). Nothing here is normative, accepted, or
//! verified: every layer name, bound, and rule below is a candidate
//! spelling that the owning RFCs accept or reject, never this module. The
//! module is English-only.
//!
//! What this module provides:
//!
//! - [`SceneLayer`] — the named spatial layers of a [`WorkspaceScene`]
//!   (`tiled`, `floating`, `pinned`, `popover`, `overlay`) with an explicit
//!   z-order contract ([`SceneLayer::z_rank`]).
//! - [`WorkspaceScene`] — headless scene state: the tiled layer reuses the
//!   accepted [`LayoutNode`](crate::layout::LayoutNode) composition, the
//!   other layers hold bounded [`LayerEntry`] placements, and panel
//!   attachments join panel identity to presentation.
//! - [`PanelAttachment`] — the `Panel`-at-`View` binding. The accepted
//!   inequality `PanelId != ViewId != TerminalId` holds by construction:
//!   [`PanelId`](crate::panel::PanelId), [`ViewId`](crate::view::ViewId),
//!   and [`TerminalBinding`] are pairwise distinct newtypes with no `From`
//!   bridge, and no function accepts one where another is expected.
//!   [`WorkspaceScene::move_panel`] re-parents the presentation attachment
//!   only; the panel identity and its terminal binding survive the move
//!   (a move is not a copy, state stays single-owned).
//! - [`ActivityStack`] — push/pop navigation within a panel (for example
//!   Overview -> Container Detail -> Logs).
//!
//! Identity mirrors: [`TerminalBinding`] and [`WorkspaceSceneId`] are
//! UI-side mirrors of the runtime `TerminalId` / `WorkspaceId` (owned by
//! `bitty-runtime`, which depends on this crate, so they cannot be
//! imported here). They are joined by the runtime, never converted: no
//! `From` bridge exists in either direction.
//!
//! Scratchpad: the candidate keeps the accepted special hidden workspace
//! per window ([`ScratchpadSlot`](crate::scratchpad::ScratchpadSlot)) and
//! does **not** add a scene layer for it — whether the scratchpad becomes
//! a layer stays Open for the owning RFC.
//!
//! No window leak: no layer, command, or value here exposes a window
//! handle, native surface, or OS window identifier.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock
//! time, randomness, or platform handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::geometry::Rect;
use crate::layout::LayoutNode;
use crate::panel::PanelId;
use crate::view::ViewId;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on entries per non-tiled layer.
///
/// Rejected with [`SceneError::LayerFull`], never silently dropped.
pub const MAX_LAYER_ENTRIES: usize = 64;

/// Hard cap on panel attachments per scene.
///
/// Rejected with [`SceneError::TooManyPanels`].
pub const MAX_PANELS_PER_SCENE: usize = 256;

/// Hard cap on navigation depth per activity stack.
///
/// Rejected with [`SceneError::ActivityOverflow`].
pub const MAX_ACTIVITY_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to mutate scene or navigation state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SceneError {
    /// The view is not part of the scene (no tiled leaf, no layer entry).
    UnknownView {
        /// The unrecognized view.
        view: ViewId,
    },
    /// The panel has no attachment in the scene.
    UnknownPanel {
        /// The unrecognized panel.
        panel: PanelId,
    },
    /// The view already hosts a panel attachment.
    ViewOccupied {
        /// The occupied view.
        view: ViewId,
    },
    /// The panel is already attached somewhere in the scene.
    PanelAlreadyAttached {
        /// The attached panel.
        panel: PanelId,
    },
    /// The scene holds [`MAX_PANELS_PER_SCENE`] attachments already.
    TooManyPanels {
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The layer holds [`MAX_LAYER_ENTRIES`] entries already.
    LayerFull {
        /// The layer at capacity.
        layer: SceneLayer,
    },
    /// The activity stack holds [`MAX_ACTIVITY_DEPTH`] entries already.
    ActivityOverflow {
        /// The cap that was exceeded.
        cap: usize,
    },
    /// Pop or replace on an empty activity stack.
    ActivityUnderflow,
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownView { view } => write!(f, "view not in scene: {view}"),
            Self::UnknownPanel { panel } => write!(f, "panel not attached: {panel}"),
            Self::ViewOccupied { view } => write!(f, "view already hosts a panel: {view}"),
            Self::PanelAlreadyAttached { panel } => {
                write!(f, "panel already attached: {panel}")
            }
            Self::TooManyPanels { cap } => {
                write!(f, "too many panel attachments: cap {cap}")
            }
            Self::LayerFull { layer } => write!(f, "scene layer at capacity: {layer}"),
            Self::ActivityOverflow { cap } => {
                write!(f, "activity stack overflow: cap {cap}")
            }
            Self::ActivityUnderflow => f.write_str("activity stack is empty"),
        }
    }
}

impl std::error::Error for SceneError {}

// ---------------------------------------------------------------------------
// Identity mirrors
// ---------------------------------------------------------------------------

/// UI-side handle for a workspace scene.
///
/// Mirror of the runtime `WorkspaceId` (owned by `bitty-runtime`); joined
/// by the runtime, never converted — no `From` bridge exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceSceneId(pub u64);

impl WorkspaceSceneId {
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

impl fmt::Display for WorkspaceSceneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorkspaceSceneId({})", self.0)
    }
}

/// UI-side mirror of the runtime `TerminalId`.
///
/// Carried on [`PanelAttachment`] so a panel move preserves the terminal
/// binding untouched. Joined by the runtime, never converted — no `From`
/// bridge exists, and it never aliases `PanelId` or `ViewId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalBinding(pub u64);

impl TerminalBinding {
    /// Creates a binding from a raw value.
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

impl fmt::Display for TerminalBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TerminalBinding({})", self.0)
    }
}

/// Unit of navigation and content within a panel.
///
/// Managed by [`ActivityStack`]; how far activity identity is user-visible
/// stays Open for the owning RFC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivityId(pub u64);

impl ActivityId {
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

impl fmt::Display for ActivityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActivityId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------

/// Named spatial layers of a [`WorkspaceScene`].
///
/// The tiled layer is the accepted `LayoutTree` composition; the other
/// layers generalize the accepted float, overlay, and hidden-workspace
/// behaviors. Whether this exact set survives stays Open for the owning
/// RFC; the z-order contract below is part of the candidate either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SceneLayer {
    /// Tiled layer (accepted `LayoutTree` composition).
    Tiled,
    /// Floating windows above tiling.
    Floating,
    /// Pinned surfaces that survive workspace switches.
    Pinned,
    /// Transient popovers.
    Popover,
    /// Overlays (command palette, jump labels, notifications).
    Overlay,
}

impl SceneLayer {
    /// Every layer, in z-order (lowest first).
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Tiled,
            Self::Floating,
            Self::Pinned,
            Self::Popover,
            Self::Overlay,
        ]
    }

    /// Every non-tiled layer, in z-order (lowest first).
    #[must_use]
    pub const fn non_tiled() -> &'static [Self] {
        &[Self::Floating, Self::Pinned, Self::Popover, Self::Overlay]
    }

    /// Z-order rank (lowest paints first).
    #[must_use]
    pub const fn z_rank(self) -> u8 {
        match self {
            Self::Tiled => 0,
            Self::Floating => 1,
            Self::Pinned => 2,
            Self::Popover => 3,
            Self::Overlay => 4,
        }
    }

    /// Whether this layer is the tiled composition (backed by
    /// [`LayoutNode`](crate::layout::LayoutNode), not [`LayerEntry`]).
    #[must_use]
    pub const fn is_tiled(self) -> bool {
        matches!(self, Self::Tiled)
    }

    /// Candidate vocabulary spelling for this layer.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tiled => "tiled",
            Self::Floating => "floating",
            Self::Pinned => "pinned",
            Self::Popover => "popover",
            Self::Overlay => "overlay",
        }
    }
}

impl fmt::Display for SceneLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One non-tiled placement: a view at explicit scene bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerEntry {
    /// The presented view.
    pub view: ViewId,
    /// Explicit scene bounds for the view.
    pub bounds: Rect,
}

// ---------------------------------------------------------------------------
// Panel attachment
// ---------------------------------------------------------------------------

/// The `Panel`-at-`View` binding: application identity mounted at an
/// internal compositor presentation point.
///
/// `View` stays hidden from end users and plugin APIs and is never a
/// user-facing target; `Panel` is the visible identity. Moving a panel
/// re-parents this binding only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelAttachment {
    /// The application and session identity.
    pub panel: PanelId,
    /// The internal compositor attachment point.
    pub view: ViewId,
    /// The terminal binding, preserved across moves.
    pub terminal: TerminalBinding,
}

// ---------------------------------------------------------------------------
// WorkspaceScene
// ---------------------------------------------------------------------------

/// Headless four-layer scene: tiled composition, layered placements, and
/// panel attachments.
///
/// The tiled layer owns a [`LayoutNode`](crate::layout::LayoutNode); the
/// floating, pinned, popover, and overlay layers hold bounded
/// [`LayerEntry`] placements. Panel attachments join [`PanelId`] to
/// [`ViewId`] with the [`TerminalBinding`] preserved across moves.
///
/// [`LayoutNode`](crate::layout::LayoutNode) carries an `f32` split ratio
/// and implements `PartialEq` only, so scene equality is `PartialEq` too.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceScene {
    id: WorkspaceSceneId,
    tiled: LayoutNode,
    floating: Vec<LayerEntry>,
    pinned: Vec<LayerEntry>,
    popover: Vec<LayerEntry>,
    overlay: Vec<LayerEntry>,
    attachments: Vec<PanelAttachment>,
}

impl WorkspaceScene {
    /// Builds a scene over a tiled composition with all other layers
    /// empty and no attachments.
    #[must_use]
    pub fn new(id: WorkspaceSceneId, tiled: LayoutNode) -> Self {
        Self {
            id,
            tiled,
            floating: Vec::new(),
            pinned: Vec::new(),
            popover: Vec::new(),
            overlay: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// The scene identity.
    #[must_use]
    pub const fn id(&self) -> WorkspaceSceneId {
        self.id
    }

    /// The tiled composition.
    #[must_use]
    pub const fn tiled(&self) -> &LayoutNode {
        &self.tiled
    }

    /// Entries of a non-tiled layer (empty slice for [`SceneLayer::Tiled`],
    /// whose content is the [`WorkspaceScene::tiled`] composition).
    #[must_use]
    pub fn layer_entries(&self, layer: SceneLayer) -> &[LayerEntry] {
        match layer {
            SceneLayer::Tiled => &[],
            SceneLayer::Floating => &self.floating,
            SceneLayer::Pinned => &self.pinned,
            SceneLayer::Popover => &self.popover,
            SceneLayer::Overlay => &self.overlay,
        }
    }

    /// Which layer presents a view, if any.
    #[must_use]
    pub fn layer_of(&self, view: ViewId) -> Option<SceneLayer> {
        if self.tiled.find_leaf(view).is_some() {
            return Some(SceneLayer::Tiled);
        }
        SceneLayer::non_tiled().iter().copied().find(|layer| {
            self.layer_entries(*layer)
                .iter()
                .any(|entry| entry.view == view)
        })
    }

    /// Places a view on a non-tiled layer.
    ///
    /// # Errors
    ///
    /// Returns [`SceneError::LayerFull`] at capacity. The tiled layer is
    /// rejected (it is owned by the [`LayoutNode`](crate::layout::LayoutNode)
    /// composition, edited through its own API).
    pub fn place(&mut self, layer: SceneLayer, entry: LayerEntry) -> Result<(), SceneError> {
        let entries = match layer {
            SceneLayer::Tiled => return Err(SceneError::LayerFull { layer }),
            SceneLayer::Floating => &mut self.floating,
            SceneLayer::Pinned => &mut self.pinned,
            SceneLayer::Popover => &mut self.popover,
            SceneLayer::Overlay => &mut self.overlay,
        };
        if entries.len() >= MAX_LAYER_ENTRIES {
            return Err(SceneError::LayerFull { layer });
        }
        entries.push(entry);
        Ok(())
    }

    /// Removes a view from its non-tiled layer.
    ///
    /// Returns whether an entry was removed. Tiled leaves are owned by
    /// the [`LayoutNode`](crate::layout::LayoutNode) composition and are
    /// never removed here.
    #[must_use]
    pub fn unplace(&mut self, view: ViewId) -> bool {
        for entries in [
            &mut self.floating,
            &mut self.pinned,
            &mut self.popover,
            &mut self.overlay,
        ] {
            if let Some(index) = entries.iter().position(|entry| entry.view == view) {
                entries.remove(index);
                return true;
            }
        }
        false
    }

    /// Attaches a panel at a scene view with its terminal binding.
    ///
    /// # Errors
    ///
    /// Returns [`SceneError::UnknownView`] when the view is not part of
    /// the scene, [`SceneError::ViewOccupied`] when the view already
    /// hosts a panel, [`SceneError::PanelAlreadyAttached`] when the panel
    /// is attached elsewhere, [`SceneError::TooManyPanels`] at capacity.
    pub fn bind(
        &mut self,
        panel: PanelId,
        view: ViewId,
        terminal: TerminalBinding,
    ) -> Result<(), SceneError> {
        if self.layer_of(view).is_none() {
            return Err(SceneError::UnknownView { view });
        }
        if self
            .attachments
            .iter()
            .any(|attachment| attachment.view == view)
        {
            return Err(SceneError::ViewOccupied { view });
        }
        if self
            .attachments
            .iter()
            .any(|attachment| attachment.panel == panel)
        {
            return Err(SceneError::PanelAlreadyAttached { panel });
        }
        if self.attachments.len() >= MAX_PANELS_PER_SCENE {
            return Err(SceneError::TooManyPanels {
                cap: MAX_PANELS_PER_SCENE,
            });
        }
        self.attachments.push(PanelAttachment {
            panel,
            view,
            terminal,
        });
        Ok(())
    }

    /// Detaches a panel, releasing its view.
    ///
    /// # Errors
    ///
    /// Returns [`SceneError::UnknownPanel`] when the panel is not
    /// attached.
    pub fn unbind_panel(&mut self, panel: PanelId) -> Result<PanelAttachment, SceneError> {
        let index = self
            .attachments
            .iter()
            .position(|attachment| attachment.panel == panel)
            .ok_or(SceneError::UnknownPanel { panel })?;
        Ok(self.attachments.remove(index))
    }

    /// Moves a panel to another scene view, re-parenting the presentation
    /// attachment only.
    ///
    /// The panel identity and its terminal binding survive unchanged; the
    /// PTY, `TerminalId`, and Lua VM sequenced behind the binding are
    /// untouched (a move is not a copy, state stays single-owned).
    ///
    /// # Errors
    ///
    /// Returns [`SceneError::UnknownPanel`] when the panel is not
    /// attached, [`SceneError::UnknownView`] when the destination is not
    /// part of the scene, [`SceneError::ViewOccupied`] when the
    /// destination already hosts a panel.
    pub fn move_panel(&mut self, panel: PanelId, view: ViewId) -> Result<(), SceneError> {
        let index = self
            .attachments
            .iter()
            .position(|attachment| attachment.panel == panel)
            .ok_or(SceneError::UnknownPanel { panel })?;
        if self.layer_of(view).is_none() {
            return Err(SceneError::UnknownView { view });
        }
        if self
            .attachments
            .iter()
            .any(|attachment| attachment.view == view)
        {
            return Err(SceneError::ViewOccupied { view });
        }
        self.attachments[index].view = view;
        Ok(())
    }

    /// The attachment of a panel, if attached.
    #[must_use]
    pub fn attachment_of(&self, panel: PanelId) -> Option<PanelAttachment> {
        self.attachments
            .iter()
            .find(|attachment| attachment.panel == panel)
            .copied()
    }

    /// The view presenting a panel, if attached.
    #[must_use]
    pub fn view_of(&self, panel: PanelId) -> Option<ViewId> {
        self.attachment_of(panel).map(|attachment| attachment.view)
    }

    /// All attachments, in bind order.
    #[must_use]
    pub fn attachments(&self) -> &[PanelAttachment] {
        &self.attachments
    }

    /// Z-order of presented views: tiled leaves in [`LayoutNode`] order,
    /// then non-tiled layers lowest rank first, each in placement order.
    ///
    /// Views without geometry here (layer entries carry explicit bounds;
    /// tiled leaves resolve through the layout solver) are listed for
    /// ordering only — this function orders, it never lays out.
    #[must_use]
    pub fn z_order(&self) -> Vec<(SceneLayer, ViewId)> {
        let mut order = Vec::new();
        order.extend(
            self.tiled
                .leaf_ids()
                .into_iter()
                .map(|id| (SceneLayer::Tiled, id)),
        );
        for layer in SceneLayer::non_tiled().iter().copied() {
            order.extend(
                self.layer_entries(layer)
                    .iter()
                    .map(|entry| (layer, entry.view)),
            );
        }
        order
    }
}

// ---------------------------------------------------------------------------
// ActivityStack
// ---------------------------------------------------------------------------

/// Push/pop navigation within one panel.
///
/// The bottom of the stack is the entry activity; the top is current.
/// Bounded by [`MAX_ACTIVITY_DEPTH`]; how the stack composes with focus
/// routing, lifecycle, and persistence stays Open for the owning RFC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityStack {
    panel: PanelId,
    stack: Vec<ActivityId>,
}

impl ActivityStack {
    /// Starts a stack with an entry activity.
    #[must_use]
    pub fn new(panel: PanelId, entry: ActivityId) -> Self {
        Self {
            panel,
            stack: vec![entry],
        }
    }

    /// The owning panel.
    #[must_use]
    pub const fn panel(&self) -> PanelId {
        self.panel
    }

    /// Current (top) activity.
    #[must_use]
    pub fn current(&self) -> ActivityId {
        // Invariant: the stack never empties (pop keeps the entry).
        self.stack.last().copied().unwrap_or(ActivityId::new(0))
    }

    /// Depth of the stack (entry counts as 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Pushes a navigated-to activity.
    ///
    /// # Errors
    ///
    /// Returns [`SceneError::ActivityOverflow`] at
    /// [`MAX_ACTIVITY_DEPTH`]; the stack is unchanged.
    pub fn push(&mut self, activity: ActivityId) -> Result<(), SceneError> {
        if self.stack.len() >= MAX_ACTIVITY_DEPTH {
            return Err(SceneError::ActivityOverflow {
                cap: MAX_ACTIVITY_DEPTH,
            });
        }
        self.stack.push(activity);
        Ok(())
    }

    /// Pops back to the previous activity.
    ///
    /// Popping the entry is a no-op success returning the entry: the
    /// stack never empties, so there is no underflow state to represent.
    #[must_use]
    pub fn pop(&mut self) -> ActivityId {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
        self.current()
    }

    /// Replaces the current activity without changing depth.
    ///
    /// Push/pop history beneath the top is preserved.
    pub fn replace(&mut self, activity: ActivityId) {
        if let Some(top) = self.stack.last_mut() {
            *top = activity;
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::View;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 80, 24))
    }

    fn scene() -> WorkspaceScene {
        WorkspaceScene::new(
            WorkspaceSceneId::new(1),
            LayoutNode::split(
                crate::geometry::SplitAxis::Vertical,
                0.5,
                leaf(11),
                leaf(12),
            ),
        )
    }

    #[test]
    fn layer_ranks_order_z_contract() {
        let ranks: Vec<u8> = SceneLayer::all()
            .iter()
            .map(|layer| layer.z_rank())
            .collect();
        let sorted = {
            let mut copy = ranks.clone();
            copy.sort();
            copy
        };
        assert_eq!(ranks, sorted, "SceneLayer::all must be in z-order");
        assert!(SceneLayer::Tiled.z_rank() < SceneLayer::Overlay.z_rank());
    }

    #[test]
    fn bind_move_preserves_panel_and_terminal() {
        let mut scene = scene();
        scene
            .bind(PanelId::new(7), ViewId::new(11), TerminalBinding::new(42))
            .expect("valid bind");
        scene
            .move_panel(PanelId::new(7), ViewId::new(12))
            .expect("valid move");
        let attachment = scene
            .attachment_of(PanelId::new(7))
            .expect("still attached");
        assert_eq!(attachment.panel, PanelId::new(7));
        assert_eq!(attachment.view, ViewId::new(12));
        assert_eq!(
            attachment.terminal,
            TerminalBinding::new(42),
            "move re-parents presentation only; the terminal binding survives"
        );
        assert_eq!(scene.view_of(PanelId::new(7)), Some(ViewId::new(12)));
    }

    #[test]
    fn bind_rejects_unknown_occupied_and_double_panel() {
        let mut scene = scene();
        assert_eq!(
            scene.bind(PanelId::new(1), ViewId::new(99), TerminalBinding::new(1)),
            Err(SceneError::UnknownView {
                view: ViewId::new(99)
            })
        );
        scene
            .bind(PanelId::new(1), ViewId::new(11), TerminalBinding::new(1))
            .expect("first bind");
        assert_eq!(
            scene.bind(PanelId::new(2), ViewId::new(11), TerminalBinding::new(2)),
            Err(SceneError::ViewOccupied {
                view: ViewId::new(11)
            })
        );
        assert_eq!(
            scene.bind(PanelId::new(1), ViewId::new(12), TerminalBinding::new(1)),
            Err(SceneError::PanelAlreadyAttached {
                panel: PanelId::new(1)
            })
        );
    }

    #[test]
    fn move_rejects_unknown_panel_view_and_occupied() {
        let mut scene = scene();
        assert_eq!(
            scene.move_panel(PanelId::new(9), ViewId::new(12)),
            Err(SceneError::UnknownPanel {
                panel: PanelId::new(9)
            })
        );
        scene
            .bind(PanelId::new(1), ViewId::new(11), TerminalBinding::new(1))
            .expect("bind");
        assert_eq!(
            scene.move_panel(PanelId::new(1), ViewId::new(99)),
            Err(SceneError::UnknownView {
                view: ViewId::new(99)
            })
        );
        scene
            .bind(PanelId::new(2), ViewId::new(12), TerminalBinding::new(2))
            .expect("second bind");
        assert_eq!(
            scene.move_panel(PanelId::new(1), ViewId::new(12)),
            Err(SceneError::ViewOccupied {
                view: ViewId::new(12)
            })
        );
    }

    #[test]
    fn non_tiled_placement_layer_lookup_and_unplace() {
        let mut scene = scene();
        let entry = LayerEntry {
            view: ViewId::new(50),
            bounds: Rect::new(0, 0, 40, 12),
        };
        scene.place(SceneLayer::Floating, entry).expect("place");
        assert_eq!(scene.layer_of(ViewId::new(50)), Some(SceneLayer::Floating));
        assert_eq!(scene.layer_of(ViewId::new(11)), Some(SceneLayer::Tiled));
        assert_eq!(scene.layer_of(ViewId::new(99)), None);
        scene
            .bind(PanelId::new(5), ViewId::new(50), TerminalBinding::new(5))
            .expect("bind on floating view");
        assert!(scene.unplace(ViewId::new(50)));
        assert!(!scene.unplace(ViewId::new(50)));
        assert_eq!(
            scene.place(SceneLayer::Tiled, entry),
            Err(SceneError::LayerFull {
                layer: SceneLayer::Tiled
            })
        );
    }

    #[test]
    fn z_order_lists_tiled_then_layers() {
        let mut scene = scene();
        scene
            .place(
                SceneLayer::Overlay,
                LayerEntry {
                    view: ViewId::new(50),
                    bounds: Rect::new(0, 0, 10, 5),
                },
            )
            .expect("place");
        let order = scene.z_order();
        let views: Vec<u64> = order.iter().map(|(_, view)| view.0).collect();
        assert_eq!(
            views,
            vec![11, 12, 50],
            "tiled leaves first, then layers: {order:?}"
        );
        assert_eq!(order[0].0, SceneLayer::Tiled);
        assert_eq!(order[2].0, SceneLayer::Overlay);
    }

    #[test]
    fn activity_stack_push_pop_replace() {
        let mut stack = ActivityStack::new(PanelId::new(3), ActivityId::new(100));
        assert_eq!(stack.current(), ActivityId::new(100));
        assert_eq!(stack.depth(), 1);
        stack.push(ActivityId::new(101)).expect("push");
        stack.push(ActivityId::new(102)).expect("push");
        assert_eq!(stack.depth(), 3);
        stack.replace(ActivityId::new(103));
        assert_eq!(stack.current(), ActivityId::new(103));
        assert_eq!(stack.depth(), 3);
        assert_eq!(stack.pop(), ActivityId::new(101));
        assert_eq!(stack.pop(), ActivityId::new(100));
        assert_eq!(
            stack.pop(),
            ActivityId::new(100),
            "entry pop is a no-op success"
        );
        assert_eq!(stack.depth(), 1);
    }

    #[test]
    fn activity_stack_overflow_fails_closed() {
        let mut stack = ActivityStack::new(PanelId::new(3), ActivityId::new(0));
        for id in 1..MAX_ACTIVITY_DEPTH {
            stack
                .push(ActivityId::new(id as u64))
                .expect("push within cap");
        }
        assert_eq!(
            stack.push(ActivityId::new(999)),
            Err(SceneError::ActivityOverflow {
                cap: MAX_ACTIVITY_DEPTH
            })
        );
        assert_eq!(stack.depth(), MAX_ACTIVITY_DEPTH);
    }

    #[test]
    fn unbind_releases_view_for_rebind() {
        let mut scene = scene();
        scene
            .bind(PanelId::new(1), ViewId::new(11), TerminalBinding::new(1))
            .expect("bind");
        let released = scene.unbind_panel(PanelId::new(1)).expect("unbind");
        assert_eq!(released.view, ViewId::new(11));
        assert_eq!(
            scene.unbind_panel(PanelId::new(1)),
            Err(SceneError::UnknownPanel {
                panel: PanelId::new(1)
            })
        );
        scene
            .bind(PanelId::new(2), ViewId::new(11), TerminalBinding::new(2))
            .expect("rebind on freed view");
    }

    #[test]
    fn error_display_is_human_readable() {
        assert_eq!(
            SceneError::ViewOccupied {
                view: ViewId::new(4)
            }
            .to_string(),
            "view already hosts a panel: ViewId(4)"
        );
        assert_eq!(
            SceneError::ActivityUnderflow.to_string(),
            "activity stack is empty"
        );
    }
}
