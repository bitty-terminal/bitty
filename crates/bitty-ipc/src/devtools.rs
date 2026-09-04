//! DevTools-facing socket contract (CTX-0144, Issue #236).
//!
//! This module is the headless, bounded core of the `BITTY_SOCKET` server that
//! `bitty-app` exposes for `bitty-devtools`. It owns no socket, spawns no
//! thread, and performs no ambient I/O beyond caller-supplied streams and the
//! socket-directory it is explicitly handed: the listener lifecycle lives in
//! `bitty-app/src/ipc_serve.rs` (the servo), which calls into this module.
//!
//! # Reference-first (DEC-0017)
//!
//! Wire behavior mirrors the sibling `bitty-devtools` repository (read-only,
//! never modified here):
//!
//! - Framing: length-prefixed `u32` big-endian + payload `<= 256 KiB`, per
//!   `bitty-devtools/src/transport.ts` (`encodeFrame` / `decodeFrame`,
//!   `MAX_FRAME_BYTES = 256 KiB`, `Framer`). Reuse here is
//!   [`crate::frame::encode_frame`] / [`crate::frame::decode_frame`].
//! - Envelope: versioned JSON with `version: "1.0"`, numeric `id`, and
//!   `method` starting with `bitty.debug/`, per
//!   `bitty-devtools/src/protocol.ts` (`RequestFrame` / `ResponseFrame`,
//!   `encodeRequest` / `decodeResponse`, `negotiateVersion`,
//!   `isValidMethodForScope`). Both shapes the sibling repo emits are
//!   accepted: with `jsonrpc: "2.0"` (`protocol.ts`) and without it
//!   (`transport.ts` `IpcRequest`). Responses always carry `jsonrpc: "2.0"`
//!   so both sibling decoders accept them.
//! - Endpoint: `BITTY_SOCKET` advisory override, else
//!   `$XDG_RUNTIME_DIR/bitty/<instance>.sock` with instance scoping and
//!   `0700` directory / `0600` socket modes plus peer-UID equality, per
//!   `bitty-devtools/src/auth.ts` (`resolveSocketPath`,
//!   `verifyUnixEndpoint`, `DIR_MODE`, `SOCKET_MODE`). `BITTY_SOCKET` and
//!   `BITTY_INSTANCE_ID` are advisory identifiers, never credentials: every
//!   connection still requires peer-credential equality and every request is
//!   evaluated against the dispatch table.
//! - Bounds: `RC-9` (100 req/s, 2x burst, 16 concurrent connections) via
//!   [`crate::limits::RateLimiter`], per-frame 256 KiB via
//!   [`crate::frame::MAX_FRAME_BYTES`], JSON depth `<= 32` via
//!   [`crate::wire::validate_json_depth`].
//!
//! # Scope of this slice (CTX-0144)
//!
//! Handshake plus a minimal read-only round-trip: `bitty.debug/ping`
//! (version/handshake probe) and `bitty.debug/getSnapshot` (runtime-stats
//! snapshot: instance, pid, versions, grid geometry at startup, uptime).
//! The snapshot is deliberately **runtime stats, not live grid text**: live
//! introspection (grid, queues, traces, control) is CTX-0159. The
//! [`Dispatcher`] is an extensible method table so CTX-0159 registers new
//! `bitty.debug/*` handlers without reworking framing, parsing, or the
//! connection loop.
//!
//! # Trust posture
//!
//! Every byte is treated as originating from an **untrusted local client**:
//! peer UID is verified before the first byte is parsed, each frame is
//! length-bounded before allocation, JSON depth is capped, `auth`/`scope`/
//! `role` envelope fields are rejected outright (no ambient authority), and
//! oversize or malformed input fails closed (counted error response or
//! connection close, never a panic, never an unauthenticated fallback).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use crate::auth::{DIR_MODE, SOCKET_MODE};
use crate::auth::{PeerCredentials, verify_peer_uid};
use crate::error::IpcError;
use crate::frame::{MAX_FRAME_BYTES, encode_frame};
use crate::limits::{RC9_MAX_CONNECTIONS, RateLimiter};
use crate::wire::{MAX_JSON_DEPTH, validate_json_depth};

// ── protocol constants ──────────────────────────────────────────────────────

/// DevTools protocol version served by this slice (`protocol.ts`
/// `PROTOCOL_VERSION`).
pub const DEVTOOLS_PROTOCOL_VERSION: &str = "1.0";

/// Required method prefix (`protocol.ts` `encodeRequest` rule).
pub const DEVTOOLS_METHOD_PREFIX: &str = "bitty.debug/";

/// Maximum bytes for `BITTY_SOCKET` (`auth.ts` rejects paths over 512).
pub const MAX_SOCKET_PATH_BYTES: usize = 512;

/// Maximum length of an instance id (`auth.ts`: 1..64).
pub const MAX_INSTANCE_ID_LEN: usize = 64;

/// Maximum bytes for a `bitty.debug/*` method name (parity with
/// [`crate::channel::MAX_METHOD_BYTES`]).
pub const MAX_DEVTOOLS_METHOD_BYTES: usize = 128;

/// Maximum bytes for the raw numeric `id` token echoed verbatim.
pub const MAX_ID_TOKEN_BYTES: usize = 32;

/// Maximum method-suffix length after `bitty.debug/` (camelCase verbs such as
/// `getSnapshot` are far shorter; the cap keeps dispatch total).
pub const MAX_METHOD_SUFFIX_LEN: usize = 64;

/// Maximum error-message characters echoed to the peer (`protocol.ts`
/// `decodeResponse` truncates to 512; we truncate at construction).
pub const MAX_ERROR_MESSAGE_CHARS: usize = 512;

/// Maximum message characters for echoed request fields inside errors.
pub const MAX_ECHO_CHARS: usize = 64;

/// Leaf directory name under the runtime dir (`<base>/bitty/<instance>.sock`).
pub const SOCKET_LEAF_DIR: &str = "bitty";

/// Default instance id (`auth.ts` falls back to `"default"`).
pub const DEFAULT_INSTANCE_ID: &str = "default";

/// Read-idle timeout applied by the servo to each connection (seconds).
/// Candidate value for this slice; CTX-0159 may tune it with RFC numbers.
pub const CONN_IDLE_TIMEOUT_SECS: u64 = 60;

/// Write timeout applied by the servo to each connection (seconds).
/// Candidate value; bounds how long a stuck peer can pin a handler thread.
pub const CONN_WRITE_TIMEOUT_SECS: u64 = 10;

/// Poll interval of the servo accept loop (milliseconds). Keeps shutdown
/// latency low without busy-spinning.
pub const ACCEPT_POLL_INTERVAL_MS: u64 = 20;

// ── socket path ─────────────────────────────────────────────────────────────

/// Resolve the Unix socket path exactly as `bitty-devtools/src/auth.ts`
/// `resolveSocketPath` does (parity, byte-based lengths documented below).
///
/// Precedence: non-empty `bitty_socket` (`BITTY_SOCKET`, advisory) wins
/// verbatim; otherwise `<base>/bitty/<instance>.sock` where `base` is
/// `xdg_runtime_dir` (`XDG_RUNTIME_DIR`) or `/run/user/<uid>`, and `instance`
/// is `instance_id` (`BITTY_INSTANCE_ID`) or `"default"`.
///
/// Validation mirrors the sibling: socket paths over 512 bytes or containing
/// NUL are rejected; instance ids must be 1..=64 ASCII alphanumeric/`-`/`_`
/// (`auth.ts` regex `^[a-z0-9_-]+$`, case-insensitive). Lengths are measured
/// in bytes here (Rust) rather than UTF-16 code units (TypeScript); for the
/// ASCII paths this contract admits, the two agree.
///
/// # Errors
///
/// Returns [`IpcError::InvalidRequest`] for overlong/NUL paths and invalid
/// instance ids.
pub fn resolve_socket_path(
    runtime_uid: u32,
    xdg_runtime_dir: Option<&str>,
    bitty_socket: Option<&str>,
    instance_id: Option<&str>,
) -> Result<String, IpcError> {
    if let Some(sock) = bitty_socket {
        if !sock.is_empty() {
            if sock.len() > MAX_SOCKET_PATH_BYTES {
                return Err(IpcError::InvalidRequest {
                    reason: format!(
                        "BITTY_SOCKET path too long ({} > {MAX_SOCKET_PATH_BYTES})",
                        sock.len()
                    ),
                });
            }
            if sock.contains('\0') {
                return Err(IpcError::InvalidRequest {
                    reason: "BITTY_SOCKET contains NUL".into(),
                });
            }
            return Ok(sock.to_string());
        }
    }
    let instance = instance_id.unwrap_or(DEFAULT_INSTANCE_ID);
    validate_instance_id(instance)?;
    let base = match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => dir.to_string(),
        _ => format!("/run/user/{runtime_uid}"),
    };
    Ok(format!("{base}/{SOCKET_LEAF_DIR}/{instance}.sock"))
}

/// Validate an instance id per `auth.ts` (`1..64`, `^[a-z0-9_-]+$`).
fn validate_instance_id(instance: &str) -> Result<(), IpcError> {
    if instance.is_empty() || instance.len() > MAX_INSTANCE_ID_LEN {
        return Err(IpcError::InvalidRequest {
            reason: format!(
                "instanceId must be 1..={MAX_INSTANCE_ID_LEN}, got {}",
                instance.len()
            ),
        });
    }
    let ok = instance
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(IpcError::InvalidRequest {
            reason: "instanceId must match ^[a-z0-9_-]+$".into(),
        });
    }
    Ok(())
}

/// Advisory environment input for socket discovery.
///
/// `BITTY_SOCKET`, `XDG_RUNTIME_DIR`, and `BITTY_INSTANCE_ID` are identifiers,
/// never credentials; a forged value still fails peer-credential verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SocketEnv {
    /// Value of `BITTY_SOCKET`, when set.
    pub bitty_socket: Option<String>,
    /// Value of `XDG_RUNTIME_DIR`, when set.
    pub xdg_runtime_dir: Option<String>,
    /// Value of `BITTY_INSTANCE_ID`, when set.
    pub instance_id: Option<String>,
}

impl SocketEnv {
    /// Read discovery variables from the process environment (advisory only).
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            bitty_socket: std::env::var("BITTY_SOCKET").ok(),
            xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
            instance_id: std::env::var("BITTY_INSTANCE_ID").ok(),
        }
    }

    /// Effective instance id after defaulting (`"default"`).
    #[must_use]
    pub fn effective_instance(&self) -> String {
        match self.instance_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => DEFAULT_INSTANCE_ID.to_string(),
        }
    }
}

/// Resolve `(socket_path, instance)` from advisory environment.
///
/// `runtime_uid` is only needed to derive the last-resort base
/// `/run/user/<uid>` when neither `BITTY_SOCKET` nor `XDG_RUNTIME_DIR` is
/// set. The servo passes `None` (it has no `getuid` without new
/// dependencies); in that case a missing base fails closed with
/// [`IpcError::Unavailable`] instead of guessing.
///
/// # Errors
///
/// Forwards [`resolve_socket_path`] validation failures, or `Unavailable`
/// when the default base cannot be derived.
pub fn resolve_socket_path_from_env(
    env: &SocketEnv,
    runtime_uid: Option<u32>,
) -> Result<(String, String), IpcError> {
    let has_socket = env.bitty_socket.as_deref().is_some_and(|s| !s.is_empty());
    let has_base = env
        .xdg_runtime_dir
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if !has_socket && !has_base && runtime_uid.is_none() {
        return Err(IpcError::Unavailable {
            reason: "cannot derive socket base without uid; set XDG_RUNTIME_DIR or BITTY_SOCKET"
                .into(),
        });
    }
    let uid = runtime_uid.unwrap_or(0);
    let path = resolve_socket_path(
        uid,
        env.xdg_runtime_dir.as_deref(),
        env.bitty_socket.as_deref(),
        env.instance_id.as_deref(),
    )?;
    Ok((path, env.effective_instance()))
}

// ── server info ─────────────────────────────────────────────────────────────

/// Static server description captured at serve time (wired by `bitty-app`).
///
/// All fields are startup facts, never live terminal content: live grid
/// introspection is CTX-0159. `cols`/`rows` are the grid geometry the runtime
/// was configured with when serving started.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Instance id scoping this socket (validated 1..=64).
    pub instance: String,
    /// Socket path being served.
    pub socket_path: String,
    /// Serving process id.
    pub pid: u32,
    /// Crate version of the serving binary's `bitty-ipc` (workspace version).
    pub app_version: String,
    /// Grid columns at startup.
    pub cols: usize,
    /// Grid rows at startup.
    pub rows: usize,
    /// Wall-clock start time (unix millis, informational only).
    pub started_unix_ms: u64,
    /// Monotonic start time for uptime accounting.
    pub started_at: Instant,
}

impl ServerInfo {
    /// Capture server facts. Total; clock failure yields `started_unix_ms`
    /// zero rather than aborting startup (fail-soft).
    #[must_use]
    pub fn new(instance: String, socket_path: String, cols: usize, rows: usize) -> Self {
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        Self {
            instance,
            socket_path,
            pid: std::process::id(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            cols,
            rows,
            started_unix_ms,
            started_at: Instant::now(),
        }
    }

    /// Milliseconds since [`ServerInfo::new`] (saturating).
    #[must_use]
    pub fn uptime_ms(&self) -> u64 {
        let millis = self.started_at.elapsed().as_millis();
        u64::try_from(millis).unwrap_or(u64::MAX)
    }
}

/// Per-request dispatch context: static server facts plus fresh uptime.
#[derive(Debug, Clone)]
pub struct ServeContext {
    /// Server facts.
    pub server: ServerInfo,
    /// Uptime at request time (millis).
    pub uptime_ms: u64,
}

impl ServeContext {
    /// Build a context from server facts, stamping uptime now.
    #[must_use]
    pub fn new(server: &ServerInfo) -> Self {
        Self {
            server: server.clone(),
            uptime_ms: server.uptime_ms(),
        }
    }
}

// ── request parsing ─────────────────────────────────────────────────────────

/// Parsed DevTools request: the only fields the dispatcher needs.
///
/// `id_raw` is the verbatim JSON number token so responses echo the exact id
/// the client sent (no float formatting drift). `params` is intentionally
/// absent: v1 handlers need none, and the envelope bytes remain available to
/// the caller for CTX-0159 handlers that will parse method-specific params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevtoolsRequest {
    /// Verbatim numeric id token (e.g. `"1"`).
    pub id_raw: String,
    /// Method such as `"bitty.debug/ping"`.
    pub method: String,
    /// Whether the envelope carried `jsonrpc: "2.0"` (`protocol.ts` shape).
    pub has_jsonrpc: bool,
}

/// A parse failure that still maps to an error response.
///
/// Carries the best-known id so the peer can correlate the rejection;
/// `None` (rendered as `0`) when no usable id was recovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFault {
    /// Verbatim id token when recovered.
    pub id_raw: Option<String>,
    /// DevTools error category (`protocol.ts` `ErrorCategory`).
    pub category: &'static str,
    /// Stable error code.
    pub code: &'static str,
    /// Bounded human-readable reason.
    pub message: String,
}

impl RequestFault {
    /// Build a fault, truncating the message to the echo bound.
    fn new(
        id_raw: Option<String>,
        category: &'static str,
        code: &'static str,
        message: String,
    ) -> Self {
        Self {
            id_raw,
            category,
            code,
            message: truncate_chars(&message, MAX_ERROR_MESSAGE_CHARS),
        }
    }
}

/// Truncate to at most `max` characters (char-boundary safe).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}...")
}

/// Truncate a request-echo snippet for error messages.
fn echo_snippet(s: &str) -> String {
    truncate_chars(s, MAX_ECHO_CHARS)
}

/// Unescape a JSON string body (without surrounding quotes).
///
/// Supports the standard escapes plus `\uXXXX` BMP escapes. Surrogate halves
/// are rejected: method/version envelopes never need them.
fn unescape_json_string(body: &str) -> Result<String, ()> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let esc = chars.next().ok_or(())?;
        match esc {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let mut code: u32 = 0;
                for _ in 0..4 {
                    let h = chars.next().ok_or(())?;
                    let digit = h.to_digit(16).ok_or(())?;
                    code = code * 16 + digit;
                }
                let decoded = char::from_u32(code).ok_or(())?;
                if (0xD800..0xE000).contains(&code) {
                    return Err(());
                }
                out.push(decoded);
            }
            _ => return Err(()),
        }
    }
    Ok(out)
}

/// Top-level JSON key spans collected in one pass (key -> raw value span).
struct EnvelopeKeys {
    /// Raw value spans by unescaped key name.
    values: BTreeMap<String, (usize, usize)>,
    /// Best-known id token for fault correlation, when recovered.
    id_raw: Option<String>,
}

/// Scan one JSON string starting at `bytes[i]` (where `bytes[i] == b'"'`).
/// Returns the inner byte range `(content_start, content_end)` and the index
/// just past the closing quote.
fn scan_string(bytes: &[u8], i: usize) -> Result<(usize, usize, usize), ()> {
    let mut j = i + 1;
    let mut escape = false;
    while j < bytes.len() {
        let b = bytes[j];
        if escape {
            escape = false;
        } else if b == b'\\' {
            escape = true;
        } else if b == b'"' {
            return Ok((i + 1, j, j + 1));
        } else if b < 0x20 {
            return Err(());
        }
        j += 1;
    }
    Err(())
}

/// Skip a balanced JSON value starting at `i`; return the index just past it.
fn skip_value(bytes: &[u8], mut i: usize) -> Result<usize, ()> {
    if i >= bytes.len() {
        return Err(());
    }
    match bytes[i] {
        b'"' => {
            let (_, _, end) = scan_string(bytes, i)?;
            Ok(end)
        }
        b'{' | b'[' => {
            let open = bytes[i];
            let close = if open == b'{' { b'}' } else { b']' };
            i += 1;
            let mut depth = 1usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        let (_, _, end) = scan_string(bytes, i)?;
                        i = end;
                        continue;
                    }
                    b if b == open => depth += 1,
                    b if b == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            Err(())
        }
        _ => {
            // Number, true, false, null: run to the next delimiter.
            let start = i;
            while i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']') {
                i += 1;
            }
            if i == start {
                return Err(());
            }
            Ok(i)
        }
    }
}

/// Skip ASCII whitespace.
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Collect top-level key/value spans of a JSON object envelope.
fn collect_envelope_keys(text: &str) -> Result<EnvelopeKeys, ()> {
    let bytes = text.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return Err(());
    }
    i += 1;
    let mut values: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    loop {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            return Err(());
        }
        if bytes[i] == b'}' {
            i += 1;
            i = skip_ws(bytes, i);
            if i != bytes.len() {
                return Err(());
            }
            break;
        }
        if bytes[i] != b'"' {
            return Err(());
        }
        let (ks, ke, after_key) = scan_string(bytes, i)?;
        let key = unescape_json_string(&text[ks..ke]).map_err(|_| ())?;
        i = skip_ws(bytes, after_key);
        if bytes.get(i) != Some(&b':') {
            return Err(());
        }
        i = skip_ws(bytes, i + 1);
        let value_start = i;
        i = skip_value(bytes, i)?;
        values.insert(key, (value_start, i));
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            return Err(());
        }
        if bytes[i] == b',' {
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            continue;
        }
        return Err(());
    }
    Ok(EnvelopeKeys {
        values,
        id_raw: None,
    })
}

/// Extract a required string field by key.
fn required_string_field(
    text: &str,
    keys: &EnvelopeKeys,
    name: &str,
    missing_code: &'static str,
) -> Result<String, RequestFault> {
    let fault_id = keys.id_raw.clone();
    let (start, end) = keys.values.get(name).ok_or_else(|| {
        RequestFault::new(
            fault_id.clone(),
            "usage",
            missing_code,
            format!("envelope missing '{name}'"),
        )
    })?;
    let raw = text[*start..*end].trim().to_string();
    if !raw.starts_with('"') {
        return Err(RequestFault::new(
            fault_id,
            "usage",
            missing_code,
            format!("envelope '{name}' must be a string"),
        ));
    }
    let body = raw[1..raw.len().saturating_sub(1)].to_string();
    unescape_json_string(&body).map_err(|_| {
        RequestFault::new(
            fault_id,
            "usage",
            "InvalidJson",
            format!("envelope '{name}' has invalid string escapes"),
        )
    })
}

/// Validate a JSON number token shape (no float parsing, echo verbatim).
fn is_valid_number_token(token: &str) -> bool {
    if token.is_empty() || token.len() > MAX_ID_TOKEN_BYTES {
        return false;
    }
    let bytes = token.as_bytes();
    let mut i = 0;
    if bytes[i] == b'-' {
        i += 1;
        if i >= bytes.len() {
            return false;
        }
    }
    if bytes[i] == b'0' {
        i += 1;
    } else if bytes[i].is_ascii_digit() && bytes[i] != b'0' {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len()
}

/// Parse and validate a DevTools request envelope.
///
/// Accepts both sibling shapes (`transport.ts` without `jsonrpc`, and
/// `protocol.ts` with `jsonrpc: "2.0"`). Rejects ambient-authority fields,
/// wrong versions, bad methods, and non-numeric ids as [`RequestFault`]
/// values that the caller renders as error responses (never panics).
///
/// # Errors
///
/// Returns a [`RequestFault`] (renderable as an error response) for every
/// malformed or unauthorized envelope; the fault carries the best-known id.
pub fn parse_request(payload: &[u8]) -> Result<DevtoolsRequest, RequestFault> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(RequestFault::new(
            None,
            "transport",
            "FrameTooLarge",
            format!("payload {} exceeds limit {MAX_FRAME_BYTES}", payload.len()),
        ));
    }
    let text = std::str::from_utf8(payload).map_err(|_| {
        RequestFault::new(
            None,
            "transport",
            "InvalidJson",
            "envelope must be utf-8 json".to_string(),
        )
    })?;
    if let Err(ipc_err) = validate_json_depth(payload, MAX_JSON_DEPTH) {
        let (category, code) = match &ipc_err {
            IpcError::PayloadTooLarge { .. } => ("transport", "PayloadTooLarge"),
            _ => ("transport", "InvalidJson"),
        };
        return Err(RequestFault::new(
            None,
            category,
            code,
            format!("envelope rejected: {ipc_err}"),
        ));
    }
    let mut keys = collect_envelope_keys(text).map_err(|_| {
        RequestFault::new(
            None,
            "usage",
            "InvalidRequest",
            "envelope must be a single JSON object".to_string(),
        )
    })?;

    // Recover the id early so later faults correlate.
    if let Some((start, end)) = keys.values.get("id") {
        let token = text[*start..*end].trim().to_string();
        if is_valid_number_token(&token) {
            keys.id_raw = Some(token);
        }
    }

    // No ambient authority travels in the envelope: a client that inserts
    // scope/auth/role cannot escalate; reject explicitly and countably.
    for forbidden in ["auth", "scope", "role"] {
        if keys.values.contains_key(forbidden) {
            return Err(RequestFault::new(
                keys.id_raw.clone(),
                "usage",
                "ForbiddenField",
                format!("forbidden ambient authority field '{forbidden}' in envelope"),
            ));
        }
    }

    let version = required_string_field(text, &keys, "version", "MissingVersion")?;
    if version != DEVTOOLS_PROTOCOL_VERSION {
        return Err(RequestFault::new(
            keys.id_raw.clone(),
            "usage",
            "UnsupportedVersion",
            format!(
                "unsupported version {}, expected {DEVTOOLS_PROTOCOL_VERSION}",
                echo_snippet(&version)
            ),
        ));
    }

    let method = required_string_field(text, &keys, "method", "InvalidRequest")?;
    validate_method(&method).map_err(|reason| {
        RequestFault::new(keys.id_raw.clone(), "usage", "InvalidMethod", reason)
    })?;

    let mut has_jsonrpc = false;
    if keys.values.contains_key("jsonrpc") {
        let tag = required_string_field(text, &keys, "jsonrpc", "InvalidJsonRpc")?;
        if tag != "2.0" {
            return Err(RequestFault::new(
                keys.id_raw.clone(),
                "usage",
                "InvalidJsonRpc",
                format!("jsonrpc must be 2.0, got {}", echo_snippet(&tag)),
            ));
        }
        has_jsonrpc = true;
    }

    let id_raw = keys.id_raw.clone().ok_or_else(|| {
        RequestFault::new(
            None,
            "usage",
            "MissingId",
            "envelope id must be a JSON number".to_string(),
        )
    })?;

    Ok(DevtoolsRequest {
        id_raw,
        method,
        has_jsonrpc,
    })
}

/// Validate a `bitty.debug/*` method name (bounded, ASCII, prefixed).
fn validate_method(method: &str) -> Result<(), String> {
    if method.len() > MAX_DEVTOOLS_METHOD_BYTES {
        return Err(format!(
            "method too long ({} > {MAX_DEVTOOLS_METHOD_BYTES})",
            method.len()
        ));
    }
    let Some(suffix) = method.strip_prefix(DEVTOOLS_METHOD_PREFIX) else {
        return Err(format!(
            "method must start with {DEVTOOLS_METHOD_PREFIX}, got {}",
            echo_snippet(method)
        ));
    };
    if suffix.is_empty() || suffix.len() > MAX_METHOD_SUFFIX_LEN {
        return Err(format!(
            "method suffix must be 1..={MAX_METHOD_SUFFIX_LEN}, got {}",
            echo_snippet(suffix)
        ));
    }
    let ok = suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(format!(
            "method suffix must be ascii alphanumeric, got {}",
            echo_snippet(suffix)
        ));
    }
    Ok(())
}

// ── dispatch ────────────────────────────────────────────────────────────────

/// A handler failure rendered as a DevTools error object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerError {
    /// DevTools error category.
    pub category: &'static str,
    /// Stable error code.
    pub code: &'static str,
    /// Bounded human-readable reason.
    pub message: String,
}

impl HandlerError {
    /// Build a handler failure.
    #[must_use]
    pub fn new(category: &'static str, code: &'static str, message: String) -> Self {
        Self {
            category,
            code,
            message: truncate_chars(&message, MAX_ERROR_MESSAGE_CHARS),
        }
    }
}

/// Handler for one `bitty.debug/*` method.
///
/// Receives the per-request context and the parsed request, and returns the
/// `result` JSON value (already a bounded JSON document) or a
/// [`HandlerError`]. Handlers are pure `fn` pointers so the table stays
/// dependency-free and CTX-0159 can register new methods with one call.
pub type DevtoolsHandler = fn(&ServeContext, &DevtoolsRequest) -> Result<String, HandlerError>;

/// Extensible `bitty.debug/*` dispatch table.
///
/// CTX-0159 adds introspection methods via [`Dispatcher::register`] without
/// touching framing, parsing, or the connection loop.
#[derive(Debug, Default)]
pub struct Dispatcher {
    /// Method name to handler, keyed by full `bitty.debug/*` name.
    handlers: BTreeMap<&'static str, DevtoolsHandler>,
}

impl Dispatcher {
    /// Table with the CTX-0144 round-trip surface (`ping`, `getSnapshot`).
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut table = Self {
            handlers: BTreeMap::new(),
        };
        table.handlers.insert("bitty.debug/ping", handle_ping);
        table
            .handlers
            .insert("bitty.debug/getSnapshot", handle_get_snapshot);
        table
    }

    /// Register a handler for a `bitty.debug/*` method (CTX-0159 hook).
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::InvalidMethod`] when `method` violates the
    /// `bitty.debug/*` grammar enforced by [`validate_method`].
    pub fn register(
        &mut self,
        method: &'static str,
        handler: DevtoolsHandler,
    ) -> Result<(), IpcError> {
        validate_method(method).map_err(|reason| IpcError::InvalidMethod {
            method: method.to_string(),
            reason,
        })?;
        self.handlers.insert(method, handler);
        Ok(())
    }

    /// Whether `method` has a handler.
    #[must_use]
    pub fn contains(&self, method: &str) -> bool {
        self.handlers.contains_key(method)
    }

    /// Number of registered methods.
    #[must_use]
    pub fn method_count(&self) -> usize {
        self.handlers.len()
    }

    /// Dispatch a parsed request to its handler.
    ///
    /// # Errors
    ///
    /// Returns `UnknownMethod` (category `usage`) for well-formed but
    /// unregistered `bitty.debug/*` methods; no partial state is created.
    pub fn dispatch(
        &self,
        context: &ServeContext,
        request: &DevtoolsRequest,
    ) -> Result<String, HandlerError> {
        match self.handlers.get(request.method.as_str()) {
            Some(handler) => handler(context, request),
            None => Err(HandlerError::new(
                "usage",
                "UnknownMethod",
                format!("unknown method {}", echo_snippet(&request.method)),
            )),
        }
    }
}

/// `bitty.debug/ping`: handshake probe echoing the protocol version.
fn handle_ping(
    _context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    Ok(format!(
        "{{\"version\":\"{DEVTOOLS_PROTOCOL_VERSION}\",\"ok\":true}}"
    ))
}

/// Escape a string as a JSON string body (without surrounding quotes).
fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7F => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// `bitty.debug/getSnapshot`: read-only runtime-stats snapshot.
///
/// Returns startup facts (instance, pid, versions, grid geometry, uptime).
/// Live terminal content is out of scope until CTX-0159; the
/// `"snapshot":"runtime-stats"` marker keeps the response honest about what
/// it carries.
fn handle_get_snapshot(
    context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let server = &context.server;
    let mut out = String::with_capacity(256);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"runtime-stats\",\"instance\":\"");
    json_escape_into(&mut out, &server.instance);
    out.push_str("\",\"pid\":");
    out.push_str(&server.pid.to_string());
    out.push_str(",\"app\":\"bitty-app\",\"app_version\":\"");
    json_escape_into(&mut out, &server.app_version);
    out.push_str("\",\"cols\":");
    out.push_str(&server.cols.to_string());
    out.push_str(",\"rows\":");
    out.push_str(&server.rows.to_string());
    out.push_str(",\"uptime_ms\":");
    out.push_str(&context.uptime_ms.to_string());
    out.push_str(",\"started_unix_ms\":");
    out.push_str(&server.started_unix_ms.to_string());
    out.push_str(",\"socket\":\"");
    json_escape_into(&mut out, &server.socket_path);
    out.push_str("\"}");
    Ok(out)
}

// ── responses ───────────────────────────────────────────────────────────────

/// Encode a success response (`protocol.ts` `ResponseFrame` shape).
#[must_use]
pub fn encode_success(id_raw: &str, result_json: &str) -> Vec<u8> {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id_raw},\"result\":{result_json},\"version\":\"{DEVTOOLS_PROTOCOL_VERSION}\"}}"
    )
    .into_bytes()
}

/// Encode an error response, truncating the message to the echo bound.
#[must_use]
pub fn encode_error(id_raw: &str, category: &str, code: &str, message: &str) -> Vec<u8> {
    let bounded = truncate_chars(message, MAX_ERROR_MESSAGE_CHARS);
    let mut escaped = String::with_capacity(bounded.len());
    json_escape_into(&mut escaped, &bounded);
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id_raw},\"error\":{{\"category\":\"{category}\",\"code\":\"{code}\",\"message\":\"{escaped}\"}},\"version\":\"{DEVTOOLS_PROTOCOL_VERSION}\"}}"
    )
    .into_bytes()
}

/// Outcome of handling one envelope: response payload plus error flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleOutcome {
    /// Response payload bytes (unframed; the caller applies `encode_frame`).
    pub response: Vec<u8>,
    /// Whether the response carries `error` rather than `result`.
    pub was_error: bool,
}

/// Handle one complete request envelope: parse, dispatch, serialize.
///
/// Total: every failure mode yields a correlated error response, never a
/// panic and never an `Err`. Framing (`encode_frame`) is left to the caller
/// so both socket and in-memory harnesses share this path.
#[must_use]
pub fn handle_envelope(
    payload: &[u8],
    dispatcher: &Dispatcher,
    context: &ServeContext,
) -> HandleOutcome {
    let request = match parse_request(payload) {
        Ok(request) => request,
        Err(fault) => {
            let id = fault.id_raw.as_deref().unwrap_or("0");
            return HandleOutcome {
                response: encode_error(id, fault.category, fault.code, &fault.message),
                was_error: true,
            };
        }
    };
    match dispatcher.dispatch(context, &request) {
        Ok(result_json) => HandleOutcome {
            response: encode_success(&request.id_raw, &result_json),
            was_error: false,
        },
        Err(handler_err) => HandleOutcome {
            response: encode_error(
                &request.id_raw,
                handler_err.category,
                handler_err.code,
                &handler_err.message,
            ),
            was_error: true,
        },
    }
}

/// Error response for failures before parsing (oversize frame, rate limit
/// with unparseable id). Always uses id `0`.
#[must_use]
pub fn id_zero_error(category: &str, code: &str, message: &str) -> Vec<u8> {
    encode_error("0", category, code, message)
}

// ── socket directory attestation (unix) ─────────────────────────────────────

/// Attested socket-directory facts established before serving.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirAttestation {
    /// Owner UID of the socket directory.
    pub dir_uid: u32,
    /// Permission bits of the socket directory (masked to `0o777`).
    pub dir_mode: u32,
}

/// Ensure the socket's parent directory exists with `0700` semantics.
///
/// Creates missing ancestors without touching their modes, and enforces
/// `0700` on the leaf directory only when this process just created it.
/// A pre-existing leaf with a wrong mode fails closed (never chmod another
/// owner's directory). Returns the leaf's owner and mode for post-bind
/// attestation.
///
/// # Errors
///
/// Returns `Unavailable` for filesystem failures and `Unauthenticated` when
/// a pre-existing leaf violates the `0700` requirement.
#[cfg(unix)]
pub fn prepare_socket_dir(socket_path: &str) -> Result<DirAttestation, IpcError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::Path;

    let path = Path::new(socket_path);
    let parent = path.parent().ok_or_else(|| IpcError::InvalidRequest {
        reason: "socket path has no parent directory".into(),
    })?;
    if parent.as_os_str().is_empty() {
        return Err(IpcError::InvalidRequest {
            reason: "socket path has no parent directory".into(),
        });
    }
    // Create missing ancestors (their modes are left alone: never touch what
    // might be /run/user/<uid> or another owner's directory).
    if let Some(grandparent) = parent.parent() {
        if !grandparent.as_os_str().is_empty() {
            std::fs::create_dir_all(grandparent).map_err(|err| IpcError::Unavailable {
                reason: format!(
                    "cannot create socket directory ancestors {}: {err}",
                    grandparent.display()
                ),
            })?;
        }
    }
    // Create the leaf exclusively: success proves this process created it, so
    // enforcing 0700 is safe. A pre-existing leaf keeps its mode and is
    // verified (never chmodded) below.
    match std::fs::DirBuilder::new()
        .recursive(false)
        .mode(DIR_MODE)
        .create(parent)
    {
        Ok(()) => {
            // umask may have narrowed the mode; set it exactly (ours).
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(DIR_MODE)).map_err(
                |err| IpcError::Unavailable {
                    reason: format!("cannot set socket directory mode: {err}"),
                },
            )?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(IpcError::Unavailable {
                reason: format!("cannot create socket directory {}: {err}", parent.display()),
            });
        }
    }
    attestation_for(parent)
}

/// Read owner/mode attestation for an existing directory.
#[cfg(unix)]
fn attestation_for(parent: &std::path::Path) -> Result<DirAttestation, IpcError> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(parent).map_err(|err| IpcError::Unavailable {
        reason: format!("cannot stat socket directory {}: {err}", parent.display()),
    })?;
    let dir_mode = meta.mode() & 0o777;
    if dir_mode != DIR_MODE {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "socket directory mode {dir_mode:o} != {:o} (must be 0700; refusing to serve)",
                DIR_MODE
            ),
        });
    }
    Ok(DirAttestation {
        dir_uid: meta.uid(),
        dir_mode,
    })
}

/// Attest a freshly bound socket: enforce `0600` and verify endpoint.
///
/// The socket file owner is the serving euid (this process just created it),
/// so `runtime_uid` is established here without `getuid`. Requires the
/// directory owner to match the socket owner and both modes to be exact.
/// Directory replacement after this point cannot escalate: every connection
/// still verifies peer UID equality.
///
/// # Errors
///
/// Returns `Unavailable` for filesystem failures and `Unauthenticated` when
/// any mode/owner check fails (fail-closed: the caller must not serve).
#[cfg(unix)]
pub fn attest_bound_socket(socket_path: &str, dir: &DirAttestation) -> Result<u32, IpcError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(SOCKET_MODE)).map_err(
        |err| IpcError::Unavailable {
            reason: format!("cannot set socket mode 0600: {err}"),
        },
    )?;
    let meta = std::fs::metadata(socket_path).map_err(|err| IpcError::Unavailable {
        reason: format!("cannot stat bound socket: {err}"),
    })?;
    let sock_mode = meta.mode() & 0o777;
    if sock_mode != SOCKET_MODE {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "socket mode {sock_mode:o} != {:o} (must be 0600)",
                SOCKET_MODE
            ),
        });
    }
    let sock_uid = meta.uid();
    if dir.dir_uid != sock_uid {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "socket directory owner {} != socket owner {sock_uid}",
                dir.dir_uid
            ),
        });
    }
    Ok(sock_uid)
}

/// Non-unix stub: socket-directory serving requires a unix platform.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirAttestation {
    /// Placeholder owner (never produced on this platform).
    pub dir_uid: u32,
    /// Placeholder mode (never produced on this platform).
    pub dir_mode: u32,
}

/// Non-unix stub for [`prepare_socket_dir`](fn.prepare_socket_dir).
#[cfg(not(unix))]
pub fn prepare_socket_dir(_socket_path: &str) -> Result<DirAttestation, IpcError> {
    Err(IpcError::Unavailable {
        reason: "unix socket serving requires a unix platform".into(),
    })
}

/// Non-unix stub for [`attest_bound_socket`](fn.attest_bound_socket).
#[cfg(not(unix))]
pub fn attest_bound_socket(_socket_path: &str, _dir: &DirAttestation) -> Result<u32, IpcError> {
    Err(IpcError::Unavailable {
        reason: "unix socket serving requires a unix platform".into(),
    })
}

// ── connection serving ──────────────────────────────────────────────────────

/// Per-connection counters for observability and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConnectionStats {
    /// Requests read from the peer.
    pub requests: u64,
    /// Responses written to the peer.
    pub responses: u64,
    /// Responses carrying `error` (validation, dispatch, or rate limit).
    pub denied: u64,
    /// Framing violations that closed the connection.
    pub framing_errors: u64,
}

/// Serve one connection until EOF, idle timeout, or fatal transport error.
///
/// Reads length-prefixed frames (`u32` BE + payload `<= 256 KiB`), verifies
/// the peer UID **before** parsing the first byte, rate-limits per request
/// (`RC-9` via `limiter` and caller-supplied `clock_ms`), and dispatches via
/// [`handle_envelope`]. Oversize frames get one correlated error response and
/// then the connection closes (fail-closed, no stream desync). Rate-limited
/// requests get an error response and the connection stays open.
///
/// The stream is generic (`Read + Write`) so headless tests drive this exact
/// function over `UnixStream::pair`; the servo passes live streams with
/// read/write timeouts already set. Idle timeouts surface as a clean close
/// (`Ok`), never an error.
///
/// # Errors
///
/// Returns `Unauthenticated` without reading when the peer UID mismatches,
/// or `Transport` when the stream fails mid-protocol.
pub fn serve_connection<S>(
    stream: &mut S,
    peer: PeerCredentials,
    runtime_uid: u32,
    dispatcher: &Dispatcher,
    context: &ServeContext,
    limiter: &mut RateLimiter,
    clock_ms: &dyn Fn() -> u64,
) -> Result<ConnectionStats, IpcError>
where
    S: Read + Write,
{
    verify_peer_uid(peer, runtime_uid)?;
    let mut stats = ConnectionStats::default();
    loop {
        // Read the 4-byte header, distinguishing clean EOF (zero bytes) from
        // truncation, and idle timeout (clean close) from hard failure.
        let mut first = [0u8; 1];
        match stream.read(&mut first) {
            Ok(0) => return Ok(stats),
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                return Ok(stats);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(IpcError::Transport {
                    reason: format!("connection header read failed: {err}"),
                });
            }
        }
        let mut rest = [0u8; 3];
        if stream.read_exact(&mut rest).is_err() {
            stats.framing_errors += 1;
            return Ok(stats);
        }
        let len = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
        if len > MAX_FRAME_BYTES {
            stats.framing_errors += 1;
            let response = id_zero_error(
                "transport",
                "FrameTooLarge",
                &format!("frame {len} exceeds limit {MAX_FRAME_BYTES}"),
            );
            if write_framed(stream, &response).is_err() {
                return Ok(stats);
            }
            return Ok(stats);
        }
        let mut payload = vec![0u8; len];
        match stream.read_exact(&mut payload) {
            Ok(()) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::UnexpectedEof
                    || err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock =>
            {
                stats.framing_errors += 1;
                return Ok(stats);
            }
            Err(err) => {
                return Err(IpcError::Transport {
                    reason: format!("connection payload read failed: {err}"),
                });
            }
        }
        stats.requests += 1;
        if limiter.check(clock_ms()).is_err() {
            stats.denied += 1;
            let id = match parse_request(&payload) {
                Ok(request) => request.id_raw,
                Err(_) => "0".to_string(),
            };
            let response = encode_error(
                &id,
                "budget",
                "RateLimited",
                "rate limited: RC-9 burst exceeded",
            );
            if write_framed(stream, &response).is_err() {
                return Err(IpcError::Transport {
                    reason: "connection response write failed".into(),
                });
            }
            stats.responses += 1;
            continue;
        }
        let outcome = handle_envelope(&payload, dispatcher, context);
        if outcome.was_error {
            stats.denied += 1;
        }
        if write_framed(stream, &outcome.response).is_err() {
            return Err(IpcError::Transport {
                reason: "connection response write failed".into(),
            });
        }
        stats.responses += 1;
    }
}

/// Frame and write one response payload.
fn write_framed<S>(stream: &mut S, response: &[u8]) -> std::io::Result<()>
where
    S: Read + Write,
{
    let wire = encode_frame(response).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response exceeds frame bound",
        )
    })?;
    stream.write_all(&wire)?;
    stream.flush()
}

/// Peer identity attested by the transport layer instead of `SO_PEERCRED`.
///
/// Contract (caller must uphold): the transport kernel-gates peer identity,
/// i.e. only the attested UID could have opened this stream. That holds for
/// the servo's owner-only socket file (`0600`): the kernel refuses `connect`
/// from any other UID with `EACCES` before userspace runs, so the socket
/// owner is the peer. A forged `BITTY_SOCKET` pointing elsewhere still fails
/// because the servo only serves the path it bound and attested itself.
///
/// `SO_PEERCRED` per-connection re-verification (defense in depth against
/// file-descriptor passing) needs either nightly
/// `peer_credentials_unix_socket` (still unstable, rust-lang/rust#42839) or
/// a reviewed `unsafe` `getsockopt` seam, both out of scope for this
/// fail-soft slice; it is recorded hardening for CTX-0159. The headless
/// [`verify_peer_uid`] primitive and its tests already encode the check the
/// live seam will call.
#[must_use]
pub fn transport_attested_peer(runtime_uid: u32) -> PeerCredentials {
    PeerCredentials::new(runtime_uid, runtime_uid, 0)
}

/// Maximum concurrent connections served (`RC-9`, shed newest).
#[must_use]
pub const fn max_connections() -> usize {
    RC9_MAX_CONNECTIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server_info() -> ServerInfo {
        ServerInfo::new(
            "test-inst".to_string(),
            "/run/user/1000/bitty/test-inst.sock".to_string(),
            80,
            24,
        )
    }

    fn test_context() -> ServeContext {
        ServeContext::new(&test_server_info())
    }

    // ── socket path ─────────────────────────────────────────────────────

    #[test]
    fn socket_path_bitty_socket_wins_verbatim() {
        let path =
            resolve_socket_path(1000, Some("/run/user/1000"), Some("/tmp/custom.sock"), None)
                .unwrap();
        assert_eq!(path, "/tmp/custom.sock");
    }

    #[test]
    fn socket_path_xdg_plus_instance() {
        let path =
            resolve_socket_path(1000, Some("/run/user/1000"), None, Some("my-inst_1")).unwrap();
        assert_eq!(path, "/run/user/1000/bitty/my-inst_1.sock");
    }

    #[test]
    fn socket_path_defaults() {
        let path = resolve_socket_path(1000, None, None, None).unwrap();
        assert_eq!(path, "/run/user/1000/bitty/default.sock");
    }

    #[test]
    fn socket_path_empty_socket_falls_through() {
        let path = resolve_socket_path(1000, Some("/run/user/1000"), Some(""), None).unwrap();
        assert_eq!(path, "/run/user/1000/bitty/default.sock");
    }

    #[test]
    fn socket_path_rejects_long_and_nul() {
        let long = "a".repeat(MAX_SOCKET_PATH_BYTES + 1);
        assert!(resolve_socket_path(1000, None, Some(&long), None).is_err());
        assert!(resolve_socket_path(1000, None, Some("/tmp/a\0b.sock"), None).is_err());
    }

    #[test]
    fn socket_path_rejects_bad_instance() {
        assert!(resolve_socket_path(1000, None, None, Some("bad/id")).is_err());
        assert!(resolve_socket_path(1000, None, None, Some("")).is_err());
        let long = "a".repeat(MAX_INSTANCE_ID_LEN + 1);
        assert!(resolve_socket_path(1000, None, None, Some(&long)).is_err());
        assert!(resolve_socket_path(1000, None, None, Some("has space")).is_err());
    }

    #[test]
    fn socket_path_from_env_needs_base_without_uid() {
        let env = SocketEnv::default();
        assert!(resolve_socket_path_from_env(&env, None).is_err());
        let (path, instance) = resolve_socket_path_from_env(&env, Some(1000)).unwrap();
        assert_eq!(path, "/run/user/1000/bitty/default.sock");
        assert_eq!(instance, "default");
    }

    #[test]
    fn socket_path_from_env_socket_override() {
        let env = SocketEnv {
            bitty_socket: Some("/tmp/x.sock".to_string()),
            xdg_runtime_dir: None,
            instance_id: Some("ignored".to_string()),
        };
        let (path, instance) = resolve_socket_path_from_env(&env, None).unwrap();
        assert_eq!(path, "/tmp/x.sock");
        assert_eq!(instance, "ignored");
    }

    // ── parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_transport_shape_without_jsonrpc() {
        let payload = br#"{"id":1,"method":"bitty.debug/ping","params":{},"version":"1.0"}"#;
        let request = parse_request(payload).unwrap();
        assert_eq!(request.id_raw, "1");
        assert_eq!(request.method, "bitty.debug/ping");
        assert!(!request.has_jsonrpc);
    }

    #[test]
    fn parse_protocol_shape_with_jsonrpc() {
        let payload =
            br#"{"jsonrpc":"2.0","id":42,"method":"bitty.debug/getSnapshot","version":"1.0"}"#;
        let request = parse_request(payload).unwrap();
        assert_eq!(request.id_raw, "42");
        assert_eq!(request.method, "bitty.debug/getSnapshot");
        assert!(request.has_jsonrpc);
    }

    #[test]
    fn parse_rejects_wrong_version_with_id() {
        let payload = br#"{"id":7,"method":"bitty.debug/ping","version":"2.0"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "UnsupportedVersion");
        assert_eq!(fault.id_raw.as_deref(), Some("7"));
    }

    #[test]
    fn parse_rejects_missing_version() {
        let payload = br#"{"id":7,"method":"bitty.debug/ping"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "MissingVersion");
    }

    #[test]
    fn parse_rejects_unprefixed_method() {
        let payload = br#"{"id":1,"method":"terminal.text","version":"1.0"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "InvalidMethod");
    }

    #[test]
    fn parse_rejects_string_id() {
        let payload = br#"{"id":"1","method":"bitty.debug/ping","version":"1.0"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "MissingId");
    }

    #[test]
    fn parse_rejects_ambient_authority() {
        let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0","scope":"admin"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "ForbiddenField");
        assert_eq!(fault.id_raw.as_deref(), Some("1"));
    }

    #[test]
    fn parse_allows_nested_scope_in_params() {
        let payload =
            br#"{"id":1,"method":"bitty.debug/ping","version":"1.0","params":{"scope":"value"}}"#;
        assert!(parse_request(payload).is_ok());
    }

    #[test]
    fn parse_rejects_non_object_and_garbage() {
        assert!(parse_request(br#"[1,2]"#).is_err());
        assert!(parse_request(b"not json").is_err());
        assert!(parse_request(b"").is_err());
        assert!(parse_request(&[0xFF, 0xFE]).is_err());
    }

    #[test]
    fn parse_rejects_bad_jsonrpc() {
        let payload = br#"{"jsonrpc":"1.0","id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
        let fault = parse_request(payload).unwrap_err();
        assert_eq!(fault.code, "InvalidJsonRpc");
    }

    #[test]
    fn parse_accepts_escaped_method() {
        let payload = br#"{"id":1,"method":"bitty.debug\u002fping","version":"1.0"}"#;
        let request = parse_request(payload).unwrap();
        assert_eq!(request.method, "bitty.debug/ping");
    }

    #[test]
    fn parse_rejects_deep_nesting() {
        let nested = "[".repeat(MAX_JSON_DEPTH + 1) + &"]".repeat(MAX_JSON_DEPTH + 1);
        assert!(parse_request(nested.as_bytes()).is_err());
    }

    // ── dispatch ────────────────────────────────────────────────────────

    #[test]
    fn dispatch_ping_round_trip() {
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":3,"method":"bitty.debug/ping","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"id\":3"));
        assert!(text.contains("\"ok\":true"));
        assert!(text.contains("\"version\":\"1.0\""));
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn dispatch_snapshot_carries_stats() {
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"jsonrpc":"2.0","id":9,"method":"bitty.debug/getSnapshot","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"id\":9"));
        assert!(text.contains("\"snapshot\":\"runtime-stats\""));
        assert!(text.contains("\"instance\":\"test-inst\""));
        assert!(text.contains("\"cols\":80"));
        assert!(text.contains("\"rows\":24"));
    }

    #[test]
    fn dispatch_unknown_method_is_error() {
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":1,"method":"bitty.debug/nope","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("UnknownMethod"));
        assert!(text.contains("\"id\":1"));
    }

    #[test]
    fn version_mismatch_is_correlated_error() {
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":11,"method":"bitty.debug/ping","version":"9.9"}"#,
            &dispatcher,
            &context,
        );
        assert!(outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("UnsupportedVersion"));
        assert!(text.contains("\"id\":11"));
    }

    #[test]
    fn dispatcher_registers_new_methods_for_follow_up() {
        fn custom(
            context: &ServeContext,
            _request: &DevtoolsRequest,
        ) -> Result<String, HandlerError> {
            Ok(format!("{{\"uptime_ms\":{}}}", context.uptime_ms))
        }
        let mut dispatcher = Dispatcher::with_defaults();
        assert_eq!(dispatcher.method_count(), 2);
        dispatcher
            .register("bitty.debug/customProbe", custom)
            .unwrap();
        assert!(dispatcher.contains("bitty.debug/customProbe"));
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":1,"method":"bitty.debug/customProbe","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        assert!(dispatcher.register("terminal.text", custom).is_err());
    }

    #[test]
    fn error_message_truncated_to_bound() {
        let long = "x".repeat(MAX_ERROR_MESSAGE_CHARS + 100);
        let bytes = encode_error("1", "usage", "InvalidRequest", &long);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.len() < long.len() + 200);
        assert!(text.contains("..."));
    }

    #[test]
    fn id_zero_error_shape() {
        let bytes = id_zero_error("transport", "FrameTooLarge", "too big");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"id\":0"));
        assert!(text.contains("FrameTooLarge"));
    }

    // ── connection serving (unix socketpair, no listener) ───────────────

    #[cfg(unix)]
    #[test]
    fn serve_connection_ping_pong_over_socketpair() {
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let peer = PeerCredentials::new(1000, 1000, 1);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
                1000,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
        });

        let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
        let wire = encode_frame(payload).unwrap();
        client.write_all(&wire).unwrap();

        let mut header = [0u8; 4];
        client.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        assert!(len <= MAX_FRAME_BYTES);
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("\"ok\":true"));

        drop(client);
        let stats = handle.join().unwrap().unwrap();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.responses, 1);
        assert_eq!(stats.denied, 0);
    }

    #[cfg(unix)]
    #[test]
    fn serve_connection_rejects_foreign_uid_without_reading() {
        use std::os::unix::net::UnixStream;

        let (client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let peer = PeerCredentials::new(2000, 2000, 99);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;

        let result = serve_connection(
            &mut server,
            peer,
            1000,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        );
        assert!(matches!(result, Err(IpcError::Unauthenticated { .. })));
        drop(client);
    }

    #[cfg(unix)]
    #[test]
    fn serve_connection_rate_limits_with_error_response() {
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let peer = PeerCredentials::new(1000, 1000, 1);
        let mut limiter = RateLimiter::new(100, 1);
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
                1000,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
        });

        for id in 1..=2u64 {
            let payload =
                format!("{{\"id\":{id},\"method\":\"bitty.debug/ping\",\"version\":\"1.0\"}}");
            let wire = encode_frame(payload.as_bytes()).unwrap();
            client.write_all(&wire).unwrap();
        }
        let mut texts = Vec::new();
        for _ in 0..2 {
            let mut header = [0u8; 4];
            client.read_exact(&mut header).unwrap();
            let len = u32::from_be_bytes(header) as usize;
            let mut body = vec![0u8; len];
            client.read_exact(&mut body).unwrap();
            texts.push(String::from_utf8(body).unwrap());
        }
        assert!(texts[0].contains("\"ok\":true"));
        assert!(texts[1].contains("RateLimited"));
        drop(client);
        let stats = handle.join().unwrap().unwrap();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.denied, 1);
    }

    #[cfg(unix)]
    #[test]
    fn serve_connection_oversize_frame_closes() {
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let peer = PeerCredentials::new(1000, 1000, 1);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
                1000,
                &dispatcher,
                &context,
                &mut limiter,
                &clock,
            )
        });

        let huge = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        client.write_all(&huge).unwrap();
        let mut header = [0u8; 4];
        client.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("FrameTooLarge"));
        let stats = handle.join().unwrap().unwrap();
        assert_eq!(stats.framing_errors, 1);
    }

    // ── directory attestation (unix, temp dirs) ─────────────────────────

    #[cfg(unix)]
    #[test]
    fn prepare_socket_dir_enforces_0700() {
        let base = std::env::temp_dir().join(format!(
            "bitty-ctx0144-{}-{}",
            std::process::id(),
            "prepare"
        ));
        let socket_path = base.join("bitty/t.sock");
        let socket_str = socket_path.to_str().unwrap();
        let attestation = prepare_socket_dir(socket_str).unwrap();
        assert_eq!(attestation.dir_mode, DIR_MODE);
        // Second call on the existing good leaf succeeds.
        let again = prepare_socket_dir(socket_str).unwrap();
        assert_eq!(again, attestation);
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn prepare_socket_dir_rejects_bad_mode() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "bitty-ctx0144-{}-{}",
            std::process::id(),
            "badmode"
        ));
        let leaf = base.join("bitty");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket_str = leaf.join("t.sock").to_str().unwrap().to_string();
        let err = prepare_socket_dir(&socket_str).unwrap_err();
        assert!(matches!(err, IpcError::Unauthenticated { .. }));
        std::fs::remove_dir_all(&base).ok();
    }
}
