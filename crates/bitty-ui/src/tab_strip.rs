//! Tab bar as a Panel projection (UX-12, issue #1018).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC; the
//! per-workspace vs per-window order and persistence stay `[BLOCKED:
//! OQ-052]`, refining `UX-05`). Nothing here is normative, accepted, or
//! verified: every bound, scope rule, and spelling below is a candidate
//! the owning RFC accepts or rejects, never this module. The module is
//! English-only.
//!
//! What this module provides:
//!
//! - [`TabStrip`] — the headless tab order for one [`TabScope`]: an
//!   ordered, bounded entry list supporting reorder (drag within the
//!   strip), move-to-scope, and close. The strip is a *projection*: it
//!   names [`PanelId`](crate::panel::PanelId) values in display order and
//!   owns no panel state — closing a tab detaches the entry, and the
//!   never-empty invariant stays with
//!   [`WorkspaceGuard`](crate::workspace_guard::WorkspaceGuard).
//! - [`TabScope`] — the two candidate orders: [`TabScope::PerWorkspace`]
//!   (one strip per workspace index) and [`TabScope::PerWindow`] (one
//!   strip following window focus). Which order is canonical stays open
//!   (`OQ-052`); both run here so the RFC compares working spellings.
//! - [`snapshot`](TabStrip::snapshot) / [`restore`](TabStrip::restore) —
//!   the persistence spelling: a strip snapshots to its id sequence and
//!   restores from one, rejecting overlong sequences fail-closed. Titles
//!   persist through
//!   [`IdentityRegistry`](crate::panel_identity::IdentityRegistry), never
//!   duplicated here.
//!
//! Display joins here with identity there: position `i` (0-based) renders
//! as physical slot `i + 1` through
//!   [`tab_label`](crate::panel_identity::tab_label), so a reorder is
//! visible as a slot change with identities untouched.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::panel::PanelId;

/// Hard cap on tabs per strip.
///
/// Rejected with [`TabError::TooManyTabs`], never silently clipped:
/// clipping would present a partial strip as the full order.
pub const MAX_TABS_PER_STRIP: usize = 64;

// ---------------------------------------------------------------------------
// TabScope
// ---------------------------------------------------------------------------

/// Candidate order a [`TabStrip`] projects (decision: `OQ-052`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabScope {
    /// One strip per workspace index.
    PerWorkspace(u32),
    /// One strip for the whole window, following focus.
    PerWindow,
}

impl TabScope {
    /// Canonical display name.
    #[must_use]
    pub fn as_str(self) -> String {
        match self {
            Self::PerWorkspace(index) => format!("workspace-{index}"),
            Self::PerWindow => "window".to_owned(),
        }
    }
}

impl fmt::Display for TabScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to evolve a tab strip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabError {
    /// The tab names a panel with no entry in this strip.
    UnknownTab {
        /// The unrecognized panel.
        panel: PanelId,
    },
    /// The panel already has a tab in this strip.
    DuplicateTab {
        /// The duplicated panel.
        panel: PanelId,
    },
    /// The strip already holds [`MAX_TABS_PER_STRIP`] tabs.
    TooManyTabs {
        /// Tabs counted including the submitted one.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A reorder endpoint is outside the strip.
    IndexOutOfRange {
        /// The rejected index.
        index: usize,
        /// Tabs currently held.
        len: usize,
    },
    /// A restore sequence exceeds [`MAX_TABS_PER_STRIP`].
    RestoreTooLong {
        /// Entries counted in the submitted sequence.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
}

impl fmt::Display for TabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTab { panel } => write!(f, "no tab for panel {panel}"),
            Self::DuplicateTab { panel } => write!(f, "duplicate tab for panel {panel}"),
            Self::TooManyTabs { found, cap } => {
                write!(f, "too many tabs: {found}, cap {cap}")
            }
            Self::IndexOutOfRange { index, len } => {
                write!(f, "tab index {index} out of range (len {len})")
            }
            Self::RestoreTooLong { found, cap } => {
                write!(f, "restore sequence too long: {found}, cap {cap}")
            }
        }
    }
}

impl std::error::Error for TabError {}

// ---------------------------------------------------------------------------
// TabCell / TabStrip
// ---------------------------------------------------------------------------

/// One rendered tab: panel identity plus its 1-based physical slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabCell {
    /// The panel this tab names.
    pub panel: PanelId,
    /// 1-based physical slot (position in the strip).
    pub slot: u32,
}

/// The headless tab order for one [`TabScope`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabStrip {
    scope: TabScope,
    order: Vec<PanelId>,
}

impl TabStrip {
    /// Creates an empty strip for `scope`.
    #[must_use]
    pub fn new(scope: TabScope) -> Self {
        Self {
            scope,
            order: Vec::new(),
        }
    }

    /// The scope this strip projects.
    #[must_use]
    pub fn scope(&self) -> TabScope {
        self.scope
    }

    /// Returns the number of tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the strip holds no tabs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The tab order (panel identities, display sequence).
    #[must_use]
    pub fn order(&self) -> &[PanelId] {
        &self.order
    }

    /// Appends a tab for `panel`.
    pub fn open_tab(&mut self, panel: PanelId) -> Result<(), TabError> {
        if self.order.contains(&panel) {
            return Err(TabError::DuplicateTab { panel });
        }
        if self.order.len() >= MAX_TABS_PER_STRIP {
            return Err(TabError::TooManyTabs {
                found: self.order.len() + 1,
                cap: MAX_TABS_PER_STRIP,
            });
        }
        self.order.push(panel);
        Ok(())
    }

    /// Detaches the tab for `panel` (projection only; panel state
    /// untouched — the never-empty invariant lives in the guard).
    pub fn close_tab(&mut self, panel: PanelId) -> Result<(), TabError> {
        match self.order.iter().position(|p| *p == panel) {
            Some(index) => {
                self.order.remove(index);
                Ok(())
            }
            None => Err(TabError::UnknownTab { panel }),
        }
    }

    /// Reorders the tab at `from` to index `to` (drag within the strip).
    ///
    /// `to` may equal `len` (move to the end). Identities untouched; only
    /// the display sequence changes.
    pub fn reorder(&mut self, from: usize, to: usize) -> Result<(), TabError> {
        if from >= self.order.len() {
            return Err(TabError::IndexOutOfRange {
                index: from,
                len: self.order.len(),
            });
        }
        if to > self.order.len() {
            return Err(TabError::IndexOutOfRange {
                index: to,
                len: self.order.len(),
            });
        }
        let panel = self.order.remove(from);
        let at = if to > from { to - 1 } else { to };
        self.order.insert(at, panel);
        Ok(())
    }

    /// Moves the tab for `panel` into `other` (cross-strip move).
    ///
    /// Atomic across the two strips: on any failure both strips keep
    /// their prior orders.
    pub fn move_to(&mut self, panel: PanelId, other: &mut TabStrip) -> Result<(), TabError> {
        if !self.order.contains(&panel) {
            return Err(TabError::UnknownTab { panel });
        }
        if other.order.contains(&panel) {
            return Err(TabError::DuplicateTab { panel });
        }
        if other.order.len() >= MAX_TABS_PER_STRIP {
            return Err(TabError::TooManyTabs {
                found: other.order.len() + 1,
                cap: MAX_TABS_PER_STRIP,
            });
        }
        self.order.retain(|p| *p != panel);
        other.order.push(panel);
        Ok(())
    }

    /// Renders the strip as cells pairing each panel with its 1-based
    /// physical slot.
    #[must_use]
    pub fn cells(&self) -> Vec<TabCell> {
        self.order
            .iter()
            .enumerate()
            .map(|(index, panel)| TabCell {
                panel: *panel,
                slot: (index + 1) as u32,
            })
            .collect()
    }

    /// Snapshots the strip to its id sequence (persistence spelling).
    #[must_use]
    pub fn snapshot(&self) -> Vec<PanelId> {
        self.order.clone()
    }

    /// Restores a strip from a snapshotted sequence, replacing the order.
    ///
    /// Rejects overlong and duplicate sequences fail-closed, leaving the
    /// current order untouched.
    pub fn restore(&mut self, sequence: &[PanelId]) -> Result<(), TabError> {
        if sequence.len() > MAX_TABS_PER_STRIP {
            return Err(TabError::RestoreTooLong {
                found: sequence.len(),
                cap: MAX_TABS_PER_STRIP,
            });
        }
        let mut seen: Vec<PanelId> = Vec::with_capacity(sequence.len());
        for panel in sequence {
            if seen.contains(panel) {
                return Err(TabError::DuplicateTab { panel: *panel });
            }
            seen.push(*panel);
        }
        self.order = seen;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_moves_identity_keeps_set() {
        let mut strip = TabStrip::new(TabScope::PerWorkspace(0));
        for id in [1, 2, 3] {
            strip.open_tab(PanelId::new(id)).unwrap();
        }
        strip.reorder(0, 3).unwrap();
        assert_eq!(
            strip.order(),
            &[PanelId::new(2), PanelId::new(3), PanelId::new(1)]
        );
        let cells = strip.cells();
        assert_eq!(cells[2].slot, 3);
        assert_eq!(cells[2].panel, PanelId::new(1));
    }

    #[test]
    fn move_to_is_atomic_on_full_target() {
        let mut a = TabStrip::new(TabScope::PerWorkspace(0));
        let mut b = TabStrip::new(TabScope::PerWindow);
        a.open_tab(PanelId::new(1)).unwrap();
        for id in 0..MAX_TABS_PER_STRIP as u64 {
            b.open_tab(PanelId::new(100 + id)).unwrap();
        }
        assert!(a.move_to(PanelId::new(1), &mut b).is_err());
        assert_eq!(a.order(), &[PanelId::new(1)]);
    }

    #[test]
    fn restore_rejects_duplicates_untouched() {
        let mut strip = TabStrip::new(TabScope::PerWindow);
        strip.open_tab(PanelId::new(1)).unwrap();
        let err = strip
            .restore(&[PanelId::new(2), PanelId::new(2)])
            .unwrap_err();
        assert_eq!(
            err,
            TabError::DuplicateTab {
                panel: PanelId::new(2)
            }
        );
        assert_eq!(strip.order(), &[PanelId::new(1)]);
    }
}
