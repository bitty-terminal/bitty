//! Peer-credential authentication for IPC (RFC OQ-018, `SO_PEERCRED`).
//!
//! The runtime creates its directory and socket with `0700`/`0600` and verifies
//! owner at connect via `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED` (macOS) /
//! `SO_PEERCRED` equivalent on BSD. The check confirms that the connecting UID
//! equals the runtime UID. A second local user fails at authentication before
//! any request is parsed (T-09, P0-AC-021 parity).
//!
//! The runtime re-checks peer credentials before each privileged action, not
//! only at connect, so a passed file descriptor cannot be confused for a
//! different principal. It detects or prevents endpoint replacement/tampering:
//! if the directory owner or permissions have changed, it refuses to serve and
//! exits the endpoint rather than falling back to an unauthenticated path.
//!
//! Windows: the named pipe carries a current-user ACL (`GRANT` to the runtime
//! SID only, `DENY` to others at the pipe level). The runtime validates the
//! client token at connect via `GetNamedPipeClientProcessId` plus token SID
//! comparison, equivalent to the Unix peer-credential check.
//!
//! Child scopes: a child process spawned inside a terminal may receive a
//! short-lived, current-terminal scope token only for the narrow operation the
//! parent requested (e.g. one `terminal.text` read scoped to `t:4` with a
//! 60-second TTL). The token is delivered over the PTY-side fd, not via
//! environment, and is never placed in `BITTY_*` variables that shell startup
//! or SSH forwarding would leak (R-012, P0-AC-023 parity). Expiry is enforced
//! server-side; a replayed token after expiry fails closed.
//!
//! This module is **headless, bounded, and `forbid(unsafe)`**. Real
//! `getsockopt(SO_PEERCRED)` and `GetNamedPipeClientProcessId` calls require
//! `unsafe` and live in the runtime/platform seam; this crate exposes only
//! pure, bounded verification over already-extracted credentials so tests run
//! on any host without a live socket.

use crate::error::IpcError;
use crate::scope::Scope;

// ── constants ───────────────────────────────────────────────────────────────

/// Unix socket directory mode (0700, owner only).
pub const DIR_MODE: u32 = 0o700;

/// Unix socket file mode (0600, owner read/write only).
pub const SOCKET_MODE: u32 = 0o600;

/// Default TTL for a short-lived child scope token (60 seconds per RFC).
pub const CHILD_TOKEN_TTL_MS: u64 = 60_000;

/// Maximum TTL allowed for any token (hard ceiling per `MAX_REQUEST_TIMEOUT_MS` parity).
pub const MAX_TOKEN_TTL_MS: u64 = 30_000 * 2; // 60s matches child token but keep explicit

/// Maximum length for a terminal/view identifier scoped to a child token.
pub const MAX_SCOPED_ID_BYTES: usize = 64;

/// Maximum number of active child tokens tracked per endpoint (bounded).
pub const MAX_CHILD_TOKENS: usize = 64;

// ── peer credentials ────────────────────────────────────────────────────────

/// Extracted peer credentials (headless, owned).
///
/// In production the runtime obtains these via `SO_PEERCRED` / `LOCAL_PEERCRED`
/// on the accepted `UnixStream` (which requires `unsafe` in the platform
/// seam). This crate only verifies the already-extracted triple, keeping the
/// crate `forbid(unsafe)` and headless-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerCredentials {
    /// User ID of the peer (connecting process).
    pub uid: u32,
    /// Group ID of the peer.
    pub gid: u32,
    /// Process ID of the peer (0 when not available, e.g. macOS `getpeereid` only gives euid/egid).
    pub pid: i32,
}

impl PeerCredentials {
    /// Create credentials for tests (headless).
    #[must_use]
    pub fn new(uid: u32, gid: u32, pid: i32) -> Self {
        Self { uid, gid, pid }
    }

    /// Current process credentials (caller's own UID/GID/PID) — headless helper.
    ///
    /// Uses `std::process::id` for pid; uid/gid are 0 on non-Unix or when
    /// unavailable without `unsafe` — callers on Unix that need real uid
    /// should supply it from `nix::unistd::getuid` in the runtime seam.
    #[must_use]
    pub fn current() -> Self {
        Self {
            uid: 0,
            gid: 0,
            pid: std::process::id() as i32,
        }
    }
}

/// Verify that `peer`'s UID equals the runtime's expected UID.
///
/// This is the core `SO_PEERCRED` check: the runtime's UID (owner of the
/// socket directory / pipe ACL) must equal the peer's UID, otherwise the
/// peer is a second local user and fails before any request is parsed.
///
/// The check is re-run before each privileged action (not only at connect),
/// so a passed file descriptor cannot be confused for a different principal.
///
/// # Errors
///
/// Returns `IpcError::Unauthenticated` when UIDs differ.
pub fn verify_peer_uid(peer: PeerCredentials, expected_uid: u32) -> Result<(), IpcError> {
    if peer.uid == expected_uid {
        Ok(())
    } else {
        Err(IpcError::Unauthenticated {
            reason: format!(
                "peer uid {} does not match runtime uid {}",
                peer.uid, expected_uid
            ),
        })
    }
}

/// Verify Unix endpoint permissions headlessly.
///
/// Checks:
/// - directory mode must be 0o700,
/// - socket mode must be 0o600,
/// - directory/socket owner must equal `runtime_uid`,
/// - caller must equal `runtime_uid` (peer check).
///
/// If any check fails, the endpoint must refuse to serve and exit the
/// endpoint rather than falling back to an unauthenticated path (fail-closed).
pub fn verify_unix_endpoint(
    runtime_uid: u32,
    peer: PeerCredentials,
    dir_mode: u32,
    dir_owner_uid: u32,
    sock_mode: u32,
    sock_owner_uid: u32,
) -> Result<(), IpcError> {
    if dir_mode != DIR_MODE {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "directory mode {dir_mode:o} != {:o} (must be 0700)",
                DIR_MODE
            ),
        });
    }
    if sock_mode != SOCKET_MODE {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "socket mode {sock_mode:o} != {:o} (must be 0600)",
                SOCKET_MODE
            ),
        });
    }
    if dir_owner_uid != runtime_uid {
        return Err(IpcError::Unauthenticated {
            reason: format!("directory owner {dir_owner_uid} != runtime {runtime_uid}"),
        });
    }
    if sock_owner_uid != runtime_uid {
        return Err(IpcError::Unauthenticated {
            reason: format!("socket owner {sock_owner_uid} != runtime {runtime_uid}"),
        });
    }
    verify_peer_uid(peer, runtime_uid)
}

/// Verify Windows named-pipe ACL headlessly.
///
/// The pipe ACL must grant only the runtime SID; this stub models SID
/// equality as `u64` comparison for headless tests. Real Windows verification
/// uses `GetNamedPipeClientProcessId` plus token SID comparison in the
/// platform seam (requires `unsafe` there, not here).
pub fn verify_windows_pipe(peer_sid: u64, runtime_sid: u64) -> Result<(), IpcError> {
    if peer_sid == runtime_sid {
        Ok(())
    } else {
        Err(IpcError::Unauthenticated {
            reason: format!("pipe peer sid {peer_sid} != runtime sid {runtime_sid}"),
        })
    }
}

// ── child scope token ───────────────────────────────────────────────────────

/// Short-lived, narrow child scope token delivered over PTY fd (not env).
///
/// A child process spawned inside a terminal may receive a token only for the
/// narrow operation the parent requested (e.g. one `terminal.text` read scoped
/// to `t:4` with a 60-second TTL). The token is never placed in `BITTY_*`
/// variables that shell startup or SSH forwarding would leak.
///
/// Tokens are bounded (`<= 64` bytes id, `<= 64` tokens tracked) and expiry
/// is enforced server-side from caller-supplied `now_ms` (deterministic, never
/// wall-clock).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildToken {
    /// Opaque token bytes (bounded, not a credential file).
    pub token: String,
    /// Scope granted to this token (single, narrow).
    pub scope: Scope,
    /// Identifier the scope is restricted to (e.g. `t:4`, `view:2`), bounded 64 bytes.
    pub scoped_id: String,
    /// Creation time (deterministic `now_ms`).
    pub created_at_ms: u64,
    /// Time-to-live in ms (1..=MAX_TOKEN_TTL_MS, default 60s).
    pub ttl_ms: u64,
}

impl ChildToken {
    /// Create a new child token, validating bounds.
    ///
    /// # Errors
    ///
    /// - `InvalidRequest` when `token` empty or >128 bytes,
    /// - `PayloadTooLarge` when `scoped_id` > 64 bytes,
    /// - `InvalidRequest` when `ttl_ms` zero or > `MAX_TOKEN_TTL_MS`.
    pub fn new(
        token: String,
        scope: Scope,
        scoped_id: String,
        created_at_ms: u64,
        ttl_ms: u64,
    ) -> Result<Self, IpcError> {
        if token.is_empty() || token.len() > 128 {
            return Err(IpcError::InvalidRequest {
                reason: format!("child token must be 1..=128 bytes, got {}", token.len()),
            });
        }
        if scoped_id.len() > MAX_SCOPED_ID_BYTES {
            return Err(IpcError::PayloadTooLarge {
                field: "scoped_id".into(),
                limit: MAX_SCOPED_ID_BYTES,
                actual: scoped_id.len(),
            });
        }
        if ttl_ms == 0 || ttl_ms > MAX_TOKEN_TTL_MS {
            return Err(IpcError::InvalidRequest {
                reason: format!("ttl_ms must be 1..={MAX_TOKEN_TTL_MS}, got {ttl_ms}"),
            });
        }
        // Tokens must not carry control bytes (would confuse PTY fd framing).
        if token.bytes().any(|b| b < 0x20 || b == 0x7F)
            || scoped_id.bytes().any(|b| b < 0x20 || b == 0x7F)
        {
            return Err(IpcError::InvalidRequest {
                reason: "token/scoped_id must not contain control bytes".into(),
            });
        }
        Ok(Self {
            token,
            scope,
            scoped_id,
            created_at_ms,
            ttl_ms,
        })
    }

    /// Absolute expiry (saturating).
    #[must_use]
    pub fn expires_at_ms(&self) -> u64 {
        self.created_at_ms.saturating_add(self.ttl_ms)
    }

    /// Whether `now_ms` is at or past expiry.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms()
    }

    /// Whether the token authorizes `scope` on `scoped_id` at `now_ms`.
    #[must_use]
    pub fn authorizes(&self, scope: Scope, scoped_id: &str, now_ms: u64) -> bool {
        !self.is_expired(now_ms) && self.scope == scope && self.scoped_id == scoped_id
    }
}

/// Bounded in-memory store for child tokens (server-side).
///
/// The runtime verifies every child request against this store; replay after
/// expiry fails closed, and the store never grows beyond `MAX_CHILD_TOKENS`.
#[derive(Debug, Default)]
pub struct ChildTokenStore {
    tokens: std::collections::BTreeMap<String, ChildToken>,
}

impl ChildTokenStore {
    /// Create empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of active tokens (including expired until drained).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether no tokens are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Insert a token; fail-closed when at capacity for a new key.
    pub fn insert(&mut self, token: ChildToken) -> Result<(), IpcError> {
        let is_new = !self.tokens.contains_key(&token.token);
        if is_new && self.tokens.len() >= MAX_CHILD_TOKENS {
            return Err(IpcError::LimitExceeded {
                field: "child_tokens".into(),
                limit: MAX_CHILD_TOKENS,
                actual: self.tokens.len() + 1,
            });
        }
        self.tokens.insert(token.token.clone(), token);
        Ok(())
    }

    /// Verify `token_str` authorizes `scope` on `scoped_id` at `now_ms`.
    ///
    /// Returns `Unauthenticated` when token missing or expired, `ScopeDenied`
    /// when scope/id mismatch. No partial state is created on denial (FS-IP1).
    pub fn verify(
        &self,
        token_str: &str,
        scope: Scope,
        scoped_id: &str,
        now_ms: u64,
    ) -> Result<(), IpcError> {
        let tok = self
            .tokens
            .get(token_str)
            .ok_or_else(|| IpcError::Unauthenticated {
                reason: format!("unknown child token '{token_str}'"),
            })?;
        if tok.is_expired(now_ms) {
            return Err(IpcError::Unauthenticated {
                reason: format!("child token '{token_str}' expired"),
            });
        }
        if tok.scope != scope || tok.scoped_id != scoped_id {
            return Err(IpcError::ScopeDenied {
                scope: scope.as_str().into(),
                action: format!("child token scope {} id {}", tok.scope, tok.scoped_id),
            });
        }
        Ok(())
    }

    /// Drain tokens whose expiry is at or past `now_ms`.
    pub fn drain_expired(&mut self, now_ms: u64) -> Vec<String> {
        let expired: Vec<String> = self
            .tokens
            .iter()
            .filter_map(|(k, tok)| {
                if tok.is_expired(now_ms) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for k in &expired {
            self.tokens.remove(k);
        }
        expired
    }

    /// Revoke a token immediately.
    pub fn revoke(&mut self, token_str: &str) -> bool {
        self.tokens.remove(token_str).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Scope;

    #[test]
    fn peer_uid_same_succeeds() {
        let peer = PeerCredentials::new(1000, 1000, 42);
        assert!(verify_peer_uid(peer, 1000).is_ok());
    }

    #[test]
    fn peer_uid_different_fails_unauthenticated() {
        let peer = PeerCredentials::new(1001, 1000, 42);
        let err = verify_peer_uid(peer, 1000).unwrap_err();
        assert!(matches!(err, IpcError::Unauthenticated { .. }));
        assert_eq!(err.error_class(), crate::error::ErrorClass::Unauthenticated);
    }

    #[test]
    fn unix_endpoint_ok() {
        let peer = PeerCredentials::new(1000, 1000, 1);
        assert!(verify_unix_endpoint(1000, peer, 0o700, 1000, 0o600, 1000).is_ok());
    }

    #[test]
    fn unix_endpoint_mode_mismatch_fails() {
        let peer = PeerCredentials::new(1000, 1000, 1);
        let err = verify_unix_endpoint(1000, peer, 0o755, 1000, 0o600, 1000).unwrap_err();
        assert!(matches!(err, IpcError::Unauthenticated { .. }));
        let err2 = verify_unix_endpoint(1000, peer, 0o700, 1000, 0o644, 1000).unwrap_err();
        assert!(matches!(err2, IpcError::Unauthenticated { .. }));
    }

    #[test]
    fn unix_endpoint_owner_mismatch_fails() {
        let peer = PeerCredentials::new(1000, 1000, 1);
        let err = verify_unix_endpoint(1000, peer, 0o700, 999, 0o600, 1000).unwrap_err();
        assert!(matches!(err, IpcError::Unauthenticated { .. }));
        let err2 = verify_unix_endpoint(1000, peer, 0o700, 1000, 0o600, 999).unwrap_err();
        assert!(matches!(err2, IpcError::Unauthenticated { .. }));
    }

    #[test]
    fn windows_pipe_ok_and_mismatch() {
        assert!(verify_windows_pipe(12345, 12345).is_ok());
        assert!(verify_windows_pipe(12345, 99999).is_err());
    }

    #[test]
    fn child_token_lifecycle() {
        let tok = ChildToken::new(
            "tok-abc".into(),
            Scope::TerminalInspect,
            "t:4".into(),
            0,
            60_000,
        )
        .unwrap();
        assert!(!tok.is_expired(59_999));
        assert!(tok.is_expired(60_000));
        assert!(tok.authorizes(Scope::TerminalInspect, "t:4", 10_000));
        assert!(!tok.authorizes(Scope::TerminalInput, "t:4", 10_000));
        assert!(!tok.authorizes(Scope::TerminalInspect, "t:5", 10_000));
    }

    #[test]
    fn child_token_validation() {
        assert!(ChildToken::new("".into(), Scope::TerminalInspect, "t:1".into(), 0, 1000).is_err());
        let long_id = "x".repeat(65);
        assert!(ChildToken::new("tok".into(), Scope::TerminalInspect, long_id, 0, 1000).is_err());
        assert!(ChildToken::new("tok".into(), Scope::TerminalInspect, "t:1".into(), 0, 0).is_err());
        assert!(
            ChildToken::new(
                "tok".into(),
                Scope::TerminalInspect,
                "t:1".into(),
                0,
                MAX_TOKEN_TTL_MS + 1
            )
            .is_err()
        );
        assert!(
            ChildToken::new(
                "bad\x01tok".into(),
                Scope::TerminalInspect,
                "t:1".into(),
                0,
                1000
            )
            .is_err()
        );
    }

    #[test]
    fn child_store_verify_and_expiry() {
        let mut store = ChildTokenStore::new();
        let tok =
            ChildToken::new("tok1".into(), Scope::TerminalInspect, "t:4".into(), 0, 1000).unwrap();
        store.insert(tok).unwrap();
        assert!(
            store
                .verify("tok1", Scope::TerminalInspect, "t:4", 500)
                .is_ok()
        );
        assert!(
            store
                .verify("tok1", Scope::TerminalInspect, "t:5", 500)
                .is_err()
        );
        assert!(
            store
                .verify("tok1", Scope::TerminalInspect, "t:4", 1000)
                .is_err()
        ); // expired
        assert!(
            store
                .verify("unknown", Scope::TerminalInspect, "t:4", 500)
                .is_err()
        );

        let drained = store.drain_expired(1000);
        assert_eq!(drained, vec!["tok1".to_string()]);
        assert!(store.is_empty());
    }

    #[test]
    fn child_store_cap() {
        let mut store = ChildTokenStore::new();
        for i in 0..MAX_CHILD_TOKENS {
            store
                .insert(
                    ChildToken::new(
                        format!("tok{i}"),
                        Scope::TerminalInspect,
                        format!("t:{i}"),
                        0,
                        60_000,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let err = store
            .insert(
                ChildToken::new(
                    "overflow".into(),
                    Scope::TerminalInspect,
                    "t:99".into(),
                    0,
                    1000,
                )
                .unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, IpcError::LimitExceeded { .. }));
    }

    #[test]
    fn bitty_env_not_used_for_auth() {
        // The BITTY_* env vars carry only opaque identifiers, never authority.
        // Forged env without owned socket must still fail peer credential check.
        let forged_peer = PeerCredentials::new(2000, 2000, 99);
        let runtime_uid = 1000;
        // Even if client forges BITTY_SOCKET="...",
        // peer check still fails because UID mismatch.
        assert!(verify_peer_uid(forged_peer, runtime_uid).is_err());
    }
}
