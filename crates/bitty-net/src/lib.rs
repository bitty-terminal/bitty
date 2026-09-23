//! `bitty-net`: network dependency seam (shell only).
//!
//! This crate is the single seam through which future Bitty components may
//! touch the network. It currently exposes only the vocabulary — capability,
//! error, and policy types — that those consumers will depend on.
//!
//! # Sealing note
//!
//! Sockets arrive in a follow-up task. This shell performs no I/O, opens no
//! sockets, spawns no background tasks, and takes no network dependencies
//! (its manifest is dependency-free). Anything that needs the network today
//! must still go through its existing path; nothing here can move a byte.
//!
//! # Example
//!
//! ```
//! use bitty_net::{NetworkCapability, NetworkError};
//!
//! let offline = NetworkCapability::offline();
//! assert!(offline.is_offline());
//! assert_eq!(
//!     offline.check("example.com"),
//!     Err(NetworkError::Offline)
//! );
//!
//! let capped = NetworkCapability::offline().with_domain("example.com");
//! assert!(capped.allows("example.com"));
//! assert!(!capped.allows("elsewhere.example"));
//! ```

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

/// Domain allowlist describing what a network consumer may contact.
///
/// The default is deny-all: [`NetworkCapability::offline`] holds no domains
/// and rejects every host with [`NetworkError::Offline`]. Consumers opt in
/// per domain with [`NetworkCapability::with_domain`]; there is no wildcard.
/// Matching is exact on the lowercased host, so `example.com` never covers
/// `sub.example.com` — each name must be listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkCapability {
    allowed_domains: HashSet<String>,
}

impl NetworkCapability {
    /// Deny-all capability: no domain is allowed (offline-first default).
    #[must_use]
    pub fn offline() -> Self {
        Self::default()
    }

    /// Allow one additional exact domain (builder style).
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.allowed_domains.insert(normalize_domain(domain.into()));
        self
    }

    /// True when no domain is allowed.
    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.allowed_domains.is_empty()
    }

    /// True when `domain` is on the allowlist (exact, case-insensitive).
    #[must_use]
    pub fn allows(&self, domain: &str) -> bool {
        self.allowed_domains.contains(&normalize_domain(domain))
    }

    /// Check `domain` against the allowlist.
    ///
    /// Returns `Ok(())` when the domain is allowed, [`NetworkError::Offline`]
    /// when the capability is deny-all, and [`NetworkError::Denied`] when
    /// other domains are allowed but this one is not.
    pub fn check(&self, domain: &str) -> Result<(), NetworkError> {
        if self.allows(domain) {
            Ok(())
        } else if self.is_offline() {
            Err(NetworkError::Offline)
        } else {
            Err(NetworkError::Denied {
                domain: normalize_domain(domain),
            })
        }
    }
}

/// Marker policy: networking is unavailable until a capability says otherwise.
///
/// Offline-first means every consumer starts from [`NetworkCapability::offline`]
/// and must be handed an explicit allowlist before any socket work (which
/// itself lands in a follow-up task). This type exists so call sites and
/// signatures can name the policy; it carries no data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OfflineFirst {
    _private: (),
}

impl OfflineFirst {
    /// Name the offline-first policy at a call site.
    #[must_use]
    pub fn policy() -> Self {
        Self { _private: () }
    }
}

/// Typed network failure for current and future `bitty-net` consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkError {
    /// The domain is not on the capability allowlist.
    Denied {
        /// Normalized domain that was rejected.
        domain: String,
    },
    /// The capability is deny-all (offline); no domain is reachable.
    Offline,
    /// An allowed operation exceeded its deadline without sockets involved
    /// yet; reserved for the socket follow-up.
    Timeout {
        /// Deadline that expired.
        after: Duration,
    },
    /// An allowed operation would exceed its transfer budget; reserved for
    /// the socket follow-up.
    Budget {
        /// Budget that would be exceeded, in bytes.
        limit_bytes: u64,
    },
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied { domain } => write!(f, "network denied: {domain}"),
            Self::Offline => write!(f, "network offline"),
            Self::Timeout { after } => {
                write!(f, "network timeout after {}ms", after.as_millis())
            }
            Self::Budget { limit_bytes } => {
                write!(f, "network budget exceeded: {limit_bytes} bytes")
            }
        }
    }
}

impl std::error::Error for NetworkError {}

/// Lowercase and trim one trailing dot (`example.com.`); keep matching exact.
fn normalize_domain(domain: impl AsRef<str>) -> String {
    domain.as_ref().trim().trim_end_matches('.').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_denies_everything() {
        let cap = NetworkCapability::offline();
        assert!(cap.is_offline());
        assert!(!cap.allows("example.com"));
        assert_eq!(cap.check("example.com"), Err(NetworkError::Offline));
    }

    #[test]
    fn allowlist_is_exact_and_case_insensitive() {
        let cap = NetworkCapability::offline().with_domain("Example.COM.");
        assert!(!cap.is_offline());
        assert!(cap.allows("example.com"));
        assert!(!cap.allows("sub.example.com"));
        assert_eq!(
            cap.check("other.example"),
            Err(NetworkError::Denied {
                domain: "other.example".to_owned()
            })
        );
    }

    #[test]
    fn error_display_is_stable() {
        assert_eq!(
            NetworkError::Offline.to_string(),
            "network offline".to_owned()
        );
        assert_eq!(
            NetworkError::Timeout {
                after: Duration::from_secs(2)
            }
            .to_string(),
            "network timeout after 2000ms".to_owned()
        );
        assert_eq!(
            NetworkError::Budget { limit_bytes: 8 }.to_string(),
            "network budget exceeded: 8 bytes".to_owned()
        );
    }
}
