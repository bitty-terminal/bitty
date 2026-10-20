//! Bounded plugin-facing native-component request surface (CTX-2254,
//! bitty#2254).
//!
//! `bitty.component.request` is the production producer DIR-030 left open: the
//! plugin-facing half of the broker that #2267 wired at the composition root.
//! The shape is deliberately two-phase so no Lua callback ever waits on a
//! component:
//!
//! 1. [`HostServices::component_request`](bitty_lua::HostServices) validates
//!    and enqueues one [`QueuedComponentRequest`] and returns a ticket. That
//!    call runs in the VM callback and never spawns, sends, or blocks.
//! 2. The composition root drains the queue once per tick
//!    (`bitty_terminal::component_bridge::pump_component_requests`),
//!    authorizes it through the runtime's canonical capability model,
//!    submits it to the existing
//!    [`ComponentBroker`](crate::component::ComponentBroker), and delivers
//!    broker events back into this queue.
//! 3. `bitty.component.poll` reads the completed result. Still in flight is
//!    `nil`; a terminal failure raises the typed class from
//!    [`bitty_lua::component_bridge_error`].
//!
//! Every bound is explicit: one submission queue, one completion window, a
//! per-plugin in-flight window, the existing 8 KiB bridge payload ceiling, an
//! 8 KiB response ceiling, and the request deadline carried from enqueue to
//! completion. There is no raw spawn path and no allow-all: authorization
//! stays with `Runtime::submit_component_request`, which re-checks the
//! plugin lifecycle, the manifest generation, and the grant record, and the
//! broker re-resolves and digest-verifies the component before every spawn.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bitty_lua::{
    BridgeError, COMPONENT_CLASS_BUDGET, COMPONENT_CLASS_TIMEOUT, COMPONENT_HEADER_MAX_BYTES,
    COMPONENT_HEADERS_MAX, COMPONENT_REQUEST_MAX_BYTES, COMPONENT_RESPONSE_MAX_BYTES,
    ComponentBridgeOutcome, ComponentBridgeRequest, ComponentMethod, component_bridge_error,
    component_response_value,
};

use crate::component::{
    BrokerEvent, BrokerEventKind, ComponentRequest, ErrorKind, Method, RequestId,
};

/// Pending submissions accepted between two application ticks.
pub const COMPONENT_REQUEST_QUEUE_CAPACITY: usize = 32;

/// Completed results retained for `bitty.component.poll` (oldest dropped
/// first; a dropped result reports the typed `budget` class on a late poll).
pub const COMPONENT_COMPLETION_CAPACITY: usize = 32;

/// Concurrent (enqueued, in-flight, or completed-but-unpolled) requests one
/// plugin generation may hold. Bounds a single plugin's share of the queue and
/// of the broker's `COMPONENT_MAX_IN_FLIGHT` window.
pub const COMPONENT_IN_FLIGHT_PER_PLUGIN: usize = 4;

/// One authorized-but-not-yet-submitted request.
#[derive(Debug, Clone)]
pub struct QueuedComponentRequest {
    /// Ticket returned to the plugin.
    pub ticket: u64,
    /// Requesting plugin id (derived host-side, never from Lua).
    pub plugin_id: String,
    /// Manifest generation the caller is bound to (derived host-side).
    pub manifest_hash: String,
    /// Bridge-validated request.
    pub request: ComponentBridgeRequest,
    /// Deadline carried from enqueue to completion.
    pub deadline: Instant,
}

struct InFlight {
    ticket: u64,
    plugin_id: String,
    deadline: Instant,
}

#[derive(Default)]
struct PartialResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketState {
    Pending,
    /// The completion window closed before the plugin polled.
    Expired,
}

/// `bitty.component.poll` outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentPollOutcome {
    /// Still in flight (the plugin polls again on a later tick).
    Pending,
    /// Terminal result.
    Ready(ComponentBridgeOutcome),
}

/// Runtime-shared bounded component request surface.
///
/// Shared by `Rc<RefCell<..>>` between the generations' `PluginServices` (which
/// enqueue and poll) and the application tick (which submits, delivers, and
/// expires). No authority lives here: every submission is authorized by the
/// runtime before it reaches the broker.
#[derive(Default)]
pub struct ComponentRequestQueue {
    next_ticket: u64,
    submissions: VecDeque<QueuedComponentRequest>,
    in_flight: BTreeMap<RequestId, InFlight>,
    partial: BTreeMap<RequestId, PartialResponse>,
    tickets: BTreeMap<(String, u64), TicketState>,
    completed: BTreeMap<(String, u64), ComponentBridgeOutcome>,
    completed_order: VecDeque<(String, u64)>,
    cancelled: Vec<RequestId>,
    dropped: u64,
}

impl ComponentRequestQueue {
    /// Empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate an enqueue against the per-generation and queue bounds and
    /// record the submission. Runs inside the VM callback; no I/O.
    ///
    /// # Errors
    ///
    /// Typed `budget` refusals when the generation's window or the queue is
    /// full, so an over-budget caller is told to back off instead of silently
    /// losing a request.
    pub fn enqueue(
        &mut self,
        plugin_id: &str,
        manifest_hash: &str,
        request: ComponentBridgeRequest,
        now: Instant,
    ) -> Result<u64, BridgeError> {
        let held = self
            .tickets
            .keys()
            .filter(|(owner, _)| owner == plugin_id)
            .count();
        if held >= COMPONENT_IN_FLIGHT_PER_PLUGIN {
            return Err(component_bridge_error(
                COMPONENT_CLASS_BUDGET,
                &format!(
                    "component request refused: plugin already holds {COMPONENT_IN_FLIGHT_PER_PLUGIN} requests"
                ),
            ));
        }
        if self.submissions.len() >= COMPONENT_REQUEST_QUEUE_CAPACITY {
            self.dropped = self.dropped.saturating_add(1);
            return Err(component_bridge_error(
                COMPONENT_CLASS_BUDGET,
                &format!(
                    "component request refused: the submission queue is full ({COMPONENT_REQUEST_QUEUE_CAPACITY})"
                ),
            ));
        }
        debug_assert!(request.payload_bytes() <= COMPONENT_REQUEST_MAX_BYTES);
        let deadline = now + Duration::from_millis(u64::from(request.timeout_ms));
        self.next_ticket = self.next_ticket.saturating_add(1);
        let ticket = self.next_ticket;
        self.tickets
            .insert((plugin_id.to_string(), ticket), TicketState::Pending);
        self.submissions.push_back(QueuedComponentRequest {
            ticket,
            plugin_id: plugin_id.to_string(),
            manifest_hash: manifest_hash.to_string(),
            request,
            deadline,
        });
        Ok(ticket)
    }

    /// Take the submissions queued since the last drain.
    pub fn take_submissions(&mut self) -> Vec<QueuedComponentRequest> {
        self.submissions.drain(..).collect()
    }

    /// Map one accepted submission to its broker request id.
    pub fn bind(&mut self, ticket: u64, plugin_id: &str, id: RequestId, deadline: Instant) {
        self.in_flight.insert(
            id,
            InFlight {
                ticket,
                plugin_id: plugin_id.to_string(),
                deadline,
            },
        );
        self.partial.insert(id, PartialResponse::default());
    }

    /// Complete one ticket with a typed failure before it reached the broker.
    pub fn fail(&mut self, ticket: u64, plugin_id: &str, class: &str, message: &str) {
        self.complete(
            plugin_id,
            ticket,
            ComponentBridgeOutcome::Failed {
                class: class.to_string(),
                message: message.to_string(),
            },
        );
    }

    /// Deliver one broker event; terminal events complete their ticket.
    pub fn deliver(&mut self, event: &BrokerEvent) {
        let Some(in_flight) = self.in_flight.get(&event.id) else {
            return;
        };
        let plugin_id = in_flight.plugin_id.clone();
        let ticket = in_flight.ticket;
        match &event.kind {
            BrokerEventKind::Head { status, headers } => {
                // The request path rejects a header above the same documented
                // bound. Fail the response closed instead of truncating, so a
                // plugin never mistakes a partial header set for the answer.
                let retained = self
                    .partial
                    .get(&event.id)
                    .map_or(0, |partial| partial.headers.len());
                if retained.saturating_add(headers.len()) > COMPONENT_HEADERS_MAX {
                    self.finish_failure(
                        event.id,
                        &plugin_id,
                        ticket,
                        COMPONENT_CLASS_BUDGET,
                        &format!(
                            "component response exceeds the {COMPONENT_HEADERS_MAX}-header bridge budget"
                        ),
                    );
                    return;
                }
                if headers.iter().any(|(name, value)| {
                    name.len() > COMPONENT_HEADER_MAX_BYTES
                        || value.len() > COMPONENT_HEADER_MAX_BYTES
                }) {
                    self.finish_failure(
                        event.id,
                        &plugin_id,
                        ticket,
                        COMPONENT_CLASS_BUDGET,
                        &format!(
                            "component response header name/value exceeds the {COMPONENT_HEADER_MAX_BYTES}-byte bridge budget"
                        ),
                    );
                    return;
                }
                let partial = self.partial.entry(event.id).or_default();
                partial.status = *status;
                partial.headers.extend(
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
            }
            BrokerEventKind::Body { data, last } => {
                let exceeds = self.partial.get(&event.id).is_none_or(|partial| {
                    partial.body.len().saturating_add(data.len()) > COMPONENT_RESPONSE_MAX_BYTES
                });
                if exceeds {
                    self.finish_failure(
                        event.id,
                        &plugin_id,
                        ticket,
                        COMPONENT_CLASS_BUDGET,
                        &format!(
                            "component response exceeded the {COMPONENT_RESPONSE_MAX_BYTES}-byte bridge budget"
                        ),
                    );
                    return;
                }
                let partial = self.partial.entry(event.id).or_default();
                partial.body.extend_from_slice(data);
                if *last {
                    let response = self.partial.remove(&event.id).unwrap_or_default();
                    self.in_flight.remove(&event.id);
                    self.complete(
                        &plugin_id,
                        ticket,
                        ComponentBridgeOutcome::Response {
                            status: response.status,
                            headers: response.headers,
                            body: response.body,
                        },
                    );
                }
            }
            BrokerEventKind::Failed { kind, message } => {
                let class = match kind {
                    ErrorKind::Timeout => COMPONENT_CLASS_TIMEOUT,
                    ErrorKind::Budget => COMPONENT_CLASS_BUDGET,
                    _ => "component_request_failed",
                };
                self.finish_failure(event.id, &plugin_id, ticket, class, message);
            }
        }
    }

    fn finish_failure(
        &mut self,
        id: RequestId,
        plugin_id: &str,
        ticket: u64,
        class: &str,
        message: &str,
    ) {
        self.in_flight.remove(&id);
        self.partial.remove(&id);
        self.cancelled.push(id);
        self.complete(
            plugin_id,
            ticket,
            ComponentBridgeOutcome::Failed {
                class: class.to_string(),
                message: message.to_string(),
            },
        );
    }

    fn complete(&mut self, plugin_id: &str, ticket: u64, outcome: ComponentBridgeOutcome) {
        let key = (plugin_id.to_string(), ticket);
        self.completed.insert(key.clone(), outcome);
        self.completed_order.push_back(key);
        while self.completed_order.len() > COMPONENT_COMPLETION_CAPACITY {
            if let Some(oldest) = self.completed_order.pop_front() {
                self.completed.remove(&oldest);
                self.tickets.insert(oldest, TicketState::Expired);
            }
        }
    }

    /// Fail every in-flight request whose deadline passed; returns the broker
    /// ids to cancel.
    pub fn expire(&mut self, now: Instant) -> Vec<RequestId> {
        let overdue: Vec<(RequestId, String, u64)> = self
            .in_flight
            .iter()
            .filter(|(_, in_flight)| in_flight.deadline <= now)
            .map(|(id, in_flight)| (*id, in_flight.plugin_id.clone(), in_flight.ticket))
            .collect();
        for (id, plugin_id, ticket) in overdue {
            self.finish_failure(
                id,
                &plugin_id,
                ticket,
                COMPONENT_CLASS_TIMEOUT,
                "component request did not finish before its deadline",
            );
        }
        self.take_cancelled()
    }

    /// Take the broker ids the bridge finished on its own (response budget,
    /// deadline) so the caller cancels them.
    pub fn take_cancelled(&mut self) -> Vec<RequestId> {
        std::mem::take(&mut self.cancelled)
    }

    /// Drop every ticket of one plugin generation (suspend, reload, dispose)
    /// and cancel its in-flight broker requests.
    ///
    /// A generation that lost its authority must never receive a result it
    /// could no longer have requested, and a reloaded generation must not
    /// observe its predecessor's results. Tickets are therefore purged rather
    /// than completed: a later poll from that plugin id reports the ticket as
    /// unknown, and every broker request of the generation is cancelled so no
    /// abandoned work survives.
    pub fn drop_plugin(&mut self, plugin_id: &str) {
        self.submissions.retain(|item| item.plugin_id != plugin_id);
        let dropped: Vec<RequestId> = self
            .in_flight
            .iter()
            .filter(|(_, in_flight)| in_flight.plugin_id == plugin_id)
            .map(|(id, _)| *id)
            .collect();
        for id in dropped {
            self.in_flight.remove(&id);
            self.partial.remove(&id);
            self.cancelled.push(id);
        }
        self.tickets.retain(|(owner, _), _| owner != plugin_id);
        self.completed.retain(|(owner, _), _| owner != plugin_id);
        self.completed_order.retain(|(owner, _)| owner != plugin_id);
    }

    /// Read one ticket for `plugin_id`.
    ///
    /// # Errors
    ///
    /// `validation` for a ticket this generation never held (unknown,
    /// already consumed, or purged with its generation); a closed completion
    /// window reports the typed `budget` class.
    pub fn poll(
        &mut self,
        plugin_id: &str,
        ticket: u64,
    ) -> Result<ComponentPollOutcome, BridgeError> {
        let key = (plugin_id.to_string(), ticket);
        if let Some(outcome) = self.completed.remove(&key) {
            self.tickets.remove(&key);
            self.completed_order.retain(|entry| entry != &key);
            return Ok(ComponentPollOutcome::Ready(outcome));
        }
        match self.tickets.get(&key) {
            Some(TicketState::Pending) => Ok(ComponentPollOutcome::Pending),
            Some(TicketState::Expired) => {
                self.tickets.remove(&key);
                Err(component_bridge_error(
                    COMPONENT_CLASS_BUDGET,
                    "component response window closed before this ticket was polled",
                ))
            }
            None => Err(BridgeError::new(
                "validation",
                "E_COMPONENT_TICKET",
                "component.poll ticket is unknown or already consumed",
            )),
        }
    }

    /// Submissions dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Requests currently in flight in the broker.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Pending submissions not yet taken by the pump.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.submissions.len()
    }
}

/// Broker wire method for one bridge method.
fn wire_method(method: ComponentMethod) -> Method {
    match method {
        ComponentMethod::Get => Method::Get,
        ComponentMethod::Head => Method::Head,
        ComponentMethod::Post => Method::Post,
        ComponentMethod::Put => Method::Put,
        ComponentMethod::Patch => Method::Patch,
        ComponentMethod::Delete => Method::Delete,
        ComponentMethod::Options => Method::Options,
    }
}

/// Build the broker request for one bridge request.
///
/// The response ceiling travels as the broker's `max_body_bytes`, so the
/// component's own budget and Core's enforcement agree with the bridge ceiling
/// instead of widening to the 8 MiB wire default.
#[must_use]
pub fn broker_request(request: &ComponentBridgeRequest) -> ComponentRequest {
    ComponentRequest {
        method: wire_method(request.method),
        url: request.url.clone(),
        headers: request.headers.clone(),
        timeout_ms: request.timeout_ms,
        max_body_bytes: COMPONENT_RESPONSE_MAX_BYTES as u64,
        body: request.body.as_ref().map(|body| body.as_bytes().to_vec()),
    }
}

/// Lua table shape of one completed outcome.
///
/// # Errors
///
/// Typed failure class for a terminal failure.
pub fn outcome_value(outcome: &ComponentBridgeOutcome) -> Result<bitty_lua::LuaValue, BridgeError> {
    match outcome {
        ComponentBridgeOutcome::Response {
            status,
            headers,
            body,
        } => Ok(component_response_value(*status, headers, body)),
        ComponentBridgeOutcome::Failed { class, message } => {
            Err(component_bridge_error(class, message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_lua::ComponentMethod;

    const PLUGIN: &str = "acme.surface";
    const HASH: &str = "manifest-hash";

    fn request(body: Option<&str>) -> ComponentBridgeRequest {
        ComponentBridgeRequest {
            component: "net".to_string(),
            method: ComponentMethod::Get,
            url: "https://api.example.com/v1/ping".to_string(),
            headers: Vec::new(),
            body: body.map(str::to_string),
            timeout_ms: 1_000,
        }
    }

    fn event(id: u64, kind: BrokerEventKind) -> BrokerEvent {
        BrokerEvent {
            id: RequestId::for_test(id),
            plugin_id: PLUGIN.to_string(),
            component: "net".to_string(),
            kind,
        }
    }

    #[test]
    fn per_plugin_window_is_bounded() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        for _ in 0..COMPONENT_IN_FLIGHT_PER_PLUGIN {
            queue
                .enqueue(PLUGIN, HASH, request(None), now)
                .expect("within window");
        }
        let error = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect_err("over window");
        assert_eq!(error.code, "E_COMPONENT_BUDGET");
        assert_eq!(error.class, "budget");
        assert_eq!(queue.pending(), COMPONENT_IN_FLIGHT_PER_PLUGIN);
        assert_eq!(queue.dropped(), 0, "a refusal is not a dropped submission");
    }

    #[test]
    fn submission_queue_is_bounded_and_counts_drops() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        for index in 0..COMPONENT_REQUEST_QUEUE_CAPACITY {
            let plugin = format!("acme.plugin-{index}");
            queue
                .enqueue(&plugin, HASH, request(None), now)
                .expect("within queue");
        }
        let error = queue
            .enqueue("acme.overflow", HASH, request(None), now)
            .expect_err("queue full");
        assert_eq!(error.code, "E_COMPONENT_BUDGET");
        assert_eq!(queue.dropped(), 1);
    }

    #[test]
    fn response_over_the_bridge_budget_fails_and_cancels() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        let ticket = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect("enqueue");
        let id = RequestId::for_test(7);
        queue.bind(ticket, PLUGIN, id, now + Duration::from_secs(1));
        queue.deliver(&event(
            7,
            BrokerEventKind::Head {
                status: 200,
                headers: Vec::new(),
            },
        ));
        queue.deliver(&event(
            7,
            BrokerEventKind::Body {
                data: vec![b'x'; COMPONENT_RESPONSE_MAX_BYTES + 1],
                last: true,
            },
        ));
        assert_eq!(queue.take_cancelled(), vec![id]);
        match queue.poll(PLUGIN, ticket).expect("poll") {
            ComponentPollOutcome::Ready(ComponentBridgeOutcome::Failed { class, .. }) => {
                assert_eq!(class, COMPONENT_CLASS_BUDGET);
            }
            other => panic!("expected a budget failure, got {other:?}"),
        }
    }

    /// Queue and bind one ticket to broker request `id`.
    fn bound_queue(id: u64) -> (ComponentRequestQueue, u64) {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        let ticket = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect("enqueue");
        queue.bind(
            ticket,
            PLUGIN,
            RequestId::for_test(id),
            now + Duration::from_secs(1),
        );
        (queue, ticket)
    }

    fn assert_header_budget_failure(
        mut queue: ComponentRequestQueue,
        ticket: u64,
        id: u64,
        expected_message: &str,
    ) {
        assert_eq!(queue.take_cancelled(), vec![RequestId::for_test(id)]);
        assert_eq!(queue.in_flight(), 0, "the refused request is cancelled");
        match queue.poll(PLUGIN, ticket).expect("poll") {
            ComponentPollOutcome::Ready(ComponentBridgeOutcome::Failed { class, message }) => {
                assert_eq!(class, COMPONENT_CLASS_BUDGET);
                assert!(
                    message.contains(expected_message),
                    "unexpected failure message: {message}"
                );
            }
            other => panic!("expected a budget failure, got {other:?}"),
        }
    }

    #[test]
    fn response_header_name_over_the_bridge_budget_fails_and_cancels() {
        let id = 21;
        let (mut queue, ticket) = bound_queue(id);
        queue.deliver(&event(
            id,
            BrokerEventKind::Head {
                status: 200,
                headers: vec![("n".repeat(COMPONENT_HEADER_MAX_BYTES + 1), "v".to_string())],
            },
        ));
        assert_header_budget_failure(queue, ticket, id, "header name/value exceeds");
    }

    #[test]
    fn response_header_value_over_the_bridge_budget_fails_and_cancels() {
        let id = 22;
        let (mut queue, ticket) = bound_queue(id);
        queue.deliver(&event(
            id,
            BrokerEventKind::Head {
                status: 200,
                headers: vec![("x".to_string(), "v".repeat(COMPONENT_HEADER_MAX_BYTES + 1))],
            },
        ));
        assert_header_budget_failure(queue, ticket, id, "header name/value exceeds");
    }

    #[test]
    fn response_header_count_over_the_bridge_budget_fails_and_cancels() {
        let id = 23;
        let (mut queue, ticket) = bound_queue(id);
        let headers = (0..=COMPONENT_HEADERS_MAX)
            .map(|index| (format!("x-{index}"), "v".to_string()))
            .collect();
        queue.deliver(&event(
            id,
            BrokerEventKind::Head {
                status: 200,
                headers,
            },
        ));
        assert_header_budget_failure(queue, ticket, id, "16-header bridge budget");
    }

    #[test]
    fn response_headers_at_the_bridge_budget_complete() {
        let id = 24;
        let (mut queue, ticket) = bound_queue(id);
        let headers: Vec<_> = (0..COMPONENT_HEADERS_MAX)
            .map(|index| (format!("x-{index}"), "v".repeat(COMPONENT_HEADER_MAX_BYTES)))
            .collect();
        queue.deliver(&event(
            id,
            BrokerEventKind::Head {
                status: 200,
                headers,
            },
        ));
        queue.deliver(&event(
            id,
            BrokerEventKind::Body {
                data: b"hello".to_vec(),
                last: true,
            },
        ));
        match queue.poll(PLUGIN, ticket).expect("poll") {
            ComponentPollOutcome::Ready(ComponentBridgeOutcome::Response { headers, .. }) => {
                assert_eq!(headers.len(), COMPONENT_HEADERS_MAX);
                assert!(
                    headers
                        .iter()
                        .all(|(_, value)| value.len() == COMPONENT_HEADER_MAX_BYTES)
                );
            }
            other => panic!("expected a bounded response, got {other:?}"),
        }
    }

    #[test]
    fn response_within_budget_completes() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        let ticket = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect("enqueue");
        queue.bind(
            ticket,
            PLUGIN,
            RequestId::for_test(3),
            now + Duration::from_secs(1),
        );
        queue.deliver(&event(
            3,
            BrokerEventKind::Head {
                status: 200,
                headers: vec![("x".to_string(), "y".to_string())],
            },
        ));
        queue.deliver(&event(
            3,
            BrokerEventKind::Body {
                data: b"hello".to_vec(),
                last: true,
            },
        ));
        match queue.poll(PLUGIN, ticket).expect("poll") {
            ComponentPollOutcome::Ready(ComponentBridgeOutcome::Response {
                status, body, ..
            }) => {
                assert_eq!(status, 200);
                assert_eq!(body, b"hello");
            }
            other => panic!("expected a response, got {other:?}"),
        }
        assert!(
            queue.poll(PLUGIN, ticket).is_err(),
            "a ticket is single-use"
        );
    }

    #[test]
    fn deadline_expiry_fails_timeout_and_cancels() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        let ticket = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect("enqueue");
        let id = RequestId::for_test(11);
        queue.bind(ticket, PLUGIN, id, now);
        assert_eq!(queue.expire(now + Duration::from_millis(1)), vec![id]);
        match queue.poll(PLUGIN, ticket).expect("poll") {
            ComponentPollOutcome::Ready(ComponentBridgeOutcome::Failed { class, .. }) => {
                assert_eq!(class, COMPONENT_CLASS_TIMEOUT);
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    #[test]
    fn a_closed_completion_window_reports_budget() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        // One ticket per plugin, so the per-plugin window does not interfere
        // with filling the completion window.
        let mut tickets = Vec::new();
        for index in 0..=COMPONENT_COMPLETION_CAPACITY {
            let plugin = format!("acme.p-{index}");
            let ticket = queue
                .enqueue(&plugin, HASH, request(None), now)
                .expect("enqueue");
            let _ = queue.take_submissions();
            queue.complete(
                &plugin,
                ticket,
                ComponentBridgeOutcome::Response {
                    status: 200,
                    headers: Vec::new(),
                    body: vec![b'x'],
                },
            );
            tickets.push((plugin, ticket));
        }
        // The oldest completion left the bounded window.
        let (oldest, ticket) = &tickets[0];
        let error = queue.poll(oldest, *ticket).expect_err("window closed");
        assert_eq!(error.code, "E_COMPONENT_BUDGET");
        let (newest, ticket) = &tickets[tickets.len() - 1];
        assert!(queue.poll(newest, *ticket).is_ok(), "newer tickets survive");
    }

    #[test]
    fn a_dropped_generation_purges_tickets_and_cancels_work() {
        let mut queue = ComponentRequestQueue::new();
        let now = Instant::now();
        let ticket = queue
            .enqueue(PLUGIN, HASH, request(None), now)
            .expect("enqueue");
        queue.bind(
            ticket,
            PLUGIN,
            RequestId::for_test(5),
            now + Duration::from_secs(1),
        );
        queue.drop_plugin(PLUGIN);
        assert!(queue.take_cancelled().contains(&RequestId::for_test(5)));
        let error = queue.poll(PLUGIN, ticket).expect_err("ticket purged");
        assert_eq!(error.code, "E_COMPONENT_TICKET");
        assert_eq!(queue.in_flight(), 0);
    }
}
