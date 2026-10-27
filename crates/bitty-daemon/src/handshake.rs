//! The `daemon.negotiate` handshake skeleton.
//!
//! Only the version half of the handshake is frozen here: the request carries
//! the client version, and an accepted negotiation returns the server
//! `service_api_version` plus the explicit capability set (`D-11`).
//!
//! No transport is implemented. The full contract response fields are added by
//! their owning slices:
//!
//! | Field | Owner |
//! | --- | --- |
//! | `daemon_instance_id` | bitty#2601 |
//! | `daemon_pid`, `server_identity`, `peer_identity`, `client_id`, `scopes`, `daemon_state` | bitty#2607 |
//! | `limits` | bitty#2603 |
//! | `server_monotonic_ms` | bitty#2607 |
//!
//! The request's optional `client_instance` (audit-only, bounded, never
//! authority; `D-08`) and `operator_capability` (secret, owner-private; `D-07`)
//! are also deferred to bitty#2607 so their bounds and secret handling land
//! with the code that reads them.
//!
//! A version mismatch is a typed refusal before any session operation; the
//! transport-level mapping to the `bitty-ipc` error taxonomy belongs to
//! bitty#2607.

use std::fmt;

use crate::contract::{
    CAPABILITIES, Capabilities, ClientVersion, SERVICE_API_VERSION, ServiceApiVersion,
};

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
/// The remaining contract response fields are listed in the module docs and
/// arrive with their owning slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiateResponse {
    /// Server service version and accepted client range.
    pub service_api_version: ServiceApiVersion,
    /// Capabilities fixed for the connection.
    pub capabilities: Capabilities,
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

/// Negotiate the client version against the compiled service version.
///
/// # Errors
///
/// Returns [`NegotiateError::VersionMismatch`] when `D-11` rejects the client:
/// a different major, or a client minor outside
/// `[min_client_minor, server_minor]`. Nothing is applied on refusal.
pub fn negotiate(request: &NegotiateRequest) -> Result<NegotiateResponse, NegotiateError> {
    if SERVICE_API_VERSION.accepts(request.client_version) {
        Ok(NegotiateResponse {
            service_api_version: SERVICE_API_VERSION,
            capabilities: CAPABILITIES,
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

    #[test]
    fn negotiate_accepts_the_first_accepted_contract() {
        let response =
            negotiate(&NegotiateRequest::new(ClientVersion::new(1, 0))).expect("1.0 is accepted");
        assert_eq!(response.service_api_version, SERVICE_API_VERSION);
        assert_eq!(response.capabilities, CAPABILITIES);
        assert!(!response.capabilities.worker_sessions);
    }

    #[test]
    fn negotiate_refuses_a_different_major() {
        let error = negotiate(&NegotiateRequest::new(ClientVersion::new(2, 0)))
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
        assert!(negotiate(&NegotiateRequest::new(ClientVersion::new(1, 1))).is_err());
        assert!(negotiate(&NegotiateRequest::new(ClientVersion::new(0, 0))).is_err());
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
