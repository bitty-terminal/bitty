#![forbid(unsafe_code)]
//! Event Bus v1 contract: topic taxonomy, capability ledger, routing scope
//! (CW-22, issue #1000; `RFC-OQ-4`/`RFC-OQ-5`).
//!
//! Contract source: the accepted Panel Runtime RFC, which requires a
//! host-mediated bus with qualified `owner.name:topic` identifiers inside
//! the three-level budget envelope (per-subscription `64`, per-panel
//! `1024`/`256 KiB`, global `8192`/`2 MiB`, `DropOldest` default), and
//! leaves the v1 topic taxonomy (`RFC-OQ-4`) and the per-type capability
//! mapping (`RFC-OQ-5`) open.
//!
//! What this module records for v1:
//!
//! - [`BusTopicFamily`] + [`core_topic`] + [`V1_CORE_TOPICS`] — the closed
//!   v1 taxonomy: panel lifecycle, focus, file, git, AI, and helper-process
//!   events under the Core owner [`CORE_TOPIC_OWNER`]. Provider topics use
//!   their own `owner.name` prefix and the same [`EventTopic`] grammar.
//! - [`CapabilityLedger`] — the v1 enforcement bound: publish and subscribe
//!   require the existing closed host capability ([`BUS_PUBLISH_CAPABILITY`]
//!   / [`BUS_SUBSCRIBE_CAPABILITY`], both `panel.provider` today), verified
//!   through the registry's deny-by-default grant table. Per-family
//!   refinement (for example future `panel.bus.*` names) stays candidate
//!   under `RFC-OQ-5`: [`CapabilityLedger::candidate_capability`] names the
//!   direction without enforcing it, because the host closed set does not
//!   know those identifiers yet.
//! - [`CrossWindowRoute`] + [`RoutingScope`] — the cross-window routing
//!   decision: **bounded in-process bus first**. Same-window traffic routes
//!   [`RoutingScope::InProcess`]; cross-window traffic fails closed with
//!   [`RoutingError::CrossProcessDeferred`] until an IPC framing follow-up
//!   reuses the accepted IPC scopes instead of inventing a new surface.
//!
//! Transport, queues, budgets, and drop policy stay with [`PanelEventBus`];
//! this module owns taxonomy, gating vocabulary, and routing scope only.

use std::collections::BTreeSet;

use super::panel::{
    BUS_EVENT_MAX_BYTES, BUS_GLOBAL_BYTES_LIMIT, BUS_GLOBAL_LIMIT, BUS_PER_PANEL_BYTES_LIMIT,
    BUS_PER_PANEL_LIMIT, BUS_PER_SUBSCRIPTION_LIMIT, EventTopic, PanelError,
};

// ---------------------------------------------------------------------------
// v1 topic taxonomy (RFC-OQ-4)
// ---------------------------------------------------------------------------

/// Core owner prefix for v1 taxonomy topics. The host mints these; the
/// bare `bitty:topic` shape is rejected by [`EventTopic::parse`].
pub const CORE_TOPIC_OWNER: &str = "bitty.panel";

/// v1 topic families: panel lifecycle, focus, file, git, AI, helper-process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusTopicFamily {
    Lifecycle,
    Focus,
    File,
    Git,
    Ai,
    Helper,
}

impl BusTopicFamily {
    /// Family label used inside the topic string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::Focus => "focus",
            Self::File => "file",
            Self::Git => "git",
            Self::Ai => "ai",
            Self::Helper => "helper",
        }
    }

    /// Parses a family label.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "lifecycle" => Some(Self::Lifecycle),
            "focus" => Some(Self::Focus),
            "file" => Some(Self::File),
            "git" => Some(Self::Git),
            "ai" => Some(Self::Ai),
            "helper" => Some(Self::Helper),
            _ => None,
        }
    }
}

/// Closed v1 Core topic list. Providers subscribe to these; only the host
/// publishes them.
pub const V1_CORE_TOPICS: &[&str] = &[
    "bitty.panel:lifecycle.created",
    "bitty.panel:lifecycle.disposed",
    "bitty.panel:focus.changed",
    "bitty.panel:file.opened",
    "bitty.panel:git.branch-changed",
    "bitty.panel:ai.response-ready",
    "bitty.panel:helper.exited",
];

/// Whether `raw` names a v1 Core topic.
#[must_use]
pub fn is_v1_core_topic(raw: &str) -> bool {
    V1_CORE_TOPICS.contains(&raw)
}

/// Mints a Core topic `bitty.panel:<family>.<event>` through the accepted
/// [`EventTopic`] grammar (validated, `<= 64` bytes).
///
/// # Errors
///
/// [`PanelError::UnknownTopic`] or [`PanelError::ResourceExhausted`] when
/// `event` violates the topic grammar.
pub fn core_topic(family: BusTopicFamily, event: &str) -> Result<EventTopic, PanelError> {
    EventTopic::parse(&format!("{CORE_TOPIC_OWNER}:{}.{event}", family.as_str()))
}

/// Mints a provider topic `owner.name:<event>` through the same grammar.
/// The caller owns `owner.name`; Core never mints outside `bitty.panel`.
///
/// # Errors
///
/// [`PanelError::UnknownTopic`] or [`PanelError::ResourceExhausted`] when
/// the qualified name violates the topic grammar.
pub fn provider_topic(owner: &str, event: &str) -> Result<EventTopic, PanelError> {
    EventTopic::parse(&format!("{owner}:{event}"))
}

// ---------------------------------------------------------------------------
// Capability ledger (RFC-OQ-5)
// ---------------------------------------------------------------------------

/// v1 publish gate: the existing closed host capability a panel must hold
/// to emit bus traffic. Bound to `panel.provider` until `RFC-OQ-5` refines
/// per-family capabilities and the host closed set learns them.
pub const BUS_PUBLISH_CAPABILITY: &str = "panel.provider";

/// v1 subscribe gate: the existing closed host capability a panel must hold
/// to observe bus traffic. Same v1 bound as publish; refinement is open.
pub const BUS_SUBSCRIBE_CAPABILITY: &str = "panel.provider";

/// Publish/subscribe direction for the candidate capability table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusDirection {
    Publish,
    Subscribe,
}

/// v1 capability ledger for bus traffic.
///
/// Enforcement uses the closed host set (`panel.provider`) through the
/// registry's deny-by-default grant table. The per-family candidate table
/// ([`CapabilityLedger::candidate_capability`]) is recorded but **not**
/// enforced: those identifiers are unknown to the host closed set, and
/// enforcing unknown names would deny all traffic including the host's.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapabilityLedger;

impl CapabilityLedger {
    /// Whether `granted` (a panel's grant set) allows publishing.
    /// Deny-by-default: empty or unrelated grants refuse.
    #[must_use]
    pub fn can_publish(granted: &BTreeSet<String>) -> bool {
        granted.contains(BUS_PUBLISH_CAPABILITY)
    }

    /// Whether `granted` allows subscribing. Deny-by-default.
    #[must_use]
    pub fn can_subscribe(granted: &BTreeSet<String>) -> bool {
        granted.contains(BUS_SUBSCRIBE_CAPABILITY)
    }

    /// Candidate (unenforced) per-family capability for the `RFC-OQ-5`
    /// follow-up, e.g. `panel.bus.publish`. Naming only: do not gate on
    /// this until the host closed set accepts it.
    #[must_use]
    pub fn candidate_capability(_family: BusTopicFamily, direction: BusDirection) -> &'static str {
        match direction {
            BusDirection::Publish => "panel.bus.publish",
            BusDirection::Subscribe => "panel.bus.subscribe",
        }
    }
}

// ---------------------------------------------------------------------------
// Routing scope: bounded in-process first
// ---------------------------------------------------------------------------

/// v1 routing scope. The only v1 scope is in-process delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoutingScope {
    /// Same-window, same-process delivery through [`PanelEventBus`].
    InProcess,
}

/// Cross-window routing request between two window indexes in one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CrossWindowRoute {
    /// Source window index.
    pub from_window: u64,
    /// Destination window index.
    pub to_window: u64,
}

impl CrossWindowRoute {
    /// Creates a cross-window routing request.
    #[must_use]
    pub const fn new(from_window: u64, to_window: u64) -> Self {
        Self {
            from_window,
            to_window,
        }
    }

    /// Resolves the v1 routing decision: same-window traffic is
    /// [`RoutingScope::InProcess`]; cross-window traffic fails closed with
    /// [`RoutingError::CrossProcessDeferred`] until an IPC follow-up lands.
    ///
    /// # Errors
    ///
    /// [`RoutingError::CrossProcessDeferred`] for differing windows.
    pub fn resolve(&self) -> Result<RoutingScope, RoutingError> {
        if self.from_window == self.to_window {
            Ok(RoutingScope::InProcess)
        } else {
            Err(RoutingError::CrossProcessDeferred {
                from_window: self.from_window,
                to_window: self.to_window,
            })
        }
    }
}

/// Typed routing failure; no traffic moves on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutingError {
    /// Cross-window delivery deferred to a future IPC framing follow-up.
    CrossProcessDeferred { from_window: u64, to_window: u64 },
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CrossProcessDeferred {
                from_window,
                to_window,
            } => write!(
                f,
                "cross-window bus route {from_window} -> {to_window} deferred: v1 is in-process only"
            ),
        }
    }
}

impl std::error::Error for RoutingError {}

/// v1 budget envelope restated at the contract boundary so taxonomy
/// consumers bind the same ceilings the bus enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusBudgetEnvelope {
    /// Per-subscription queue depth.
    pub per_subscription: usize,
    /// Per-panel event aggregate.
    pub per_panel_events: usize,
    /// Per-panel byte aggregate.
    pub per_panel_bytes: usize,
    /// Global event aggregate.
    pub global_events: usize,
    /// Global byte aggregate.
    pub global_bytes: usize,
    /// Admission payload ceiling.
    pub max_event_bytes: usize,
}

/// The accepted three-level envelope (`64`/`1024`/`256 KiB`/`8192`/`2 MiB`/
/// `8 KiB`), sourced from the same constants the bus enforces.
pub const BUS_V1_BUDGET: BusBudgetEnvelope = BusBudgetEnvelope {
    per_subscription: BUS_PER_SUBSCRIPTION_LIMIT,
    per_panel_events: BUS_PER_PANEL_LIMIT,
    per_panel_bytes: BUS_PER_PANEL_BYTES_LIMIT,
    global_events: BUS_GLOBAL_LIMIT,
    global_bytes: BUS_GLOBAL_BYTES_LIMIT,
    max_event_bytes: BUS_EVENT_MAX_BYTES,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn grants_of(caps: &[&str]) -> BTreeSet<String> {
        caps.iter().map(|cap| cap.to_string()).collect()
    }

    #[test]
    fn v1_core_topics_parse_through_accepted_grammar() {
        for raw in V1_CORE_TOPICS {
            let topic = EventTopic::parse(raw).unwrap();
            assert_eq!(topic.as_str(), *raw);
        }
    }

    #[test]
    fn core_topic_mints_validated_family_topics() {
        let topic = core_topic(BusTopicFamily::Git, "branch-changed").unwrap();
        assert_eq!(topic.as_str(), "bitty.panel:git.branch-changed");
        assert!(core_topic(BusTopicFamily::Focus, "Bad Event").is_err());
    }

    #[test]
    fn provider_topics_use_owner_prefix() {
        let topic = provider_topic("example.git", "branch-changed").unwrap();
        assert_eq!(topic.as_str(), "example.git:branch-changed");
    }

    #[test]
    fn core_owner_shape_is_enforced() {
        // Single-segment `bitty:topic` violates the owner.name grammar.
        assert!(EventTopic::parse("bitty:impersonate").is_err());
        assert!(!is_v1_core_topic("bitty.panel:focus.unknown-event"));
    }

    #[test]
    fn ledger_denies_by_default() {
        let empty = grants_of(&[]);
        assert!(!CapabilityLedger::can_publish(&empty));
        assert!(!CapabilityLedger::can_subscribe(&empty));
        let unrelated = grants_of(&["panel.focus"]);
        assert!(!CapabilityLedger::can_publish(&unrelated));
        assert!(!CapabilityLedger::can_subscribe(&unrelated));
    }

    #[test]
    fn ledger_allows_granted_provider() {
        let granted = grants_of(&["panel.provider"]);
        assert!(CapabilityLedger::can_publish(&granted));
        assert!(CapabilityLedger::can_subscribe(&granted));
    }

    #[test]
    fn candidate_capabilities_stay_unenforced_names() {
        assert_eq!(
            CapabilityLedger::candidate_capability(BusTopicFamily::Git, BusDirection::Publish),
            "panel.bus.publish"
        );
        assert_eq!(
            CapabilityLedger::candidate_capability(BusTopicFamily::Ai, BusDirection::Subscribe),
            "panel.bus.subscribe"
        );
    }

    #[test]
    fn routing_decision_is_in_process_first() {
        assert_eq!(
            CrossWindowRoute::new(1, 1).resolve(),
            Ok(RoutingScope::InProcess)
        );
        assert_eq!(
            CrossWindowRoute::new(1, 2).resolve(),
            Err(RoutingError::CrossProcessDeferred {
                from_window: 1,
                to_window: 2,
            })
        );
    }

    #[test]
    fn budget_envelope_matches_bus_constants() {
        assert_eq!(BUS_V1_BUDGET.per_subscription, 64);
        assert_eq!(BUS_V1_BUDGET.per_panel_events, 1024);
        assert_eq!(BUS_V1_BUDGET.global_events, 8192);
        assert_eq!(BUS_V1_BUDGET.max_event_bytes, 8 * 1024);
    }
}
