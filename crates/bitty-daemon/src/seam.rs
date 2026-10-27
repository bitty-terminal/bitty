//! The explicit not-a-listener seam.
//!
//! `bittyd` without arguments prints [`SeamStatus`]. The status is deliberately
//! a value, not a set of `println!` calls in the binary: the IPC service task
//! (bitty#2607) replaces the [`IpcServiceState::NotWired`] branch and the
//! [`EndpointState::Unbound`] branch with the real `bitty-ipc` v1 server loop
//! and endpoint binding, while the binary and its output stay unchanged.
//!
//! This module performs no I/O, resolves no endpoint path, reads no
//! environment variable, and creates no socket file (contract "Endpoint
//! discovery"; the path rule is owned by bitty#2604).

use std::fmt::Write as _;

use crate::CRATE_VERSION;
use crate::contract::{CAPABILITIES, Capabilities, SERVICE_API_VERSION, ServiceApiVersion};

/// Whether the IPC service loop is installed behind this seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcServiceState {
    /// The `bitty-ipc` v1 server loop has not been wired (bitty#2607).
    NotWired,
}

impl IpcServiceState {
    /// Whether a server loop accepts connections.
    #[must_use]
    pub const fn is_wired(self) -> bool {
        match self {
            Self::NotWired => false,
        }
    }
}

impl std::fmt::Display for IpcServiceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWired => formatter.write_str("not wired (bitty#2607)"),
        }
    }
}

/// Whether the daemon has bound its local endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    /// No endpoint exists; no listener and no socket file.
    Unbound,
}

impl EndpointState {
    /// Whether an endpoint is bound.
    #[must_use]
    pub const fn is_bound(self) -> bool {
        match self {
            Self::Unbound => false,
        }
    }
}

impl std::fmt::Display for EndpointState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound => formatter.write_str("none bound (no socket file created)"),
        }
    }
}

/// Status of the session-host seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeamStatus {
    /// Service version this build would negotiate.
    pub service_api_version: ServiceApiVersion,
    /// Capabilities this build would advertise.
    pub capabilities: Capabilities,
    /// Whether the IPC service loop is installed.
    pub ipc_service: IpcServiceState,
    /// Whether a local endpoint is bound.
    pub endpoint: EndpointState,
}

impl SeamStatus {
    /// The status of this build: the contract version and capability set with
    /// no listener and no endpoint.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            service_api_version: SERVICE_API_VERSION,
            capabilities: CAPABILITIES,
            ipc_service: IpcServiceState::NotWired,
            endpoint: EndpointState::Unbound,
        }
    }

    /// Whether any endpoint is bound.
    #[must_use]
    pub const fn endpoint_bound(&self) -> bool {
        self.endpoint.is_bound()
    }

    /// Render the status as the `bittyd` no-argument report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{binary} {version}: session-host seam",
            binary = crate::contract::BINARY_NAME,
            version = CRATE_VERSION
        );
        let _ = writeln!(
            out,
            "service API: {service} (accepted client range {range})",
            service = self.service_api_version,
            range = self.service_api_version.client_range()
        );
        let enabled = self
            .capabilities
            .enabled_names()
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "capabilities: {enabled}");
        let _ = writeln!(out, "worker sessions: unsupported (contract D-01)");
        let _ = writeln!(out, "ipc service: {}", self.ipc_service);
        let _ = writeln!(out, "endpoint: {}", self.endpoint);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seam_has_no_listener_and_no_endpoint() {
        let status = SeamStatus::current();
        assert_eq!(status.ipc_service, IpcServiceState::NotWired);
        assert!(!status.ipc_service.is_wired());
        assert_eq!(status.endpoint, EndpointState::Unbound);
        assert!(!status.endpoint.is_bound());
        assert!(!status.endpoint_bound());
    }

    #[test]
    fn rendered_status_names_the_seam_explicitly() {
        let rendered = SeamStatus::current().render();
        assert!(rendered.contains("session-host seam"));
        assert!(rendered.contains("service API: 1.0 (accepted client range 1.0..=1.0)"));
        assert!(rendered.contains("terminal_sessions"));
        assert!(rendered.contains("worker sessions: unsupported"));
        assert!(rendered.contains("ipc service: not wired"));
        assert!(rendered.contains("endpoint: none bound"));
    }
}
