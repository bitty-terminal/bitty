//! CTX-0720 integration: RUN kernels wired to live paths.
//!
//! Each seam is exercised through its live owner, not the pure kernel:
//!
//! - RUN-21 (#1052): the [`PanelRuntime`] host issues one [`PanelLease`]
//!   per panel at creation, moves it only through the host
//!   acquire/release/handoff entries, validates titles/descriptions at the
//!   host boundary, and clears the binding at dispose.
//! - RUN-22 (#1053): the [`JobRegistry`] input boundary re-checks the
//!   recorded echo state and interaction class on every `write_input_as`
//!   dispatch and refuses with a typed denial while the interlock holds.
//! - RUN-23 (#1054): the [`JobRegistry`] agent-command boundary classifies
//!   every `spawn_as`/`spawn_checked_as` argv with [`classify_argv`] and
//!   refuses hard-deny and consent-gated shapes before tracking, threading,
//!   or execution.
//!
//! Spawned children are this test binary (hermetic, argv-first, no shell),
//! following the `job_capability_ops.rs` precedent. Denied or gated spawns
//! never start a process; allowed spawns run the `quiet` child and are
//! cancelled at the end of the test.

use std::time::Duration;

use bitty_runtime::execution::{
    EchoState, InteractionClass, JobError, JobPrincipal, JobRegistry, JobSpec, LeaseEvent,
    LeaseHolder, LeaseState, OperationIntent, PanelLease,
};
use bitty_runtime::registry::{
    Generation, PanelError, PanelRegistryConfig, PanelRuntime, PanelState, PanelType, WorkspaceId,
};

const HELPER_ENV: &str = "__BITTY_RUN_WIRING_HELPER";

/// Child entry point: selected by `HELPER_ENV`, runs only in spawned
/// children (the parent suite runs it as a no-op and it passes).
#[test]
fn __bitty_run_wiring_helper_entry__() {
    match std::env::var(HELPER_ENV).as_deref() {
        Ok("quiet") => {}
        Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
        _ => {}
    }
}

fn helper_exe() -> String {
    std::env::current_exe()
        .expect("test binary path")
        .to_string_lossy()
        .into_owned()
}

/// Argv-first spec for the test binary in `mode`, with the explicit
/// environment the closed-env backend requires.
fn helper_spec(mode: &str) -> JobSpec {
    JobSpec::new(
        helper_exe(),
        vec![
            "__bitty_run_wiring_helper_entry__".to_owned(),
            "--nocapture".to_owned(),
        ],
    )
    .with_env(
        bitty_ipc::execution::EnvPolicy::explicit(vec![(HELPER_ENV.to_owned(), mode.to_owned())])
            .expect("explicit env"),
    )
}

fn owner(name: &str) -> JobPrincipal {
    JobPrincipal::new(name).expect("valid principal")
}

fn host() -> PanelRuntime {
    PanelRuntime::new(PanelRegistryConfig::default()).expect("panel host")
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new(1)
}

fn create_terminal(host: &mut PanelRuntime) -> (bitty_runtime::registry::PanelId, Generation) {
    let handle = host
        .create_panel(PanelType::Terminal, Some(workspace()))
        .expect("panel created");
    (handle.id, handle.generation)
}

const HOLDER_A: LeaseHolder = LeaseHolder(7);
const HOLDER_B: LeaseHolder = LeaseHolder(9);

// ── RUN-21: lease issuance and transitions through the panel host ───────────

#[test]
fn lease_issued_idle_at_create() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    assert_eq!(host.panel_lease_state(id, generation), Ok(LeaseState::Idle));
    assert_eq!(
        host.panel_description(id, generation),
        Ok((None, None)),
        "no orientation text until stored"
    );
}

#[test]
fn lease_recreated_panel_starts_idle_after_dispose() {
    // CTX-0727 (#1315): `create_panel` unconditionally resets the lease
    // binding, and `dispose_panel` clears it — a panel created after a
    // dispose (id reuse or not) always starts `Idle`, never inheriting a
    // previous occupant's lease.
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("acquire");
    host.dispose_panel(id, generation).expect("dispose");
    let (id2, generation2) = create_terminal(&mut host);
    assert_eq!(
        host.panel_lease_state(id2, generation2),
        Ok(LeaseState::Idle),
        "recreated panel must start idle"
    );
    // The recreated binding is live: it can be acquired fresh.
    assert_eq!(
        host.acquire_panel_lease(id2, generation2, HOLDER_B),
        Ok(LeaseEvent::Acquired { holder: HOLDER_B })
    );
}

#[test]
fn lease_acquire_release_round_trip_through_host() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    assert_eq!(
        host.acquire_panel_lease(id, generation, HOLDER_A),
        Ok(LeaseEvent::Acquired { holder: HOLDER_A })
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied { holder: HOLDER_A })
    );
    assert_eq!(
        host.release_panel_lease(id, generation, HOLDER_A),
        Ok(LeaseEvent::Released { holder: HOLDER_A })
    );
    assert_eq!(host.panel_lease_state(id, generation), Ok(LeaseState::Idle));
}

#[test]
fn lease_double_acquire_denied_keeps_holder() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("first acquire");
    let denial = host
        .acquire_panel_lease(id, generation, HOLDER_B)
        .expect_err("second acquire must fail");
    match &denial {
        PanelError::LeaseDenied { panel_id, reason } => {
            assert_eq!(*panel_id, id);
            assert!(
                reason.starts_with("already_occupied"),
                "stable audit name first, got {reason}"
            );
        }
        other => panic!("expected LeaseDenied, got {other:?}"),
    }
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied { holder: HOLDER_A }),
        "refusal changes nothing"
    );
}

#[test]
fn lease_handoff_moves_occupancy_without_idle_gap() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("acquire");
    assert_eq!(
        host.handoff_panel_lease(id, generation, HOLDER_A, HOLDER_B),
        Ok(LeaseEvent::Handoff {
            from: HOLDER_A,
            to: HOLDER_B
        })
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied { holder: HOLDER_B })
    );
    // A handoff from the departed holder is refused; occupancy is unchanged.
    let denial = host
        .handoff_panel_lease(id, generation, HOLDER_A, HOLDER_B)
        .expect_err("stale handoff must fail");
    assert!(
        matches!(denial, PanelError::LeaseDenied { .. }),
        "expected LeaseDenied, got {denial:?}"
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied { holder: HOLDER_B })
    );
}

#[test]
fn lease_idle_release_and_wrong_holder_release_denied() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    let idle_denial = host
        .release_panel_lease(id, generation, HOLDER_A)
        .expect_err("idle release must fail");
    assert!(
        matches!(idle_denial, PanelError::LeaseDenied { .. }),
        "expected LeaseDenied, got {idle_denial:?}"
    );
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("acquire");
    let holder_denial = host
        .release_panel_lease(id, generation, HOLDER_B)
        .expect_err("non-holder release must fail");
    match &holder_denial {
        PanelError::LeaseDenied { reason, .. } => assert!(
            reason.starts_with("not_holder"),
            "stable audit name first, got {reason}"
        ),
        other => panic!("expected LeaseDenied, got {other:?}"),
    }
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied { holder: HOLDER_A })
    );
}

#[test]
fn lease_stale_handle_rejected_before_kernel() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    let stale = Generation(generation.0.wrapping_add(1000).max(1));
    assert!(matches!(
        host.acquire_panel_lease(id, stale, HOLDER_A),
        Err(PanelError::StaleHandle { .. })
    ));
    assert!(matches!(
        host.panel_lease_state(id, stale),
        Err(PanelError::StaleHandle { .. })
    ));
    // The valid handle still works afterwards: failure was fail-closed.
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("valid handle unaffected");
}

#[test]
fn lease_description_bounds_enforced_at_host() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.set_panel_description(
        id,
        generation,
        "agent workstation",
        "Tracks the migration.\nSecond line.",
    )
    .expect("valid orientation text");
    assert_eq!(
        host.panel_description(id, generation),
        Ok((
            Some("agent workstation".to_owned()),
            Some("Tracks the migration.\nSecond line.".to_owned())
        ))
    );
    // Empty titles never reach chrome.
    assert!(matches!(
        host.set_panel_description(id, generation, "", "kept"),
        Err(PanelError::InvalidDescription { .. })
    ));
    // Control characters never reach chrome.
    assert!(matches!(
        host.set_panel_description(id, generation, "bad\x07title", "kept"),
        Err(PanelError::InvalidDescription { .. })
    ));
    // Over-long descriptions are refused.
    assert!(matches!(
        host.set_panel_description(id, generation, "kept", &"d".repeat(1025)),
        Err(PanelError::InvalidDescription { .. })
    ));
    // Refusals store nothing: the last valid text survives.
    assert_eq!(
        host.panel_description(id, generation).expect("read back").0,
        Some("agent workstation".to_owned())
    );
}

#[test]
fn lease_cleared_at_dispose() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("acquire");
    host.dispose_panel(id, generation).expect("dispose");
    assert!(
        host.panel_lease_state(id, generation).is_err(),
        "a disposed panel holds no lease"
    );
}

#[test]
fn lease_kernel_still_pure_beside_host() {
    // The host owns the binding; the kernel still owns the transition.
    // This pins the layering: no bus, clock, or agent symbol in the kernel.
    let mut lease = PanelLease::idle();
    assert_eq!(lease.state(), LeaseState::Idle);
    assert_eq!(
        lease.acquire(HOLDER_A),
        Ok(LeaseEvent::Acquired { holder: HOLDER_A })
    );
    assert_eq!(lease.state(), LeaseState::Occupied { holder: HOLDER_A });
}

#[test]
fn lease_lifecycle_state_untouched_by_host() {
    // Lease moves never disturb panel lifecycle: creation still lands in
    // `Created`, and the lease table is orthogonal to mount state.
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    assert_eq!(
        host.panel_state(id, generation)
            .expect("lifecycle readable"),
        PanelState::Created
    );
    host.acquire_panel_lease(id, generation, HOLDER_A)
        .expect("acquire");
    assert_eq!(
        host.panel_state(id, generation)
            .expect("lifecycle unchanged"),
        PanelState::Created
    );
}

// ── RUN-22: sensitive-input interlock at the input boundary ─────────────────

fn tracked_pipe_job(
    registry: &JobRegistry,
    principal: &JobPrincipal,
) -> bitty_runtime::execution::JobId {
    registry
        .spawn_as(principal.clone(), helper_spec("quiet"))
        .expect("pipe job tracked")
}

#[test]
fn input_gate_defaults_to_echo_on_safe() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run22-owner");
    let id = tracked_pipe_job(&registry, &principal);
    assert_eq!(
        registry.job_input_gate(id),
        Ok((EchoState::EchoOn, InteractionClass::SafeInteractive))
    );
    let _ = registry.cancel_as(&principal, id);
}

#[test]
fn input_gate_no_echo_denies_write_through_live_registry() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run22-owner");
    let id = tracked_pipe_job(&registry, &principal);
    registry
        .set_job_input_gate(id, EchoState::NoEcho, InteractionClass::SafeInteractive)
        .expect("host records echo loss");
    // The interlock wins over the stale safe label: text plays no role.
    let denial = registry
        .write_input_as(&principal, id, b"password\r")
        .expect_err("no-echo must deny");
    match &denial {
        JobError::SecureInputDenied { denial, .. } => {
            assert_eq!(denial, "target_in_secure_input_mode");
        }
        other => panic!("expected SecureInputDenied, got {other:?}"),
    }
    // The interlock runs before payload validation: even an empty payload
    // reports the gate, never a payload error.
    assert!(
        matches!(
            registry.write_input_as(&principal, id, b""),
            Err(JobError::SecureInputDenied { .. })
        ),
        "gate precedes payload checks"
    );
    // Restoring echo resumes dispatch: the denial suspended nothing else.
    registry
        .set_job_input_gate(id, EchoState::EchoOn, InteractionClass::SafeInteractive)
        .expect("host records echo restore");
    assert!(
        matches!(
            registry.write_input_as(&principal, id, b"echo-back"),
            Err(JobError::Unsupported { .. })
        ),
        "echo-on safe input passes the gate to backend gating (pipe stdin closed)"
    );
    let _ = registry.cancel_as(&principal, id);
}

#[test]
fn input_gate_confirmation_requires_human() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run22-owner");
    let id = tracked_pipe_job(&registry, &principal);
    registry
        .set_job_input_gate(
            id,
            EchoState::EchoOn,
            InteractionClass::PrivilegedConfirmation,
        )
        .expect("host sorts confirmation class");
    let denial = registry
        .write_input_as(&principal, id, b"yes\r")
        .expect_err("privileged confirmation needs a human");
    match &denial {
        JobError::SecureInputDenied { denial, .. } => {
            assert_eq!(denial, "confirmation_requires_human");
        }
        other => panic!("expected SecureInputDenied, got {other:?}"),
    }
    let _ = registry.cancel_as(&principal, id);
}

#[test]
fn input_gate_stale_secret_label_stays_denied_when_echo_on() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run22-owner");
    let id = tracked_pipe_job(&registry, &principal);
    registry
        .set_job_input_gate(id, EchoState::EchoOn, InteractionClass::SecretInput)
        .expect("host records inconsistent label");
    // An inconsistent label refuses rather than guessing safe.
    assert!(
        matches!(
            registry.write_input_as(&principal, id, b"anything"),
            Err(JobError::SecureInputDenied { .. })
        ),
        "stale secret label stays denied"
    );
    let _ = registry.cancel_as(&principal, id);
}

#[test]
fn input_gate_denies_before_existence_leak() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run22-owner");
    let stranger = owner("run22-stranger");
    let id = tracked_pipe_job(&registry, &principal);
    registry
        .set_job_input_gate(id, EchoState::NoEcho, InteractionClass::SecretInput)
        .expect("host records echo loss");
    // Authorization still runs first: an unauthorized caller learns
    // nothing, not even the gate state.
    assert!(
        matches!(
            registry.write_input_as(&stranger, id, b"probe"),
            Err(JobError::Denied { .. })
        ),
        "denied callers must not observe the interlock"
    );
    assert!(
        matches!(
            registry.set_job_input_gate(
                bitty_runtime::execution::JobId::from_raw(999_999).expect("nonzero"),
                EchoState::NoEcho,
                InteractionClass::SecretInput
            ),
            Err(JobError::UnknownJob(..))
        ),
        "gate writes to unknown jobs fail"
    );
    let _ = registry.cancel_as(&principal, id);
}

// ── RUN-23: command-risk verdict at the agent-command boundary ──────────────

#[test]
fn risk_escalation_wrapper_denied_before_tracking() {
    let registry = JobRegistry::with_capacity(8);
    let before = registry.len();
    let denial = registry
        .spawn_as(
            owner("run23-owner"),
            JobSpec::new("sudo", vec!["ls".to_owned()]),
        )
        .expect_err("privilege escalation must deny");
    match &denial {
        JobError::CommandRiskDenied { deny, .. } => {
            assert_eq!(deny, "privilege_escalation");
        }
        other => panic!("expected CommandRiskDenied, got {other:?}"),
    }
    assert_eq!(registry.len(), before, "denied spawn tracks nothing");
}

#[test]
fn risk_broad_root_delete_denied_before_tracking() {
    let registry = JobRegistry::with_capacity(8);
    let before = registry.len();
    let denial = registry
        .spawn_checked_as(
            owner("run23-owner"),
            JobSpec::new("rm", vec!["-rf".to_owned(), "/".to_owned()]),
            OperationIntent::Write,
        )
        .expect_err("broad-root recursive force delete must deny");
    match &denial {
        JobError::CommandRiskDenied { deny, .. } => {
            assert_eq!(deny, "broad_root_delete");
        }
        other => panic!("expected CommandRiskDenied, got {other:?}"),
    }
    assert_eq!(registry.len(), before, "denied spawn tracks nothing");
}

#[test]
fn risk_system_config_write_denied_with_declared_intent() {
    let registry = JobRegistry::with_capacity(8);
    let denial = registry
        .spawn_checked_as(
            owner("run23-owner"),
            JobSpec::new("tee", vec!["/etc/hosts".to_owned()]),
            OperationIntent::Write,
        )
        .expect_err("system-config write must deny");
    assert!(
        matches!(
            denial,
            JobError::CommandRiskDenied { ref deny, .. } if deny == "system_config_write"
        ),
        "expected system_config_write deny, got {denial:?}"
    );
}

#[test]
fn risk_narrow_destructive_shape_needs_consent() {
    let registry = JobRegistry::with_capacity(8);
    let before = registry.len();
    // Fail-closed without a ledger: consent-gated shapes refuse (PP-3 open
    // work), so nothing executes on this answer alone.
    let gated = registry
        .spawn_checked_as(
            owner("run23-owner"),
            JobSpec::new("rm", vec!["-rf".to_owned(), "/tmp/scratch".to_owned()]),
            OperationIntent::Write,
        )
        .expect_err("narrow recursive force delete needs consent");
    match &gated {
        JobError::CommandRiskNeedsConsent { tier, .. } => {
            assert_eq!(tier, "restricted");
        }
        other => panic!("expected CommandRiskNeedsConsent, got {other:?}"),
    }
    assert_eq!(registry.len(), before, "gated spawn tracks nothing");
}

#[test]
fn risk_allow_spawns_and_runs_under_declared_intent() {
    let registry = JobRegistry::with_capacity(8);
    let principal = owner("run23-owner");
    // Unknown binaries sort to Standard and proceed under the caller's
    // scope; the hermetic helper child proves the allow path end to end.
    let id = registry
        .spawn_checked_as(
            principal.clone(),
            helper_spec("quiet"),
            OperationIntent::Execute,
        )
        .expect("standard command allowed");
    assert_eq!(registry.len(), 1, "allowed spawn tracks exactly one job");
    let snapshot = registry.get_as(&principal, id).expect("job observable");
    assert_eq!(snapshot.spec.program, helper_exe());
    let _ = registry.cancel_as(&principal, id);
}

#[test]
fn risk_spec_bounds_checked_before_classification() {
    let registry = JobRegistry::with_capacity(8);
    // An empty executable fails CTX-0442 validation before the kernel ever
    // sees it: bounds first, risk second.
    let refused = registry
        .spawn_as(owner("run23-owner"), JobSpec::new("", vec![]))
        .expect_err("empty program must not classify");
    assert!(
        matches!(refused, JobError::InvalidSpec { .. }),
        "expected InvalidSpec, got {refused:?}"
    );
    assert_eq!(registry.len(), 0, "invalid spec tracks nothing");
}
