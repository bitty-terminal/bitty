//! The per-session event stream: attach snapshot plus bounded backlog, revision
//! ordering, per-lease routing, control-frame reservation, the terminal-event
//! guarantee, and heartbeat/last-seen (bitty#2792; contract `D-10`, `D-12`,
//! `D-14`, `D-18`).
//!
//! # Invariants
//!
//! - **Revision agreement.** The registry owns `state_revision`; every visible
//!   mutation is applied there first, and the stream appends exactly one event
//!   at the returned revision. [`SessionStream::append`] refuses a revision
//!   that is not `previous + 1`, so a gap or a duplicate cannot enter the ring.
//! - **One event per visible mutation.** Bind, attach, detach, lease expiry,
//!   control transfer, begin-stopping, and terminate each append one event.
//!   Heartbeat, `last_seen`, and a repeated idempotent request append none.
//! - **Per-lease routing.** The ring stores canonical entries; the stream
//!   enqueues a materialized copy on each recipient lease queue and never on a
//!   detached lease. There is no broadcast and no fan-out to another session.
//! - **Never a partial event.** The ring and every queue hold whole events; a
//!   cap evicts whole events and a control frame that cannot be queued closes
//!   the lease ([`LeaseCloseReason::TerminalEventLost`]) instead of being shed.
//! - **Terminal guarantee.** At `terminate` the stream first closes every lease
//!   that cannot take one more control frame, then appends the terminal event,
//!   then frees the ring. The terminal event is pinned in the terminal slot
//!   until every recipient observes it or retention collects the record; it is
//!   enqueued exactly once per lease and acknowledged exactly once.
//! - **Bounded memory.** Every queue is capped by [`EventLimits`]; a stalled
//!   consumer costs at most one lease queue and is closed when a control frame
//!   no longer fits.
//! - **Retention-bound stream state.** A stream entry exists only while the
//!   registry still resolves its id. A terminal record collected by TTL or
//!   evicted by `max_retained_terminal_records` releases its stream entry in
//!   the same operation; every still-attached lease is closed with
//!   [`LeaseCloseReason::TerminalEventLost`] and reported, never dropped
//!   silently.
//! - **Retained terminal leases.** A terminal transition freezes the registry
//!   record (`D-04`), so a retained snapshot's `attach_count` can be higher
//!   than the leases the stream still holds. Closing a lease on a terminal
//!   record is stream-only and never calls `registry.detach`; the divergence is
//!   bounded by `max_attaches_per_session` per retained record and by the
//!   retention cap, which now bounds the stream map as well.
//!
//! # Compiled caps
//!
//! See [`super::limits`] for the table: the backlog is 256 events / 262,144
//! bytes per session (`max_event_backlog_events` / `max_event_backlog_bytes`),
//! one lease queue is 256 events / 65,536 bytes (`max_lease_queue_events` /
//! `max_lease_queue_bytes`), leases per session are 8 default / 16 maximum
//! (`max_attaches_per_session`), the lease TTL is 15,000 ms
//! (`attach_lease_ttl_ms`), and a session keeps at most 512 attach keys / 65,536
//! accounted bytes (`max_idempotency_keys_per_session` /
//! `max_idempotency_bytes_per_session`). The control reservation is 8 frames /
//! 8,192 bytes of every lease queue.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::registry::{
    AttachId, CreateSession, DaemonInstanceId, MonotonicMs, RegistryRefusal, SessionId,
    SessionRegistry, SessionSnapshot, TerminalOutcome,
};
use crate::registry::{Entropy, ID_GENERATION_ATTEMPTS, OsEntropy};

use super::backlog::Backlog;
use super::event::{AttachKey, Event, EventDetail, EventKind, TerminalMarker};
use super::lease::{Admission, FrameClass, Lease, LeaseClose, LeaseCloseReason};
use super::limits::EventLimits;

/// A refused event-stream operation. Every variant is a whole refusal: no
/// record was inserted, removed, or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRefusal {
    /// The session registry refused the underlying call.
    Registry(RegistryRefusal),
    /// The constructor received a registry that already holds records.
    RegistryNotEmpty,
    /// No stream state exists for this session; it is unknown or collected.
    UnknownSession,
    /// The named lease is not attached to this session. The service layer maps
    /// it to `NotFound` for a foreign lease heartbeat (`D-12`) and to
    /// `not_attached` for a duplicate keyed attach whose lease is gone (`D-18`).
    NotAttached,
    /// `max_attaches_per_session` leases are already live on this session.
    MaxAttaches {
        /// The enforced bound.
        limit: usize,
    },
    /// The attach-key store is at its entry or accounted-byte bound.
    IdempotencyCapacity {
        /// The enforced bound.
        limit: usize,
    },
    /// The same attach key arrived with different parameters (`D-18`).
    IdempotencyConflict,
    /// A replay asked for events after a revision the session has not reached.
    RevisionAhead {
        /// The requested revision.
        requested: u64,
        /// The current `state_revision`.
        current: u64,
    },
    /// A registry mutation did not yield the next revision; the stream fails
    /// closed rather than serve a gap.
    RevisionGap {
        /// The revision the stream required.
        expected: u64,
        /// The revision the registry reported.
        observed: u64,
    },
    /// The terminal event was acknowledged before being handed to the transport.
    TerminalNotDelivered,
    /// A terminal snapshot carried no terminal marker; the registry and the
    /// stream disagree, so the stream fails closed.
    TerminalMarkerMissing,
    /// The terminal event was already observed by this lease.
    TerminalAlreadyObserved,
    /// The operation needs at least one lease id.
    EmptyHeartbeat,
    /// A control transfer named the same lease on both sides.
    InvalidRequest {
        /// Static reason; no caller bytes are echoed.
        reason: &'static str,
    },
    /// Server-side attach id generation failed.
    IdGenerationFailed,
    /// Fresh attach ids repeatedly collided with a live lease.
    IdCollision,
    /// The close cascade exceeded its structural bound; the stream fails closed.
    DeliveryLoop {
        /// Steps attempted before the refusal.
        steps: usize,
    },
    /// A configured bound is outside the compiled range.
    InvalidLimits {
        /// Limit name.
        limit: &'static str,
        /// Requested value.
        value: u64,
        /// Compiled floor.
        minimum: u64,
        /// Compiled ceiling.
        maximum: u64,
    },
}

impl From<RegistryRefusal> for EventRefusal {
    fn from(refusal: RegistryRefusal) -> Self {
        Self::Registry(refusal)
    }
}

impl fmt::Display for EventRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(refusal) => write!(formatter, "registry refused: {refusal}"),
            Self::RegistryNotEmpty => {
                formatter.write_str("the event stream requires an empty registry")
            }
            Self::UnknownSession => formatter.write_str("session stream not found"),
            Self::NotAttached => formatter.write_str("lease is not attached to this session"),
            Self::MaxAttaches { limit } => {
                write!(formatter, "attach limit {limit} reached for this session")
            }
            Self::IdempotencyCapacity { limit } => {
                write!(formatter, "attach key store is full at {limit}")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("attach key was already used with different parameters")
            }
            Self::RevisionAhead { requested, current } => write!(
                formatter,
                "replay requested revision {requested} after the current revision {current}"
            ),
            Self::RevisionGap { expected, observed } => write!(
                formatter,
                "event revision {observed} does not follow the expected {expected}"
            ),
            Self::TerminalNotDelivered => {
                formatter.write_str("terminal event was not handed to the transport yet")
            }
            Self::TerminalMarkerMissing => {
                formatter.write_str("terminal snapshot carried no terminal marker")
            }
            Self::TerminalAlreadyObserved => {
                formatter.write_str("terminal event was already observed by this lease")
            }
            Self::EmptyHeartbeat => formatter.write_str("heartbeat names no lease"),
            Self::InvalidRequest { reason } => write!(formatter, "invalid request: {reason}"),
            Self::IdGenerationFailed => formatter.write_str("attach id generation failed"),
            Self::IdCollision => formatter.write_str("attach id generation collided repeatedly"),
            Self::DeliveryLoop { steps } => {
                write!(
                    formatter,
                    "close cascade exceeded its bound after {steps} steps"
                )
            }
            Self::InvalidLimits {
                limit,
                value,
                minimum,
                maximum,
            } => write!(
                formatter,
                "limit {limit} is {value}, outside the compiled range {minimum}..={maximum}"
            ),
        }
    }
}

impl std::error::Error for EventRefusal {}

/// One session's stream state: the mirrored revision, the ring, the lease
/// queues, the attach-key store, and the terminal slot.
#[derive(Debug)]
pub(crate) struct SessionStream {
    instance_id: DaemonInstanceId,
    session_id: SessionId,
    revision: u64,
    terminalized: bool,
    backlog: Backlog,
    leases: BTreeMap<AttachId, Lease>,
    attach_keys: BTreeMap<AttachKey, AttachKeyEntry>,
    terminal: Option<TerminalSlot>,
}

/// A stored attach key (`D-18`): the lease it produced and the replay window it
/// was created with, so a duplicate can be proven identical. The entry outlives
/// the lease (a duplicate after detach is `NotAttached`, never a second lease)
/// and is released with the ring at the terminal transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachKeyEntry {
    attach_id: AttachId,
    after_revision: u64,
}

/// The pinned terminal event (`D-10`): retained until every recipient observes
/// it or retention collects the record.
#[derive(Debug)]
struct TerminalSlot {
    event: Event,
    pending: BTreeSet<AttachId>,
}

impl SessionStream {
    fn new(instance_id: DaemonInstanceId, session_id: SessionId, limits: EventLimits) -> Self {
        Self {
            instance_id,
            session_id,
            revision: 0,
            terminalized: false,
            backlog: Backlog::new(limits),
            leases: BTreeMap::new(),
            attach_keys: BTreeMap::new(),
            terminal: None,
        }
    }

    /// Append the one event for the revision the registry just reported.
    fn append(
        &mut self,
        revision: u64,
        kind: EventKind,
        terminal: Option<TerminalMarker>,
        detail: EventDetail,
        now: MonotonicMs,
    ) -> Result<Event, EventRefusal> {
        let expected = self
            .revision
            .checked_add(1)
            .ok_or(EventRefusal::RevisionGap {
                expected: u64::MAX,
                observed: revision,
            })?;
        if revision != expected {
            return Err(EventRefusal::RevisionGap {
                expected,
                observed: revision,
            });
        }
        self.revision = revision;
        let event = Event {
            instance_id: self.instance_id,
            session_id: self.session_id,
            attach_id: None,
            revision,
            kind,
            terminal,
            detail,
            emitted_at: now,
        };
        self.backlog.append(event.clone());
        Ok(event)
    }

    fn remove_lease(&mut self, attach_id: AttachId) {
        // The attach key intentionally survives: `D-18` releases session keys at
        // the terminal transition, so a retry with the same key answers
        // `NotAttached` instead of minting a second lease.
        self.leases.remove(&attach_id);
    }

    fn attach_keys_bytes(&self) -> usize {
        self.attach_keys
            .keys()
            .map(|key| key.accounted_bytes() + std::mem::size_of::<AttachKeyEntry>())
            .sum()
    }
}

/// An attach request (`D-10`, `D-18`).
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The session to attach to.
    pub session_id: SessionId,
    /// The last revision the client has already applied; the replay serves
    /// events strictly after it.
    pub after_revision: u64,
    /// Optional client idempotency key. A duplicate with the same parameters
    /// returns the same lease and does not bump the revision.
    pub idempotency_key: Option<AttachKey>,
}

/// The result of an attach: the lease, the snapshot revision, and the replay.
#[derive(Debug, Clone)]
pub struct AttachOutcome {
    /// The minted (or replayed) lease identity.
    pub attach_id: AttachId,
    /// The session snapshot at the current revision.
    pub snapshot: SessionSnapshot,
    /// Snapshot plus bounded backlog plus any pinned terminal event.
    pub replay: Replay,
    /// Whether this call replayed an earlier attach instead of creating one.
    pub replayed: bool,
    /// Leases closed because a control frame no longer fit.
    pub closed: Vec<LeaseClose>,
}

/// Snapshot-at-revision plus the bounded backlog after the requested revision
/// (`D-10`).
#[derive(Debug, Clone)]
pub struct Replay {
    /// The snapshot at the current `state_revision`.
    pub snapshot: SessionSnapshot,
    /// Retained events strictly after the requested revision, in revision
    /// order, routed to the requesting lease when one was named.
    pub events: Vec<Event>,
    /// Whether events after the requested revision were dropped.
    pub backlog_truncated: bool,
    /// The oldest revision the stream can still serve for this session.
    pub earliest_revision: Option<u64>,
    /// The pinned terminal event when this lease has not observed it yet and it
    /// is no longer queued (delivery recovery).
    pub terminal_event: Option<Event>,
}

/// Why a lease is being detached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachCause {
    /// The lease owner detached itself.
    SelfDetach,
    /// The controller or operator revoked the lease (`D-09`).
    Revoked,
}

/// The result of one detach.
#[derive(Debug, Clone)]
pub struct DetachOutcome {
    /// The snapshot after the detach and any close cascade.
    pub snapshot: SessionSnapshot,
    /// The canonical `session_state` event at the detach revision. Survivors
    /// receive this frame; the revoked target receives `final_frame` instead.
    pub event: Event,
    /// The `lease_revoked` frame for the removed lease, when it was revoked.
    /// The transport writes it before closing the connection; a self-detach
    /// produces none.
    pub final_frame: Option<Event>,
    /// Leases closed because a control frame no longer fit.
    pub closed: Vec<LeaseClose>,
}

/// The result of one plain invisible-state mutation (`bind`,
/// `begin_stopping`).
#[derive(Debug, Clone)]
pub struct MutationReport {
    /// The snapshot at the current revision.
    pub snapshot: SessionSnapshot,
    /// The canonical event at the mutation revision.
    pub event: Event,
    /// Leases closed because a control frame no longer fit.
    pub closed: Vec<LeaseClose>,
}

/// A terminal session whose stream state was dropped because the registry no
/// longer resolved its id (TTL collection or retention-cap eviction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredSession {
    /// The retired session.
    pub session_id: SessionId,
    /// Leases still attached when the record was retired; each is closed with
    /// [`LeaseCloseReason::TerminalEventLost`].
    pub closed: Vec<LeaseClose>,
}

/// The result of a terminal transition (`D-10` terminal guarantee).
#[derive(Debug, Clone)]
pub struct TerminalReport {
    /// The immutable terminal snapshot.
    pub snapshot: SessionSnapshot,
    /// The canonical `session_terminal` event.
    pub event: Event,
    /// Leases closed with `TerminalEventLost` before the transition became
    /// visible, plus every lease on a session the retention cap evicted during
    /// this call. This flat list overlaps [`RetiredSession::closed`]; a consumer
    /// that needs the session association uses [`TerminalReport::retired`].
    pub lost: Vec<AttachId>,
    /// Terminal sessions whose stream state this call pruned after the
    /// registry evicted them.
    pub retired: Vec<RetiredSession>,
}

/// The result of [`SessionEventStream::collect_retained`].
#[derive(Debug, Clone)]
pub struct RetentionReport {
    /// Session ids collected by TTL.
    pub collected: Vec<SessionId>,
    /// Stream entries dropped because the registry no longer resolves them,
    /// with every still-attached lease closed as lost.
    pub retired: Vec<RetiredSession>,
}

/// The result of `expire_leases`.
#[derive(Debug, Clone)]
pub struct ExpiryOutcome {
    /// The snapshot at the current revision.
    pub snapshot: SessionSnapshot,
    /// One canonical event per expired lease.
    pub events: Vec<Event>,
    /// Leases detached by expiry, in `attach_id` order.
    pub expired: Vec<AttachId>,
    /// Leases closed because a control frame no longer fit.
    pub closed: Vec<LeaseClose>,
}

/// One lease's heartbeat state after a refresh (`D-12`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseHeartbeat {
    /// The refreshed lease.
    pub attach_id: AttachId,
    /// The refreshed last-seen value.
    pub last_seen_ms: MonotonicMs,
    /// The absolute expiry derived from the TTL.
    pub lease_expires_at_ms: MonotonicMs,
}

/// The result of a heartbeat (`D-12`). A heartbeat never bumps a revision and
/// never allocates per-lease storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatReport {
    /// The server monotonic time the refresh used.
    pub server_monotonic_ms: MonotonicMs,
    /// One entry per named lease, in the request order.
    pub leases: Vec<LeaseHeartbeat>,
}

/// The result of a daemon drain announcement (`D-05`, `D-10`).
#[derive(Debug, Clone)]
pub struct DaemonStoppingReport {
    /// Live sessions that had at least one lease.
    pub sessions: usize,
    /// Lease copies queued.
    pub delivered: usize,
    /// Leases closed because the control frame no longer fit.
    pub closed: Vec<(SessionId, LeaseClose)>,
    /// Sessions whose announcement failed an internal invariant; the drain
    /// continues so one broken session cannot strand the rest.
    pub failed: Vec<SessionId>,
}

/// One daemon instance's session event stream around one [`SessionRegistry`].
///
/// The stream owns the registry so a mutation and its event are applied under
/// one borrow: a caller cannot bump a revision without emitting its event, and
/// the ring cannot disagree with `state_revision`.
pub struct SessionEventStream {
    registry: SessionRegistry,
    limits: EventLimits,
    entropy: Box<dyn Entropy + Send>,
    sessions: BTreeMap<SessionId, SessionStream>,
    pending_retired: Vec<RetiredSession>,
}

impl fmt::Debug for SessionEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionEventStream")
            .field("instance_id", &self.registry.instance_id())
            .field("limits", &self.limits)
            .field("sessions", &self.sessions.len())
            .field("pending_retired", &self.pending_retired.len())
            .finish()
    }
}

impl SessionEventStream {
    /// Wrap an empty registry with the default event bounds and the OS CSPRNG.
    ///
    /// The registry must be empty: the stream mirrors every session's
    /// `state_revision` and cannot reconstruct a ring that predates it. The
    /// daemon composes both at startup, before the first `create`.
    ///
    /// # Errors
    ///
    /// Returns [`EventRefusal::RegistryNotEmpty`] when the registry already
    /// holds a live or retained record.
    pub fn new(registry: SessionRegistry) -> Result<Self, EventRefusal> {
        Self::with_limits(registry, EventLimits::default())
    }

    /// Wrap an empty registry with enforced event bounds and the OS CSPRNG.
    ///
    /// The registry must be empty; see [`Self::new`].
    ///
    /// # Errors
    ///
    /// Returns [`EventRefusal::RegistryNotEmpty`] when the registry already
    /// holds a live or retained record.
    pub fn with_limits(
        registry: SessionRegistry,
        limits: EventLimits,
    ) -> Result<Self, EventRefusal> {
        Self::with_entropy(registry, limits, Box::new(OsEntropy))
    }

    pub(crate) fn with_entropy(
        registry: SessionRegistry,
        limits: EventLimits,
        entropy: Box<dyn Entropy + Send>,
    ) -> Result<Self, EventRefusal> {
        if registry.snapshots().next().is_some() {
            return Err(EventRefusal::RegistryNotEmpty);
        }
        Ok(Self {
            registry,
            limits,
            entropy,
            sessions: BTreeMap::new(),
            pending_retired: Vec::new(),
        })
    }

    /// The wrapped registry, for status reads that carry no stream state.
    #[must_use]
    pub const fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// The enforced event bounds.
    #[must_use]
    pub const fn limits(&self) -> EventLimits {
        self.limits
    }

    /// Sessions with stream state (live plus retained).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Register a new session and its empty ring at revision `0`.
    ///
    /// A defensive sweep also drops any terminal entry the registry can no
    /// longer resolve; the cap normally evicts during `terminate`, so this is a
    /// no-op for a correctly sequenced caller. Any retired lease is available
    /// through [`Self::take_retired`].
    ///
    /// # Errors
    ///
    /// Returns the registry refusal unchanged; no stream state is inserted.
    pub fn create(
        &mut self,
        request: CreateSession,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, EventRefusal> {
        let snapshot = self.registry.create(request, now)?;
        let instance_id = self.registry.instance_id();
        self.sessions.insert(
            snapshot.session_id,
            SessionStream::new(instance_id, snapshot.session_id, self.limits),
        );
        let retired = self.prune_unresolved();
        self.pending_retired.extend(retired);
        Ok(snapshot)
    }

    /// Attach one lease: snapshot at the current revision plus the bounded
    /// backlog after `after_revision` (`D-10`, `D-18`).
    ///
    /// A duplicate idempotency key with the same parameters replays the original
    /// lease: no second lease, no revision bump, no duplicated event.
    ///
    /// # Errors
    ///
    /// Refuses whole when the session or lease state is missing, the attach cap
    /// is reached, the key store is full, the key conflicts, a counter would
    /// wrap, or the replay asks for a revision the session has not reached.
    pub fn attach(
        &mut self,
        request: AttachRequest,
        now: MonotonicMs,
    ) -> Result<AttachOutcome, EventRefusal> {
        let session_id = request.session_id;
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;

        if let Some(key) = request.idempotency_key.as_ref() {
            if let Some(entry) = session.attach_keys.get(key).copied() {
                if entry.after_revision != request.after_revision {
                    return Err(EventRefusal::IdempotencyConflict);
                }
                if !session.leases.contains_key(&entry.attach_id) {
                    return Err(EventRefusal::NotAttached);
                }
                let replay =
                    self.replay_for(session_id, Some(entry.attach_id), request.after_revision)?;
                return Ok(AttachOutcome {
                    attach_id: entry.attach_id,
                    snapshot: replay.snapshot.clone(),
                    replay,
                    replayed: true,
                    closed: Vec::new(),
                });
            }
            let keys_limit = self.limits.max_attach_keys_per_session();
            if session.attach_keys.len() >= keys_limit {
                return Err(EventRefusal::IdempotencyCapacity { limit: keys_limit });
            }
            let bytes_limit = self.limits.max_attach_keys_bytes_per_session();
            let entry_bytes = key.accounted_bytes() + std::mem::size_of::<AttachKeyEntry>();
            if session.attach_keys_bytes().saturating_add(entry_bytes) > bytes_limit {
                return Err(EventRefusal::IdempotencyCapacity { limit: bytes_limit });
            }
        }

        if session.leases.len() >= self.limits.max_attaches_per_session() {
            return Err(EventRefusal::MaxAttaches {
                limit: self.limits.max_attaches_per_session(),
            });
        }
        let current = session.revision;
        if request.after_revision > current {
            return Err(EventRefusal::RevisionAhead {
                requested: request.after_revision,
                current,
            });
        }

        // Never reuse a live lease id: an overwrite would desynchronise the
        // registry's attach count from the lease map. The key store also
        // reserves its attach ids: a stored key outlives its lease, and a
        // replayed key must resolve only the lease that key originally minted.
        // The retry bound keeps a broken entropy source a typed refusal rather
        // than a loop.
        let mut reserved: BTreeSet<AttachId> = session.leases.keys().copied().collect();
        reserved.extend(session.attach_keys.values().map(|entry| entry.attach_id));
        let mut attach_id = None;
        for _ in 0..ID_GENERATION_ATTEMPTS {
            let candidate = AttachId::generate_with(self.entropy.as_mut())
                .map_err(|_| EventRefusal::IdGenerationFailed)?;
            if !reserved.contains(&candidate) {
                attach_id = Some(candidate);
                break;
            }
        }
        let attach_id = attach_id.ok_or(EventRefusal::IdCollision)?;
        let snapshot = self.registry.attach(session_id, now)?;
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.append(
                snapshot.state_revision,
                EventKind::SessionState,
                None,
                EventDetail::None,
                now,
            )?
        };
        // The attaching lease receives this event through its replay, so it is
        // routed to the leases that were already attached.
        let delivered = self.deliver(session_id, event, now)?;

        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        session.leases.insert(attach_id, Lease::new(now));
        if let Some(key) = request.idempotency_key {
            session.attach_keys.insert(
                key,
                AttachKeyEntry {
                    attach_id,
                    after_revision: request.after_revision,
                },
            );
        }

        let replay = self.replay_for(session_id, Some(attach_id), request.after_revision)?;
        Ok(AttachOutcome {
            attach_id,
            snapshot: replay.snapshot.clone(),
            replay,
            replayed: false,
            closed: delivered.closed,
        })
    }

    /// Detach one lease and emit the one event for that visible mutation.
    ///
    /// Survivors receive the canonical `session_state` frame. A revocation adds
    /// a target-only `lease_revoked` final frame, so the revoked flag is
    /// attributable to the lease it names and is not broadcast to every
    /// surviving lease.
    ///
    /// # Errors
    ///
    /// Refuses when the session or the lease is unknown, or the registry refuses
    /// the detach (a terminal session is read-only; use [`Self::close_lease`]).
    pub fn detach(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
        cause: DetachCause,
        now: MonotonicMs,
    ) -> Result<DetachOutcome, EventRefusal> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if !session.leases.contains_key(&attach_id) {
            return Err(EventRefusal::NotAttached);
        }

        let snapshot = self.registry.detach(session_id, now)?;
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.append(
                snapshot.state_revision,
                EventKind::SessionState,
                None,
                EventDetail::None,
                now,
            )?
        };
        let final_frame = match cause {
            DetachCause::SelfDetach => None,
            DetachCause::Revoked => {
                let mut frame = event.routed_to(attach_id);
                frame.kind = EventKind::LeaseRevoked;
                Some(frame)
            }
        };
        {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.remove_lease(attach_id);
        }
        let delivered = self.deliver(session_id, event.clone(), now)?;
        Ok(DetachOutcome {
            snapshot: self.current_snapshot(session_id)?,
            event,
            final_frame,
            closed: delivered.closed,
        })
    }

    /// Drop a lease whose transport is gone, without a session-visible event it
    /// could not receive.
    ///
    /// This is the `TerminalEventLost` path: the lease is already closed, so it
    /// is removed from the stream. On a live session the registry detach and its
    /// one `session_state` event still apply; on a retained terminal record the
    /// registry is read-only, so only the stream state is released.
    ///
    /// # Errors
    ///
    /// Refuses when the session or the lease is unknown. The returned list
    /// starts with the requested lease and continues with any lease the close
    /// cascade had to close in turn.
    pub fn close_lease(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<Vec<LeaseClose>, EventRefusal> {
        let mut closed = Vec::new();
        let mut pending = VecDeque::new();
        self.close_with_loss(session_id, attach_id, now, &mut closed, &mut pending)?;
        let cascaded = self.drain_pending_with_recipients(session_id, pending, now)?;
        closed.extend(cascaded.closed);
        if closed.is_empty() {
            return Err(EventRefusal::NotAttached);
        }
        Ok(closed)
    }

    /// Refresh every named lease without bumping a revision (`D-12`).
    ///
    /// # Errors
    ///
    /// Refuses whole when the list is empty, the session is unknown or terminal,
    /// or any named lease is not attached to this session (`NotAttached`; a
    /// foreign lease is never refreshed).
    pub fn heartbeat(
        &mut self,
        session_id: SessionId,
        attach_ids: &[AttachId],
        now: MonotonicMs,
    ) -> Result<HeartbeatReport, EventRefusal> {
        if attach_ids.is_empty() {
            return Err(EventRefusal::EmptyHeartbeat);
        }
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if session.terminalized {
            let state = self
                .registry
                .snapshot(session_id)
                .ok_or(EventRefusal::UnknownSession)?
                .state;
            return Err(EventRefusal::Registry(RegistryRefusal::NotRunning {
                state,
            }));
        }
        for attach_id in attach_ids {
            if !session.leases.contains_key(attach_id) {
                return Err(EventRefusal::NotAttached);
            }
        }

        self.registry.heartbeat(session_id, now)?;
        let ttl = self.limits.attach_lease_ttl_ms();
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        let mut leases = Vec::with_capacity(attach_ids.len());
        for attach_id in attach_ids {
            let lease = session.leases.get_mut(attach_id).expect("validated above");
            lease.last_seen = now;
            leases.push(LeaseHeartbeat {
                attach_id: *attach_id,
                last_seen_ms: now,
                lease_expires_at_ms: MonotonicMs::from_millis(now.as_millis().saturating_add(ttl)),
            });
        }
        Ok(HeartbeatReport {
            server_monotonic_ms: now,
            leases,
        })
    }

    /// Detach every lease whose heartbeat is at or past the TTL (`D-12`).
    ///
    /// Staleness detaches only: no abort, no generation change, no terminal
    /// reason. Each expiry is one visible mutation with one event.
    ///
    /// # Errors
    ///
    /// Refuses when the session is unknown or the registry refuses the detach.
    pub fn expire_leases(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<ExpiryOutcome, EventRefusal> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if session.terminalized {
            return Ok(ExpiryOutcome {
                snapshot: self.current_snapshot(session_id)?,
                events: Vec::new(),
                expired: Vec::new(),
                closed: Vec::new(),
            });
        }
        let ttl = self.limits.attach_lease_ttl_ms();
        let expired: Vec<AttachId> = session
            .leases
            .iter()
            .filter(|(_, lease)| now.saturating_since(lease.last_seen) >= ttl)
            .map(|(attach_id, _)| *attach_id)
            .collect();
        if expired.is_empty() {
            return Ok(ExpiryOutcome {
                snapshot: self.current_snapshot(session_id)?,
                events: Vec::new(),
                expired,
                closed: Vec::new(),
            });
        }

        let mut events = Vec::with_capacity(expired.len());
        let mut detached = Vec::with_capacity(expired.len());
        let mut closed = Vec::new();
        for attach_id in expired {
            // A close cascade from an earlier expiry may already have removed
            // this lease; detaching it again would desynchronise the counters.
            let still_attached = self
                .sessions
                .get(&session_id)
                .is_some_and(|session| session.leases.contains_key(&attach_id));
            if !still_attached {
                continue;
            }
            let snapshot = self.registry.detach(session_id, now)?;
            let event = {
                let session = self
                    .sessions
                    .get_mut(&session_id)
                    .ok_or(EventRefusal::UnknownSession)?;
                session.append(
                    snapshot.state_revision,
                    EventKind::SessionState,
                    None,
                    EventDetail::None,
                    now,
                )?
            };
            {
                let session = self
                    .sessions
                    .get_mut(&session_id)
                    .ok_or(EventRefusal::UnknownSession)?;
                session.remove_lease(attach_id);
            }
            let delivered = self.deliver(session_id, event.clone(), now)?;
            closed.extend(delivered.closed);
            events.push(event);
            detached.push(attach_id);
        }
        Ok(ExpiryOutcome {
            snapshot: self.current_snapshot(session_id)?,
            events,
            expired: detached,
            closed,
        })
    }

    /// Bind the worker: one revision bump and one `session_state` event.
    ///
    /// # Errors
    ///
    /// Returns the registry refusal unchanged.
    pub fn bind(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<MutationReport, EventRefusal> {
        let snapshot = self.registry.bind(session_id, now)?;
        self.mutation(session_id, snapshot.state_revision, now)
    }

    /// Accept a graceful stop: one revision bump and one `session_state` event.
    ///
    /// # Errors
    ///
    /// Returns the registry refusal unchanged.
    pub fn begin_stopping(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<MutationReport, EventRefusal> {
        let snapshot = self.registry.begin_stopping(session_id, now)?;
        self.mutation(session_id, snapshot.state_revision, now)
    }

    /// Move control between two attached leases: one revision bump and one
    /// `control_transfer` event addressed to both (`D-09`, `D-10`).
    ///
    /// Lease authority (the control token, the controller role, the target's
    /// scopes) belongs to bitty#2609; this method records the revision and the
    /// event once that slice has authorized the move.
    ///
    /// # Errors
    ///
    /// Refuses when either lease is not attached, both sides are the same lease,
    /// or the registry refuses the revision bump.
    pub fn publish_control_transfer(
        &mut self,
        session_id: SessionId,
        from: AttachId,
        to: AttachId,
        now: MonotonicMs,
    ) -> Result<MutationReport, EventRefusal> {
        if from == to {
            return Err(EventRefusal::InvalidRequest {
                reason: "control transfer target is the current controller lease",
            });
        }
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if !session.leases.contains_key(&from) || !session.leases.contains_key(&to) {
            return Err(EventRefusal::NotAttached);
        }
        let snapshot = self.registry.note_visible_mutation(session_id, now)?;
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.append(
                snapshot.state_revision,
                EventKind::ControlTransfer,
                None,
                EventDetail::ControlTransfer { from, to },
                now,
            )?
        };
        let delivered = self.deliver(session_id, event.clone(), now)?;
        {
            // `D-12`: a successful request that names a lease refreshes it.
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            for attach_id in [from, to] {
                if let Some(lease) = session.leases.get_mut(&attach_id) {
                    lease.last_seen = now;
                }
            }
        }
        Ok(MutationReport {
            snapshot: self.current_snapshot(session_id)?,
            event,
            closed: delivered.closed,
        })
    }

    /// Terminate the session and pin the terminal event for every lease
    /// (`D-10` terminal guarantee).
    ///
    /// Leases whose queue cannot accept one more control frame are closed first
    /// and reported in [`TerminalReport::lost`]; the terminal state becomes
    /// visible only after that reservation step. The ring is freed only after
    /// the terminal event is pinned, so trimming can never lose it.
    ///
    /// # Errors
    ///
    /// Returns the registry refusal unchanged; a refused transition leaves the
    /// stream untouched.
    pub fn terminate(
        &mut self,
        session_id: SessionId,
        outcome: TerminalOutcome,
        now: MonotonicMs,
    ) -> Result<TerminalReport, EventRefusal> {
        if !self.sessions.contains_key(&session_id) {
            return Err(EventRefusal::UnknownSession);
        }
        // `D-04` first: a refused transition must not have closed a lease.
        self.registry
            .check_terminal_transition(session_id, outcome)?;

        // Reserve one control slot per attached lease before the transition
        // becomes visible. Closing a lease detaches it, which is itself a
        // visible mutation, so the cascade is bounded by the lease count.
        let mut lost = Vec::new();
        let max_steps = self.limits.max_attaches_per_session() * 2 + 2;
        for _ in 0..max_steps {
            let overflow: Vec<AttachId> = {
                let session = self
                    .sessions
                    .get(&session_id)
                    .ok_or(EventRefusal::UnknownSession)?;
                session
                    .leases
                    .iter()
                    .filter(|(_, lease)| {
                        !lease
                            .queue
                            .can_admit_control(&self.limits, std::mem::size_of::<Event>())
                    })
                    .map(|(attach_id, _)| *attach_id)
                    .collect()
            };
            if overflow.is_empty() {
                break;
            }
            let mut closed = Vec::new();
            let mut pending = VecDeque::new();
            for attach_id in overflow {
                self.close_with_loss(session_id, attach_id, now, &mut closed, &mut pending)?;
            }
            let cascaded = self.drain_pending_with_recipients(session_id, pending, now)?;
            closed.extend(cascaded.closed);
            lost.extend(closed.into_iter().map(|close| close.attach_id));
        }
        if lost.len() > self.limits.max_attaches_per_session() {
            return Err(EventRefusal::DeliveryLoop { steps: lost.len() });
        }

        let snapshot = self.registry.terminate(session_id, outcome, now)?;
        let marker =
            TerminalMarker::from_snapshot(&snapshot).ok_or(EventRefusal::TerminalMarkerMissing)?;
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            let event = session.append(
                snapshot.state_revision,
                EventKind::SessionTerminal,
                Some(marker),
                EventDetail::None,
                now,
            )?;
            // The ring and the attach-key store are released at the terminal
            // transition (`D-18`); the terminal event is pinned in the slot
            // below, so it survives the free.
            session.backlog.free();
            session.attach_keys.clear();
            session.terminalized = true;
            event
        };

        // The pre-reservation guarantees room for exactly this frame.
        let mut pending = BTreeSet::new();
        let mut still_lost = Vec::new();
        let lease_ids: Vec<AttachId> = {
            let session = self
                .sessions
                .get(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.leases.keys().copied().collect()
        };
        for attach_id in lease_ids {
            let frame = event.routed_to(attach_id);
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            match session
                .leases
                .get_mut(&attach_id)
                .expect("listed above")
                .queue
                .admit(frame, FrameClass::Control, &self.limits)
            {
                Admission::Accepted => {
                    pending.insert(attach_id);
                }
                Admission::Lost | Admission::Shed => {
                    session.remove_lease(attach_id);
                    still_lost.push(attach_id);
                }
            }
        }
        lost.extend(still_lost);

        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        // No pending consumer means no pin: storing an empty slot would retain
        // the terminal event until retention for nobody.
        if pending.is_empty() {
            session.terminal = None;
        } else {
            session.terminal = Some(TerminalSlot {
                event: event.clone(),
                pending,
            });
        }

        // The registry may have evicted an older retained record while inserting
        // this one. Drop its stream state and report every still-attached lease.
        let retired = self.prune_unresolved();
        for session in &retired {
            lost.extend(session.closed.iter().map(|close| close.attach_id));
        }
        Ok(TerminalReport {
            snapshot,
            event,
            lost,
            retired,
        })
    }

    /// Acknowledge the terminal event for one lease, releasing the pin once
    /// every recipient has observed it (`D-10`).
    ///
    /// # Errors
    ///
    /// Refuses when the session or lease is unknown, the lease was not a
    /// recipient, the frame is still queued (not yet handed to the transport),
    /// or the lease already observed it.
    pub fn ack_terminal(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
    ) -> Result<(), EventRefusal> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if !session.leases.contains_key(&attach_id) {
            return Err(EventRefusal::NotAttached);
        }
        let Some(slot) = session.terminal.as_mut() else {
            return Err(EventRefusal::TerminalAlreadyObserved);
        };
        if !slot.pending.contains(&attach_id) {
            return Err(EventRefusal::TerminalAlreadyObserved);
        }
        let lease = session
            .leases
            .get_mut(&attach_id)
            .ok_or(EventRefusal::NotAttached)?;
        if lease
            .queue
            .contains(slot.event.revision, EventKind::SessionTerminal)
        {
            return Err(EventRefusal::TerminalNotDelivered);
        }
        lease.terminal_observed = true;
        slot.pending.remove(&attach_id);
        let done = slot.pending.is_empty();
        if done {
            session.terminal = None;
        }
        Ok(())
    }

    /// Announce a daemon drain to every live lease (`D-05`, `D-10`).
    ///
    /// `daemon_stopping` is a daemon-scoped control frame, not a session
    /// mutation, so it neither bumps a revision nor enters a session backlog.
    #[must_use]
    pub fn publish_daemon_stopping(
        &mut self,
        deadline_ms: u64,
        now: MonotonicMs,
    ) -> DaemonStoppingReport {
        let session_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let mut report = DaemonStoppingReport {
            sessions: 0,
            delivered: 0,
            closed: Vec::new(),
            failed: Vec::new(),
        };
        for session_id in session_ids {
            let Some(session) = self.sessions.get(&session_id) else {
                continue;
            };
            if session.terminalized || session.leases.is_empty() {
                continue;
            }
            let event = Event {
                instance_id: session.instance_id,
                session_id,
                attach_id: None,
                revision: session.revision,
                kind: EventKind::DaemonStopping,
                terminal: None,
                detail: EventDetail::DaemonStopping { deadline_ms },
                emitted_at: now,
            };
            report.sessions += 1;
            match self.deliver(session_id, event, now) {
                Ok(delivered) => {
                    report.delivered += delivered.recipients.len();
                    for close in delivered.closed {
                        report.closed.push((session_id, close));
                    }
                }
                Err(_) => report.failed.push(session_id),
            }
        }
        report
    }

    /// Snapshot at the current revision plus the backlog after
    /// `after_revision` (`D-10`).
    ///
    /// # Errors
    ///
    /// Refuses on an unknown session or a revision the session has not reached.
    pub fn replay(
        &self,
        session_id: SessionId,
        after_revision: u64,
    ) -> Result<Replay, EventRefusal> {
        self.replay_for(session_id, None, after_revision)
    }

    /// Replay for one attached lease, including a pinned terminal event that
    /// this lease has not observed yet.
    ///
    /// This is the reconnect/recovery path for a lease whose terminal frame was
    /// handed to the transport but not acknowledged; a lease that is still
    /// holding the frame in its queue does not receive a second copy.
    ///
    /// # Errors
    ///
    /// Refuses on an unknown session or lease, or a revision the session has not
    /// reached.
    pub fn replay_for_lease(
        &self,
        session_id: SessionId,
        attach_id: AttachId,
        after_revision: u64,
    ) -> Result<Replay, EventRefusal> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if !session.leases.contains_key(&attach_id) {
            return Err(EventRefusal::NotAttached);
        }
        self.replay_for(session_id, Some(attach_id), after_revision)
    }

    /// The events currently queued for one lease, in order.
    ///
    /// The transport (bitty#2607) drains a lease queue and writes each event;
    /// this method is the handoff point, so a drained event counts as delivered.
    ///
    /// # Errors
    ///
    /// Refuses when the session or lease is unknown.
    pub fn take_deliveries(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
    ) -> Result<Vec<Event>, EventRefusal> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        let lease = session
            .leases
            .get_mut(&attach_id)
            .ok_or(EventRefusal::NotAttached)?;
        Ok(lease.queue.drain())
    }

    /// Collect retained terminal records and their stream state (`D-04`,
    /// `D-14`).
    ///
    /// Collection is the bound on a pinned terminal event that no consumer ever
    /// acknowledges: the record, the slot, and every lease record go away
    /// together. Every still-attached lease is reported as `TerminalEventLost`
    /// in [`RetentionReport::retired`], so the transport is never dropped
    /// silently.
    pub fn collect_retained(&mut self, now: MonotonicMs) -> RetentionReport {
        let collected = self.registry.collect_retained(now);
        let mut retired = Vec::with_capacity(collected.len());
        for session_id in &collected {
            if let Some(session) = self.sessions.remove(session_id) {
                retired.push(RetiredSession {
                    session_id: *session_id,
                    closed: retired_closes(&session),
                });
            }
        }
        retired.extend(self.prune_unresolved());
        RetentionReport { collected, retired }
    }

    /// Drain terminal sessions pruned by the defensive `create` sweep.
    ///
    /// `terminate` and `collect_retained` return their retired entries directly;
    /// this is the escape hatch for the documented no-op sweep in `create`.
    #[must_use]
    pub fn take_retired(&mut self) -> Vec<RetiredSession> {
        std::mem::take(&mut self.pending_retired)
    }

    /// Live leases on one session.
    #[must_use]
    pub fn lease_count(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .get(&session_id)
            .map(|session| session.leases.len())
    }

    /// Queued events for one lease.
    #[must_use]
    pub fn lease_queue_len(&self, session_id: SessionId, attach_id: AttachId) -> Option<usize> {
        self.lease(session_id, attach_id)
            .map(|lease| lease.queue.len())
    }

    /// Queued event bytes for one lease.
    #[must_use]
    pub fn lease_queue_bytes(&self, session_id: SessionId, attach_id: AttachId) -> Option<usize> {
        self.lease(session_id, attach_id)
            .map(|lease| lease.queue.bytes())
    }

    /// Retained backlog events for one session.
    #[must_use]
    pub fn backlog_len(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .get(&session_id)
            .map(|session| session.backlog.len())
    }

    /// Retained backlog bytes for one session.
    #[must_use]
    pub fn backlog_bytes(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .get(&session_id)
            .map(|session| session.backlog.bytes())
    }

    /// Leases that have not yet observed the pinned terminal event.
    #[must_use]
    pub fn pending_terminal_consumers(&self, session_id: SessionId) -> Option<usize> {
        self.sessions.get(&session_id).map(|session| {
            session
                .terminal
                .as_ref()
                .map_or(0, |slot| slot.pending.len())
        })
    }

    /// Whether one lease has already observed the terminal event.
    #[must_use]
    pub fn terminal_observed(&self, session_id: SessionId, attach_id: AttachId) -> bool {
        self.lease(session_id, attach_id)
            .is_some_and(|lease| lease.terminal_observed)
    }

    /// Stored attach keys for one session.
    #[must_use]
    pub fn attach_key_count(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .get(&session_id)
            .map(|session| session.attach_keys.len())
    }

    /// Accounted bytes for one session's attach-key store.
    #[must_use]
    pub fn attach_key_bytes(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .get(&session_id)
            .map(SessionStream::attach_keys_bytes)
    }

    /// Accounted stream bytes for one session: the ring, every lease queue, the
    /// fixed per-lease and per-key records, and the terminal slot event.
    ///
    /// Deterministic for a given state, so a heartbeat that allocates nothing
    /// leaves the number unchanged. It is an accounting estimate for the
    /// `D-14` bounds, not an allocator measurement.
    #[must_use]
    pub fn accounted_bytes(&self, session_id: SessionId) -> Option<usize> {
        let session = self.sessions.get(&session_id)?;
        let mut bytes = std::mem::size_of::<SessionStream>()
            + session.backlog.bytes()
            + session.attach_keys_bytes();
        for lease in session.leases.values() {
            bytes += std::mem::size_of::<Lease>() + lease.queue.bytes();
        }
        if let Some(slot) = session.terminal.as_ref() {
            bytes += std::mem::size_of::<TerminalSlot>()
                + slot.event.accounted_bytes()
                + slot.pending.len() * std::mem::size_of::<AttachId>();
        }
        Some(bytes)
    }

    fn lease(&self, session_id: SessionId, attach_id: AttachId) -> Option<&Lease> {
        self.sessions
            .get(&session_id)
            .and_then(|session| session.leases.get(&attach_id))
    }

    fn current_snapshot(&self, session_id: SessionId) -> Result<SessionSnapshot, EventRefusal> {
        self.registry
            .snapshot(session_id)
            .ok_or(EventRefusal::UnknownSession)
    }

    fn mutation(
        &mut self,
        session_id: SessionId,
        revision: u64,
        now: MonotonicMs,
    ) -> Result<MutationReport, EventRefusal> {
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.append(
                revision,
                EventKind::SessionState,
                None,
                EventDetail::None,
                now,
            )?
        };
        let delivered = self.deliver(session_id, event.clone(), now)?;
        Ok(MutationReport {
            snapshot: self.current_snapshot(session_id)?,
            event,
            closed: delivered.closed,
        })
    }

    /// Drop terminal stream entries the registry no longer resolves.
    ///
    /// Only a terminal entry can disappear from the registry without a stream
    /// call: the retention cap evicts during `terminate`, and TTL collection
    /// removes records directly. A live entry that is missing is an invariant
    /// failure, not a retirement, and is left for the next operation to refuse.
    fn prune_unresolved(&mut self) -> Vec<RetiredSession> {
        let stale: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(session_id, session)| {
                session.terminalized && self.registry.snapshot(**session_id).is_none()
            })
            .map(|(session_id, _)| *session_id)
            .collect();
        let mut retired = Vec::with_capacity(stale.len());
        for session_id in stale {
            if let Some(session) = self.sessions.remove(&session_id) {
                retired.push(RetiredSession {
                    session_id,
                    closed: retired_closes(&session),
                });
            }
        }
        retired
    }

    fn replay_for(
        &self,
        session_id: SessionId,
        attach_id: Option<AttachId>,
        after_revision: u64,
    ) -> Result<Replay, EventRefusal> {
        let snapshot = self.current_snapshot(session_id)?;
        if after_revision > snapshot.state_revision {
            return Err(EventRefusal::RevisionAhead {
                requested: after_revision,
                current: snapshot.state_revision,
            });
        }
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(EventRefusal::UnknownSession)?;
        if session.revision != snapshot.state_revision {
            return Err(EventRefusal::RevisionGap {
                expected: snapshot.state_revision,
                observed: session.revision,
            });
        }

        let mut events = session.backlog.after(after_revision);
        if let Some(attach_id) = attach_id {
            for event in &mut events {
                event.attach_id = Some(attach_id);
            }
        }
        let earliest = session.backlog.earliest();
        let backlog_truncated = match earliest {
            Some(front) => after_revision.saturating_add(1) < front,
            None => session.terminalized && after_revision < session.revision,
        };
        let earliest_revision = earliest.or_else(|| {
            session
                .terminalized
                .then_some(session.revision)
                .filter(|_| after_revision < session.revision)
        });

        let terminal_event = session.terminal.as_ref().and_then(|slot| {
            let attach_id = attach_id?;
            if !slot.pending.contains(&attach_id) {
                return None;
            }
            let queued = session.leases.get(&attach_id).is_some_and(|lease| {
                lease
                    .queue
                    .contains(slot.event.revision, EventKind::SessionTerminal)
            });
            if queued {
                // Still in the lease queue: the queue is the single delivery.
                None
            } else {
                Some(slot.event.routed_to(attach_id))
            }
        });

        Ok(Replay {
            snapshot,
            events,
            backlog_truncated,
            earliest_revision,
            terminal_event,
        })
    }

    /// Apply one canonical event to every attached lease, closing any lease
    /// whose control queue is full (`D-10`).
    fn deliver(
        &mut self,
        session_id: SessionId,
        event: Event,
        now: MonotonicMs,
    ) -> Result<Delivered, EventRefusal> {
        let mut pending = VecDeque::new();
        pending.push_back((event, true));
        self.drain_pending_with_recipients(session_id, pending, now)
    }

    fn drain_pending_with_recipients(
        &mut self,
        session_id: SessionId,
        mut pending: VecDeque<(Event, bool)>,
        now: MonotonicMs,
    ) -> Result<Delivered, EventRefusal> {
        let mut recipients = Vec::new();
        let mut closed = Vec::new();
        let mut steps = 0usize;
        let max_steps = self.limits.max_attaches_per_session() * 2 + 4;
        while let Some((event, count_recipients)) = pending.pop_front() {
            steps += 1;
            if steps > max_steps {
                return Err(EventRefusal::DeliveryLoop { steps });
            }
            let lease_ids: Vec<AttachId> = {
                let session = self
                    .sessions
                    .get(&session_id)
                    .ok_or(EventRefusal::UnknownSession)?;
                session.leases.keys().copied().collect()
            };
            for attach_id in lease_ids {
                let accepted = {
                    let session = self
                        .sessions
                        .get_mut(&session_id)
                        .ok_or(EventRefusal::UnknownSession)?;
                    let Some(lease) = session.leases.get_mut(&attach_id) else {
                        continue;
                    };
                    lease.queue.admit(
                        event.routed_to(attach_id),
                        FrameClass::Control,
                        &self.limits,
                    )
                };
                match accepted {
                    Admission::Accepted => {
                        if count_recipients {
                            recipients.push(attach_id);
                        }
                    }
                    Admission::Lost | Admission::Shed => {
                        self.close_with_loss(
                            session_id,
                            attach_id,
                            now,
                            &mut closed,
                            &mut pending,
                        )?;
                    }
                }
            }
        }
        Ok(Delivered { recipients, closed })
    }

    /// Close one lease with `TerminalEventLost` and announce the detach when the
    /// session is still live.
    fn close_with_loss(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
        now: MonotonicMs,
        closed: &mut Vec<LeaseClose>,
        pending: &mut VecDeque<(Event, bool)>,
    ) -> Result<(), EventRefusal> {
        let terminalized = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            if !session.leases.contains_key(&attach_id) {
                return Err(EventRefusal::NotAttached);
            }
            session.remove_lease(attach_id);
            if let Some(slot) = session.terminal.as_mut() {
                slot.pending.remove(&attach_id);
                if slot.pending.is_empty() {
                    session.terminal = None;
                }
            }
            session.terminalized
        };
        closed.push(LeaseClose {
            attach_id,
            reason: LeaseCloseReason::TerminalEventLost,
        });
        if terminalized {
            // A retained terminal record is read-only; the lease is only stream
            // state and its guarantee is discharged by the typed close.
            return Ok(());
        }
        let snapshot = self.registry.detach(session_id, now)?;
        let event = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(EventRefusal::UnknownSession)?;
            session.append(
                snapshot.state_revision,
                EventKind::SessionState,
                None,
                EventDetail::None,
                now,
            )?
        };
        pending.push_back((event, false));
        Ok(())
    }
}

/// Every lease still attached when its session record was retired. The
/// terminal record is gone, so each lease gets the typed close instead of being
/// dropped with the map entry.
fn retired_closes(session: &SessionStream) -> Vec<LeaseClose> {
    session
        .leases
        .keys()
        .map(|attach_id| LeaseClose {
            attach_id: *attach_id,
            reason: LeaseCloseReason::TerminalEventLost,
        })
        .collect()
}

/// The leases that accepted one canonical event.
#[derive(Debug)]
struct Delivered {
    recipients: Vec<AttachId>,
    closed: Vec<LeaseClose>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CreateSession, IdError, RegistryLimits, TerminalId};

    struct FixedEntropy(u8);

    impl Entropy for FixedEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            destination.fill(self.0);
            Ok(())
        }
    }

    struct SequenceEntropy(VecDeque<u8>);

    impl Entropy for SequenceEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            destination.fill(self.0.pop_front().unwrap_or(0xaa));
            Ok(())
        }
    }

    fn registry_limits() -> RegistryLimits {
        RegistryLimits::new(4, 4, 4096, 600_000).expect("registry limits")
    }

    fn created(stream: &mut SessionEventStream) -> SessionSnapshot {
        stream
            .create(
                CreateSession {
                    terminal_id: Some(
                        TerminalId::supplied("term-collision").expect("valid terminal id"),
                    ),
                    owner_peer: None,
                },
                MonotonicMs::from_millis(1),
            )
            .expect("create")
    }

    fn attach(stream: &mut SessionEventStream, session_id: SessionId, now: u64) -> AttachOutcome {
        stream
            .attach(
                AttachRequest {
                    session_id,
                    after_revision: 0,
                    idempotency_key: None,
                },
                MonotonicMs::from_millis(now),
            )
            .expect("attach")
    }

    #[test]
    fn a_live_attach_id_collision_is_retried_then_refused() {
        let registry = SessionRegistry::new(registry_limits()).expect("registry");
        let mut stream = SessionEventStream::with_entropy(
            registry,
            EventLimits::default(),
            Box::new(FixedEntropy(0x11)),
        )
        .expect("empty registry");
        let snapshot = created(&mut stream);
        let first = attach(&mut stream, snapshot.session_id, 2);
        assert_eq!(first.attach_id.as_hex(), "1".repeat(32));

        assert_eq!(
            stream
                .attach(
                    AttachRequest {
                        session_id: snapshot.session_id,
                        after_revision: 1,
                        idempotency_key: None,
                    },
                    MonotonicMs::from_millis(3),
                )
                .unwrap_err(),
            EventRefusal::IdCollision
        );
        assert_eq!(stream.lease_count(snapshot.session_id), Some(1));
        assert_eq!(
            stream
                .registry()
                .snapshot(snapshot.session_id)
                .expect("snapshot")
                .attach_count,
            1
        );
    }

    #[test]
    fn a_stored_key_reserves_its_attach_id_after_the_lease_is_gone() {
        let registry = SessionRegistry::new(registry_limits()).expect("registry");
        let mut stream = SessionEventStream::with_entropy(
            registry,
            EventLimits::default(),
            Box::new(FixedEntropy(0x11)),
        )
        .expect("empty registry");
        let snapshot = created(&mut stream);
        let first = stream
            .attach(
                AttachRequest {
                    session_id: snapshot.session_id,
                    after_revision: 0,
                    idempotency_key: Some(AttachKey::new(b"k1").expect("key")),
                },
                MonotonicMs::from_millis(2),
            )
            .expect("attach k1");
        stream
            .detach(
                snapshot.session_id,
                first.attach_id,
                DetachCause::SelfDetach,
                MonotonicMs::from_millis(3),
            )
            .expect("detach");
        let revision = stream
            .registry()
            .snapshot(snapshot.session_id)
            .expect("snapshot")
            .state_revision;

        assert_eq!(
            stream
                .attach(
                    AttachRequest {
                        session_id: snapshot.session_id,
                        after_revision: revision,
                        idempotency_key: Some(AttachKey::new(b"k2").expect("key")),
                    },
                    MonotonicMs::from_millis(4),
                )
                .unwrap_err(),
            EventRefusal::IdCollision
        );
        assert_eq!(stream.lease_count(snapshot.session_id), Some(0));
        assert_eq!(
            stream
                .registry()
                .snapshot(snapshot.session_id)
                .expect("snapshot")
                .attach_count,
            0
        );
        assert_eq!(
            stream
                .attach(
                    AttachRequest {
                        session_id: snapshot.session_id,
                        after_revision: 0,
                        idempotency_key: Some(AttachKey::new(b"k1").expect("key")),
                    },
                    MonotonicMs::from_millis(5),
                )
                .unwrap_err(),
            EventRefusal::NotAttached
        );
    }

    #[test]
    fn a_collision_within_the_retry_bound_mints_a_fresh_lease() {
        let registry = SessionRegistry::new(registry_limits()).expect("registry");
        let mut entropy = VecDeque::new();
        entropy.push_back(0x11);
        entropy.push_back(0x11);
        entropy.push_back(0x22);
        let mut stream = SessionEventStream::with_entropy(
            registry,
            EventLimits::default(),
            Box::new(SequenceEntropy(entropy)),
        )
        .expect("empty registry");
        let snapshot = created(&mut stream);
        let first = attach(&mut stream, snapshot.session_id, 2);
        let second = attach(&mut stream, snapshot.session_id, 3);
        assert_ne!(first.attach_id, second.attach_id);
        assert_eq!(stream.lease_count(snapshot.session_id), Some(2));
        assert_eq!(
            stream
                .registry()
                .snapshot(snapshot.session_id)
                .expect("snapshot")
                .attach_count,
            2
        );
    }
}
