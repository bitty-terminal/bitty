#![forbid(unsafe_code)]
//! Panel placement decision contract (CW-19, issue #997; `RFC-OQ-3`).
//!
//! Contract source: the accepted [Panel Runtime RFC] lists placement
//! between three options, and the owner ruling of 2026-09-23 accepts
//! Option A over the `bitty` owner decision packet (merged `bitty`
//! #1300; register `bitty-docs` #367, Accepted Option A; `bitty` #997
//! closed):
//!
//! - Option A — Panel as typed `View` content
//!   (`ViewContent::Panel(PanelId)`); smallest change, `ViewId` stays leaf.
//!   **Accepted.**
//! - Option B — Panel replaces `View` as `LayoutTree` leaf; **rejected** by
//!   the accepted RFC (breaks `ViewId` generation history).
//! - Option C — Panel composes beside `View` as a side-car binding.
//!   **Not selected**: the refined Option C direction was the prior
//!   candidate and is superseded by the Option A acceptance.
//!
//! Accepted direction: **Option A** in identity terms — Panel is the
//! visible application identity carried as typed `View` content,
//! `View` remains the tiling leaf identity. The `ViewContent::Panel`
//! encoding is the accepted spelling, not a transitional placeholder.
//!
//! What this module provides:
//!
//! - [`PlacementOption`] — the three RFC options, plus [`PLACEMENT_DIRECTION`]
//!   naming the accepted direction (`A`).
//! - [`Placement`] — the explicit directional binding map (`PanelId` mounts
//!   onto a `ViewId`, at most one-to-one). A move re-parents the binding
//!   while preserving both identities; unbinding suspends, never destroys.
//! - [`TRANSITIONAL_ENCODING`] — retained name for the accepted
//!   `ViewContent::Panel(PanelId)` spelling, so call sites keep a stable
//!   reference while the semantics stay Option A.
//! - [`FocusTarget`] + [`resolve_focus`] — the visible focus target is a
//!   `PanelId`; internal hit-testing stays a `View` rectangle; panel focus
//!   wins over view focus.
//!
//! All identities stay pairwise-incompatible newtypes with no `From` bridge:
//! this module performs no `PanelId`/`ViewId` conversion, only map lookups.
//! No PTY descriptor, GPU object, or window handle is held here; the binding
//! map is Core-owned presentation state. Every failure is fail-closed and
//! typed: a failed bind, move, or unbind leaves the map unchanged.
//!
//! [Panel Runtime RFC]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/panel-runtime-rfc.md
//! [Panel Placement Decision]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/panel-placement-decision.md

use std::collections::HashMap;

use crate::panel::PanelId;
use crate::view::ViewId;

// ---------------------------------------------------------------------------
// Decision record: the three RFC options and the recorded direction
// ---------------------------------------------------------------------------

/// The three placement options listed by the accepted Panel Runtime RFC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlacementOption {
    /// Panel as typed `View` content: `ViewContent::Panel(PanelId)`.
    /// Smallest change; `ViewId` remains the tiling leaf identity.
    A,
    /// Panel replaces `View` as the `LayoutTree` leaf. Rejected: breaks
    /// `ViewId` generation history relied on by the compositor.
    B,
    /// Panel composes beside `View` as a side-car binding tracked outside
    /// `ViewContent`. Preserves `View` while giving Panel its own identity.
    C,
}

/// Accepted direction: Option A (Panel as typed `View` content).
///
/// Accepted by the owner ruling of 2026-09-23 (`RFC-OQ-3` Option A;
/// `bitty` #997 closed); supersedes the prior refined Option C candidate.
pub const PLACEMENT_DIRECTION: PlacementOption = PlacementOption::A;

/// Accepted encoding: the `ViewContent::Panel(PanelId)` spelling.
/// Retained under its historic name so call sites keep a stable reference.
pub const TRANSITIONAL_ENCODING: &str = "ViewContent::Panel(PanelId)";

// ---------------------------------------------------------------------------
// Binding map: directional PanelId -> ViewId, at most one-to-one
// ---------------------------------------------------------------------------

/// Typed placement failure; the binding map is unchanged on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementError {
    /// The target view already hosts a panel.
    ViewOccupied { view: ViewId },
    /// The panel is already mounted on a (different) view.
    PanelAlreadyMounted { panel: PanelId },
    /// The panel has no current attachment.
    NotMounted { panel: PanelId },
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ViewOccupied { view } => write!(f, "view {view} already hosts a panel"),
            Self::PanelAlreadyMounted { panel } => {
                write!(f, "panel {panel} is already mounted")
            }
            Self::NotMounted { panel } => write!(f, "panel {panel} is not mounted"),
        }
    }
}

impl std::error::Error for PlacementError {}

/// Explicit directional placement binding: a panel mounts onto a view.
///
/// Mirrors the accepted mount rule (`PanelId` binds to an empty `ViewId`,
/// at most one-to-one in both directions) without owning lifecycle,
/// layout, or focus state. Unbinding suspends the panel; destruction stays
/// with the panel runtime.
#[derive(Clone, Debug, Default)]
pub struct Placement {
    panel_to_view: HashMap<PanelId, ViewId>,
    view_to_panel: HashMap<ViewId, PanelId>,
}

impl Placement {
    /// Creates an empty binding map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mounts `panel` onto `view`. Fails when either side is already bound.
    ///
    /// # Errors
    ///
    /// [`PlacementError::PanelAlreadyMounted`] or
    /// [`PlacementError::ViewOccupied`]; the map is unchanged.
    pub fn bind(&mut self, panel: PanelId, view: ViewId) -> Result<(), PlacementError> {
        if self.panel_to_view.contains_key(&panel) {
            return Err(PlacementError::PanelAlreadyMounted { panel });
        }
        if self.view_to_panel.contains_key(&view) {
            return Err(PlacementError::ViewOccupied { view });
        }
        self.panel_to_view.insert(panel, view);
        self.view_to_panel.insert(view, panel);
        Ok(())
    }

    /// Removes the panel's attachment, returning its former view.
    /// A panel without attachment is suspended, not destroyed.
    ///
    /// # Errors
    ///
    /// [`PlacementError::NotMounted`]; the map is unchanged.
    pub fn unbind(&mut self, panel: PanelId) -> Result<ViewId, PlacementError> {
        let view = self
            .panel_to_view
            .remove(&panel)
            .ok_or(PlacementError::NotMounted { panel })?;
        self.view_to_panel.remove(&view);
        Ok(view)
    }

    /// Moves `panel` to `new_view`, preserving both identities.
    /// Re-parenting the binding is never a copy and never renames either id.
    /// Moving onto the currently attached view is a no-op success.
    ///
    /// # Errors
    ///
    /// [`PlacementError::NotMounted`] or [`PlacementError::ViewOccupied`];
    /// the map is unchanged.
    pub fn reparent(&mut self, panel: PanelId, new_view: ViewId) -> Result<(), PlacementError> {
        let current = self
            .panel_to_view
            .get(&panel)
            .copied()
            .ok_or(PlacementError::NotMounted { panel })?;
        if current == new_view {
            return Ok(());
        }
        if self.view_to_panel.contains_key(&new_view) {
            return Err(PlacementError::ViewOccupied { view: new_view });
        }
        self.panel_to_view.insert(panel, new_view);
        self.view_to_panel.remove(&current);
        self.view_to_panel.insert(new_view, panel);
        Ok(())
    }

    /// Returns the view `panel` is mounted on, if any.
    #[must_use]
    pub fn view_of(&self, panel: PanelId) -> Option<ViewId> {
        self.panel_to_view.get(&panel).copied()
    }

    /// Returns the panel mounted on `view`, if any.
    #[must_use]
    pub fn panel_of(&self, view: ViewId) -> Option<PanelId> {
        self.view_to_panel.get(&view).copied()
    }

    /// Whether `panel` currently has an attachment.
    #[must_use]
    pub fn is_bound(&self, panel: PanelId) -> bool {
        self.panel_to_view.contains_key(&panel)
    }

    /// Number of live bindings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panel_to_view.len()
    }

    /// Whether no binding exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panel_to_view.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Focus routing: visible PanelId target over internal ViewId rectangle
// ---------------------------------------------------------------------------

/// Visible focus target within the active workspace: at most one.
/// The internal hit-testing rectangle remains a `View`; the routed target
/// presented to input is the `Panel` when one is focused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FocusTarget {
    Panel(PanelId),
    View(ViewId),
}

/// Resolves the focused target with the accepted precedence: panel focus
/// wins over view focus; `None` only when neither is focused.
#[must_use]
pub fn resolve_focus(
    panel_focus: Option<PanelId>,
    view_focus: Option<ViewId>,
) -> Option<FocusTarget> {
    if let Some(panel) = panel_focus {
        return Some(FocusTarget::Panel(panel));
    }
    view_focus.map(FocusTarget::View)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(raw: u64) -> PanelId {
        PanelId::new(raw)
    }

    fn view(raw: u64) -> ViewId {
        ViewId::new(raw)
    }

    #[test]
    fn bind_and_lookup_roundtrip() {
        let mut placement = Placement::new();
        assert!(placement.is_empty());
        placement.bind(panel(1), view(10)).unwrap();
        assert_eq!(placement.view_of(panel(1)), Some(view(10)));
        assert_eq!(placement.panel_of(view(10)), Some(panel(1)));
        assert!(placement.is_bound(panel(1)));
        assert_eq!(placement.len(), 1);
    }

    #[test]
    fn double_bind_same_panel_fails_closed() {
        let mut placement = Placement::new();
        placement.bind(panel(1), view(10)).unwrap();
        let err = placement.bind(panel(1), view(11)).unwrap_err();
        assert_eq!(err, PlacementError::PanelAlreadyMounted { panel: panel(1) });
        assert_eq!(placement.view_of(panel(1)), Some(view(10)));
    }

    #[test]
    fn bind_occupied_view_fails_closed() {
        let mut placement = Placement::new();
        placement.bind(panel(1), view(10)).unwrap();
        let err = placement.bind(panel(2), view(10)).unwrap_err();
        assert_eq!(err, PlacementError::ViewOccupied { view: view(10) });
        assert_eq!(placement.panel_of(view(10)), Some(panel(1)));
    }

    #[test]
    fn unbind_suspends_without_destroying_identity() {
        let mut placement = Placement::new();
        placement.bind(panel(1), view(10)).unwrap();
        assert_eq!(placement.unbind(panel(1)), Ok(view(10)));
        assert!(!placement.is_bound(panel(1)));
        assert_eq!(placement.panel_of(view(10)), None);
        // Rebinding elsewhere reuses the same identity.
        placement.bind(panel(1), view(11)).unwrap();
        assert_eq!(placement.view_of(panel(1)), Some(view(11)));
    }

    #[test]
    fn unbind_unknown_panel_fails() {
        let mut placement = Placement::new();
        assert_eq!(
            placement.unbind(panel(9)),
            Err(PlacementError::NotMounted { panel: panel(9) })
        );
    }

    #[test]
    fn reparent_preserves_both_identities() {
        let mut placement = Placement::new();
        placement.bind(panel(1), view(10)).unwrap();
        placement.reparent(panel(1), view(11)).unwrap();
        assert_eq!(placement.view_of(panel(1)), Some(view(11)));
        assert_eq!(placement.panel_of(view(10)), None);
        assert_eq!(placement.panel_of(view(11)), Some(panel(1)));
        // Same-view move is a no-op success.
        placement.reparent(panel(1), view(11)).unwrap();
        assert_eq!(placement.len(), 1);
    }

    #[test]
    fn reparent_to_occupied_view_fails_closed() {
        let mut placement = Placement::new();
        placement.bind(panel(1), view(10)).unwrap();
        placement.bind(panel(2), view(11)).unwrap();
        let err = placement.reparent(panel(1), view(11)).unwrap_err();
        assert_eq!(err, PlacementError::ViewOccupied { view: view(11) });
        assert_eq!(placement.view_of(panel(1)), Some(view(10)));
    }

    #[test]
    fn panel_focus_wins_over_view_focus() {
        assert_eq!(
            resolve_focus(Some(panel(1)), Some(view(10))),
            Some(FocusTarget::Panel(panel(1)))
        );
        assert_eq!(
            resolve_focus(None, Some(view(10))),
            Some(FocusTarget::View(view(10)))
        );
        assert_eq!(resolve_focus(None, None), None);
    }

    #[test]
    fn accepted_direction_is_option_a() {
        assert_eq!(PLACEMENT_DIRECTION, PlacementOption::A);
        assert_eq!(TRANSITIONAL_ENCODING, "ViewContent::Panel(PanelId)");
    }
}
