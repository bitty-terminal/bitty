//! CTX-0720 integration: RUN-21 panel lease wired to the live host (W-140 Core side).
//!
//! The [`PanelRuntime`] host issues one [`PanelLease`] per panel at creation,
//! moves it only through the host acquire/release/handoff entries, validates
//! titles/descriptions at the host boundary, and clears the binding at
//! dispose. The RUN-22/RUN-23 interlock halves moved to `bitty-execution`
//! with the supervisor; this file keeps the 14 `lease_*` host tests only.

use bitty_runtime::registry::{
    BUS_SUBSCRIBE_CAPABILITY, Generation, LeaseEvent, LeaseHolder, LeaseState,
    MAX_LEASE_TERM_TICKS, PanelError, PanelLease, PanelRegistryConfig, PanelRuntime, PanelState,
    PanelType, WorkspaceId,
};

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
/// Host tick for lease tests (monotonic, never wall-clock).
const NOW: u64 = 1_000;
/// Tenure for lease tests in host ticks.
const TERM: u64 = 100;

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
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
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
        host.acquire_panel_lease(id2, generation2, HOLDER_B, TERM, NOW),
        Ok(LeaseEvent::Acquired {
            holder: HOLDER_B,
            expires_at: NOW + TERM
        })
    );
}

#[test]
fn lease_acquire_release_round_trip_through_host() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    assert_eq!(
        host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW),
        Ok(LeaseEvent::Acquired {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        })
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        })
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
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("first acquire");
    let denial = host
        .acquire_panel_lease(id, generation, HOLDER_B, TERM, NOW)
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
        Ok(LeaseState::Occupied {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        }),
        "refusal changes nothing"
    );
}

#[test]
fn lease_handoff_moves_occupancy_without_idle_gap() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("acquire");
    assert_eq!(
        host.handoff_panel_lease(id, generation, HOLDER_A, HOLDER_B, NOW),
        Ok(LeaseEvent::Handoff {
            from: HOLDER_A,
            to: HOLDER_B,
            expires_at: NOW + TERM
        })
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied {
            holder: HOLDER_B,
            expires_at: NOW + TERM
        })
    );
    // A handoff from the departed holder is refused; occupancy is unchanged.
    let denial = host
        .handoff_panel_lease(id, generation, HOLDER_A, HOLDER_B, NOW)
        .expect_err("stale handoff must fail");
    assert!(
        matches!(denial, PanelError::LeaseDenied { .. }),
        "expected LeaseDenied, got {denial:?}"
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied {
            holder: HOLDER_B,
            expires_at: NOW + TERM
        })
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
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
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
        Ok(LeaseState::Occupied {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        })
    );
}

#[test]
fn lease_stale_handle_rejected_before_kernel() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    let stale = Generation(generation.0.wrapping_add(1000).max(1));
    assert!(matches!(
        host.acquire_panel_lease(id, stale, HOLDER_A, TERM, NOW),
        Err(PanelError::StaleHandle { .. })
    ));
    assert!(matches!(
        host.panel_lease_state(id, stale),
        Err(PanelError::StaleHandle { .. })
    ));
    // The valid handle still works afterwards: failure was fail-closed.
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
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
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("acquire");
    host.dispose_panel(id, generation).expect("dispose");
    assert!(
        host.panel_lease_state(id, generation).is_err(),
        "a disposed panel holds no lease"
    );
}

#[test]
fn lease_kernel_still_pure_beside_host() {
    // The host owns the binding, the clock, and the bus routing; the kernel
    // still owns the transition and only compares host-supplied ticks.
    let mut lease = PanelLease::idle();
    assert_eq!(lease.state(), LeaseState::Idle);
    assert_eq!(
        lease.acquire(HOLDER_A, TERM, NOW),
        Ok(LeaseEvent::Acquired {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        })
    );
    assert_eq!(
        lease.state(),
        LeaseState::Occupied {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        }
    );
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
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("acquire");
    assert_eq!(
        host.panel_state(id, generation)
            .expect("lifecycle unchanged"),
        PanelState::Created
    );
}

// ── #1095: bounded tenure, tick clock, bus routing, write gate ──────────────

#[test]
fn lease_bounded_term_enforced_at_host() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    for bad_term in [0, MAX_LEASE_TERM_TICKS + 1] {
        let denial = host
            .acquire_panel_lease(id, generation, HOLDER_A, bad_term, NOW)
            .expect_err("out-of-bound term must fail");
        match &denial {
            PanelError::LeaseDenied { reason, .. } => assert!(
                reason.starts_with("invalid_term"),
                "stable audit name first, got {reason}"
            ),
            other => panic!("expected LeaseDenied, got {other:?}"),
        }
    }
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Idle),
        "refusals acquire nothing"
    );
    host.acquire_panel_lease(id, generation, HOLDER_A, MAX_LEASE_TERM_TICKS, NOW)
        .expect("max term acquires");
}

#[test]
fn lease_expiry_sweep_and_write_gate() {
    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("acquire");
    // Live tenure: the occupant may write.
    assert!(
        host.check_panel_write(id, generation, HOLDER_A, NOW)
            .is_ok()
    );
    assert!(
        !host
            .lease_is_expired(id, generation, NOW)
            .expect("expiry readable")
    );
    // Past the deadline: writes deny, the lapse is reported, and the lease
    // still reads occupied until swept.
    let denial = host
        .check_panel_write(id, generation, HOLDER_A, NOW + TERM)
        .expect_err("lapsed tenure writes nothing");
    match &denial {
        PanelError::LeaseDenied { reason, .. } => assert!(
            reason.starts_with("expired"),
            "stable audit name first, got {reason}"
        ),
        other => panic!("expected LeaseDenied, got {other:?}"),
    }
    assert!(
        host.lease_is_expired(id, generation, NOW + TERM)
            .expect("lapse readable")
    );
    assert_eq!(
        host.panel_lease_state(id, generation),
        Ok(LeaseState::Occupied {
            holder: HOLDER_A,
            expires_at: NOW + TERM
        })
    );
    // Sweep moves the lapsed tenure back to idle and reports it.
    assert_eq!(
        host.sweep_expired_leases(NOW),
        Vec::new(),
        "live sweep moves nothing"
    );
    let swept = host.sweep_expired_leases(NOW + TERM);
    assert_eq!(swept, vec![(id, LeaseEvent::Expired { holder: HOLDER_A })]);
    assert_eq!(host.panel_lease_state(id, generation), Ok(LeaseState::Idle));
    assert!(
        host.check_panel_write(id, generation, HOLDER_A, NOW + TERM)
            .is_err(),
        "idle panels write nothing"
    );
    // The panel is acquirable again after the sweep.
    host.acquire_panel_lease(id, generation, HOLDER_B, TERM, NOW + TERM)
        .expect("re-acquire after sweep");
}

#[test]
fn lease_transitions_route_to_bus() {
    use bitty_runtime::registry::lease_event_topic;

    let mut host = host();
    let (id, generation) = create_terminal(&mut host);
    let topic = lease_event_topic().expect("lease topic mints");
    assert_eq!(topic.as_str(), "bitty.panel:lifecycle.lease-changed");
    host.declare_topic(topic.as_str()).expect("topic declared");
    host.grant_capability(id, generation, BUS_SUBSCRIBE_CAPABILITY)
        .expect("subscribe capability");
    host.subscribe(id, generation, &topic).expect("subscribed");
    host.acquire_panel_lease(id, generation, HOLDER_A, TERM, NOW)
        .expect("acquire");
    let events = host
        .drain_batch(id, generation, topic.as_str(), 8, 8192)
        .expect("drain acquire event");
    assert_eq!(events.len(), 1, "acquire routes exactly one bus event");
    assert_eq!(events[0].topic, topic);
    let payload = events[0].payload.as_str();
    assert!(payload.contains("lease=acquired"), "{payload}");
    assert!(payload.contains("holder-7"), "{payload}");
    assert!(!payload.contains("Tracks"), "{payload}");
    host.release_panel_lease(id, generation, HOLDER_A)
        .expect("release");
    let events = host
        .drain_batch(id, generation, topic.as_str(), 8, 8192)
        .expect("drain release event");
    assert_eq!(events.len(), 1, "release routes exactly one bus event");
    assert!(
        events[0].payload.as_str().contains("lease=released"),
        "{payload}"
    );
}
