//! Compressed long-run event-ring soak for bitty#2830 (headless).
//!
//! The owner's unresponsiveness appeared after long uptime, so the session
//! event plane is exercised with thousands of attach/detach/terminal cycles
//! against an injected clock. The test asserts the retention bounds the
//! daemon promises (`D-10`, `D-14`, `D-18`): the backlog never exceeds its
//! cap, no lease queue survives a detach, the terminal pin is released when
//! its consumers close, and accounted bytes stay bounded.

use bitty_daemon::events::{AttachRequest, DetachCause, EventLimits, SessionEventStream};
use bitty_daemon::registry::{
    CleanupClass, CreateSession, ExitInformation, MonotonicMs, RegistryLimits, SessionId,
    SessionRegistry, TerminalId, TerminalOutcome, TerminalReason,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn new_stream() -> SessionEventStream {
    SessionEventStream::with_limits(
        SessionRegistry::new(RegistryLimits::new(4, 4, 4096, 600_000).expect("registry limits"))
            .expect("registry"),
        EventLimits::default(),
    )
    .expect("empty registry")
}

fn create(stream: &mut SessionEventStream, terminal_id: &str, now: u64) -> SessionId {
    stream
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied(terminal_id).expect("terminal id")),
                owner_peer: None,
            },
            ms(now),
        )
        .expect("create")
        .session_id
}

/// 2,000 attach/detach cycles plus five full terminal/retention cycles.
#[test]
fn event_ring_and_lease_queues_stay_bounded_over_many_cycles() {
    let mut stream = new_stream();
    let mut now = 1u64;
    let mut session = create(&mut stream, "longrun-0", now);
    let mut terminal_cycles = 0u32;

    for cycle in 0..2_000u64 {
        now += 1;
        let lease = stream
            .attach(
                AttachRequest {
                    session_id: session,
                    after_revision: 0,
                    idempotency_key: None,
                },
                ms(now),
            )
            .expect("attach");
        assert!(
            stream.lease_count(session).is_some_and(|count| count <= 2),
            "cycle {cycle}: lease count exceeded the configured cap"
        );
        now += 1;
        stream
            .detach(session, lease.attach_id, DetachCause::SelfDetach, ms(now))
            .expect("detach");
        assert_eq!(
            stream.lease_count(session),
            Some(0),
            "cycle {cycle}: detach left a lease"
        );
        assert_eq!(
            stream.lease_queue_len(session, lease.attach_id),
            None,
            "cycle {cycle}: detached lease queue survived"
        );

        // Every 400 cycles: terminate, pin the terminal event for one lease,
        // then close it and collect the record. This is the retention path the
        // issue calls out (ring freed at terminal; pin released on ack/close).
        if cycle % 400 == 399 {
            terminal_cycles += 1;
            let terminal_lease = stream
                .attach(
                    AttachRequest {
                        session_id: session,
                        after_revision: 0,
                        idempotency_key: None,
                    },
                    ms(now),
                )
                .expect("terminal attach");
            let outcome = TerminalOutcome::new(
                TerminalReason::OperatorAbort,
                ExitInformation::NotApplicable,
                CleanupClass::Swept,
            )
            .expect("outcome");
            let terminal = stream
                .terminate(session, outcome, ms(now))
                .expect("terminate");
            assert!(
                terminal.lost.is_empty(),
                "cycle {cycle}: a lease lost the terminal frame"
            );
            assert_eq!(
                stream.backlog_len(session),
                Some(0),
                "cycle {cycle}: the ring must be freed at the terminal transition"
            );
            assert_eq!(
                stream.pending_terminal_consumers(session),
                Some(1),
                "cycle {cycle}: the terminal event must stay pinned for its consumer"
            );
            let closed = stream
                .close_lease(session, terminal_lease.attach_id, ms(now))
                .expect("close terminal lease");
            assert_eq!(closed.len(), 1);
            assert!(
                matches!(stream.pending_terminal_consumers(session), None | Some(0)),
                "cycle {cycle}: closing the last consumer must release the pin"
            );

            // Collect past the retention window, then start a fresh session.
            now += 600_000;
            let collected = stream.collect_retained(ms(now));
            assert!(
                collected.collected.contains(&session),
                "cycle {cycle}: retained terminal record must be collected"
            );
            assert_eq!(stream.lease_count(session), None);
            assert_eq!(stream.backlog_len(session), None);
            now += 1;
            session = create(&mut stream, &format!("longrun-{terminal_cycles}"), now);
        }
    }

    assert_eq!(terminal_cycles, 5);
    assert_eq!(stream.lease_count(session), Some(0));
    assert!(
        stream.backlog_len(session).unwrap_or(0) <= 256,
        "backlog events exceeded the compiled cap"
    );
    assert!(
        stream.backlog_bytes(session).unwrap_or(0) <= 262_144,
        "backlog bytes exceeded the compiled cap"
    );
    assert!(
        stream.accounted_bytes(session).unwrap_or(0) <= 512 * 1024,
        "accounted stream bytes grew past the bounded fixture budget"
    );
    assert!(
        matches!(stream.pending_terminal_consumers(session), None | Some(0)),
        "no terminal pin may remain on a live session"
    );
}

/// Content read/write churn: a slow reader never grows past the cap and every
/// accepted byte is either popped or retained (never silently lost).
#[test]
fn content_read_write_stays_bounded() {
    use bitty_daemon::content::ContentBuffer;

    const CAPACITY: usize = 4 * 1024;
    let mut buffer = ContentBuffer::with_capacity(CAPACITY);
    let chunk = [b'x'; 97];
    let mut accepted = 0u64;
    let mut popped = 0u64;
    for cycle in 0..5_000u64 {
        accepted += buffer.push(&chunk) as u64;
        assert!(
            buffer.len() <= CAPACITY,
            "cycle {cycle}: content buffer exceeded its cap"
        );
        // Alternate between no read (backpressure) and bounded reads.
        if cycle % 3 != 0 {
            popped += buffer.pop(256).len() as u64;
        }
        assert_eq!(
            buffer.len() as u64,
            accepted - popped,
            "cycle {cycle}: retained bytes must equal accepted minus popped"
        );
    }
    popped += buffer.pop(buffer.len()).len() as u64;
    assert!(buffer.is_empty());
    assert_eq!(accepted, popped, "no accepted byte may be lost");
    assert!(accepted > 0);
}
