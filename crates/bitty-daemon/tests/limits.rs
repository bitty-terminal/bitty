//! bitty#2603 / CTX-1622: public limits, advertisement, and whole-request
//! refusal behavior.
//!
//! These tests use only the exported crate surface.

use bitty_daemon::audit::{AuditAction, AuditCounts, AuditEntry, AuditResult, AuditRing};
use bitty_daemon::codes::RefusalCode;
use bitty_daemon::contract::ClientVersion;
use bitty_daemon::handshake::{NegotiateRequest, negotiate};
use bitty_daemon::limits::{
    LIMIT_COUNT, LimitId, LimitOverride, LimitRefusal, LimitSource, Limits, LimitsError,
};
use bitty_daemon::orphan::OrphanJournalLimits;
use bitty_daemon::registry::{
    CreateSession, DaemonInstanceId, MonotonicMs, RegistryCounts, RegistryLimits, RegistryRefusal,
    SessionRegistry, TerminalId,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn term(value: &str) -> TerminalId {
    TerminalId::supplied(value).expect("valid terminal id")
}

#[test]
fn every_cap_accepts_limit_minus_one_and_limit_and_refuses_limit_plus_one() {
    for limit in LimitId::ALL {
        let maximum = limit.system_max_value();
        let at_max = Limits::new(&[LimitOverride::new(limit, maximum)])
            .unwrap_or_else(|error| panic!("{limit} at maximum: {error}"));
        assert_eq!(at_max.effective(limit), maximum, "{limit}");

        let below_max = Limits::new(&[LimitOverride::new(limit, maximum - 1)]);
        let above_max = Limits::new(&[LimitOverride::new(limit, maximum + 1)]);
        if limit.is_fixed() {
            assert_eq!(
                below_max,
                Err(LimitsError::FixedLimit {
                    limit,
                    requested: maximum - 1,
                    fixed: maximum,
                }),
                "{limit}"
            );
            assert_eq!(
                above_max,
                Err(LimitsError::FixedLimit {
                    limit,
                    requested: maximum + 1,
                    fixed: maximum,
                }),
                "{limit}"
            );
        } else {
            assert_eq!(
                below_max
                    .expect("below maximum is accepted")
                    .effective(limit),
                maximum - 1,
                "{limit}"
            );
            assert_eq!(
                above_max,
                Err(LimitsError::AboveSystemMax {
                    limit,
                    requested: maximum + 1,
                    system_max: maximum,
                }),
                "{limit}"
            );
        }

        let floor = limit.floor();
        assert_eq!(
            Limits::new(&[LimitOverride::new(limit, floor)])
                .expect("floor is accepted")
                .effective(limit),
            floor,
            "{limit}"
        );
        assert!(floor >= 1, "{limit} has no positive floor");
        let below_floor = Limits::new(&[LimitOverride::new(limit, floor - 1)]);
        if limit.is_fixed() {
            assert!(matches!(below_floor, Err(LimitsError::FixedLimit { .. })));
        } else {
            assert_eq!(
                below_floor,
                Err(LimitsError::BelowFloor {
                    limit,
                    requested: floor - 1,
                    floor,
                }),
                "{limit}"
            );
        }
    }
}

#[test]
fn advertised_object_is_effective_default_system_max_with_unique_names() {
    let limits = Limits::default();
    let entries: Vec<_> = limits.advertised().collect();
    assert_eq!(entries.len(), LIMIT_COUNT);
    assert_eq!(LIMIT_COUNT, 54);

    let mut d14 = 0;
    let mut inherited = 0;
    for (entry, limit) in entries.iter().zip(LimitId::ALL) {
        assert_eq!(entry.name, limit.name());
        let value = entry.value;
        assert_eq!(value.default_value(), limit.default_value());
        assert_eq!(value.system_max(), limit.system_max_value());
        assert_eq!(
            value.triple(),
            (
                limit.default_value(),
                limit.default_value(),
                limit.system_max_value()
            )
        );
        assert!(value.effective() >= limit.floor());
        assert!(value.effective() <= value.system_max());
        match limit.source() {
            LimitSource::ContractD14 => d14 += 1,
            LimitSource::InheritedTransport => {
                inherited += 1;
                assert_eq!(value.effective(), value.default_value());
                assert_eq!(value.default_value(), value.system_max());
            }
        }
    }
    assert_eq!(d14, 46);
    assert_eq!(inherited, 8);
}

#[test]
fn configuration_may_lower_or_raise_to_the_maximum_but_never_above_it() {
    let lowered =
        Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 1)]).expect("lowering is allowed");
    assert_eq!(lowered.effective(LimitId::MaxSessions), 1);
    assert!(!lowered.is_default());

    let raised = Limits::new(&[LimitOverride::new(
        LimitId::MaxSessions,
        LimitId::MaxSessions.system_max_value(),
    )])
    .expect("raising to the maximum is allowed");
    assert_eq!(raised.effective(LimitId::MaxSessions), 64);

    assert_eq!(
        Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 65)]),
        Err(LimitsError::AboveSystemMax {
            limit: LimitId::MaxSessions,
            requested: 65,
            system_max: 64,
        })
    );
    assert_eq!(
        Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 0)]),
        Err(LimitsError::BelowFloor {
            limit: LimitId::MaxSessions,
            requested: 0,
            floor: 1,
        })
    );
}

#[test]
fn inherited_transport_bounds_refuse_any_change() {
    for limit in LimitId::ALL.into_iter().filter(|limit| limit.is_fixed()) {
        let fixed = limit.default_value();
        assert_eq!(
            Limits::new(&[LimitOverride::new(limit, fixed)])
                .expect("the fixed value is accepted")
                .effective(limit),
            fixed
        );
        for requested in [fixed - 1, fixed + 1] {
            assert_eq!(
                Limits::new(&[LimitOverride::new(limit, requested)]),
                Err(LimitsError::FixedLimit {
                    limit,
                    requested,
                    fixed,
                })
            );
        }
    }
}

#[test]
fn duplicate_overrides_refuse_the_whole_configuration() {
    let error = Limits::new(&[
        LimitOverride::new(LimitId::MaxSessions, 8),
        LimitOverride::new(LimitId::MaxSessions, 4),
    ])
    .expect_err("duplicate configuration is refused");
    assert_eq!(
        error,
        LimitsError::DuplicateOverride {
            limit: LimitId::MaxSessions
        }
    );
}

#[test]
fn registry_and_orphan_bounds_follow_the_effective_limits() {
    let limits = Limits::new(&[
        LimitOverride::new(LimitId::MaxSessions, 2),
        LimitOverride::new(LimitId::MaxRetainedTerminalRecords, 1),
        LimitOverride::new(LimitId::SessionRecordBytes, 2048),
        LimitOverride::new(LimitId::TerminalRetentionMs, 100),
        LimitOverride::new(LimitId::MaxOrphanRecords, 4),
        LimitOverride::new(LimitId::OrphanRecordBytes, 1024),
        LimitOverride::new(LimitId::OrphanRecordRetentionMs, 2000),
    ])
    .expect("valid lowered limits");

    let registry = RegistryLimits::from_limits(&limits);
    assert_eq!(registry.max_sessions(), 2);
    assert_eq!(registry.max_retained_terminal_records(), 1);
    assert_eq!(registry.session_record_bytes(), 2048);
    assert_eq!(registry.terminal_retention_ms(), 100);

    let orphan = OrphanJournalLimits::from_limits(&limits);
    assert_eq!(orphan.max_records(), 4);
    assert_eq!(orphan.record_bytes(), 1024);
    assert_eq!(orphan.quarantine_retention_ms(), 2000);
}

#[test]
fn handshake_advertises_the_effective_limits_and_compiled_ceiling() {
    let limits =
        Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 3)]).expect("valid lowered limits");
    let response = negotiate(&NegotiateRequest::new(ClientVersion::new(1, 0)), &limits)
        .expect("negotiation succeeds");
    let advertised = response
        .limits
        .advertised()
        .find(|entry| entry.name == "max_sessions")
        .expect("max_sessions is advertised");
    assert_eq!(advertised.value.effective(), 3);
    assert_eq!(advertised.value.default_value(), 16);
    assert_eq!(advertised.value.system_max(), 64);
}

#[test]
fn a_limit_refusal_leaves_the_registry_unchanged_and_is_auditable() {
    let limits =
        Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 1)]).expect("valid lowered limits");
    let mut registry =
        SessionRegistry::new(RegistryLimits::from_limits(&limits)).expect("registry");
    let first = registry
        .create(
            CreateSession {
                terminal_id: Some(term("term-a")),
                owner_peer: None,
            },
            ms(1),
        )
        .expect("first create");
    let before: RegistryCounts = registry.counts();

    assert_eq!(
        registry.create(
            CreateSession {
                terminal_id: Some(term("term-b")),
                owner_peer: None,
            },
            ms(2),
        ),
        Err(RegistryRefusal::MaxSessions { limit: 1 })
    );
    assert_eq!(registry.counts(), before);
    assert_eq!(registry.live_count(), 1);
    assert_eq!(registry.retained_count(), 0);
    assert_eq!(
        registry
            .snapshot(first.session_id)
            .map(|snapshot| snapshot.terminal_id),
        Some(term("term-a"))
    );

    let mut ring = AuditRing::new(DaemonInstanceId::generate().expect("instance id"), &limits);
    let refusal = LimitRefusal::new(LimitId::MaxSessions, 1, 2);
    ring.record(
        AuditEntry::new(AuditAction::Create)
            .result(AuditResult::Refused(RefusalCode::LimitExceeded))
            .counts(AuditCounts::from_limit_refusal(refusal)),
        ms(2),
    )
    .expect("refusal record");
    assert_eq!(ring.len(), 1);
    assert_eq!(
        ring.newest().expect("record").counts,
        Some(AuditCounts::new(2, 1, Some(LimitId::MaxSessions)))
    );
    assert_eq!(registry.counts(), before);
}
