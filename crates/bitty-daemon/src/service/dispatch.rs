//! The eleven-stage authorization order and the closed refusal mapping
//! (`D-07`, `D-08`, `D-11`, `D-16`, `D-18`).
//!
//! Every request walks the same stages, in the same order, and a request that
//! fails a stage stops there:
//!
//! | Stage | Check | Refusal |
//! | --- | --- | --- |
//! | 1 | Transport, single instance, captured peer, per-frame `v` | `VersionMismatch`, `Unauthenticated` (server loop) |
//! | 2 | Negotiate completed, one negotiate only, pre-auth budget | `Denied/AlreadyNegotiated`, `Timeout`, `LimitExceeded` |
//! | 3 | Service version and capability | `VersionMismatch`, `Denied/UnsupportedCapability` |
//! | 4 | Method resolves | `NotFound`, `InvalidMethod` |
//! | 5 | Peer still the same live process; per-frame credentials match | `Unauthenticated` |
//! | 6 | Scope, decided before any target lookup | `Denied/ScopeViolation` |
//! | 7 | Derived authority: host consent, observer grant, control relation, operator | `Denied/ConsentRequired`, `Denied/ObserverGrantRequired`, `Denied/ControllerRequired`, `Denied/InvalidControlToken`, `Denied/ControlHeld` |
//! | 8 | Target: instance binding, visibility, existence | `Denied/StaleInstance`, `NotFound` |
//! | 9 | Generation on effectful methods | `Denied/StaleGeneration` |
//! | 10 | Idempotency on keyed methods | `Denied/IdempotencyConflict`, `Denied/IdempotencyCapacity` |
//! | 11 | Peer re-check, effect, revision, attribution, typed result | `Denied/Internal`, `Internal`, or the result |
//!
//! Rules that this module enforces structurally:
//!
//! - scope is decided before target lookup, so a caller without the scope
//!   learns nothing about whether a target exists;
//! - `session.create`'s two authorities are ordered by the no-oracle rule: both
//!   missing is `ScopeViolation`, host consent missing with `process.spawn`
//!   present is `ConsentRequired`, and `process.spawn` missing with host
//!   consent present is `ScopeViolation`;
//! - every privileged effect re-checks the captured peer after authorization
//!   and before the effect;
//! - every privileged mutation is admitted by the audit sink first, so a
//!   degraded sink is `Denied/Internal` with no state change;
//! - a refusal carries static codes, counts, and limits only, never caller
//!   bytes, a path, terminal text, a token, a bearer, or another session's
//!   fields.
//!
//! # Deliberate deferrals
//!
//! `session.resize` needs `terminal.manage`, a `bitty-ipc` v1 scope the daemon
//! does not grant in this slice, so it is a typed `Denied/ScopeViolation`.
//! `daemon.orphans.verify` needs the orphan journal and exact-identity reaper
//! owned by bitty#2604; it refuses with `Denied/UnsupportedCapability` instead
//! of pretending an empty verification. Both are recorded in the crate README.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use bitty_ipc_api::error::ErrorClass;
use bitty_ipc_api::scope::Scope as TransportScope;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::audit::{
    AuditAction, AuditAggregate, AuditCounts, AuditEntry, AuditResult, AuditSession, AuditTarget,
    AuditTerminal,
};
use crate::audit_sink::{AuditSink, AuditWriter};
use crate::codes::RefusalCode;
use crate::contract::CAPABILITIES;
use crate::handshake::{
    ClientCorrelation, DaemonState, HandshakeContext, HandshakeResponse, NegotiateRequest,
    PeerIdentityFacts, ServerIdentity,
};
use crate::idempotency::{
    Admission, AppliedClass, AppliedResult, Fingerprint, IdempotencyKey, IdempotencyRefusal,
    IdempotencyResult, IdempotencyStore, IdempotencyTarget, IdempotentMethod, IdempotentRequest,
};
use crate::lease::{
    ConsumerStopCause, ControlAuthority, ControlToken, DaemonStopCause, LeaseRefusal, SessionHost,
    StopOutcome, Termination,
};
use crate::limits::{LimitId, Limits};
use crate::operator::{OperatorError, OperatorStore};
use crate::peer::DispatchBoundary;
use crate::registry::RegistryLimits;
use crate::registry::{
    AttachId, CleanupClass, DaemonInstanceId, LifecycleState, MonotonicMs, OwnerPeer,
    RegistryRefusal, SessionId, SessionSnapshot, TerminalId,
};
use crate::service::grants::{
    GrantId, GrantKind, GrantRefusal, GrantStore, MintError, RevokeOutcome,
};
use crate::service::protocol::Connection;
use crate::service::scopes::{ScopeRequirement, ServiceMethod, ServiceScope};
use crate::service::wire::params::{
    instance_field, object, required_string, required_u64, string_field,
};

/// The `daemon.stop` mode (`D-05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    /// Stop only when no session is active; otherwise `Denied/ActiveSessions`.
    Refuse,
    /// Notify, refuse new work, wait the drain grace, then terminate.
    Drain,
    /// Terminate immediately.
    Force,
}

impl StopMode {
    /// The frozen spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Drain => "drain",
            Self::Force => "force",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "refuse" => Some(Self::Refuse),
            "drain" => Some(Self::Drain),
            "force" => Some(Self::Force),
            _ => None,
        }
    }
}

impl fmt::Display for StopMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A static refusal.
///
/// `kind` selects the closed result class; `counts` carries the numeric facts a
/// refusal is allowed to expose. No variant carries caller text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    kind: RefusalKind,
    counts: Option<(u64, u64)>,
}

/// The closed refusal classes from the freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalKind {
    /// `Denied/<code>`.
    Denied(RefusalCode),
    /// The uniform target result and the well-formed unknown-method result.
    NotFound,
    /// A method name that violates the wire grammar.
    InvalidMethod,
    /// A malformed payload or field.
    InvalidRequest,
    /// A request deadline.
    Timeout {
        /// The request id that timed out.
        request_id: u64,
        /// The enforced bound.
        timeout_ms: u64,
    },
    /// An unsupported wire or service version.
    VersionMismatch,
    /// A peer credential or endpoint tamper failure.
    Unauthenticated,
    /// A frame, payload, or limit breach.
    LimitExceeded {
        /// The enforced bound.
        limit: u64,
        /// The observed value.
        actual: u64,
    },
    /// A bare invariant failure.
    Internal,
}

impl Refusal {
    /// `Denied/<code>`.
    #[must_use]
    pub const fn denied(code: RefusalCode) -> Self {
        Self {
            kind: RefusalKind::Denied(code),
            counts: None,
        }
    }

    /// The uniform `NotFound`.
    #[must_use]
    pub const fn not_found() -> Self {
        Self {
            kind: RefusalKind::NotFound,
            counts: None,
        }
    }

    /// A malformed payload or field.
    #[must_use]
    pub const fn invalid_request() -> Self {
        Self {
            kind: RefusalKind::InvalidRequest,
            counts: None,
        }
    }

    /// A grammar-invalid method name.
    #[must_use]
    pub const fn invalid_method() -> Self {
        Self {
            kind: RefusalKind::InvalidMethod,
            counts: None,
        }
    }

    /// A bare internal invariant failure.
    #[must_use]
    pub const fn internal() -> Self {
        Self {
            kind: RefusalKind::Internal,
            counts: None,
        }
    }

    /// An authentication failure that closes the connection.
    #[must_use]
    pub const fn unauthenticated() -> Self {
        Self {
            kind: RefusalKind::Unauthenticated,
            counts: None,
        }
    }

    /// A version mismatch that closes the connection.
    #[must_use]
    pub const fn version_mismatch() -> Self {
        Self {
            kind: RefusalKind::VersionMismatch,
            counts: None,
        }
    }

    /// A limit breach with its counts.
    #[must_use]
    pub const fn limit_exceeded(limit: u64, actual: u64) -> Self {
        Self {
            kind: RefusalKind::LimitExceeded { limit, actual },
            counts: Some((actual, limit)),
        }
    }

    /// A request-deadline refusal.
    #[must_use]
    pub const fn timeout(request_id: u64, timeout_ms: u64) -> Self {
        Self {
            kind: RefusalKind::Timeout {
                request_id,
                timeout_ms,
            },
            counts: None,
        }
    }

    /// A static refusal with numeric counts.
    #[must_use]
    pub const fn with_counts(mut self, actual: u64, limit: u64) -> Self {
        self.counts = Some((actual, limit));
        self
    }

    /// The closed kind.
    #[must_use]
    pub const fn kind(self) -> RefusalKind {
        self.kind
    }

    /// The static counts, when the refusal has them.
    #[must_use]
    pub const fn counts(self) -> Option<(u64, u64)> {
        self.counts
    }

    /// The `Denied` code, when this is a `Denied` refusal.
    #[must_use]
    pub const fn denied_code(self) -> Option<RefusalCode> {
        match self.kind {
            RefusalKind::Denied(code) => Some(code),
            _ => None,
        }
    }

    /// The static code recorded in audit for any refusal class.
    #[must_use]
    pub const fn audit_code(self) -> RefusalCode {
        match self.kind {
            RefusalKind::Denied(code) => code,
            RefusalKind::NotFound => RefusalCode::NotFound,
            RefusalKind::InvalidMethod => RefusalCode::InvalidMethod,
            RefusalKind::InvalidRequest => RefusalCode::InvalidRequest,
            RefusalKind::Timeout { .. } => RefusalCode::Timeout,
            RefusalKind::VersionMismatch => RefusalCode::VersionMismatch,
            RefusalKind::Unauthenticated => RefusalCode::Unauthenticated,
            RefusalKind::LimitExceeded { .. } => RefusalCode::LimitExceeded,
            RefusalKind::Internal => RefusalCode::Internal,
        }
    }

    /// Whether this refusal closes the connection.
    #[must_use]
    pub const fn closes_connection(self) -> bool {
        matches!(
            self.kind,
            RefusalKind::VersionMismatch
                | RefusalKind::Unauthenticated
                | RefusalKind::Denied(RefusalCode::AlreadyNegotiated)
        )
    }

    /// The `bitty-ipc` error class used on the wire.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self.kind {
            RefusalKind::Denied(_) => ErrorClass::Denied,
            RefusalKind::NotFound => ErrorClass::NotFound,
            RefusalKind::InvalidMethod | RefusalKind::InvalidRequest => ErrorClass::Validation,
            RefusalKind::Timeout { .. } => ErrorClass::Timeout,
            RefusalKind::VersionMismatch => ErrorClass::Validation,
            RefusalKind::Unauthenticated => ErrorClass::Unauthenticated,
            // `LimitExceeded` is a transport/validation class in the frozen
            // taxonomy, never a `Denied/<code>`: `denied/LimitExceeded` would
            // name a code outside the closed `Denied` set (D-07/D-14).
            RefusalKind::LimitExceeded { .. } => ErrorClass::Framing,
            RefusalKind::Internal => ErrorClass::Internal,
        }
    }

    /// The static `code` string. Never caller text.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self.kind {
            RefusalKind::Denied(code) => code.wire_name(),
            RefusalKind::NotFound => "NotFound",
            RefusalKind::InvalidMethod => "InvalidMethod",
            RefusalKind::InvalidRequest => "InvalidRequest",
            RefusalKind::Timeout { .. } => "Timeout",
            RefusalKind::VersionMismatch => "VersionMismatch",
            RefusalKind::Unauthenticated => "Unauthenticated",
            RefusalKind::LimitExceeded { .. } => "LimitExceeded",
            RefusalKind::Internal => "Internal",
        }
    }

    /// The static message. Never caller text.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self.kind {
            RefusalKind::Denied(_) => "request denied",
            RefusalKind::NotFound => "target not found",
            RefusalKind::InvalidMethod => "method name violates the wire grammar",
            RefusalKind::InvalidRequest => "request payload is malformed",
            RefusalKind::Timeout { .. } => "request deadline exceeded",
            RefusalKind::VersionMismatch => "service version is not accepted",
            RefusalKind::Unauthenticated => "authentication failed",
            RefusalKind::LimitExceeded { .. } => "request exceeds an advertised limit",
            RefusalKind::Internal => "internal failure",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for Refusal {}

/// The server-side facts the handshake needs that are not per-connection.
#[derive(Debug, Clone, Copy)]
pub struct ServerFacts {
    /// Observational endpoint facts.
    pub server_identity: ServerIdentity,
    /// Observational daemon pid.
    pub daemon_pid: u32,
}

/// A service construction failure. The daemon must not start on any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    /// The compiled limits were rejected by the state machine.
    InvalidLimits,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("service limits are invalid"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<OperatorError> for ServiceError {
    fn from(_: OperatorError) -> Self {
        Self::InvalidLimits
    }
}

/// The session host plus every bounded store the service surface needs.
pub struct Service {
    host: SessionHost,
    limits: Limits,
    audit: AuditSink<Box<dyn AuditWriter>>,
    operator: OperatorStore,
    grants: GrantStore,
    create_keys: IdempotencyStore,
    session_keys: BTreeMap<SessionId, IdempotencyStore>,
    create_key_by_session: BTreeMap<SessionId, IdempotencyKey>,
    initial_tokens: BTreeMap<SessionId, ControlToken>,
    pending_deliveries: BTreeMap<AttachId, ControlToken>,
    observer_grant_ids: BTreeMap<SessionId, ObserverGrantIds>,
    state: DaemonState,
    stop_requested: Option<StopMode>,
    drain_deadline_ms: Option<u64>,
}

/// Full observer-grant ids retained for keyed `session.grant` replays.
///
/// The idempotency descriptor keeps only a 64-bit reference; this table keeps
/// the full 128-bit id while the session is live, in mint order, bounded by the
/// per-session idempotency key cap.
#[derive(Debug, Default)]
struct ObserverGrantIds {
    ids: BTreeMap<u64, GrantId>,
    order: VecDeque<u64>,
}

impl Service {
    /// Build the service over one daemon lifetime.
    ///
    /// # Errors
    ///
    /// [`ServiceError::InvalidLimits`] when the state machine rejects the
    /// compiled bounds. The operator store and audit writer are constructed by
    /// the caller so their file effects stay at startup.
    pub fn new(
        limits: Limits,
        operator: OperatorStore,
        writer: Box<dyn AuditWriter>,
    ) -> Result<Self, ServiceError> {
        let host = SessionHost::new(
            RegistryLimits::from_limits(&limits),
            crate::lease::LeaseLimits::from_limits(&limits),
        )
        .map_err(|_| ServiceError::InvalidLimits)?;
        let create_keys = IdempotencyStore::create(&limits);
        let instance_id = host.instance_id();
        let audit = AuditSink::new(instance_id, &limits, writer);
        Ok(Self {
            host,
            grants: GrantStore::new(&limits),
            limits,
            audit,
            operator,
            create_keys,
            session_keys: BTreeMap::new(),
            create_key_by_session: BTreeMap::new(),
            initial_tokens: BTreeMap::new(),
            pending_deliveries: BTreeMap::new(),
            observer_grant_ids: BTreeMap::new(),
            state: DaemonState::Running,
            stop_requested: None,
            drain_deadline_ms: None,
        })
    }

    /// The daemon instance identity.
    #[must_use]
    pub const fn instance_id(&self) -> DaemonInstanceId {
        self.host.instance_id()
    }

    /// The effective limits for this run.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The daemon lifecycle state.
    #[must_use]
    pub const fn daemon_state(&self) -> DaemonState {
        self.state
    }

    /// The accepted stop request, when one was issued.
    #[must_use]
    pub const fn stop_requested(&self) -> Option<StopMode> {
        self.stop_requested
    }

    /// The drain deadline, when a drain was accepted.
    #[must_use]
    pub const fn drain_deadline_ms(&self) -> Option<u64> {
        self.drain_deadline_ms
    }

    /// Whether the audit sink is degraded.
    #[must_use]
    pub fn audit_degraded(&self) -> bool {
        self.audit.is_degraded()
    }

    /// The append/refusal/terminal counters of the audit ring.
    #[must_use]
    pub fn audit_counters(&self) -> crate::audit::AuditCounters {
        self.audit.counters()
    }

    /// Read-only access for adapters and tests.
    #[must_use]
    pub const fn host(&self) -> &SessionHost {
        &self.host
    }

    /// Mutable access for the server's timer and routing steps.
    pub fn host_mut(&mut self) -> &mut SessionHost {
        &mut self.host
    }

    /// Read-only grant store access.
    #[must_use]
    pub const fn grants(&self) -> &GrantStore {
        &self.grants
    }

    /// Drop retired grant records so the store cannot grow with total mints.
    ///
    /// The server calls this on its poll tick; a consumed host grant is kept
    /// until its TTL because `host_grant_bound` still needs it, while every
    /// other inactive record is dropped immediately.
    pub fn sweep_grants(&mut self, now: MonotonicMs) -> usize {
        self.grants.evict_expired(now.as_millis())
    }

    /// Record exactly one refusal that the server loop refuses before a method
    /// is resolved (envelope, frame, budget, or deadline closure).
    ///
    /// The dispatch path records its own refusals; this is the attribution for
    /// the transport stages that never reach a handler, so every refusal has
    /// one audit record and no path can suppress it.
    pub fn record_refusal(
        &mut self,
        connection: &Connection,
        action: AuditAction,
        refusal: Refusal,
        now: MonotonicMs,
    ) {
        let mut entry = AuditEntry::new(action)
            .actor(connection.audit_actor())
            .result(AuditResult::Refused(refusal.audit_code()))
            .target(AuditTarget::none());
        if let Some((actual, limit)) = refusal.counts() {
            entry = entry.counts(AuditCounts::new(actual, limit, None));
        }
        let _ = self.audit.record(entry, now);
    }

    /// Record the one aggregate for a wire-refusal budget window (`D-16`).
    ///
    /// A window that suppressed at least one refusal gets exactly one `wire`
    /// record: the suppressed count, the budget, and the bounded static code
    /// histogram. The record carries no refused frame and no caller label, and
    /// the aggregate's fixed-size histogram keeps it from rotating the sink
    /// beyond the budget the next window enforces.
    pub fn record_wire_aggregate(
        &mut self,
        connection: &Connection,
        aggregate: AuditAggregate,
        now: MonotonicMs,
    ) {
        let entry = AuditEntry::new(AuditAction::Wire)
            .actor(connection.audit_actor())
            .result(AuditResult::Refused(RefusalCode::RateLimited))
            .aggregate(aggregate);
        let _ = self.audit.record(entry, now);
    }

    /// How many sessions are non-terminal.
    #[must_use]
    pub fn active_sessions(&self, now: MonotonicMs) -> usize {
        self.host
            .snapshots(now)
            .filter(|snapshot| !snapshot.state.is_terminal())
            .count()
    }

    /// Take a pending control-token delivery for one of a connection's leases.
    ///
    /// The server routes it to the connection that owns the attach id; the
    /// token is delivered only on the target's own connection.
    #[must_use]
    pub fn take_pending_delivery(&mut self, attach_id: AttachId) -> Option<ControlToken> {
        self.pending_deliveries.remove(&attach_id)
    }

    /// The attach ids with a control token waiting for their owning connection.
    #[must_use]
    pub fn pending_delivery_targets(&self) -> Vec<AttachId> {
        self.pending_deliveries.keys().copied().collect()
    }

    /// Route every pending control token to the connection that owns it.
    ///
    /// The server calls this after each dispatch; a token is queued only on the
    /// connection that owns the target lease, so a transfer response can never
    /// carry the new controller's secret.
    pub fn route_deliveries(&mut self, connection: &mut Connection) -> usize {
        let mut routed = 0;
        for attach_id in self.pending_delivery_targets() {
            if connection.owns_lease(attach_id) {
                if let Some(token) = self.take_pending_delivery(attach_id) {
                    connection.queue_control_token(attach_id, token);
                    routed += 1;
                }
            }
        }
        routed
    }

    /// Negotiate one connection.
    ///
    /// # Errors
    ///
    /// - `Denied/AlreadyNegotiated` for a second negotiate on the connection;
    /// - `VersionMismatch` for an unsupported client version (closes);
    /// - `Unauthenticated` for a present but invalid operator capability
    ///   (closes). A host grant that cannot be consumed adds no scope and is
    ///   not an error.
    pub fn negotiate(
        &mut self,
        connection: &mut Connection,
        request: &NegotiateRequest,
        facts: ServerFacts,
        now: MonotonicMs,
    ) -> Result<HandshakeResponse, Refusal> {
        if connection.is_negotiated() {
            let refusal = Refusal::denied(RefusalCode::AlreadyNegotiated);
            self.record_refusal(connection, AuditAction::Negotiate, refusal, now);
            return Err(refusal);
        }
        let negotiated = match crate::handshake::negotiate(request, &self.limits) {
            Ok(negotiated) => negotiated,
            Err(_) => {
                let _ = self.audit.record(
                    AuditEntry::new(AuditAction::Negotiate)
                        .actor(connection.audit_actor())
                        .result(AuditResult::Refused(RefusalCode::VersionMismatch)),
                    now,
                );
                return Err(Refusal::version_mismatch());
            }
        };

        if let Some(presented) = request.operator_capability.as_ref() {
            if !self.operator.verify(presented) {
                let _ = self.audit.record(
                    AuditEntry::new(AuditAction::Negotiate)
                        .actor(connection.audit_actor())
                        .result(AuditResult::Refused(RefusalCode::Unauthenticated)),
                    now,
                );
                return Err(Refusal::unauthenticated());
            }
            for scope in ServiceScope::operator().iter() {
                connection.scopes_mut().service.insert(scope);
            }
        }

        if let Some(presented) = request.host_grant.as_ref() {
            if let Ok(consumed) = self.grants.consume(
                presented,
                GrantKind::Host,
                connection.peer_uid(),
                None,
                now.as_millis(),
            ) {
                connection
                    .scopes_mut()
                    .service
                    .insert(ServiceScope::SessionHost);
                connection.set_host_grant(*consumed.grant_id.as_bytes());
            }
        }

        connection.set_correlation_digest(
            request
                .client_correlation
                .as_ref()
                .map(ClientCorrelation::digest),
        );
        connection.mark_negotiated();

        let _ = self.audit.record(
            AuditEntry::new(AuditAction::Negotiate).actor(connection.audit_actor()),
            now,
        );

        Ok(HandshakeResponse::new(HandshakeContext {
            daemon_instance_id: self.instance_id(),
            daemon_pid: facts.daemon_pid,
            server_identity: facts.server_identity,
            peer_identity: PeerIdentityFacts::from_peer(&connection.peer().identity()),
            client_id: connection.client_id(),
            scopes: connection.scopes().clone(),
            limits: negotiated.limits,
            server_monotonic_ms: now.as_millis(),
            daemon_state: self.state,
        }))
    }

    /// Dispatch one resolved method.
    ///
    /// # Errors
    ///
    /// Returns a [`Refusal`] for every failure stage. Nothing is applied on a
    /// refusal.
    pub fn dispatch(
        &mut self,
        connection: &mut Connection,
        method: ServiceMethod,
        params: &Value,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let result = self.dispatch_inner(connection, method, params, now);
        if let Err(refusal) = result {
            // D-16: a refusal is attributed. The per-connection suppression
            // budget/aggregation is a tracked follow-up (README).
            self.record_refusal(connection, method.audit_action(), refusal, now);
        }
        result
    }

    fn dispatch_inner(
        &mut self,
        connection: &mut Connection,
        method: ServiceMethod,
        params: &Value,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        // Stage 2: exactly one negotiate per connection.
        if method == ServiceMethod::DaemonNegotiate {
            return Err(Refusal::denied(RefusalCode::AlreadyNegotiated));
        }
        // Stage 2: a stopping daemon serves reads only. The contract's
        // `daemon_state: stopping` means new sessions are refused, and a
        // draining daemon must not apply any other mutation either. A repeat
        // `daemon.stop` is exempt so it keeps its own `Denied/DaemonStopping`
        // answer from the handler.
        if self.state != DaemonState::Running
            && method != ServiceMethod::DaemonStop
            && method.is_mutating()
        {
            return Err(Refusal::denied(RefusalCode::DaemonStopping));
        }
        // Stage 3: capability.
        if let Some(capability) = method.capability() {
            if !CAPABILITIES.is_enabled(capability) {
                return Err(Refusal::denied(RefusalCode::UnsupportedCapability));
            }
        }
        // Stage 6: scope, before any target lookup. Create, attach, and detach
        // have mode-specific scope rules and run them inside their handlers.
        if method == ServiceMethod::SessionCreate {
            self.check_create_scope(connection)?;
        } else if method != ServiceMethod::SessionAttach && method != ServiceMethod::SessionDetach {
            self.check_scope(connection, method)?;
        }
        let fields = object(params)?;
        // Stage 8 (before authority): the instance binding.
        if let Some(instance) = instance_field(fields, "daemon_instance_id")? {
            if instance != self.instance_id() {
                return Err(Refusal::denied(RefusalCode::StaleInstance));
            }
        }
        match method {
            ServiceMethod::DaemonStatus => self.daemon_status(now),
            ServiceMethod::DaemonStop => self.daemon_stop(connection, fields, now),
            ServiceMethod::DaemonHostGrant => self.daemon_host_grant(connection, fields, now),
            ServiceMethod::DaemonOrphansVerify => {
                Err(Refusal::denied(RefusalCode::UnsupportedCapability))
            }
            ServiceMethod::SessionCreate => self.session_create(connection, fields, now),
            ServiceMethod::SessionStatus => self.session_status(connection, fields, now),
            ServiceMethod::SessionList => self.session_list(connection, now),
            ServiceMethod::SessionAttach => self.session_attach(connection, fields, now),
            ServiceMethod::SessionGrant => self.session_grant(connection, fields, now),
            ServiceMethod::SessionGrantRevoke => self.session_grant_revoke(connection, fields, now),
            ServiceMethod::SessionDetach => self.session_detach(connection, fields, now),
            ServiceMethod::SessionControlTransfer => {
                self.session_control_transfer(connection, fields, now)
            }
            ServiceMethod::SessionResize => Err(Refusal::denied(RefusalCode::ScopeViolation)),
            ServiceMethod::SessionAbort => self.session_abort(connection, fields, now),
            ServiceMethod::SessionKill => self.session_kill(connection, fields, now),
            ServiceMethod::LeaseHeartbeat => self.lease_heartbeat(connection, fields, now),
            ServiceMethod::DaemonNegotiate => Err(Refusal::denied(RefusalCode::AlreadyNegotiated)),
        }
    }

    /// The signal path: run the drain policy with the `host_shutdown` reason.
    ///
    /// # Errors
    ///
    /// A refusal here is an invariant failure; the process still exits.
    pub fn signal_stop(&mut self, now: MonotonicMs) -> Result<Value, Refusal> {
        self.state = DaemonState::Stopping;
        let terminated = self.terminate_all(DaemonStopCause::HostShutdown, now);
        self.stop_requested = Some(StopMode::Drain);
        Ok(json!({
            "accepted": true,
            "mode": "drain",
            "terminated": terminated,
            "reason": "host_shutdown",
        }))
    }

    // ── stage helpers ───────────────────────────────────────────────────────

    fn check_scope(&self, connection: &Connection, method: ServiceMethod) -> Result<(), Refusal> {
        match method.scope() {
            ScopeRequirement::None => Ok(()),
            ScopeRequirement::Service(any_of) => {
                if any_of.iter().any(|scope| connection.has_service(*scope)) {
                    Ok(())
                } else {
                    Err(Refusal::denied(RefusalCode::ScopeViolation))
                }
            }
            ScopeRequirement::Transport(any_of) => {
                if any_of
                    .iter()
                    .any(|scope| connection.scopes().has_transport(*scope))
                {
                    Ok(())
                } else {
                    Err(Refusal::denied(RefusalCode::ScopeViolation))
                }
            }
        }
    }

    /// The `session.create` two-authority order and its no-oracle rule.
    fn check_create_scope(&self, connection: &Connection) -> Result<(), Refusal> {
        let spawn = connection
            .scopes()
            .has_transport(TransportScope::ProcessSpawn);
        let host = connection.has_service(ServiceScope::SessionHost);
        match (spawn, host) {
            (false, false) => Err(Refusal::denied(RefusalCode::ScopeViolation)),
            (true, false) => Err(Refusal::denied(RefusalCode::ConsentRequired)),
            (false, true) => Err(Refusal::denied(RefusalCode::ScopeViolation)),
            (true, true) => Ok(()),
        }
    }

    fn recheck(&self, connection: &Connection, boundary: DispatchBoundary) -> Result<(), Refusal> {
        connection
            .recheck(boundary)
            .map_err(|_| Refusal::unauthenticated())
    }

    fn admit(&mut self) -> Result<(), Refusal> {
        self.audit
            .admit_mutation()
            .map_err(|_| Refusal::denied(RefusalCode::Internal))
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        action: AuditAction,
        connection: &Connection,
        session: Option<AuditSession>,
        target: AuditTarget,
        result: AuditResult,
        counts: Option<AuditCounts>,
        terminal: Option<AuditTerminal>,
        now: MonotonicMs,
    ) {
        let mut entry = AuditEntry::new(action)
            .actor(connection.audit_actor())
            .target(target)
            .result(result);
        if let Some(session) = session {
            entry = entry.session(session);
        }
        if let Some(counts) = counts {
            entry = entry.counts(counts);
        }
        if let Some(terminal) = terminal {
            entry = entry.terminal(terminal);
        }
        let _ = self.audit.record(entry, now);
    }

    fn audit_session(snapshot: &SessionSnapshot) -> AuditSession {
        AuditSession {
            session_id: snapshot.session_id,
            generation: snapshot.generation,
            state_revision: snapshot.state_revision,
        }
    }

    fn snapshot(
        &self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, Refusal> {
        self.host
            .snapshot(session_id, now)
            .ok_or_else(Refusal::not_found)
    }

    /// Whether the connection holds the operator control capability
    /// (`daemon.control`).
    ///
    /// This is the authority for control actions and the operator stop path; the
    /// all-session *view* is a distinct `daemon.inspect` right (see
    /// [`Self::has_operator_view`]). The operator capability always adds both,
    /// so splitting them changes no privilege, only which scope is spelled.
    fn is_operator(&self, connection: &Connection) -> bool {
        connection.has_service(ServiceScope::DaemonControl)
    }

    /// Whether the connection holds the operator all-session view
    /// (`daemon.inspect`).
    fn has_operator_view(&self, connection: &Connection) -> bool {
        connection.has_service(ServiceScope::DaemonInspect)
    }

    fn visible(
        &self,
        connection: &Connection,
        session_id: SessionId,
        token: Option<&ControlToken>,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, Refusal> {
        let snapshot = self.snapshot(session_id, now)?;
        if self.has_operator_view(connection) || self.holds_live_lease(connection, session_id, now)
        {
            return Ok(snapshot);
        }
        // A connection that held a lease on this session may read the immutable
        // terminal snapshot until retention, even though the terminal
        // transition detached every lease. This is not an oracle: a caller that
        // never held a lease still gets `NotFound`.
        if snapshot.state.is_terminal() && connection.has_lease_on(session_id) {
            return Ok(snapshot);
        }
        if let Some(token) = token {
            if self.host.verify_control_token(session_id, token) {
                return Ok(snapshot);
            }
        }
        Err(Refusal::not_found())
    }

    fn holds_live_lease(
        &self,
        connection: &Connection,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> bool {
        connection.leases().any(|(attach, owned)| {
            owned == session_id
                && self
                    .host
                    .lease(attach)
                    .is_some_and(|lease| !lease.is_expired_at(now))
        })
    }

    /// Whether a live session exists with no controller lease.
    ///
    /// Control methods consult this through [`Self::control_target`]: it
    /// distinguishes the contract's `Denied/InvalidControlToken` (a missing,
    /// stale, or rotated token at a vacant control) from the uniform
    /// `NotFound` for an unknown, terminal, or otherwise invisible target.
    fn live_vacant_control(&self, session_id: SessionId, now: MonotonicMs) -> bool {
        let Some(snapshot) = self.host.snapshot(session_id, now) else {
            return false;
        };
        if snapshot.state.is_terminal() {
            return false;
        }
        self.host
            .control_view(session_id, now)
            .is_some_and(|view| view.controller.is_none())
    }

    /// Resolve visibility for a control method and map the contract's one
    /// control-relation branch.
    ///
    /// A live session with a vacant control answers
    /// `Denied/InvalidControlToken` when the presented token is missing,
    /// stale, or rotated; an unknown, terminal, or occupied target keeps the
    /// uniform `NotFound`. The mapping is the contract's denial-order step 4,
    /// not a general visibility widening: `session.status` and `session.list`
    /// keep [`Self::visible`] and never map. The refusal echoes no token and
    /// applies nothing.
    fn control_target(
        &self,
        connection: &Connection,
        session_id: SessionId,
        token: Option<&ControlToken>,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, Refusal> {
        match self.visible(connection, session_id, token, now) {
            Ok(snapshot) => Ok(snapshot),
            Err(refusal)
                if refusal.kind() == RefusalKind::NotFound
                    && self.live_vacant_control(session_id, now) =>
            {
                Err(Refusal::denied(RefusalCode::InvalidControlToken))
            }
            Err(refusal) => Err(refusal),
        }
    }

    fn control_authority<'a>(
        &self,
        connection: &Connection,
        session_id: SessionId,
        token: Option<&'a ControlToken>,
        allow_operator: bool,
        now: MonotonicMs,
    ) -> Result<ControlAuthority<'a>, Refusal> {
        if allow_operator && self.is_operator(connection) {
            return Ok(ControlAuthority::operator());
        }
        let view = self
            .host
            .control_view(session_id, now)
            .ok_or_else(Refusal::not_found)?;
        let mut own_lease = false;
        for (attach, owned) in connection.leases() {
            if owned != session_id {
                continue;
            }
            if view.controller == Some(attach) {
                return Ok(ControlAuthority::lease(attach));
            }
            if self
                .host
                .lease(attach)
                .is_some_and(|lease| !lease.is_expired_at(now))
            {
                own_lease = true;
            }
        }
        if own_lease {
            return Err(Refusal::denied(RefusalCode::ControllerRequired));
        }
        if view.controller.is_some() {
            return Err(Refusal::denied(RefusalCode::ControlHeld));
        }
        match token {
            Some(token) => Ok(ControlAuthority::token(token)),
            None => Err(Refusal::denied(RefusalCode::InvalidControlToken)),
        }
    }

    /// Remember the full id of an observer grant minted for a keyed request.
    ///
    /// `AppliedResult::result_ref` is only the 64-bit prefix and the 128-byte
    /// idempotency descriptor has no spare bytes for the full 128-bit id, so the
    /// service keeps this bounded table for the lifetime of the session's
    /// applied keyed entries: a replay must answer the same `grant_id` even
    /// after the grant record has been swept or revoked. The table is capped at
    /// the store's per-session key bound (the most live applied entries the
    /// store can hold) and is dropped at the terminal transition.
    fn remember_observer_grant(&mut self, session_id: SessionId, grant_id: GrantId) {
        let reference = crate::service::grants::ref_of(grant_id.as_bytes());
        let limit = self
            .limits
            .effective_usize(LimitId::MaxIdempotencyKeysPerSession);
        let entry = self.observer_grant_ids.entry(session_id).or_default();
        if entry.ids.insert(reference, grant_id).is_none() {
            entry.order.push_back(reference);
        }
        while entry.order.len() > limit {
            if let Some(oldest) = entry.order.pop_front() {
                entry.ids.remove(&oldest);
            }
        }
    }

    /// The full id of a previously minted observer grant for this replay.
    fn remembered_observer_grant(&self, session_id: SessionId, reference: u64) -> Option<GrantId> {
        self.observer_grant_ids
            .get(&session_id)
            .and_then(|entry| entry.ids.get(&reference))
            .copied()
    }

    fn retire_session(&mut self, session_id: SessionId) {
        self.initial_tokens.remove(&session_id);
        self.observer_grant_ids.remove(&session_id);
        if let Some(key) = self.create_key_by_session.remove(&session_id) {
            let _ = self.create_keys.mark_control_claimed(key);
        }
        if let Some(mut store) = self.session_keys.remove(&session_id) {
            let _ = store.retire_session(session_id);
        }
        let targets: Vec<AttachId> = self
            .pending_deliveries
            .keys()
            .copied()
            .filter(|attach| self.host.session_for_lease(*attach) == Some(session_id))
            .collect();
        for attach in targets {
            self.pending_deliveries.remove(&attach);
        }
    }

    fn session_store(&mut self, session_id: SessionId) -> &mut IdempotencyStore {
        if !self.session_keys.contains_key(&session_id) {
            let store = IdempotencyStore::session(&self.limits, session_id);
            self.session_keys.insert(session_id, store);
        }
        self.session_keys
            .get_mut(&session_id)
            .expect("just inserted")
    }

    fn idempotency_key(
        &self,
        fields: Option<&Map<String, Value>>,
    ) -> Result<IdempotencyKey, Refusal> {
        let raw = required_string(fields, "idempotency_key")?;
        if raw.is_empty() || raw.len() > 128 {
            return Err(Refusal::invalid_request());
        }
        let mut hasher = Sha256::new();
        hasher.update(b"bittyd/idempotency-key/v1\0");
        hasher.update(raw.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        Ok(IdempotencyKey::from_bytes(key))
    }

    // ── daemon methods ─────────────────────────────────────────────────────

    fn daemon_status(&mut self, now: MonotonicMs) -> Result<Value, Refusal> {
        let snapshots: Vec<SessionSnapshot> = self.host.snapshots(now).collect();
        let mut counts = Counts::default();
        for snapshot in &snapshots {
            counts.add(snapshot.state);
        }
        let limits: Vec<Value> = self
            .limits
            .advertised()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "effective": entry.value.effective(),
                    "default": entry.value.default_value(),
                    "system_max": entry.value.system_max(),
                })
            })
            .collect();
        Ok(json!({
            "daemon_instance_id": self.instance_id().as_hex(),
            "daemon_state": self.state.as_str(),
            "active_sessions": counts.active(),
            "sessions": {
                "created": counts.created,
                "running": counts.running,
                "stopping": counts.stopping,
                "terminal": counts.terminal,
            },
            "audit_degraded": self.audit.is_degraded(),
            "drain_deadline_ms": self.drain_deadline_ms,
            "tcp_listeners": tcp_listener_count(),
            "limits": limits,
            "orphans": {"verify": "deferred"},
        }))
    }

    fn daemon_stop(
        &mut self,
        connection: &Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let mode = match string_field(fields, "mode")? {
            Some(value) => StopMode::parse(value).ok_or_else(Refusal::invalid_request)?,
            None => StopMode::Refuse,
        };
        if self.state == DaemonState::Stopping {
            return Err(Refusal::denied(RefusalCode::DaemonStopping));
        }
        let active = self.active_sessions(now);
        if mode == StopMode::Refuse && active > 0 {
            return Err(Refusal::denied(RefusalCode::ActiveSessions).with_counts(
                u64::try_from(active).unwrap_or(u64::MAX),
                self.limits.effective(LimitId::MaxSessions),
            ));
        }
        self.recheck(connection, DispatchBoundary::DaemonStop)?;
        self.admit()?;
        self.state = DaemonState::Stopping;
        if mode == StopMode::Drain {
            self.drain_deadline_ms = Some(
                now.as_millis()
                    .saturating_add(self.limits.effective(LimitId::DrainGraceMs)),
            );
        }
        let terminated = self.terminate_all(DaemonStopCause::DaemonStop, now);
        self.stop_requested = Some(mode);
        self.record(
            AuditAction::Stop,
            connection,
            None,
            AuditTarget::none(),
            AuditResult::Ok,
            Some(AuditCounts::new(terminated, 0, None)),
            None,
            now,
        );
        Ok(json!({
            "accepted": true,
            "mode": mode.as_str(),
            "active_sessions": active,
            "terminated": terminated,
            "drain_deadline_ms": self.drain_deadline_ms,
        }))
    }

    /// Terminate every non-terminal session with `cause` and return the count.
    fn terminate_all(&mut self, cause: DaemonStopCause, now: MonotonicMs) -> u64 {
        let session_ids: Vec<SessionId> = self
            .host
            .snapshots(now)
            .filter(|snapshot| !snapshot.state.is_terminal())
            .map(|snapshot| snapshot.session_id)
            .collect();
        let mut terminated = 0u64;
        for session_id in session_ids {
            let Ok(snapshot) = self.snapshot(session_id, now) else {
                continue;
            };
            if snapshot.state != LifecycleState::Created {
                continue;
            }
            let generation = snapshot.generation;
            if self
                .host
                .operator_stop(session_id, generation, cause, now)
                .is_err()
            {
                continue;
            }
            if let Ok(Termination::Completed(notice)) =
                self.host
                    .complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, now)
            {
                terminated = terminated.saturating_add(1);
                self.retire_session(session_id);
                let _ = self.audit.record(
                    AuditEntry::new(AuditAction::Stop)
                        .result(AuditResult::Ok)
                        .session(AuditSession {
                            session_id,
                            generation: notice.session.generation,
                            state_revision: notice.session.state_revision,
                        })
                        .target(AuditTarget::session(session_id))
                        .terminal(AuditTerminal {
                            state: notice.session.state,
                            reason: notice.reason,
                            exit: notice.exit,
                            cleanup: notice.cleanup,
                        }),
                    now,
                );
            }
        }
        terminated
    }

    fn daemon_host_grant(
        &mut self,
        connection: &Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let action = required_string(fields, "action")?;
        match action {
            "mint" => {
                self.recheck(connection, DispatchBoundary::HostGrantMint)?;
                self.admit()?;
                let minted = self
                    .grants
                    .mint_host(connection.peer_uid(), now.as_millis())
                    .map_err(map_mint)?;
                self.record(
                    AuditAction::HostGrant,
                    connection,
                    None,
                    AuditTarget::grant(*minted.grant_id.as_bytes()),
                    AuditResult::Ok,
                    None,
                    None,
                    now,
                );
                Ok(json!({
                    "grant_id": minted.grant_id.as_hex(),
                    "grant": crate::handshake::hex(minted.bearer.expose()),
                    "expires_at_ms": minted.expires_at_ms,
                }))
            }
            "revoke" => {
                let grant_id = parse_grant_id(fields)?;
                self.recheck(connection, DispatchBoundary::HostGrantRevoke)?;
                self.admit()?;
                let outcome = self
                    .grants
                    .revoke_kind(grant_id, GrantKind::Host, now.as_millis());
                self.record(
                    AuditAction::HostGrantRevoke,
                    connection,
                    None,
                    AuditTarget::grant(*grant_id.as_bytes()),
                    AuditResult::Ok,
                    None,
                    None,
                    now,
                );
                Ok(json!({
                    "grant_id": grant_id.as_hex(),
                    "revoked": outcome == RevokeOutcome::Revoked,
                    "not_active": outcome == RevokeOutcome::NotActive,
                }))
            }
            _ => Err(Refusal::invalid_request()),
        }
    }

    // ── session methods ────────────────────────────────────────────────────

    fn session_create(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let kind = string_field(fields, "kind")?.unwrap_or("terminal");
        match kind {
            "terminal" => {}
            "worker" => {
                return Err(Refusal::denied(RefusalCode::UnsupportedCapability));
            }
            _ => return Err(Refusal::invalid_request()),
        }
        // Stage 7: the consumed host consent must still be live.
        let grant_id = connection
            .host_grant()
            .map(GrantId::from_bytes)
            .ok_or_else(|| Refusal::denied(RefusalCode::ConsentRequired))?;
        if !self
            .grants
            .host_grant_bound(grant_id, connection.peer_uid(), now.as_millis())
        {
            return Err(Refusal::denied(RefusalCode::ConsentRequired));
        }

        let terminal_id = match string_field(fields, "terminal_id")? {
            Some(value) => {
                Some(TerminalId::supplied(value).map_err(|_| Refusal::invalid_request())?)
            }
            None => None,
        };
        let digest = terminal_digest(terminal_id.as_ref());
        let key = self.idempotency_key(fields)?;
        let fingerprint = fingerprint(&[
            "create",
            kind,
            terminal_id.as_ref().map(TerminalId::as_str).unwrap_or(""),
        ]);
        let request = IdempotentRequest::new(
            key,
            IdempotentMethod::Create,
            IdempotencyTarget::terminal_digest(digest),
            fingerprint,
            connection.audit_ref(),
        );
        match self.create_keys.begin(request, now) {
            Ok(Admission::Replay(IdempotencyResult::Applied(applied))) => {
                let snapshot = self.snapshot(applied.session_id, now)?;
                self.create_replay(connection, &snapshot, applied.control_claimed, now)
            }
            Ok(Admission::Replay(IdempotencyResult::Refused(code))) => Err(Refusal::denied(code)),
            Ok(Admission::Execute(reservation)) => {
                if let Err(error) = self
                    .recheck(connection, DispatchBoundary::CreateSession)
                    .and_then(|()| self.admit())
                {
                    let _ = self
                        .create_keys
                        .complete(reservation, IdempotencyResult::Refused(store_code(error)));
                    return Err(error);
                }
                let owner_peer = OwnerPeer::new(&connection.peer_uid().to_be_bytes())
                    .map_err(|_| Refusal::internal())?;
                let created = self.host.create(
                    crate::registry::CreateSession {
                        terminal_id,
                        owner_peer: Some(owner_peer),
                    },
                    now,
                );
                match created {
                    Ok(created) => {
                        let session_id = created.session.session_id;
                        self.initial_tokens
                            .insert(session_id, created.control_token);
                        self.create_key_by_session.insert(session_id, key);
                        let applied = AppliedResult::new(
                            AppliedClass::Created,
                            session_id,
                            created.session.generation,
                            created.session.state_revision,
                            0,
                            false,
                        );
                        let _ = self
                            .create_keys
                            .complete(reservation, IdempotencyResult::Applied(applied));
                        self.record(
                            AuditAction::Create,
                            connection,
                            Some(Self::audit_session(&created.session)),
                            AuditTarget::terminal_digest(digest),
                            AuditResult::Ok,
                            None,
                            None,
                            now,
                        );
                        let token = self
                            .initial_tokens
                            .get(&session_id)
                            .map(|token| crate::handshake::hex(token.expose_secret()));
                        Ok(json!({
                            "session": session_json(&created.session),
                            "control_token": token,
                            "replayed": false,
                        }))
                    }
                    Err(error) => {
                        let refusal = map_lease(error);
                        let _ = self
                            .create_keys
                            .complete(reservation, IdempotencyResult::Refused(store_code(refusal)));
                        self.record(
                            AuditAction::Create,
                            connection,
                            None,
                            AuditTarget::terminal_digest(digest),
                            AuditResult::Refused(store_code(refusal)),
                            None,
                            None,
                            now,
                        );
                        Err(refusal)
                    }
                }
            }
            Err(error) => Err(map_idempotency(error)),
        }
    }

    fn create_replay(
        &mut self,
        connection: &mut Connection,
        snapshot: &SessionSnapshot,
        control_claimed: bool,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let token = if control_claimed {
            None
        } else {
            self.initial_tokens
                .get(&snapshot.session_id)
                .map(|token| crate::handshake::hex(token.expose_secret()))
        };
        self.record(
            AuditAction::Create,
            connection,
            Some(Self::audit_session(snapshot)),
            AuditTarget::terminal_digest(terminal_digest(Some(&snapshot.terminal_id))),
            AuditResult::Ok,
            None,
            None,
            now,
        );
        Ok(json!({
            "session": session_json(snapshot),
            "control_token": token,
            "replayed": true,
        }))
    }

    fn session_status(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let session_id = parse_session_id(fields)?;
        let token = optional_control_token(fields)?;
        let snapshot = self.visible(connection, session_id, token.as_ref(), now)?;
        Ok(json!({"session": session_json(&snapshot)}))
    }

    fn session_list(
        &mut self,
        connection: &mut Connection,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let sessions: Vec<Value> = self
            .host
            .snapshots(now)
            .filter(|snapshot| {
                self.has_operator_view(connection)
                    || self.holds_live_lease(connection, snapshot.session_id, now)
            })
            .map(|snapshot| session_json(&snapshot))
            .collect();
        Ok(json!({"sessions": sessions}))
    }

    fn session_attach(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let role = required_string(fields, "attach_role")?;
        match role {
            "controller" => self.attach_controller(connection, fields, now),
            "observer" => self.attach_observer(connection, fields, now),
            _ => Err(Refusal::invalid_request()),
        }
    }

    fn attach_controller(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        if !connection.has_service(ServiceScope::SessionControl) {
            return Err(Refusal::denied(RefusalCode::ScopeViolation));
        }
        let session_id = parse_session_id(fields)?;
        let expected_generation = required_u64(fields, "expected_generation")?;
        let token = optional_control_token(fields)?;
        let key = self.idempotency_key(fields)?;
        let snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
        // A controller claim always needs the token. A missing token is the
        // contract's `Denied/InvalidControlToken` for a vacant control; a
        // visible target cannot be claimed without one either.
        let Some(token) = token else {
            return Err(Refusal::denied(RefusalCode::InvalidControlToken));
        };
        check_generation(&snapshot, expected_generation)?;
        let fingerprint = fingerprint(&[
            "attach",
            "controller",
            &session_id.as_hex(),
            &expected_generation.to_string(),
        ]);
        let request = IdempotentRequest::new(
            key,
            IdempotentMethod::Attach,
            IdempotencyTarget::session(session_id),
            fingerprint,
            connection.audit_ref(),
        );
        let admission = self.session_store(session_id).begin(request, now);
        match admission {
            Ok(Admission::Replay(IdempotencyResult::Applied(applied))) => {
                let snapshot = self.snapshot(applied.session_id, now)?;
                let attach = self
                    .host
                    .find_attach_by_ref(applied.session_id, applied.result_ref);
                Ok(json!({
                    "session": session_json(&snapshot),
                    "attach_id": attach.map(|attach| attach.as_hex()),
                    "role": "controller",
                    "replayed": true,
                    "not_attached": attach.is_none(),
                }))
            }
            Ok(Admission::Replay(IdempotencyResult::Refused(code))) => Err(Refusal::denied(code)),
            Ok(Admission::Execute(reservation)) => {
                if let Err(error) = self
                    .recheck(connection, DispatchBoundary::ControllerAttach)
                    .and_then(|()| self.admit())
                {
                    let _ = self
                        .session_store(session_id)
                        .complete(reservation, IdempotencyResult::Refused(store_code(error)));
                    return Err(error);
                }
                match self
                    .host
                    .claim_controller(session_id, expected_generation, &token, now)
                {
                    Ok(claimed) => {
                        let attach_id = claimed.lease.attach_id();
                        connection.register_lease(attach_id, session_id);
                        let result_ref = crate::service::grants::ref_of(attach_id.as_bytes());
                        let applied = AppliedResult::new(
                            AppliedClass::Attached,
                            session_id,
                            claimed.session.generation,
                            claimed.session.state_revision,
                            result_ref,
                            false,
                        );
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Applied(applied));
                        self.mark_control_claimed(session_id);
                        self.record(
                            AuditAction::Attach,
                            connection,
                            Some(Self::audit_session(&claimed.session)),
                            AuditTarget::attach(*attach_id.as_bytes()),
                            AuditResult::Ok,
                            None,
                            None,
                            now,
                        );
                        let rotated = crate::handshake::hex(claimed.control_token.expose_secret());
                        Ok(json!({
                            "session": session_json(&claimed.session),
                            "attach_id": attach_id.as_hex(),
                            "role": "controller",
                            "lease_expires_at_ms": claimed.lease.expires_at().as_millis(),
                            "control_token": rotated,
                            "replayed": false,
                        }))
                    }
                    Err(error) => {
                        let refusal = map_lease(error);
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Refused(store_code(refusal)));
                        self.record(
                            AuditAction::Attach,
                            connection,
                            Some(Self::audit_session(&snapshot)),
                            AuditTarget::session(session_id),
                            AuditResult::Refused(store_code(refusal)),
                            None,
                            None,
                            now,
                        );
                        Err(refusal)
                    }
                }
            }
            Err(error) => Err(map_idempotency(error)),
        }
    }

    fn attach_observer(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        if !connection.has_service(ServiceScope::SessionAttach) {
            return Err(Refusal::denied(RefusalCode::ScopeViolation));
        }
        let session_id = parse_session_id(fields)?;
        let grant = parse_bearer(fields, "observer_grant")?;
        // Stage 7: a valid single-use grant is the derived authority. Validate
        // before the lease exists so a refused attach does not burn it.
        self.grants
            .validate(
                &grant,
                GrantKind::Observer,
                connection.peer_uid(),
                Some(session_id),
                now.as_millis(),
            )
            .map_err(|_| Refusal::denied(RefusalCode::ObserverGrantRequired))?;
        self.snapshot(session_id, now)?;
        self.admit()?;
        match self.host.attach_observer(session_id, now) {
            Ok(attached) => {
                if self
                    .grants
                    .consume(
                        &grant,
                        GrantKind::Observer,
                        connection.peer_uid(),
                        Some(session_id),
                        now.as_millis(),
                    )
                    .is_err()
                {
                    // Single-threaded server: unreachable in practice, but a
                    // lease must never outlive a grant that was not consumed.
                    let _ = self.host.detach(
                        session_id,
                        attached.lease.attach_id(),
                        &ControlAuthority::lease(attached.lease.attach_id()),
                        now,
                    );
                    return Err(Refusal::denied(RefusalCode::ObserverGrantRequired));
                }
                let attach_id = attached.lease.attach_id();
                connection.register_lease(attach_id, session_id);
                self.record(
                    AuditAction::Attach,
                    connection,
                    Some(Self::audit_session(&attached.session)),
                    AuditTarget::attach(*attach_id.as_bytes()),
                    AuditResult::Ok,
                    None,
                    None,
                    now,
                );
                Ok(json!({
                    "session": session_json(&attached.session),
                    "attach_id": attach_id.as_hex(),
                    "role": "observer",
                    "lease_expires_at_ms": attached.lease.expires_at().as_millis(),
                    "replayed": false,
                }))
            }
            Err(error) => Err(map_lease(error)),
        }
    }

    fn session_grant(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let session_id = parse_session_id(fields)?;
        let key = self.idempotency_key(fields)?;
        let token = optional_control_token(fields)?;
        let snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
        let _authority =
            self.control_authority(connection, session_id, token.as_ref(), true, now)?;
        let fingerprint = fingerprint(&["grant", &session_id.as_hex()]);
        let request = IdempotentRequest::new(
            key,
            IdempotentMethod::Grant,
            IdempotencyTarget::session(session_id),
            fingerprint,
            connection.audit_ref(),
        );
        let admission = self.session_store(session_id).begin(request, now);
        match admission {
            Ok(Admission::Replay(IdempotencyResult::Applied(applied))) => {
                // The grant record may have been swept or revoked since the
                // mint; the applied entry must still replay its original
                // `grant_id`. The full id is remembered for the entry's
                // lifetime; `find_by_ref` is the fallback for a record that is
                // still present.
                let grant_id = self
                    .remembered_observer_grant(applied.session_id, applied.result_ref)
                    .or_else(|| self.grants.find_by_ref(applied.result_ref));
                let Some(grant_id) = grant_id else {
                    return Err(Refusal::internal());
                };
                let bearer =
                    self.grants
                        .replayable_bearer(grant_id, GrantKind::Observer, now.as_millis());
                Ok(json!({
                    "grant_id": grant_id.as_hex(),
                    "grant": bearer.as_ref().map(|bearer| crate::handshake::hex(bearer.expose())),
                    "grant_active": bearer.is_some(),
                    "session_id": session_id.as_hex(),
                    "replayed": true,
                }))
            }
            Ok(Admission::Replay(IdempotencyResult::Refused(code))) => Err(Refusal::denied(code)),
            Ok(Admission::Execute(reservation)) => {
                if let Err(error) = self
                    .recheck(connection, DispatchBoundary::ObserverGrantMint)
                    .and_then(|()| self.admit())
                {
                    let _ = self
                        .session_store(session_id)
                        .complete(reservation, IdempotencyResult::Refused(store_code(error)));
                    return Err(error);
                }
                let minted =
                    self.grants
                        .mint_observer(session_id, connection.peer_uid(), now.as_millis());
                match minted {
                    Ok(minted) => {
                        self.remember_observer_grant(session_id, minted.grant_id);
                        let result_ref = crate::service::grants::ref_of(minted.grant_id.as_bytes());
                        let applied = AppliedResult::new(
                            AppliedClass::GrantMinted,
                            session_id,
                            snapshot.generation,
                            snapshot.state_revision,
                            result_ref,
                            false,
                        );
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Applied(applied));
                        self.record(
                            AuditAction::Grant,
                            connection,
                            Some(Self::audit_session(&snapshot)),
                            AuditTarget::grant(*minted.grant_id.as_bytes()),
                            AuditResult::Ok,
                            None,
                            None,
                            now,
                        );
                        Ok(json!({
                            "grant_id": minted.grant_id.as_hex(),
                            "grant": crate::handshake::hex(minted.bearer.expose()),
                            "expires_at_ms": minted.expires_at_ms,
                            "session_id": session_id.as_hex(),
                            "replayed": false,
                        }))
                    }
                    Err(error) => {
                        let refusal = map_mint(error);
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Refused(store_code(refusal)));
                        Err(refusal)
                    }
                }
            }
            Err(error) => Err(map_idempotency(error)),
        }
    }

    fn session_grant_revoke(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let grant_id = parse_grant_id(fields)?;
        // Stage 7/8 first: a retained record (even one that is consumed,
        // expired, or revoked) still names its session, so the uniform
        // visibility and authority stages run before `not_active` is reported.
        // An unknown id has no target to look up, so it can only answer
        // `not_active`; the id is 128 bits and reveals nothing the caller did
        // not already hold.
        if let Some(session_id) = self.grants.observer_grant_session_retained(grant_id) {
            let token = optional_control_token(fields)?;
            let _snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
            let _authority =
                self.control_authority(connection, session_id, token.as_ref(), true, now)?;
            self.recheck(connection, DispatchBoundary::ObserverGrantRevoke)?;
            self.admit()?;
        }
        let outcome = self
            .grants
            .revoke_kind(grant_id, GrantKind::Observer, now.as_millis());
        self.record(
            AuditAction::GrantRevoke,
            connection,
            None,
            AuditTarget::grant(*grant_id.as_bytes()),
            AuditResult::Ok,
            None,
            None,
            now,
        );
        Ok(json!({
            "grant_id": grant_id.as_hex(),
            "revoked": outcome == RevokeOutcome::Revoked,
            "not_active": outcome == RevokeOutcome::NotActive,
        }))
    }

    fn session_detach(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let target = parse_attach_id(fields)?;
        let own = connection.owns_lease(target);
        // Stage 6, own-lease exception: detecting your own lease needs no
        // scope; detaching another connection's lease needs `session.control`
        // or `daemon.control`. The check stays ahead of the target lookup.
        if !own
            && !(connection.has_service(ServiceScope::SessionControl)
                || connection.has_service(ServiceScope::DaemonControl))
        {
            return Err(Refusal::denied(RefusalCode::ScopeViolation));
        }
        let token = optional_control_token(fields)?;
        let session_id;
        if own {
            session_id = connection
                .lease_session(target)
                .ok_or_else(Refusal::not_found)?;
        } else {
            session_id = self
                .host
                .session_for_lease(target)
                .ok_or_else(Refusal::not_found)?;
            let _snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
            self.recheck(connection, DispatchBoundary::DetachOther)?;
        }
        let authority = if own {
            ControlAuthority::lease(target)
        } else {
            self.control_authority(connection, session_id, token.as_ref(), true, now)?
        };
        self.admit()?;
        match self.host.detach(session_id, target, &authority, now) {
            Ok(detached) => {
                connection.forget_lease(target);
                self.record(
                    AuditAction::Detach,
                    connection,
                    Some(Self::audit_session(&detached.session)),
                    AuditTarget::attach(*target.as_bytes()),
                    AuditResult::Ok,
                    None,
                    None,
                    now,
                );
                Ok(json!({
                    "session": session_json(&detached.session),
                    "attach_id": target.as_hex(),
                    "was_attached": detached.was_attached,
                    "not_attached": !detached.was_attached,
                    "revoked": detached.revoked,
                }))
            }
            Err(error) => Err(map_lease(error)),
        }
    }

    fn session_control_transfer(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let session_id = parse_session_id(fields)?;
        let target = parse_attach_id(fields)?;
        let expected_generation = required_u64(fields, "expected_generation")?;
        let token = optional_control_token(fields)?;
        let key = self.idempotency_key(fields)?;
        let snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
        check_generation(&snapshot, expected_generation)?;
        let authority =
            self.control_authority(connection, session_id, token.as_ref(), false, now)?;
        let fingerprint = fingerprint(&[
            "transfer",
            &session_id.as_hex(),
            &target.as_hex(),
            &expected_generation.to_string(),
        ]);
        let request = IdempotentRequest::new(
            key,
            IdempotentMethod::Transfer,
            IdempotencyTarget::session(session_id),
            fingerprint,
            connection.audit_ref(),
        );
        let admission = self.session_store(session_id).begin(request, now);
        match admission {
            Ok(Admission::Replay(IdempotencyResult::Applied(applied))) => {
                let snapshot = self.snapshot(applied.session_id, now)?;
                let controller = self
                    .host
                    .find_attach_by_ref(applied.session_id, applied.result_ref);
                Ok(json!({
                    "session": session_json(&snapshot),
                    "controller": controller.map(|attach| attach.as_hex()),
                    "replayed": true,
                    "not_attached": controller.is_none(),
                }))
            }
            Ok(Admission::Replay(IdempotencyResult::Refused(code))) => Err(Refusal::denied(code)),
            Ok(Admission::Execute(reservation)) => {
                if let Err(error) = self
                    .recheck(connection, DispatchBoundary::ControlTransfer)
                    .and_then(|()| self.admit())
                {
                    let _ = self
                        .session_store(session_id)
                        .complete(reservation, IdempotencyResult::Refused(store_code(error)));
                    return Err(error);
                }
                match self.host.transfer_control(
                    session_id,
                    expected_generation,
                    &authority,
                    target,
                    now,
                ) {
                    Ok(transferred) => {
                        let controller = transferred.controller;
                        let delivery = transferred.delivery;
                        let result_ref = crate::service::grants::ref_of(controller.as_bytes());
                        let applied = AppliedResult::new(
                            AppliedClass::Transferred,
                            session_id,
                            transferred.session.generation,
                            transferred.session.state_revision,
                            result_ref,
                            false,
                        );
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Applied(applied));
                        self.mark_control_claimed(session_id);
                        // The rotated token goes to the target connection only.
                        if let Ok(token) = delivery.take_for(controller) {
                            self.pending_deliveries.insert(controller, token);
                        }
                        self.record(
                            AuditAction::Transfer,
                            connection,
                            Some(Self::audit_session(&transferred.session)),
                            AuditTarget::attach(*controller.as_bytes()),
                            AuditResult::Ok,
                            None,
                            None,
                            now,
                        );
                        Ok(json!({
                            "session": session_json(&transferred.session),
                            "controller": controller.as_hex(),
                            "previous_controller": transferred
                                .previous_controller
                                .map(AttachId::as_hex),
                            "replayed": false,
                        }))
                    }
                    Err(error) => {
                        let refusal = map_lease(error);
                        let _ = self
                            .session_store(session_id)
                            .complete(reservation, IdempotencyResult::Refused(store_code(refusal)));
                        Err(refusal)
                    }
                }
            }
            Err(error) => Err(map_idempotency(error)),
        }
    }

    fn session_abort(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let session_id = parse_session_id(fields)?;
        let expected_generation = required_u64(fields, "expected_generation")?;
        let reason = required_string(fields, "reason_class")?;
        let token = optional_control_token(fields)?;
        let snapshot = self.control_target(connection, session_id, token.as_ref(), now)?;
        if snapshot.state.is_terminal() {
            return Ok(json!({
                "session": session_json(&snapshot),
                "accepted": false,
                "not_running": true,
            }));
        }
        check_generation(&snapshot, expected_generation)?;
        let outcome = match reason {
            "cancel" | "timeout" => {
                let authority =
                    self.control_authority(connection, session_id, token.as_ref(), false, now)?;
                self.recheck(connection, DispatchBoundary::Abort)?;
                self.admit()?;
                let cause = if reason == "cancel" {
                    ConsumerStopCause::Cancel
                } else {
                    ConsumerStopCause::Timeout
                };
                self.host
                    .abort(session_id, expected_generation, &authority, cause, now)
            }
            "operator" => {
                if !self.is_operator(connection) {
                    return Err(Refusal::denied(RefusalCode::ScopeViolation));
                }
                self.recheck(connection, DispatchBoundary::Abort)?;
                self.admit()?;
                self.host.operator_stop(
                    session_id,
                    expected_generation,
                    DaemonStopCause::OperatorAbort,
                    now,
                )
            }
            _ => return Err(Refusal::invalid_request()),
        };
        let outcome = outcome.map_err(map_lease)?;
        let terminal_reason = match reason {
            "cancel" => crate::registry::TerminalReason::ConsumerCancel,
            "timeout" => crate::registry::TerminalReason::ConsumerTimeout,
            _ => crate::registry::TerminalReason::OperatorAbort,
        };
        self.finish_stop(
            connection,
            session_id,
            outcome,
            AuditAction::Abort,
            terminal_reason,
            now,
        )
    }

    fn session_kill(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let session_id = parse_session_id(fields)?;
        let expected_generation = required_u64(fields, "expected_generation")?;
        let snapshot = self.visible(connection, session_id, None, now)?;
        if snapshot.state.is_terminal() {
            return Ok(json!({
                "session": session_json(&snapshot),
                "accepted": false,
                "not_running": true,
            }));
        }
        check_generation(&snapshot, expected_generation)?;
        self.recheck(connection, DispatchBoundary::Kill)?;
        self.admit()?;
        let outcome = self
            .host
            .kill(session_id, expected_generation, now)
            .map_err(map_lease)?;
        self.finish_stop(
            connection,
            session_id,
            outcome,
            AuditAction::Kill,
            crate::registry::TerminalReason::OperatorAbort,
            now,
        )
    }

    fn finish_stop(
        &mut self,
        connection: &Connection,
        session_id: SessionId,
        outcome: StopOutcome,
        action: AuditAction,
        terminal_reason: crate::registry::TerminalReason,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        match outcome {
            StopOutcome::AlreadyTerminal { session } => Ok(json!({
                "session": session_json(&session),
                "accepted": false,
                "not_running": true,
            })),
            StopOutcome::PreBindCancel(accepted) => {
                // No worker exists in this slice, so the generation-0 cancel
                // completes in the same request with `not_started` cleanup.
                let terminated = self
                    .host
                    .complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, now)
                    .map_err(map_lease)?;
                let snapshot = match terminated {
                    Termination::Completed(notice) => {
                        self.retire_session(session_id);
                        let terminal = Some(AuditTerminal {
                            state: notice.session.state,
                            reason: notice.reason,
                            exit: notice.exit,
                            cleanup: notice.cleanup,
                        });
                        self.record(
                            action,
                            connection,
                            Some(Self::audit_session(&notice.session)),
                            AuditTarget::session(session_id),
                            AuditResult::Ok,
                            None,
                            terminal,
                            now,
                        );
                        notice.session
                    }
                    Termination::AlreadyTerminal { session } => session,
                };
                Ok(json!({
                    "session": session_json(&snapshot),
                    "accepted": true,
                    "reason": accepted.reason.as_str(),
                    "stage": "not_started",
                    "cleanup": "not_started",
                }))
            }
            StopOutcome::Accepted(accepted) => {
                self.record(
                    action,
                    connection,
                    Some(Self::audit_session(&accepted.session)),
                    AuditTarget::session(session_id),
                    AuditResult::Ok,
                    None,
                    None,
                    now,
                );
                Ok(json!({
                    "session": session_json(&accepted.session),
                    "accepted": true,
                    "reason": terminal_reason.as_str(),
                    "stage": accepted.stage.as_str(),
                    "deadline_ms": accepted.deadline.as_millis(),
                }))
            }
        }
    }

    fn lease_heartbeat(
        &mut self,
        connection: &mut Connection,
        fields: Option<&Map<String, Value>>,
        now: MonotonicMs,
    ) -> Result<Value, Refusal> {
        let attach_id = parse_attach_id(fields)?;
        if !connection.owns_lease(attach_id) {
            return Err(Refusal::not_found());
        }
        let heartbeat = self.host.heartbeat(attach_id, now).map_err(map_lease)?;
        let token = connection
            .take_control_token(attach_id)
            .map(|token| crate::handshake::hex(token.expose_secret()));
        self.record(
            AuditAction::Heartbeat,
            connection,
            Some(Self::audit_session(&heartbeat.session)),
            AuditTarget::attach(*attach_id.as_bytes()),
            AuditResult::Ok,
            None,
            None,
            now,
        );
        Ok(json!({
            "session": session_json(&heartbeat.session),
            "attach_id": attach_id.as_hex(),
            "lease_expires_at_ms": heartbeat.lease.expires_at().as_millis(),
            "control_token": token,
        }))
    }

    fn mark_control_claimed(&mut self, session_id: SessionId) {
        if let Some(key) = self.create_key_by_session.remove(&session_id) {
            let _ = self.create_keys.mark_control_claimed(key);
        }
        self.initial_tokens.remove(&session_id);
    }
}

// ── rendering and parsing helpers ──────────────────────────────────────────

fn session_json(snapshot: &SessionSnapshot) -> Value {
    let mut value = json!({
        "session_id": snapshot.session_id.as_hex(),
        "kind": snapshot.kind.as_str(),
        "terminal_id": snapshot.terminal_id.as_str(),
        "state": snapshot.state.as_str(),
        "generation": snapshot.generation,
        "state_revision": snapshot.state_revision,
        "attach_count": snapshot.attach_count,
        "attached": snapshot.attached,
        "created_at_ms": snapshot.created_at_ms.as_millis(),
        "last_seen_ms": snapshot.last_seen_ms.as_millis(),
    });
    if let Some(map) = value.as_object_mut() {
        if snapshot.state.is_terminal() {
            map.insert(
                "terminal_reason".to_owned(),
                snapshot
                    .terminal_reason
                    .map(|reason| Value::String(reason.as_str().to_owned()))
                    .unwrap_or(Value::Null),
            );
            map.insert(
                "cleanup".to_owned(),
                snapshot
                    .cleanup
                    .map(|cleanup| Value::String(cleanup.as_str().to_owned()))
                    .unwrap_or(Value::Null),
            );
        }
    }
    value
}

fn check_generation(snapshot: &SessionSnapshot, expected: u64) -> Result<(), Refusal> {
    if snapshot.generation == expected {
        Ok(())
    } else {
        Err(Refusal::denied(RefusalCode::StaleGeneration)
            .with_counts(snapshot.generation, snapshot.state_revision))
    }
}

fn terminal_digest(terminal_id: Option<&TerminalId>) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"bittyd/terminal-id-digest/v1\0");
    match terminal_id {
        Some(id) => hasher.update(id.as_str().as_bytes()),
        None => hasher.update([0u8]),
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn fingerprint(parts: &[&str]) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(b"bittyd/request-fingerprint/v1\0");
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    Fingerprint::from_bytes(out)
}

fn store_code(refusal: Refusal) -> RefusalCode {
    refusal.denied_code().unwrap_or(RefusalCode::Internal)
}

fn parse_session_id(fields: Option<&Map<String, Value>>) -> Result<SessionId, Refusal> {
    required_string(fields, "session_id")?
        .parse()
        .map_err(|_| Refusal::invalid_request())
}

fn parse_attach_id(fields: Option<&Map<String, Value>>) -> Result<AttachId, Refusal> {
    required_string(fields, "attach_id")?
        .parse()
        .map_err(|_| Refusal::invalid_request())
}

fn parse_grant_id(fields: Option<&Map<String, Value>>) -> Result<GrantId, Refusal> {
    GrantId::parse(required_string(fields, "grant_id")?).map_err(|_| Refusal::invalid_request())
}

fn parse_bearer(
    fields: Option<&Map<String, Value>>,
    key: &str,
) -> Result<crate::handshake::SecretBearer, Refusal> {
    crate::handshake::SecretBearer::parse(required_string(fields, key)?)
        .map_err(|_| Refusal::invalid_request())
}

fn optional_control_token(
    fields: Option<&Map<String, Value>>,
) -> Result<Option<ControlToken>, Refusal> {
    match string_field(fields, "control_token")? {
        Some(value) => {
            let bearer = crate::handshake::SecretBearer::parse(value)
                .map_err(|_| Refusal::invalid_request())?;
            Ok(Some(ControlToken::from_bytes(*bearer.expose())))
        }
        None => Ok(None),
    }
}

/// Count this process's own TCP listening sockets.
///
/// The daemon clears its dumpable flag, so an external same-uid observer
/// cannot enumerate `/proc/<pid>/fd`; `daemon.status` reports the count the
/// process can verify about itself from `/proc/self` instead. The scan is
/// bounded by the process fd table and the namespace TCP tables, and returns
/// `None` outside Linux or when `/proc/self` is unavailable. It is an
/// observability fact, never an authority input.
fn tcp_listener_count() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut sockets = std::collections::BTreeSet::new();
        let entries = std::fs::read_dir("/proc/self/fd").ok()?;
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(text) = target.to_str() else {
                continue;
            };
            if let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                sockets.insert(inode.to_owned());
            }
        }
        let mut count = 0u32;
        for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
            let Ok(content) = std::fs::read_to_string(table) else {
                return None;
            };
            for line in content.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 10 && fields[3] == "0A" && sockets.contains(fields[9]) {
                    count = count.saturating_add(1);
                }
            }
        }
        Some(count)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn map_idempotency(error: IdempotencyRefusal) -> Refusal {
    match error {
        IdempotencyRefusal::Conflict { .. }
        | IdempotencyRefusal::InFlight
        | IdempotencyRefusal::ReplayOnOtherConnection => {
            Refusal::denied(RefusalCode::IdempotencyConflict)
        }
        IdempotencyRefusal::Capacity { max_keys, .. } => {
            Refusal::denied(RefusalCode::IdempotencyCapacity)
                .with_counts(0, u64::try_from(max_keys).unwrap_or(u64::MAX))
        }
        IdempotencyRefusal::WrongScope { .. }
        | IdempotencyRefusal::UnknownReservation
        | IdempotencyRefusal::WrongStore
        | IdempotencyRefusal::WrongSession { .. }
        | IdempotencyRefusal::AlreadyCompleted
        | IdempotencyRefusal::ControlNotApplicable
        | IdempotencyRefusal::ResultMismatch { .. }
        | IdempotencyRefusal::SequenceExhausted => Refusal::internal(),
    }
}

fn map_mint(error: MintError) -> Refusal {
    match error {
        MintError::Refused(GrantRefusal::Capacity { limit, .. }) => {
            Refusal::limit_exceeded(u64::try_from(limit).unwrap_or(u64::MAX), 0)
        }
        MintError::Refused(GrantRefusal::Unusable) | MintError::Error(_) => Refusal::internal(),
    }
}

fn map_registry(error: RegistryRefusal) -> Refusal {
    match error {
        RegistryRefusal::MaxSessions { limit } => {
            Refusal::limit_exceeded(u64::try_from(limit).unwrap_or(u64::MAX), 0)
        }
        RegistryRefusal::SessionRecordTooLarge { .. } => Refusal::invalid_request(),
        RegistryRefusal::TerminalAlreadyHosted => {
            Refusal::denied(RefusalCode::TerminalAlreadyHosted)
        }
        RegistryRefusal::NotFound => Refusal::not_found(),
        RegistryRefusal::NotRunning { .. } => Refusal::denied(RefusalCode::NotRunning),
        RegistryRefusal::IdGenerationFailed | RegistryRefusal::IdCollision => Refusal::internal(),
        RegistryRefusal::AlreadyBound { .. }
        | RegistryRefusal::InvalidTransition { .. }
        | RegistryRefusal::ReasonNotAllowed { .. }
        | RegistryRefusal::CleanupNotStarted { .. }
        | RegistryRefusal::RevisionOverflow
        | RegistryRefusal::GenerationOverflow
        | RegistryRefusal::AttachmentOverflow
        | RegistryRefusal::InvalidLimits { .. } => Refusal::internal(),
    }
}

fn map_lease(error: LeaseRefusal) -> Refusal {
    match error {
        LeaseRefusal::SessionNotFound | LeaseRefusal::NotAttached => Refusal::not_found(),
        LeaseRefusal::NotRunning { .. } => Refusal::denied(RefusalCode::NotRunning),
        LeaseRefusal::SessionStopping | LeaseRefusal::CancelPending => {
            Refusal::denied(RefusalCode::SessionStopping)
        }
        LeaseRefusal::StaleGeneration {
            current_generation,
            current_revision,
        } => Refusal::denied(RefusalCode::StaleGeneration)
            .with_counts(current_generation, current_revision),
        LeaseRefusal::ControllerRequired => Refusal::denied(RefusalCode::ControllerRequired),
        LeaseRefusal::ControlHeld => Refusal::denied(RefusalCode::ControlHeld),
        LeaseRefusal::InvalidControlToken => Refusal::denied(RefusalCode::InvalidControlToken),
        LeaseRefusal::TargetNotObserver => Refusal::denied(RefusalCode::ScopeViolation),
        LeaseRefusal::MaxAttaches { limit } => {
            Refusal::limit_exceeded(u64::try_from(limit).unwrap_or(u64::MAX), 0)
        }
        LeaseRefusal::OperatorPathRequired
        | LeaseRefusal::CancelNotPending
        | LeaseRefusal::UnexpectedExit { .. }
        | LeaseRefusal::TerminalOutcome(_)
        | LeaseRefusal::RevisionOverflow
        | LeaseRefusal::GenerationOverflow
        | LeaseRefusal::EntropyUnavailable
        | LeaseRefusal::IdCollision
        | LeaseRefusal::InvalidLimits { .. } => Refusal::internal(),
        LeaseRefusal::Registry(refusal) => map_registry(refusal),
    }
}

#[derive(Default)]
struct Counts {
    created: u64,
    running: u64,
    stopping: u64,
    terminal: u64,
}

impl Counts {
    fn add(&mut self, state: LifecycleState) {
        match state {
            LifecycleState::Created => self.created += 1,
            LifecycleState::Running => self.running += 1,
            LifecycleState::Stopping => self.stopping += 1,
            LifecycleState::Exited | LifecycleState::Failed | LifecycleState::Terminated => {
                self.terminal += 1;
            }
        }
    }

    fn active(&self) -> u64 {
        self.created + self.running + self.stopping
    }
}

#[cfg(all(test, target_os = "linux"))]
// The dispatch tests build a real PeerGuard from /proc; the served loop is
// Linux-only (D-15), so they do not run on the other tested cells.
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use bitty_ipc_api::scope::ScopeSet;

    use crate::audit_sink::AuditSinkError;
    use crate::handshake::{ClientId, EffectiveScopes, EndpointKind, SecretBearer};
    use crate::limits::LimitOverride;
    use crate::peer::PeerGuard;
    use crate::service::scopes::{ServiceScope, ServiceScopeSet};

    struct TestWriter {
        fail: Arc<AtomicBool>,
        lines: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TestWriter {
        fn new() -> (Self, Arc<AtomicBool>) {
            let fail = Arc::new(AtomicBool::new(false));
            (
                Self {
                    fail: fail.clone(),
                    lines: Arc::new(Mutex::new(Vec::new())),
                },
                fail,
            )
        }
    }

    impl AuditWriter for TestWriter {
        fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AuditSinkError::Io);
            }
            self.lines.lock().expect("lines").push(line.to_vec());
            Ok(())
        }

        fn sync(&mut self) -> Result<(), AuditSinkError> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AuditSinkError::Io);
            }
            Ok(())
        }
    }

    fn service() -> (Service, tempfile::TempDir) {
        service_with(Limits::default())
    }

    fn service_with(limits: Limits) -> (Service, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod");
        let operator = OperatorStore::create_in(dir.path()).expect("operator store");
        let (writer, _fail) = TestWriter::new();
        let service = Service::new(limits, operator, Box::new(writer)).expect("service");
        (service, dir)
    }

    fn facts() -> ServerFacts {
        ServerFacts {
            server_identity: ServerIdentity {
                uid: nix::unistd::geteuid().as_raw(),
                endpoint_kind: EndpointKind::UnixSocket,
                endpoint_owner: nix::unistd::geteuid().as_raw(),
                endpoint_mode: 0o600,
            },
            daemon_pid: std::process::id(),
        }
    }

    fn connection(scopes: ServiceScopeSet, spawn: bool) -> Connection {
        let mut effective = EffectiveScopes {
            service: scopes,
            transport: ScopeSet::new(),
        };
        if spawn {
            effective.transport.insert(TransportScope::ProcessSpawn);
        }
        Connection::new(
            ClientId::generate().expect("entropy"),
            PeerGuard::for_current_process(),
            effective,
        )
    }

    fn consumer() -> Connection {
        connection(ServiceScopeSet::baseline(), true)
    }

    fn operator_connection(service: &mut Service) -> Connection {
        let mut request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0));
        request = request.with_operator_capability(service.operator.capability().to_bearer());
        let mut connection = consumer();
        service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(1),
            )
            .expect("operator negotiate");
        connection
    }

    fn negotiate_ok(service: &mut Service, connection: &mut Connection, now: u64) {
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0));
        service
            .negotiate(connection, &request, facts(), MonotonicMs::from_millis(now))
            .expect("negotiate");
    }

    fn mint_host_grant(service: &mut Service, operator: &mut Connection, now: u64) -> String {
        mint_host_grant_pair(service, operator, now).1
    }

    /// Mint a host grant and return its non-secret id and bearer.
    fn mint_host_grant_pair(
        service: &mut Service,
        operator: &mut Connection,
        now: u64,
    ) -> (String, String) {
        let result = service
            .dispatch(
                operator,
                ServiceMethod::DaemonHostGrant,
                &json!({"action": "mint"}),
                MonotonicMs::from_millis(now),
            )
            .expect("host grant");
        (
            result["grant_id"].as_str().expect("grant id").to_owned(),
            result["grant"].as_str().expect("bearer").to_owned(),
        )
    }

    fn create_session(service: &mut Service, connection: &mut Connection, key: &str) -> Value {
        service
            .dispatch(
                connection,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": key}),
                MonotonicMs::from_millis(10),
            )
            .expect("create")
    }

    fn session_id(value: &Value) -> String {
        value["session"]["session_id"]
            .as_str()
            .expect("session id")
            .to_owned()
    }

    #[test]
    fn negotiation_freezes_the_version_and_baseline_scopes() {
        let (mut service, _dir) = service();
        let mut connection = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0));
        let response = service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(5),
            )
            .expect("accepted");
        assert_eq!(response.wire_version, 1);
        assert_eq!(response.service_api_version.to_string(), "1.0");
        assert_eq!(response.scopes.service, ServiceScopeSet::baseline());
        assert_eq!(response.daemon_pid, std::process::id());
        assert_eq!(response.peer_identity.pid, std::process::id());
        assert_eq!(response.daemon_state, DaemonState::Running);
        assert!(
            response
                .limits
                .advertised()
                .any(|entry| entry.name == "max_sessions")
        );
        assert!(connection.is_negotiated());
        assert!(!connection.has_service(ServiceScope::DaemonControl));
    }

    #[test]
    fn a_second_negotiate_is_refused() {
        let (mut service, _dir) = service();
        let mut connection = consumer();
        negotiate_ok(&mut service, &mut connection, 1);
        let refusals = service.audit_counters().refusals;
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0));
        let error = service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(2),
            )
            .expect_err("second negotiate");
        assert_eq!(error.denied_code(), Some(RefusalCode::AlreadyNegotiated));
        assert!(error.closes_connection());
        assert!(
            service.audit_counters().refusals > refusals,
            "a second negotiate is attributed (D-16)"
        );
    }

    #[test]
    fn a_version_mismatch_is_typed_and_closes() {
        let (mut service, _dir) = service();
        let mut connection = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(2, 0));
        let error = service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(1),
            )
            .expect_err("major 2");
        assert_eq!(error.kind(), RefusalKind::VersionMismatch);
        assert!(error.closes_connection());
        assert!(!connection.is_negotiated());
    }

    #[test]
    fn a_bad_operator_capability_is_one_static_unauthenticated_refusal() {
        let (mut service, _dir) = service();
        let mut connection = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_operator_capability(SecretBearer::from_bytes([0u8; 32]));
        let error = service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(1),
            )
            .expect_err("wrong capability");
        assert_eq!(error.kind(), RefusalKind::Unauthenticated);
        assert_eq!(error.code(), "Unauthenticated");
        assert_eq!(error.message(), "authentication failed");
        assert!(error.closes_connection());
        assert!(!connection.has_service(ServiceScope::DaemonControl));
        assert!(!connection.has_service(ServiceScope::DaemonInspect));
        assert!(!connection.is_negotiated());
    }

    #[test]
    fn an_operator_capability_adds_only_the_daemon_scopes() {
        let (mut service, _dir) = service();
        let mut connection = consumer();
        let mut request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0));
        request = request.with_operator_capability(service.operator.capability().to_bearer());
        let response = service
            .negotiate(
                &mut connection,
                &request,
                facts(),
                MonotonicMs::from_millis(1),
            )
            .expect("operator accepted");
        assert!(connection.has_service(ServiceScope::DaemonControl));
        assert!(connection.has_service(ServiceScope::DaemonInspect));
        assert!(!connection.has_service(ServiceScope::SessionHost));
        assert_eq!(response.scopes.service.len(), 5);
    }

    #[test]
    fn a_host_grant_adds_session_host_only_when_consumed() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);

        let mut with_grant = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(
                &mut with_grant,
                &request,
                facts(),
                MonotonicMs::from_millis(3),
            )
            .expect("negotiate");
        assert!(with_grant.has_service(ServiceScope::SessionHost));
        assert!(with_grant.host_grant().is_some());

        // A second connection presenting the same (now consumed) grant gets no
        // scope and no error.
        let mut replay = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut replay, &request, facts(), MonotonicMs::from_millis(4))
            .expect("negotiate still succeeds");
        assert!(!replay.has_service(ServiceScope::SessionHost));
        let error = service
            .dispatch(
                &mut replay,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "k"}),
                MonotonicMs::from_millis(5),
            )
            .expect_err("no consent");
        assert_eq!(error.denied_code(), Some(RefusalCode::ConsentRequired));
    }

    /// A consume-then-revoke withdraws both a fresh create and its keyed replay.
    #[test]
    fn a_consumed_host_grant_revoke_withdraws_create_and_its_replay() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let (grant_id, grant) = mint_host_grant_pair(&mut service, &mut operator, 2);

        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "revoke-k1"}),
                MonotonicMs::from_millis(4),
            )
            .expect("create");
        assert!(created["session"]["session_id"].is_string());
        assert_eq!(
            service
                .host()
                .snapshots(MonotonicMs::from_millis(4))
                .count(),
            1
        );

        let revoked = service
            .dispatch(
                &mut operator,
                ServiceMethod::DaemonHostGrant,
                &json!({"action": "revoke", "grant_id": grant_id}),
                MonotonicMs::from_millis(5),
            )
            .expect("revoke");
        assert_eq!(revoked["revoked"], true);
        assert_eq!(revoked["not_active"], false);

        // Both the fresh key and the already-applied key are refused: the
        // revoke withdraws the create authority before the idempotency store is
        // consulted, so a keyed replay cannot resurrect a session.
        for (index, key) in ["revoke-k1", "revoke-k2"].into_iter().enumerate() {
            let error = service
                .dispatch(
                    &mut creator,
                    ServiceMethod::SessionCreate,
                    &json!({"kind": "terminal", "idempotency_key": key}),
                    MonotonicMs::from_millis(6 + index as u64),
                )
                .expect_err("no consent");
            assert_eq!(error.denied_code(), Some(RefusalCode::ConsentRequired));
        }
        assert_eq!(
            service
                .host()
                .snapshots(MonotonicMs::from_millis(8))
                .count(),
            1,
            "the refused replay created no second session"
        );
    }

    #[test]
    fn create_authorities_follow_the_no_oracle_order() {
        let (mut service, _dir) = service();
        let cases = [
            (false, false, RefusalCode::ScopeViolation),
            (true, false, RefusalCode::ConsentRequired),
            (false, true, RefusalCode::ScopeViolation),
        ];
        for (spawn, host, expected) in cases {
            let mut scopes = ServiceScopeSet::baseline();
            if host {
                scopes.insert(ServiceScope::SessionHost);
            }
            let mut connection = connection(scopes, spawn);
            negotiate_ok(&mut service, &mut connection, 1);
            let error = service
                .dispatch(
                    &mut connection,
                    ServiceMethod::SessionCreate,
                    &json!({"kind": "terminal", "idempotency_key": "k"}),
                    MonotonicMs::from_millis(2),
                )
                .expect_err("refused");
            assert_eq!(
                error.denied_code(),
                Some(expected),
                "spawn={spawn} host={host}"
            );
        }
        assert_eq!(
            service
                .host()
                .snapshots(MonotonicMs::from_millis(3))
                .count(),
            0
        );
    }

    #[test]
    fn scope_is_decided_before_target_lookup() {
        let (mut service, _dir) = service();
        let mut blind = connection(ServiceScopeSet::EMPTY, true);
        negotiate_ok(&mut service, &mut blind, 1);
        let unknown = SessionId::parse(&"a".repeat(32)).expect("session id");
        let error = service
            .dispatch(
                &mut blind,
                ServiceMethod::SessionStatus,
                &json!({"session_id": unknown.as_hex()}),
                MonotonicMs::from_millis(2),
            )
            .expect_err("no scope");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));

        let mut inspector = connection(
            ServiceScopeSet::EMPTY.with(ServiceScope::SessionInspect),
            true,
        );
        negotiate_ok(&mut service, &mut inspector, 1);
        let error = service
            .dispatch(
                &mut inspector,
                ServiceMethod::SessionStatus,
                &json!({"session_id": unknown.as_hex()}),
                MonotonicMs::from_millis(2),
            )
            .expect_err("unknown");
        assert_eq!(error.kind(), RefusalKind::NotFound);
    }

    #[test]
    fn the_full_control_flow_is_reachable_and_keyed() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);

        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");

        let created = create_session(&mut service, &mut creator, "create-1");
        let id = session_id(&created);
        assert_eq!(created["session"]["state"], "created");
        assert_eq!(created["session"]["generation"], 0);
        let create_token = created["control_token"].as_str().expect("token").to_owned();

        // A duplicate create with the same key replays the same session and
        // the same token while the token is unclaimed.
        let replay = create_session(&mut service, &mut creator, "create-1");
        assert_eq!(session_id(&replay), id);
        assert_eq!(
            replay["control_token"].as_str(),
            Some(create_token.as_str())
        );
        assert_eq!(replay["replayed"], true);

        // Claim control.
        let claimed = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "controller",
                    "session_id": id,
                    "expected_generation": 0,
                    "control_token": create_token,
                    "idempotency_key": "attach-1",
                }),
                MonotonicMs::from_millis(11),
            )
            .expect("claim");
        assert_eq!(claimed["role"], "controller");
        let controller_token = claimed["control_token"].as_str().expect("token").to_owned();
        assert_ne!(controller_token, create_token);

        // A post-claim duplicate create returns no token.
        let replay = create_session(&mut service, &mut creator, "create-1");
        assert!(replay["control_token"].is_null());

        // Mint an observer grant as the controller.
        let minted = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "grant-1"}),
                MonotonicMs::from_millis(12),
            )
            .expect("grant");
        let observer_grant = minted["grant"].as_str().expect("bearer").to_owned();
        let replay = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "grant-1"}),
                MonotonicMs::from_millis(12),
            )
            .expect("grant replay");
        assert_eq!(replay["grant"].as_str(), Some(observer_grant.as_str()));
        assert_eq!(replay["replayed"], true);

        // A second connection attaches as an observer with the grant.
        let mut observer = consumer();
        negotiate_ok(&mut service, &mut observer, 13);
        let attached = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "observer",
                    "session_id": id,
                    "observer_grant": observer_grant,
                }),
                MonotonicMs::from_millis(14),
            )
            .expect("observer attach");
        let observer_attach = attached["attach_id"].as_str().expect("attach").to_owned();
        assert_eq!(attached["role"], "observer");

        // A second observer attach with the same consumed grant is refused.
        let error = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "observer",
                    "session_id": id,
                    "observer_grant": observer_grant,
                }),
                MonotonicMs::from_millis(15),
            )
            .expect_err("consumed grant");
        assert_eq!(
            error.denied_code(),
            Some(RefusalCode::ObserverGrantRequired)
        );

        // Transfer control to the observer.
        let transferred = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionControlTransfer,
                &json!({
                    "session_id": id,
                    "attach_id": observer_attach,
                    "expected_generation": 0,
                    "idempotency_key": "transfer-1",
                }),
                MonotonicMs::from_millis(16),
            )
            .expect("transfer");
        assert_eq!(transferred["controller"], observer_attach);
        assert_eq!(transferred["replayed"], false);

        // The rotated token is routed to the target's connection only.
        assert_eq!(service.route_deliveries(&mut creator), 0);
        assert_eq!(service.route_deliveries(&mut observer), 1);
        let heartbeat = service
            .dispatch(
                &mut observer,
                ServiceMethod::LeaseHeartbeat,
                &json!({"attach_id": observer_attach}),
                MonotonicMs::from_millis(17),
            )
            .expect("heartbeat");
        let rotated = heartbeat["control_token"].as_str().expect("delivery");
        assert_ne!(rotated, controller_token);
        let second_heartbeat = service
            .dispatch(
                &mut observer,
                ServiceMethod::LeaseHeartbeat,
                &json!({"attach_id": observer_attach}),
                MonotonicMs::from_millis(18),
            )
            .expect("heartbeat");
        assert!(
            second_heartbeat["control_token"].is_null(),
            "delivered once"
        );

        // The previous controller is now an observer and cannot abort.
        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAbort,
                &json!({
                    "session_id": id,
                    "expected_generation": 0,
                    "reason_class": "cancel",
                }),
                MonotonicMs::from_millis(19),
            )
            .expect_err("not controller");
        assert_eq!(error.denied_code(), Some(RefusalCode::ControllerRequired));

        // The new controller aborts; the created session completes not_started.
        let aborted = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAbort,
                &json!({
                    "session_id": id,
                    "expected_generation": 0,
                    "reason_class": "cancel",
                }),
                MonotonicMs::from_millis(20),
            )
            .expect("abort");
        assert_eq!(aborted["session"]["state"], "terminated");
        assert_eq!(aborted["session"]["terminal_reason"], "consumer_cancel");
        assert_eq!(aborted["cleanup"], "not_started");

        // A terminal session is read-only and a repeat returns the snapshot.
        let repeat = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAbort,
                &json!({
                    "session_id": id,
                    "expected_generation": 0,
                    "reason_class": "cancel",
                }),
                MonotonicMs::from_millis(21),
            )
            .expect("terminal snapshot");
        assert_eq!(repeat["not_running"], true);
        assert_eq!(repeat["session"]["state"], "terminated");
    }

    #[test]
    fn a_degraded_sink_refuses_the_mutation_and_changes_no_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod");
        let operator = OperatorStore::create_in(dir.path()).expect("operator");
        let (writer, fail) = TestWriter::new();
        let mut service =
            Service::new(Limits::default(), operator, Box::new(writer)).expect("service");
        let mut operator_conn = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator_conn, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");

        fail.store(true, Ordering::SeqCst);
        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "degraded"}),
                MonotonicMs::from_millis(4),
            )
            .expect_err("audit degraded");
        assert_eq!(error.denied_code(), Some(RefusalCode::Internal));
        assert!(service.audit_degraded());
        assert_eq!(
            service
                .host()
                .snapshots(MonotonicMs::from_millis(5))
                .count(),
            0,
            "no partial mutation"
        );

        // Recovery allows the same key to be executed once.
        // The failed key is stored as refused; a client retries with a fresh
        // key after the sink recovers.
        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "degraded"}),
                MonotonicMs::from_millis(5),
            )
            .expect_err("the refused key replays its refusal");
        assert_eq!(error.denied_code(), Some(RefusalCode::Internal));

        fail.store(false, Ordering::SeqCst);
        let created = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "degraded-retry"}),
                MonotonicMs::from_millis(6),
            )
            .expect("created after recovery");
        assert_eq!(created["session"]["state"], "created");
        assert!(!service.audit_degraded());
    }

    #[test]
    fn daemon_status_is_operator_only_and_advertises_limit_triples() {
        let (mut service, _dir) = service();
        let mut consumer = consumer();
        negotiate_ok(&mut service, &mut consumer, 1);
        let error = service
            .dispatch(
                &mut consumer,
                ServiceMethod::DaemonStatus,
                &Value::Null,
                MonotonicMs::from_millis(2),
            )
            .expect_err("not operator");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));

        let mut operator = operator_connection(&mut service);
        let status = service
            .dispatch(
                &mut operator,
                ServiceMethod::DaemonStatus,
                &Value::Null,
                MonotonicMs::from_millis(3),
            )
            .expect("status");
        assert_eq!(status["daemon_state"], "running");
        assert_eq!(status["audit_degraded"], false);
        let limits = status["limits"].as_array().expect("limits");
        assert_eq!(limits.len(), crate::limits::LIMIT_COUNT);
        for limit in limits {
            for key in ["name", "effective", "default", "system_max"] {
                assert!(limit.get(key).is_some(), "missing {key} in {limit}");
            }
        }
    }

    #[test]
    fn daemon_stop_refuses_with_active_sessions_and_force_terminates() {
        let (mut service, _dir) = service();
        let mut operator_conn = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator_conn, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "stop-create");
        let id = session_id(&created);

        let error = service
            .dispatch(
                &mut operator_conn,
                ServiceMethod::DaemonStop,
                &json!({"mode": "refuse"}),
                MonotonicMs::from_millis(12),
            )
            .expect_err("active sessions");
        assert_eq!(error.denied_code(), Some(RefusalCode::ActiveSessions));
        assert_eq!(error.counts(), Some((1, 16)));

        let stopped = service
            .dispatch(
                &mut operator_conn,
                ServiceMethod::DaemonStop,
                &json!({"mode": "force"}),
                MonotonicMs::from_millis(13),
            )
            .expect("force");
        assert_eq!(stopped["accepted"], true);
        assert_eq!(stopped["terminated"], 1);
        assert_eq!(service.daemon_state(), DaemonState::Stopping);
        assert_eq!(service.stop_requested(), Some(StopMode::Force));

        // The terminal transition retires the create token, so the operator
        // view is the reconciliation read here.
        let terminal = service
            .dispatch(
                &mut operator_conn,
                ServiceMethod::SessionStatus,
                &json!({"session_id": id}),
                MonotonicMs::from_millis(14),
            )
            .expect("status");
        assert_eq!(terminal["session"]["state"], "terminated");
        assert_eq!(terminal["session"]["terminal_reason"], "daemon_stop");
    }

    #[test]
    fn a_forged_peer_start_time_closes_every_privileged_boundary() {
        let (mut service, _dir) = service();
        for boundary in DispatchBoundary::ALL {
            let guard =
                PeerGuard::with_forged_start_time(std::process::id(), std::process::id().into());
            let mut connection = Connection::new(
                ClientId::generate().expect("entropy"),
                guard,
                EffectiveScopes {
                    service: ServiceScopeSet::all(),
                    transport: ScopeSet::all(),
                },
            );
            negotiate_ok(&mut service, &mut connection, 1);
            let error = service
                .recheck(&connection, boundary)
                .expect_err("recycled pid");
            assert_eq!(error.kind(), RefusalKind::Unauthenticated, "{boundary}");
        }
    }

    #[test]
    fn deferred_methods_return_typed_refusals() {
        let (mut service, _dir) = service();
        let mut operator_conn = operator_connection(&mut service);
        let error = service
            .dispatch(
                &mut operator_conn,
                ServiceMethod::DaemonOrphansVerify,
                &json!({"action": "verify"}),
                MonotonicMs::from_millis(2),
            )
            .expect_err("deferred");
        assert_eq!(
            error.denied_code(),
            Some(RefusalCode::UnsupportedCapability)
        );

        let mut creator = consumer();
        negotiate_ok(&mut service, &mut creator, 3);
        let refusals = service.audit_counters().refusals;
        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionResize,
                &json!({"session_id": "a".repeat(32), "attach_id": "b".repeat(32), "columns": 80, "rows": 24}),
                MonotonicMs::from_millis(4),
            )
            .expect_err("no terminal.manage");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));
        assert!(
            service.audit_counters().refusals > refusals,
            "a refused mutation is attributed (D-16)"
        );
    }

    #[test]
    fn a_stopping_daemon_refuses_mutations_and_still_serves_reads() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "stopping-create");
        let id = session_id(&created);

        let stopped = service
            .dispatch(
                &mut operator,
                ServiceMethod::DaemonStop,
                &json!({"mode": "force"}),
                MonotonicMs::from_millis(12),
            )
            .expect("force");
        assert_eq!(stopped["accepted"], true);
        let before = service
            .host()
            .snapshots(MonotonicMs::from_millis(13))
            .count();

        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "after-stop"}),
                MonotonicMs::from_millis(13),
            )
            .expect_err("a stopping daemon refuses new sessions");
        assert_eq!(error.denied_code(), Some(RefusalCode::DaemonStopping));

        // A different connection gets the same answer: the guard is daemon
        // state, not a property of the connection that stopped it.
        let mut late = consumer();
        negotiate_ok(&mut service, &mut late, 13);
        let error = service
            .dispatch(
                &mut late,
                ServiceMethod::SessionCreate,
                &json!({"kind": "terminal", "idempotency_key": "late-create"}),
                MonotonicMs::from_millis(13),
            )
            .expect_err("a second connection is refused too");
        assert_eq!(error.denied_code(), Some(RefusalCode::DaemonStopping));
        assert_eq!(
            service
                .host()
                .snapshots(MonotonicMs::from_millis(13))
                .count(),
            before,
            "a refused create applies nothing"
        );

        let error = service
            .dispatch(
                &mut operator,
                ServiceMethod::DaemonStop,
                &json!({"mode": "force"}),
                MonotonicMs::from_millis(14),
            )
            .expect_err("already stopping");
        assert_eq!(error.denied_code(), Some(RefusalCode::DaemonStopping));

        // Reconciliation reads still serve the stopping state.
        let status = service
            .dispatch(
                &mut operator,
                ServiceMethod::DaemonStatus,
                &Value::Null,
                MonotonicMs::from_millis(15),
            )
            .expect("status");
        assert_eq!(status["daemon_state"], "stopping");
        let terminal = service
            .dispatch(
                &mut operator,
                ServiceMethod::SessionStatus,
                &json!({"session_id": id}),
                MonotonicMs::from_millis(15),
            )
            .expect("status read");
        assert_eq!(terminal["session"]["state"], "terminated");
    }

    #[test]
    fn an_own_lease_detach_needs_no_session_control_scope() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "detach-create");
        let id = session_id(&created);
        let create_token = created["control_token"].as_str().expect("token").to_owned();
        let claimed = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "controller",
                    "session_id": id,
                    "expected_generation": 0,
                    "control_token": create_token,
                    "idempotency_key": "detach-attach",
                }),
                MonotonicMs::from_millis(11),
            )
            .expect("claim");
        let controller_attach = claimed["attach_id"].as_str().expect("attach").to_owned();
        let minted = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "detach-grant"}),
                MonotonicMs::from_millis(12),
            )
            .expect("observer grant");

        let mut observer = consumer();
        negotiate_ok(&mut service, &mut observer, 13);
        let attached = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "observer",
                    "session_id": id,
                    "observer_grant": minted["grant"],
                }),
                MonotonicMs::from_millis(14),
            )
            .expect("observer attach");
        let observer_attach = attached["attach_id"].as_str().expect("attach");
        observer
            .scopes_mut()
            .service
            .remove(ServiceScope::SessionControl);

        // The caller's own lease is scope-free.
        let detached = service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionDetach,
                &json!({"attach_id": observer_attach}),
                MonotonicMs::from_millis(15),
            )
            .expect("own detach");
        assert_eq!(detached["was_attached"], true);

        // Another connection's lease still needs session.control or
        // daemon.control, decided before the target lookup.
        let mut stranger = consumer();
        stranger
            .scopes_mut()
            .service
            .remove(ServiceScope::SessionControl);
        negotiate_ok(&mut service, &mut stranger, 16);
        let error = service
            .dispatch(
                &mut stranger,
                ServiceMethod::SessionDetach,
                &json!({"attach_id": controller_attach}),
                MonotonicMs::from_millis(17),
            )
            .expect_err("no control scope");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));
    }

    #[test]
    fn the_operator_all_session_view_is_daemon_inspect() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        create_session(&mut service, &mut creator, "view-create");

        // daemon.inspect without daemon.control is enough for the operator
        // all-session view, and not enough for a control action.
        let mut inspector = connection(
            ServiceScopeSet::baseline().with(ServiceScope::DaemonInspect),
            true,
        );
        negotiate_ok(&mut service, &mut inspector, 4);
        let listed = service
            .dispatch(
                &mut inspector,
                ServiceMethod::SessionList,
                &Value::Null,
                MonotonicMs::from_millis(5),
            )
            .expect("operator view");
        assert_eq!(
            listed["sessions"].as_array().expect("sessions").len(),
            1,
            "daemon.inspect sees every session"
        );
        let error = service
            .dispatch(
                &mut inspector,
                ServiceMethod::SessionKill,
                &json!({"session_id": "a".repeat(32), "expected_generation": 0}),
                MonotonicMs::from_millis(6),
            )
            .expect_err("daemon.control is required");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));
    }

    #[test]
    fn grant_revoke_runs_the_visibility_check_before_not_active() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "revoke-create");
        let id = session_id(&created);
        let create_token = created["control_token"].as_str().expect("token").to_owned();
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "controller",
                    "session_id": id,
                    "expected_generation": 0,
                    "control_token": create_token,
                    "idempotency_key": "revoke-attach",
                }),
                MonotonicMs::from_millis(11),
            )
            .expect("claim");
        let minted = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "revoke-grant"}),
                MonotonicMs::from_millis(12),
            )
            .expect("observer grant");
        let grant_id = minted["grant_id"].as_str().expect("grant id").to_owned();

        // Consume the grant so the record is inactive but still retained.
        let mut observer = consumer();
        negotiate_ok(&mut service, &mut observer, 13);
        service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "observer",
                    "session_id": id,
                    "observer_grant": minted["grant"],
                }),
                MonotonicMs::from_millis(14),
            )
            .expect("observer attach");

        // A caller that cannot see the session learns nothing about the grant.
        let mut stranger = consumer();
        negotiate_ok(&mut service, &mut stranger, 15);
        let error = service
            .dispatch(
                &mut stranger,
                ServiceMethod::SessionGrantRevoke,
                &json!({"grant_id": grant_id}),
                MonotonicMs::from_millis(16),
            )
            .expect_err("no visibility");
        assert_eq!(error.kind(), RefusalKind::NotFound);

        // The controller runs the uniform stages and then sees `not_active`.
        let revoked = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrantRevoke,
                &json!({"grant_id": grant_id}),
                MonotonicMs::from_millis(17),
            )
            .expect("uniform stages pass");
        assert_eq!(revoked["not_active"], true);
        assert_eq!(revoked["revoked"], false);
    }

    #[test]
    fn the_service_sweep_drops_retired_grants() {
        let (mut service, _dir) = service();
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "sweep-create");
        let id = session_id(&created);
        let create_token = created["control_token"].as_str().expect("token").to_owned();
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "controller",
                    "session_id": id,
                    "expected_generation": 0,
                    "control_token": create_token,
                    "idempotency_key": "sweep-attach",
                }),
                MonotonicMs::from_millis(11),
            )
            .expect("claim");
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-grant"}),
                MonotonicMs::from_millis(12),
            )
            .expect("observer grant");
        assert_eq!(service.grants().len(), 2, "host and observer records");
        let dropped = service.sweep_grants(MonotonicMs::from_millis(13));
        assert_eq!(dropped, 0, "live records are never swept");
        assert_eq!(service.grants().len(), 2);

        // Revoke the observer grant: it is retired immediately and the next
        // sweep drops it, while the host record stays until its TTL.
        let minted = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-grant"}),
                MonotonicMs::from_millis(14),
            )
            .expect("replay");
        let grant_id = minted["grant_id"].as_str().expect("grant id").to_owned();
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrantRevoke,
                &json!({"grant_id": grant_id}),
                MonotonicMs::from_millis(15),
            )
            .expect("revoke");
        let dropped = service.sweep_grants(MonotonicMs::from_millis(16));
        assert_eq!(dropped, 1, "a revoked observer grant is retired");
        assert_eq!(
            service.grants().len(),
            1,
            "the host grant is kept until its TTL"
        );
    }

    #[test]
    fn a_limit_exceeded_refusal_uses_the_framing_class() {
        let refusal = Refusal::limit_exceeded(8_192, 20_000);
        assert_eq!(refusal.code(), "LimitExceeded");
        assert_eq!(refusal.class(), ErrorClass::Framing);
        assert_eq!(refusal.counts(), Some((20_000, 8_192)));
        assert!(!refusal.closes_connection(), "the server decides the close");
    }

    #[test]
    fn a_keyed_grant_replay_survives_the_grant_sweep() {
        // A one-second observer TTL lets the sweep retire the record while the
        // controller's default 15-second lease is still live.
        let limits = Limits::new(&[LimitOverride::new(LimitId::ObserverGrantTtlMs, 1_000)])
            .expect("valid override");
        let (mut service, _dir) = service_with(limits);
        let mut operator = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "sweep-replay-create");
        let id = session_id(&created);
        let create_token = created["control_token"].as_str().expect("token").to_owned();
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "controller",
                    "session_id": id,
                    "expected_generation": 0,
                    "control_token": create_token,
                    "idempotency_key": "sweep-replay-claim",
                }),
                MonotonicMs::from_millis(11),
            )
            .expect("claim");
        let minted = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-replay-grant"}),
                MonotonicMs::from_millis(12),
            )
            .expect("mint");
        let grant_id = minted["grant_id"].as_str().expect("grant id").to_owned();
        let bearer = minted["grant"].as_str().expect("bearer").to_owned();

        // Consume it, then sweep past the grant TTL: the record is gone.
        let mut observer = consumer();
        negotiate_ok(&mut service, &mut observer, 13);
        service
            .dispatch(
                &mut observer,
                ServiceMethod::SessionAttach,
                &json!({
                    "attach_role": "observer",
                    "session_id": id,
                    "observer_grant": bearer,
                }),
                MonotonicMs::from_millis(14),
            )
            .expect("observer attach");
        let dropped = service.sweep_grants(MonotonicMs::from_millis(1_100));
        assert!(dropped >= 1, "the expired observer grant is swept");
        let parsed = GrantId::parse(&grant_id).expect("grant id");
        assert!(
            service
                .grants()
                .find_by_ref(crate::service::grants::ref_of(parsed.as_bytes()))
                .is_none(),
            "the swept record is gone"
        );

        // The replay still answers the original grant id, with no bearer.
        let replay = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-replay-grant"}),
                MonotonicMs::from_millis(1_100),
            )
            .expect("replay after the sweep");
        assert_eq!(replay["grant_id"], grant_id);
        assert_eq!(replay["grant"], Value::Null);
        assert_eq!(replay["grant_active"], false);
        assert_eq!(replay["replayed"], true);

        // A revoked grant that the sweep drops behaves the same way.
        let second = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-replay-grant-2"}),
                MonotonicMs::from_millis(1_200),
            )
            .expect("second mint");
        let second_id = second["grant_id"].as_str().expect("grant id").to_owned();
        service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrantRevoke,
                &json!({"grant_id": second_id}),
                MonotonicMs::from_millis(1_201),
            )
            .expect("revoke");
        assert!(service.sweep_grants(MonotonicMs::from_millis(1_201)) >= 1);
        let replay = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionGrant,
                &json!({"session_id": id, "idempotency_key": "sweep-replay-grant-2"}),
                MonotonicMs::from_millis(1_202),
            )
            .expect("replay after revoke and sweep");
        assert_eq!(replay["grant_id"], second_id);
        assert_eq!(replay["grant_active"], false);
    }

    #[test]
    fn kill_is_operator_only_and_never_succeeds_for_a_consumer() {
        let (mut service, _dir) = service();
        let mut operator_conn = operator_connection(&mut service);
        let grant = mint_host_grant(&mut service, &mut operator_conn, 2);
        let mut creator = consumer();
        let request = NegotiateRequest::new(crate::contract::ClientVersion::new(1, 0))
            .with_host_grant(SecretBearer::parse(&grant).expect("bearer"));
        service
            .negotiate(&mut creator, &request, facts(), MonotonicMs::from_millis(3))
            .expect("negotiate");
        let created = create_session(&mut service, &mut creator, "kill-create");
        let id = session_id(&created);

        let error = service
            .dispatch(
                &mut creator,
                ServiceMethod::SessionKill,
                &json!({"session_id": id, "expected_generation": 0}),
                MonotonicMs::from_millis(12),
            )
            .expect_err("consumer cannot kill");
        assert_eq!(error.denied_code(), Some(RefusalCode::ScopeViolation));

        let killed = service
            .dispatch(
                &mut operator_conn,
                ServiceMethod::SessionKill,
                &json!({"session_id": id, "expected_generation": 0}),
                MonotonicMs::from_millis(13),
            )
            .expect("operator kill");
        assert_eq!(killed["session"]["state"], "terminated");
        assert_eq!(killed["reason"], "operator_abort");
    }
}
