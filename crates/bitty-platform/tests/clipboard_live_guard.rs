//! Pure regressions for the live harness isolation precondition (PX-6838).

#[path = "support/clipboard_isolation.rs"]
#[allow(dead_code)]
mod clipboard_isolation;

use clipboard_isolation::{IsolationEvidence, run_if_isolated, validate};

fn isolated() -> IsolationEvidence {
    IsolationEvidence {
        linux: true,
        display_number: Some(23),
        wayland_present: false,
        server_pid: Some(120),
        display_lock_pid: Some(120),
        server_parent: Some(100),
        test_parent: Some(100),
        server_is_xvfb: true,
        server_uses_displayfd: true,
        server_disables_tcp: true,
        private_tmp: true,
        private_run: true,
        server_owns_display_socket: true,
    }
}

#[test]
fn isolated_xvfb_evidence_allows_native_access() {
    assert_eq!(validate(&isolated()), Ok(()));
}

#[test]
fn ordinary_gui_feature_or_opt_in_is_not_isolation() {
    assert!(validate(&IsolationEvidence::default()).is_err());
}

#[test]
fn every_isolation_requirement_is_checked_before_native_access() {
    let mutations: &[fn(&mut IsolationEvidence)] = &[
        |e| e.linux = false,
        |e| e.display_number = None,
        |e| e.wayland_present = true,
        |e| e.server_pid = None,
        |e| e.display_lock_pid = None,
        |e| e.display_lock_pid = Some(121),
        |e| e.server_parent = None,
        |e| e.test_parent = None,
        |e| e.server_parent = Some(101),
        |e| e.server_is_xvfb = false,
        |e| e.server_uses_displayfd = false,
        |e| e.server_disables_tcp = false,
        |e| e.private_tmp = false,
        |e| e.private_run = false,
        |e| e.server_owns_display_socket = false,
    ];
    for mutate in mutations {
        let mut evidence = isolated();
        mutate(&mut evidence);
        let mut native_calls = 0;
        assert!(
            run_if_isolated(&evidence, || {
                native_calls += 1;
            })
            .is_err()
        );
        assert_eq!(native_calls, 0, "invalid evidence: {evidence:?}");
    }
}

#[test]
fn valid_guard_executes_operation_once_and_propagates_panic() {
    let mut calls = 0;
    run_if_isolated(&isolated(), || calls += 1).unwrap();
    assert_eq!(calls, 1);
    assert!(
        std::panic::catch_unwind(|| {
            let _ = run_if_isolated(&isolated(), || panic!("native assertion failure"));
        })
        .is_err()
    );
}
