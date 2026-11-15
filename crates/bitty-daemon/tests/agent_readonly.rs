//! P0-AC-024 (bitty#2921): the MCP/Agent read-only default, the untrusted
//! observation label, and "no connection holds a write authority implicitly",
//! as one runnable matrix.
//!
//! `P0-AC-024` asks that an MCP/Agent client without elevation only succeeds at
//! read-style operations, that every response containing terminal content
//! labels it untrusted observation data, and that sending input, spawning
//! processes, installing plugins, or writing configuration stays behind
//! per-client consent. Its pass threshold is a "default-deny matrix" with
//! labeling asserted in API responses.
//!
//! Three shipped layers answer that here, and every cell is decided by the
//! product code rather than by a copy of the rule:
//!
//! - the v1 registry (`bitty-ipc-api`): [`ScopeSet::mcp_default`] is the
//!   Agent/MCP profile and `authorize_method` is the same decision point the
//!   transport uses. All 51 frozen methods are walked: the 10 under the four
//!   read scopes are allowed, the 41 effectful verbs are refused with
//!   `Denied/ScopeDenied` naming that verb's own scope;
//! - the daemon catalog (`bitty_daemon::service`): the baseline consumer set is
//!   three `session.*` scopes, never `session.host`, never a `daemon.*` scope,
//!   and the scopes the catalog can require are exactly the transport scopes
//!   the handshake grants plus the two `session.*` authorities;
//! - the label (`content::UntrustedObservation`): one value, always `true`,
//!   attached by dispatch from [`ServiceMethod::carries_terminal_content`], so
//!   `session.read` is the only labeled method and a future byte-serving verb
//!   cannot be added without being labeled; a non-object payload is refused
//!   rather than served unlabeled.
//!
//! Run with `cargo test -p bitty-daemon --test agent_readonly -- --nocapture`;
//! the table between the `AGENT_DEFAULT_MATRIX_*` markers is the artifact the
//! dated evidence record pastes. The assertions are what make it trustworthy.

use serde_json::{Value, json};

use bitty_daemon::content::UntrustedObservation;
use bitty_daemon::service::{
    ScopeRequirement, ServiceMethod, ServiceScope, ServiceScopeSet, scopes::is_transport_family,
};
use bitty_ipc_api::IpcError;
use bitty_ipc_api::scope::{
    Scope, ScopeSet, all_known_methods, authorize_method, required_scope_for_method,
};

/// The Agent/MCP profile's held scopes, as the transport defines them.
fn agent_default() -> ScopeSet {
    ScopeSet::mcp_default()
}

/// The scope name a `ScopeDenied` refusal carries, when it is a scope refusal.
fn denied_scope(error: &IpcError) -> Option<&str> {
    match error {
        IpcError::ScopeDenied { scope, .. } => Some(scope.as_str()),
        _ => None,
    }
}

/// Whether a required scope is effectful: it can send input, spawn, manage, or
/// observe debug trace state. Every one of them is outside the Agent default.
fn is_effectful(scope: Scope) -> bool {
    !matches!(
        scope,
        Scope::TerminalInspect | Scope::ViewInspect | Scope::ConfigInspect | Scope::PluginInspect
    )
}

#[test]
fn the_agent_default_profile_holds_exactly_the_four_read_scopes() {
    let agent = agent_default();
    assert_eq!(agent.len(), 4, "the Agent/MCP profile changed size");
    assert_eq!(
        agent.iter().map(|scope| scope.as_str()).collect::<Vec<_>>(),
        [
            "terminal.inspect",
            "view.inspect",
            "config.inspect",
            "plugin.inspect"
        ],
        "the Agent/MCP default is read-only inspection"
    );
    for scope in Scope::all() {
        assert_eq!(
            agent.contains(*scope),
            !is_effectful(*scope),
            "{scope} must be held exactly when it is a read scope"
        );
        assert_eq!(
            ScopeSet::requires_mcp_elevation(*scope),
            is_effectful(*scope),
            "{scope} must require explicit MCP elevation exactly when it is effectful"
        );
    }
    // `debug.inspect` is the one debug scope the profile can be given, and only
    // when the client presents it; the read-only base never carries it.
    let with_debug = ScopeSet::mcp_default_with_debug_inspect();
    assert_eq!(with_debug.len(), 5);
    assert!(with_debug.contains(Scope::DebugInspect));
    assert!(!with_debug.contains(Scope::DebugTrace));
    assert!(!with_debug.contains(Scope::DebugControl));
}

#[test]
fn every_effectful_registry_verb_is_refused_under_the_agent_default() {
    let agent = agent_default();
    let mut allowed = Vec::new();
    let mut denied = Vec::new();
    for method in all_known_methods() {
        let required = required_scope_for_method(method)
            .unwrap_or_else(|| panic!("{method} declares no required scope"));
        match authorize_method(method, &agent) {
            Ok(scope) => {
                assert_eq!(scope, required, "{method} was allowed for the wrong scope");
                assert!(
                    !is_effectful(required),
                    "{method} requires the effectful {required} and must not be allowed"
                );
                allowed.push((*method, required));
            }
            Err(error) => {
                assert_eq!(
                    denied_scope(&error),
                    Some(required.as_str()),
                    "{method} must be denied for its own scope"
                );
                match &error {
                    IpcError::ScopeDenied { action, .. } => {
                        assert_eq!(action, method, "{method} must name itself");
                    }
                    other => panic!("{method} must be ScopeDenied, got {other:?}"),
                }
                assert!(
                    is_effectful(required),
                    "{method} is a read verb and must not be refused under the read profile"
                );
                denied.push((*method, required));
            }
        }
    }
    assert_eq!(allowed.len(), 10, "the read set changed size");
    assert_eq!(denied.len(), 41, "the effectful set changed size");
    assert_eq!(allowed.len() + denied.len(), all_known_methods().len());

    // Every effectful scope has at least one refused verb, so no write family is
    // unrepresented in the matrix and none is silently absent from the registry.
    for scope in [
        Scope::TerminalInput,
        Scope::TerminalManage,
        Scope::ViewManage,
        Scope::ConfigModify,
        Scope::PluginManage,
        Scope::ProcessSpawn,
        Scope::DebugInspect,
        Scope::DebugTrace,
        Scope::DebugControl,
        Scope::AiInspect,
        Scope::AiInvoke,
    ] {
        assert!(
            denied.iter().any(|(_, required)| *required == scope),
            "{scope} has no verb in the refused set"
        );
    }
    // The four read scopes are the only ones that allow anything at all.
    for scope in Scope::all() {
        let holds = allowed.iter().any(|(_, required)| required == scope);
        assert_eq!(holds, !is_effectful(*scope), "{scope} read/effectful split");
    }
}

#[test]
fn the_daemon_baseline_grants_no_write_authority_and_no_devtools_scope() {
    let baseline = ServiceScopeSet::baseline();
    assert_eq!(baseline.len(), 3, "the consumer baseline changed size");
    assert!(baseline.contains(ServiceScope::SessionInspect));
    assert!(baseline.contains(ServiceScope::SessionAttach));
    assert!(baseline.contains(ServiceScope::SessionControl));
    // The one authority that creates a session is never implicit: neither the
    // baseline nor the operator set carries it, so only a consumed host grant
    // can add it (`daemon.negotiate`).
    assert!(!baseline.contains(ServiceScope::SessionHost));
    assert!(!ServiceScopeSet::operator().contains(ServiceScope::SessionHost));
    assert!(
        !baseline
            .union(ServiceScopeSet::operator())
            .contains(ServiceScope::SessionHost)
    );
    assert!(baseline.iter().all(|scope| !scope.is_operator()));

    // The catalog can require service scopes and v1 transport scopes. Every
    // transport scope it names must be one the daemon actually grants, and no
    // requirement may name a `debug.*`, `config.*`, `plugin.*`, `view.*`, or
    // `ai.*` authority: a session verb never reaches a DevTools method.
    let mut transport_requirements = Vec::new();
    for method in ServiceMethod::ALL {
        match method.scope() {
            ScopeRequirement::None => {}
            ScopeRequirement::Service(_) => {}
            ScopeRequirement::Transport(any_of) => {
                for scope in any_of {
                    transport_requirements.push((method.name(), *scope));
                }
            }
        }
    }
    let required: Vec<&str> = transport_requirements
        .iter()
        .map(|(_, scope)| scope.as_str())
        .collect();
    assert_eq!(
        required,
        [
            "terminal.manage",
            "terminal.inspect",
            "terminal.input",
            "terminal.inspect"
        ],
        "the content and resize plane reuses only the terminal scopes"
    );
    for scope in Scope::all() {
        if scope.family() == "debug"
            || *scope == Scope::ConfigModify
            || *scope == Scope::PluginManage
        {
            assert!(
                !transport_requirements
                    .iter()
                    .any(|(_, required)| required == scope),
                "{scope} must not gate a session method"
            );
        }
    }
    // A session verb is never a v1 family member, so a DevTools verb cannot be
    // reached through the service catalog at all.
    for method in ServiceMethod::ALL {
        assert!(
            !is_transport_family(method.name()),
            "{} is a v1 family member inside the service catalog",
            method.name()
        );
    }
    assert!(is_transport_family("debug.inspect"));
}

#[test]
fn the_untrusted_label_is_decided_by_the_catalog_predicate_alone() {
    // `dispatch` attaches the label from this predicate, so the predicate is the
    // labelling contract: exactly the byte-serving methods, and nothing else.
    let content: Vec<&str> = ServiceMethod::ALL
        .iter()
        .filter(|method| method.carries_terminal_content())
        .map(|method| method.name())
        .collect();
    assert_eq!(
        content,
        ["session.read"],
        "the content plane changed shape: a new byte-serving verb must be labeled"
    );

    // The label type has one value, so a response cannot be built false.
    let label = UntrustedObservation::new();
    assert!(label.is_untrusted_surface());
    assert_eq!(UntrustedObservation::FIELD, "is_untrusted_surface");

    // Attaching over a forged field wins: dispatch attaches after the handler
    // returns, so the handler's own value cannot survive.
    let mut response: Value = json!({
        "session_id": "0".repeat(32),
        "generation": 1,
        "data": "414243",
        "bytes": 3,
        "buffered_bytes": 3,
        "is_untrusted_surface": false,
        "untrusted": false,
    });
    assert!(label.attach(&mut response));
    assert_eq!(
        response[UntrustedObservation::FIELD],
        Value::Bool(true),
        "the server-attached label must overwrite a forged field"
    );
    assert!(response[UntrustedObservation::FIELD].is_boolean());
    // A non-object payload cannot carry the label, so dispatch refuses it
    // instead of serving unlabeled bytes (`UntrustedObservation::attach`
    // returns `false`; `Service::dispatch` maps that to `Refusal::internal`).
    let mut not_an_object = json!("raw-bytes");
    assert!(!label.attach(&mut not_an_object));
    assert_eq!(not_an_object, json!("raw-bytes"), "left untouched");

    // An observer lease never receives content in the first place; the labeling
    // rule only matters once the controller path is open. The predicate and the
    // content-access split are the two halves the live cells exercise.
    assert_eq!(
        ServiceMethod::SessionRead.boundary(),
        Some(bitty_daemon::peer::DispatchBoundary::ContentPlane)
    );
}

#[test]
fn the_agent_default_matrix_is_printed_as_the_artifact() {
    let agent = agent_default();
    let mut out = String::new();
    out.push_str("AGENT_DEFAULT_MATRIX_BEGIN\n");
    out.push_str("| method | required scope | agent default (4 read scopes) | typed refusal |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    let mut allowed = 0_usize;
    let mut denied = 0_usize;
    for method in all_known_methods() {
        let required = required_scope_for_method(method)
            .unwrap_or_else(|| panic!("{method} declares no required scope"));
        let (cell, typed) = match authorize_method(method, &agent) {
            Ok(_) => {
                allowed += 1;
                ("ok", "—")
            }
            Err(error) => {
                denied += 1;
                let scope = denied_scope(&error).unwrap_or("?");
                assert_eq!(scope, required.as_str());
                ("denied", "Denied/ScopeDenied")
            }
        };
        out.push_str(&format!(
            "| `{method}` | `{required}` | {cell} | {typed} |\n"
        ));
    }
    out.push_str("AGENT_DEFAULT_MATRIX_END\n");
    print!("{out}");
    println!(
        "legend: ok = allowed by the Agent/MCP read profile; denied = \
         Denied/ScopeDenied(class `scope`, code `ScopeDenied`) naming the method's own scope; \
         methods = {}, allowed = {allowed}, denied = {denied}",
        all_known_methods().len()
    );
}
