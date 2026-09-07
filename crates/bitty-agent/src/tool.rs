//! Tool-call stubs — no LLM I/O, no process spawn, no network.
//!
//! This module owns the **vocabulary** that will travel over the `bitty-ipc`
//! transport when the `OQ-018` RFC lands. It validates and stores tool
//! declarations and individual calls/results, but it never contacts an LLM,
//! never executes a tool, and never performs I/O. Real dispatch will be owned
//! by the runtime's capability-checked service layer (or by an external
//! helper process behind `bitty-ipc`), not by this crate.
//!
//! # Bounds (threat `T-01`)
//!
//! Every field is bounded and owned so untrusted agent payloads cannot grow
//! memory without limit. The side queue and message caps are independent — a
//! flood of tool calls cannot exceed `MAX_TOOL_CALLS_PER_TURN` and each
//! argument/result is capped.
//!
//! # Credential scrubbing (threat `T-10`, `P0-AC-026` parity)
//!
//! [`ToolCall::arguments`] and [`ToolResult::content`] are untrusted text that
//! may carry secrets (passwords, tokens, API keys, PEM blocks, bearer
//! credentials). Raw values are stored for host dispatch, but **no log line
//! or IPC payload must carry them**. Callers crossing a log/IPC boundary
//! must use the scrubbed views:
//!
//! - [`scrub_text`] / [`scrub_tool_args`] / [`scrub_tool_result`]
//! - [`ToolCall::scrubbed_arguments`] / [`ToolCall::scrubbed`] /
//!   [`ToolCall::log_safe`]
//! - [`ToolResult::scrubbed_content`] / [`ToolResult::scrubbed`] /
//!   [`ToolResult::log_safe`]
//!
//! `Debug` for [`ToolCall`] and [`ToolResult`] is redacting on purpose: even
//! `format!("{:?}", call)` never emits a credential. Fail-safe rule: any
//! value under a sensitive key is replaced with [`REDACTED_MARKER`]
//! regardless of JSON type (string, number, object, array), and any malformed
//! / unbalanced shape under a sensitive key redacts the whole payload rather
//! than passing bytes through.

use crate::error::AgentError;

/// Maximum tool name length (bytes).
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Maximum tool description length (bytes).
pub const MAX_TOOL_DESCRIPTION_LEN: usize = 1024;

/// Maximum input-schema length (bytes) — JSON Schema text, bounded.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 8 * 1024;

/// Maximum tool arguments length (bytes) — JSON text, bounded.
pub const MAX_TOOL_ARGS_BYTES: usize = 16 * 1024;

/// Maximum tool result length (bytes).
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// Maximum tool calls per turn / per message.
pub const MAX_TOOL_CALLS_PER_TURN: usize = 8;

/// Maximum declared tools per agent session.
pub const MAX_TOOLS_PER_AGENT: usize = 32;

/// Maximum tool-call id length.
pub const MAX_TOOL_CALL_ID_LEN: usize = 128;

/// Redaction marker replacing credential bytes before logs/IPC.
///
/// Mirrors `bitty-ipc`'s `REDACTED_MARKER` (`"[redacted]"`) so scrubbed
/// agent payloads and redacted frame lines share one greppable token.
pub const REDACTED_MARKER: &str = "[redacted]";

/// Quoted redaction payload (`"[redacted]"`) used when the redacted value
/// sits in a JSON string position so the scrubbed text stays valid JSON.
const QUOTED_REDACTED: &str = "\"[redacted]\"";

/// Return `true` when a tool-argument/result key likely carries a secret.
///
/// Matching is case-insensitive and intentionally fail-closed (over-redact
/// rather than leak). Bare short tokens (`auth`, `key`, `pwd`) match only on
/// exact equality or explicit `_key`/`_token`/`_secret` suffixes so legit
/// keys like `author`, `path`, or `description` pass through.
#[must_use]
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let l = lower.as_str();
    if matches!(
        l,
        "auth"
            | "pwd"
            | "key"
            | "token"
            | "secret"
            | "password"
            | "passwd"
            | "credential"
            | "credentials"
            | "bearer"
            | "cookie"
            | "cookies"
            | "authorization"
            | "passphrase"
    ) {
        return true;
    }
    for suffix in [
        "_key",
        "-key",
        ".key",
        "_token",
        "-token",
        ".token",
        "_secret",
        "-secret",
        ".secret",
        "_pwd",
        "-pwd",
        "_passwd",
        "-passwd",
        "_password",
        "-password",
    ] {
        if l.ends_with(suffix) {
            return true;
        }
    }
    for marker in [
        "password",
        "passwd",
        "secret",
        "credential",
        "bearer",
        "authorization",
        "cookie",
        "private_key",
        "privatekey",
        "private-key",
        "api_key",
        "apikey",
        "api-key",
        "access_key",
        "accesskey",
        "session_key",
        "client_secret",
        "auth_token",
        "refresh_token",
        "id_token",
        "access_token",
        "encryption_key",
        "signing_key",
        "aws_secret",
        "aws_session",
        "x-api-key",
        "set-cookie",
    ] {
        if l.contains(marker) {
            return true;
        }
    }
    l.contains("token")
}

/// Return `true` when a free-text value looks like a credential even without
/// a sensitive key (PEM block, known token prefix, JWT, bearer).
///
/// Short opaque strings (under 8 chars) never match so legit values like
/// `/tmp/x` or `hello` pass through. Already-redacted markers never match.
#[must_use]
pub fn looks_like_secret_token(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() || v == REDACTED_MARKER || v == "[REDACTED]" || v == "***" {
        return false;
    }
    if v.len() < 8 {
        return false;
    }
    if v.contains("-----BEGIN") {
        return true;
    }
    if v.starts_with("eyJ") && v.contains('.') && v.len() >= 20 {
        return true;
    }
    const PREFIXES: &[&str] = &[
        "AKIA",
        "ASIA",
        "ABIA",
        "ACCA",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxo-",
        "xoxs-",
        "sk-live-",
        "sk-test-",
        "sk-ant-",
        "AIza",
        "ya29.",
        "glpat-",
        "dop_v1_",
        "sq0atp-",
        "rk-live-",
        "pk-live-",
    ];
    for p in PREFIXES {
        if v.contains(p) {
            return true;
        }
    }
    if v.len() >= 12 {
        if v.contains("sk-") {
            return true;
        }
        let lower = v.to_ascii_lowercase();
        if lower.contains("bearer ") || lower.contains("basic ") {
            return true;
        }
    }
    false
}

/// Scrub credential bytes from arbitrary tool-argument/result text.
///
/// Applies, in order: JSON quoted-key redaction, bare `key=value`/`key: value`
/// redaction, then unstructured prefix/PEM/bearer redaction. Unknown or
/// ambiguous shapes under a sensitive key redact the whole payload (fail-safe:
/// never pass bytes through when a secret may be present). Legit text without
/// sensitive keys or known secret patterns is returned unchanged.
///
/// This function is `std`-only, deterministic, allocation-bounded (output is
/// at most a small constant larger than the input), and never panics on
/// UTF-8 (all scans are char-boundary safe).
#[must_use]
pub fn scrub_text(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let Some(step1) = scrub_quoted_keys(input) else {
        return REDACTED_MARKER.to_string();
    };
    let Some(step2) = scrub_bare_keys(&step1) else {
        return REDACTED_MARKER.to_string();
    };
    let step3 = scrub_unstructured(&step2);
    fail_safe_or_redacted(input, &step3)
}

/// Scrub tool arguments for a log/IPC boundary, enforcing [`MAX_TOOL_ARGS_BYTES`].
///
/// Output is truncated on a char boundary when redaction markers grow the
/// payload past the cap. Truncation never leaks: it only shortens already
/// redacted text.
#[must_use]
pub fn scrub_tool_args(input: &str) -> String {
    truncate_to_limit(&scrub_text(input), MAX_TOOL_ARGS_BYTES)
}

/// Scrub tool-result content for a log/IPC boundary, enforcing
/// [`MAX_TOOL_RESULT_BYTES`].
#[must_use]
pub fn scrub_tool_result(input: &str) -> String {
    truncate_to_limit(&scrub_text(input), MAX_TOOL_RESULT_BYTES)
}

fn truncate_to_limit(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn char_len_at(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map_or(1, |c| c.len_utf8())
}

fn find_closing_quote(bytes: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i = i.saturating_add(2);
            continue;
        }
        if b == quote {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_matching_bracket(input: &[u8], start: usize) -> Option<usize> {
    let open = *input.get(start)?;
    let (open_c, close_c) = match open {
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = start;
    let mut quote: Option<u8> = None;
    while i < input.len() {
        let b = input[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i = i.saturating_add(2);
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == open_c {
            depth += 1;
        } else if b == close_c {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// Scrub `"key": value` / `'key': value` pairs. Returns `None` when a
/// sensitive key has a malformed/unbalanced value (fail-safe: whole payload
/// must be redacted by the caller).
fn scrub_quoted_keys(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'"' && b != b'\'' {
            let len = char_len_at(input, i);
            out.push_str(&input[i..i + len]);
            i += len;
            continue;
        }
        let quote = b;
        let key_start = i + 1;
        let Some(key_end) = find_closing_quote(bytes, key_start, quote) else {
            let len = char_len_at(input, i);
            out.push_str(&input[i..i + len]);
            i += len;
            continue;
        };
        let key = &input[key_start..key_end];
        let mut j = key_end + 1;
        while j < bytes.len() && is_ascii_ws(bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            if looks_like_secret_token(key) {
                out.push_str(if quote == b'"' {
                    QUOTED_REDACTED
                } else {
                    "'[redacted]'"
                });
            } else {
                out.push_str(&input[i..key_end + 1]);
            }
            i = key_end + 1;
            continue;
        }
        let colon = j;
        j += 1;
        while j < bytes.len() && is_ascii_ws(bytes[j]) {
            j += 1;
        }
        if !is_sensitive_key(key) {
            let is_secret_value = j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') && {
                let q = bytes[j];
                find_closing_quote(bytes, j + 1, q)
                    .is_some_and(|end| looks_like_secret_token(&input[j + 1..end]))
            };
            if is_secret_value {
                out.push_str(&input[i..=colon]);
                let ws_start = colon + 1;
                out.push_str(&input[ws_start..j]);
                let q = bytes[j];
                let end = find_closing_quote(bytes, j + 1, q).unwrap_or(j);
                out.push_str(if q == b'"' {
                    QUOTED_REDACTED
                } else {
                    "'[redacted]'"
                });
                i = end + 1;
            } else {
                out.push_str(&input[i..j]);
                i = j;
            }
            continue;
        }
        out.push_str(&input[i..=colon]);
        let mut k = colon + 1;
        while k < j {
            out.push(input.as_bytes()[k] as char);
            k += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        let vb = bytes[j];
        if vb == b'"' || vb == b'\'' {
            let end = find_closing_quote(bytes, j + 1, vb)?;
            out.push_str(if vb == b'"' {
                QUOTED_REDACTED
            } else {
                "'[redacted]'"
            });
            i = end + 1;
        } else if vb == b'{' || vb == b'[' {
            let end = find_matching_bracket(bytes, j)?;
            out.push_str(QUOTED_REDACTED);
            i = end + 1;
        } else {
            let mut end = j;
            while end < bytes.len()
                && !matches!(bytes[end], b',' | b'}' | b']' | b'&' | b';' | b'\n' | b'\r')
                && !is_ascii_ws(bytes[end])
                && bytes[end] != b'"'
                && bytes[end] != b'\''
            {
                end += 1;
            }
            if end == j {
                return None;
            }
            out.push_str(QUOTED_REDACTED);
            i = end;
        }
    }
    Some(out)
}

/// Scrub bare `key=value` / `key: value` pairs (unquoted keys). Returns `None`
/// on malformed values under sensitive keys (fail-safe).
fn scrub_bare_keys(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if !(b.is_ascii_alphanumeric() || b == b'_')
            || (i > 0
                && is_key_char(bytes[i - 1])
                && out.ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-'))
        {
            let len = char_len_at(input, i);
            out.push_str(&input[i..i + len]);
            i += len;
            continue;
        }
        if i > 0 {
            let prev = bytes[i - 1];
            if is_key_char(prev) || prev == b'"' || prev == b'\'' {
                let len = char_len_at(input, i);
                out.push_str(&input[i..i + len]);
                i += len;
                continue;
            }
        }
        let mut key_end = i;
        while key_end < bytes.len() && is_key_char(bytes[key_end]) {
            key_end += 1;
        }
        if key_end == i {
            let len = char_len_at(input, i);
            out.push_str(&input[i..i + len]);
            i += len;
            continue;
        }
        let key = &input[i..key_end];
        // Avoid re-scrubbing the marker itself (`[redacted]` contains `-`).
        if key == "redacted" {
            out.push_str(key);
            i = key_end;
            continue;
        }
        let mut j = key_end;
        while j < bytes.len() && is_ascii_ws(bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() || (bytes[j] != b'=' && bytes[j] != b':') {
            out.push_str(key);
            i = key_end;
            continue;
        }
        // `::` (e.g. Rust paths) is not a key/value delimiter.
        if bytes[j] == b':' && j + 1 < bytes.len() && bytes[j + 1] == b':' {
            out.push_str(key);
            i = key_end;
            continue;
        }
        // `://` (URLs) is not a key/value delimiter either.
        if bytes[j] == b':' && j + 2 < bytes.len() && bytes[j + 1] == b'/' && bytes[j + 2] == b'/' {
            out.push_str(key);
            i = key_end;
            continue;
        }
        if !is_sensitive_key(key) {
            out.push_str(key);
            i = key_end;
            continue;
        }
        let delim = j;
        j += 1;
        // `=>`, `==`, `:=` — keep the second char with the delimiter.
        if j < bytes.len() && matches!(bytes[j], b'=' | b'>') {
            j += 1;
        }
        while j < bytes.len() && is_ascii_ws(bytes[j]) {
            j += 1;
        }
        out.push_str(&input[i..j]);
        // Preserve the `=`/`:` delimiter run already pushed via input[i..j].
        let _ = delim;
        if j >= bytes.len() {
            out.push_str(REDACTED_MARKER);
            i = j;
            continue;
        }
        let vb = bytes[j];
        if vb == b'"' || vb == b'\'' {
            let end = find_closing_quote(bytes, j + 1, vb)?;
            // Preserve the caller's quoting style.
            if vb == b'"' {
                out.push_str(QUOTED_REDACTED);
            } else {
                out.push_str("'[redacted]'");
            }
            i = end + 1;
        } else {
            let mut end = j;
            while end < bytes.len()
                && !matches!(bytes[end], b',' | b';' | b'&' | b'\n' | b'\r' | b'}' | b']')
                && !is_ascii_ws(bytes[end])
                && bytes[end] != b'"'
                && bytes[end] != b'\''
            {
                end += 1;
            }
            // `Bearer <token>` / `Basic <token>` carry the secret in the second
            // word — redact both so `Authorization: Bearer abc` never leaks `abc`.
            if end > j {
                let first = &input[j..end];
                if first.eq_ignore_ascii_case("bearer") || first.eq_ignore_ascii_case("basic") {
                    let mut k = end;
                    while k < bytes.len() && is_ascii_ws(bytes[k]) {
                        k += 1;
                    }
                    let mut token_end = k;
                    while token_end < bytes.len()
                        && !matches!(
                            bytes[token_end],
                            b',' | b';' | b'&' | b'\n' | b'\r' | b'}' | b']'
                        )
                        && !is_ascii_ws(bytes[token_end])
                        && bytes[token_end] != b'"'
                        && bytes[token_end] != b'\''
                    {
                        token_end += 1;
                    }
                    if token_end > k {
                        end = token_end;
                    }
                }
            }
            out.push_str(REDACTED_MARKER);
            i = end;
        }
    }
    Some(out)
}

/// Redact unstructured credential shapes anywhere in the text: PEM blocks,
/// known token prefixes, and `Bearer`/`Basic` credential runs.
fn scrub_unstructured(input: &str) -> String {
    let after_pem = redact_pem_blocks(input);
    let after_tokens = redact_prefixed_tokens(&after_pem);
    redact_bearer_runs(&after_tokens)
}

fn redact_pem_blocks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let Some(start) = rest.find("-----BEGIN") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(end_marker) = tail.find("-----END") {
            let after_end = &tail[end_marker..];
            if let Some(close) = after_end.find("-----") {
                // `close` is the leading dashes of the footer; skip the full run.
                let mut footer_end = end_marker + close + 5;
                while footer_end < tail.len() && tail.as_bytes()[footer_end] == b'-' {
                    footer_end += 1;
                }
                out.push_str(REDACTED_MARKER);
                rest = &tail[footer_end..];
                continue;
            }
        }
        // Truncated / unbalanced PEM: fail-safe redacts the marker to the end.
        out.push_str(REDACTED_MARKER);
        break;
    }
    out
}

fn redact_prefixed_tokens(input: &str) -> String {
    const PREFIXES: &[&str] = &[
        "AKIA",
        "ASIA",
        "ABIA",
        "ACCA",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxo-",
        "xoxs-",
        "sk-live-",
        "sk-test-",
        "sk-ant-",
        "AIza",
        "ya29.",
        "glpat-",
        "dop_v1_",
        "sq0atp-",
        "rk-live-",
        "pk-live-",
        "sk-",
        "eyJ",
    ];
    let mut current = input.to_string();
    for prefix in PREFIXES {
        while let Some(found) = current.find(prefix) {
            // Never redact inside our own marker.
            if current[..found].ends_with("[redacted")
                || current[found..].starts_with(REDACTED_MARKER)
            {
                break;
            }
            let bytes_len = current.len();
            let mut end = found + prefix.len();
            while end < bytes_len && is_token_char(current.as_bytes()[end]) {
                // `eyJ` JWTs also carry `.` separators; `is_token_char` covers `.`.
                end += 1;
            }
            // Extend through trailing `=` padding for base64 JWT segments.
            while end < bytes_len && current.as_bytes()[end] == b'=' {
                end += 1;
            }
            if end <= found + prefix.len() {
                // Bare prefix with no token run (e.g. truncated): still redact it.
                end = found + prefix.len();
            }
            current.replace_range(found..end, REDACTED_MARKER);
        }
    }
    current
}

fn redact_bearer_runs(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if matches_scheme_at(bytes, i, "bearer") || matches_scheme_at(bytes, i, "basic") {
            let mut j = i;
            while j < bytes.len() && bytes[j] != b' ' && bytes[j] != b'\t' {
                j += 1;
            }
            out.push_str(&input[i..j]);
            let mut k = j;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                k += 1;
            }
            out.push_str(&input[j..k]);
            let mut end = k;
            while end < bytes.len() && is_token_char(bytes[end]) {
                end += 1;
            }
            if end > k {
                out.push_str(REDACTED_MARKER);
                i = end;
                continue;
            }
            i = k;
            continue;
        }
        // Copy one char (UTF-8 safe).
        let len = char_len_at(input, i);
        out.push_str(&input[i..i + len]);
        i += len;
    }
    out
}

fn matches_scheme_at(bytes: &[u8], idx: usize, scheme: &str) -> bool {
    if idx + scheme.len() >= bytes.len() {
        return false;
    }
    if !bytes[idx..idx + scheme.len()].eq_ignore_ascii_case(scheme.as_bytes()) {
        return false;
    }
    let next = bytes[idx + scheme.len()];
    if next != b' ' && next != b'\t' {
        return false;
    }
    if idx > 0 {
        let prev = bytes[idx - 1];
        // Only match at a boundary (start, whitespace, quote, colon, equals).
        if !(is_ascii_ws(prev) || matches!(prev, b'"' | b'\'' | b':' | b'=' | b'(' | b'[' | b'{')) {
            return false;
        }
    }
    true
}

/// Fail-safe gate: when the original payload carried a sensitive shape but
/// scrubbing left no marker (ambiguous parse), redact the whole payload
/// rather than passing bytes through.
fn fail_safe_or_redacted(original: &str, scrubbed: &str) -> String {
    if scrubbed.contains(REDACTED_MARKER) || scrubbed.contains(QUOTED_REDACTED) {
        return scrubbed.to_string();
    }
    if original.contains("-----BEGIN") && scrubbed.contains("-----BEGIN") {
        return REDACTED_MARKER.to_string();
    }
    for prefix in [
        "AKIA", "ASIA", "ghp_", "gho_", "ghs_", "xoxb-", "sk-live-", "sk-test-", "AIza", "ya29.",
    ] {
        if original.contains(prefix) && scrubbed.contains(prefix) {
            return REDACTED_MARKER.to_string();
        }
    }
    scrubbed.to_string()
}

/// Owned description of a tool an agent may call (stub, not executable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    /// Tool name, e.g. `read_file` or `run_command` (candidate names, not normative).
    pub name: String,
    /// Human-readable description (bounded).
    pub description: String,
    /// JSON Schema for arguments as bounded text (no schema validation beyond bounds here).
    pub input_schema: String,
}

impl ToolSpec {
    /// Validate this spec.
    pub fn validate(&self) -> Result<(), AgentError> {
        validate_tool_name(&self.name)?;
        if self.description.len() > MAX_TOOL_DESCRIPTION_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool description".to_string(),
                limit: MAX_TOOL_DESCRIPTION_LEN,
                actual: self.description.len(),
            });
        }
        if self.input_schema.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool input_schema".to_string(),
                limit: MAX_TOOL_SCHEMA_BYTES,
                actual: self.input_schema.len(),
            });
        }
        if self.description.contains('\0') || self.input_schema.contains('\0') {
            return Err(AgentError::validation("tool spec", "must not contain NUL"));
        }
        Ok(())
    }
}

/// A single tool call requested by the agent (stub, not executed here).
///
/// `Debug` is redacting on purpose: it prints [`ToolCall::scrubbed_arguments`]
/// so `format!("{:?}", call)` is safe for logs. Use the raw `arguments` field
/// only for host dispatch, never for logging or IPC framing — use
/// [`scrub_tool_args`] / [`ToolCall::scrubbed`] there instead.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Stable call id (owned, bounded).
    pub id: String,
    /// Tool name (must match a declared `ToolSpec::name` to be considered valid — not enforced here).
    pub name: String,
    /// Arguments as bounded JSON text (untrusted, never `eval`ed here).
    pub arguments: String,
}

impl std::fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("arguments", &self.scrubbed_arguments())
            .finish()
    }
}

impl ToolCall {
    /// Create and validate a tool call.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<Self, AgentError> {
        let id = id.into();
        let name = name.into();
        let arguments = arguments.into();
        let call = Self {
            id,
            name,
            arguments,
        };
        call.validate()?;
        Ok(call)
    }

    /// Validate this call.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.id.is_empty() {
            return Err(AgentError::validation("tool call id", "must not be empty"));
        }
        if self.id.len() > MAX_TOOL_CALL_ID_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool call id".to_string(),
                limit: MAX_TOOL_CALL_ID_LEN,
                actual: self.id.len(),
            });
        }
        if self.id.contains('\0') {
            return Err(AgentError::validation(
                "tool call id",
                "must not contain NUL",
            ));
        }
        validate_tool_name(&self.name)?;
        if self.arguments.len() > MAX_TOOL_ARGS_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool arguments".to_string(),
                limit: MAX_TOOL_ARGS_BYTES,
                actual: self.arguments.len(),
            });
        }
        if self.arguments.contains('\0') {
            return Err(AgentError::validation(
                "tool arguments",
                "must not contain NUL",
            ));
        }
        Ok(())
    }

    /// Approximate byte size (for batch budgets).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.id.len() + self.name.len() + self.arguments.len()
    }

    /// Scrubbed arguments safe for logs and IPC payloads.
    ///
    /// Secret values under sensitive keys and known credential shapes are
    /// replaced with [`REDACTED_MARKER`]; legit values pass through unchanged.
    /// Output is capped at [`MAX_TOOL_ARGS_BYTES`].
    #[must_use]
    pub fn scrubbed_arguments(&self) -> String {
        scrub_tool_args(&self.arguments)
    }

    /// Clone with arguments replaced by [`ToolCall::scrubbed_arguments`].
    ///
    /// Use before framing for `bitty-ipc` or attaching to diagnostics. The
    /// scrubbed clone always validates because redaction never introduces
    /// NUL and the output is truncated to the args cap.
    #[must_use]
    pub fn scrubbed(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            arguments: self.scrubbed_arguments(),
        }
    }

    /// One-line log-safe summary. Never includes raw arguments.
    #[must_use]
    pub fn log_safe(&self) -> String {
        format!(
            "ToolCall{{id={}, name={}, arguments={}}}",
            self.id,
            self.name,
            self.scrubbed_arguments()
        )
    }
}

/// Result of a tool call (stub, produced by the host outside this crate).
///
/// `Debug` is redacting on purpose: it prints
/// [`ToolResult::scrubbed_content`] so `format!("{:?}", result)` is safe for
/// logs. Use the raw `content` field only for host-side handling, never for
/// logging or IPC framing — use [`scrub_tool_result`] / [`ToolResult::scrubbed`]
/// there instead.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Call id this result answers.
    pub call_id: String,
    /// Bounded result content (JSON or text, untrusted).
    pub content: String,
    /// Whether the tool reported an error (host-owned flag, not inferred from content).
    pub is_error: bool,
}

impl std::fmt::Debug for ToolResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolResult")
            .field("call_id", &self.call_id)
            .field("content", &self.scrubbed_content())
            .field("is_error", &self.is_error)
            .finish()
    }
}

impl ToolResult {
    /// Create and validate a tool result.
    pub fn new(
        call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<Self, AgentError> {
        let call_id = call_id.into();
        let content = content.into();
        let r = Self {
            call_id,
            content,
            is_error,
        };
        r.validate()?;
        Ok(r)
    }

    /// Validate this result.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.call_id.is_empty() {
            return Err(AgentError::validation(
                "tool result call_id",
                "must not be empty",
            ));
        }
        if self.call_id.len() > MAX_TOOL_CALL_ID_LEN {
            return Err(AgentError::LimitExceeded {
                field: "tool result call_id".to_string(),
                limit: MAX_TOOL_CALL_ID_LEN,
                actual: self.call_id.len(),
            });
        }
        if self.content.len() > MAX_TOOL_RESULT_BYTES {
            return Err(AgentError::LimitExceeded {
                field: "tool result".to_string(),
                limit: MAX_TOOL_RESULT_BYTES,
                actual: self.content.len(),
            });
        }
        if self.call_id.contains('\0') || self.content.contains('\0') {
            return Err(AgentError::validation(
                "tool result",
                "must not contain NUL",
            ));
        }
        Ok(())
    }

    /// Approximate byte size.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.call_id.len() + self.content.len()
    }

    /// Scrubbed content safe for logs and IPC payloads.
    ///
    /// Same guarantees as [`scrub_tool_result`]: secrets redacted, legit text
    /// unchanged, output capped at [`MAX_TOOL_RESULT_BYTES`].
    #[must_use]
    pub fn scrubbed_content(&self) -> String {
        scrub_tool_result(&self.content)
    }

    /// Clone with content replaced by [`ToolResult::scrubbed_content`].
    #[must_use]
    pub fn scrubbed(&self) -> Self {
        Self {
            call_id: self.call_id.clone(),
            content: self.scrubbed_content(),
            is_error: self.is_error,
        }
    }

    /// One-line log-safe summary. Never includes raw content.
    #[must_use]
    pub fn log_safe(&self) -> String {
        format!(
            "ToolResult{{call_id={}, is_error={}, content={}}}",
            self.call_id,
            self.is_error,
            self.scrubbed_content()
        )
    }
}

/// In-crate stub registry that tracks declared tools and validates calls
/// syntactically without ever executing them.
///
/// Real execution (capability-checked, rate-limited, per-client scoped) will
/// be owned by the runtime/IPc layer. This registry is intentionally
/// headless and `std`-only so the vocabulary can be tested without a display
/// server, GPU, or network.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
}

impl ToolRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Create from specs (validates each).
    pub fn from_specs(specs: Vec<ToolSpec>) -> Result<Self, AgentError> {
        if specs.len() > MAX_TOOLS_PER_AGENT {
            return Err(AgentError::LimitExceeded {
                field: "tools per agent".to_string(),
                limit: MAX_TOOLS_PER_AGENT,
                actual: specs.len(),
            });
        }
        for s in &specs {
            s.validate()?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &specs {
            if !seen.insert(s.name.clone()) {
                return Err(AgentError::Duplicate {
                    kind: "tool".to_string(),
                    value: s.name.clone(),
                });
            }
        }
        Ok(Self { specs })
    }

    /// Insert a spec (validates, checks duplicates and cap).
    pub fn insert(&mut self, spec: ToolSpec) -> Result<(), AgentError> {
        spec.validate()?;
        if self.specs.len() >= MAX_TOOLS_PER_AGENT {
            return Err(AgentError::LimitExceeded {
                field: "tools per agent".to_string(),
                limit: MAX_TOOLS_PER_AGENT,
                actual: self.specs.len() + 1,
            });
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err(AgentError::Duplicate {
                kind: "tool".to_string(),
                value: spec.name,
            });
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Declared specs (read-only).
    #[must_use]
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Whether a tool name is declared.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// Syntactically validate a call against the registry (declared-name check +
    /// per-call bounds). No I/O, no execution.
    pub fn validate_call(&self, call: &ToolCall) -> Result<(), AgentError> {
        call.validate()?;
        if !self.contains(&call.name) {
            return Err(AgentError::Tool {
                message: format!("unknown tool '{}'", call.name),
            });
        }
        Ok(())
    }

    /// Stub `invoke` — never contacts an LLM or the filesystem. Returns a
    /// deterministic placeholder result that records the call as observed.
    ///
    /// Callers that need real tool execution must go through the capability-
    /// checked host outside this crate.
    pub fn stub_invoke(&self, call: &ToolCall) -> Result<ToolResult, AgentError> {
        self.validate_call(call)?;
        // Deterministic stub payload: no wall-clock, no randomness.
        let content = format!(
            "{{\"stub\":true,\"tool\":\"{}\",\"args_len\":{}}}",
            call.name,
            call.arguments.len()
        );
        ToolResult::new(call.id.clone(), content, false)
    }
}

fn validate_tool_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty() {
        return Err(AgentError::validation("tool name", "must not be empty"));
    }
    if name.len() > MAX_TOOL_NAME_LEN {
        return Err(AgentError::LimitExceeded {
            field: "tool name".to_string(),
            limit: MAX_TOOL_NAME_LEN,
            actual: name.len(),
        });
    }
    if name.contains('\0') {
        return Err(AgentError::validation("tool name", "must not contain NUL"));
    }
    // Grammar: start with [a-z], then [a-z0-9_.-]
    let first = name.as_bytes()[0];
    if !first.is_ascii_lowercase() {
        return Err(AgentError::validation(
            "tool name",
            "must start with lowercase letter",
        ));
    }
    for b in name.bytes() {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-') {
            return Err(AgentError::validation("tool name", "must be [a-z0-9_.-]"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "stub".into(),
            input_schema: "{}".into(),
        }
    }

    #[test]
    fn valid_tool_names() {
        for n in ["read_file", "run-command", "a.b", "tool1", "my-tool_2"] {
            validate_tool_name(n).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(validate_tool_name("").is_err());
        assert!(validate_tool_name("1tool").is_err());
        assert!(validate_tool_name("Tool").is_err());
        assert!(validate_tool_name("tool with space").is_err());
        assert!(validate_tool_name(&"a".repeat(MAX_TOOL_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn registry_insert_and_duplicate() {
        let mut r = ToolRegistry::new();
        r.insert(spec("read_file")).expect("insert");
        assert!(r.contains("read_file"));
        assert!(matches!(
            r.insert(spec("read_file")),
            Err(AgentError::Duplicate { .. })
        ));
    }

    #[test]
    fn validate_call_unknown_tool() {
        let r = ToolRegistry::from_specs(vec![spec("read_file")]).unwrap();
        let call = ToolCall::new("id1", "unknown_tool", "{}").unwrap();
        assert!(r.validate_call(&call).is_err());
    }

    #[test]
    fn stub_invoke_deterministic() {
        let r = ToolRegistry::from_specs(vec![spec("read_file")]).unwrap();
        let call = ToolCall::new("id1", "read_file", "{\"path\":\"/tmp/x\"}").unwrap();
        let res = r.stub_invoke(&call).unwrap();
        assert_eq!(res.call_id, "id1");
        assert!(!res.is_error);
        assert!(res.content.contains("\"stub\":true"));
        // Same call -> same result deterministically.
        let res2 = r.stub_invoke(&call).unwrap();
        assert_eq!(res, res2);
    }

    #[test]
    fn args_bytes_cap() {
        let big = "x".repeat(MAX_TOOL_ARGS_BYTES + 1);
        assert!(ToolCall::new("id", "read_file", big).is_err());
    }

    #[test]
    fn result_bytes_cap() {
        let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
        assert!(ToolResult::new("id", big, false).is_err());
    }

    #[test]
    fn scrub_redacts_password_in_json_args() {
        let call = ToolCall::new(
            "id1",
            "read_file",
            r#"{"path":"/tmp/x","password":"hunter2"}"#,
        )
        .unwrap();
        let scrubbed = call.scrubbed_arguments();
        assert!(!scrubbed.contains("hunter2"), "secret leaked: {scrubbed}");
        assert!(
            scrubbed.contains(REDACTED_MARKER),
            "missing marker: {scrubbed}"
        );
        assert!(scrubbed.contains("/tmp/x"), "legit value lost: {scrubbed}");
        // Raw is preserved for host dispatch; scrubbed clone is safe for IPC.
        assert!(call.arguments.contains("hunter2"));
        let ipc = call.scrubbed();
        assert!(!ipc.arguments.contains("hunter2"));
        assert_eq!(ipc.id, "id1");
    }

    #[test]
    fn scrub_redacts_token_in_result() {
        let res = ToolResult::new(
            "id1",
            r#"{"token":"ghp_abcdefgh12345678","ok":true}"#,
            false,
        )
        .unwrap();
        let scrubbed = res.scrubbed_content();
        assert!(
            !scrubbed.contains("ghp_abcdefgh12345678"),
            "leak: {scrubbed}"
        );
        assert!(
            scrubbed.contains(REDACTED_MARKER),
            "missing marker: {scrubbed}"
        );
        let ipc = res.scrubbed();
        assert!(!ipc.content.contains("ghp_"));
    }

    #[test]
    fn scrub_preserves_legit_values() {
        for legit in [
            r#"{"path":"/tmp/x"}"#,
            r#"{"command":"ls -la","cwd":"/home/user"}"#,
            r#"{"author":"jane","description":"list files"}"#,
            "{}",
        ] {
            let out = scrub_text(legit);
            assert_eq!(out, legit, "legit payload altered: {legit} -> {out}");
        }
        assert!(!is_sensitive_key("path"));
        assert!(!is_sensitive_key("author"));
        assert!(!is_sensitive_key("description"));
    }

    #[test]
    fn scrub_is_case_insensitive_and_covers_bare_pairs() {
        let out = scrub_text(r#"{"Password":"hunter2"}"#);
        assert!(!out.contains("hunter2"), "case leak: {out}");
        let out2 = scrub_text("api_key=hunter2-value-123");
        assert!(!out2.contains("hunter2"), "bare leak: {out2}");
        assert!(out2.contains(REDACTED_MARKER));
        let out3 = scrub_text("Authorization: Bearer abcdefgh12345678");
        assert!(!out3.contains("abcdefgh12345678"), "bearer leak: {out3}");
        assert!(out3.contains(REDACTED_MARKER));
    }

    #[test]
    fn scrub_redacts_pem_and_known_prefixes_anywhere() {
        let pem =
            "key data -----BEGIN PRIVATE KEY-----\nMIIEvgIBADAN\n-----END PRIVATE KEY----- tail";
        let out = scrub_text(pem);
        assert!(!out.contains("MIIEvgIBADAN"), "pem leak: {out}");
        assert!(out.contains(REDACTED_MARKER));
        let aws = "deploy with AKIAIOSFODNN7EXAMPLE now";
        let out2 = scrub_text(aws);
        assert!(!out2.contains("AKIAIOSFODNN7EXAMPLE"), "aws leak: {out2}");
        assert!(out2.contains(REDACTED_MARKER));
    }

    #[test]
    fn scrub_fail_closed_on_ambiguous_shapes() {
        // Nested object under a sensitive key: value shape unknown -> redacted.
        let nested = scrub_text(r#"{"password":{"nested":"hunter2"}}"#);
        assert!(!nested.contains("hunter2"), "nested leak: {nested}");
        assert!(nested.contains(REDACTED_MARKER));
        // Numeric secret under sensitive key: still redacted, not passed through.
        let num = scrub_text(r#"{"api_key":12345678}"#);
        assert!(!num.contains("12345678"), "numeric leak: {num}");
        assert!(num.contains(REDACTED_MARKER));
        // Unterminated string under sensitive key: whole payload redacted.
        let broken = scrub_text(r#"{"password":"hunter2}"#);
        assert_eq!(
            broken, REDACTED_MARKER,
            "ambiguous must redact all: {broken}"
        );
        // Single-quoted JSON-ish also covered.
        let single = scrub_text("{'token':'abcdefgh12345678'}");
        assert!(
            !single.contains("abcdefgh12345678"),
            "single-quote leak: {single}"
        );
    }

    #[test]
    fn scrub_debug_and_log_safe_never_leak() {
        let call = ToolCall::new("id1", "read_file", r#"{"password":"hunter2"}"#).unwrap();
        let debug = format!("{call:?}");
        assert!(!debug.contains("hunter2"), "debug leak: {debug}");
        assert!(debug.contains(REDACTED_MARKER));
        let log = call.log_safe();
        assert!(!log.contains("hunter2"), "log leak: {log}");
        let res = ToolResult::new("id1", "token=abcdefgh12345678", false).unwrap();
        let rdebug = format!("{res:?}");
        assert!(
            !rdebug.contains("abcdefgh12345678"),
            "result debug leak: {rdebug}"
        );
        let rlog = res.log_safe();
        assert!(
            !rlog.contains("abcdefgh12345678"),
            "result log leak: {rlog}"
        );
        // Raw PartialEq still compares raw bytes (storage), scrubbed compares redacted.
        assert_eq!(call, call.clone());
        assert_ne!(call.arguments, call.scrubbed_arguments());
    }

    #[test]
    fn scrub_output_stays_within_caps() {
        let call = ToolCall::new("id1", "read_file", r#"{"password":"x"}"#).unwrap();
        assert!(call.scrubbed_arguments().len() <= MAX_TOOL_ARGS_BYTES);
        let res = ToolResult::new("id1", r#"{"password":"x"}"#, false).unwrap();
        assert!(res.scrubbed_content().len() <= MAX_TOOL_RESULT_BYTES);
        assert!(scrub_tool_args(r#"{"password":"x"}"#).len() <= MAX_TOOL_ARGS_BYTES);
        assert!(scrub_tool_result("plain ok").len() <= MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn sensitive_key_heuristics() {
        for k in [
            "password",
            "Password",
            "api_key",
            "apiKey",
            "accessKey",
            "token",
            "auth",
            "authorization",
            "cookie",
            "private_key",
            "client_secret",
            "session_key",
        ] {
            assert!(is_sensitive_key(k), "should be sensitive: {k}");
        }
        for k in ["path", "command", "author", "description", "message", "cwd"] {
            assert!(!is_sensitive_key(k), "should pass through: {k}");
        }
    }
}
