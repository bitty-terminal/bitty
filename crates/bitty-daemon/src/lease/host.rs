//! The attach, control-token, and abort/kill state machine (bitty#2609).
//!
//! [`SessionHost`] owns the registry plus the per-session lease state, so one
//! `&mut SessionHost` makes an attach, detach, claim, transfer, or terminal
//! transition atomic: a refused operation changes neither the registry nor the
//! lease table. It starts no listener, receives no IPC frame, spawns no worker,
//! signals no process, and performs no containment check; those belong to
//! bitty#2607, bitty#2604, and bitty#2602.
//!
//! # Time
//!
//! Every operation takes the caller's [`MonotonicMs`]. Lease expiry, renewal,
//! abort deadlines, and escalation are computed from that value; this module
//! never reads a clock and never sleeps. Expiry is swept by [`SessionHost::expire_due`]
//! (the daemon's timer) or by the explicit checks below; an expired lease can
//! never act even before the sweep removes it.

use std::collections::BTreeMap;
use std::fmt;

use crate::registry::{
    AttachId, CleanupClass, CreateSession, DaemonInstanceId, Entropy, ExitInformation,
    ID_GENERATION_ATTEMPTS, LifecycleState, MonotonicMs, OsEntropy, RegistryLimits, SessionId,
    SessionRegistry, SessionSnapshot, TerminalOutcome, TerminalReason,
};

use super::record::{Lease, LeaseLimits, LeaseRole};
use super::refusal::LeaseRefusal;
use super::token::{ControlToken, ControlTokenDigest};

/// The control authority a request carries.
///
/// The service layer builds this from the authenticated connection:
///
/// - [`ControlAuthority::lease`] for a request that arrived on a live lease;
/// - [`ControlAuthority::token`] for a request presenting the current control
///   token while control is vacant;
/// - [`ControlAuthority::with_token`] to add the token to a lease request;
/// - [`ControlAuthority::operator`] only for operator paths (`daemon.control`).
///
/// Scope evaluation and the operator-capability check happen in bitty#2607
/// before this value is built; the state machine enforces the session-level
/// relation, generation, and token.
#[derive(Debug, Clone, Copy)]
pub struct ControlAuthority<'a> {
    lease: Option<AttachId>,
    token: Option<&'a ControlToken>,
    operator: bool,
}

impl<'a> ControlAuthority<'a> {
    /// A request on one live lease.
    #[must_use]
    pub const fn lease(attach_id: AttachId) -> Self {
        Self {
            lease: Some(attach_id),
            token: None,
            operator: false,
        }
    }

    /// A request presenting the current control token while control is vacant.
    #[must_use]
    pub const fn token(token: &'a ControlToken) -> Self {
        Self {
            lease: None,
            token: Some(token),
            operator: false,
        }
    }

    /// The operator path (requires `daemon.control` in bitty#2607).
    #[must_use]
    pub const fn operator() -> Self {
        Self {
            lease: None,
            token: None,
            operator: true,
        }
    }

    /// Attach a presented control token to this authority.
    ///
    /// The token is only consulted when the authority names no lease (a
    /// connection that presented the token without an attach). A lease-bearing
    /// authority is decided by its role alone: an observer lease never becomes
    /// controller, with or without a token (`D-09`). The setter stays available
    /// so a service layer that always records the presented token cannot
    /// accidentally turn that token into authority.
    #[must_use]
    pub const fn with_token(mut self, token: &'a ControlToken) -> Self {
        self.token = Some(token);
        self
    }

    const fn is_operator(self) -> bool {
        self.operator
    }
}

/// Who caused a mutation, for attribution and audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// A request that arrived on this lease.
    Lease(AttachId),
    /// A request presenting the control token while control was vacant.
    ControlToken,
    /// The operator path; the peer facts and capability are bitty#2607's.
    Operator,
    /// The daemon observed the worker without a controller request.
    Worker,
}

impl Actor {
    /// Contract spelling for audit/event attribution.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lease(_) => "lease",
            Self::ControlToken => "control_token",
            Self::Operator => "operator",
            Self::Worker => "worker",
        }
    }
}

/// A successful create, including the one-time control token.
///
/// The token is deliberately not `Clone`; pass it to the create-key store or
/// the wire encoder and drop it. A duplicate create response is bitty#2603's
/// create-key store, which asks [`SessionHost::control_view`] whether the
/// token has been claimed.
#[derive(Debug)]
pub struct CreatedSession {
    /// The `created` snapshot at revision `0`.
    pub session: SessionSnapshot,
    /// The initial 256-bit control token (returned once).
    pub control_token: ControlToken,
}

/// A successful observer attach.
#[derive(Debug, PartialEq, Eq)]
pub struct Attached {
    /// The new observer lease.
    pub lease: Lease,
    /// The session snapshot after the attach revision bump. `attach_count` is
    /// the live count (expired leases are not counted).
    pub session: SessionSnapshot,
    /// Expired leases removed by the attach-entry sweep, in sweep order.
    ///
    /// The attach is the cap entry point, so it sweeps the target session's
    /// expired records before counting (`D-12`, `R-S18`) instead of letting
    /// them accumulate until the timer. The event layer must publish one
    /// detach event per entry, at the carried revision, before the attach
    /// event. A refusal before the sweep carries none; a counter fault after
    /// the sweep is fail-closed and can drop those due detaches because the
    /// session's counters are already unusable.
    pub expired: Vec<LeaseExpiry>,
}

/// A successful controller claim.
#[derive(Debug)]
pub struct Claimed {
    /// The new controller lease.
    pub lease: Lease,
    /// The rotated 256-bit control token (returned once to this claim).
    pub control_token: ControlToken,
    /// The session snapshot after the claim revision bump.
    pub session: SessionSnapshot,
    /// An expired controller lease that the claim vacated first.
    pub vacated_expired_controller: Option<LeaseExpiry>,
}

/// A successful control transfer.
///
/// The rotated token is **not** part of the requester's result. It is carried
/// by [`TransferDelivery`], which only yields the secret to a caller resolving
/// the target lease, so a transfer response on the requesting connection can
/// never contain the new controller's token (`D-09`). The service layer
/// (bitty#2607) routes the delivery to the target lease's own queue and
/// excludes it from the requester's response.
#[derive(Debug)]
pub struct Transferred {
    /// The session snapshot after the transfer revision bump.
    pub session: SessionSnapshot,
    /// The authority that requested the transfer: the previous controller lease
    /// or the control token while control was vacant.
    pub actor: Actor,
    /// The previous controller, demoted to observer.
    pub previous_controller: Option<AttachId>,
    /// The new controller lease.
    pub controller: AttachId,
    /// The rotated token addressed to the new controller.
    pub delivery: TransferDelivery,
    /// An expired controller lease that the transfer vacated first.
    pub vacated_expired_controller: Option<LeaseExpiry>,
}

/// A rotated control token addressed to exactly one target lease.
///
/// A transfer rotates the token for the *new* controller, so the secret must
/// travel to that lease and never back to the requester (`D-09`). The wrapper
/// makes the target part of the value: [`TransferDelivery::take_for`] yields
/// the token only for the target attach id and returns the delivery unchanged
/// otherwise, so a response encoder cannot silently take another lease's
/// secret. This is a check on the value's shape, not a routing guarantee:
/// bitty#2607 owns delivering the token to the target lease's own queue and
/// excluding it from the requesting connection's response. The token is
/// zeroized on drop when it is never taken.
#[derive(Debug)]
pub struct TransferDelivery {
    target: AttachId,
    token: ControlToken,
}

impl TransferDelivery {
    /// The one lease that may receive this token.
    #[must_use]
    pub const fn target(&self) -> AttachId {
        self.target
    }

    /// Consume the delivery for `attach_id`.
    ///
    /// # Errors
    ///
    /// Returns the unchanged delivery when `attach_id` is not the target, so a
    /// mismatched caller cannot consume or leak the secret.
    pub fn take_for(self, attach_id: AttachId) -> Result<ControlToken, Self> {
        if attach_id == self.target {
            Ok(self.token)
        } else {
            Err(self)
        }
    }
}

/// A lease removed by [`SessionHost::expire_due`], an attach-entry sweep, or a
/// claim/transfer vacating an expired controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseExpiry {
    /// Session the lease belonged to.
    pub session_id: SessionId,
    /// Expired attach id.
    pub attach_id: AttachId,
    /// Role at expiry.
    pub role: LeaseRole,
    /// `state_revision` of the detach mutation that removed this lease.
    ///
    /// For a sweep this is the post-detach revision. When a claim or transfer
    /// vacates an expired controller, the detach commits first and the
    /// claim/transfer then bumps once more, so this value is the final snapshot
    /// revision minus one; the detach event must use this revision, not the
    /// operation's final revision.
    pub revision: u64,
}

/// A successful detach or revocation.
#[derive(Debug, PartialEq, Eq)]
pub struct Detached {
    /// Session snapshot after the detach (unchanged on an idempotent repeat).
    pub session: SessionSnapshot,
    /// The handle named by the request.
    pub attach_id: AttachId,
    /// The role the lease had, when it was live.
    pub role: Option<LeaseRole>,
    /// Whether a live lease was actually removed.
    pub was_attached: bool,
    /// Whether the actor detached another lease (a revocation).
    pub revoked: bool,
    /// Whether an operator revocation permanently retired control.
    pub control_retired: bool,
    /// Who performed the detach.
    pub actor: Actor,
}

/// A successful lease heartbeat.
#[derive(Debug, PartialEq, Eq)]
pub struct LeaseHeartbeat {
    /// The renewed lease.
    pub lease: Lease,
    /// Session snapshot; `state_revision` is unchanged by a heartbeat.
    pub session: SessionSnapshot,
}

/// The two stages of an accepted stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortStage {
    /// The graceful window is open; the caller signals the tree and waits.
    Graceful,
    /// The window elapsed (or a kill escalated it); the caller kills the tree.
    KillDue,
}

impl AbortStage {
    /// Contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::KillDue => "kill_due",
        }
    }
}

/// A committed stop acceptance.
#[derive(Debug, PartialEq, Eq)]
pub struct StopAccepted {
    /// Snapshot after the `stopping` transition.
    pub session: SessionSnapshot,
    /// Generation the request targeted (unchanged by the stop).
    pub generation: u64,
    /// The current stage.
    pub stage: AbortStage,
    /// Absolute monotonic instant at which the graceful window closes.
    pub deadline: MonotonicMs,
    /// Who requested the stop.
    pub actor: Actor,
    /// Whether this was a repeat of an already accepted stop.
    pub already_accepted: bool,
}

/// A committed generation-0 cancel intent.
#[derive(Debug, PartialEq, Eq)]
pub struct PreBindCancelAccepted {
    /// Snapshot while the session is still `created`; terminalization follows.
    pub session: SessionSnapshot,
    /// The terminal reason the completion will carry.
    pub reason: TerminalReason,
    /// Who committed the intent.
    pub actor: Actor,
    /// Whether this was a repeat of an already committed intent.
    pub already_accepted: bool,
}

/// The result of an abort, kill, or operator stop request.
#[derive(Debug, PartialEq, Eq)]
pub enum StopOutcome {
    /// The stop is committed; terminate the tree and then record the exit.
    Accepted(StopAccepted),
    /// The generation-0 cancel intent is committed; never bind a child now.
    PreBindCancel(PreBindCancelAccepted),
    /// The session is already terminal; the immutable snapshot is replayed.
    AlreadyTerminal {
        /// The terminal snapshot.
        session: SessionSnapshot,
    },
}

/// An observed worker termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitObservation {
    /// The worker exited on its own with a wait status.
    Exited {
        /// Wait-status exit code.
        code: u8,
    },
    /// The worker died from a signal the daemon did not send.
    Signalled {
        /// Terminating signal number (nonzero).
        signal: u32,
    },
    /// The worker never bound (spawn error, missing executable, bind timeout).
    SpawnFailed,
    /// The daemon could not observe, reap, or verify the worker's exit.
    SupervisionFailed,
    /// The daemon terminated the tree after an accepted stop.
    DaemonTerminated,
}

/// The terminal notice the event layer publishes exactly once.
///
/// It carries no token, no terminal content, and no caller string. `attached`
/// is every lease record still registered at the transition — including one
/// whose TTL had already lapsed but which the sweep had not removed yet, because
/// its connection may still be subscribed — and bitty#2602 must reserve a
/// terminal control-frame slot for each entry or close that lease with
/// `TerminalEventLost` **before** the terminal snapshot becomes visible.
///
/// # Ordering
///
/// [`SessionHost::record_termination`] and
/// [`SessionHost::complete_pre_bind_cancel`] make the terminal snapshot visible
/// in the registry as they return. The caller must therefore reserve the slots
/// for `attached`, enqueue the notices, and only then release the `&mut
/// SessionHost` (or serve any `session.status`); otherwise a concurrent status
/// can observe `terminated` before a lease's terminal slot exists. Slot
/// reservation, queues, and `TerminalEventLost` belong to bitty#2602.
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalNotice {
    /// The terminal snapshot (attach count already released).
    pub session: SessionSnapshot,
    /// Immutable terminal reason.
    pub reason: TerminalReason,
    /// Exit information belonging to the reason.
    pub exit: ExitInformation,
    /// Cleanup classification supplied by the containment layer.
    pub cleanup: CleanupClass,
    /// Who caused the terminal transition.
    pub actor: Actor,
    /// Lease records still registered at the transition.
    ///
    /// This includes an expired lease that the sweep has not removed yet: its
    /// connection may still be subscribed, so the event layer must reserve or
    /// close it rather than leak the subscription. Order is unspecified.
    pub attached: Vec<AttachId>,
}

/// The result of completing a terminal transition.
#[derive(Debug, PartialEq, Eq)]
pub enum Termination {
    /// The one and only terminal transition for this session.
    Completed(TerminalNotice),
    /// A duplicate observation after the terminal transition; no new notice.
    AlreadyTerminal {
        /// The immutable terminal snapshot.
        session: SessionSnapshot,
    },
}

/// Whether the control token can still be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTokenState {
    /// The stored digest is the current token.
    Current,
    /// Control was revoked or the session terminated; no token verifies.
    Retired,
}

/// The non-secret control summary `session.status` may expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlView {
    /// Session the view describes.
    pub session_id: SessionId,
    /// The live controller lease, if any.
    pub controller: Option<AttachId>,
    /// Live lease count at the query instant, controller included. An expired
    /// lease that the sweep has not removed yet is not counted.
    pub attach_count: u16,
    /// Whether a claim or transfer has ever rotated the create token.
    pub claimed: bool,
    /// Whether the current token can still verify.
    pub token_state: ControlTokenState,
    /// Whether a generation-0 cancel intent is committed.
    pub pending_cancel: bool,
    /// The accepted stop, if one is in progress.
    pub stop: Option<StopView>,
}

/// The non-secret view of an accepted stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopView {
    /// Terminal reason the completion will carry.
    pub cause: TerminalReason,
    /// Absolute monotonic deadline of the graceful window.
    pub deadline: MonotonicMs,
    /// Current stage at the time of the query.
    pub stage: AbortStage,
    /// Whether the kill stage was already requested.
    pub escalated: bool,
}

/// The one-shot escalation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbortEscalation {
    /// The deadline that was reached.
    pub deadline: MonotonicMs,
    /// `true` only on the first escalation request.
    pub first: bool,
}

/// A consumer abort cause (controller authority only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerStopCause {
    /// The consumer cancelled the run.
    Cancel,
    /// The consumer's execution deadline expired.
    Timeout,
}

impl ConsumerStopCause {
    /// The terminal reason.
    #[must_use]
    pub const fn reason(self) -> TerminalReason {
        match self {
            Self::Cancel => TerminalReason::ConsumerCancel,
            Self::Timeout => TerminalReason::ConsumerTimeout,
        }
    }
}

/// A daemon/operator stop cause (`daemon.control` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStopCause {
    /// The operator aborted the session.
    OperatorAbort,
    /// An explicit `daemon.stop` drain or force reached the session.
    DaemonStop,
    /// A host or service-manager signal reached the session.
    HostShutdown,
}

impl DaemonStopCause {
    /// The terminal reason.
    #[must_use]
    pub const fn reason(self) -> TerminalReason {
        match self {
            Self::OperatorAbort => TerminalReason::OperatorAbort,
            Self::DaemonStop => TerminalReason::DaemonStop,
            Self::HostShutdown => TerminalReason::HostShutdown,
        }
    }

    const fn grace_ms(self, limits: LeaseLimits) -> u64 {
        match self {
            Self::OperatorAbort => limits.abort_grace_ms(),
            Self::DaemonStop | Self::HostShutdown => limits.drain_grace_ms(),
        }
    }
}

/// One daemon instance's session, lease, and control state.
pub struct SessionHost {
    registry: SessionRegistry,
    limits: LeaseLimits,
    entropy: Box<dyn Entropy + Send>,
    sessions: BTreeMap<SessionId, SessionLeaseState>,
    lease_sessions: BTreeMap<AttachId, SessionId>,
}

impl fmt::Debug for SessionHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render a session record: the token digest and the raw initial
        // token must not reach a debug log.
        formatter
            .debug_struct("SessionHost")
            .field("instance_id", &self.registry.instance_id())
            .field("limits", &self.limits)
            .field("sessions", &self.sessions.len())
            .field("leases", &self.lease_sessions.len())
            .finish()
    }
}

impl SessionHost {
    /// Create an empty host with a fresh instance id from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseRefusal::Registry`] when the registry rejects the limits
    /// or the OS CSPRNG is unavailable for the instance id.
    pub fn new(registry_limits: RegistryLimits, limits: LeaseLimits) -> Result<Self, LeaseRefusal> {
        let registry = SessionRegistry::new(registry_limits)?;
        Ok(Self::with_parts(registry, limits, Box::new(OsEntropy)))
    }

    fn with_parts(
        registry: SessionRegistry,
        limits: LeaseLimits,
        entropy: Box<dyn Entropy + Send>,
    ) -> Self {
        Self {
            registry,
            limits,
            entropy,
            sessions: BTreeMap::new(),
            lease_sessions: BTreeMap::new(),
        }
    }

    /// This instance's immutable identity (`D-02`).
    #[must_use]
    pub const fn instance_id(&self) -> DaemonInstanceId {
        self.registry.instance_id()
    }

    /// Number of live sessions in the registry (retained terminal records are
    /// not live).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.registry.live_count()
    }

    /// The enforced lease bounds.
    #[must_use]
    pub const fn limits(&self) -> LeaseLimits {
        self.limits
    }

    /// The contract-visible status snapshot for a session (`D-03`, `R-S18`).
    ///
    /// `attach_count` and `attached` are composed from the live lease table at
    /// `now`, so an expired lease that [`SessionHost::expire_due`] has not
    /// removed yet is not counted. The registry revision and the stored record
    /// count are untouched by expiry until the sweep; this method is a view,
    /// not a mutation.
    #[must_use]
    pub fn snapshot(&self, session_id: SessionId, now: MonotonicMs) -> Option<SessionSnapshot> {
        self.registry
            .snapshot(session_id)
            .map(|snapshot| self.compose_snapshot(snapshot, now))
    }

    /// Every live then retained snapshot in session-id order, each with the
    /// live attach count at `now` (`session.list`, `daemon.inspect`).
    ///
    /// Bounded by `max_sessions + max_retained_terminal_records`; the raw
    /// registry count is never served because expiry belongs to this layer.
    pub fn snapshots(&self, now: MonotonicMs) -> impl Iterator<Item = SessionSnapshot> + '_ {
        self.registry
            .snapshots()
            .map(move |snapshot| self.compose_snapshot(snapshot, now))
    }

    /// The non-secret control summary, or `None` for an unknown session.
    #[must_use]
    pub fn control_view(&self, session_id: SessionId, now: MonotonicMs) -> Option<ControlView> {
        let state = self.sessions.get(&session_id)?;
        let token_state = match state.token {
            TokenSlot::Current(_) => ControlTokenState::Current,
            TokenSlot::Retired => ControlTokenState::Retired,
        };
        Some(ControlView {
            session_id,
            controller: state.live_controller(now),
            attach_count: state.live_attach_count(now),
            claimed: state.claimed,
            token_state,
            pending_cancel: state.cancel.is_some(),
            stop: state.stop.as_ref().map(|stop| stop.view(now)),
        })
    }

    /// The lease record for a handle, or `None` when it was never issued,
    /// detached, revoked, or collected.
    ///
    /// The record is returned even when it has expired and the sweep has not
    /// removed it yet, so this is a lookup, not an authorization: a caller that
    /// needs a live lease must check [`Lease::is_expired_at`] against its own
    /// `now`, or call [`SessionHost::authorize_content_read`] /
    /// [`SessionHost::authorize_controller`], which check liveness.
    #[must_use]
    pub fn lease(&self, attach_id: AttachId) -> Option<Lease> {
        let session_id = self.lease_sessions.get(&attach_id)?;
        self.sessions
            .get(session_id)
            .and_then(|state| state.leases.get(&attach_id))
            .copied()
    }

    /// Authorize a role-agnostic content read (metadata events, snapshot
    /// subscription) against a live lease.
    ///
    /// Both roles pass; the point is liveness: an expired, revoked, or foreign
    /// handle is [`LeaseRefusal::NotAttached`]. The caller still checks the
    /// `session.inspect` scope and, for terminal content, `terminal.inspect`,
    /// plus the event-subscription rules (bitty#2607 / bitty#2602). This is
    /// read-only and never renews the lease; the heartbeat or a controller
    /// content operation does.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotAttached`] for an unknown, revoked, or
    /// expired lease.
    pub fn authorize_content_read(
        &self,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<Lease, LeaseRefusal> {
        let session_id = *self
            .lease_sessions
            .get(&attach_id)
            .ok_or(LeaseRefusal::NotAttached)?;
        self.actionable_lease(session_id, attach_id, now)
    }

    /// The stop view for a session, if a stop is in progress.
    #[must_use]
    pub fn stop_view(&self, session_id: SessionId, now: MonotonicMs) -> Option<StopView> {
        self.sessions
            .get(&session_id)
            .and_then(|state| state.stop.as_ref())
            .map(|stop| stop.view(now))
    }

    /// Register a session and mint its initial control token.
    ///
    /// The token is minted before the registry record so an entropy failure
    /// leaves no session behind. The raw token is returned once and only its
    /// digest is stored.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseRefusal::EntropyUnavailable`] when the CSPRNG fails, or
    /// the registry's create refusal (`max_sessions`, record bytes, duplicate
    /// terminal id).
    pub fn create(
        &mut self,
        request: CreateSession,
        now: MonotonicMs,
    ) -> Result<CreatedSession, LeaseRefusal> {
        let control_token = self.mint_token()?;
        let digest = ControlTokenDigest::of(&control_token);
        let session = self.registry.create(request, now)?;
        self.sessions
            .insert(session.session_id, SessionLeaseState::new(digest));
        Ok(CreatedSession {
            session: self.compose_snapshot(session, now),
            control_token,
        })
    }

    /// Bind the first (and only) worker to a `created` session.
    ///
    /// A committed generation-0 cancel intent refuses the bind, so a spawn path
    /// that races an abort can never bind a child the abort covered.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::CancelPending`] while a cancel intent is
    /// committed, or with the registry's bind refusal.
    pub fn bind(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, LeaseRefusal> {
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|state| state.cancel.is_some())
        {
            return Err(LeaseRefusal::CancelPending);
        }
        self.registry
            .bind(session_id, now)
            .map(|snapshot| self.compose_snapshot(snapshot, now))
            .map_err(Into::into)
    }

    /// Collect retained terminal records past their TTL and drop their control
    /// state.
    pub fn collect_retained(&mut self, now: MonotonicMs) -> Vec<SessionId> {
        let collected = self.registry.collect_retained(now);
        for session_id in &collected {
            if let Some(state) = self.sessions.remove(session_id) {
                for attach_id in state.leases.keys() {
                    self.lease_sessions.remove(attach_id);
                }
            }
        }
        collected
    }

    /// Attach an observer lease.
    ///
    /// Authorization precondition (bitty#2607): `session.attach` **and** a
    /// valid single-use observer grant. Grant mint, consume, TTL, and peer
    /// binding are the grant store's responsibility; this method assumes the
    /// grant was already validated and must not be exposed to a bare
    /// connection. The grant must be consumed only **after** this call returns
    /// `Ok`, so a refused attach (`MaxAttaches`, `NotRunning`) does not burn a
    /// single-use grant for a lease that was never created.
    ///
    /// # Errors
    ///
    /// Refuses when the session is unknown or terminal, or when the
    /// per-session observer cap is reached. The controller lease is additional
    /// (`D-09`), so observers never consume the controller's slot. Expired
    /// observers are swept at this entry point before the attach (`Attached::expired`),
    /// so they neither consume a slot nor accumulate while the timer is late; a
    /// hard record backstop (`max_attaches_per_session + controller`) refuses
    /// `MaxAttaches` if a sweep could not keep the table inside the bound.
    pub fn attach_observer(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<Attached, LeaseRefusal> {
        self.live_session(session_id)?;
        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let limit = self.limits.max_attaches_per_session();
        let live_observers = state
            .leases
            .values()
            .filter(|lease| lease.role() == LeaseRole::Observer)
            .filter(|lease| !lease.is_expired_at(now))
            .count();
        if live_observers >= limit {
            return Err(LeaseRefusal::MaxAttaches { limit });
        }

        let attach_id = self.mint_attach_id()?;
        let expired = self.sweep_session(session_id, now);
        // Backstop: after a successful sweep the table holds the live observer
        // count plus at most one controller, so the bound is already held. A
        // surviving overage means the sweep could not remove a record (counter
        // fault); refuse rather than let the table grow.
        let records = self
            .sessions
            .get(&session_id)
            .map_or(0, |state| state.leases.len());
        let hard_limit = limit.saturating_add(1);
        if records >= hard_limit {
            return Err(LeaseRefusal::MaxAttaches { limit });
        }

        let lease = Lease::new(
            attach_id,
            LeaseRole::Observer,
            now,
            self.limits.attach_lease_ttl_ms(),
        );
        let session = self.registry.attach(session_id, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        state.leases.insert(attach_id, lease);
        self.lease_sessions.insert(attach_id, session_id);
        Ok(Attached {
            lease,
            session: self.compose_snapshot(session, now),
            expired,
        })
    }

    /// Claim the controller lease with the current control token.
    ///
    /// The claim creates a new controller lease (it never promotes an existing
    /// observer lease), rotates the token, and returns the new token once on
    /// this connection: a claim names no lease, so the requester *is* the new
    /// controller and the secret has no other recipient. The generation is
    /// checked before the token digest so a stale request cannot probe the
    /// token.
    ///
    /// Vacating an expired controller is a separate snapshot-visible
    /// mutation from the claim itself, so that claim advances
    /// `state_revision` twice (the due detach, then the attach) and reports the
    /// detach as [`Claimed::vacated_expired_controller`]. A claim that refuses
    /// changes nothing, with one documented exception: if the expired-controller
    /// detach succeeds and a later counter fault refuses the attach, the due
    /// detach stands, control stays vacant, and the token is unchanged.
    ///
    /// # Errors
    ///
    /// - [`LeaseRefusal::NotRunning`] for a terminal session;
    /// - [`LeaseRefusal::SessionStopping`] while a stop is accepted;
    /// - [`LeaseRefusal::StaleGeneration`] for a generation mismatch;
    /// - [`LeaseRefusal::ControlHeld`] when a live controller exists;
    /// - [`LeaseRefusal::InvalidControlToken`] for a missing, stale, rotated,
    ///   or retired token;
    /// - [`LeaseRefusal::EntropyUnavailable`] when the CSPRNG fails or returns
    ///   the current secret again (a failed rotation).
    ///
    /// The single controller lease is additional to the observer cap (`D-09`),
    /// so a session full of observers can still gain a controller without
    /// evicting any observer.
    pub fn claim_controller(
        &mut self,
        session_id: SessionId,
        expected_generation: u64,
        token: &ControlToken,
        now: MonotonicMs,
    ) -> Result<Claimed, LeaseRefusal> {
        let session = self.live_session(session_id)?;
        if session.state == LifecycleState::Stopping
            || self
                .sessions
                .get(&session_id)
                .is_some_and(|state| state.cancel.is_some())
        {
            return Err(LeaseRefusal::SessionStopping);
        }
        if session.generation != expected_generation {
            return Err(stale_generation(&session));
        }

        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let expired_controller = state.expired_controller(now);
        if state.controller.is_some() && expired_controller.is_none() {
            return Err(LeaseRefusal::ControlHeld);
        }
        if !state.token_matches(token) {
            return Err(LeaseRefusal::InvalidControlToken);
        }

        let control_token = self.mint_rotated_token(session_id)?;
        let digest = ControlTokenDigest::of(&control_token);
        let attach_id = self.mint_attach_id()?;
        let lease = Lease::new(
            attach_id,
            LeaseRole::Controller,
            now,
            self.limits.attach_lease_ttl_ms(),
        );

        let vacated_controller = if let Some(expired) = expired_controller {
            // A due detach commits before the attach; its own snapshot revision
            // is the detach event's revision, not the claim's final revision.
            Some(self.remove_lease(session_id, expired.attach_id(), now)?)
        } else {
            None
        };

        let session = self.registry.attach(session_id, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        state.leases.insert(attach_id, lease);
        state.controller = Some(attach_id);
        state.token = TokenSlot::Current(digest);
        state.claimed = true;
        self.lease_sessions.insert(attach_id, session_id);

        let vacated_expired_controller =
            vacated_controller.map(|(vacated, detached)| LeaseExpiry {
                session_id,
                attach_id: vacated.attach_id(),
                role: vacated.role(),
                revision: detached.state_revision,
            });

        Ok(Claimed {
            lease,
            control_token,
            session: self.compose_snapshot(session, now),
            vacated_expired_controller,
        })
    }

    /// Refresh one lease from a heartbeat.
    ///
    /// A heartbeat never increments `state_revision` (`D-03`) and never aborts
    /// the session (`D-12`). The service layer checks that the lease belongs to
    /// the calling connection; a foreign attach id is `NotFound` there.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotAttached`] for an unknown, revoked, or
    /// expired lease.
    pub fn heartbeat(
        &mut self,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<LeaseHeartbeat, LeaseRefusal> {
        let session_id = *self
            .lease_sessions
            .get(&attach_id)
            .ok_or(LeaseRefusal::NotAttached)?;
        let ttl = self.limits.attach_lease_ttl_ms();
        // Liveness is checked before the registry mutation; the lease is
        // renewed only after the registry accepted the heartbeat, so a refusal
        // leaves the lease table untouched (whole refusal).
        let live = self
            .sessions
            .get(&session_id)
            .and_then(|state| state.leases.get(&attach_id))
            .copied()
            .ok_or(LeaseRefusal::NotAttached)?;
        if live.is_expired_at(now) {
            return Err(LeaseRefusal::NotAttached);
        }
        let session = self.registry.heartbeat(session_id, now)?;
        let lease = {
            let state = self
                .sessions
                .get_mut(&session_id)
                .ok_or(LeaseRefusal::NotAttached)?;
            let lease = state
                .leases
                .get_mut(&attach_id)
                .ok_or(LeaseRefusal::NotAttached)?;
            lease.renew(now, ttl);
            *lease
        };
        Ok(LeaseHeartbeat {
            lease,
            session: self.compose_snapshot(session, now),
        })
    }

    /// Detach one lease, or revoke another lease with control authority.
    ///
    /// Own detach is always allowed and never requires the control token.
    /// Detaching another lease requires the current controller lease, the
    /// current control token while control is vacant, or the operator path.
    /// Detach never aborts and never changes the generation.
    ///
    /// A target handle that was issued to another session is
    /// [`LeaseRefusal::NotAttached`] (mapped by bitty#2607 to the uniform
    /// `NotFound`), so a handle cannot probe or revoke a foreign lease. A handle
    /// never issued in this session — including one already removed — is the
    /// idempotent `Detached { was_attached: false }` acknowledgement without a
    /// revision bump; the host keeps no per-connection handle history, so
    /// bitty#2607 maps a previously issued but removed handle to the contract's
    /// `not_attached` reply.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotAttached`] for an expired or unknown
    /// actor or a foreign target, [`LeaseRefusal::ControllerRequired`] for an
    /// observer actor, or [`LeaseRefusal::InvalidControlToken`] for a stale
    /// token.
    pub fn detach(
        &mut self,
        session_id: SessionId,
        target: AttachId,
        authority: &ControlAuthority<'_>,
        now: MonotonicMs,
    ) -> Result<Detached, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(Detached {
                session: self.compose_snapshot(snapshot, now),
                attach_id: target,
                role: None,
                was_attached: false,
                revoked: false,
                control_retired: false,
                actor: authority_actor(authority),
            });
        }

        if self
            .lease_sessions
            .get(&target)
            .is_some_and(|owner| *owner != session_id)
        {
            // A handle issued to another session is a foreign handle, not a
            // removed one: it must not read as an idempotent no-op, on the own
            // path or through any other authority.
            return Err(LeaseRefusal::NotAttached);
        }

        let own = !authority.is_operator() && authority.lease == Some(target);
        let authorized = if own {
            let target_lease = self
                .sessions
                .get(&session_id)
                .and_then(|state| state.leases.get(&target))
                .copied();
            if target_lease.is_some_and(|lease| lease.is_expired_at(now)) {
                // An expired lease is already detached. A repeated own detach
                // is the contract's idempotent `not_attached` acknowledgement,
                // not a mutation.
                return Ok(Detached {
                    session: self.compose_snapshot(snapshot, now),
                    attach_id: target,
                    role: None,
                    was_attached: false,
                    revoked: false,
                    control_retired: false,
                    actor: Actor::Lease(target),
                });
            }
            self.actionable_lease(session_id, target, now)?;
            Authorized::Controller(target)
        } else {
            match self.resolve_relation(session_id, authority, now)? {
                Relation::Controller(controller) => Authorized::Controller(controller),
                Relation::VacantToken(token) => {
                    self.verify_token(session_id, token)?;
                    Authorized::VacantToken
                }
                Relation::Operator => Authorized::Operator,
            }
        };

        let target_lease = self
            .sessions
            .get(&session_id)
            .and_then(|state| state.leases.get(&target))
            .copied();
        let controller_target = self
            .sessions
            .get(&session_id)
            .is_some_and(|state| state.controller == Some(target));
        if target_lease.is_none_or(|lease| lease.is_expired_at(now)) {
            if controller_target && !own && authorized == Authorized::Operator {
                // The lease is already detached by TTL, but an explicit
                // operator revoke of the controller must still leave no
                // recovery path: the stored token is retired in the same
                // call.
                self.remove_lease(session_id, target, now)?;
                if let Some(state) = self.sessions.get_mut(&session_id) {
                    state.token = TokenSlot::Retired;
                }
                let session = self
                    .registry
                    .snapshot(session_id)
                    .ok_or(LeaseRefusal::SessionNotFound)?;
                return Ok(Detached {
                    session: self.compose_snapshot(session, now),
                    attach_id: target,
                    role: Some(LeaseRole::Controller),
                    was_attached: false,
                    revoked: true,
                    control_retired: true,
                    actor: Actor::Operator,
                });
            }
            return Ok(Detached {
                session: self.compose_snapshot(snapshot, now),
                attach_id: target,
                role: None,
                was_attached: false,
                revoked: false,
                control_retired: false,
                actor: authorized.actor(),
            });
        }

        let control_retired = controller_target && !own && authorized == Authorized::Operator;

        let (lease, session) = self.remove_lease(session_id, target, now)?;
        if control_retired {
            if let Some(state) = self.sessions.get_mut(&session_id) {
                // No recovery path may reveal or reset the token after an
                // operator revokes the controller (`D-09`).
                state.token = TokenSlot::Retired;
            }
        }
        if let Some(actor_lease) = authority.lease {
            if actor_lease != target {
                self.renew_lease(actor_lease, now);
            }
        }

        Ok(Detached {
            session: self.compose_snapshot(session, now),
            attach_id: target,
            role: Some(lease.role()),
            was_attached: true,
            revoked: !own,
            control_retired,
            actor: authorized.actor(),
        })
    }

    /// Authorize a controller-only content-plane operation (terminal input or
    /// resize) against a live lease.
    ///
    /// The caller still checks `terminal.input` or `terminal.manage` scope and
    /// the actual PTY operation (bitty#2607/bitty#2604). An observer is
    /// [`LeaseRefusal::ControllerRequired`]; an expired or revoked lease is
    /// [`LeaseRefusal::NotAttached`]. This is a read-only check that never
    /// renews the lease: liveness comes from the client heartbeat
    /// ([`SessionHost::heartbeat`]), and [`SessionHost::record_resize`] renews
    /// the controller as part of its mutation.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotAttached`] or
    /// [`LeaseRefusal::ControllerRequired`].
    pub fn authorize_controller(
        &self,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<Lease, LeaseRefusal> {
        let session_id = *self
            .lease_sessions
            .get(&attach_id)
            .ok_or(LeaseRefusal::NotAttached)?;
        let lease = self.actionable_lease(session_id, attach_id, now)?;
        if lease.role().can_control() {
            Ok(lease)
        } else {
            Err(LeaseRefusal::ControllerRequired)
        }
    }

    /// The session a live or retained lease handle belongs to.
    ///
    /// A handle issued to another session is not visible through this lookup;
    /// the service maps a foreign handle to the uniform `NotFound`. This is the
    /// lookup `session.detach` and `session.control.transfer` need for a target
    /// attach id before they build the control relation.
    #[must_use]
    pub fn session_for_lease(&self, attach_id: AttachId) -> Option<SessionId> {
        self.lease_sessions.get(&attach_id).copied()
    }

    /// Resolve a stored 64-bit result ref to one of a session's lease handles.
    ///
    /// The service derives `result_ref` from the first 64 bits of the attach id
    /// so a keyed replay needs no unbounded reference table. A detached lease
    /// is gone and yields `None` (the replay reports `not_attached`); two live
    /// ids sharing 64 bits also yield `None` and the replay fails closed.
    #[must_use]
    pub fn find_attach_by_ref(&self, session_id: SessionId, result_ref: u64) -> Option<AttachId> {
        let state = self.sessions.get(&session_id)?;
        let mut found = None;
        for attach_id in state.leases.keys() {
            let mut prefix = [0u8; 8];
            prefix.copy_from_slice(&attach_id.as_bytes()[..8]);
            if u64::from_be_bytes(prefix) == result_ref {
                if found.is_some() {
                    return None;
                }
                found = Some(*attach_id);
            }
        }
        found
    }

    /// Verify a presented control token without mutating anything.
    ///
    /// Visibility for `session.status` and for a reconnect is "holds a live
    /// lease **or** presents the current token". This read-only check never
    /// rotates, renews, or retires the token; the effectful paths still verify
    /// through [`Self::claim_controller`], [`Self::abort`], or
    /// [`Self::transfer_control`].
    #[must_use]
    pub fn verify_control_token(&self, session_id: SessionId, token: &ControlToken) -> bool {
        self.sessions
            .get(&session_id)
            .is_some_and(|state| state.token_matches(token))
    }

    /// Record one controller-authorized resize (`D-03`, `D-09`).
    ///
    /// The PTY resize itself belongs to the content plane; this method
    /// enforces that only the current controller lease may resize, increments
    /// `state_revision` once, and renews the controller lease. An observer
    /// resize is refused and changes nothing.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotAttached`] for an expired or revoked
    /// lease, [`LeaseRefusal::ControllerRequired`] for an observer, or
    /// [`LeaseRefusal::RevisionOverflow`].
    pub fn record_resize(
        &mut self,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<SessionSnapshot, LeaseRefusal> {
        self.authorize_controller(attach_id, now)?;
        let session_id = *self
            .lease_sessions
            .get(&attach_id)
            .ok_or(LeaseRefusal::NotAttached)?;
        let snapshot = self.registry.record_lease_mutation(session_id, now)?;
        self.renew_lease(attach_id, now);
        Ok(self.compose_snapshot(snapshot, now))
    }

    /// Remove every lease whose TTL lapsed at `now`.
    ///
    /// The daemon's timer calls this; the returned entries are the exact detach
    /// mutations the event layer must publish. Expiry only detaches: the worker
    /// keeps running, the generation is unchanged, and a controller expiry
    /// leaves control vacant with the current token still reclaimable. Each
    /// entry carries the `state_revision` of the detach mutation, ready for the
    /// event envelope.
    pub fn expire_due(&mut self, now: MonotonicMs) -> Vec<LeaseExpiry> {
        let session_ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let mut expired = Vec::new();
        for session_id in session_ids {
            expired.extend(self.sweep_session(session_id, now));
        }
        expired
    }

    /// Remove one session's expired leases and return their detach mutations.
    ///
    /// [`SessionHost::expire_due`] sweeps every session and
    /// [`SessionHost::attach_observer`] calls this at its entry point. The
    /// status/list/authorize paths may call it when they want to publish due
    /// detaches immediately; they never need it for the live count, which
    /// [`SessionHost::snapshot`] composes without a mutation. The registry
    /// detach runs first; a counter fault leaves the lease in place for the
    /// next sweep instead of desynchronizing the derived count. Expiry is never
    /// an abort and never changes the generation.
    pub fn sweep_session(&mut self, session_id: SessionId, now: MonotonicMs) -> Vec<LeaseExpiry> {
        let due: Vec<(AttachId, LeaseRole)> = self
            .sessions
            .get(&session_id)
            .map(|state| {
                state
                    .leases
                    .values()
                    .filter(|lease| lease.is_expired_at(now))
                    .map(|lease| (lease.attach_id(), lease.role()))
                    .collect()
            })
            .unwrap_or_default();

        let mut expired = Vec::with_capacity(due.len());
        for (attach_id, role) in due {
            let exists = self
                .sessions
                .get(&session_id)
                .is_some_and(|state| state.leases.contains_key(&attach_id));
            if !exists {
                continue;
            }
            let Ok(session) = self.registry.detach(session_id, now) else {
                continue;
            };
            if let Some(state) = self.sessions.get_mut(&session_id) {
                state.leases.remove(&attach_id);
                if state.controller == Some(attach_id) {
                    state.controller = None;
                }
            }
            self.lease_sessions.remove(&attach_id);
            expired.push(LeaseExpiry {
                session_id,
                attach_id,
                role,
                revision: session.state_revision,
            });
        }
        expired
    }

    /// Move control to a live observer lease.
    ///
    /// The previous controller (when one exists) becomes an observer, the
    /// target becomes the controller, the token rotates, and the previous token
    /// is refused from this instant. The target must hold `session.control` and
    /// (on the terminal content plane) `terminal.input`/`terminal.manage`;
    /// bitty#2607 checks those scopes, this method checks the lease role.
    ///
    /// # Token delivery (`D-09`)
    ///
    /// The rotated token belongs to the **new controller**, not to the
    /// requester. [`Transferred`] therefore carries it only inside a
    /// [`TransferDelivery`] addressed to the target attach id; the caller takes
    /// it with [`TransferDelivery::take_for`] while routing the new
    /// controller's own message and must never encode it into the requesting
    /// connection's response. This holds for a transfer started by the previous
    /// controller and for one started with the token while control was vacant.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::NotRunning`], [`LeaseRefusal::StaleGeneration`],
    /// [`LeaseRefusal::ControllerRequired`], [`LeaseRefusal::ControlHeld`],
    /// [`LeaseRefusal::InvalidControlToken`], [`LeaseRefusal::NotAttached`] (a
    /// foreign, unknown, or expired target), or
    /// [`LeaseRefusal::TargetNotObserver`]; [`LeaseRefusal::EntropyUnavailable`]
    /// when the CSPRNG fails or repeats the current secret. Nothing changes on
    /// refusal, except that vacating an already-expired controller commits that
    /// due detach first: if a later counter fault refuses the transfer, the
    /// expired lease is gone, control stays vacant, and the token is unchanged.
    pub fn transfer_control(
        &mut self,
        session_id: SessionId,
        expected_generation: u64,
        authority: &ControlAuthority<'_>,
        target: AttachId,
        now: MonotonicMs,
    ) -> Result<Transferred, LeaseRefusal> {
        self.live_session(session_id)?;
        if authority.is_operator() {
            return Err(LeaseRefusal::OperatorPathRequired);
        }
        let authorized = self.authorize_control(session_id, expected_generation, authority, now)?;

        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let target_lease = state
            .leases
            .get(&target)
            .copied()
            .ok_or(LeaseRefusal::NotAttached)?;
        if target_lease.is_expired_at(now) {
            return Err(LeaseRefusal::NotAttached);
        }
        if target_lease.role() != LeaseRole::Observer {
            return Err(LeaseRefusal::TargetNotObserver);
        }

        let (previous_controller, expired_controller) = match authorized {
            Authorized::Controller(controller) => (Some(controller), None),
            Authorized::VacantToken => (None, state.expired_controller(now)),
            Authorized::Operator => return Err(LeaseRefusal::OperatorPathRequired),
        };

        let control_token = self.mint_rotated_token(session_id)?;
        let digest = ControlTokenDigest::of(&control_token);

        let vacated_controller = if let Some(expired) = expired_controller {
            Some(self.remove_lease(session_id, expired.attach_id(), now)?)
        } else {
            None
        };

        let session = self.registry.record_lease_mutation(session_id, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if let Some(previous) = previous_controller {
            if let Some(lease) = state.leases.get_mut(&previous) {
                lease.set_role(LeaseRole::Observer);
            }
        }
        if let Some(lease) = state.leases.get_mut(&target) {
            lease.set_role(LeaseRole::Controller);
        }
        state.controller = Some(target);
        state.token = TokenSlot::Current(digest);
        state.claimed = true;

        if let Some(previous) = previous_controller {
            self.renew_lease(previous, now);
        }
        self.renew_lease(target, now);

        let vacated_expired_controller =
            vacated_controller.map(|(vacated, detached)| LeaseExpiry {
                session_id,
                attach_id: vacated.attach_id(),
                role: vacated.role(),
                revision: detached.state_revision,
            });

        Ok(Transferred {
            session: self.compose_snapshot(session, now),
            actor: authorized.actor(),
            previous_controller,
            controller: target,
            delivery: TransferDelivery {
                target,
                token: control_token,
            },
            vacated_expired_controller,
        })
    }

    /// Abort with consumer authority (`cancel` or `timeout`).
    ///
    /// On a running session the state moves to `stopping`, the revision
    /// increments once, and the caller receives the graceful deadline. On a
    /// `created` session the generation-0 cancel intent is committed instead:
    /// no worker exists, so [`SessionHost::bind`] is refused until
    /// [`SessionHost::complete_pre_bind_cancel`] records the terminal
    /// transition. A terminal session replays its snapshot.
    ///
    /// # Errors
    ///
    /// Refuses with the control-relation, generation, or token refusals above,
    /// and [`LeaseRefusal::OperatorPathRequired`] when handed the operator
    /// authority. An observer is always [`LeaseRefusal::ControllerRequired`].
    pub fn abort(
        &mut self,
        session_id: SessionId,
        expected_generation: u64,
        authority: &ControlAuthority<'_>,
        cause: ConsumerStopCause,
        now: MonotonicMs,
    ) -> Result<StopOutcome, LeaseRefusal> {
        if authority.is_operator() {
            return Err(LeaseRefusal::OperatorPathRequired);
        }
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(StopOutcome::AlreadyTerminal {
                session: self.compose_snapshot(snapshot, now),
            });
        }
        let authorized = self.authorize_control(session_id, expected_generation, authority, now)?;
        self.commit_stop(
            session_id,
            authorized,
            cause.reason(),
            now,
            self.limits.abort_grace_ms(),
        )
    }

    /// Abort, drain, or shut down with daemon authority (`daemon.control`).
    ///
    /// `OperatorAbort` uses the abort grace; `DaemonStop` and `HostShutdown`
    /// use the longer drain grace (`D-05`). Immediate termination is
    /// [`SessionHost::kill`].
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::StaleGeneration`]. An accepted stop is
    /// replayed, and a terminal session returns its immutable snapshot.
    pub fn operator_stop(
        &mut self,
        session_id: SessionId,
        expected_generation: u64,
        cause: DaemonStopCause,
        now: MonotonicMs,
    ) -> Result<StopOutcome, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(StopOutcome::AlreadyTerminal {
                session: self.compose_snapshot(snapshot, now),
            });
        }
        if snapshot.generation != expected_generation {
            return Err(stale_generation(&snapshot));
        }
        let grace = cause.grace_ms(self.limits);
        self.commit_stop(session_id, Authorized::Operator, cause.reason(), now, grace)
    }

    /// Terminate immediately with operator authority (`session.kill`).
    ///
    /// A running session enters `stopping` with a deadline of `now` and the
    /// `operator_abort` reason; the caller then kills the containment identity
    /// and records the exit. A `created` session commits the same generation-0
    /// cancel intent as [`SessionHost::abort`]. A stop already accepted keeps
    /// its first effective reason and is only escalated.
    ///
    /// `kill` is the operator abort path, so it always classifies an accepted
    /// stop as `operator_abort` (or keeps an earlier consumer reason). A
    /// `daemon.stop force` must **not** use it as its first step: force calls
    /// [`SessionHost::operator_stop`] with [`DaemonStopCause::DaemonStop`] (or
    /// [`DaemonStopCause::HostShutdown`]) and then calls `kill` at the same
    /// instant to move the deadline to `now`. That two-step keeps the
    /// daemon-stop reason and still kills immediately; `kill` alone would
    /// report `operator_abort`.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::StaleGeneration`]. A terminal session
    /// replays its snapshot.
    pub fn kill(
        &mut self,
        session_id: SessionId,
        expected_generation: u64,
        now: MonotonicMs,
    ) -> Result<StopOutcome, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(StopOutcome::AlreadyTerminal {
                session: self.compose_snapshot(snapshot, now),
            });
        }
        if snapshot.generation != expected_generation {
            return Err(stale_generation(&snapshot));
        }
        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if let Some(intent) = state.cancel {
            return Ok(StopOutcome::PreBindCancel(PreBindCancelAccepted {
                session: self.compose_snapshot(snapshot, now),
                reason: intent.reason,
                actor: intent.actor,
                already_accepted: true,
            }));
        }
        if snapshot.state == LifecycleState::Stopping {
            let state = self
                .sessions
                .get_mut(&session_id)
                .ok_or(LeaseRefusal::SessionNotFound)?;
            let Some(stop) = state.stop.as_mut() else {
                return Err(LeaseRefusal::UnexpectedExit {
                    state: snapshot.state,
                });
            };
            stop.deadline = now;
            stop.escalated = true;
            let generation = snapshot.generation;
            return Ok(StopOutcome::Accepted(StopAccepted {
                session: self.compose_snapshot(snapshot, now),
                generation,
                stage: AbortStage::KillDue,
                deadline: now,
                actor: Actor::Operator,
                already_accepted: true,
            }));
        }
        if snapshot.state == LifecycleState::Created {
            let state = self
                .sessions
                .get_mut(&session_id)
                .ok_or(LeaseRefusal::SessionNotFound)?;
            state.cancel = Some(CancelIntent {
                reason: TerminalReason::OperatorAbort,
                actor: Actor::Operator,
            });
            return Ok(StopOutcome::PreBindCancel(PreBindCancelAccepted {
                session: self.compose_snapshot(snapshot, now),
                reason: TerminalReason::OperatorAbort,
                actor: Actor::Operator,
                already_accepted: false,
            }));
        }

        let session = self.registry.begin_stopping(session_id, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        state.stop = Some(StopState {
            reason: TerminalReason::OperatorAbort,
            actor: Actor::Operator,
            deadline: now,
            escalated: true,
        });
        Ok(StopOutcome::Accepted(StopAccepted {
            session: self.compose_snapshot(session, now),
            generation: expected_generation,
            stage: AbortStage::KillDue,
            deadline: now,
            actor: Actor::Operator,
            already_accepted: false,
        }))
    }

    /// Return the kill escalation when the graceful window has elapsed.
    ///
    /// This is a supervision step, not a snapshot-visible mutation: the state
    /// stays `stopping` and no revision moves. `first` is `true` only once, so
    /// the caller can avoid re-issuing the kill signal on every tick.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseRefusal::SessionNotFound`] for an unknown session.
    pub fn escalate_abort(
        &mut self,
        session_id: SessionId,
        now: MonotonicMs,
    ) -> Result<Option<AbortEscalation>, LeaseRefusal> {
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let Some(stop) = state.stop.as_mut() else {
            return Ok(None);
        };
        if now.as_millis() < stop.deadline.as_millis() {
            return Ok(None);
        }
        let first = !stop.escalated;
        stop.escalated = true;
        Ok(Some(AbortEscalation {
            deadline: stop.deadline,
            first,
        }))
    }

    /// Complete a committed generation-0 cancel.
    ///
    /// The caller must have ensured no child is bound and must pass the cleanup
    /// class it verified: `not_started` when nothing was spawned, `swept` when
    /// the spawn syscall had already returned and the unbound child was
    /// terminated, or `unverified` when the containment layer could not prove
    /// the tree gone.
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::CancelNotPending`] when no intent is
    /// committed, or a terminal/cleanup refusal. A terminal session replays its
    /// snapshot.
    pub fn complete_pre_bind_cancel(
        &mut self,
        session_id: SessionId,
        cleanup: CleanupClass,
        now: MonotonicMs,
    ) -> Result<Termination, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(Termination::AlreadyTerminal {
                session: self.compose_snapshot(snapshot, now),
            });
        }
        let intent = self
            .sessions
            .get(&session_id)
            .and_then(|state| state.cancel)
            .ok_or(LeaseRefusal::CancelNotPending)?;
        if snapshot.state != LifecycleState::Created {
            return Err(LeaseRefusal::CancelNotPending);
        }
        let outcome = TerminalOutcome::new(intent.reason, ExitInformation::NotApplicable, cleanup)?;
        self.finish_terminal(session_id, outcome, intent.actor, now)
    }

    /// Record an observed worker termination and publish the one terminal
    /// notice.
    ///
    /// Classification (`D-04`):
    ///
    /// - a natural exit while `running` is `worker_exit` with its exit code;
    /// - a signal death while `running` is `worker_signal` with its signal;
    /// - a spawn failure while `created` is `spawn_failed`;
    /// - a supervision loss while `running` is `supervision_failed`;
    /// - any termination after an accepted stop keeps the stop's first
    ///   effective reason (`consumer_cancel`, `consumer_timeout`,
    ///   `operator_abort`, `daemon_stop`, or `host_shutdown`) with no exit
    ///   status.
    ///
    /// The registry records the terminal transition once, releases the derived
    /// attach count, and the host retires the token and invalidates every
    /// lease. A duplicate observation returns [`Termination::AlreadyTerminal`]
    /// and never produces a second notice.
    ///
    /// The terminal snapshot becomes visible in the registry as this returns;
    /// the caller must reserve the terminal slot for every `attached` lease and
    /// enqueue the notice before it releases the `&mut SessionHost` (see
    /// [`TerminalNotice`]).
    ///
    /// # Errors
    ///
    /// Refuses with [`LeaseRefusal::CancelPending`] when the caller must first
    /// complete a pre-bind cancel, [`LeaseRefusal::UnexpectedExit`] when the
    /// observation does not fit the state, or a cleanup/exit validation
    /// refusal.
    pub fn record_termination(
        &mut self,
        session_id: SessionId,
        observation: ExitObservation,
        cleanup: CleanupClass,
        now: MonotonicMs,
    ) -> Result<Termination, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if snapshot.state.is_terminal() {
            return Ok(Termination::AlreadyTerminal {
                session: self.compose_snapshot(snapshot, now),
            });
        }
        let (reason, exit, actor) = {
            let state = self
                .sessions
                .get(&session_id)
                .ok_or(LeaseRefusal::SessionNotFound)?;
            match snapshot.state {
                LifecycleState::Created => {
                    if state.cancel.is_some() {
                        return Err(LeaseRefusal::CancelPending);
                    }
                    match observation {
                        ExitObservation::SpawnFailed => (
                            TerminalReason::SpawnFailed,
                            ExitInformation::NotApplicable,
                            Actor::Worker,
                        ),
                        ExitObservation::SupervisionFailed => (
                            TerminalReason::SupervisionFailed,
                            ExitInformation::NotApplicable,
                            Actor::Worker,
                        ),
                        _ => {
                            return Err(LeaseRefusal::UnexpectedExit {
                                state: snapshot.state,
                            });
                        }
                    }
                }
                LifecycleState::Running => match observation {
                    ExitObservation::Exited { code } => (
                        TerminalReason::WorkerExit,
                        ExitInformation::ExitCode(code),
                        Actor::Worker,
                    ),
                    ExitObservation::Signalled { signal } if signal > 0 => (
                        TerminalReason::WorkerSignal,
                        ExitInformation::Signal(signal),
                        Actor::Worker,
                    ),
                    ExitObservation::SupervisionFailed => (
                        TerminalReason::SupervisionFailed,
                        ExitInformation::NotApplicable,
                        Actor::Worker,
                    ),
                    _ => {
                        return Err(LeaseRefusal::UnexpectedExit {
                            state: snapshot.state,
                        });
                    }
                },
                LifecycleState::Stopping => {
                    let stop = state.stop.as_ref().ok_or(LeaseRefusal::UnexpectedExit {
                        state: snapshot.state,
                    })?;
                    (stop.reason, ExitInformation::NotApplicable, stop.actor)
                }
                _ => {
                    return Err(LeaseRefusal::UnexpectedExit {
                        state: snapshot.state,
                    });
                }
            }
        };
        let outcome = TerminalOutcome::new(reason, exit, cleanup)?;
        self.finish_terminal(session_id, outcome, actor, now)
    }

    fn commit_stop(
        &mut self,
        session_id: SessionId,
        authorized: Authorized,
        reason: TerminalReason,
        now: MonotonicMs,
        grace_ms: u64,
    ) -> Result<StopOutcome, LeaseRefusal> {
        let snapshot = self
            .registry
            .snapshot(session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if let Some(intent) = self
            .sessions
            .get(&session_id)
            .and_then(|state| state.cancel)
        {
            return Ok(StopOutcome::PreBindCancel(PreBindCancelAccepted {
                session: self.compose_snapshot(snapshot, now),
                reason: intent.reason,
                actor: intent.actor,
                already_accepted: true,
            }));
        }
        if snapshot.state == LifecycleState::Stopping {
            let stop_generation = snapshot.generation;
            let stop = self
                .sessions
                .get(&session_id)
                .and_then(|state| state.stop.as_ref())
                .ok_or(LeaseRefusal::UnexpectedExit {
                    state: snapshot.state,
                })?;
            return Ok(StopOutcome::Accepted(StopAccepted {
                session: self.compose_snapshot(snapshot, now),
                generation: stop_generation,
                stage: stop.stage(now),
                deadline: stop.deadline,
                actor: stop.actor,
                already_accepted: true,
            }));
        }
        let actor = authorized.actor();
        let generation = snapshot.generation;
        if snapshot.state == LifecycleState::Created {
            let state = self
                .sessions
                .get_mut(&session_id)
                .ok_or(LeaseRefusal::SessionNotFound)?;
            state.cancel = Some(CancelIntent { reason, actor });
            return Ok(StopOutcome::PreBindCancel(PreBindCancelAccepted {
                session: self.compose_snapshot(snapshot, now),
                reason,
                actor,
                already_accepted: false,
            }));
        }

        let session = self.registry.begin_stopping(session_id, now)?;
        let deadline = MonotonicMs::from_millis(now.as_millis().saturating_add(grace_ms));
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        state.stop = Some(StopState {
            reason,
            actor,
            deadline,
            escalated: false,
        });
        self.renew_authorized(authorized, now);
        Ok(StopOutcome::Accepted(StopAccepted {
            session: self.compose_snapshot(session, now),
            generation,
            stage: AbortStage::Graceful,
            deadline,
            actor,
            already_accepted: false,
        }))
    }

    /// Commit the one terminal transition.
    ///
    /// The terminal snapshot becomes visible to the registry here. The caller
    /// must reserve every lease slot listed in the notice and enqueue it before
    /// it releases the `&mut SessionHost` (see [`TerminalNotice`]); this method
    /// cannot reserve slots because the queues belong to bitty#2602.
    fn finish_terminal(
        &mut self,
        session_id: SessionId,
        outcome: TerminalOutcome,
        actor: Actor,
        now: MonotonicMs,
    ) -> Result<Termination, LeaseRefusal> {
        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let attached: Vec<AttachId> = state.leases.keys().copied().collect();
        // `D-04` first: a refused transition must not have detached a lease.
        self.registry
            .check_terminal_transition(session_id, outcome)?;
        // `D-04`/`D-09`: a terminal session has no live lease. Every lease is
        // invalidated by this transition, so release the derived attach count
        // through one visible detach per lease before the record freezes. The
        // registry keeps the count it is given: the event stream pins surviving
        // leases on a terminal record (`D-18`), while this host invalidates all
        // of them, so the terminal notice reports no attaches.
        for _ in &attached {
            self.registry.detach(session_id, now)?;
        }
        let session = self.registry.terminate(session_id, outcome, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        state.leases.clear();
        state.controller = None;
        state.stop = None;
        state.cancel = None;
        state.token = TokenSlot::Retired;
        for attach_id in &attached {
            self.lease_sessions.remove(attach_id);
        }
        Ok(Termination::Completed(TerminalNotice {
            session,
            reason: outcome.reason(),
            exit: outcome.exit(),
            cleanup: outcome.cleanup(),
            actor,
            attached,
        }))
    }

    fn resolve_relation<'a>(
        &self,
        session_id: SessionId,
        authority: &ControlAuthority<'a>,
        now: MonotonicMs,
    ) -> Result<Relation<'a>, LeaseRefusal> {
        if authority.is_operator() {
            return Ok(Relation::Operator);
        }
        if let Some(lease_id) = authority.lease {
            // A lease-bearing request is decided by its role alone. An
            // observer never carries control authority, even with a valid
            // token and even with control vacant (`D-09`), so the token is
            // never consulted on this branch.
            let lease = self.actionable_lease(session_id, lease_id, now)?;
            if lease.role().can_control() {
                return Ok(Relation::Controller(lease_id));
            }
            return Err(LeaseRefusal::ControllerRequired);
        }
        // No lease: the only remaining non-operator authority is the token,
        // and it is valid only while control is vacant.
        let token = authority.token.ok_or(LeaseRefusal::ControllerRequired)?;
        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if state.live_controller(now).is_some() {
            return Err(LeaseRefusal::ControlHeld);
        }
        Ok(Relation::VacantToken(token))
    }

    fn authorize_control(
        &self,
        session_id: SessionId,
        expected_generation: u64,
        authority: &ControlAuthority<'_>,
        now: MonotonicMs,
    ) -> Result<Authorized, LeaseRefusal> {
        let snapshot = self.live_session(session_id)?;
        let relation = self.resolve_relation(session_id, authority, now)?;
        match relation {
            Relation::Operator => Err(LeaseRefusal::OperatorPathRequired),
            Relation::Controller(controller) => {
                if snapshot.generation != expected_generation {
                    return Err(stale_generation(&snapshot));
                }
                Ok(Authorized::Controller(controller))
            }
            Relation::VacantToken(token) => {
                // Generation before the token comparison: a stale request must
                // not become a token-verification oracle.
                if snapshot.generation != expected_generation {
                    return Err(stale_generation(&snapshot));
                }
                self.verify_token(session_id, token)?;
                Ok(Authorized::VacantToken)
            }
        }
    }

    fn verify_token(
        &self,
        session_id: SessionId,
        token: &ControlToken,
    ) -> Result<(), LeaseRefusal> {
        let state = self
            .sessions
            .get(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        if state.token_matches(token) {
            Ok(())
        } else {
            Err(LeaseRefusal::InvalidControlToken)
        }
    }

    fn actionable_lease(
        &self,
        session_id: SessionId,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<Lease, LeaseRefusal> {
        if self.lease_sessions.get(&attach_id) != Some(&session_id) {
            return Err(LeaseRefusal::NotAttached);
        }
        let lease = self
            .sessions
            .get(&session_id)
            .and_then(|state| state.leases.get(&attach_id))
            .copied()
            .ok_or(LeaseRefusal::NotAttached)?;
        if lease.is_expired_at(now) {
            return Err(LeaseRefusal::NotAttached);
        }
        Ok(lease)
    }

    fn remove_lease(
        &mut self,
        session_id: SessionId,
        attach_id: AttachId,
        now: MonotonicMs,
    ) -> Result<(Lease, SessionSnapshot), LeaseRefusal> {
        let exists = self
            .sessions
            .get(&session_id)
            .is_some_and(|state| state.leases.contains_key(&attach_id));
        if !exists {
            return Err(LeaseRefusal::NotAttached);
        }
        let session = self.registry.detach(session_id, now)?;
        let state = self
            .sessions
            .get_mut(&session_id)
            .ok_or(LeaseRefusal::SessionNotFound)?;
        let lease = state
            .leases
            .remove(&attach_id)
            .ok_or(LeaseRefusal::NotAttached)?;
        if state.controller == Some(attach_id) {
            state.controller = None;
        }
        self.lease_sessions.remove(&attach_id);
        Ok((lease, session))
    }

    fn renew_authorized(&mut self, authorized: Authorized, now: MonotonicMs) {
        if let Authorized::Controller(attach_id) = authorized {
            self.renew_lease(attach_id, now);
        }
    }

    fn renew_lease(&mut self, attach_id: AttachId, now: MonotonicMs) {
        let Some(session_id) = self.lease_sessions.get(&attach_id).copied() else {
            return;
        };
        let ttl = self.limits.attach_lease_ttl_ms();
        if let Some(state) = self.sessions.get_mut(&session_id) {
            if let Some(lease) = state.leases.get_mut(&attach_id) {
                lease.renew(now, ttl);
            }
        }
    }

    /// Compose the live attach count into a registry snapshot.
    ///
    /// The registry stores attach/detach records; only this layer knows the
    /// lease TTL, so `attach_count`/`attached` must be derived here at the
    /// request instant (`R-S18`). A session without a lease table is terminal
    /// or already collected: its count is zero.
    fn compose_snapshot(&self, mut snapshot: SessionSnapshot, now: MonotonicMs) -> SessionSnapshot {
        let count = self
            .sessions
            .get(&snapshot.session_id)
            .map_or(0, |state| state.live_attach_count(now));
        snapshot.attach_count = count;
        snapshot.attached = count > 0;
        snapshot
    }

    fn live_session(&self, session_id: SessionId) -> Result<SessionSnapshot, LeaseRefusal> {
        match self.registry.snapshot(session_id) {
            Some(snapshot) if !snapshot.state.is_terminal() => Ok(snapshot),
            Some(snapshot) => Err(LeaseRefusal::NotRunning {
                state: snapshot.state,
            }),
            None => Err(LeaseRefusal::SessionNotFound),
        }
    }

    fn mint_token(&mut self) -> Result<ControlToken, LeaseRefusal> {
        ControlToken::generate_with(self.entropy.as_mut())
            .map_err(|_| LeaseRefusal::EntropyUnavailable)
    }

    /// Mint the replacement token for a claim or transfer.
    ///
    /// A rotation must change the secret. A CSPRNG that repeats the currently
    /// stored token is broken, and silently accepting the repeat would leave the
    /// previous holder's secret valid after a "rotation", so the repeat is
    /// refused as [`LeaseRefusal::EntropyUnavailable`] (fail closed) and the
    /// operation changes nothing.
    fn mint_rotated_token(&mut self, session_id: SessionId) -> Result<ControlToken, LeaseRefusal> {
        let token = self.mint_token()?;
        let repeats_current = self
            .sessions
            .get(&session_id)
            .is_some_and(|state| state.token_matches(&token));
        if repeats_current {
            return Err(LeaseRefusal::EntropyUnavailable);
        }
        Ok(token)
    }

    fn mint_attach_id(&mut self) -> Result<AttachId, LeaseRefusal> {
        for _ in 0..ID_GENERATION_ATTEMPTS {
            let attach_id = AttachId::generate_with(self.entropy.as_mut())
                .map_err(|_| LeaseRefusal::EntropyUnavailable)?;
            if !self.lease_sessions.contains_key(&attach_id) {
                return Ok(attach_id);
            }
        }
        Err(LeaseRefusal::IdCollision)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authorized {
    Controller(AttachId),
    VacantToken,
    Operator,
}

impl Authorized {
    const fn actor(self) -> Actor {
        match self {
            Self::Controller(attach_id) => Actor::Lease(attach_id),
            Self::VacantToken => Actor::ControlToken,
            Self::Operator => Actor::Operator,
        }
    }
}

enum Relation<'a> {
    Controller(AttachId),
    VacantToken(&'a ControlToken),
    Operator,
}

struct SessionLeaseState {
    token: TokenSlot,
    claimed: bool,
    controller: Option<AttachId>,
    leases: BTreeMap<AttachId, Lease>,
    stop: Option<StopState>,
    cancel: Option<CancelIntent>,
}

impl SessionLeaseState {
    fn new(digest: ControlTokenDigest) -> Self {
        Self {
            token: TokenSlot::Current(digest),
            claimed: false,
            controller: None,
            leases: BTreeMap::new(),
            stop: None,
            cancel: None,
        }
    }

    fn token_matches(&self, token: &ControlToken) -> bool {
        match &self.token {
            TokenSlot::Current(digest) => digest.matches(token),
            TokenSlot::Retired => false,
        }
    }

    fn live_controller(&self, now: MonotonicMs) -> Option<AttachId> {
        self.controller.filter(|attach_id| {
            self.leases
                .get(attach_id)
                .is_some_and(|lease| !lease.is_expired_at(now))
        })
    }

    fn expired_controller(&self, now: MonotonicMs) -> Option<Lease> {
        self.controller
            .and_then(|attach_id| self.leases.get(&attach_id))
            .filter(|lease| lease.is_expired_at(now))
            .copied()
    }

    /// Live lease count at `now`, controller included.
    ///
    /// An expired lease awaiting [`SessionHost::expire_due`] is not live and is
    /// excluded, so the view says what the contract means by `attach_count`
    /// for authority rather than how many records the sweep still holds.
    fn live_attach_count(&self, now: MonotonicMs) -> u16 {
        let live = self
            .leases
            .values()
            .filter(|lease| !lease.is_expired_at(now))
            .count();
        u16::try_from(live).unwrap_or(u16::MAX)
    }
}

enum TokenSlot {
    Current(ControlTokenDigest),
    Retired,
}

struct StopState {
    reason: TerminalReason,
    actor: Actor,
    deadline: MonotonicMs,
    escalated: bool,
}

impl StopState {
    fn stage(&self, now: MonotonicMs) -> AbortStage {
        if self.escalated || now.as_millis() >= self.deadline.as_millis() {
            AbortStage::KillDue
        } else {
            AbortStage::Graceful
        }
    }

    fn view(&self, now: MonotonicMs) -> StopView {
        StopView {
            cause: self.reason,
            deadline: self.deadline,
            stage: self.stage(now),
            escalated: self.escalated,
        }
    }
}

#[derive(Clone, Copy)]
struct CancelIntent {
    reason: TerminalReason,
    actor: Actor,
}

fn stale_generation(snapshot: &SessionSnapshot) -> LeaseRefusal {
    LeaseRefusal::StaleGeneration {
        current_generation: snapshot.generation,
        current_revision: snapshot.state_revision,
    }
}

fn authority_actor(authority: &ControlAuthority<'_>) -> Actor {
    if authority.is_operator() {
        Actor::Operator
    } else if let Some(attach_id) = authority.lease {
        Actor::Lease(attach_id)
    } else {
        Actor::ControlToken
    }
}
#[cfg(test)]
mod tests;
