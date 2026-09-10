use super::*;

use super::handlers::json_escape_into;
use super::json::truncate_chars;

use crate::auth::VerifiedPeer;
#[cfg(unix)]
use crate::auth::{DIR_MODE, SOCKET_MODE};
use crate::error::IpcError;
use crate::frame::{MAX_FRAME_BYTES, encode_frame};
use crate::limits::{RC9_MAX_CONNECTIONS, RateLimiter};
use std::io::{Read, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
