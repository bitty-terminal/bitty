//! Attach-lease records, roles, and enforced lease bounds (contract `D-09`,
//! `D-12`, `D-14`).
//!
//! A lease is the session-side half of a connection binding; bitty#2607 owns
//! the connection map that associates an `attach_id` with a live connection.
//! The role is either `observer` or `controller`; a session has at most one
//! controller lease and observers up to
//! [`MAX_ATTACHES_PER_SESSION_SYSTEM_MAX`], with the controller additional to
//! that cap (`D-09`). Every lease carries its own server-minted 128-bit
//! [`crate::registry::AttachId`], so two attaches of the same client are two
//! distinct handles.
//!
//! Time is always injected by the caller as [`MonotonicMs`]; there is no
//! wall-clock read and no sleep in this module.

use std::fmt;

use crate::registry::{AttachId, MonotonicMs};

use super::refusal::LeaseRefusal;

/// Default per-session observer cap (`D-14` `max_attaches_per_session`).
pub const MAX_ATTACHES_PER_SESSION_DEFAULT: usize = 8;

/// Compiled per-session observer cap (`D-14` system maximum).
pub const MAX_ATTACHES_PER_SESSION_SYSTEM_MAX: usize = 16;

/// Default lease TTL (`D-14` `attach_lease_ttl_ms`).
pub const ATTACH_LEASE_TTL_MS_DEFAULT: u64 = 15_000;

/// Compiled lease TTL ceiling (`D-14` system maximum).
pub const ATTACH_LEASE_TTL_MS_SYSTEM_MAX: u64 = 15_000;

/// Default abort grace before escalation to kill (`D-14` `abort_grace_ms`).
pub const ABORT_GRACE_MS_DEFAULT: u64 = 5_000;

/// Compiled abort-grace ceiling (`D-14` system maximum).
pub const ABORT_GRACE_MS_SYSTEM_MAX: u64 = 5_000;

/// Default daemon drain grace (`D-14` `drain_grace_ms`).
pub const DRAIN_GRACE_MS_DEFAULT: u64 = 10_000;

/// Compiled drain-grace ceiling (`D-14` system maximum).
pub const DRAIN_GRACE_MS_SYSTEM_MAX: u64 = 10_000;

/// Enforced lease bounds for one daemon instance.
///
/// The service layer receives effective values from configuration and passes
/// them here; bitty#2603 owns advertising them as
/// `{effective, default, system_max}`. Construction refuses a value above the
/// compiled ceiling, so configuration cannot raise a bound past the reviewed
/// maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseLimits {
    max_attaches_per_session: usize,
    attach_lease_ttl_ms: u64,
    abort_grace_ms: u64,
    drain_grace_ms: u64,
}

impl LeaseLimits {
    /// Validate and build enforced lease bounds.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseRefusal::InvalidLimits`] when a value exceeds its
    /// compiled system maximum.
    pub fn new(
        max_attaches_per_session: usize,
        attach_lease_ttl_ms: u64,
        abort_grace_ms: u64,
        drain_grace_ms: u64,
    ) -> Result<Self, LeaseRefusal> {
        if max_attaches_per_session > MAX_ATTACHES_PER_SESSION_SYSTEM_MAX {
            return Err(LeaseRefusal::InvalidLimits {
                limit: "max_attaches_per_session",
                value: u64::try_from(max_attaches_per_session).unwrap_or(u64::MAX),
                maximum: u64::try_from(MAX_ATTACHES_PER_SESSION_SYSTEM_MAX).unwrap_or(u64::MAX),
            });
        }
        check_limit(
            "attach_lease_ttl_ms",
            attach_lease_ttl_ms,
            ATTACH_LEASE_TTL_MS_SYSTEM_MAX,
        )?;
        check_limit("abort_grace_ms", abort_grace_ms, ABORT_GRACE_MS_SYSTEM_MAX)?;
        check_limit("drain_grace_ms", drain_grace_ms, DRAIN_GRACE_MS_SYSTEM_MAX)?;
        Ok(Self {
            max_attaches_per_session,
            attach_lease_ttl_ms,
            abort_grace_ms,
            drain_grace_ms,
        })
    }

    /// Per-session observer cap (`D-09`); the single controller lease is
    /// additional.
    #[must_use]
    pub const fn max_attaches_per_session(self) -> usize {
        self.max_attaches_per_session
    }

    /// Lease TTL in milliseconds.
    #[must_use]
    pub const fn attach_lease_ttl_ms(self) -> u64 {
        self.attach_lease_ttl_ms
    }

    /// Graceful abort window in milliseconds.
    #[must_use]
    pub const fn abort_grace_ms(self) -> u64 {
        self.abort_grace_ms
    }

    /// Daemon drain window in milliseconds.
    #[must_use]
    pub const fn drain_grace_ms(self) -> u64 {
        self.drain_grace_ms
    }
}

impl Default for LeaseLimits {
    fn default() -> Self {
        Self {
            max_attaches_per_session: MAX_ATTACHES_PER_SESSION_DEFAULT,
            attach_lease_ttl_ms: ATTACH_LEASE_TTL_MS_DEFAULT,
            abort_grace_ms: ABORT_GRACE_MS_DEFAULT,
            drain_grace_ms: DRAIN_GRACE_MS_DEFAULT,
        }
    }
}

fn check_limit(limit: &'static str, value: u64, maximum: u64) -> Result<(), LeaseRefusal> {
    if value > maximum {
        return Err(LeaseRefusal::InvalidLimits {
            limit,
            value,
            maximum,
        });
    }
    Ok(())
}

/// Role of one attach lease (`D-09`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaseRole {
    /// Receives metadata state events; no control, input, or resize authority.
    Observer,
    /// The single lease that holds input, abort, grant, and transfer authority.
    Controller,
}

impl LeaseRole {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Controller => "controller",
        }
    }

    /// Whether the role carries control authority.
    #[must_use]
    pub const fn can_control(self) -> bool {
        matches!(self, Self::Controller)
    }
}

impl fmt::Display for LeaseRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One live attach lease (`D-09`).
///
/// The lease is a metadata record: it holds no connection, no queue, and no
/// terminal content. `attached_at`, `last_seen_at`, and `expires_at` are server
/// monotonic values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    attach_id: AttachId,
    role: LeaseRole,
    attached_at: MonotonicMs,
    last_seen_at: MonotonicMs,
    expires_at: MonotonicMs,
}

impl Lease {
    /// Build a lease that expires `ttl_ms` after `now`.
    pub(crate) fn new(attach_id: AttachId, role: LeaseRole, now: MonotonicMs, ttl_ms: u64) -> Self {
        Self {
            attach_id,
            role,
            attached_at: now,
            last_seen_at: now,
            expires_at: MonotonicMs::from_millis(now.as_millis().saturating_add(ttl_ms)),
        }
    }

    /// The server-assigned handle used for detach, transfer, and heartbeat.
    #[must_use]
    pub const fn attach_id(self) -> AttachId {
        self.attach_id
    }

    /// The lease role.
    #[must_use]
    pub const fn role(self) -> LeaseRole {
        self.role
    }

    /// When the lease was created.
    #[must_use]
    pub const fn attached_at(self) -> MonotonicMs {
        self.attached_at
    }

    /// The last successful request naming this lease.
    #[must_use]
    pub const fn last_seen_at(self) -> MonotonicMs {
        self.last_seen_at
    }

    /// The absolute monotonic expiry instant.
    #[must_use]
    pub const fn expires_at(self) -> MonotonicMs {
        self.expires_at
    }

    /// Whether the lease is expired at `now`.
    ///
    /// A lease is live before its expiry instant and expired at exactly the
    /// instant, so a TTL of `T` admits requests at `T - 1` and refuses `T`.
    #[must_use]
    pub const fn is_expired_at(self, now: MonotonicMs) -> bool {
        now.as_millis() >= self.expires_at.as_millis()
    }

    /// Refresh `last_seen_at` and move the expiry to `now + ttl_ms`.
    ///
    /// Renewal is not a snapshot-visible mutation (`D-03`): the caller must not
    /// bump `state_revision` for it.
    pub(crate) fn renew(&mut self, now: MonotonicMs, ttl_ms: u64) {
        self.last_seen_at = now;
        self.expires_at = MonotonicMs::from_millis(now.as_millis().saturating_add(ttl_ms));
    }

    /// Change the role in place during an explicit control transfer.
    pub(crate) fn set_role(&mut self, role: LeaseRole) {
        self.role = role;
    }
}

/// The contract's compiled `lease_record_bytes` ceiling (`D-14`).
// A lease record must stay small enough for the per-session metadata budget;
// 512 bytes is the ceiling the contract's `lease_record_bytes` accounting
// assumes. Kept as a literal so the compile-time assert also builds on the
// MSRV toolchain, where a const used only by another const is dead code.
const _: () = assert!(std::mem::size_of::<Lease>() <= 512);

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> MonotonicMs {
        MonotonicMs::from_millis(value)
    }

    fn attach_id(seed: u8) -> AttachId {
        let mut bytes = [0u8; 16];
        bytes.fill(seed);
        AttachId::parse(
            &bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
        .expect("canonical id")
    }

    #[test]
    fn limits_accept_the_contract_maxima_and_refuse_more() {
        assert_eq!(
            LeaseLimits::default(),
            LeaseLimits::new(8, 15_000, 5_000, 10_000).expect("defaults are valid")
        );
        assert!(
            LeaseLimits::new(16, 15_000, 5_000, 10_000).is_ok(),
            "system maxima are valid"
        );
        assert!(matches!(
            LeaseLimits::new(17, 15_000, 5_000, 10_000),
            Err(LeaseRefusal::InvalidLimits {
                limit: "max_attaches_per_session",
                ..
            })
        ));
        assert!(matches!(
            LeaseLimits::new(8, 15_001, 5_000, 10_000),
            Err(LeaseRefusal::InvalidLimits {
                limit: "attach_lease_ttl_ms",
                ..
            })
        ));
        assert!(matches!(
            LeaseLimits::new(8, 15_000, 5_001, 10_000),
            Err(LeaseRefusal::InvalidLimits {
                limit: "abort_grace_ms",
                ..
            })
        ));
        assert!(matches!(
            LeaseLimits::new(8, 15_000, 5_000, 10_001),
            Err(LeaseRefusal::InvalidLimits {
                limit: "drain_grace_ms",
                ..
            })
        ));
    }

    #[test]
    fn expiry_is_inclusive_at_the_ttl_boundary() {
        let lease = Lease::new(attach_id(1), LeaseRole::Observer, ms(100), 100);
        assert_eq!(lease.expires_at(), ms(200));
        assert!(!lease.is_expired_at(ms(100)), "elapsed 0 is live");
        assert!(!lease.is_expired_at(ms(199)), "elapsed TTL-1 is live");
        assert!(lease.is_expired_at(ms(200)), "elapsed TTL is expired");
        assert!(lease.is_expired_at(ms(201)), "elapsed TTL+1 is expired");
    }

    #[test]
    fn renewal_moves_the_expiry_from_the_last_seen_instant() {
        let mut lease = Lease::new(attach_id(2), LeaseRole::Controller, ms(100), 100);
        lease.renew(ms(199), 100);
        assert_eq!(lease.last_seen_at(), ms(199));
        assert_eq!(lease.expires_at(), ms(299));
        assert!(!lease.is_expired_at(ms(298)));
        assert!(lease.is_expired_at(ms(299)));
    }

    #[test]
    fn zero_ttl_expires_immediately() {
        let lease = Lease::new(attach_id(3), LeaseRole::Observer, ms(7), 0);
        assert!(lease.is_expired_at(ms(7)));
    }

    #[test]
    fn role_spellings_match_the_contract() {
        assert_eq!(LeaseRole::Observer.as_str(), "observer");
        assert_eq!(LeaseRole::Controller.as_str(), "controller");
        assert!(!LeaseRole::Observer.can_control());
        assert!(LeaseRole::Controller.can_control());
    }

    #[test]
    fn lease_fits_the_contract_record_budget() {
        assert!(std::mem::size_of::<Lease>() <= 512);
    }
}
