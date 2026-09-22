//! Five-level UI architecture contract (UX-16, CTX-0668).
//!
//! Candidate implementation of U-3
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every level name, assignment, and rule below is a
//! candidate spelling that the UI Runtime RFC accepts or rejects, never this
//! module. The module is English-only.
//!
//! The five levels, from mechanism to app:
//!
//! - **L0 Mechanism** (`UiLevel::Mechanism`) — Rust owns behavior:
//!   virtualization, IME composition, scroll physics, hit-testing, layout,
//!   shaping, and paint. Lua never implements these; it only supplies
//!   appearance. [`UiNodeKind`](crate::uitree::UiNodeKind) complex widgets
//!   (`ScrollView`, `VirtualList`, `TextInput`, `Canvas`) are L0 residents:
//!   Lua emits the node, Rust runs it.
//! - **L1 Primitive** (`UiLevel::Primitive`) — Lua emits retained nodes from
//!   the Level-1 set; Rust validates and reconciles by stable
//!   [`UiNodeId`](crate::uitree::UiNodeId). No per-frame Lua draw loop.
//! - **L2 Core** (`UiLevel::Core`) — the shared `bitty-ui-core` vocabulary
//!   (planned). No Rust items in this crate carry this level yet.
//! - **L3 Domain** (`UiLevel::Domain`) — domain widgets composed from L2
//!   (planned). No Rust items in this crate carry this level yet.
//! - **L4 App** (`UiLevel::App`) — app-specific composition (planned). No
//!   Rust items in this crate carry this level yet.
//!
//! Dependency rule: a higher level may use a lower one (and its own);
//! a lower level must never depend on a higher one, so mechanisms stay
//! headless and Lua/app policy never leaks into Rust-owned behavior.
//! [`may_depend_on`] decides, [`check_flow`] fails closed.
//!
//! Governance and versioning are open (issue #1022 scope): the candidate
//! contract version below is a placeholder the RFC replaces, not a stable
//! promise.
//!
//! Plugin-migration follow-up: beacon (U-8) is plugin-future. If L0
//! mechanisms later move behind a plugin boundary, this level contract and
//! the [`crate::widget_mech`] headless mechanism state migrate as the
//! boundary spec. This module takes no render, exec, or plugin dependency
//! so the migration stays mechanical.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, or platform handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::uitree::UiNodeKind;

// ---------------------------------------------------------------------------
// Contract version (candidate placeholder; governance open)
// ---------------------------------------------------------------------------

/// Candidate version of this level contract.
///
/// `0` marks unaccepted scaffolding: governance and versioning are open
/// (issue #1022) and the UI Runtime RFC replaces this with a real scheme.
pub const U3_LEVELS_CONTRACT_VERSION: u32 = 0;

// ---------------------------------------------------------------------------
// Levels
// ---------------------------------------------------------------------------

/// One of the five U-3 architecture levels, ordered mechanism-first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UiLevel {
    /// L0: Rust-owned behavior (virtualization, IME, scroll physics,
    /// hit-testing, layout, paint).
    Mechanism,
    /// L1: Lua-emitted retained primitives, reconciled by Rust.
    Primitive,
    /// L2: shared `bitty-ui-core` vocabulary (planned, unpopulated).
    Core,
    /// L3: domain widgets composed from L2 (planned, unpopulated).
    Domain,
    /// L4: app-specific composition (planned, unpopulated).
    App,
}

impl UiLevel {
    /// Rank from mechanism (`0`) to app (`4`); ordering drives the flow rule.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Mechanism => 0,
            Self::Primitive => 1,
            Self::Core => 2,
            Self::Domain => 3,
            Self::App => 4,
        }
    }

    /// Candidate vocabulary spelling for this level.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mechanism => "l0-mechanism",
            Self::Primitive => "l1-primitive",
            Self::Core => "l2-core",
            Self::Domain => "l3-domain",
            Self::App => "l4-app",
        }
    }

    /// One-line owner statement for this level.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Mechanism => "rust owns behavior; lua supplies appearance only",
            Self::Primitive => "lua emits retained nodes; rust validates and reconciles",
            Self::Core => "shared bitty-ui-core vocabulary (planned)",
            Self::Domain => "domain widgets composed from core (planned)",
            Self::App => "app-specific composition (planned)",
        }
    }

    /// Whether any [`UiNodeKind`](crate::uitree::UiNodeKind) currently
    /// classifies at this level. L2-L4 are unpopulated by construction
    /// until the RFC accepts their vocabularies.
    #[must_use]
    pub const fn is_populated(self) -> bool {
        match self {
            Self::Mechanism | Self::Primitive => true,
            Self::Core | Self::Domain | Self::App => false,
        }
    }
}

impl fmt::Display for UiLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classifies a Level-1 primitive into its architecture level.
///
/// Complex widgets whose behavior is Rust-owned (`ScrollView`,
/// `VirtualList`, `TextInput`, `Canvas`) are L0 mechanisms: Lua declares
/// the node and its appearance, Rust runs virtualization, IME composition,
/// scroll physics, and paint budgeting (see [`crate::widget_mech`]).
/// Every other primitive is a plain L1 declaration.
///
/// Total: every [`UiNodeKind`](crate::uitree::UiNodeKind) maps.
#[must_use]
pub fn level_of(kind: &UiNodeKind) -> UiLevel {
    match kind {
        UiNodeKind::ScrollView
        | UiNodeKind::VirtualList
        | UiNodeKind::TextInput { .. }
        | UiNodeKind::Canvas => UiLevel::Mechanism,
        UiNodeKind::Box
        | UiNodeKind::Text(_)
        | UiNodeKind::Image
        | UiNodeKind::Terminal
        | UiNodeKind::Overlay => UiLevel::Primitive,
    }
}

// ---------------------------------------------------------------------------
// Dependency flow
// ---------------------------------------------------------------------------

/// Rejected level dependency: a lower level was asked to depend on a
/// higher one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LevelFlowError {
    /// The level that wants to depend.
    pub dependent: UiLevel,
    /// The level it wants to depend on.
    pub dependency: UiLevel,
}

impl fmt::Display for LevelFlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "level flow rejected: {} must not depend on {}",
            self.dependent, self.dependency
        )
    }
}

impl std::error::Error for LevelFlowError {}

/// Whether `dependent` may use `dependency`: same level or downward only.
///
/// Higher levels compose lower ones; lower levels never reach upward, so
/// Rust mechanisms stay callable without Lua, core, domain, or app state.
#[must_use]
pub const fn may_depend_on(dependent: UiLevel, dependency: UiLevel) -> bool {
    dependent.rank() >= dependency.rank()
}

/// Fails closed when `dependent` must not use `dependency`.
///
/// # Errors
///
/// Returns [`LevelFlowError`] for an upward dependency.
pub const fn check_flow(dependent: UiLevel, dependency: UiLevel) -> Result<(), LevelFlowError> {
    if may_depend_on(dependent, dependency) {
        Ok(())
    } else {
        Err(LevelFlowError {
            dependent,
            dependency,
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> Vec<UiNodeKind> {
        vec![
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
        ]
    }

    #[test]
    fn classification_is_total_over_primitives() {
        for kind in all_kinds() {
            let _ = level_of(&kind);
        }
    }

    #[test]
    fn complex_widgets_are_mechanisms() {
        assert_eq!(level_of(&UiNodeKind::VirtualList), UiLevel::Mechanism);
        assert_eq!(level_of(&UiNodeKind::ScrollView), UiLevel::Mechanism);
        assert_eq!(level_of(&UiNodeKind::Canvas), UiLevel::Mechanism);
        assert_eq!(
            level_of(&UiNodeKind::TextInput {
                value: String::new(),
                placeholder: String::new(),
            }),
            UiLevel::Mechanism
        );
    }

    #[test]
    fn plain_primitives_stay_l1() {
        assert_eq!(level_of(&UiNodeKind::Box), UiLevel::Primitive);
        assert_eq!(
            level_of(&UiNodeKind::Text("t".to_string())),
            UiLevel::Primitive
        );
        assert_eq!(level_of(&UiNodeKind::Terminal), UiLevel::Primitive);
    }

    #[test]
    fn flow_allows_downward_and_same_level() {
        assert!(may_depend_on(UiLevel::App, UiLevel::Mechanism));
        assert!(may_depend_on(UiLevel::Domain, UiLevel::Core));
        assert!(may_depend_on(UiLevel::Primitive, UiLevel::Primitive));
        assert!(check_flow(UiLevel::App, UiLevel::Mechanism).is_ok());
    }

    #[test]
    fn flow_rejects_upward_dependency() {
        assert!(!may_depend_on(UiLevel::Mechanism, UiLevel::App));
        let err = check_flow(UiLevel::Mechanism, UiLevel::Primitive)
            .expect_err("mechanism must not depend on primitives");
        assert_eq!(
            err,
            LevelFlowError {
                dependent: UiLevel::Mechanism,
                dependency: UiLevel::Primitive,
            }
        );
        assert_eq!(
            err.to_string(),
            "level flow rejected: l0-mechanism must not depend on l1-primitive"
        );
    }

    #[test]
    fn future_levels_are_unpopulated() {
        assert!(UiLevel::Mechanism.is_populated());
        assert!(UiLevel::Primitive.is_populated());
        assert!(!UiLevel::Core.is_populated());
        assert!(!UiLevel::Domain.is_populated());
        assert!(!UiLevel::App.is_populated());
    }
}
