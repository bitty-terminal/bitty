//! `bitty ctl` target resolution and IPC round-trip (split from `ctl.rs`, CTX-0307).

use super::CtlTargeting;
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
/// A candidate is live when `connect` succeeds; stale files (refused) are
/// skipped, never removed here (the servo reclaims on bind). Best-effort:
/// unreadable directories yield no candidates (ambiguity error downstream).
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

/// Non-unix: no socket discovery (single-platform servo is unix-only).
#[cfg(not(unix))]
fn discover_live_sockets(_xdg_runtime_dir: Option<&str>, _uid: u32) -> Vec<ResolvedTarget> {
    Vec::new()
}

/// Local `instance list` discovery: all live sockets (same-UID only by
/// construction: the leaf dir is `0700` and sockets are `0600`).
pub fn list_live_instances(xdg_runtime_dir: Option<&str>, uid: u32) -> Vec<ResolvedTarget> {
    discover_live_sockets(xdg_runtime_dir, uid)
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
/// Unix-only (the servo is unix-only); non-unix returns unavailable.
/// Time-bounded (`ipc_ctl::CTL_TIMEOUT` read/write timeouts, shared with
/// the server-side reply wait) so a dead peer cannot hang the CLI.
#[cfg(unix)]
pub fn ctl_roundtrip(
    socket_path: &str,
    method: &str,
    params: Option<&str>,
) -> Result<CtlIpcOutcome, String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("bitty ctl: cannot connect to {socket_path:?}: {err}"))?;
    stream
        .set_read_timeout(Some(ipc_ctl::CTL_TIMEOUT))
        .map_err(|err| format!("bitty ctl: cannot set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(ipc_ctl::CTL_TIMEOUT))
        .map_err(|err| format!("bitty ctl: cannot set write timeout: {err}"))?;

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

/// Non-unix stub: the servo never serves here.
#[cfg(not(unix))]
pub fn ctl_roundtrip(
    _socket_path: &str,
    _method: &str,
    _params: Option<&str>,
) -> Result<CtlIpcOutcome, String> {
    Err(String::from(
        "bitty ctl: IPC control requires a unix platform",
    ))
}

/// Parse a devtools response envelope into an outcome.
///
/// Minimal manual scan (no new deps); malformed envelopes are transport errors.
#[cfg(unix)]
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
