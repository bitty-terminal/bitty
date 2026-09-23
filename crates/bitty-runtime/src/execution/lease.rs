//! Panel lease/description/handoff signal (RUN-21, #1052; SEC-26 cross-ref #1095).
//!
//! Adopted direction for
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
//! [`LeaseError`]. Panel write surfaces gate on the lease through
//! [`PanelLease::may_write`] / [`PanelLease::check_write`]: only the current
//! occupant may write within its tenure (SEC-26 write hook). Tenure is
//! bounded: every acquisition names a term in host ticks capped at
//! [`MAX_LEASE_TERM_TICKS`], and an expired occupant writes nothing until
//! it re-acquires. Time is a host-supplied monotonic tick (`u64`, never
//! wall-clock — the same tick pattern as the routable ledger); the kernel
//! compares ticks but never reads a clock. Lease transitions route to the
//! event bus through the host ([`PanelRuntime`](crate::registry::PanelRuntime)
//! publishes `bitty.panel:lifecycle.lease-changed` per transition), and
//! carry no agent symbol beyond the opaque [`LeaseHolder`] tag the host
//! assigns.
//!
//! Fail-closed defaults: a fresh lease is `Idle`; acquiring an occupied panel
//! fails; acquiring with an empty or over-bound term fails; releasing or
//! handing off requires the current holder; handoff and writes require a
//! live (unexpired) tenure; writing requires occupancy by the writer; an
//! event is produced only for a transition that actually happened.

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

/// Longest single lease tenure in host ticks (OQ-083 adopted bound).
///
/// Every acquisition names a term in host-supplied monotonic ticks; terms
/// past this bound fail closed with [`LeaseError::InvalidTerm`]. The unit
/// is deliberately host-defined (the routable ledger's `now_ticks` is one
/// such clock): the kernel compares ticks but never reads a clock.
pub const MAX_LEASE_TERM_TICKS: u64 = 1 << 20;

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
    /// Driven by exactly one holder until `expires_at` (host ticks).
    Occupied {
        /// Current occupant.
        holder: LeaseHolder,
        /// Host tick at which the tenure ends; the occupant writes nothing
        /// at or past this tick until it re-acquires.
        expires_at: u64,
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
            Self::Occupied { holder, .. } => Some(holder),
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
        /// Host tick at which the tenure ends.
        expires_at: u64,
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
        /// Preserved tenure deadline (handoff never extends tenure).
        expires_at: u64,
    },
    /// An expired tenure was swept back to idle.
    Expired {
        /// Occupant whose tenure lapsed.
        holder: LeaseHolder,
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
            Self::Expired { .. } => "expired",
        }
    }

    /// Current occupant named by the event, if any.
    ///
    /// `Acquired` names the new occupant, `Handoff` the new occupant, and
    /// `Expired` the lapsed one; `Released` returns `None` because the
    /// panel is idle afterwards.
    #[must_use]
    pub const fn holder(self) -> Option<LeaseHolder> {
        match self {
            Self::Acquired { holder, .. } => Some(holder),
            Self::Released { .. } => None,
            Self::Handoff { to, .. } => Some(to),
            Self::Expired { holder } => Some(holder),
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
    /// The requested tenure is empty or past [`MAX_LEASE_TERM_TICKS`].
    InvalidTerm,
    /// The tenure lapsed at `now`; the lease is unchanged (re-acquire or
    /// sweep first).
    Expired {
        /// Occupant whose tenure lapsed; it keeps nothing.
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
            Self::InvalidTerm => "invalid_term",
            Self::Expired { .. } => "expired",
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
            Self::InvalidTerm => write!(
                f,
                "lease term must be 1..={} host ticks",
                MAX_LEASE_TERM_TICKS
            ),
            Self::Expired { holder } => {
                write!(f, "lease tenure of {holder} lapsed")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

/// Pure lease transition kernel for one panel.
///
/// Created [`Idle`](LeaseState::Idle); every method is deterministic and
/// infallible except for the documented refusal, which leaves the lease
/// unchanged. `now` is always a host-supplied monotonic tick: the kernel
/// compares ticks but never reads a clock.
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

    /// Whether the current tenure lapsed at `now`.
    ///
    /// Idle leases never lapse; an occupied lease lapses at or past its
    /// `expires_at`. A lapsed lease still reads `Occupied` until
    /// [`Self::sweep`] moves it back to idle.
    #[must_use]
    pub const fn is_expired(self, now: u64) -> bool {
        match self.state {
            LeaseState::Idle => false,
            LeaseState::Occupied { expires_at, .. } => now >= expires_at,
        }
    }

    /// Acquires an idle panel for `holder` for `term_ticks` host ticks
    /// starting at `now`.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::AlreadyOccupied`] when the panel is occupied,
    /// or [`LeaseError::InvalidTerm`] when the term is zero or past
    /// [`MAX_LEASE_TERM_TICKS`]; the lease is unchanged.
    pub fn acquire(
        &mut self,
        holder: LeaseHolder,
        term_ticks: u64,
        now: u64,
    ) -> Result<LeaseEvent, LeaseError> {
        if term_ticks == 0 || term_ticks > MAX_LEASE_TERM_TICKS {
            return Err(LeaseError::InvalidTerm);
        }
        match self.state {
            LeaseState::Idle => {
                let expires_at = now.saturating_add(term_ticks);
                self.state = LeaseState::Occupied { holder, expires_at };
                Ok(LeaseEvent::Acquired { holder, expires_at })
            }
            LeaseState::Occupied {
                holder: current, ..
            } => Err(LeaseError::AlreadyOccupied { holder: current }),
        }
    }

    /// Releases an occupied panel back to idle; only the occupant may release.
    ///
    /// Release stays available past expiry (releasing is the safe
    /// direction): the holder match is still required.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotOccupied`] when idle, or
    /// [`LeaseError::NotHolder`] when the caller is not the occupant; the
    /// lease is unchanged.
    pub fn release(&mut self, holder: LeaseHolder) -> Result<LeaseEvent, LeaseError> {
        match self.state {
            LeaseState::Idle => Err(LeaseError::NotOccupied),
            LeaseState::Occupied {
                holder: current, ..
            } => {
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
    /// The tenure deadline is preserved: handoff never extends tenure, so
    /// the new occupant inherits the remaining ticks and must re-acquire
    /// after expiry.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotOccupied`] when idle,
    /// [`LeaseError::NotHolder`] when `from` is not the occupant, or
    /// [`LeaseError::Expired`] when the tenure lapsed at `now`; the lease
    /// is unchanged.
    pub fn handoff(
        &mut self,
        from: LeaseHolder,
        to: LeaseHolder,
        now: u64,
    ) -> Result<LeaseEvent, LeaseError> {
        match self.state {
            LeaseState::Idle => Err(LeaseError::NotOccupied),
            LeaseState::Occupied {
                holder: current,
                expires_at,
            } => {
                if current != from {
                    return Err(LeaseError::NotHolder { holder: current });
                }
                if now >= expires_at {
                    return Err(LeaseError::Expired { holder: current });
                }
                self.state = LeaseState::Occupied {
                    holder: to,
                    expires_at,
                };
                Ok(LeaseEvent::Handoff {
                    from,
                    to,
                    expires_at,
                })
            }
        }
    }

    /// Sweeps a lapsed tenure back to idle (OQ-083 adopted expiry).
    ///
    /// Returns the [`LeaseEvent::Expired`] transition when the lease was
    /// occupied and lapsed at `now`, and `None` otherwise (idle, or a live
    /// tenure — both unchanged). The host calls this with its tick before
    /// enforcing write gates.
    pub fn sweep(&mut self, now: u64) -> Option<LeaseEvent> {
        match self.state {
            LeaseState::Idle => None,
            LeaseState::Occupied { holder, expires_at } => {
                if now >= expires_at {
                    self.state = LeaseState::Idle;
                    Some(LeaseEvent::Expired { holder })
                } else {
                    None
                }
            }
        }
    }

    /// Whether `holder` may write to this panel at `now` (SEC-26 write
    /// hook, OQ-083 adopted).
    ///
    /// Only the current occupant within a live tenure may write; idle
    /// panels, non-holders, and lapsed tenures fail. The human path acts
    /// outside the lease (a takeover releases to idle first). Panel write
    /// surfaces must call [`Self::check_write`] before mutating panel
    /// content.
    #[must_use]
    pub fn may_write(&self, holder: LeaseHolder, now: u64) -> bool {
        match self.state {
            LeaseState::Idle => false,
            LeaseState::Occupied {
                holder: current,
                expires_at,
            } => current == holder && now < expires_at,
        }
    }

    /// Check write permission for `holder` at `now` without changing the
    /// lease.
    ///
    /// Idle panels deny with [`LeaseError::NotOccupied`]; occupied panels
    /// deny non-holders with [`LeaseError::NotHolder`] and deny lapsed
    /// tenures (even the occupant's) with [`LeaseError::Expired`].
    /// Holder tags are opaque: diagnostics never carry panel content.
    pub fn check_write(&self, holder: LeaseHolder, now: u64) -> Result<(), LeaseError> {
        match self.state {
            LeaseState::Idle => Err(LeaseError::NotOccupied),
            LeaseState::Occupied {
                holder: current,
                expires_at,
            } => {
                if current != holder {
                    return Err(LeaseError::NotHolder { holder: current });
                }
                if now >= expires_at {
                    return Err(LeaseError::Expired { holder: current });
                }
                Ok(())
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
    const NOW: u64 = 1_000;
    const TERM: u64 = 100;

    fn occupied() -> PanelLease {
        let mut lease = PanelLease::idle();
        lease.acquire(A, TERM, NOW).expect("test acquire succeeds");
        lease
    }

    #[test]
    fn fresh_lease_is_idle() {
        assert_eq!(PanelLease::idle().state(), LeaseState::Idle);
        assert_eq!(PanelLease::default().state(), LeaseState::Idle);
        assert!(!PanelLease::idle().is_expired(u64::MAX));
    }

    #[test]
    fn acquire_release_round_trip() {
        let mut lease = PanelLease::idle();
        assert_eq!(
            lease.acquire(A, TERM, NOW),
            Ok(LeaseEvent::Acquired {
                holder: A,
                expires_at: NOW + TERM
            })
        );
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
        assert!(!lease.is_expired(NOW));
        assert_eq!(lease.release(A), Ok(LeaseEvent::Released { holder: A }));
        assert_eq!(lease.state(), LeaseState::Idle);
    }

    #[test]
    fn acquire_term_bounds_hold() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.acquire(A, 0, NOW), Err(LeaseError::InvalidTerm));
        assert_eq!(
            lease.acquire(A, MAX_LEASE_TERM_TICKS + 1, NOW),
            Err(LeaseError::InvalidTerm)
        );
        assert_eq!(lease.state(), LeaseState::Idle);
        assert!(lease.acquire(A, MAX_LEASE_TERM_TICKS, NOW).is_ok());
        assert_eq!(lease.release(A), Ok(LeaseEvent::Released { holder: A }));
        // Term validation runs before occupancy: an occupied lease still
        // refuses a bad term without disturbing the occupant.
        assert!(lease.acquire(A, TERM, NOW).is_ok());
        assert_eq!(lease.acquire(B, 0, NOW), Err(LeaseError::InvalidTerm));
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
    }

    #[test]
    fn double_acquire_fails_closed_keeps_holder() {
        let mut lease = occupied();
        assert_eq!(
            lease.acquire(B, TERM, NOW),
            Err(LeaseError::AlreadyOccupied { holder: A })
        );
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
    }

    #[test]
    fn release_by_non_holder_fails_closed() {
        let mut lease = occupied();
        assert_eq!(lease.release(B), Err(LeaseError::NotHolder { holder: A }));
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
    }

    #[test]
    fn release_when_idle_fails() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.release(A), Err(LeaseError::NotOccupied));
    }

    #[test]
    fn release_past_expiry_stays_available() {
        let mut lease = occupied();
        assert!(lease.is_expired(NOW + TERM));
        assert_eq!(
            lease.release(A),
            Ok(LeaseEvent::Released { holder: A }),
            "releasing is the safe direction, even past expiry"
        );
        assert_eq!(lease.state(), LeaseState::Idle);
    }

    #[test]
    fn handoff_moves_occupancy_without_idle_gap() {
        let mut lease = occupied();
        assert_eq!(
            lease.handoff(A, B, NOW),
            Ok(LeaseEvent::Handoff {
                from: A,
                to: B,
                expires_at: NOW + TERM
            })
        );
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: B,
                expires_at: NOW + TERM
            },
            "handoff preserves the tenure deadline"
        );
    }

    #[test]
    fn handoff_by_non_holder_fails_closed() {
        let mut lease = occupied();
        assert_eq!(
            lease.handoff(B, A, NOW),
            Err(LeaseError::NotHolder { holder: A })
        );
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
    }

    #[test]
    fn handoff_when_idle_fails() {
        let mut lease = PanelLease::idle();
        assert_eq!(lease.handoff(A, B, NOW), Err(LeaseError::NotOccupied));
    }

    #[test]
    fn handoff_past_expiry_denies() {
        let mut lease = occupied();
        assert_eq!(
            lease.handoff(A, B, NOW + TERM),
            Err(LeaseError::Expired { holder: A })
        );
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            },
            "denials leave the lease unchanged"
        );
    }

    #[test]
    fn sweep_moves_lapsed_tenure_to_idle() {
        let mut lease = occupied();
        assert_eq!(lease.sweep(NOW), None, "live tenure is untouched");
        assert_eq!(
            lease.sweep(NOW + TERM),
            Some(LeaseEvent::Expired { holder: A })
        );
        assert_eq!(lease.state(), LeaseState::Idle);
        assert_eq!(lease.sweep(NOW + TERM), None, "idle stays idle");
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
        assert_eq!(
            LeaseEvent::Acquired {
                holder: A,
                expires_at: NOW + TERM
            }
            .as_str(),
            "acquired"
        );
        assert_eq!(LeaseEvent::Released { holder: A }.as_str(), "released");
        assert_eq!(
            LeaseEvent::Handoff {
                from: A,
                to: B,
                expires_at: NOW + TERM
            }
            .as_str(),
            "handoff"
        );
        assert_eq!(LeaseEvent::Expired { holder: A }.as_str(), "expired");
        assert_eq!(
            LeaseEvent::Acquired {
                holder: A,
                expires_at: NOW + TERM
            }
            .holder(),
            Some(A)
        );
        assert_eq!(LeaseEvent::Released { holder: A }.holder(), None);
        assert_eq!(
            LeaseEvent::Handoff {
                from: A,
                to: B,
                expires_at: NOW + TERM
            }
            .holder(),
            Some(B)
        );
        assert_eq!(
            LeaseError::AlreadyOccupied { holder: A }.as_str(),
            "already_occupied"
        );
        assert_eq!(LeaseError::NotOccupied.as_str(), "not_occupied");
        assert_eq!(LeaseError::NotHolder { holder: A }.as_str(), "not_holder");
        assert_eq!(LeaseError::InvalidTerm.as_str(), "invalid_term");
        assert_eq!(LeaseError::Expired { holder: A }.as_str(), "expired");
    }

    #[test]
    fn write_hook_allows_only_the_occupant() {
        let lease = PanelLease::idle();
        assert!(!lease.may_write(A, NOW));
        assert_eq!(lease.check_write(A, NOW), Err(LeaseError::NotOccupied));
        let lease = occupied();
        assert!(lease.may_write(A, NOW));
        assert!(!lease.may_write(B, NOW));
        assert_eq!(lease.check_write(A, NOW), Ok(()));
        assert_eq!(
            lease.check_write(B, NOW),
            Err(LeaseError::NotHolder { holder: A })
        );
        // Denials leave the lease unchanged.
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
    }

    #[test]
    fn write_hook_denies_past_expiry() {
        let mut lease = occupied();
        assert!(!lease.may_write(A, NOW + TERM));
        assert_eq!(
            lease.check_write(A, NOW + TERM),
            Err(LeaseError::Expired { holder: A })
        );
        // The lapsed lease still reads occupied until swept.
        assert_eq!(
            lease.state(),
            LeaseState::Occupied {
                holder: A,
                expires_at: NOW + TERM
            }
        );
        assert_eq!(
            lease.sweep(NOW + TERM),
            Some(LeaseEvent::Expired { holder: A })
        );
        assert_eq!(
            lease.check_write(A, NOW + TERM),
            Err(LeaseError::NotOccupied)
        );
    }

    #[test]
    fn write_hook_follows_handoff() {
        let mut lease = occupied();
        assert!(lease.handoff(A, B, NOW).is_ok());
        assert!(!lease.may_write(A, NOW));
        assert!(lease.may_write(B, NOW));
        assert_eq!(lease.check_write(B, NOW), Ok(()));
    }
}
