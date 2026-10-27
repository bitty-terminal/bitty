//! Bounded idempotency stores for keyed mutations (contract `D-18`, bitty#2603).
//!
//! Two stores share this engine:
//!
//! - [`IdempotencyStore::session`] is the per-session descriptor store keyed by
//!   `(authenticated uid or SID, key)`. It is bounded by
//!   `max_idempotency_keys_per_session` (512) and
//!   `max_idempotency_bytes_per_session` (65,536) at the compiled maxima, and
//!   the caller releases it at the terminal transition.
//! - [`IdempotencyStore::create`] is the pre-session create-key store keyed by
//!   `(authenticated uid or SID, key)`. It is bounded by
//!   `max_create_idempotency_keys` (128) and `max_create_idempotency_bytes`
//!   (32,768), and a key is retained for at least `create_idempotency_ttl_ms`
//!   after insertion and never retired while its session is live.
//!
//! The key namespace is supplied by the caller: the service layer hashes the
//! authenticated peer identity and the client key into an [`IdempotencyKey`],
//! so two peers cannot collide on the same client key. Keys are
//! token-equivalent secrets for `session.create`, so [`IdempotencyKey`] has a
//! redacted [`fmt::Debug`] and no `Display`.
//!
//! # Admission protocol
//!
//! 1. [`IdempotencyStore::begin`] returns [`Admission::Execute`] with a
//!    [`Reservation`] for a new key, or [`Admission::Replay`] with the stored
//!    result for the same key and identical request identity. It never returns
//!    `Execute` twice for one live entry, so the executor cannot run twice.
//!    A duplicate while the first execution is still in flight is refused with
//!    [`IdempotencyRefusal::InFlight`] instead of being re-executed. A
//!    lease-bound result (attach, grant, transfer) is replayed only on the
//!    connection that created it (`D-18`); another connection is
//!    [`IdempotencyRefusal::ReplayOnOtherConnection`]. A create result is
//!    replayable across connections so a lost create response can be
//!    recovered.
//! 2. The caller executes once and calls [`IdempotencyStore::complete`].
//! 3. A different fingerprint, method, or target for the same key is
//!    [`IdempotencyRefusal::Conflict`] and applies nothing.
//!
//! # Capacity and retirement (`D-18`)
//!
//! At capacity the store evicts the oldest *retirable* entry first: a stored
//! refusal that applied nothing, or a create key whose session is terminal and
//! whose retention floor has passed. An in-flight entry, and an applied entry
//! whose session is still live, is never evicted. When no entry is retirable
//! the store fails closed with [`IdempotencyRefusal::Capacity`], exactly as the
//! contract requires; termination and revocation are keyless and never consult
//! either store. A hostile stream of fresh keys therefore cannot grow the
//! store past its compiled key or byte bound.

use std::collections::BTreeMap;
use std::fmt;

use crate::codes::RefusalCode;
use crate::limits::{
    CREATE_IDEMPOTENCY_ENTRY_BYTES, LimitId, Limits, SESSION_IDEMPOTENCY_ENTRY_BYTES,
};
use crate::registry::{MonotonicMs, SessionId};

/// One 128-bit client-generated idempotency key.
///
/// The raw bytes are a credential for `session.create` (`D-18`); this type has
/// a redacted [`fmt::Debug`] and no `Display`, so a key cannot reach a log,
/// an audit record, or a refusal message by accident.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyKey([u8; 16]);

impl IdempotencyKey {
    /// Build a key from its 128 raw bits.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IdempotencyKey(***)")
    }
}

/// A truncated 128-bit request fingerprint (`D-18`).
///
/// The service layer derives it from the method, target, generation, reason
/// class, or spawn digest. It is a digest, not caller content, and is safe to
/// print.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint([u8; 16]);

impl Fingerprint {
    /// Build a fingerprint from its 128 bits.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw fingerprint bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Fingerprint(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// A keyed method (`D-18`). Termination and revocation are absent on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdempotentMethod {
    /// `session.create` — pre-session create-key store.
    Create,
    /// `session.attach` — per-session store.
    Attach,
    /// `session.grant` mint — per-session store.
    Grant,
    /// `session.control.transfer` — per-session store.
    Transfer,
}

impl IdempotentMethod {
    /// The contract method name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "session.create",
            Self::Attach => "session.attach",
            Self::Grant => "session.grant",
            Self::Transfer => "session.control.transfer",
        }
    }

    /// The store that owns keys for this method.
    #[must_use]
    pub const fn scope(self) -> IdempotencyScope {
        match self {
            Self::Create => IdempotencyScope::Create,
            Self::Attach | Self::Grant | Self::Transfer => IdempotencyScope::Session,
        }
    }
}

impl fmt::Display for IdempotentMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which of the two `D-18` stores a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdempotencyScope {
    /// Per-session descriptor store, released at the terminal transition.
    Session,
    /// Pre-session create-key store, retained for the create-key TTL floor.
    Create,
}

impl IdempotencyScope {
    /// Static spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Create => "create",
        }
    }
}

impl fmt::Display for IdempotencyScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a target id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetKind {
    /// A `session_id`.
    Session,
    /// An `attach_id`.
    Attach,
    /// A non-secret `grant_id`.
    Grant,
    /// A `terminal_id_digest` (the raw `terminal_id` is never stored in audit).
    TerminalDigest,
}

impl TargetKind {
    /// Static spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Attach => "attach",
            Self::Grant => "grant",
            Self::TerminalDigest => "terminal_digest",
        }
    }
}

/// The method target recorded with a key (`D-18`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdempotencyTarget {
    kind: TargetKind,
    id: [u8; 16],
}

impl IdempotencyTarget {
    /// A target that names a session.
    #[must_use]
    pub const fn session(session_id: SessionId) -> Self {
        Self {
            kind: TargetKind::Session,
            id: *session_id.as_bytes(),
        }
    }

    /// A target that names an attach lease.
    #[must_use]
    pub const fn attach(attach_id: [u8; 16]) -> Self {
        Self {
            kind: TargetKind::Attach,
            id: attach_id,
        }
    }

    /// A target that names a non-secret grant.
    #[must_use]
    pub const fn grant(grant_id: [u8; 16]) -> Self {
        Self {
            kind: TargetKind::Grant,
            id: grant_id,
        }
    }

    /// A target that names a terminal-id digest for `session.create`.
    #[must_use]
    pub const fn terminal_digest(digest: [u8; 16]) -> Self {
        Self {
            kind: TargetKind::TerminalDigest,
            id: digest,
        }
    }

    /// What the target id names.
    #[must_use]
    pub const fn kind(self) -> TargetKind {
        self.kind
    }

    /// The 128-bit target value.
    #[must_use]
    pub const fn id(self) -> [u8; 16] {
        self.id
    }
}

/// The request identity compared for replay or conflict (`D-18`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdempotentIdentity {
    /// The keyed method.
    pub method: IdempotentMethod,
    /// The method target.
    pub target: IdempotencyTarget,
    /// The truncated request fingerprint.
    pub fingerprint: Fingerprint,
}

/// One keyed request presented to a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdempotentRequest {
    /// Client-generated key, already bound to the authenticated peer namespace.
    pub key: IdempotencyKey,
    /// Keyed method.
    pub method: IdempotentMethod,
    /// Method target.
    pub target: IdempotencyTarget,
    /// Request fingerprint.
    pub fingerprint: Fingerprint,
    /// Server-assigned connection identity that presented the request.
    ///
    /// A lease-bound result is replayed only on this connection (`D-18`); a
    /// create result may be replayed on a new connection after a lost response.
    pub connection_id: u64,
}

impl IdempotentRequest {
    /// Build a request.
    #[must_use]
    pub const fn new(
        key: IdempotencyKey,
        method: IdempotentMethod,
        target: IdempotencyTarget,
        fingerprint: Fingerprint,
        connection_id: u64,
    ) -> Self {
        Self {
            key,
            method,
            target,
            fingerprint,
            connection_id,
        }
    }

    /// The request identity used for replay and conflict checks.
    #[must_use]
    pub const fn identity(self) -> IdempotentIdentity {
        IdempotentIdentity {
            method: self.method,
            target: self.target,
            fingerprint: self.fingerprint,
        }
    }
}

/// The result class of an applied keyed method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AppliedClass {
    /// `session.create` returned a new session.
    Created,
    /// `session.attach` returned a lease.
    Attached,
    /// `session.grant` minted a grant.
    GrantMinted,
    /// `session.control.transfer` moved control.
    Transferred,
}

impl AppliedClass {
    /// Static spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Attached => "attached",
            Self::GrantMinted => "grant_minted",
            Self::Transferred => "transferred",
        }
    }

    const fn matches(self, method: IdempotentMethod) -> bool {
        matches!(
            (method, self),
            (IdempotentMethod::Create, Self::Created)
                | (IdempotentMethod::Attach, Self::Attached)
                | (IdempotentMethod::Grant, Self::GrantMinted)
                | (IdempotentMethod::Transfer, Self::Transferred)
        )
    }
}

impl fmt::Display for AppliedClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The bounded descriptor stored for an applied keyed method (`D-18`).
///
/// `result_ref` is the 64-bit reference to the lease or grant record created by
/// the original call. It is not a raw id and is never reused while the entry
/// exists. The control token itself is not stored here; the create store keeps
/// only `control_claimed`, and the session record owns the token lifetime
/// (bitty#2607/bitty#2609).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedResult {
    /// Result class.
    pub class: AppliedClass,
    /// Session the result belongs to.
    pub session_id: SessionId,
    /// Generation at the original call.
    pub generation: u64,
    /// State revision at the original call.
    pub revision: u64,
    /// 64-bit result handle.
    pub result_ref: u64,
    /// Whether control had already been claimed when the descriptor was stored.
    pub control_claimed: bool,
}

impl AppliedResult {
    /// Build an applied descriptor.
    #[must_use]
    pub const fn new(
        class: AppliedClass,
        session_id: SessionId,
        generation: u64,
        revision: u64,
        result_ref: u64,
        control_claimed: bool,
    ) -> Self {
        Self {
            class,
            session_id,
            generation,
            revision,
            result_ref,
            control_claimed,
        }
    }
}

/// The stored result of one keyed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyResult {
    /// The original call applied; replay returns this descriptor.
    Applied(AppliedResult),
    /// The original call refused with a static code and applied nothing.
    Refused(RefusalCode),
}

impl IdempotencyResult {
    /// Whether the stored result applied a mutation.
    #[must_use]
    pub const fn applied(self) -> bool {
        matches!(self, Self::Applied(_))
    }

    /// The static refusal code, when the result is a refusal.
    #[must_use]
    pub const fn refusal_code(self) -> Option<RefusalCode> {
        match self {
            Self::Applied(_) => None,
            Self::Refused(code) => Some(code),
        }
    }
}

/// The outcome of [`IdempotencyStore::begin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The key is new and reserved; execute the request exactly once.
    Execute(Reservation),
    /// The key and request identity match a completed entry; return the stored
    /// result and do not execute again.
    Replay(IdempotencyResult),
}

/// A reserved in-flight entry.
///
/// The raw key is private and redacted in [`fmt::Debug`]; the reservation is
/// only valid for the store that produced it and for that one entry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    key: IdempotencyKey,
    sequence: u64,
    method: IdempotentMethod,
}

impl Reservation {
    /// The method being executed.
    #[must_use]
    pub const fn method(self) -> IdempotentMethod {
        self.method
    }
}

impl fmt::Debug for Reservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reservation")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// A refused idempotency operation. No variant carries a raw key or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyRefusal {
    /// The same key was presented with a different method, target, or
    /// fingerprint (`Denied/IdempotencyConflict`).
    Conflict {
        /// What the stored entry holds.
        existing: IdempotentIdentity,
        /// What the caller presented.
        presented: IdempotentIdentity,
    },
    /// A duplicate arrived while the original execution is still in flight.
    /// The caller must not execute again.
    InFlight,
    /// A lease-bound result was presented on a different connection
    /// (`Denied/IdempotencyConflict`). The caller must not execute it again.
    ReplayOnOtherConnection,
    /// No entry is retirable and the store is at its key or byte bound
    /// (`Denied/IdempotencyCapacity`).
    Capacity {
        /// The compiled key bound.
        max_keys: usize,
        /// The compiled byte bound.
        max_bytes: usize,
    },
    /// The method belongs to the other `D-18` store.
    WrongScope {
        /// The store's scope.
        scope: IdempotencyScope,
        /// The presented method.
        method: IdempotentMethod,
    },
    /// The reservation does not belong to a live in-flight entry in this store.
    UnknownReservation,
    /// The entry was already completed.
    AlreadyCompleted,
    /// The applied result class does not match the reserved method.
    ResultMismatch {
        /// The reserved method.
        method: IdempotentMethod,
        /// The result class supplied.
        class: AppliedClass,
    },
    /// The store's insertion sequence would wrap; it fails closed.
    SequenceExhausted,
}

impl fmt::Display for IdempotencyRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict {
                existing,
                presented,
            } => write!(
                formatter,
                "idempotency key reused for {} instead of {}",
                presented.method, existing.method
            ),
            Self::InFlight => formatter.write_str("original request is still in flight"),
            Self::ReplayOnOtherConnection => {
                formatter.write_str("lease-bound result belongs to another connection")
            }
            Self::Capacity {
                max_keys,
                max_bytes,
            } => write!(
                formatter,
                "idempotency store is full ({max_keys} keys, {max_bytes} bytes)"
            ),
            Self::WrongScope { scope, method } => {
                write!(formatter, "{method} does not belong to the {scope} store")
            }
            Self::UnknownReservation => formatter.write_str("unknown idempotency reservation"),
            Self::AlreadyCompleted => formatter.write_str("request is already completed"),
            Self::ResultMismatch { method, class } => {
                write!(
                    formatter,
                    "result class {class} does not belong to {method}"
                )
            }
            Self::SequenceExhausted => formatter.write_str("idempotency sequence would wrap"),
        }
    }
}

impl std::error::Error for IdempotencyRefusal {}

/// How a completed entry may be retired (`D-18`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retirement {
    /// Applied result whose session is still live; never evicted while live.
    Live,
    /// A refusal that applied nothing; retirable under capacity pressure.
    Refused,
    /// A create key whose session is terminal; retirable after the TTL floor.
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    InFlight,
    Complete {
        result: IdempotencyResult,
        retirement: Retirement,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    method: IdempotentMethod,
    target: IdempotencyTarget,
    fingerprint: Fingerprint,
    connection_id: u64,
    inserted_at: MonotonicMs,
    sequence: u64,
    state: EntryState,
}

impl Entry {
    fn identity(self) -> IdempotentIdentity {
        IdempotentIdentity {
            method: self.method,
            target: self.target,
            fingerprint: self.fingerprint,
        }
    }

    fn matches(self, request: IdempotentRequest) -> bool {
        self.identity() == request.identity()
    }

    fn is_in_flight(self) -> bool {
        matches!(self.state, EntryState::InFlight)
    }

    fn result(self) -> Option<IdempotencyResult> {
        match self.state {
            EntryState::InFlight => None,
            EntryState::Complete { result, .. } => Some(result),
        }
    }

    fn applied_session(self) -> Option<SessionId> {
        match self.state {
            EntryState::Complete {
                result: IdempotencyResult::Applied(applied),
                retirement: Retirement::Live,
            } => Some(applied.session_id),
            _ => None,
        }
    }

    fn is_retirable(self, now: MonotonicMs, ttl_floor_ms: Option<u64>) -> bool {
        match self.state {
            EntryState::InFlight => false,
            EntryState::Complete { retirement, .. } => match retirement {
                Retirement::Live => false,
                Retirement::Refused => true,
                Retirement::Terminal => {
                    let ttl = ttl_floor_ms.unwrap_or(0);
                    now.saturating_since(self.inserted_at) >= ttl
                }
            },
        }
    }

    fn is_expired(self, now: MonotonicMs, ttl_floor_ms: Option<u64>) -> bool {
        match self.state {
            EntryState::Complete {
                retirement: Retirement::Terminal,
                ..
            } => {
                let ttl = ttl_floor_ms.unwrap_or(0);
                now.saturating_since(self.inserted_at) >= ttl
            }
            _ => false,
        }
    }
}

/// A bounded idempotency store for one scope (`D-18`).
///
/// Construction takes the effective limits; see the module documentation for
/// the admission protocol and the retirement rules.
pub struct IdempotencyStore {
    scope: IdempotencyScope,
    max_keys: usize,
    max_bytes: usize,
    entry_bytes: usize,
    ttl_floor_ms: Option<u64>,
    entries: BTreeMap<IdempotencyKey, Entry>,
    order: BTreeMap<u64, IdempotencyKey>,
    next_sequence: u64,
}

impl IdempotencyStore {
    /// Build the per-session store from the effective limits.
    #[must_use]
    pub fn session(limits: &Limits) -> Self {
        Self::new(
            IdempotencyScope::Session,
            limits.effective_usize(LimitId::MaxIdempotencyKeysPerSession),
            limits.effective_usize(LimitId::MaxIdempotencyBytesPerSession),
            SESSION_IDEMPOTENCY_ENTRY_BYTES,
            None,
        )
    }

    /// Build the pre-session create-key store from the effective limits.
    #[must_use]
    pub fn create(limits: &Limits) -> Self {
        Self::new(
            IdempotencyScope::Create,
            limits.effective_usize(LimitId::MaxCreateIdempotencyKeys),
            limits.effective_usize(LimitId::MaxCreateIdempotencyBytes),
            CREATE_IDEMPOTENCY_ENTRY_BYTES,
            Some(limits.effective(LimitId::CreateIdempotencyTtlMs)),
        )
    }

    fn new(
        scope: IdempotencyScope,
        max_keys: usize,
        max_bytes: usize,
        entry_bytes: usize,
        ttl_floor_ms: Option<u64>,
    ) -> Self {
        Self {
            scope,
            max_keys,
            max_bytes,
            entry_bytes,
            ttl_floor_ms,
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            next_sequence: 1,
        }
    }

    /// The store's scope.
    #[must_use]
    pub const fn scope(&self) -> IdempotencyScope {
        self.scope
    }

    /// The configured key bound.
    #[must_use]
    pub const fn max_keys(&self) -> usize {
        self.max_keys
    }

    /// The configured byte bound.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The accounted bytes per entry.
    #[must_use]
    pub const fn entry_bytes(&self) -> usize {
        self.entry_bytes
    }

    /// The create-key retention floor, if this is the create store.
    #[must_use]
    pub const fn ttl_floor_ms(&self) -> Option<u64> {
        self.ttl_floor_ms
    }

    /// The maximum number of entries that fits both the key and byte bounds.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        let byte_capacity = self.max_bytes / self.entry_bytes;
        if self.max_keys < byte_capacity {
            self.max_keys
        } else {
            byte_capacity
        }
    }

    /// Current entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Accounted bytes of the current entries.
    #[must_use]
    pub fn accounted_bytes(&self) -> usize {
        self.entries.len().saturating_mul(self.entry_bytes)
    }

    /// Admit one keyed request.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyRefusal::WrongScope`] for a method owned by the
    /// other store, [`IdempotencyRefusal::Conflict`] for the same key with a
    /// different identity, [`IdempotencyRefusal::InFlight`] for a duplicate
    /// while the first call is executing, and [`IdempotencyRefusal::Capacity`]
    /// when no retirable entry can make room. No refusal changes the store.
    pub fn begin(
        &mut self,
        request: IdempotentRequest,
        now: MonotonicMs,
    ) -> Result<Admission, IdempotencyRefusal> {
        if request.method.scope() != self.scope {
            return Err(IdempotencyRefusal::WrongScope {
                scope: self.scope,
                method: request.method,
            });
        }

        if let Some(entry) = self.entries.get(&request.key).copied() {
            if !entry.matches(request) {
                return Err(IdempotencyRefusal::Conflict {
                    existing: entry.identity(),
                    presented: request.identity(),
                });
            }
            if request.method != IdempotentMethod::Create
                && entry.connection_id != request.connection_id
            {
                return Err(IdempotencyRefusal::ReplayOnOtherConnection);
            }
            if entry.is_in_flight() {
                return Err(IdempotencyRefusal::InFlight);
            }
            return Ok(Admission::Replay(
                entry.result().expect("a completed entry has a result"),
            ));
        }

        if self.capacity() == 0 {
            return Err(IdempotencyRefusal::Capacity {
                max_keys: self.max_keys,
                max_bytes: self.max_bytes,
            });
        }
        let sequence = self.next_sequence;
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(IdempotencyRefusal::SequenceExhausted)?;

        while self.entries.len() >= self.capacity() {
            if !self.evict_oldest_retirable(now) {
                return Err(IdempotencyRefusal::Capacity {
                    max_keys: self.max_keys,
                    max_bytes: self.max_bytes,
                });
            }
        }
        self.next_sequence = next_sequence;
        self.entries.insert(
            request.key,
            Entry {
                method: request.method,
                target: request.target,
                fingerprint: request.fingerprint,
                connection_id: request.connection_id,
                inserted_at: now,
                sequence,
                state: EntryState::InFlight,
            },
        );
        self.order.insert(sequence, request.key);
        Ok(Admission::Execute(Reservation {
            key: request.key,
            sequence,
            method: request.method,
        }))
    }

    /// Store the result of the reserved execution.
    ///
    /// # Errors
    ///
    /// Refuses an unknown, stale, or already completed reservation, and an
    /// applied result whose class does not match the reserved method. A
    /// refusal leaves the entry in flight so the caller can retry with a valid
    /// result.
    pub fn complete(
        &mut self,
        reservation: Reservation,
        result: IdempotencyResult,
    ) -> Result<(), IdempotencyRefusal> {
        let entry = self
            .entries
            .get_mut(&reservation.key)
            .ok_or(IdempotencyRefusal::UnknownReservation)?;
        if entry.sequence != reservation.sequence || entry.method != reservation.method {
            return Err(IdempotencyRefusal::UnknownReservation);
        }
        if !entry.is_in_flight() {
            return Err(IdempotencyRefusal::AlreadyCompleted);
        }
        if let IdempotencyResult::Applied(applied) = result {
            if !applied.class.matches(reservation.method) {
                return Err(IdempotencyRefusal::ResultMismatch {
                    method: reservation.method,
                    class: applied.class,
                });
            }
        }

        let retirement = match result {
            IdempotencyResult::Refused(_) => Retirement::Refused,
            IdempotencyResult::Applied(_) => Retirement::Live,
        };
        entry.state = EntryState::Complete { result, retirement };
        Ok(())
    }

    /// Apply a session's terminal transition to this store.
    ///
    /// The per-session store is released in full (`D-18`: keys are released at
    /// the terminal transition, not at collection). The create store marks the
    /// matching applied entries as retirable once `create_idempotency_ttl_ms`
    /// has elapsed since insertion; refused entries are already retirable.
    /// Returns the number of entries released or marked.
    pub fn retire_session(&mut self, session_id: SessionId) -> usize {
        if self.scope == IdempotencyScope::Session {
            let released = self.entries.len();
            self.entries.clear();
            self.order.clear();
            return released;
        }

        let mut marked = 0;
        for entry in self.entries.values_mut() {
            if entry.applied_session() == Some(session_id) {
                if let EntryState::Complete { retirement, .. } = &mut entry.state {
                    *retirement = Retirement::Terminal;
                    marked += 1;
                }
            }
        }
        marked
    }

    /// Remove create keys whose retention floor has passed and whose session is
    /// terminal. Refused entries are not expired by TTL; they stay replayable
    /// until capacity pressure retires them.
    ///
    /// Returns the number of removed entries.
    pub fn evict_expired(&mut self, now: MonotonicMs) -> usize {
        let expired: Vec<(u64, IdempotencyKey)> = self
            .order
            .iter()
            .filter_map(|(sequence, key)| {
                self.entries
                    .get(key)
                    .filter(|entry| entry.is_expired(now, self.ttl_floor_ms))
                    .map(|_| (*sequence, *key))
            })
            .collect();
        for (sequence, key) in &expired {
            self.order.remove(sequence);
            self.entries.remove(key);
        }
        expired.len()
    }

    fn evict_oldest_retirable(&mut self, now: MonotonicMs) -> bool {
        let victim = self.order.iter().find_map(|(sequence, key)| {
            self.entries
                .get(key)
                .filter(|entry| entry.is_retirable(now, self.ttl_floor_ms))
                .map(|_| (*sequence, *key))
        });
        let Some((sequence, key)) = victim else {
            return false;
        };
        self.order.remove(&sequence);
        self.entries.remove(&key);
        true
    }
}

impl fmt::Debug for IdempotencyStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdempotencyStore")
            .field("scope", &self.scope)
            .field("entries", &self.entries.len())
            .field("max_keys", &self.max_keys)
            .field("max_bytes", &self.max_bytes)
            .field("entry_bytes", &self.entry_bytes)
            .field("ttl_floor_ms", &self.ttl_floor_ms)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitOverride;

    fn ms(value: u64) -> MonotonicMs {
        MonotonicMs::from_millis(value)
    }

    fn key(seed: u8) -> IdempotencyKey {
        IdempotencyKey::from_bytes([seed; 16])
    }

    fn fingerprint(seed: u8) -> Fingerprint {
        Fingerprint::from_bytes([seed; 16])
    }

    fn session_id(seed: u8) -> SessionId {
        SessionId::parse(&format!("{seed:02x}").repeat(16)).expect("canonical session id")
    }

    fn target(seed: u8) -> IdempotencyTarget {
        IdempotencyTarget::session(session_id(seed))
    }

    fn request(seed: u8, method: IdempotentMethod) -> IdempotentRequest {
        IdempotentRequest::new(
            key(seed),
            method,
            target(seed.wrapping_add(1)),
            fingerprint(seed.wrapping_add(2)),
            1,
        )
    }

    fn applied(seed: u8, class: AppliedClass) -> IdempotencyResult {
        IdempotencyResult::Applied(AppliedResult::new(class, session_id(seed), 0, 0, 1, false))
    }

    #[test]
    fn entry_fits_the_contract_descriptor_budget() {
        assert!(
            std::mem::size_of::<Entry>() <= SESSION_IDEMPOTENCY_ENTRY_BYTES,
            "entry is {} bytes",
            std::mem::size_of::<Entry>()
        );
    }

    #[test]
    fn keys_are_redacted_in_debug_and_have_no_display() {
        let value = key(0xab);
        assert_eq!(format!("{value:?}"), "IdempotencyKey(***)");
        assert!(!format!("{value:?}").contains("ab"));
        let reservation = Reservation {
            key: value,
            sequence: 1,
            method: IdempotentMethod::Attach,
        };
        let rendered = format!("{reservation:?}");
        assert!(!rendered.contains("ab"));
        assert!(rendered.contains("Attach"));

        let store = IdempotencyStore::session(&Limits::default());
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
        assert_eq!(store.capacity(), 512);
        assert_eq!(store.accounted_bytes(), 0);
        assert_eq!(store.scope(), IdempotencyScope::Session);
        assert_eq!(store.max_keys(), 512);
        assert_eq!(store.max_bytes(), 65_536);
        assert_eq!(store.entry_bytes(), SESSION_IDEMPOTENCY_ENTRY_BYTES);
        assert_eq!(store.ttl_floor_ms(), None);
    }

    #[test]
    fn replay_returns_the_identical_result_and_never_executes_twice() {
        let mut store = IdempotencyStore::session(&Limits::default());
        let request = request(1, IdempotentMethod::Attach);
        let stored = applied(2, AppliedClass::Attached);

        let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
            panic!("first request executes");
        };
        store.complete(reservation, stored).expect("complete");
        assert_eq!(store.len(), 1);

        for _ in 0..4 {
            assert_eq!(store.begin(request, ms(2)), Ok(Admission::Replay(stored)));
        }
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn duplicate_in_flight_is_refused_without_a_second_admission() {
        let mut store = IdempotencyStore::session(&Limits::default());
        let request = request(1, IdempotentMethod::Attach);
        let first = store.begin(request, ms(1)).expect("begin");
        assert!(matches!(first, Admission::Execute(_)));

        assert_eq!(
            store.begin(request, ms(2)),
            Err(IdempotencyRefusal::InFlight)
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_different_fingerprint_for_the_same_key_is_a_conflict() {
        let mut store = IdempotencyStore::session(&Limits::default());
        let request = request(1, IdempotentMethod::Attach);
        let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
            panic!("first request executes");
        };
        store
            .complete(reservation, applied(2, AppliedClass::Attached))
            .expect("complete");

        let mut conflicting = request;
        conflicting.fingerprint = fingerprint(99);
        assert!(matches!(
            store.begin(conflicting, ms(2)),
            Err(IdempotencyRefusal::Conflict { .. })
        ));
        let mut conflicting = request;
        conflicting.target = target(99);
        assert!(matches!(
            store.begin(conflicting, ms(3)),
            Err(IdempotencyRefusal::Conflict { .. })
        ));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn scope_guards_separate_the_two_stores() {
        let mut session_store = IdempotencyStore::session(&Limits::default());
        assert!(matches!(
            session_store.begin(request(1, IdempotentMethod::Create), ms(1)),
            Err(IdempotencyRefusal::WrongScope { .. })
        ));

        let mut create_store = IdempotencyStore::create(&Limits::default());
        assert!(matches!(
            create_store.begin(request(1, IdempotentMethod::Attach), ms(1)),
            Err(IdempotencyRefusal::WrongScope { .. })
        ));
    }

    #[test]
    fn completion_rejects_unknown_reservations_and_wrong_classes() {
        let mut store = IdempotencyStore::session(&Limits::default());
        let Admission::Execute(reservation) = store
            .begin(request(1, IdempotentMethod::Attach), ms(1))
            .expect("begin")
        else {
            panic!("new key executes");
        };

        assert_eq!(
            store.complete(
                Reservation {
                    key: key(9),
                    sequence: 9,
                    method: IdempotentMethod::Attach,
                },
                applied(2, AppliedClass::Attached)
            ),
            Err(IdempotencyRefusal::UnknownReservation)
        );
        assert_eq!(
            store.complete(reservation, applied(2, AppliedClass::GrantMinted)),
            Err(IdempotencyRefusal::ResultMismatch {
                method: IdempotentMethod::Attach,
                class: AppliedClass::GrantMinted,
            })
        );
        // The first reservation is still valid after the failed completion.
        store
            .complete(reservation, applied(2, AppliedClass::Attached))
            .expect("valid completion");
        assert_eq!(
            store.complete(reservation, applied(2, AppliedClass::Attached)),
            Err(IdempotencyRefusal::AlreadyCompleted)
        );
    }

    #[test]
    fn refusal_results_are_retirable_before_live_results() {
        let limits = Limits::new(&[
            LimitOverride::new(LimitId::MaxIdempotencyKeysPerSession, 2),
            LimitOverride::new(LimitId::MaxIdempotencyBytesPerSession, 256),
        ])
        .expect("small valid store");
        let mut store = IdempotencyStore::session(&limits);
        assert_eq!(store.capacity(), 2);

        let first = request(1, IdempotentMethod::Attach);
        let Admission::Execute(reservation) = store.begin(first, ms(1)).expect("begin") else {
            panic!("new key executes");
        };
        store
            .complete(
                reservation,
                IdempotencyResult::Refused(RefusalCode::ScopeViolation),
            )
            .expect("complete refusal");

        let second = request(2, IdempotentMethod::Attach);
        let Admission::Execute(reservation) = store.begin(second, ms(2)).expect("begin") else {
            panic!("new key executes");
        };
        store
            .complete(reservation, applied(3, AppliedClass::Attached))
            .expect("complete applied");

        let third = request(3, IdempotentMethod::Attach);
        let Admission::Execute(reservation) = store.begin(third, ms(3)).expect("begin") else {
            panic!("oldest refusal is evicted");
        };
        store
            .complete(reservation, applied(4, AppliedClass::Attached))
            .expect("complete applied");

        assert_eq!(store.len(), 2);
        assert_eq!(store.accounted_bytes(), 256);
        // The refusal was evicted, the live entry was not.
        assert_eq!(
            store.begin(first, ms(4)),
            Err(IdempotencyRefusal::Capacity {
                max_keys: 2,
                max_bytes: 256,
            })
        );
        assert_eq!(
            store.begin(second, ms(5)),
            Ok(Admission::Replay(applied(3, AppliedClass::Attached)))
        );
    }

    #[test]
    fn in_flight_and_live_entries_block_capacity_instead_of_being_evicted() {
        let limits = Limits::new(&[
            LimitOverride::new(LimitId::MaxIdempotencyKeysPerSession, 1),
            LimitOverride::new(LimitId::MaxIdempotencyBytesPerSession, 128),
        ])
        .expect("small valid store");
        let mut store = IdempotencyStore::session(&limits);

        let first = request(1, IdempotentMethod::Attach);
        assert!(matches!(
            store.begin(first, ms(1)),
            Ok(Admission::Execute(_))
        ));
        assert_eq!(
            store.begin(request(2, IdempotentMethod::Attach), ms(2)),
            Err(IdempotencyRefusal::Capacity {
                max_keys: 1,
                max_bytes: 128,
            })
        );
        assert_eq!(store.len(), 1);

        assert_eq!(store.begin(first, ms(3)), Err(IdempotencyRefusal::InFlight));
    }

    #[test]
    fn hostile_key_stream_stays_within_the_key_and_byte_bounds() {
        let limits = Limits::new(&[
            LimitOverride::new(LimitId::MaxIdempotencyKeysPerSession, 4),
            LimitOverride::new(LimitId::MaxIdempotencyBytesPerSession, 512),
        ])
        .expect("small valid store");
        let mut store = IdempotencyStore::session(&limits);
        for seed in 0..4u8 {
            let request = request(seed, IdempotentMethod::Attach);
            let Admission::Execute(reservation) =
                store.begin(request, ms(u64::from(seed))).expect("begin")
            else {
                panic!("new key executes");
            };
            store
                .complete(
                    reservation,
                    applied(seed.wrapping_add(10), AppliedClass::Attached),
                )
                .expect("complete");
        }
        assert_eq!(store.len(), 4);

        for seed in 100..1_100u64 {
            let byte = u8::try_from(seed % 251).expect("bounded");
            let request = request(byte, IdempotentMethod::Attach);
            let outcome = store.begin(request, ms(seed));
            assert!(
                matches!(outcome, Err(IdempotencyRefusal::Capacity { .. }))
                    || matches!(outcome, Ok(Admission::Replay(_)))
                    || matches!(outcome, Ok(Admission::Execute(_))),
                "unexpected outcome"
            );
            assert!(store.len() <= 4);
            assert!(store.accounted_bytes() <= 512);
        }
    }

    #[test]
    fn create_keys_are_retained_until_the_session_is_terminal() {
        let limits = Limits::default();
        let mut store = IdempotencyStore::create(&limits);
        let request = IdempotentRequest::new(
            key(1),
            IdempotentMethod::Create,
            IdempotencyTarget::terminal_digest([7; 16]),
            fingerprint(2),
            1,
        );
        let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
            panic!("new create key executes");
        };
        store
            .complete(reservation, applied(3, AppliedClass::Created))
            .expect("complete");

        assert_eq!(store.evict_expired(ms(u64::MAX)), 0);
        assert_eq!(
            store.begin(request, ms(2)),
            Ok(Admission::Replay(applied(3, AppliedClass::Created)))
        );

        assert_eq!(store.retire_session(session_id(3)), 1);
        assert_eq!(store.len(), 1);
        let ttl = limits.effective(LimitId::CreateIdempotencyTtlMs);
        let due = 1 + ttl;
        assert_eq!(store.evict_expired(ms(due - 1)), 0);
        assert_eq!(store.evict_expired(ms(due)), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn create_keys_never_retire_while_the_session_is_live() {
        let mut store = IdempotencyStore::create(&Limits::default());
        let request = IdempotentRequest::new(
            key(1),
            IdempotentMethod::Create,
            IdempotencyTarget::terminal_digest([7; 16]),
            fingerprint(2),
            1,
        );
        let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
            panic!("new create key executes");
        };
        store
            .complete(reservation, applied(3, AppliedClass::Created))
            .expect("complete");

        let ttl = store.ttl_floor_ms().expect("create store has a TTL");
        assert_eq!(store.evict_expired(ms(ttl + 1)), 0);
        assert_eq!(store.retire_session(session_id(99)), 0);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn session_store_release_is_immediate_and_total() {
        let mut store = IdempotencyStore::session(&Limits::default());
        for seed in 0..8u8 {
            let request = request(seed, IdempotentMethod::Attach);
            let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin")
            else {
                panic!("new key executes");
            };
            store
                .complete(reservation, applied(seed, AppliedClass::Attached))
                .expect("complete");
        }
        assert_eq!(store.retire_session(session_id(1)), 8);
        assert!(store.is_empty());
        assert_eq!(store.accounted_bytes(), 0);
    }
}
