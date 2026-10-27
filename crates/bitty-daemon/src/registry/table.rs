//! The in-memory session registry: lifecycle mutations, counters, retention,
//! and explicit refusals (contract `D-02`, `D-03`, `D-04`, `D-14`).
//!
//! Ownership is explicit: one [`SessionRegistry`] belongs to the daemon
//! instance that created it. There is no global, no static, and no client-
//! supplied identity: session ids are minted here, `terminal_id` is immutable,
//! and every mutation validates before it changes one record. A refusal leaves
//! the registry exactly as it was.
//!
//! The registry holds no listener, spawns no process, and touches no file.
//! Leases drive the derived attach count through [`SessionRegistry::attach`]
//! and [`SessionRegistry::detach`] (bitty#2609 owns lease authority); the IPC
//! service (bitty#2607) owns request decoding and error mapping; platform
//! cells (bitty#2604) own process containment.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::id::{DaemonInstanceId, Entropy, ID_GENERATION_ATTEMPTS, OsEntropy, SessionId};
use super::session::{
    CleanupClass, LifecycleState, MonotonicMs, OwnerPeer, SessionRecord, SessionSnapshot,
    TerminalId, TerminalOutcome, TerminalReason,
};
use crate::limits::{LimitId, Limits};

/// Default live-session bound (`D-14` `max_sessions`).
pub const MAX_SESSIONS_DEFAULT: usize = LimitId::MaxSessions.default_value() as usize;

/// Compiled ceiling for live sessions (`D-14` `max_sessions` system maximum).
pub const MAX_SESSIONS_SYSTEM_MAX: usize = LimitId::MaxSessions.system_max_value() as usize;

/// Default and compiled ceiling for retained terminal records
/// (`D-14` `max_retained_terminal_records`).
pub const MAX_RETAINED_TERMINAL_RECORDS_SYSTEM_MAX: usize =
    LimitId::MaxRetainedTerminalRecords.system_max_value() as usize;

/// Default and compiled ceiling for one session's metadata
/// (`D-14` `session_record_bytes`).
pub const SESSION_RECORD_BYTES_SYSTEM_MAX: usize =
    LimitId::SessionRecordBytes.system_max_value() as usize;

/// Default terminal-record retention (`D-14` `terminal_retention_ms`).
pub const TERMINAL_RETENTION_MS_DEFAULT: u64 = LimitId::TerminalRetentionMs.default_value();

/// Compiled ceiling for terminal-record retention
/// (`D-14` `terminal_retention_ms` system maximum).
pub const TERMINAL_RETENTION_MS_SYSTEM_MAX: u64 = LimitId::TerminalRetentionMs.system_max_value();

/// Enforced registry bounds for one daemon instance.
///
/// The service layer derives these from the effective [`Limits`] with
/// [`Self::from_limits`]; the compiled `{effective, default, system_max}`
/// triples are advertised by `crate::limits`. Construction refuses a value
/// above the compiled ceiling (`D-14`): a configuration mistake cannot raise a
/// bound past the reviewed maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryLimits {
    max_sessions: usize,
    max_retained_terminal_records: usize,
    session_record_bytes: usize,
    terminal_retention_ms: u64,
}

impl RegistryLimits {
    /// Validate and build enforced limits.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryRefusal::InvalidLimits`] when a value exceeds its
    /// compiled system maximum.
    pub fn new(
        max_sessions: usize,
        max_retained_terminal_records: usize,
        session_record_bytes: usize,
        terminal_retention_ms: u64,
    ) -> Result<Self, RegistryRefusal> {
        check_limit("max_sessions", max_sessions, MAX_SESSIONS_SYSTEM_MAX)?;
        check_limit(
            "max_retained_terminal_records",
            max_retained_terminal_records,
            MAX_RETAINED_TERMINAL_RECORDS_SYSTEM_MAX,
        )?;
        check_limit(
            "session_record_bytes",
            session_record_bytes,
            SESSION_RECORD_BYTES_SYSTEM_MAX,
        )?;
        if terminal_retention_ms > TERMINAL_RETENTION_MS_SYSTEM_MAX {
            return Err(RegistryRefusal::InvalidLimits {
                limit: "terminal_retention_ms",
                value: terminal_retention_ms,
                maximum: TERMINAL_RETENTION_MS_SYSTEM_MAX,
            });
        }
        Ok(Self {
            max_sessions,
            max_retained_terminal_records,
            session_record_bytes,
            terminal_retention_ms,
        })
    }

    /// Live-session bound.
    #[must_use]
    pub const fn max_sessions(self) -> usize {
        self.max_sessions
    }

    /// Retained terminal-record bound.
    #[must_use]
    pub const fn max_retained_terminal_records(self) -> usize {
        self.max_retained_terminal_records
    }

    /// Per-record metadata byte bound.
    #[must_use]
    pub const fn session_record_bytes(self) -> usize {
        self.session_record_bytes
    }

    /// Terminal-record retention in milliseconds.
    #[must_use]
    pub const fn terminal_retention_ms(self) -> u64 {
        self.terminal_retention_ms
    }

    /// Derive the registry bounds from the effective advertised limits.
    ///
    /// [`Limits`] has already validated every effective value against its
    /// compiled floor and system maximum, so this constructor cannot produce
    /// an out-of-range value and does not re-run the refusal path.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            max_sessions: limits.effective_usize(LimitId::MaxSessions),
            max_retained_terminal_records: limits
                .effective_usize(LimitId::MaxRetainedTerminalRecords),
            session_record_bytes: limits.effective_usize(LimitId::SessionRecordBytes),
            terminal_retention_ms: limits.effective(LimitId::TerminalRetentionMs),
        }
    }
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self {
            max_sessions: MAX_SESSIONS_DEFAULT,
            max_retained_terminal_records: MAX_RETAINED_TERMINAL_RECORDS_SYSTEM_MAX,
            session_record_bytes: SESSION_RECORD_BYTES_SYSTEM_MAX,
            terminal_retention_ms: TERMINAL_RETENTION_MS_DEFAULT,
        }
    }
}

fn check_limit(limit: &'static str, value: usize, maximum: usize) -> Result<(), RegistryRefusal> {
    if value > maximum {
        return Err(RegistryRefusal::InvalidLimits {
            limit,
            value: u64::try_from(value).unwrap_or(u64::MAX),
            maximum: u64::try_from(maximum).unwrap_or(u64::MAX),
        });
    }
    Ok(())
}

/// A create request for the registry.
///
/// The service layer validates scopes, consent, and the spawn specification
/// before it builds this value. `terminal_id` is either a validated client
/// value or absent, in which case the registry assigns one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateSession {
    /// Client-supplied terminal identity, or `None` for a server-assigned one.
    pub terminal_id: Option<TerminalId>,
    /// Opaque authenticated owner facts captured by the IPC slice.
    pub owner_peer: Option<OwnerPeer>,
}

/// A refused registry operation. Every variant is a whole-refusal: no record
/// was inserted, removed, or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryRefusal {
    /// `max_sessions` live records already exist.
    MaxSessions {
        /// The enforced bound.
        limit: usize,
    },
    /// The record would exceed `session_record_bytes`.
    SessionRecordTooLarge {
        /// Accounted record bytes.
        bytes: usize,
        /// The enforced bound.
        limit: usize,
    },
    /// A non-terminal session already hosts this `terminal_id`.
    TerminalAlreadyHosted,
    /// No record exists in this instance; it is unknown or collected.
    NotFound,
    /// The session is terminal and therefore read-only (`Denied/NotRunning`).
    NotRunning {
        /// The immutable terminal state.
        state: LifecycleState,
    },
    /// A worker already bound; v0.2.0 permits one bind.
    AlreadyBound {
        /// The current generation.
        generation: u64,
    },
    /// The lifecycle transition is not in the `D-04` table.
    InvalidTransition {
        /// Current state.
        from: LifecycleState,
        /// Refused target state.
        to: LifecycleState,
    },
    /// The terminal reason cannot originate in the current state.
    ReasonNotAllowed {
        /// Requested terminal reason.
        reason: TerminalReason,
        /// Current state.
        from: LifecycleState,
    },
    /// `not_started` cleanup was claimed after a worker could have spawned.
    CleanupNotStarted {
        /// Current state; only `created` may use `not_started`.
        from: LifecycleState,
    },
    /// The state-revision counter would wrap; the registry fails closed.
    RevisionOverflow,
    /// The generation counter would wrap; the registry fails closed.
    GenerationOverflow,
    /// The attach counter would wrap; the registry fails closed.
    AttachmentOverflow,
    /// The OS CSPRNG was unavailable while minting a server-side id.
    IdGenerationFailed,
    /// Fresh random ids repeatedly collided with resolvable records.
    IdCollision,
    /// A configured bound exceeds the reviewed system maximum.
    InvalidLimits {
        /// Limit name.
        limit: &'static str,
        /// Requested value.
        value: u64,
        /// Compiled ceiling.
        maximum: u64,
    },
}

impl fmt::Display for RegistryRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxSessions { limit } => {
                write!(formatter, "live session limit {limit} reached")
            }
            Self::SessionRecordTooLarge { bytes, limit } => {
                write!(
                    formatter,
                    "session record is {bytes} bytes, above limit {limit}"
                )
            }
            Self::TerminalAlreadyHosted => {
                formatter.write_str("terminal id is already hosted by a live session")
            }
            Self::NotFound => formatter.write_str("session not found in this instance"),
            Self::NotRunning { state } => {
                write!(formatter, "session is terminal in state {state}")
            }
            Self::AlreadyBound { generation } => {
                write!(
                    formatter,
                    "session is already bound at generation {generation}"
                )
            }
            Self::InvalidTransition { from, to } => {
                write!(
                    formatter,
                    "lifecycle transition {from} -> {to} is not allowed"
                )
            }
            Self::ReasonNotAllowed { reason, from } => {
                write!(
                    formatter,
                    "reason {reason} cannot originate in state {from}"
                )
            }
            Self::CleanupNotStarted { from } => {
                write!(
                    formatter,
                    "not_started cleanup is invalid from state {from}"
                )
            }
            Self::RevisionOverflow => formatter.write_str("state revision would wrap"),
            Self::GenerationOverflow => formatter.write_str("generation would wrap"),
            Self::AttachmentOverflow => formatter.write_str("attach count would wrap"),
            Self::IdGenerationFailed => formatter.write_str("server-side id generation failed"),
            Self::IdCollision => formatter.write_str("server-side id generation kept colliding"),
            Self::InvalidLimits {
                limit,
                value,
                maximum,
            } => write!(
                formatter,
                "limit {limit} is {value}, above compiled maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for RegistryRefusal {}

/// Session counts for `daemon.status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegistryCounts {
    /// Records in `created`.
    pub created: usize,
    /// Records in `running`.
    pub running: usize,
    /// Records in `stopping`.
    pub stopping: usize,
    /// Records in `exited`.
    pub exited: usize,
    /// Records in `failed`.
    pub failed: usize,
    /// Records in `terminated`.
    pub terminated: usize,
    /// Live (non-terminal) records.
    pub live: usize,
    /// Retained terminal records.
    pub retained: usize,
}

impl RegistryCounts {
    /// Every record, live plus retained.
    #[must_use]
    pub const fn total(self) -> usize {
        self.live + self.retained
    }

    fn observe(&mut self, state: LifecycleState) {
        match state {
            LifecycleState::Created => self.created += 1,
            LifecycleState::Running => self.running += 1,
            LifecycleState::Stopping => self.stopping += 1,
            LifecycleState::Exited => self.exited += 1,
            LifecycleState::Failed => self.failed += 1,
            LifecycleState::Terminated => self.terminated += 1,
        }
    }
}

/// One daemon instance's session registry.
///
/// Live records and retained terminal records are separate maps, so a terminal
/// session is never counted as live, never blocks capacity, and can never be
/// revived. Terminal retention is bounded by both a TTL and a record cap;
/// collection order is `(terminalized_at, session_id)`, which makes the result
/// deterministic when several records share a timestamp.
pub struct SessionRegistry {
    instance_id: DaemonInstanceId,
    limits: RegistryLimits,
    entropy: Box<dyn Entropy + Send>,
    live: BTreeMap<SessionId, SessionRecord>,
    retained: BTreeMap<SessionId, SessionRecord>,
    retention_order: BTreeSet<(MonotonicMs, SessionId)>,
}

impl fmt::Debug for SessionRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRegistry")
            .field("instance_id", &self.instance_id)
            .field("limits", &self.limits)
            .field("live", &self.live.len())
            .field("retained", &self.retained.len())
            .finish()
    }
}

impl SessionRegistry {
    /// Create an empty registry with a fresh instance id from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryRefusal::IdGenerationFailed`] when the OS CSPRNG is
    /// unavailable; the daemon must not start with a fallback identity.
    pub fn new(limits: RegistryLimits) -> Result<Self, RegistryRefusal> {
        Self::with_entropy(limits, Box::new(OsEntropy))
    }

    pub(crate) fn with_entropy(
        limits: RegistryLimits,
        mut entropy: Box<dyn Entropy + Send>,
    ) -> Result<Self, RegistryRefusal> {
        let instance_id = DaemonInstanceId::generate_with(entropy.as_mut())
            .map_err(|_| RegistryRefusal::IdGenerationFailed)?;
        Ok(Self {
            instance_id,
            limits,
            entropy,
            live: BTreeMap::new(),
            retained: BTreeMap::new(),
            retention_order: BTreeSet::new(),
        })
    }

    /// This instance's immutable identity (`D-02`).
    #[must_use]
    pub const fn instance_id(&self) -> DaemonInstanceId {
        self.instance_id
    }

    /// The enforced bounds.
    #[must_use]
    pub const fn limits(&self) -> RegistryLimits {
        self.limits
    }

    /// Live (non-terminal) records.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Retained terminal records.
    #[must_use]
    pub fn retained_count(&self) -> usize {
        self.retained.len()
    }

    /// Counts by state, including retained terminal records.
    #[must_use]
    pub fn counts(&self) -> RegistryCounts {
        let mut counts = RegistryCounts::default();
        for record in self.live.values().chain(self.retained.values()) {
            counts.observe(record.state);
        }
        counts.live = self.live.len();
        counts.retained = self.retained.len();
        counts
    }

    /// A snapshot for a live or retained session.
    #[must_use]
    pub fn snapshot(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.live
            .get(&session_id)
            .or_else(|| self.retained.get(&session_id))
            .map(SessionRecord::snapshot)
    }

    /// Every live snapshot followed by every retained terminal snapshot, in
    /// session-id order.
    ///
    /// Bounded by `max_sessions + max_retained_terminal_records` (128 at the
    /// compiled maxima); `daemon.inspect` and `daemon.status` use this read
    /// surface. Authorization and serialization stay in the IPC slice.
    pub fn snapshots(&self) -> impl Iterator<Item = SessionSnapshot> + '_ {
        self.live
            .values()
            .chain(self.retained.values())
            .map(SessionRecord::snapshot)
    }

    /// Register a new terminal session in `created` at generation and revision
    /// `0`.
    ///
    /// The live-session bound is checked first, before any identity work. Once
    /// capacity admits the request, a duplicate live `terminal_id` is refused
    /// before a session id is minted or a record is inserted.
    ///
    /// # Errors
    ///
    /// Refuses whole, leaving no partial record, when the live-session bound is
    /// reached, the record exceeds `session_record_bytes`, the terminal id is
    /// already hosted by a live session, or server-side id generation fails.
    pub fn create(
        &mut self,
        request: CreateSession,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if self.live.len() >= self.limits.max_sessions {
            return Err(RegistryRefusal::MaxSessions {
                limit: self.limits.max_sessions,
            });
        }

        let terminal_id = match request.terminal_id {
            Some(terminal_id) => {
                if self
                    .live
                    .values()
                    .any(|record| record.terminal_id() == &terminal_id)
                {
                    return Err(RegistryRefusal::TerminalAlreadyHosted);
                }
                terminal_id
            }
            None => self.generate_terminal_id()?,
        };

        let session_id = self.generate_session_id()?;
        let record = SessionRecord::new(session_id, terminal_id, request.owner_peer, now);
        let bytes = record.record_bytes();
        if bytes > self.limits.session_record_bytes {
            return Err(RegistryRefusal::SessionRecordTooLarge {
                bytes,
                limit: self.limits.session_record_bytes,
            });
        }

        let snapshot = record.snapshot();
        self.live.insert(session_id, record);
        Ok(snapshot)
    }

    /// Bind the first (and, in v0.2.0, only) worker to a `created` session.
    ///
    /// Increments `generation` and `state_revision` exactly once and moves the
    /// record to `running`.
    ///
    /// # Errors
    ///
    /// Refuses when the session is unknown, already bound, terminal, or a
    /// counter would wrap.
    pub fn bind(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");

        if record.state != LifecycleState::Created {
            return Err(if record.generation > 0 {
                RegistryRefusal::AlreadyBound {
                    generation: record.generation,
                }
            } else {
                RegistryRefusal::InvalidTransition {
                    from: record.state,
                    to: LifecycleState::Running,
                }
            });
        }

        let generation = record
            .generation
            .checked_add(1)
            .ok_or(RegistryRefusal::GenerationOverflow)?;
        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;

        record.generation = generation;
        record.state_revision = revision;
        record.state = LifecycleState::Running;
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Record one live lease (bitty#2609 calls this after authorization).
    ///
    /// The derived attach count rises by one and the revision increments once.
    ///
    /// # Errors
    ///
    /// Refuses on an unknown or terminal session, or when a counter would wrap.
    pub fn attach(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");
        if record.state.is_terminal() {
            return Err(RegistryRefusal::NotRunning {
                state: record.state,
            });
        }

        let attach_count = record
            .attach_count
            .checked_add(1)
            .ok_or(RegistryRefusal::AttachmentOverflow)?;
        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;

        record.attach_count = attach_count;
        record.state_revision = revision;
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Record one lease going away (bitty#2609 calls this after authorization).
    ///
    /// The revision increments only when the count actually changes; a repeat
    /// with no live lease is idempotent and does not bump it.
    ///
    /// # Errors
    ///
    /// Refuses on an unknown or terminal session, or when the revision would
    /// wrap.
    pub fn detach(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");
        if record.state.is_terminal() {
            return Err(RegistryRefusal::NotRunning {
                state: record.state,
            });
        }

        if record.attach_count == 0 {
            record.last_seen = now;
            return Ok(record.snapshot());
        }

        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;
        record.attach_count -= 1;
        record.state_revision = revision;
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Refresh `last_seen` without touching `state_revision` (`D-03`, `D-12`).
    ///
    /// # Errors
    ///
    /// Refuses on an unknown or terminal session.
    pub fn heartbeat(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");
        if record.state.is_terminal() {
            return Err(RegistryRefusal::NotRunning {
                state: record.state,
            });
        }
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Record one snapshot-visible mutation owned by another layer (`D-03`).
    ///
    /// `D-03` lists control claim, control transfer, control vacancy, controller
    /// resize, observer-grant mint/consume/revoke, and lease revocation as
    /// revision bumps. Their authority lives in the lease layer (bitty#2609)
    /// while the counter belongs to the session record, so the lease layer
    /// applies the mutation and then records the single revision bump here.
    /// Only `state_revision` and `last_seen` change: no lifecycle state, no
    /// generation, no attach count, and no terminal field.
    ///
    /// # Errors
    ///
    /// Refuses on an unknown or terminal session, or when the revision would
    /// wrap; a refusal leaves the record unchanged.
    pub fn note_visible_mutation(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");
        if record.state.is_terminal() {
            return Err(RegistryRefusal::NotRunning {
                state: record.state,
            });
        }
        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;
        record.state_revision = revision;
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Accept a graceful stop: `running` -> `stopping`, one revision bump.
    ///
    /// # Errors
    ///
    /// Refuses when the session is not `running`; a `created` session is
    /// terminated directly (a pre-bind cancel), and a terminal session is
    /// read-only.
    pub fn begin_stopping(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get_mut(&session_id).expect("checked above");
        if record.state != LifecycleState::Running {
            return Err(RegistryRefusal::InvalidTransition {
                from: record.state,
                to: LifecycleState::Stopping,
            });
        }

        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;
        record.state = LifecycleState::Stopping;
        record.state_revision = revision;
        record.last_seen = now;
        Ok(record.snapshot())
    }

    /// Record the terminal transition and move the record to retention.
    ///
    /// The revision increments exactly once; the generation never changes. The
    /// terminal record is immutable from this point and only retention removes
    /// it.
    ///
    /// # Errors
    ///
    /// Refuses when the session is unknown, already terminal, the transition is
    /// not allowed by `D-04`, or the revision would wrap.
    pub fn terminate(
        &mut self,
        session_id: SessionId,
        outcome: TerminalOutcome,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, RegistryRefusal> {
        let (target, revision) = self.check_terminal_transition(session_id, outcome)?;
        let mut record = self.live.remove(&session_id).expect("checked above");
        record.state = target;
        record.state_revision = revision;
        record.terminal_reason = Some(outcome.reason());
        record.exit = outcome.exit();
        record.cleanup = Some(outcome.cleanup());
        record.last_seen = now;
        record.terminalized_at = Some(now);
        let snapshot = record.snapshot();

        self.retained.insert(session_id, record);
        self.retention_order.insert((now, session_id));
        self.enforce_retention_cap();
        Ok(snapshot)
    }

    /// Validate a terminal transition without applying it (`D-04`).
    ///
    /// The event stream calls this before it reserves a control slot on every
    /// lease, so a transition the registry would refuse cannot have closed a
    /// lease first. Returns the target state and the revision the transition
    /// would take.
    ///
    /// # Errors
    ///
    /// Returns the same refusal [`SessionRegistry::terminate`] would: unknown or
    /// terminal session, a transition outside the `D-04` table, a reason that
    /// cannot originate in the current state, or a revision wrap.
    pub(crate) fn check_terminal_transition(
        &self,
        session_id: SessionId,
        outcome: TerminalOutcome,
    ) -> Result<(LifecycleState, u64), RegistryRefusal> {
        if !self.live.contains_key(&session_id) {
            return Err(self.lookup_refusal(session_id));
        }
        let record = self.live.get(&session_id).expect("checked above");
        let from = record.state;
        let target = outcome.reason().terminal_state();
        if !from.allows_terminal(target) {
            return Err(RegistryRefusal::InvalidTransition { from, to: target });
        }
        if !outcome.reason().can_terminate_from(from) {
            return Err(RegistryRefusal::ReasonNotAllowed {
                reason: outcome.reason(),
                from,
            });
        }
        if outcome.cleanup() == CleanupClass::NotStarted && from != LifecycleState::Created {
            return Err(RegistryRefusal::CleanupNotStarted { from });
        }
        let revision = record
            .state_revision
            .checked_add(1)
            .ok_or(RegistryRefusal::RevisionOverflow)?;
        Ok((target, revision))
    }

    /// Collect retained terminal records older than `terminal_retention_ms`.
    ///
    /// Returns the collected ids in `(terminalized_at, session_id)` order. A
    /// retained record is never live and collection never revives one.
    pub fn collect_retained(&mut self, now: MonotonicMs) -> Vec<SessionId> {
        let retention = self.limits.terminal_retention_ms;
        let mut collected = Vec::new();
        while let Some((terminalized_at, session_id)) = self.retention_order.first().copied() {
            if now.saturating_since(terminalized_at) < retention {
                break;
            }
            self.retention_order.pop_first();
            self.retained.remove(&session_id);
            collected.push(session_id);
        }
        collected
    }

    fn lookup_refusal(&self, session_id: SessionId) -> RegistryRefusal {
        self.retained
            .get(&session_id)
            .map_or(RegistryRefusal::NotFound, |record| {
                RegistryRefusal::NotRunning {
                    state: record.state,
                }
            })
    }

    fn generate_session_id(&mut self) -> Result<SessionId, RegistryRefusal> {
        // D-02: never reused within one instance. Every create mints a fresh
        // 128-bit CSPRNG value and regenerates when it collides with a
        // resolvable live or retained record. Keeping every id ever issued
        // would be an unbounded registry structure (D-14), so a collected id is
        // never intentionally recycled and a collision stays at 2^-128.
        for _ in 0..ID_GENERATION_ATTEMPTS {
            let session_id = SessionId::generate_with(self.entropy.as_mut())
                .map_err(|_| RegistryRefusal::IdGenerationFailed)?;
            if !self.live.contains_key(&session_id) && !self.retained.contains_key(&session_id) {
                return Ok(session_id);
            }
        }
        Err(RegistryRefusal::IdCollision)
    }

    fn generate_terminal_id(&mut self) -> Result<TerminalId, RegistryRefusal> {
        for _ in 0..ID_GENERATION_ATTEMPTS {
            let terminal_id = TerminalId::generate_with(self.entropy.as_mut())
                .map_err(|_| RegistryRefusal::IdGenerationFailed)?;
            if !self
                .live
                .values()
                .any(|record| record.terminal_id() == &terminal_id)
            {
                return Ok(terminal_id);
            }
        }
        Err(RegistryRefusal::IdCollision)
    }

    fn enforce_retention_cap(&mut self) {
        while self.retained.len() > self.limits.max_retained_terminal_records {
            let Some((_, session_id)) = self.retention_order.pop_first() else {
                break;
            };
            self.retained.remove(&session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::registry::id::IdError;
    use crate::registry::session::{CleanupClass, ExitInformation, SessionKind, TerminalReason};

    fn ms(value: u64) -> MonotonicMs {
        MonotonicMs::from_millis(value)
    }

    fn limits(
        max_sessions: usize,
        max_retained_terminal_records: usize,
        session_record_bytes: usize,
        terminal_retention_ms: u64,
    ) -> RegistryLimits {
        RegistryLimits::new(
            max_sessions,
            max_retained_terminal_records,
            session_record_bytes,
            terminal_retention_ms,
        )
        .expect("valid test limits")
    }

    /// Entropy that fills each call with a distinct byte, so every generated id
    /// is unique without a fixture.
    struct CountingEntropy {
        next: u8,
        fail_at: Option<usize>,
        fills: usize,
    }

    impl CountingEntropy {
        fn new() -> Self {
            Self {
                next: 1,
                fail_at: None,
                fills: 0,
            }
        }

        fn failing_at(call: usize) -> Self {
            Self {
                next: 1,
                fail_at: Some(call),
                fills: 0,
            }
        }
    }

    impl Entropy for CountingEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            let call = self.fills;
            self.fills += 1;
            if self.fail_at == Some(call) {
                return Err(IdError::RandomUnavailable);
            }
            destination.fill(self.next);
            self.next = self.next.wrapping_add(1);
            Ok(())
        }
    }

    /// Entropy that replays a byte script, then repeats `fallback`.
    struct ScriptedEntropy {
        script: VecDeque<u8>,
        fallback: u8,
        fills: usize,
    }

    impl ScriptedEntropy {
        fn new(script: impl IntoIterator<Item = u8>, fallback: u8) -> Self {
            Self {
                script: script.into_iter().collect(),
                fallback,
                fills: 0,
            }
        }
    }

    impl Entropy for ScriptedEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            self.fills += 1;
            let byte = self.script.pop_front().unwrap_or(self.fallback);
            destination.fill(byte);
            Ok(())
        }
    }

    fn registry(limits: RegistryLimits) -> SessionRegistry {
        SessionRegistry::with_entropy(limits, Box::new(CountingEntropy::new()))
            .expect("instance id generation succeeds")
    }

    fn default_registry() -> SessionRegistry {
        registry(limits(16, 64, 4096, 600_000))
    }

    fn request(terminal_id: &str) -> CreateSession {
        CreateSession {
            terminal_id: Some(TerminalId::supplied(terminal_id).expect("valid terminal id")),
            owner_peer: None,
        }
    }

    fn create(registry: &mut SessionRegistry, terminal_id: &str, now: u64) -> SessionSnapshot {
        registry
            .create(request(terminal_id), ms(now))
            .expect("create succeeds")
    }

    fn terminate(
        registry: &mut SessionRegistry,
        session_id: SessionId,
        reason: TerminalReason,
        exit: ExitInformation,
        cleanup: CleanupClass,
        now: u64,
    ) -> SessionSnapshot {
        let outcome = TerminalOutcome::new(reason, exit, cleanup).expect("valid outcome");
        registry
            .terminate(session_id, outcome, ms(now))
            .expect("terminal transition succeeds")
    }

    #[test]
    fn create_starts_at_generation_zero_and_revision_zero() {
        let mut registry = default_registry();
        let snapshot = registry
            .create(
                CreateSession {
                    terminal_id: None,
                    owner_peer: Some(OwnerPeer::new(b"uid=1000").expect("bounded")),
                },
                ms(1),
            )
            .expect("create succeeds");

        assert_eq!(snapshot.state, LifecycleState::Created);
        assert_eq!(snapshot.kind, SessionKind::Terminal);
        assert_eq!(snapshot.generation, 0);
        assert_eq!(snapshot.state_revision, 0);
        assert_eq!(snapshot.attach_count, 0);
        assert!(!snapshot.attached);
        assert_eq!(snapshot.terminal_id.as_str().len(), 32);
        assert_eq!(
            snapshot.owner_peer,
            Some(OwnerPeer::new(b"uid=1000").expect("bounded"))
        );
        assert_eq!(snapshot.last_seen_ms, ms(1));
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.retained_count(), 0);
        assert_eq!(registry.counts().created, 1);
    }

    #[test]
    fn bind_increments_generation_and_revision_exactly_once() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        let running = registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");

        assert_eq!(running.state, LifecycleState::Running);
        assert_eq!(running.generation, 1);
        assert_eq!(running.state_revision, 1);
        assert_eq!(registry.snapshot(created.session_id), Some(running));
    }

    #[test]
    fn second_bind_is_refused_without_mutation() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        let bound = registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");

        assert_eq!(
            registry.bind(created.session_id, ms(3)),
            Err(RegistryRefusal::AlreadyBound { generation: 1 })
        );
        assert_eq!(registry.snapshot(created.session_id), Some(bound));
    }

    #[test]
    fn note_visible_mutation_bumps_only_the_revision() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");
        let before = registry
            .snapshot(created.session_id)
            .expect("record exists");

        let after = registry
            .note_visible_mutation(created.session_id, ms(3))
            .expect("control mutation is recorded");
        assert_eq!(after.state_revision, before.state_revision + 1);
        assert_eq!(after.state, before.state);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.attach_count, before.attach_count);
        assert_eq!(after.last_seen_ms, ms(3));
        assert_eq!(after.terminal_reason, None);

        let terminal = terminate(
            &mut registry,
            created.session_id,
            TerminalReason::WorkerExit,
            ExitInformation::ExitCode(0),
            CleanupClass::Swept,
            4,
        );
        assert_eq!(
            registry.note_visible_mutation(created.session_id, ms(5)),
            Err(RegistryRefusal::NotRunning {
                state: terminal.state
            })
        );
        assert_eq!(registry.snapshot(created.session_id), Some(terminal));
    }

    #[test]
    fn note_visible_mutation_overflow_fails_closed() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        {
            let record = registry
                .live
                .get_mut(&created.session_id)
                .expect("record is live");
            record.state_revision = u64::MAX;
        }
        assert_eq!(
            registry.note_visible_mutation(created.session_id, ms(2)),
            Err(RegistryRefusal::RevisionOverflow)
        );
        assert_eq!(
            registry
                .snapshot(created.session_id)
                .map(|snapshot| snapshot.state_revision),
            Some(u64::MAX)
        );
    }

    #[test]
    fn note_visible_mutation_refuses_an_unknown_session() {
        let mut registry = default_registry();
        let unknown = SessionId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("canonical");
        assert_eq!(
            registry.note_visible_mutation(unknown, ms(1)),
            Err(RegistryRefusal::NotFound)
        );
    }

    #[test]
    fn heartbeat_updates_last_seen_without_bumping_revision() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        let bound = registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");
        let heartbeat = registry
            .heartbeat(created.session_id, ms(50))
            .expect("heartbeat succeeds");

        assert_eq!(heartbeat.state_revision, bound.state_revision);
        assert_eq!(heartbeat.generation, bound.generation);
        assert_eq!(heartbeat.last_seen_ms, ms(50));
        assert_eq!(registry.counts().running, 1);
    }

    #[test]
    fn attach_and_detach_bump_revision_and_derive_attached() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");

        let attached = registry
            .attach(created.session_id, ms(3))
            .expect("attach succeeds");
        assert_eq!(attached.attach_count, 1);
        assert!(attached.attached);
        assert_eq!(attached.state_revision, 2);

        let detached = registry
            .detach(created.session_id, ms(4))
            .expect("detach succeeds");
        assert_eq!(detached.attach_count, 0);
        assert!(!detached.attached);
        assert_eq!(detached.state_revision, 3);
    }

    #[test]
    fn detach_without_attach_is_idempotent_and_keeps_the_revision() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");

        let detached = registry
            .detach(created.session_id, ms(3))
            .expect("detach succeeds");
        assert_eq!(detached.attach_count, 0);
        assert_eq!(detached.state_revision, 1);
        assert_eq!(detached.last_seen_ms, ms(3));
    }

    #[test]
    fn terminal_transition_bumps_revision_once_and_keeps_generation() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");
        let stopping = registry
            .begin_stopping(created.session_id, ms(3))
            .expect("stopping succeeds");
        assert_eq!(stopping.state, LifecycleState::Stopping);
        assert_eq!(stopping.state_revision, 2);

        let terminal = terminate(
            &mut registry,
            created.session_id,
            TerminalReason::DaemonStop,
            ExitInformation::NotApplicable,
            CleanupClass::Swept,
            4,
        );
        assert_eq!(terminal.state, LifecycleState::Terminated);
        assert_eq!(terminal.generation, 1);
        assert_eq!(terminal.state_revision, 3);
        assert_eq!(terminal.terminal_reason, Some(TerminalReason::DaemonStop));
        assert_eq!(terminal.cleanup, Some(CleanupClass::Swept));
    }

    #[test]
    fn terminal_state_is_immutable_and_only_retention_removes_it() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        let terminal = terminate(
            &mut registry,
            created.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            2,
        );
        assert_eq!(terminal.state, LifecycleState::Terminated);
        assert_eq!(terminal.state_revision, 1);
        assert_eq!(terminal.generation, 0);

        let not_running = Err(RegistryRefusal::NotRunning {
            state: LifecycleState::Terminated,
        });
        assert_eq!(registry.bind(created.session_id, ms(3)), not_running);
        assert_eq!(registry.attach(created.session_id, ms(4)), not_running);
        assert_eq!(registry.detach(created.session_id, ms(5)), not_running);
        assert_eq!(registry.heartbeat(created.session_id, ms(6)), not_running);
        assert_eq!(
            registry.begin_stopping(created.session_id, ms(7)),
            not_running
        );
        assert_eq!(
            registry.terminate(
                created.session_id,
                TerminalOutcome::new(
                    TerminalReason::DaemonStop,
                    ExitInformation::NotApplicable,
                    CleanupClass::Swept
                )
                .expect("valid outcome"),
                ms(8)
            ),
            not_running
        );

        assert_eq!(registry.snapshot(created.session_id), Some(terminal));
        assert_eq!(registry.live_count(), 0);
        assert_eq!(registry.retained_count(), 1);
    }

    #[test]
    fn retained_record_is_not_live_and_cannot_be_revived() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        terminate(
            &mut registry,
            created.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            2,
        );
        assert_eq!(registry.live_count(), 0);
        assert_eq!(registry.retained_count(), 1);
        assert_eq!(
            registry
                .create(request("term"), ms(3))
                .map(|snapshot| snapshot.terminal_id),
            Ok(TerminalId::supplied("term").expect("valid"))
        );

        let collected = registry.collect_retained(ms(2 + 600_000));
        assert_eq!(collected, vec![created.session_id]);
        assert_eq!(registry.snapshot(created.session_id), None);
        assert_eq!(registry.retained_count(), 0);
        assert_eq!(registry.live_count(), 1);
    }

    #[test]
    fn terminal_id_is_unique_among_live_sessions() {
        let mut registry = default_registry();
        let first = create(&mut registry, "term", 1);
        let refusal = registry.create(request("term"), ms(2));

        assert_eq!(refusal, Err(RegistryRefusal::TerminalAlreadyHosted));
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.retained_count(), 0);
        assert_eq!(registry.counts().total(), 1);
        assert_eq!(
            registry
                .snapshot(first.session_id)
                .map(|snapshot| snapshot.terminal_id),
            Some(TerminalId::supplied("term").expect("valid"))
        );
    }

    #[test]
    fn terminal_id_can_be_reused_after_the_terminal_transition() {
        let mut registry = default_registry();
        let first = create(&mut registry, "term", 1);
        terminate(
            &mut registry,
            first.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            2,
        );

        let second = create(&mut registry, "term", 3);
        assert_ne!(first.session_id, second.session_id);
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.retained_count(), 1);
        assert_eq!(
            registry
                .snapshot(first.session_id)
                .map(|snapshot| snapshot.terminal_id),
            Some(TerminalId::supplied("term").expect("valid"))
        );
    }

    #[test]
    fn max_sessions_refusal_leaves_the_registry_unchanged() {
        let mut registry = registry(limits(1, 64, 4096, 600_000));
        let first = create(&mut registry, "first", 1);

        assert_eq!(
            registry.create(request("second"), ms(2)),
            Err(RegistryRefusal::MaxSessions { limit: 1 })
        );
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.retained_count(), 0);
        assert_eq!(registry.counts().created, 1);
        assert_eq!(
            registry
                .snapshot(first.session_id)
                .map(|snapshot| snapshot.terminal_id),
            Some(TerminalId::supplied("first").expect("valid"))
        );
    }

    #[test]
    fn record_byte_cap_refusal_leaves_the_registry_unchanged() {
        let fixed = std::mem::size_of::<SessionRecord>();
        let mut registry = registry(limits(4, 64, fixed, 600_000));

        let refusal = registry.create(request("a"), ms(1));
        assert!(matches!(
            refusal,
            Err(RegistryRefusal::SessionRecordTooLarge { .. })
        ));
        assert_eq!(registry.live_count(), 0);
        assert_eq!(registry.retained_count(), 0);
        assert_eq!(registry.counts().total(), 0);
    }

    #[test]
    fn retention_ttl_collects_expired_records_deterministically() {
        let mut registry = registry(limits(4, 8, 4096, 100));
        let first = create(&mut registry, "first", 1);
        let second = create(&mut registry, "second", 2);
        terminate(
            &mut registry,
            first.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            10,
        );
        terminate(
            &mut registry,
            second.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            50,
        );

        assert!(registry.collect_retained(ms(109)).is_empty());
        assert_eq!(registry.retained_count(), 2);
        assert_eq!(registry.collect_retained(ms(110)), vec![first.session_id]);
        assert_eq!(registry.retained_count(), 1);
        assert!(registry.collect_retained(ms(110)).is_empty());
        assert_eq!(registry.collect_retained(ms(150)), vec![second.session_id]);
        assert_eq!(registry.retained_count(), 0);
    }

    #[test]
    fn retention_cap_evicts_the_oldest_terminal_records() {
        let mut registry = registry(limits(4, 2, 4096, 600_000));
        let first = create(&mut registry, "first", 1);
        let second = create(&mut registry, "second", 2);
        let third = create(&mut registry, "third", 3);
        for (session_id, now) in [
            (first.session_id, 10),
            (second.session_id, 20),
            (third.session_id, 30),
        ] {
            terminate(
                &mut registry,
                session_id,
                TerminalReason::OperatorAbort,
                ExitInformation::NotApplicable,
                CleanupClass::NotStarted,
                now,
            );
        }

        assert_eq!(registry.retained_count(), 2);
        assert_eq!(registry.snapshot(first.session_id), None);
        assert!(registry.snapshot(second.session_id).is_some());
        assert!(registry.snapshot(third.session_id).is_some());
        assert_eq!(registry.counts().terminated, 2);
    }

    #[test]
    fn running_cannot_terminate_without_stopping() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        let running = registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");

        let outcome = TerminalOutcome::new(
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::Swept,
        )
        .expect("valid outcome");
        assert_eq!(
            registry.terminate(created.session_id, outcome, ms(3)),
            Err(RegistryRefusal::InvalidTransition {
                from: LifecycleState::Running,
                to: LifecycleState::Terminated,
            })
        );
        assert_eq!(registry.snapshot(created.session_id), Some(running));
    }

    #[test]
    fn terminal_reason_must_match_the_origin_state() {
        let mut registry = default_registry();
        let running = create(&mut registry, "running", 1);
        registry
            .bind(running.session_id, ms(2))
            .expect("bind succeeds");
        let spawn_failed = TerminalOutcome::new(
            TerminalReason::SpawnFailed,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
        )
        .expect("valid outcome");
        assert_eq!(
            registry.terminate(running.session_id, spawn_failed, ms(3)),
            Err(RegistryRefusal::ReasonNotAllowed {
                reason: TerminalReason::SpawnFailed,
                from: LifecycleState::Running,
            })
        );

        let created = create(&mut registry, "created", 4);
        let worker_signal = TerminalOutcome::new(
            TerminalReason::WorkerSignal,
            ExitInformation::Signal(9),
            CleanupClass::Swept,
        )
        .expect("valid outcome");
        assert_eq!(
            registry.terminate(created.session_id, worker_signal, ms(5)),
            Err(RegistryRefusal::ReasonNotAllowed {
                reason: TerminalReason::WorkerSignal,
                from: LifecycleState::Created,
            })
        );
        assert_eq!(registry.counts().running, 1);
        assert_eq!(registry.counts().created, 1);
    }

    #[test]
    fn not_started_cleanup_is_only_valid_from_created() {
        let mut registry = default_registry();
        let running = create(&mut registry, "running", 1);
        registry
            .bind(running.session_id, ms(2))
            .expect("bind succeeds");
        registry
            .begin_stopping(running.session_id, ms(3))
            .expect("stopping succeeds");
        let not_started = TerminalOutcome::new(
            TerminalReason::ConsumerCancel,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
        )
        .expect("outcome is valid without an origin");
        assert_eq!(
            registry.terminate(running.session_id, not_started, ms(4)),
            Err(RegistryRefusal::CleanupNotStarted {
                from: LifecycleState::Stopping,
            })
        );
        assert_eq!(
            registry
                .snapshot(running.session_id)
                .map(|snapshot| snapshot.state),
            Some(LifecycleState::Stopping)
        );

        let created = create(&mut registry, "created", 5);
        let terminal = registry
            .terminate(created.session_id, not_started, ms(6))
            .expect("created may claim not_started");
        assert_eq!(terminal.state, LifecycleState::Terminated);
        assert_eq!(terminal.cleanup, Some(CleanupClass::NotStarted));
    }

    #[test]
    fn created_session_can_fail_or_terminate_without_a_bind() {
        let mut registry = default_registry();
        let failed = create(&mut registry, "failed", 1);
        let failed = terminate(
            &mut registry,
            failed.session_id,
            TerminalReason::SpawnFailed,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            2,
        );
        assert_eq!(failed.state, LifecycleState::Failed);
        assert_eq!(failed.generation, 0);

        let aborted = create(&mut registry, "aborted", 3);
        let aborted = terminate(
            &mut registry,
            aborted.session_id,
            TerminalReason::ConsumerCancel,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            4,
        );
        assert_eq!(aborted.state, LifecycleState::Terminated);
        assert_eq!(aborted.state_revision, 1);
    }

    #[test]
    fn revision_overflow_fails_closed_without_mutation() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");
        {
            let record = registry
                .live
                .get_mut(&created.session_id)
                .expect("record is live");
            record.state_revision = u64::MAX;
        }

        assert_eq!(
            registry.attach(created.session_id, ms(3)),
            Err(RegistryRefusal::RevisionOverflow)
        );
        assert_eq!(
            registry
                .snapshot(created.session_id)
                .map(|snapshot| snapshot.state_revision),
            Some(u64::MAX)
        );
        assert_eq!(registry.counts().running, 1);
    }

    #[test]
    fn generation_overflow_fails_closed_without_mutation() {
        let mut registry = default_registry();
        let created = create(&mut registry, "term", 1);
        {
            let record = registry
                .live
                .get_mut(&created.session_id)
                .expect("record is live");
            record.generation = u64::MAX;
        }

        assert_eq!(
            registry.bind(created.session_id, ms(2)),
            Err(RegistryRefusal::GenerationOverflow)
        );
        let snapshot = registry
            .snapshot(created.session_id)
            .expect("record exists");
        assert_eq!(snapshot.generation, u64::MAX);
        assert_eq!(snapshot.state, LifecycleState::Created);
        assert_eq!(snapshot.state_revision, 0);
    }

    #[test]
    fn entropy_failure_during_create_leaves_no_record() {
        let mut registry = SessionRegistry::with_entropy(
            limits(4, 8, 4096, 600_000),
            Box::new(CountingEntropy::failing_at(1)),
        )
        .expect("instance id generation succeeds");
        let refusal = registry.create(request("term"), ms(1));

        assert_eq!(refusal, Err(RegistryRefusal::IdGenerationFailed));
        assert_eq!(registry.live_count(), 0);
        assert_eq!(registry.counts().total(), 0);
    }

    #[test]
    fn session_id_collision_is_retried_and_never_reused() {
        // Instance id uses byte 0, the first session byte 1, then the scripted
        // collision (1 again) and finally byte 2.
        let entropy = ScriptedEntropy::new([0, 1, 1, 2], 2);
        let mut registry =
            SessionRegistry::with_entropy(limits(4, 8, 4096, 600_000), Box::new(entropy))
                .expect("instance id generation succeeds");
        let first = create(&mut registry, "first", 1);
        let second = create(&mut registry, "second", 2);

        assert_ne!(first.session_id, second.session_id);
        assert_eq!(second.session_id.as_hex(), "02".repeat(16));
    }

    #[test]
    fn session_id_collision_exhaustion_refuses_without_a_record() {
        let entropy = ScriptedEntropy::new([0, 1], 1);
        let mut registry =
            SessionRegistry::with_entropy(limits(4, 8, 4096, 600_000), Box::new(entropy))
                .expect("instance id generation succeeds");
        let first = create(&mut registry, "first", 1);

        assert_eq!(
            registry.create(request("second"), ms(2)),
            Err(RegistryRefusal::IdCollision)
        );
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.counts().total(), 1);
        assert_eq!(
            registry
                .snapshot(first.session_id)
                .map(|snapshot| snapshot.terminal_id),
            Some(TerminalId::supplied("first").expect("valid"))
        );
    }

    #[test]
    fn unknown_and_collected_sessions_are_not_found() {
        let mut registry = default_registry();
        let unknown = SessionId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("canonical");
        assert_eq!(registry.snapshot(unknown), None);
        assert_eq!(
            registry.heartbeat(unknown, ms(1)),
            Err(RegistryRefusal::NotFound)
        );

        let created = create(&mut registry, "term", 1);
        terminate(
            &mut registry,
            created.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            2,
        );
        registry.collect_retained(ms(2 + 600_000));
        assert_eq!(registry.snapshot(created.session_id), None);
        assert_eq!(
            registry.attach(created.session_id, ms(3)),
            Err(RegistryRefusal::NotFound)
        );
    }

    #[test]
    fn snapshot_reports_counters_and_terminal_classification() {
        let mut registry = default_registry();
        let created = registry
            .create(
                CreateSession {
                    terminal_id: Some(TerminalId::supplied("term").expect("valid")),
                    owner_peer: Some(OwnerPeer::new(b"uid=1000,pid=7").expect("bounded")),
                },
                ms(1),
            )
            .expect("create succeeds");
        registry
            .bind(created.session_id, ms(2))
            .expect("bind succeeds");
        registry
            .attach(created.session_id, ms(3))
            .expect("attach succeeds");
        registry
            .begin_stopping(created.session_id, ms(4))
            .expect("stopping succeeds");
        let snapshot = terminate(
            &mut registry,
            created.session_id,
            TerminalReason::WorkerExit,
            ExitInformation::ExitCode(7),
            CleanupClass::Swept,
            5,
        );

        assert_eq!(snapshot.state, LifecycleState::Exited);
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.state_revision, 4);
        assert_eq!(snapshot.attach_count, 1);
        assert!(snapshot.attached);
        assert_eq!(snapshot.terminal_reason, Some(TerminalReason::WorkerExit));
        assert_eq!(snapshot.exit, ExitInformation::ExitCode(7));
        assert_eq!(snapshot.cleanup, Some(CleanupClass::Swept));
        assert_eq!(
            snapshot.owner_peer,
            Some(OwnerPeer::new(b"uid=1000,pid=7").expect("bounded"))
        );
        let counts = registry.counts();
        assert_eq!(counts.exited, 1);
        assert_eq!(counts.live, 0);
        assert_eq!(counts.retained, 1);
        assert_eq!(counts.total(), 1);
    }

    #[test]
    fn snapshots_cover_live_and_retained_records_in_id_order() {
        let mut registry = default_registry();
        let live = create(&mut registry, "live", 1);
        let terminal = create(&mut registry, "terminal", 2);
        terminate(
            &mut registry,
            terminal.session_id,
            TerminalReason::OperatorAbort,
            ExitInformation::NotApplicable,
            CleanupClass::NotStarted,
            3,
        );

        let snapshots: Vec<SessionSnapshot> = registry.snapshots().collect();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].session_id, live.session_id);
        assert_eq!(snapshots[1].session_id, terminal.session_id);
        assert_eq!(snapshots[0].state, LifecycleState::Created);
        assert_eq!(snapshots[1].state, LifecycleState::Terminated);
    }

    #[test]
    fn limit_configuration_above_contract_maxima_is_refused() {
        assert!(matches!(
            RegistryLimits::new(MAX_SESSIONS_SYSTEM_MAX + 1, 64, 4096, 600_000),
            Err(RegistryRefusal::InvalidLimits {
                limit: "max_sessions",
                ..
            })
        ));
        assert!(matches!(
            RegistryLimits::new(
                16,
                MAX_RETAINED_TERMINAL_RECORDS_SYSTEM_MAX + 1,
                4096,
                600_000
            ),
            Err(RegistryRefusal::InvalidLimits {
                limit: "max_retained_terminal_records",
                ..
            })
        ));
        assert!(matches!(
            RegistryLimits::new(16, 64, SESSION_RECORD_BYTES_SYSTEM_MAX + 1, 600_000),
            Err(RegistryRefusal::InvalidLimits {
                limit: "session_record_bytes",
                ..
            })
        ));
        assert!(matches!(
            RegistryLimits::new(16, 64, 4096, TERMINAL_RETENTION_MS_SYSTEM_MAX + 1),
            Err(RegistryRefusal::InvalidLimits {
                limit: "terminal_retention_ms",
                ..
            })
        ));
        assert_eq!(RegistryLimits::default(), limits(16, 64, 4096, 600_000));
    }
}
