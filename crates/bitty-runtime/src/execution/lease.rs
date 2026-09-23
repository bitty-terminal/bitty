//! Panel lease/description/handoff candidate signal (RUN-21, #1052).
//!
//! Candidate evidence for
//! [OQ-083](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/open-questions.md):
//! a panel as a workstation with a stable id, a human-readable
//! title/description, a lease state (`Idle` / `Occupied`), and
//! acquire/release/handoff events. The lease vocabulary stays a UX metaphor,
//! never the core agent ontology, and presentation stays non-authoritative:
//! the lease records who may drive a panel, never what is true.
//!
//! The host owns identity, routing, and the event bus; this module owns only
//! the pure transition kernel: [`PanelLease`] moves between [`LeaseState`]
//! variants and answers every move with a [`LeaseEvent`] or a
//! [`LeaseError`]. There is no clock (a bounded lease term stays OQ-083 open
//! work), no bus, and no agent symbol beyond the opaque [`LeaseHolder`] tag
//! the host assigns.
//!
//! Fail-closed defaults: a fresh lease is `Idle`; acquiring an occupied panel
//! fails; releasing or handing off requires the current holder; an event is
//! produced only for a transition that actually happened.

use std::fmt;

/// Longest panel title in characters.
///
/// Titles render in workspace chrome; the bound keeps one panel from
/// crowding out its neighbors.
pub const MAX_PANEL_TITLE_CHARS: usize = 128;

/// Longest panel description in characters.
///
/// Descriptions are human and agent orientation text, not a data channel.
pub const MAX_PANEL_DESCRIPTION_CHARS: usize = 1024;

/// Opaque holder tag for the current lease occupant.
///
/// The host assigns tags (one agent holds many panels; the mapping lives
/// outside this module). A human takeover is a release back to [`Idle`],
/// never a holder value: the human path acts outside the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseHolder(pub u64);

impl LeaseHolder {
    /// Stable text form for audit records.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LeaseHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "holder-{}", self.0)
    }
}

/// Lease state of one panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LeaseState {
    /// No occupant; acquirable by any holder.
    #[default]
    Idle,
    /// Driven by exactly one holder; other holders fail closed.
    Occupied {
        /// Current occupant.
        holder: LeaseHolder,
    },
}

impl LeaseState {
    /// Stable lowercase name for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Occupied { .. } => "occupied",
        }
    }

    /// Current occupant, if any.
    #[must_use]
    pub const fn holder(self) -> Option<LeaseHolder> {
        match self {
            Self::Idle => None,
            Self::Occupied { holder } => Some(holder),
        }
    }
}

impl fmt::Display for LeaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Observable outcome of a lease transition.
///
/// Events describe transitions that already happened; producing one never
/// authorizes the next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseEvent {
    /// An idle panel was acquired.
    Acquired {
        /// New occupant.
        holder: LeaseHolder,
    },
    /// An occupied panel was released back to idle.
    Released {
        /// Previous occupant.
        holder: LeaseHolder,
    },
    /// Occupancy moved directly from one holder to another.
    Handoff {
        /// Previous occupant.
        from: LeaseHolder,
        /// New occupant.
        to: LeaseHolder,
    },
}

impl LeaseEvent {
    /// Stable lowercase name for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acquired { .. } => "acquired",
            Self::Released { .. } => "released",
            Self::Handoff { .. } => "handoff",
        }
    }
}

/// A refused lease transition; the lease is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseError {
    /// Acquire or handoff-target hit an occupied panel.
    AlreadyOccupied {
        /// Current occupant, which keeps the lease.
        holder: LeaseHolder,
    },
    /// Release or handoff-source hit an idle panel.
    NotOccupied,
    /// The caller is not the current occupant.
    NotHolder {
        /// Current occupant, which keeps the lease.
        holder: LeaseHolder,
    },
}

impl LeaseError {
    /// Stable lowercase name for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyOccupied { .. } => "already_occupied",
            Self::NotOccupied => "not_occupied",
            Self::NotHolder { .. } => "not_holder",
        }
    }
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOccupied { holder } => {
                write!(f, "panel occupied by {holder}")
            }
            Self::NotOccupied => f.write_str("panel is idle"),
            Self::NotHolder { holder } => {
                write!(f, "lease held by {holder}")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

/// Pure lease transition kernel for one panel.
///
/// Created [`Idle`](LeaseState::Idle); every method is deterministic and
/// infallible except for the documented refusal, which leaves the lease
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PanelLease {
    state: LeaseState,
}

impl PanelLease {
    /// A fresh idle lease.
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            state: LeaseState::Idle,
        }
    }

    /// Current lease state.
    #[must_use]
    pub const fn state(self) -> LeaseState {
        self.state
    }

    /// Acquires an idle panel for `holder`.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::AlreadyOccupied`] when the panel is occupied;
    /// the lease is unchanged.
    pub fn acquire(&mut self, holder: LeaseHolder) -> Result<LeaseEvent, LeaseError> {
        match self.state {
            LeaseState::Idle => {
                self.state = LeaseState::Occupied { holder };
                Ok(LeaseEvent::Acquired { holder })
            }
            LeaseState::Occupied { holder: current } => {
                Err(LeaseError::AlreadyOccupied { holder: current })
            }
        }
    }

    /// Releases an occupied panel back to idle; only the occupant may release.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotOccupied`] when idle, or
    /// [`LeaseError::NotHolder`] when the caller is not the occupant; the
    /// lease is unchanged.
    pub fn release(&mut self, holder: LeaseHolder) -> Result<LeaseEvent, LeaseError> {
        match self.state {
            LeaseState::Idle => Err(LeaseError::NotOccupied),
            LeaseState::Occupied { holder: current } => {
                if current == holder {
                    self.state = LeaseState::Idle;
                    Ok(LeaseEvent::Released { holder })
                } else {
                    Err(LeaseError::NotHolder { holder: current })
                }
            }
        }
    }

    /// Moves occupancy directly from `from` to `to` without an idle gap.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotOccupied`] when idle, or
    /// [`LeaseError::NotHolder`] when `from` is not the occupant; the lease
    /// is unchanged.
    pub fn handoff(
        &mut self,
        from: LeaseHolder,
        to: LeaseHolder,
    ) -> Result<LeaseEvent, LeaseError> {
        match self.state {
            LeaseState::Idle => Err(LeaseError::NotOccupied),
            LeaseState::Occupied { holder: current } => {
                if current == from {
                    self.state = LeaseState::Occupied { holder: to };
                    Ok(LeaseEvent::Handoff { from, to })
                } else {
                    Err(LeaseError::NotHolder { holder: current })
                }
            }
        }
    }
}

/// Validates a panel title: at most [`MAX_PANEL_TITLE_CHARS`] characters,
/// non-empty, and free of control characters (titles render in chrome).
#[must_use]
pub fn validate_title(title: &str) -> bool {
    let count = title.chars().count();
    count > 0 && count <= MAX_PANEL_TITLE_CHARS && !title.chars().any(|c| c.is_control())
}

/// Validates a panel description: at most [`MAX_PANEL_DESCRIPTION_CHARS`]
/// characters and free of control characters other than newline.
#[must_use]
pub fn validate_description(description: &str) -> bool {
    let count = description.chars().count();
    count <= MAX_PANEL_DESCRIPTION_CHARS
        && !description.chars().any(|c| c.is_control() && c != '\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: LeaseHolder = LeaseHolder(7);
    const B: LeaseHolder = LeaseHolder(9);

    #[test]
    fn fresh_lease_is_idle() {
        assert_eq!(PanelLease::idle().state(), LeaseState::Idle);
        assert_eq!(PanelLease::default().state(), LeaseState::Idle);
    }

    #[test]
    fn acquire_release_round_trip() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.acquire(A), Ok(LeaseEvent::Acquired { holder: A }));
        assert_eq!(lease.state(), LeaseState::Occupied { holder: A });
        assert_eq!(lease.release(A), Ok(LeaseEvent::Released { holder: A }));
        assert_eq!(lease.state(), LeaseState::Idle);
    }

    #[test]
    fn double_acquire_fails_closed_keeps_holder() {
        let mut lease = PanelLease::idle();
        assert!(lease.acquire(A).is_ok());
        assert_eq!(
            lease.acquire(B),
            Err(LeaseError::AlreadyOccupied { holder: A })
        );
        assert_eq!(lease.state(), LeaseState::Occupied { holder: A });
    }

    #[test]
    fn release_by_non_holder_fails_closed() {
        let mut lease = PanelLease::idle();
        assert!(lease.acquire(A).is_ok());
        assert_eq!(lease.release(B), Err(LeaseError::NotHolder { holder: A }));
        assert_eq!(lease.state(), LeaseState::Occupied { holder: A });
    }

    #[test]
    fn release_when_idle_fails() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.release(A), Err(LeaseError::NotOccupied));
    }

    #[test]
    fn handoff_moves_occupancy_without_idle_gap() {
        let mut lease = PanelLease::idle();
        assert!(lease.acquire(A).is_ok());
        assert_eq!(
            lease.handoff(A, B),
            Ok(LeaseEvent::Handoff { from: A, to: B })
        );
        assert_eq!(lease.state(), LeaseState::Occupied { holder: B });
    }

    #[test]
    fn handoff_by_non_holder_fails_closed() {
        let mut lease = PanelLease::idle();
        assert!(lease.acquire(A).is_ok());
        assert_eq!(
            lease.handoff(B, A),
            Err(LeaseError::NotHolder { holder: A })
        );
        assert_eq!(lease.state(), LeaseState::Occupied { holder: A });
    }

    #[test]
    fn handoff_when_idle_fails() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.handoff(A, B), Err(LeaseError::NotOccupied));
    }

    #[test]
    fn title_bounds_hold() {
        assert!(validate_title("agent workstation"));
        assert!(!validate_title(""));
        assert!(!validate_title(&"t".repeat(MAX_PANEL_TITLE_CHARS + 1)));
        assert!(validate_title(&"t".repeat(MAX_PANEL_TITLE_CHARS)));
        assert!(!validate_title("bad\x07title"));
    }

    #[test]
    fn description_bounds_hold() {
        assert!(validate_description("Tracks the checkout migration."));
        assert!(validate_description("line one\nline two"));
        assert!(!validate_description(
            &"d".repeat(MAX_PANEL_DESCRIPTION_CHARS + 1)
        ));
        assert!(!validate_description("bad\x00desc"));
    }

    #[test]
    fn event_and_error_names_are_stable() {
        assert_eq!(LeaseEvent::Acquired { holder: A }.as_str(), "acquired");
        assert_eq!(LeaseEvent::Released { holder: A }.as_str(), "released");
        assert_eq!(LeaseEvent::Handoff { from: A, to: B }.as_str(), "handoff");
        assert_eq!(
            LeaseError::AlreadyOccupied { holder: A }.as_str(),
            "already_occupied"
        );
        assert_eq!(LeaseError::NotOccupied.as_str(), "not_occupied");
        assert_eq!(LeaseError::NotHolder { holder: A }.as_str(), "not_holder");
    }
}
