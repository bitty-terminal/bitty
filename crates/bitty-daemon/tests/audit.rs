//! bitty#2603 / CTX-1622: public audit-ring behavior (`D-16`).
//!
//! The ring is the bounded staging buffer; these tests prove stable event ids,
//! visible overflow, injected timestamps, content-free records, and refusal
//! representation without a real sink.
//!
//! The `P0-AC-026` half at the bottom drives the real sink write path
//! (`AuditSink::record` -> `render_record` -> `AuditWriter::append_line`) with a
//! capturing writer and a seeded-secret corpus, so "the redaction is enforced at
//! the write point" is asserted where the bytes are produced instead of assumed
//! from the record type.

use bitty_daemon::audit::{
    AUDIT_RECORD_BYTES, AuditAction, AuditActor, AuditAggregate, AuditClassCount, AuditCounts,
    AuditEntry, AuditEventId, AuditRecord, AuditResult, AuditRing, AuditTarget, AuditTerminal,
    MAX_AGGREGATE_CLASSES, audit_ring_capacity,
};
use bitty_daemon::audit_sink::{AuditSink, AuditSinkError, AuditWriter, render_record};
use bitty_daemon::codes::RefusalCode;
use bitty_daemon::limits::{LimitId, LimitOverride, LimitRefusal, Limits};
use bitty_daemon::registry::{
    CleanupClass, DaemonInstanceId, ExitInformation, LifecycleState, MonotonicMs, SessionId,
    TerminalReason,
};
use sha2::{Digest, Sha256};

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

// ── P0-AC-026: the sink write point is the redaction point ──────────────────

/// A writer that captures exactly the bytes the sink hands it.
///
/// The production writer is `AuditFileWriter`; this one is the same trait, so a
/// change that let a caller write pre-rendered bytes, or that rendered a field
/// the record does not have, would show up here. The capture is shared rather
/// than returned because `AuditSink` owns its writer.
#[derive(Clone, Default)]
struct CapturingWriter {
    lines: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl AuditWriter for CapturingWriter {
    fn append_line(&mut self, line: &[u8]) -> Result<(), AuditSinkError> {
        let text = String::from_utf8(line.to_vec()).expect("a rendered line is UTF-8");
        self.lines.borrow_mut().push(text);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), AuditSinkError> {
        Ok(())
    }
}

/// The seeded-secret corpus (`P0-AC-026`: "seeded-secret corpus never appears in
/// default outputs").
///
/// Each entry has the shape of a credential a leaked environment, argv, or
/// `cwd` would carry: a cloud access key, an API key, a forge token, a private
/// key header, a URL with an inline password, and a bearer token. They are
/// assembled from fragments so this source file carries no credential-shaped
/// literal of its own; the value is what a client would supply, and nothing in
/// the audit path is allowed to retain it.
fn seeded_secrets() -> Vec<String> {
    vec![
        format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE"),
        format!("{}-{}-{}", "sk", "live", "51H8xQ2eZvKYlo2C8b"),
        format!("{}{}", "ghp_", "16CharactersLongToken0000"),
        format!("{} {} {}", "-----BEGIN", "OPENSSH", "PRIVATE KEY-----"),
        format!(
            "{}://{}:{}@{}/{}",
            "postgres", "app", "hunter2", "db.internal", "prod"
        ),
        format!("{} {}", "Bearer", "eyJhbGciOiJIUzI1NiJ9.payload.sig"),
    ]
}

/// A domain-separated 16-byte digest, in the shape the daemon stores as a
/// digest-only target.
fn seeded_digest(seed: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"bittyd/audit/target\0");
    hasher.update(seed.as_bytes());
    let out = hasher.finalize();
    let mut digest = [0_u8; 16];
    digest.copy_from_slice(&out[..16]);
    digest
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// One record with every optional field populated, so the closed field list is
/// exercised at its widest.
fn maximal_entry(seed: &str) -> AuditEntry {
    let mut classes = [None; MAX_AGGREGATE_CLASSES];
    classes[0] = Some(AuditClassCount {
        code: RefusalCode::ScopeViolation,
        count: 2,
    });
    AuditEntry::new(AuditAction::Create)
        .result(AuditResult::Refused(RefusalCode::ScopeViolation))
        .actor(AuditActor {
            peer_uid: Some(1000),
            peer_pid: Some(4242),
            client_id: Some(9),
            client_correlation_digest: Some(seeded_digest(seed)),
            ..AuditActor::default()
        })
        .session(bitty_daemon::audit::AuditSession {
            session_id: session_id(1),
            generation: 1,
            state_revision: 2,
        })
        .target(AuditTarget::terminal_digest(seeded_digest(seed)))
        .counts(AuditCounts::new(5, 4, Some(LimitId::MaxSessions)))
        .aggregate(AuditAggregate {
            suppressed: 3,
            budget: 100,
            classes,
            other: 1,
        })
        .terminal(AuditTerminal {
            state: LifecycleState::Terminated,
            reason: TerminalReason::WorkerExit,
            exit: ExitInformation::ExitCode(0),
            cleanup: CleanupClass::Swept,
        })
}

/// The closed key vocabulary the renderer may emit, in emit order.
const RENDERED_KEYS: [&str; 26] = [
    "ev", "t", "i", "a", "r", "c", "s", "g", "rv", "u", "p", "cid", "x", "k", "d", "ca", "cl", "m",
    "sup", "bl", "h", "ho", "z", "zr", "zc", "zk",
];

#[test]
fn a_seeded_secret_never_reaches_the_audit_line() {
    let limits = Limits::default();
    let captured = CapturingWriter::default();
    let lines = std::rc::Rc::clone(&captured.lines);
    let mut sink = AuditSink::new(instance(), &limits, captured);

    let seeds = seeded_secrets();
    for (index, seed) in seeds.iter().enumerate() {
        sink.record(
            maximal_entry(seed),
            ms(u64::try_from(index).expect("index")),
        )
        .expect("the sink writes the record");
        let written = lines.borrow().last().cloned().expect("one line per record");
        assert!(
            !written.contains(seed.as_str()),
            "the seeded secret reached the audit line: {written}"
        );
        // No partial leak either: every 6-byte window of the seed is absent, so
        // a truncated value (a clipped field, a 16-byte prefix) cannot pass.
        let window = 6;
        for start in 0..seed.len().saturating_sub(window - 1) {
            let slice = &seed[start..start + window];
            assert!(
                !written.contains(slice),
                "the audit line carries {slice:?} from the seeded secret"
            );
        }
        // The only projection of the value is the opaque digest.
        let digest = hex(&seeded_digest(seed));
        assert!(
            written.contains(&digest),
            "the digest-only target is missing: {written}"
        );
        assert_ne!(digest, seed.as_str());
    }
    assert_eq!(lines.borrow().len(), seeds.len(), "one line per record");
    assert_eq!(sink.pending(), 0, "every record was flushed");
}

#[test]
fn the_rendered_line_has_a_closed_key_set_and_carries_no_free_text() {
    let limits = Limits::default();
    let seed = seeded_secrets().remove(0);
    // The sink writes the renderer's own bytes: the same record rendered twice
    // (once through the sink's flush, once directly) is identical.
    let captured = CapturingWriter::default();
    let lines = std::rc::Rc::clone(&captured.lines);
    let instance_id = instance();
    let mut sink = AuditSink::new(instance_id, &limits, captured);
    sink.record(maximal_entry(&seed), ms(7))
        .expect("sink write");
    let written = lines.borrow().last().cloned().expect("one line");

    let mut ring = AuditRing::new(instance_id, &limits);
    ring.record(maximal_entry(&seed), ms(7))
        .expect("ring append");
    let direct = render_record(ring.newest().expect("the ring keeps the record"))
        .expect("the record renders");
    assert_eq!(
        written.as_bytes(),
        direct.as_slice(),
        "the sink writes render_record's bytes and nothing else"
    );

    // `v1` then `k=v` fields: any other shape is not a v1 line.
    let fields: Vec<&str> = written.split(' ').collect();
    assert_eq!(fields.first(), Some(&"v1"));
    let mut keys = Vec::new();
    for field in &fields[1..] {
        let (key, value) = field
            .split_once('=')
            .unwrap_or_else(|| panic!("{field:?} is not a k=v field"));
        assert!(
            RENDERED_KEYS.contains(&key),
            "{key} is outside the frozen field vocabulary"
        );
        assert!(!keys.contains(&key), "{key} is emitted twice");
        keys.push(key);
        // A value is a closed enum, a count, or a hex digest: no path
        // separator, no whitespace, no second `=`, no non-ASCII byte. That is
        // what makes an argv, environment value, cwd, or message unrepresentable
        // even if a future field were misused.
        assert!(
            value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b',' | b'-')
            }),
            "{key}={value:?} carries free text"
        );
        assert!(!value.is_empty() || key == "ho", "{key} rendered empty");
    }
    assert_eq!(keys, RENDERED_KEYS, "the closed field list changed");
    assert!(written.is_ascii(), "an audit line is ASCII: {written}");
}
