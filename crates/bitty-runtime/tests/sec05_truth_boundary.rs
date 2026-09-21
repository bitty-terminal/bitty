//! SEC-05 (R-008 / P0-AC-016 / P0-AC-017): Terminal Truth audit.
//!
//! Proves at the runtime seam that no plugin-reachable path mutates
//! canonical terminal state: side-queue observations, snapshot reads, and
//! interception decisions leave the [`Runtime`] snapshot untouched; the
//! handed-out snapshot is a detached copy; and `protocol.register` /
//! `ui.protocol-register` dispatch is denied without an explicit grant
//! (P0-AC-017, first half). The second half — exclusive second-claimant
//! rejection — has no protocol-handler registry to exercise yet (see the
//! CTX-0626 audit note); only the capability/deny half is asserted here.

use bitty_plugin_host::{CapabilityId, HostObservation, PluginId};
use bitty_runtime::Runtime;

fn runtime_with_content() -> Runtime {
    let mut runtime = Runtime::with_defaults().expect("defaults must build");
    // Printable text plus a bell: exercises grid truth and the cold-event
    // bridge (Bell -> HostObservation) without any plugin involved.
    runtime.handle_pty_bytes(b"hello\x07");
    runtime
}

#[test]
fn observation_and_read_paths_leave_terminal_truth_untouched() {
    let mut runtime = runtime_with_content();
    let before = runtime.snapshot();

    // Read paths: repeated snapshots are pure.
    let _ = runtime.snapshot();
    assert_eq!(runtime.snapshot(), before);

    // Observation paths: bridge cold events into the bounded side queue and
    // drain them; producer-side drops are counted, truth is untouched.
    runtime.bridge_cold_to_side_queue();
    let observations = runtime.drain_plugin_observations();
    assert!(
        observations
            .iter()
            .any(|observation| matches!(observation, HostObservation::Bell)),
        "bell byte must surface as a bounded post-state observation"
    );
    assert_eq!(
        runtime.snapshot(),
        before,
        "SEC-05: cold-to-side bridging must not rewrite terminal truth"
    );

    // Direct side-queue traffic (host-mediated observations only).
    runtime.push_plugin_observation(HostObservation::Bell);
    let bounded = runtime.drain_plugin_observations_bounded(16);
    assert_eq!(bounded.len(), 1);
    assert_eq!(
        runtime.snapshot(),
        before,
        "SEC-05: side-queue traffic must not rewrite terminal truth"
    );
}

#[test]
fn interception_is_veto_only_and_stateless() {
    use bitty_plugin_host::InterceptionDecision as Decision;

    let runtime = runtime_with_content();
    let before = runtime.snapshot();

    // Veto-wins, fail-closed timeouts: pure functions over decisions.
    assert!(Runtime::intercept_command_dispatch(
        &[Decision::Approve],
        false
    ));
    assert!(!Runtime::intercept_command_dispatch(
        &[Decision::Approve, Decision::Veto],
        false
    ));
    assert!(!Runtime::intercept_command_dispatch(
        &[Decision::Approve],
        true
    ));

    assert_eq!(
        runtime.snapshot(),
        before,
        "SEC-05: interception decisions carry no truth mutation"
    );
}

#[test]
fn snapshot_handed_to_plugins_is_detached() {
    let runtime = runtime_with_content();
    let pristine = runtime.snapshot();

    let mut tampered = pristine.clone();
    assert!(tampered.cells.len() >= 2, "grid must hold cells");
    tampered.cells.swap(0, 1);
    tampered.generation = tampered.generation.wrapping_add(1);

    assert_eq!(
        runtime.snapshot(),
        pristine,
        "SEC-05: snapshot tampering must not reach live terminal truth"
    );
}

#[test]
fn protocol_registration_without_grant_is_denied() {
    let runtime = Runtime::with_defaults().expect("defaults must build");
    let plugin = PluginId::new("audit.no-grant").expect("valid plugin id");

    // P0-AC-017, first half: no grant record exists, so both protocol
    // registration identifiers fail closed under deny-by-default.
    for identifier in ["protocol.register", "ui.protocol-register"] {
        let capability = CapabilityId::parse(identifier).expect("known capability");
        assert!(
            runtime
                .check_command_grant(&plugin, "unrelated-hash", &capability)
                .is_err(),
            "SEC-05: '{identifier}' must be denied without an explicit grant"
        );
    }

    // The custom-URL-protocol identifier is flagged high-risk per RFC rule 3:
    // consent UI must present it distinctly, never granted implicitly.
    assert!(
        CapabilityId::parse("ui.protocol-register")
            .expect("known capability")
            .is_high_risk(),
        "SEC-05: 'ui.protocol-register' must stay high-risk"
    );
}
