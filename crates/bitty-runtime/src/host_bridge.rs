//! Live host bridge: Runtime-owned provider wiring for the Phase-A IPC
//! services (CTX-0439, G-5).
//!
//! The `bitty-ipc` half ([`bitty_ipc::host_bridge`]) owns the bounded live
//! store plus `fn` providers; this module is the Runtime half. It reads
//! **committed** terminal state only (`State::generation`, `OSC 7` cwd,
//! `OSC 133` zone markers, grid text via the accepted inspect extractor)
//! and publishes it for out-of-process consumers. Nothing here paints,
//! writes to the PTY, mutates terminal truth, spawns a process, or opens a
//! socket: publication copies bounded values into the `bitty-ipc` store
//! (`&self` only), exactly like
//! [`Runtime::publish_inspect_snapshot`](crate::Runtime::publish_inspect_snapshot).
//!
//! # What is live and what stays a test double
//!
//! - Snapshot: live. [`publish_live_snapshot`] reads the Runtime's primary
//!   terminal state and serves it through the existing
//!   `SnapshotService::dispatch` path with per-budget bounds and the
//!   provider-echo match. Grid internals (cells, dimensions, cursor, modes)
//!   never escape: only joined text lines, the generation, the cwd, and
//!   zone-kind metadata cross the boundary.
//! - Read-only inspect tools: live. [`live_tool_service`] registers the
//!   `bitty-ipc` read-only inspect tools behind the existing scope,
//!   consent, and explicit effect gates. Effect tools are never registered
//!   here and stay deny-by-default (`NotFound`).
//! - Execution: live process path, allowlist-gated. [`authorized_execution_provider`]
//!   is the [`ExecutionService`](bitty_ipc::ExecutionService) provider that
//!   routes through the CTX-0444 [`HostToolsAuthorizer`](super::spawn::HostToolsAuthorizer)
//!   (real process spawn only where a `[tools.*]` declaration exists) and
//!   then the CTX-0445 real-process runner (argv-direct, `env_clear`,
//!   bounded drain, timeout-kill with `Unknown` reconcile). No other
//!   executable can be reached through this provider: the authorizer owns
//!   the tool vocabulary.
//! - Rich projection: transport stays in `bitty-ipc`; the
//!   fragment-to-`RichBlock` mapping lives in `bitty-rich` (`projection`).
//!
//! # Trust binding at this wiring site (CTX-0421 review outcome)
//!
//! - Snapshot/tools/execution dispatches take server-evaluated [`ScopeSet`](bitty_ipc::ScopeSet)
//!   and server-clock `now_ms`; the IPC half binds them via
//!   [`HostCaller`](bitty_ipc::HostCaller), which has no caller-scopes and
//!   no caller-clock field to smuggle through.
//! - The Lua/plugin path binds `client_id` to the installed plugin identity
//!   (verified at install/activation, never caller-asserted), the clock to
//!   the server `now_ms()`, and scopes to the activation grant snapshot.
//! - What this slice cannot enforce (sequel work, documented honestly):
//!   UID-to-`client_id` allocation on the socket accept boundary, per-tool-name
//!   consent granularity (the accepted ledger is per `(client_id, scope)`),
//!   and line-anchored zone spans (State retains marker ordinals, not rows:
//!   published spans are degenerate `0..=0`, explicitly unanchored —
//!   consumers must key on `(kind, oldest-first position)`, never on lines).
//!
//! # Bounds (accepted contracts, verified first-hand)
//!
//! - Grid text extraction reuses `inspect::INSPECT_MAX_ROWS` (64) and
//!   `inspect::INSPECT_MAX_COLS` (256); the service truncates to budget.
//! - Zone metadata caps at `snapshot::MAX_SNAPSHOT_ZONES` (64, newest kept).
//! - `cwd` is the already-bounded `OSC 7` report (parser-bounded 4096).
//!
//! The module is headless-testable, `forbid(unsafe)`, `std` plus the
//! existing `bitty-ipc` workspace dependency only. No network, no new
//! external crates, no hardcoded host paths.

#![forbid(unsafe_code)]

use bitty_ipc::execution::{ExecutionRequest, RawExecutionOutput};
use bitty_ipc::{
    IpcError, SnapshotData, SnapshotService, ToolDispatchService, live_snapshot_provider,
    publish_live_snapshot as ipc_publish_live_snapshot, register_live_inspect_tools,
};
use bitty_term_state::ZoneKind as StateZoneKind;

use crate::inspect::{INSPECT_MAX_COLS, INSPECT_MAX_ROWS, grid_text_from_state};
use crate::plugin_runtime::spawn::{
    HostToolsAuthorizer, SpawnAuthorizer, spawn_process, validate_resolved,
};
use crate::runtime::Runtime;
use crate::shell_integration::ShellIntegration;

/// Map a live state zone marker to the snapshot zone vocabulary (CP-9).
fn map_zone_kind(kind: StateZoneKind) -> bitty_ipc::ZoneKind {
    match kind {
        StateZoneKind::PromptStart => bitty_ipc::ZoneKind::Prompt,
        StateZoneKind::InputStart => bitty_ipc::ZoneKind::Input,
        StateZoneKind::OutputStart => bitty_ipc::ZoneKind::Command,
        StateZoneKind::OutputEnd => bitty_ipc::ZoneKind::Output,
    }
}

/// Read committed Runtime state as provider input for `terminal_id`.
///
/// Serves the primary terminal state: generation, `OSC 7` cwd, zone-kind
/// metadata (newest [`MAX`](bitty_ipc::MAX_SNAPSHOT_ZONES) markers, spans
/// degenerate `0..=0` — explicitly unanchored, see the module docs), and
/// joined grid text. Grid internals never enter the output.
///
/// # Errors
///
/// Returns `InvalidRequest` when `terminal_id` violates the host
/// `t:<digits>` grammar.
pub fn live_snapshot_data(runtime: &Runtime, terminal_id: &str) -> Result<SnapshotData, IpcError> {
    bitty_ipc::ctl::parse_terminal_id(terminal_id).map(|_| ())?;
    let state = runtime.state();
    let grid = grid_text_from_state(state, INSPECT_MAX_ROWS, INSPECT_MAX_COLS);
    let zones: Vec<bitty_ipc::SemanticZone> = ShellIntegration::zones(state)
        .iter()
        .rev()
        .take(bitty_ipc::MAX_SNAPSHOT_ZONES)
        .rev()
        .map(|record| bitty_ipc::SemanticZone {
            kind: map_zone_kind(record.kind),
            line_start: 0,
            line_end: 0,
        })
        .collect();
    for zone in &zones {
        zone.validate()?;
    }
    Ok(SnapshotData {
        terminal_id: terminal_id.to_owned(),
        generation: state.generation(),
        cwd: ShellIntegration::cwd(state).unwrap_or_default().to_owned(),
        semantic_zones: zones,
        text: grid.lines.join("\n"),
    })
}

/// Publish committed Runtime state for `terminal_id` into the live store.
///
/// Returns whether the entry was stored (`Ok(false)` when the store is at
/// capacity with another terminal: fail-closed, no eviction).
///
/// # Errors
///
/// Returns `InvalidRequest` when `terminal_id` violates the host grammar,
/// or `Internal` when the store lock is poisoned.
pub fn publish_live_snapshot(runtime: &Runtime, terminal_id: &str) -> Result<bool, IpcError> {
    ipc_publish_live_snapshot(live_snapshot_data(runtime, terminal_id)?)
}

/// Snapshot service wired to the live store (fail-closed until published).
#[must_use]
pub fn live_snapshot_service() -> SnapshotService {
    SnapshotService::with_defaults(live_snapshot_provider)
}

/// Tool service with the live read-only inspect tools registered.
///
/// Effect tools are never registered: they stay deny-by-default.
///
/// # Errors
///
/// Returns the registration failure (duplicate or registry capacity).
pub fn live_tool_service() -> Result<ToolDispatchService, IpcError> {
    let mut service = ToolDispatchService::new();
    register_live_inspect_tools(&mut service)?;
    Ok(service)
}

/// [`ExecutionService`](bitty_ipc::ExecutionService) provider over the
/// CTX-0445 real-process path, gated by the CTX-0444 authorizer.
///
/// The executable name is treated as the `[tools.*]` tool identity:
/// well-formed tools without a declaration fail as `NotFound`, and
///   declared tools with non-allowlisted args fail as
///   `Denied[AllowlistDenied]` — before any process contact. Only the
/// authorizer-resolved `(executable, args)` reach the runner, and a
/// resolution that rewrites the executable fails closed as
/// `InvalidRequest`. Scope, consent, and explicit effect opt-in are enforced
/// by the dispatching [`ExecutionService`](bitty_ipc::ExecutionService)
/// before this provider runs; budgets, char-boundary truncation, the
/// `Unknown` agreement, trust labeling, and reconcile/resolve are inherited
/// from that service unchanged.
///
/// # Errors
///
/// - `InvalidRequest` when the tool name violates the host grammar or the
///   authorizer rewrites the executable.
/// - `NotFound` when the tool has no `[tools.*]` declaration.
/// - `Denied[AllowlistDenied]` when the args fall outside the allowlist.
/// - `Unavailable` when the child cannot be spawned at all.
pub fn authorized_execution_provider(
    request: &ExecutionRequest,
) -> Result<RawExecutionOutput, IpcError> {
    let authorizer = HostToolsAuthorizer;
    let resolved = authorizer.authorize(&request.executable, &request.args)?;
    validate_resolved(&resolved)?;
    if resolved.executable != request.executable {
        return Err(IpcError::InvalidRequest {
            reason: "authorizer must not rewrite the executable".into(),
        });
    }
    let mut authorized = ExecutionRequest::new(resolved.executable, resolved.args);
    authorized = authorized.with_cwd(request.cwd.clone());
    authorized = authorized.with_env_policy(request.env_policy.clone());
    if let Some(target) = request.target.clone() {
        authorized = authorized.with_target(target);
    }
    authorized = authorized.with_timeout_ms(request.timeout_ms);
    if let Some(budget) = request.output_budget {
        authorized = authorized.with_output_budget(budget);
    }
    authorized = authorized.with_allow_effects(request.allow_effects);
    authorized.validate()?;
    spawn_process(&authorized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    use bitty_ipc::execution::ExecutionRequest;
    use bitty_ipc::scope::{ConsentLedger, Scope, ScopeSet};
    use bitty_ipc::snapshot::{DetailLevel, SnapshotRequest};
    use bitty_ipc::{
        ExecutionService, INSPECT_STATUS_TOOL, INSPECT_TEXT_TOOL, IpcError,
        clear_live_snapshots_for_tests,
    };

    /// Serialize the store-touching tests in this module (the live store is
    /// process-global; parallel tests must not interleave publishes).
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn live_runtime() -> Runtime {
        Runtime::with_defaults().expect("headless runtime builds")
    }

    fn feed_live(rt: &mut Runtime) {
        rt.handle_pty_bytes(b"typed-live-bytes");
        rt.handle_pty_bytes(b"\x1b]7;file:///example/wd-81\x07");
        rt.handle_pty_bytes(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        rt.handle_pty_bytes(b"\x1b]133;D;0\x07");
    }

    fn granted_inspect() -> ScopeSet {
        ScopeSet::single(Scope::TerminalInspect)
    }

    fn consented_inspect(now_ms: u64) -> ConsentLedger {
        let mut ledger = ConsentLedger::new();
        ledger
            .grant(
                "bridge-tests".to_owned(),
                Scope::TerminalInspect,
                now_ms,
                60_000,
                "test".to_owned(),
            )
            .expect("grant");
        ledger
    }

    #[test]
    fn live_snapshot_serves_committed_state_through_dispatch() {
        let _guard = test_lock();
        clear_live_snapshots_for_tests();
        let mut rt = live_runtime();
        feed_live(&mut rt);
        assert!(publish_live_snapshot(&rt, "t:81").expect("publish serves"));
        let service = live_snapshot_service();
        let snapshot = service
            .dispatch(
                bitty_ipc::SNAPSHOT_METHOD,
                &SnapshotRequest::new("t:81", DetailLevel::Standard),
                &granted_inspect(),
            )
            .expect("live dispatch serves");
        assert_eq!(snapshot.terminal_id, "t:81");
        assert_eq!(snapshot.generation, rt.state().generation());
        assert_eq!(snapshot.cwd, "file:///example/wd-81");
        assert!(
            snapshot.text.contains("typed-live-bytes"),
            "live text must serve, got {:?}",
            snapshot.text
        );
        assert_eq!(
            snapshot
                .semantic_zones
                .iter()
                .map(|zone| zone.kind)
                .collect::<Vec<_>>(),
            vec![
                bitty_ipc::ZoneKind::Prompt,
                bitty_ipc::ZoneKind::Input,
                bitty_ipc::ZoneKind::Command,
                bitty_ipc::ZoneKind::Output,
            ]
        );
        for zone in &snapshot.semantic_zones {
            assert_eq!((zone.line_start, zone.line_end), (0, 0));
        }
        assert!(snapshot.is_untrusted_surface);
    }

    #[test]
    fn live_zone_narrowing_filters_to_one_kind() {
        let _guard = test_lock();
        clear_live_snapshots_for_tests();
        let mut rt = live_runtime();
        feed_live(&mut rt);
        publish_live_snapshot(&rt, "t:82").expect("publish");
        let service = live_snapshot_service();
        let snapshot = service
            .dispatch(
                bitty_ipc::SNAPSHOT_METHOD,
                &SnapshotRequest::new("t:82", DetailLevel::Standard)
                    .with_zone(bitty_ipc::ZoneKind::Prompt),
                &granted_inspect(),
            )
            .expect("narrowed dispatch serves");
        assert_eq!(snapshot.semantic_zones.len(), 1);
        assert_eq!(snapshot.semantic_zones[0].kind, bitty_ipc::ZoneKind::Prompt);
        assert!(snapshot.text.contains("typed-live-bytes"));
    }

    #[test]
    fn live_snapshot_rejects_bad_terminal_grammar() {
        let _guard = test_lock();
        clear_live_snapshots_for_tests();
        let rt = live_runtime();
        let error = publish_live_snapshot(&rt, "nope").expect_err("bad grammar must fail");
        assert!(
            matches!(error, IpcError::InvalidRequest { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn live_snapshot_unpublished_is_not_found() {
        let _guard = test_lock();
        clear_live_snapshots_for_tests();
        let service = live_snapshot_service();
        let error = service
            .dispatch(
                bitty_ipc::SNAPSHOT_METHOD,
                &SnapshotRequest::new("t:83", DetailLevel::Standard),
                &granted_inspect(),
            )
            .expect_err("unpublished must fail closed");
        assert!(matches!(error, IpcError::NotFound { .. }), "got {error:?}");
    }

    #[test]
    fn live_tool_service_serves_text_and_status() {
        let _guard = test_lock();
        clear_live_snapshots_for_tests();
        let mut rt = live_runtime();
        feed_live(&mut rt);
        publish_live_snapshot(&rt, "t:84").expect("publish");
        let service = live_tool_service().expect("register");
        assert_eq!(service.tool_count(), 2);
        let text = service
            .dispatch(
                &bitty_ipc::ToolRequest::new(INSPECT_TEXT_TOOL, b"{}".to_vec()).with_target("t:84"),
                &granted_inspect(),
                &consented_inspect(1_000),
                "bridge-tests",
                1_000,
                61,
            )
            .expect("text serves");
        assert_eq!(
            String::from_utf8(text.data).expect("UTF-8"),
            snapshot_text_for(&rt, "t:84")
        );
        let status = service
            .dispatch(
                &bitty_ipc::ToolRequest::new(INSPECT_STATUS_TOOL, b"{}".to_vec())
                    .with_target("t:84"),
                &granted_inspect(),
                &consented_inspect(1_000),
                "bridge-tests",
                1_000,
                62,
            )
            .expect("status serves");
        let body = String::from_utf8(status.data).expect("UTF-8");
        assert!(body.contains("cwd: file:///example/wd-81"), "got {body:?}");
        assert!(body.contains("zones: 4"), "got {body:?}");
    }

    /// Expected tool text for a published terminal (same committed read the
    /// publish path uses; pins text-equality, not just containment).
    fn snapshot_text_for(rt: &Runtime, terminal_id: &str) -> String {
        live_snapshot_data(rt, terminal_id)
            .expect("live read serves")
            .text
    }

    #[test]
    fn live_tool_service_registers_no_effect_tools() {
        let service = live_tool_service().expect("register");
        let error = service
            .dispatch(
                &bitty_ipc::ToolRequest::new("effect_tool", b"{}".to_vec()),
                &granted_inspect(),
                &consented_inspect(1_000),
                "bridge-tests",
                1_000,
                63,
            )
            .expect_err("unregistered effect tool must fail closed");
        assert!(matches!(error, IpcError::NotFound { .. }), "got {error:?}");
    }

    #[test]
    fn authorized_provider_denies_before_any_spawn() {
        let scope_granted = {
            let mut set = ScopeSet::new();
            set.insert(Scope::ProcessSpawn);
            set
        };
        let mut consent = ConsentLedger::new();
        consent
            .grant(
                "bridge-tests".to_owned(),
                Scope::ProcessSpawn,
                1_000,
                60_000,
                "test".to_owned(),
            )
            .expect("grant");
        let mut service = ExecutionService::with_provider(authorized_execution_provider);
        let undeclared = ExecutionRequest::new("rg", vec!["x".to_owned()]).with_allow_effects(true);
        let error = service
            .dispatch(
                &undeclared,
                &scope_granted,
                &consent,
                "bridge-tests",
                1_000,
                501,
            )
            .expect_err("undeclared tool must fail without spawning");
        assert!(matches!(error, IpcError::NotFound { .. }), "got {error:?}");
        assert!(!service.contains(501), "denied dispatch stores nothing");
        let write_verb =
            ExecutionRequest::new("git", vec!["commit".to_owned()]).with_allow_effects(true);
        let error = service
            .dispatch(
                &write_verb,
                &scope_granted,
                &consent,
                "bridge-tests",
                1_000,
                502,
            )
            .expect_err("write verb must fail without spawning");
        assert!(
            matches!(
                error,
                IpcError::Denied { ref code, .. } if code == "AllowlistDenied"
            ),
            "got {error:?}"
        );
        assert!(!service.contains(502), "denied dispatch stores nothing");
    }
}
