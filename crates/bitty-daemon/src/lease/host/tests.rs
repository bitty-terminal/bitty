use std::collections::HashSet;

use super::*;
use crate::registry::{CreateSession, RegistryLimits, TerminalId};

/// Assert a refusal without requiring `PartialEq` on the secret-bearing
/// success types (`Claimed`/`Transferred` contain a `ControlToken`).
macro_rules! assert_refused {
    ($result:expr, $expected:expr) => {
        assert_eq!(
            $result.expect_err("expected a refusal"),
            $expected,
            "wrong refusal"
        );
    };
}

struct CounterEntropy {
    next: u8,
}

impl Entropy for CounterEntropy {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), crate::registry::IdError> {
        destination.fill(self.next);
        self.next = self.next.wrapping_add(1);
        Ok(())
    }
}

struct BudgetEntropy {
    remaining: usize,
}

impl Entropy for BudgetEntropy {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), crate::registry::IdError> {
        if self.remaining == 0 {
            return Err(crate::registry::IdError::RandomUnavailable);
        }
        self.remaining -= 1;
        destination.fill(0x33);
        Ok(())
    }
}

/// A broken source that returns the same bytes every time; a rotation from it
/// is not a rotation.
struct ConstantEntropy;

impl Entropy for ConstantEntropy {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), crate::registry::IdError> {
        destination.fill(0x2a);
        Ok(())
    }
}

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

struct Harness {
    host: SessionHost,
}

impl Harness {
    fn new() -> Self {
        Self::with_limits(LeaseLimits::new(4, 100, 50, 70).expect("valid lease limits"))
    }

    fn with_limits(limits: LeaseLimits) -> Self {
        let registry_limits =
            RegistryLimits::new(8, 8, 4096, 600_000).expect("valid registry limits");
        let registry =
            SessionRegistry::with_entropy(registry_limits, Box::new(CounterEntropy { next: 1 }))
                .expect("registry");
        Self {
            host: SessionHost::with_parts(registry, limits, Box::new(CounterEntropy { next: 100 })),
        }
    }

    fn create(&mut self, terminal_id: &str, now: u64) -> (SessionId, ControlToken) {
        let created = self
            .host
            .create(
                CreateSession {
                    terminal_id: Some(
                        TerminalId::supplied(terminal_id).expect("valid terminal id"),
                    ),
                    owner_peer: None,
                },
                ms(now),
            )
            .expect("create succeeds");
        (created.session.session_id, created.control_token)
    }

    fn bind(&mut self, session_id: SessionId, now: u64) -> SessionSnapshot {
        self.host.bind(session_id, ms(now)).expect("bind succeeds")
    }

    fn attach(&mut self, session_id: SessionId, now: u64) -> Lease {
        self.host
            .attach_observer(session_id, ms(now))
            .expect("observer attach succeeds")
            .lease
    }

    fn claim(
        &mut self,
        session_id: SessionId,
        generation: u64,
        token: &ControlToken,
        now: u64,
    ) -> (Lease, ControlToken) {
        let claimed = self
            .host
            .claim_controller(session_id, generation, token, ms(now))
            .expect("claim succeeds");
        (claimed.lease, claimed.control_token)
    }

    fn control(&self, session_id: SessionId, now: u64) -> ControlView {
        self.host
            .control_view(session_id, ms(now))
            .expect("session exists")
    }
}

fn token_hex(token: &ControlToken) -> String {
    token
        .expose_secret()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn wrong_token() -> ControlToken {
    ControlToken::from_bytes([0u8; ControlToken::BYTES])
}

#[test]
fn create_returns_the_token_once_and_stores_only_a_digest() {
    let mut harness = Harness::new();
    let (session_id, token) = harness.create("term-digest", 1);
    let hex = token_hex(&token);

    assert_eq!(token.expose_secret().len(), 32);
    let created_debug = format!("{token:?}");
    assert!(!created_debug.contains(&hex));
    assert!(!format!("{:?}", harness.host).contains(&hex));

    let view = harness.control(session_id, 1);
    assert_eq!(view.token_state, ControlTokenState::Current);
    assert!(!view.claimed);
    assert_eq!(view.attach_count, 0);
    assert_eq!(view.controller, None);
    assert!(!format!("{view:?}").contains(&hex));

    // Structural digest-only storage: the stored value is the digest of
    // the token, not the token itself, and it has the digest width.
    let state = harness
        .host
        .sessions
        .get(&session_id)
        .expect("control state exists");
    let TokenSlot::Current(stored) = &state.token else {
        panic!("a fresh session must hold a current token digest");
    };
    let expected = ControlTokenDigest::of(&token);
    assert_eq!(stored.as_bytes(), expected.as_bytes());
    assert_eq!(stored.as_bytes().len(), 32);
    assert_ne!(stored.as_bytes(), token.expose_secret());
}

#[test]
fn bind_then_claim_rotates_the_token_and_binds_one_controller() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-claim", 1);
    let bound = harness.bind(session_id, 2);
    assert_eq!(bound.generation, 1);

    let (controller, claimed_token) = harness.claim(session_id, 1, &create_token, 3);
    assert_eq!(controller.role(), LeaseRole::Controller);
    assert_ne!(token_hex(&create_token), token_hex(&claimed_token));
    let view = harness.control(session_id, 3);
    assert_eq!(view.controller, Some(controller.attach_id()));
    assert_eq!(view.attach_count, 1);
    assert!(view.claimed);
    assert_eq!(view.token_state, ControlTokenState::Current);

    let before = harness.host.snapshot(session_id).expect("snapshot");
    assert_refused!(
        harness
            .host
            .claim_controller(session_id, 1, &claimed_token, ms(4)),
        LeaseRefusal::ControlHeld
    );
    assert_eq!(harness.host.snapshot(session_id).expect("snapshot"), before);
}

#[test]
fn a_claim_before_bind_persists_across_the_generation_bump() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-claim-before-bind", 1);
    let (controller, _token) = harness.claim(session_id, 0, &create_token, 2);
    assert_eq!(
        harness.control(session_id, 2).controller,
        Some(controller.attach_id())
    );

    let bound = harness.bind(session_id, 3);
    assert_eq!(bound.generation, 1);
    assert_eq!(
        harness.control(session_id, 3).controller,
        Some(controller.attach_id()),
        "a bind does not invalidate the controller lease or rotate the token"
    );
    let StopOutcome::Accepted(accepted) = harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(4),
        )
        .expect("abort at the bound generation")
    else {
        panic!("accepted stop");
    };
    assert_eq!(accepted.session.state, LifecycleState::Stopping);
}

#[test]
fn one_controller_survives_sequential_attach_and_claim_interleavings() {
    let mut harness =
        Harness::with_limits(LeaseLimits::new(8, 100, 50, 70).expect("valid lease limits"));
    let (session_id, create_token) = harness.create("term-one-controller", 1);
    harness.bind(session_id, 2);

    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let mut attach_ids = vec![controller.attach_id()];
    for step in 0..6u64 {
        let observer = harness.attach(session_id, 10 + step);
        attach_ids.push(observer.attach_id());
        assert_eq!(
            harness
                .host
                .claim_controller(session_id, 1, &token, ms(20 + step))
                .err(),
            Some(LeaseRefusal::ControlHeld),
            "a second claim must never create a second controller"
        );
    }

    let controllers: Vec<AttachId> = attach_ids
        .iter()
        .filter(|attach_id| {
            harness
                .host
                .lease(**attach_id)
                .is_some_and(|lease| lease.role() == LeaseRole::Controller)
        })
        .copied()
        .collect();
    assert_eq!(controllers, vec![controller.attach_id()]);
    assert_eq!(
        harness.control(session_id, 30).attach_count,
        u16::try_from(attach_ids.len()).expect("bounded")
    );
}

#[test]
fn attach_ids_are_distinct_and_128_bit() {
    let mut harness = Harness::new();
    let (first, _) = harness.create("term-ids-a", 1);
    let (second, _) = harness.create("term-ids-b", 1);
    let mut seen = HashSet::new();
    for index in 0..4u64 {
        for session in [first, second] {
            let lease = harness.attach(session, 10 + index);
            let text = lease.attach_id().as_hex();
            assert_eq!(text.len(), 32);
            assert!(
                text.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert!(seen.insert(lease.attach_id()));
        }
    }
    assert_eq!(seen.len(), 8);
}

#[test]
fn observer_attach_cap_sheds_without_evicting() {
    let mut harness =
        Harness::with_limits(LeaseLimits::new(3, 100, 50, 70).expect("valid lease limits"));
    let (session_id, _) = harness.create("term-cap", 1);
    let first = harness.attach(session_id, 2);
    let second = harness.attach(session_id, 3);
    let third = harness.attach(session_id, 4);
    assert_eq!(
        harness.host.attach_observer(session_id, ms(5)),
        Err(LeaseRefusal::MaxAttaches { limit: 3 })
    );
    assert_eq!(harness.control(session_id, 5).attach_count, 3);
    for lease in [first, second, third] {
        assert!(
            harness.host.heartbeat(lease.attach_id(), ms(6)).is_ok(),
            "existing leases survive a shed attach"
        );
    }
}

#[test]
fn claim_at_the_observer_cap_adds_the_controller_without_evicting() {
    let mut harness =
        Harness::with_limits(LeaseLimits::new(2, 100, 50, 70).expect("valid lease limits"));
    let (session_id, create_token) = harness.create("term-cap-claim", 1);
    harness.bind(session_id, 2);
    let first = harness.attach(session_id, 3);
    let second = harness.attach(session_id, 4);
    assert_refused!(
        harness.host.attach_observer(session_id, ms(5)),
        LeaseRefusal::MaxAttaches { limit: 2 }
    );

    let claimed = harness
        .host
        .claim_controller(session_id, 1, &create_token, ms(6))
        .expect("the controller is additional to the observer cap");
    assert_eq!(harness.control(session_id, 6).attach_count, 3);
    assert!(harness.host.lease(first.attach_id()).is_some());
    assert!(harness.host.lease(second.attach_id()).is_some());
    assert_eq!(claimed.lease.role(), LeaseRole::Controller);
    assert_refused!(
        harness.host.attach_observer(session_id, ms(7)),
        LeaseRefusal::MaxAttaches { limit: 2 }
    );
}

#[test]
fn stale_generation_is_refused_before_the_token_comparison() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-stale", 1);
    harness.bind(session_id, 2);

    let stale = LeaseRefusal::StaleGeneration {
        current_generation: 1,
        current_revision: 1,
    };
    assert_refused!(
        harness
            .host
            .claim_controller(session_id, 0, &create_token, ms(3)),
        stale.clone()
    );
    assert_eq!(
        harness
            .host
            .claim_controller(session_id, 0, &wrong_token(), ms(3))
            .err(),
        Some(stale.clone()),
        "a stale generation must not expose whether the token is right"
    );

    // A vacant control relation still checks the generation before the
    // token digest.
    harness
        .host
        .claim_controller(session_id, 1, &create_token, ms(4))
        .expect("claim");
    let view = harness.control(session_id, 4);
    let controller = view.controller.expect("controller");
    harness
        .host
        .detach(
            session_id,
            controller,
            &ControlAuthority::lease(controller),
            ms(5),
        )
        .expect("self detach");
    let revision = harness
        .host
        .snapshot(session_id)
        .expect("snapshot")
        .state_revision;
    assert_refused!(
        harness.host.abort(
            session_id,
            0,
            &ControlAuthority::token(&wrong_token()),
            ConsumerStopCause::Cancel,
            ms(6),
        ),
        LeaseRefusal::StaleGeneration {
            current_generation: 1,
            current_revision: revision
        }
    );
}

#[test]
fn observer_cannot_abort_transfer_or_revoke() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-observer", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let revision = harness
        .host
        .snapshot(session_id)
        .expect("snapshot")
        .state_revision;

    assert_eq!(
        harness.host.abort(
            session_id,
            1,
            &ControlAuthority::lease(observer.attach_id()),
            ConsumerStopCause::Cancel,
            ms(5),
        ),
        Err(LeaseRefusal::ControllerRequired)
    );
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(observer.attach_id()),
            observer.attach_id(),
            ms(5),
        ),
        LeaseRefusal::ControllerRequired
    );
    assert_eq!(
        harness.host.detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::lease(observer.attach_id()),
            ms(5),
        ),
        Err(LeaseRefusal::ControllerRequired)
    );
    // Even a valid token does not let an observer act while a controller
    // lease is live: the observer relation is refused first.
    assert_eq!(
        harness.host.abort(
            session_id,
            1,
            &ControlAuthority::lease(observer.attach_id()).with_token(&token),
            ConsumerStopCause::Cancel,
            ms(5),
        ),
        Err(LeaseRefusal::ControllerRequired)
    );

    let after = harness.host.snapshot(session_id).expect("snapshot");
    assert_eq!(after.state_revision, revision);
    assert_eq!(after.state, LifecycleState::Running);
    assert_eq!(after.attach_count, 2);
}

#[test]
fn controller_self_detach_vacates_control_and_the_current_token_reclaims() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-self-detach", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    let detached = harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(5),
        )
        .expect("self detach");
    assert!(detached.was_attached);
    assert!(!detached.revoked);
    assert!(!detached.control_retired);
    assert_eq!(detached.session.state_revision, before.state_revision + 1);
    let view = harness.control(session_id, 5);
    assert_eq!(view.controller, None);
    assert_eq!(view.token_state, ControlTokenState::Current);
    assert_eq!(view.attach_count, 1);
    assert!(harness.host.lease(observer.attach_id()).is_some());

    // The current token reclaims control; the detached create token stays
    // rotated and cannot.
    assert_refused!(
        harness
            .host
            .claim_controller(session_id, 1, &create_token, ms(6)),
        LeaseRefusal::InvalidControlToken
    );
    let (new_controller, new_token) = harness.claim(session_id, 1, &token, 6);
    assert_ne!(new_controller.attach_id(), controller.attach_id());
    assert_ne!(token_hex(&token), token_hex(&new_token));

    // Rotate again, then the previous token is refused at vacant control.
    harness
        .host
        .detach(
            session_id,
            new_controller.attach_id(),
            &ControlAuthority::lease(new_controller.attach_id()),
            ms(7),
        )
        .expect("self detach");
    assert_refused!(
        harness.host.claim_controller(session_id, 1, &token, ms(8)),
        LeaseRefusal::InvalidControlToken
    );
    assert!(
        harness
            .host
            .claim_controller(session_id, 1, &new_token, ms(8))
            .is_ok()
    );
}

#[test]
fn operator_revoke_of_the_controller_retires_control_forever() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-retire", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let generation = harness
        .host
        .snapshot(session_id)
        .expect("snapshot")
        .generation;

    let revoked = harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::operator(),
            ms(5),
        )
        .expect("operator revoke");
    assert!(revoked.was_attached);
    assert!(revoked.revoked);
    assert!(revoked.control_retired);
    assert_eq!(revoked.actor, Actor::Operator);

    let view = harness.control(session_id, 5);
    assert_eq!(view.controller, None);
    assert_eq!(view.token_state, ControlTokenState::Retired);
    assert_eq!(view.attach_count, 1);
    assert!(harness.host.lease(observer.attach_id()).is_some());

    let running = harness.host.snapshot(session_id).expect("snapshot");
    assert_eq!(running.state, LifecycleState::Running);
    assert_eq!(running.generation, generation);
    for presented in [&token, &wrong_token()] {
        assert_eq!(
            harness
                .host
                .claim_controller(session_id, generation, presented, ms(6))
                .err(),
            Some(LeaseRefusal::InvalidControlToken),
            "control never recovers after an operator revoke"
        );
    }
}

#[test]
fn controller_and_operator_revoke_observer_leases() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-revoke-observers", 1);
    harness.bind(session_id, 2);
    let (controller, _token) = harness.claim(session_id, 1, &create_token, 3);
    let first = harness.attach(session_id, 4);
    let second = harness.attach(session_id, 5);

    let revoked = harness
        .host
        .detach(
            session_id,
            first.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(6),
        )
        .expect("controller revoke");
    assert!(revoked.revoked);
    assert!(!revoked.control_retired);
    assert_eq!(revoked.role, Some(LeaseRole::Observer));
    assert_eq!(
        harness.host.heartbeat(first.attach_id(), ms(7)),
        Err(LeaseRefusal::NotAttached)
    );

    harness
        .host
        .detach(
            session_id,
            second.attach_id(),
            &ControlAuthority::operator(),
            ms(7),
        )
        .expect("operator revoke");
    assert_eq!(
        harness.host.heartbeat(second.attach_id(), ms(8)),
        Err(LeaseRefusal::NotAttached)
    );
    assert_eq!(harness.control(session_id, 8).attach_count, 1);
    assert_eq!(
        harness.control(session_id, 8).controller,
        Some(controller.attach_id())
    );
}

#[test]
fn duplicate_detach_is_idempotent_and_keeps_the_revision() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-detach-idempotent", 1);
    let observer = harness.attach(session_id, 2);
    let first = harness
        .host
        .detach(
            session_id,
            observer.attach_id(),
            &ControlAuthority::lease(observer.attach_id()),
            ms(3),
        )
        .expect("self detach");
    assert!(first.was_attached);
    let revision = first.session.state_revision;

    // The host keeps no per-connection handle history, so a second own
    // detach of the removed handle is `NotAttached` and changes nothing;
    // bitty#2607 maps a previously issued handle to the contract's
    // idempotent `not_attached` reply.
    assert_refused!(
        harness.host.detach(
            session_id,
            observer.attach_id(),
            &ControlAuthority::lease(observer.attach_id()),
            ms(4),
        ),
        LeaseRefusal::NotAttached
    );
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .state_revision,
        revision
    );

    // An operator naming a lease that is not live is an idempotent no-op
    // for a known session.
    let no_op = harness
        .host
        .detach(
            session_id,
            observer.attach_id(),
            &ControlAuthority::operator(),
            ms(5),
        )
        .expect("operator no-op");
    assert!(!no_op.was_attached);
    assert!(!no_op.revoked);
    assert_eq!(no_op.attach_id, observer.attach_id());
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .state_revision,
        revision
    );
}

#[test]
fn lease_expiry_boundaries_renewal_and_detach_only() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-expiry", 10);
    let bound = harness.bind(session_id, 11);
    let observer = harness.attach(session_id, 100);

    // Elapsed 0 and TTL-1 are live.
    assert!(
        harness
            .host
            .heartbeat(observer.attach_id(), ms(100))
            .is_ok()
    );
    assert!(
        harness
            .host
            .heartbeat(observer.attach_id(), ms(199))
            .is_ok()
    );
    // Renewal at 199 moves the expiry to 299. TTL-1 (298) is live, but
    // reading the lease must not renew it, so 299 is still expired.
    assert!(
        !harness
            .host
            .lease(observer.attach_id())
            .expect("lease")
            .is_expired_at(ms(298))
    );
    assert_refused!(
        harness.host.heartbeat(observer.attach_id(), ms(299)),
        LeaseRefusal::NotAttached
    );
    assert_refused!(
        harness.host.heartbeat(observer.attach_id(), ms(300)),
        LeaseRefusal::NotAttached
    );

    let revision = harness
        .host
        .snapshot(session_id)
        .expect("snapshot")
        .state_revision;
    let expired = harness.host.expire_due(ms(299));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attach_id, observer.attach_id());
    assert_eq!(expired[0].role, LeaseRole::Observer);
    assert!(harness.host.expire_due(ms(299)).is_empty());

    let after = harness.host.snapshot(session_id).expect("snapshot");
    assert_eq!(after.state, LifecycleState::Running);
    assert_eq!(after.generation, bound.generation);
    assert_eq!(after.attach_count, 0);
    assert_eq!(after.state_revision, revision + 1);
    assert!(!after.attached);
}

#[test]
fn expired_leases_cannot_act_and_claim_vacates_an_expired_controller() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-expired-act", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 100);
    let observer = harness.attach(session_id, 100);
    let revision = harness
        .host
        .snapshot(session_id)
        .expect("snapshot")
        .state_revision;

    // At exactly TTL both leases are expired and cannot act.
    assert_eq!(
        harness.host.abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(200),
        ),
        Err(LeaseRefusal::NotAttached)
    );
    assert!(
        !harness
            .host
            .detach(
                session_id,
                observer.attach_id(),
                &ControlAuthority::lease(observer.attach_id()),
                ms(200),
            )
            .expect("expired own detach is an idempotent no-op")
            .was_attached
    );
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .state_revision,
        revision
    );

    // A claim at the next instant vacates the expired controller and
    // succeeds with the current token.
    let claimed = harness
        .host
        .claim_controller(session_id, 1, &token, ms(201))
        .expect("claim vacates the expired controller");
    let vacated = claimed
        .vacated_expired_controller
        .expect("expired controller reported");
    assert_eq!(vacated.attach_id, controller.attach_id());
    assert_eq!(
        claimed.session.state_revision,
        revision + 2,
        "the due detach and the claim's attach are two snapshot-visible mutations"
    );
    assert_eq!(
        vacated.revision, claimed.session.state_revision,
        "the expiry is visible in the claim's snapshot revision"
    );
    assert_eq!(
        harness.control(session_id, 201).controller,
        Some(claimed.lease.attach_id())
    );
    // The expired observer remains until the sweep removes it.
    assert!(harness.host.lease(observer.attach_id()).is_some());
    assert_eq!(harness.host.expire_due(ms(201)).len(), 1);
    assert!(harness.host.lease(observer.attach_id()).is_none());
}

#[test]
fn controller_expiry_vacates_control_but_keeps_the_token_reclaimable() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-expire-controller", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 100);

    let expired = harness.host.expire_due(ms(200));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attach_id, controller.attach_id());
    assert_eq!(expired[0].role, LeaseRole::Controller);
    let view = harness.control(session_id, 200);
    assert_eq!(view.controller, None);
    assert_eq!(view.attach_count, 0);
    assert_eq!(
        view.token_state,
        ControlTokenState::Current,
        "expiry detaches; it never revokes or rotates the token"
    );

    // The holder of the current token reclaims control after expiry.
    assert!(
        harness
            .host
            .claim_controller(session_id, 1, &token, ms(201))
            .is_ok()
    );
}

#[test]
fn operator_revoke_of_an_expired_controller_retires_control() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-revoke-expired", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 100);

    let revoked = harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::operator(),
            ms(200),
        )
        .expect("operator revoke");
    assert!(!revoked.was_attached, "the lease had already expired");
    assert!(revoked.revoked);
    assert!(revoked.control_retired);
    assert_eq!(revoked.role, Some(LeaseRole::Controller));
    assert!(harness.host.lease(controller.attach_id()).is_none());
    assert_eq!(
        harness.control(session_id, 200).token_state,
        ControlTokenState::Retired
    );
    assert_refused!(
        harness
            .host
            .claim_controller(session_id, 1, &token, ms(201)),
        LeaseRefusal::InvalidControlToken
    );
}

#[test]
fn observer_resize_is_refused_and_controller_resize_bumps_once() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-resize", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    assert_refused!(
        harness.host.record_resize(observer.attach_id(), ms(5)),
        LeaseRefusal::ControllerRequired
    );
    assert_refused!(
        harness
            .host
            .authorize_controller(observer.attach_id(), ms(5)),
        LeaseRefusal::ControllerRequired
    );
    assert_eq!(
        harness.host.snapshot(session_id).expect("snapshot"),
        before,
        "an observer resize applies nothing"
    );

    let resized = harness
        .host
        .record_resize(controller.attach_id(), ms(6))
        .expect("controller resize");
    assert_eq!(resized.state_revision, before.state_revision + 1);
    assert_eq!(resized.generation, before.generation);
    assert_eq!(resized.attach_count, before.attach_count);

    harness
        .host
        .record_termination(
            session_id,
            ExitObservation::Exited { code: 0 },
            CleanupClass::Swept,
            ms(7),
        )
        .expect("terminal");
    assert_refused!(
        harness.host.record_resize(controller.attach_id(), ms(8)),
        LeaseRefusal::NotAttached
    );
}

#[test]
fn a_vacant_control_token_authorises_a_consumer_abort() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-vacant-abort", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(4),
        )
        .expect("self detach");

    let StopOutcome::Accepted(accepted) = harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Timeout,
            ms(5),
        )
        .expect("token abort")
    else {
        panic!("accepted stop");
    };
    assert_eq!(accepted.actor, Actor::ControlToken);
    assert_eq!(accepted.session.state, LifecycleState::Stopping);
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(6),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::ConsumerTimeout);
    assert_eq!(notice.actor, Actor::ControlToken);
}

#[test]
fn transfer_moves_control_demotes_the_old_controller_and_rotates_the_token() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-transfer", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    let transferred = harness
        .host
        .transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            observer.attach_id(),
            ms(5),
        )
        .expect("transfer");
    assert_eq!(
        transferred.previous_controller,
        Some(controller.attach_id())
    );
    assert_eq!(transferred.controller, observer.attach_id());
    assert_eq!(
        transferred.actor,
        Actor::Lease(controller.attach_id()),
        "the transfer result records who requested it (audit D-16)"
    );
    assert_eq!(
        transferred.session.state_revision,
        before.state_revision + 1,
        "a transfer is one revision, not a detach plus an attach"
    );
    assert_eq!(
        harness
            .host
            .lease(controller.attach_id())
            .expect("lease")
            .role(),
        LeaseRole::Observer
    );
    assert_eq!(
        harness
            .host
            .lease(observer.attach_id())
            .expect("lease")
            .role(),
        LeaseRole::Controller
    );
    assert_eq!(
        harness.control(session_id, 5).controller,
        Some(observer.attach_id())
    );

    // D-09: the rotated token is addressed to the target lease only. The
    // requesting connection (the previous controller) cannot take it, and the
    // target takes the secret that actually verifies.
    let rendered = format!("{transferred:?}");
    assert!(
        rendered.contains("ControlToken(<redacted>)") && !rendered.contains(&token_hex(&token)),
        "debug output never carries a token"
    );
    let delivery = transferred
        .delivery
        .take_for(controller.attach_id())
        .expect_err("the source response must not carry the rotated token");
    assert_eq!(delivery.target(), observer.attach_id());
    let rotated_token = delivery
        .take_for(observer.attach_id())
        .expect("target delivery");
    assert_ne!(token_hex(&token), token_hex(&rotated_token));

    // The old controller is now an observer and cannot act.
    assert_eq!(
        harness.host.abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(6),
        ),
        Err(LeaseRefusal::ControllerRequired)
    );

    // The previous token is refused at vacant control.
    harness
        .host
        .detach(
            session_id,
            observer.attach_id(),
            &ControlAuthority::lease(observer.attach_id()),
            ms(7),
        )
        .expect("self detach");
    assert_refused!(
        harness.host.claim_controller(session_id, 1, &token, ms(8)),
        LeaseRefusal::InvalidControlToken
    );
    assert!(
        harness
            .host
            .claim_controller(session_id, 1, &rotated_token, ms(8))
            .is_ok()
    );
}

#[test]
fn transfer_from_vacant_control_uses_the_token_and_requires_an_observer_target() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-transfer-vacant", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(5),
        )
        .expect("self detach");

    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::token(&wrong_token()),
            observer.attach_id(),
            ms(6),
        ),
        LeaseRefusal::InvalidControlToken
    );
    assert_eq!(
        harness
            .host
            .transfer_control(
                session_id,
                1,
                &ControlAuthority::token(&token),
                observer.attach_id(),
                ms(6),
            )
            .expect("transfer from vacant control")
            .previous_controller,
        None
    );
    assert_eq!(
        harness.control(session_id, 6).controller,
        Some(observer.attach_id())
    );

    // The promoted lease is no longer an observer target.
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(observer.attach_id()),
            observer.attach_id(),
            ms(7),
        ),
        LeaseRefusal::TargetNotObserver
    );
    // A stale generation applies nothing.
    let before = harness.host.snapshot(session_id).expect("snapshot");
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            0,
            &ControlAuthority::lease(observer.attach_id()),
            observer.attach_id(),
            ms(8),
        ),
        LeaseRefusal::StaleGeneration {
            current_generation: 1,
            current_revision: before.state_revision
        }
    );
    assert_eq!(harness.host.snapshot(session_id).expect("snapshot"), before);
}

#[test]
fn pre_bind_cancel_blocks_bind_and_completes_not_started() {
    let mut harness = Harness::new();
    let (session_id, token) = harness.create("term-prebind", 1);

    let accepted = harness
        .host
        .abort(
            session_id,
            0,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Cancel,
            ms(2),
        )
        .expect("generation-0 abort");
    let StopOutcome::PreBindCancel(accepted) = accepted else {
        panic!("expected a pre-bind cancel intent");
    };
    assert!(!accepted.already_accepted);
    assert_eq!(accepted.reason, TerminalReason::ConsumerCancel);
    assert!(harness.control(session_id, 2).pending_cancel);
    assert_eq!(
        harness.host.snapshot(session_id).expect("snapshot").state,
        LifecycleState::Created
    );

    // The spawn path observes the committed intent and never binds.
    assert_eq!(
        harness.host.bind(session_id, ms(3)),
        Err(LeaseRefusal::CancelPending)
    );

    let repeated = harness
        .host
        .abort(
            session_id,
            0,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Cancel,
            ms(3),
        )
        .expect("repeat abort");
    let StopOutcome::PreBindCancel(repeated) = repeated else {
        panic!("expected the committed intent");
    };
    assert!(repeated.already_accepted);
    assert_eq!(repeated.reason, TerminalReason::ConsumerCancel);

    let termination = harness
        .host
        .complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, ms(4))
        .expect("complete cancel");
    let Termination::Completed(notice) = termination else {
        panic!("first completion must emit the notice");
    };
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.session.generation, 0);
    assert_eq!(notice.session.state_revision, 1);
    assert_eq!(notice.session.attach_count, 0);
    assert_eq!(notice.reason, TerminalReason::ConsumerCancel);
    assert_eq!(notice.cleanup, CleanupClass::NotStarted);
    assert_eq!(notice.exit, ExitInformation::NotApplicable);
    assert_eq!(notice.actor, Actor::ControlToken);
    assert!(notice.attached.is_empty());

    assert!(matches!(
        harness
            .host
            .complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, ms(5))
            .expect("duplicate completion"),
        Termination::AlreadyTerminal { .. }
    ));
    let repeated_abort = harness
        .host
        .abort(
            session_id,
            0,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Cancel,
            ms(5),
        )
        .expect("duplicate abort");
    assert!(matches!(
        repeated_abort,
        StopOutcome::AlreadyTerminal { .. }
    ));
    assert_eq!(
        harness.control(session_id, 5).token_state,
        ControlTokenState::Retired
    );
    assert_refused!(
        harness.host.claim_controller(session_id, 0, &token, ms(6)),
        LeaseRefusal::NotRunning {
            state: LifecycleState::Terminated
        }
    );

    // The spawn syscall had already returned and the unbound child was
    // terminated: the same intent completes with `swept`.
    let (swept_session, swept_token) = harness.create("term-prebind-swept", 1);
    let StopOutcome::PreBindCancel(intent) = harness
        .host
        .abort(
            swept_session,
            0,
            &ControlAuthority::token(&swept_token),
            ConsumerStopCause::Cancel,
            ms(2),
        )
        .expect("pre-bind abort")
    else {
        panic!("pre-bind cancel intent");
    };
    assert_eq!(intent.reason, TerminalReason::ConsumerCancel);
    let Termination::Completed(swept) = harness
        .host
        .complete_pre_bind_cancel(swept_session, CleanupClass::Swept, ms(3))
        .expect("complete swept")
    else {
        panic!("terminal notice");
    };
    assert_eq!(swept.reason, TerminalReason::ConsumerCancel);
    assert_eq!(swept.cleanup, CleanupClass::Swept);
    assert_eq!(swept.session.generation, 0);
}

#[test]
fn generation_zero_stop_loses_to_a_committed_bind() {
    let mut harness = Harness::new();
    let (session_id, token) = harness.create("term-bind-race", 1);
    harness.bind(session_id, 2);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    assert_eq!(
        harness.host.abort(
            session_id,
            0,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Cancel,
            ms(3),
        ),
        Err(LeaseRefusal::StaleGeneration {
            current_generation: 1,
            current_revision: before.state_revision
        })
    );
    assert_eq!(
        harness.host.kill(session_id, 0, ms(3)),
        Err(LeaseRefusal::StaleGeneration {
            current_generation: 1,
            current_revision: before.state_revision
        })
    );
    let after = harness.host.snapshot(session_id).expect("snapshot");
    assert_eq!(after, before);
    assert_eq!(after.state, LifecycleState::Running);
}

#[test]
fn abort_grace_then_kill_escalation_keeps_the_consumer_reason() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-grace", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);

    let outcome = harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(10),
        )
        .expect("abort accepted");
    let StopOutcome::Accepted(accepted) = outcome else {
        panic!("expected an accepted stop");
    };
    assert_eq!(accepted.stage, AbortStage::Graceful);
    assert_eq!(accepted.deadline, ms(60));
    assert_eq!(accepted.actor, Actor::Lease(controller.attach_id()));
    assert!(!accepted.already_accepted);
    assert_eq!(accepted.session.state, LifecycleState::Stopping);

    // D-09: observer attach is allowed while stopping; a controller claim
    // is not. The observer receives the terminal notice at the transition.
    let late_observer = harness.attach(session_id, 11);
    assert_eq!(
        harness
            .host
            .lease(late_observer.attach_id())
            .expect("lease")
            .role(),
        LeaseRole::Observer
    );
    assert_refused!(
        harness.host.claim_controller(session_id, 1, &token, ms(11)),
        LeaseRefusal::SessionStopping
    );

    assert_eq!(
        harness
            .host
            .stop_view(session_id, ms(59))
            .expect("stop")
            .stage,
        AbortStage::Graceful
    );
    assert_eq!(harness.host.escalate_abort(session_id, ms(59)), Ok(None));
    assert_eq!(
        harness
            .host
            .stop_view(session_id, ms(60))
            .expect("stop")
            .stage,
        AbortStage::KillDue
    );
    let first = harness
        .host
        .escalate_abort(session_id, ms(60))
        .expect("escalation")
        .expect("kill is due");
    assert!(first.first);
    let second = harness
        .host
        .escalate_abort(session_id, ms(61))
        .expect("escalation")
        .expect("kill remains due");
    assert!(!second.first);

    let termination = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(62),
        )
        .expect("terminal transition");
    let Termination::Completed(notice) = termination else {
        panic!("first termination must emit the notice");
    };
    assert_eq!(notice.reason, TerminalReason::ConsumerCancel);
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.cleanup, CleanupClass::Swept);
    assert_eq!(notice.exit, ExitInformation::NotApplicable);
    assert_eq!(notice.actor, Actor::Lease(controller.attach_id()));
    let mut attached = notice.attached.clone();
    attached.sort();
    let mut expected = vec![controller.attach_id(), late_observer.attach_id()];
    expected.sort();
    assert_eq!(attached, expected);
}

#[test]
fn natural_exit_and_spawn_failure_classification() {
    let mut harness = Harness::new();
    let (exit_session, _) = harness.create("term-exit", 1);
    harness.bind(exit_session, 2);
    let Termination::Completed(exit) = harness
        .host
        .record_termination(
            exit_session,
            ExitObservation::Exited { code: 7 },
            CleanupClass::Swept,
            ms(3),
        )
        .expect("exit classified")
    else {
        panic!("terminal notice");
    };
    assert_eq!(exit.session.state, LifecycleState::Exited);
    assert_eq!(exit.reason, TerminalReason::WorkerExit);
    assert_eq!(exit.exit, ExitInformation::ExitCode(7));
    assert_eq!(exit.actor, Actor::Worker);

    let (signal_session, _) = harness.create("term-signal", 1);
    harness.bind(signal_session, 2);
    let Termination::Completed(signal) = harness
        .host
        .record_termination(
            signal_session,
            ExitObservation::Signalled { signal: 9 },
            CleanupClass::Swept,
            ms(3),
        )
        .expect("signal classified")
    else {
        panic!("terminal notice");
    };
    assert_eq!(signal.session.state, LifecycleState::Failed);
    assert_eq!(signal.reason, TerminalReason::WorkerSignal);
    assert_eq!(signal.exit, ExitInformation::Signal(9));

    let (spawn_session, _) = harness.create("term-spawn", 1);
    let Termination::Completed(spawn) = harness
        .host
        .record_termination(
            spawn_session,
            ExitObservation::SpawnFailed,
            CleanupClass::NotStarted,
            ms(2),
        )
        .expect("spawn failure classified")
    else {
        panic!("terminal notice");
    };
    assert_eq!(spawn.session.state, LifecycleState::Failed);
    assert_eq!(spawn.reason, TerminalReason::SpawnFailed);
    assert_eq!(spawn.cleanup, CleanupClass::NotStarted);
    assert_eq!(spawn.session.generation, 0);

    let (supervision_session, _) = harness.create("term-supervision", 1);
    harness.bind(supervision_session, 2);
    let Termination::Completed(supervision) = harness
        .host
        .record_termination(
            supervision_session,
            ExitObservation::SupervisionFailed,
            CleanupClass::Unverified,
            ms(3),
        )
        .expect("supervision failure classified")
    else {
        panic!("terminal notice");
    };
    assert_eq!(supervision.session.state, LifecycleState::Failed);
    assert_eq!(supervision.reason, TerminalReason::SupervisionFailed);
    assert_eq!(supervision.cleanup, CleanupClass::Unverified);

    let (created_session, _) = harness.create("term-unexpected", 1);
    assert_eq!(
        harness.host.record_termination(
            created_session,
            ExitObservation::Exited { code: 0 },
            CleanupClass::Swept,
            ms(2),
        ),
        Err(LeaseRefusal::UnexpectedExit {
            state: LifecycleState::Created
        })
    );
    let (running_session, _) = harness.create("term-spawn-late", 1);
    harness.bind(running_session, 2);
    assert_eq!(
        harness.host.record_termination(
            running_session,
            ExitObservation::SpawnFailed,
            CleanupClass::Swept,
            ms(3),
        ),
        Err(LeaseRefusal::UnexpectedExit {
            state: LifecycleState::Running
        })
    );
    assert_eq!(
        harness.host.record_termination(
            running_session,
            ExitObservation::Signalled { signal: 0 },
            CleanupClass::Swept,
            ms(3),
        ),
        Err(LeaseRefusal::UnexpectedExit {
            state: LifecycleState::Running
        })
    );
}

#[test]
fn abort_then_natural_exit_keeps_the_first_effective_cause() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-abort-race", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 3);
    harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Timeout,
            ms(4),
        )
        .expect("timeout abort accepted");

    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::Exited { code: 0 },
            CleanupClass::Swept,
            ms(5),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::ConsumerTimeout);
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.exit, ExitInformation::NotApplicable);
    assert_eq!(notice.cleanup, CleanupClass::Swept);
}

#[test]
fn operator_stop_reasons_use_the_contract_grace_windows() {
    let mut harness = Harness::new();
    let (drain_session, _create_token) = harness.create("term-drain", 1);
    harness.bind(drain_session, 2);
    let StopOutcome::Accepted(drain) = harness
        .host
        .operator_stop(drain_session, 1, DaemonStopCause::DaemonStop, ms(10))
        .expect("drain stop")
    else {
        panic!("accepted stop");
    };
    assert_eq!(drain.deadline, ms(80), "drain uses the 70 ms drain grace");
    assert_eq!(drain.actor, Actor::Operator);
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            drain_session,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(11),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::DaemonStop);
    assert_eq!(notice.session.state, LifecycleState::Terminated);

    let (shutdown_session, _) = harness.create("term-shutdown", 1);
    harness.bind(shutdown_session, 2);
    let StopOutcome::Accepted(shutdown) = harness
        .host
        .operator_stop(shutdown_session, 1, DaemonStopCause::HostShutdown, ms(10))
        .expect("shutdown stop")
    else {
        panic!("accepted stop");
    };
    assert_eq!(shutdown.deadline, ms(80));
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            shutdown_session,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(11),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::HostShutdown);

    let (abort_session, _) = harness.create("term-operator-abort", 1);
    harness.bind(abort_session, 2);
    let StopOutcome::Accepted(abort) = harness
        .host
        .operator_stop(abort_session, 1, DaemonStopCause::OperatorAbort, ms(10))
        .expect("operator abort")
    else {
        panic!("accepted stop");
    };
    assert_eq!(
        abort.deadline,
        ms(60),
        "operator abort uses the 50 ms grace"
    );

    let (created_session, create_token) = harness.create("term-drain-created", 1);
    let StopOutcome::PreBindCancel(prebind) = harness
        .host
        .operator_stop(created_session, 0, DaemonStopCause::DaemonStop, ms(12))
        .expect("pre-bind drain")
    else {
        panic!("pre-bind cancel intent");
    };
    assert_eq!(prebind.reason, TerminalReason::DaemonStop);
    assert_eq!(
        harness.host.bind(created_session, ms(13)),
        Err(LeaseRefusal::CancelPending)
    );
    let Termination::Completed(notice) = harness
        .host
        .complete_pre_bind_cancel(created_session, CleanupClass::NotStarted, ms(14))
        .expect("complete")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::DaemonStop);
    assert_eq!(notice.cleanup, CleanupClass::NotStarted);
    assert!(create_token.expose_secret().len() == 32);
}

#[test]
fn kill_is_immediate_operator_abort_and_keeps_an_accepted_first_cause() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-kill", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 3);
    let outcome = harness
        .host
        .kill(session_id, 1, ms(10))
        .expect("kill accepted");
    let StopOutcome::Accepted(accepted) = outcome else {
        panic!("accepted stop");
    };
    assert_eq!(accepted.stage, AbortStage::KillDue);
    assert_eq!(accepted.deadline, ms(10));
    assert_eq!(accepted.actor, Actor::Operator);
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(11),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::OperatorAbort);

    // A kill during a consumer abort only escalates; the first effective
    // cause stays the consumer reason.
    let (race_session, create_token) = harness.create("term-kill-race", 1);
    harness.bind(race_session, 2);
    let (race_controller, _) = harness.claim(race_session, 1, &create_token, 3);
    harness
        .host
        .abort(
            race_session,
            1,
            &ControlAuthority::lease(race_controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(10),
        )
        .expect("consumer abort");
    let StopOutcome::Accepted(escalated) = harness
        .host
        .kill(race_session, 1, ms(20))
        .expect("escalating kill")
    else {
        panic!("accepted stop");
    };
    assert!(escalated.already_accepted);
    assert_eq!(escalated.stage, AbortStage::KillDue);
    assert_eq!(escalated.deadline, ms(20));
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            race_session,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(21),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::ConsumerCancel);

    // Generation-0 kill commits the pre-bind cancel intent.
    let (created_session, _) = harness.create("term-kill-created", 1);
    let StopOutcome::PreBindCancel(prebind) = harness
        .host
        .kill(created_session, 0, ms(2))
        .expect("pre-bind kill")
    else {
        panic!("pre-bind cancel intent");
    };
    assert_eq!(prebind.reason, TerminalReason::OperatorAbort);
    let Termination::Completed(notice) = harness
        .host
        .complete_pre_bind_cancel(created_session, CleanupClass::NotStarted, ms(3))
        .expect("complete")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::OperatorAbort);
    assert_eq!(notice.cleanup, CleanupClass::NotStarted);
    assert_eq!(controller.role(), LeaseRole::Controller);
}

#[test]
fn terminal_transition_invalidates_every_lease_and_emits_one_notice() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-terminal", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    let first = harness.attach(session_id, 4);
    let second = harness.attach(session_id, 5);

    harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(6),
        )
        .expect("abort");
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(7),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    let mut attached = notice.attached.clone();
    attached.sort();
    let mut expected = vec![
        controller.attach_id(),
        first.attach_id(),
        second.attach_id(),
    ];
    expected.sort();
    assert_eq!(attached, expected);
    assert_eq!(notice.session.attach_count, 0);
    assert!(!notice.session.attached);
    assert_eq!(notice.session.state, LifecycleState::Terminated);

    let view = harness.control(session_id, 7);
    assert_eq!(view.attach_count, 0);
    assert_eq!(view.controller, None);
    assert_eq!(view.token_state, ControlTokenState::Retired);
    assert_eq!(view.stop, None);
    assert!(!view.pending_cancel);
    for attach_id in [
        controller.attach_id(),
        first.attach_id(),
        second.attach_id(),
    ] {
        assert!(harness.host.lease(attach_id).is_none());
        assert_eq!(
            harness.host.heartbeat(attach_id, ms(8)),
            Err(LeaseRefusal::NotAttached)
        );
    }
    assert_refused!(
        harness.host.claim_controller(session_id, 1, &token, ms(8)),
        LeaseRefusal::NotRunning {
            state: LifecycleState::Terminated
        }
    );

    // Duplicate observations and aborts replay the terminal snapshot and
    // never produce a second notice.
    assert!(matches!(
        harness
            .host
            .record_termination(
                session_id,
                ExitObservation::DaemonTerminated,
                CleanupClass::Swept,
                ms(9),
            )
            .expect("duplicate"),
        Termination::AlreadyTerminal { .. }
    ));
    assert!(matches!(
        harness
            .host
            .abort(
                session_id,
                1,
                &ControlAuthority::lease(controller.attach_id()),
                ConsumerStopCause::Cancel,
                ms(9),
            )
            .expect("duplicate abort"),
        StopOutcome::AlreadyTerminal { .. }
    ));
    assert!(matches!(
        harness
            .host
            .kill(session_id, 1, ms(9))
            .expect("duplicate kill"),
        StopOutcome::AlreadyTerminal { .. }
    ));
    assert!(matches!(
        harness
            .host
            .operator_stop(session_id, 1, DaemonStopCause::DaemonStop, ms(9))
            .expect("duplicate stop"),
        StopOutcome::AlreadyTerminal { .. }
    ));
    assert_eq!(
        harness.control(session_id, 9).token_state,
        ControlTokenState::Retired
    );
    assert_refused!(
        harness.host.attach_observer(session_id, ms(9)),
        LeaseRefusal::NotRunning {
            state: LifecycleState::Terminated
        }
    );
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            first.attach_id(),
            ms(9),
        ),
        LeaseRefusal::NotRunning {
            state: LifecycleState::Terminated
        }
    );
    let no_op = harness
        .host
        .detach(
            session_id,
            first.attach_id(),
            &ControlAuthority::operator(),
            ms(9),
        )
        .expect("terminal detach replay");
    assert!(!no_op.was_attached);
    assert_eq!(
        no_op.session.state_revision, notice.session.state_revision,
        "a terminal detach replay changes nothing"
    );

    assert_eq!(harness.host.collect_retained(ms(600_007)), vec![session_id]);
    assert!(harness.host.control_view(session_id, ms(600_007)).is_none());
}

#[test]
fn the_token_never_appears_in_snapshots_notices_or_debug_output() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-secret", 1);
    harness.bind(session_id, 2);
    let (controller, rotated_token) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    harness
        .host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            ConsumerStopCause::Cancel,
            ms(5),
        )
        .expect("abort");
    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(6),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    let snapshot = notice.session.clone();
    let view = harness.control(session_id, 6);
    let debug = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        harness.host, snapshot, view, notice, controller
    );
    for secret in [&create_token, &rotated_token] {
        let hex = token_hex(secret);
        assert!(!debug.contains(&hex));
        assert!(!format!("{secret:?}").contains(&hex));
    }
    assert!(format!("{create_token:?}").contains("<redacted>"));
    assert_eq!(notice.attached.len(), 2);
    assert!(notice.attached.contains(&observer.attach_id()));
}

#[test]
fn entropy_failure_on_create_and_claim_leaves_no_state() {
    let registry_limits = RegistryLimits::new(4, 4, 4096, 600_000).expect("valid registry limits");
    let registry =
        SessionRegistry::with_entropy(registry_limits, Box::new(CounterEntropy { next: 1 }))
            .expect("registry");
    let mut host = SessionHost::with_parts(
        registry,
        LeaseLimits::new(4, 100, 50, 70).expect("limits"),
        Box::new(BudgetEntropy { remaining: 0 }),
    );
    let refusal = host.create(
        CreateSession {
            terminal_id: Some(TerminalId::supplied("term-entropy").expect("valid")),
            owner_peer: None,
        },
        ms(1),
    );
    assert!(matches!(refusal, Err(LeaseRefusal::EntropyUnavailable)));
    assert_eq!(host.registry().live_count(), 0);

    let registry = SessionRegistry::with_entropy(
        RegistryLimits::new(4, 4, 4096, 600_000).expect("valid"),
        Box::new(CounterEntropy { next: 1 }),
    )
    .expect("registry");
    let mut host = SessionHost::with_parts(
        registry,
        LeaseLimits::new(4, 100, 50, 70).expect("limits"),
        // One fill for the create token, none left for the claim's token.
        Box::new(BudgetEntropy { remaining: 1 }),
    );
    let created = host
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied("term-claim-entropy").expect("valid")),
                owner_peer: None,
            },
            ms(1),
        )
        .expect("create");
    let token = created.control_token;
    host.bind(created.session.session_id, ms(2)).expect("bind");
    let before = host.snapshot(created.session.session_id).expect("snapshot");
    assert!(matches!(
        host.claim_controller(created.session.session_id, 1, &token, ms(3)),
        Err(LeaseRefusal::EntropyUnavailable)
    ));
    assert_eq!(
        host.snapshot(created.session.session_id).expect("snapshot"),
        before
    );
    assert_eq!(
        host.control_view(created.session.session_id, ms(3))
            .expect("view")
            .attach_count,
        0
    );
}

#[test]
fn a_lease_from_another_session_is_not_attached() {
    let mut harness = Harness::new();
    let (first, _) = harness.create("term-cross-a", 1);
    let (second, _) = harness.create("term-cross-b", 1);
    let lease = harness.attach(first, 2);
    assert_eq!(
        harness.host.abort(
            second,
            0,
            &ControlAuthority::lease(lease.attach_id()),
            ConsumerStopCause::Cancel,
            ms(3),
        ),
        Err(LeaseRefusal::NotAttached)
    );
    assert_eq!(
        harness
            .host
            .heartbeat(lease.attach_id(), ms(3))
            .expect("own heartbeat")
            .session
            .session_id,
        first
    );
}

#[test]
fn an_observer_lease_with_the_current_token_is_never_control_at_vacant_control() {
    // D-09: an observer carries no abort, transfer, or detach-other authority.
    // Presenting the current token must not promote it; only a connection with
    // no lease can use the token, and only while control is vacant.
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-observer-token", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 3);
    harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(4),
        )
        .expect("controller self detach");
    let observer = harness.attach(session_id, 5);
    let other = harness.attach(session_id, 6);
    assert_eq!(
        harness.control(session_id, 6).controller,
        None,
        "control is vacant"
    );
    let before = harness.host.snapshot(session_id).expect("snapshot");

    let observer_with_token = ControlAuthority::lease(observer.attach_id()).with_token(&token);
    assert_eq!(
        harness.host.abort(
            session_id,
            1,
            &observer_with_token,
            ConsumerStopCause::Cancel,
            ms(7),
        ),
        Err(LeaseRefusal::ControllerRequired)
    );
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &observer_with_token,
            other.attach_id(),
            ms(7),
        ),
        LeaseRefusal::ControllerRequired
    );
    assert_eq!(
        harness
            .host
            .detach(session_id, other.attach_id(), &observer_with_token, ms(7),),
        Err(LeaseRefusal::ControllerRequired)
    );

    assert_eq!(
        harness.host.snapshot(session_id).expect("snapshot"),
        before,
        "no observer attempt applied anything"
    );
    assert_eq!(harness.control(session_id, 7).attach_count, 2);
    assert_eq!(
        harness.control(session_id, 7).token_state,
        ControlTokenState::Current,
        "the observer attempts did not rotate or retire the token"
    );

    // The same token on a connection with no lease still authorizes the
    // vacant-control path.
    assert!(matches!(
        harness
            .host
            .abort(
                session_id,
                1,
                &ControlAuthority::token(&token),
                ConsumerStopCause::Cancel,
                ms(8),
            )
            .expect("token-only abort"),
        StopOutcome::Accepted(_)
    ));
}

#[test]
fn a_control_token_is_bound_to_one_session() {
    let mut harness = Harness::new();
    let (first, first_create) = harness.create("term-token-a", 1);
    let (second, _) = harness.create("term-token-b", 1);
    harness.bind(first, 2);
    harness.bind(second, 2);
    let (controller, first_token) = harness.claim(first, 1, &first_create, 3);
    let second_before = harness.host.snapshot(second).expect("snapshot");

    // Cross-session replay: the first session's rotated token carries no
    // authority in the second, on either the claim or the token-only path.
    assert_refused!(
        harness
            .host
            .claim_controller(second, 1, &first_token, ms(4)),
        LeaseRefusal::InvalidControlToken
    );
    assert_eq!(
        harness.host.abort(
            second,
            1,
            &ControlAuthority::token(&first_token),
            ConsumerStopCause::Cancel,
            ms(4),
        ),
        Err(LeaseRefusal::InvalidControlToken)
    );
    assert_eq!(
        harness.host.snapshot(second).expect("snapshot"),
        second_before
    );
    assert_eq!(harness.control(second, 4).controller, None);

    // The token still verifies in its own session.
    harness
        .host
        .detach(
            first,
            controller.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(5),
        )
        .expect("self detach");
    assert!(
        harness
            .host
            .claim_controller(first, 1, &first_token, ms(5))
            .is_ok()
    );
}

#[test]
fn an_expired_controller_with_a_wrong_token_is_refused_and_survives() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-expired-wrong-token", 1);
    harness.bind(session_id, 2);
    let (controller, token) = harness.claim(session_id, 1, &create_token, 100);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    assert_refused!(
        harness
            .host
            .claim_controller(session_id, 1, &wrong_token(), ms(200)),
        LeaseRefusal::InvalidControlToken
    );
    assert_eq!(
        harness.host.snapshot(session_id).expect("snapshot"),
        before,
        "a refused claim does not touch the revision"
    );
    assert!(
        harness.host.lease(controller.attach_id()).is_some(),
        "the expired controller is not removed by a refused claim"
    );
    assert_eq!(harness.control(session_id, 200).controller, None);
    assert_eq!(
        harness.control(session_id, 200).token_state,
        ControlTokenState::Current
    );

    let expired = harness.host.expire_due(ms(200));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attach_id, controller.attach_id());
    assert_eq!(expired[0].revision, before.state_revision + 1);
    // The current token still reclaims control after the sweep.
    assert!(
        harness
            .host
            .claim_controller(session_id, 1, &token, ms(201))
            .is_ok()
    );
}

#[test]
fn transfer_targets_must_be_live_observers_of_this_session() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-target-a", 1);
    let (foreign_session, _) = harness.create("term-target-b", 1);
    harness.bind(session_id, 2);
    harness.bind(foreign_session, 2);
    let observer = harness.attach(session_id, 100);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 150);
    let foreign = harness.attach(foreign_session, 100);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    // A foreign-session handle is not a transfer target.
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            foreign.attach_id(),
            ms(160),
        ),
        LeaseRefusal::NotAttached
    );
    assert_eq!(harness.host.snapshot(session_id).expect("snapshot"), before);
    assert_eq!(
        harness.control(session_id, 160).controller,
        Some(controller.attach_id())
    );

    // The controller is live at 200 (claimed at 150); the observer is not.
    assert_refused!(
        harness.host.transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            observer.attach_id(),
            ms(200),
        ),
        LeaseRefusal::NotAttached
    );
    assert_eq!(harness.host.snapshot(session_id).expect("snapshot"), before);
    assert!(harness.host.lease(foreign.attach_id()).is_some());
    assert_eq!(harness.control(foreign_session, 160).attach_count, 1);
}

#[test]
fn after_a_transfer_the_demoted_controller_cannot_resize_or_authorize() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-demoted", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 4);
    let transferred = harness
        .host
        .transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(controller.attach_id()),
            observer.attach_id(),
            ms(5),
        )
        .expect("transfer");

    assert_refused!(
        harness.host.record_resize(controller.attach_id(), ms(6)),
        LeaseRefusal::ControllerRequired
    );
    assert_eq!(
        harness
            .host
            .authorize_controller(controller.attach_id(), ms(6)),
        Err(LeaseRefusal::ControllerRequired)
    );
    // The demoted lease is still attached for content reads and metadata.
    assert_eq!(
        harness
            .host
            .authorize_content_read(controller.attach_id(), ms(6))
            .expect("attached as observer")
            .role(),
        LeaseRole::Observer
    );

    let resized = harness
        .host
        .record_resize(transferred.controller, ms(6))
        .expect("the new controller may resize");
    assert_eq!(
        resized.state_revision,
        transferred.session.state_revision + 1
    );
}

#[test]
fn revocation_and_expiry_during_stopping_are_reflected_in_the_terminal_notice() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-stopping-revoke", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 150);
    let expiring = harness.attach(session_id, 100);
    let live = harness.attach(session_id, 180);
    harness
        .host
        .operator_stop(session_id, 1, DaemonStopCause::HostShutdown, ms(190))
        .expect("shutdown stop");

    let revoked = harness
        .host
        .detach(
            session_id,
            controller.attach_id(),
            &ControlAuthority::operator(),
            ms(191),
        )
        .expect("operator revoke during stopping");
    assert!(revoked.was_attached);
    assert!(revoked.revoked);
    assert!(revoked.control_retired);

    let expired = harness.host.expire_due(ms(200));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attach_id, expiring.attach_id());
    assert_eq!(expired[0].role, LeaseRole::Observer);

    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(201),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::HostShutdown);
    assert_eq!(notice.attached, vec![live.attach_id()]);
    assert_eq!(
        harness.control(session_id, 201).token_state,
        ControlTokenState::Retired
    );
}

#[test]
fn an_unswept_expired_lease_is_closed_by_the_terminal_notice() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-expired-terminate", 1);
    harness.bind(session_id, 2);
    let observer = harness.attach(session_id, 100);

    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::Exited { code: 0 },
            CleanupClass::Swept,
            ms(300),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(
        notice.attached,
        vec![observer.attach_id()],
        "the event layer must still account for the connection"
    );
    assert_eq!(notice.session.attach_count, 0);
    assert!(harness.host.lease(observer.attach_id()).is_none());
    assert!(
        harness.host.expire_due(ms(300)).is_empty(),
        "the terminal transition removed the expired record"
    );
}

#[test]
fn a_daemon_termination_without_an_accepted_stop_is_an_unexpected_exit() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-unexpected-daemon", 1);
    harness.bind(session_id, 2);
    let before = harness.host.snapshot(session_id).expect("snapshot");
    assert_eq!(
        harness.host.record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(3),
        ),
        Err(LeaseRefusal::UnexpectedExit {
            state: LifecycleState::Running
        })
    );
    assert_eq!(harness.host.snapshot(session_id).expect("snapshot"), before);
}

#[test]
fn pre_bind_cancel_completes_with_unverified_cleanup() {
    let mut harness = Harness::new();
    let (session_id, token) = harness.create("term-prebind-unverified", 1);
    let StopOutcome::PreBindCancel(intent) = harness
        .host
        .abort(
            session_id,
            0,
            &ControlAuthority::token(&token),
            ConsumerStopCause::Cancel,
            ms(2),
        )
        .expect("pre-bind abort")
    else {
        panic!("pre-bind cancel intent");
    };
    assert_eq!(intent.reason, TerminalReason::ConsumerCancel);

    let Termination::Completed(notice) = harness
        .host
        .complete_pre_bind_cancel(session_id, CleanupClass::Unverified, ms(3))
        .expect("complete unverified")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.cleanup, CleanupClass::Unverified);
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.session.generation, 0);
    assert_eq!(notice.reason, TerminalReason::ConsumerCancel);
    assert!(notice.attached.is_empty());
}

#[test]
fn expired_observers_do_not_consume_the_attach_cap() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-cap-expired", 1);
    for _ in 0..4 {
        harness.attach(session_id, 100);
    }
    assert_refused!(
        harness.host.attach_observer(session_id, ms(150)),
        LeaseRefusal::MaxAttaches { limit: 4 }
    );
    assert_eq!(
        harness.control(session_id, 200).attach_count,
        0,
        "every lease had expired"
    );

    let fresh = harness.attach(session_id, 200);
    assert_eq!(harness.control(session_id, 200).attach_count, 1);
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .attach_count,
        5,
        "the registry still holds the unswept records"
    );

    let expired = harness.host.expire_due(ms(200));
    assert_eq!(expired.len(), 4);
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .attach_count,
        1
    );
    assert!(harness.host.lease(fresh.attach_id()).is_some());
}

#[test]
fn expire_due_reports_the_post_detach_revision_for_each_removal() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-expire-revisions", 1);
    harness.bind(session_id, 2);
    let first = harness.attach(session_id, 100);
    let second = harness.attach(session_id, 100);
    let before = harness.host.snapshot(session_id).expect("snapshot");

    let expired = harness.host.expire_due(ms(200));
    assert_eq!(expired.len(), 2);
    let mut revisions: Vec<u64> = expired.iter().map(|entry| entry.revision).collect();
    revisions.sort_unstable();
    assert_eq!(
        revisions,
        vec![before.state_revision + 1, before.state_revision + 2]
    );
    let ids: HashSet<AttachId> = expired.iter().map(|entry| entry.attach_id).collect();
    assert_eq!(ids, HashSet::from([first.attach_id(), second.attach_id()]));
    assert_eq!(
        harness
            .host
            .snapshot(session_id)
            .expect("snapshot")
            .state_revision,
        before.state_revision + 2
    );
}

#[test]
fn daemon_stop_force_is_stop_then_kill_and_keeps_the_daemon_reason() {
    let mut harness = Harness::new();
    let (session_id, _) = harness.create("term-force", 1);
    harness.bind(session_id, 2);
    let StopOutcome::Accepted(graceful) = harness
        .host
        .operator_stop(session_id, 1, DaemonStopCause::DaemonStop, ms(10))
        .expect("drain")
    else {
        panic!("accepted stop");
    };
    assert_eq!(graceful.stage, AbortStage::Graceful);

    let StopOutcome::Accepted(forced) = harness
        .host
        .kill(session_id, 1, ms(10))
        .expect("force escalation")
    else {
        panic!("accepted stop");
    };
    assert!(forced.already_accepted);
    assert_eq!(forced.stage, AbortStage::KillDue);
    assert_eq!(forced.deadline, ms(10));

    let Termination::Completed(notice) = harness
        .host
        .record_termination(
            session_id,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(11),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(
        notice.reason,
        TerminalReason::DaemonStop,
        "kill must not overwrite the force stop's daemon reason"
    );
}

#[test]
fn a_foreign_detach_target_is_not_attached_and_survives() {
    let mut harness = Harness::new();
    let (first, first_token) = harness.create("term-foreign-detach-a", 1);
    let (second, _) = harness.create("term-foreign-detach-b", 1);
    harness.bind(first, 2);
    let (controller, _) = harness.claim(first, 1, &first_token, 3);
    let foreign = harness.attach(second, 4);
    let before = harness.host.snapshot(second).expect("snapshot");

    assert_refused!(
        harness.host.detach(
            first,
            foreign.attach_id(),
            &ControlAuthority::lease(controller.attach_id()),
            ms(5),
        ),
        LeaseRefusal::NotAttached
    );
    assert_refused!(
        harness.host.detach(
            first,
            foreign.attach_id(),
            &ControlAuthority::operator(),
            ms(5),
        ),
        LeaseRefusal::NotAttached
    );
    assert_eq!(harness.host.snapshot(second).expect("snapshot"), before);
    assert!(harness.host.lease(foreign.attach_id()).is_some());
}

#[test]
fn a_repeated_token_mint_is_refused_as_a_failed_rotation() {
    let registry = SessionRegistry::with_entropy(
        RegistryLimits::new(4, 4, 4096, 600_000).expect("valid registry limits"),
        Box::new(CounterEntropy { next: 1 }),
    )
    .expect("registry");
    let mut host = SessionHost::with_parts(
        registry,
        LeaseLimits::new(4, 100, 50, 70).expect("limits"),
        Box::new(ConstantEntropy),
    );
    let created = host
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied("term-mint-repeat").expect("valid")),
                owner_peer: None,
            },
            ms(1),
        )
        .expect("create");
    let token = created.control_token;
    host.bind(created.session.session_id, ms(2)).expect("bind");
    let before = host.snapshot(created.session.session_id).expect("snapshot");

    assert_eq!(
        host.claim_controller(created.session.session_id, 1, &token, ms(3))
            .err(),
        Some(LeaseRefusal::EntropyUnavailable)
    );
    assert_eq!(
        host.snapshot(created.session.session_id).expect("snapshot"),
        before
    );
    let view = host
        .control_view(created.session.session_id, ms(3))
        .expect("view");
    assert_eq!(view.attach_count, 0, "no controller lease was created");
    assert_eq!(
        view.token_state,
        ControlTokenState::Current,
        "the unrotated token is still current"
    );
}

#[test]
fn authorize_content_read_checks_liveness_for_both_roles() {
    let mut harness = Harness::new();
    let (session_id, create_token) = harness.create("term-content-read", 1);
    harness.bind(session_id, 2);
    let (controller, _) = harness.claim(session_id, 1, &create_token, 3);
    let observer = harness.attach(session_id, 100);

    assert_eq!(
        harness
            .host
            .authorize_content_read(controller.attach_id(), ms(4))
            .expect("controller read")
            .role(),
        LeaseRole::Controller
    );
    assert_eq!(
        harness
            .host
            .authorize_content_read(observer.attach_id(), ms(199))
            .expect("observer read")
            .role(),
        LeaseRole::Observer
    );
    assert_refused!(
        harness
            .host
            .authorize_content_read(observer.attach_id(), ms(200)),
        LeaseRefusal::NotAttached
    );
    // The raw lookup still returns the record; only the authorized entry point
    // treats it as detached.
    let expired = harness
        .host
        .lease(observer.attach_id())
        .expect("lookup returns the unswept record");
    assert!(expired.is_expired_at(ms(200)));
}
