//! Package lifecycle — 6-state lifecycle per RFC.
//!
//! ```text
//! discovered -> fetched -> verified -> staged -> activated -> retained
//!                                        |          |
//!                             (approval gate)  (activation txn)
//!                                                   |
//!                                            restored (rollback)
//! ```
//!
//! Each transition is a named gate; failing a gate fails closed and leaves
//! the previous state unchanged. Installation (`add`, `update`, `sync`) spans
//! discovery through staging and executes zero package code (Invariant 8).

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::manifest::PackageId;

// ── state enum ───────────────────────────────────────────────────────────

/// Six-state package lifecycle.
///
/// `Retained` keeps superseded generations for bounded rollback; `Restored`
/// is the conceptual state after a rollback transaction (modeled as re-entering
/// `Activated` from `Retained`, since the store content is identical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackageState {
    /// Source declares an ID/version; nothing is trusted yet.
    Discovered,
    /// Bytes land in quarantine; no execution, no plugin VM contact.
    Fetched,
    /// Every integrity check passes against the lock record and capabilities
    /// pass the approval gate.
    Verified,
    /// Verified content sits in the package store as a complete generation
    /// not yet active.
    Staged,
    /// Atomically switched to be the active environment the host loads from.
    /// Earliest step at which any package code could execute, and only after
    /// all earlier gates.
    Activated,
    /// Superseded generation retained for rollback up to a bounded count.
    Retained,
}

impl PackageState {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Fetched => "fetched",
            Self::Verified => "verified",
            Self::Staged => "staged",
            Self::Activated => "activated",
            Self::Retained => "retained",
        }
    }

    /// Whether this state is considered active (host loads plugins from it).
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Activated)
    }

    /// Whether package code could ever execute in this state.
    ///
    /// Only `Activated` (and restored via re-activation) allows execution;
    /// all earlier states are still quarantine. This enforces Invariant 8.
    #[must_use]
    pub fn may_execute_code(self) -> bool {
        matches!(self, Self::Activated)
    }
}

impl std::fmt::Display for PackageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── transition table ─────────────────────────────────────────────────────

/// Whether `from -> to` is a legal direct transition.
///
/// The RFC's lifecycle is linear through the happy path; rollback creates a
/// `Retained -> Activated` edge via the generation manager (not modeled here
/// as a bare state transition, but as `Generation` selection).
#[must_use]
pub fn can_transition(from: PackageState, to: PackageState) -> bool {
    matches!(
        (from, to),
        (PackageState::Discovered, PackageState::Fetched)
            | (PackageState::Fetched, PackageState::Verified)
            | (PackageState::Verified, PackageState::Staged)
            | (PackageState::Staged, PackageState::Activated)
            | (PackageState::Activated, PackageState::Retained)
            // Rollback edge: retained generation re-activated (restored).
            | (PackageState::Retained, PackageState::Activated)
            // Retained may also be pruned (terminal) or re-staged on update.
            | (PackageState::Retained, PackageState::Staged)
    )
}

/// Human-readable list of legal successors for a state.
#[must_use]
pub fn successors(state: PackageState) -> Vec<PackageState> {
    let all = [
        PackageState::Discovered,
        PackageState::Fetched,
        PackageState::Verified,
        PackageState::Staged,
        PackageState::Activated,
        PackageState::Retained,
    ];
    all.iter()
        .copied()
        .filter(|to| can_transition(state, *to))
        .collect()
}

// ── per-package lifecycle record ─────────────────────────────────────────

/// Owned lifecycle record for one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLifecycle {
    /// Package id.
    pub id: PackageId,
    /// Current state.
    pub state: PackageState,
    /// Generation id that owns this lifecycle entry, if any.
    pub generation: Option<u64>,
    /// Last error message, if the last transition failed.
    pub last_error: Option<String>,
}

impl PackageLifecycle {
    /// Create a new record at `Discovered`.
    #[must_use]
    pub fn discovered(id: PackageId) -> Self {
        Self {
            id,
            state: PackageState::Discovered,
            generation: None,
            last_error: None,
        }
    }

    /// Attempt to transition to `target`; fails closed (state unchanged on error).
    pub fn transition(&mut self, target: PackageState) -> Result<(), PackageError> {
        if self.state == target {
            return Ok(());
        }
        if !can_transition(self.state, target) {
            return Err(PackageError::InvalidState {
                id: self.id.to_string(),
                current: self.state.label().to_string(),
                expected: format!("successor of {}", self.state.label()),
            });
        }
        self.state = target;
        self.last_error = None;
        Ok(())
    }

    /// Record a failure without changing state (gate fails closed).
    pub fn record_failure(&mut self, msg: impl Into<String>) {
        self.last_error = Some(msg.into());
    }
}

// ── lifecycle registry (in-memory) ───────────────────────────────────────

/// In-memory registry of lifecycles keyed by package id.
#[derive(Debug, Default, Clone)]
pub struct LifecycleRegistry {
    entries: BTreeMap<String, PackageLifecycle>,
}

impl LifecycleRegistry {
    /// Create empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a new package (creates `Discovered` entry).
    pub fn declare(&mut self, id: PackageId) -> Result<(), PackageError> {
        let key = id.as_str().to_string();
        if self.entries.contains_key(&key) {
            return Err(PackageError::Duplicate {
                kind: "package".to_string(),
                value: key,
            });
        }
        self.entries.insert(key, PackageLifecycle::discovered(id));
        Ok(())
    }

    /// Get immutable record.
    #[must_use]
    pub fn get(&self, id: &PackageId) -> Option<&PackageLifecycle> {
        self.entries.get(id.as_str())
    }

    /// Get mutable record.
    pub fn get_mut(&mut self, id: &PackageId) -> Option<&mut PackageLifecycle> {
        self.entries.get_mut(id.as_str())
    }

    /// Transition a package; fails closed (no state change on error, error recorded).
    pub fn transition(&mut self, id: &PackageId, target: PackageState) -> Result<(), PackageError> {
        let entry = self
            .entries
            .get_mut(id.as_str())
            .ok_or_else(|| PackageError::NotFound { id: id.to_string() })?;
        let from = entry.state;
        if let Err(e) = entry.transition(target) {
            entry.record_failure(e.to_string());
            // Ensure state unchanged (transition already left it unchanged on can_transition failure).
            debug_assert_eq!(entry.state, from);
            return Err(e);
        }
        Ok(())
    }

    /// Number of tracked packages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether any package is still in a state that may execute code.
    #[must_use]
    pub fn any_active(&self) -> bool {
        self.entries.values().any(|e| e.state.is_active())
    }

    /// Validate that no package outside `Activated` is considered executable.
    #[must_use]
    pub fn invariant_no_early_execution(&self) -> bool {
        self.entries
            .values()
            .all(|e| !e.state.may_execute_code() || e.state == PackageState::Activated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PackageId;

    fn pid(s: &str) -> PackageId {
        PackageId::new(s).unwrap()
    }

    #[test]
    fn happy_path() {
        let mut reg = LifecycleRegistry::new();
        reg.declare(pid("xuepoo.a")).unwrap();
        reg.transition(&pid("xuepoo.a"), PackageState::Fetched)
            .unwrap();
        reg.transition(&pid("xuepoo.a"), PackageState::Verified)
            .unwrap();
        reg.transition(&pid("xuepoo.a"), PackageState::Staged)
            .unwrap();
        reg.transition(&pid("xuepoo.a"), PackageState::Activated)
            .unwrap();
        assert_eq!(
            reg.get(&pid("xuepoo.a")).unwrap().state,
            PackageState::Activated
        );
    }

    #[test]
    fn skipped_stage_rejected_fail_closed() {
        let mut reg = LifecycleRegistry::new();
        reg.declare(pid("xuepoo.a")).unwrap();
        // Cannot jump Discovered -> Verified.
        let err = reg
            .transition(&pid("xuepoo.a"), PackageState::Verified)
            .unwrap_err();
        assert_eq!(
            reg.get(&pid("xuepoo.a")).unwrap().state,
            PackageState::Discovered
        );
        assert!(reg.get(&pid("xuepoo.a")).unwrap().last_error.is_some());
        assert!(format!("{err}").contains("invalid state"));
    }

    #[test]
    fn retained_rollback_edge() {
        let mut reg = LifecycleRegistry::new();
        reg.declare(pid("xuepoo.a")).unwrap();
        for s in [
            PackageState::Fetched,
            PackageState::Verified,
            PackageState::Staged,
            PackageState::Activated,
            PackageState::Retained,
        ] {
            reg.transition(&pid("xuepoo.a"), s).unwrap();
        }
        // Rollback re-activates.
        reg.transition(&pid("xuepoo.a"), PackageState::Activated)
            .unwrap();
        assert_eq!(
            reg.get(&pid("xuepoo.a")).unwrap().state,
            PackageState::Activated
        );
    }

    #[test]
    fn no_execution_before_activated() {
        assert!(!PackageState::Staged.may_execute_code());
        assert!(!PackageState::Verified.may_execute_code());
        assert!(PackageState::Activated.may_execute_code());
    }

    #[test]
    fn duplicate_declare_rejected() {
        let mut reg = LifecycleRegistry::new();
        reg.declare(pid("xuepoo.a")).unwrap();
        assert!(reg.declare(pid("xuepoo.a")).is_err());
    }

    #[test]
    fn invariant_holds() {
        let mut reg = LifecycleRegistry::new();
        reg.declare(pid("xuepoo.a")).unwrap();
        reg.transition(&pid("xuepoo.a"), PackageState::Fetched)
            .unwrap();
        assert!(reg.invariant_no_early_execution());
    }

    #[test]
    fn can_transition_table() {
        assert!(can_transition(
            PackageState::Discovered,
            PackageState::Fetched
        ));
        assert!(!can_transition(
            PackageState::Discovered,
            PackageState::Staged
        ));
        assert!(can_transition(
            PackageState::Retained,
            PackageState::Activated
        ));
    }
}
