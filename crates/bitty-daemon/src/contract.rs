//! Service-contract constants for the `bittyd` session host.
//!
//! Source of truth: the accepted [`bittyd` Session-Host Contract], decision
//! `D-11` ("service API versioning and capabilities") and the "Handshake"
//! section of the service surface. Only the values this seam can freeze today
//! are defined here; scopes (`D-07`), limits (`D-14`), endpoint discovery, and
//! the full handshake response belong to the slices named in the crate docs.
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

use std::fmt;

/// Name of the daemon binary.
pub const BINARY_NAME: &str = "bittyd";

/// Contract method name for the handshake. It follows the `bitty-ipc` method
/// grammar (lowercase dotted segments) and is part of the conformance surface.
pub const NEGOTIATE_METHOD: &str = "daemon.negotiate";

/// First accepted service-contract major version (`D-11`): the v0.2.0 slice.
pub const SERVICE_API_MAJOR: u16 = 1;

/// First accepted service-contract minor version (`D-11`).
pub const SERVICE_API_MINOR: u16 = 0;

/// Lowest client minor the server accepts for [`SERVICE_API_MAJOR`] (`D-11`:
/// `min_client_minor` starts at `0` for `1.0`).
pub const MIN_CLIENT_MINOR: u16 = 0;

/// Advertised `service_api_version` value from the handshake.
pub const SERVICE_API_VERSION: ServiceApiVersion =
    ServiceApiVersion::new(SERVICE_API_MAJOR, SERVICE_API_MINOR, MIN_CLIENT_MINOR);

/// Capability names in contract order (`D-11`).
pub const CAPABILITY_NAMES: [&str; 5] = [
    "terminal_sessions",
    "worker_sessions",
    "events",
    "control_transfer",
    "idempotent_mutations",
];

/// The explicit capability set the v0.2.0 surface advertises.
///
/// `worker_sessions` stays `false` in every v0.2.0 build (`D-01`): a worker
/// kind must be refused with `UnsupportedCapability`, and enabling it needs an
/// ADR 0019 amendment plus a dedicated security review. The other capabilities
/// are the contract target set; they become observable only when the IPC
/// service task (bitty#2607) serves the handshake.
pub const CAPABILITIES: Capabilities = Capabilities {
    terminal_sessions: true,
    worker_sessions: false,
    events: true,
    control_transfer: true,
    idempotent_mutations: true,
};

/// The `client_version {major, minor}` a client sends in `daemon.negotiate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientVersion {
    /// Client contract major version.
    pub major: u16,
    /// Client contract minor version.
    pub minor: u16,
}

impl ClientVersion {
    /// Construct a client version.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

impl fmt::Display for ClientVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// The `service_api_version {major, minor, min_client_minor}` the server
/// returns in `daemon.negotiate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceApiVersion {
    /// Server contract major version; one major is served at a time.
    pub major: u16,
    /// Server contract minor version; the highest accepted client minor.
    pub minor: u16,
    /// Lowest client minor accepted for [`Self::major`].
    pub min_client_minor: u16,
}

impl ServiceApiVersion {
    /// Construct a service version.
    ///
    /// # Panics
    ///
    /// Panics when `min_client_minor > minor`, which would describe an empty
    /// accepted range. The constants in this crate satisfy the invariant.
    #[must_use]
    pub const fn new(major: u16, minor: u16, min_client_minor: u16) -> Self {
        assert!(
            min_client_minor <= minor,
            "min_client_minor cannot exceed the server minor"
        );
        Self {
            major,
            minor,
            min_client_minor,
        }
    }

    /// The `D-11` acceptance rule: the same major, and a client minor within
    /// `[min_client_minor, server_minor]`.
    ///
    /// There is no best-effort mode, probe-based downgrade, or partial
    /// surface; a refused client gets `VersionMismatch` before any session
    /// operation.
    #[must_use]
    pub const fn accepts(self, client: ClientVersion) -> bool {
        client.major == self.major
            && client.minor >= self.min_client_minor
            && client.minor <= self.minor
    }

    /// The accepted client range, for diagnostics (`1.0..=1.0`).
    #[must_use]
    pub fn client_range(self) -> String {
        format!(
            "{}.{}..={}.{}",
            self.major, self.min_client_minor, self.major, self.minor
        )
    }
}

impl fmt::Display for ServiceApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Explicit, immutable capability set from `D-11`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Terminal sessions are hosted in v0.2.0.
    pub terminal_sessions: bool,
    /// Worker sessions are contract-defined but disabled in every v0.2.0
    /// build (`D-01`).
    pub worker_sessions: bool,
    /// Ordered, revision-tagged event delivery is available.
    pub events: bool,
    /// Explicit controller transfer is available.
    pub control_transfer: bool,
    /// Mutating methods accept idempotency keys.
    pub idempotent_mutations: bool,
}

impl Capabilities {
    /// Pair every contract capability name with its advertised value, in
    /// contract order.
    #[must_use]
    pub const fn all(self) -> [(&'static str, bool); 5] {
        [
            ("terminal_sessions", self.terminal_sessions),
            ("worker_sessions", self.worker_sessions),
            ("events", self.events),
            ("control_transfer", self.control_transfer),
            ("idempotent_mutations", self.idempotent_mutations),
        ]
    }

    /// Whether `name` is an enabled capability.
    ///
    /// Unknown names are not enabled; a forward-compatible client ignores
    /// them, and an unknown method is `InvalidMethod` rather than a no-op
    /// (`D-11`).
    #[must_use]
    pub fn is_enabled(self, name: &str) -> bool {
        self.all()
            .iter()
            .any(|(capability, enabled)| *capability == name && *enabled)
    }

    /// Enabled capability names in contract order.
    pub fn enabled_names(self) -> impl Iterator<Item = &'static str> {
        self.all()
            .into_iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(name, _)| name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_version_matches_the_accepted_contract() {
        assert_eq!(SERVICE_API_MAJOR, 1);
        assert_eq!(SERVICE_API_MINOR, 0);
        assert_eq!(MIN_CLIENT_MINOR, 0);
        assert_eq!(SERVICE_API_VERSION, ServiceApiVersion::new(1, 0, 0));
        assert_eq!(SERVICE_API_VERSION.to_string(), "1.0");
        assert_eq!(SERVICE_API_VERSION.client_range(), "1.0..=1.0");
    }

    #[test]
    fn acceptance_uses_same_major_and_the_minor_range() {
        let server = SERVICE_API_VERSION;
        assert!(server.accepts(ClientVersion::new(1, 0)));
        assert!(!server.accepts(ClientVersion::new(0, 0)));
        assert!(!server.accepts(ClientVersion::new(2, 0)));
        assert!(!server.accepts(ClientVersion::new(1, 1)));

        let wider = ServiceApiVersion::new(1, 4, 2);
        assert!(!wider.accepts(ClientVersion::new(1, 1)));
        assert!(wider.accepts(ClientVersion::new(1, 2)));
        assert!(wider.accepts(ClientVersion::new(1, 4)));
        assert!(!wider.accepts(ClientVersion::new(1, 5)));
    }

    #[test]
    fn capability_names_are_the_contract_set_in_order() {
        assert_eq!(
            CAPABILITY_NAMES,
            [
                "terminal_sessions",
                "worker_sessions",
                "events",
                "control_transfer",
                "idempotent_mutations",
            ]
        );
        assert_eq!(CAPABILITIES.all().map(|(name, _)| name), CAPABILITY_NAMES);
    }

    #[test]
    fn worker_sessions_stay_disabled_and_terminal_sessions_enabled() {
        assert_eq!(
            CAPABILITIES,
            Capabilities {
                terminal_sessions: true,
                worker_sessions: false,
                events: true,
                control_transfer: true,
                idempotent_mutations: true,
            }
        );
        assert_eq!(
            CAPABILITIES.enabled_names().collect::<Vec<_>>(),
            vec![
                "terminal_sessions",
                "events",
                "control_transfer",
                "idempotent_mutations",
            ]
        );
        assert!(CAPABILITIES.is_enabled("terminal_sessions"));
        assert!(!CAPABILITIES.is_enabled("worker_sessions"));
        assert!(!CAPABILITIES.is_enabled("unknown_capability"));
    }
}
