//! P0-AC-025 (bitty#2921): the DevTools scope family is distinct from the
//! session scopes and a connection alone grants none of it.
//!
//! `P0-AC-025` asks that a DevTools client that has merely connected is denied
//! `debug.inspect`, `debug.trace`, and `debug.control` until each is separately
//! granted, and its pass threshold is a three-scope matrix showing that
//! connection alone grants none of them.
//!
//! The matrix here walks the whole closed DevTools family (`debug.*`, 8 frozen
//! methods) against all 15 v1 scopes, and against the two shipped default
//! profiles, at the same decision point the transport uses
//! (`bitty_ipc_api::scope::authorize_method`). It then proves the two
//! disjointness facts the criterion needs at the daemon boundary:
//!
//! - no `bittyd` service scope name is a v1 scope name, and no v1 `debug.*`
//!   name is a service method: `ServiceMethod::from_name("debug.inspect")` is
//!   `None`, so the service catalog cannot require, grant, or route a DevTools
//!   verb;
//! - the daemon's handshake advertises four transport scopes and three service
//!   scopes, none of which is `debug.*` (asserted live in
//!   `tests/service_loop.rs`); the in-process half here pins the catalog and
//!   profile side of the same rule.
//!
//! Run with `cargo test -p bitty-daemon --test devtools_scope_boundary --
//! --nocapture`; the table between the `DEVTOOLS_SCOPE_MATRIX_*` markers is the
//! artifact the dated evidence record pastes.

use bitty_daemon::codes::RefusalCode;
use bitty_daemon::service::scopes::is_transport_family;
use bitty_daemon::service::{
    MethodResolution, ServiceMethod, ServiceScope, ServiceScopeSet, resolve,
};
use bitty_ipc_api::IpcError;
use std::str::FromStr;

use bitty_ipc_api::scope::{Scope, ScopeSet, authorize_method, required_scope_for_method};

/// The frozen DevTools verbs, in registry order.
fn devtools_verbs() -> Vec<&'static str> {
    vec![
        "debug.snapshot",
        "debug.inspect",
        "debug.get",
        "debug.start_trace",
        "debug.trace",
        "debug.break",
        "debug.control",
        "debug.pause",
    ]
}

/// The three DevTools scopes.
fn devtools_scopes() -> [Scope; 3] {
    [Scope::DebugInspect, Scope::DebugTrace, Scope::DebugControl]
}

/// A grant set holding exactly one scope.
fn single(scope: Scope) -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(scope);
    set
}

#[test]
fn the_devtools_family_is_three_scopes_and_eight_frozen_verbs() {
    let debug: Vec<&str> = Scope::all()
        .iter()
        .filter(|scope| scope.family() == "debug")
        .map(|scope| scope.as_str())
        .collect();
    assert_eq!(debug, ["debug.inspect", "debug.trace", "debug.control"]);
    assert_eq!(devtools_scopes().len(), 3);

    for verb in devtools_verbs() {
        let required = required_scope_for_method(verb)
            .unwrap_or_else(|| panic!("{verb} has no required scope"));
        assert_eq!(required.family(), "debug", "{verb} is not a DevTools verb");
        assert!(
            is_transport_family(verb),
            "{verb} must be a v1 family member"
        );
        // The daemon's catalog has no DevTools method at all: a well-formed
        // `debug.*` name resolves to `Unknown`, which the service renders as
        // the typed `NotFound/NotFound`, never as a granted method.
        assert_eq!(
            ServiceMethod::from_name(verb),
            None,
            "{verb} in the catalog"
        );
        assert_eq!(resolve(verb), MethodResolution::Unknown, "{verb} resolved");
    }
    // 51 v1 methods: 8 debug + 43 others.
    assert_eq!(devtools_verbs().len(), 8);
}

#[test]
fn no_service_scope_is_a_v1_scope_and_no_v1_scope_is_a_service_scope() {
    for scope in ServiceScope::ALL {
        assert!(
            Scope::from_str(scope.as_str()).is_err(),
            "{} is both a service scope and a v1 scope",
            scope.as_str()
        );
        assert!(
            !is_transport_family(scope.as_str()),
            "{} reads as a v1 family member",
            scope.as_str()
        );
    }
    // The services keep their own closed vocabulary; the DevTools scopes are not
    // in it under a different spelling.
    let service_names: Vec<String> = ServiceScopeSet::all()
        .names()
        .map(std::string::ToString::to_string)
        .collect();
    assert_eq!(
        service_names,
        [
            "session.inspect",
            "session.host",
            "session.attach",
            "session.control",
            "daemon.inspect",
            "daemon.control",
        ]
    );
    for scope in devtools_scopes() {
        assert!(
            !service_names.iter().any(|name| name == scope.as_str()),
            "{} is spelled as a service scope",
            scope.as_str()
        );
    }
    for method in ServiceMethod::ALL {
        assert!(
            !method.name().starts_with("debug."),
            "{} is a DevTools verb in the service catalog",
            method.name()
        );
    }
}

#[test]
fn connection_alone_grants_none_of_the_three_devtools_scopes() {
    for verb in devtools_verbs() {
        let required =
            required_scope_for_method(verb).unwrap_or_else(|| panic!("{verb} has no scope"));
        for scope in Scope::all() {
            let granted = single(*scope);
            let outcome = authorize_method(verb, &granted);
            if *scope == required {
                assert_eq!(
                    outcome.expect("the verb's own scope allows it"),
                    required,
                    "{verb} must be allowed by its own scope"
                );
            } else {
                let error = outcome.expect_err("no other scope may allow a DevTools verb");
                match &error {
                    IpcError::ScopeDenied {
                        scope: named,
                        action,
                    } => {
                        assert_eq!(named.as_str(), required.as_str(), "{verb}");
                        assert_eq!(action, verb, "{verb} must name itself");
                    }
                    other => panic!("{verb} under {scope} must be ScopeDenied, got {other:?}"),
                }
                assert_eq!(error.error_class().to_string(), "scope", "{verb}");
            }
        }
    }
    // The two shipped read-only profiles grant no DevTools scope; the MCP
    // profile can be given `debug.inspect` only when the client presents it, and
    // never trace or control.
    for (label, set) in [
        ("mcp", ScopeSet::mcp_default()),
        ("cli", ScopeSet::cli_default()),
    ] {
        for scope in devtools_scopes() {
            assert!(!set.contains(scope), "the {label} default grants {scope}");
        }
    }
    let mcp_debug = ScopeSet::mcp_default_with_debug_inspect();
    assert!(mcp_debug.contains(Scope::DebugInspect));
    assert!(!mcp_debug.contains(Scope::DebugTrace));
    assert!(!mcp_debug.contains(Scope::DebugControl));
    for verb in [
        "debug.start_trace",
        "debug.trace",
        "debug.break",
        "debug.control",
        "debug.pause",
    ] {
        let error = authorize_method(verb, &mcp_debug).expect_err("trace/control stay denied");
        assert_eq!(error.error_class().to_string(), "scope", "{verb}");
    }
    // Holding every scope allows every DevTools verb, so the denials above are
    // caused by the missing grant and not by a second stage.
    for verb in devtools_verbs() {
        assert!(authorize_method(verb, &ScopeSet::all()).is_ok(), "{verb}");
    }
}

#[test]
fn a_devtools_verb_without_the_grant_carries_the_typed_code_at_both_layers() {
    // Transport layer: `Denied/ScopeDenied` naming the verb's own scope.
    let error = authorize_method("debug.trace", &ScopeSet::mcp_default())
        .expect_err("a DevTools verb without its scope is refused");
    assert_eq!(error.error_class().to_string(), "scope");
    let IpcError::ScopeDenied { scope, action } = &error else {
        panic!("the refusal must be ScopeDenied, got {error:?}");
    };
    assert_eq!(scope.as_str(), "debug.trace");
    assert_eq!(action, "debug.trace");

    // Service layer: the same name is not a catalog method, so the daemon's own
    // refusal is the typed `NotFound/NotFound` (deny-by-default through the
    // method catalog), never a granted method and never a different code.
    assert_eq!(resolve("debug.trace"), MethodResolution::Unknown);
    assert_eq!(resolve("debug.inspect"), MethodResolution::Unknown);
    assert_eq!(resolve("debug.control"), MethodResolution::Unknown);
    assert_eq!(RefusalCode::NotFound.wire_name(), "NotFound");
    assert_eq!(RefusalCode::NotFound.as_str(), "not_found");
    // A malformed name is a different typed refusal, so the two are not merged.
    assert_eq!(resolve("Debug.Trace"), MethodResolution::Malformed);
}

#[test]
fn the_devtools_scope_matrix_is_printed_as_the_artifact() {
    let scopes = Scope::all();
    let mut out = String::new();
    out.push_str("DEVTOOLS_SCOPE_MATRIX_BEGIN\n");
    out.push_str("| verb | required scope |");
    for scope in scopes {
        out.push(' ');
        out.push_str(scope.as_str());
        out.push_str(" |");
    }
    out.push('\n');
    out.push_str("| --- | --- |");
    for _ in scopes {
        out.push_str(" --- |");
    }
    out.push('\n');
    let mut allowed = 0_usize;
    for verb in devtools_verbs() {
        let required = required_scope_for_method(verb)
            .unwrap_or_else(|| panic!("{verb} has no required scope"));
        out.push_str(&format!("| `{verb}` | `{required}` |"));
        for scope in scopes {
            let cell = match authorize_method(verb, &single(*scope)) {
                Ok(_) => {
                    allowed += 1;
                    " ok |"
                }
                Err(_) => " — |",
            };
            out.push_str(cell);
        }
        out.push('\n');
    }
    out.push_str("DEVTOOLS_SCOPE_MATRIX_END\n");
    print!("{out}");
    println!(
        "legend: ok = allowed; — = Denied/ScopeDenied(class `scope`, code `ScopeDenied`); \
         cells = {}, allowed = {allowed}, denied = {}",
        scopes.len() * devtools_verbs().len(),
        scopes.len() * devtools_verbs().len() - allowed
    );
}
