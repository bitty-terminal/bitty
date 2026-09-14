//! Generic supervised-execution backend with structured outcome (CTX-0442, Issue #706).
//!
//! DIR-018 step 3 (031 §4-§5, exec hard blocker): Bitty owns PTY/process
//! lifecycle but has no generic supervised-execution primitive, so a future
//! `exec` tool cannot run under host supervision with bounded evidence.
//! This integration test pins the contract: `ExecutionRequest`
//! (executable/args/cwd/env_policy/target/timeout/output_budget) dispatches
//! through scope + consent + explicit effect gating into a bounded
//! `ExecutionResult` with a closed effect state, and `Unknown` reconciles
//! through an explicit query path instead of a blind retry.
//!
//! Reference evidence (read-only, never modified here): CTX-0420 snapshot
//! service (`SnapshotService` bounded DTO + provider-echo match +
//! `is_untrusted_surface`) and CTX-0421 tool dispatch (`ToolDispatchService`
//! routing + authorization + consent + captured target + budget +
//! attribution + outcome, effect tools need explicit opt-in).
//!
//! Headless and network-free: no sockets, no threads, no wall-clock, no
//! process spawn. The provider is a pure `fn`.

use bitty_ipc::error::IpcError;
use bitty_ipc::execution::{
    EffectState, EnvPolicy, ExecutionRequest, ExecutionService, ExecutionStatus,
    MAX_EXEC_EVIDENCE_REF_BYTES, MAX_EXEC_EVIDENCE_REFS, MAX_EXEC_STREAM_BYTES, RawExecutionOutput,
};
use bitty_ipc::scope::{ConsentLedger, Scope, ScopeSet};

const CLIENT: &str = "agent-0442";
const NOW_MS: u64 = 1_000;
const TTL_MS: u64 = 60_000;

fn minimal_request() -> ExecutionRequest {
    ExecutionRequest::new("git", vec!["diff".to_owned(), "--stat".to_owned()])
}

fn canned_provider(request: &ExecutionRequest) -> Result<RawExecutionOutput, IpcError> {
    Ok(RawExecutionOutput {
        target_id: request.target.clone(),
        status: ExecutionStatus::Completed,
        exit_code: Some(0),
        stdout: "$ git diff --stat\n 1 file changed\n".to_owned(),
        stderr: String::new(),
        evidence_refs: vec!["evidence:git-diff-stat".to_owned()],
        effect_state: EffectState::Completed,
    })
}

fn granted_spawn() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::ProcessSpawn);
    set
}

fn service() -> ExecutionService {
    ExecutionService::with_provider(canned_provider)
}

fn consented() -> ConsentLedger {
    let mut ledger = ConsentLedger::new();
    ledger
        .grant(
            CLIENT.to_owned(),
            Scope::ProcessSpawn,
            NOW_MS,
            TTL_MS,
            "test".to_owned(),
        )
        .expect("grant");
    ledger
}

fn dispatch_ok(
    service: &mut ExecutionService,
    request: &ExecutionRequest,
    execution_id: u64,
) -> bitty_ipc::execution::ExecutionResult {
    service
        .dispatch(
            request,
            &granted_spawn(),
            &consented(),
            CLIENT,
            NOW_MS,
            execution_id,
        )
        .expect("dispatch serves")
}

#[test]
fn execution_dispatches_with_scope_consent_and_effect_opt_in() {
    let mut service = service();
    let request = minimal_request().with_allow_effects(true);
    let outcome = dispatch_ok(&mut service, &request, 1);
    assert_eq!(outcome.execution_id, 1);
    assert_eq!(outcome.client_id, CLIENT);
    assert_eq!(outcome.status, ExecutionStatus::Completed);
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.effect_state, EffectState::Completed);
    assert!(!outcome.needs_reconciliation());
    assert!(outcome.is_untrusted_surface);
    assert!(outcome.is_untrusted_surface());
    assert!(!outcome.truncated);
    assert!(outcome.stdout_summary.contains("1 file changed"));
}

#[test]
fn missing_scope_fails_closed() {
    let mut service = ExecutionService::new();
    let request = minimal_request().with_allow_effects(true);
    let error = service
        .dispatch(&request, &ScopeSet::new(), &consented(), CLIENT, NOW_MS, 2)
        .expect_err("missing scope must fail closed");
    assert!(
        matches!(error, IpcError::ScopeDenied { .. }),
        "got {error:?}"
    );
}

#[test]
fn missing_consent_fails_closed() {
    let mut service = ExecutionService::new();
    let request = minimal_request().with_allow_effects(true);
    let error = service
        .dispatch(
            &request,
            &granted_spawn(),
            &ConsentLedger::new(),
            CLIENT,
            NOW_MS,
            3,
        )
        .expect_err("missing consent must fail closed");
    assert!(matches!(error, IpcError::Denied { .. }), "got {error:?}");
}

#[test]
fn execution_without_explicit_effect_opt_in_is_denied() {
    let mut service = ExecutionService::new();
    let request = minimal_request();
    assert!(
        !request.allow_effects,
        "execution requests deny effects by default"
    );
    let error = service
        .dispatch(&request, &granted_spawn(), &consented(), CLIENT, NOW_MS, 4)
        .expect_err("effect without opt-in must deny");
    assert!(matches!(error, IpcError::Denied { .. }), "got {error:?}");
}

#[test]
fn oversized_stdout_is_bounded_never_silently_grown() {
    fn big_provider(request: &ExecutionRequest) -> Result<RawExecutionOutput, IpcError> {
        Ok(RawExecutionOutput {
            target_id: request.target.clone(),
            status: ExecutionStatus::Completed,
            exit_code: Some(0),
            stdout: "o".repeat(MAX_EXEC_STREAM_BYTES + 1),
            stderr: String::new(),
            evidence_refs: Vec::new(),
            effect_state: EffectState::Completed,
        })
    }
    let mut service = ExecutionService::with_provider(big_provider);
    let request = minimal_request().with_allow_effects(true);
    let outcome = dispatch_ok(&mut service, &request, 5);
    assert!(
        outcome.stdout_summary.len() <= MAX_EXEC_STREAM_BYTES,
        "stdout must be bounded at {MAX_EXEC_STREAM_BYTES}"
    );
    assert!(outcome.truncated, "oversize output must set truncated");
}

#[test]
fn caller_output_budget_narrows_the_stream_ceiling() {
    let mut service = service();
    let request = minimal_request()
        .with_allow_effects(true)
        .with_output_budget(16);
    let outcome = dispatch_ok(&mut service, &request, 6);
    assert!(
        outcome.stdout_summary.len() <= 16,
        "caller budget must narrow stdout"
    );
    assert!(outcome.truncated, "narrowed output must set truncated");
}

#[test]
fn provider_target_mismatch_fails_closed() {
    fn rogue_provider(_request: &ExecutionRequest) -> Result<RawExecutionOutput, IpcError> {
        Ok(RawExecutionOutput {
            target_id: Some("t:99".to_owned()),
            status: ExecutionStatus::Completed,
            exit_code: Some(0),
            stdout: "rogue".to_owned(),
            stderr: String::new(),
            evidence_refs: Vec::new(),
            effect_state: EffectState::Completed,
        })
    }
    let mut service = ExecutionService::with_provider(rogue_provider);
    let request = minimal_request()
        .with_allow_effects(true)
        .with_target("t:1");
    let error = service
        .dispatch(&request, &granted_spawn(), &consented(), CLIENT, NOW_MS, 7)
        .expect_err("target mismatch must fail closed");
    assert!(
        matches!(error, IpcError::InvalidRequest { .. }),
        "got {error:?}"
    );
}

#[test]
fn unknown_reconciles_through_the_query_path_never_blind_retries() {
    fn unknown_provider(request: &ExecutionRequest) -> Result<RawExecutionOutput, IpcError> {
        Ok(RawExecutionOutput {
            target_id: request.target.clone(),
            status: ExecutionStatus::Unknown,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            evidence_refs: Vec::new(),
            effect_state: EffectState::Unknown,
        })
    }
    let mut service = ExecutionService::with_provider(unknown_provider);
    let request = minimal_request().with_allow_effects(true);
    let outcome = dispatch_ok(&mut service, &request, 8);
    assert_eq!(outcome.effect_state, EffectState::Unknown);
    assert!(outcome.needs_reconciliation());
    // The reconciliation query path returns the stored Unknown outcome.
    let reconciled = service.reconcile(8).expect("Unknown must be queryable");
    assert_eq!(reconciled.effect_state, EffectState::Unknown);
    // Blind retry under the same id is rejected: no silent overwrite.
    let retry = service.dispatch(&request, &granted_spawn(), &consented(), CLIENT, NOW_MS, 8);
    assert!(
        matches!(retry, Err(IpcError::InvalidRequest { .. })),
        "re-dispatch under a tracked id must fail, got {retry:?}"
    );
    // Explicit resolution moves Unknown to a terminal state.
    let resolved = bitty_ipc::execution::ExecutionResult::new(
        8,
        CLIENT.to_owned(),
        None,
        ExecutionStatus::Completed,
        Some(0),
        "done".to_owned(),
        String::new(),
        false,
        Vec::new(),
        EffectState::Completed,
    )
    .expect("valid terminal result");
    service.resolve(8, resolved).expect("resolve serves");
    let after = service.reconcile(8).expect("stored");
    assert_eq!(after.effect_state, EffectState::Completed);
    assert!(!after.needs_reconciliation());
}

#[test]
fn evidence_refs_are_bounded() {
    let service = ExecutionService::new();
    let refs: Vec<String> = (0..(MAX_EXEC_EVIDENCE_REFS + 1))
        .map(|i| format!("evidence:{i}"))
        .collect();
    let request = minimal_request().with_allow_effects(true);
    let mut output = canned_provider(&request).expect("canned");
    output.evidence_refs = refs;
    let error = output.validate().expect_err("over-count refs must fail");
    assert!(
        matches!(error, IpcError::LimitExceeded { .. }),
        "got {error:?}"
    );
    let long = "e".repeat(MAX_EXEC_EVIDENCE_REF_BYTES + 1);
    let mut output = canned_provider(&request).expect("canned");
    output.evidence_refs = vec![long];
    let error = output.validate().expect_err("over-long ref must fail");
    assert!(
        matches!(error, IpcError::LimitExceeded { .. }),
        "got {error:?}"
    );
    let _ = service;
}

#[test]
fn env_policy_is_closed_no_ambient_passthrough() {
    assert!(
        EnvPolicy::Isolated.is_isolated(),
        "isolated policy must report isolated"
    );
    let explicit = EnvPolicy::explicit(vec![("GIT_PAGER".to_owned(), "cat".to_owned())])
        .expect("bounded explicit env");
    assert!(!explicit.is_isolated());
    // Ambient inheritance cannot be expressed: the enum has no inherit
    // variant, so matching stays exhaustive over isolated/explicit only.
    match explicit {
        EnvPolicy::Isolated => panic!("want explicit"),
        EnvPolicy::Explicit { .. } => {}
    }
}
