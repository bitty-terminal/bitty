//! Enforced event-stream bounds and the control-frame reservation
//! (contract `D-10`, `D-14`, `D-18`).
//!
//! # Caps and their contract source
//!
//! | Bound | Default | Compiled maximum | Contract |
//! | --- | --- | --- | --- |
//! | One session's retained event ring, events | 256 | 256 | `max_event_backlog_events` |
//! | One session's retained event ring, bytes | 262,144 | 262,144 | `max_event_backlog_bytes` |
//! | One lease's outbound queue, events | 256 | 256 | `max_lease_queue_events` |
//! | One lease's outbound queue, bytes | 65,536 | 65,536 | `max_lease_queue_bytes` |
//! | Leases per session | 8 | 16 | `max_attaches_per_session` |
//! | Lease heartbeat TTL | 15,000 ms | 15,000 ms | `attach_lease_ttl_ms` |
//! | Attach keys per session | 512 | 512 | `max_idempotency_keys_per_session` |
//! | Attach-key store bytes per session | 65,536 | 65,536 | `max_idempotency_bytes_per_session` |
//!
//! The reservation inside one lease queue is [`LEASE_CONTROL_RESERVATION_EVENTS`]
//! frames and [`LEASE_CONTROL_RESERVATION_BYTES`] bytes: the share a future
//! shed-able observability class may never claim, so a data flood cannot starve
//! a terminal, kill, abort, or lease-loss frame (`D-10`). v0.2.0 defines no
//! observability frames, so the whole queue is available to control frames and
//! the reservation is a structural floor rather than a subtraction.

use super::event::Event;
use super::stream::EventRefusal;

/// Default and compiled ceiling for one session's retained event ring
/// (`D-14` `max_event_backlog_events`).
pub const MAX_EVENT_BACKLOG_EVENTS: usize = 256;

/// Default and compiled ceiling for one session's retained event bytes
/// (`D-14` `max_event_backlog_bytes`, which reuses the RC-10 frame ceiling).
pub const MAX_EVENT_BACKLOG_BYTES: usize = 262_144;

/// Default and compiled ceiling for one lease's outbound queue depth
/// (`D-14` `max_lease_queue_events`).
pub const MAX_LEASE_QUEUE_EVENTS: usize = 256;

/// Default and compiled ceiling for one lease's outbound queue bytes
/// (`D-14` `max_lease_queue_bytes`).
pub const MAX_LEASE_QUEUE_BYTES: usize = 65_536;

/// Default `max_attaches_per_session` (`D-14`).
pub const MAX_ATTACHES_PER_SESSION_DEFAULT: usize = 8;

/// Compiled ceiling for `max_attaches_per_session` (`D-14`).
pub const MAX_ATTACHES_PER_SESSION_SYSTEM_MAX: usize = 16;

/// Default and compiled ceiling for the lease heartbeat TTL
/// (`D-14` `attach_lease_ttl_ms`: three missed 5 s heartbeats detach).
pub const ATTACH_LEASE_TTL_MS: u64 = 15_000;

/// Default and compiled ceiling for one session's attach-key dedupe store
/// (`D-18` `max_idempotency_keys_per_session`).
pub const MAX_ATTACH_KEYS_PER_SESSION: usize = 512;

/// Default and compiled ceiling for the accounted bytes of one session's
/// attach-key dedupe store (`D-18` `max_idempotency_bytes_per_session`).
///
/// The key count alone is not a memory bound: a 128-byte key plus its fixed
/// entry can cost roughly 168 bytes, so 512 entries would exceed the contract's
/// 65,536-byte limit. The stream enforces both bounds.
pub const MAX_ATTACH_KEYS_BYTES_PER_SESSION: usize = 65_536;

/// Frames one lease queue always keeps free for control frames (`D-10`).
///
/// Derivation: one terminal transition emits at most one terminal frame plus
/// one state frame per lease it has to close before the transition, and a drain
/// emits one `daemon_stopping` frame; eight frames cover that burst with room
/// for a revocation pair, at a cost of 8 × ~136 bytes.
pub const LEASE_CONTROL_RESERVATION_EVENTS: usize = 8;

/// Bytes one lease queue always keeps free for control frames (`D-10`).
///
/// Derivation: [`LEASE_CONTROL_RESERVATION_EVENTS`] rounded up to a power of
/// two so the reservation survives an even split with the 64 KiB queue bound.
pub const LEASE_CONTROL_RESERVATION_BYTES: usize = 8_192;

/// Smallest event record the module can hold.
///
/// A bound below one record would make the stream unable to retain any event,
/// so configuration refuses it instead of silently storing nothing.
pub(crate) const MIN_EVENT_BYTES: usize = std::mem::size_of::<Event>();

/// Enforced event-stream bounds for one daemon instance.
///
/// The service layer receives the effective values from configuration and
/// passes validated limits here; bitty#2603 owns advertising them as
/// `{effective, default, system_max}`. Construction refuses a value above the
/// compiled ceiling and below the compiled floor (`D-14`): a configuration
/// mistake can neither raise a bound past the reviewed maximum nor disable the
/// stream by setting a zero bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLimits {
    max_event_backlog_events: usize,
    max_event_backlog_bytes: usize,
    max_lease_queue_events: usize,
    max_lease_queue_bytes: usize,
    max_attaches_per_session: usize,
    attach_lease_ttl_ms: u64,
    max_attach_keys_per_session: usize,
}

impl EventLimits {
    /// Validate and build enforced event-stream limits.
    ///
    /// # Errors
    ///
    /// Returns [`EventRefusal::InvalidLimits`] when a value is outside the
    /// compiled `[minimum, maximum]` range.
    pub fn new(
        max_event_backlog_events: usize,
        max_event_backlog_bytes: usize,
        max_lease_queue_events: usize,
        max_lease_queue_bytes: usize,
        max_attaches_per_session: usize,
        attach_lease_ttl_ms: u64,
        max_attach_keys_per_session: usize,
    ) -> Result<Self, EventRefusal> {
        check_bound(
            "max_event_backlog_events",
            max_event_backlog_events,
            1,
            MAX_EVENT_BACKLOG_EVENTS,
        )?;
        check_bound(
            "max_event_backlog_bytes",
            max_event_backlog_bytes,
            MIN_EVENT_BYTES,
            MAX_EVENT_BACKLOG_BYTES,
        )?;
        check_bound(
            "max_lease_queue_events",
            max_lease_queue_events,
            1,
            MAX_LEASE_QUEUE_EVENTS,
        )?;
        check_bound(
            "max_lease_queue_bytes",
            max_lease_queue_bytes,
            MIN_EVENT_BYTES,
            MAX_LEASE_QUEUE_BYTES,
        )?;
        check_bound(
            "max_attaches_per_session",
            max_attaches_per_session,
            1,
            MAX_ATTACHES_PER_SESSION_SYSTEM_MAX,
        )?;
        check_millis_bound(
            "attach_lease_ttl_ms",
            attach_lease_ttl_ms,
            1,
            ATTACH_LEASE_TTL_MS,
        )?;
        check_bound(
            "max_attach_keys_per_session",
            max_attach_keys_per_session,
            1,
            MAX_ATTACH_KEYS_PER_SESSION,
        )?;
        Ok(Self {
            max_event_backlog_events,
            max_event_backlog_bytes,
            max_lease_queue_events,
            max_lease_queue_bytes,
            max_attaches_per_session,
            attach_lease_ttl_ms,
            max_attach_keys_per_session,
        })
    }

    /// Retained event depth for one session.
    #[must_use]
    pub const fn max_event_backlog_events(self) -> usize {
        self.max_event_backlog_events
    }

    /// Retained event bytes for one session.
    #[must_use]
    pub const fn max_event_backlog_bytes(self) -> usize {
        self.max_event_backlog_bytes
    }

    /// Outbound queue depth for one lease.
    #[must_use]
    pub const fn max_lease_queue_events(self) -> usize {
        self.max_lease_queue_events
    }

    /// Outbound queue bytes for one lease.
    #[must_use]
    pub const fn max_lease_queue_bytes(self) -> usize {
        self.max_lease_queue_bytes
    }

    /// Leases per session.
    #[must_use]
    pub const fn max_attaches_per_session(self) -> usize {
        self.max_attaches_per_session
    }

    /// Lease heartbeat TTL in milliseconds.
    #[must_use]
    pub const fn attach_lease_ttl_ms(self) -> u64 {
        self.attach_lease_ttl_ms
    }

    /// Attach-key dedupe depth for one session.
    #[must_use]
    pub const fn max_attach_keys_per_session(self) -> usize {
        self.max_attach_keys_per_session
    }

    /// Accounted bytes for one session's attach-key dedupe store.
    ///
    /// This is the compiled contract ceiling; the method deliberately ignores
    /// `self` because `D-18` fixes one value and no configuration surface may
    /// raise or lower it.
    #[must_use]
    pub const fn max_attach_keys_bytes_per_session(self) -> usize {
        MAX_ATTACH_KEYS_BYTES_PER_SESSION
    }

    /// Depth a future shed-able class may use: the queue minus the reservation.
    #[must_use]
    pub const fn observability_queue_events(self) -> usize {
        self.max_lease_queue_events
            .saturating_sub(LEASE_CONTROL_RESERVATION_EVENTS)
    }

    /// Bytes a future shed-able class may use: the queue minus the reservation.
    #[must_use]
    pub const fn observability_queue_bytes(self) -> usize {
        self.max_lease_queue_bytes
            .saturating_sub(LEASE_CONTROL_RESERVATION_BYTES)
    }
}

impl Default for EventLimits {
    fn default() -> Self {
        Self {
            max_event_backlog_events: MAX_EVENT_BACKLOG_EVENTS,
            max_event_backlog_bytes: MAX_EVENT_BACKLOG_BYTES,
            max_lease_queue_events: MAX_LEASE_QUEUE_EVENTS,
            max_lease_queue_bytes: MAX_LEASE_QUEUE_BYTES,
            max_attaches_per_session: MAX_ATTACHES_PER_SESSION_DEFAULT,
            attach_lease_ttl_ms: ATTACH_LEASE_TTL_MS,
            max_attach_keys_per_session: MAX_ATTACH_KEYS_PER_SESSION,
        }
    }
}

fn check_bound(
    limit: &'static str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), EventRefusal> {
    if value < minimum || value > maximum {
        return Err(EventRefusal::InvalidLimits {
            limit,
            value: u64::try_from(value).unwrap_or(u64::MAX),
            minimum: u64::try_from(minimum).unwrap_or(u64::MAX),
            maximum: u64::try_from(maximum).unwrap_or(u64::MAX),
        });
    }
    Ok(())
}

fn check_millis_bound(
    limit: &'static str,
    value: u64,
    minimum: u64,
    maximum: u64,
) -> Result<(), EventRefusal> {
    if value < minimum || value > maximum {
        return Err(EventRefusal::InvalidLimits {
            limit,
            value,
            minimum,
            maximum,
        });
    }
    Ok(())
}
