//! Per-connection state for the `bittyd` service (`D-07`, `D-08`, `D-09`).
//!
//! A [`Connection`] is created at transport accept, after the peer credential
//! capture and before any frame is parsed. It holds:
//!
//! - the server-assigned 128-bit [`ClientId`] (`D-08`), never a client value;
//! - the captured [`PeerGuard`] that every privileged dispatch re-checks;
//! - the effective [`EffectiveScopes`], fixed once negotiation succeeds;
//! - the connection's live attach leases, so visibility and heartbeat never
//!   consult another connection's state, plus a bounded per-connection history
//!   of handles it detached itself for the idempotent repeat-detach reply;
//! - a single pending control token per transferred lease, so a transfer
//!   delivers the rotated token to the target connection and never to the
//!   requester.
//!
//! Scope changes are impossible after negotiation: the host grant is consumed
//! at `daemon.negotiate`, and the connection has no method that widens its own
//! scopes (`D-07`).

use std::collections::{BTreeMap, VecDeque};

use crate::audit::AuditActor;
use crate::events::Event;
use crate::handshake::{ClientId, EffectiveScopes, PeerIdentityFacts};
use crate::lease::ControlToken;
use crate::peer::{DispatchBoundary, PeerGuard, PeerIdentity};
use crate::registry::{AttachId, SessionId};
use crate::service::ServiceScope;

/// Detached handles one connection remembers for the idempotent repeat detach
/// (`L-02`, `V-07`).
///
/// The bound is twice the largest attach table (one controller plus the
/// sixteen-lease observer ceiling) plus headroom, so a full detach and
/// re-attach cycle fits while a connection cannot grow the history without
/// bound. The entry names only a handle this connection detached — its own or
/// one it revoked with `session.control`/`daemon.control` — and the session it
/// belonged to; it dies with the session record and never authorizes anything.
const DETACHED_HANDLE_HISTORY: usize = 64;

/// One authenticated client connection.
pub struct Connection {
    client_id: ClientId,
    peer: PeerGuard,
    scopes: EffectiveScopes,
    negotiated: bool,
    correlation_digest: Option<[u8; 16]>,
    host_grant: Option<[u8; 16]>,
    leases: BTreeMap<AttachId, SessionId>,
    detached: VecDeque<(AttachId, SessionId)>,
    pending_tokens: BTreeMap<AttachId, ControlToken>,
    /// The client asked for pushed session events at negotiate (`D-10`).
    events: bool,
    /// Frames that must be written before the connection's next response, in
    /// order: drained lease queues and the final `lease_revoked` frame.
    pending_frames: Vec<Event>,
}

impl Connection {
    /// Build the connection state captured at accept.
    #[must_use]
    pub fn new(client_id: ClientId, peer: PeerGuard, scopes: EffectiveScopes) -> Self {
        Self {
            client_id,
            peer,
            scopes,
            negotiated: false,
            correlation_digest: None,
            host_grant: None,
            leases: BTreeMap::new(),
            detached: VecDeque::new(),
            pending_tokens: BTreeMap::new(),
            events: false,
            pending_frames: Vec::new(),
        }
    }

    /// The server-assigned connection identity.
    #[must_use]
    pub const fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// The audit projection of [`Self::client_id`].
    #[must_use]
    pub fn audit_ref(&self) -> u64 {
        self.client_id.audit_ref()
    }

    /// The captured peer.
    #[must_use]
    pub const fn peer(&self) -> &PeerGuard {
        &self.peer
    }

    /// The peer facts captured at accept.
    #[must_use]
    pub fn peer_facts(&self) -> PeerIdentityFacts {
        PeerIdentityFacts::from_peer(&self.peer.identity())
    }

    /// The authenticated peer uid.
    #[must_use]
    pub fn peer_uid(&self) -> u32 {
        self.peer.identity().uid()
    }

    /// The audit actor projection for this connection.
    #[must_use]
    pub fn audit_actor(&self) -> AuditActor {
        let identity: PeerIdentity = self.peer.identity();
        AuditActor {
            peer_uid: Some(identity.uid()),
            peer_sid: None,
            peer_pid: Some(identity.pid()),
            client_id: Some(self.audit_ref()),
            client_correlation_digest: self.correlation_digest,
        }
    }

    /// Effective scopes fixed for this connection.
    #[must_use]
    pub const fn scopes(&self) -> &EffectiveScopes {
        &self.scopes
    }

    /// Mutable access used only by the negotiate path.
    pub(crate) fn scopes_mut(&mut self) -> &mut EffectiveScopes {
        &mut self.scopes
    }

    /// Whether the connection holds a service scope.
    #[must_use]
    pub const fn has_service(&self, scope: ServiceScope) -> bool {
        self.scopes.has_service(scope)
    }

    /// Whether negotiation completed.
    #[must_use]
    pub const fn is_negotiated(&self) -> bool {
        self.negotiated
    }

    /// Mark the connection negotiated.
    pub(crate) fn mark_negotiated(&mut self) {
        self.negotiated = true;
    }

    /// The audited correlation digest, when the client sent a valid value.
    #[must_use]
    pub const fn correlation_digest(&self) -> Option<[u8; 16]> {
        self.correlation_digest
    }

    /// Record the correlation digest (never the raw value).
    pub(crate) fn set_correlation_digest(&mut self, digest: Option<[u8; 16]>) {
        self.correlation_digest = digest;
    }

    /// The consumed host-consent grant id, when one was consumed.
    #[must_use]
    pub const fn host_grant(&self) -> Option<[u8; 16]> {
        self.host_grant
    }

    /// Bind the consumed host grant to this connection.
    pub(crate) fn set_host_grant(&mut self, grant_id: [u8; 16]) {
        self.host_grant = Some(grant_id);
    }

    /// Re-check the captured peer before a privileged effect.
    ///
    /// A dead peer, a recycled pid, a changed identity, or an unavailable
    /// credential refuses the dispatch and applies nothing.
    ///
    /// # Errors
    ///
    /// Returns [`crate::peer::PeerError`] for the failed check.
    pub fn recheck(&self, boundary: DispatchBoundary) -> Result<(), crate::peer::PeerError> {
        self.peer.recheck(boundary)
    }

    /// Record a lease this connection owns.
    pub(crate) fn register_lease(&mut self, attach_id: AttachId, session_id: SessionId) {
        self.detached.retain(|(detached, _)| *detached != attach_id);
        self.leases.insert(attach_id, session_id);
    }

    /// Remove a lease this connection owned.
    ///
    /// A removed handle stays in the bounded history so a repeat detach can
    /// answer the contract's idempotent `not_attached` acknowledgement
    /// (`L-02`, `V-07`) instead of the uniform `NotFound`.
    pub(crate) fn forget_lease(&mut self, attach_id: AttachId) {
        if let Some(session_id) = self.leases.remove(&attach_id) {
            self.remember_detached(attach_id, session_id);
        }
    }

    /// Remember a handle this connection successfully detached.
    ///
    /// The acting connection is recorded for its own detach and for a
    /// controller/operator revocation of another connection's lease, so a
    /// repeat of either answers `not_attached` rather than `NotFound`
    /// (`L-02`, `V-07`, bitty#2768). The history is a bounded FIFO and an
    /// already-present handle moves to the back instead of duplicating.
    pub(crate) fn remember_detached(&mut self, attach_id: AttachId, session_id: SessionId) {
        self.detached.retain(|(detached, _)| *detached != attach_id);
        if self.detached.len() == DETACHED_HANDLE_HISTORY {
            self.detached.pop_front();
        }
        self.detached.push_back((attach_id, session_id));
    }

    /// The session of a handle this connection detached.
    ///
    /// `None` for a live lease, a handle another connection detached without
    /// this connection acting, one aged out of the bounded history, and any
    /// handle this connection never touched. The lookup never implies
    /// liveness: it only names the session the acknowledgement is about.
    #[must_use]
    pub fn detached_session(&self, attach_id: AttachId) -> Option<SessionId> {
        self.detached
            .iter()
            .rev()
            .find(|(detached, _)| *detached == attach_id)
            .map(|(_, session_id)| *session_id)
    }

    /// Whether this connection owns `attach_id`.
    #[must_use]
    pub fn owns_lease(&self, attach_id: AttachId) -> bool {
        self.leases.contains_key(&attach_id)
    }

    /// The session a lease of this connection belongs to.
    #[must_use]
    pub fn lease_session(&self, attach_id: AttachId) -> Option<SessionId> {
        self.leases.get(&attach_id).copied()
    }

    /// Every lease this connection owns.
    pub fn leases(&self) -> impl Iterator<Item = (AttachId, SessionId)> + '_ {
        self.leases
            .iter()
            .map(|(attach, session)| (*attach, *session))
    }

    /// Whether this connection holds a live lease on `session_id`.
    #[must_use]
    pub fn has_lease_on(&self, session_id: SessionId) -> bool {
        self.leases.values().any(|owned| *owned == session_id)
    }

    /// Queue the rotated token addressed to a transfer target.
    ///
    /// The caller must have already checked that the target lease belongs to
    /// this connection; the token is delivered on this connection's next
    /// response that names the lease, never on the requester's response.
    pub(crate) fn queue_control_token(&mut self, attach_id: AttachId, token: ControlToken) {
        self.pending_tokens.insert(attach_id, token);
    }

    /// Take the pending control token for one of this connection's leases.
    #[must_use]
    pub(crate) fn take_control_token(&mut self, attach_id: AttachId) -> Option<ControlToken> {
        if !self.owns_lease(attach_id) {
            return None;
        }
        self.pending_tokens.remove(&attach_id)
    }

    /// Record the client's `events` opt-in from the accepted negotiate.
    pub(crate) const fn mark_events(&mut self, events: bool) {
        self.events = events;
    }

    /// Whether this connection receives pushed session events.
    #[must_use]
    pub const fn events_enabled(&self) -> bool {
        self.events
    }

    /// Queue frames that must be written before the next response.
    ///
    /// The service uses this for the final frames of a lease that is about to
    /// disappear: its queued events and the target-only `lease_revoked` frame
    /// stay ordered and are never dropped by the lease bookkeeping.
    pub(crate) fn queue_frames(&mut self, frames: Vec<Event>) {
        self.pending_frames.extend(frames);
    }

    /// Take every queued frame, in order.
    #[cfg(all(unix, target_os = "linux"))]
    #[must_use]
    pub(crate) fn take_pending_frames(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending_frames)
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("client_id", &self.client_id)
            .field("peer", &self.peer)
            .field("scopes", &self.scopes)
            .field("negotiated", &self.negotiated)
            .field("leases", &self.leases.len())
            .field("detached", &self.detached.len())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, target_os = "linux"))]
// The protocol tests build a real PeerGuard from /proc; the served loop is
// Linux-only (D-15), so they do not run on the other tested cells.
mod tests {
    use super::*;
    use crate::handshake::ClientId;
    use crate::service::ServiceScopeSet;

    fn connection() -> Connection {
        let peer = PeerGuard::for_current_process();
        let mut scopes = EffectiveScopes::new();
        scopes.service = ServiceScopeSet::baseline();
        Connection::new(ClientId::generate().expect("entropy"), peer, scopes)
    }

    #[test]
    fn a_fresh_connection_is_deny_by_default_and_not_negotiated() {
        let connection = connection();
        assert!(!connection.is_negotiated());
        assert!(connection.has_service(ServiceScope::SessionInspect));
        assert!(!connection.has_service(ServiceScope::SessionHost));
        assert!(!connection.has_service(ServiceScope::DaemonControl));
        assert_eq!(connection.correlation_digest(), None);
        assert_eq!(connection.host_grant(), None);
    }

    #[test]
    fn leases_are_owned_per_connection_and_never_cross() {
        let mut first = connection();
        let second = connection();
        let session = SessionId::parse(&"1".repeat(32)).expect("session id");
        let attach = AttachId::parse(&"2".repeat(32)).expect("attach id");
        first.register_lease(attach, session);
        assert!(first.owns_lease(attach));
        assert!(first.has_lease_on(session));
        assert!(!second.owns_lease(attach));
        assert!(!second.has_lease_on(session));
        assert_eq!(second.lease_session(attach), None);
        assert_eq!(first.lease_session(attach), Some(session));
        first.forget_lease(attach);
        assert!(!first.owns_lease(attach));
    }

    #[test]
    fn a_detached_handle_is_remembered_only_by_the_acting_connection_and_within_bound() {
        let mut first = connection();
        let second = connection();
        let session = SessionId::parse(&"1".repeat(32)).expect("session id");
        let attach = AttachId::parse(&"2".repeat(32)).expect("attach id");
        first.register_lease(attach, session);
        first.forget_lease(attach);
        assert_eq!(first.detached_session(attach), Some(session));
        assert_eq!(second.detached_session(attach), None);
        assert_eq!(first.lease_session(attach), None);

        // Re-attaching the same handle (a reissued id) drops the stale entry.
        first.register_lease(attach, session);
        assert_eq!(first.detached_session(attach), None);

        // A controller/operator revocation of another connection's lease
        // records the acting connection too, so its repeat is `not_attached`.
        let foreign = AttachId::parse(&"3".repeat(32)).expect("attach id");
        first.remember_detached(foreign, session);
        assert_eq!(first.detached_session(foreign), Some(session));
        assert_eq!(second.detached_session(foreign), None);

        // The history is a bounded FIFO: the oldest handle ages out and the
        // newest still answers the repeat.
        let mut ids = Vec::new();
        for seed in 0u8..=u8::try_from(DETACHED_HANDLE_HISTORY).expect("small bound") {
            let id = AttachId::parse(&format!("{seed:02x}").repeat(16)).expect("attach id");
            first.register_lease(id, session);
            first.forget_lease(id);
            ids.push(id);
        }
        assert_eq!(first.detached_session(ids[0]), None);
        assert_eq!(
            first.detached_session(*ids.last().expect("one id")),
            Some(session)
        );
        assert!(first.detached.len() <= DETACHED_HANDLE_HISTORY);
    }

    #[test]
    fn a_pending_control_token_is_delivered_only_to_its_owner() {
        let mut first = connection();
        let mut second = connection();
        let session = SessionId::parse(&"1".repeat(32)).expect("session id");
        let attach = AttachId::parse(&"2".repeat(32)).expect("attach id");
        first.register_lease(attach, session);
        first.queue_control_token(attach, ControlToken::from_bytes([7u8; 32]));
        assert!(second.take_control_token(attach).is_none());
        let token = first.take_control_token(attach).expect("delivery");
        assert_eq!(token.expose_secret(), &[7u8; 32]);
        assert!(first.take_control_token(attach).is_none(), "delivered once");
    }

    #[test]
    fn debug_never_prints_a_pending_token() {
        let mut connection = connection();
        let session = SessionId::parse(&"1".repeat(32)).expect("session id");
        let attach = AttachId::parse(&"2".repeat(32)).expect("attach id");
        connection.register_lease(attach, session);
        connection.queue_control_token(attach, ControlToken::from_bytes([9u8; 32]));
        let debug = format!("{connection:?}");
        assert!(!debug.contains("090909"));
        assert!(debug.contains("Connection"));
    }
}
