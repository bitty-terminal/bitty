//! The `daemon.negotiate` handshake skeleton.
//!
//! The request carries the client version; an accepted negotiation returns the
//! server `service_api_version`, the explicit capability set (`D-11`), and the
//! effective `limits` triples (`D-14`).
//!
//! No transport is implemented. The remaining response fields are added by
//! their owning slices:
//!
//! | Field | Owner |
//! | --- | --- |
//! | `wire_version` (transport-owned) | bitty#2607 |
//! | `daemon_instance_id` | bitty#2601 |
//! | `daemon_pid`, `server_identity`, `peer_identity`, `client_id`, `scopes`, `daemon_state` | bitty#2607 |
//! | `server_monotonic_ms` | bitty#2607 |
//!
//! The request's optional fields are also deferred to bitty#2607 so their
//! bounds and secret handling land with the code that reads them:
//!
//! - `client_correlation`: an audit-only consumer value bounded to 128 bytes
//!   and valid UTF-8 with no control characters. The daemon hashes it on
//!   receipt and records only a truncated 128-bit SHA-256 digest as
//!   `client_correlation_digest`; the raw value is never stored, logged,
//!   echoed, or evaluated for authorization, and a malformed value is refused
//!   with `InvalidRequest` rather than normalized (`D-08`).
//! - `operator_capability`: a secret, owner-private capability (`D-07`).
//! - `host_grant`: an optional single-use, peer-bound, TTL-bounded host-consent
//!   grant, consumed at most once by `daemon.negotiate` (`D-07`).
//!
//! A version mismatch is a typed refusal before any session operation; the
//! transport-level mapping to the `bitty-ipc` error taxonomy belongs to
//! bitty#2607.

use std::fmt;

use crate::contract::{
    CAPABILITIES, Capabilities, ClientVersion, SERVICE_API_VERSION, ServiceApiVersion,
};
use crate::limits::Limits;

/// Request for [`crate::contract::NEGOTIATE_METHOD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiateRequest {
    /// The client's contract version.
    pub client_version: ClientVersion,
}

impl NegotiateRequest {
    /// Construct a request from a client version.
    #[must_use]
    pub const fn new(client_version: ClientVersion) -> Self {
        Self { client_version }
    }
}

/// Accepted handshake facts that belong to the contract seam.
///
/// `limits` carries the effective `{effective, default, system_max}` triples for
/// this daemon run; a lowered configuration is advertised here, never replaced
/// by a hardcoded default. The remaining contract response fields are listed in
/// the module docs and arrive with their owning slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiateResponse {
    /// Server service version and accepted client range.
    pub service_api_version: ServiceApiVersion,
    /// Capabilities fixed for the connection.
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
        assert!(!response.capabilities.worker_sessions);
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
        assert!(error.to_string().contains("1.0..=1.0"));
    }

    #[test]
    fn negotiate_refuses_a_minor_outside_the_accepted_range() {
        assert!(
            negotiate(
                &NegotiateRequest::new(ClientVersion::new(1, 1)),
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
}
