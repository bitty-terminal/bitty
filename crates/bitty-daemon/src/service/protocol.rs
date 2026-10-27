//! Per-connection state for the `bittyd` service (`D-07`, `D-08`, `D-09`).
//!
//! A [`Connection`] is created at transport accept, after the peer credential
//! capture and before any frame is parsed. It holds:
//!
//! - the server-assigned 128-bit [`ClientId`] (`D-08`), never a client value;
//! - the captured [`PeerGuard`] that every privileged dispatch re-checks;
//! - the effective [`EffectiveScopes`], fixed once negotiation succeeds;
//! - the connection's live attach leases, so visibility and heartbeat never
//!   consult another connection's state;
//! - a single pending control token per transferred lease, so a transfer
//!   delivers the rotated token to the target connection and never to the
//!   requester.
//!
//! Scope changes are impossible after negotiation: the host grant is consumed
//! at `daemon.negotiate`, and the connection has no method that widens its own
//! scopes (`D-07`).

use std::collections::BTreeMap;

use crate::audit::AuditActor;
use crate::handshake::{ClientId, EffectiveScopes, PeerIdentityFacts};
use crate::lease::ControlToken;
use crate::peer::{DispatchBoundary, PeerGuard, PeerIdentity};
use crate::registry::{AttachId, SessionId};
use crate::service::ServiceScope;

/// One authenticated client connection.
pub struct Connection {
    client_id: ClientId,
    peer: PeerGuard,
    scopes: EffectiveScopes,
    negotiated: bool,
    correlation_digest: Option<[u8; 16]>,
    host_grant: Option<[u8; 16]>,
    leases: BTreeMap<AttachId, SessionId>,
    pending_tokens: BTreeMap<AttachId, ControlToken>,
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
            pending_tokens: BTreeMap::new(),
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
        self.leases.insert(attach_id, session_id);
    }

    /// Remove a lease this connection owned.
    pub(crate) fn forget_lease(&mut self, attach_id: AttachId) {
        self.leases.remove(&attach_id);
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
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, unix))]
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
