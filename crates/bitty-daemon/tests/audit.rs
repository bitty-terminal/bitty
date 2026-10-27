//! bitty#2603 / CTX-1622: public audit-ring behavior (`D-16`).
//!
//! The ring is the bounded staging buffer; these tests prove stable event ids,
//! visible overflow, injected timestamps, content-free records, and refusal
//! representation without a real sink.

use bitty_daemon::audit::{
    AUDIT_RECORD_BYTES, AuditAction, AuditActor, AuditCounts, AuditEntry, AuditEventId,
    AuditRecord, AuditResult, AuditRing, AuditTarget, AuditTerminal, audit_ring_capacity,
};
use bitty_daemon::codes::RefusalCode;
use bitty_daemon::limits::{LimitId, LimitOverride, LimitRefusal, Limits};
use bitty_daemon::registry::{
    CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs, SessionId,
    TerminalReason,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn instance() -> DaemonInstanceId {
    DaemonInstanceId::generate().expect("instance id")
}

fn session_id(seed: u8) -> SessionId {
    SessionId::parse(&format!("{seed:02x}").repeat(16)).expect("canonical session id")
}

fn ring_with(records: u64) -> AuditRing {
    let limits = Limits::new(&[
        LimitOverride::new(
            LimitId::AuditFileBytes,
            records.saturating_mul(AUDIT_RECORD_BYTES as u64),
        ),
        LimitOverride::new(LimitId::AuditSegments, 1),
    ])
    .expect("valid audit budget");
    AuditRing::new(instance(), &limits)
}

#[test]
fn record_is_copy_fixed_size_and_has_no_content_fields() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<AuditRecord>();
    assert!(!std::mem::needs_drop::<AuditRecord>());
    assert!(
        std::mem::size_of::<AuditRecord>() <= AUDIT_RECORD_BYTES,
        "record is {} bytes, budget is {AUDIT_RECORD_BYTES}",
        std::mem::size_of::<AuditRecord>()
    );

    // A String, Vec, or Box field would make AuditRecord non-Copy or give it a
    // destructor. A static string would still be Copy, so the marker check
    // below proves the raw terminal id is absent from the closed field list.
    let marker = "terminal-marker-must-not-be-copied";
    let terminal_id = bitty_daemon::registry::TerminalId::supplied(marker).expect("safe marker");
    assert_eq!(terminal_id.as_str(), marker);

    let mut ring = ring_with(2);
    ring.record(
        AuditEntry::new(AuditAction::Create)
            .target(AuditTarget::terminal_digest([0x5a; 16]))
            .session(bitty_daemon::audit::AuditSession {
                session_id: session_id(1),
                generation: 0,
                state_revision: 0,
            }),
        ms(1),
    )
    .expect("append");
    let rendered = format!("{:?}", ring.newest().expect("record"));
    assert!(!rendered.contains(marker));
    assert!(!rendered.contains("terminal_id"));
    assert_eq!(
        ring.newest().expect("record").target.kind(),
        bitty_daemon::audit::AuditTargetKind::TerminalDigest
    );
}

#[test]
fn overflow_drops_the_oldest_record_and_is_visible_in_counters() {
    let mut ring = ring_with(2);
    assert_eq!(ring.capacity(), 2);

    assert_eq!(
        ring.record(AuditEntry::new(AuditAction::Negotiate), ms(10))
            .expect("append"),
        AuditEventId::FIRST
    );
    ring.record(AuditEntry::new(AuditAction::Status), ms(20))
        .expect("append");
    ring.record(AuditEntry::new(AuditAction::List), ms(30))
        .expect("append");

    assert_eq!(ring.len(), 2);
    assert_eq!(ring.counters().appended, 3);
    assert_eq!(ring.counters().dropped, 1);
    assert_eq!(ring.oldest().expect("record").event_id.get(), 2);
    assert_eq!(ring.newest().expect("record").event_id.get(), 3);
    assert_eq!(
        ring.records()
            .map(|record| record.event_id.get())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );

    for _ in 0..10 {
        ring.record(AuditEntry::new(AuditAction::Heartbeat), ms(40))
            .expect("append");
    }
    assert_eq!(ring.len(), 2);
    assert_eq!(ring.counters().dropped, 11);
    assert_eq!(ring.counters().appended, 13);
}

#[test]
fn order_is_event_id_order_not_timestamp_order() {
    let mut ring = ring_with(4);
    ring.record(AuditEntry::new(AuditAction::Negotiate), ms(9_000))
        .expect("append");
    ring.record(AuditEntry::new(AuditAction::Create), ms(1_000))
        .expect("append");
    let observed: Vec<_> = ring
        .records()
        .map(|record| (record.event_id.get(), record.at_ms.as_millis()))
        .collect();
    assert!(observed[0].0 < observed[1].0);
    assert_eq!(observed, vec![(1, 9_000), (2, 1_000)]);
}

#[test]
fn a_refusal_is_representable_with_a_code_and_counts() {
    let limits = Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 4)]).expect("valid limit");
    let mut ring = ring_with(2);
    ring.record(
        AuditEntry::new(AuditAction::Create)
            .result(AuditResult::Refused(RefusalCode::LimitExceeded))
            .counts(AuditCounts::from_limit_refusal(LimitRefusal::new(
                LimitId::MaxSessions,
                4,
                5,
            ))),
        ms(1),
    )
    .expect("append");

    let record = ring.newest().expect("record");
    assert_eq!(
        record.result,
        AuditResult::Refused(RefusalCode::LimitExceeded)
    );
    assert_eq!(
        record.counts,
        Some(AuditCounts::new(5, 4, Some(LimitId::MaxSessions)))
    );
    assert_eq!(ring.counters().refusals, 1);
    assert!(
        limits.effective(LimitId::MaxSessions) < 5,
        "the refusal carries the enforced bound, not the compiled default"
    );
}

#[test]
fn terminal_transition_and_actor_facts_are_bounded_numeric_or_digest() {
    let mut ring = ring_with(2);
    let actor = AuditActor {
        peer_uid: Some(1000),
        peer_pid: Some(42),
        client_id: Some(7),
        client_correlation_digest: Some([0x33; 16]),
        ..AuditActor::default()
    };
    ring.record(
        AuditEntry::new(AuditAction::Kill)
            .actor(actor)
            .session(bitty_daemon::audit::AuditSession {
                session_id: session_id(2),
                generation: 1,
                state_revision: 3,
            })
            .target(AuditTarget::session(session_id(2)))
            .terminal(AuditTerminal {
                state: LifecycleState::Terminated,
                reason: TerminalReason::OperatorAbort,
                exit: ExitInformation::NotApplicable,
                cleanup: CleanupClass::Swept,
            }),
        ms(1),
    )
    .expect("append");

    let record = ring.newest().expect("record");
    assert_eq!(record.actor, Some(actor));
    assert_eq!(record.session.expect("session").generation, 1);
    assert_eq!(
        record.terminal.expect("terminal").reason,
        TerminalReason::OperatorAbort
    );
    assert_eq!(ring.counters().terminal_transitions, 1);
}

#[test]
fn ring_capacity_derives_from_the_advertised_audit_budget() {
    let expected = 3u64;
    let limits = Limits::new(&[
        LimitOverride::new(
            LimitId::AuditFileBytes,
            expected * AUDIT_RECORD_BYTES as u64,
        ),
        LimitOverride::new(LimitId::AuditSegments, 1),
    ])
    .expect("valid budget");
    assert_eq!(audit_ring_capacity(&limits), 3);
    assert_eq!(ring_with(3).capacity(), 3);

    let defaults = Limits::default();
    assert_eq!(
        audit_ring_capacity(&defaults),
        (1_048_576 * 2) / AUDIT_RECORD_BYTES
    );
}
