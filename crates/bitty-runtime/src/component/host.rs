//! Production host for installed native components (CTX-1361, bitty#2254).
//!
//! [`ComponentHost`] owns exactly one [`ComponentBroker`] for a Bitty
//! instance. It adds the request/response shape the composition root and
//! its tests need without introducing a second broker or a second protocol:
//! every spawn still goes through [`ComponentBroker::submit`], which
//! resolves the installed store and verifies the descriptor digest before
//! every process start.
//!
//! Capability authorization stays outside this type (the broker receives an
//! already-computed [`PluginGrant`]); `bitty-runtime`'s `Runtime` derives
//! that grant from the accepted `PluginHost` grant store and manifest
//! capabilities. [`ComponentHost::request`] is the bounded synchronous
//! convenience over `submit` + `wait` for callers that do not own an event
//! loop.

use std::fmt;
use std::time::{Duration, Instant};

use bitty_network_wire::ErrorKind;

use super::broker::{
    BrokerConfig, BrokerError, BrokerEvent, BrokerEventKind, ComponentBroker, ComponentRequest,
    ComponentStatus, RequestId,
};
use super::descriptor::ResolveError;
use super::grant::{GrantError, PluginGrant};
use super::{COMPONENT_HANDSHAKE_TIMEOUT, COMPONENT_REQUEST_DEADLINE_GRACE, effective_timeout};

/// Real-time slice one [`ComponentHost::request`] blocks in `wait`.
pub const COMPONENT_HOST_WAIT_SLICE: Duration = Duration::from_millis(50);

/// Assembled response for one successful component request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentResponse {
    /// HTTP status from the component.
    pub status: u16,
    /// Response headers in component order.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Hosted request failure, classified for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentHostError {
    /// No broker/host is installed on this runtime (fail closed).
    Unavailable,
    /// The accepted capability model does not authorize this request.
    CapabilityDenied {
        /// Requesting plugin id (when known).
        plugin: String,
        /// Stable, bounded reason.
        reason: String,
    },
    /// Grant computation rejected the plugin's declared authority.
    Grant(GrantError),
    /// The broker refused to submit or lost the component.
    Broker(BrokerError),
    /// The component answered with a terminal failure.
    RequestFailed {
        /// Wire error class.
        kind: ErrorKind,
        /// Redacted detail.
        message: String,
    },
    /// The component did not finish before the Core deadline.
    Timeout,
}

impl ComponentHostError {
    /// Stable error class for diagnostics and acceptance tests.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Unavailable => "component_host_unavailable",
            Self::CapabilityDenied { .. } => "capability_denied",
            Self::Grant(_) => "component_grant_invalid",
            Self::Broker(error) => match error {
                BrokerError::Resolve(ResolveError::NotInstalled { .. } | ResolveError::NoRoot) => {
                    "component_unavailable"
                }
                BrokerError::Resolve(
                    ResolveError::DigestMismatch { .. }
                    | ResolveError::Descriptor(_)
                    | ResolveError::NotRegular { .. }
                    | ResolveError::NameMismatch { .. },
                ) => "component_untrusted",
                BrokerError::Resolve(_) => "component_invalid",
                BrokerError::Backoff { .. } => "component_backoff",
                BrokerError::Unavailable => "component_unavailable",
                BrokerError::Busy => "component_busy",
                BrokerError::InvalidRequest(_) => "component_invalid_request",
                BrokerError::BodyTooLarge => "component_body_too_large",
                BrokerError::Spawn(_) => "component_spawn_failed",
            },
            Self::RequestFailed { .. } => "component_request_failed",
            Self::Timeout => "component_timeout",
        }
    }
}

impl fmt::Display for ComponentHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("no component host is installed"),
            Self::CapabilityDenied { plugin, reason } => {
                write!(
                    f,
                    "component request denied for plugin '{plugin}': {reason}"
                )
            }
            Self::Grant(error) => write!(f, "component grant invalid: {error}"),
            Self::Broker(error) => write!(f, "{error}"),
            Self::RequestFailed { kind, message } => {
                write!(f, "component request failed ({kind:?}): {message}")
            }
            Self::Timeout => f.write_str("component request timed out"),
        }
    }
}

impl std::error::Error for ComponentHostError {}

impl From<BrokerError> for ComponentHostError {
    fn from(error: BrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<GrantError> for ComponentHostError {
    fn from(error: GrantError) -> Self {
        Self::Grant(error)
    }
}

/// One Bitty instance's native-component host.
pub struct ComponentHost {
    broker: ComponentBroker,
}

impl fmt::Debug for ComponentHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComponentHost")
            .field("broker", &self.broker)
            .finish()
    }
}

impl ComponentHost {
    /// Host over an already-configured broker.
    #[must_use]
    pub fn new(broker: ComponentBroker) -> Self {
        Self { broker }
    }

    /// Host over the DIR-030 broker configuration.
    #[must_use]
    pub fn from_config(config: BrokerConfig) -> Self {
        Self::new(ComponentBroker::new(config))
    }

    /// Install the broker's warn sink for crash/handshake/idle stderr tails.
    #[must_use]
    pub fn with_warn_sink(mut self, sink: impl super::ComponentWarnSink + 'static) -> Self {
        self.broker = self.broker.with_warn_sink(sink);
        self
    }

    /// Broker access for diagnostics and headless callers.
    #[must_use]
    pub fn broker(&self) -> &ComponentBroker {
        &self.broker
    }

    /// Mutable broker access for non-blocking submit/poll callers.
    pub fn broker_mut(&mut self) -> &mut ComponentBroker {
        &mut self.broker
    }

    /// Non-blocking submit; semantics are exactly [`ComponentBroker::submit`].
    pub fn submit(
        &mut self,
        now: Instant,
        component: &str,
        grant: &PluginGrant,
        request: ComponentRequest,
    ) -> Result<RequestId, ComponentHostError> {
        Ok(self.broker.submit(now, component, grant, request)?)
    }

    /// Submit one request and wait (real time) for its terminal event.
    ///
    /// The wait bound is the broker's effective request timeout plus the
    /// deadline grace and handshake allowance. The component still enforces
    /// its own deadline first; this bound only guarantees the caller cannot
    /// wait forever on a silent process.
    pub fn request(
        &mut self,
        component: &str,
        grant: &PluginGrant,
        request: ComponentRequest,
    ) -> Result<ComponentResponse, ComponentHostError> {
        let bound = effective_timeout(request.timeout_ms)
            + COMPONENT_REQUEST_DEADLINE_GRACE
            + COMPONENT_HANDSHAKE_TIMEOUT;
        let id = self.submit(Instant::now(), component, grant, request)?;
        let deadline = Instant::now() + bound;
        let mut response = ComponentResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let mut saw_head = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.broker.cancel(id);
                return Err(ComponentHostError::Timeout);
            }
            let wait = remaining.min(COMPONENT_HOST_WAIT_SLICE);
            let events = self.broker.wait(Instant::now(), wait);
            for event in events {
                if event.id != id {
                    continue;
                }
                match event.kind {
                    BrokerEventKind::Head { status, headers } => {
                        if !saw_head {
                            response.status = status;
                            response.headers = headers;
                            saw_head = true;
                        }
                    }
                    BrokerEventKind::Body { data, last } => {
                        response.body.extend_from_slice(&data);
                        if last {
                            return Ok(response);
                        }
                    }
                    BrokerEventKind::Failed { kind, message } => {
                        return Err(ComponentHostError::RequestFailed { kind, message });
                    }
                }
            }
        }
    }

    /// Process pending component output and lifecycle policy at `now`.
    pub fn poll(&mut self, now: Instant) -> Vec<BrokerEvent> {
        self.broker.poll(now)
    }

    /// Diagnostic snapshot for a component this host has used.
    #[must_use]
    pub fn status(&self, component: &str) -> Option<ComponentStatus> {
        self.broker.status(component)
    }

    /// Graceful stop of every recorded child.
    pub fn shutdown(&mut self) -> Vec<BrokerEvent> {
        self.broker.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classes_are_stable() {
        assert_eq!(
            ComponentHostError::Unavailable.class(),
            "component_host_unavailable"
        );
        assert_eq!(
            ComponentHostError::CapabilityDenied {
                plugin: "acme.test".into(),
                reason: "no grant".into(),
            }
            .class(),
            "capability_denied"
        );
        assert_eq!(
            ComponentHostError::Broker(BrokerError::Resolve(ResolveError::DigestMismatch {
                expected: "0".repeat(64),
                actual: "1".repeat(64),
            }))
            .class(),
            "component_untrusted"
        );
    }
}
