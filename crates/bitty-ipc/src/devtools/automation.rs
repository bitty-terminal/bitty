use super::*;

use crate::error::IpcError;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

// ── test automation (CTX-0188, Amendment A1 candidate) ───────────────────────
//
// `synthesizeInput` (keystroke injection) and `captureFrame` (frame capture)
// for the headless verify harness. Security lens mandatory: fail closed
// everywhere.
//
// - Scopes: `synthesizeInput` requires `debug.control` + `terminal.input`;
//   `captureFrame` requires `debug.trace` + `terminal.inspect` (capability-
//   plus-scope intersection, `getSnapshot` parity). Either missing yields
//   `scope`/`ScopeDenied` with zero partial state.
// - Bearers: per-session single-terminal sub-grants, consent-issued via
//   [`issue_automation_bearer`] (no IPC issuance method, no env/config/flag
//   path, never persisted, 10 min TTL). Unscoped callers (absent, expired,
//   wrong-session, wrong-terminal, wrong-family) get `scope`/`ScopeDenied`.
// - Bounds: 64 events/call, 10 calls/s (`synthesizeInput`), 10 fps
//   (`captureFrame`), params 32 KiB, responses 32 KiB, frames 256 KiB.
// - Redaction: P0-AC-026 parity before any frame enters a response;
//   `pixels` is masked (zero text), per-call opt-in, audited.
// - Headless receipt semantics: `synthesizeInput` success means validated,
//   authorized, rate-checked, and marked synthetic (input-ring publish with
//   indelible `[synthetic]` marker); the servo applies injection on the main
//   thread as follow-up. `captureFrame` serves the redacted grid store.

/// Automation method family bound into each bearer (never widened: a
/// synthesize bearer cannot capture, a capture bearer cannot digest, and
/// vice versa — CTX-0244: widening `Capture` would silently upgrade every
/// outstanding 10-minute bearer into a digest oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationFamily {
    /// `synthesizeInput` family (`debug.control`).
    Synthesize,
    /// `captureFrame` family (`debug.trace`).
    Capture,
    /// `frameHash` digest family (CTX-0244; `debug.trace` +
    /// `terminal.inspect`, 2 min TTL cap, 2 digests/s).
    FrameDigest,
}

impl AutomationFamily {
    /// Canonical family token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthesize => "synthesize",
            Self::Capture => "capture",
            Self::FrameDigest => "frame-digest",
        }
    }
}

/// One issued automation bearer (server-side only, never persisted).
#[derive(Debug, Clone)]
struct AutomationBearerRecord {
    /// Debug-session identity it was issued to.
    session_id: String,
    /// Single terminal it may address (`t:N`).
    terminal_id: String,
    /// Method family it may call.
    family: AutomationFamily,
    /// Expiry time (issuance + TTL, saturating; bearer-clock base matches
    /// `ServeContext::uptime_ms`).
    expires_at_ms: u64,
}

/// One audited frame-observation entry (bounded, drop-oldest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameAuditEntry {
    /// Caller session identity.
    pub session_id: String,
    /// Addressed terminal.
    pub terminal_id: String,
    /// Observation format (`semantic`, `pixels`, or `digest`).
    pub format: String,
    /// Bearer-clock time of capture.
    pub now_ms: u64,
    /// Presented frame sequence the entry attests to (`0` when no frame
    /// was observed, e.g. a denied digest call or a text capture).
    pub frame_seq: u64,
    /// Served digest hex for `digest` entries (uninvertible, safe to log);
    /// empty for `semantic`/`pixels` entries and denied calls.
    pub digest_hex: String,
}

/// Automation store: bearers plus per-bearer rate windows, synthetic sequence,
/// and pixels audit. In-memory only (never persisted, never exported).
#[derive(Debug, Default)]
pub(super) struct AutomationStore {
    /// Token to record.
    bearers: BTreeMap<String, AutomationBearerRecord>,
    /// Per-token `synthesizeInput` timestamps (1 s window).
    synth_hits: BTreeMap<String, std::collections::VecDeque<u64>>,
    /// Per-token `captureFrame` timestamps (1 s window).
    capture_hits: BTreeMap<String, std::collections::VecDeque<u64>>,
    /// Per-token `frameHash` timestamps (1 s window, CTX-0244).
    digest_hits: BTreeMap<String, std::collections::VecDeque<u64>>,
    /// Issuance counter (token uniqueness).
    counter: u64,
    /// Monotonic synthetic-event sequence.
    pub(super) synth_seq: u64,
    /// Bounded pixels/semantic audit (drop-oldest at 64).
    pub(super) audit: Vec<FrameAuditEntry>,
}

/// Global automation store (empty until consent issuance).
pub(super) fn automation_store() -> &'static Mutex<AutomationStore> {
    static STORE: OnceLock<Mutex<AutomationStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(AutomationStore::default()))
}

/// 64-bit FNV-1a (std-only, deterministic token mixing, not a security hash
/// on its own: uniqueness comes from the per-process counter + time + pid).
fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
    const PRIME: u64 = 0x100000001b3;
    let mut hash = seed;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Render a 32-hex bearer token from issuance coordinates.
fn render_bearer_token(
    session_id: &str,
    terminal_id: &str,
    family: AutomationFamily,
    counter: u64,
    now_ms: u64,
) -> String {
    let pid = std::process::id();
    let mut material = Vec::with_capacity(128);
    material.extend_from_slice(session_id.as_bytes());
    material.push(0);
    material.extend_from_slice(terminal_id.as_bytes());
    material.push(0);
    material.extend_from_slice(family.as_str().as_bytes());
    material.push(0);
    material.extend_from_slice(&counter.to_le_bytes());
    material.extend_from_slice(&now_ms.to_le_bytes());
    material.extend_from_slice(&pid.to_le_bytes());
    let h1 = fnv1a64(&material, 0xcbf29ce484222325);
    let h2 = fnv1a64(&material, 0x84222325cbf29ce4);
    format!("{h1:016x}{h2:016x}")
}

/// Validate a session id for bearer binding (1..=64 chars, no NUL/control).
fn validate_session_id(session_id: &str) -> Result<(), IpcError> {
    if session_id.is_empty() || session_id.len() > 64 {
        return Err(IpcError::InvalidRequest {
            reason: "session_id must be 1..=64 bytes".into(),
        });
    }
    if session_id.contains('\0') || session_id.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(IpcError::InvalidRequest {
            reason: "session_id must not contain control bytes".into(),
        });
    }
    Ok(())
}

/// Validate a bearer token shape (opaque, bounded, no control).
fn validate_bearer_shape(token: &str) -> Result<(), ()> {
    if token.is_empty() || token.len() > MAX_BEARER_TOKEN_CHARS {
        return Err(());
    }
    let ok = token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { Ok(()) } else { Err(()) }
}

/// Issue an automation bearer for one session/terminal/family (consent path).
///
/// Called server-side after explicit local-user consent (DevTools gesture or
/// `bitty dev` prompt). There is deliberately no IPC method, env var, config
/// key, flag, or child-inheritance path that issues bearers (P0-AC-023
/// parity; no-bypass audit). The bearer lives in-memory only and expires
/// after [`AUTOMATION_BEARER_TTL_MS`].
///
/// CTX-0244: [`AutomationFamily::FrameDigest`] cannot use this minter — its
/// 10-minute default TTL exceeds the 2-minute digest cap, so digest grants
/// require an explicit `ttl_ms` via
/// [`issue_automation_bearer_with_ttl`] (fail-closed `InvalidRequest`
/// here).
///
/// # Errors
///
/// Returns `InvalidRequest` for bad session/terminal ids and `LimitExceeded`
/// when the store is at capacity (fail-closed, no silent eviction).
pub fn issue_automation_bearer(
    session_id: &str,
    terminal_id: &str,
    family: AutomationFamily,
    now_ms: u64,
) -> Result<String, IpcError> {
    if family == AutomationFamily::FrameDigest {
        return Err(IpcError::InvalidRequest {
            reason: format!(
                "frame-digest bearers require an explicit ttl_ms of 1..={}",
                crate::frame_digest::FRAME_DIGEST_TTL_MS
            ),
        });
    }
    issue_automation_bearer_with_ttl(
        session_id,
        terminal_id,
        family,
        now_ms,
        AUTOMATION_BEARER_TTL_MS,
    )
}

/// Issue with an explicit TTL (capped to [`AUTOMATION_BEARER_TTL_MS`]).
///
/// CTX-0244: [`AutomationFamily::FrameDigest`] bearers carry their own
/// stricter cap ([`crate::frame_digest::FRAME_DIGEST_TTL_MS`], 2 min);
/// larger digest TTLs fail closed.
///
/// # Errors
///
/// Same as [`issue_automation_bearer`], plus `InvalidRequest` when `ttl_ms`
/// is zero or exceeds the cap.
pub fn issue_automation_bearer_with_ttl(
    session_id: &str,
    terminal_id: &str,
    family: AutomationFamily,
    now_ms: u64,
    ttl_ms: u64,
) -> Result<String, IpcError> {
    validate_session_id(session_id)?;
    crate::ctl::parse_terminal_id(terminal_id)?;
    let ttl_cap = if family == AutomationFamily::FrameDigest {
        crate::frame_digest::FRAME_DIGEST_TTL_MS
    } else {
        AUTOMATION_BEARER_TTL_MS
    };
    if ttl_ms == 0 || ttl_ms > ttl_cap {
        return Err(IpcError::InvalidRequest {
            reason: format!("ttl_ms must be 1..={ttl_cap}"),
        });
    }
    let mut store = automation_store()
        .lock()
        .map_err(|_| IpcError::Unavailable {
            reason: "automation store unavailable".into(),
        })?;
    if store.bearers.len() >= MAX_AUTOMATION_BEARERS && !store.bearers.is_empty() {
        // At capacity and no expiry drain helps yet: prune expired first,
        // then fail closed if still full (never silent eviction).
        let expired: Vec<String> = store
            .bearers
            .iter()
            .filter_map(|(tok, rec)| {
                if now_ms >= rec.expires_at_ms {
                    Some(tok.clone())
                } else {
                    None
                }
            })
            .collect();
        for tok in expired {
            store.bearers.remove(&tok);
            store.synth_hits.remove(&tok);
            store.capture_hits.remove(&tok);
            store.digest_hits.remove(&tok);
        }
        if store.bearers.len() >= MAX_AUTOMATION_BEARERS {
            return Err(IpcError::LimitExceeded {
                field: "automation_bearers".into(),
                limit: MAX_AUTOMATION_BEARERS,
                actual: store.bearers.len() + 1,
            });
        }
    }
    store.counter = store.counter.wrapping_add(1);
    let token = render_bearer_token(session_id, terminal_id, family, store.counter, now_ms);
    if store.bearers.contains_key(&token) {
        return Err(IpcError::Internal {
            reason: "bearer token collision (retry issuance)".into(),
        });
    }
    let record = AutomationBearerRecord {
        session_id: session_id.to_string(),
        terminal_id: terminal_id.to_string(),
        family,
        expires_at_ms: now_ms.saturating_add(ttl_ms),
    };
    store.bearers.insert(token.clone(), record);
    Ok(token)
}

/// Revoke one bearer immediately (explicit revoke + session-end parity).
/// Returns true when a bearer was present.
pub fn revoke_automation_bearer(token: &str) -> bool {
    let Ok(mut store) = automation_store().lock() else {
        return false;
    };
    let existed = store.bearers.remove(token).is_some();
    store.synth_hits.remove(token);
    store.capture_hits.remove(token);
    store.digest_hits.remove(token);
    existed
}

/// Clear all automation state (test helper only; production never calls it).
pub fn clear_automation_for_tests() {
    if let Ok(mut store) = automation_store().lock() {
        store.bearers.clear();
        store.synth_hits.clear();
        store.capture_hits.clear();
        store.digest_hits.clear();
        store.counter = 0;
        store.synth_seq = 0;
        store.audit.clear();
    }
    // Input/grid/focus stores are cleared by the caller's introspection
    // helper; automation never clears them here (no cross-module coupling).
}

/// Whether any live [`AutomationFamily::FrameDigest`] bearer exists.
///
/// Production probe for the present path: the runtime publishes RGBA into
/// the digest store only while a digest grant is live, so the multi-MB
/// clone costs nothing when no test holds a grant. Expiry is enforced at
/// authorize time, not here — a stale record only causes bounded extra
/// publishing, never an extra served digest.
#[must_use]
pub fn frame_digest_publish_wanted() -> bool {
    automation_store().lock().is_ok_and(|store| {
        store
            .bearers
            .values()
            .any(|rec| rec.family == AutomationFamily::FrameDigest)
    })
}

/// Number of live bearers (test probe only).
pub fn automation_bearer_count_for_tests() -> usize {
    automation_store()
        .lock()
        .map(|s| s.bearers.len())
        .unwrap_or(0)
}

/// Current synthetic sequence (test probe only).
pub fn synthetic_seq_for_tests() -> u64 {
    automation_store().lock().map(|s| s.synth_seq).unwrap_or(0)
}

/// Number of audited frame captures (test probe only).
pub fn frame_audit_len_for_tests() -> usize {
    automation_store()
        .lock()
        .map(|s| s.audit.len())
        .unwrap_or(0)
}

/// Snapshot of the frame audit log, oldest first (test probe only).
/// Lets digest tests verify `format:"digest"` entries carry the served
/// digest hex; production callers never read the log.
pub fn frame_audit_snapshot_for_tests() -> Vec<FrameAuditEntry> {
    automation_store()
        .lock()
        .map(|s| s.audit.clone())
        .unwrap_or_default()
}

/// Authorize one automation call: scope intersection, bearer binding, expiry,
/// and per-method rate ceiling, atomically under one lock (no TOCTOU).
///
/// `required` holds the two scopes the caller must possess (debug + terminal
/// capability). Bearer failures and scope failures share the typed
/// `scope`/`ScopeDenied` shape (no oracle distinguishing token validity from
/// authority). Rate overruns yield `budget`/`RateLimited` with zero partial
/// state.
pub(super) fn authorize_automation(
    granted: &crate::scope::ScopeSet,
    required: &[crate::scope::Scope; 2],
    token_opt: Option<&str>,
    session_id: &str,
    terminal_id: &str,
    family: AutomationFamily,
    now_ms: u64,
) -> Result<(), HandlerError> {
    for scope in required {
        if !granted.contains(*scope) {
            return Err(HandlerError::new(
                "scope",
                "ScopeDenied",
                format!(
                    "permission denied: scope '{}' denied for automation (needs elevation)",
                    scope.as_str()
                ),
            ));
        }
    }
    let Some(token) = token_opt else {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: missing automation bearer".to_string(),
        ));
    };
    if validate_bearer_shape(token).is_err() {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: invalid automation bearer".to_string(),
        ));
    }
    let mut store = automation_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "automation store unavailable".to_string(),
        )
    })?;
    let record = match store.bearers.get(token) {
        Some(rec) => rec.clone(),
        None => {
            return Err(HandlerError::new(
                "scope",
                "ScopeDenied",
                "permission denied: unknown automation bearer".to_string(),
            ));
        }
    };
    if record.session_id != session_id {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: bearer bound to another session".to_string(),
        ));
    }
    if record.terminal_id != terminal_id {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: bearer bound to another terminal".to_string(),
        ));
    }
    if record.family != family {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: bearer family mismatch".to_string(),
        ));
    }
    if now_ms >= record.expires_at_ms {
        store.bearers.remove(token);
        store.synth_hits.remove(token);
        store.capture_hits.remove(token);
        store.digest_hits.remove(token);
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: automation bearer expired".to_string(),
        ));
    }
    let (cap, hits) = match family {
        AutomationFamily::Synthesize => (MAX_SYNTH_CALLS_PER_SEC, &mut store.synth_hits),
        AutomationFamily::Capture => (MAX_CAPTURE_FPS, &mut store.capture_hits),
        AutomationFamily::FrameDigest => (
            crate::frame_digest::MAX_FRAME_DIGEST_PER_SEC,
            &mut store.digest_hits,
        ),
    };
    let queue = hits.entry(token.to_string()).or_default();
    while let Some(&front) = queue.front() {
        if now_ms.saturating_sub(front) >= 1_000 {
            queue.pop_front();
        } else {
            break;
        }
    }
    if queue.len() >= cap {
        return Err(HandlerError::new(
            "budget",
            "RateLimited",
            format!("rate limited: automation ceiling {cap}/s exceeded"),
        ));
    }
    queue.push_back(now_ms);
    Ok(())
}
