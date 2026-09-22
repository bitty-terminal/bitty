//! Stable panel identity and physical-slot display semantics (UX-06,
//! issue #1012).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC).
//! Nothing here is normative, accepted, or verified: every bound, spelling,
//! and rule below is a candidate that the owning RFC accepts or rejects,
//! never this module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`SlotNumber`] — a physical slot (`1..=MAX_IDENTITY_SLOTS`) selected
//!   by `Mod`+Number. Slots are positions, never identities: reseating a
//!   slot never changes the [`PanelId`](crate::panel::PanelId) behind it.
//! - [`IdentityRegistry`] — the stable `PanelId` <-> [`SlotNumber`]
//!   binding with a bounded display title per panel. `Mod`+Number calls
//!   [`IdentityRegistry::reseat`], which moves a panel into a slot (or
//!   swaps occupants); the panel identity and its terminal binding survive
//!   verbatim (a reseat is not a copy, state stays single-owned).
//! - [`OpaquePanelHandle`] — the handle shape commands and Lua receive:
//!   equality only, no raw id accessor, no `From` bridge to `PanelId` in
//!   either direction. Resolution happens through
//!   [`IdentityRegistry::resolve`] only, so a holder of a handle alone can
//!   neither forge nor inspect the underlying identity.
//! - [`tab_label`] — the display spelling `"<slot>:<title>"` joining the
//!   physical slot to the stable title.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::panel::PanelId;

/// Hard cap on physical slots addressable by `Mod`+Number.
///
/// Rejected with [`IdentityError::SlotOutOfRange`], never wrapped or
/// silently clamped: wrapping would display a panel under the wrong slot.
pub const MAX_IDENTITY_SLOTS: u8 = 9;

/// Hard cap on panels carrying identity bindings in one registry.
///
/// Rejected with [`IdentityError::TooManyPanels`], never silently pruned.
pub const MAX_IDENTITY_PANELS: usize = 256;

/// Hard cap in characters on a panel display title.
///
/// Rejected with [`IdentityError::TitleTooLong`], never silently cut:
/// truncation would mislabel the surface.
pub const MAX_IDENTITY_TITLE_LEN: usize = 64;

// ---------------------------------------------------------------------------
// SlotNumber: physical position, never identity
// ---------------------------------------------------------------------------

/// Physical slot selected by `Mod`+Number (`1..=MAX_IDENTITY_SLOTS`).
///
/// A slot is a position on the tab strip, never a panel identity. Two
/// reseats may move the same [`PanelId`](crate::panel::PanelId) through
/// many slots; the identity never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotNumber(u8);

impl SlotNumber {
    /// Binds a raw slot value; fails closed outside `1..=MAX_IDENTITY_SLOTS`.
    pub const fn new(raw: u8) -> Result<Self, IdentityError> {
        if raw == 0 || raw > MAX_IDENTITY_SLOTS {
            return Err(IdentityError::SlotOutOfRange { found: raw });
        }
        Ok(Self(raw))
    }

    /// Returns the raw slot value (`1..=MAX_IDENTITY_SLOTS`).
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for SlotNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "slot {}", self.0)
    }
}

// ---------------------------------------------------------------------------
// OpaquePanelHandle: equality-only handle for commands/Lua
// ---------------------------------------------------------------------------

/// Opaque handle naming a panel in commands and Lua.
///
/// Deliberately capability-free: holders can compare handles and hand them
/// back to [`IdentityRegistry::resolve`], but cannot read the raw id, forge
/// a handle from an integer, or convert to
/// [`PanelId`](crate::panel::PanelId). There is no `From` bridge in either
/// direction by construction. `Display` stays opaque as well.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpaquePanelHandle {
    panel: PanelId,
}

impl OpaquePanelHandle {
    /// Issues a handle for `panel`. Only the registry (and this module)
    /// can mint handles; callers cannot construct one from a raw integer.
    #[must_use]
    pub(crate) fn mint(panel: PanelId) -> Self {
        Self { panel }
    }

    /// Names the panel for diagnostics without exposing the raw id.
    #[must_use]
    pub fn panel_for_registry(self) -> PanelId {
        self.panel
    }
}

impl fmt::Display for OpaquePanelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PanelHandle(opaque)")
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to evolve stable panel identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// No binding exists for this panel.
    UnknownPanel {
        /// The unrecognized panel.
        panel: PanelId,
    },
    /// No panel occupies this slot.
    EmptySlot {
        /// The vacant slot.
        slot: SlotNumber,
    },
    /// The slot already names another panel; bind leaves it untouched.
    SlotOccupied {
        /// The occupied slot.
        slot: SlotNumber,
    },
    /// The raw slot value is outside `1..=MAX_IDENTITY_SLOTS`.
    SlotOutOfRange {
        /// The rejected value.
        found: u8,
    },
    /// The title exceeds [`MAX_IDENTITY_TITLE_LEN`] characters.
    TitleTooLong {
        /// Characters counted in the submitted title.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// More than [`MAX_IDENTITY_PANELS`] panels were submitted.
    TooManyPanels {
        /// Panels counted in the submitted batch.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPanel { panel } => write!(f, "unknown panel: {panel}"),
            Self::EmptySlot { slot } => write!(f, "slot is empty: {slot}"),
            Self::SlotOccupied { slot } => write!(f, "slot occupied: {slot}"),
            Self::SlotOutOfRange { found } => {
                write!(f, "slot out of range: {found}, cap {MAX_IDENTITY_SLOTS}")
            }
            Self::TitleTooLong { found, cap } => {
                write!(f, "title too long: {found} chars, cap {cap}")
            }
            Self::TooManyPanels { found, cap } => {
                write!(f, "too many panels: {found}, cap {cap}")
            }
        }
    }
}

impl std::error::Error for IdentityError {}

// ---------------------------------------------------------------------------
// IdentityRegistry
// ---------------------------------------------------------------------------

/// Stable `PanelId` <-> [`SlotNumber`] bindings with display titles.
///
/// Invariants: every bound panel has exactly one slot and one title; every
/// occupied slot names exactly one panel. [`reseat`](Self::reseat) (the
/// `Mod`+Number path) changes the slot side only — panel identities,
/// titles, and handles are never recreated by a reseat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityRegistry {
    slots: BTreeMap<PanelId, SlotNumber>,
    titles: BTreeMap<PanelId, String>,
}

impl IdentityRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of bound panels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no panel is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Binds `panel` to `slot` with `title`.
    ///
    /// Fails closed (registry untouched) on an out-of-range slot, an
    /// overlong title, a full registry, or an already-bound panel or slot.
    /// Use [`reseat`](Self::reseat) to move an existing binding.
    pub fn bind(
        &mut self,
        panel: PanelId,
        slot: SlotNumber,
        title: &str,
    ) -> Result<OpaquePanelHandle, IdentityError> {
        if self.slots.contains_key(&panel) {
            return Err(IdentityError::UnknownPanel { panel });
        }
        if self.slots.len() >= MAX_IDENTITY_PANELS {
            return Err(IdentityError::TooManyPanels {
                found: self.slots.len() + 1,
                cap: MAX_IDENTITY_PANELS,
            });
        }
        if self.slots.values().any(|s| *s == slot) {
            return Err(IdentityError::SlotOccupied { slot });
        }
        let title = check_title(title)?;
        self.slots.insert(panel, slot);
        self.titles.insert(panel, title);
        Ok(OpaquePanelHandle::mint(panel))
    }

    /// `Mod`+Number: moves `panel` into `slot`.
    ///
    /// If another panel occupies `slot`, the two panels swap slots so no
    /// panel is ever left slotless by a reseat. Identity, titles, and
    /// handles survive verbatim; only the slot side changes.
    pub fn reseat(&mut self, panel: PanelId, slot: SlotNumber) -> Result<(), IdentityError> {
        if !self.slots.contains_key(&panel) {
            return Err(IdentityError::UnknownPanel { panel });
        }
        let occupant = self
            .slots
            .iter()
            .find(|(_, s)| **s == slot)
            .map(|(p, _)| *p);
        if let Some(other) = occupant {
            if other != panel {
                let prev = self.slots[&panel];
                self.slots.insert(other, prev);
            }
        }
        self.slots.insert(panel, slot);
        Ok(())
    }

    /// Removes the binding for `panel`, freeing its slot.
    pub fn unbind(&mut self, panel: PanelId) -> Result<(), IdentityError> {
        if self.slots.remove(&panel).is_none() {
            return Err(IdentityError::UnknownPanel { panel });
        }
        self.titles.remove(&panel);
        Ok(())
    }

    /// Returns the slot bound to `panel`.
    pub fn slot_of(&self, panel: PanelId) -> Result<SlotNumber, IdentityError> {
        self.slots
            .get(&panel)
            .copied()
            .ok_or(IdentityError::UnknownPanel { panel })
    }

    /// Returns the panel occupying `slot`.
    pub fn panel_at(&self, slot: SlotNumber) -> Result<PanelId, IdentityError> {
        self.slots
            .iter()
            .find(|(_, s)| **s == slot)
            .map(|(p, _)| *p)
            .ok_or(IdentityError::EmptySlot { slot })
    }

    /// Renames the display title for `panel` (identity untouched).
    pub fn set_title(&mut self, panel: PanelId, title: &str) -> Result<(), IdentityError> {
        if !self.slots.contains_key(&panel) {
            return Err(IdentityError::UnknownPanel { panel });
        }
        let title = check_title(title)?;
        self.titles.insert(panel, title);
        Ok(())
    }

    /// Returns the display title for `panel`.
    pub fn title_of(&self, panel: PanelId) -> Result<&str, IdentityError> {
        self.titles
            .get(&panel)
            .map(String::as_str)
            .ok_or(IdentityError::UnknownPanel { panel })
    }

    /// Resolves an opaque handle back to its panel identity.
    ///
    /// Returns `None` (fails closed) when the panel has since been
    /// unbound; a stale handle never resolves to a recycled identity
    /// because ids are never recycled by this registry.
    #[must_use]
    pub fn resolve(&self, handle: OpaquePanelHandle) -> Option<PanelId> {
        let panel = handle.panel_for_registry();
        self.slots.contains_key(&panel).then_some(panel)
    }

    /// Issues the opaque handle for a bound panel.
    pub fn handle_of(&self, panel: PanelId) -> Result<OpaquePanelHandle, IdentityError> {
        if self.slots.contains_key(&panel) {
            Ok(OpaquePanelHandle::mint(panel))
        } else {
            Err(IdentityError::UnknownPanel { panel })
        }
    }
}

fn check_title(title: &str) -> Result<String, IdentityError> {
    let found = title.chars().count();
    if found > MAX_IDENTITY_TITLE_LEN {
        return Err(IdentityError::TitleTooLong {
            found,
            cap: MAX_IDENTITY_TITLE_LEN,
        });
    }
    Ok(title.to_owned())
}

/// Display spelling for one tab: `"<slot>:<title>"`.
///
/// Joins the physical slot (left, changes on reseat) to the stable title
/// (right, survives reseats), so a `Mod`+Number move is visible as a slot
/// change, never an identity change.
#[must_use]
pub fn tab_label(slot: SlotNumber, title: &str) -> String {
    format!("{}:{title}", slot.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_rejects_zero_and_overflow() {
        assert!(SlotNumber::new(0).is_err());
        assert!(SlotNumber::new(MAX_IDENTITY_SLOTS + 1).is_err());
        assert_eq!(SlotNumber::new(1).unwrap().get(), 1);
    }

    #[test]
    fn reseat_changes_slot_never_identity() {
        let mut reg = IdentityRegistry::new();
        let a = PanelId::new(7);
        let b = PanelId::new(9);
        reg.bind(a, SlotNumber::new(1).unwrap(), "editor").unwrap();
        reg.bind(b, SlotNumber::new(2).unwrap(), "logs").unwrap();
        reg.reseat(a, SlotNumber::new(2).unwrap()).unwrap();
        assert_eq!(reg.slot_of(a).unwrap().get(), 2);
        assert_eq!(reg.slot_of(b).unwrap().get(), 1);
        assert_eq!(reg.title_of(a).unwrap(), "editor");
        assert_eq!(reg.panel_at(SlotNumber::new(2).unwrap()).unwrap(), a);
    }

    #[test]
    fn stale_handle_fails_closed() {
        let mut reg = IdentityRegistry::new();
        let a = PanelId::new(3);
        let handle = reg.bind(a, SlotNumber::new(1).unwrap(), "term").unwrap();
        assert_eq!(reg.resolve(handle), Some(a));
        reg.unbind(a).unwrap();
        assert_eq!(reg.resolve(handle), None);
    }
}
