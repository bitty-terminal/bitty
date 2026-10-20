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

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use bitty_network_wire::ErrorKind;

use super::activation::{ActivationDenial, ActivationState};
use super::broker::{
    BrokerConfig, BrokerError, BrokerEvent, BrokerEventKind, ComponentBroker, ComponentRequest,
    ComponentStatus, RequestId,
};
use super::descriptor::ResolveError;
use super::grant::{GrantError, PluginGrant};
use super::{COMPONENT_HANDSHAKE_TIMEOUT, COMPONENT_REQUEST_DEADLINE_GRACE, effective_timeout};

/// Real-time slice one [`ComponentHost::request`] blocks in `wait`.
pub const COMPONENT_HOST_WAIT_SLICE: Duration = Duration::from_millis(50);

/// Events observed by [`ComponentHost::request`] for other request ids that
/// are retained for a later [`ComponentHost::poll`] (oldest dropped first).
pub const COMPONENT_HOST_DEFERRED_MAX_EVENTS: usize = 256;

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
    /// The plugin is not in the `Activated` lifecycle state.
    PluginNotActive {
        /// Requesting plugin id.
        plugin: String,
        /// Current lifecycle state (`Registered`, `Suspended`, ...).
        state: String,
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
            Self::PluginNotActive { .. } => "plugin_not_active",
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
            Self::PluginNotActive { plugin, state } => write!(
                f,
                "component request denied for plugin '{plugin}': state {state} is not Activated"
            ),
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

/// Activation state a fail-closed host failure records for `bitty extension
/// list` (#2254).
///
/// Only refusals that leave the component unserved map to a state: denials
/// and resolution failures become `denied:<class>`, and the post-crash latch
/// (`BrokerError::Unavailable`) stays `crashed`. Transient conditions
/// (`Busy`, `Backoff`, invalid request/body, spawn I/O) record nothing — the
/// crash path already recorded the underlying outcome, and a transient
/// refusal must not overwrite a meaningful state.
fn activation_for_error(error: &ComponentHostError) -> Option<ActivationState> {
    match error {
        ComponentHostError::CapabilityDenied { .. } => {
            Some(ActivationState::Denied(ActivationDenial::CapabilityDenied))
        }
        ComponentHostError::PluginNotActive { .. } => {
            Some(ActivationState::Denied(ActivationDenial::PluginNotActive))
        }
        ComponentHostError::Grant(_) => {
            Some(ActivationState::Denied(ActivationDenial::GrantInvalid))
        }
        ComponentHostError::Broker(BrokerError::Resolve(error)) => match error {
            ResolveError::DigestMismatch { .. }
            | ResolveError::Descriptor(_)
            | ResolveError::NotRegular { .. }
            | ResolveError::NameMismatch { .. } => {
                Some(ActivationState::Denied(ActivationDenial::Untrusted))
            }
            ResolveError::NotInstalled { .. } | ResolveError::NoRoot => {
                Some(ActivationState::Denied(ActivationDenial::Unavailable))
            }
            _ => None,
        },
        ComponentHostError::Broker(BrokerError::Unavailable) => Some(ActivationState::Crashed),
        _ => None,
    }
}

/// One Bitty instance's native-component host.
pub struct ComponentHost {
    broker: ComponentBroker,
    /// Events for other request ids observed while a synchronous `request`
    /// waited; handed back by the next `poll`.
    deferred: VecDeque<BrokerEvent>,
    deferred_dropped: u64,
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
        Self {
            broker,
            deferred: VecDeque::new(),
            deferred_dropped: 0,
        }
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
        match self.broker.submit(now, component, grant, request) {
            Ok(id) => Ok(id),
            Err(error) => {
                let error = ComponentHostError::from(error);
                self.record_denial(component, &error);
                Err(error)
            }
        }
    }

    /// Record a fail-closed denial for `component` (best effort, #2254).
    ///
    /// The runtime calls this for authorization failures that happen before
    /// the broker is reached (grant/lifecycle denials), and [`Self::submit`]
    /// calls it for pre-spawn broker failures, so `bitty extension list`
    /// reports one `denied:<class>` label for either path.
    pub fn record_denial(&mut self, component: &str, error: &ComponentHostError) {
        if let Some(state) = activation_for_error(error) {
            self.broker.record_activation(component, state);
        }
    }

    /// Submit one request and wait (real time) for its terminal event.
    ///
    /// The wait bound is the broker's effective request timeout plus the
    /// deadline grace and handshake allowance (up to 310 s). This call blocks
    /// and must never run on the runtime owner/UI thread; use `submit` plus
    /// `poll` there until an asynchronous delivery surface lands.
    ///
    /// Events for other in-flight request ids are retained (bounded) and
    /// returned by the next [`Self::poll`], so mixing this call with
    /// non-blocking submitters does not lose their responses.
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
                    self.defer(event);
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

    fn defer(&mut self, event: BrokerEvent) {
        if self.deferred.len() >= COMPONENT_HOST_DEFERRED_MAX_EVENTS {
            let _ = self.deferred.pop_front();
            self.deferred_dropped = self.deferred_dropped.saturating_add(1);
        }
        self.deferred.push_back(event);
    }

    /// Process pending component output and lifecycle policy at `now`.
    ///
    /// Events retained by an earlier synchronous [`Self::request`] come
    /// first, then newly polled events.
    pub fn poll(&mut self, now: Instant) -> Vec<BrokerEvent> {
        let mut events: Vec<BrokerEvent> = self.deferred.drain(..).collect();
        events.extend(self.broker.poll(now));
        events
    }

    /// Events dropped from the bounded deferred queue.
    #[must_use]
    pub fn deferred_dropped(&self) -> u64 {
        self.deferred_dropped
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

    /// Cancel one in-flight request (CTX-2254).
    ///
    /// Used by the plugin-facing request surface when its deadline or response
    /// budget closes before the component answered: the broker sends `Cancel`
    /// and discards late frames, so no unbounded work survives an abandoned
    /// ticket.
    pub fn cancel(&mut self, id: RequestId) {
        self.broker.cancel(id);
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
