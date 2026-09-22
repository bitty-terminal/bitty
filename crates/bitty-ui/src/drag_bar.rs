//! Drag-to-Bar semantics (UX-10, issue #1016).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC; hit
//! zones, preview, undo, and the capacity bound stay `[BLOCKED: OQ-052]`).
//! Nothing here is normative, accepted, or verified: the center/edge
//! split, the edge width, and the capacity below are candidate spellings
//! the owning RFC accepts or rejects, never this module. The module is
//! English-only.
//!
//! What this module provides:
//!
//! - [`classify_bar_drop`] — the candidate hit zones over the Bar width
//!   in cells: the center band means move+switch
//!   ([`BarZone::MoveSwitch`]), the outer [`BAR_EDGE_CELLS`] cells on each
//!   side mean new workspace ([`BarZone::NewWorkspace`]). Off-Bar positions
//!   return `None` (no zone, no preview, no commit).
//! - [`BarDropSession`] — the lift/preview/commit/cancel lifecycle for one
//!   drag onto the Bar. Preview is advisory only and never mutates the
//!   order; commit validates the bound workspace command, applies the move
//!   or the new-workspace split, and pushes a bounded undo entry.
//!   Cancellation (Esc) settles as [`BarOutcome::Cancelled`] with the order
//!   and the undo stack untouched. The bound command is stored, never
//!   executed here.
//! - [`BarUndoStack`] — bounded (`MAX_BAR_UNDO`, `DropOldest`) undo for Bar
//!   commits: each entry restores the exact pre-commit order and workspace
//!   assignment. Undo of an undo is not stacked (redo stays open in the
//!   RFC).
//!
//! The "order" here is the Bar's panel sequence (`Vec<PanelId>`) plus a
//! workspace-of-origin index per dragged panel, kept by the caller (the
//! runtime owns the tree). This module owns the zone and lifecycle rules
//! only, mirroring the [`DragHistory`](crate::drag::DragHistory) undo
//! discipline without taking a `LayoutNode`.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::panel::{CommandRegistry, PanelId, QualifiedCommand};

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Cells on each Bar edge resolving to [`BarZone::NewWorkspace`].
///
/// Rejected positions (off-Bar) return no zone instead of clamping:
/// clamping would preview a workspace the pointer never indicated.
pub const BAR_EDGE_CELLS: u16 = 3;

/// Hard cap on workspaces a Bar may address.
///
/// An edge drop at capacity fails with [`BarError::WorkspaceCap`], never
/// by evicting a workspace.
pub const MAX_BAR_WORKSPACES: usize = 16;

/// Hard cap on undo entries per [`BarUndoStack`] (`DropOldest`).
pub const MAX_BAR_UNDO: usize = 16;

/// Workspace command applying a Bar move (`owner.name:command` grammar,
/// see [`QualifiedCommand`]).
pub const BAR_MOVE_CMD: &str = "bitty.workspace:bar-move";

/// Workspace command applying a Bar edge split onto a new workspace.
pub const BAR_SPLIT_CMD: &str = "bitty.workspace:bar-split";

// ---------------------------------------------------------------------------
// BarZone + classify
// ---------------------------------------------------------------------------

/// Candidate drop zone over the Bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarZone {
    /// Center band: move the dragged panel and switch to its workspace.
    MoveSwitch,
    /// Outer edge: drop onto a new workspace.
    NewWorkspace,
}

impl BarZone {
    /// Canonical kebab-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MoveSwitch => "move-switch",
            Self::NewWorkspace => "new-workspace",
        }
    }

    /// The workspace command committed for this zone.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::MoveSwitch => BAR_MOVE_CMD,
            Self::NewWorkspace => BAR_SPLIT_CMD,
        }
    }
}

impl fmt::Display for BarZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classifies a pointer cell `x` over a Bar of `width` cells.
///
/// Returns `None` for `x >= width` (off-Bar). A Bar narrower than twice
/// the edge is all center: edges never swallow the move target.
#[must_use]
pub const fn classify_bar_drop(width: u16, x: u16) -> Option<BarZone> {
    if x >= width {
        return None;
    }
    if width >= BAR_EDGE_CELLS * 2 && (x < BAR_EDGE_CELLS || x >= width - BAR_EDGE_CELLS) {
        return Some(BarZone::NewWorkspace);
    }
    Some(BarZone::MoveSwitch)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to lift, preview, commit, cancel, or undo a Bar drop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarError {
    /// Dragging needs the modifier held; the session never lifts.
    ModNotHeld,
    /// The command fails the `owner.name:command` grammar.
    BadCommand(String),
    /// The command is not a Bar command for this zone.
    UnexpectedCommand {
        /// The bound command.
        found: String,
        /// The command the zone requires.
        expected: &'static str,
    },
    /// The command is unregistered, so the commit is refused.
    UnregisteredCommand(String),
    /// An edge split needs a new workspace but the Bar is at capacity.
    WorkspaceCap {
        /// Workspaces already addressed.
        current: usize,
    },
    /// Nothing was recorded to undo.
    NothingToUndo,
    /// The dragged panel is not in the submitted order.
    UnknownPanel {
        /// The unrecognized panel.
        panel: PanelId,
    },
}

impl fmt::Display for BarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModNotHeld => f.write_str("bar drag needs the modifier held"),
            Self::BadCommand(cmd) => write!(f, "bad bar command: {cmd}"),
            Self::UnexpectedCommand { found, expected } => {
                write!(f, "unexpected bar command '{found}', expected '{expected}'")
            }
            Self::UnregisteredCommand(cmd) => {
                write!(f, "bar command not registered: {cmd}")
            }
            Self::WorkspaceCap { current } => {
                write!(f, "bar workspace capacity reached: {current}")
            }
            Self::NothingToUndo => f.write_str("no bar drop to undo"),
            Self::UnknownPanel { panel } => write!(f, "bar order has no panel {panel}"),
        }
    }
}

impl std::error::Error for BarError {}

// ---------------------------------------------------------------------------
// BarDropPreview / BarOutcome
// ---------------------------------------------------------------------------

/// Advisory preview of a Bar hover: zone only, never a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BarDropPreview {
    /// The classified zone.
    pub zone: BarZone,
    /// The hovered workspace index (center) or the would-be new index (edge).
    pub workspace: u32,
}

/// Settlement report for a [`BarDropSession`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarOutcome {
    /// The drop committed through the command registry.
    Committed(BarZone),
    /// The drag was cancelled (Esc); order and undo stack untouched.
    Cancelled,
}

// ---------------------------------------------------------------------------
// BarUndoStack
// ---------------------------------------------------------------------------

/// One restorable Bar commit: pre-commit order plus workspace count.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BarUndoEntry {
    order: Vec<PanelId>,
    workspaces: usize,
}

/// Bounded undo for Bar commits (`DropOldest`, no redo stacking).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BarUndoStack {
    entries: Vec<BarUndoEntry>,
}

impl BarUndoStack {
    /// Creates an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of stored entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn push(&mut self, entry: BarUndoEntry) {
        if self.entries.len() >= MAX_BAR_UNDO {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }

    /// Restores the most recent entry into `order`/`workspaces`.
    fn pop_into(
        &mut self,
        order: &mut Vec<PanelId>,
        workspaces: &mut usize,
    ) -> Result<(), BarError> {
        match self.entries.pop() {
            Some(entry) => {
                *order = entry.order;
                *workspaces = entry.workspaces;
                Ok(())
            }
            None => Err(BarError::NothingToUndo),
        }
    }
}

// ---------------------------------------------------------------------------
// BarDropSession
// ---------------------------------------------------------------------------

/// Lifecycle phase of a [`BarDropSession`] before settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarPhase {
    /// Lifted with the modifier held; no hover recorded yet.
    Lifted,
    /// At least one preview hover was recorded.
    Previewing,
}

/// One drag onto the Bar: lift, advisory preview, atomic commit or cancel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarDropSession {
    panel: PanelId,
    command: String,
    phase: BarPhase,
    preview: Option<BarDropPreview>,
}

impl BarDropSession {
    /// Lifts a drag of `panel` with the modifier held.
    ///
    /// The command must parse as [`QualifiedCommand`]; it is stored, never
    /// executed here.
    pub fn lift(panel: PanelId, mod_held: bool, command: &str) -> Result<Self, BarError> {
        if !mod_held {
            return Err(BarError::ModNotHeld);
        }
        QualifiedCommand::parse(command).map_err(|_| BarError::BadCommand(command.to_owned()))?;
        Ok(Self {
            panel,
            command: command.to_owned(),
            phase: BarPhase::Lifted,
            preview: None,
        })
    }

    /// The dragged panel.
    #[must_use]
    pub fn panel(&self) -> PanelId {
        self.panel
    }

    /// Current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> BarPhase {
        self.phase
    }

    /// Last advisory preview, if any.
    #[must_use]
    pub fn preview(&self) -> Option<BarDropPreview> {
        self.preview
    }

    /// Records an advisory hover over `workspace` at Bar cell `x`.
    ///
    /// Off-Bar positions clear the preview (no zone, no commit target).
    pub fn preview_hover(&mut self, width: u16, x: u16, workspace: u32) {
        self.preview = classify_bar_drop(width, x).map(|zone| BarDropPreview { zone, workspace });
        self.phase = BarPhase::Previewing;
    }

    /// Cancels the drag (Esc): order and undo stack untouched.
    #[must_use]
    pub fn cancel(self) -> BarOutcome {
        BarOutcome::Cancelled
    }

    /// Commits the previewed drop.
    ///
    /// Validation order (order and undo stack untouched on every failure):
    /// zone command match, registry ownership, panel membership, workspace
    /// capacity (edge only). A center drop moves the panel to the end of
    /// `order` (the switch target); an edge drop appends it and grows the
    /// workspace count by one. The pre-commit state pushes to `undo`.
    pub fn commit(
        self,
        registry: &CommandRegistry,
        order: &mut Vec<PanelId>,
        workspaces: &mut usize,
        undo: &mut BarUndoStack,
    ) -> Result<BarOutcome, BarError> {
        let preview = self.preview.ok_or_else(|| BarError::UnexpectedCommand {
            found: self.command.clone(),
            expected: "previewed zone command",
        })?;
        if self.command != preview.zone.command() {
            return Err(BarError::UnexpectedCommand {
                found: self.command.clone(),
                expected: preview.zone.command(),
            });
        }
        if registry.owner_of(&self.command).is_none() {
            return Err(BarError::UnregisteredCommand(self.command.clone()));
        }
        if !order.contains(&self.panel) {
            return Err(BarError::UnknownPanel { panel: self.panel });
        }
        if *workspaces > MAX_BAR_WORKSPACES {
            return Err(BarError::WorkspaceCap {
                current: *workspaces,
            });
        }
        if preview.zone == BarZone::NewWorkspace && *workspaces >= MAX_BAR_WORKSPACES {
            return Err(BarError::WorkspaceCap {
                current: *workspaces,
            });
        }
        undo.push(BarUndoEntry {
            order: order.clone(),
            workspaces: *workspaces,
        });
        order.retain(|p| *p != self.panel);
        order.push(self.panel);
        if preview.zone == BarZone::NewWorkspace {
            *workspaces += 1;
        }
        Ok(BarOutcome::Committed(preview.zone))
    }
}

/// Undoes the most recent Bar commit into `order`/`workspaces`.
pub fn undo_bar_drop(
    undo: &mut BarUndoStack,
    order: &mut Vec<PanelId>,
    workspaces: &mut usize,
) -> Result<(), BarError> {
    undo.pop_into(order, workspaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_split_center_from_edges() {
        assert_eq!(classify_bar_drop(20, 0), Some(BarZone::NewWorkspace));
        assert_eq!(classify_bar_drop(20, 19), Some(BarZone::NewWorkspace));
        assert_eq!(classify_bar_drop(20, 10), Some(BarZone::MoveSwitch));
        assert_eq!(classify_bar_drop(20, 20), None);
        assert_eq!(classify_bar_drop(4, 0), Some(BarZone::MoveSwitch));
    }

    #[test]
    fn cancel_leaves_everything_untouched() {
        let session = BarDropSession::lift(PanelId::new(1), true, BAR_MOVE_CMD).unwrap();
        assert_eq!(session.cancel(), BarOutcome::Cancelled);
    }
}
