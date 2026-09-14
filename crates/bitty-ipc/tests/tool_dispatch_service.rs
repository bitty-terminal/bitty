//! Host-registered tool dispatch with per-tool consent (CTX-0421, Issue #704).
//!
//! The Core-internal registry (`bitty-agent/src/tool.rs`) validates calls
//! syntactically but never executes them; there is no IPC tool-dispatch
//! method with per-tool consent (TB-4). This integration test pins the G-3
//! contract: a generic host-registered dispatch implementing the DIR-018
//! formula (routing + authorization + consent + captured target + budget +
//! attribution + outcome), read-only by default, fail-closed.
//!
//! Reference evidence (read-only, never modified here): `bitty-ai` PR #7
//! tool bus (`ToolBus::precheck`/`dispatch`, `AllowReadOnly` harness,
//! inspect-tier read-only gate, per-tool authorize seam; unknown tool fails
//! closed before dispatch, write tool denied by default).
//!
//! Headless and network-free: no sockets, no threads, no wall-clock.

use bitty_ipc::error::IpcError;
use bitty_ipc::scope::{ConsentLedger, Scope, ScopeSet};
use bitty_ipc::tool_dispatch::{
    MAX_TOOL_ARGS_BYTES, MAX_TOOL_RESULT_BYTES, ToolDispatchService, ToolOutput, ToolRequest,
    ToolSpec,
};

const CLIENT: &str = "agent-0421";
const NOW_MS: u64 = 1_000;
const TTL_MS: u64 = 60_000;

fn read_only_spec() -> ToolSpec {
    ToolSpec::new(
        "terminal_read_zone",
        "Read a bounded terminal semantic zone (read-only)",
        br#"{"type":"object"}"#.to_vec(),
        Scope::TerminalInspect,
        true,
    )
    .expect("valid read-only spec")
}

fn effect_spec() -> ToolSpec {
    ToolSpec::new(
        "terminal_send",
        "Send input to a terminal (effect)",
        br#"{"type":"object"}"#.to_vec(),
        Scope::TerminalInput,
        false,
    )
    .expect("valid effect spec")
}

fn read_only_provider(request: &ToolRequest) -> Result<ToolOutput, IpcError> {
    Ok(ToolOutput {
        target_id: request.target.clone(),
        data: b"$ echo hello\nhello\n".to_vec(),
        summary: "terminal snapshot".to_owned(),
    })
}

fn effect_provider(request: &ToolRequest) -> Result<ToolOutput, IpcError> {
    Ok(ToolOutput {
        target_id: request.target.clone(),
        data: b"sent".to_vec(),
        summary: "input sent".to_owned(),
    })
}

fn granted(scopes: &[Scope]) -> ScopeSet {
    let mut set = ScopeSet::new();
    for scope in scopes {
        set.insert(*scope);
    }
    set
}

fn consented(client: &str, scope: Scope) -> ConsentLedger {
    let mut ledger = ConsentLedger::new();
    ledger
        .grant(client.to_owned(), scope, NOW_MS, TTL_MS, "test".to_owned())
        .expect("grant");
    ledger
}

#[test]
fn read_only_tool_dispatches_with_scope_and_consent() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register");
    let request =
        ToolRequest::new("terminal_read_zone", br#"{"zone":"output"}"#.to_vec()).with_target("t:1");
    let outcome = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            1,
        )
        .expect("read-only dispatch serves");
    assert_eq!(outcome.tool, "terminal_read_zone");
    assert_eq!(outcome.client_id, CLIENT);
    assert_eq!(outcome.execution_id, 1);
    assert!(outcome.is_untrusted_surface);
    assert_eq!(outcome.data, b"$ echo hello\nhello\n".to_vec());
}

#[test]
fn unknown_tool_fails_closed_before_dispatch() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_destroy", b"{}".to_vec());
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            2,
        )
        .expect_err("unknown tool must fail closed");
    assert!(matches!(error, IpcError::NotFound { .. }), "got {error:?}");
}

#[test]
fn missing_scope_fails_closed() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_read_zone", b"{}".to_vec());
    let error = service
        .dispatch(
            &request,
            &granted(&[]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            3,
        )
        .expect_err("missing scope must fail closed");
    assert!(
        matches!(error, IpcError::ScopeDenied { .. }),
        "got {error:?}"
    );
}

#[test]
fn missing_consent_fails_closed() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_read_zone", b"{}".to_vec());
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &ConsentLedger::new(),
            CLIENT,
            NOW_MS,
            4,
        )
        .expect_err("missing consent must fail closed");
    assert!(matches!(error, IpcError::Denied { .. }), "got {error:?}");
}

#[test]
fn effect_tool_is_denied_by_default_without_explicit_path() {
    let mut service = ToolDispatchService::new();
    service
        .register(effect_spec(), effect_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_send", b"{}".to_vec()).with_target("t:2");
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInput]),
            &consented(CLIENT, Scope::TerminalInput),
            CLIENT,
            NOW_MS,
            5,
        )
        .expect_err("effect tool must be denied without explicit path");
    assert!(matches!(error, IpcError::Denied { .. }), "got {error:?}");
}

#[test]
fn effect_tool_dispatches_with_explicit_consent_path() {
    let mut service = ToolDispatchService::new();
    service
        .register(effect_spec(), effect_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_send", b"{}".to_vec())
        .with_target("t:2")
        .with_allow_effects(true);
    let outcome = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInput]),
            &consented(CLIENT, Scope::TerminalInput),
            CLIENT,
            NOW_MS,
            6,
        )
        .expect("explicit effect path serves");
    assert_eq!(outcome.tool, "terminal_send");
    assert!(outcome.is_untrusted_surface);
}

#[test]
fn oversized_arguments_fail_closed() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_read_zone", vec![b'x'; MAX_TOOL_ARGS_BYTES + 1]);
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            7,
        )
        .expect_err("oversized args must fail closed");
    assert!(
        matches!(error, IpcError::LimitExceeded { .. }),
        "got {error:?}"
    );
}

#[test]
fn oversized_result_fails_closed() {
    fn big_provider(request: &ToolRequest) -> Result<ToolOutput, IpcError> {
        Ok(ToolOutput {
            target_id: request.target.clone(),
            data: vec![b'y'; MAX_TOOL_RESULT_BYTES + 1],
            summary: "too big".to_owned(),
        })
    }
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), big_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_read_zone", b"{}".to_vec());
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            8,
        )
        .expect_err("oversized result must fail closed");
    assert!(
        matches!(error, IpcError::LimitExceeded { .. }),
        "got {error:?}"
    );
}

#[test]
fn captured_target_mismatch_fails_closed() {
    fn rogue_provider(_request: &ToolRequest) -> Result<ToolOutput, IpcError> {
        Ok(ToolOutput {
            target_id: Some("t:99".to_owned()),
            data: b"rogue".to_vec(),
            summary: "rogue".to_owned(),
        })
    }
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), rogue_provider)
        .expect("register");
    let request = ToolRequest::new("terminal_read_zone", b"{}".to_vec()).with_target("t:1");
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            9,
        )
        .expect_err("target mismatch must fail closed");
    assert!(
        matches!(error, IpcError::InvalidRequest { .. }),
        "got {error:?}"
    );
}

#[test]
fn host_without_handler_fails_closed() {
    let service = ToolDispatchService::new();
    let request = ToolRequest::new("terminal_read_zone", b"{}".to_vec());
    let error = service
        .dispatch(
            &request,
            &granted(&[Scope::TerminalInspect]),
            &consented(CLIENT, Scope::TerminalInspect),
            CLIENT,
            NOW_MS,
            10,
        )
        .expect_err("missing handler must fail closed");
    assert!(matches!(error, IpcError::NotFound { .. }), "got {error:?}");
}

fn second_read_only_spec() -> ToolSpec {
    ToolSpec::new(
        "terminal_read_stats",
        "Read bounded terminal statistics (read-only)",
        br#"{"type":"object"}"#.to_vec(),
        Scope::TerminalInspect,
        true,
    )
    .expect("valid second read-only spec")
}

#[test]
fn consent_is_shared_per_client_scope_not_per_tool() {
    let mut service = ToolDispatchService::new();
    service
        .register(read_only_spec(), read_only_provider)
        .expect("register first");
    service
        .register(second_read_only_spec(), read_only_provider)
        .expect("register second");
    // One grant for (client, TerminalInspect) serves both tools: consent
    // granularity is per (client_id, scope), not per tool name.
    let ledger = consented(CLIENT, Scope::TerminalInspect);
    for (tool, execution_id) in [("terminal_read_zone", 1), ("terminal_read_stats", 2)] {
        let request = ToolRequest::new(tool, br#"{}"#.to_vec()).with_target("t:1");
        let outcome = service
            .dispatch(
                &request,
                &granted(&[Scope::TerminalInspect]),
                &ledger,
                CLIENT,
                NOW_MS,
                execution_id,
            )
            .expect("shared consent must serve both tools");
        assert_eq!(outcome.tool, tool);
    }
}
