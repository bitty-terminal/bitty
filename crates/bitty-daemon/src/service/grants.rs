//! Host-consent and observer grants (`D-07`, `D-09`, `D-16`).
//!
//! Both grant kinds share one bounded store and one record shape:
//!
//! - a fresh 256-bit [`SecretBearer`] delivered once at mint;
//! - a non-secret 128-bit `grant_id`, used for audit, revoke, and idempotent
//!   replay;
//! - the authenticated peer uid the grant is bound to;
//! - the session an observer grant names (a host grant names none);
//! - a TTL from the compiled `D-14` table (`host_grant_ttl_ms`,
//!   `observer_grant_ttl_ms`);
//! - single use: the first successful consume retires the bearer, and every
//!   later attempt is the uniform `Unusable` refusal.
//!
//! Rules encoded here:
//!
//! - the store keeps a bearer only while the grant is unconsumed, unexpired,
//!   and unrevoked, so a keyed `session.grant` replay can return the same
//!   unconsumed bearer while the record exists (`D-18`);
//! - a missing, expired, consumed, revoked, wrong-session, or wrong-peer grant
//!   is **one** [`GrantRefusal::Unusable`], so an observer attach cannot use
//!   the refusal to probe a grant's state;
//! - a host grant that cannot be consumed at `daemon.negotiate` simply adds no
//!   `session.host` scope (the caller still sees one `Denied/ConsentRequired`
//!   at create); it never closes the connection;
//! - revoke is keyless and idempotent by `grant_id`; a second revoke is a
//!   `not_active` result, never a refusal;
//! - the pending caps (`max_pending_host_grants` and
//!   `max_pending_observer_grants_per_session`) are whole refusals mapped to
//!   `LimitExceeded`; a mint never evicts a live grant.

use std::collections::BTreeMap;
use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::handshake::{SecretBearer, hex};
use crate::limits::{LimitId, Limits};
use crate::registry::SessionId;

/// Domain separator for the grant-bearer digest.
const GRANT_DIGEST_DOMAIN: &[u8] = b"bittyd/grant-bearer/v1\0";

/// Bytes in a grant id (128 bits).
pub const GRANT_ID_BYTES: usize = 16;

/// A non-secret grant identity (`D-16`).
///
/// The id is the audit target and the revoke target. It is not a bearer and
/// grants nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantId([u8; GRANT_ID_BYTES]);

impl GrantId {
    /// Mint a fresh id from the operating-system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError::EntropyUnavailable`] when the CSPRNG fails.
    pub fn generate() -> Result<Self, GrantError> {
        let mut bytes = [0u8; GRANT_ID_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| GrantError::EntropyUnavailable)?;
        Ok(Self(bytes))
    }

    /// Build an id from raw bytes (the connection stores the consumed id as
    /// bytes; this is the inverse projection).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; GRANT_ID_BYTES]) -> Self {
        Self(bytes)
    }

    /// Parse 32 lowercase hex characters.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError::InvalidGrantId`] for any other form.
    pub fn parse(value: &str) -> Result<Self, GrantError> {
        if value.len() != 32 {
            return Err(GrantError::InvalidGrantId);
        }
        let mut bytes = [0u8; GRANT_ID_BYTES];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = nibble(chunk[0]).ok_or(GrantError::InvalidGrantId)?;
            let low = nibble(chunk[1]).ok_or(GrantError::InvalidGrantId)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// The raw 128 bits.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; GRANT_ID_BYTES] {
        &self.0
    }

    /// The canonical 32 lowercase hex spelling.
    #[must_use]
    pub fn as_hex(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

const fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// What a grant authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// Host consent: adds `session.host` when consumed at `daemon.negotiate`.
    Host,
    /// Observer consent: authorizes one observer attach on the named session.
    Observer,
}

impl GrantKind {
    /// Static spelling for results and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Observer => "observer",
        }
    }
}

impl fmt::Display for GrantKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A refused grant operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantRefusal {
    /// The pending cap for this grant kind is reached.
    Capacity {
        /// The refused kind.
        kind: GrantKind,
        /// The enforced cap.
        limit: usize,
    },
    /// The grant cannot be used. One variant for missing, expired, consumed,
    /// revoked, wrong-session, and wrong-peer so the client cannot distinguish
    /// them.
    Unusable,
}

impl fmt::Display for GrantRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity { kind, limit } => {
                write!(formatter, "{kind} grant cap {limit} reached")
            }
            Self::Unusable => formatter.write_str("grant is not usable"),
        }
    }
}

impl std::error::Error for GrantRefusal {}

/// A malformed or unavailable grant store operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    /// The operating-system CSPRNG failed.
    EntropyUnavailable,
    /// A grant id is not 32 lowercase hex characters.
    InvalidGrantId,
    /// The grant record exceeds `host_grant_record_bytes`.
    RecordTooLarge,
}

impl fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => formatter.write_str("operating system entropy unavailable"),
            Self::InvalidGrantId => {
                formatter.write_str("grant id is not 32 lowercase hex characters")
            }
            Self::RecordTooLarge => formatter.write_str("grant record exceeds its bound"),
        }
    }
}

impl std::error::Error for GrantError {}

/// A freshly minted grant.
pub struct MintedGrant {
    /// The non-secret id.
    pub grant_id: GrantId,
    /// The bearer, returned to the minter exactly once.
    pub bearer: SecretBearer,
    /// Absolute expiry on the daemon monotonic clock.
    pub expires_at_ms: u64,
}

impl fmt::Debug for MintedGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MintedGrant")
            .field("grant_id", &self.grant_id)
            .field("bearer", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// A consumed grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumedGrant {
    /// The non-secret id.
    pub grant_id: GrantId,
    /// The consumed kind.
    pub kind: GrantKind,
    /// The session an observer grant named.
    pub session_id: Option<SessionId>,
}

/// The result of a revoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// An unconsumed, unexpired grant was revoked.
    Revoked,
    /// The grant is unknown, consumed, expired, or already revoked.
    NotActive,
}

struct GrantRecord {
    grant_id: GrantId,
    kind: GrantKind,
    session_id: Option<SessionId>,
    peer_uid: u32,
    expires_at_ms: u64,
    consumed: bool,
    revoked: bool,
    bearer: Option<SecretBearer>,
    digest: [u8; 32],
}

impl GrantRecord {
    fn is_active(&self, now_ms: u64) -> bool {
        !self.consumed && !self.revoked && now_ms < self.expires_at_ms
    }

    fn accounted_bytes(&self) -> usize {
        // grant id + kind + optional session id + peer uid + expiry + flags +
        // bearer + digest, with the fixed widths the record actually holds.
        GRANT_ID_BYTES + 1 + GRANT_ID_BYTES + 4 + 8 + 2 + 32 + 32
    }
}

impl fmt::Debug for GrantRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrantRecord")
            .field("grant_id", &self.grant_id)
            .field("kind", &self.kind)
            .field("session_id", &self.session_id)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("consumed", &self.consumed)
            .field("revoked", &self.revoked)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// Effective grant bounds from the compiled `D-14` table.
#[derive(Debug, Clone, Copy)]
struct GrantLimits {
    host_ttl_ms: u64,
    observer_ttl_ms: u64,
    max_host: usize,
    max_observer_per_session: usize,
    record_bytes: usize,
}

impl GrantLimits {
    fn from_limits(limits: &Limits) -> Self {
        Self {
            host_ttl_ms: limits.effective(LimitId::HostGrantTtlMs),
            observer_ttl_ms: limits.effective(LimitId::ObserverGrantTtlMs),
            max_host: limits.effective_usize(LimitId::MaxPendingHostGrants),
            max_observer_per_session: limits
                .effective_usize(LimitId::MaxPendingObserverGrantsPerSession),
            record_bytes: limits.effective_usize(LimitId::HostGrantRecordBytes),
        }
    }
}

/// The bounded host- and observer-grant store.
#[derive(Debug)]
pub struct GrantStore {
    limits: GrantLimits,
    grants: BTreeMap<GrantId, GrantRecord>,
}

impl GrantStore {
    /// Build an empty store from the effective limits.
    #[must_use]
    pub fn new(limits: &Limits) -> Self {
        Self {
            limits: GrantLimits::from_limits(limits),
            grants: BTreeMap::new(),
        }
    }

    /// Number of records, active or retired, still awaiting eviction.
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// Whether the store holds no record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Whether an unconsumed host grant is live.
    #[must_use]
    pub fn host_grant_active(&self, grant_id: GrantId, now_ms: u64) -> bool {
        self.grants
            .get(&grant_id)
            .is_some_and(|record| record.kind == GrantKind::Host && record.is_active(now_ms))
    }

    /// Whether a consumed host grant still authorizes its consuming connection.
    ///
    /// The grant was consumed at `daemon.negotiate`, so `consumed` is expected;
    /// the create-time check is that the same grant record was not revoked and
    /// has not expired since, and that it is bound to this peer. This is what
    /// turns a missing, expired, revoked, consumed-elsewhere, or wrong-peer
    /// grant into the one `Denied/ConsentRequired` at create.
    #[must_use]
    pub fn host_grant_bound(&self, grant_id: GrantId, peer_uid: u32, now_ms: u64) -> bool {
        self.grants.get(&grant_id).is_some_and(|record| {
            record.kind == GrantKind::Host
                && record.peer_uid == peer_uid
                && !record.revoked
                && now_ms < record.expires_at_ms
        })
    }

    /// Mint a host-consent grant bound to `peer_uid`.
    ///
    /// # Errors
    ///
    /// [`GrantRefusal::Capacity`] when the pending cap is reached;
    /// [`GrantError`] for an entropy, id, or record-size failure.
    pub fn mint_host(&mut self, peer_uid: u32, now_ms: u64) -> Result<MintedGrant, MintError> {
        let active = self
            .grants
            .values()
            .filter(|record| record.kind == GrantKind::Host && record.is_active(now_ms))
            .count();
        if active >= self.limits.max_host {
            return Err(MintError::Refused(GrantRefusal::Capacity {
                kind: GrantKind::Host,
                limit: self.limits.max_host,
            }));
        }
        self.mint(
            GrantKind::Host,
            None,
            peer_uid,
            now_ms,
            self.limits.host_ttl_ms,
        )
    }

    /// Mint an observer grant for one session, bound to `peer_uid`.
    ///
    /// # Errors
    ///
    /// [`GrantRefusal::Capacity`] when the per-session observer cap is reached;
    /// [`GrantError`] for an entropy, id, or record-size failure.
    pub fn mint_observer(
        &mut self,
        session_id: SessionId,
        peer_uid: u32,
        now_ms: u64,
    ) -> Result<MintedGrant, MintError> {
        let active = self
            .grants
            .values()
            .filter(|record| {
                record.kind == GrantKind::Observer
                    && record.session_id == Some(session_id)
                    && record.is_active(now_ms)
            })
            .count();
        if active >= self.limits.max_observer_per_session {
            return Err(MintError::Refused(GrantRefusal::Capacity {
                kind: GrantKind::Observer,
                limit: self.limits.max_observer_per_session,
            }));
        }
        self.mint(
            GrantKind::Observer,
            Some(session_id),
            peer_uid,
            now_ms,
            self.limits.observer_ttl_ms,
        )
    }

    fn mint(
        &mut self,
        kind: GrantKind,
        session_id: Option<SessionId>,
        peer_uid: u32,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<MintedGrant, MintError> {
        let grant_id = GrantId::generate().map_err(MintError::Error)?;
        let bearer = mint_bearer().map_err(MintError::Error)?;
        let record = GrantRecord {
            grant_id,
            kind,
            session_id,
            peer_uid,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            consumed: false,
            revoked: false,
            digest: digest_of(&bearer),
            bearer: Some(bearer.clone()),
        };
        if record.accounted_bytes() > self.limits.record_bytes {
            return Err(MintError::Error(GrantError::RecordTooLarge));
        }
        let expires_at_ms = record.expires_at_ms;
        self.grants.insert(grant_id, record);
        Ok(MintedGrant {
            grant_id,
            bearer,
            expires_at_ms,
        })
    }

    /// Validate one grant without consuming it.
    ///
    /// The observer-attach path validates before it creates a lease and
    /// consumes only after the lease exists, so a refused attach does not burn
    /// a single-use grant. Every unusable cause is the same
    /// [`GrantRefusal::Unusable`].
    ///
    /// # Errors
    ///
    /// [`GrantRefusal::Unusable`] when no record matches every binding.
    pub fn validate(
        &self,
        bearer: &SecretBearer,
        kind: GrantKind,
        peer_uid: u32,
        session_id: Option<SessionId>,
        now_ms: u64,
    ) -> Result<GrantId, GrantRefusal> {
        let digest = digest_of(bearer);
        let mut found: Option<&GrantRecord> = None;
        for record in self.grants.values() {
            if bool::from(record.digest.ct_eq(&digest)) {
                found = Some(record);
            }
        }
        let Some(record) = found else {
            return Err(GrantRefusal::Unusable);
        };
        if record.kind != kind
            || record.peer_uid != peer_uid
            || record.session_id != session_id
            || !record.is_active(now_ms)
        {
            return Err(GrantRefusal::Unusable);
        }
        Ok(record.grant_id)
    }

    /// Resolve a stored 64-bit result ref to the grant it was derived from.
    ///
    /// The daemon derives `result_ref` from the first 64 bits of the grant id,
    /// so a replay never needs a separate unbounded reference table. A
    /// collision (two live grants sharing 64 bits) yields `None` and the replay
    /// fails closed rather than guessing.
    #[must_use]
    pub fn find_by_ref(&self, result_ref: u64) -> Option<GrantId> {
        let mut found = None;
        for grant_id in self.grants.keys() {
            if ref_of(grant_id.as_bytes()) == result_ref {
                if found.is_some() {
                    return None;
                }
                found = Some(*grant_id);
            }
        }
        found
    }

    /// The session a grant names, if the grant is still usable.
    ///
    /// `None` means the id is unknown, retired, or names a host grant. A
    /// `Some(None)` would be a host grant, which this read collapses into the
    /// same `None`: host grants are revoked through `daemon.host_grant`, never
    /// through `session.grant.revoke`.
    #[must_use]
    pub fn observer_grant_session(&self, grant_id: GrantId, now_ms: u64) -> Option<SessionId> {
        let record = self.grants.get(&grant_id)?;
        if record.kind != GrantKind::Observer || !record.is_active(now_ms) {
            return None;
        }
        record.session_id
    }

    /// Consume one grant.
    ///
    /// The same [`GrantRefusal::Unusable`] is returned for a missing, expired,
    /// consumed, revoked, wrong-kind, wrong-session, or wrong-peer grant.
    ///
    /// # Errors
    ///
    /// [`GrantRefusal::Unusable`] when no record matches every binding.
    pub fn consume(
        &mut self,
        bearer: &SecretBearer,
        kind: GrantKind,
        peer_uid: u32,
        session_id: Option<SessionId>,
        now_ms: u64,
    ) -> Result<ConsumedGrant, GrantRefusal> {
        let digest = digest_of(bearer);
        let mut found: Option<GrantId> = None;
        for record in self.grants.values() {
            let matches = bool::from(record.digest.ct_eq(&digest));
            if matches {
                found = Some(record.grant_id);
            }
        }
        let Some(grant_id) = found else {
            return Err(GrantRefusal::Unusable);
        };
        let record = self.grants.get_mut(&grant_id).expect("just found");
        if record.kind != kind
            || record.peer_uid != peer_uid
            || record.session_id != session_id
            || !record.is_active(now_ms)
        {
            return Err(GrantRefusal::Unusable);
        }
        record.consumed = true;
        record.bearer = None;
        Ok(ConsumedGrant {
            grant_id,
            kind,
            session_id,
        })
    }

    /// Revoke a grant by its non-secret id when it has `kind`.
    ///
    /// `daemon.host_grant` revokes host consent and `session.grant.revoke`
    /// revokes observer grants; a wrong-kind id is `NotActive`, never a
    /// cross-kind revoke.
    pub fn revoke_kind(
        &mut self,
        grant_id: GrantId,
        kind: GrantKind,
        now_ms: u64,
    ) -> RevokeOutcome {
        let Some(record) = self.grants.get_mut(&grant_id) else {
            return RevokeOutcome::NotActive;
        };
        if record.kind != kind || !record.is_active(now_ms) {
            return RevokeOutcome::NotActive;
        }
        record.revoked = true;
        record.bearer = None;
        RevokeOutcome::Revoked
    }

    /// The bearer of an unconsumed, unexpired, unrevoked grant, for a keyed mint
    /// replay (`D-18`).
    ///
    /// The bearer is returned only while the original response can still be
    /// replayed; once the grant is consumed, expired, or revoked, there is no
    /// bearer to replay.
    #[must_use]
    pub fn replayable_bearer(
        &self,
        grant_id: GrantId,
        kind: GrantKind,
        now_ms: u64,
    ) -> Option<SecretBearer> {
        let record = self.grants.get(&grant_id)?;
        if record.kind != kind || !record.is_active(now_ms) {
            return None;
        }
        record.bearer.clone()
    }

    /// Drop records that can no longer be used or replayed.
    pub fn evict_expired(&mut self, now_ms: u64) -> usize {
        let before = self.grants.len();
        self.grants.retain(|_, record| record.is_active(now_ms));
        before - self.grants.len()
    }
}

/// A mint failure: either a typed capacity refusal or an internal error.
#[derive(Debug)]
pub enum MintError {
    /// The pending cap is reached; a whole refusal.
    Refused(GrantRefusal),
    /// An internal failure (entropy, id, or record size).
    Error(GrantError),
}

impl fmt::Display for MintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::Error(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MintError {}

fn mint_bearer() -> Result<SecretBearer, GrantError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| GrantError::EntropyUnavailable)?;
    Ok(SecretBearer::from_bytes(bytes))
}

/// The 64-bit result ref derived from an id's first eight bytes.
#[must_use]
pub fn ref_of(bytes: &[u8; 16]) -> u64 {
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(prefix)
}

fn digest_of(bearer: &SecretBearer) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(GRANT_DIGEST_DOMAIN);
    hasher.update(bearer.expose());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::LimitOverride;

    fn store() -> GrantStore {
        GrantStore::new(&Limits::default())
    }

    fn session() -> SessionId {
        SessionId::parse(&"1".repeat(32)).expect("session id")
    }

    #[test]
    fn a_host_grant_is_single_use_peer_bound_and_ttl_bound() {
        let mut store = store();
        let minted = store.mint_host(1000, 10).expect("mint");
        assert_eq!(minted.expires_at_ms, 10 + 300_000);
        assert!(store.host_grant_active(minted.grant_id, 9));

        let consumed = store
            .consume(&minted.bearer, GrantKind::Host, 1000, None, 11)
            .expect("consume");
        assert_eq!(consumed.grant_id, minted.grant_id);
        assert!(!store.host_grant_active(minted.grant_id, 12));
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Host, 1000, None, 12)
                .expect_err("consumed"),
            GrantRefusal::Unusable
        );
    }

    #[test]
    fn every_unusable_cause_is_one_refusal() {
        let mut store = store();
        let minted = store.mint_host(1000, 10).expect("mint");

        // Wrong peer.
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Host, 1001, None, 11)
                .expect_err("wrong peer"),
            GrantRefusal::Unusable
        );
        // Wrong kind.
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Observer, 1000, None, 11)
                .expect_err("wrong kind"),
            GrantRefusal::Unusable
        );
        // Expired.
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Host, 1000, None, 300_010)
                .expect_err("expired"),
            GrantRefusal::Unusable
        );
        // Revoked.
        assert_eq!(
            store.revoke_kind(minted.grant_id, GrantKind::Host, 11),
            RevokeOutcome::Revoked
        );
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Host, 1000, None, 12)
                .expect_err("revoked"),
            GrantRefusal::Unusable
        );
        // Missing.
        let other = SecretBearer::from_bytes([1u8; 32]);
        assert_eq!(
            store
                .consume(&other, GrantKind::Host, 1000, None, 12)
                .expect_err("missing"),
            GrantRefusal::Unusable
        );
    }

    #[test]
    fn an_observer_grant_is_bound_to_one_session() {
        let mut store = store();
        let first = session();
        let second = SessionId::parse(&"3".repeat(32)).expect("session id");
        let minted = store.mint_observer(first, 1000, 10).expect("mint");
        assert_eq!(
            store
                .consume(&minted.bearer, GrantKind::Observer, 1000, Some(second), 11)
                .expect_err("wrong session"),
            GrantRefusal::Unusable
        );
        let consumed = store
            .consume(&minted.bearer, GrantKind::Observer, 1000, Some(first), 11)
            .expect("consume");
        assert_eq!(consumed.session_id, Some(first));
    }

    #[test]
    fn pending_caps_are_whole_refusals_and_never_evict() {
        let limits = Limits::new(&[
            LimitOverride::new(LimitId::MaxPendingHostGrants, 2),
            LimitOverride::new(LimitId::MaxPendingObserverGrantsPerSession, 1),
        ])
        .expect("valid overrides");
        let mut store = GrantStore::new(&limits);
        store.mint_host(1000, 10).expect("first");
        store.mint_host(1000, 11).expect("second");
        assert!(matches!(
            store.mint_host(1000, 12),
            Err(MintError::Refused(GrantRefusal::Capacity {
                kind: GrantKind::Host,
                limit: 2
            }))
        ));
        assert_eq!(store.len(), 2, "a refusal never evicts a live grant");

        let session = session();
        store.mint_observer(session, 1000, 10).expect("first");
        assert!(matches!(
            store.mint_observer(session, 1000, 11),
            Err(MintError::Refused(GrantRefusal::Capacity {
                kind: GrantKind::Observer,
                limit: 1
            }))
        ));
    }

    #[test]
    fn revoke_is_idempotent_and_reports_not_active() {
        let mut store = store();
        let minted = store.mint_host(1000, 10).expect("mint");
        assert_eq!(
            store.revoke_kind(minted.grant_id, GrantKind::Host, 11),
            RevokeOutcome::Revoked
        );
        assert_eq!(
            store.revoke_kind(minted.grant_id, GrantKind::Host, 12),
            RevokeOutcome::NotActive
        );
        assert_eq!(
            store.revoke_kind(GrantId::generate().expect("entropy"), GrantKind::Host, 12),
            RevokeOutcome::NotActive
        );
    }

    #[test]
    fn a_keyed_mint_replay_returns_the_same_bearer_until_consumed() {
        let mut store = store();
        let minted = store.mint_observer(session(), 1000, 10).expect("mint");
        let replay = store
            .replayable_bearer(minted.grant_id, GrantKind::Observer, 11)
            .expect("replay");
        assert_eq!(replay, minted.bearer);
        // A wrong kind never yields a bearer.
        assert!(
            store
                .replayable_bearer(minted.grant_id, GrantKind::Host, 11)
                .is_none()
        );
        let session_id = store
            .grants
            .get(&minted.grant_id)
            .and_then(|record| record.session_id)
            .expect("session");
        store
            .consume(
                &minted.bearer,
                GrantKind::Observer,
                1000,
                Some(session_id),
                11,
            )
            .expect("consume");
        assert!(
            store
                .replayable_bearer(minted.grant_id, GrantKind::Observer, 12)
                .is_none(),
            "a consumed grant has no replayable bearer"
        );
    }

    #[test]
    fn eviction_drops_retired_records() {
        let mut store = store();
        let minted = store.mint_host(1000, 10).expect("mint");
        store.revoke_kind(minted.grant_id, GrantKind::Host, 11);
        assert_eq!(store.evict_expired(12), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn no_debug_output_contains_a_bearer() {
        let mut store = store();
        let minted = store.mint_host(1000, 10).expect("mint");
        let debug = format!("{store:?}");
        assert!(!debug.contains(&hex(minted.bearer.expose())));
        let minted_debug = format!("{minted:?}");
        assert!(minted_debug.contains("<redacted>"));
        assert!(!minted_debug.contains(&hex(minted.bearer.expose())));
    }

    #[test]
    fn grant_ids_are_canonical() {
        let id = GrantId::generate().expect("entropy");
        assert_eq!(id.as_hex().len(), 32);
        assert_eq!(GrantId::parse(&id.as_hex()).expect("round trip"), id);
        assert_eq!(
            GrantId::parse("not-a-grant").expect_err("invalid"),
            GrantError::InvalidGrantId
        );
        assert_eq!(id.to_string(), id.as_hex());
    }
}
