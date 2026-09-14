//! Generic out-of-process bridge client boundary (CTX-0419, Issue #708).
//!
//! G-1 from the CTX-0407 pressure test: `bitty-ipc` was `publish = false`,
//! so out-of-process consumers (`bitty-ai`, devtools) pinned it via a git
//! rev plus a `deny.toml` `allow-git` exception (`bitty-ai` PR #7,
//! read-only reference). This integration test pins the replacement
//! contract: [`BridgeClient`](bitty_ipc::bridge::BridgeClient) is the
//! canonical published boundary — mechanism-only, no AI specifics —
//! composing the accepted primitives (method registry, scope
//! authorization, consent ledger, bounded endpoint, wire envelope) in the
//! DIR-018 dispatch order.
//!
//! Reference evidence (read-only, never modified here): `bitty-ai` PR #7
//! `IpcBridge` (`validate_method_name` + `required_scope_for_method` +
//! `authorize_method` + `ConsentLedger` + `IpcEndpoint` +
//! `validate_request_envelope` / `validate_response_envelope`; unknown
//! method fails closed before dispatch).
//!
//! Headless and network-free: no sockets, no threads, no wall-clock.

use bitty_ipc::bridge::{BridgeClient, MAX_BRIDGE_CLIENT_ID_BYTES, MAX_BRIDGE_PARAMS_BYTES};
use bitty_ipc::channel::RequestId;
use bitty_ipc::error::IpcError;
use bitty_ipc::scope::{Scope, ScopeSet};

const CLIENT: &str = "agent-0419";
const NOW_MS: u64 = 1_000;
const TTL_MS: u64 = 60_000;

/// A bridge with `terminal.inspect` granted and consented at `NOW_MS`.
fn consented_bridge() -> BridgeClient {
    let mut bridge = BridgeClient::new(CLIENT, ScopeSet::cli_default()).expect("valid bridge");
    bridge
        .grant_consent(Scope::TerminalInspect, NOW_MS, TTL_MS)
        .expect("consent grant fits");
    bridge
}

#[test]
fn bridge_params_bound_reuses_accepted_tool_args_cap() {
    assert_eq!(MAX_BRIDGE_PARAMS_BYTES, 16 * 1024);
    assert_eq!(
        MAX_BRIDGE_PARAMS_BYTES,
        bitty_ipc::tool_dispatch::MAX_TOOL_ARGS_BYTES
    );
    assert_eq!(MAX_BRIDGE_CLIENT_ID_BYTES, 64);
}

#[test]
fn bridge_rejects_empty_and_overlong_client_id() {
    let err = BridgeClient::new("", ScopeSet::cli_default()).unwrap_err();
    assert!(matches!(err, IpcError::InvalidRequest { .. }));
    let long = "c".repeat(MAX_BRIDGE_CLIENT_ID_BYTES + 1);
    let err = BridgeClient::new(long, ScopeSet::cli_default()).unwrap_err();
    assert!(matches!(
        err,
        IpcError::LimitExceeded { .. } | IpcError::PayloadTooLarge { .. }
    ));
}

#[test]
fn bridge_unknown_method_fails_closed_before_dispatch() {
    let mut bridge = consented_bridge();
    let err = bridge
        .call("panel.context", b"{}".to_vec(), NOW_MS)
        .unwrap_err();
    assert!(matches!(err, IpcError::NotFound { .. }));
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_missing_scope_fails_closed() {
    let mut bridge = BridgeClient::new(CLIENT, ScopeSet::new()).expect("empty scope set is valid");
    bridge
        .grant_consent(Scope::TerminalInspect, NOW_MS, TTL_MS)
        .expect("ledger grant is scope-independent");
    let err = bridge
        .call("terminal.snapshot", b"{}".to_vec(), NOW_MS)
        .unwrap_err();
    assert!(matches!(err, IpcError::ScopeDenied { .. }));
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_missing_consent_fails_closed() {
    let mut bridge = BridgeClient::new(CLIENT, ScopeSet::cli_default()).expect("valid bridge");
    let err = bridge
        .call("terminal.snapshot", b"{}".to_vec(), NOW_MS)
        .unwrap_err();
    assert!(matches!(err, IpcError::Denied { .. }));
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_expired_consent_fails_closed() {
    let mut bridge = BridgeClient::new(CLIENT, ScopeSet::cli_default()).expect("valid bridge");
    bridge
        .grant_consent(Scope::TerminalInspect, NOW_MS, 100)
        .expect("short grant fits");
    let err = bridge
        .call("terminal.snapshot", b"{}".to_vec(), NOW_MS + 100)
        .unwrap_err();
    assert!(matches!(err, IpcError::Denied { .. }));
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_over_bound_params_fail_closed() {
    let mut bridge = consented_bridge();
    let big = vec![b'a'; MAX_BRIDGE_PARAMS_BYTES + 1];
    let err = bridge.call("terminal.snapshot", big, NOW_MS).unwrap_err();
    assert!(matches!(
        err,
        IpcError::PayloadTooLarge { .. } | IpcError::LimitExceeded { .. }
    ));
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_happy_path_builds_correlated_request() {
    let mut bridge = consented_bridge();
    let params = br#"{"terminal_id":"t:4"}"#.to_vec();
    let id = bridge
        .call("terminal.snapshot", params, NOW_MS)
        .expect("call fits");
    assert!(!id.is_zero());
    assert_eq!(bridge.pending_count(), 1);

    // Handover to the transport: the queued request carries method + params.
    let queued = bridge.take_request().expect("request queued");
    assert_eq!(queued.id, id);
    assert_eq!(queued.method, "terminal.snapshot");

    // Peer answers; the bridge correlates and drains pending.
    assert!(
        bridge
            .answer(id, b"{}".to_vec(), false)
            .expect("answer fits")
    );
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_answer_for_unknown_id_returns_false() {
    let mut bridge = consented_bridge();
    assert!(
        !bridge
            .answer(RequestId(999_999), b"{}".to_vec(), false)
            .expect("answer fits")
    );
    assert_eq!(bridge.pending_count(), 0);
}

#[test]
fn bridge_denials_leave_no_partial_state() {
    let mut bridge = consented_bridge();
    let _ = bridge.call("panel.context", b"{}".to_vec(), NOW_MS);
    let _ = bridge.call(
        "terminal.snapshot",
        vec![b'x'; MAX_BRIDGE_PARAMS_BYTES + 1],
        NOW_MS,
    );
    assert_eq!(bridge.pending_count(), 0);
    assert!(bridge.take_request().is_none());
}
