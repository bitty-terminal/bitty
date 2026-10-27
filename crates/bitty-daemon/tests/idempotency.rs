//! bitty#2603 / CTX-1622: public idempotency behavior (`D-18`).
//!
//! The counting executor in these tests is the proof that a replay does not
//! re-execute the request.

use bitty_daemon::codes::RefusalCode;
use bitty_daemon::idempotency::{
    Admission, AppliedClass, AppliedResult, Fingerprint, IdempotencyKey, IdempotencyRefusal,
    IdempotencyResult, IdempotencyStore, IdempotencyTarget, IdempotentMethod, IdempotentRequest,
};
use bitty_daemon::limits::{LimitId, LimitOverride, Limits};
use bitty_daemon::registry::{MonotonicMs, SessionId};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn key(seed: u16) -> IdempotencyKey {
    let mut bytes = [0u8; 16];
    bytes[..2].copy_from_slice(&seed.to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

fn fingerprint(seed: u8) -> Fingerprint {
    Fingerprint::from_bytes([seed; 16])
}

fn session_id(seed: u8) -> SessionId {
    SessionId::parse(&format!("{seed:02x}").repeat(16)).expect("canonical session id")
}

fn request(seed: u16, method: IdempotentMethod) -> IdempotentRequest {
    IdempotentRequest::new(
        key(seed),
        method,
        IdempotencyTarget::session(session_id(u8::try_from(seed % 251).expect("bounded"))),
        fingerprint(u8::try_from(seed % 253).expect("bounded")),
        1,
    )
}

fn create_request(seed: u16) -> IdempotentRequest {
    IdempotentRequest::new(
        key(seed),
        IdempotentMethod::Create,
        IdempotencyTarget::terminal_digest([u8::try_from(seed % 251).expect("bounded"); 16]),
        fingerprint(u8::try_from(seed % 253).expect("bounded")),
        1,
    )
}

fn applied(seed: u8, class: AppliedClass) -> IdempotencyResult {
    IdempotencyResult::Applied(AppliedResult::new(class, session_id(seed), 0, 0, 1, false))
}

fn small_session_store(max_keys: u64, max_bytes: u64) -> IdempotencyStore {
    let limits = Limits::new(&[
        LimitOverride::new(LimitId::MaxIdempotencyKeysPerSession, max_keys),
        LimitOverride::new(LimitId::MaxIdempotencyBytesPerSession, max_bytes),
    ])
    .expect("valid small store");
    IdempotencyStore::session(&limits, session_id(1))
}

struct Executor {
    runs: u32,
}

impl Executor {
    fn new() -> Self {
        Self { runs: 0 }
    }
}

fn execute(
    store: &mut IdempotencyStore,
    request: IdempotentRequest,
    executor: &mut Executor,
    result: IdempotencyResult,
    now: u64,
) -> IdempotencyResult {
    match store
        .begin(request, ms(now))
        .expect("admission is not refused")
    {
        Admission::Replay(result) => result,
        Admission::Execute(reservation) => {
            executor.runs += 1;
            store.complete(reservation, result).expect("completion");
            result
        }
    }
}

fn seed(store: &mut IdempotencyStore, request: IdempotentRequest, result: IdempotencyResult) {
    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new key executes");
    };
    store.complete(reservation, result).expect("complete");
}

#[test]
fn replay_returns_the_identical_result_and_never_executes_twice() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let mut executor = Executor::new();
    let request = request(1, IdempotentMethod::Attach);
    let result = applied(2, AppliedClass::Attached);

    assert_eq!(
        execute(&mut store, request, &mut executor, result, 1),
        result
    );
    assert_eq!(executor.runs, 1);

    for now in 2..12 {
        assert_eq!(
            execute(&mut store, request, &mut executor, result, now),
            result
        );
    }
    assert_eq!(executor.runs, 1);
    assert_eq!(store.len(), 1);
}

#[test]
fn duplicate_while_in_flight_is_refused_without_executing_again() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let mut executor = Executor::new();
    let request = request(1, IdempotentMethod::Attach);
    let result = applied(2, AppliedClass::Attached);

    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new key executes");
    };
    executor.runs += 1;
    assert_eq!(
        store.begin(request, ms(2)),
        Err(IdempotencyRefusal::InFlight)
    );
    assert_eq!(executor.runs, 1);

    store.complete(reservation, result).expect("complete");
    assert_eq!(store.begin(request, ms(3)), Ok(Admission::Replay(result)));
    assert_eq!(executor.runs, 1);
}

#[test]
fn same_key_with_a_different_fingerprint_is_a_conflict() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let request = request(1, IdempotentMethod::Attach);
    seed(&mut store, request, applied(2, AppliedClass::Attached));

    let mut conflicting = request;
    conflicting.fingerprint = fingerprint(99);
    assert_eq!(
        store.begin(conflicting, ms(2)),
        Err(IdempotencyRefusal::Conflict {
            existing: request.identity(),
            presented: conflicting.identity(),
        })
    );
    assert_eq!(store.len(), 1);
    assert_eq!(
        store.begin(request, ms(3)),
        Ok(Admission::Replay(applied(2, AppliedClass::Attached)))
    );
}

#[test]
fn a_refused_result_replays_without_re_execution() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let mut executor = Executor::new();
    let request = request(1, IdempotentMethod::Grant);
    let refused = IdempotencyResult::Refused(RefusalCode::ConsentRequired);

    assert_eq!(
        execute(&mut store, request, &mut executor, refused, 1),
        refused
    );
    assert_eq!(
        execute(&mut store, request, &mut executor, refused, 2),
        refused
    );
    assert_eq!(executor.runs, 1);
    assert_eq!(store.len(), 1);
}

#[test]
fn create_keys_retire_only_after_the_terminal_transition_and_ttl_floor() {
    let limits = Limits::default();
    let mut store = IdempotencyStore::create(&limits);
    let request = create_request(1);
    let ttl = limits.effective(LimitId::CreateIdempotencyTtlMs);

    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new create key executes");
    };
    store
        .complete(reservation, applied(2, AppliedClass::Created))
        .expect("complete");

    let due = 1 + ttl;
    assert_eq!(store.evict_expired(ms(due + 10)), 0);
    assert_eq!(store.retire_session(session_id(2)).expect("retire"), 1);
    assert_eq!(store.evict_expired(ms(due - 1)), 0);
    assert_eq!(store.len(), 1);

    assert_eq!(store.evict_expired(ms(due)), 1);
    assert!(store.is_empty());
    assert!(matches!(
        store.begin(request, ms(due + 1)),
        Ok(Admission::Execute(_))
    ));
}

#[test]
fn create_keys_are_not_retired_while_the_session_is_live_even_past_the_ttl() {
    let limits = Limits::default();
    let mut store = IdempotencyStore::create(&limits);
    let request = create_request(1);
    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new create key executes");
    };
    store
        .complete(reservation, applied(2, AppliedClass::Created))
        .expect("complete");

    let ttl = limits.effective(LimitId::CreateIdempotencyTtlMs);
    assert_eq!(store.evict_expired(ms(ttl + 10_000)), 0);
    assert_eq!(store.retire_session(session_id(99)).expect("retire"), 0);
    assert_eq!(store.len(), 1);
    assert_eq!(
        store.begin(request, ms(ttl + 10_001)),
        Ok(Admission::Replay(applied(2, AppliedClass::Created)))
    );
}

#[test]
fn capacity_evicts_the_oldest_retirable_entry_first() {
    let mut store = small_session_store(2, 256);
    assert_eq!(store.capacity(), 2);

    let first = request(1, IdempotentMethod::Attach);
    let second = request(2, IdempotentMethod::Attach);
    let third = request(3, IdempotentMethod::Attach);
    seed(
        &mut store,
        first,
        IdempotencyResult::Refused(RefusalCode::ScopeViolation),
    );
    seed(
        &mut store,
        second,
        IdempotencyResult::Refused(RefusalCode::ScopeViolation),
    );

    let Admission::Execute(reservation) = store.begin(third, ms(3)).expect("evicts the oldest")
    else {
        panic!("third key executes after the oldest refusal is evicted");
    };
    store
        .complete(reservation, applied(4, AppliedClass::Attached))
        .expect("complete");

    // The second refusal is still replayable, so the first was the victim.
    assert_eq!(
        store.begin(second, ms(4)),
        Ok(Admission::Replay(IdempotencyResult::Refused(
            RefusalCode::ScopeViolation
        )))
    );
    // The next capacity decision evicts the next-oldest retirable entry.
    assert!(matches!(
        store.begin(first, ms(5)),
        Ok(Admission::Execute(_))
    ));
    assert_eq!(
        store.begin(second, ms(6)),
        Err(IdempotencyRefusal::Capacity {
            max_keys: 2,
            max_bytes: 256,
        })
    );
    assert!(store.len() <= 2);
}

#[test]
fn in_flight_entries_are_never_evicted_for_capacity() {
    let mut store = small_session_store(2, 256);
    let first = request(1, IdempotentMethod::Attach);
    let second = request(2, IdempotentMethod::Attach);
    let third = request(3, IdempotentMethod::Attach);

    let Admission::Execute(_reservation) = store.begin(first, ms(1)).expect("begin") else {
        panic!("first key executes");
    };
    seed(
        &mut store,
        second,
        IdempotencyResult::Refused(RefusalCode::ScopeViolation),
    );

    // The refusal is retirable, so the third key may replace it; the in-flight
    // first key stays.
    assert!(matches!(
        store.begin(third, ms(3)),
        Ok(Admission::Execute(_))
    ));
    assert_eq!(store.begin(first, ms(4)), Err(IdempotencyRefusal::InFlight));
    assert_eq!(store.len(), 2);

    // With two in-flight entries and no retirable one, a fourth key is refused.
    let fourth = request(4, IdempotentMethod::Attach);
    assert_eq!(
        store.begin(fourth, ms(5)),
        Err(IdempotencyRefusal::Capacity {
            max_keys: 2,
            max_bytes: 256,
        })
    );
    assert_eq!(store.len(), 2);
    assert_eq!(store.accounted_bytes(), 256);
}

#[test]
fn a_hostile_key_stream_stays_within_the_store_bounds() {
    let mut store = small_session_store(4, 512);
    for seed in 0..4u16 {
        seed_static(&mut store, seed);
    }
    assert_eq!(store.len(), 4);

    for seed in 100..1_100u16 {
        let request = request(seed, IdempotentMethod::Attach);
        assert_eq!(
            store.begin(request, ms(u64::from(seed))),
            Err(IdempotencyRefusal::Capacity {
                max_keys: 4,
                max_bytes: 512,
            })
        );
        assert!(store.len() <= 4);
        assert!(store.accounted_bytes() <= 512);
    }
}

fn seed_static(store: &mut IdempotencyStore, seed: u16) {
    let request = request(seed, IdempotentMethod::Attach);
    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new key executes");
    };
    store
        .complete(
            reservation,
            applied(
                u8::try_from(seed % 251).expect("bounded"),
                AppliedClass::Attached,
            ),
        )
        .expect("complete");
}

#[test]
fn stores_refuse_methods_from_the_other_scope() {
    let mut session_store = IdempotencyStore::session(&Limits::default(), session_id(1));
    assert!(matches!(
        session_store.begin(create_request(1), ms(1)),
        Err(IdempotencyRefusal::WrongScope { .. })
    ));
    assert!(session_store.is_empty());

    let mut create_store = IdempotencyStore::create(&Limits::default());
    assert!(matches!(
        create_store.begin(request(1, IdempotentMethod::Attach), ms(1)),
        Err(IdempotencyRefusal::WrongScope { .. })
    ));
    assert!(create_store.is_empty());
}

#[test]
fn a_conflict_never_prints_the_raw_key() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let request = request(0xabab, IdempotentMethod::Attach);
    seed(&mut store, request, applied(2, AppliedClass::Attached));

    let mut conflicting = request;
    conflicting.fingerprint = fingerprint(0x11);
    let error = store.begin(conflicting, ms(2)).expect_err("conflict");
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains("abab"));
    assert!(rendered.contains("session.attach"));
}

#[test]
fn lease_bound_replays_are_restricted_to_the_originating_connection() {
    let mut store = IdempotencyStore::session(&Limits::default(), session_id(1));
    let request = request(1, IdempotentMethod::Attach);
    seed(&mut store, request, applied(2, AppliedClass::Attached));

    let mut other_connection = request;
    other_connection.connection_id = 2;
    assert_eq!(
        store.begin(other_connection, ms(2)),
        Err(IdempotencyRefusal::ReplayOnOtherConnection)
    );
    assert_eq!(
        store.begin(request, ms(3)),
        Ok(Admission::Replay(applied(2, AppliedClass::Attached)))
    );
}

#[test]
fn create_results_replay_across_connections_after_a_lost_response() {
    let mut store = IdempotencyStore::create(&Limits::default());
    let request = create_request(1);
    seed(&mut store, request, applied(2, AppliedClass::Created));

    let mut other_connection = request;
    other_connection.connection_id = 2;
    assert_eq!(
        store.begin(other_connection, ms(2)),
        Ok(Admission::Replay(applied(2, AppliedClass::Created)))
    );
}

#[test]
fn create_store_capacity_is_128_entries_and_32_768_bytes() {
    let limits = Limits::default();
    let mut store = IdempotencyStore::create(&limits);
    assert_eq!(store.capacity(), 128);
    assert_eq!(store.max_bytes(), 32_768);

    for seed in 0..128u16 {
        let request = create_request(seed);
        let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
            panic!("new create key executes");
        };
        store
            .complete(
                reservation,
                applied(u8::try_from(seed).expect("bounded"), AppliedClass::Created),
            )
            .expect("complete");
    }
    assert_eq!(store.len(), 128);
    assert_eq!(store.accounted_bytes(), 32_768);
    assert_eq!(
        store.begin(create_request(128), ms(2)),
        Err(IdempotencyRefusal::Capacity {
            max_keys: 128,
            max_bytes: 32_768,
        })
    );
    assert_eq!(store.len(), 128);
}

#[test]
fn create_store_byte_bound_can_be_lowered_independently() {
    let limits = Limits::new(&[
        LimitOverride::new(LimitId::MaxCreateIdempotencyKeys, 128),
        LimitOverride::new(LimitId::MaxCreateIdempotencyBytes, 256),
    ])
    .expect("valid create-store budget");
    let mut store = IdempotencyStore::create(&limits);
    assert_eq!(store.capacity(), 1);

    let Admission::Execute(reservation) = store.begin(create_request(1), ms(1)).expect("begin")
    else {
        panic!("new create key executes");
    };
    store
        .complete(reservation, applied(2, AppliedClass::Created))
        .expect("complete");
    assert_eq!(
        store.begin(create_request(2), ms(2)),
        Err(IdempotencyRefusal::Capacity {
            max_keys: 128,
            max_bytes: 256,
        })
    );
    assert_eq!(store.accounted_bytes(), 256);
}

#[test]
fn lowered_create_ttl_is_honored_after_the_terminal_transition() {
    let limits = Limits::new(&[LimitOverride::new(LimitId::CreateIdempotencyTtlMs, 10)])
        .expect("valid lowered TTL");
    let mut store = IdempotencyStore::create(&limits);
    let request = create_request(1);
    let Admission::Execute(reservation) = store.begin(request, ms(1)).expect("begin") else {
        panic!("new create key executes");
    };
    store
        .complete(reservation, applied(2, AppliedClass::Created))
        .expect("complete");

    assert_eq!(
        store.evict_expired(ms(100)),
        0,
        "live session never retires"
    );
    assert_eq!(store.retire_session(session_id(2)).expect("retire"), 1);
    let due = 1 + 10;
    assert_eq!(store.evict_expired(ms(due - 1)), 0);
    assert_eq!(store.len(), 1);
    assert_eq!(store.evict_expired(ms(due)), 1);
    assert!(store.is_empty());
}
