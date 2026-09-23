//! `bitty-network-api`: stable network vocabulary (types only).
//!
//! This crate is the single vocabulary through which Bitty components name
//! network work: HTTP [`Request`]/[`Response`], [`WebSocketRequest`],
//! capability ([`NetworkCapability`], [`OfflineFirst`]), failure
//! ([`NetworkError`]), and the [`NetworkService`] boundary that
//! implementations serve behind.
//!
//! # Sealing note
//!
//! Types only. This crate performs no I/O, opens no sockets, spawns no
//! background tasks, and takes no implementation dependencies (its manifest
//! is dependency-free; `std` only). Anything that needs the network today
//! must still go through its existing path; nothing here can move a byte.
//! Sockets and transports arrive in follow-up tasks behind
//! [`NetworkService`], implemented in `bitty-network`.
//!
//! # Example
//!
//! ```
//! use bitty_network_api::{NetworkCapability, NetworkError};
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

/// Typed network failure for current and future network consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkError {
    /// The domain is not on the capability allowlist.
    Denied {
        /// Normalized domain that was rejected.
        domain: String,
    },
    /// The capability is deny-all (offline); no domain is reachable.
    Offline,
    /// An allowed operation exceeded its deadline; reserved for the socket
    /// follow-up that produces it.
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

/// HTTP method for [`Request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// Read-only retrieval.
    Get,
    /// Create a subordinate resource.
    Post,
    /// Replace the target resource.
    Put,
    /// Remove the target resource.
    Delete,
    /// Retrieval without a body.
    Head,
    /// Capability discovery.
    Options,
    /// Partial modification.
    Patch,
}

/// Outgoing HTTP request vocabulary (no I/O).
///
/// A value of this type describes intent only; executing it is the
/// [`NetworkService`] implementor's job. Check
/// [`NetworkCapability::check`] on [`Request::host`] before sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Request method.
    pub method: HttpMethod,
    /// Full URL (for example `https://example.com/path`).
    pub url: String,
    /// Request headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Request body bytes (empty for bodyless methods).
    pub body: Vec<u8>,
    /// Per-request deadline, when the caller sets one.
    pub timeout: Option<Duration>,
}

impl Request {
    /// Bodyless `GET` request for `url`.
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
        }
    }

    /// `POST` request for `url` with `body`.
    #[must_use]
    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: body.into(),
            timeout: None,
        }
    }

    /// Append one header (builder style).
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the per-request deadline (builder style).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Best-effort host authority of [`Request::url`].
    ///
    /// Strips the scheme, userinfo, and port so the result feeds
    /// [`NetworkCapability::check`] directly. Returns an empty string when
    /// the URL carries no parseable authority.
    #[must_use]
    pub fn host(&self) -> &str {
        url_host(&self.url)
    }
}

/// HTTP response vocabulary (no I/O).
///
/// Produced by [`NetworkService`] implementors; carries no socket handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Numeric status code (for example `200`).
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

impl Response {
    /// True for `2xx` statuses.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// First header value for `name` (case-insensitive on the name).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// WebSocket handshake request vocabulary (no I/O).
///
/// Describes the upgrade handshake only; the established stream type is the
/// implementor's [`NetworkService::Socket`] associated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketRequest {
    /// Full URL (for example `wss://example.com/socket`).
    pub url: String,
    /// Requested subprotocols, in preference order.
    pub protocols: Vec<String>,
    /// Handshake deadline, when the caller sets one.
    pub timeout: Option<Duration>,
}

impl WebSocketRequest {
    /// Handshake request for `url` with no subprotocols.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            protocols: Vec::new(),
            timeout: None,
        }
    }

    /// Offer one more subprotocol (builder style).
    #[must_use]
    pub fn with_protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocols.push(protocol.into());
        self
    }

    /// Set the handshake deadline (builder style).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Best-effort host authority of [`WebSocketRequest::url`]; see
    /// [`Request::host`] for the extraction rule.
    #[must_use]
    pub fn host(&self) -> &str {
        url_host(&self.url)
    }
}

/// Service boundary behind which network implementations live.
///
/// Consumers depend on this trait (via `bitty-network-api`) and reach the
/// implementation through IPC or service lookup; they never depend on the
/// implementing crate directly. Callers check the [`NetworkCapability`]
/// before invoking either method and treat [`NetworkError::Offline`] /
/// [`NetworkError::Denied`] as normal fail-closed control flow.
///
/// # Example
///
/// ```
/// use bitty_network_api::{
///     NetworkError, NetworkService, Request, Response, WebSocketRequest,
/// };
///
/// struct Offline;
///
/// impl NetworkService for Offline {
///     type Socket = ();
///
///     fn request(&self, _request: &Request) -> Result<Response, NetworkError> {
///         Err(NetworkError::Offline)
///     }
///
///     fn websocket(
///         &self,
///         _request: &WebSocketRequest,
///     ) -> Result<Self::Socket, NetworkError> {
///         Err(NetworkError::Offline)
///     }
/// }
/// ```
pub trait NetworkService {
    /// Established-connection type for [`NetworkService::websocket`].
    type Socket;

    /// Execute one HTTP [`Request`] synchronously.
    ///
    /// Returns [`NetworkError::Offline`] / [`NetworkError::Denied`] when the
    /// caller's capability rejects [`Request::host`].
    fn request(&self, request: &Request) -> Result<Response, NetworkError>;

    /// Perform one WebSocket handshake, returning the open stream.
    ///
    /// Returns [`NetworkError::Offline`] / [`NetworkError::Denied`] when the
    /// caller's capability rejects [`WebSocketRequest::host`].
    fn websocket(&self, request: &WebSocketRequest) -> Result<Self::Socket, NetworkError>;
}

/// Lowercase and trim one trailing dot (`example.com.`); keep matching exact.
fn normalize_domain(domain: impl AsRef<str>) -> String {
    domain.as_ref().trim().trim_end_matches('.').to_lowercase()
}

/// Best-effort host extraction over the URL vocabulary (no parsing
/// dependencies): strip the scheme, take the authority, drop userinfo and
/// port; IPv6 brackets are honored.
fn url_host(url: &str) -> &str {
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    if let Some(stripped) = hostport.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((host, _)) => host,
            None => stripped,
        }
    } else {
        match hostport.split_once(':') {
            Some((host, _)) => host,
            None => hostport,
        }
    }
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

    #[test]
    fn request_builders_carry_intent() {
        let request = Request::get("https://example.com/path")
            .with_header("Accept", "text/plain")
            .with_timeout(Duration::from_secs(5));
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.host(), "example.com");
        assert_eq!(
            request.headers,
            vec![("Accept".to_owned(), "text/plain".to_owned())]
        );
        assert_eq!(request.timeout, Some(Duration::from_secs(5)));

        let post = Request::post("http://example.com:8080/submit", vec![1, 2]);
        assert_eq!(post.method, HttpMethod::Post);
        assert_eq!(post.host(), "example.com");
        assert_eq!(post.body, vec![1, 2]);
    }

    #[test]
    fn host_extraction_is_best_effort() {
        assert_eq!(
            Request::get("https://user@example.com./a?b#c").host(),
            "example.com."
        );
        assert_eq!(
            Request::get("ws://sub.example.com/socket").host(),
            "sub.example.com"
        );
        assert_eq!(Request::get("http://[::1]:8080/").host(), "::1");
        assert_eq!(Request::get("example.com").host(), "example.com");
        assert_eq!(Request::get("").host(), "");
    }

    #[test]
    fn response_helpers_read_status_and_headers() {
        let response = Response {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
            body: b"hi".to_vec(),
        };
        assert!(response.is_success());
        assert_eq!(response.header("content-type"), Some("text/plain"));
        assert_eq!(response.header("missing"), None);

        let failure = Response {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(!failure.is_success());
    }

    #[test]
    fn websocket_request_builders_carry_intent() {
        let handshake = WebSocketRequest::new("wss://example.com:443/socket")
            .with_protocol("chat")
            .with_timeout(Duration::from_secs(3));
        assert_eq!(handshake.host(), "example.com");
        assert_eq!(handshake.protocols, vec!["chat".to_owned()]);
        assert_eq!(handshake.timeout, Some(Duration::from_secs(3)));
    }

    #[test]
    fn offline_service_stub_fails_closed() {
        struct Offline;

        impl NetworkService for Offline {
            type Socket = ();

            fn request(&self, _request: &Request) -> Result<Response, NetworkError> {
                Err(NetworkError::Offline)
            }

            fn websocket(&self, _request: &WebSocketRequest) -> Result<Self::Socket, NetworkError> {
                Err(NetworkError::Offline)
            }
        }

        let service = Offline;
        assert_eq!(
            service.request(&Request::get("https://example.com")),
            Err(NetworkError::Offline)
        );
        assert_eq!(
            service.websocket(&WebSocketRequest::new("wss://example.com")),
            Err(NetworkError::Offline)
        );
    }
}
