//! `bitty ctl` control plane: pure validation + scope mapping (CTX-0171).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`ctl` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty ctl`, runtime class) and
//! `docs/specifications/ipc-agent-rfc.md` (scopes, instance selection).
//!
//! This module is pure data, bounded, headless, and `forbid(unsafe)`:
//! it owns no socket, spawns no thread, and performs no I/O. It defines the
//! control verbs that travel over the existing `BITTY_SOCKET` framing
//! (`bitty.debug/*` via [`crate::devtools`]) and maps each to its required
//! [`Scope`](crate::scope::Scope). Server-side authorization uses
//! [`authorize_ctl_method`], which never trusts client-asserted scopes:
//! the caller passes the server-evaluated [`ScopeSet`](crate::scope::ScopeSet)
//! derived from the authenticated peer identity, exactly like
//! [`crate::scope::authorize_method`].
//!
//! # Wire methods
//!
//! Control verbs reuse the `bitty.debug/` prefix so the existing
//! [`crate::devtools::Dispatcher`], framing (`u32` BE + `<= 256 KiB`),
//! JSON-depth cap (32), and `auth`/`scope`/`role` rejection apply unchanged:
//!
//! | `ctl` verb | wire method | required scope |
//! |---|---|---|
//! | `instance list` | local discovery, no IPC | none (same-UID discovery) |
//! | `window list` | `bitty.debug/listWindows` | `view.inspect` |
//! | `view list` | `bitty.debug/listViews` | `view.inspect` |
//! | `terminal list` | `bitty.debug/listTerminals` | `terminal.inspect` |
//! | `terminal spawn` | `bitty.debug/spawnTerminal` | `terminal.manage` (elevation) |
//! | `terminal close` | `bitty.debug/closeTerminal` | `terminal.manage` (elevation) |
//! | `terminal send` | `bitty.debug/sendInput` | `terminal.input` |
//! | `terminal text` | `bitty.debug/getTerminalText` | `terminal.inspect` |
//! | `view split` | `bitty.debug/splitView` | `view.manage` |
//! | `view focus` | `bitty.debug/focusView` | `view.manage` |
//! | `config reload` | `bitty.debug/reloadConfig` | `config.modify` (elevation) |
//!
//! `terminal.manage`, `config.modify` require explicit elevation per the IPC
//! RFC (confirmation prompt or pre-granted per-instance allowlist). Without
//! elevation the server denies with `Denied/ScopeViolation` (CLI exit 7) and
//! creates no partial state.
//!
//! # Bounds (T-01 parity, fail closed)
//!
//! - Terminal ids: `t:<1..10 digits>` (e.g. `t:3`). View ids: `v:<1..10 digits>`.
//! - Send text: 1..=`MAX_SEND_TEXT_BYTES` bytes, no NUL, valid UTF-8 (checked by caller).
//! - `--cwd`: 1..=`MAX_CTL_CWD_LEN` bytes, no NUL.
//! - Split direction: `--left` | `--right` | `--up` | `--down` (exactly one; default `--right`).
//! - All params objects are `<= MAX_CTL_PARAMS_BYTES` and depth-checked by the
//!   devtools envelope before reaching here.

#![forbid(unsafe_code)]

use crate::error::IpcError;
use crate::scope::{Scope, ScopeSet};

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes for `terminal send` text (fail-closed, well under frame bound).
pub const MAX_SEND_TEXT_BYTES: usize = 16 * 1024;

/// Maximum bytes for `terminal spawn --cwd`.
pub const MAX_CTL_CWD_LEN: usize = 4096;

/// Maximum bytes for a serialized control params object.
pub const MAX_CTL_PARAMS_BYTES: usize = 4096;

/// Maximum digits after `t:` / `v:` (u32 range with margin).
pub const MAX_CTL_ID_DIGITS: usize = 10;

// ── wire method names ─────────────────────────────────────────────────────

/// Wire method for `ctl window list`.
pub const METHOD_LIST_WINDOWS: &str = "bitty.debug/listWindows";
/// Wire method for `ctl view list`.
pub const METHOD_LIST_VIEWS: &str = "bitty.debug/listViews";
/// Wire method for `ctl terminal list`.
pub const METHOD_LIST_TERMINALS: &str = "bitty.debug/listTerminals";
/// Wire method for `ctl terminal spawn`.
pub const METHOD_SPAWN_TERMINAL: &str = "bitty.debug/spawnTerminal";
/// Wire method for `ctl terminal close`.
pub const METHOD_CLOSE_TERMINAL: &str = "bitty.debug/closeTerminal";
/// Wire method for `ctl terminal send`.
pub const METHOD_SEND_INPUT: &str = "bitty.debug/sendInput";
/// Wire method for `ctl terminal text`.
pub const METHOD_GET_TERMINAL_TEXT: &str = "bitty.debug/getTerminalText";
/// Wire method for `ctl view split`.
pub const METHOD_SPLIT_VIEW: &str = "bitty.debug/splitView";
/// Wire method for `ctl view focus`.
pub const METHOD_FOCUS_VIEW: &str = "bitty.debug/focusView";
/// Wire method for `ctl config reload`.
pub const METHOD_RELOAD_CONFIG: &str = "bitty.debug/reloadConfig";

/// All control wire methods (excluding local `instance list`).
#[must_use]
pub fn all_control_methods() -> &'static [&'static str] {
    &[
        METHOD_LIST_WINDOWS,
        METHOD_LIST_VIEWS,
        METHOD_LIST_TERMINALS,
        METHOD_SPAWN_TERMINAL,
        METHOD_CLOSE_TERMINAL,
        METHOD_SEND_INPUT,
        METHOD_GET_TERMINAL_TEXT,
        METHOD_SPLIT_VIEW,
        METHOD_FOCUS_VIEW,
        METHOD_RELOAD_CONFIG,
    ]
}

/// Map a control wire method to its required scope.
///
/// Returns `None` for unknown methods (fail-closed `NotFound`, no partial state).
#[must_use]
pub fn required_scope_for_ctl_method(method: &str) -> Option<Scope> {
    match method {
        METHOD_LIST_WINDOWS | METHOD_LIST_VIEWS => Some(Scope::ViewInspect),
        METHOD_LIST_TERMINALS | METHOD_GET_TERMINAL_TEXT => Some(Scope::TerminalInspect),
        METHOD_SEND_INPUT => Some(Scope::TerminalInput),
        METHOD_SPAWN_TERMINAL | METHOD_CLOSE_TERMINAL => Some(Scope::TerminalManage),
        METHOD_SPLIT_VIEW | METHOD_FOCUS_VIEW => Some(Scope::ViewManage),
        METHOD_RELOAD_CONFIG => Some(Scope::ConfigModify),
        _ => None,
    }
}

/// Server-side authorization for control methods.
///
/// Validates the `bitty.debug/*` grammar via the devtools prefix rule
/// (ASCII alphanumeric/`_`/`-` suffix, bounded) and denies unknown methods
/// with `NotFound`. Known methods require the mapped scope in `granted`;
/// otherwise denies with `ScopeDenied` (no partial state, fail-closed).
/// Clients never assert scopes: `granted` is the server-evaluated set.
///
/// # Errors
///
/// - `InvalidMethod` when the method violates the wire grammar.
/// - `NotFound` when the method is well-formed but not a control method.
/// - `ScopeDenied` when `granted` lacks the required scope.
pub fn authorize_ctl_method(method: &str, granted: &ScopeSet) -> Result<Scope, IpcError> {
    validate_ctl_method_name(method)?;
    let required = required_scope_for_ctl_method(method).ok_or_else(|| IpcError::NotFound {
        reason: format!("unknown control method '{method}'"),
    })?;
    if granted.contains(required) {
        Ok(required)
    } else {
        Err(IpcError::ScopeDenied {
            scope: required.as_str().into(),
            action: method.into(),
        })
    }
}

/// Validate a `bitty.debug/*` control method name (bounded, ASCII).
fn validate_ctl_method_name(method: &str) -> Result<(), IpcError> {
    const PREFIX: &str = "bitty.debug/";
    if method.len() > crate::devtools::MAX_DEVTOOLS_METHOD_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "method".into(),
            limit: crate::devtools::MAX_DEVTOOLS_METHOD_BYTES,
            actual: method.len(),
        });
    }
    let Some(suffix) = method.strip_prefix(PREFIX) else {
        return Err(IpcError::InvalidMethod {
            method: method.to_string(),
            reason: "control method must start with bitty.debug/".into(),
        });
    };
    if suffix.is_empty() || suffix.len() > crate::devtools::MAX_METHOD_SUFFIX_LEN {
        return Err(IpcError::InvalidMethod {
            method: method.to_string(),
            reason: "control method suffix must be 1..=64".into(),
        });
    }
    let ok = suffix
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(IpcError::InvalidMethod {
            method: method.to_string(),
            reason: "control method suffix must be ascii alphanumeric".into(),
        });
    }
    Ok(())
}

// ── id validation ─────────────────────────────────────────────────────────

/// Validate a terminal id (`t:<digits>`), returning the numeric id.
///
/// Shape-only: existence is resolved server-side (`NotFound` when absent).
pub fn parse_terminal_id(raw: &str) -> Result<u32, IpcError> {
    let digits = raw
        .strip_prefix("t:")
        .ok_or_else(|| IpcError::InvalidRequest {
            reason: format!("terminal id must match ^t:[0-9]+$, got '{raw}'"),
        })?;
    parse_id_digits(digits, "t")
}

/// Validate a view id (`v:<digits>`), returning the numeric id.
pub fn parse_view_id(raw: &str) -> Result<u32, IpcError> {
    let digits = raw
        .strip_prefix("v:")
        .ok_or_else(|| IpcError::InvalidRequest {
            reason: format!("view id must match ^v:[0-9]+$, got '{raw}'"),
        })?;
    parse_id_digits(digits, "v")
}

fn parse_id_digits(digits: &str, prefix: &str) -> Result<u32, IpcError> {
    if digits.is_empty() || digits.len() > MAX_CTL_ID_DIGITS {
        return Err(IpcError::InvalidRequest {
            reason: format!("{prefix}: id must be 1..={MAX_CTL_ID_DIGITS} digits"),
        });
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(IpcError::InvalidRequest {
            reason: format!("{prefix}: id must match ^{prefix}:[0-9]+$"),
        });
    }
    // Reject leading zeros (`t:007`) to keep `t:7` canonical; `t:0` itself
    // parses (existence resolves server-side to NotFound).
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(IpcError::InvalidRequest {
            reason: format!("{prefix}: id must not have leading zeros"),
        });
    }
    digits.parse::<u32>().map_err(|_| IpcError::InvalidRequest {
        reason: format!("{prefix}: id out of range"),
    })
}

/// Validate `terminal send` text (non-empty, bounded, no NUL).
pub fn validate_send_text(text: &str) -> Result<(), IpcError> {
    if text.is_empty() {
        return Err(IpcError::InvalidRequest {
            reason: "send text must be non-empty".into(),
        });
    }
    if text.len() > MAX_SEND_TEXT_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "text".into(),
            limit: MAX_SEND_TEXT_BYTES,
            actual: text.len(),
        });
    }
    if text.contains('\0') {
        return Err(IpcError::InvalidRequest {
            reason: "send text must not contain NUL".into(),
        });
    }
    Ok(())
}

/// Validate `terminal spawn --cwd` (non-empty, bounded, no NUL).
pub fn validate_ctl_cwd(cwd: &str) -> Result<(), IpcError> {
    if cwd.is_empty() || cwd.len() > MAX_CTL_CWD_LEN {
        return Err(IpcError::InvalidRequest {
            reason: format!("cwd must be 1..={MAX_CTL_CWD_LEN} bytes"),
        });
    }
    if cwd.contains('\0') {
        return Err(IpcError::InvalidRequest {
            reason: "cwd must not contain NUL".into(),
        });
    }
    Ok(())
}

// ── split direction ───────────────────────────────────────────────────────

/// Split direction for `ctl view split`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SplitDirection {
    /// Canonical wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    /// Parse a direction token (`left|right|up|down`, case-insensitive).
    pub fn parse(raw: &str) -> Result<Self, IpcError> {
        match raw.to_ascii_lowercase().as_str() {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            _ => Err(IpcError::InvalidRequest {
                reason: format!("split direction must be left|right|up|down, got '{raw}'"),
            }),
        }
    }
}

// ── params builders (client) ──────────────────────────────────────────────

/// Escape a string as JSON string content (no surrounding quotes).
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

/// Build `{ "terminal_id": "t:N" }` params for close/text.
#[must_use]
pub fn params_terminal_id(terminal_id: &str) -> String {
    let mut out = String::from("{\"terminal_id\":\"");
    json_escape_into(&mut out, terminal_id);
    out.push_str("\"}");
    out
}

/// Build `{ "terminal_id": "t:N", "text": "..." }` params for send.
#[must_use]
pub fn params_send_input(terminal_id: &str, text: &str) -> String {
    let mut out = String::from("{\"terminal_id\":\"");
    json_escape_into(&mut out, terminal_id);
    out.push_str("\",\"text\":\"");
    json_escape_into(&mut out, text);
    out.push_str("\"}");
    out
}

/// Build `{ "cwd": "..." }` or `{}` params for spawn.
#[must_use]
pub fn params_spawn(cwd: Option<&str>) -> String {
    match cwd {
        None => String::from("{}"),
        Some(dir) => {
            let mut out = String::from("{\"cwd\":\"");
            json_escape_into(&mut out, dir);
            out.push_str("\"}");
            out
        }
    }
}

/// Build `{ "direction": "right" }` params for split.
#[must_use]
pub fn params_split(direction: SplitDirection) -> String {
    format!("{{\"direction\":\"{}\"}}", direction.as_str())
}

/// Build `{ "view_id": "v:N" }` params for focus.
#[must_use]
pub fn params_focus(view_id: &str) -> String {
    let mut out = String::from("{\"view_id\":\"");
    json_escape_into(&mut out, view_id);
    out.push_str("\"}");
    out
}

// ── params parsing (server, bounded, no new deps) ─────────────────────────

/// Extract a top-level string field from a flat params object.
///
/// Bounded, quote-aware, backslash-aware; rejects nested objects for the
/// requested key (control params are flat). Returns `None` when absent.
fn extract_string_field(params: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search = 0usize;
    let bytes = params.as_bytes();
    while let Some(pos) = params[search..].find(&needle) {
        let abs = search + pos;
        // Key must be followed by optional ws + `:`.
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
                            // Minimal \uXXXX (BMP only, no surrogate handling:
                            // control params never need astral escapes).
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
                    // Raw UTF-8: advance by char.
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

/// Parse `close`/`text` params (`{ "terminal_id": "t:N" }`).
pub fn parse_terminal_id_params(params: Option<&str>) -> Result<String, IpcError> {
    let raw = params.ok_or_else(|| IpcError::InvalidRequest {
        reason: "missing params.terminal_id".into(),
    })?;
    if raw.len() > MAX_CTL_PARAMS_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "params".into(),
            limit: MAX_CTL_PARAMS_BYTES,
            actual: raw.len(),
        });
    }
    let id = extract_string_field(raw, "terminal_id").ok_or_else(|| IpcError::InvalidRequest {
        reason: "params.terminal_id must be a string like \"t:3\"".into(),
    })?;
    parse_terminal_id(&id)?;
    Ok(id)
}

/// Parse `send` params (`{ "terminal_id": "t:N", "text": "..." }`).
pub fn parse_send_params(params: Option<&str>) -> Result<(String, String), IpcError> {
    let raw = params.ok_or_else(|| IpcError::InvalidRequest {
        reason: "missing params.terminal_id/params.text".into(),
    })?;
    if raw.len() > MAX_CTL_PARAMS_BYTES + MAX_SEND_TEXT_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "params".into(),
            limit: MAX_CTL_PARAMS_BYTES + MAX_SEND_TEXT_BYTES,
            actual: raw.len(),
        });
    }
    let id = extract_string_field(raw, "terminal_id").ok_or_else(|| IpcError::InvalidRequest {
        reason: "params.terminal_id must be a string like \"t:1\"".into(),
    })?;
    parse_terminal_id(&id)?;
    let text = extract_string_field(raw, "text").ok_or_else(|| IpcError::InvalidRequest {
        reason: "params.text must be a non-empty string".into(),
    })?;
    validate_send_text(&text)?;
    Ok((id, text))
}

/// Parse `spawn` params (`{}` or `{ "cwd": "..." }`).
pub fn parse_spawn_params(params: Option<&str>) -> Result<Option<String>, IpcError> {
    let Some(raw) = params else {
        return Ok(None);
    };
    if raw.len() > MAX_CTL_PARAMS_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "params".into(),
            limit: MAX_CTL_PARAMS_BYTES,
            actual: raw.len(),
        });
    }
    match extract_string_field(raw, "cwd") {
        None => Ok(None),
        Some(cwd) => {
            validate_ctl_cwd(&cwd)?;
            Ok(Some(cwd))
        }
    }
}

/// Parse `split` params (`{ "direction": "left|right|up|down" }`, default right).
pub fn parse_split_params(params: Option<&str>) -> Result<SplitDirection, IpcError> {
    let Some(raw) = params else {
        return Ok(SplitDirection::Right);
    };
    if raw.len() > MAX_CTL_PARAMS_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "params".into(),
            limit: MAX_CTL_PARAMS_BYTES,
            actual: raw.len(),
        });
    }
    match extract_string_field(raw, "direction") {
        None => Ok(SplitDirection::Right),
        Some(dir) => SplitDirection::parse(&dir),
    }
}

/// Parse `focus` params (`{ "view_id": "v:N" }`).
pub fn parse_focus_params(params: Option<&str>) -> Result<String, IpcError> {
    let raw = params.ok_or_else(|| IpcError::InvalidRequest {
        reason: "missing params.view_id".into(),
    })?;
    if raw.len() > MAX_CTL_PARAMS_BYTES {
        return Err(IpcError::LimitExceeded {
            field: "params".into(),
            limit: MAX_CTL_PARAMS_BYTES,
            actual: raw.len(),
        });
    }
    let id = extract_string_field(raw, "view_id").ok_or_else(|| IpcError::InvalidRequest {
        reason: "params.view_id must be a string like \"v:3\"".into(),
    })?;
    parse_view_id(&id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::ScopeSet;

    #[test]
    fn control_methods_map_to_expected_scopes() {
        assert_eq!(
            required_scope_for_ctl_method(METHOD_SEND_INPUT),
            Some(Scope::TerminalInput)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_CLOSE_TERMINAL),
            Some(Scope::TerminalManage)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_SPAWN_TERMINAL),
            Some(Scope::TerminalManage)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_SPLIT_VIEW),
            Some(Scope::ViewManage)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_FOCUS_VIEW),
            Some(Scope::ViewManage)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_RELOAD_CONFIG),
            Some(Scope::ConfigModify)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_LIST_TERMINALS),
            Some(Scope::TerminalInspect)
        );
        assert_eq!(
            required_scope_for_ctl_method(METHOD_GET_TERMINAL_TEXT),
            Some(Scope::TerminalInspect)
        );
        assert_eq!(required_scope_for_ctl_method("bitty.debug/nope"), None);
    }

    #[test]
    fn cli_default_allows_send_split_focus_text_but_not_close_spawn_reload() {
        let cli = ScopeSet::cli_default();
        // Allowed without elevation.
        assert!(authorize_ctl_method(METHOD_SEND_INPUT, &cli).is_ok());
        assert!(authorize_ctl_method(METHOD_SPLIT_VIEW, &cli).is_ok());
        assert!(authorize_ctl_method(METHOD_FOCUS_VIEW, &cli).is_ok());
        assert!(authorize_ctl_method(METHOD_GET_TERMINAL_TEXT, &cli).is_ok());
        assert!(authorize_ctl_method(METHOD_LIST_TERMINALS, &cli).is_ok());
        // Require explicit elevation: unscoped CLI callers are rejected.
        assert!(authorize_ctl_method(METHOD_CLOSE_TERMINAL, &cli).is_err());
        assert!(authorize_ctl_method(METHOD_SPAWN_TERMINAL, &cli).is_err());
        assert!(authorize_ctl_method(METHOD_RELOAD_CONFIG, &cli).is_err());
    }

    #[test]
    fn unscoped_callers_rejected_for_every_control_op() {
        let empty = ScopeSet::new();
        for method in all_control_methods() {
            let err = authorize_ctl_method(method, &empty).unwrap_err();
            assert!(
                matches!(err, IpcError::ScopeDenied { .. }),
                "{method} with empty scopes must be ScopeDenied, got {err:?}"
            );
        }
    }

    #[test]
    fn mcp_readonly_cannot_send_or_manage() {
        let mcp = ScopeSet::mcp_default();
        assert!(authorize_ctl_method(METHOD_LIST_TERMINALS, &mcp).is_ok());
        assert!(authorize_ctl_method(METHOD_SEND_INPUT, &mcp).is_err());
        assert!(authorize_ctl_method(METHOD_CLOSE_TERMINAL, &mcp).is_err());
        assert!(authorize_ctl_method(METHOD_RELOAD_CONFIG, &mcp).is_err());
    }

    #[test]
    fn elevated_all_allows_every_control_op() {
        let all = ScopeSet::all();
        for method in all_control_methods() {
            assert!(
                authorize_ctl_method(method, &all).is_ok(),
                "{method} with all scopes must succeed"
            );
        }
    }

    #[test]
    fn terminal_and_view_ids_validate_shape_only() {
        assert_eq!(parse_terminal_id("t:3").unwrap(), 3);
        assert_eq!(parse_view_id("v:12").unwrap(), 12);
        assert!(parse_terminal_id("t:0").is_ok());
        assert!(parse_terminal_id("t:").is_err());
        assert!(parse_terminal_id("t:007").is_err());
        assert!(parse_terminal_id("3").is_err());
        assert!(parse_terminal_id("t:abc").is_err());
        assert!(parse_terminal_id("t:1;rm").is_err());
        assert!(parse_view_id("v:").is_err());
        assert!(parse_view_id("t:3").is_err());
    }

    #[test]
    fn send_text_bounds_hold() {
        assert!(validate_send_text("cargo test").is_ok());
        assert!(validate_send_text("").is_err());
        assert!(validate_send_text("a\0b").is_err());
        let big = "x".repeat(MAX_SEND_TEXT_BYTES + 1);
        assert!(validate_send_text(&big).is_err());
    }

    #[test]
    fn send_params_roundtrip() {
        let params = params_send_input("t:1", "cargo test");
        let (id, text) = parse_send_params(Some(&params)).unwrap();
        assert_eq!(id, "t:1");
        assert_eq!(text, "cargo test");
    }

    #[test]
    fn split_params_default_right() {
        assert_eq!(parse_split_params(None).unwrap(), SplitDirection::Right);
        let params = params_split(SplitDirection::Left);
        assert_eq!(
            parse_split_params(Some(&params)).unwrap(),
            SplitDirection::Left
        );
        assert!(parse_split_params(Some("{\"direction\":\"diagonal\"}")).is_err());
    }

    #[test]
    fn unknown_control_method_is_not_found_not_ambient() {
        let cli = ScopeSet::cli_default();
        let err = authorize_ctl_method("bitty.debug/rmRf", &cli).unwrap_err();
        assert!(matches!(err, IpcError::NotFound { .. }));
    }

    #[test]
    fn denial_returns_permission_without_enqueue() {
        // CTX-0231: a scope denial must fail fast as a permission error and
        // must never touch the control queue (no enqueue means no 5 s drain
        // wait, so a denial can never surface as a timeout). No timing
        // asserts: the queue-emptiness check is the proof.
        while pop_pending_control().is_some() {}
        let empty = ScopeSet::new();
        for method in all_control_methods() {
            let required = required_scope_for_ctl_method(method).expect("known method");
            let reply = enqueue_control_and_wait(method, None, "1", &empty);
            assert!(!reply.ok, "{method} with empty scopes must fail");
            assert_eq!(
                reply.category, "auth",
                "{method} denial must be auth, got {:?}",
                reply.category
            );
            assert_eq!(
                reply.code, "ScopeDenied",
                "{method} denial must be ScopeDenied, got {:?}",
                reply.code
            );
            assert!(
                reply.message.contains(required.as_str()),
                "{method} denial must name scope '{}', got {:?}",
                required.as_str(),
                reply.message
            );
            assert!(
                reply.message.contains("BITTY_CTL_ELEVATE"),
                "{method} denial must name the elevation surface, got {:?}",
                reply.message
            );
            assert!(
                !reply.message.contains("timed out"),
                "{method} denial must never read as a timeout, got {:?}",
                reply.message
            );
        }
        assert!(
            pop_pending_control().is_none(),
            "denials must not enqueue (nothing to drain, nothing to time out)"
        );
    }
}

// ── elevation allowlist (pre-granted per-instance, explicit) ───────────────

/// Server-side elevation allowlist from `BITTY_CTL_ELEVATE`.
///
/// Comma-separated scopes (e.g. `terminal.manage,config.modify`); unknown
/// names are ignored (fail-closed: they grant nothing). Starts from the CLI
/// default and adds each named scope. Empty/unset means no elevation.
#[must_use]
pub fn elevation_from_env(raw: Option<&str>) -> ScopeSet {
    use std::str::FromStr as _;
    let mut set = ScopeSet::cli_default();
    let Some(list) = raw else {
        return set;
    };
    for token in list.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Ok(scope) = Scope::from_str(token) {
            set.insert(scope);
        }
    }
    set
}

// ── cross-thread control queue (servo producers, main-thread consumer) ─────
//
// `Runtime` is `!Send`, so it never crosses threads: IPC connection threads
// (via `devtools` control handlers) enqueue [`PendingControl`] (pure data +
// reply channel) and block on the reply; the main thread (sole `Runtime`
// owner) drains via `pop_pending_control` and applies each action, then
// replies. Bounded (drop-newest at cap, fail-closed).

/// Maximum queued control actions (drop-newest past this, fail-closed).
pub const MAX_QUEUED_CONTROLS: usize = 64;

/// One queued control action (all `Send`; `Runtime` never crosses threads).
#[derive(Debug)]
pub struct PendingControl {
    /// Wire method (e.g. `bitty.debug/sendInput`).
    pub method: String,
    /// Raw params object (if any).
    pub params: Option<String>,
    /// Verbatim numeric id token for response correlation.
    pub id_raw: String,
    /// Reply channel back to the connection thread.
    pub reply: std::sync::mpsc::Sender<ControlReply>,
}

/// Synchronous control result for the reply channel (all `Send`).
#[derive(Debug, Clone)]
pub struct ControlReply {
    /// Whether the action succeeded.
    pub ok: bool,
    /// Result JSON on success (already bounded).
    pub result_json: String,
    /// Error category on failure.
    pub category: &'static str,
    /// Error code on failure.
    pub code: &'static str,
    /// Human message on failure.
    pub message: String,
}

/// Global control queue shared between IPC connection threads and the main thread.
pub fn global_control_queue()
-> &'static std::sync::Mutex<std::collections::VecDeque<PendingControl>> {
    static QUEUE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::VecDeque<PendingControl>>,
    > = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// Pop one queued action (main-thread consumer).
pub fn pop_pending_control() -> Option<PendingControl> {
    global_control_queue()
        .lock()
        .map(|mut q| q.pop_front())
        .unwrap_or(None)
}

/// Clear the queue (test hook only; drops pending replies).
#[cfg(test)]
pub fn clear_control_queue_for_tests() {
    if let Ok(mut guard) = global_control_queue().lock() {
        guard.clear();
    }
}

/// Enqueue a control action and wait for the main thread to apply it.
///
/// Called on IPC connection threads (via `devtools` handlers). Authorizes
/// via `granted` before enqueue (fail-closed, no partial state); the main
/// thread re-authorizes at apply (defense in depth). Waits up to 5 s for
/// the reply; timeout or a full queue becomes `Unavailable`.
pub fn enqueue_control_and_wait(
    method: &str,
    params: Option<&str>,
    id_raw: &str,
    granted: &ScopeSet,
) -> ControlReply {
    if let Err(ipc_err) = authorize_ctl_method(method, granted) {
        let (category, code, message) = match ipc_err {
            IpcError::ScopeDenied { .. } => (
                "auth",
                "ScopeDenied",
                format!("permission denied: {ipc_err} (needs elevation via BITTY_CTL_ELEVATE)"),
            ),
            // Auth-family failures are permission errors (CLI exit 7), never
            // transport timeouts or usage errors: name the denial and the
            // elevation surface so operators never read them as timeouts.
            IpcError::Denied { code, reason } => (
                "auth",
                "Denied",
                format!(
                    "permission denied: [{code}] {reason} (needs elevation via BITTY_CTL_ELEVATE)"
                ),
            ),
            IpcError::Unauthenticated { .. } => (
                "auth",
                "Unauthenticated",
                format!("permission denied: {ipc_err}"),
            ),
            IpcError::NotFound { .. } => ("usage", "NotFound", format!("{ipc_err}")),
            _ => ("usage", "InvalidMethod", format!("{ipc_err}")),
        };
        return ControlReply {
            ok: false,
            result_json: String::new(),
            category,
            code,
            message,
        };
    }
    let (tx, rx) = std::sync::mpsc::channel::<ControlReply>();
    let pending = PendingControl {
        method: method.to_string(),
        params: params.map(str::to_string),
        id_raw: id_raw.to_string(),
        reply: tx,
    };
    {
        let queue = global_control_queue();
        let mut guard = queue.lock().unwrap_or_else(|poison| poison.into_inner());
        if guard.len() >= MAX_QUEUED_CONTROLS {
            return ControlReply {
                ok: false,
                result_json: String::new(),
                category: "budget",
                code: "RateLimited",
                message: String::from("control queue full; try again"),
            };
        }
        guard.push_back(pending);
    }
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(reply) => reply,
        Err(_) => ControlReply {
            ok: false,
            result_json: String::new(),
            category: "transport",
            code: "Unavailable",
            message: String::from("control timed out (no live runtime draining)"),
        },
    }
}
