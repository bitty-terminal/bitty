//! Per-plugin network grant: granted `network.connect:*` capabilities
//! intersected with the manifest `[[network.egress]]` declarations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bitty_network_wire::{
    Grant, GrantHost, MAX_GRANT_HOST_BYTES, MAX_GRANT_HOSTS, MAX_GRANT_PORTS, MAX_PLUGIN_ID_BYTES,
};
use bitty_plugin_host::capability::CapabilityId;
use bitty_plugin_host::manifest::NetworkEgress;

/// Capability head whose parameter names a destination.
const NETWORK_CONNECT_PREFIX: &str = "network.connect:";

/// Core-built-in installer egress host (first-party CDN, issue #1905).
///
/// The minimal install seed (`bitty component install`) fetches exactly this
/// host over HTTPS; strict configurations must allow it without a per-plugin
/// `[[network.egress]]` declaration or per-source consent. Every other host
/// stays consent-gated (fail closed without explicit consent).
pub const BUILTIN_INSTALLER_EGRESS_HOST: &str = "cdn.bitty.run";

/// Core-built-in installer egress port (HTTPS).
pub const BUILTIN_INSTALLER_EGRESS_PORT: u16 = 443;

/// Core-built-in installer egress in `host:port` form for strict-config
/// allowlists.
pub const BUILTIN_INSTALLER_EGRESS: &str = "cdn.bitty.run:443";

/// Whether `(host, port)` is the core-built-in installer egress.
///
/// Exact, case-sensitive match on the host plus equality on the port.
/// Suffix tricks (`cdn.bitty.run.evil.com`), userinfo shapes (split before
/// calling: `cdn.bitty.run@evil` never equals the host), case tricks
/// (`CDN.BITTY.RUN`), and port swaps (`:8443`) all return `false` by
/// construction. Callers must pass an already-split host/port pair, never a
/// raw URL.
#[must_use]
pub fn is_builtin_installer_egress(host: &str, port: u16) -> bool {
    host == BUILTIN_INSTALLER_EGRESS_HOST && port == BUILTIN_INSTALLER_EGRESS_PORT
}

/// The core-built-in installer egress entry for strict-config allowlists.
///
/// Returns the single `[[network.egress]]`-shaped entry covering the
/// first-party CDN (`cdn.bitty.run:443`). Third-party hosts have no
/// built-in entry and must be declared plus consented explicitly.
#[must_use]
pub fn builtin_installer_egress() -> NetworkEgress {
    NetworkEgress {
        host: BUILTIN_INSTALLER_EGRESS_HOST.to_string(),
        ports: vec![BUILTIN_INSTALLER_EGRESS_PORT],
    }
}

/// A Core-computed grant bound to the plugin it was computed for.
///
/// The only constructor is [`PluginGrant::compute`], so a request can never
/// carry a hand-built grant wider than the plugin's consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginGrant {
    plugin_id: String,
    grant: Grant,
}

/// Why a grant could not be computed (fail closed: no request is sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// The plugin id is empty or longer than the wire bound.
    InvalidPluginId,
    /// More distinct hosts than the wire grant can carry.
    TooManyHosts {
        /// Distinct hosts in the intersection.
        actual: usize,
    },
    /// More ports for one host than the wire grant can carry.
    TooManyPorts {
        /// Host with too many ports.
        host: String,
        /// Distinct ports in the intersection.
        actual: usize,
    },
    /// A host is longer than the wire bound.
    HostTooLong {
        /// Byte length of the host.
        len: usize,
    },
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrantError::InvalidPluginId => write!(
                f,
                "plugin id must be 1..={MAX_PLUGIN_ID_BYTES} bytes for component attribution"
            ),
            GrantError::TooManyHosts { actual } => {
                write!(f, "grant has {actual} hosts (max {MAX_GRANT_HOSTS})")
            }
            GrantError::TooManyPorts { host, actual } => write!(
                f,
                "grant host '{host}' has {actual} ports (max {MAX_GRANT_PORTS})"
            ),
            GrantError::HostTooLong { len } => {
                write!(
                    f,
                    "grant host of {len} bytes exceeds {MAX_GRANT_HOST_BYTES}"
                )
            }
        }
    }
}

impl std::error::Error for GrantError {}

impl PluginGrant {
    /// Compute the grant for `plugin_id`.
    ///
    /// For every granted `network.connect:HOST[:PORT]` capability, each
    /// egress entry for exactly `HOST` contributes its declared ports; an
    /// explicit `:PORT` contributes only that port, and only when an entry
    /// declares it. Hosts without a matching declaration contribute nothing,
    /// and no method restriction is derived (the capability grammar has
    /// none). A plugin without network capabilities gets an empty grant,
    /// which a component treats as offline.
    pub fn compute<'a>(
        plugin_id: &str,
        granted: impl IntoIterator<Item = &'a CapabilityId>,
        egress: &[NetworkEgress],
    ) -> Result<Self, GrantError> {
        if plugin_id.is_empty() || plugin_id.len() > MAX_PLUGIN_ID_BYTES {
            return Err(GrantError::InvalidPluginId);
        }
        let mut hosts: BTreeMap<&str, BTreeSet<u16>> = BTreeMap::new();
        for capability in granted {
            let Some(param) = capability.as_str().strip_prefix(NETWORK_CONNECT_PREFIX) else {
                continue;
            };
            let (host, port) = split_host_port(param);
            for entry in egress.iter().filter(|entry| entry.host == host) {
                let ports = entry.ports.iter().copied().filter(|p| *p != 0);
                match port {
                    Some(port) => {
                        if entry.ports.contains(&port) && port != 0 {
                            hosts.entry(entry.host.as_str()).or_default().insert(port);
                        }
                    }
                    None => hosts.entry(entry.host.as_str()).or_default().extend(ports),
                }
            }
        }
        hosts.retain(|_, ports| !ports.is_empty());
        if hosts.len() > MAX_GRANT_HOSTS {
            return Err(GrantError::TooManyHosts {
                actual: hosts.len(),
            });
        }
        let mut grant_hosts = Vec::with_capacity(hosts.len());
        for (host, ports) in hosts {
            if host.len() > MAX_GRANT_HOST_BYTES {
                return Err(GrantError::HostTooLong { len: host.len() });
            }
            if ports.len() > MAX_GRANT_PORTS {
                return Err(GrantError::TooManyPorts {
                    host: host.to_owned(),
                    actual: ports.len(),
                });
            }
            grant_hosts.push(GrantHost {
                host: host.to_owned(),
                ports: ports.into_iter().collect(),
            });
        }
        Ok(Self {
            plugin_id: plugin_id.to_owned(),
            grant: Grant {
                hosts: grant_hosts,
                methods: None,
            },
        })
    }

    /// Plugin the grant was computed for (request attribution).
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// The wire grant attached to every request of this plugin.
    #[must_use]
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// Whether the grant allows no destination at all (offline).
    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.grant.hosts.is_empty()
    }
}

/// Split a `network.connect` parameter into `(host, port)` exactly like the
/// manifest pairing check: a trailing all-digit `:PORT` that fits `u16` is
/// the port, anything else is a bare host (which then simply fails to match
/// a declaration).
fn split_host_port(param: &str) -> (&str, Option<u16>) {
    match param.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            match port.parse::<u16>() {
                Ok(port) => (host, Some(port)),
                Err(_) => (param, None),
            }
        }
        _ => (param, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(raw: &[&str]) -> Vec<CapabilityId> {
        raw.iter()
            .map(|raw| CapabilityId::parse(raw).expect("valid capability"))
            .collect()
    }

    fn egress(host: &str, ports: &[u16]) -> NetworkEgress {
        NetworkEgress {
            host: host.to_owned(),
            ports: ports.to_vec(),
        }
    }

    #[test]
    fn bare_host_capability_takes_declared_ports() {
        let granted = caps(&["network.connect:api.example.com", "platform.notify"]);
        let grant = PluginGrant::compute(
            "acme.weather",
            &granted,
            &[
                egress("api.example.com", &[443, 8443]),
                egress("other.example.com", &[443]),
            ],
        )
        .expect("grant");
        assert_eq!(grant.plugin_id(), "acme.weather");
        assert_eq!(
            grant.grant().hosts,
            [GrantHost {
                host: "api.example.com".into(),
                ports: vec![443, 8443]
            }]
        );
        assert_eq!(grant.grant().methods, None);
    }

    #[test]
    fn explicit_port_narrows_to_declared_port_only() {
        let granted = caps(&[
            "network.connect:api.example.com:8443",
            "network.connect:api.example.com:9999",
        ]);
        let grant = PluginGrant::compute(
            "acme.weather",
            &granted,
            &[egress("api.example.com", &[443, 8443])],
        )
        .expect("grant");
        assert_eq!(grant.grant().hosts[0].ports, [8443]);
    }

    #[test]
    fn undeclared_or_ungranted_hosts_are_absent() {
        // Declared but not granted, and granted but not declared.
        let granted = caps(&["network.connect:granted.example.com"]);
        let grant = PluginGrant::compute(
            "acme.weather",
            &granted,
            &[egress("declared.example.com", &[443])],
        )
        .expect("grant");
        assert!(grant.is_offline());
    }

    #[test]
    fn no_network_capability_is_offline() {
        let grant = PluginGrant::compute(
            "acme.weather",
            &caps(&["platform.notify"]),
            &[egress("api.example.com", &[443])],
        )
        .expect("grant");
        assert!(grant.is_offline());
    }

    #[test]
    fn plugin_id_is_bounded() {
        assert_eq!(
            PluginGrant::compute("", &caps(&[]), &[]),
            Err(GrantError::InvalidPluginId)
        );
        let long = "p".repeat(MAX_PLUGIN_ID_BYTES + 1);
        assert_eq!(
            PluginGrant::compute(&long, &caps(&[]), &[]),
            Err(GrantError::InvalidPluginId)
        );
    }

    #[test]
    fn host_count_is_bounded_fail_closed() {
        let raw: Vec<String> = (0..=MAX_GRANT_HOSTS)
            .map(|i| format!("network.connect:h{i}.example.com"))
            .collect();
        let refs: Vec<&str> = raw.iter().map(String::as_str).collect();
        let entries: Vec<NetworkEgress> = (0..=MAX_GRANT_HOSTS)
            .map(|i| egress(&format!("h{i}.example.com"), &[443]))
            .collect();
        assert_eq!(
            PluginGrant::compute("acme.many", &caps(&refs), &entries),
            Err(GrantError::TooManyHosts {
                actual: MAX_GRANT_HOSTS + 1
            })
        );
    }

    #[test]
    fn split_host_port_matches_manifest_rules() {
        assert_eq!(split_host_port("a.example:443"), ("a.example", Some(443)));
        assert_eq!(split_host_port("a.example"), ("a.example", None));
        assert_eq!(
            split_host_port("a.example:99999"),
            ("a.example:99999", None)
        );
        assert_eq!(split_host_port(":443"), (":443", None));
    }

    #[test]
    fn builtin_installer_egress_is_pinned_first_party() {
        assert_eq!(BUILTIN_INSTALLER_EGRESS_HOST, "cdn.bitty.run");
        assert_eq!(BUILTIN_INSTALLER_EGRESS_PORT, 443);
        assert_eq!(BUILTIN_INSTALLER_EGRESS, "cdn.bitty.run:443");
        assert!(is_builtin_installer_egress("cdn.bitty.run", 443));
        let entry = builtin_installer_egress();
        assert_eq!(entry.host, "cdn.bitty.run");
        assert_eq!(entry.ports, vec![443]);
        assert!(entry.validate().is_ok());
    }

    #[test]
    fn builtin_installer_egress_rejects_hostile_shapes() {
        // Suffix, userinfo-split, case, and port-swap shapes all fail.
        for (host, port) in [
            ("cdn.bitty.run.evil.com", 443),
            ("cdn.bitty.run@evil", 443),
            ("evil.com", 443),
            ("CDN.BITTY.RUN", 443),
            ("Cdn.BitTy.Run", 443),
            ("cdn.bitty.run", 8443),
            ("cdn.bitty.run", 80),
            ("cdn.bitty.run", 0),
            ("", 443),
        ] {
            assert!(
                !is_builtin_installer_egress(host, port),
                "hostile egress must not be builtin: {host}:{port}"
            );
        }
    }
}
