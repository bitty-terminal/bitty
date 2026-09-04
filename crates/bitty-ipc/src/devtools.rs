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
//! # Scope of this slice (CTX-0144 plus CTX-0159)
//!
//! Handshake plus a read-only round-trip: `bitty.debug/ping`
//! (version/handshake probe) and `bitty.debug/getSnapshot` (runtime-stats
//! snapshot: instance, pid, versions, grid geometry at startup, uptime).
//! CTX-0159 adds live read-only introspection so input probes need no
//! screenshots: `bitty.debug/getGridText` (bounded grid text plus cursor),
//! `bitty.debug/getInputRing` (bounded ring of keys, modifiers, and mouse
//! buttons with coordinates), `bitty.debug/getModifiers` (modifier/latch
//! state), and `bitty.debug/getFocus` (focus/window state). The live store is
//! published by `bitty-runtime/src/inspect.rs` (`&self` only, never mutating
//! terminal truth); every query is read-only. The
//! [`Dispatcher`] remains an extensible method table: new `bitty.debug/*`
//! handlers register via [`Dispatcher::register`] without reworking framing,
//! parsing, or the connection loop. Input injection is explicitly out of
//! scope (separate next slice after introspection lands, DEC-0018).
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

use crate::auth::VerifiedPeer;
#[cfg(unix)]
use crate::auth::{DIR_MODE, SOCKET_MODE};
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

/// Portable `AF_UNIX` socket-path ceiling in payload bytes (excl. NUL).
///
/// `sockaddr_un.sun_path` is 108 bytes incl. NUL on Linux and 104 bytes incl.
/// NUL on macOS/BSD (`SUN_LEN`; macOS limit documented in `sys/un.h`, Linux
/// in `unix(7)`). A 100-byte payload (101 incl. NUL) fits every target with
/// margin, including smaller historical limits (92). This intentionally
/// diverges from `auth.ts`'s 512-byte advisory check: 512 is wrong for
/// `bind`/`connect`, which fail with `EINVAL`/`InvalidInput` ("path must be
/// shorter than SUN_LEN") past the kernel bound. All socket paths produced or
/// accepted here must fit this portable bound.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// `sun_path` size incl. NUL on Linux (108) per `unix(7)`.
pub const SUN_LEN_LINUX: usize = 108;

/// `sun_path` size incl. NUL on macOS/BSD (104) per `sys/un.h`.
pub const SUN_LEN_MACOS: usize = 104;

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

// ── introspection bounds (CTX-0159) ─────────────────────────────────────────

/// Maximum grid rows per `getGridText` snapshot (deterministic top-first
/// truncation; mirrors `bitty-devtools/src/bounds.ts` preview caps).
pub const MAX_INSPECT_ROWS: usize = 64;

/// Maximum grid columns per row (char-boundary truncation).
pub const MAX_INSPECT_COLS: usize = 256;

/// Maximum grid text bytes retained in the live store (fail-closed bound for
/// the published snapshot, well under [`MAX_FRAME_BYTES`]).
pub const MAX_INSPECT_TEXT_BYTES: usize = 16 * 1024;

/// Maximum input-ring events retained and served (drop-oldest).
pub const MAX_INPUT_RING: usize = 64;

/// Maximum characters per input-event label echoed to the peer.
pub const MAX_INPUT_LABEL_CHARS: usize = 64;

/// Maximum bytes for the raw `params` object in a request envelope.
/// Method params are tiny (`rows`/`cols`/`limit`); anything larger fails
/// closed before dispatch.
pub const MAX_PARAMS_BYTES: usize = 4096;

/// Maximum rendered introspection JSON bytes per response (fail-closed; grid
/// text dominates and is truncated row-first to fit).
pub const MAX_INSPECT_JSON_BYTES: usize = 32 * 1024;

// ── socket path ─────────────────────────────────────────────────────────────

/// Resolve the Unix socket path with `auth.ts` precedence but a portable
/// `AF_UNIX` bound.
///
/// Precedence: non-empty `bitty_socket` (`BITTY_SOCKET`, advisory) wins
/// verbatim; otherwise `<base>/bitty/<instance>.sock` where `base` is
/// `xdg_runtime_dir` (`XDG_RUNTIME_DIR`) or `/run/user/<uid>`, and `instance`
/// is `instance_id` (`BITTY_INSTANCE_ID`) or `"default"`.
///
/// Validation: socket paths over [`MAX_SOCKET_PATH_BYTES`] payload bytes or
/// containing NUL are rejected fail-closed; instance ids must be 1..=64 ASCII
/// alphanumeric/`-`/`_` (`auth.ts` regex `^[a-z0-9_-]+$`, case-insensitive).
/// Lengths are measured in bytes here (Rust) rather than UTF-16 code units
/// (TypeScript); for the ASCII paths this contract admits, the two agree.
///
/// When the constructed `<base>/bitty/<instance>.sock` exceeds the portable
/// bound, the instance id is clamped to a deterministic 16-hex FNV-1a hash
/// (`<base>/bitty/<hash>.sock`) to keep short names stable; when even the
/// hashed form is too long the base directory itself is too long and
/// resolution fails closed with a clear `AF_UNIX`/`SUN_LEN` error. `auth.ts`
/// parity is precedence and instance grammar only: its 512-byte length check
/// is not portable to `bind` (Linux 108 / macOS 104 incl. NUL) and is not
/// adopted here.
///
/// # Errors
///
/// Returns [`IpcError::InvalidRequest`] for overlong/NUL paths, invalid
/// instance ids, and overlong base directories.
pub fn resolve_socket_path(
    runtime_uid: u32,
    xdg_runtime_dir: Option<&str>,
    bitty_socket: Option<&str>,
    instance_id: Option<&str>,
) -> Result<String, IpcError> {
    if let Some(sock) = bitty_socket {
        if !sock.is_empty() {
            if sock.contains('\0') {
                return Err(IpcError::InvalidRequest {
                    reason: "BITTY_SOCKET contains NUL".into(),
                });
            }
            if sock.len() > MAX_SOCKET_PATH_BYTES {
                return Err(IpcError::InvalidRequest {
                    reason: format!(
                        "BITTY_SOCKET path too long for AF_UNIX ({} > {MAX_SOCKET_PATH_BYTES} payload bytes; portable SUN_LEN: Linux {SUN_LEN_LINUX} / macOS {SUN_LEN_MACOS} incl. NUL)",
                        sock.len()
                    ),
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
    let direct = format!("{base}/{SOCKET_LEAF_DIR}/{instance}.sock");
    if direct.len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(direct);
    }
    let hashed = format!(
        "{base}/{SOCKET_LEAF_DIR}/{}.sock",
        short_instance_hash(instance)
    );
    if hashed.len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(hashed);
    }
    Err(IpcError::InvalidRequest {
        reason: format!(
            "socket base dir too long for AF_UNIX ({} > {MAX_SOCKET_PATH_BYTES} payload bytes even with hashed instance; portable SUN_LEN: Linux {SUN_LEN_LINUX} / macOS {SUN_LEN_MACOS} incl. NUL; shorten XDG_RUNTIME_DIR or set BITTY_SOCKET)",
            hashed.len()
        ),
    })
}

/// Deterministic 64-bit FNV-1a hash rendered as 16 lowercase hex chars.
///
/// `std`-only (no new dependencies): used solely to clamp long instance ids
/// into short, stable socket leaf names that fit the portable `AF_UNIX`
/// bound. Not a security hash; collision handling is fail-soft via live
/// socket reclaim in the servo.
fn short_instance_hash(instance: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in instance.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
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
/// the client sent (no float formatting drift). `params_raw` carries the raw
/// `params` object bytes when present (bounded to [`MAX_PARAMS_BYTES`]) so
/// CTX-0159 handlers can parse method-specific params (`rows`/`cols`/`limit`)
/// without a new dependency; v1 handlers (`ping`, `getSnapshot`) ignore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevtoolsRequest {
    /// Verbatim numeric id token (e.g. `"1"`).
    pub id_raw: String,
    /// Method such as `"bitty.debug/ping"`.
    pub method: String,
    /// Whether the envelope carried `jsonrpc: "2.0"` (`protocol.ts` shape).
    pub has_jsonrpc: bool,
    /// Raw `params` object bytes when the envelope carried one.
    pub params_raw: Option<String>,
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

    // Capture the raw `params` object when present so method handlers can
    // parse per-method scopes (`rows`/`cols`/`limit`) without a JSON
    // dependency. The slice is bounded before retention: oversize params fail
    // closed here rather than reaching dispatch.
    let params_raw = match keys.values.get("params") {
        None => None,
        Some((start, end)) => {
            let raw = text[*start..*end].trim().to_string();
            if raw.len() > MAX_PARAMS_BYTES {
                return Err(RequestFault::new(
                    keys.id_raw.clone(),
                    "transport",
                    "PayloadTooLarge",
                    format!("params {} exceeds limit {MAX_PARAMS_BYTES}", raw.len()),
                ));
            }
            // `params` must be an object or null; arrays and scalars are
            // rejected fail-closed (per-method handlers expect an object).
            if !(raw.starts_with('{') || raw == "null") {
                return Err(RequestFault::new(
                    keys.id_raw.clone(),
                    "usage",
                    "InvalidParams",
                    "envelope params must be an object".to_string(),
                ));
            }
            if raw == "null" { None } else { Some(raw) }
        }
    };

    Ok(DevtoolsRequest {
        id_raw,
        method,
        has_jsonrpc,
        params_raw,
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
    /// Table with the round-trip surface plus CTX-0159 read-only
    /// introspection (`getGridText`, `getInputRing`, `getModifiers`,
    /// `getFocus`).
    ///
    /// Introspection handlers register via [`Dispatcher::register`] (the
    /// CTX-0159 hook) so the registration path itself is exercised here, not
    /// just in tests. Method names are statically valid, so a registration
    /// failure here is a programming error surfaced loudly rather than a
    /// silent partial table.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut table = Self {
            handlers: BTreeMap::new(),
        };
        table.handlers.insert("bitty.debug/ping", handle_ping);
        table
            .handlers
            .insert("bitty.debug/getSnapshot", handle_get_snapshot);
        // CTX-0159 read-only introspection (fail-closed, bounded, no
        // injection). Names are statically valid per `validate_method`.
        let introspection: &[(&'static str, DevtoolsHandler)] = &[
            ("bitty.debug/getGridText", handle_get_grid_text),
            ("bitty.debug/getInputRing", handle_get_input_ring),
            ("bitty.debug/getModifiers", handle_get_modifiers),
            ("bitty.debug/getFocus", handle_get_focus),
        ];
        for (method, handler) in introspection {
            if table.register(method, *handler).is_err() {
                debug_assert!(false, "statically valid introspection method rejected");
            }
        }
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
/// Live terminal content is served by `bitty.debug/getGridText` (CTX-0159);
/// the `"snapshot":"runtime-stats"` marker keeps this response honest about
/// what it carries.
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

// ── introspection live store (CTX-0159) ─────────────────────────────────────
//
// The live store is published by `bitty-runtime/src/inspect.rs` (`&self`
// only) and served read-only here. All stored values are bounded at publish
// time; all served slices are bounded per-request params. No socket query
// mutates the store, the runtime, or terminal truth. Scope: every method in
// this section requires only `debug.inspect` (read-only default per
// `bitty-devtools/src/inspection.ts`); no `debug.control` surface is exposed.

/// Grid text published by the runtime (bounded at publish time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridPublish {
    /// Grid text rows (each already char-bounded and trailing-trimmed).
    pub lines: Vec<String>,
    /// Live cursor row (`0`-based).
    pub cursor_row: u16,
    /// Live cursor column (`0`-based).
    pub cursor_col: u16,
    /// Whether the cursor is visible.
    pub cursor_visible: bool,
    /// Damage generation at capture time.
    pub generation: u64,
    /// Grid width in columns at capture time.
    pub cols: usize,
    /// Grid height in rows at capture time.
    pub rows: usize,
}

/// One input event published by the runtime (bounded at publish time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEventPublish {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Kind label (`"key"`, `"modifiers"`, `"mouse"`, `"wheel"`, `"focus"`).
    pub kind: String,
    /// Bounded human-readable summary.
    pub label: String,
    /// Whether Shift was held.
    pub shift: bool,
    /// Whether Control was held.
    pub control: bool,
    /// Whether Alt was held.
    pub alt: bool,
    /// Mouse button name when applicable.
    pub button: Option<String>,
    /// Cell column (`0`-based) when applicable.
    pub col: Option<u16>,
    /// Cell row (`0`-based) when applicable.
    pub row: Option<u16>,
    /// Pressed (`true`) or released (`false`) when applicable.
    pub pressed: Option<bool>,
}

/// Modifier/latch state published by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifiersPublish {
    /// Whether Shift is latched.
    pub shift: bool,
    /// Whether Control is latched.
    pub control: bool,
    /// Whether Alt is latched.
    pub alt: bool,
    /// Live Kitty keyboard flags (`0` means legacy).
    pub kitty_flags: u32,
}

/// Focus/window state published by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusPublish {
    /// Whether the window holds keyboard focus.
    pub focused: bool,
    /// Focused view id when the layout has one.
    pub focused_view: Option<u64>,
    /// Whether mouse-event capture is active.
    pub mouse_capture: bool,
    /// Whether the alternate screen is active.
    pub alt_screen: bool,
    /// Whether bracketed paste (`2004`) is active.
    pub bracketed_paste: bool,
    /// Whether focus-event reporting (`1004`) is active.
    pub focus_events: bool,
}

/// Stored grid snapshot (private; published values are validated on entry).
#[derive(Debug, Clone, Default)]
struct StoredGrid {
    /// Bounded grid lines.
    lines: Vec<String>,
    /// Cursor row.
    cursor_row: u16,
    /// Cursor column.
    cursor_col: u16,
    /// Cursor visibility.
    cursor_visible: bool,
    /// Generation.
    generation: u64,
    /// Grid width.
    cols: usize,
    /// Grid height.
    rows: usize,
}

/// Stored modifier snapshot.
#[derive(Debug, Clone, Copy, Default)]
struct StoredModifiers {
    /// Shift latch.
    shift: bool,
    /// Control latch.
    control: bool,
    /// Alt latch.
    alt: bool,
    /// Kitty flags.
    kitty_flags: u32,
}

/// Stored focus snapshot.
#[derive(Debug, Clone, Copy, Default)]
struct StoredFocus {
    /// Window focus.
    focused: bool,
    /// Focused view.
    focused_view: Option<u64>,
    /// Mouse capture.
    mouse_capture: bool,
    /// Alt screen.
    alt_screen: bool,
    /// Bracketed paste.
    bracketed_paste: bool,
    /// Focus events.
    focus_events: bool,
}

use std::sync::{Mutex, OnceLock};

/// Live grid store (empty until the runtime publishes).
fn live_grid_store() -> &'static Mutex<StoredGrid> {
    static STORE: OnceLock<Mutex<StoredGrid>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoredGrid::default()))
}

/// Live input-ring store (empty until the runtime publishes).
fn live_input_store() -> &'static Mutex<Vec<InputEventPublish>> {
    static STORE: OnceLock<Mutex<Vec<InputEventPublish>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Live modifier store (defaults to all-released).
fn live_modifiers_store() -> &'static Mutex<StoredModifiers> {
    static STORE: OnceLock<Mutex<StoredModifiers>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoredModifiers::default()))
}

/// Live focus store (defaults to unfocused; the runtime publishes `true` on
/// startup via its `focused: true` initial state on the next tick).
fn live_focus_store() -> &'static Mutex<StoredFocus> {
    static STORE: OnceLock<Mutex<StoredFocus>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoredFocus::default()))
}

/// Truncate a line to at most `max` characters (char-boundary safe).
fn truncate_line(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Publish grid text to the live store (called by `bitty-runtime`, `&self`
/// only).
///
/// Bounds are enforced deterministically: at most [`MAX_INSPECT_ROWS`] rows,
/// each at most [`MAX_INSPECT_COLS`] characters, total at most
/// [`MAX_INSPECT_TEXT_BYTES`] bytes (row-first truncation). A poisoned mutex
/// fails closed by dropping the publish (the next tick republishes).
pub fn publish_grid_text(
    lines: Vec<String>,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    generation: u64,
    cols: usize,
    rows: usize,
) {
    let mut bounded: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.into_iter().take(MAX_INSPECT_ROWS) {
        let cut = truncate_line(&line, MAX_INSPECT_COLS);
        let len = cut.len();
        if bytes + len > MAX_INSPECT_TEXT_BYTES {
            break;
        }
        bytes += len;
        bounded.push(cut);
    }
    let stored = StoredGrid {
        lines: bounded,
        cursor_row,
        cursor_col,
        cursor_visible,
        generation,
        cols,
        rows,
    };
    if let Ok(mut guard) = live_grid_store().lock() {
        *guard = stored;
    }
}

/// Publish the input ring to the live store (called by `bitty-runtime`).
///
/// At most [`MAX_INPUT_RING`] events are retained; each `kind`/`label`/
/// `button` is truncated to its bound. Oversize input beyond the ring is
/// dropped oldest-first (never an error, never unbounded).
pub fn publish_input_ring(events: Vec<InputEventPublish>) {
    let mut bounded: Vec<InputEventPublish> = Vec::with_capacity(events.len().min(MAX_INPUT_RING));
    for mut e in events.into_iter().take(MAX_INPUT_RING) {
        e.kind = truncate_chars(&e.kind, 16);
        e.label = truncate_chars(&e.label, MAX_INPUT_LABEL_CHARS);
        if let Some(button) = e.button {
            e.button = Some(truncate_chars(&button, 16));
        }
        bounded.push(e);
    }
    if let Ok(mut guard) = live_input_store().lock() {
        *guard = bounded;
    }
}

/// Publish modifier/latch state to the live store (called by `bitty-runtime`).
pub fn publish_modifiers(snapshot: ModifiersPublish) {
    if let Ok(mut guard) = live_modifiers_store().lock() {
        *guard = StoredModifiers {
            shift: snapshot.shift,
            control: snapshot.control,
            alt: snapshot.alt,
            kitty_flags: snapshot.kitty_flags,
        };
    }
}

/// Publish focus/window state to the live store (called by `bitty-runtime`).
pub fn publish_focus(snapshot: FocusPublish) {
    if let Ok(mut guard) = live_focus_store().lock() {
        *guard = StoredFocus {
            focused: snapshot.focused,
            focused_view: snapshot.focused_view,
            mouse_capture: snapshot.mouse_capture,
            alt_screen: snapshot.alt_screen,
            bracketed_paste: snapshot.bracketed_paste,
            focus_events: snapshot.focus_events,
        };
    }
}

/// Clear the live introspection store (test helper only).
///
/// Tests publish known snapshots and must not leak them into parallel tests:
/// clear before and after each global round-trip. Production never calls this.
pub fn clear_introspection_for_tests() {
    if let Ok(mut guard) = live_grid_store().lock() {
        *guard = StoredGrid::default();
    }
    if let Ok(mut guard) = live_input_store().lock() {
        guard.clear();
    }
    if let Ok(mut guard) = live_modifiers_store().lock() {
        *guard = StoredModifiers::default();
    }
    if let Ok(mut guard) = live_focus_store().lock() {
        *guard = StoredFocus::default();
    }
}

/// Parse an optional unsigned param from raw `params` JSON.
///
/// Returns `default` when `params` is absent or the key is absent (absent
/// means default scope). Fails closed with `InvalidParams` when the key is
/// present but not a plain non-negative integer, or when the value exceeds
/// `max`. Unknown keys are ignored (forward compatible). The scan is a
/// bounded substring search over at most [`MAX_PARAMS_BYTES`] bytes: no
/// allocation beyond the returned value, no recursion, no backtracking.
fn parse_optional_uint_param(
    params_raw: Option<&str>,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, HandlerError> {
    let Some(params) = params_raw else {
        return Ok(default);
    };
    let needle = format!("\"{key}\"");
    let Some(key_pos) = params.find(needle.as_str()) else {
        return Ok(default);
    };
    let after_key = &params[key_pos + needle.len()..];
    let Some(colon) = after_key.find(':') else {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params {key} must be a number"),
        ));
    };
    let mut value_part = after_key[colon + 1..].trim_start();
    // Reject quoted strings, objects, arrays, and signs up front.
    if value_part.starts_with('"')
        || value_part.starts_with('{')
        || value_part.starts_with('[')
        || value_part.starts_with('-')
        || value_part.starts_with('+')
    {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params {key} must be a number"),
        ));
    }
    let mut len = 0usize;
    for b in value_part.bytes() {
        if b.is_ascii_digit() {
            len += 1;
        } else {
            break;
        }
    }
    if len == 0 || len > 6 {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params {key} must be a number"),
        ));
    }
    value_part = &value_part[..len];
    let value: usize = value_part.parse().map_err(|_| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params {key} must be a number"),
        )
    })?;
    if value == 0 || value > max {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params {key} must be 1..={max}"),
        ));
    }
    Ok(value)
}

/// `bitty.debug/getGridText`: bounded grid text plus cursor.
///
/// Params scope (all optional, fail-closed on oversize/unknown types):
/// `{ "rows": 1..=64, "cols": 1..=256 }` (defaults: full bounded store).
/// Returns `{"snapshot":"grid-text","lines":[...],"cursor":{...},"cols",
/// `"rows","generation"}`. Empty store (never published) yields empty lines
/// with generation `0` rather than an error.
fn handle_get_grid_text(
    _context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let rows = parse_optional_uint_param(
        request.params_raw.as_deref(),
        "rows",
        MAX_INSPECT_ROWS,
        MAX_INSPECT_ROWS,
    )?;
    let cols = parse_optional_uint_param(
        request.params_raw.as_deref(),
        "cols",
        MAX_INSPECT_COLS,
        MAX_INSPECT_COLS,
    )?;
    let guard = live_grid_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "introspection store unavailable".to_string(),
        )
    })?;
    let take = rows.min(guard.lines.len());
    let mut out = String::with_capacity(1024.min(MAX_INSPECT_JSON_BYTES));
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"grid-text\",\"lines\":[");
    for (i, line) in guard.lines.iter().take(take).enumerate() {
        if i > 0 {
            out.push(',');
        }
        let cut = truncate_line(line, cols);
        out.push('"');
        json_escape_into(&mut out, &cut);
        out.push('"');
        if out.len() > MAX_INSPECT_JSON_BYTES {
            return Err(HandlerError::new(
                "transport",
                "PayloadTooLarge",
                "grid snapshot exceeds response bound".to_string(),
            ));
        }
    }
    out.push_str("],\"cursor\":{\"row\":");
    out.push_str(&guard.cursor_row.to_string());
    out.push_str(",\"col\":");
    out.push_str(&guard.cursor_col.to_string());
    out.push_str(",\"visible\":");
    out.push_str(if guard.cursor_visible {
        "true"
    } else {
        "false"
    });
    out.push_str("},\"cols\":");
    out.push_str(&guard.cols.to_string());
    out.push_str(",\"rows\":");
    out.push_str(&guard.rows.to_string());
    out.push_str(",\"generation\":");
    out.push_str(&guard.generation.to_string());
    out.push('}');
    if out.len() > MAX_INSPECT_JSON_BYTES {
        return Err(HandlerError::new(
            "transport",
            "PayloadTooLarge",
            "grid snapshot exceeds response bound".to_string(),
        ));
    }
    Ok(out)
}

/// `bitty.debug/getInputRing`: bounded last-input events.
///
/// Params scope: `{ "limit": 1..=64 }` (default: full ring). Returns
/// `{"snapshot":"input-ring","events":[{seq,kind,label,shift,control,alt,
/// button,col,row,pressed}],"dropped_notice":false}`. Empty store yields an
/// empty array rather than an error.
fn handle_get_input_ring(
    _context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let limit = parse_optional_uint_param(
        request.params_raw.as_deref(),
        "limit",
        MAX_INPUT_RING,
        MAX_INPUT_RING,
    )?;
    let guard = live_input_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "introspection store unavailable".to_string(),
        )
    })?;
    let total = guard.len();
    let take = limit.min(total);
    let start = total - take;
    let mut out = String::with_capacity(512.min(MAX_INSPECT_JSON_BYTES));
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"input-ring\",\"events\":[");
    for (i, e) in guard.iter().skip(start).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"seq\":");
        out.push_str(&e.seq.to_string());
        out.push_str(",\"kind\":\"");
        json_escape_into(&mut out, &truncate_chars(&e.kind, 16));
        out.push_str("\",\"label\":\"");
        json_escape_into(&mut out, &truncate_chars(&e.label, MAX_INPUT_LABEL_CHARS));
        out.push_str("\",\"shift\":");
        out.push_str(if e.shift { "true" } else { "false" });
        out.push_str(",\"control\":");
        out.push_str(if e.control { "true" } else { "false" });
        out.push_str(",\"alt\":");
        out.push_str(if e.alt { "true" } else { "false" });
        out.push_str(",\"button\":");
        match &e.button {
            Some(b) => {
                out.push('"');
                json_escape_into(&mut out, &truncate_chars(b, 16));
                out.push('"');
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"col\":");
        match e.col {
            Some(c) => out.push_str(&c.to_string()),
            None => out.push_str("null"),
        }
        out.push_str(",\"row\":");
        match e.row {
            Some(r) => out.push_str(&r.to_string()),
            None => out.push_str("null"),
        }
        out.push_str(",\"pressed\":");
        match e.pressed {
            Some(true) => out.push_str("true"),
            Some(false) => out.push_str("false"),
            None => out.push_str("null"),
        }
        out.push('}');
        if out.len() > MAX_INSPECT_JSON_BYTES {
            return Err(HandlerError::new(
                "transport",
                "PayloadTooLarge",
                "input ring exceeds response bound".to_string(),
            ));
        }
    }
    out.push_str("],\"count\":");
    out.push_str(&take.to_string());
    out.push('}');
    Ok(out)
}

/// `bitty.debug/getModifiers`: modifier/latch state (no params).
fn handle_get_modifiers(
    _context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let guard = live_modifiers_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "introspection store unavailable".to_string(),
        )
    })?;
    let mut out = String::with_capacity(128);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"modifiers\",\"shift\":");
    out.push_str(if guard.shift { "true" } else { "false" });
    out.push_str(",\"control\":");
    out.push_str(if guard.control { "true" } else { "false" });
    out.push_str(",\"alt\":");
    out.push_str(if guard.alt { "true" } else { "false" });
    out.push_str(",\"kitty_flags\":");
    out.push_str(&guard.kitty_flags.to_string());
    out.push('}');
    Ok(out)
}

/// `bitty.debug/getFocus`: focus/window state (no params).
fn handle_get_focus(
    _context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let guard = live_focus_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "introspection store unavailable".to_string(),
        )
    })?;
    let mut out = String::with_capacity(192);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"focus\",\"focused\":");
    out.push_str(if guard.focused { "true" } else { "false" });
    out.push_str(",\"focused_view\":");
    match guard.focused_view {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
    out.push_str(",\"mouse_capture\":");
    out.push_str(if guard.mouse_capture { "true" } else { "false" });
    out.push_str(",\"alt_screen\":");
    out.push_str(if guard.alt_screen { "true" } else { "false" });
    out.push_str(",\"bracketed_paste\":");
    out.push_str(if guard.bracketed_paste {
        "true"
    } else {
        "false"
    });
    out.push_str(",\"focus_events\":");
    out.push_str(if guard.focus_events { "true" } else { "false" });
    out.push('}');
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
/// Reads length-prefixed frames (`u32` BE + payload `<= 256 KiB`), rate-limits
/// per request (`RC-9` via `limiter` and caller-supplied `clock_ms`), and
/// dispatches via [`handle_envelope`]. Oversize frames get one correlated
/// error response and then the connection closes (fail-closed, no stream
/// desync). Rate-limited requests get an error response and the connection
/// stays open.
///
/// Authentication happens at the accept boundary, not here: the caller must
/// verify peer UID before the first byte via
/// [`verify_peer_for_connection`](crate::auth::verify_peer_for_connection)
/// or [`transport_attested_peer`], and pass only the resulting sanitized
/// [`VerifiedPeer`] marker. This function takes no `PeerCredentials`-typed
/// value, so credential dataflow ends at the accept boundary and never
/// reaches serving counters or logging (CodeQL `cleartext logging of
/// sensitive information` clean by construction).
///
/// The stream is generic (`Read + Write`) so headless tests drive this exact
/// function over `UnixStream::pair`; the servo passes live streams with
/// read/write timeouts already set. Idle timeouts surface as a clean close
/// (`Ok`), never an error.
///
/// # Errors
///
/// Returns `Transport` when the stream fails mid-protocol.
pub fn serve_connection<S>(
    stream: &mut S,
    _peer: VerifiedPeer,
    dispatcher: &Dispatcher,
    context: &ServeContext,
    limiter: &mut RateLimiter,
    clock_ms: &dyn Fn() -> u64,
) -> Result<ConnectionStats, IpcError>
where
    S: Read + Write,
{
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
/// Returns a sanitized [`VerifiedPeer`] marker carrying no credential bytes:
/// the accept boundary in `bitty-app/src/ipc_serve.rs` calls this before
/// [`serve_connection`], so no `PeerCredentials`-typed value flows into the
/// serving/logging path.
///
/// `SO_PEERCRED` per-connection re-verification (defense in depth against
/// file-descriptor passing) needs either nightly
/// `peer_credentials_unix_socket` (still unstable, rust-lang/rust#42839) or
/// a reviewed `unsafe` `getsockopt` seam, both out of scope for this
/// fail-soft slice; it is recorded hardening for CTX-0159. The headless
/// [`crate::auth::verify_peer_uid`] primitive and its tests already encode
/// the check the live seam will call.
#[must_use]
pub fn transport_attested_peer(runtime_uid: u32) -> VerifiedPeer {
    VerifiedPeer::attested(runtime_uid)
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
    fn socket_path_portable_bound_is_pinned() {
        // Portable AF_UNIX ceiling: 100 payload bytes fits Linux 108 and
        // macOS/BSD 104 incl. NUL with margin (historical floor 92).
        const { assert!(MAX_SOCKET_PATH_BYTES <= 100) };
        const { assert!(SUN_LEN_LINUX == 108) };
        const { assert!(SUN_LEN_MACOS == 104) };
        const { assert!(MAX_SOCKET_PATH_BYTES < SUN_LEN_MACOS) };
        // Every resolved path fits the portable bound incl. NUL.
        let path = resolve_socket_path(1000, Some("/run/user/1000"), None, None).unwrap();
        assert!(path.len() <= MAX_SOCKET_PATH_BYTES);
        assert!(path.len() < SUN_LEN_MACOS);
    }

    #[test]
    fn socket_path_hashes_long_instance_to_fit() {
        // A 64-char instance with a medium base overflows direct form but
        // fits via deterministic hash clamping.
        let base = format!("/tmp/{}", "b".repeat(50));
        let long_instance = "c".repeat(MAX_INSTANCE_ID_LEN);
        let direct_len =
            base.len() + 1 + SOCKET_LEAF_DIR.len() + 1 + long_instance.len() + ".sock".len();
        assert!(direct_len > MAX_SOCKET_PATH_BYTES);
        let path = resolve_socket_path(1000, Some(&base), None, Some(&long_instance)).unwrap();
        assert!(path.len() <= MAX_SOCKET_PATH_BYTES);
        assert!(!path.contains(&long_instance));
        assert!(path.ends_with(".sock"));
        // Deterministic: same instance hashes identically.
        let again = resolve_socket_path(1000, Some(&base), None, Some(&long_instance)).unwrap();
        assert_eq!(path, again);
    }

    #[test]
    fn socket_path_rejects_long_base_fail_closed() {
        // Even the hashed leaf cannot save a base dir that is itself too long.
        let base = format!("/tmp/{}", "d".repeat(120));
        let err = resolve_socket_path(1000, Some(&base), None, None).unwrap_err();
        let reason = format!("{err}");
        assert!(reason.contains("AF_UNIX") || reason.contains("too long"));
        let long_socket = format!("/tmp/{}.sock", "e".repeat(120));
        assert!(resolve_socket_path(1000, None, Some(&long_socket), None).is_err());
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
        // CTX-0144 (ping, getSnapshot) plus CTX-0159 introspection
        // (getGridText, getInputRing, getModifiers, getFocus).
        assert_eq!(dispatcher.method_count(), 6);
        assert!(dispatcher.contains("bitty.debug/getGridText"));
        assert!(dispatcher.contains("bitty.debug/getInputRing"));
        assert!(dispatcher.contains("bitty.debug/getModifiers"));
        assert!(dispatcher.contains("bitty.debug/getFocus"));
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
        let peer = transport_attested_peer(1000);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
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
    fn serve_path_takes_verified_marker_only() {
        use crate::auth::{PeerCredentials, verify_peer_for_connection};
        // Regression for CodeQL HIGH `cleartext logging of sensitive
        // information`: `serve_connection` takes only the pre-verified
        // `VerifiedPeer` marker, so no `PeerCredentials`-typed value flows
        // into the serving path. Accept-boundary verification is fail-closed.
        let good = PeerCredentials::new(1000, 1000, 1);
        let verified = verify_peer_for_connection(good, 1000).unwrap();
        let attested = transport_attested_peer(1000);
        assert_eq!(verified, attested);

        // Foreign UID cannot produce a marker: rejected before any byte read.
        let foreign = PeerCredentials::new(2000, 2000, 99);
        let err = verify_peer_for_connection(foreign, 1000).unwrap_err();
        assert!(matches!(err, IpcError::Unauthenticated { .. }));

        // Verified marker serves correctly over a socketpair.
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;
        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                verified,
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
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        assert!(String::from_utf8(body).unwrap().contains("\"ok\":true"));
        drop(client);
        let stats = handle.join().unwrap().unwrap();
        assert_eq!(stats.requests, 1);
    }

    #[cfg(unix)]
    #[test]
    fn serve_connection_rate_limits_with_error_response() {
        use std::os::unix::net::UnixStream;

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let peer = transport_attested_peer(1000);
        let mut limiter = RateLimiter::new(100, 1);
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
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
        let peer = transport_attested_peer(1000);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || 0u64;

        let handle = std::thread::spawn(move || {
            serve_connection(
                &mut server,
                peer,
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

    // ── introspection (CTX-0159, read-only, bounded) ───────────────────────
    //
    // The global live store is shared across tests in this binary: each test
    // below publishes its own known snapshot, asserts, then clears, and the
    // single round-trip test covers all four methods sequentially so parallel
    // execution cannot interleave publishes.

    #[test]
    fn introspection_params_are_per_method_bounded() {
        // Absent params mean defaults.
        assert_eq!(
            parse_optional_uint_param(None, "rows", MAX_INSPECT_ROWS, MAX_INSPECT_ROWS).unwrap(),
            MAX_INSPECT_ROWS
        );
        // Present and valid.
        assert_eq!(
            parse_optional_uint_param(
                Some(r#"{"rows":10,"cols":40}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .unwrap(),
            10
        );
        assert_eq!(
            parse_optional_uint_param(
                Some(r#"{"limit":5}"#),
                "limit",
                MAX_INPUT_RING,
                MAX_INPUT_RING
            )
            .unwrap(),
            5
        );
        // Unknown keys are ignored (forward compatible).
        assert_eq!(
            parse_optional_uint_param(
                Some(r#"{"other":99}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .unwrap(),
            MAX_INSPECT_ROWS
        );
        // Oversize, zero, non-numeric, and signed values fail closed.
        assert!(
            parse_optional_uint_param(
                Some(r#"{"rows":999}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .is_err()
        );
        assert!(
            parse_optional_uint_param(
                Some(r#"{"rows":0}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .is_err()
        );
        assert!(
            parse_optional_uint_param(
                Some(r#"{"rows":"10"}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .is_err()
        );
        assert!(
            parse_optional_uint_param(
                Some(r#"{"rows":-3}"#),
                "rows",
                MAX_INSPECT_ROWS,
                MAX_INSPECT_ROWS
            )
            .is_err()
        );
    }

    #[test]
    fn introspection_envelope_params_shape() {
        // Object params are captured verbatim for handlers.
        let request = parse_request(
            br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":10}}"#,
        )
        .unwrap();
        assert_eq!(request.params_raw.as_deref(), Some(r#"{"rows":10}"#));
        // Absent params yield None (defaults apply).
        let request =
            parse_request(br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#)
                .unwrap();
        assert_eq!(request.params_raw, None);
        // Array params fail closed.
        let fault = parse_request(
            br#"{"id":2,"method":"bitty.debug/getGridText","version":"1.0","params":[1]}"#,
        )
        .unwrap_err();
        assert_eq!(fault.code, "InvalidParams");
        // Oversize params fail closed before dispatch.
        let big = format!(
            "{{\"id\":3,\"method\":\"bitty.debug/getGridText\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{}\"}}}}",
            "p".repeat(MAX_PARAMS_BYTES)
        );
        let fault = parse_request(big.as_bytes()).unwrap_err();
        assert_eq!(fault.code, "PayloadTooLarge");
    }

    #[test]
    fn introspection_round_trip_all_methods_sequential() {
        clear_introspection_for_tests();
        publish_grid_text(
            vec!["hello introspect".to_string(), "second row".to_string()],
            0,
            16,
            true,
            7,
            80,
            24,
        );
        publish_input_ring(vec![
            InputEventPublish {
                seq: 1,
                kind: "key".to_string(),
                label: "key:a".to_string(),
                shift: false,
                control: false,
                alt: false,
                button: None,
                col: None,
                row: None,
                pressed: Some(true),
            },
            InputEventPublish {
                seq: 2,
                kind: "mouse".to_string(),
                label: "mouse:Left pressed col=10 row=5".to_string(),
                shift: false,
                control: false,
                alt: false,
                button: Some("Left".to_string()),
                col: Some(10),
                row: Some(5),
                pressed: Some(true),
            },
        ]);
        publish_modifiers(ModifiersPublish {
            shift: true,
            control: false,
            alt: false,
            kitty_flags: 0,
        });
        publish_focus(FocusPublish {
            focused: true,
            focused_view: Some(1),
            mouse_capture: false,
            alt_screen: false,
            bracketed_paste: false,
            focus_events: false,
        });

        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();

        let outcome = handle_envelope(
            br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"snapshot\":\"grid-text\""));
        assert!(text.contains("hello introspect"));
        assert!(text.contains("\"row\":0"));

        // Bounded slice via params.
        let outcome = handle_envelope(
            br#"{"id":2,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":1,"cols":5}}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("hello"));

        let outcome = handle_envelope(
            br#"{"id":3,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"snapshot\":\"input-ring\""));
        assert!(text.contains("key:a"));
        assert!(text.contains("mouse:Left"));

        let outcome = handle_envelope(
            br#"{"id":4,"method":"bitty.debug/getInputRing","version":"1.0","params":{"limit":1}}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("mouse:Left"));
        assert!(!text.contains("key:a"));

        let outcome = handle_envelope(
            br#"{"id":5,"method":"bitty.debug/getModifiers","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"snapshot\":\"modifiers\""));
        assert!(text.contains("\"shift\":true"));

        let outcome = handle_envelope(
            br#"{"id":6,"method":"bitty.debug/getFocus","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"snapshot\":\"focus\""));
        assert!(text.contains("\"focused\":true"));

        // Oversize params fail closed with correlated errors.
        let outcome = handle_envelope(
            br#"{"id":7,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":999}}"#,
            &dispatcher,
            &context,
        );
        assert!(outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("InvalidParams"));
        assert!(text.contains("\"id\":7"));

        clear_introspection_for_tests();
    }

    #[test]
    fn introspection_empty_store_is_not_an_error() {
        clear_introspection_for_tests();
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"lines\":[]"));
        let outcome = handle_envelope(
            br#"{"id":2,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("\"events\":[]")
        );
        clear_introspection_for_tests();
    }

    #[test]
    fn introspection_publish_is_bounded() {
        clear_introspection_for_tests();
        // Overlong grid input is truncated deterministically at publish time.
        let long_line = "x".repeat(MAX_INSPECT_COLS + 50);
        let many: Vec<String> = (0..(MAX_INSPECT_ROWS + 10))
            .map(|i| format!("{long_line}-{i}"))
            .collect();
        publish_grid_text(many, 0, 0, true, 1, 80, 24);
        let dispatcher = Dispatcher::with_defaults();
        let context = test_context();
        let outcome = handle_envelope(
            br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.len() <= MAX_INSPECT_JSON_BYTES + 512);
        // Overlong labels are truncated at publish time.
        publish_input_ring(vec![InputEventPublish {
            seq: 1,
            kind: "key".to_string(),
            label: "y".repeat(MAX_INPUT_LABEL_CHARS + 100),
            shift: false,
            control: false,
            alt: false,
            button: None,
            col: None,
            row: None,
            pressed: Some(true),
        }]);
        let outcome = handle_envelope(
            br#"{"id":2,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
            &dispatcher,
            &context,
        );
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.len() <= MAX_INSPECT_JSON_BYTES + 512);
        clear_introspection_for_tests();
    }
}
