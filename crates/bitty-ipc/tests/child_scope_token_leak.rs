//! R-012 child scope-token leak closure (SEC-09, Issue #1078).
//!
//! A child spawned inside a terminal holds only a short-lived
//! current-terminal scope, never a runtime administrator token, and no
//! credential appears where shell startup or SSH forwarding leaks it
//! (P0-AC-023). The token travels over the PTY-side fd, never via
//! environment; expiry is enforced server-side and replay after expiry
//! fails closed.
//!
//! Headless and network-free: no sockets, no threads, no wall-clock —
//! `now_ms` is caller-supplied throughout.

use bitty_ipc::auth::{
    CHILD_TOKEN_TTL_MS, ChildToken, ChildTokenStore, MAX_CHILD_TOKENS, MAX_TOKEN_TTL_MS,
    PeerCredentials, is_child_eligible_scope, verify_peer_uid,
};
use bitty_ipc::error::{ErrorClass, IpcError};
use bitty_ipc::execution::EnvPolicy;
use bitty_ipc::scope::Scope;

const RUNTIME_UID: u32 = 1000;
const FOREIGN_UID: u32 = 2000;

/// All 13 accepted v1 scopes: exactly one is child-eligible.
const ALL_SCOPES: [Scope; 13] = [
    Scope::TerminalInspect,
    Scope::TerminalInput,
    Scope::TerminalManage,
    Scope::ViewInspect,
    Scope::ViewManage,
    Scope::ConfigInspect,
    Scope::ConfigModify,
    Scope::PluginInspect,
    Scope::PluginManage,
    Scope::ProcessSpawn,
    Scope::DebugInspect,
    Scope::DebugTrace,
    Scope::DebugControl,
];

fn mint(scope: Scope, token: &str, id: &str, ttl_ms: u64) -> Result<ChildToken, IpcError> {
    ChildToken::new(token.into(), scope, id.into(), 0, ttl_ms)
}

// ── short-lived token ───────────────────────────────────────────────────────

#[test]
fn default_ttl_is_sixty_seconds() {
    assert_eq!(CHILD_TOKEN_TTL_MS, 60_000);
    // The ceiling must admit the default TTL: minting at both succeeds
    // (behavioral check; a const-vs-const assert would trip
    // `clippy::assertions_on_constants`).
    let tok = mint(
        Scope::TerminalInspect,
        "tok-default",
        "t:4",
        CHILD_TOKEN_TTL_MS,
    )
    .unwrap();
    assert!(!tok.is_expired(CHILD_TOKEN_TTL_MS - 1));
    assert!(tok.is_expired(CHILD_TOKEN_TTL_MS));
    assert_eq!(tok.expires_at_ms(), CHILD_TOKEN_TTL_MS);
}

#[test]
fn ttl_bounds_are_fail_closed() {
    assert!(mint(Scope::TerminalInspect, "tok", "t:1", 0).is_err());
    assert!(mint(Scope::TerminalInspect, "tok", "t:1", MAX_TOKEN_TTL_MS + 1).is_err());
    assert!(mint(Scope::TerminalInspect, "tok", "t:1", 1).is_ok());
    assert!(mint(Scope::TerminalInspect, "tok", "t:1", MAX_TOKEN_TTL_MS).is_ok());
}

#[test]
fn expiry_replay_fails_closed() {
    let mut store = ChildTokenStore::new();
    store
        .insert(mint(Scope::TerminalInspect, "tok-replay", "t:4", 1_000).unwrap())
        .unwrap();
    assert!(
        store
            .verify("tok-replay", Scope::TerminalInspect, "t:4", 999)
            .is_ok()
    );
    // At and past expiry every presentation fails; replaying never revives.
    for now in [1_000, 1_001, 60_000, u64::MAX] {
        let err = store
            .verify("tok-replay", Scope::TerminalInspect, "t:4", now)
            .unwrap_err();
        assert!(
            matches!(err, IpcError::Unauthenticated { .. }),
            "now={now}: expected Unauthenticated, got {err:?}"
        );
        assert_eq!(err.error_class(), ErrorClass::Unauthenticated);
    }
    let drained = store.drain_expired(1_000);
    assert_eq!(drained, vec!["tok-replay".to_string()]);
    assert!(store.is_empty());
}

#[test]
fn saturating_expiry_never_wraps_live() {
    let tok = ChildToken::new(
        "tok-sat".into(),
        Scope::TerminalInspect,
        "t:4".into(),
        u64::MAX,
        1_000,
    )
    .unwrap();
    assert_eq!(tok.expires_at_ms(), u64::MAX);
    assert!(tok.is_expired(u64::MAX));
}

// ── never a runtime administrator token ─────────────────────────────────────

#[test]
fn only_terminal_inspect_mints() {
    for scope in ALL_SCOPES {
        assert_eq!(
            is_child_eligible_scope(scope),
            scope == Scope::TerminalInspect,
            "eligibility must be terminal.inspect-only, scope={scope}"
        );
        let result = mint(scope, "tok-elig", "t:4", 1_000);
        if scope == Scope::TerminalInspect {
            assert!(result.is_ok(), "terminal.inspect must mint");
        } else {
            let err = result.unwrap_err();
            let (denied_scope, action) = match err {
                IpcError::ScopeDenied { scope, action } => (scope, action),
                other => panic!("{scope}: expected ScopeDenied, got {other:?}"),
            };
            assert_eq!(denied_scope, scope.as_str(), "{scope}: wrong scope field");
            assert_eq!(action, "child token mint", "{scope}: wrong action");
        }
    }
}

#[test]
fn hostile_admin_mint_matrix_denied() {
    // Every scope that would make the child a runtime admin — effectful
    // scopes plus the wider read-only inspects — fails closed at mint.
    for scope in [
        Scope::TerminalInput,
        Scope::TerminalManage,
        Scope::ViewInspect,
        Scope::ViewManage,
        Scope::ConfigInspect,
        Scope::ConfigModify,
        Scope::PluginInspect,
        Scope::PluginManage,
        Scope::ProcessSpawn,
        Scope::DebugInspect,
        Scope::DebugTrace,
        Scope::DebugControl,
    ] {
        let err = mint(scope, "tok-admin", "t:4", 1_000).unwrap_err();
        assert!(
            matches!(err, IpcError::ScopeDenied { .. }),
            "{scope}: admin mint must fail ScopeDenied, got {err:?}"
        );
        assert_eq!(err.error_class(), ErrorClass::Scope);
    }
}

#[test]
fn store_never_serves_unmintable_scope() {
    let mut store = ChildTokenStore::new();
    store
        .insert(mint(Scope::TerminalInspect, "tok-narrow", "t:4", 60_000).unwrap())
        .unwrap();
    // The stored narrow token does not authorize any wider scope, even for
    // the same terminal id.
    for scope in ALL_SCOPES {
        let result = store.verify("tok-narrow", scope, "t:4", 1_000);
        if scope == Scope::TerminalInspect {
            assert!(result.is_ok());
        } else {
            assert!(
                matches!(result.unwrap_err(), IpcError::ScopeDenied { .. }),
                "{scope}: stored token must not authorize wider scope"
            );
        }
    }
    // …nor a sibling terminal id, even with the eligible scope.
    assert!(
        store
            .verify("tok-narrow", Scope::TerminalInspect, "t:5", 1_000)
            .is_err()
    );
}

#[test]
fn store_cap_bounds_token_sprawl() {
    let mut store = ChildTokenStore::new();
    for i in 0..MAX_CHILD_TOKENS {
        store
            .insert(
                mint(
                    Scope::TerminalInspect,
                    &format!("tok-cap-{i}"),
                    &format!("t:{i}"),
                    60_000,
                )
                .unwrap(),
            )
            .unwrap();
    }
    let err = store
        .insert(mint(Scope::TerminalInspect, "tok-overflow", "t:99", 1_000).unwrap())
        .unwrap_err();
    assert!(matches!(err, IpcError::LimitExceeded { .. }));
}

// ── control-byte rejection (PTY-fd framing) ─────────────────────────────────

#[test]
fn control_byte_matrix_rejected() {
    // C0, DEL, and C1 (multi-byte UTF-8, invisible to a raw-byte scan).
    let hostile = [
        "tok\x00nul",
        "tok\x01soh",
        "tok\x07bel",
        "tok\x08bs",
        "tok\nlf",
        "tok\rcr",
        "tok\x1besc",
        "tok\x7fdel",
        "tok\u{85}nel",
        "tok\u{80}pad",
        "tok\u{9b}csi",
        "tok\nSendEnv LD_PRELOAD",
    ];
    for token in hostile {
        let err = mint(Scope::TerminalInspect, token, "t:4", 1_000).unwrap_err();
        assert!(
            matches!(err, IpcError::InvalidRequest { .. }),
            "token {token:?} must fail InvalidRequest, got {err:?}"
        );
        // Same matrix for the scoped id.
        let err_id = ChildToken::new(
            "tok-ok".into(),
            Scope::TerminalInspect,
            token.into(),
            0,
            1_000,
        )
        .unwrap_err();
        assert!(
            matches!(err_id, IpcError::InvalidRequest { .. }),
            "scoped_id {token:?} must fail InvalidRequest, got {err_id:?}"
        );
    }
}

#[test]
fn benign_tokens_still_mint() {
    for (token, id) in [
        ("tok-abc_123.xyz~", "t:4"),
        ("tök-✓-4", "t:4"),
        ("A", "v:12"),
    ] {
        assert!(
            mint(Scope::TerminalInspect, token, id, 1_000).is_ok(),
            "benign token {token:?} id {id:?} must mint"
        );
    }
    assert!(mint(Scope::TerminalInspect, "", "t:1", 1_000).is_err());
    assert!(
        ChildToken::new(
            "tok".into(),
            Scope::TerminalInspect,
            "x".repeat(65),
            0,
            1_000
        )
        .is_err()
    );
}

// ── env / SSH leak probes: env is never authority ───────────────────────────

#[test]
fn isolated_child_env_is_empty() {
    let policy = EnvPolicy::Isolated;
    assert!(policy.is_isolated());
    assert!(policy.is_empty());
    assert_eq!(policy.len(), 0);
    assert!(policy.validate().is_ok());
}

#[test]
fn planted_env_token_grants_nothing() {
    // Hostile probe: the attacker plants the live token value into child-env
    // variables (including a `BITTY_*` name and SSH-forwardable `LC_*`
    // names). Env entries are inert data — the only gate is the token store,
    // keyed on the exact (token, scope, id) triple.
    let real = "live-child-token-abc123";
    let mut store = ChildTokenStore::new();
    store
        .insert(mint(Scope::TerminalInspect, real, "t:4", 60_000).unwrap())
        .unwrap();
    let hostile_env = EnvPolicy::explicit(vec![
        ("BITTY_CHILD_TOKEN".into(), real.into()),
        ("LC_BITTY_TOKEN".into(), real.into()),
        ("LANG".into(), real.into()),
    ])
    .expect("shape-valid hostile env must construct");
    assert_eq!(hostile_env.len(), 3);

    // Guessing from env-shaped names without the exact triple fails.
    assert!(
        store
            .verify("BITTY_CHILD_TOKEN", Scope::TerminalInspect, "t:4", 1_000)
            .is_err()
    );
    // A wrong id fails even with the live token string (env cannot re-scope).
    assert!(
        store
            .verify(real, Scope::TerminalInspect, "t:5", 1_000)
            .is_err()
    );
    // Only the exact triple verifies — env presence added no authority.
    assert!(
        store
            .verify(real, Scope::TerminalInspect, "t:4", 1_000)
            .is_ok()
    );
}

#[test]
fn forged_bitty_socket_and_ssh_env_grant_nothing() {
    // Even with a forged `BITTY_SOCKET` identifier and SSH-forwarded vars in
    // the environment, a foreign UID fails the peer-credential gate before
    // any request is parsed — identifiers are advisory, never credentials.
    let foreign = PeerCredentials::new(FOREIGN_UID, FOREIGN_UID, 99);
    let err = verify_peer_uid(foreign, RUNTIME_UID).unwrap_err();
    assert!(matches!(err, IpcError::Unauthenticated { .. }));
    let good = PeerCredentials::new(RUNTIME_UID, RUNTIME_UID, 42);
    assert!(verify_peer_uid(good, RUNTIME_UID).is_ok());
}

#[test]
fn ssh_forward_probe_errors_are_token_free() {
    // Probes smuggled through SSH-forwardable names fail as unknown tokens
    // and the reasons echo no probe material.
    let mut store = ChildTokenStore::new();
    store
        .insert(mint(Scope::TerminalInspect, "real-token-xyz", "t:4", 1_000).unwrap())
        .unwrap();
    for probe in [
        "LC_ALL=evil-guess-1",
        "SendEnv probe zzz999",
        "attacker-tok",
    ] {
        let err = store
            .verify(probe, Scope::TerminalInspect, "t:4", 500)
            .unwrap_err();
        let reason = match err {
            IpcError::Unauthenticated { reason } => reason,
            other => panic!("probe {probe:?}: expected Unauthenticated, got {other:?}"),
        };
        assert_eq!(reason, "unknown child token");
        assert!(
            !reason.contains(probe),
            "reason must not echo probe {probe:?}: {reason}"
        );
    }
}

// ── token-free errors and redacted debug ────────────────────────────────────

#[test]
fn errors_and_debug_never_carry_token_material() {
    let secret = "secret-child-token-abc123XYZ";
    let mut store = ChildTokenStore::new();
    store
        .insert(mint(Scope::TerminalInspect, secret, "t:4", 1_000).unwrap())
        .unwrap();
    let expired = store
        .verify(secret, Scope::TerminalInspect, "t:4", 1_000)
        .unwrap_err();
    let reason = match expired {
        IpcError::Unauthenticated { reason } => reason,
        other => panic!("expected Unauthenticated, got {other:?}"),
    };
    assert_eq!(reason, "child token expired");
    assert!(!reason.contains(secret));

    let debug = format!(
        "{:?}",
        mint(Scope::TerminalInspect, secret, "t:4", 1_000).unwrap()
    );
    assert!(!debug.contains(secret), "Debug leaked token: {debug}");
    assert!(debug.contains("***REDACTED***"));
    let store_debug = format!("{store:?}");
    assert!(
        !store_debug.contains(secret),
        "store Debug leaked: {store_debug}"
    );
}
