//! The `daemon.negotiate` handshake (`D-07`, `D-08`, `D-11`).
//!
//! The request carries the client version and up to three optional values:
//!
//! - `client_correlation`: a consumer value bounded to 128 bytes and valid
//!   UTF-8 with no control characters. The daemon hashes it on receipt and
//!   records only a truncated 128-bit SHA-256 digest as
//!   `client_correlation_digest`; the raw value is never stored, logged, or
//!   echoed, and a malformed value is refused with `InvalidRequest` rather
//!   than normalized (`D-08`). The digest is audit metadata and, when a grant
//!   mint names it, the binding that makes the grant usable only from the
//!   connection presenting the same correlation (see
//!   [`GrantCorrelation`]).
//! - `operator_capability`: the per-boot bearer that requests
//!   `daemon.inspect` and `daemon.control`. A present but invalid bearer is
//!   one static `Unauthenticated` refusal that closes the connection; the
//!   response never reveals whether the capability file exists.
//! - `host_grant`: a single-use, peer-bound, TTL-bounded host-consent bearer
//!   minted by `daemon.host_grant`. Consuming it adds `session.host` to this
//!   connection; a missing, expired, wrong-peer, consumed, or revoked grant
//!   adds nothing and leaves `session.create` to fail with
//!   `Denied/ConsentRequired`.
//!
//! The response carries every frozen handshake field: `wire_version` (an echo
//! of the accepted transport's version, never a second negotiation),
//! `service_api_version`, `daemon_instance_id`, `daemon_pid`, `server_identity`,
//! `peer_identity`, `client_id`, `scopes`, `capabilities`, `limits`,
//! `server_monotonic_ms`, and `daemon_state`. It is sent once and is immutable
//! for the connection; it carries no session data, no terminal content, and no
//! secret beyond the already-consumed grant's effect on `scopes`.
//!
//! The wire version is not a negotiate field: every frame carries `v`, and the
//! transport refuses an unsupported value before the service sees the request.

use std::fmt;

use bitty_ipc_api::scope::ScopeSet;
use bitty_ipc_api::wire::WIRE_VERSION;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::contract::{
    CAPABILITIES, Capabilities, ClientVersion, SERVICE_API_VERSION, ServiceApiVersion,
};
use crate::limits::Limits;
use crate::registry::DaemonInstanceId;
use crate::service::ServiceScopeSet;

/// Maximum bytes of a `client_correlation` value (`D-08`).
pub const MAX_CLIENT_CORRELATION_BYTES: usize = 128;

/// Bytes in a 256-bit bearer encoded as lowercase hex.
pub const BEARER_HEX_BYTES: usize = 64;

/// Domain separator for the `client_correlation_digest` hash.
const CORRELATION_DOMAIN: &[u8] = b"bittyd:client_correlation:v1\0";

/// Characters of a grant correlation digest accepted on the wire.
///
/// The accepted digest is the 128-bit `client_correlation_digest` encoded as
/// exactly 32 lowercase hex characters. No other width is accepted: a wider or
/// shorter value could never equal a connection's 128-bit digest, so accepting
/// it would mint a grant that is dead on arrival.
pub const GRANT_CORRELATION_HEX: usize = 32;

/// The accepted transport wire version, echoed in the handshake response.
///
/// The value is owned by `bitty-ipc`; this re-export keeps the handshake from
/// declaring a second wire-version constant.
pub const ACCEPTED_WIRE_VERSION: u16 = WIRE_VERSION;

/// An audit-only consumer correlation value, hashed on receipt (`D-08`).
///
/// The raw value is consumed by [`Self::parse`] and never stored: the value
/// keeps only the truncated 128-bit SHA-256 digest that audit records.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCorrelation {
    digest: [u8; 16],
}

impl ClientCorrelation {
    /// Validate and hash a correlation value.
    ///
    /// # Errors
    ///
    /// Returns [`CorrelationError::TooLong`] above 128 bytes and
    /// [`CorrelationError::ControlCharacter`] for `0x00..=0x1F` and `0x7F`.
    /// The raw value is never echoed in the error.
    pub fn parse(value: &str) -> Result<Self, CorrelationError> {
        if value.len() > MAX_CLIENT_CORRELATION_BYTES {
            return Err(CorrelationError::TooLong {
                bytes: value.len(),
                limit: MAX_CLIENT_CORRELATION_BYTES,
            });
        }
        if value.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
            return Err(CorrelationError::ControlCharacter);
        }
        let mut hasher = Sha256::new();
        hasher.update(CORRELATION_DOMAIN);
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        let mut truncated = [0u8; 16];
        truncated.copy_from_slice(&digest[..16]);
        Ok(Self { digest: truncated })
    }

    /// The truncated 128-bit SHA-256 digest recorded as
    /// `client_correlation_digest`.
    #[must_use]
    pub const fn digest(&self) -> [u8; 16] {
        self.digest
    }
}

impl fmt::Debug for ClientCorrelation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ClientCorrelation")
            .field(&format_args!("digest={}", hex(&self.digest)))
            .finish()
    }
}

/// A malformed `client_correlation` value.
///
/// No variant carries the raw value: the error is safe to render in a static
/// refusal message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationError {
    /// The value exceeds the 128-byte bound.
    TooLong {
        /// Observed bytes.
        bytes: usize,
        /// The enforced bound.
        limit: usize,
    },
    /// The value contains a control character.
    ControlCharacter,
    /// A grant correlation digest is not exactly 32 lowercase hex characters.
    Malformed,
}

impl fmt::Display for CorrelationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes, limit } => {
                write!(
                    formatter,
                    "correlation is {bytes} bytes, above the {limit}-byte bound"
                )
            }
            Self::ControlCharacter => {
                formatter.write_str("correlation contains a control character")
            }
            Self::Malformed => {
                formatter.write_str("correlation digest is not 32 lowercase hexadecimal characters")
            }
        }
    }
}

impl std::error::Error for CorrelationError {}

/// A 128-bit binding between a grant and the consuming connection's
/// `client_correlation` digest.
///
/// The wire value is the same 32-hex `client_correlation_digest` the handshake
/// records, so a mint can name exactly the digest a later connection presents.
/// The binding is not secret and never carries the raw correlation value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GrantCorrelation {
    binding: [u8; 16],
}

impl GrantCorrelation {
    /// Parse exactly [`GRANT_CORRELATION_HEX`] lowercase hex characters into a
    /// grant binding.
    ///
    /// # Errors
    ///
    /// [`CorrelationError::Malformed`] for any other width or a non-lowercase
    /// hex character.
    pub fn parse(value: &str) -> Result<Self, CorrelationError> {
        let raw = value.as_bytes();
        if raw.len() != GRANT_CORRELATION_HEX {
            return Err(CorrelationError::Malformed);
        }
        let mut binding = [0u8; 16];
        for (index, chunk) in raw.chunks_exact(2).enumerate() {
            let high = hex_nibble(chunk[0]).ok_or(CorrelationError::Malformed)?;
            let low = hex_nibble(chunk[1]).ok_or(CorrelationError::Malformed)?;
            binding[index] = (high << 4) | low;
        }
        Ok(Self { binding })
    }

    /// The binding of a connection whose `client_correlation_digest` is known.
    #[must_use]
    pub const fn from_digest(digest: [u8; 16]) -> Self {
        Self { binding: digest }
    }

    /// The 128-bit binding.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.binding
    }
}

impl fmt::Debug for GrantCorrelation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("GrantCorrelation")
            .field(&format_args!("binding={}", hex(&self.binding)))
            .finish()
    }
}

/// A 256-bit bearer value presented on the wire and redacted everywhere else.
///
/// The operator capability and the host grant share the frozen encoding: 64
/// lowercase hex characters. The value has a redacted [`fmt::Debug`], is not
/// [`Copy`], and is zeroized on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBearer {
    bytes: Zeroizing<[u8; 32]>,
}

impl SecretBearer {
    /// Parse exactly 64 lowercase hex characters.
    ///
    /// # Errors
    ///
    /// Returns [`BearerError::WrongLength`] or [`BearerError::NotLowercaseHex`].
    /// Neither error echoes the value.
    pub fn parse(value: &str) -> Result<Self, BearerError> {
        if value.len() != BEARER_HEX_BYTES {
            return Err(BearerError::WrongLength {
                bytes: value.len(),
                expected: BEARER_HEX_BYTES,
            });
        }
        let mut bytes = Zeroizing::new([0u8; 32]);
        let raw = value.as_bytes();
        for (index, chunk) in raw.chunks_exact(2).enumerate() {
            let high = hex_nibble(chunk[0]).ok_or(BearerError::NotLowercaseHex)?;
            let low = hex_nibble(chunk[1]).ok_or(BearerError::NotLowercaseHex)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self { bytes })
    }

    /// Construct from raw bytes (server-side minting and tests).
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    /// The raw bytes. Only the verifier reads them.
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        &self.bytes
    }
}

impl fmt::Debug for SecretBearer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBearer(<redacted>)")
    }
}

/// A malformed bearer value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerError {
    /// The encoding is not exactly 64 characters.
    WrongLength {
        /// Observed characters.
        bytes: usize,
        /// The required count.
        expected: usize,
    },
    /// The value contains a character outside `0-9a-f`.
    NotLowercaseHex,
}

impl fmt::Display for BearerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { bytes, expected } => {
                write!(
                    formatter,
                    "bearer is {bytes} characters, expected {expected}"
                )
            }
            Self::NotLowercaseHex => formatter.write_str("bearer is not lowercase hex"),
        }
    }
}

impl std::error::Error for BearerError {}

/// Request for [`crate::contract::NEGOTIATE_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiateRequest {
    /// The client's contract version.
    pub client_version: ClientVersion,
    /// Optional audit-only correlation value, already hashed.
    pub client_correlation: Option<ClientCorrelation>,
    /// Optional operator capability presented for `daemon.*` scopes.
    pub operator_capability: Option<SecretBearer>,
    /// Optional single-use host-consent grant.
    pub host_grant: Option<SecretBearer>,
    /// Whether the client asked for pushed session events on this connection.
    ///
    /// Absent is `false`: a `1.0` client never receives a frame it did not
    /// request, so the additive field preserves the `1.0` behavior. A request
    /// on a cell that does not serve delivery is refused typed at negotiate.
    pub events: bool,
}

impl NegotiateRequest {
    /// Construct a request with no optional fields.
    #[must_use]
    pub fn new(client_version: ClientVersion) -> Self {
        Self {
            client_version,
            client_correlation: None,
            operator_capability: None,
            host_grant: None,
            events: false,
        }
    }

    /// Attach an already-validated correlation value.
    #[must_use]
    pub fn with_client_correlation(mut self, correlation: ClientCorrelation) -> Self {
        self.client_correlation = Some(correlation);
        self
    }

    /// Present an operator capability.
    #[must_use]
    pub fn with_operator_capability(mut self, capability: SecretBearer) -> Self {
        self.operator_capability = Some(capability);
        self
    }

    /// Present a host-consent grant.
    #[must_use]
    pub fn with_host_grant(mut self, grant: SecretBearer) -> Self {
        self.host_grant = Some(grant);
        self
    }

    /// Ask for pushed session events on this connection.
    #[must_use]
    pub const fn with_events(mut self, events: bool) -> Self {
        self.events = events;
        self
    }
}

/// A server-assigned 128-bit connection identity (`D-08`).
///
/// It is minted per connection, never supplied by the client, and never
/// durable. It is not a secret: audit records it, and no authority derives
/// from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId([u8; 16]);

impl ClientId {
    /// Mint a fresh id from the operating-system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`ClientIdError::EntropyUnavailable`] when the CSPRNG fails.
    /// There is no fallback id.
    pub fn generate() -> Result<Self, ClientIdError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| ClientIdError::EntropyUnavailable)?;
        Ok(Self(bytes))
    }

    /// Parse 32 lowercase hex characters (echo and test paths only).
    ///
    /// # Errors
    ///
    /// Returns [`ClientIdError::Invalid`] for any other form.
    pub fn parse(value: &str) -> Result<Self, ClientIdError> {
        if value.len() != 32 {
            return Err(ClientIdError::Invalid);
        }
        let mut bytes = [0u8; 16];
        let raw = value.as_bytes();
        for (index, chunk) in raw.chunks_exact(2).enumerate() {
            let high = hex_nibble(chunk[0]).ok_or(ClientIdError::Invalid)?;
            let low = hex_nibble(chunk[1]).ok_or(ClientIdError::Invalid)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// The raw 128 bits.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The audit projection: the existing [`crate::audit::AuditActor`] field is
    /// a `u64`, so the first 64 bits of the connection id are used. This is a
    /// truncation of the same value, not a second identity.
    #[must_use]
    pub fn audit_ref(&self) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.0[..8]);
        u64::from_be_bytes(bytes)
    }

    /// The canonical 32 lowercase hex spelling.
    #[must_use]
    pub fn as_hex(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

/// A refused client-id operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIdError {
    /// The operating-system CSPRNG failed.
    EntropyUnavailable,
    /// The value is not 32 lowercase hex characters.
    Invalid,
}

impl fmt::Display for ClientIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => formatter.write_str("operating system entropy unavailable"),
            Self::Invalid => formatter.write_str("client id is not 32 lowercase hex characters"),
        }
    }
}

impl std::error::Error for ClientIdError {}

/// The daemon endpoint kind reported in `server_identity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// A `0600` Unix-domain socket inside the `0700` runtime directory.
    UnixSocket,
    /// A current-user-only named pipe (not served in this slice).
    NamedPipe,
}

impl EndpointKind {
    /// The frozen spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnixSocket => "unix_socket",
            Self::NamedPipe => "named_pipe",
        }
    }
}

impl fmt::Display for EndpointKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Observational server endpoint facts (`server_identity`).
///
/// This is tamper detection for the client, not server authentication: a
/// same-uid impostor can bind the path first. The contract requires it to be
/// labelled observational, never proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerIdentity {
    /// The daemon's OS user id.
    pub uid: u32,
    /// The endpoint kind.
    pub endpoint_kind: EndpointKind,
    /// The endpoint object's owner uid.
    pub endpoint_owner: u32,
    /// The endpoint object's permission bits.
    pub endpoint_mode: u32,
}

/// The authenticated peer facts captured before any frame was parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentityFacts {
    /// Unix uid, or the Windows SID projection where a cell provides one.
    pub uid: u32,
    /// Peer process id; observational because a pid can be reused.
    pub pid: u32,
    /// Peer process start time where the platform exposes one.
    pub start_time: Option<u64>,
}

impl PeerIdentityFacts {
    /// Build the handshake projection of a captured peer.
    #[must_use]
    pub const fn from_peer(identity: &crate::peer::PeerIdentity) -> Self {
        Self {
            uid: identity.uid(),
            pid: identity.pid(),
            start_time: identity.start_time(),
        }
    }
}

/// The daemon lifecycle state reported at handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// Accepting connections and new sessions.
    Running,
    /// Draining; new sessions are refused.
    Stopping,
}

impl DaemonState {
    /// The frozen spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
        }
    }
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The effective scope set for one connection.
///
/// The service half is the bittyd-owned `session.*` / `daemon.*` family; the
/// transport half is the unchanged `bitty-ipc` v1 vocabulary. Keeping both in
/// one value is what lets the `scopes` handshake field report the real
/// authority without stretching either family.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveScopes {
    /// Service scopes (`session.*`, `daemon.*`).
    pub service: ServiceScopeSet,
    /// `bitty-ipc` v1 transport scopes, unchanged.
    pub transport: ScopeSet,
}

impl EffectiveScopes {
    /// An empty, deny-by-default set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the service set holds `scope`.
    #[must_use]
    pub const fn has_service(&self, scope: crate::service::ServiceScope) -> bool {
        self.service.contains(scope)
    }

    /// Whether the transport set holds `scope`.
    #[must_use]
    pub fn has_transport(&self, scope: bitty_ipc_api::scope::Scope) -> bool {
        self.transport.contains(scope)
    }

    /// Every held scope name, service names first, each family in its own
    /// deterministic order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.service.names().collect();
        names.extend(self.transport.iter().map(|scope| scope.as_str()));
        names
    }

    /// The number of held scopes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.service.len() + self.transport.len()
    }

    /// Whether no scope is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The complete, immutable handshake response for one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResponse {
    /// Accepted wire version (an echo, not a negotiation).
    pub wire_version: u16,
    /// Server service version and accepted client range.
    pub service_api_version: ServiceApiVersion,
    /// The fresh daemon instance id.
    pub daemon_instance_id: DaemonInstanceId,
    /// Observational daemon pid; never a signal target or authority input.
    pub daemon_pid: u32,
    /// Observational server endpoint facts.
    pub server_identity: ServerIdentity,
    /// The authenticated peer captured before parsing.
    pub peer_identity: PeerIdentityFacts,
    /// The server-assigned connection identity.
    pub client_id: ClientId,
    /// Effective scopes for this connection.
    pub scopes: EffectiveScopes,
    /// Capabilities fixed for the connection.
    pub capabilities: Capabilities,
    /// Every effective bound and its compiled default and ceiling (`D-14`).
    pub limits: Limits,
    /// The server monotonic clock at handshake.
    pub server_monotonic_ms: u64,
    /// Whether the daemon is accepting new sessions.
    pub daemon_state: DaemonState,
}

/// The server-side facts needed to build a [`HandshakeResponse`].
#[derive(Debug, Clone)]
pub struct HandshakeContext {
    /// The daemon instance id.
    pub daemon_instance_id: DaemonInstanceId,
    /// The daemon pid.
    pub daemon_pid: u32,
    /// Observational server endpoint facts.
    pub server_identity: ServerIdentity,
    /// The authenticated peer.
    pub peer_identity: PeerIdentityFacts,
    /// The server-assigned connection identity.
    pub client_id: ClientId,
    /// Effective scopes, already including any consumed grant and operator
    /// capability effect.
    pub scopes: EffectiveScopes,
    /// The capabilities this probed server process advertises.
    ///
    /// A build's [`crate::contract::CAPABILITIES`] are the contract target
    /// set; the served daemon narrows `worker_sessions` to the containment
    /// plan its startup probe selected.
    pub capabilities: Capabilities,
    /// Effective limits for this run.
    pub limits: Limits,
    /// The server monotonic clock at handshake.
    pub server_monotonic_ms: u64,
    /// The daemon lifecycle state.
    pub daemon_state: DaemonState,
}

impl HandshakeResponse {
    /// Build a response from the server-side facts.
    #[must_use]
    pub fn new(context: HandshakeContext) -> Self {
        Self {
            wire_version: ACCEPTED_WIRE_VERSION,
            service_api_version: SERVICE_API_VERSION,
            daemon_instance_id: context.daemon_instance_id,
            daemon_pid: context.daemon_pid,
            server_identity: context.server_identity,
            peer_identity: context.peer_identity,
            client_id: context.client_id,
            scopes: context.scopes,
            capabilities: context.capabilities,
            limits: context.limits,
            server_monotonic_ms: context.server_monotonic_ms,
            daemon_state: context.daemon_state,
        }
    }
}

/// Accepted handshake facts that belong to the contract seam.
///
/// `limits` carries the effective `{effective, default, system_max}` triples for
/// this daemon run; a lowered configuration is advertised here, never replaced
/// by a hardcoded default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiateResponse {
    /// Server service version and accepted client range.
    pub service_api_version: ServiceApiVersion,
    /// Capabilities fixed for the connection.
    ///
    /// This is the compiled build set; the served daemon narrows
    /// `worker_sessions` to its startup containment plan when it builds the
    /// [`HandshakeResponse`].
    pub capabilities: Capabilities,
    /// Every effective bound and its compiled default and ceiling (`D-14`).
    pub limits: Limits,
}

/// A refused negotiation (`D-11`: `VersionMismatch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiateError {
    /// The client major differs, or its minor is outside the accepted range.
    VersionMismatch {
        /// The version the client sent.
        client: ClientVersion,
        /// The server version and accepted range.
        server: ServiceApiVersion,
    },
}

impl fmt::Display for NegotiateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionMismatch { client, server } => write!(
                formatter,
                "client {client} is not accepted by service API {server} (accepted client range {})",
                server.client_range()
            ),
        }
    }
}

impl std::error::Error for NegotiateError {}

/// Negotiate the client version against the compiled service version and
/// advertise the effective limits for this run.
///
/// # Errors
///
/// Returns [`NegotiateError::VersionMismatch`] when `D-11` rejects the client:
/// a different major, or a client minor outside
/// `[min_client_minor, server_minor]`. Nothing is applied on refusal.
pub fn negotiate(
    request: &NegotiateRequest,
    limits: &Limits,
) -> Result<NegotiateResponse, NegotiateError> {
    if SERVICE_API_VERSION.accepts(request.client_version) {
        Ok(NegotiateResponse {
            service_api_version: SERVICE_API_VERSION,
            capabilities: CAPABILITIES,
            limits: limits.clone(),
        })
    } else {
        Err(NegotiateError::VersionMismatch {
            client: request.client_version,
            server: SERVICE_API_VERSION,
        })
    }
}

/// Lowercase hex rendering of a byte slice.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::NEGOTIATE_METHOD;
    use crate::limits::{LimitId, LimitOverride};

    #[test]
    fn negotiate_accepts_the_first_accepted_contract() {
        let response = negotiate(
            &NegotiateRequest::new(ClientVersion::new(1, 0)),
            &Limits::default(),
        )
        .expect("1.0 is accepted");
        assert_eq!(response.service_api_version, SERVICE_API_VERSION);
        assert_eq!(response.capabilities, CAPABILITIES);
        assert_eq!(
            response.capabilities.worker_sessions,
            crate::worker::WORKER_SESSIONS_SERVED
        );
        assert_eq!(
            response.limits.effective(LimitId::MaxSessions),
            LimitId::MaxSessions.default_value()
        );
    }

    #[test]
    fn negotiate_advertises_the_effective_limits_not_compiled_defaults() {
        let limits =
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 4)]).expect("valid override");
        let response = negotiate(&NegotiateRequest::new(ClientVersion::new(1, 0)), &limits)
            .expect("1.0 is accepted");
        let advertised = response
            .limits
            .advertised()
            .find(|entry| entry.name == LimitId::MaxSessions.name())
            .expect("max_sessions is advertised");
        assert_eq!(advertised.value.effective(), 4);
        assert_eq!(advertised.value.default_value(), 16);
        assert_eq!(advertised.value.system_max(), 64);
    }

    #[test]
    fn negotiate_refuses_a_different_major() {
        let error = negotiate(
            &NegotiateRequest::new(ClientVersion::new(2, 0)),
            &Limits::default(),
        )
        .expect_err("major 2 is not served");
        assert_eq!(
            error,
            NegotiateError::VersionMismatch {
                client: ClientVersion::new(2, 0),
                server: SERVICE_API_VERSION,
            }
        );
        assert!(error.to_string().contains("1.0..=1.3"));
    }

    #[test]
    fn negotiate_refuses_a_minor_outside_the_accepted_range() {
        assert!(
            negotiate(
                &NegotiateRequest::new(ClientVersion::new(1, 4)),
                &Limits::default()
            )
            .is_err()
        );
        assert!(
            negotiate(
                &NegotiateRequest::new(ClientVersion::new(0, 0)),
                &Limits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn negotiate_method_matches_the_contract_grammar() {
        assert_eq!(NEGOTIATE_METHOD, "daemon.negotiate");
        assert!(NEGOTIATE_METHOD.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        }));
    }

    #[test]
    fn request_optional_fields_default_to_absent() {
        let request = NegotiateRequest::new(ClientVersion::new(1, 0));
        assert!(request.client_correlation.is_none());
        assert!(request.operator_capability.is_none());
        assert!(request.host_grant.is_none());
        let request = request
            .with_client_correlation(ClientCorrelation::parse("build-42").expect("valid"))
            .with_operator_capability(SecretBearer::from_bytes([7u8; 32]))
            .with_host_grant(SecretBearer::from_bytes([9u8; 32]));
        assert!(request.client_correlation.is_some());
        assert!(request.operator_capability.is_some());
        assert!(request.host_grant.is_some());
    }

    #[test]
    fn correlation_is_bounded_and_rejects_control_characters() {
        assert!(ClientCorrelation::parse(&"a".repeat(MAX_CLIENT_CORRELATION_BYTES)).is_ok());
        assert_eq!(
            ClientCorrelation::parse(&"a".repeat(MAX_CLIENT_CORRELATION_BYTES + 1))
                .expect_err("over the bound"),
            CorrelationError::TooLong {
                bytes: MAX_CLIENT_CORRELATION_BYTES + 1,
                limit: MAX_CLIENT_CORRELATION_BYTES,
            }
        );
        assert_eq!(
            ClientCorrelation::parse("bad\nvalue").expect_err("control character"),
            CorrelationError::ControlCharacter
        );
        assert_eq!(
            ClientCorrelation::parse("bad\u{7f}value").expect_err("DEL is a control character"),
            CorrelationError::ControlCharacter
        );
    }

    #[test]
    fn correlation_is_hashed_and_the_raw_value_never_survives() {
        let correlation = ClientCorrelation::parse("reconnect-hint-17").expect("valid");
        let debug = format!("{correlation:?}");
        assert!(!debug.contains("reconnect-hint-17"));
        assert!(debug.contains("digest="));
        assert_eq!(
            correlation.digest(),
            ClientCorrelation::parse("reconnect-hint-17")
                .expect("valid")
                .digest()
        );
        assert_ne!(
            correlation.digest(),
            ClientCorrelation::parse("reconnect-hint-18")
                .expect("valid")
                .digest()
        );
        assert_eq!(correlation.digest().len(), 16);
    }

    #[test]
    fn a_grant_correlation_digest_is_exactly_32_lowercase_hex() {
        let digest = ClientCorrelation::parse("observer-a").expect("valid");
        let hex = hex(&digest.digest());
        let parsed = GrantCorrelation::parse(&hex).expect("32 lowercase hex");
        assert_eq!(parsed, GrantCorrelation::from_digest(digest.digest()));
        assert_eq!(parsed.as_bytes(), &digest.digest());
        assert_ne!(
            parsed,
            GrantCorrelation::from_digest(
                ClientCorrelation::parse("observer-b")
                    .expect("valid")
                    .digest()
            )
        );

        // 64 hex characters (a 256-bit digest) can never equal a connection's
        // 128-bit digest, so it is refused instead of minting a dead grant.
        assert_eq!(
            GrantCorrelation::parse(&"ab".repeat(32)).expect_err("64 characters"),
            CorrelationError::Malformed
        );
        assert_eq!(
            GrantCorrelation::parse(&"a".repeat(GRANT_CORRELATION_HEX - 1))
                .expect_err("short digest"),
            CorrelationError::Malformed
        );
        assert_eq!(
            GrantCorrelation::parse(&"a".repeat(GRANT_CORRELATION_HEX + 1))
                .expect_err("long digest"),
            CorrelationError::Malformed
        );
        assert_eq!(
            GrantCorrelation::parse(&hex.to_uppercase()).expect_err("uppercase"),
            CorrelationError::Malformed
        );
        assert_eq!(
            GrantCorrelation::parse(&"zz".repeat(16)).expect_err("non-hex"),
            CorrelationError::Malformed
        );
        assert_eq!(
            GrantCorrelation::parse("").expect_err("empty"),
            CorrelationError::Malformed
        );
    }

    #[test]
    fn bearers_are_exactly_64_lowercase_hex_characters() {
        let hex = "0123456789abcdef".repeat(4);
        let bearer = SecretBearer::parse(&hex).expect("valid bearer");
        assert_eq!(bearer.expose()[0], 0x01);
        assert_eq!(bearer.expose()[31], 0xef);
        assert_eq!(format!("{bearer:?}"), "SecretBearer(<redacted>)");
        assert_eq!(
            SecretBearer::parse(&hex.to_uppercase()).expect_err("uppercase is refused"),
            BearerError::NotLowercaseHex
        );
        assert_eq!(
            SecretBearer::parse(&hex[..62]).expect_err("short"),
            BearerError::WrongLength {
                bytes: 62,
                expected: BEARER_HEX_BYTES,
            }
        );
        assert_eq!(
            SecretBearer::parse(&format!("{hex}g0")).expect_err("not hex"),
            BearerError::WrongLength {
                bytes: 66,
                expected: BEARER_HEX_BYTES,
            }
        );
        assert_eq!(
            SecretBearer::parse(&format!("{}g0", &hex[..62])).expect_err("not hex"),
            BearerError::NotLowercaseHex
        );
    }

    #[test]
    fn client_ids_are_canonical_hex_and_unique() {
        let first = ClientId::generate().expect("entropy");
        let second = ClientId::generate().expect("entropy");
        assert_ne!(first, second);
        assert_eq!(first.as_hex().len(), 32);
        assert_eq!(ClientId::parse(&first.as_hex()).expect("round trip"), first);
        assert_eq!(
            ClientId::parse("ABC").expect_err("invalid"),
            ClientIdError::Invalid
        );
        assert_eq!(
            ClientId::parse(&"A".repeat(32)).expect_err("uppercase"),
            ClientIdError::Invalid
        );
        assert_eq!(first.to_string(), first.as_hex());
    }

    #[test]
    fn client_id_audit_ref_is_a_truncation_not_a_second_identity() {
        let id = ClientId::parse("0000000000000001ffffffffffffffff").expect("valid");
        assert_eq!(id.audit_ref(), 1);
        assert_eq!(
            id.audit_ref().to_be_bytes(),
            [0, 0, 0, 0, 0, 0, 0, 1],
            "the audit ref is the first 64 bits of the same id"
        );
    }

    #[test]
    fn daemon_state_and_endpoint_kind_spellings_are_frozen() {
        assert_eq!(DaemonState::Running.as_str(), "running");
        assert_eq!(DaemonState::Stopping.as_str(), "stopping");
        assert_eq!(EndpointKind::UnixSocket.as_str(), "unix_socket");
        assert_eq!(EndpointKind::NamedPipe.as_str(), "named_pipe");
    }

    #[test]
    fn effective_scopes_report_both_families_deterministically() {
        let scopes = EffectiveScopes {
            service: ServiceScopeSet::baseline(),
            transport: ScopeSet::cli_default(),
        };
        let names = scopes.names();
        assert_eq!(
            &names[..3],
            &["session.inspect", "session.attach", "session.control"]
        );
        assert!(names.contains(&"terminal.inspect"));
        assert!(!names.contains(&"process.spawn"));
        assert_eq!(scopes.len(), 3 + ScopeSet::cli_default().len());
        assert!(!scopes.is_empty());
        assert!(EffectiveScopes::new().is_empty());
    }

    #[test]
    fn handshake_response_echoes_the_transport_version_and_freezes_capabilities() {
        let context = HandshakeContext {
            daemon_instance_id: DaemonInstanceId::generate().expect("entropy"),
            daemon_pid: 4242,
            server_identity: ServerIdentity {
                uid: 1000,
                endpoint_kind: EndpointKind::UnixSocket,
                endpoint_owner: 1000,
                endpoint_mode: 0o600,
            },
            peer_identity: PeerIdentityFacts {
                uid: 1000,
                pid: 4243,
                start_time: Some(99),
            },
            client_id: ClientId::generate().expect("entropy"),
            scopes: EffectiveScopes::new(),
            capabilities: CAPABILITIES,
            limits: Limits::default(),
            server_monotonic_ms: 7,
            daemon_state: DaemonState::Running,
        };
        let response = HandshakeResponse::new(context.clone());
        assert_eq!(response.wire_version, ACCEPTED_WIRE_VERSION);
        assert_eq!(response.wire_version, 1);
        assert_eq!(response.daemon_pid, 4242);
        assert_eq!(response.peer_identity.pid, 4243);
        assert_eq!(response.client_id, context.client_id);
        assert_eq!(response.capabilities, CAPABILITIES);
        assert_eq!(response.daemon_state, DaemonState::Running);
        assert_eq!(response.service_api_version, SERVICE_API_VERSION);
    }

    #[test]
    fn hex_helpers_round_trip() {
        let bytes = [0x00u8, 0x0f, 0x10, 0xff];
        assert_eq!(hex(&bytes), "000f10ff");
        assert_eq!(hex_nibble(b'0'), Some(0));
        assert_eq!(hex_nibble(b'9'), Some(9));
        assert_eq!(hex_nibble(b'a'), Some(10));
        assert_eq!(hex_nibble(b'f'), Some(15));
        assert_eq!(hex_nibble(b'A'), None);
        assert_eq!(hex_nibble(b'g'), None);
    }
}
