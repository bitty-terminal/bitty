//! bitty#2609 / CTX-1623: public attach-lease, control-token, and stop
//! behavior.
//!
//! These tests use only the exported crate surface, so they also prove the
//! state machine is usable by the future IPC layer (bitty#2607) without
//! reaching into internals. Time is injected as [`MonotonicMs`]; nothing here
//! sleeps.

use bitty_daemon::lease::{
    Actor, ConsumerStopCause, ControlAuthority, ControlToken, ControlTokenState, DaemonStopCause,
    ExitObservation, LeaseLimits, LeaseRole, SessionHost, StopOutcome, Termination,
};
use bitty_daemon::registry::{
    CleanupClass, CreateSession, ExitInformation, LifecycleState, MonotonicMs, RegistryLimits,
    TerminalId, TerminalReason,
};

fn ms(value: u64) -> MonotonicMs {
    MonotonicMs::from_millis(value)
}

fn host(max_attaches: usize) -> SessionHost {
    SessionHost::new(
        RegistryLimits::new(8, 8, 4096, 600_000).expect("valid registry limits"),
        LeaseLimits::new(max_attaches, 100, 50, 70).expect("valid lease limits"),
    )
    .expect("host")
}

fn create(
    host: &mut SessionHost,
    terminal_id: &str,
    now: u64,
) -> (bitty_daemon::registry::SessionId, ControlToken) {
    let created = host
        .create(
            CreateSession {
                terminal_id: Some(TerminalId::supplied(terminal_id).expect("valid terminal id")),
                owner_peer: None,
            },
            ms(now),
        )
        .expect("create");
    (created.session.session_id, created.control_token)
}

fn token_hex(token: &ControlToken) -> String {
    token
        .expose_secret()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn attach_claim_transfer_and_terminal_flow_through_the_public_surface() {
    let mut host = host(8);
    let (session_id, create_token) = create(&mut host, "term-flow", 1);
    let bound = host.bind(session_id, ms(2)).expect("bind");
    assert_eq!(bound.generation, 1);

    let claimed = host
        .claim_controller(session_id, 1, &create_token, ms(3))
        .expect("claim");
    assert_eq!(claimed.lease.role(), LeaseRole::Controller);
    assert_ne!(token_hex(&create_token), token_hex(&claimed.control_token));

    let observer = host
        .attach_observer(session_id, ms(4))
        .expect("observer attach");
    assert_eq!(observer.lease.role(), LeaseRole::Observer);
    assert_eq!(observer.session.attach_count, 2);
    assert!(observer.session.attached);

    let transferred = host
        .transfer_control(
            session_id,
            1,
            &ControlAuthority::lease(claimed.lease.attach_id()),
            observer.lease.attach_id(),
            ms(5),
        )
        .expect("transfer");
    assert_eq!(
        transferred.previous_controller,
        Some(claimed.lease.attach_id())
    );
    assert_eq!(transferred.controller, observer.lease.attach_id());
    assert_eq!(transferred.session.attach_count, 2);
    assert_eq!(
        host.lease(claimed.lease.attach_id()).expect("lease").role(),
        LeaseRole::Observer
    );

    let accepted = host
        .abort(
            session_id,
            1,
            &ControlAuthority::lease(observer.lease.attach_id()),
            ConsumerStopCause::Cancel,
            ms(6),
        )
        .expect("abort");
    let StopOutcome::Accepted(accepted) = accepted else {
        panic!("expected an accepted stop");
    };
    assert_eq!(accepted.session.state, LifecycleState::Stopping);
    assert_eq!(accepted.deadline, ms(56));
    assert_eq!(accepted.actor, Actor::Lease(observer.lease.attach_id()));

    let Termination::Completed(notice) = host
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
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.reason, TerminalReason::ConsumerCancel);
    assert_eq!(notice.cleanup, CleanupClass::Swept);
    assert_eq!(notice.session.attach_count, 0);
    assert_eq!(notice.attached.len(), 2);

    let view = host.control_view(session_id, ms(7)).expect("control view");
    assert_eq!(view.token_state, ControlTokenState::Retired);
    assert!(matches!(
        host.abort(
            session_id,
            1,
            &ControlAuthority::lease(observer.lease.attach_id()),
            ConsumerStopCause::Cancel,
            ms(8),
        )
        .expect("duplicate abort"),
        StopOutcome::AlreadyTerminal { .. }
    ));
}

#[test]
fn expiry_detaches_without_aborting_or_changing_the_generation() {
    let mut host = host(8);
    let (session_id, _) = create(&mut host, "term-expiry", 1);
    host.bind(session_id, ms(2)).expect("bind");
    let observer = host
        .attach_observer(session_id, ms(10))
        .expect("observer attach");
    let revision = host.snapshot(session_id).expect("snapshot").state_revision;

    assert!(host.heartbeat(observer.lease.attach_id(), ms(109)).is_ok());
    assert!(
        host.heartbeat(observer.lease.attach_id(), ms(209)).is_err(),
        "elapsed TTL expires the lease"
    );
    let expired = host.expire_due(ms(209));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attach_id, observer.lease.attach_id());

    let snapshot = host.snapshot(session_id).expect("snapshot");
    assert_eq!(snapshot.state, LifecycleState::Running);
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.attach_count, 0);
    assert!(!snapshot.attached);
    assert_eq!(snapshot.state_revision, revision + 1);
}

#[test]
fn generation_zero_kill_commits_the_cancel_intent() {
    let mut host = host(8);
    let (session_id, _) = create(&mut host, "term-prebind", 1);
    let StopOutcome::PreBindCancel(intent) = host.kill(session_id, 0, ms(2)).expect("kill") else {
        panic!("expected a pre-bind cancel intent");
    };
    assert_eq!(intent.reason, TerminalReason::OperatorAbort);
    assert!(!intent.already_accepted);

    assert!(host.bind(session_id, ms(3)).is_err(), "bind is refused");
    let Termination::Completed(notice) = host
        .complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, ms(4))
        .expect("complete")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.session.state, LifecycleState::Terminated);
    assert_eq!(notice.session.generation, 0);
    assert_eq!(notice.reason, TerminalReason::OperatorAbort);
    assert_eq!(notice.cleanup, CleanupClass::NotStarted);
    assert!(matches!(
        host.complete_pre_bind_cancel(session_id, CleanupClass::NotStarted, ms(5))
            .expect("duplicate"),
        Termination::AlreadyTerminal { .. }
    ));
}

#[test]
fn attach_cap_is_public_and_never_evicts() {
    let mut host = host(2);
    let (session_id, _) = create(&mut host, "term-cap", 1);
    let first = host
        .attach_observer(session_id, ms(2))
        .expect("first attach");
    host.attach_observer(session_id, ms(3))
        .expect("second attach");
    let third = host.attach_observer(session_id, ms(4));
    assert!(
        matches!(
            third,
            Err(bitty_daemon::lease::LeaseRefusal::MaxAttaches { limit: 2 })
        ),
        "the cap sheds the new attach without evicting"
    );
    assert!(
        host.heartbeat(first.lease.attach_id(), ms(5)).is_ok(),
        "an existing lease survives the shed attach"
    );
    assert_eq!(
        host.control_view(session_id, ms(5))
            .expect("view")
            .attach_count,
        2
    );
}

#[test]
fn token_never_appears_in_public_snapshots_or_debug_output() {
    let mut host = host(8);
    let (session_id, create_token) = create(&mut host, "term-secret", 1);
    host.bind(session_id, ms(2)).expect("bind");
    let claimed = host
        .claim_controller(session_id, 1, &create_token, ms(3))
        .expect("claim");
    let Termination::Completed(notice) = host
        .record_termination(
            session_id,
            ExitObservation::Exited { code: 0 },
            CleanupClass::Swept,
            ms(4),
        )
        .expect("exit")
    else {
        panic!("terminal notice");
    };
    let rendered = format!(
        "{:?}|{:?}|{:?}|{:?}",
        host,
        host.snapshot(session_id).expect("snapshot"),
        notice,
        claimed.lease
    );
    for token in [&create_token, &claimed.control_token] {
        assert!(!rendered.contains(&token_hex(token)));
        assert!(format!("{token:?}").contains("<redacted>"));
    }
    assert_eq!(notice.reason, TerminalReason::WorkerExit);
    assert_eq!(notice.exit, ExitInformation::ExitCode(0));
}

#[test]
fn detach_and_revocation_follow_the_control_relation() {
    let mut host = host(8);
    let (session_id, create_token) = create(&mut host, "term-revoke", 1);
    host.bind(session_id, ms(2)).expect("bind");
    let claimed = host
        .claim_controller(session_id, 1, &create_token, ms(3))
        .expect("claim");
    let observer = host.attach_observer(session_id, ms(4)).expect("observer");

    let revoked = host
        .detach(
            session_id,
            observer.lease.attach_id(),
            &ControlAuthority::lease(claimed.lease.attach_id()),
            ms(5),
        )
        .expect("controller revoke");
    assert!(revoked.was_attached && revoked.revoked);
    assert_eq!(revoked.role, Some(LeaseRole::Observer));

    let retired = host
        .detach(
            session_id,
            claimed.lease.attach_id(),
            &ControlAuthority::operator(),
            ms(6),
        )
        .expect("operator revoke");
    assert!(retired.control_retired);
    let view = host.control_view(session_id, ms(6)).expect("view");
    assert_eq!(view.token_state, ControlTokenState::Retired);
    assert_eq!(view.controller, None);
    assert!(
        host.claim_controller(session_id, 1, &claimed.control_token, ms(7))
            .is_err(),
        "an operator revoke leaves no recovery path"
    );
}

#[test]
fn operator_stop_and_exit_classification_are_public() {
    let mut host = host(8);
    let (drain_session, _) = create(&mut host, "term-drain", 1);
    host.bind(drain_session, ms(2)).expect("bind");
    let StopOutcome::Accepted(stop) = host
        .operator_stop(drain_session, 1, DaemonStopCause::DaemonStop, ms(3))
        .expect("drain")
    else {
        panic!("accepted stop");
    };
    assert_eq!(stop.actor, Actor::Operator);
    assert_eq!(stop.deadline, ms(73));
    let Termination::Completed(notice) = host
        .record_termination(
            drain_session,
            ExitObservation::DaemonTerminated,
            CleanupClass::Swept,
            ms(4),
        )
        .expect("terminal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(notice.reason, TerminalReason::DaemonStop);

    let (signal_session, _) = create(&mut host, "term-signal", 1);
    host.bind(signal_session, ms(2)).expect("bind");
    let Termination::Completed(signal) = host
        .record_termination(
            signal_session,
            ExitObservation::Signalled { signal: 15 },
            CleanupClass::Swept,
            ms(3),
        )
        .expect("signal")
    else {
        panic!("terminal notice");
    };
    assert_eq!(signal.session.state, LifecycleState::Failed);
    assert_eq!(signal.reason, TerminalReason::WorkerSignal);
    assert_eq!(signal.exit, ExitInformation::Signal(15));

    let (spawn_session, _) = create(&mut host, "term-spawn", 1);
    let Termination::Completed(spawn) = host
        .record_termination(
            spawn_session,
            ExitObservation::SpawnFailed,
            CleanupClass::NotStarted,
            ms(2),
        )
        .expect("spawn")
    else {
        panic!("terminal notice");
    };
    assert_eq!(spawn.session.state, LifecycleState::Failed);
    assert_eq!(spawn.reason, TerminalReason::SpawnFailed);

    assert!(
        host.attach_observer(signal_session, ms(3)).is_err(),
        "a terminal session is read-only for attach"
    );
}
