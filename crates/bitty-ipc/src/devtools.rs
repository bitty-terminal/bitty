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
//! parsing, or the connection loop. CTX-0188 adds test automation
//! (`synthesizeInput` + `captureFrame`, bearer-scoped per Amendment A1):
//! key/mouse/wheel/paste synthesis and redacted frame capture for the
//! headless verify harness; profiling stays owned by CTX-0189.
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

/// Windows named-pipe namespace hosting instance endpoints (CTX-0196).
pub const WINDOWS_PIPE_NAMESPACE: &str = r"\\.\pipe\";

/// Windows pipe-name prefix scoping bitty instances (`\\.\pipe\bitty-<id>`).
///
/// Mirrors [`SOCKET_LEAF_DIR`]: the kernel pipe namespace is the Windows
/// registry the way the socket directory is the Unix registry. Only pipes
/// carrying this prefix are enumerated by `bitty list instances`; foreign
/// pipes are never touched.
pub const WINDOWS_PIPE_PREFIX: &str = "bitty-";

/// Map an instance id to its Windows named-pipe path (CTX-0196).
///
/// Pure string logic (no I/O, no `unsafe`): `\\.\pipe\bitty-<instance>`.
/// The caller must have validated `instance` against the shared grammar
/// (1..=[`MAX_INSTANCE_ID_LEN`], `^[a-z0-9_-]+$` case-insensitive); this
/// function maps verbatim so probes fail fast on malformed input rather
/// than inventing a second grammar.
#[must_use]
pub fn windows_pipe_name(instance: &str) -> String {
    format!("{WINDOWS_PIPE_NAMESPACE}{WINDOWS_PIPE_PREFIX}{instance}")
}

/// Parse an instance id out of a Windows pipe file name (CTX-0196).
///
/// Accepts the bare pipe name as listed from the pipe namespace (e.g.
/// `bitty-default`); the `.sock` suffix is never part of a pipe name.
/// Returns `None` for foreign pipes (wrong prefix) or ids violating the
/// shared grammar, so enumeration skips entries another application owns.
#[must_use]
pub fn windows_instance_from_pipe_name(pipe_name: &str) -> Option<String> {
    let id = pipe_name.strip_prefix(WINDOWS_PIPE_PREFIX)?;
    if id.is_empty() || id.len() > MAX_INSTANCE_ID_LEN {
        return None;
    }
    let ok = id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return None;
    }
    Some(id.to_string())
}

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

// ── test-automation bounds (CTX-0188, Amendment A1 candidate) ────────────────
//
// `synthesizeInput` (`debug.control` + bearer) and `captureFrame`
// (`debug.trace` + bearer) are the only keystroke-injection / frame-capture
// surface. Both require the debug scope, the terminal capability, and a
// per-session single-terminal bearer; unscoped callers get `ScopeDenied`
// with zero partial state (fail closed everywhere).

/// Wire method for `synthesizeInput` (key/mouse/wheel/paste synthesis).
pub const METHOD_SYNTHESIZE_INPUT: &str = "bitty.debug/synthesizeInput";

/// Wire method for `captureFrame` (bounded redacted frame capture).
pub const METHOD_CAPTURE_FRAME: &str = "bitty.debug/captureFrame";

/// Wire method for `frameHash` (CTX-0244: SHA-256 digest over canonical
/// `headless_rgba` + geometry header; 32-byte digest, zero pixel bytes).
pub const METHOD_FRAME_HASH: &str = "bitty.debug/frameHash";

/// Maximum synthetic events per `synthesizeInput` call (Amendment A1).
pub const MAX_SYNTH_EVENTS_PER_CALL: usize = 64;

/// Sustained `synthesizeInput` calls per second per bearer (Amendment A1).
pub const MAX_SYNTH_CALLS_PER_SEC: usize = 10;

/// Sustained `captureFrame` frames per second per bearer (Amendment A1).
pub const MAX_CAPTURE_FPS: usize = 10;

/// Wire method for `getProcessStats` (read-only process health snapshot).
pub const METHOD_GET_PROCESS_STATS: &str = "bitty.debug/getProcessStats";

/// Wire method for `getFrameStats` (read-only rendering health snapshot).
pub const METHOD_GET_FRAME_STATS: &str = "bitty.debug/getFrameStats";

/// Wire method for `streamProcessStats` (sampled process-stats drain).
pub const METHOD_STREAM_PROCESS_STATS: &str = "bitty.debug/streamProcessStats";

/// Wire method for `streamFrameStats` (sampled frame-stats drain).
pub const METHOD_STREAM_FRAME_STATS: &str = "bitty.debug/streamFrameStats";

/// Maximum retained samples per profiling family (CTX-0189; accepted
/// batching bound reused: at most 32 records per wakeup, drop-oldest with
/// counted drops so consumers converge to latest state).
pub const MAX_PROF_SAMPLES: usize = 32;

/// Maximum encoded sample bytes per stream drain (CTX-0189; accepted 8 KiB
/// aggregate reused: the drain stops before the next sample would exceed
/// this budget and reports `truncated: true`).
pub const MAX_PROF_DRAIN_BYTES: usize = 8 * 1024;

/// Sampling interval floor in ms (CTX-0189; Amendment A1 candidate: sampling
/// is the only profiling posture, cold-path counter snapshots, 100 ms
/// floor; per-frame tracing and sub-100 ms cadences are deferred).
pub const MIN_PROF_INTERVAL_MS: u64 = 100;

/// Default stream cadence in ms when `intervalMs` is absent.
pub const DEFAULT_PROF_INTERVAL_MS: u64 = 1_000;

/// Maximum declared stream cadence in ms (PB-7 ten-minute window parity).
pub const MAX_PROF_INTERVAL_MS: u64 = 600_000;

/// Maximum renderer-supplied label characters per frame sample (CTX-0189;
/// Amendment A1 candidate bound: at most 256 characters, never terminal
/// content, labeled untrusted observation data).
pub const MAX_PROF_LABEL_CHARS: usize = 256;

/// Maximum CPU-average window in ms (CTX-0189; PB-7 ten-minute window
/// parity: the average covers a bounded window, never an open-ended one).
pub const MAX_PROF_WINDOW_MS: u64 = 600_000;

/// Automation bearer TTL in ms (candidate default 10 minutes, never exceeds
/// the owning session lifetime; expiry revokes immediately).
pub const AUTOMATION_BEARER_TTL_MS: u64 = 600_000;

/// Maximum automation bearers tracked (parity with consent-ledger scale;
/// fail-closed at capacity, never evicted silently).
pub const MAX_AUTOMATION_BEARERS: usize = 64;

/// Maximum bytes for the raw automation `params` object (`synthesizeInput`
/// with 64 events needs more than the 4 KiB introspection bound; still well
/// under the 256 KiB frame bound).
pub const MAX_AUTOMATION_PARAMS_BYTES: usize = 32 * 1024;

/// Maximum characters for `originLabel` (audit attribution, required).
pub const MAX_ORIGIN_LABEL_CHARS: usize = 64;

/// Maximum bytes for one synthetic paste-text event (text-only, T-04 parity;
/// well under the frame bound).
pub const MAX_SYNTH_PASTE_BYTES: usize = 16 * 1024;

/// Maximum characters for a synthetic key name.
pub const MAX_SYNTH_KEY_CHARS: usize = 64;

/// Maximum characters for an automation bearer token (opaque, unguessable,
/// never persisted).
pub const MAX_BEARER_TOKEN_CHARS: usize = 128;

/// Maximum grid cells per axis for synthetic mouse coordinates (fail-closed).
pub const MAX_SYNTH_CELL: u16 = 1024;

/// Maximum absolute wheel delta rows/cols per event (fail-closed).
pub const MAX_SYNTH_WHEEL_DELTA: i32 = 64;

/// Redaction marker replacing sensitive frame lines (P0-AC-026 parity).
pub const REDACTED_MARKER: &str = "[redacted]";

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
///
/// `granted` is the server-evaluated scope set for the authenticated peer
/// (CLI default plus explicit `BITTY_CTL_ELEVATE` allowlist). Read-only
/// handlers ignore it (any authenticated same-UID peer may read); control
/// and automation handlers authorize against it on every request (never
/// ambient authority). `session_id` binds automation bearers to one debug
/// session: a bearer issued for another session fails closed with
/// `ScopeDenied` even when the token is otherwise valid.
#[derive(Debug, Clone)]
pub struct ServeContext {
    /// Server facts.
    pub server: ServerInfo,
    /// Uptime at request time (millis). Automation handlers reuse this as
    /// the deterministic bearer/rate clock (headless, no wall-clock).
    pub uptime_ms: u64,
    /// Server-evaluated granted scopes for this peer.
    pub granted: crate::scope::ScopeSet,
    /// Opaque debug-session identity for bearer binding (per connection;
    /// the servo must set a distinct id per accepted connection).
    pub session_id: String,
    /// Local-transport attestation (CTX-0244): true only when the serving
    /// path verified the peer is local — the Unix-socket accept boundary
    /// (`transport_attested_peer`, P0-AC-021) or same-process in-process
    /// dispatch. Fail-closed default `false`: `frameHash` denies without
    /// it, so a future non-local dispatch path can never serve digests by
    /// accident (no TCP listener exists today — keep it that way).
    pub local_attested: bool,
}

impl ServeContext {
    /// Build a context from server facts, stamping uptime now.
    ///
    /// Granted scopes default to the CLI interactive set plus the explicit
    /// `BITTY_CTL_ELEVATE` allowlist (impure: reads one env var; tests that
    /// need hermetic scopes use [`ServeContext::with_granted`]).
    /// `session_id` defaults to `"local"`; the servo overrides it per
    /// connection before dispatch.
    #[must_use]
    pub fn new(server: &ServerInfo) -> Self {
        Self {
            server: server.clone(),
            uptime_ms: server.uptime_ms(),
            granted: crate::ctl::elevation_from_env(
                std::env::var("BITTY_CTL_ELEVATE").ok().as_deref(),
            ),
            session_id: String::from("local"),
            local_attested: false,
        }
    }

    /// Mark this context as served over a verified-local transport
    /// (CTX-0244): call after [`transport_attested_peer`] at the
    /// Unix-socket accept boundary, or for same-process in-process
    /// dispatch (the headless verify harness). `frameHash` denies without
    /// this mark; all other methods ignore it.
    pub fn attest_local_peer(&mut self) {
        self.local_attested = true;
    }

    /// Build a context with explicit granted scopes (hermetic tests).
    #[must_use]
    pub fn with_granted(server: &ServerInfo, granted: crate::scope::ScopeSet) -> Self {
        Self {
            server: server.clone(),
            uptime_ms: server.uptime_ms(),
            granted,
            session_id: String::from("local"),
            local_attested: false,
        }
    }

    /// Build a context with explicit scopes and session binding (hermetic
    /// automation tests). `session_id` is truncated to 64 chars.
    #[must_use]
    pub fn with_granted_session(
        server: &ServerInfo,
        granted: crate::scope::ScopeSet,
        session_id: &str,
    ) -> Self {
        let mut id = session_id.to_string();
        if id.chars().count() > 64 {
            id = id.chars().take(64).collect();
        }
        if id.is_empty() {
            id = String::from("local");
        }
        Self {
            server: server.clone(),
            uptime_ms: server.uptime_ms(),
            granted,
            session_id: id,
            local_attested: false,
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
    // parse per-method scopes (`rows`/`cols`/`limit`, automation payloads)
    // without a JSON dependency. The slice is bounded before retention:
    // oversize params fail closed here rather than reaching dispatch.
    // Automation methods (`synthesizeInput`, `captureFrame`, `frameHash`)
    // carry up to 64 events and allow 32 KiB; all other methods stay at 4 KiB.
    let params_raw = match keys.values.get("params") {
        None => None,
        Some((start, end)) => {
            let raw = text[*start..*end].trim().to_string();
            let cap = if method == METHOD_SYNTHESIZE_INPUT
                || method == METHOD_CAPTURE_FRAME
                || method == METHOD_FRAME_HASH
            {
                MAX_AUTOMATION_PARAMS_BYTES
            } else {
                MAX_PARAMS_BYTES
            };
            if raw.len() > cap {
                return Err(RequestFault::new(
                    keys.id_raw.clone(),
                    "transport",
                    "PayloadTooLarge",
                    format!("params {} exceeds limit {cap}", raw.len()),
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
    /// `getFocus`) plus CTX-0171 runtime control (`listWindows`,
    /// `listViews`, `listTerminals`, `spawnTerminal`, `closeTerminal`,
    /// `sendInput`, `getTerminalText`, `splitView`, `focusView`,
    /// `reloadConfig`) plus CTX-0257 workspace entry (`listWorkspaces`,
    /// `createWorkspace`, `closeWorkspace`, `focusWorkspace`) plus CTX-0259
    /// workspace move (`moveWorkspace`) plus CTX-0188
    /// test automation (`synthesizeInput`,
    /// `captureFrame`, bearer-scoped per Amendment A1) plus CTX-0189 live
    /// profiling (`getProcessStats`, `getFrameStats`, `streamProcessStats`,
    /// `streamFrameStats`; sampling-only, scope-gated per Amendment A1).
    ///
    /// Introspection handlers register via [`Dispatcher::register`] (the
    /// CTX-0159 hook) so the registration path itself is exercised here, not
    /// just in tests. Method names are statically valid, so a registration
    /// failure here is a programming error surfaced loudly rather than a
    /// silent partial table. Control handlers authorize against
    /// `context.granted` on every request and enqueue to the cross-thread
    /// queue for the main thread to apply (the connection thread never
    /// touches `Runtime`).
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
        // CTX-0171 runtime control (scope-gated, main-thread applied).
        let control: &[(&'static str, DevtoolsHandler)] = &[
            (crate::ctl::METHOD_LIST_WINDOWS, handle_control),
            (crate::ctl::METHOD_LIST_VIEWS, handle_control),
            (crate::ctl::METHOD_LIST_TERMINALS, handle_control),
            (crate::ctl::METHOD_SPAWN_TERMINAL, handle_control),
            (crate::ctl::METHOD_CLOSE_TERMINAL, handle_control),
            (crate::ctl::METHOD_SEND_INPUT, handle_control),
            (crate::ctl::METHOD_GET_TERMINAL_TEXT, handle_control),
            (crate::ctl::METHOD_SPLIT_VIEW, handle_control),
            (crate::ctl::METHOD_FOCUS_VIEW, handle_control),
            (crate::ctl::METHOD_LIST_WORKSPACES, handle_control),
            (crate::ctl::METHOD_NEW_WORKSPACE, handle_control),
            (crate::ctl::METHOD_CLOSE_WORKSPACE, handle_control),
            (crate::ctl::METHOD_FOCUS_WORKSPACE, handle_control),
            (crate::ctl::METHOD_MOVE_WORKSPACE, handle_control),
            (crate::ctl::METHOD_RELOAD_CONFIG, handle_control),
        ];
        for (method, handler) in control {
            if table.register(method, *handler).is_err() {
                debug_assert!(false, "statically valid control method rejected");
            }
        }
        // CTX-0188 test automation (bearer-scoped, rate-capped, redacted).
        // CTX-0244 adds `frameHash` (digest-only, new `FrameDigest` family;
        // the `Capture` family is never widened).
        let automation: &[(&'static str, DevtoolsHandler)] = &[
            (METHOD_SYNTHESIZE_INPUT, handle_synthesize_input),
            (METHOD_CAPTURE_FRAME, handle_capture_frame),
            (METHOD_FRAME_HASH, handle_frame_hash),
        ];
        for (method, handler) in automation {
            if table.register(method, *handler).is_err() {
                debug_assert!(false, "statically valid automation method rejected");
            }
        }
        // CTX-0189 live profiling (sampling-only, bounded, redacted;
        // getters need `debug.inspect`, streams need `debug.trace`).
        let profiling: &[(&'static str, DevtoolsHandler)] = &[
            (METHOD_GET_PROCESS_STATS, handle_get_process_stats),
            (METHOD_GET_FRAME_STATS, handle_get_frame_stats),
            (METHOD_STREAM_PROCESS_STATS, handle_stream_process_stats),
            (METHOD_STREAM_FRAME_STATS, handle_stream_frame_stats),
        ];
        for (method, handler) in profiling {
            if table.register(method, *handler).is_err() {
                debug_assert!(false, "statically valid profiling method rejected");
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

// ── frame-digest live store (CTX-0244) ──────────────────────────────────────
//
// The runtime publishes the last presented headless RGBA frame here (only
// while a `FrameDigest` bearer is live — see
// [`frame_digest_publish_wanted`]); `handle_frame_hash` digests it without
// ever placing pixel bytes in a response. Same `&self`-only, bounded,
// drop-on-poison posture as the grid store.

/// Hard cap on RGBA bytes retained for digesting (64 MiB): mirrors
/// `bitty-render`'s `MAX_HEADLESS_SURFACE_BYTES` (CR-RENDER-01 parity — the
/// canonical RGBA read reuses the present-path allocation cap, no new
/// unbounded surface allocation). A local const (not an import) keeps
/// `bitty-ipc` dependency-free; the value is pinned by the digest tests
/// against multi-megapixel frames.
pub const MAX_DIGEST_RGBA_BYTES: usize = 64 * 1024 * 1024;

/// Stored headless frame for digesting (private; validated on entry).
#[derive(Debug, Clone, Default)]
struct StoredRgba {
    /// Frame width in physical pixels.
    width_px: u32,
    /// Frame height in physical pixels.
    height_px: u32,
    /// Present-path frame sequence bound into the digest.
    frame_seq: u64,
    /// Premultiplied RGBA bytes (`width*height*4`), never served raw.
    rgba: Vec<u8>,
}

/// Live RGBA store (empty until the runtime publishes after a present).
fn live_rgba_store() -> &'static Mutex<StoredRgba> {
    static STORE: OnceLock<Mutex<StoredRgba>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoredRgba::default()))
}

/// Publish one presented headless frame for digesting (called by
/// `bitty-runtime` after a successful headless present, gated on
/// [`frame_digest_publish_wanted`] so idle production pays nothing).
///
/// Fail-closed validation: zero extents, `rgba.len() != w*h*4` (checked
/// arithmetic, no overflow), or `rgba.len() > MAX_DIGEST_RGBA_BYTES` all
/// drop the publish (the next present republishes). A poisoned mutex drops
/// the publish. Pixel bytes are stored, never served: only the digest
/// leaves over IPC.
pub fn publish_frame_rgba(width_px: u32, height_px: u32, frame_seq: u64, rgba: Vec<u8>) {
    if width_px == 0 || height_px == 0 {
        return;
    }
    let expect = u64::from(width_px)
        .checked_mul(u64::from(height_px))
        .and_then(|pixels| pixels.checked_mul(4));
    let Some(expect) = expect else {
        return;
    };
    if expect == 0 || expect > MAX_DIGEST_RGBA_BYTES as u64 || rgba.len() as u64 != expect {
        return;
    }
    if let Ok(mut guard) = live_rgba_store().lock() {
        *guard = StoredRgba {
            width_px,
            height_px,
            frame_seq,
            rgba,
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
    if let Ok(mut guard) = live_rgba_store().lock() {
        *guard = StoredRgba::default();
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

// ── runtime control (CTX-0171) ─────────────────────────────────────────────
//
// Control handlers authorize against `context.granted` (server-evaluated,
// never client-asserted) and enqueue to the cross-thread queue for the main
// thread — the sole `Runtime` owner — to apply. The connection thread blocks
// up to 5 s for the reply; timeout becomes `Unavailable` (fail-closed, no
// partial state). List verbs (`listWindows`, `listViews`, `listTerminals`)
// also flow through the queue so `view`/`terminal` listings reflect live
// `Runtime` layout rather than stale startup facts.

/// Shared control handler for all fourteen `bitty.debug/*` control methods.
///
/// Validates params shape via `ctl` parsers (fail-closed `InvalidParams`
/// before enqueue), authorizes via `context.granted` (fail-closed
/// `ScopeDenied` without elevation), then enqueues and waits.
fn handle_control(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    // Fail fast on malformed params before touching the queue: each verb's
    // parser enforces its bounds (ids, text, cwd, direction).
    if let Err(reason) = prevalidate_control_params(&request.method, request.params_raw.as_deref())
    {
        return Err(HandlerError::new("usage", "InvalidParams", reason));
    }
    let reply = crate::ctl::enqueue_control_and_wait(
        &request.method,
        request.params_raw.as_deref(),
        &request.id_raw,
        &context.granted,
    );
    if reply.ok {
        Ok(reply.result_json)
    } else {
        Err(HandlerError::new(reply.category, reply.code, reply.message))
    }
}

/// Pre-enqueue params shape check (bounds only; existence resolves at apply).
fn prevalidate_control_params(method: &str, params: Option<&str>) -> Result<(), String> {
    let res: Result<(), crate::error::IpcError> = match method {
        m if m == crate::ctl::METHOD_CLOSE_TERMINAL
            || m == crate::ctl::METHOD_GET_TERMINAL_TEXT =>
        {
            crate::ctl::parse_terminal_id_params(params).map(|_| ())
        }
        m if m == crate::ctl::METHOD_SEND_INPUT => {
            crate::ctl::parse_send_params(params).map(|_| ())
        }
        m if m == crate::ctl::METHOD_SPAWN_TERMINAL => {
            crate::ctl::parse_spawn_params(params).map(|_| ())
        }
        m if m == crate::ctl::METHOD_SPLIT_VIEW => {
            crate::ctl::parse_split_params(params).map(|_| ())
        }
        m if m == crate::ctl::METHOD_FOCUS_VIEW => {
            crate::ctl::parse_focus_params(params).map(|_| ())
        }
        // listWindows/listViews/listTerminals/reloadConfig take no params.
        _ => Ok(()),
    };
    res.map_err(|err| format!("{err}"))
}

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
struct AutomationStore {
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
    synth_seq: u64,
    /// Bounded pixels/semantic audit (drop-oldest at 64).
    audit: Vec<FrameAuditEntry>,
}

/// Global automation store (empty until consent issuance).
fn automation_store() -> &'static Mutex<AutomationStore> {
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
fn authorize_automation(
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

// ── automation params parsing (bounded, no JSON deps) ───────────────────────

/// Extract a top-level string field from a flat params object (bounded,
/// quote-aware; nested objects for the key are rejected).
fn extract_top_string(params: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search = 0usize;
    let bytes = params.as_bytes();
    while let Some(pos) = params[search..].find(&needle) {
        let abs = search + pos;
        let mut i = abs + needle.len();
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' {
            search = abs + needle.len();
            continue;
        }
        i += 1;
        while i < bytes.len()
            && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r')
        {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return None;
        }
        i += 1;
        let mut out = String::new();
        while i < bytes.len() {
            match bytes[i] {
                b'"' => return Some(out),
                b'\\' => {
                    i += 1;
                    if i >= bytes.len() {
                        return None;
                    }
                    match bytes[i] {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if i + 4 >= bytes.len() {
                                return None;
                            }
                            let hex = &params[i + 1..i + 5];
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            out.push(char::from_u32(code)?);
                            i += 4;
                        }
                        _ => return None,
                    }
                    i += 1;
                }
                _ => {
                    let ch = params[i..].chars().next()?;
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        return None;
    }
    None
}

/// Extract a top-level boolean field (`true`/`false`); `None` when absent or
/// not a bare boolean.
fn extract_top_bool(params: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let pos = params.find(&needle)?;
    let after = &params[pos + needle.len()..];
    let colon = after.find(':')?;
    let value = after[colon + 1..].trim_start();
    if value.starts_with("true") {
        Some(true)
    } else if value.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Extract a top-level signed integer field; `None` when absent or malformed.
fn extract_top_int(params: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\"");
    let pos = params.find(&needle)?;
    let after = &params[pos + needle.len()..];
    let colon = after.find(':')?;
    let mut value = after[colon + 1..].trim_start();
    if value.starts_with('"') || value.starts_with('{') || value.starts_with('[') {
        return None;
    }
    let negative = value.starts_with('-');
    if negative || value.starts_with('+') {
        value = &value[1..];
    }
    let mut len = 0usize;
    for b in value.bytes() {
        if b.is_ascii_digit() {
            len += 1;
        } else {
            break;
        }
    }
    if len == 0 || len > 10 {
        return None;
    }
    let digits = &value[..len];
    let parsed: i64 = digits.parse().ok()?;
    Some(if negative { -parsed } else { parsed })
}

/// Extract a top-level unsigned integer field; `None` when absent/malformed.
fn extract_top_uint(params: &str, key: &str) -> Option<u64> {
    let value = extract_top_int(params, key)?;
    u64::try_from(value).ok()
}

/// Terminal id accepting Amendment A1 camelCase (`terminalId`) and the
/// CTX-0171 snake_case (`terminal_id`) harness shape.
fn extract_terminal_id(params: &str) -> Option<String> {
    extract_top_string(params, "terminalId").or_else(|| extract_top_string(params, "terminal_id"))
}

/// Origin label accepting `originLabel` (Amendment A1) and `origin_label`.
fn extract_origin_label(params: &str) -> Option<String> {
    extract_top_string(params, "originLabel").or_else(|| extract_top_string(params, "origin_label"))
}

/// Locate the `events` array span `(inner_start, inner_end)` inside `params`.
fn find_events_array(params: &str) -> Option<(usize, usize)> {
    let needle = "\"events\"";
    let pos = params.find(needle)?;
    let after_key = &params[pos + needle.len()..];
    let colon_rel = after_key.find(':')?;
    let mut i = pos + needle.len() + colon_rel + 1;
    let bytes = params.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let inner_start = i;
    let mut depth = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip strings (escape-aware).
                i += 1;
                let mut escape = false;
                while i < bytes.len() {
                    if escape {
                        escape = false;
                    } else if bytes[i] == b'\\' {
                        escape = true;
                    } else if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((inner_start, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split top-level `{...}` objects inside an array inner slice.
fn split_top_objects(inner: &str) -> Result<Vec<String>, ()> {
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'{' {
            return Err(());
        }
        let start = i;
        let mut depth = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i += 1;
                    let mut escape = false;
                    while i < bytes.len() {
                        if escape {
                            escape = false;
                        } else if bytes[i] == b'\\' {
                            escape = true;
                        } else if bytes[i] == b'"' {
                            break;
                        }
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return Err(());
                    }
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return Err(());
        }
        out.push(inner[start..i].to_string());
        if out.len() > MAX_SYNTH_EVENTS_PER_CALL {
            return Err(());
        }
    }
    Ok(out)
}

/// Validated synthetic event (headless; the servo maps it to input encoding).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SyntheticEvent {
    /// Key press/release.
    Key {
        /// Key name (bounded).
        key: String,
        /// Modifier summary (bounded, e.g. `ctrl+shift`).
        mods: String,
        /// Pressed (`true`) or released (`false`).
        pressed: bool,
    },
    /// Mouse button action at a cell.
    Mouse {
        /// `Left`, `Right`, or `Middle`.
        button: String,
        /// `pressed`, `released`, `click`, `drag`, or `move`.
        action: String,
        /// Cell column.
        col: u16,
        /// Cell row.
        row: u16,
    },
    /// Wheel scroll delta (cells).
    Wheel {
        /// Row delta (-64..=64).
        delta_rows: i32,
        /// Column delta (-64..=64).
        delta_cols: i32,
        /// Optional cell column.
        col: Option<u16>,
        /// Optional cell row.
        row: Option<u16>,
    },
    /// Paste-text (T-04 text-only parity).
    Paste {
        /// Pasted text (bounded, no NUL).
        text: String,
    },
}

/// Validate one event object; fail-closed with a bounded reason.
fn validate_synthetic_event(obj: &str) -> Result<SyntheticEvent, String> {
    let kind = extract_top_string(obj, "type")
        .ok_or_else(|| "event.type must be key|mouse|wheel|paste".to_string())?;
    match kind.as_str() {
        "key" => {
            let key = extract_top_string(obj, "key")
                .ok_or_else(|| "key event requires string key".to_string())?;
            if key.is_empty() || key.chars().count() > MAX_SYNTH_KEY_CHARS {
                return Err(format!("key must be 1..={MAX_SYNTH_KEY_CHARS} chars"));
            }
            if key.contains('\0') || key.bytes().any(|b| b < 0x20 && b != b'\t') {
                return Err("key must not contain control bytes".to_string());
            }
            let mods = extract_top_string(obj, "mods").unwrap_or_default();
            if mods.chars().count() > 16 {
                return Err("key mods must be <= 16 chars".to_string());
            }
            if !mods
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'_')
            {
                return Err("key mods must be alphanumeric with +-|_".to_string());
            }
            let pressed = extract_top_bool(obj, "pressed").unwrap_or(true);
            Ok(SyntheticEvent::Key { key, mods, pressed })
        }
        "mouse" => {
            let button = extract_top_string(obj, "button")
                .ok_or_else(|| "mouse event requires button".to_string())?;
            if !matches!(button.as_str(), "Left" | "Right" | "Middle") {
                return Err("mouse button must be Left|Right|Middle".to_string());
            }
            let action = extract_top_string(obj, "action").unwrap_or_else(|| "click".to_string());
            if !matches!(
                action.as_str(),
                "pressed" | "released" | "click" | "drag" | "move"
            ) {
                return Err("mouse action must be pressed|released|click|drag|move".to_string());
            }
            let col = extract_top_uint(obj, "col")
                .ok_or_else(|| "mouse event requires col".to_string())?;
            let row = extract_top_uint(obj, "row")
                .ok_or_else(|| "mouse event requires row".to_string())?;
            if col > u64::from(MAX_SYNTH_CELL) || row > u64::from(MAX_SYNTH_CELL) {
                return Err(format!("mouse col/row must be 0..={MAX_SYNTH_CELL}"));
            }
            Ok(SyntheticEvent::Mouse {
                button,
                action,
                col: col as u16,
                row: row as u16,
            })
        }
        "wheel" => {
            let delta_rows = extract_top_int(obj, "deltaRows")
                .or_else(|| extract_top_int(obj, "delta_rows"))
                .ok_or_else(|| "wheel event requires deltaRows".to_string())?;
            let delta_cols = extract_top_int(obj, "deltaCols")
                .or_else(|| extract_top_int(obj, "delta_cols"))
                .unwrap_or(0);
            if delta_rows < i64::from(-MAX_SYNTH_WHEEL_DELTA)
                || delta_rows > i64::from(MAX_SYNTH_WHEEL_DELTA)
                || delta_cols < i64::from(-MAX_SYNTH_WHEEL_DELTA)
                || delta_cols > i64::from(MAX_SYNTH_WHEEL_DELTA)
            {
                return Err(format!(
                    "wheel delta must be -{MAX_SYNTH_WHEEL_DELTA}..={MAX_SYNTH_WHEEL_DELTA}"
                ));
            }
            if delta_rows == 0 && delta_cols == 0 {
                return Err("wheel delta must be non-zero".to_string());
            }
            let col = match extract_top_uint(obj, "col") {
                Some(v) => {
                    if v > u64::from(MAX_SYNTH_CELL) {
                        return Err(format!("wheel col must be 0..={MAX_SYNTH_CELL}"));
                    }
                    Some(v as u16)
                }
                None => None,
            };
            let row = match extract_top_uint(obj, "row") {
                Some(v) => {
                    if v > u64::from(MAX_SYNTH_CELL) {
                        return Err(format!("wheel row must be 0..={MAX_SYNTH_CELL}"));
                    }
                    Some(v as u16)
                }
                None => None,
            };
            Ok(SyntheticEvent::Wheel {
                delta_rows: delta_rows as i32,
                delta_cols: delta_cols as i32,
                col,
                row,
            })
        }
        "paste" => {
            let text = extract_top_string(obj, "text")
                .ok_or_else(|| "paste event requires string text".to_string())?;
            if text.is_empty() || text.len() > MAX_SYNTH_PASTE_BYTES {
                return Err(format!(
                    "paste text must be 1..={MAX_SYNTH_PASTE_BYTES} bytes"
                ));
            }
            if text.contains('\0') {
                return Err("paste text must not contain NUL".to_string());
            }
            Ok(SyntheticEvent::Paste { text })
        }
        other => Err(format!(
            "event.type must be key|mouse|wheel|paste, got {}",
            echo_snippet(other)
        )),
    }
}

/// Whether a frame line carries sensitive content (P0-AC-026 parity: secrets,
/// clipboard bytes, environment bytes never appear in default outputs).
fn is_sensitive_frame_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "secret",
        "password",
        "passwd",
        "token",
        "clipboard",
        "bearer",
        "aws_",
        "begin private",
        "env=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Redact one frame line (whole-line replacement, fail-closed minimizing).
fn redact_frame_line(line: &str) -> String {
    if is_sensitive_frame_line(line) {
        REDACTED_MARKER.to_string()
    } else {
        line.to_string()
    }
}

/// `bitty.debug/synthesizeInput`: bearer-scoped input synthesis.
///
/// Params (object, `<= 32 KiB`): `{ terminalId|terminal_id: "t:N", bearer:
/// "<token>", originLabel|origin_label: "...", events: [...] }` with 1..=64
/// events of type key/mouse/wheel/paste. Requires `debug.control` +
/// `terminal.input` plus a live `synthesize` bearer for the addressed
/// terminal/session. Success publishes indelible `[synthetic]` markers into
/// the input ring (harness/user distinguishable) and returns a receipt with
/// `accepted`, `rejected: 0`, and the new `syntheticSeq`.
fn handle_synthesize_input(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::{DebugControl, TerminalInput};
    let params = request.params_raw.as_deref().ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "synthesizeInput requires params".to_string(),
        )
    })?;
    let terminal_id = extract_terminal_id(params).ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "synthesizeInput requires terminalId \"t:N\"".to_string(),
        )
    })?;
    if crate::ctl::parse_terminal_id(&terminal_id).is_err() {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "terminalId must match ^t:[0-9]+$ (no wildcards)".to_string(),
        ));
    }
    let origin = extract_origin_label(params).ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "synthesizeInput requires originLabel".to_string(),
        )
    })?;
    if origin.is_empty() || origin.chars().count() > MAX_ORIGIN_LABEL_CHARS {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("originLabel must be 1..={MAX_ORIGIN_LABEL_CHARS} chars"),
        ));
    }
    if origin.contains('\0') || origin.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "originLabel must not contain control bytes".to_string(),
        ));
    }
    let bearer = extract_top_string(params, "bearer");
    let (inner_start, inner_end) = find_events_array(params).ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "synthesizeInput requires events array".to_string(),
        )
    })?;
    let inner = &params[inner_start..inner_end];
    let objects = split_top_objects(inner).map_err(|()| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "events must be objects".to_string(),
        )
    })?;
    if objects.is_empty() || objects.len() > MAX_SYNTH_EVENTS_PER_CALL {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            format!("events must be 1..={MAX_SYNTH_EVENTS_PER_CALL}"),
        ));
    }
    // Validate every event before touching any state (transactional: no
    // partial publish on a malformed call).
    let mut validated: Vec<SyntheticEvent> = Vec::with_capacity(objects.len());
    for obj in &objects {
        match validate_synthetic_event(obj) {
            Ok(event) => validated.push(event),
            Err(reason) => {
                return Err(HandlerError::new("usage", "InvalidParams", reason));
            }
        }
    }
    // Authorize (scope intersection + bearer binding + rate ceiling) before
    // any observable effect.
    authorize_automation(
        &context.granted,
        &[DebugControl, TerminalInput],
        bearer.as_deref(),
        &context.session_id,
        &terminal_id,
        AutomationFamily::Synthesize,
        context.uptime_ms,
    )?;
    // Publish indelible synthetic markers into the input ring (drop-oldest,
    // bounded). A poisoned mutex fails closed without a receipt.
    let accepted = validated.len();
    let synth_seq = {
        let mut store = automation_store().lock().map_err(|_| {
            HandlerError::new(
                "transport",
                "Unavailable",
                "automation store unavailable".to_string(),
            )
        })?;
        store.synth_seq = store.synth_seq.saturating_add(accepted as u64);
        store.synth_seq
    };
    {
        let mut ring = live_input_store().lock().map_err(|_| {
            HandlerError::new(
                "transport",
                "Unavailable",
                "introspection store unavailable".to_string(),
            )
        })?;
        for event in &validated {
            let (kind, label) = match event {
                SyntheticEvent::Key { key, mods, pressed } => (
                    "key".to_string(),
                    format!("[synthetic:{origin}] key:{key} mods:{mods} pressed:{pressed}"),
                ),
                SyntheticEvent::Mouse {
                    button,
                    action,
                    col,
                    row,
                } => (
                    "mouse".to_string(),
                    format!("[synthetic:{origin}] mouse:{button} {action} col={col} row={row}"),
                ),
                SyntheticEvent::Wheel {
                    delta_rows,
                    delta_cols,
                    col,
                    row,
                } => (
                    "wheel".to_string(),
                    format!(
                        "[synthetic:{origin}] wheel rows={delta_rows} cols={delta_cols} col={} row={}",
                        col.map_or(String::from("-"), |c| c.to_string()),
                        row.map_or(String::from("-"), |r| r.to_string()),
                    ),
                ),
                SyntheticEvent::Paste { text } => {
                    let preview: String = text.chars().take(32).collect();
                    (
                        "paste".to_string(),
                        format!("[synthetic:{origin}] paste:{preview}"),
                    )
                }
            };
            ring.push(InputEventPublish {
                seq: synth_seq,
                kind: truncate_chars(&kind, 16),
                label: truncate_chars(&label, MAX_INPUT_LABEL_CHARS),
                shift: false,
                control: false,
                alt: false,
                button: None,
                col: None,
                row: None,
                pressed: None,
            });
            while ring.len() > MAX_INPUT_RING {
                ring.remove(0);
            }
        }
    }
    let mut out = String::with_capacity(256);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"receipt\":\"synthesize\",\"terminalId\":\"");
    json_escape_into(&mut out, &terminal_id);
    out.push_str("\",\"accepted\":");
    out.push_str(&accepted.to_string());
    out.push_str(",\"rejected\":0,\"syntheticSeq\":");
    out.push_str(&synth_seq.to_string());
    out.push_str(",\"originLabel\":\"");
    json_escape_into(&mut out, &origin);
    out.push_str("\",\"synthetic\":true}");
    Ok(out)
}

/// `bitty.debug/captureFrame`: bearer-scoped redacted frame capture.
///
/// Params (object, `<= 32 KiB`): `{ terminalId|terminal_id: "t:N", bearer:
/// "<token>", format: "semantic"|"pixels" (default semantic),
/// explicitOptIn: true (required for pixels), rows/cols viewport caps }.
/// Requires `debug.trace` + `terminal.inspect` plus a live `capture` bearer.
/// `semantic` returns redacted grid text; `pixels` returns a masked record
/// with zero text (audited with caller identity). Every response carries
/// `"trust":"untrusted-observation"` (T-10 parity).
fn handle_capture_frame(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::{DebugTrace, TerminalInspect};
    let params = request.params_raw.as_deref().ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "captureFrame requires params".to_string(),
        )
    })?;
    let terminal_id = extract_terminal_id(params).ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "captureFrame requires terminalId \"t:N\"".to_string(),
        )
    })?;
    if crate::ctl::parse_terminal_id(&terminal_id).is_err() {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "terminalId must match ^t:[0-9]+$ (no wildcards)".to_string(),
        ));
    }
    let bearer = extract_top_string(params, "bearer");
    let format = extract_top_string(params, "format").unwrap_or_else(|| "semantic".to_string());
    if format != "semantic" && format != "pixels" {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "format must be semantic|pixels".to_string(),
        ));
    }
    if format == "pixels" && extract_top_bool(params, "explicitOptIn") != Some(true) {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "pixels capture requires explicitOptIn true".to_string(),
        ));
    }
    authorize_automation(
        &context.granted,
        &[DebugTrace, TerminalInspect],
        bearer.as_deref(),
        &context.session_id,
        &terminal_id,
        AutomationFamily::Capture,
        context.uptime_ms,
    )?;
    // Viewport caps reuse the introspection bounds (fail-closed).
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
    let grid_cols = if guard.cols == 0 {
        context.server.cols
    } else {
        guard.cols
    };
    let grid_rows = if guard.rows == 0 {
        context.server.rows
    } else {
        guard.rows
    };
    let frame_seq = guard.generation;
    // Audit every capture with caller identity (pixels mandatory, semantic
    // uniform). Bounded drop-oldest; poison fails closed without a record.
    {
        if let Ok(mut store) = automation_store().lock() {
            store.audit.push(FrameAuditEntry {
                session_id: context.session_id.clone(),
                terminal_id: terminal_id.clone(),
                format: format.clone(),
                now_ms: context.uptime_ms,
                frame_seq,
                digest_hex: String::new(),
            });
            while store.audit.len() > MAX_AUTOMATION_BEARERS {
                store.audit.remove(0);
            }
        }
    }
    if format == "pixels" {
        let mut out = String::with_capacity(256);
        out.push_str("{\"version\":\"");
        out.push_str(DEVTOOLS_PROTOCOL_VERSION);
        out.push_str("\",\"snapshot\":\"frame\",\"format\":\"pixels\",\"terminalId\":\"");
        json_escape_into(&mut out, &terminal_id);
        out.push_str("\",\"masked\":true,\"cols\":");
        out.push_str(&grid_cols.to_string());
        out.push_str(",\"rows\":");
        out.push_str(&grid_rows.to_string());
        out.push_str(",\"frameSeq\":");
        out.push_str(&frame_seq.to_string());
        out.push_str(",\"trust\":\"untrusted-observation\",\"caller\":\"");
        json_escape_into(&mut out, &context.session_id);
        out.push_str("\",\"audited\":true}");
        return Ok(out);
    }
    let take = rows.min(guard.lines.len());
    let mut out = String::with_capacity(1024.min(MAX_INSPECT_JSON_BYTES));
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"frame\",\"format\":\"semantic\",\"terminalId\":\"");
    json_escape_into(&mut out, &terminal_id);
    out.push_str("\",\"lines\":[");
    for (i, line) in guard.lines.iter().take(take).enumerate() {
        if i > 0 {
            out.push(',');
        }
        let cut = truncate_line(line, cols);
        let redacted = redact_frame_line(&cut);
        out.push('"');
        json_escape_into(&mut out, &redacted);
        out.push('"');
        if out.len() > MAX_INSPECT_JSON_BYTES {
            return Err(HandlerError::new(
                "transport",
                "PayloadTooLarge",
                "frame snapshot exceeds response bound".to_string(),
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
    out.push_str(&grid_cols.to_string());
    out.push_str(",\"rows\":");
    out.push_str(&grid_rows.to_string());
    out.push_str(",\"frameSeq\":");
    out.push_str(&frame_seq.to_string());
    out.push_str(",\"trust\":\"untrusted-observation\"}");
    if out.len() > MAX_INSPECT_JSON_BYTES {
        return Err(HandlerError::new(
            "transport",
            "PayloadTooLarge",
            "frame snapshot exceeds response bound".to_string(),
        ));
    }
    Ok(out)
}

/// Append one `digest` audit entry to the bounded log (64, drop-oldest).
///
/// Poisoned store fails closed without a record (existing parity). Called
/// for every attributable `frameHash` call — granted AND denied — so the
/// digest oracle leaves a per-call trace with caller identity, frame
/// sequence, and the served digest (uninvertible, safe to log).
fn audit_digest_attempt(
    session_id: &str,
    terminal_id: &str,
    now_ms: u64,
    frame_seq: u64,
    digest_hex: &str,
) {
    if let Ok(mut store) = automation_store().lock() {
        store.audit.push(FrameAuditEntry {
            session_id: session_id.to_string(),
            terminal_id: terminal_id.to_string(),
            format: String::from("digest"),
            now_ms,
            frame_seq,
            digest_hex: digest_hex.to_string(),
        });
        while store.audit.len() > MAX_AUTOMATION_BEARERS {
            store.audit.remove(0);
        }
    }
}

/// `bitty.debug/frameHash`: bearer-scoped lossless frame digest (CTX-0244).
///
/// Params (object, `<= 32 KiB`): `{ terminalId|terminal_id: "t:N", bearer:
/// "<token>" }`. Requires `debug.trace` + `terminal.inspect` plus a live
/// `frame-digest` bearer for the addressed terminal/session, a
/// local-attested transport ([`ServeContext::local_attested`], P0-AC-021
/// parity), and a published headless frame. Returns the SHA-256 hex digest
/// over `canonical_frame_bytes(width_px, height_px, frame_seq, rgba)` — 32
/// bytes that prove frame equality with zero pixel bytes on the wire —
/// plus the bound geometry and `"trust":"untrusted-observation"` (T-10
/// parity). No `explicitOptIn`: nothing human-readable is returned, grant
/// possession IS the opt-in.
///
/// Fail-closed ordering (no oracle, zero partial state): params shape, then
/// local attestation, then scope+bearer+expiry+rate (all `ScopeDenied`),
/// then frame availability (`Unavailable` — never a hash of nothing, which
/// would read as false equality). A `Capture` bearer MUST NOT authorize
/// here (family-mismatch `ScopeDenied`, never widened).
fn handle_frame_hash(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::frame_digest::{FRAME_DIGEST_ALGO, frame_digest_hex};
    use crate::scope::Scope::{DebugTrace, TerminalInspect};
    let params = request.params_raw.as_deref().ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "frameHash requires params".to_string(),
        )
    })?;
    let terminal_id = extract_terminal_id(params).ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "frameHash requires terminalId \"t:N\"".to_string(),
        )
    })?;
    if crate::ctl::parse_terminal_id(&terminal_id).is_err() {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "terminalId must match ^t:[0-9]+$ (no wildcards)".to_string(),
        ));
    }
    // Local-only transport, revalidated per call before any digest work:
    // the Unix-socket accept boundary (`transport_attested_peer`) or
    // same-process dispatch must have marked this context. Never over TCP
    // (no listener exists) and never for a foreign user.
    if !context.local_attested {
        audit_digest_attempt(&context.session_id, &terminal_id, context.uptime_ms, 0, "");
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "permission denied: frameHash requires a local attested transport".to_string(),
        ));
    }
    let bearer = extract_top_string(params, "bearer");
    if let Err(err) = authorize_automation(
        &context.granted,
        &[DebugTrace, TerminalInspect],
        bearer.as_deref(),
        &context.session_id,
        &terminal_id,
        AutomationFamily::FrameDigest,
        context.uptime_ms,
    ) {
        audit_digest_attempt(&context.session_id, &terminal_id, context.uptime_ms, 0, "");
        return Err(err);
    }
    // Snapshot the published present source (clone under the lock, hash
    // after release). An empty/unpresented surface is `Unavailable`, which
    // reads as indeterminate — never as false equality.
    let (width_px, height_px, frame_seq, rgba) = {
        let guard = live_rgba_store().lock().map_err(|_| {
            HandlerError::new(
                "transport",
                "Unavailable",
                "frame store unavailable".to_string(),
            )
        })?;
        if guard.rgba.is_empty() {
            audit_digest_attempt(&context.session_id, &terminal_id, context.uptime_ms, 0, "");
            return Err(HandlerError::new(
                "transport",
                "Unavailable",
                "no presented frame to digest".to_string(),
            ));
        }
        (
            guard.width_px,
            guard.height_px,
            guard.frame_seq,
            guard.rgba.clone(),
        )
    };
    let digest = frame_digest_hex(width_px, height_px, frame_seq, &rgba);
    audit_digest_attempt(
        &context.session_id,
        &terminal_id,
        context.uptime_ms,
        frame_seq,
        &digest,
    );
    // Informational grid geometry (captureFrame parity: grid store, server
    // fallback on poison — the digest itself already binds pixel geometry
    // plus frameSeq, so no security decision depends on these numbers).
    let (grid_cols, grid_rows) =
        live_grid_store()
            .lock()
            .map_or((context.server.cols, context.server.rows), |guard| {
                (
                    if guard.cols == 0 {
                        context.server.cols
                    } else {
                        guard.cols
                    },
                    if guard.rows == 0 {
                        context.server.rows
                    } else {
                        guard.rows
                    },
                )
            });
    let mut out = String::with_capacity(256);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"frameHash\",\"terminalId\":\"");
    json_escape_into(&mut out, &terminal_id);
    out.push_str("\",\"cols\":");
    out.push_str(&grid_cols.to_string());
    out.push_str(",\"rows\":");
    out.push_str(&grid_rows.to_string());
    out.push_str(",\"widthPx\":");
    out.push_str(&width_px.to_string());
    out.push_str(",\"heightPx\":");
    out.push_str(&height_px.to_string());
    out.push_str(",\"frameSeq\":");
    out.push_str(&frame_seq.to_string());
    out.push_str(",\"algo\":\"");
    out.push_str(FRAME_DIGEST_ALGO);
    out.push_str("\",\"digest\":\"");
    out.push_str(&digest);
    out.push_str("\",\"trust\":\"untrusted-observation\"}");
    Ok(out)
}

// ── live profiling (CTX-0189, Amendment A1 candidate) ───────────────────────
//
// Read-only observation of process and rendering health through the same
// versioned debug protocol. Metric definitions and measurement conditions
// are reused from the Performance Budget RFC (PB-2 idle memory, PB-3
// typical-session memory and growth, PB-4 input latency, PB-7 idle
// resources); this surface observes those budgets and changes no number.
//
// Sampling is the only posture: the host publishes pre-aggregated counters
// on the cold path (`publish_process_stats` / `publish_frame_stats`,
// `&self`-style side-effect-free entry points the runtime drives from its
// sampler); this module retains the latest [`MAX_PROF_SAMPLES`] records per
// family (drop-oldest, counted) and serves point-in-time getters plus
// cursor-based stream drains over the accepted batching (32 records or
// 8 KiB per wakeup, sequence plus drop-count headers). No profiler code
// runs on the parser, render, or input hot paths; no per-frame tracing and
// no sub-100 ms cadence exists in this scope.
//
// Privacy: records carry zero terminal bytes (no PTY output, no clipboard,
// no environment maps, no frame text). The only string is the
// renderer-supplied backend label, bounded to [`MAX_PROF_LABEL_CHARS`]
// characters with control bytes stripped at publish time, and every
// frame-stats response carries `"trust":"untrusted-observation"` (T-10
// parity). No profiling state crosses the v1 MCP adapter.
//
// Authorization: getters require `debug.inspect`; streams require
// `debug.trace`. Scope checks run before any state is touched; unscoped
// callers get `scope`/`ScopeDenied` with zero partial state (fail closed).
// Reads are in-memory bounded ring reads under the transport RC-9 rate
// limits, so no per-method rate ceiling applies; the declared stream
// cadence is validated against the 100 ms floor instead (client-paced
// polling, server-enforced floor).

/// Process health counters published by the host sampler (numeric only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStatsPublish {
    /// Resident set size in bytes (PB-2/PB-3 conditions).
    pub rss_bytes: u64,
    /// Average CPU over `window_ms`, fixed-point percent x100 (PB-7
    /// conditions; e.g. 1% reads as 100). Multi-core hosts may exceed 100%.
    pub cpu_avg_pct_x100: u32,
    /// Live task count (PB-7 conditions).
    pub tasks: u64,
    /// Live timer count (PB-7 conditions).
    pub timers: u64,
    /// Window the CPU average covers, in ms (bounded to
    /// [`MAX_PROF_WINDOW_MS`]).
    pub window_ms: u64,
}

/// Rendering health counters published by the host sampler (numeric only,
/// plus one bounded renderer label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStatsPublish {
    /// Frame-time p50 in microseconds (PB-4 conditions).
    pub frame_p50_us: u64,
    /// Frame-time p99 in microseconds (PB-4 conditions).
    pub frame_p99_us: u64,
    /// Presented frames per second.
    pub presented_fps: u32,
    /// Missed-present (dropped vsync) count, monotonic.
    pub missed_presents: u64,
    /// GPU memory in bytes where the renderer exposes it (`None` omits the
    /// field rather than fabricating a zero).
    pub gpu_bytes: Option<u64>,
    /// Renderer backend label (e.g. `"wgpu-vulkan"`); truncated to
    /// [`MAX_PROF_LABEL_CHARS`] characters, control bytes stripped, never
    /// terminal content.
    pub backend: String,
}

/// One retained process sample (monotonic `seq`, host clock `now_ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ProcessSample {
    /// Monotonic per-family sequence (starts at 1).
    seq: u64,
    /// Host monotonic clock at sampling time (ms; matches the
    /// `ServeContext::uptime_ms` base, never wall-clock).
    now_ms: u64,
    /// Resident set size in bytes.
    rss_bytes: u64,
    /// Average CPU percent x100 over `window_ms`.
    cpu_avg_pct_x100: u32,
    /// Live task count.
    tasks: u64,
    /// Live timer count.
    timers: u64,
    /// CPU-average window in ms.
    window_ms: u64,
}

/// One retained frame sample (monotonic `seq`, host clock `now_ms`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FrameSample {
    /// Monotonic per-family sequence (starts at 1).
    seq: u64,
    /// Host monotonic clock at sampling time (ms).
    now_ms: u64,
    /// Frame-time p50 in microseconds.
    frame_p50_us: u64,
    /// Frame-time p99 in microseconds.
    frame_p99_us: u64,
    /// Presented frames per second.
    presented_fps: u32,
    /// Missed-present count, monotonic.
    missed_presents: u64,
    /// GPU memory in bytes, when exposed.
    gpu_bytes: Option<u64>,
    /// Bounded renderer backend label (untrusted observation data).
    backend: String,
}

/// Profiling store: two bounded latest-wins rings with counted drops.
#[derive(Debug, Default)]
struct ProfilingStore {
    /// Retained process samples (drop-oldest at [`MAX_PROF_SAMPLES`]).
    process: std::collections::VecDeque<ProcessSample>,
    /// Retained frame samples (drop-oldest at [`MAX_PROF_SAMPLES`]).
    frame: std::collections::VecDeque<FrameSample>,
    /// Next process sequence (monotonic, never reused).
    process_seq: u64,
    /// Process samples dropped at the bound (cumulative).
    process_dropped: u64,
    /// Next frame sequence (monotonic, never reused).
    frame_seq: u64,
    /// Frame samples dropped at the bound (cumulative).
    frame_dropped: u64,
}

/// Live profiling store (empty until the host sampler publishes).
fn live_profiling_store() -> &'static Mutex<ProfilingStore> {
    static STORE: OnceLock<Mutex<ProfilingStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(ProfilingStore::default()))
}

/// Sanitize a renderer label: strip NUL/control bytes, truncate to
/// [`MAX_PROF_LABEL_CHARS`] characters (char-boundary safe, no ellipsis so
/// the bound holds exactly).
fn sanitize_prof_label(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PROF_LABEL_CHARS)
        .collect();
    clean
}

/// Publish one process sample (called by the host sampler on the cold path).
///
/// The ring is latest-wins: beyond [`MAX_PROF_SAMPLES`] the oldest sample
/// is dropped and the drop counter increments (consumers converge to latest
/// state). `window_ms` is clamped to `1..=[`MAX_PROF_WINDOW_MS`]`; a
/// poisoned mutex fails closed by dropping the publish (the next tick
/// republishes).
pub fn publish_process_stats(now_ms: u64, stats: ProcessStatsPublish) {
    let Ok(mut store) = live_profiling_store().lock() else {
        return;
    };
    store.process_seq = store.process_seq.saturating_add(1);
    let seq = store.process_seq;
    if store.process.len() >= MAX_PROF_SAMPLES {
        store.process.pop_front();
        store.process_dropped = store.process_dropped.saturating_add(1);
    }
    store.process.push_back(ProcessSample {
        seq,
        now_ms,
        rss_bytes: stats.rss_bytes,
        cpu_avg_pct_x100: stats.cpu_avg_pct_x100,
        tasks: stats.tasks,
        timers: stats.timers,
        window_ms: stats.window_ms.clamp(1, MAX_PROF_WINDOW_MS),
    });
}

/// Publish one frame sample (called by the host sampler on the cold path).
///
/// Same latest-wins ring discipline as [`publish_process_stats`]. The
/// backend label is sanitized (control bytes stripped, truncated to
/// [`MAX_PROF_LABEL_CHARS`] characters) so no terminal content can enter
/// the store through a renderer string.
pub fn publish_frame_stats(now_ms: u64, stats: FrameStatsPublish) {
    let Ok(mut store) = live_profiling_store().lock() else {
        return;
    };
    store.frame_seq = store.frame_seq.saturating_add(1);
    let seq = store.frame_seq;
    if store.frame.len() >= MAX_PROF_SAMPLES {
        store.frame.pop_front();
        store.frame_dropped = store.frame_dropped.saturating_add(1);
    }
    store.frame.push_back(FrameSample {
        seq,
        now_ms,
        frame_p50_us: stats.frame_p50_us,
        frame_p99_us: stats.frame_p99_us,
        presented_fps: stats.presented_fps,
        missed_presents: stats.missed_presents,
        gpu_bytes: stats.gpu_bytes,
        backend: sanitize_prof_label(&stats.backend),
    });
}

/// Clear the live profiling store (test helper only; production never calls
/// it). Tests publish known samples and must not leak them into parallel
/// tests sharing the process-global store.
pub fn clear_profiling_for_tests() {
    if let Ok(mut store) = live_profiling_store().lock() {
        store.process.clear();
        store.frame.clear();
        store.process_seq = 0;
        store.process_dropped = 0;
        store.frame_seq = 0;
        store.frame_dropped = 0;
    }
}

/// Require one debug scope for a profiling call (fail-closed).
///
/// Scope checks run before any state is touched; denial carries the typed
/// `scope`/`ScopeDenied` shape with zero partial state.
fn require_profiling_scope(
    granted: &crate::scope::ScopeSet,
    scope: crate::scope::Scope,
) -> Result<(), HandlerError> {
    if granted.contains(scope) {
        return Ok(());
    }
    Err(HandlerError::new(
        "scope",
        "ScopeDenied",
        format!(
            "permission denied: scope '{}' denied for profiling (needs elevation)",
            scope.as_str()
        ),
    ))
}

/// Parse the declared stream cadence `intervalMs` (default
/// [`DEFAULT_PROF_INTERVAL_MS`]; fail-closed outside
/// `MIN_PROF_INTERVAL_MS..=MAX_PROF_INTERVAL_MS` or when present but not a
/// plain non-negative integer).
fn parse_interval_ms_param(params_raw: Option<&str>) -> Result<u64, HandlerError> {
    let invalid = || {
        HandlerError::new(
            "usage",
            "InvalidParams",
            format!("params intervalMs must be {MIN_PROF_INTERVAL_MS}..={MAX_PROF_INTERVAL_MS}"),
        )
    };
    let Some(params) = params_raw else {
        return Ok(DEFAULT_PROF_INTERVAL_MS);
    };
    let needle = "\"intervalMs\"";
    let Some(key_pos) = params.find(needle) else {
        return Ok(DEFAULT_PROF_INTERVAL_MS);
    };
    let after_key = &params[key_pos + needle.len()..];
    let Some(colon) = after_key.find(':') else {
        return Err(invalid());
    };
    let mut value_part = after_key[colon + 1..].trim_start();
    if value_part.starts_with('"')
        || value_part.starts_with('{')
        || value_part.starts_with('[')
        || value_part.starts_with('-')
        || value_part.starts_with('+')
    {
        return Err(invalid());
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
        return Err(invalid());
    }
    value_part = &value_part[..len];
    let value: u64 = value_part.parse().map_err(|_| invalid())?;
    if !(MIN_PROF_INTERVAL_MS..=MAX_PROF_INTERVAL_MS).contains(&value) {
        return Err(invalid());
    }
    Ok(value)
}

/// Parse the stream cursor `afterSeq` (default 0: drain from the oldest
/// retained sample; fail-closed when present but not a plain non-negative
/// integer).
fn parse_after_seq_param(params_raw: Option<&str>) -> Result<u64, HandlerError> {
    let invalid = || {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "params afterSeq must be a non-negative integer".to_string(),
        )
    };
    let Some(params) = params_raw else {
        return Ok(0);
    };
    let needle = "\"afterSeq\"";
    let Some(key_pos) = params.find(needle) else {
        return Ok(0);
    };
    let after_key = &params[key_pos + needle.len()..];
    let Some(colon) = after_key.find(':') else {
        return Err(invalid());
    };
    let mut value_part = after_key[colon + 1..].trim_start();
    if value_part.starts_with('"')
        || value_part.starts_with('{')
        || value_part.starts_with('[')
        || value_part.starts_with('-')
        || value_part.starts_with('+')
    {
        return Err(invalid());
    }
    let mut len = 0usize;
    for b in value_part.bytes() {
        if b.is_ascii_digit() {
            len += 1;
        } else {
            break;
        }
    }
    if len == 0 || len > 20 {
        return Err(invalid());
    }
    value_part = &value_part[..len];
    value_part.parse().map_err(|_| invalid())
}

/// Encode one process sample as a JSON object (numeric fields only; zero
/// terminal bytes by construction).
fn encode_process_sample(out: &mut String, sample: &ProcessSample) {
    out.push_str("{\"seq\":");
    out.push_str(&sample.seq.to_string());
    out.push_str(",\"nowMs\":");
    out.push_str(&sample.now_ms.to_string());
    out.push_str(",\"rssBytes\":");
    out.push_str(&sample.rss_bytes.to_string());
    out.push_str(",\"cpuAvgPctX100\":");
    out.push_str(&sample.cpu_avg_pct_x100.to_string());
    out.push_str(",\"cpuWindowMs\":");
    out.push_str(&sample.window_ms.to_string());
    out.push_str(",\"tasks\":");
    out.push_str(&sample.tasks.to_string());
    out.push_str(",\"timers\":");
    out.push_str(&sample.timers.to_string());
    out.push('}');
}

/// Encode one frame sample as a JSON object (numeric fields plus the
/// bounded renderer label; zero terminal bytes by construction).
fn encode_frame_sample(out: &mut String, sample: &FrameSample) {
    out.push_str("{\"seq\":");
    out.push_str(&sample.seq.to_string());
    out.push_str(",\"nowMs\":");
    out.push_str(&sample.now_ms.to_string());
    out.push_str(",\"frameP50Us\":");
    out.push_str(&sample.frame_p50_us.to_string());
    out.push_str(",\"frameP99Us\":");
    out.push_str(&sample.frame_p99_us.to_string());
    out.push_str(",\"presentedFps\":");
    out.push_str(&sample.presented_fps.to_string());
    out.push_str(",\"missedPresents\":");
    out.push_str(&sample.missed_presents.to_string());
    if let Some(gpu) = sample.gpu_bytes {
        out.push_str(",\"gpuBytes\":");
        out.push_str(&gpu.to_string());
    }
    out.push_str(",\"backend\":\"");
    json_escape_into(out, &sample.backend);
    out.push_str("\"}");
}

/// `bitty.debug/getProcessStats`: latest process health snapshot.
///
/// Requires `debug.inspect`. Returns numeric aggregates only (RSS, average
/// CPU over a bounded window, task/timer counts); an empty store returns
/// `"sample":"none"` with no numeric fields rather than fabricated zeros.
fn handle_get_process_stats(
    context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::DebugInspect;
    require_profiling_scope(&context.granted, DebugInspect)?;
    let guard = live_profiling_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "profiling store unavailable".to_string(),
        )
    })?;
    let mut out = String::with_capacity(256);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"process-stats\"");
    let Some(latest) = guard.process.back() else {
        out.push_str(",\"sample\":\"none\"}");
        return Ok(out);
    };
    out.push_str(",\"sample\":\"latest\",\"seq\":");
    out.push_str(&latest.seq.to_string());
    out.push_str(",\"nowMs\":");
    out.push_str(&latest.now_ms.to_string());
    out.push_str(",\"rssBytes\":");
    out.push_str(&latest.rss_bytes.to_string());
    out.push_str(",\"cpuAvgPctX100\":");
    out.push_str(&latest.cpu_avg_pct_x100.to_string());
    out.push_str(",\"cpuWindowMs\":");
    out.push_str(&latest.window_ms.to_string());
    out.push_str(",\"tasks\":");
    out.push_str(&latest.tasks.to_string());
    out.push_str(",\"timers\":");
    out.push_str(&latest.timers.to_string());
    out.push('}');
    Ok(out)
}

/// `bitty.debug/getFrameStats`: latest rendering health snapshot.
///
/// Requires `debug.inspect`. Returns numeric aggregates plus the bounded
/// renderer backend label (`gpuBytes` present only when the renderer
/// exposes it); every response carries `"trust":"untrusted-observation"`.
/// An empty store returns `"sample":"none"` with no numeric fields.
fn handle_get_frame_stats(
    context: &ServeContext,
    _request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::DebugInspect;
    require_profiling_scope(&context.granted, DebugInspect)?;
    let guard = live_profiling_store().lock().map_err(|_| {
        HandlerError::new(
            "transport",
            "Unavailable",
            "profiling store unavailable".to_string(),
        )
    })?;
    let mut out = String::with_capacity(256);
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"snapshot\":\"frame-stats\"");
    let Some(latest) = guard.frame.back() else {
        out.push_str(",\"sample\":\"none\",\"trust\":\"untrusted-observation\"}");
        return Ok(out);
    };
    out.push_str(",\"sample\":\"latest\",\"seq\":");
    out.push_str(&latest.seq.to_string());
    out.push_str(",\"nowMs\":");
    out.push_str(&latest.now_ms.to_string());
    out.push_str(",\"frameP50Us\":");
    out.push_str(&latest.frame_p50_us.to_string());
    out.push_str(",\"frameP99Us\":");
    out.push_str(&latest.frame_p99_us.to_string());
    out.push_str(",\"presentedFps\":");
    out.push_str(&latest.presented_fps.to_string());
    out.push_str(",\"missedPresents\":");
    out.push_str(&latest.missed_presents.to_string());
    if let Some(gpu) = latest.gpu_bytes {
        out.push_str(",\"gpuBytes\":");
        out.push_str(&gpu.to_string());
    }
    out.push_str(",\"backend\":\"");
    json_escape_into(&mut out, &latest.backend);
    out.push_str("\",\"trust\":\"untrusted-observation\"}");
    Ok(out)
}

/// Drain retained process samples newer than `after_seq` (non-destructive
/// cursor read; at most `max` records and [`MAX_PROF_DRAIN_BYTES`] encoded
/// bytes). Returns the encoded array body, whether retained records were
/// omitted (`truncated`, i.e. re-poll with a newer cursor to continue), and
/// the current head sequence plus cumulative drops for the header.
/// Historical loss (dropped before the cursor window) surfaces via `dropped`
/// and the head `seq`, never via `truncated`: `truncated` is strictly "this
/// response omits records that are still retained".
fn drain_process_samples(after_seq: u64, max: usize) -> (String, bool, u64, u64) {
    let Ok(guard) = live_profiling_store().lock() else {
        return (String::new(), false, 0, 0);
    };
    let mut body = String::with_capacity(1024);
    let mut truncated = false;
    for (emitted, sample) in guard
        .process
        .iter()
        .filter(|s| s.seq > after_seq)
        .enumerate()
    {
        if emitted >= max {
            truncated = true;
            break;
        }
        let mut encoded = String::with_capacity(160);
        encode_process_sample(&mut encoded, sample);
        let add = encoded.len() + usize::from(emitted > 0);
        if body.len() + add > MAX_PROF_DRAIN_BYTES {
            truncated = true;
            break;
        }
        if emitted > 0 {
            body.push(',');
        }
        body.push_str(&encoded);
    }
    (
        body,
        truncated,
        store_head_seq_process(&guard),
        guard.process_dropped,
    )
}

/// Drain retained frame samples newer than `after_seq` (same cursor
/// discipline as [`drain_process_samples`]).
fn drain_frame_samples(after_seq: u64, max: usize) -> (String, bool, u64, u64) {
    let Ok(guard) = live_profiling_store().lock() else {
        return (String::new(), false, 0, 0);
    };
    let mut body = String::with_capacity(1024);
    let mut truncated = false;
    for (emitted, sample) in guard.frame.iter().filter(|s| s.seq > after_seq).enumerate() {
        if emitted >= max {
            truncated = true;
            break;
        }
        let mut encoded = String::with_capacity(192);
        encode_frame_sample(&mut encoded, sample);
        let add = encoded.len() + usize::from(emitted > 0);
        if body.len() + add > MAX_PROF_DRAIN_BYTES {
            truncated = true;
            break;
        }
        if emitted > 0 {
            body.push(',');
        }
        body.push_str(&encoded);
    }
    (
        body,
        truncated,
        store_head_seq_frame(&guard),
        guard.frame_dropped,
    )
}

/// Head process sequence (0 when the ring is empty).
fn store_head_seq_process(store: &ProfilingStore) -> u64 {
    store.process.back().map_or(0, |s| s.seq)
}

/// Head frame sequence (0 when the ring is empty).
fn store_head_seq_frame(store: &ProfilingStore) -> u64 {
    store.frame.back().map_or(0, |s| s.seq)
}

/// `bitty.debug/streamProcessStats`: sampled process-stats subscription.
///
/// Requires `debug.trace`. Params (all optional, fail-closed on malformed):
/// `{ "intervalMs": 100..=600000 (default 1000), "maxSamples": 1..=32
/// (default 32), "afterSeq": <cursor> (default 0) }`. The declared cadence
/// is client-paced polling; the server enforces the 100 ms floor on the
/// declaration and bounds every drain (32 records or 8 KiB, sequence plus
/// drop-count headers). Reads never consume: producers never block and
/// consumers converge to latest state.
fn handle_stream_process_stats(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::DebugTrace;
    require_profiling_scope(&context.granted, DebugTrace)?;
    let interval_ms = parse_interval_ms_param(request.params_raw.as_deref())?;
    let max_samples = parse_optional_uint_param(
        request.params_raw.as_deref(),
        "maxSamples",
        MAX_PROF_SAMPLES,
        MAX_PROF_SAMPLES,
    )?;
    let after_seq = parse_after_seq_param(request.params_raw.as_deref())?;
    let (body, truncated, head_seq, dropped) = drain_process_samples(after_seq, max_samples);
    let mut out = String::with_capacity(1024 + body.len().min(MAX_PROF_DRAIN_BYTES));
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"subscription\":{\"family\":\"process-stats\",\"intervalMs\":");
    out.push_str(&interval_ms.to_string());
    out.push_str("},\"samples\":[");
    out.push_str(&body);
    out.push_str("],\"seq\":");
    out.push_str(&head_seq.to_string());
    out.push_str(",\"dropped\":");
    out.push_str(&dropped.to_string());
    out.push_str(",\"truncated\":");
    out.push_str(if truncated { "true" } else { "false" });
    out.push('}');
    Ok(out)
}

/// `bitty.debug/streamFrameStats`: sampled frame-stats subscription.
///
/// Requires `debug.trace`. Same cursor discipline as
/// [`handle_stream_process_stats`]; every response carries
/// `"trust":"untrusted-observation"` and zero terminal bytes.
fn handle_stream_frame_stats(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    use crate::scope::Scope::DebugTrace;
    require_profiling_scope(&context.granted, DebugTrace)?;
    let interval_ms = parse_interval_ms_param(request.params_raw.as_deref())?;
    let max_samples = parse_optional_uint_param(
        request.params_raw.as_deref(),
        "maxSamples",
        MAX_PROF_SAMPLES,
        MAX_PROF_SAMPLES,
    )?;
    let after_seq = parse_after_seq_param(request.params_raw.as_deref())?;
    let (body, truncated, head_seq, dropped) = drain_frame_samples(after_seq, max_samples);
    let mut out = String::with_capacity(1024 + body.len().min(MAX_PROF_DRAIN_BYTES));
    out.push_str("{\"version\":\"");
    out.push_str(DEVTOOLS_PROTOCOL_VERSION);
    out.push_str("\",\"subscription\":{\"family\":\"frame-stats\",\"intervalMs\":");
    out.push_str(&interval_ms.to_string());
    out.push_str("},\"samples\":[");
    out.push_str(&body);
    out.push_str("],\"seq\":");
    out.push_str(&head_seq.to_string());
    out.push_str(",\"dropped\":");
    out.push_str(&dropped.to_string());
    out.push_str(",\"truncated\":");
    out.push_str(if truncated { "true" } else { "false" });
    out.push_str(",\"trust\":\"untrusted-observation\"}");
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
///
/// CR-IPC-01 (fail-closed): uses `symlink_metadata` and rejects symlinks
/// outright. `std::fs::metadata` follows symlinks, which would let a second
/// local user redirect the socket directory to an attacker-controlled target
/// on a multi-user machine and have its mode/owner attested as ours.
#[cfg(unix)]
fn attestation_for(parent: &std::path::Path) -> Result<DirAttestation, IpcError> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::symlink_metadata(parent).map_err(|err| IpcError::Unavailable {
        reason: format!("cannot stat socket directory {}: {err}", parent.display()),
    })?;
    if meta.file_type().is_symlink() {
        return Err(IpcError::Unauthenticated {
            reason: format!(
                "socket directory {} is a symlink (refusing to serve)",
                parent.display()
            ),
        });
    }
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

/// Fail-closed pre-check: reject a symlinked socket path before chmod.
///
/// `set_permissions` follows symlinks, so without this guard a symlinked
/// socket path would chmod an attacker-chosen target. Missing paths map to
/// `Unavailable` (filesystem failure); symlinks map to `Unauthenticated`.
#[cfg(unix)]
fn reject_socket_symlink(socket_path: &str) -> Result<(), IpcError> {
    let pre = std::fs::symlink_metadata(socket_path).map_err(|err| IpcError::Unavailable {
        reason: format!("cannot stat bound socket: {err}"),
    })?;
    if pre.file_type().is_symlink() {
        return Err(IpcError::Unauthenticated {
            reason: "bound socket is a symlink (refusing to serve)".into(),
        });
    }
    Ok(())
}

/// Attest a freshly bound socket: enforce `0600` and verify endpoint.
///
/// CR-IPC-01 (fail-closed): the socket path is `symlink_metadata`-checked
/// and symlink-rejected both before `set_permissions` (so a symlink can never
/// redirect the `0600` chmod onto another owner's file) and after (so a
/// swapped-in symlink is never attested as the bound socket).
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

    reject_socket_symlink(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(SOCKET_MODE)).map_err(
        |err| IpcError::Unavailable {
            reason: format!("cannot set socket mode 0600: {err}"),
        },
    )?;
    let meta = std::fs::symlink_metadata(socket_path).map_err(|err| IpcError::Unavailable {
        reason: format!("cannot stat bound socket: {err}"),
    })?;
    if meta.file_type().is_symlink() {
        return Err(IpcError::Unauthenticated {
            reason: "bound socket is a symlink (refusing to serve)".into(),
        });
    }
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

    // ── windows pipe naming (CTX-0196) ──────────────────────────────────

    #[test]
    fn windows_pipe_name_maps_instance_verbatim() {
        assert_eq!(
            windows_pipe_name("default"),
            r"\\.\pipe\bitty-default".to_string()
        );
        assert_eq!(
            windows_pipe_name("my-inst_1"),
            r"\\.\pipe\bitty-my-inst_1".to_string()
        );
    }

    #[test]
    fn windows_pipe_name_roundtrips_through_parser() {
        for id in ["default", "a", "my-inst_1", "ABC-9_z"] {
            let pipe = windows_pipe_name(id);
            let file = pipe.rsplit('\\').next().unwrap();
            assert_eq!(windows_instance_from_pipe_name(file).as_deref(), Some(id));
        }
    }

    #[test]
    fn windows_pipe_parser_skips_foreign_and_malformed() {
        assert_eq!(windows_instance_from_pipe_name("bitty-"), None);
        assert_eq!(windows_instance_from_pipe_name("other-pipe"), None);
        assert_eq!(windows_instance_from_pipe_name(""), None);
        assert_eq!(windows_instance_from_pipe_name("bitty-has space"), None);
        assert_eq!(windows_instance_from_pipe_name("bitty-bad/id"), None);
        assert_eq!(windows_instance_from_pipe_name("BITTY-default"), None);
        let long = format!("bitty-{}", "a".repeat(MAX_INSTANCE_ID_LEN + 1));
        assert_eq!(windows_instance_from_pipe_name(&long), None);
        // Pipe names never carry the socket suffix.
        assert_eq!(windows_instance_from_pipe_name("bitty-default.sock"), None);
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
        // (getGridText, getInputRing, getModifiers, getFocus) plus CTX-0171
        // control (listWindows, listViews, listTerminals, spawnTerminal,
        // closeTerminal, sendInput, getTerminalText, splitView, focusView,
        // reloadConfig) plus CTX-0257 workspace entry (listWorkspaces,
        // createWorkspace, closeWorkspace, focusWorkspace) plus CTX-0259 move
        // (moveWorkspace) plus CTX-0188
        // automation (synthesizeInput,
        // captureFrame) plus CTX-0244 digest (frameHash) plus CTX-0189
        // profiling (getProcessStats, getFrameStats, streamProcessStats,
        // streamFrameStats).
        assert_eq!(dispatcher.method_count(), 28);
        assert!(dispatcher.contains("bitty.debug/getGridText"));
        assert!(dispatcher.contains("bitty.debug/getInputRing"));
        assert!(dispatcher.contains("bitty.debug/getModifiers"));
        assert!(dispatcher.contains("bitty.debug/getFocus"));
        assert!(dispatcher.contains(METHOD_SYNTHESIZE_INPUT));
        assert!(dispatcher.contains(METHOD_CAPTURE_FRAME));
        assert!(dispatcher.contains(METHOD_FRAME_HASH));
        assert!(dispatcher.contains(METHOD_GET_PROCESS_STATS));
        assert!(dispatcher.contains(METHOD_GET_FRAME_STATS));
        assert!(dispatcher.contains(METHOD_STREAM_PROCESS_STATS));
        assert!(dispatcher.contains(METHOD_STREAM_FRAME_STATS));
        for method in crate::ctl::all_control_methods() {
            assert!(
                dispatcher.contains(method),
                "control method {method} must be registered"
            );
        }
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

    /// CR-IPC-01: a symlinked socket directory must fail closed even when the
    /// link target is a well-formed `0700` directory owned by us.
    #[cfg(unix)]
    #[test]
    fn prepare_socket_dir_rejects_symlinked_leaf() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "bitty-ctx0203-{}-{}",
            std::process::id(),
            "symlink-leaf"
        ));
        let target = base.join("real");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("bitty");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let socket_str = link.join("t.sock").to_str().unwrap().to_string();
        let err = prepare_socket_dir(&socket_str).unwrap_err();
        assert!(
            matches!(err, IpcError::Unauthenticated { .. }),
            "symlinked leaf must fail closed, got: {err:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// CR-IPC-01: a symlinked socket path must fail closed before any chmod
    /// is applied to its target.
    #[cfg(unix)]
    #[test]
    fn attest_bound_socket_rejects_symlink() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let base = std::env::temp_dir().join(format!(
            "bitty-ctx0203-{}-{}",
            std::process::id(),
            "symlink-sock"
        ));
        let dir_path = base.join("bitty");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_str = dir_path.join("t.sock").to_str().unwrap().to_string();
        let attestation = prepare_socket_dir(&socket_str).unwrap();

        let real = dir_path.join("real.sock");
        std::fs::write(&real, b"x").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link_path = dir_path.join("link.sock");
        std::os::unix::fs::symlink(&real, &link_path).unwrap();
        let link_str = link_path.to_str().unwrap();

        let err = attest_bound_socket(link_str, &attestation).unwrap_err();
        assert!(
            matches!(err, IpcError::Unauthenticated { .. }),
            "symlinked socket must fail closed, got: {err:?}"
        );
        // Fail-closed before chmod: the link target keeps its pre-existing mode.
        let target_mode = std::fs::metadata(&real).unwrap().mode() & 0o777;
        assert_eq!(target_mode, 0o644);
        std::fs::remove_dir_all(&base).ok();
    }

    // ── introspection (CTX-0159, read-only, bounded) ───────────────────────
    //
    // The global live stores (`live_grid_store`, `live_input_store`, ...) are
    // shared across tests in this binary. Rust runs tests in parallel
    // threads, so one test's `clear_introspection_for_tests` can wipe another
    // test's published snapshot mid-sequence (CTX-0179 CI flake: 134 passed /
    // 1 failed on `introspection_round_trip_all_methods_sequential`). Every
    // test below that touches the globals holds
    // `lock_introspection_for_test` for its whole publish→assert sequence;
    // tests that only exercise pure parsers need no guard. Test-only:
    // production paths never take this lock (lock order is always
    // serial-guard → store locks, never the reverse, so no deadlock).

    /// Serial guard for the process-global live introspection stores.
    ///
    /// std-only on purpose (`serial_test` would add a dev-dependency for
    /// what is ten lines): same `OnceLock<Mutex<...>>` idiom as the stores
    /// themselves. Poison-safe so a panicking holder cannot cascade-fail the
    /// rest of the suite.
    fn introspection_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Hold for the whole body of any test that publishes, reads, or clears
    /// the global introspection stores.
    fn lock_introspection_for_test() -> std::sync::MutexGuard<'static, ()> {
        introspection_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        let _introspection_guard = lock_introspection_for_test();
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
        let _introspection_guard = lock_introspection_for_test();
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
        let _introspection_guard = lock_introspection_for_test();
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

    // ── test automation (CTX-0188, Amendment A1 candidate) ──────────────────
    //
    // Bearer matrix, scope intersection, bounds, redaction, and rate shedding
    // for `synthesizeInput` + `captureFrame`. Every test holds the serial
    // guard (automation shares the input/grid stores) and clears automation
    // plus introspection before and after.

    fn automation_scopes_synthesize() -> crate::scope::ScopeSet {
        let mut set = crate::scope::ScopeSet::new();
        set.insert(crate::scope::Scope::DebugControl);
        set.insert(crate::scope::Scope::TerminalInput);
        set
    }

    fn automation_scopes_capture() -> crate::scope::ScopeSet {
        let mut set = crate::scope::ScopeSet::new();
        set.insert(crate::scope::Scope::DebugTrace);
        set.insert(crate::scope::Scope::TerminalInspect);
        set
    }

    fn automation_context(
        server: &ServerInfo,
        granted: crate::scope::ScopeSet,
        session: &str,
        now_ms: u64,
    ) -> ServeContext {
        let mut ctx = ServeContext::with_granted_session(server, granted, session);
        ctx.uptime_ms = now_ms;
        ctx
    }

    fn synth_envelope(id: u64, params: &str) -> Vec<u8> {
        format!(
            "{{\"id\":{id},\"method\":\"{METHOD_SYNTHESIZE_INPUT}\",\"version\":\"1.0\",\"params\":{params}}}"
        )
        .into_bytes()
    }

    fn capture_envelope(id: u64, params: &str) -> Vec<u8> {
        format!(
            "{{\"id\":{id},\"method\":\"{METHOD_CAPTURE_FRAME}\",\"version\":\"1.0\",\"params\":{params}}}"
        )
        .into_bytes()
    }

    // ── frameHash digest (CTX-0244) ─────────────────────────────────────────
    //
    // Digest equality vs local computation, full denial matrix, TTL cap,
    // 2/s rate ceiling, fail-closed publish validation, bounded audit with
    // served-digest content, and no-bypass isolation in both family
    // directions. Every test holds the serial guard and clears automation
    // plus introspection (incl. the RGBA store) before and after. All
    // content is synthetic fixture bytes — never secrets (P0-AC-026
    // harness rule).

    fn digest_envelope(id: u64, params: &str) -> Vec<u8> {
        format!(
            "{{\"id\":{id},\"method\":\"{METHOD_FRAME_HASH}\",\"version\":\"1.0\",\"params\":{params}}}"
        )
        .into_bytes()
    }

    fn digest_context(
        server: &ServerInfo,
        granted: crate::scope::ScopeSet,
        session: &str,
        now_ms: u64,
    ) -> ServeContext {
        // Attested like the production accept boundary
        // (`transport_attested_peer` + `attest_local_peer`): same-process
        // in-process dispatch is local by construction.
        let mut ctx = automation_context(server, granted, session, now_ms);
        ctx.attest_local_peer();
        ctx
    }

    /// Deterministic synthetic RGBA fixture (never secrets): a gradient
    /// over `w*h*4` bytes with an ASCII marker row to prove no pixel bytes
    /// reach the response.
    fn fixture_rgba(width: u32, height: u32, seed: u8) -> Vec<u8> {
        let len = width as usize * height as usize * 4;
        let mut rgba = Vec::with_capacity(len);
        for i in 0..len {
            rgba.push(
                (i as u8)
                    .wrapping_add(seed)
                    .wrapping_mul(31)
                    .wrapping_add(7),
            );
        }
        // ASCII marker the response must never contain (uninvertibility
        // spot-check, not a proof — the proof is the 32-byte digest).
        let marker = b"FRAMEHASH-MARKER-NEVER-ON-WIRE";
        let at = len.min(256);
        for (i, b) in marker.iter().enumerate() {
            if at + i < len {
                rgba[at + i] = *b;
            }
        }
        rgba
    }

    fn response_text(outcome: &HandleOutcome) -> String {
        String::from_utf8(outcome.response.clone()).unwrap()
    }

    #[test]
    fn frame_hash_digest_equals_local_computation_across_geometries() {
        use crate::frame_digest::{FRAME_DIGEST_ALGO, frame_digest_hex};
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        // Three geometries (tiny, odd-sized, and multi-kilopixel) with
        // distinct seeds and frame sequences.
        for (id, (w, h, seq, seed)) in [(80u32, 60u32, 9u64, 1u8), (17, 5, 41, 2), (320, 200, 7, 3)]
            .into_iter()
            .enumerate()
        {
            let rgba = fixture_rgba(w, h, seed);
            publish_frame_rgba(w, h, seq, rgba.clone());
            publish_grid_text(vec!["synthetic".to_string()], 0, 0, true, seq, 80, 24);
            let tok = issue_automation_bearer_with_ttl(
                "digest-eq",
                "t:1",
                AutomationFamily::FrameDigest,
                0,
                60_000,
            )
            .unwrap();
            let ctx = digest_context(&server, automation_scopes_capture(), "digest-eq", 0);
            let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
            let outcome =
                handle_envelope(&digest_envelope(id as u64 + 1, &params), &dispatcher, &ctx);
            assert!(!outcome.was_error, "geometry {w}x{h} must verify");
            let text = response_text(&outcome);
            let expect = frame_digest_hex(w, h, seq, &rgba);
            assert!(text.contains("\"snapshot\":\"frameHash\""), "got: {text}");
            assert!(
                text.contains(&format!("\"algo\":\"{FRAME_DIGEST_ALGO}\"")),
                "got: {text}"
            );
            assert!(
                text.contains(&format!("\"digest\":\"{expect}\"")),
                "digest mismatch at {w}x{h}: {text}"
            );
            assert!(text.contains(&format!("\"frameSeq\":{seq}")), "got: {text}");
            assert!(
                text.contains("\"trust\":\"untrusted-observation\""),
                "got: {text}"
            );
            assert!(text.len() < 512, "digest response must stay tiny: {text}");
            assert!(
                !text.contains("FRAMEHASH-MARKER-NEVER-ON-WIRE"),
                "pixel bytes reached the wire: {text}"
            );
            clear_automation_for_tests();
        }
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_auth_matrix_denies_everything_unauthorized() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        publish_frame_rgba(8, 8, 3, fixture_rgba(8, 8, 9));
        let digest_tok =
            issue_automation_bearer_with_ttl("m", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
                .unwrap();
        let capture_tok =
            issue_automation_bearer("m", "t:1", AutomationFamily::Capture, 0).unwrap();

        // (label, scopes, session, terminal-in-params, token, now, attest)
        let full = automation_scopes_capture();
        let mut no_trace = automation_scopes_capture();
        no_trace.remove(crate::scope::Scope::DebugTrace);
        let mut no_inspect = automation_scopes_capture();
        no_inspect.remove(crate::scope::Scope::TerminalInspect);
        let cases: Vec<(&str, crate::scope::ScopeSet, &str, &str, String, u64, bool)> = vec![
            (
                "no-bearer",
                full.clone(),
                "m",
                "t:1",
                String::new(),
                0,
                true,
            ),
            (
                "wrong-family-capture-token",
                full.clone(),
                "m",
                "t:1",
                capture_tok.clone(),
                0,
                true,
            ),
            (
                "wrong-terminal",
                full.clone(),
                "m",
                "t:2",
                digest_tok.clone(),
                0,
                true,
            ),
            (
                "wrong-session",
                full.clone(),
                "other",
                "t:1",
                digest_tok.clone(),
                0,
                true,
            ),
            (
                "missing-debug-trace",
                no_trace,
                "m",
                "t:1",
                digest_tok.clone(),
                0,
                true,
            ),
            (
                "missing-terminal-inspect",
                no_inspect,
                "m",
                "t:1",
                digest_tok.clone(),
                0,
                true,
            ),
            (
                "unattested-transport",
                full.clone(),
                "m",
                "t:1",
                digest_tok.clone(),
                0,
                false,
            ),
            (
                "forged-token-full-scopes",
                crate::scope::ScopeSet::all(),
                "m",
                "t:1",
                "forged-token".to_string(),
                0,
                true,
            ),
        ];
        for (label, scopes, session, term, token, now, attest) in cases {
            let mut ctx = automation_context(&server, scopes, session, now);
            if attest {
                ctx.attest_local_peer();
            }
            let params = if token.is_empty() {
                format!("{{\"terminalId\":\"{term}\"}}")
            } else {
                format!("{{\"terminalId\":\"{term}\",\"bearer\":\"{token}\"}}")
            };
            let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
            assert!(outcome.was_error, "{label} must fail");
            assert!(
                response_text(&outcome).contains("ScopeDenied"),
                "{label}: bearer-vs-scope failures must share the ScopeDenied shape, got: {}",
                response_text(&outcome)
            );
        }
        // Malformed shape fails closed without attribution (no audit needed).
        let ctx = digest_context(&server, automation_scopes_capture(), "m", 0);
        let outcome = handle_envelope(&digest_envelope(2, "{}"), &dispatcher, &ctx);
        assert!(response_text(&outcome).contains("InvalidParams"));
        let outcome = handle_envelope(
            &digest_envelope(3, r#"{"terminalId":"t:*"}"#),
            &dispatcher,
            &ctx,
        );
        assert!(response_text(&outcome).contains("InvalidParams"));
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_ttl_capped_at_two_minutes_and_expiry_revokes() {
        use crate::frame_digest::FRAME_DIGEST_TTL_MS;
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        assert_eq!(FRAME_DIGEST_TTL_MS, 120_000);
        // The default 10-minute minter refuses digest grants fail-closed:
        // a digest TTL must be explicit.
        assert!(issue_automation_bearer("ttl", "t:1", AutomationFamily::FrameDigest, 0).is_err());
        // Zero and over-cap TTLs fail closed.
        assert!(
            issue_automation_bearer_with_ttl("ttl", "t:1", AutomationFamily::FrameDigest, 0, 0)
                .is_err()
        );
        assert!(
            issue_automation_bearer_with_ttl(
                "ttl",
                "t:1",
                AutomationFamily::FrameDigest,
                0,
                FRAME_DIGEST_TTL_MS + 1
            )
            .is_err()
        );
        // Cap edge issues; sibling families still enjoy the 10-minute cap.
        let tok = issue_automation_bearer_with_ttl(
            "ttl",
            "t:1",
            AutomationFamily::FrameDigest,
            500,
            FRAME_DIGEST_TTL_MS,
        )
        .unwrap();
        assert!(
            issue_automation_bearer_with_ttl(
                "ttl",
                "t:1",
                AutomationFamily::Synthesize,
                0,
                AUTOMATION_BEARER_TTL_MS
            )
            .is_ok()
        );
        // Grant valid at issuance, denied exactly at issue+TTL
        // (virtual clock: no sleeps, no wall-clock).
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        publish_frame_rgba(4, 4, 1, fixture_rgba(4, 4, 5));
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
        let ctx = digest_context(&server, automation_scopes_capture(), "ttl", 500);
        let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "grant must verify at issuance");
        let ctx = digest_context(
            &server,
            automation_scopes_capture(),
            "ttl",
            500 + FRAME_DIGEST_TTL_MS,
        );
        let outcome = handle_envelope(&digest_envelope(2, &params), &dispatcher, &ctx);
        assert!(outcome.was_error);
        assert!(
            response_text(&outcome).contains("ScopeDenied"),
            "expired digest grant must deny: {}",
            response_text(&outcome)
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_rate_sheds_third_digest_per_second() {
        use crate::frame_digest::MAX_FRAME_DIGEST_PER_SEC;
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        assert_eq!(MAX_FRAME_DIGEST_PER_SEC, 2);
        publish_frame_rgba(4, 4, 1, fixture_rgba(4, 4, 5));
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok =
            issue_automation_bearer_with_ttl("rl", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
                .unwrap();
        let ctx = digest_context(&server, automation_scopes_capture(), "rl", 0);
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
        for id in 1..=MAX_FRAME_DIGEST_PER_SEC as u64 {
            let outcome = handle_envelope(&digest_envelope(id, &params), &dispatcher, &ctx);
            assert!(!outcome.was_error, "digest {id} must pass under ceiling");
        }
        let outcome = handle_envelope(
            &digest_envelope(MAX_FRAME_DIGEST_PER_SEC as u64 + 1, &params),
            &dispatcher,
            &ctx,
        );
        assert!(outcome.was_error);
        assert!(
            response_text(&outcome).contains("RateLimited"),
            "third digest in one window must shed: {}",
            response_text(&outcome)
        );
        // Window slides: one second later the ceiling admits again.
        let ctx = digest_context(&server, automation_scopes_capture(), "rl", 1_000);
        let outcome = handle_envelope(&digest_envelope(9, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "window must slide after 1 s");
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_unavailable_without_presented_frame_and_rejects_bad_publish() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok =
            issue_automation_bearer_with_ttl("np", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
                .unwrap();
        // Authorized-but-unavailable calls still consume the rate budget
        // (authorize runs first), so each probe advances the virtual clock.
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
        // Never a hash of nothing: empty store reads as indeterminate.
        let ctx = digest_context(&server, automation_scopes_capture(), "np", 0);
        let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
        assert!(outcome.was_error);
        assert!(
            response_text(&outcome).contains("Unavailable"),
            "got: {}",
            response_text(&outcome)
        );
        // Fail-closed publish validation: zero extents, length mismatch,
        // and over-cap frames never become digestable.
        publish_frame_rgba(0, 8, 1, vec![0u8; 32]);
        publish_frame_rgba(8, 8, 1, vec![0u8; 8 * 8 * 4 - 1]);
        publish_frame_rgba(8, 8, 1, vec![0u8; 8 * 8 * 4 + 1]);
        publish_frame_rgba(u32::MAX, u32::MAX, 1, vec![0u8; 16]);
        let ctx = digest_context(&server, automation_scopes_capture(), "np", 1_000);
        let outcome = handle_envelope(&digest_envelope(2, &params), &dispatcher, &ctx);
        assert!(
            response_text(&outcome).contains("Unavailable"),
            "bad publishes must not become digestable: {}",
            response_text(&outcome)
        );
        // A valid publish after bad ones still verifies (drop, not poison).
        publish_frame_rgba(8, 8, 2, fixture_rgba(8, 8, 1));
        let ctx = digest_context(&server, automation_scopes_capture(), "np", 2_000);
        let outcome = handle_envelope(&digest_envelope(3, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "valid publish must verify");
        assert!(
            response_text(&outcome).contains("\"frameSeq\":2"),
            "got: {}",
            response_text(&outcome)
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_audit_covers_granted_and_denied_and_stays_bounded() {
        use crate::frame_digest::frame_digest_hex;
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let rgba = fixture_rgba(8, 8, 4);
        publish_frame_rgba(8, 8, 55, rgba.clone());
        let tok = issue_automation_bearer_with_ttl(
            "au",
            "t:1",
            AutomationFamily::FrameDigest,
            0,
            120_000,
        )
        .unwrap();
        // Granted call appends a digest entry carrying the served digest.
        let before = frame_audit_len_for_tests();
        let ctx = digest_context(&server, automation_scopes_capture(), "au", 0);
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error);
        let text = response_text(&outcome);
        let served = frame_digest_hex(8, 8, 55, &rgba);
        assert!(text.contains(&served));
        assert_eq!(frame_audit_len_for_tests(), before + 1);
        let snap = frame_audit_snapshot_for_tests();
        let entry = snap.last().unwrap();
        assert_eq!(entry.format, "digest");
        assert_eq!(entry.session_id, "au");
        assert_eq!(entry.terminal_id, "t:1");
        assert_eq!(entry.frame_seq, 55);
        assert_eq!(entry.digest_hex, served);
        // Denied calls append too (attributable terminal): wrong-terminal
        // denials are unbounded by rate (auth fails first), so 65 of them
        // prove the 64-entry drop-oldest bound deterministically.
        for id in 2..=66u64 {
            let bad = format!("{{\"terminalId\":\"t:9\",\"bearer\":\"{tok}\"}}");
            let outcome = handle_envelope(&digest_envelope(id, &bad), &dispatcher, &ctx);
            assert!(outcome.was_error);
        }
        assert_eq!(frame_audit_len_for_tests(), MAX_AUTOMATION_BEARERS);
        let snap = frame_audit_snapshot_for_tests();
        assert!(snap.iter().all(|e| e.format == "digest"));
        assert!(
            snap.iter().all(|e| e.digest_hex.is_empty()),
            "denied entries carry no digest"
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_hash_family_isolation_holds_in_both_directions() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        publish_frame_rgba(8, 8, 1, fixture_rgba(8, 8, 6));
        publish_grid_text(vec!["ok".to_string()], 0, 0, true, 1, 80, 24);
        let digest_tok = issue_automation_bearer_with_ttl(
            "iso",
            "t:1",
            AutomationFamily::FrameDigest,
            0,
            60_000,
        )
        .unwrap();
        let capture_tok =
            issue_automation_bearer("iso", "t:1", AutomationFamily::Capture, 0).unwrap();
        let ctx = digest_context(&server, automation_scopes_capture(), "iso", 0);
        // Capture bearer on frameHash: denied (never widened).
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{capture_tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
        assert!(response_text(&outcome).contains("ScopeDenied"));
        // Digest bearer on captureFrame: denied (never widened).
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\",\"format\":\"semantic\"}}"
        );
        let outcome = handle_envelope(&capture_envelope(2, &params), &dispatcher, &ctx);
        assert!(
            response_text(&outcome).contains("ScopeDenied"),
            "digest bearer must not capture: {}",
            response_text(&outcome)
        );
        // Digest bearer on frameHash: verifies (control case).
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(3, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error);
        // No-bypass: even every scope granted, a forged digest token denies,
        // and revocation takes effect immediately.
        let all = digest_context(&server, crate::scope::ScopeSet::all(), "iso", 0);
        let params = r#"{"terminalId":"t:1","bearer":"forged-digest-token"}"#;
        let outcome = handle_envelope(&digest_envelope(4, params), &dispatcher, &all);
        assert!(response_text(&outcome).contains("ScopeDenied"));
        assert!(revoke_automation_bearer(&digest_tok));
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(5, &params), &dispatcher, &ctx);
        assert!(response_text(&outcome).contains("ScopeDenied"));
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn frame_digest_publish_gate_arms_only_with_live_grant() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        assert!(!frame_digest_publish_wanted());
        let synth =
            issue_automation_bearer("gate", "t:1", AutomationFamily::Synthesize, 0).unwrap();
        let cap = issue_automation_bearer("gate", "t:1", AutomationFamily::Capture, 0).unwrap();
        // Other families never arm the RGBA publish path.
        assert!(!frame_digest_publish_wanted());
        let digest = issue_automation_bearer_with_ttl(
            "gate",
            "t:1",
            AutomationFamily::FrameDigest,
            0,
            60_000,
        )
        .unwrap();
        assert!(frame_digest_publish_wanted());
        assert!(revoke_automation_bearer(&digest));
        assert!(!frame_digest_publish_wanted());
        assert!(revoke_automation_bearer(&synth));
        assert!(revoke_automation_bearer(&cap));
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_unscoped_synthesize_is_scope_denied_no_partial_state() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let seq_before = synthetic_seq_for_tests();
        // Empty scopes + no bearer: fail fast ScopeDenied.
        let ctx = automation_context(&server, crate::scope::ScopeSet::new(), "s1", 1000);
        let outcome = handle_envelope(
            &synth_envelope(
                1,
                r#"{"terminalId":"t:1","bearer":"nope","originLabel":"harness","events":[{"type":"key","key":"a"}]}"#,
            ),
            &dispatcher,
            &ctx,
        );
        assert!(outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("ScopeDenied"), "got: {text}");
        assert_eq!(synthetic_seq_for_tests(), seq_before);
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_bearer_matrix_fails_closed() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let synth_scopes = automation_scopes_synthesize();
        let capture_scopes = automation_scopes_capture();
        // Issue valid bearers at t=1000.
        let synth_tok =
            issue_automation_bearer("sess-a", "t:1", AutomationFamily::Synthesize, 1000).unwrap();
        let capture_tok =
            issue_automation_bearer("sess-a", "t:1", AutomationFamily::Capture, 1000).unwrap();
        // Absent bearer.
        let ctx = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
        let outcome = handle_envelope(
            &synth_envelope(
                1,
                r#"{"terminalId":"t:1","originLabel":"h","events":[{"type":"key","key":"a"}]}"#,
            ),
            &dispatcher,
            &ctx,
        );
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        assert!(outcome.was_error);
        // Wrong-session bearer.
        let ctx_other = automation_context(&server, synth_scopes.clone(), "sess-b", 1000);
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx_other);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // Wrong-terminal bearer.
        let ctx = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
        let params = format!(
            "{{\"terminalId\":\"t:2\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(3, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // Wrong-family bearer (capture token on synthesize).
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{capture_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(4, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // Expired bearer.
        let mut ctx_expired = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
        ctx_expired.uptime_ms = 1000 + AUTOMATION_BEARER_TTL_MS;
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(5, &params), &dispatcher, &ctx_expired);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("ScopeDenied"), "expired must deny: {text}");
        // Capture wrong-family (synthesize token on capture): re-issue a live
        // synthesize token to prove family mismatch distinctly (the earlier
        // synth token was consumed by the expiry check above).
        publish_grid_text(vec!["hello".to_string()], 0, 5, true, 3, 80, 24);
        let ctx_cap = automation_context(&server, capture_scopes, "sess-a", 1000);
        let live_synth =
            issue_automation_bearer("sess-a", "t:1", AutomationFamily::Synthesize, 1000).unwrap();
        let params2 = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{live_synth}\",\"format\":\"semantic\"}}"
        );
        let outcome = handle_envelope(&capture_envelope(6, &params2), &dispatcher, &ctx_cap);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        let _ = capture_tok;
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_scope_intersection_requires_debug_plus_terminal() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 500).unwrap();
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        // DebugControl alone (missing TerminalInput) denies.
        let mut only_debug = crate::scope::ScopeSet::new();
        only_debug.insert(crate::scope::Scope::DebugControl);
        let ctx = automation_context(&server, only_debug, "s1", 500);
        let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // TerminalInput alone (missing DebugControl) denies.
        let mut only_term = crate::scope::ScopeSet::new();
        only_term.insert(crate::scope::Scope::TerminalInput);
        let ctx = automation_context(&server, only_term, "s1", 500);
        let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // Capture: DebugTrace alone denies, TerminalInspect alone denies.
        let cap_tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 500).unwrap();
        publish_grid_text(vec!["x".to_string()], 0, 1, true, 1, 80, 24);
        let cap_params =
            format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{cap_tok}\",\"format\":\"semantic\"}}");
        let mut only_trace = crate::scope::ScopeSet::new();
        only_trace.insert(crate::scope::Scope::DebugTrace);
        let ctx = automation_context(&server, only_trace, "s1", 500);
        let outcome = handle_envelope(&capture_envelope(3, &cap_params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_synthesize_bounds_fail_closed() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
        let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
        // Zero events.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[]}}"
        );
        let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // 65 events exceeds the 64/call ceiling.
        let many: Vec<String> = (0..65)
            .map(|_| "{\"type\":\"key\",\"key\":\"a\"}".to_string())
            .collect();
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{}]}}",
            many.join(",")
        );
        let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // Wildcard terminal fails closed (exactly one t:N per call).
        let params = format!(
            "{{\"terminalId\":\"*\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(3, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // Unknown event type.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"teleport\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(4, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // Bad mouse button.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"mouse\",\"button\":\"Side\",\"action\":\"click\",\"col\":1,\"row\":1}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(5, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // Zero wheel delta.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"wheel\",\"deltaRows\":0}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(6, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // Paste with NUL.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"paste\",\"text\":\"a\\u0000b\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(7, &params), &dispatcher, &ctx);
        assert!(outcome.was_error);
        // Missing originLabel.
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(8, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_synthesize_success_marks_synthetic_origin() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok =
            issue_automation_bearer("harness", "t:1", AutomationFamily::Synthesize, 2000).unwrap();
        let ctx = automation_context(&server, automation_scopes_synthesize(), "harness", 2000);
        let seq_before = synthetic_seq_for_tests();
        let params = format!(
            concat!(
                "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"e2e-harness\",",
                "\"events\":[{{\"type\":\"key\",\"key\":\"Enter\",\"pressed\":true}},",
                "{{\"type\":\"mouse\",\"button\":\"Left\",\"action\":\"click\",\"col\":10,\"row\":5}},",
                "{{\"type\":\"wheel\",\"deltaRows\":-3}},",
                "{{\"type\":\"paste\",\"text\":\"echo hi\"}}]}}"
            ),
            tok = tok
        );
        let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"accepted\":4"), "got: {text}");
        assert!(text.contains("\"rejected\":0"), "got: {text}");
        assert!(text.contains("\"synthetic\":true"), "got: {text}");
        assert!(synthetic_seq_for_tests() > seq_before);
        // Input-ring observability carries the indelible synthetic marker.
        let ring_ctx = automation_context(
            &server,
            crate::scope::ScopeSet::cli_default(),
            "harness",
            2000,
        );
        let _ = ring_ctx;
        let guard = live_input_store().lock().unwrap();
        assert!(guard.len() >= 4);
        assert!(
            guard
                .iter()
                .all(|e| e.label.contains("[synthetic:e2e-harness]"))
        );
        drop(guard);
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_synthesize_rate_sheds_with_budget_error() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
        let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"load\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        for id in 1..=MAX_SYNTH_CALLS_PER_SEC as u64 {
            let outcome = handle_envelope(&synth_envelope(id, &params), &dispatcher, &ctx);
            assert!(!outcome.was_error, "call {id} must pass under ceiling");
        }
        let outcome = handle_envelope(
            &synth_envelope(MAX_SYNTH_CALLS_PER_SEC as u64 + 1, &params),
            &dispatcher,
            &ctx,
        );
        assert!(outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("RateLimited"), "got: {text}");
        // A different bearer (benign concurrent session) is unaffected.
        let tok2 = issue_automation_bearer("s2", "t:2", AutomationFamily::Synthesize, 0).unwrap();
        let ctx2 = automation_context(&server, automation_scopes_synthesize(), "s2", 0);
        let params2 = format!(
            "{{\"terminalId\":\"t:2\",\"bearer\":\"{tok2}\",\"originLabel\":\"load\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(100, &params2), &dispatcher, &ctx2);
        assert!(!outcome.was_error);
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_capture_semantic_redacts_and_labels_untrusted() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        publish_grid_text(
            vec![
                "$ echo hello".to_string(),
                "hello".to_string(),
                "DB_PASSWORD=hunter2".to_string(),
                "clipboard bytes leak".to_string(),
            ],
            1,
            5,
            true,
            9,
            80,
            24,
        );
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 7000).unwrap();
        let ctx = automation_context(&server, automation_scopes_capture(), "s1", 7000);
        let params =
            format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"semantic\"}}");
        let outcome = handle_envelope(&capture_envelope(1, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"snapshot\":\"frame\""), "got: {text}");
        assert!(
            text.contains("\"trust\":\"untrusted-observation\""),
            "got: {text}"
        );
        assert!(text.contains("hello"), "got: {text}");
        assert!(!text.contains("hunter2"), "secret leaked: {text}");
        assert!(
            !text.contains("clipboard bytes leak"),
            "clipboard leaked: {text}"
        );
        assert!(text.contains(REDACTED_MARKER), "got: {text}");
        assert!(text.contains("\"frameSeq\":9"), "got: {text}");
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_capture_pixels_requires_opt_in_masks_and_audits() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        publish_grid_text(vec!["SECRET=topsecret".to_string()], 0, 1, true, 11, 80, 24);
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 8000).unwrap();
        let ctx = automation_context(&server, automation_scopes_capture(), "s1", 8000);
        // Without explicit opt-in: fail closed.
        let params =
            format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"pixels\"}}");
        let outcome = handle_envelope(&capture_envelope(1, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("InvalidParams")
        );
        // With opt-in: masked record, zero text, audited caller.
        let audit_before = frame_audit_len_for_tests();
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"pixels\",\"explicitOptIn\":true}}"
        );
        let outcome = handle_envelope(&capture_envelope(2, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error);
        let text = String::from_utf8(outcome.response).unwrap();
        assert!(text.contains("\"format\":\"pixels\""), "got: {text}");
        assert!(text.contains("\"masked\":true"), "got: {text}");
        assert!(text.contains("\"audited\":true"), "got: {text}");
        assert!(text.contains("\"caller\":\"s1\""), "got: {text}");
        assert!(!text.contains("topsecret"), "pixels leaked text: {text}");
        assert!(
            !text.contains("\"lines\""),
            "pixels must carry zero text: {text}"
        );
        assert!(frame_audit_len_for_tests() > audit_before);
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_capture_rate_sheds_at_fps_ceiling() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        publish_grid_text(vec!["f".to_string()], 0, 1, true, 1, 80, 24);
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 0).unwrap();
        let ctx = automation_context(&server, automation_scopes_capture(), "s1", 0);
        let params =
            format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"semantic\"}}");
        for id in 1..=MAX_CAPTURE_FPS as u64 {
            let outcome = handle_envelope(&capture_envelope(id, &params), &dispatcher, &ctx);
            assert!(!outcome.was_error, "frame {id} must pass under ceiling");
        }
        let outcome = handle_envelope(
            &capture_envelope(MAX_CAPTURE_FPS as u64 + 1, &params),
            &dispatcher,
            &ctx,
        );
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("RateLimited")
        );
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_bearer_lifecycle_revoke_ttl_cap_and_no_env_issuance() {
        let _guard = lock_introspection_for_test();
        clear_introspection_for_tests();
        clear_automation_for_tests();
        // TTL bounds hold fail-closed.
        assert!(
            issue_automation_bearer_with_ttl("s", "t:1", AutomationFamily::Synthesize, 0, 0)
                .is_err()
        );
        assert!(
            issue_automation_bearer_with_ttl(
                "s",
                "t:1",
                AutomationFamily::Synthesize,
                0,
                AUTOMATION_BEARER_TTL_MS + 1
            )
            .is_err()
        );
        // Revocation takes effect immediately.
        let server = test_server_info();
        let dispatcher = Dispatcher::with_defaults();
        let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
        assert!(revoke_automation_bearer(&tok));
        let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
        let params = format!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
        );
        let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // No-bypass: elevation alone never substitutes for a bearer. Even
        // with every scope granted, a forged token still fails closed, and
        // issuance has no env/config/flag path (code inspection: only
        // `issue_automation_bearer` inserts into the memory-only store).
        let ctx = automation_context(&server, crate::scope::ScopeSet::all(), "s1", 0);
        let params = r#"{"terminalId":"t:1","bearer":"forged-token","originLabel":"h","events":[{"type":"key","key":"a"}]}"#;
        let outcome = handle_envelope(&synth_envelope(2, params), &dispatcher, &ctx);
        assert!(
            String::from_utf8(outcome.response)
                .unwrap()
                .contains("ScopeDenied")
        );
        // Bearers are never persisted: the store is memory-only.
        assert_eq!(automation_bearer_count_for_tests(), 0);
        clear_automation_for_tests();
        clear_introspection_for_tests();
    }

    #[test]
    fn automation_params_two_tier_envelope_bound() {
        // Automation methods admit larger params than the 4 KiB
        // introspection bound (up to 32 KiB); other methods stay capped.
        let big_pad = "p".repeat(MAX_PARAMS_BYTES);
        let auto_payload = format!(
            "{{\"id\":1,\"method\":\"{METHOD_SYNTHESIZE_INPUT}\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{big_pad}\"}}}}"
        );
        assert!(parse_request(auto_payload.as_bytes()).is_ok());
        let plain_payload = format!(
            "{{\"id\":1,\"method\":\"bitty.debug/getGridText\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{big_pad}\"}}}}"
        );
        assert!(parse_request(plain_payload.as_bytes()).is_err());
    }
}
