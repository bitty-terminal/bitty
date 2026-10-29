//! bitty#2601 / CTX-1619: public registry behavior.
//!
//! These tests use only the exported crate surface, so they also prove the
//! registry is usable by the future service layer without reaching into
//! internals.

use std::collections::HashSet;

use bitty_daemon::registry::{
    CleanupClass, CreateSession, ExitInformation, LifecycleState, MAX_SESSIONS_SYSTEM_MAX,
    MonotonicMs, OwnerPeer, RegistryLimits, RegistryRefusal, SESSION_RECORD_BYTES_SYSTEM_MAX,
    SessionRegistry, TERMINAL_RETENTION_MS_SYSTEM_MAX, TerminalId, TerminalOutcome, TerminalReason,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn limits(
    max_sessions: usize,
    max_retained: usize,
    record_bytes: usize,
    retention_ms: u64,
) -> RegistryLimits {
    RegistryLimits::new(max_sessions, max_retained, record_bytes, retention_ms)
        .expect("valid limits")
}

fn terminal(id: &str) -> TerminalId {
    TerminalId::supplied(id).expect("valid terminal id")
}

#[test]
fn lifecycle_counters_and_retention_work_through_the_public_surface() {
    let mut registry = SessionRegistry::new(limits(4, 2, 4096, 100)).expect("registry");
    assert_eq!(registry.instance_id().as_hex().len(), 32);
    assert_eq!(registry.live_count(), 0);

    let created = registry
        .create(
            CreateSession {
                terminal_id: Some(terminal("term-1")),
                owner_peer: Some(OwnerPeer::new(1000, 7)),
            },
            ms(1),
        )
        .expect("create");
    assert_eq!(created.state, LifecycleState::Created);
    assert_eq!(created.generation, 0);
    assert_eq!(created.state_revision, 0);
    assert!(!created.attached);

    let running = registry.bind(created.session_id, ms(2)).expect("bind");
    assert_eq!(running.generation, 1);
    assert_eq!(running.state_revision, 1);

    let heartbeat = registry
        .heartbeat(created.session_id, ms(3))
        .expect("heartbeat");
    assert_eq!(heartbeat.state_revision, running.state_revision);
    assert_eq!(heartbeat.last_seen_ms, ms(3));

    let attached = registry.attach(created.session_id, ms(4)).expect("attach");
    assert!(attached.attached);
    assert_eq!(attached.state_revision, 2);

    let stopping = registry
        .begin_stopping(created.session_id, ms(5))
        .expect("stopping");
    assert_eq!(stopping.state, LifecycleState::Stopping);

    let outcome = TerminalOutcome::new(
        TerminalReason::ConsumerCancel,
        ExitInformation::NotApplicable,
        CleanupClass::Swept,
    )
    .expect("valid outcome");
    let terminal_snapshot = registry
        .terminate(created.session_id, outcome, ms(6))
        .expect("terminate");
    assert_eq!(terminal_snapshot.state, LifecycleState::Terminated);
    assert_eq!(terminal_snapshot.generation, 1);
    assert_eq!(terminal_snapshot.state_revision, 4);
    assert_eq!(
        terminal_snapshot.terminal_reason,
        Some(TerminalReason::ConsumerCancel)
    );
    assert_eq!(registry.live_count(), 0);
    assert_eq!(registry.retained_count(), 1);
    assert_eq!(registry.counts().terminated, 1);

    assert!(registry.collect_retained(ms(105)).is_empty());
    assert_eq!(registry.collect_retained(ms(106)), vec![created.session_id]);
    assert_eq!(registry.snapshot(created.session_id), None);
    assert_eq!(
        registry.heartbeat(created.session_id, ms(200)),
        Err(RegistryRefusal::NotFound)
    );
}

#[test]
fn many_created_sessions_get_fresh_ids_and_terminal_ids_stay_unique() {
    let mut registry =
        SessionRegistry::new(limits(MAX_SESSIONS_SYSTEM_MAX, 0, 4096, 600_000)).expect("registry");
    let mut session_ids = HashSet::new();

    for index in 0..MAX_SESSIONS_SYSTEM_MAX {
        let snapshot = registry
            .create(
                CreateSession {
                    terminal_id: Some(terminal(&format!("term-{index}"))),
                    owner_peer: None,
                },
                ms(1),
            )
            .expect("create");
        assert!(session_ids.insert(snapshot.session_id));
        assert_eq!(snapshot.terminal_id, terminal(&format!("term-{index}")));
    }
    assert_eq!(session_ids.len(), MAX_SESSIONS_SYSTEM_MAX);

    assert_eq!(
        registry.create(
            CreateSession {
                terminal_id: Some(terminal("term-0")),
                owner_peer: None,
            },
            ms(2)
        ),
        Err(RegistryRefusal::MaxSessions {
            limit: MAX_SESSIONS_SYSTEM_MAX
        })
    );
}

#[test]
fn refusals_leave_the_public_counts_unchanged() {
    let mut registry = SessionRegistry::new(limits(4, 2, 4096, 600_000)).expect("registry");
    let first = registry
        .create(
            CreateSession {
                terminal_id: Some(terminal("term-a")),
                owner_peer: None,
            },
            ms(1),
        )
        .expect("create");

    let before = registry.counts();
    assert_eq!(
        registry.create(
            CreateSession {
                terminal_id: Some(terminal("term-a")),
                owner_peer: None,
            },
            ms(2)
        ),
        Err(RegistryRefusal::TerminalAlreadyHosted)
    );
    assert_eq!(registry.counts(), before);
    assert_eq!(
        registry
            .snapshot(first.session_id)
            .map(|snapshot| snapshot.terminal_id),
        Some(terminal("term-a"))
    );

    let mut capacity_limited = SessionRegistry::new(limits(1, 2, 4096, 600_000)).expect("registry");
    capacity_limited
        .create(
            CreateSession {
                terminal_id: Some(terminal("term-b")),
                owner_peer: None,
            },
            ms(1),
        )
        .expect("create");
    assert_eq!(
        capacity_limited.create(
            CreateSession {
                terminal_id: Some(terminal("term-d")),
                owner_peer: None,
            },
            ms(2)
        ),
        Err(RegistryRefusal::MaxSessions { limit: 1 })
    );
    assert_eq!(capacity_limited.counts().total(), 1);

    let mut byte_limited = SessionRegistry::new(limits(2, 0, 1, 600_000)).expect("registry");
    let refusal = byte_limited.create(
        CreateSession {
            terminal_id: Some(terminal("term-c")),
            owner_peer: None,
        },
        ms(1),
    );
    assert!(matches!(
        refusal,
        Err(RegistryRefusal::SessionRecordTooLarge { .. })
    ));
    assert_eq!(byte_limited.counts().total(), 0);
}

#[test]
fn public_limits_cannot_exceed_the_compiled_maxima() {
    assert!(matches!(
        RegistryLimits::new(
            MAX_SESSIONS_SYSTEM_MAX + 1,
            64,
            SESSION_RECORD_BYTES_SYSTEM_MAX,
            TERMINAL_RETENTION_MS_SYSTEM_MAX
        ),
        Err(RegistryRefusal::InvalidLimits {
            limit: "max_sessions",
            ..
        })
    ));
    assert!(matches!(
        RegistryLimits::new(
            16,
            64,
            SESSION_RECORD_BYTES_SYSTEM_MAX + 1,
            TERMINAL_RETENTION_MS_SYSTEM_MAX
        ),
        Err(RegistryRefusal::InvalidLimits {
            limit: "session_record_bytes",
            ..
        })
    ));
}
