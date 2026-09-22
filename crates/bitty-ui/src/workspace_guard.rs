//! Never-empty workspace contract support (UX-08, issue #1014).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC; the
//! last-panel-close policy itself is `[BLOCKED: OQ-058]`). Nothing here is
//! normative, accepted, or verified: this module does not choose between
//! reassign, merge, and park — it implements all three as explicit,
//! caller-selected outcomes so the RFC decides between spellings that
//! already run. Every bound and rule below is a candidate the owning RFC
//! accepts or rejects, never this module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`LastPanelPolicy`] — the three candidate outcomes for closing the
//!   last panel of a workspace: [`LastPanelPolicy::Reassign`] (pull a
//!   donor panel from another workspace), [`LastPanelPolicy::Merge`]
//!   (fold the workspace into a surviving target), [`LastPanelPolicy::Park`]
//!   (keep the workspace as an explicitly parked shell).
//! - [`WorkspaceGuard`] — a headless workspace set enforcing the
//!   never-empty invariant: a close that would empty a workspace applies
//!   the caller-selected policy instead, and the call reports a
//!   [`CloseResolution`] naming exactly what happened.
//! - [`WorkspaceGuard::reconcile_focus`] — zero-focus reconciliation: a
//!   workspace whose focus names a departed panel falls back to the
//!   smallest surviving [`PanelId`](crate::panel::PanelId), and reports
//!   [`FocusResolution::Cleared`] only when truly panel-less (parked).
//!
//! A workspace is identified by a plain `u32` index. These are UI-side
//! ordinals for the guard table only, never window handles and never
//! terminal ids.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::panel::PanelId;

/// Hard cap on workspaces tracked by one guard.
///
/// Rejected with [`GuardError::TooManyWorkspaces`], never silently merged.
pub const MAX_GUARDED_WORKSPACES: usize = 16;

/// Hard cap on panels per guarded workspace.
///
/// Rejected with [`GuardError::WorkspaceFull`], never silently dropped.
pub const MAX_GUARD_PANELS: usize = 256;

// ---------------------------------------------------------------------------
// LastPanelPolicy: the open decision, as data
// ---------------------------------------------------------------------------

/// Candidate outcome for closing the last panel of a workspace
/// (decision: `OQ-058`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LastPanelPolicy {
    /// Pull a donor panel from another workspace into the closing one.
    /// Needs [`CloseRequest::donor`](CloseRequest); without a donor the
    /// close fails with [`GuardError::NeedsDonor`] and nothing changes.
    Reassign,
    /// Fold the closing workspace into a surviving target workspace.
    /// Needs [`CloseRequest::target`](CloseRequest); without a target the
    /// close fails with [`GuardError::NeedsTarget`] and nothing changes.
    Merge,
    /// Keep the workspace as an explicitly parked shell: zero panels, no
    /// focus, [`GuardedWorkspace::parked`](GuardedWorkspace::parked) set.
    /// Parking is visible state, never a silent empty.
    Park,
}

impl LastPanelPolicy {
    /// Canonical kebab-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reassign => "reassign",
            Self::Merge => "merge",
            Self::Park => "park",
        }
    }
}

impl fmt::Display for LastPanelPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to evolve guarded workspace membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardError {
    /// No workspace exists at this index.
    UnknownWorkspace {
        /// The unrecognized index.
        workspace: u32,
    },
    /// The panel is not a member of this workspace.
    UnknownPanel {
        /// The workspace index.
        workspace: u32,
        /// The unrecognized panel.
        panel: PanelId,
    },
    /// A `Reassign` close named no donor; nothing changed.
    NeedsDonor,
    /// The named donor panel is not where the request claims.
    BadDonor {
        /// The workspace index.
        workspace: u32,
        /// The missing donor panel.
        panel: PanelId,
    },
    /// A `Merge` close named no target; nothing changed.
    NeedsTarget,
    /// A `Merge` close targeted the closing workspace itself.
    SelfMerge {
        /// The workspace index.
        workspace: u32,
    },
    /// More than [`MAX_GUARDED_WORKSPACES`] workspaces were submitted.
    TooManyWorkspaces {
        /// Workspaces counted in the submitted batch.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The workspace already holds [`MAX_GUARD_PANELS`] panels.
    WorkspaceFull {
        /// The workspace index.
        workspace: u32,
    },
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWorkspace { workspace } => {
                write!(f, "unknown workspace: {workspace}")
            }
            Self::UnknownPanel { workspace, panel } => {
                write!(f, "workspace {workspace} has no panel {panel}")
            }
            Self::NeedsDonor => f.write_str("reassign close needs a donor panel"),
            Self::BadDonor { workspace, panel } => {
                write!(f, "donor {panel} is not in workspace {workspace}")
            }
            Self::NeedsTarget => f.write_str("merge close needs a target workspace"),
            Self::SelfMerge { workspace } => {
                write!(f, "cannot merge workspace {workspace} into itself")
            }
            Self::TooManyWorkspaces { found, cap } => {
                write!(f, "too many workspaces: {found}, cap {cap}")
            }
            Self::WorkspaceFull { workspace } => {
                write!(f, "workspace {workspace} is full")
            }
        }
    }
}

impl std::error::Error for GuardError {}

// ---------------------------------------------------------------------------
// GuardedWorkspace / CloseRequest / CloseResolution / FocusResolution
// ---------------------------------------------------------------------------

/// One guarded workspace: member panels, optional focus, parked flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedWorkspace {
    panels: BTreeSet<PanelId>,
    focus: Option<PanelId>,
    parked: bool,
}

impl GuardedWorkspace {
    /// Whether the workspace holds no panels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// Returns the number of member panels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    /// Whether this workspace is a parked shell (kept, panel-less, by a
    /// [`LastPanelPolicy::Park`] close).
    #[must_use]
    pub fn parked(&self) -> bool {
        self.parked
    }

    /// Member panels in ascending id order.
    #[must_use]
    pub fn panels(&self) -> Vec<PanelId> {
        self.panels.iter().copied().collect()
    }

    /// The focused panel, if any.
    #[must_use]
    pub fn focus(&self) -> Option<PanelId> {
        self.focus
    }
}

/// A close call: which panel, which last-panel policy, which companions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CloseRequest {
    /// The panel to close.
    pub panel: PanelId,
    /// The outcome when `panel` is the last member.
    pub policy: LastPanelPolicy,
    /// Donor `(workspace, panel)` pulled in under `Reassign`.
    pub donor: Option<(u32, PanelId)>,
    /// Surviving workspace absorbing members under `Merge`.
    pub target: Option<u32>,
}

/// What a guarded close did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseResolution {
    /// The panel closed; the workspace still holds panels.
    Closed,
    /// The last panel closed and the donor moved in (reassign).
    Reassigned {
        /// The donor panel now member here.
        donor: PanelId,
    },
    /// The workspace folded into the target (merge); it no longer exists.
    Merged {
        /// The surviving workspace index.
        into: u32,
    },
    /// The workspace kept as a parked shell (park).
    Parked,
}

/// What focus reconciliation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FocusResolution {
    /// Focus already names a member; untouched.
    Kept,
    /// Focus fell back to the smallest surviving panel.
    FellBack {
        /// The new focus.
        to: PanelId,
    },
    /// No panel survives (parked); focus cleared explicitly.
    Cleared,
}

// ---------------------------------------------------------------------------
// WorkspaceGuard
// ---------------------------------------------------------------------------

/// A headless workspace set enforcing the never-empty invariant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceGuard {
    workspaces: BTreeMap<u32, GuardedWorkspace>,
}

impl WorkspaceGuard {
    /// Creates an empty guard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of tracked workspaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    /// Whether no workspace is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// Views one workspace.
    #[must_use]
    pub fn get(&self, workspace: u32) -> Option<&GuardedWorkspace> {
        self.workspaces.get(&workspace)
    }

    /// Adds an empty workspace at `index`.
    pub fn add_workspace(&mut self, index: u32) -> Result<(), GuardError> {
        if self.workspaces.contains_key(&index) {
            return Ok(());
        }
        if self.workspaces.len() >= MAX_GUARDED_WORKSPACES {
            return Err(GuardError::TooManyWorkspaces {
                found: self.workspaces.len() + 1,
                cap: MAX_GUARDED_WORKSPACES,
            });
        }
        self.workspaces.insert(
            index,
            GuardedWorkspace {
                panels: BTreeSet::new(),
                focus: None,
                parked: false,
            },
        );
        Ok(())
    }

    /// Opens `panel` in `workspace` (unparks a parked shell).
    pub fn open_panel(&mut self, workspace: u32, panel: PanelId) -> Result<(), GuardError> {
        let ws = self
            .workspaces
            .get_mut(&workspace)
            .ok_or(GuardError::UnknownWorkspace { workspace })?;
        if ws.panels.len() >= MAX_GUARD_PANELS {
            return Err(GuardError::WorkspaceFull { workspace });
        }
        ws.panels.insert(panel);
        ws.parked = false;
        if ws.focus.is_none() {
            ws.focus = Some(panel);
        }
        Ok(())
    }

    /// Focuses `panel` in `workspace`.
    pub fn focus_panel(&mut self, workspace: u32, panel: PanelId) -> Result<(), GuardError> {
        let ws = self
            .workspaces
            .get_mut(&workspace)
            .ok_or(GuardError::UnknownWorkspace { workspace })?;
        if !ws.panels.contains(&panel) {
            return Err(GuardError::UnknownPanel { workspace, panel });
        }
        ws.focus = Some(panel);
        Ok(())
    }

    /// Closes a panel under the never-empty invariant.
    ///
    /// Non-last closes remove the panel and reconcile focus. Last-panel
    /// closes apply `request.policy` instead of emptying the workspace.
    /// Every failure leaves all workspaces untouched.
    pub fn close_panel(
        &mut self,
        workspace: u32,
        request: CloseRequest,
    ) -> Result<CloseResolution, GuardError> {
        let ws = self
            .workspaces
            .get(&workspace)
            .ok_or(GuardError::UnknownWorkspace { workspace })?;
        if !ws.panels.contains(&request.panel) {
            return Err(GuardError::UnknownPanel {
                workspace,
                panel: request.panel,
            });
        }
        if ws.panels.len() > 1 {
            let ws = self.workspaces.get_mut(&workspace).expect("checked above");
            ws.panels.remove(&request.panel);
            if ws.focus == Some(request.panel) {
                let next = ws.panels.iter().copied().next();
                ws.focus = next;
            }
            return Ok(CloseResolution::Closed);
        }
        match request.policy {
            LastPanelPolicy::Reassign => {
                let (donor_ws, donor) = request.donor.ok_or(GuardError::NeedsDonor)?;
                if donor_ws == workspace {
                    return Err(GuardError::BadDonor {
                        workspace: donor_ws,
                        panel: donor,
                    });
                }
                let donor_state =
                    self.workspaces
                        .get(&donor_ws)
                        .ok_or(GuardError::UnknownWorkspace {
                            workspace: donor_ws,
                        })?;
                if !donor_state.panels.contains(&donor) {
                    return Err(GuardError::BadDonor {
                        workspace: donor_ws,
                        panel: donor,
                    });
                }
                if donor_state.panels.len() <= 1 {
                    return Err(GuardError::BadDonor {
                        workspace: donor_ws,
                        panel: donor,
                    });
                }
                let from = self.workspaces.get_mut(&donor_ws).expect("checked above");
                from.panels.remove(&donor);
                if from.focus == Some(donor) {
                    from.focus = from.panels.iter().copied().next();
                }
                let into = self.workspaces.get_mut(&workspace).expect("checked above");
                into.panels.remove(&request.panel);
                into.panels.insert(donor);
                into.focus = Some(donor);
                Ok(CloseResolution::Reassigned { donor })
            }
            LastPanelPolicy::Merge => {
                let target = request.target.ok_or(GuardError::NeedsTarget)?;
                if target == workspace {
                    return Err(GuardError::SelfMerge { workspace });
                }
                if !self.workspaces.contains_key(&target) {
                    return Err(GuardError::UnknownWorkspace { workspace: target });
                }
                let closing = self.workspaces.remove(&workspace).expect("checked above");
                let survivor = self.workspaces.get_mut(&target).expect("checked above");
                for panel in closing.panels {
                    if panel != request.panel && survivor.panels.len() < MAX_GUARD_PANELS {
                        survivor.panels.insert(panel);
                    }
                }
                if survivor.focus.is_none() {
                    survivor.focus = survivor.panels.iter().copied().next();
                }
                Ok(CloseResolution::Merged { into: target })
            }
            LastPanelPolicy::Park => {
                let ws = self.workspaces.get_mut(&workspace).expect("checked above");
                ws.panels.remove(&request.panel);
                ws.focus = None;
                ws.parked = true;
                Ok(CloseResolution::Parked)
            }
        }
    }

    /// Reconciles focus after an out-of-band membership change.
    ///
    /// Keeps a focus that still names a member, falls back to the smallest
    /// surviving panel, and clears explicitly only when panel-less.
    pub fn reconcile_focus(&mut self, workspace: u32) -> Result<FocusResolution, GuardError> {
        let ws = self
            .workspaces
            .get_mut(&workspace)
            .ok_or(GuardError::UnknownWorkspace { workspace })?;
        if let Some(focus) = ws.focus {
            if ws.panels.contains(&focus) {
                return Ok(FocusResolution::Kept);
            }
        }
        match ws.panels.iter().copied().next() {
            Some(next) => {
                ws.focus = Some(next);
                Ok(FocusResolution::FellBack { to: next })
            }
            None => {
                ws.focus = None;
                Ok(FocusResolution::Cleared)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_panel_guard() -> WorkspaceGuard {
        let mut g = WorkspaceGuard::new();
        g.add_workspace(0).unwrap();
        g.open_panel(0, PanelId::new(5)).unwrap();
        g.open_panel(0, PanelId::new(7)).unwrap();
        g
    }

    #[test]
    fn non_last_close_removes_and_reconciles() {
        let mut g = two_panel_guard();
        g.focus_panel(0, PanelId::new(5)).unwrap();
        let out = g
            .close_panel(
                0,
                CloseRequest {
                    panel: PanelId::new(5),
                    policy: LastPanelPolicy::Park,
                    donor: None,
                    target: None,
                },
            )
            .unwrap();
        assert_eq!(out, CloseResolution::Closed);
        assert_eq!(g.get(0).unwrap().focus(), Some(PanelId::new(7)));
    }

    #[test]
    fn last_close_parks_instead_of_emptying_silently() {
        let mut g = WorkspaceGuard::new();
        g.add_workspace(0).unwrap();
        g.open_panel(0, PanelId::new(1)).unwrap();
        let out = g
            .close_panel(
                0,
                CloseRequest {
                    panel: PanelId::new(1),
                    policy: LastPanelPolicy::Park,
                    donor: None,
                    target: None,
                },
            )
            .unwrap();
        assert_eq!(out, CloseResolution::Parked);
        let ws = g.get(0).unwrap();
        assert!(ws.is_empty() && ws.parked() && ws.focus().is_none());
    }

    #[test]
    fn reassign_without_donor_fails_closed() {
        let mut g = WorkspaceGuard::new();
        g.add_workspace(0).unwrap();
        g.open_panel(0, PanelId::new(1)).unwrap();
        let err = g
            .close_panel(
                0,
                CloseRequest {
                    panel: PanelId::new(1),
                    policy: LastPanelPolicy::Reassign,
                    donor: None,
                    target: None,
                },
            )
            .unwrap_err();
        assert_eq!(err, GuardError::NeedsDonor);
        assert_eq!(g.get(0).unwrap().len(), 1);
    }
}
