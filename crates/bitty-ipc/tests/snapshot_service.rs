//! Host-registered bounded `terminal.snapshot` read service (CTX-0420, Issue #701).
//!
//! The generic scope registry maps `terminal.snapshot` to `terminal.inspect`
//! (`scope::required_scope_for_method`) but no host dispatcher registers a
//! handler, so out-of-process context reads fail closed. This integration test
//! pins the G-2 contract: a host-registered, zone-scoped, bounded snapshot
//! service under the existing generic scopes, with `is_untrusted_surface`
//! labeling and fail-closed denial for unknown methods and missing scopes.
//!
//! Reference evidence (read-only, never modified here): `bitty-ai` PR #7
//! (`collect_terminal_context` -> `IpcBridge::call("terminal.snapshot", ...)`;
//! `panel.context` fails closed as `UnsupportedHostMethod`).
//!
//! Headless and network-free: no sockets, no threads, no wall-clock.

use bitty_ipc::error::IpcError;
use bitty_ipc::scope::{Scope, ScopeSet, required_scope_for_method};
use bitty_ipc::snapshot::{
    DetailLevel, MAX_SNAPSHOT_CWD_BYTES, MAX_SNAPSHOT_ZONES, SNAPSHOT_METHOD, SemanticZone,
    SnapshotData, SnapshotRequest, SnapshotService, ZoneKind,
};

fn canned_provider(request: &SnapshotRequest) -> Result<SnapshotData, IpcError> {
    Ok(SnapshotData {
        terminal_id: request.terminal_id.clone(),
        generation: 7,
        cwd: "/work/bitty".to_owned(),
        semantic_zones: vec![
            SemanticZone {
                kind: ZoneKind::Command,
                line_start: 0,
                line_end: 1,
            },
            SemanticZone {
                kind: ZoneKind::Output,
                line_start: 2,
                line_end: 5,
            },
        ],
        text: "$ echo hello\nhello\n".to_owned(),
    })
}

fn oversized_provider(request: &SnapshotRequest) -> Result<SnapshotData, IpcError> {
    Ok(SnapshotData {
        terminal_id: request.terminal_id.clone(),
        generation: 9,
        cwd: "x".repeat(MAX_SNAPSHOT_CWD_BYTES + 100),
        semantic_zones: (0..(MAX_SNAPSHOT_ZONES + 20))
            .map(|i| SemanticZone {
                kind: ZoneKind::Output,
                line_start: i as u32,
                line_end: i as u32,
            })
            .collect(),
        text: "a".repeat(100 * 1024),
    })
}

fn granted_inspect() -> ScopeSet {
    ScopeSet::single(Scope::TerminalInspect)
}

#[test]
fn snapshot_handler_is_registered_under_terminal_inspect() {
    assert_eq!(
        required_scope_for_method(SNAPSHOT_METHOD),
        Some(Scope::TerminalInspect),
        "terminal.snapshot must stay under terminal.inspect"
    );
    let service = SnapshotService::with_defaults(canned_provider);
    assert!(
        service.contains(SNAPSHOT_METHOD),
        "host must register a terminal.snapshot handler"
    );
    assert_eq!(service.method_count(), 1);
}

#[test]
fn snapshot_output_is_bounded_per_budget_contracts() {
    let service = SnapshotService::with_defaults(oversized_provider);
    let request = SnapshotRequest::new("t:4", DetailLevel::Minimal).with_max_bytes(4096);
    let snapshot = service
        .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
        .expect("registered handler serves");
    assert!(
        snapshot.text.len() <= 4096,
        "caller max_bytes must bound text, got {}",
        snapshot.text.len()
    );
    assert!(
        snapshot.semantic_zones.len() <= MAX_SNAPSHOT_ZONES,
        "zones must be capped at {MAX_SNAPSHOT_ZONES}"
    );
    assert!(
        snapshot.cwd.len() <= MAX_SNAPSHOT_CWD_BYTES,
        "cwd must be capped at {MAX_SNAPSHOT_CWD_BYTES}"
    );
    assert!(snapshot.truncated, "oversize input must set truncated");
}

#[test]
fn snapshot_detail_budgets_apply_without_caller_ceiling() {
    let service = SnapshotService::with_defaults(oversized_provider);
    for (detail, ceiling) in [
        (DetailLevel::Minimal, 8 * 1024),
        (DetailLevel::Standard, 16 * 1024),
        (DetailLevel::Full, 32 * 1024),
    ] {
        let request = SnapshotRequest::new("t:4", detail);
        let snapshot = service
            .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
            .expect("registered handler serves");
        assert!(
            snapshot.text.len() <= ceiling,
            "{detail:?} must bound text at {ceiling}, got {}",
            snapshot.text.len()
        );
        assert!(snapshot.truncated, "{detail:?} oversize must truncate");
    }
}

#[test]
fn snapshot_is_labeled_untrusted_surface() {
    let service = SnapshotService::with_defaults(canned_provider);
    let request = SnapshotRequest::new("t:4", DetailLevel::Standard);
    let snapshot = service
        .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
        .expect("registered handler serves");
    assert!(
        snapshot.is_untrusted_surface,
        "terminal text is attacker-controlled data, never instructions"
    );
    assert!(
        snapshot.is_untrusted_surface(),
        "accessor must agree with the DTO field"
    );
    assert_eq!(snapshot.terminal_id, "t:4");
    assert_eq!(snapshot.generation, 7);
    assert!(
        !snapshot.truncated,
        "in-budget snapshot must not claim truncation"
    );
}

#[test]
fn snapshot_never_exposes_grid_internals() {
    let service = SnapshotService::with_defaults(canned_provider);
    let request = SnapshotRequest::new("t:4", DetailLevel::Full);
    let snapshot = service
        .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
        .expect("registered handler serves");
    let debug = format!("{snapshot:?}");
    for field in ["cells", "width", "height", "cursor", "grid"] {
        assert!(
            !debug.contains(field),
            "DTO must never carry grid internals, found {field}"
        );
    }
}

#[test]
fn unknown_method_fails_closed_as_not_found() {
    // Mirrors the bitty-ai pressure-test proof: `panel.context` is not in the
    // generic registry, so it must fail closed without side effects.
    assert_eq!(required_scope_for_method("panel.context"), None);
    let service = SnapshotService::with_defaults(canned_provider);
    let request = SnapshotRequest::new("t:4", DetailLevel::Standard);
    let error = service
        .dispatch("panel.context", &request, &granted_inspect())
        .expect_err("unknown method must fail closed");
    assert!(
        matches!(error, IpcError::NotFound { .. }),
        "unknown method must be NotFound, got {error:?}"
    );
    let mut service = SnapshotService::new();
    let rejected = service.register("panel.context", canned_provider);
    assert!(
        matches!(rejected, Err(IpcError::NotFound { .. })),
        "registering an unknown method must fail closed, got {rejected:?}"
    );
    assert!(!service.contains("panel.context"));
}

#[test]
fn missing_scope_fails_closed_without_partial_state() {
    let service = SnapshotService::with_defaults(canned_provider);
    let request = SnapshotRequest::new("t:4", DetailLevel::Standard);
    let error = service
        .dispatch(SNAPSHOT_METHOD, &request, &ScopeSet::new())
        .expect_err("missing scope must fail closed");
    assert!(
        matches!(error, IpcError::ScopeDenied { .. }),
        "missing scope must be ScopeDenied, got {error:?}"
    );
}

#[test]
fn host_without_snapshot_handler_fails_closed() {
    let service = SnapshotService::new();
    assert!(!service.contains(SNAPSHOT_METHOD));
    let request = SnapshotRequest::new("t:4", DetailLevel::Standard);
    let error = service
        .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
        .expect_err("missing handler must fail closed");
    assert!(
        matches!(error, IpcError::NotFound { .. }),
        "missing handler must be NotFound, got {error:?}"
    );
}

#[test]
fn terminal_id_grammar_is_host_shaped() {
    let service = SnapshotService::with_defaults(canned_provider);
    for bad in ["", "term-1", "t:007", "t:", "v:1"] {
        let request = SnapshotRequest::new(bad, DetailLevel::Standard);
        let error = service
            .dispatch(SNAPSHOT_METHOD, &request, &granted_inspect())
            .expect_err("bad terminal id must fail closed");
        assert!(
            matches!(error, IpcError::InvalidRequest { .. }),
            "terminal id {bad:?} must be InvalidRequest, got {error:?}"
        );
    }
}
