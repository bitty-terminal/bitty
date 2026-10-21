//! `bitty ctl` target resolution and IPC round-trip (split from `ctl.rs`, CTX-0307).

use super::CtlTargeting;
#[cfg(any(unix, windows))]
use bitty_ipc::ctl as ipc_ctl;

// ── socket resolution (client) ────────────────────────────────────────────

/// Resolved IPC target: socket path plus a human label for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Socket path to connect.
    pub socket_path: String,
    /// Instance label (for table output and errors).
    pub instance: String,
}

/// Resolve the socket path per the RFC precedence: explicit `--socket`,
/// then `--instance`, then inherited `BITTY_SOCKET` / `BITTY_INSTANCE_ID`,
/// then the exactly-one-live shortcut, else ambiguity (exit 6).
///
/// Pure over injected env (no process-env access) so tests stay hermetic.
/// `uid` seeds the last-resort base `/run/user/<uid>`; filesystem discovery
/// (listing live sockets) runs only when no explicit target selects one.
#[cfg(unix)]
pub fn resolve_ctl_target(
    targeting: &CtlTargeting,
    env_socket: Option<&str>,
    env_instance: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    uid: u32,
) -> Result<ResolvedTarget, String> {
    // 1. Explicit --socket bypasses all discovery (fails closed on shape;
    //    authentication happens at connect via socket modes + UID).
    if let Some(sock) = targeting.socket.as_deref() {
        if sock.is_empty() || sock.contains('\0') {
            return Err(String::from("bitty ctl: --socket is not a usable path"));
        }
        if sock.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "bitty ctl: --socket path too long for AF_UNIX ({} > {} payload bytes)",
                sock.len(),
                bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES
            ));
        }
        let instance = targeting
            .instance
            .clone()
            .or_else(|| env_instance.filter(|s| !s.is_empty()).map(String::from))
            .unwrap_or_else(|| String::from("default"));
        return Ok(ResolvedTarget {
            socket_path: sock.to_string(),
            instance,
        });
    }
    // 2. Explicit --instance resolves via the discovery file layout.
    if let Some(id) = targeting.instance.as_deref() {
        let path = bitty_ipc::devtools::resolve_socket_path(uid, xdg_runtime_dir, None, Some(id))
            .map_err(|err| format!("bitty ctl: cannot resolve --instance {id:?}: {err}"))?;
        return Ok(ResolvedTarget {
            socket_path: path,
            instance: id.to_string(),
        });
    }
    // 3. Inherited advisory context (still authenticated at connect).
    let has_env_socket = env_socket.is_some_and(|s| !s.is_empty());
    let has_env_base = xdg_runtime_dir.is_some_and(|s| !s.is_empty());
    if has_env_socket || env_instance.is_some_and(|s| !s.is_empty()) || has_env_base {
        let path = bitty_ipc::devtools::resolve_socket_path(
            uid,
            xdg_runtime_dir,
            env_socket.filter(|s| !s.is_empty()),
            env_instance.filter(|s| !s.is_empty()),
        )
        .map_err(|err| format!("bitty ctl: cannot resolve inherited target: {err}"))?;
        let instance = env_instance
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string();
        return Ok(ResolvedTarget {
            socket_path: path,
            instance,
        });
    }
    // 4. Exactly-one-live shortcut: enumerate candidate sockets and require
    //    exactly one live peer. Zero or many is an ambiguity error (exit 6),
    //    never a silent pick.
    let candidates = discover_live_sockets(xdg_runtime_dir, uid);
    match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(String::from(
            "bitty ctl: no live instance (set --socket or --instance, or run bitty first)",
        )),
        many => {
            let mut names: Vec<String> = many.iter().map(|c| c.instance.clone()).collect();
            names.sort();
            Err(format!(
                "bitty ctl: ambiguous instance ({} live: {}); pass --socket or --instance (see `bitty ctl instance list`)",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

/// Resolve the Windows ctl target: an explicit pipe path, an
/// instance-derived pipe, the inherited advisory environment, or the
/// exactly-one-live shortcut (CTX-1405, #2244).
///
/// No hardcoded pipe name is used: `--instance` and `BITTY_INSTANCE_ID` resolve
/// through [`bitty_ipc::devtools::windows_pipe_name`]. An explicit
/// `--socket`/`BITTY_SOCKET` must be a `\\.\pipe\` device path; a filesystem
/// socket path fails closed instead of silently falling back.
///
/// # Errors
///
/// Returns the documented exit-6 text for invalid pipe paths, invalid instance
/// ids, no live instance, or an ambiguous live set. Every resolved target is
/// still authenticated by [`ctl_roundtrip`] before a request byte is sent.
#[cfg(windows)]
pub fn resolve_ctl_target(
    targeting: &CtlTargeting,
    env_socket: Option<&str>,
    env_instance: Option<&str>,
    _xdg_runtime_dir: Option<&str>,
    _uid: u32,
) -> Result<ResolvedTarget, String> {
    use bitty_ipc::devtools::{
        DEFAULT_INSTANCE_ID, SocketEnv, WINDOWS_PIPE_NAMESPACE, resolve_windows_pipe_path_from_env,
        windows_instance_from_pipe_name,
    };

    let from_pipe = |pipe: &str, instance: Option<String>| -> Result<ResolvedTarget, String> {
        let instance = instance
            .filter(|id| !id.is_empty())
            .or_else(|| {
                pipe.strip_prefix(WINDOWS_PIPE_NAMESPACE)
                    .and_then(windows_instance_from_pipe_name)
            })
            .unwrap_or_else(|| String::from(DEFAULT_INSTANCE_ID));
        let env = SocketEnv {
            bitty_socket: Some(pipe.to_string()),
            xdg_runtime_dir: None,
            instance_id: Some(instance),
        };
        let (path, instance) = resolve_windows_pipe_path_from_env(&env)
            .map_err(|err| format!("bitty ctl: --socket is not a usable pipe path: {err}"))?;
        Ok(ResolvedTarget {
            socket_path: path,
            instance,
        })
    };

    fn by_instance(id: &str, origin: &str) -> Result<ResolvedTarget, String> {
        let env = bitty_ipc::devtools::SocketEnv {
            bitty_socket: None,
            xdg_runtime_dir: None,
            instance_id: Some(id.to_string()),
        };
        let (path, instance) = bitty_ipc::devtools::resolve_windows_pipe_path_from_env(&env)
            .map_err(|err| format!("bitty ctl: cannot resolve {origin} {id:?}: {err}"))?;
        Ok(ResolvedTarget {
            socket_path: path,
            instance,
        })
    }

    if let Some(pipe) = targeting.socket.as_deref() {
        if pipe.is_empty() {
            return Err(String::from(
                "bitty ctl: --socket is not a usable pipe path",
            ));
        }
        return from_pipe(pipe, targeting.instance.clone());
    }
    if let Some(id) = targeting.instance.as_deref() {
        return by_instance(id, "--instance");
    }
    if let Some(pipe) = env_socket.filter(|value| !value.is_empty()) {
        return from_pipe(pipe, env_instance.map(String::from));
    }
    if let Some(id) = env_instance.filter(|value| !value.is_empty()) {
        return by_instance(id, "inherited instance");
    }

    // Exactly-one-live shortcut: live pipes only, same-UID by construction
    // (owner SID is verified before the probe reports live).
    let candidates = discover_live_pipes().map_err(|err| err.message)?;
    match candidates.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(String::from(
            "bitty ctl: no live instance (set --socket or --instance, or run bitty first)",
        )),
        many => {
            let mut names: Vec<String> = many.iter().map(|c| c.instance.clone()).collect();
            names.sort();
            Err(format!(
                "bitty ctl: ambiguous instance ({} live: {}); pass --socket or --instance (see `bitty ctl instance list`)",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

/// Platforms with neither Unix sockets nor Windows pipes: no target exists.
#[cfg(not(any(unix, windows)))]
pub fn resolve_ctl_target(
    _targeting: &CtlTargeting,
    _env_socket: Option<&str>,
    _env_instance: Option<&str>,
    _xdg_runtime_dir: Option<&str>,
    _uid: u32,
) -> Result<ResolvedTarget, String> {
    Err(generic_ctl_unavailable_message())
}

/// Live Windows pipe instances as ctl targets.
///
/// Registry/namespace read failures stay typed (`InstanceError`) so target
/// resolution reports the documented exit-6 class instead of silently
/// treating an unreadable registry as "no live instance".
#[cfg(windows)]
fn discover_live_pipes() -> Result<Vec<ResolvedTarget>, crate::list::InstanceError> {
    let mut live: Vec<ResolvedTarget> = crate::list::discover_instances(None, None)?
        .into_iter()
        .filter(|row| row.live)
        .map(|row| ResolvedTarget {
            socket_path: row.socket,
            instance: row.instance,
        })
        .collect();
    live.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(live)
}

/// Base directory for socket discovery (`XDG_RUNTIME_DIR` or `/run/user/<uid>`).
#[cfg(unix)]
fn discovery_base(xdg_runtime_dir: Option<&str>, uid: u32) -> Option<String> {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => Some(dir.to_string()),
        _ => Some(format!("/run/user/{uid}")),
    }
}

/// Enumerate live sockets under `<base>/bitty/*.sock`.
///
/// A candidate is live only when the endpoint first verifies (`0700` dir +
/// `0600` socket, both owned by `uid`, no symlinks — CTX-0528/IPC-001: a
/// foreign-owned or world-writable endpoint never reaches `connect`) and
/// `connect` then succeeds; stale files (refused) are skipped, never
/// removed here (the servo reclaims on bind). Best-effort: unreadable
/// directories yield no candidates (ambiguity error downstream).
#[cfg(unix)]
fn discover_live_sockets(xdg_runtime_dir: Option<&str>, uid: u32) -> Vec<ResolvedTarget> {
    use std::os::unix::net::UnixStream;

    let Some(base) = discovery_base(xdg_runtime_dir, uid) else {
        return Vec::new();
    };
    let leaf = std::path::Path::new(&base).join(bitty_ipc::devtools::SOCKET_LEAF_DIR);
    let entries = std::fs::read_dir(&leaf).ok();
    let mut live = Vec::new();
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "sock") {
                continue;
            }
            let path_str = path.to_string_lossy().into_owned();
            if path_str.len() > bitty_ipc::devtools::MAX_SOCKET_PATH_BYTES {
                continue;
            }
            // CTX-0528 (IPC-001): verify the endpoint before connect — a
            // forged `BITTY_SOCKET`/planted entry pointing at another
            // owner's socket fails closed here, never reaching I/O.
            if bitty_ipc::devtools::verify_socket_endpoint_for_connect(&path_str, uid).is_err() {
                continue;
            }
            if UnixStream::connect(&path).is_ok() {
                let instance = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from("default"));
                live.push(ResolvedTarget {
                    socket_path: path_str,
                    instance,
                });
            }
        }
    }
    live.sort_by(|a, b| a.instance.cmp(&b.instance));
    live
}

/// Platforms with neither Unix sockets nor Windows pipes: no discovery.
#[cfg(not(any(unix, windows)))]
fn discover_live_sockets(_xdg_runtime_dir: Option<&str>, _uid: u32) -> Vec<ResolvedTarget> {
    Vec::new()
}

/// Local `instance list` discovery: all live sockets (same-UID only by
/// construction: the leaf dir is `0700` and sockets are `0600`).
#[cfg(unix)]
pub fn list_live_instances(xdg_runtime_dir: Option<&str>, uid: u32) -> Vec<ResolvedTarget> {
    discover_live_sockets(xdg_runtime_dir, uid)
}

/// Local `instance list` discovery on Windows: live instance pipes, owner-SID
/// verified before a row reports live (CTX-1405, #2244).
#[cfg(windows)]
pub fn list_live_instances(_xdg_runtime_dir: Option<&str>, _uid: u32) -> Vec<ResolvedTarget> {
    // Listing is best-effort; target resolution keeps the typed error.
    discover_live_pipes().unwrap_or_default()
}

/// Platforms with neither Unix sockets nor Windows pipes: empty discovery.
#[cfg(not(any(unix, windows)))]
pub fn list_live_instances(_xdg_runtime_dir: Option<&str>, _uid: u32) -> Vec<ResolvedTarget> {
    Vec::new()
}

// ── IPC client ────────────────────────────────────────────────────────────

/// One IPC round-trip outcome (std-only, no new struct in the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlIpcOutcome {
    /// True on `result`, false on `error`.
    pub ok: bool,
    /// Raw `result` JSON on success.
    pub result_json: String,
    /// Error category on failure.
    pub category: String,
    /// Error code on failure.
    pub code: String,
    /// Human message on failure.
    pub message: String,
}

/// Connect, send one framed request, read one framed response.
///
/// Time-bounded (`ipc_ctl::CTL_CLIENT_TIMEOUT` read timeout, which outlives
/// the server-side reply wait by a fixed grace, and `ipc_ctl::CTL_TIMEOUT`
/// write timeout) so a dead peer cannot hang the CLI.
///
/// Server identity is established in two stages (CTX-0539). Before connect
/// the endpoint is verified (`0700` dir + `0600` socket, both owned by
/// `runtime_uid`, no symlinks) and its inode identity captured. After connect
/// the connected stream's kernel-reported peer address is re-bound to that
/// captured identity: a checked-then-swapped path (unlink + rebind between
/// the check and the connect), a forged `BITTY_SOCKET`, or any other foreign
/// endpoint fails closed before one request byte is written. The pre-connect
/// check is defense in depth, never the sole gate.
#[cfg(unix)]
pub fn ctl_roundtrip(
    socket_path: &str,
    method: &str,
    params: Option<&str>,
    runtime_uid: u32,
) -> Result<CtlIpcOutcome, String> {
    let expected =
        bitty_ipc::devtools::verify_socket_endpoint_for_connect(socket_path, runtime_uid).map_err(
            |err| {
                format!("bitty ctl: socket endpoint verification failed for {socket_path:?}: {err}")
            },
        )?;
    let mut stream = connect_verified_endpoint(socket_path, expected)?;
    stream
        .set_read_timeout(Some(ipc_ctl::CTL_CLIENT_TIMEOUT))
        .map_err(|err| format!("bitty ctl: cannot set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(ipc_ctl::CTL_TIMEOUT))
        .map_err(|err| format!("bitty ctl: cannot set write timeout: {err}"))?;
    exchange(&mut stream, method, params)
}

/// Connect over the instance's Windows named pipe, then send one framed
/// request and read one framed response (CTX-1405, #2244).
///
/// `connect` compares the serving process token SID with the current user's
/// before returning, and the pipe's protected DACL already refuses a client
/// that is not the current user; both checks fail closed before one request
/// byte is written. Framing, timeout, and reply parsing are the same
/// [`exchange`] used by the Unix path (no second protocol).
#[cfg(windows)]
pub fn ctl_roundtrip(
    socket_path: &str,
    method: &str,
    params: Option<&str>,
    _runtime_uid: u32,
) -> Result<CtlIpcOutcome, String> {
    let mut connection = bitty_ipc::winpipe::connect(socket_path, ipc_ctl::CTL_TIMEOUT)
        .map_err(|err| format!("bitty ctl: cannot connect to pipe {socket_path:?}: {err}"))?;
    connection.set_read_timeout(Some(ipc_ctl::CTL_CLIENT_TIMEOUT));
    connection.set_write_timeout(Some(ipc_ctl::CTL_TIMEOUT));
    exchange(&mut connection, method, params)
}

/// Non-Windows, non-Unix stub: no local IPC transport exists here.
///
/// # Errors
///
/// Always returns the documented generic unavailable message.
#[cfg(not(any(unix, windows)))]
pub fn ctl_roundtrip(
    _socket_path: &str,
    _method: &str,
    _params: Option<&str>,
    _runtime_uid: u32,
) -> Result<CtlIpcOutcome, String> {
    Err(generic_ctl_unavailable_message())
}

/// Non-Windows, non-Unix degradation message (same fail-soft shape).
///
/// Kept alive on Unix/Windows builds for the cross-check and the
/// all-platform unit tests.
#[allow(dead_code)]
pub(crate) fn generic_ctl_unavailable_message() -> String {
    String::from(
        "bitty ctl: IPC control is unavailable on this platform (`bitty ctl instance list` stays local discovery only)",
    )
}

/// Write one framed request and read one framed response over `stream`.
///
/// Shared by the Unix socket and Windows named-pipe clients so the envelope,
/// frame bound, and reply parsing cannot drift (CTX-1405, #2244).
fn exchange<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    method: &str,
    params: Option<&str>,
) -> Result<CtlIpcOutcome, String> {
    let params_part = match params {
        None => String::new(),
        Some(p) => format!(",\"params\":{p}"),
    };
    let envelope = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"version\":\"1.0\",\"method\":\"{method}\"{params_part}}}"
    );
    let wire = bitty_ipc::encode_frame(envelope.as_bytes())
        .map_err(|err| format!("bitty ctl: request too large: {err}"))?;
    stream
        .write_all(&wire)
        .map_err(|err| format!("bitty ctl: send failed: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("bitty ctl: send flush failed: {err}"))?;

    // Read the 4-byte header then exactly the framed payload.
    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|err| format!("bitty ctl: no response (instance may have exited): {err}"))?;
    let len = u32::from_be_bytes(header) as usize;
    if len > bitty_ipc::MAX_FRAME_BYTES {
        return Err(format!(
            "bitty ctl: response frame {len} exceeds limit {}",
            bitty_ipc::MAX_FRAME_BYTES
        ));
    }
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|err| format!("bitty ctl: truncated response: {err}"))?;
    parse_ctl_response(&payload)
}

/// Connect to `socket_path` and re-bind the live stream to the endpoint
/// identity captured by the pre-connect verification (CTX-0539).
///
/// Split out from [`ctl_roundtrip`] so the checked-then-swapped fixture can
/// exercise the real connect + post-connect verification without a race:
/// the caller captures `expected` from the legitimate endpoint, the path is
/// then swapped to a spoof, and this function must fail closed before the
/// caller can write any request byte.
#[cfg(unix)]
pub(super) fn connect_verified_endpoint(
    socket_path: &str,
    expected: bitty_ipc::devtools::SocketEndpointIdentity,
) -> Result<std::os::unix::net::UnixStream, String> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("bitty ctl: cannot connect to {socket_path:?}: {err}"))?;
    bitty_ipc::devtools::verify_connected_endpoint(&stream, expected).map_err(|err| {
        format!("bitty ctl: connected server identity check failed for {socket_path:?}: {err}")
    })?;
    Ok(stream)
}

/// Parse a devtools response envelope into an outcome.
///
/// Minimal manual scan (no new deps); malformed envelopes are transport errors.
pub(super) fn parse_ctl_response(payload: &[u8]) -> Result<CtlIpcOutcome, String> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| String::from("bitty ctl: response is not utf-8 json"))?;
    if text.contains("\"result\"") && !text.contains("\"error\"") {
        let result = extract_top_value(text, "result")
            .ok_or_else(|| String::from("bitty ctl: response has no result"))?;
        return Ok(CtlIpcOutcome {
            ok: true,
            result_json: result,
            category: String::new(),
            code: String::new(),
            message: String::new(),
        });
    }
    if text.contains("\"error\"") {
        let err_obj = extract_top_value(text, "error").unwrap_or_default();
        let category =
            extract_string_from(&err_obj, "category").unwrap_or_else(|| String::from("transport"));
        let code =
            extract_string_from(&err_obj, "code").unwrap_or_else(|| String::from("Transport"));
        let message = extract_string_from(&err_obj, "message")
            .unwrap_or_else(|| String::from("unknown IPC error"));
        return Ok(CtlIpcOutcome {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        });
    }
    Err(String::from("bitty ctl: malformed response envelope"))
}

/// Extract a top-level `"key": <value>` JSON value (object, string, number,
/// bool, null) as raw text. Balanced-brace scan, quote-aware.
//
// Used by both the unix IPC client (`parse_ctl_response`) and the
// platform-independent table renderer (`render_table`), so it stays
// compiled on all targets.
fn extract_top_value(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = text.find(&needle)?;
    let after = &text[pos + needle.len()..];
    let colon = after.find(':')?;
    let mut rest = after[colon + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let first = rest.as_bytes()[0];
    if first == b'"' {
        // String: scan to closing unescaped quote.
        let mut i = 1usize;
        let bytes = rest.as_bytes();
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    return Some(rest[..i + 1].to_string());
                }
                b'\\' => {
                    i += 2;
                }
                _ => {
                    let ch = rest[i..].chars().next()?;
                    i += ch.len_utf8();
                }
            }
        }
        return None;
    }
    if first == b'{' {
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        for (idx, ch) in rest.char_indices() {
            if in_str {
                if esc {
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else if ch == '"' {
                    in_str = false;
                }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(rest[..idx + ch.len_utf8()].to_string());
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    // Number/bool/null: to next `,` or `}`.
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest = rest[..end].trim_end();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Extract a `"key": "string"` field from a flat JSON object (unescaped).
//
// Shared by the unix client and the table renderer (all targets).
pub(super) fn extract_string_from(obj: &str, key: &str) -> Option<String> {
    let raw = extract_top_value(obj, key)?;
    if !raw.starts_with('"') {
        return None;
    }
    unescape_json_string(&raw)
}

/// Unescape a JSON string literal (including surrounding quotes).
//
// Shared by the unix client and the table renderer (all targets).
fn unescape_json_string(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                if hex.len() != 4 {
                    return None;
                }
                let code = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(code)?);
            }
            _ => return None,
        }
    }
    Some(out)
}
