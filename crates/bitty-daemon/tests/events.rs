//! bitty#2602 / CTX-1621: public event-stream behavior.
//!
//! These tests use only the exported crate surface, so they also prove the
//! stream is usable by the future service layer (bitty#2607) and the attach
//! slice (bitty#2609) without reaching into internals. Time is injected
//! everywhere; nothing here sleeps, reads a wall clock, or depends on host
//! state.

use std::collections::BTreeMap;

use bitty_daemon::events::{
    AttachKey, AttachRequest, DetachCause, Event, EventDetail, EventKind, EventLimits,
    EventRefusal, LEASE_CONTROL_RESERVATION_BYTES, LEASE_CONTROL_RESERVATION_EVENTS,
    LeaseCloseReason, MAX_LEASE_QUEUE_BYTES, MAX_LEASE_QUEUE_EVENTS, SessionEventStream,
};
use bitty_daemon::registry::{
    AttachId, CleanupClass, CreateSession, ExitInformation, LifecycleState, MonotonicMs,
    RegistryLimits, RegistryRefusal, SessionId, SessionRegistry, SessionSnapshot, TerminalId,
    TerminalOutcome, TerminalReason,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn registry_limits() -> RegistryLimits {
    RegistryLimits::new(4, 4, 4096, 600_000).expect("valid registry limits")
}

fn event_limits(backlog_events: usize, attaches: usize, attach_keys: usize) -> EventLimits {
    EventLimits::new(
        backlog_events,
        262_144,
        256,
        65_536,
        attaches,
        15_000,
        attach_keys,
    )
    .expect("valid event limits")
}

fn new_stream(limits: EventLimits) -> SessionEventStream {
    SessionEventStream::with_limits(
        SessionRegistry::new(registry_limits()).expect("registry"),
        limits,
    )
}

fn create(stream: &mut SessionEventStream, terminal_id: &str, now: u64) -> SessionSnapshot {
    stream
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied(terminal_id).expect("valid terminal id")),
                owner_peer: None,
            },
            ms(now),
        )
        .expect("create")
}

fn attach(
    stream: &mut SessionEventStream,
    session_id: SessionId,
    after_revision: u64,
    key: Option<&str>,
    now: u64,
) -> bitty_daemon::events::AttachOutcome {
    stream
        .attach(
            AttachRequest {
                session_id,
                after_revision,
                idempotency_key: key
                    .map(|key| AttachKey::new(key.as_bytes()).expect("bounded key")),
            },
            ms(now),
        )
        .expect("attach")
}

fn terminate(
    stream: &mut SessionEventStream,
    session_id: SessionId,
    reason: TerminalReason,
    exit: ExitInformation,
    cleanup: CleanupClass,
    now: u64,
) -> bitty_daemon::events::TerminalReport {
    let outcome = TerminalOutcome::new(reason, exit, cleanup).expect("valid outcome");
    stream
        .terminate(session_id, outcome, ms(now))
        .expect("terminal transition")
}

fn revisions(events: &[Event]) -> Vec<u64> {
    events.iter().map(|event| event.revision).collect()
}

#[test]
fn revision_ordering_holds_under_interleaved_mutations_and_heartbeats() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-1", 1);
    assert_eq!(created.state_revision, 0);

    let first = attach(&mut stream, created.session_id, 0, None, 2);
    assert!(!first.replayed);
    assert_eq!(first.snapshot.state_revision, 1);
    assert_eq!(revisions(&first.replay.events), vec![1]);
    assert_eq!(first.replay.events[0].kind, EventKind::SessionState);
    assert_eq!(first.replay.events[0].attach_id, Some(first.attach_id));
    assert_eq!(first.replay.earliest_revision, Some(1));
    assert!(!first.replay.backlog_truncated);

    // A heartbeat refreshes last-seen and emits nothing.
    let heartbeat = stream
        .heartbeat(created.session_id, &[first.attach_id], ms(3))
        .expect("heartbeat");
    assert_eq!(heartbeat.server_monotonic_ms, ms(3));
    assert_eq!(heartbeat.leases[0].lease_expires_at_ms, ms(15_003));
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        1
    );

    let bound = stream.bind(created.session_id, ms(4)).expect("bind");
    assert_eq!(bound.snapshot.state_revision, 2);
    assert_eq!(bound.snapshot.generation, 1);
    assert_eq!(bound.event.revision, 2);

    let second = attach(&mut stream, created.session_id, 1, None, 5);
    assert_eq!(second.snapshot.state_revision, 3);
    assert_eq!(revisions(&second.replay.events), vec![2, 3]);

    let stopping = stream
        .begin_stopping(created.session_id, ms(6))
        .expect("stopping");
    assert_eq!(stopping.snapshot.state.as_str(), "stopping");
    assert_eq!(stopping.event.revision, 4);

    let terminal = terminate(
        &mut stream,
        created.session_id,
        TerminalReason::WorkerExit,
        ExitInformation::ExitCode(0),
        CleanupClass::Swept,
        7,
    );
    assert_eq!(terminal.snapshot.state, LifecycleState::Exited);
    assert_eq!(terminal.snapshot.state_revision, 5);
    assert_eq!(terminal.event.revision, 5);
    assert_eq!(terminal.event.kind, EventKind::SessionTerminal);
    let marker = terminal.event.terminal.expect("terminal marker");
    assert_eq!(marker.state, LifecycleState::Exited);
    assert_eq!(marker.reason, TerminalReason::WorkerExit);
    assert_eq!(marker.exit, ExitInformation::ExitCode(0));
    assert_eq!(marker.cleanup, CleanupClass::Swept);
    assert!(terminal.lost.is_empty());

    // Every lease saw a strictly increasing stream that agrees with the
    // registry revision, and the terminal event is the last one exactly once.
    for attach_id in [first.attach_id, second.attach_id] {
        let delivered = stream
            .take_deliveries(created.session_id, attach_id)
            .expect("drain");
        let seen = revisions(&delivered);
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "lease stream is not strictly increasing: {seen:?}"
        );
        assert!(
            seen.iter().all(|revision| *revision <= 5),
            "lease saw a revision ahead of the registry"
        );
        assert_eq!(
            seen.iter().filter(|revision| **revision == 5).count(),
            1,
            "terminal event must be delivered exactly once"
        );
        assert_eq!(
            delivered.last().map(|event| event.kind),
            Some(EventKind::SessionTerminal)
        );
        assert!(
            delivered
                .iter()
                .all(|event| event.attach_id == Some(attach_id))
        );
    }
}

#[test]
fn snapshot_at_revision_and_replay_from_an_older_revision() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-2", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    stream.bind(created.session_id, ms(3)).expect("bind");
    let second = attach(&mut stream, created.session_id, 2, None, 4);
    stream
        .begin_stopping(created.session_id, ms(5))
        .expect("stopping");

    let replay = stream
        .replay(created.session_id, 2)
        .expect("replay from an older revision");
    assert_eq!(replay.snapshot.state_revision, 4);
    assert_eq!(revisions(&replay.events), vec![3, 4]);
    assert!(replay.events.iter().all(|event| event.attach_id.is_none()));
    assert!(!replay.backlog_truncated);
    assert_eq!(replay.earliest_revision, Some(1));

    let current = stream
        .replay(created.session_id, 4)
        .expect("replay from the current revision");
    assert!(current.events.is_empty());
    assert!(!current.backlog_truncated);

    assert!(matches!(
        stream.replay(created.session_id, 5),
        Err(EventRefusal::RevisionAhead {
            requested: 5,
            current: 4
        })
    ));

    // A lease replay carries its own routing id.
    let for_lease = stream
        .replay_for_lease(created.session_id, second.attach_id, 3)
        .expect("lease replay");
    assert_eq!(revisions(&for_lease.events), vec![4]);
    assert_eq!(for_lease.events[0].attach_id, Some(second.attach_id));

    // The snapshot the replay carries is the current one, not the requested
    // revision, and it agrees with the registry.
    assert_eq!(
        replay.snapshot,
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("registry snapshot")
    );
    assert_ne!(first.attach_id, second.attach_id);
}

#[test]
fn backlog_truncation_boundaries_report_earliest_revision_accurately() {
    // One event per visible mutation, three retained events.
    let mut stream = new_stream(event_limits(3, 8, 8));
    let created = create(&mut stream, "term-3", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    assert_eq!(first.snapshot.state_revision, 1);

    // cap - 1 retained events.
    let detached = stream
        .detach(
            created.session_id,
            first.attach_id,
            DetachCause::SelfDetach,
            ms(3),
        )
        .expect("detach");
    assert_eq!(detached.event.revision, 2);
    assert_eq!(stream.backlog_len(created.session_id), Some(2));
    assert_eq!(
        stream.backlog_bytes(created.session_id),
        Some(2 * detached.event.accounted_bytes())
    );
    let replay = stream.replay(created.session_id, 0).expect("replay");
    assert_eq!(revisions(&replay.events), vec![1, 2]);
    assert!(!replay.backlog_truncated);
    assert_eq!(replay.earliest_revision, Some(1));

    // cap retained events.
    let third = attach(&mut stream, created.session_id, 2, None, 4);
    assert_eq!(third.snapshot.state_revision, 3);
    assert_eq!(stream.backlog_len(created.session_id), Some(3));
    let replay = stream.replay(created.session_id, 0).expect("replay");
    assert_eq!(revisions(&replay.events), vec![1, 2, 3]);
    assert!(!replay.backlog_truncated);
    assert_eq!(replay.earliest_revision, Some(1));

    // cap + 1: the oldest event is evicted whole.
    let fourth = stream
        .detach(
            created.session_id,
            third.attach_id,
            DetachCause::SelfDetach,
            ms(5),
        )
        .expect("detach");
    assert_eq!(fourth.event.revision, 4);
    assert_eq!(stream.backlog_len(created.session_id), Some(3));
    assert_eq!(
        stream.backlog_bytes(created.session_id),
        Some(3 * fourth.event.accounted_bytes())
    );

    let replay = stream.replay(created.session_id, 0).expect("replay");
    assert_eq!(revisions(&replay.events), vec![2, 3, 4]);
    assert!(replay.backlog_truncated, "a dropped event must be explicit");
    assert_eq!(replay.earliest_revision, Some(2));

    // A client already at revision 1 misses nothing.
    let caught_up = stream.replay(created.session_id, 1).expect("replay");
    assert_eq!(revisions(&caught_up.events), vec![2, 3, 4]);
    assert!(!caught_up.backlog_truncated);
    assert_eq!(caught_up.earliest_revision, Some(2));

    // A client at the current revision gets an empty, untruncated window.
    let current = stream.replay(created.session_id, 4).expect("replay");
    assert!(current.events.is_empty());
    assert!(!current.backlog_truncated);
}

#[test]
fn terminal_event_survives_trimming_and_is_delivered_once() {
    // Two retained events force trimming before the terminal transition.
    let mut stream = new_stream(event_limits(2, 8, 8));
    let created = create(&mut stream, "term-4", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    stream.bind(created.session_id, ms(3)).expect("bind");
    let second = attach(&mut stream, created.session_id, 2, None, 4);
    stream
        .begin_stopping(created.session_id, ms(5))
        .expect("stopping");
    let replay = stream.replay(created.session_id, 0).expect("replay");
    assert!(
        replay.backlog_truncated,
        "the ring must have trimmed before the terminal transition"
    );
    assert_eq!(replay.earliest_revision, Some(3));

    let terminal = terminate(
        &mut stream,
        created.session_id,
        TerminalReason::DaemonStop,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
        6,
    );
    assert_eq!(terminal.event.revision, 5);
    assert!(terminal.lost.is_empty());
    assert_eq!(
        stream.pending_terminal_consumers(created.session_id),
        Some(2)
    );
    // The ring is released at the terminal transition.
    assert_eq!(stream.backlog_len(created.session_id), Some(0));

    // Both leases get the terminal frame exactly once; the pinned slot serves a
    // recovery replay only while the frame is not queued.
    for attach_id in [first.attach_id, second.attach_id] {
        let queued = stream
            .replay_for_lease(created.session_id, attach_id, 0)
            .expect("lease replay");
        assert!(queued.terminal_event.is_none());
        assert!(queued.backlog_truncated);
        assert_eq!(queued.earliest_revision, Some(5));

        let delivered = stream
            .take_deliveries(created.session_id, attach_id)
            .expect("drain");
        assert_eq!(
            delivered
                .iter()
                .filter(|event| event.kind == EventKind::SessionTerminal)
                .count(),
            1
        );
        assert_eq!(
            delivered.last().map(|event| event.kind),
            Some(EventKind::SessionTerminal)
        );

        let recovered = stream
            .replay_for_lease(created.session_id, attach_id, 0)
            .expect("lease replay");
        assert_eq!(
            recovered.terminal_event.map(|event| event.revision),
            Some(5),
            "a drained but unacknowledged terminal event must be recoverable"
        );

        stream
            .ack_terminal(created.session_id, attach_id)
            .expect("acknowledge");
        assert!(stream.terminal_observed(created.session_id, attach_id));
        assert_eq!(
            stream
                .replay_for_lease(created.session_id, attach_id, 0)
                .expect("lease replay")
                .terminal_event,
            None
        );
        assert_eq!(
            stream.ack_terminal(created.session_id, attach_id),
            Err(EventRefusal::TerminalAlreadyObserved)
        );
    }
    assert_eq!(
        stream.pending_terminal_consumers(created.session_id),
        Some(0)
    );

    // The retained record still answers status until retention.
    let snapshot = stream
        .registry()
        .snapshot(created.session_id)
        .expect("retained");
    assert_eq!(snapshot.state, LifecycleState::Terminated);
    assert_eq!(snapshot.terminal_reason, Some(TerminalReason::DaemonStop));
}

#[test]
fn an_unacknowledged_terminal_event_is_pinned_only_until_retention() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-5", 1);
    attach(&mut stream, created.session_id, 0, None, 2);
    let terminal = terminate(
        &mut stream,
        created.session_id,
        TerminalReason::OperatorAbort,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
        3,
    );
    assert!(terminal.lost.is_empty());
    assert_eq!(
        stream.pending_terminal_consumers(created.session_id),
        Some(1)
    );

    assert!(stream.collect_retained(ms(3 + 600_000 - 1)).is_empty());
    assert_eq!(
        stream.pending_terminal_consumers(created.session_id),
        Some(1)
    );
    assert_eq!(
        stream.collect_retained(ms(3 + 600_000)),
        vec![created.session_id]
    );
    assert_eq!(stream.pending_terminal_consumers(created.session_id), None);
    assert_eq!(stream.session_count(), 0);
}

#[test]
fn a_stalled_consumer_is_closed_with_terminal_event_lost() {
    // Three queue slots. One lease fills its queue, the other keeps room.
    let mut stream =
        new_stream(EventLimits::new(256, 262_144, 3, 65_536, 8, 15_000, 8).expect("limits"));
    let created = create(&mut stream, "term-6", 1);
    let stalled = attach(&mut stream, created.session_id, 0, None, 2);
    let healthy = attach(&mut stream, created.session_id, 1, None, 3);
    stream.bind(created.session_id, ms(4)).expect("bind");
    stream
        .begin_stopping(created.session_id, ms(5))
        .expect("stopping");
    assert_eq!(
        stream.lease_queue_len(created.session_id, stalled.attach_id),
        Some(3),
        "the stalled lease has no room left"
    );
    assert_eq!(
        stream.lease_queue_len(created.session_id, healthy.attach_id),
        Some(2)
    );
    let _ = stream
        .take_deliveries(created.session_id, healthy.attach_id)
        .expect("drain the healthy lease");

    let terminal = terminate(
        &mut stream,
        created.session_id,
        TerminalReason::DaemonStop,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
        6,
    );
    assert_eq!(terminal.lost, vec![stalled.attach_id]);
    assert_eq!(
        terminal.snapshot.state_revision, 6,
        "the close adds one revision, then the terminal transition"
    );
    assert_eq!(stream.lease_count(created.session_id), Some(1));

    let delivered = stream
        .take_deliveries(created.session_id, healthy.attach_id)
        .expect("drain");
    assert_eq!(
        delivered
            .iter()
            .filter(|event| event.kind == EventKind::SessionTerminal)
            .count(),
        1
    );
    // The guarantee is visible, not silent: the close carries the contract name
    // and the terminal record reconciles status.
    assert_eq!(
        LeaseCloseReason::TerminalEventLost.as_str(),
        "terminal_event_lost"
    );
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("retained")
            .state,
        LifecycleState::Terminated
    );
    assert_eq!(
        stream.take_deliveries(created.session_id, stalled.attach_id),
        Err(EventRefusal::NotAttached)
    );
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .attach_count,
        1
    );
}

#[test]
fn a_refused_terminal_transition_does_not_close_a_lease() {
    // Two queue slots: the first lease is already full when the transition is
    // refused, so a pre-check that closed first would be visible.
    let mut stream =
        new_stream(EventLimits::new(256, 262_144, 2, 65_536, 8, 15_000, 8).expect("limits"));
    let created = create(&mut stream, "term-6b", 1);
    let stalled = attach(&mut stream, created.session_id, 0, None, 2);
    attach(&mut stream, created.session_id, 1, None, 3);
    stream.bind(created.session_id, ms(4)).expect("bind");
    assert_eq!(
        stream.lease_queue_len(created.session_id, stalled.attach_id),
        Some(2),
        "the stalled lease has no room left"
    );
    let revision = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;

    // `spawn_failed` can only originate in `created`; a `running` session must
    // be refused without touching the stream.
    let outcome = TerminalOutcome::new(
        TerminalReason::SpawnFailed,
        ExitInformation::NotApplicable,
        CleanupClass::NotStarted,
    )
    .expect("valid outcome");
    assert!(matches!(
        stream.terminate(created.session_id, outcome, ms(5)),
        Err(EventRefusal::Registry(RegistryRefusal::ReasonNotAllowed {
            reason: TerminalReason::SpawnFailed,
            from: LifecycleState::Running,
        }))
    ));
    assert_eq!(stream.lease_count(created.session_id), Some(2));
    assert_eq!(
        stream.lease_queue_len(created.session_id, stalled.attach_id),
        Some(2),
        "a refused transition must not have closed or drained a lease"
    );
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision
    );
}

#[test]
fn the_control_reservation_is_compiled_and_leaves_room_for_control_frames() {
    const {
        assert!(LEASE_CONTROL_RESERVATION_EVENTS < MAX_LEASE_QUEUE_EVENTS);
        assert!(LEASE_CONTROL_RESERVATION_BYTES < MAX_LEASE_QUEUE_BYTES);
    }
    let defaults = EventLimits::default();
    assert_eq!(
        defaults.observability_queue_events(),
        MAX_LEASE_QUEUE_EVENTS - LEASE_CONTROL_RESERVATION_EVENTS
    );
    assert_eq!(
        defaults.observability_queue_bytes(),
        MAX_LEASE_QUEUE_BYTES - LEASE_CONTROL_RESERVATION_BYTES
    );
}

#[test]
fn consumer_disconnect_and_reconnect_replays_the_gap() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-8", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    stream.bind(created.session_id, ms(3)).expect("bind");
    let _ = stream
        .take_deliveries(created.session_id, first.attach_id)
        .expect("drain");
    let seen = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;
    assert_eq!(seen, 2);

    // The transport drops: the lease is closed and the detach is one visible
    // mutation.
    let closed = stream
        .close_lease(created.session_id, first.attach_id, ms(4))
        .expect("close");
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].reason, LeaseCloseReason::TerminalEventLost);
    assert_eq!(stream.lease_count(created.session_id), Some(0));
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .attach_count,
        0
    );

    let late = attach(&mut stream, created.session_id, seen, None, 5);
    assert_eq!(revisions(&late.replay.events), vec![3, 4]);
    assert_eq!(late.replay.snapshot.state_revision, 4);
    assert!(!late.replay.backlog_truncated);

    // A replayed attach key whose lease is gone is not a second lease (`D-18`).
    let mut with_key = new_stream(event_limits(16, 8, 8));
    let keyed_session = create(&mut with_key, "term-9", 1);
    let keyed = attach(&mut with_key, keyed_session.session_id, 0, Some("key-1"), 2);
    let replay = attach(&mut with_key, keyed_session.session_id, 0, Some("key-1"), 3);
    assert!(replay.replayed);
    assert_eq!(replay.attach_id, keyed.attach_id);
    assert_eq!(replay.snapshot.state_revision, 1);
    assert_eq!(with_key.lease_count(keyed_session.session_id), Some(1));
    with_key
        .detach(
            keyed_session.session_id,
            keyed.attach_id,
            DetachCause::SelfDetach,
            ms(4),
        )
        .expect("detach");
    assert!(matches!(
        with_key.attach(
            AttachRequest {
                session_id: keyed_session.session_id,
                after_revision: 0,
                idempotency_key: Some(AttachKey::new(b"key-1").expect("key")),
            },
            ms(5)
        ),
        Err(EventRefusal::NotAttached)
    ));
}

#[test]
fn idempotent_attach_replay_does_not_duplicate_or_reorder_events() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-10", 1);
    let first = attach(&mut stream, created.session_id, 0, Some("attach-a"), 2);
    stream.bind(created.session_id, ms(3)).expect("bind");
    let before = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;
    let keys_before = stream.attach_key_count(created.session_id);

    let second = attach(&mut stream, created.session_id, 0, Some("attach-a"), 4);
    assert!(second.replayed);
    assert_eq!(second.attach_id, first.attach_id);
    assert_eq!(stream.lease_count(created.session_id), Some(1));
    assert_eq!(stream.attach_key_count(created.session_id), keys_before);
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        before,
        "a replayed attach must not bump the revision"
    );
    assert_eq!(revisions(&second.replay.events), vec![1, 2]);
    assert!(
        second
            .replay
            .events
            .windows(2)
            .all(|pair| pair[0].revision < pair[1].revision)
    );
    // The same key with a different replay window is a conflict, not a new
    // lease (`D-18`).
    assert!(matches!(
        stream.attach(
            AttachRequest {
                session_id: created.session_id,
                after_revision: 1,
                idempotency_key: Some(AttachKey::new(b"attach-a").expect("key")),
            },
            ms(5)
        ),
        Err(EventRefusal::IdempotencyConflict)
    ));

    // The store is bounded, and a new key at capacity is refused whole.
    let mut capped = new_stream(event_limits(16, 8, 1));
    let capped_session = create(&mut capped, "term-11", 1);
    attach(&mut capped, capped_session.session_id, 0, Some("k1"), 2);
    assert!(matches!(
        capped.attach(
            AttachRequest {
                session_id: capped_session.session_id,
                after_revision: 0,
                idempotency_key: Some(AttachKey::new(b"k2").expect("key")),
            },
            ms(3)
        ),
        Err(EventRefusal::IdempotencyCapacity { limit: 1 })
    ));
    assert_eq!(capped.lease_count(capped_session.session_id), Some(1));
}

#[test]
fn heartbeat_does_not_bump_revision_or_grow_memory() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-12", 1);
    let lease = attach(&mut stream, created.session_id, 0, None, 2);
    stream.bind(created.session_id, ms(3)).expect("bind");
    let revision = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;
    let bytes = stream
        .accounted_bytes(created.session_id)
        .expect("accounted");
    let queue = stream
        .lease_queue_bytes(created.session_id, lease.attach_id)
        .expect("queue");

    for tick in 0..10_000u64 {
        let now = 10 + tick;
        let report = stream
            .heartbeat(created.session_id, &[lease.attach_id], ms(now))
            .expect("heartbeat");
        assert_eq!(report.server_monotonic_ms, ms(now));
        assert_eq!(report.leases[0].lease_expires_at_ms, ms(now + 15_000));
    }

    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision,
        "heartbeat must not bump the revision"
    );
    assert_eq!(
        stream
            .accounted_bytes(created.session_id)
            .expect("accounted"),
        bytes,
        "heartbeat must not grow stream memory"
    );
    assert_eq!(
        stream
            .lease_queue_bytes(created.session_id, lease.attach_id)
            .expect("queue"),
        queue
    );
    assert_eq!(stream.lease_count(created.session_id), Some(1));

    // A heartbeat for a lease that is not on this session is NotFound and never
    // refreshes it.
    let foreign = AttachId::parse("cccccccccccccccccccccccccccccccc").expect("canonical");
    assert_eq!(
        stream.heartbeat(created.session_id, &[foreign], ms(20_000)),
        Err(EventRefusal::NotAttached)
    );
    assert_eq!(
        stream.heartbeat(created.session_id, &[], ms(20_000)),
        Err(EventRefusal::EmptyHeartbeat)
    );

    // Silence detaches only: one revision bump, no terminal reason, no abort.
    let expiry = stream
        .expire_leases(created.session_id, ms(10 + 9_999 + 15_000))
        .expect("expire");
    assert_eq!(expiry.expired, vec![lease.attach_id]);
    assert_eq!(expiry.events.len(), 1);
    assert_eq!(expiry.events[0].revision, revision + 1);
    let snapshot = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot");
    assert_eq!(snapshot.state, LifecycleState::Running);
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.terminal_reason, None);
    assert_eq!(snapshot.attach_count, 0);
}

#[test]
fn expiry_detaches_every_silent_lease_exactly_once() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-12b", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    let second = attach(&mut stream, created.session_id, 1, None, 3);
    stream.bind(created.session_id, ms(4)).expect("bind");
    let _ = stream
        .take_deliveries(created.session_id, first.attach_id)
        .expect("drain");
    let _ = stream
        .take_deliveries(created.session_id, second.attach_id)
        .expect("drain");
    let revision = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;

    // One silent lease is refreshed, the other expires.
    stream
        .heartbeat(created.session_id, &[second.attach_id], ms(20_000))
        .expect("heartbeat");
    let first_expiry = stream
        .expire_leases(created.session_id, ms(4 + 15_000))
        .expect("expire");
    assert_eq!(first_expiry.expired, vec![first.attach_id]);
    assert_eq!(first_expiry.events.len(), 1);
    assert_eq!(first_expiry.events[0].revision, revision + 1);
    assert_eq!(stream.lease_count(created.session_id), Some(1));

    // The repeated call is idempotent: nothing is detached twice.
    let repeated = stream
        .expire_leases(created.session_id, ms(4 + 15_000))
        .expect("expire");
    assert!(repeated.expired.is_empty());
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision + 1
    );

    // Then the refreshed lease expires at its own TTL.
    let second_expiry = stream
        .expire_leases(created.session_id, ms(20_000 + 15_000))
        .expect("expire");
    assert_eq!(second_expiry.expired, vec![second.attach_id]);
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision + 2
    );
    assert_eq!(stream.lease_count(created.session_id), Some(0));
}

#[test]
fn control_transfer_and_daemon_stopping_are_control_frames() {
    let mut stream = new_stream(EventLimits::default());
    let created = create(&mut stream, "term-13", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    let second = attach(&mut stream, created.session_id, 1, None, 3);
    let _ = stream
        .take_deliveries(created.session_id, first.attach_id)
        .expect("drain");
    let _ = stream
        .take_deliveries(created.session_id, second.attach_id)
        .expect("drain");

    let transfer = stream
        .publish_control_transfer(created.session_id, first.attach_id, second.attach_id, ms(4))
        .expect("control transfer");
    assert_eq!(transfer.event.kind, EventKind::ControlTransfer);
    assert_eq!(
        transfer.event.detail,
        EventDetail::ControlTransfer {
            from: first.attach_id,
            to: second.attach_id
        }
    );
    assert_eq!(transfer.snapshot.state_revision, 3);
    for attach_id in [first.attach_id, second.attach_id] {
        let delivered = stream
            .take_deliveries(created.session_id, attach_id)
            .expect("drain");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].kind, EventKind::ControlTransfer);
        assert_eq!(delivered[0].revision, 3);
    }
    assert!(matches!(
        stream.publish_control_transfer(
            created.session_id,
            first.attach_id,
            first.attach_id,
            ms(5)
        ),
        Err(EventRefusal::InvalidRequest { .. })
    ));

    let stopping = stream.publish_daemon_stopping(60_000, ms(6));
    assert_eq!(stopping.sessions, 1);
    assert_eq!(stopping.delivered, 2);
    assert!(stopping.closed.is_empty());
    for attach_id in [first.attach_id, second.attach_id] {
        let delivered = stream
            .take_deliveries(created.session_id, attach_id)
            .expect("drain");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].kind, EventKind::DaemonStopping);
        assert_eq!(
            delivered[0].detail,
            EventDetail::DaemonStopping {
                deadline_ms: 60_000
            }
        );
        assert_eq!(
            delivered[0].revision, 3,
            "a daemon frame never bumps a revision"
        );
    }
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        3
    );

    // A revoked lease receives the typed frame and the remaining leases see the
    // one visible mutation.
    let revoked = stream
        .detach(
            created.session_id,
            second.attach_id,
            DetachCause::Revoked,
            ms(7),
        )
        .expect("revoke");
    assert_eq!(revoked.event.kind, EventKind::LeaseRevoked);
    assert_eq!(
        revoked
            .final_frame
            .map(|frame| (frame.kind, frame.attach_id)),
        Some((EventKind::LeaseRevoked, Some(second.attach_id)))
    );
    let delivered = stream
        .take_deliveries(created.session_id, first.attach_id)
        .expect("drain");
    assert_eq!(revisions(&delivered), vec![4]);
    assert_eq!(delivered[0].kind, EventKind::LeaseRevoked);
    assert_eq!(
        stream.take_deliveries(created.session_id, second.attach_id),
        Err(EventRefusal::NotAttached)
    );
}

#[test]
fn bounds_refuse_whole_and_leave_the_stream_unchanged() {
    let mut stream = new_stream(event_limits(16, 2, 8));
    let created = create(&mut stream, "term-14", 1);
    let first = attach(&mut stream, created.session_id, 0, None, 2);
    let second = attach(&mut stream, created.session_id, 1, None, 3);
    let revision = stream
        .registry()
        .snapshot(created.session_id)
        .expect("snapshot")
        .state_revision;
    assert!(matches!(
        stream.attach(
            AttachRequest {
                session_id: created.session_id,
                after_revision: revision,
                idempotency_key: None,
            },
            ms(4)
        ),
        Err(EventRefusal::MaxAttaches { limit: 2 })
    ));
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision
    );

    let unknown = SessionId::parse("dddddddddddddddddddddddddddddddd").expect("canonical");
    assert!(matches!(
        stream.replay(unknown, 0),
        Err(EventRefusal::UnknownSession)
    ));
    assert!(matches!(
        stream.bind(unknown, ms(5)),
        Err(EventRefusal::Registry(RegistryRefusal::NotFound))
    ));

    // A heartbeat may not revive a terminal session.
    let terminal = terminate(
        &mut stream,
        created.session_id,
        TerminalReason::ConsumerTimeout,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
        6,
    );
    assert_eq!(terminal.event.revision, revision + 1);
    assert!(matches!(
        stream.heartbeat(created.session_id, &[first.attach_id], ms(7)),
        Err(EventRefusal::Registry(RegistryRefusal::NotRunning { .. }))
    ));
    // A retained terminal lease is closed through the stream, not the registry.
    assert_eq!(
        stream
            .close_lease(created.session_id, second.attach_id, ms(8))
            .expect("close retained lease")[0]
            .attach_id,
        second.attach_id
    );
    assert_eq!(stream.lease_count(created.session_id), Some(1));
    assert_eq!(
        stream.pending_terminal_consumers(created.session_id),
        Some(1),
        "the typed close discharges that lease's terminal guarantee"
    );
    assert_eq!(
        stream
            .registry()
            .snapshot(created.session_id)
            .expect("snapshot")
            .state_revision,
        revision + 1,
        "closing a retained lease must not bump the revision"
    );
}

#[test]
fn limits_are_validated_against_the_compiled_maxima() {
    assert!(matches!(
        EventLimits::new(257, 262_144, 256, 65_536, 8, 15_000, 512),
        Err(EventRefusal::InvalidLimits {
            limit: "max_event_backlog_events",
            value: 257,
            minimum: 1,
            maximum: 256
        })
    ));
    assert!(matches!(
        EventLimits::new(256, 262_144, 256, 65_536, 17, 15_000, 512),
        Err(EventRefusal::InvalidLimits {
            limit: "max_attaches_per_session",
            ..
        })
    ));
    assert!(matches!(
        EventLimits::new(256, 262_144, 256, 65_536, 8, 15_001, 512),
        Err(EventRefusal::InvalidLimits {
            limit: "attach_lease_ttl_ms",
            ..
        })
    ));
    assert!(matches!(
        EventLimits::new(256, 262_144, 256, 65_536, 8, 15_000, 0),
        Err(EventRefusal::InvalidLimits {
            limit: "max_attach_keys_per_session",
            ..
        })
    ));
    assert!(matches!(
        EventLimits::new(0, 262_144, 256, 65_536, 8, 15_000, 512),
        Err(EventRefusal::InvalidLimits {
            limit: "max_event_backlog_events",
            ..
        })
    ));
    let defaults = EventLimits::default();
    assert_eq!(defaults.max_event_backlog_events(), 256);
    assert_eq!(defaults.max_event_backlog_bytes(), 262_144);
    assert_eq!(defaults.max_lease_queue_events(), 256);
    assert_eq!(defaults.max_lease_queue_bytes(), 65_536);
    assert_eq!(defaults.max_attaches_per_session(), 8);
    assert_eq!(defaults.attach_lease_ttl_ms(), 15_000);
    assert_eq!(defaults.max_attach_keys_per_session(), 512);
}

/// Deterministic, seeded LCG so a failure reproduces from the seed alone.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            usize::try_from(self.next() % u64::try_from(bound).expect("small")).expect("bounded")
        }
    }
}

/// One session's expected state in the randomized audit.
struct Model {
    revision: u64,
    state: LifecycleState,
    generation: u64,
    leases: BTreeMap<AttachId, Vec<Event>>,
    terminal: bool,
}

impl Model {
    fn new() -> Self {
        Self {
            revision: 0,
            state: LifecycleState::Created,
            generation: 0,
            leases: BTreeMap::new(),
            terminal: false,
        }
    }
}

#[test]
fn no_event_loss_or_duplication_across_ten_thousand_randomized_operations() {
    let seed = 0x5eed_1621_1619_2602u64;
    let mut rng = Lcg(seed);
    let limits = event_limits(64, 4, 8);
    let mut stream = new_stream(limits);
    let mut models: BTreeMap<SessionId, Model> = BTreeMap::new();
    let mut next_terminal_id = 0usize;
    let mut now = 1u64;
    let mut attachments = 0usize;
    let mut terminals = 0usize;
    let mut replays = 0usize;

    for step in 0..10_000u64 {
        now += 1;
        if models.is_empty() {
            next_terminal_id += 1;
            let snapshot = create(&mut stream, &format!("term-r{next_terminal_id}"), now);
            models.insert(snapshot.session_id, Model::new());
        }
        let session_id = {
            let keys: Vec<SessionId> = models.keys().copied().collect();
            keys[rng.pick(keys.len())]
        };

        // Expiry always runs first, exactly as a timer-driven service would.
        let expiry = stream.expire_leases(session_id, ms(now)).expect("expire");
        if !expiry.expired.is_empty() {
            let model = models.get_mut(&session_id).expect("model");
            for attach_id in &expiry.expired {
                model.leases.remove(attach_id);
            }
            for event in &expiry.events {
                model.revision = event.revision;
                for (attach_id, expected) in model.leases.iter_mut() {
                    expected.push(event.routed_to(*attach_id));
                }
            }
        }

        match rng.next() % 12 {
            0 | 1 => {
                // Heartbeat every lease of the session.
                let ids: Vec<AttachId> = models[&session_id].leases.keys().copied().collect();
                if !ids.is_empty() && !models[&session_id].terminal {
                    let before = models[&session_id].revision;
                    stream
                        .heartbeat(session_id, &ids, ms(now))
                        .expect("heartbeat");
                    assert_eq!(models[&session_id].revision, before);
                }
            }
            2 | 3 => {
                // Attach, at or below the current revision.
                let before = models[&session_id].revision;
                if models[&session_id].terminal {
                    continue;
                }
                let after = if before == 0 {
                    0
                } else {
                    u64::try_from(rng.pick(usize::try_from(before).expect("small") + 1))
                        .expect("bounded")
                };
                match stream.attach(
                    AttachRequest {
                        session_id,
                        after_revision: after,
                        idempotency_key: None,
                    },
                    ms(now),
                ) {
                    Ok(outcome) => {
                        attachments += 1;
                        assert!(!outcome.replayed);
                        assert_eq!(outcome.snapshot.state_revision, before + 1);
                        let attach_event = outcome
                            .replay
                            .events
                            .iter()
                            .find(|event| event.revision == before + 1)
                            .expect("the attach event is in every replay window")
                            .clone();
                        let model = models.get_mut(&session_id).expect("model");
                        model.revision += 1;
                        for (attach_id, expected) in model.leases.iter_mut() {
                            expected.push(attach_event.routed_to(*attach_id));
                        }
                        // The replay is handed over synchronously, so the new
                        // lease has no queued backlog.
                        model.leases.insert(outcome.attach_id, Vec::new());
                        assert!(
                            revisions(&outcome.replay.events)
                                .windows(2)
                                .all(|pair| pair[0] < pair[1])
                        );
                    }
                    Err(EventRefusal::MaxAttaches { limit }) => {
                        assert_eq!(limit, 4);
                        assert_eq!(models[&session_id].leases.len(), 4);
                    }
                    Err(other) => panic!("unexpected attach refusal at step {step}: {other}"),
                }
            }
            4 | 5 => {
                // Self-detach a random lease.
                let model = models.get(&session_id).expect("model");
                if model.terminal || model.leases.is_empty() {
                    continue;
                }
                let ids: Vec<AttachId> = model.leases.keys().copied().collect();
                let attach_id = ids[rng.pick(ids.len())];
                let outcome = stream
                    .detach(session_id, attach_id, DetachCause::SelfDetach, ms(now))
                    .expect("detach");
                let model = models.get_mut(&session_id).expect("model");
                assert_eq!(outcome.event.revision, model.revision + 1);
                model.revision = outcome.event.revision;
                model.leases.remove(&attach_id);
                for (attach_id, expected) in model.leases.iter_mut() {
                    expected.push(outcome.event.routed_to(*attach_id));
                }
                assert!(outcome.closed.is_empty(), "queues are drained every step");
            }
            6 => {
                // Bind a created session.
                if models[&session_id].state == LifecycleState::Created {
                    let report = stream.bind(session_id, ms(now)).expect("bind");
                    let model = models.get_mut(&session_id).expect("model");
                    assert_eq!(report.event.revision, model.revision + 1);
                    model.revision = report.event.revision;
                    model.state = LifecycleState::Running;
                    model.generation += 1;
                    for (attach_id, expected) in model.leases.iter_mut() {
                        expected.push(report.event.routed_to(*attach_id));
                    }
                }
            }
            7 => {
                // Graceful stop.
                if models[&session_id].state == LifecycleState::Running {
                    let report = stream
                        .begin_stopping(session_id, ms(now))
                        .expect("stopping");
                    let model = models.get_mut(&session_id).expect("model");
                    assert_eq!(report.event.revision, model.revision + 1);
                    model.revision = report.event.revision;
                    model.state = LifecycleState::Stopping;
                    for (attach_id, expected) in model.leases.iter_mut() {
                        expected.push(report.event.routed_to(*attach_id));
                    }
                }
            }
            8 => {
                // Control transfer between two attached leases.
                let model = models.get(&session_id).expect("model");
                if model.leases.len() >= 2 {
                    let ids: Vec<AttachId> = model.leases.keys().copied().collect();
                    let from = ids[0];
                    let to = ids[1];
                    let report = stream
                        .publish_control_transfer(session_id, from, to, ms(now))
                        .expect("transfer");
                    let model = models.get_mut(&session_id).expect("model");
                    assert_eq!(report.event.revision, model.revision + 1);
                    model.revision = report.event.revision;
                    for (attach_id, expected) in model.leases.iter_mut() {
                        expected.push(report.event.routed_to(*attach_id));
                    }
                }
            }
            9 => {
                // Daemon-scoped frame: no revision change, delivered to every
                // live lease of every session.
                let deadline = now + 60_000;
                let live: Vec<SessionId> = models
                    .iter()
                    .filter(|(_, model)| !model.terminal && !model.leases.is_empty())
                    .map(|(session_id, _)| *session_id)
                    .collect();
                let report = stream.publish_daemon_stopping(deadline, ms(now));
                assert_eq!(report.sessions, live.len());
                assert!(report.closed.is_empty());
                for session_id in live {
                    let model = models.get_mut(&session_id).expect("model");
                    let revision = model.revision;
                    for (attach_id, expected) in model.leases.iter_mut() {
                        expected.push(Event {
                            instance_id: stream.registry().instance_id(),
                            session_id,
                            attach_id: Some(*attach_id),
                            revision,
                            kind: EventKind::DaemonStopping,
                            terminal: None,
                            detail: EventDetail::DaemonStopping {
                                deadline_ms: deadline,
                            },
                            emitted_at: ms(now),
                        });
                    }
                }
            }
            _ => {
                // Terminate, drain, acknowledge, collect.
                if models[&session_id].terminal {
                    continue;
                }
                let state = models[&session_id].state;
                let reason = match state {
                    LifecycleState::Created => TerminalReason::SpawnFailed,
                    LifecycleState::Running => TerminalReason::WorkerExit,
                    _ => TerminalReason::DaemonStop,
                };
                let exit = match reason {
                    TerminalReason::WorkerExit => ExitInformation::ExitCode(0),
                    _ => ExitInformation::NotApplicable,
                };
                let cleanup = match state {
                    LifecycleState::Created => CleanupClass::NotStarted,
                    _ => CleanupClass::Swept,
                };
                let report = terminate(&mut stream, session_id, reason, exit, cleanup, now);
                terminals += 1;
                assert!(report.lost.is_empty());
                let model = models.get_mut(&session_id).expect("model");
                assert_eq!(report.event.revision, model.revision + 1);
                model.revision = report.event.revision;
                model.state = report.snapshot.state;
                model.terminal = true;
                for (attach_id, expected) in model.leases.iter_mut() {
                    expected.push(report.event.routed_to(*attach_id));
                }
            }
        }

        // Audit the replay invariants at the current revision.
        let model = models.get(&session_id).expect("model");
        let after = if model.revision == 0 {
            0
        } else {
            u64::try_from(rng.pick(usize::try_from(model.revision).expect("small") + 1))
                .expect("bounded")
        };
        let replay = stream.replay(session_id, after).expect("replay");
        replays += 1;
        assert_eq!(replay.snapshot.state_revision, model.revision);
        assert!(
            revisions(&replay.events)
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "replay is not strictly increasing at step {step}"
        );
        assert!(
            replay
                .events
                .iter()
                .all(|event| event.revision <= model.revision),
            "replay served a revision ahead of the registry at step {step}"
        );
        if !replay.backlog_truncated {
            let expected: Vec<u64> = if after == model.revision {
                Vec::new()
            } else {
                ((after + 1)..=model.revision).collect()
            };
            assert_eq!(
                revisions(&replay.events),
                expected,
                "an untruncated replay must be contiguous at step {step}"
            );
        } else if !replay.events.is_empty() {
            let earliest = replay
                .earliest_revision
                .expect("truncation names a revision");
            assert!(
                earliest > after + 1,
                "a truncated replay must start after the requested revision at step {step}"
            );
            assert_eq!(replay.events[0].revision, earliest);
        }

        // Drain every lease and compare against the model exactly: no loss, no
        // duplication, no reordering.
        let ids: Vec<AttachId> = models[&session_id].leases.keys().copied().collect();
        for attach_id in ids {
            let delivered = stream
                .take_deliveries(session_id, attach_id)
                .expect("drain");
            {
                let model = models.get_mut(&session_id).expect("model");
                let expected = model.leases.get_mut(&attach_id).expect("lease");
                assert_eq!(
                    delivered, *expected,
                    "lease stream diverged from the expected stream at step {step}"
                );
                expected.clear();
            }
            if models[&session_id].terminal {
                stream
                    .ack_terminal(session_id, attach_id)
                    .expect("ack the terminal event");
            }
        }

        // A terminal session is collected once its retention TTL passes.
        if models[&session_id].terminal {
            let collected = stream.collect_retained(ms(now + 600_000));
            for collected_id in collected {
                models.remove(&collected_id);
            }
        }
    }

    assert!(
        attachments > 100,
        "the run exercised attaches: {attachments}"
    );
    assert!(terminals > 10, "the run exercised terminals: {terminals}");
    assert!(
        replays > 8_000,
        "the run audited a replay on most steps: {replays}"
    );
    let _ = seed;
}
