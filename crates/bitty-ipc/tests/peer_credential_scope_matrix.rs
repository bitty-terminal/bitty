//! R-011 IPC scope closure (SEC-08, Issue #1077).
//!
//! Full scope x action matrix over the generic IPC registry and the `bitty
//! ctl` control registry. Peer-credential and connected-stream regressions
//! live with the platform-bound IPC tests.
//!
//! Headless and network-free: no sockets, no threads, no wall-clock.

use bitty_ipc::bridge::BridgeClient;
use bitty_ipc::ctl::{all_control_methods, authorize_ctl_method, required_scope_for_ctl_method};
use bitty_ipc::error::{ErrorClass, IpcError};
use bitty_ipc::scope::{
    ConsentLedger, Scope, ScopeSet, all_known_methods, authorize_method, required_scope_for_method,
};
use bitty_ipc::wire::{validate_no_ambient_auth, validate_request_envelope};

fn expect_scope_denied(result: Result<Scope, IpcError>, method: &str, scope: Scope) {
    let err = result.unwrap_err();
    let (denied_scope, action) = match err {
        IpcError::ScopeDenied { scope, action } => (scope, action),
        other => panic!("{method}: expected ScopeDenied, got {other:?}"),
    };
    assert_eq!(denied_scope, scope.as_str(), "{method}: wrong scope");
    assert_eq!(action, method, "{method}: wrong action");
}

#[test]
fn client_asserted_scope_is_rejected_at_wire() {
    // A client that inserts scope/auth/role cannot escalate: rejected outright.
    for params in [
        br#"{"scope": "terminal.manage"}"#.as_slice(),
        br#"{"auth": "token123"}"#.as_slice(),
        br#"{"role": "admin"}"#.as_slice(),
    ] {
        assert!(validate_no_ambient_auth(params).is_err());
        assert!(
            validate_request_envelope(1, "id-1", "terminal.text", params).is_err(),
            "envelope must reject ambient authority"
        );
    }
    // Normal params still pass.
    assert!(
        validate_request_envelope(1, "id-1", "terminal.text", br#"{"terminal_id":"t:4"}"#).is_ok()
    );
}

// ── full scope x action matrix ──────────────────────────────────────────────

/// Expected `(method, required scope)` for the whole generic registry.
fn expected_matrix() -> &'static [(&'static str, Scope)] {
    &[
        ("terminal.list", Scope::TerminalInspect),
        ("terminal.text", Scope::TerminalInspect),
        ("terminal.get_text", Scope::TerminalInspect),
        ("terminal.snapshot", Scope::TerminalInspect),
        ("terminal.send", Scope::TerminalInput),
        ("terminal.input", Scope::TerminalInput),
        ("terminal.write", Scope::TerminalInput),
        ("terminal.close", Scope::TerminalManage),
        ("terminal.spawn", Scope::TerminalManage),
        ("terminal.kill", Scope::TerminalManage),
        ("terminal.manage", Scope::TerminalManage),
        ("view.list", Scope::ViewInspect),
        ("view.split", Scope::ViewManage),
        ("view.focus", Scope::ViewManage),
        ("view.close", Scope::ViewManage),
        ("view.create", Scope::ViewManage),
        ("view.manage", Scope::ViewManage),
        ("config.show", Scope::ConfigInspect),
        ("config.get", Scope::ConfigInspect),
        ("config.inspect", Scope::ConfigInspect),
        ("config.reload", Scope::ConfigModify),
        ("config.set", Scope::ConfigModify),
        ("config.modify", Scope::ConfigModify),
        ("plugin.list", Scope::PluginInspect),
        ("plugin.get", Scope::PluginInspect),
        ("plugin.install", Scope::PluginManage),
        ("plugin.disable", Scope::PluginManage),
        ("plugin.enable", Scope::PluginManage),
        ("plugin.remove", Scope::PluginManage),
        ("process.spawn", Scope::ProcessSpawn),
        ("debug.snapshot", Scope::DebugInspect),
        ("debug.inspect", Scope::DebugInspect),
        ("debug.get", Scope::DebugInspect),
        ("debug.start_trace", Scope::DebugTrace),
        ("debug.trace", Scope::DebugTrace),
        ("debug.break", Scope::DebugControl),
        ("debug.control", Scope::DebugControl),
        ("debug.pause", Scope::DebugControl),
    ]
}

#[test]
fn registry_is_countable_and_in_sync() {
    let listed = all_known_methods();
    let expected = expected_matrix();
    assert_eq!(listed.len(), expected.len(), "registry size drift");
    assert_eq!(listed.len(), 38);
    // Listed order matches the expected table and every entry maps.
    for (i, (method, scope)) in expected.iter().enumerate() {
        assert_eq!(listed[i], *method, "registry order drift at {i}");
        assert_eq!(
            required_scope_for_method(method),
            Some(*scope),
            "{method}: mapping drift"
        );
    }
    // No duplicates in the registry.
    let mut seen = std::collections::BTreeSet::new();
    for method in listed {
        assert!(seen.insert(*method), "duplicate registry entry {method}");
    }
}

#[test]
fn every_registry_entry_passes_grammar_no_dead_mappings() {
    // Regression: `debug.start-trace` was mapped but unreachable because the
    // wire grammar rejects `-`. Every mapped method must survive validation
    // so the registry is exactly the authorizable set.
    for method in all_known_methods() {
        assert!(
            bitty_ipc::scope::validate_method_name(method).is_ok(),
            "{method}: registry entry unreachable behind grammar"
        );
    }
    // The removed hyphen alias stays denied (grammar), never authorized.
    let err = authorize_method("debug.start-trace", &ScopeSet::all()).unwrap_err();
    assert!(
        matches!(err, IpcError::InvalidMethod { .. }),
        "hyphen alias must stay denied, got {err:?}"
    );
}

#[test]
fn exact_scope_allows_every_method() {
    for (method, scope) in expected_matrix() {
        let granted = ScopeSet::single(*scope);
        let got = authorize_method(method, &granted).unwrap_err_or_ok(method);
        assert_eq!(got, *scope, "{method}");
    }
}

#[test]
fn empty_set_denies_every_method_with_scope_denied() {
    let empty = ScopeSet::new();
    for (method, scope) in expected_matrix() {
        expect_scope_denied(authorize_method(method, &empty), method, *scope);
    }
}

#[test]
fn any_other_single_scope_denies_every_method() {
    for (method, required) in expected_matrix() {
        for other in Scope::all() {
            if *other == *required {
                continue;
            }
            let granted = ScopeSet::single(*other);
            expect_scope_denied(authorize_method(method, &granted), method, *required);
        }
    }
}

#[test]
fn cli_and_mcp_defaults_allow_exactly_their_subset() {
    for defaults in [ScopeSet::cli_default(), ScopeSet::mcp_default()] {
        for (method, required) in expected_matrix() {
            let result = authorize_method(method, &defaults);
            if defaults.contains(*required) {
                assert!(result.is_ok(), "{method} must be allowed");
            } else {
                expect_scope_denied(result, method, *required);
            }
        }
    }
    // Pin the subset sizes so default expansion is a visible diff.
    assert_eq!(ScopeSet::cli_default().len(), 6);
    assert_eq!(ScopeSet::mcp_default().len(), 4);
}

#[test]
fn inspect_only_cannot_effect_escalation_adversarial_list() {
    // Evidence-matrix adversarial list (P0-AC-022): with only inspect/read
    // granted, each effectful action is denied ScopeDenied.
    let read_only = ScopeSet::single(Scope::TerminalInspect);
    for (method, scope) in [
        ("terminal.send", Scope::TerminalInput),
        ("process.spawn", Scope::ProcessSpawn),
        ("config.set", Scope::ConfigModify),
        ("plugin.install", Scope::PluginManage),
        ("debug.control", Scope::DebugControl),
        ("terminal.close", Scope::TerminalManage),
        ("view.split", Scope::ViewManage),
        ("debug.trace", Scope::DebugTrace),
    ] {
        let err = authorize_method(method, &read_only).unwrap_err();
        assert!(
            matches!(err, IpcError::ScopeDenied { .. }),
            "{method}: expected ScopeDenied, got {err:?}"
        );
        assert_eq!(err.error_class(), ErrorClass::Scope);
        let _ = scope;
    }
    // Scope-name-shaped method strings grant nothing: fail-closed NotFound.
    for probe in ["plugin.manage", "terminal.elevate", "debug.grant"] {
        let err = authorize_method(probe, &ScopeSet::all()).unwrap_err();
        assert!(
            matches!(err, IpcError::NotFound { .. }),
            "{probe}: expected NotFound, got {err:?}"
        );
    }
}

#[test]
fn unknown_and_malformed_methods_fail_closed() {
    let all = ScopeSet::all();
    let not_found = authorize_method("unknown.method", &all).unwrap_err();
    assert!(matches!(not_found, IpcError::NotFound { .. }));
    for bad in ["", "bad..method", "Terminal.text", "terminal .text"] {
        let err = authorize_method(bad, &all).unwrap_err();
        assert!(
            matches!(err, IpcError::InvalidMethod { .. }),
            "{bad}: expected InvalidMethod, got {err:?}"
        );
    }
}

#[test]
fn ctl_registry_matrix_exact_scope_allows_others_deny() {
    let methods = all_control_methods();
    assert!(!methods.is_empty());
    for method in methods {
        let required = required_scope_for_ctl_method(method)
            .unwrap_or_else(|| panic!("{method}: control registry must map"));
        assert!(
            authorize_ctl_method(method, &ScopeSet::single(required)).is_ok(),
            "{method}"
        );
        expect_scope_denied(
            authorize_ctl_method(method, &ScopeSet::new()),
            method,
            required,
        );
    }
    // Test-mode teardown needs debug.control like every elevated verb.
    assert_eq!(
        required_scope_for_ctl_method(bitty_ipc::devtools::METHOD_TEST_EXIT),
        Some(Scope::DebugControl)
    );
    // Unknown control method is NotFound even with full grants.
    let err = authorize_ctl_method("bitty.debug/noSuchVerb", &ScopeSet::all()).unwrap_err();
    assert!(matches!(err, IpcError::NotFound { .. }));
}

#[test]
fn elevation_requires_separate_consent() {
    // One grant never implies another: per-client, per-scope, ledgered.
    let mut ledger = ConsentLedger::new();
    ledger
        .grant(
            "agent".into(),
            Scope::TerminalInput,
            0,
            10_000,
            "user".into(),
        )
        .unwrap();
    assert!(ledger.is_granted("agent", Scope::TerminalInput, 0));
    for other in [
        Scope::TerminalManage,
        Scope::ConfigModify,
        Scope::PluginManage,
        Scope::ProcessSpawn,
        Scope::DebugControl,
    ] {
        assert!(!ledger.is_granted("agent", other, 0), "{other:?}");
    }
    // Bridge: granted scope without live consent is Denied[ConsentRequired].
    let mut bridge = BridgeClient::new("agent", ScopeSet::all()).expect("valid bridge");
    let err = bridge.call("terminal.text", b"{}".to_vec(), 0).unwrap_err();
    match err {
        IpcError::Denied { code, .. } => assert_eq!(code, "ConsentRequired"),
        other => panic!("expected ConsentRequired, got {other:?}"),
    }
    assert_eq!(bridge.pending_count(), 0, "denial leaves no pending state");
}

/// Helper: `Ok` on success; panics with context on denial (keeps the
/// matrix test body one line per method).
trait UnwrapErrOrOk {
    fn unwrap_err_or_ok(self, method: &str) -> Scope;
}

impl UnwrapErrOrOk for Result<Scope, IpcError> {
    fn unwrap_err_or_ok(self, method: &str) -> Scope {
        match self {
            Ok(scope) => scope,
            Err(err) => panic!("{method}: expected Ok, got {err:?}"),
        }
    }
}
