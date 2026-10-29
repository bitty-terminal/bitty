#![forbid(unsafe_code)]
//! Bounded Core attach/detach seam (bitty#2729, CTX-1655).
//!
//! Every test runs headlessly against a fake [`SessionHost`]. The fake proves
//! the seam's local contract: one binding per `(window, session)`, the
//! per-window cap, idempotent attach/detach, release on window close, session
//! termination, and daemon stop, per-lease event routing, no delivery after
//! detach, bounded queues, and no retained token or handle.

use bitty_runtime::attach::{
    AttachConfig, AttachError, AttachId, AttachRequest, AttachSeam, AttachStats, DrainOutcome,
    EventDelivery, EventDetail, EventKind, HostAttach, HostAttachGrant, HostDetach,
    HostDetachOutcome, HostRefusal, IdError, LeaseRole, MAX_BINDINGS_PER_WINDOW,
    MAX_QUEUED_EVENTS_PER_BINDING, ReleaseCause, ReleaseOutcome, SessionEvent, SessionHost,
    SessionId, SessionState, TerminalReason, WindowId,
};
use bitty_runtime::registry::PanelId;

const TOKEN: &str = "controller-token-for-tests";
const GRANT: &str = "observer-grant-for-tests";
const OTHER_TOKEN: &str = "rotated-controller-token-for-tests";

fn session(raw: u64) -> SessionId {
    SessionId::parse(&format!("{raw:032x}")).expect("test session id is canonical")
}

fn attach_id(raw: u64) -> AttachId {
    AttachId::parse(&format!("{raw:032x}")).expect("test attach id is canonical")
}

fn window(raw: u64) -> WindowId {
    WindowId::new(raw)
}

fn panel(raw: u64) -> PanelId {
    PanelId::new(raw)
}

fn controller(raw_session: u64, raw_window: u64, raw_panel: u64) -> AttachRequest<'static> {
    AttachRequest::controller(
        window(raw_window),
        panel(raw_panel),
        session(raw_session),
        1,
        TOKEN,
    )
}

fn observer(raw_session: u64, raw_window: u64, raw_panel: u64) -> AttachRequest<'static> {
    AttachRequest::observer(
        window(raw_window),
        panel(raw_panel),
        session(raw_session),
        GRANT,
    )
}

#[derive(Default)]
struct FakeHost {
    attach_calls: usize,
    detach_calls: Vec<(SessionId, AttachId)>,
    next_attach: u64,
    forced_attach_id: Option<AttachId>,
    attach_refusal: Option<HostRefusal>,
    detach_refusal: Option<HostRefusal>,
    detach_was_attached: Option<bool>,
    forced_role: Option<LeaseRole>,
    forced_generation: Option<u64>,
    last_expected_generation: Option<u64>,
    last_token_len: Option<usize>,
    last_grant_len: Option<usize>,
}

impl FakeHost {
    fn refusing(refusal: HostRefusal) -> Self {
        Self {
            attach_refusal: Some(refusal),
            ..Self::default()
        }
    }
}

impl SessionHost for FakeHost {
    fn attach(&mut self, request: HostAttach<'_>) -> Result<HostAttachGrant, HostRefusal> {
        self.attach_calls += 1;
        self.last_expected_generation = request.expected_generation;
        self.last_token_len = request.control_token.map(str::len);
        self.last_grant_len = request.observer_grant.map(str::len);
        if let Some(refusal) = self.attach_refusal {
            return Err(refusal);
        }
        self.next_attach += 1;
        Ok(HostAttachGrant {
            attach_id: self
                .forced_attach_id
                .unwrap_or_else(|| attach_id(self.next_attach)),
            role: self.forced_role.unwrap_or(request.role),
            generation: self
                .forced_generation
                .or(request.expected_generation)
                .unwrap_or(1),
        })
    }

    fn detach(
        &mut self,
        session: SessionId,
        attach_id: AttachId,
    ) -> Result<HostDetach, HostRefusal> {
        self.detach_calls.push((session, attach_id));
        if let Some(refusal) = self.detach_refusal {
            return Err(refusal);
        }
        Ok(HostDetach {
            was_attached: self.detach_was_attached.unwrap_or(true),
        })
    }
}

fn seam(max_bindings: usize) -> AttachSeam<FakeHost> {
    AttachSeam::new(
        FakeHost::default(),
        AttachConfig::with_bounds(max_bindings, 8),
    )
    .expect("test bounds are valid")
}

fn seam_with_queue(max_bindings: usize, queue: usize) -> AttachSeam<FakeHost> {
    AttachSeam::new(
        FakeHost::default(),
        AttachConfig::with_bounds(max_bindings, queue),
    )
    .expect("test bounds are valid")
}

fn event(
    raw_session: u64,
    raw_attach: u64,
    revision: u64,
    kind: EventKind,
    detail: EventDetail,
) -> SessionEvent {
    SessionEvent {
        session: session(raw_session),
        attach_id: attach_id(raw_attach),
        revision,
        kind,
        detail,
    }
}

fn state_event(raw_session: u64, raw_attach: u64, revision: u64) -> SessionEvent {
    event(
        raw_session,
        raw_attach,
        revision,
        EventKind::SessionState,
        EventDetail::None,
    )
}

fn terminal_event(raw_session: u64, raw_attach: u64, revision: u64) -> SessionEvent {
    event(
        raw_session,
        raw_attach,
        revision,
        EventKind::SessionTerminal,
        EventDetail::Terminal {
            state: SessionState::Terminated,
            reason: TerminalReason::DaemonStop,
        },
    )
}

fn released(outcome: ReleaseOutcome) -> bitty_runtime::attach::ReleaseReceipt {
    match outcome {
        ReleaseOutcome::Released(receipt) => receipt,
        ReleaseOutcome::NotAttached => panic!("expected a released binding"),
    }
}

#[test]
fn bind_release_lifecycle_is_headless_and_leak_free() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("first bind succeeds");

    assert_eq!(
        seam.stats(),
        AttachStats {
            bindings: 1,
            windows: 1,
            queued_events: 0,
            queued_event_bytes: 0,
        }
    );
    let binding = seam.binding(handle).expect("binding is live");
    assert_eq!(binding.handle, handle);
    assert_eq!(binding.session, session(1));
    assert_eq!(binding.panel, panel(1));
    assert_eq!(binding.role, LeaseRole::Controller);
    assert_eq!(binding.generation, 1);
    assert_eq!(binding.attach_id, attach_id(1));
    assert_eq!(seam.host().attach_calls, 1);
    assert_eq!(seam.host().last_expected_generation, Some(1));
    assert_eq!(seam.host().last_token_len, Some(TOKEN.len()));
    assert_eq!(seam.host().last_grant_len, None);

    let receipt = released(seam.release(handle));
    assert_eq!(receipt.cause, ReleaseCause::Requested);
    assert_eq!(receipt.binding.attach_id, attach_id(1));
    assert!(receipt.host.is_acknowledged());
    assert_eq!(receipt.discarded_events, 0);
    assert_eq!(seam.stats(), AttachStats::default());
    assert!(seam.binding(handle).is_none());
    assert_eq!(seam.host().detach_calls, vec![(session(1), attach_id(1))]);

    assert_eq!(seam.release(handle), ReleaseOutcome::NotAttached);
    assert_eq!(seam.host().detach_calls.len(), 1, "no duplicate detach");
    assert_eq!(seam.drain_events(handle), Err(AttachError::NotAttached));
    assert_eq!(seam.pending_events(handle), Err(AttachError::NotAttached));
}

#[test]
fn double_bind_returns_the_existing_handle_without_a_second_lease() {
    let mut seam = seam(4);
    let first = seam.bind(controller(1, 1, 1)).expect("first bind");
    let second = seam.bind(controller(1, 1, 1)).expect("idempotent bind");

    assert_eq!(first, second);
    assert_eq!(seam.host().attach_calls, 1, "one host lease only");
    assert_eq!(seam.stats().bindings, 1);

    let different_panel = seam.bind(controller(1, 1, 2));
    assert_eq!(
        different_panel,
        Err(AttachError::AlreadyAttached {
            role: LeaseRole::Controller,
            panel: panel(1),
        })
    );
    let different_role = seam.bind(observer(1, 1, 1));
    assert!(matches!(
        different_role,
        Err(AttachError::AlreadyAttached { .. })
    ));
    assert_eq!(seam.host().attach_calls, 1);
}

#[test]
fn window_limit_refuses_the_whole_bind_and_never_evicts() {
    let mut seam = seam(2);
    let first = seam.bind(controller(1, 1, 1)).expect("first bind");
    let second = seam.bind(controller(2, 1, 2)).expect("second bind");

    assert_eq!(
        seam.bind(controller(3, 1, 3)),
        Err(AttachError::WindowLimitReached { max: 2, current: 2 })
    );
    assert_eq!(seam.stats().bindings, 2);
    assert_eq!(
        seam.host().attach_calls,
        2,
        "refused bind never reached the host"
    );
    assert!(seam.binding(first).is_some());
    assert!(seam.binding(second).is_some());

    // Another window has its own budget.
    let other = seam.bind(controller(3, 2, 3)).expect("other window");
    assert_eq!(seam.stats().windows, 2);

    released(seam.release(first));
    let replacement = seam.bind(controller(3, 1, 3)).expect("released slot");
    assert!(seam.binding(replacement).is_some());
    assert!(seam.binding(other).is_some());
}

#[test]
fn a_zero_window_bound_disables_binding_without_evicting() {
    let mut seam = seam(0);
    assert_eq!(
        seam.bind(controller(1, 1, 1)),
        Err(AttachError::WindowLimitReached { max: 0, current: 0 })
    );
    assert_eq!(seam.host().attach_calls, 0);
    assert_eq!(seam.stats(), AttachStats::default());
}

#[test]
fn window_close_releases_only_that_window() {
    let mut seam = seam(4);
    let first = seam.bind(controller(1, 1, 1)).expect("window 1 bind");
    let second = seam.bind(controller(2, 2, 2)).expect("window 2 bind");

    let receipts = seam.on_window_closed(window(1));
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].cause, ReleaseCause::WindowClosed);
    assert_eq!(receipts[0].binding.handle, first);
    assert!(receipts[0].host.is_acknowledged());
    assert!(seam.binding(first).is_none());
    assert!(seam.binding(second).is_some());
    assert_eq!(seam.stats().bindings, 1);
    assert_eq!(seam.host().detach_calls.len(), 1);

    assert!(seam.on_window_closed(window(1)).is_empty());
    assert_eq!(seam.host().detach_calls.len(), 1);
}

#[test]
fn session_termination_releases_every_window_for_that_session() {
    let mut seam = seam(4);
    let first = seam.bind(controller(7, 1, 1)).expect("window 1 bind");
    let second = seam.bind(controller(7, 2, 2)).expect("window 2 bind");
    let other = seam.bind(controller(8, 3, 3)).expect("other session");

    let receipts = seam.on_session_terminated(session(7));
    assert_eq!(receipts.len(), 2);
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.cause == ReleaseCause::SessionTerminated)
    );
    assert_eq!(seam.stats().bindings, 1);
    assert!(seam.binding(first).is_none());
    assert!(seam.binding(second).is_none());
    assert!(seam.binding(other).is_some());

    assert!(seam.on_session_terminated(session(7)).is_empty());
}

#[test]
fn daemon_stop_releases_everything_and_refuses_until_started() {
    let mut seam = seam(4);
    let first = seam.bind(controller(1, 1, 1)).expect("first bind");
    let second = seam.bind(controller(2, 2, 2)).expect("second bind");

    let receipts = seam.on_daemon_stopped();
    assert_eq!(receipts.len(), 2);
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.cause == ReleaseCause::DaemonStopped)
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert!(!seam.daemon_available());
    assert!(seam.binding(first).is_none());
    assert!(seam.binding(second).is_none());
    assert_eq!(
        seam.bind(controller(3, 3, 3)),
        Err(AttachError::DaemonStopped)
    );
    assert_eq!(
        seam.host().attach_calls,
        2,
        "stopped daemon is never called"
    );

    assert!(seam.on_daemon_stopped().is_empty(), "stop is idempotent");
    seam.on_daemon_started();
    assert!(seam.daemon_available());
    assert!(seam.bind(controller(3, 3, 3)).is_ok());
}

#[test]
fn events_route_per_lease_and_never_after_detach() {
    let mut seam = seam(4);
    let first = seam.bind(controller(1, 1, 1)).expect("first bind");
    let second = seam.bind(controller(2, 1, 2)).expect("second bind");

    assert_eq!(
        seam.deliver_event(state_event(1, 1, 5)),
        EventDelivery::Queued {
            handle: first,
            queued_events: 1,
        }
    );
    assert_eq!(seam.pending_events(first), Ok(1));
    assert_eq!(seam.pending_events(second), Ok(0));

    // A lease-bound event naming a different session is never delivered.
    assert_eq!(
        seam.deliver_event(state_event(2, 1, 6)),
        EventDelivery::DroppedForeignSession
    );
    assert_eq!(seam.pending_events(first), Ok(1));

    // The other lease receives only its own copy.
    assert!(matches!(
        seam.deliver_event(state_event(2, 2, 7)),
        EventDelivery::Queued { handle, .. } if handle == second
    ));
    assert_eq!(seam.pending_events(second), Ok(1));

    let drained = seam.drain_events(first).expect("live binding drains");
    assert_eq!(drained.events.len(), 1);
    assert_eq!(drained.events[0].revision, 5);
    assert!(drained.released.is_none());

    released(seam.release(first));
    assert_eq!(
        seam.deliver_event(state_event(1, 1, 8)),
        EventDelivery::DroppedUnknownLease
    );
    assert_eq!(seam.drain_events(first), Err(AttachError::NotAttached));
    assert_eq!(seam.pending_events(second), Ok(1));
}

#[test]
fn duplicate_and_stale_revisions_are_dropped() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    assert!(matches!(
        seam.deliver_event(state_event(1, 1, 5)),
        EventDelivery::Queued { .. }
    ));
    assert_eq!(
        seam.deliver_event(state_event(1, 1, 5)),
        EventDelivery::DroppedStale
    );
    assert!(matches!(
        seam.deliver_event(event(
            1,
            1,
            5,
            EventKind::ControlTransfer,
            EventDetail::ControlTransfer { to: attach_id(1) },
        )),
        EventDelivery::Queued { .. }
    ));
    // A same-revision duplicate is still dropped after another kind at that
    // revision was delivered (contract dedupe is per kind).
    assert_eq!(
        seam.deliver_event(state_event(1, 1, 5)),
        EventDelivery::DroppedStale
    );
    assert_eq!(
        seam.deliver_event(state_event(1, 1, 4)),
        EventDelivery::DroppedStale
    );
    assert!(matches!(
        seam.deliver_event(state_event(1, 1, 6)),
        EventDelivery::Queued { .. }
    ));
    assert_eq!(seam.pending_events(handle), Ok(3));

    let drained = seam.drain_events(handle).expect("drain");
    assert_eq!(
        drained
            .events
            .iter()
            .map(|event| event.revision)
            .collect::<Vec<_>>(),
        vec![5, 5, 6]
    );
}

#[test]
fn terminal_event_is_delivered_once_then_releases_the_binding() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    assert!(matches!(
        seam.deliver_event(terminal_event(1, 1, 2)),
        EventDelivery::Queued { .. }
    ));
    assert_eq!(
        seam.deliver_event(state_event(1, 1, 3)),
        EventDelivery::DroppedClosing
    );

    let DrainOutcome { events, released } = seam.drain_events(handle).expect("drain");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::SessionTerminal);
    assert_eq!(
        events[0].detail,
        EventDetail::Terminal {
            state: SessionState::Terminated,
            reason: TerminalReason::DaemonStop,
        }
    );
    let receipt = released.expect("terminal drain releases the binding");
    assert_eq!(receipt.cause, ReleaseCause::SessionTerminated);
    assert_eq!(
        receipt.discarded_events, 0,
        "the terminal frame was delivered"
    );
    assert!(receipt.host.is_acknowledged());
    assert_eq!(seam.stats(), AttachStats::default());
    assert!(seam.binding(handle).is_none());
    assert_eq!(
        seam.deliver_event(terminal_event(1, 1, 2)),
        EventDelivery::DroppedUnknownLease
    );
}

#[test]
fn revocation_event_is_delivered_once_then_releases_the_binding() {
    let mut seam = seam(4);
    let handle = seam.bind(observer(1, 1, 1)).expect("bind");

    assert!(matches!(
        seam.deliver_event(event(1, 1, 2, EventKind::LeaseRevoked, EventDetail::None)),
        EventDelivery::Queued { .. }
    ));
    let DrainOutcome { events, released } = seam.drain_events(handle).expect("drain");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::LeaseRevoked);
    let receipt = released.expect("revocation drain releases");
    assert_eq!(receipt.cause, ReleaseCause::LeaseRevoked);
}

#[test]
fn daemon_stopping_event_is_delivered_once_then_releases_the_binding() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    assert!(matches!(
        seam.deliver_event(event(
            1,
            1,
            2,
            EventKind::DaemonStopping,
            EventDetail::DaemonStopping {
                deadline_ms: 15_000
            },
        )),
        EventDelivery::Queued { .. }
    ));
    let DrainOutcome { events, released } = seam.drain_events(handle).expect("drain");
    assert_eq!(events.len(), 1);
    let receipt = released.expect("drain start releases this lease");
    assert_eq!(receipt.cause, ReleaseCause::DaemonStopped);
}

#[test]
fn queue_overflow_closes_the_lease_without_shedding_the_frame() {
    let mut seam = seam_with_queue(4, 1);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    assert!(matches!(
        seam.deliver_event(state_event(1, 1, 1)),
        EventDelivery::Queued {
            queued_events: 1,
            ..
        }
    ));
    let delivery = seam.deliver_event(state_event(1, 1, 2));
    let EventDelivery::LeaseClosed {
        handle: closed,
        receipt,
    } = delivery
    else {
        panic!("overflow must close the lease");
    };
    assert_eq!(closed, handle);
    assert_eq!(receipt.cause, ReleaseCause::TerminalEventLost);
    assert_eq!(receipt.discarded_events, 1);
    assert_eq!(seam.stats(), AttachStats::default());
    assert_eq!(seam.drain_events(handle), Err(AttachError::NotAttached));
    assert_eq!(seam.host().detach_calls.len(), 1);
}

#[test]
fn malformed_role_requests_never_reach_the_host() {
    let mut seam = seam(4);
    let target = session(1);
    let mut request = AttachRequest::controller(window(1), panel(1), target, 1, TOKEN);
    request.control_token = None;
    assert_eq!(seam.bind(request), Err(AttachError::MissingControlToken));

    let mut request = AttachRequest::controller(window(1), panel(1), target, 1, TOKEN);
    request.observer_grant = Some(GRANT);
    assert_eq!(
        seam.bind(request),
        Err(AttachError::UnexpectedObserverGrant)
    );

    let mut request = AttachRequest::observer(window(1), panel(1), target, GRANT);
    request.observer_grant = None;
    assert_eq!(seam.bind(request), Err(AttachError::MissingObserverGrant));

    let mut request = AttachRequest::observer(window(1), panel(1), target, GRANT);
    request.control_token = Some(TOKEN);
    assert_eq!(seam.bind(request), Err(AttachError::UnexpectedControlToken));

    let mut request = AttachRequest::controller(window(1), panel(1), target, 1, TOKEN);
    request.expected_generation = None;
    assert_eq!(seam.bind(request), Err(AttachError::GenerationRequired));

    let request = AttachRequest::controller(window(1), panel(1), target, 1, "");
    assert_eq!(seam.bind(request), Err(AttachError::InvalidCredential));

    let long = "x".repeat(513);
    let request = AttachRequest::controller(window(1), panel(1), target, 1, &long);
    assert_eq!(seam.bind(request), Err(AttachError::InvalidCredential));

    assert_eq!(seam.host().attach_calls, 0);
    assert_eq!(seam.stats(), AttachStats::default());
}

#[test]
fn generation_mismatch_discards_the_host_lease() {
    let host = FakeHost {
        forced_generation: Some(2),
        forced_role: Some(LeaseRole::Controller),
        ..FakeHost::default()
    };
    let mut seam = AttachSeam::new(host, AttachConfig::with_bounds(4, 8)).expect("seam");
    assert_eq!(
        seam.bind(controller(1, 1, 1)),
        Err(AttachError::GenerationMismatch {
            expected: 1,
            found: 2,
        })
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert_eq!(seam.host().detach_calls, vec![(session(1), attach_id(1))]);
}

#[test]
fn role_mismatch_discards_the_host_lease() {
    let host = FakeHost {
        forced_role: Some(LeaseRole::Observer),
        ..FakeHost::default()
    };
    let mut seam = AttachSeam::new(host, AttachConfig::with_bounds(4, 8)).expect("seam");
    assert_eq!(
        seam.bind(controller(1, 1, 1)),
        Err(AttachError::RoleMismatch {
            requested: LeaseRole::Controller,
            granted: LeaseRole::Observer,
        })
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert_eq!(seam.host().detach_calls.len(), 1);
}

#[test]
fn duplicate_host_attach_id_never_detaches_the_existing_lease() {
    let host = FakeHost {
        forced_attach_id: Some(attach_id(1)),
        ..FakeHost::default()
    };
    let mut seam = AttachSeam::new(host, AttachConfig::with_bounds(4, 8)).expect("seam");
    let first = seam.bind(controller(1, 1, 1)).expect("first bind");
    assert_eq!(
        seam.binding(first).expect("first binding").attach_id,
        attach_id(1)
    );

    assert_eq!(
        seam.bind(controller(2, 1, 2)),
        Err(AttachError::DuplicateAttachId {
            attach_id: attach_id(1),
        })
    );
    assert!(
        seam.host().detach_calls.is_empty(),
        "the existing live lease must not be detached"
    );
    assert!(seam.binding(first).is_some());
    assert_eq!(seam.stats().bindings, 1);
}

#[test]
fn host_refusal_leaves_no_binding() {
    let mut seam = AttachSeam::new(
        FakeHost::refusing(HostRefusal::ControlHeld),
        AttachConfig::default(),
    )
    .expect("seam");
    assert_eq!(
        seam.bind(controller(1, 1, 1)),
        Err(AttachError::Refused(HostRefusal::ControlHeld))
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert_eq!(seam.host().detach_calls.len(), 0);
}

#[test]
fn host_detach_refusal_still_releases_the_local_binding() {
    let host = FakeHost {
        detach_refusal: Some(HostRefusal::Unavailable),
        ..FakeHost::default()
    };
    let mut seam = AttachSeam::new(host, AttachConfig::default()).expect("seam");
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    let receipt = released(seam.release(handle));
    assert_eq!(
        receipt.host,
        bitty_runtime::attach::HostDetachOutcome::Refused(HostRefusal::Unavailable)
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert!(seam.binding(handle).is_none());
}

#[test]
fn host_not_attached_detach_is_acknowledged_without_a_local_leak() {
    let host = FakeHost {
        detach_was_attached: Some(false),
        ..FakeHost::default()
    };
    let mut seam = AttachSeam::new(host, AttachConfig::default()).expect("seam");
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    let receipt = released(seam.release(handle));
    assert_eq!(
        receipt.host,
        HostDetachOutcome::Acknowledged {
            was_attached: false,
        }
    );
    assert_eq!(seam.stats(), AttachStats::default());
    assert!(seam.binding(handle).is_none());
}

#[test]
fn release_session_is_idempotent() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");
    let receipt = released(seam.release_session(window(1), session(1)));
    assert_eq!(receipt.binding.handle, handle);
    assert_eq!(
        seam.release_session(window(1), session(1)),
        ReleaseOutcome::NotAttached
    );
    assert_eq!(seam.host().detach_calls.len(), 1);
}

#[test]
fn invalid_configuration_is_refused() {
    let over_window = AttachSeam::new(
        FakeHost::default(),
        AttachConfig::with_bounds(MAX_BINDINGS_PER_WINDOW + 1, 8),
    );
    assert!(matches!(
        over_window,
        Err(AttachError::InvalidLimit {
            limit: "max_bindings_per_window",
            ..
        })
    ));

    let zero_queue = AttachSeam::new(FakeHost::default(), AttachConfig::with_bounds(4, 0));
    assert!(matches!(
        zero_queue,
        Err(AttachError::InvalidLimit {
            limit: "max_queued_events_per_binding",
            ..
        })
    ));

    let over_queue = AttachSeam::new(
        FakeHost::default(),
        AttachConfig::with_bounds(4, MAX_QUEUED_EVENTS_PER_BINDING + 1),
    );
    assert!(matches!(
        over_queue,
        Err(AttachError::InvalidLimit {
            limit: "max_queued_events_per_binding",
            ..
        })
    ));
}

#[test]
fn credentials_are_forwarded_but_never_retained_or_printed() {
    let mut seam = seam(4);
    let request = controller(1, 1, 1);
    let debug = format!("{request:?}");
    assert!(!debug.contains(TOKEN), "controller token must not print");
    assert!(debug.contains("<redacted>"));

    let handle = seam.bind(request).expect("bind");
    assert_eq!(seam.host().last_token_len, Some(TOKEN.len()));
    assert_eq!(seam.host().last_grant_len, None);

    let seam_debug = format!("{seam:?}");
    assert!(!seam_debug.contains(TOKEN));
    assert!(!seam_debug.contains(GRANT));
    let binding = seam.binding(handle).expect("binding");
    let binding_debug = format!("{binding:?}");
    assert!(!binding_debug.contains(TOKEN));
    assert!(!binding_debug.contains(GRANT));

    let observer_request = observer(2, 1, 2);
    let debug = format!("{observer_request:?}");
    assert!(!debug.contains(GRANT), "observer grant must not print");
    seam.bind(observer_request).expect("observer bind");
    assert_eq!(seam.host().last_grant_len, Some(GRANT.len()));
}

#[test]
fn malformed_events_are_dropped_without_delivery() {
    let mut seam = seam(4);
    let handle = seam.bind(controller(1, 1, 1)).expect("bind");

    // A terminal kind without a terminal detail is invalid.
    assert_eq!(
        seam.deliver_event(event(
            1,
            1,
            1,
            EventKind::SessionTerminal,
            EventDetail::None
        )),
        EventDelivery::DroppedInvalid
    );
    // A terminal detail whose reason contradicts the state is invalid.
    assert_eq!(
        seam.deliver_event(event(
            1,
            1,
            1,
            EventKind::SessionTerminal,
            EventDetail::Terminal {
                state: SessionState::Exited,
                reason: TerminalReason::DaemonStop,
            },
        )),
        EventDelivery::DroppedInvalid
    );
    assert_eq!(seam.pending_events(handle), Ok(0));
}

#[test]
fn identifier_parsing_rejects_non_canonical_values() {
    assert_eq!(SessionId::parse("abc"), Err(IdError::InvalidLength));
    assert_eq!(
        SessionId::parse("ABCDEFABCDEFABCDEFABCDEFABCDEFAB"),
        Err(IdError::InvalidCharacter)
    );
    let parsed = SessionId::parse("0000000000000000000000000000000f").expect("canonical");
    assert_eq!(parsed.as_hex(), "0000000000000000000000000000000f");
    assert_eq!(parsed.as_bytes()[15], 15);
}

#[test]
fn control_token_and_observer_grant_shapes_are_distinct() {
    // Controller: generation required, token required, grant forbidden.
    let request = AttachRequest::controller(window(1), panel(1), session(1), 1, TOKEN);
    assert_eq!(request.role, LeaseRole::Controller);
    assert_eq!(request.expected_generation, Some(1));
    assert_eq!(request.observer_grant, None);

    // Observer: grant required, token forbidden, no generation fence.
    let request = AttachRequest::observer(window(1), panel(1), session(1), GRANT);
    assert_eq!(request.role, LeaseRole::Observer);
    assert_eq!(request.expected_generation, None);
    assert_eq!(request.control_token, None);
}

#[test]
fn observer_attach_forwards_the_grant_and_not_a_token() {
    let mut seam = seam(4);
    let handle = seam.bind(observer(1, 1, 1)).expect("observer bind");
    assert_eq!(seam.host().last_grant_len, Some(GRANT.len()));
    assert_eq!(seam.host().last_token_len, None);
    assert_eq!(
        seam.binding(handle).expect("binding").role,
        LeaseRole::Observer
    );
}

#[test]
fn stale_generation_retry_is_not_treated_as_idempotent() {
    let mut seam = seam(4);
    let first = seam.bind(controller(1, 1, 1)).expect("bind");
    let retry = AttachRequest::controller(window(1), panel(1), session(1), 2, OTHER_TOKEN);
    assert_eq!(
        seam.bind(retry),
        Err(AttachError::AlreadyAttached {
            role: LeaseRole::Controller,
            panel: panel(1),
        })
    );
    assert_eq!(seam.stats().bindings, 1);
    assert!(seam.binding(first).is_some());
    assert_eq!(seam.host().attach_calls, 1);
}
