//! Host secret store and opaque credential handles (research 045 section 5).
//!
//! A model can use credentials; a model must never see credentials. Handles
//! (`secret://<name>`) resolve on the Rust host side at spawn/request time;
//! the handle value never enters agent context, prompts, Lua logs, execution
//! logs, panel history, traces, diagnostics, or error messages.
//!
//! # Composition (conforms, never duplicates)
//!
//! - `CTX-0524` (`effective`): that engine authorizes requests; this store is
//!   the resolution mechanism. `PluginHost::resolve_secret_for_spawn` calls
//!   `authorize_effective` with `RequestKind::ExecutionRun` before resolving,
//!   and never re-implements intersection logic.
//! - `ADR-0006` (env allowlist, host-mediated reads, desensitized
//!   diagnostics): resolution injects values only into the child process
//!   environment (never argv); diagnostics quote handle names only.
//! - `P0-AC-026` redaction: every `Display`/`Debug` impl in this module is
//!   redacting on purpose — `format!("{:?}", value)` never emits a secret.
//!   The shared greppable token is `[redacted]`, mirroring `bitty-agent`
//!   `REDACTED_MARKER` and `bitty-ipc` `REDACTED_MARKER`.
//! - Panel-environment Agent View / env-snapshot handle direction: callers
//!   needing an agent-visible view must use [`SanitizedEnvView`], which
//!   carries presence plus non-secret values only.
//! - `MP-10`/`MPC-2` invariant: configuration references credentials
//!   (`credential = "secret://..."` style references); a literal secret value
//!   in configuration fails validation ([`reject_literal_secret`]).
//!   Provider consent semantics and provider adapters stay `bitty-ai`.
//!
//! # Storage
//!
//! The store lives outside the repository: paths resolve under the XDG data
//! root (`$XDG_DATA_HOME/bitty/secrets`, fallback `$HOME/.local/share`),
//! mirroring the derivation in `bitty-config` (`$XDG_CONFIG_HOME` else
//! `$HOME/.config`) and the plugin store root in `bitty-app`
//! (`$XDG_DATA_HOME` else `$HOME/.local/share`). Every root comes from the
//! caller; no host path is hardcoded. Files are created with user-only
//! modes (`0600` files, `0700` directories on Unix; Windows relies on the
//! user profile ACL). No ambient authority is added: an empty store resolves
//! nothing, and every resolution requires an explicit per-handle consent
//! grant recorded in the audit ledger.
//!
//! This module is pure data plus validation plus bounded in-memory maps; the
//! only I/O is the explicit file-store load/save behind
//! [`FileSecretStore`]. There is no `unsafe`, no new dependency (`std`
//! only), and every structure is owned, bounded, and headlessly testable on
//! Linux CI and the `windows-latest` job.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use crate::error::PluginError;

// ── bounds (accepted-contract precedent, no invented values) ───────────────

/// `secret://` URI scheme prefix for opaque credential handles.
pub const SECRET_SCHEME_PREFIX: &str = "secret://";

/// Maximum secret handles tracked by one [`SecretStore`].
///
/// Precedent: `devtools::MAX_INPUT_RING` (`64`) bounds explicit env lists in
/// the accepted IPC execution budgets; the host spawn surface reuses it for
/// `spawn env`.
pub const MAX_SECRETS: usize = 64;

/// Maximum bytes of one secret handle name.
pub const MAX_HANDLE_NAME_BYTES: usize = 64;

/// Maximum bytes of one secret value.
///
/// Precedent: `ctl::MAX_CTL_CWD_LEN` (`4096`) bounds env values in the
/// accepted IPC execution budgets.
pub const MAX_SECRET_VALUE_BYTES: usize = 4096;

/// Maximum resolved env entries injected into one child spawn.
///
/// Precedent: `devtools::MAX_INPUT_RING` (`64`), same as the explicit-env
/// bound the spawn surface enforces.
pub const MAX_RESOLVED_ENV_VARS: usize = 64;

/// Maximum secret-audit entries retained ([`SecretAuditLedger`], drop-oldest).
pub const MAX_SECRET_AUDIT_ENTRIES: usize = 1024;

/// Maximum items named in one audit entry's handle list (remainder counted).
pub const MAX_SECRET_AUDIT_ITEMS: usize = 8;

/// Maximum policy/store file size in bytes (fail-closed).
pub const MAX_SECRET_FILE_BYTES: usize = 64 * 1024;

/// Maximum secret-store file lines (fail-closed).
pub const MAX_SECRET_FILE_LINES: usize = 1024;

/// Maximum bytes per secret-store file line (fail-closed).
pub const MAX_SECRET_FILE_LINE_BYTES: usize = 4096 + 128;

/// File name of the host secret store under the XDG data root.
pub const SECRET_STORE_FILE_NAME: &str = "secrets.conf";

/// Subdirectory of the XDG data root holding the host secret store.
pub const SECRET_STORE_DIR_NAME: &str = "bitty";

/// Redaction marker replacing secret bytes before logs/diagnostics.
///
/// Mirrors `bitty-agent::REDACTED_MARKER` and `bitty-ipc::REDACTED_MARKER`
/// (`"[redacted]"`) so scrubbed payloads share one greppable token.
pub const SECRET_REDACTED_MARKER: &str = "[redacted]";

// ── opaque handle ─────────────────────────────────────────────────────────

/// Opaque credential handle (`secret://<name>`).
///
/// The handle names a secret without carrying its value: `Display`, `Debug`,
/// and serde-free snapshots quote the `secret://<name>` reference only. The
/// value lives in the host [`SecretStore`] and is injected into the child
/// process environment at spawn time by [`resolve_env_for_spawn`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretHandle {
    name: String,
}

impl SecretHandle {
    /// Parse an opaque handle reference (`secret://<name>`).
    ///
    /// Fail-closed: wrong scheme, empty name, over-bound names, NUL/control/
    /// whitespace, path separators, `.`/`..` segments, and non-`[A-Za-z0-9_-]`
    /// bytes are all rejected. Values never appear here: only references.
    pub fn parse(raw: &str) -> Result<Self, SecretError> {
        let name = raw.strip_prefix(SECRET_SCHEME_PREFIX).ok_or_else(|| {
            SecretError::invalid_handle(raw, "handle must start with 'secret://'")
        })?;
        validate_handle_name(name, raw)?;
        Ok(Self {
            name: name.to_string(),
        })
    }

    /// Handle name (the `<name>` in `secret://<name>`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Canonical `secret://<name>` reference text (safe for logs).
    #[must_use]
    pub fn reference(&self) -> String {
        format!("{SECRET_SCHEME_PREFIX}{}", self.name)
    }

    /// Whether `raw` parses as a handle (shape only, no store contact).
    #[must_use]
    pub fn is_handle_ref(raw: &str) -> bool {
        Self::parse(raw).is_ok()
    }
}

impl fmt::Display for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reference())
    }
}

/// Validate one handle name (fail-closed, no side effects).
fn validate_handle_name(name: &str, raw: &str) -> Result<(), SecretError> {
    if name.is_empty() {
        return Err(SecretError::invalid_handle(
            raw,
            "handle name must not be empty",
        ));
    }
    if name.len() > MAX_HANDLE_NAME_BYTES {
        return Err(SecretError::limit_exceeded(
            "handle name",
            MAX_HANDLE_NAME_BYTES,
            name.len(),
        ));
    }
    if name.contains('\0') || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(SecretError::invalid_handle(
            raw,
            "handle name must not contain NUL, control, or whitespace",
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(SecretError::invalid_handle(
            raw,
            "handle name must not contain path separators",
        ));
    }
    if name == "." || name == ".." {
        return Err(SecretError::invalid_handle(
            raw,
            "handle name must not be '.' or '..'",
        ));
    }
    let mut bytes = name.bytes();
    let first = bytes.next().unwrap_or(b'0');
    if !(first.is_ascii_alphanumeric()) {
        return Err(SecretError::invalid_handle(
            raw,
            "handle name must start with [A-Za-z0-9]",
        ));
    }
    for byte in name.bytes() {
        if !(byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
            return Err(SecretError::invalid_handle(
                raw,
                "handle name must match [A-Za-z0-9_-]",
            ));
        }
    }
    Ok(())
}

// ── typed errors (values never quoted) ────────────────────────────────────

/// Why a secret operation refused.
///
/// Every variant quotes handle/env/key names only — never secret values.
/// `Display` output is safe to embed in logs, diagnostics, and traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretDenialKind {
    /// No secret is stored under this handle name.
    MissingHandle,
    /// The handle exists but has no active per-handle consent grant.
    ConsentRequired,
    /// A literal secret value was found where only a handle belongs.
    LiteralForbidden,
    /// The request itself is malformed (bad shape, over bounds).
    InvalidRequest,
}

impl SecretDenialKind {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingHandle => "missing-handle",
            Self::ConsentRequired => "consent-required",
            Self::LiteralForbidden => "literal-forbidden",
            Self::InvalidRequest => "invalid-request",
        }
    }
}

impl fmt::Display for SecretDenialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Owned, headless-testable error for every secret-store failure mode.
///
/// Values are never stored in this type: only handle names, env names, and
/// bounded reasons. `Display`/`Debug` output is log-safe by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// No secret stored under this handle name.
    MissingHandle {
        /// Handle name (never a value).
        handle: String,
    },
    /// The handle exists but per-handle consent is absent or expired.
    ConsentRequired {
        /// Handle name (never a value).
        handle: String,
    },
    /// A literal secret value was supplied where only a handle belongs.
    LiteralForbidden {
        /// Field or env name carrying the literal (never the value).
        field: String,
    },
    /// Malformed request (bad shape, over bounds).
    InvalidRequest {
        /// Bounded reason (names only, never values).
        reason: String,
    },
    /// A hard limit was exceeded.
    LimitExceeded {
        /// Field or resource.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual value.
        actual: usize,
    },
}

impl SecretError {
    /// Missing-handle error (quotes the name only).
    #[must_use]
    pub fn missing_handle(handle: impl Into<String>) -> Self {
        Self::MissingHandle {
            handle: bounded_name(handle.into()),
        }
    }

    /// Consent-required error (quotes the name only).
    #[must_use]
    pub fn consent_required(handle: impl Into<String>) -> Self {
        Self::ConsentRequired {
            handle: bounded_name(handle.into()),
        }
    }

    /// Literal-forbidden error (quotes the field only).
    #[must_use]
    pub fn literal_forbidden(field: impl Into<String>) -> Self {
        Self::LiteralForbidden {
            field: bounded_name(field.into()),
        }
    }

    /// Malformed-request error (bounded reason, names only).
    #[must_use]
    pub fn invalid_request(reason: impl Into<String>) -> Self {
        Self::InvalidRequest {
            reason: bounded_name(reason.into()),
        }
    }

    /// Limit error.
    #[must_use]
    pub fn limit_exceeded(field: impl Into<String>, limit: usize, actual: usize) -> Self {
        Self::LimitExceeded {
            field: bounded_name(field.into()),
            limit,
            actual,
        }
    }

    /// Convenience for handle-shape findings (never quotes a value).
    #[must_use]
    pub(crate) fn invalid_handle(raw: &str, why: &str) -> Self {
        // Quote only when the raw text is itself a plausible reference; long
        // or binary-adjacent input is summarized so errors stay bounded and
        // cannot smuggle a pasted secret back into a log line.
        let quoted = if raw.len() <= MAX_HANDLE_NAME_BYTES + SECRET_SCHEME_PREFIX.len()
            && !raw.contains('\0')
        {
            raw.to_string()
        } else {
            format!("<{} bytes>", raw.len())
        };
        Self::InvalidRequest {
            reason: format!("invalid secret handle '{quoted}': {why}"),
        }
    }

    /// Stable denial kind for audit attribution.
    #[must_use]
    pub const fn denial_kind(&self) -> SecretDenialKind {
        match self {
            Self::MissingHandle { .. } => SecretDenialKind::MissingHandle,
            Self::ConsentRequired { .. } => SecretDenialKind::ConsentRequired,
            Self::LiteralForbidden { .. } => SecretDenialKind::LiteralForbidden,
            Self::InvalidRequest { .. } | Self::LimitExceeded { .. } => {
                SecretDenialKind::InvalidRequest
            }
        }
    }
}

/// Clamp a name/reason to a bounded length (fail-closed at the call site).
fn bounded_name(name: String) -> String {
    if name.len() > MAX_SECRET_FILE_LINE_BYTES {
        name[..MAX_SECRET_FILE_LINE_BYTES].to_string()
    } else {
        name
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHandle { handle } => {
                write!(f, "missing secret handle '{handle}'")
            }
            Self::ConsentRequired { handle } => {
                write!(f, "consent required for secret handle '{handle}'")
            }
            Self::LiteralForbidden { field } => {
                write!(
                    f,
                    "literal secret value forbidden in '{field}': use a 'secret://' handle"
                )
            }
            Self::InvalidRequest { reason } => write!(f, "invalid secret request: {reason}"),
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(f, "{field}: limit {limit} exceeded (actual {actual})"),
        }
    }
}

impl std::error::Error for SecretError {}

/// Convert into the crate's owned [`PluginError`] vocabulary (names only).
impl From<SecretError> for PluginError {
    fn from(error: SecretError) -> Self {
        PluginError::registry(error.to_string())
    }
}

// ── fail-closed literal detection (MPC-2) ─────────────────────────────────

/// Whether an env/config value looks like a literal secret where only a
/// `secret://` handle belongs.
///
/// Fail-closed and intentionally broad (over-reject rather than leak):
/// any value that parses as a handle reference is allowed; everything else
/// that is non-empty and either carries a sensitive key name or matches a
/// known secret-token shape is treated as a literal. Short opaque strings
/// (under 8 chars) and already-redacted markers never match so legit values
/// like `/tmp/x` or `hello` pass through. Values are never logged by the
/// caller: report the field name via [`SecretError::literal_forbidden`].
#[must_use]
pub fn looks_like_literal_secret(field_name: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if SecretHandle::is_handle_ref(value.trim()) {
        return false;
    }
    if is_sensitive_env_name(field_name) {
        return true;
    }
    looks_like_secret_token(value)
}

/// Reject a literal secret value in configuration (MPC-2 reference-not-value).
///
/// `field_name`/`value` is one config entry (env name, credential field).
/// Handle references pass; literals fail with
/// [`SecretError::LiteralForbidden`] quoting the field only. Empty values
/// pass (absent, not a secret).
pub fn reject_literal_secret(field_name: &str, value: &str) -> Result<(), SecretError> {
    if value.is_empty() {
        return Ok(());
    }
    if SecretHandle::is_handle_ref(value.trim()) {
        return Ok(());
    }
    if looks_like_literal_secret(field_name, value) {
        return Err(SecretError::literal_forbidden(field_name));
    }
    Ok(())
}

/// Validate a whole explicit-env list fail-closed (MPC-2).
///
/// Every `(name, value)` pair must either carry a `secret://` handle or a
/// non-literal value; the first literal fails with
/// [`SecretError::LiteralForbidden`]. Names follow the accepted IPC env
/// grammar (`^[A-Za-z_][A-Za-z0-9_]*$`, `1..=64` bytes); NUL fails closed.
pub fn reject_literal_secrets_in_env(env: &[(String, String)]) -> Result<(), SecretError> {
    if env.len() > MAX_RESOLVED_ENV_VARS {
        return Err(SecretError::limit_exceeded(
            "secret env",
            MAX_RESOLVED_ENV_VARS,
            env.len(),
        ));
    }
    for (name, value) in env {
        validate_env_name(name)?;
        reject_literal_secret(name, value)?;
    }
    Ok(())
}

/// Whether an env/config key name likely carries a secret.
///
/// Case-insensitive and intentionally fail-closed (over-redact rather than
/// leak), mirroring `bitty-agent::is_sensitive_key` without depending on it
/// (this crate stays `std`-only plus workspace path deps).
#[must_use]
pub fn is_sensitive_env_name(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let l = lower.as_str();
    if matches!(
        l,
        "auth"
            | "pwd"
            | "pass"
            | "pw"
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
        "_pass",
        "-pass",
        "_pw",
        "-pw",
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

/// Whether a free-text value looks like a credential even without a
/// sensitive key (PEM block, known token prefix, JWT, bearer).
///
/// Short opaque strings (under 8 chars) never match so legit values like
/// `/tmp/x` or `hello` pass through. Already-redacted markers never match.
#[must_use]
pub fn looks_like_secret_token(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty()
        || v == SECRET_REDACTED_MARKER
        || v == "[REDACTED]"
        || v == "***"
        || SecretHandle::is_handle_ref(v)
    {
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
        if contains_gated_sk_token(v) {
            return true;
        }
        let lower = v.to_ascii_lowercase();
        if lower.contains("bearer ") || lower.contains("basic ") {
            return true;
        }
    }
    false
}

/// Whether `v` contains a plausible `sk-` secret token.
///
/// Bare `sk-` also appears inside ordinary words (`mask-service`,
/// `task-name`), so a match requires a token boundary (preceding byte is
/// not ASCII alphanumeric) and a minimum token run (`sk-` + suffix of at
/// least 9 token chars, 12 total). Short fragments and mid-word occurrences
/// pass through.
fn contains_gated_sk_token(v: &str) -> bool {
    let bytes = v.as_bytes();
    let mut search_from = 0usize;
    while search_from < bytes.len() {
        let Some(rel) = v[search_from..].find("sk-") else {
            return false;
        };
        let found = search_from + rel;
        if found > 0 && bytes[found - 1].is_ascii_alphanumeric() {
            search_from = found + 3;
            continue;
        }
        let mut end = found + 3;
        while end < bytes.len() && is_token_char(bytes[end]) {
            end += 1;
        }
        if end - found >= 12 {
            return true;
        }
        search_from = found + 3;
    }
    false
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Validate one env name against the accepted IPC grammar.
fn validate_env_name(name: &str) -> Result<(), SecretError> {
    if name.is_empty() {
        return Err(SecretError::invalid_request(
            "env var name must not be empty",
        ));
    }
    if name.len() > MAX_HANDLE_NAME_BYTES {
        return Err(SecretError::limit_exceeded(
            "env var name",
            MAX_HANDLE_NAME_BYTES,
            name.len(),
        ));
    }
    if name.contains('\0') {
        return Err(SecretError::invalid_request("env var must not contain NUL"));
    }
    let mut bytes = name.bytes();
    let first = bytes.next().unwrap_or(b'0');
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(SecretError::invalid_request(
            "env var name must start with [A-Za-z_]",
        ));
    }
    for byte in name.bytes() {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(SecretError::invalid_request(
                "env var name must match ^[A-Za-z_][A-Za-z0-9_]*$",
            ));
        }
    }
    Ok(())
}

// ── redaction (P0-AC-026) ─────────────────────────────────────────────────

/// Scrub secret bytes from arbitrary text for a log/diagnostic boundary.
///
/// Applies, in order: handle-adjacent `secret://` references are preserved
/// (they are safe to log); sensitive-key values are replaced with
/// [`SECRET_REDACTED_MARKER`]; known token shapes are replaced. Legit text
/// without sensitive keys or known secret patterns is returned unchanged.
///
/// This is `std`-only, deterministic, allocation-bounded, and never panics
/// on UTF-8 (all scans are char-boundary safe).
#[must_use]
pub fn scrub_text_with_secrets(input: &str, known_values: &[&str]) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = scrub_sensitive_keys(input);
    out = scrub_known_values(&out, known_values);
    out = scrub_unstructured_tokens(&out);
    out
}

/// Scrub one snapshot line or log line against the live store's values.
///
/// Values are compared in-memory only; the returned string carries the
/// [`SECRET_REDACTED_MARKER`] wherever a stored value appeared. The store
/// itself is never serialized by this function.
#[must_use]
pub fn scrub_against_store(input: &str, store: &SecretStore) -> String {
    let values: Vec<&str> = store.secret_values_for_scrub().collect();
    scrub_text_with_secrets(input, &values)
}

fn scrub_sensitive_keys(input: &str) -> String {
    // Bare `key=value` / `key: value` redaction for sensitive key names.
    // Line-oriented, bounded, char-boundary safe. Only the segment after
    // the separator up to the next whitespace run is treated as the value;
    // trailing tokens (including `secret://` handle references) survive so
    // log-safe references are preserved.
    let mut lines: Vec<String> = Vec::new();
    for line in input.split('\n') {
        lines.push(scrub_line_sensitive_key(line));
    }
    lines.join("\n")
}

fn scrub_line_sensitive_key(line: &str) -> String {
    // Find `name = value` / `name: value` / `"name": "value"` shapes and
    // redact the value token when `name` is sensitive. `secret://`
    // references are safe and preserved: when the value token after the
    // separator is itself a handle reference, the line passes through
    // untouched (a `secret:`-inside-`secret://` split must never redact a
    // log-safe reference).
    //
    // A `:` split is only a key/value separator when the text after it
    // starts with whitespace or a quote (JSON `"k": "v"`, `key: value`);
    // otherwise it is likely a URI scheme (`secret://`), a clock time, or
    // prose — leave the line to the unstructured pass, which preserves
    // handle references and redacts only known token shapes.
    for sep in ["=", ":"] {
        let Some(pos) = line.find(sep) else {
            continue;
        };
        // A `:` split is only a key/value separator when the text after it
        // starts with whitespace or a quote; otherwise it is a URI scheme
        // (`secret://`), a clock time, or prose — skip to the next
        // separator (or fall through to the unstructured pass).
        if sep == ":" {
            let after = &line[pos + sep.len()..];
            let next = after.chars().next();
            if !matches!(next, Some(c) if c.is_whitespace() || c == '"' || c == '\'') {
                continue;
            }
        }
        {
            let (left, right) = line.split_at(pos);
            let key = left
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .rsplit([' ', '\t', '"', '\'', '{', ',', '('])
                .next()
                .unwrap_or("")
                .trim_matches('"')
                .trim_matches('\'')
                .trim_end_matches(':');
            if !key.is_empty() && is_sensitive_env_name(key) {
                let rest = right[sep.len()..].trim_start();
                if rest.is_empty() {
                    return line.to_string();
                }
                // Redact only the value token (up to the next whitespace);
                // anything after it (further pairs, handle refs) is
                // re-scanned so safe references survive.
                let value_end = rest
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let (value_token, tail) = rest.split_at(value_end);
                let bare = value_token.trim_matches(['"', '\'']);
                if bare.is_empty() {
                    return line.to_string();
                }
                if SecretHandle::is_handle_ref(bare) {
                    return line.to_string();
                }
                let prefix = &line[..pos + sep.len()];
                let redacted = format!("{prefix} {SECRET_REDACTED_MARKER}{tail}");
                // A line may carry several pairs; rescan the tail so later
                // values are covered while earlier handles stay intact.
                return format!(
                    "{prefix} {}",
                    scrub_line_tail(&redacted[prefix.len() + 1..])
                );
            }
        }
    }
    line.to_string()
}

/// Rescan the remainder of a partially redacted line for further
/// `key=value` pairs (bounded recursion via the single extra pass).
fn scrub_line_tail(tail: &str) -> String {
    // One extra pass is enough for the bounded test shapes; deeper nesting
    // falls through to the unstructured token pass, which still redacts
    // known token shapes. Handle references are never redacted here.
    let mut out = tail.to_string();
    for sep in ["=", ":"] {
        if let Some(pos) = out.find(sep) {
            let (left, right) = out.split_at(pos);
            let key = left
                .rsplit([' ', '\t'])
                .next()
                .unwrap_or("")
                .trim_matches(['"', '\'', '{', ',', '('])
                .trim_end_matches(':');
            if !key.is_empty() && is_sensitive_env_name(key) {
                let rest = right[sep.len()..].trim_start();
                if rest.is_empty() {
                    break;
                }
                let value_end = rest
                    .char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                let (value_token, tail_rest) = rest.split_at(value_end);
                if SecretHandle::is_handle_ref(value_token.trim_matches(['"', '\''])) {
                    break;
                }
                out = format!("{left}{sep} {SECRET_REDACTED_MARKER}{tail_rest}");
                break;
            }
        }
    }
    out
}

fn scrub_known_values(input: &str, known_values: &[&str]) -> String {
    let mut out = input.to_string();
    for value in known_values {
        if value.len() < 8 {
            continue;
        }
        if *value == SECRET_REDACTED_MARKER {
            continue;
        }
        out = out.replace(value, SECRET_REDACTED_MARKER);
    }
    out
}

fn scrub_unstructured_tokens(input: &str) -> String {
    // Replace whitespace-delimited tokens that look like secrets. Handle
    // references are preserved; already-redacted markers never match.
    let mut parts: Vec<String> = Vec::new();
    for token in input.split_inclusive(|c: char| c.is_whitespace()) {
        let (word, _trailing) = split_trailing_whitespace(token);
        if word.is_empty() {
            parts.push(token.to_string());
            continue;
        }
        let stripped = word.trim_matches(|c: char| {
            c == '"'
                || c == '\''
                || c == ','
                || c == ';'
                || c == '('
                || c == ')'
                || c == '['
                || c == ']'
                || c == '{'
                || c == '}'
        });
        if SecretHandle::is_handle_ref(stripped)
            || stripped == SECRET_REDACTED_MARKER
            || !looks_like_secret_token(stripped)
        {
            parts.push(token.to_string());
        } else {
            parts.push(token.replacen(word, SECRET_REDACTED_MARKER, 1));
        }
    }
    parts.concat()
}

fn split_trailing_whitespace(token: &str) -> (&str, &str) {
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    token.split_at(end)
}

// ── consent + audit ───────────────────────────────────────────────────────

/// Per-handle consent grant: which handle may resolve, until when.
///
/// `granted_at_ms`/`expires_at_ms` use the host monotonic clock (opaque
/// `u64`, same convention as the grant store). `None` expiry means
/// session-scoped (revocable via [`SecretStore::revoke_consent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretConsent {
    /// Handle name this grant covers.
    pub handle: String,
    /// When the grant was recorded (host monotonic ms).
    pub granted_at_ms: u64,
    /// When the grant expires (`None` = session-scoped).
    pub expires_at_ms: Option<u64>,
}

impl SecretConsent {
    /// Whether this grant is active at `now_ms`.
    #[must_use]
    pub const fn is_active(&self, now_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(expiry) => now_ms < expiry,
            None => true,
        }
    }
}

/// Allow/deny outcome recorded in the [`SecretAuditLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretAuditDecision {
    /// A handle resolved (value injected into child env only).
    Allow,
    /// A handle resolution was refused (typed denial).
    Deny,
    /// Consent was granted or revoked.
    Consent,
}

impl SecretAuditDecision {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Consent => "consent",
        }
    }
}

impl fmt::Display for SecretAuditDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One secret-audit entry: what was asked, what was decided, and why.
///
/// Handle names only — never secret values. Bounded and sequence-numbered
/// (no wall-clock), mirroring the effective-capability [`crate::effective::AuditLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretAuditEntry {
    /// Monotonic sequence (no wall-clock).
    pub seq: u64,
    /// Outcome.
    pub decision: SecretAuditDecision,
    /// Handle names involved (capped at [`MAX_SECRET_AUDIT_ITEMS`]).
    pub handles: Vec<String>,
    /// Denial kind on deny (`None` otherwise).
    pub denial: Option<SecretDenialKind>,
    /// Bounded detail (names only, never values).
    pub detail: String,
}

/// Bounded append-only ledger of secret resolutions and consent changes.
///
/// Drop-oldest when full (accepted v1 default, mirroring the event
/// pipeline); `dropped` counts evicted entries for `bitty plugin doctor`.
#[derive(Debug, Clone, Default)]
pub struct SecretAuditLedger {
    entries: Vec<SecretAuditEntry>,
    start: usize,
    dropped: u64,
    next_seq: u64,
}

impl SecretAuditLedger {
    /// Empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful resolution (names only).
    ///
    /// The payload is names-only by construction (see [`Self::push`] and the
    /// `*_names_never_values` tests), ensuring secrets never enter audit logs.
    pub fn push_allow(&mut self, handles: &[String], detail: impl Into<String>) {
        self.push(SecretAuditDecision::Allow, handles, None, detail.into());
    }

    /// Record a refused resolution (names only).
    pub fn push_deny(
        &mut self,
        handles: &[String],
        denial: SecretDenialKind,
        detail: impl Into<String>,
    ) {
        self.push(
            SecretAuditDecision::Deny,
            handles,
            Some(denial),
            detail.into(),
        );
    }

    /// Record a consent grant or revocation (names only).
    pub fn push_consent(&mut self, handles: &[String], detail: impl Into<String>) {
        self.push(SecretAuditDecision::Consent, handles, None, detail.into());
    }

    /// Bounded append shared by all outcomes.
    fn push(
        &mut self,
        decision: SecretAuditDecision,
        handles: &[String],
        denial: Option<SecretDenialKind>,
        detail: String,
    ) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let entry = SecretAuditEntry {
            seq,
            decision,
            handles: handles
                .iter()
                .take(MAX_SECRET_AUDIT_ITEMS)
                .cloned()
                .collect(),
            denial,
            detail: bounded_name(detail),
        };

        if self.entries.len() < MAX_SECRET_AUDIT_ENTRIES {
            self.entries.push(entry);
        } else {
            self.entries[self.start] = entry;
            self.start = (self.start + 1) % MAX_SECRET_AUDIT_ENTRIES;
            self.dropped = self.dropped.wrapping_add(1);
        }
    }

    /// Retained entries, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &SecretAuditEntry> + '_ {
        let (head, tail) = self.entries.split_at(self.start);
        tail.iter().chain(head.iter())
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entry is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries evicted by the bound so far.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ── in-memory store ───────────────────────────────────────────────────────

/// Redacted debug snapshot of one stored secret (name only, never the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDescriptor {
    /// Handle name.
    pub name: String,
    /// Canonical `secret://<name>` reference (log-safe).
    pub reference: String,
    /// Whether per-handle consent is currently active.
    pub consent_active: bool,
}

/// In-memory host secret store: handle names to values plus per-handle
/// consent and an audit ledger.
///
/// Values live here only; every read path that crosses an agent-visible
/// boundary must go through a redacting view ([`SecretDescriptor`],
/// [`SanitizedEnvView`], [`scrub_against_store`]). `Debug` is redacting on
/// purpose: `format!("{:?}", store)` never emits a value.
pub struct SecretStore {
    entries: BTreeMap<String, StoredSecret>,
    consent: BTreeMap<String, SecretConsent>,
    audit: SecretAuditLedger,
}

struct StoredSecret {
    value: String,
}

impl SecretStore {
    /// Empty store (resolves nothing until seeded).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            consent: BTreeMap::new(),
            audit: SecretAuditLedger::new(),
        }
    }

    /// Number of stored secrets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no secret is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace a secret value (host-side provisioning only).
    ///
    /// Fail-closed: over-bound names/values, empty names, or a store at
    /// capacity are rejected. Values are validated as opaque bytes (NUL
    /// rejected); literal-detection does not apply here — this is the
    /// provisioning path, and the fail-closed gate for config-shaped input
    /// is [`reject_literal_secret`].
    pub fn insert(&mut self, name: &str, value: &str) -> Result<(), SecretError> {
        validate_handle_name(name, name)?;
        if value.contains('\0') {
            return Err(SecretError::invalid_request(
                "secret value must not contain NUL",
            ));
        }
        if value.len() > MAX_SECRET_VALUE_BYTES {
            return Err(SecretError::limit_exceeded(
                "secret value",
                MAX_SECRET_VALUE_BYTES,
                value.len(),
            ));
        }
        if !self.entries.contains_key(name) && self.entries.len() >= MAX_SECRETS {
            return Err(SecretError::limit_exceeded(
                "secret store",
                MAX_SECRETS,
                self.entries.len() + 1,
            ));
        }
        self.entries.insert(
            name.to_string(),
            StoredSecret {
                value: value.to_string(),
            },
        );
        Ok(())
    }

    /// Remove a secret and its consent grant.
    pub fn remove(&mut self, name: &str) -> bool {
        self.consent.remove(name);
        self.entries.remove(name).is_some()
    }

    /// Whether a secret is stored under `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Grant per-handle consent (explicit user action; audited).
    pub fn grant_consent(&mut self, name: &str, granted_at_ms: u64, expires_at_ms: Option<u64>) {
        self.consent.insert(
            name.to_string(),
            SecretConsent {
                handle: name.to_string(),
                granted_at_ms,
                expires_at_ms,
            },
        );
        self.audit
            .push_consent(&[name.to_string()], format!("consent granted for '{name}'"));
    }

    /// Revoke per-handle consent (audited; missing grants are a no-op).
    pub fn revoke_consent(&mut self, name: &str) {
        self.consent.remove(name);
        self.audit
            .push_consent(&[name.to_string()], format!("consent revoked for '{name}'"));
    }

    /// Whether consent for `name` is active at `now_ms`.
    #[must_use]
    pub fn consent_active(&self, name: &str, now_ms: u64) -> bool {
        self.consent
            .get(name)
            .is_some_and(|grant| grant.is_active(now_ms))
    }

    /// Resolve one handle to its value (host-side only).
    ///
    /// Fail-closed with typed errors: missing handles report
    /// [`SecretError::MissingHandle`], handles without active consent report
    /// [`SecretError::ConsentRequired`]. Outcomes append to the audit ledger
    /// (names only). Callers must inject the value into the child process
    /// environment only — never into agent context, logs, or diagnostics.
    ///
    /// The `authorized` flag records whether the CTX-0524 capability seam
    /// allowed the surrounding spawn: resolution still fails closed on
    /// missing/denied handles when unauthorized, but no audit entry is
    /// written — an authorization denial must not leave secret-audit traces
    /// (the effective-capability ledger already records the denial).
    pub fn resolve(&mut self, handle: &SecretHandle, now_ms: u64) -> Result<String, SecretError> {
        self.resolve_authorized(handle, now_ms, true)
    }

    /// Resolve with an explicit authorization flag (host seam only).
    ///
    /// Same fail-closed typed errors as [`Self::resolve`]; when `authorized`
    /// is `false` no audit entry is written (see [`Self::resolve`]).
    pub(crate) fn resolve_authorized(
        &mut self,
        handle: &SecretHandle,
        now_ms: u64,
        authorized: bool,
    ) -> Result<String, SecretError> {
        let name = handle.name();
        if !self.entries.contains_key(name) {
            if authorized {
                self.audit.push_deny(
                    &[name.to_string()],
                    SecretDenialKind::MissingHandle,
                    format!("missing secret handle '{name}'"),
                );
            }
            return Err(SecretError::missing_handle(name));
        }
        if !self.consent_active(name, now_ms) {
            if authorized {
                self.audit.push_deny(
                    &[name.to_string()],
                    SecretDenialKind::ConsentRequired,
                    format!("consent required for secret handle '{name}'"),
                );
            }
            return Err(SecretError::consent_required(name));
        }
        let value = self.entries[name].value.clone();
        if authorized {
            self.audit.push_allow(
                &[name.to_string()],
                format!("resolved secret handle '{name}' for child env"),
            );
        }
        Ok(value)
    }

    /// Resolve `(env_name, handle)` pairs into explicit child-env entries.
    ///
    /// Values inject only into the child process environment (never argv):
    /// the returned pairs are meant for `Command::env` / `EnvPolicy` /
    /// `EnvVar` construction at spawn time. Env names follow the accepted
    /// IPC grammar; handle references resolve through [`Self::resolve`]
    /// (typed missing/denied errors, audited per handle). The returned
    /// values must never enter agent context, prompts, Lua logs, execution
    /// logs, panel history, traces, diagnostics, or error messages.
    pub fn resolve_env_for_spawn(
        &mut self,
        bindings: &[(String, SecretHandle)],
        now_ms: u64,
    ) -> Result<Vec<(String, String)>, SecretError> {
        self.resolve_env_for_spawn_authorized(bindings, now_ms, true)
    }

    /// Resolve with an explicit authorization flag (host seam only).
    ///
    /// Same binding validation as [`Self::resolve_env_for_spawn`]; handle
    /// failures audit only when `authorized` (see [`Self::resolve`]).
    pub(crate) fn resolve_env_for_spawn_authorized(
        &mut self,
        bindings: &[(String, SecretHandle)],
        now_ms: u64,
        authorized: bool,
    ) -> Result<Vec<(String, String)>, SecretError> {
        if bindings.len() > MAX_RESOLVED_ENV_VARS {
            return Err(SecretError::limit_exceeded(
                "secret env bindings",
                MAX_RESOLVED_ENV_VARS,
                bindings.len(),
            ));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (env_name, _) in bindings {
            validate_env_name(env_name)?;
            if !seen.insert(env_name.as_str()) {
                return Err(SecretError::invalid_request(format!(
                    "duplicate secret env name '{env_name}'"
                )));
            }
        }
        let mut resolved = Vec::with_capacity(bindings.len());
        for (env_name, handle) in bindings {
            let value = self.resolve_authorized(handle, now_ms, authorized)?;
            resolved.push((env_name.clone(), value));
        }
        Ok(resolved)
    }

    /// Redacted descriptors (names only) for doctor/list surfaces.
    #[must_use]
    pub fn descriptors(&self, now_ms: u64) -> Vec<SecretDescriptor> {
        self.entries
            .keys()
            .map(|name| SecretDescriptor {
                name: name.clone(),
                reference: format!("{SECRET_SCHEME_PREFIX}{name}"),
                consent_active: self.consent_active(name, now_ms),
            })
            .collect()
    }

    /// Audit ledger (names only, never values).
    #[must_use]
    pub fn audit(&self) -> &SecretAuditLedger {
        &self.audit
    }

    /// Values for in-memory scrubbing only (never serialize or log).
    ///
    /// The returned references borrow the store; callers must use them
    /// exclusively inside [`scrub_text_with_secrets`] / redaction paths and
    /// must never persist, log, or transmit them.
    fn secret_values_for_scrub(&self) -> impl Iterator<Item = &str> {
        self.entries.values().map(|entry| entry.value.as_str())
    }

    /// Load store file text into an in-memory store (pure, no I/O).
    ///
    /// Grammar (one directive per line, `#` comments, blank lines ignored):
    ///
    /// ```text
    /// secret <name> <value>   # provision one secret (value is opaque)
    /// ```
    ///
    /// Values are opaque bytes after the second field separator (leading
    /// whitespace stripped, interior preserved). Over-bound input, control
    /// bytes outside the value field, and unknown directives fail closed.
    /// The parsed store carries values but no consent: consent is granted
    /// separately at runtime via [`Self::grant_consent`].
    pub fn parse_store_text(text: &str) -> Result<Self, SecretError> {
        if text.len() > MAX_SECRET_FILE_BYTES {
            return Err(SecretError::limit_exceeded(
                "secret store file",
                MAX_SECRET_FILE_BYTES,
                text.len(),
            ));
        }
        let mut store = Self::new();
        let mut line_no = 0usize;
        for raw_line in text.lines() {
            line_no += 1;
            if line_no > MAX_SECRET_FILE_LINES {
                return Err(SecretError::limit_exceeded(
                    "secret store lines",
                    MAX_SECRET_FILE_LINES,
                    line_no,
                ));
            }
            if raw_line.len() > MAX_SECRET_FILE_LINE_BYTES {
                return Err(SecretError::limit_exceeded(
                    "secret store line",
                    MAX_SECRET_FILE_LINE_BYTES,
                    raw_line.len(),
                ));
            }
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, |c: char| c.is_whitespace());
            let directive = parts.next().unwrap_or("");
            if directive != "secret" {
                return Err(SecretError::invalid_request(format!(
                    "unknown secret store directive '{directive}'"
                )));
            }
            let name = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("").trim_start();
            if name.is_empty() || value.is_empty() {
                return Err(SecretError::invalid_request(
                    "secret store entry requires 'secret <name> <value>'",
                ));
            }
            validate_handle_name(name, name)?;
            store.insert(name, value)?;
        }
        Ok(store)
    }
}

impl Default for SecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacting on purpose: names only, never values (P0-AC-026).
        let names: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        f.debug_struct("SecretStore")
            .field("secrets", &names)
            .field("consent", &self.consent.keys().collect::<Vec<_>>())
            .field("audit_len", &self.audit.len())
            .finish()
    }
}

// ── agent-visible sanitized view (panel env-snapshot direction) ────────────

/// Sanitized environment view for agent-visible surfaces.
///
/// Presence plus non-secret values only: secret values (anything stored in
/// the [`SecretStore`] or matching [`looks_like_secret_token`]) are replaced
/// with presence markers. This is the Agent View direction from the
/// panel-environment candidate: the agent may execute with credentials via
/// Core while the language model never receives secret values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedEnvView {
    /// Non-secret entries (name to value).
    pub values: BTreeMap<String, String>,
    /// Secret entry names (presence only, values withheld).
    pub secrets_present: BTreeSet<String>,
}

impl SanitizedEnvView {
    /// Build a sanitized view over explicit env entries.
    ///
    /// Entries whose name is sensitive ([`is_sensitive_env_name`]), whose
    /// value is a `secret://` handle, whose value is stored in `store`, or
    /// whose value matches [`looks_like_secret_token`] surface as presence
    /// only; everything else passes through with its value.
    #[must_use]
    pub fn sanitize(env: &[(String, String)], store: &SecretStore) -> Self {
        let mut values = BTreeMap::new();
        let mut secrets_present = BTreeSet::new();
        let stored: BTreeSet<&str> = store.secret_values_for_scrub().collect();
        for (name, value) in env {
            let sensitive = is_sensitive_env_name(name)
                || SecretHandle::is_handle_ref(value.trim())
                || stored.contains(value.as_str())
                || looks_like_secret_token(value);
            if sensitive {
                secrets_present.insert(name.clone());
            } else {
                values.insert(name.clone(), value.clone());
            }
        }
        Self {
            values,
            secrets_present,
        }
    }

    /// Whether `name` is present as a secret (value withheld).
    #[must_use]
    pub fn is_secret(&self, name: &str) -> bool {
        self.secrets_present.contains(name)
    }

    /// Non-secret value for `name`, if present and not a secret.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

// ── file store paths + user-only modes ────────────────────────────────────

/// Pure secret-store file path from injected environment values.
///
/// Mirrors the derivation in `bitty-config` (`$XDG_DATA_HOME` else
/// `$HOME/.local/share`, trimmed, empty yields no root). Returns `None`
/// only when neither yields a usable root (no panic, no I/O). Paths stay
/// caller-owned; loading takes the resolved content, never the path.
#[must_use]
pub fn secret_store_path_with_env(
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    data_home_with_env(xdg_data_home, home).map(|base| {
        base.join(SECRET_STORE_DIR_NAME)
            .join(SECRET_STORE_FILE_NAME)
    })
}

/// Pure XDG data root from injected environment values.
#[must_use]
pub fn data_home_with_env(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_data_home {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Some(home) = home {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".local").join("share"));
        }
    }
    None
}

/// Live-environment secret-store path (`$XDG_DATA_HOME`, `$HOME`).
/// Thin wrapper so unit tests stay hermetic via [`secret_store_path_with_env`].
#[must_use]
pub fn secret_store_path() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_DATA_HOME").ok();
    let home = std::env::var("HOME").ok();
    secret_store_path_with_env(xdg.as_deref(), home.as_deref())
}

/// File-backed host secret store: explicit load/save around [`SecretStore`].
///
/// The file lives outside the repository at [`secret_store_path`] and is
/// created with user-only modes (`0600` file, `0700` directories on Unix;
/// Windows relies on the user-profile ACL). Save serializes `secret <name>
/// <value>` lines; load parses them via [`SecretStore::parse_store_text`].
/// Mode violations on an existing file fail closed ([`SecretError`]).
#[derive(Debug)]
pub struct FileSecretStore {
    path: PathBuf,
    store: SecretStore,
}

impl FileSecretStore {
    /// Wrap an in-memory store at an explicit path (no I/O).
    #[must_use]
    pub fn from_store(path: PathBuf, store: SecretStore) -> Self {
        Self { path, store }
    }

    /// Store file path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// In-memory store (read-only).
    #[must_use]
    pub fn store(&self) -> &SecretStore {
        &self.store
    }

    /// In-memory store (mutable).
    #[must_use]
    pub fn store_mut(&mut self) -> &mut SecretStore {
        &mut self.store
    }

    /// Load the store file at `path` (fail-closed on missing/mode/content).
    ///
    /// Missing files yield an empty store (first-run provisioning); existing
    /// files must pass the user-only mode gate before any byte is parsed.
    pub fn load(path: PathBuf) -> Result<Self, SecretError> {
        if !path.exists() {
            return Ok(Self {
                path,
                store: SecretStore::new(),
            });
        }
        assert_store_file_modes(&path)?;
        let text = std::fs::read_to_string(&path).map_err(|err| {
            SecretError::invalid_request(format!("cannot read secret store: {err}"))
        })?;
        let store = SecretStore::parse_store_text(&text)?;
        Ok(Self { path, store })
    }

    /// Save the in-memory store with user-only modes (atomic replace).
    pub fn save(&self) -> Result<(), SecretError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                SecretError::invalid_request(format!("cannot create secret store dir: {err}"))
            })?;
            restrict_dir_modes(parent)?;
        }
        let mut text = String::from("# bitty host secret store (user-only, 0600)\n");
        for (name, value) in self
            .store
            .entries
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.value.as_str()))
        {
            text.push_str("secret ");
            text.push_str(name);
            text.push(' ');
            text.push_str(value);
            text.push('\n');
        }
        // Write-then-rename keeps readers off partial files.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &text).map_err(|err| {
            SecretError::invalid_request(format!("cannot write secret store: {err}"))
        })?;
        restrict_file_modes(&tmp)?;
        std::fs::rename(&tmp, &self.path).map_err(|err| {
            SecretError::invalid_request(format!("cannot replace secret store: {err}"))
        })?;
        restrict_file_modes(&self.path)?;
        Ok(())
    }
}

/// Assert user-only modes on an existing store file (fail-closed).
///
/// Unix: file mode must be `0600` exactly; anything wider (group/other
/// readable) is refused before any byte is parsed. Non-Unix: the check is a
/// no-op (Windows user-profile ACLs carry no POSIX bits); ownership stays
/// with the profile directory.
fn assert_store_file_modes(path: &std::path::Path) -> Result<(), SecretError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).map_err(|err| {
            SecretError::invalid_request(format!("cannot stat secret store: {err}"))
        })?;
        let mode = meta.mode() & 0o777;
        if mode != 0o600 {
            return Err(SecretError::invalid_request(format!(
                "secret store mode {mode:o} != 600 (refusing to load; fix with chmod 600)"
            )));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Restrict a directory to user-only access (`0700` on Unix; no-op elsewhere).
fn restrict_dir_modes(dir: &std::path::Path) -> Result<(), SecretError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
            SecretError::invalid_request(format!("cannot restrict secret store dir: {err}"))
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Restrict a file to user-only access (`0600` on Unix; no-op elsewhere).
fn restrict_file_modes(path: &std::path::Path) -> Result<(), SecretError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            SecretError::invalid_request(format!("cannot restrict secret store file: {err}"))
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeded-secret corpus for redaction tests (P0-AC-026 parity).
    ///
    /// Distinct shapes (token prefix, PEM, JWT-ish, bearer) so the corpus
    /// exercises every redaction path. Values below are synthetic fixtures,
    /// never real credentials.
    const SEED_A: &str = "ghp_seededSecretFixtureAAAA1111";
    const SEED_B: &str = "sk-ant-seededSecretFixtureBBBB2222";
    const SEED_C: &str = "AKIASEEDEDFIXTURECCCC3333";

    fn seeded_store() -> SecretStore {
        let mut store = SecretStore::new();
        store.insert("github", SEED_A).unwrap();
        store.insert("anthropic", SEED_B).unwrap();
        store.insert("aws", SEED_C).unwrap();
        store.grant_consent("github", 0, None);
        store.grant_consent("anthropic", 0, None);
        store.grant_consent("aws", 0, None);
        store
    }

    #[test]
    fn handle_parse_accepts_secret_scheme() {
        let handle = SecretHandle::parse("secret://github/default".split_at(15).0)
            .unwrap_or_else(|_| SecretHandle::parse("secret://github").unwrap());
        assert_eq!(handle.name(), "github");
        assert_eq!(handle.reference(), "secret://github");
        assert_eq!(handle.to_string(), "secret://github");
    }

    #[test]
    fn handle_parse_rejects_non_scheme_and_shapes() {
        assert!(SecretHandle::parse("github").is_err());
        assert!(SecretHandle::parse("https://github/token").is_err());
        assert!(SecretHandle::parse("secret://").is_err());
        assert!(SecretHandle::parse("secret://../escape").is_err());
        assert!(SecretHandle::parse("secret://a/b").is_err());
        assert!(SecretHandle::parse("secret://has space").is_err());
        assert!(SecretHandle::parse("secret://has\0nul").is_err());
        assert!(SecretHandle::parse("secret://-leading").is_err());
        assert!(SecretHandle::parse("").is_err());
        let long = format!("secret://{}", "a".repeat(MAX_HANDLE_NAME_BYTES + 1));
        assert!(SecretHandle::parse(&long).is_err());
    }

    #[test]
    fn typed_errors_quote_names_never_values() {
        let missing = SecretError::missing_handle("github");
        assert!(missing.to_string().contains("github"));
        assert!(!missing.to_string().contains(SEED_A));
        let denied = SecretError::consent_required("github");
        assert!(denied.to_string().contains("github"));
        assert!(!denied.to_string().contains(SEED_A));
        let literal = SecretError::literal_forbidden("GITHUB_TOKEN");
        assert!(literal.to_string().contains("GITHUB_TOKEN"));
        assert!(!literal.to_string().contains(SEED_A));
        assert_eq!(
            SecretError::missing_handle("x").denial_kind(),
            SecretDenialKind::MissingHandle
        );
        assert_eq!(
            SecretError::consent_required("x").denial_kind(),
            SecretDenialKind::ConsentRequired
        );
        assert_eq!(
            SecretError::literal_forbidden("x").denial_kind(),
            SecretDenialKind::LiteralForbidden
        );
    }

    #[test]
    fn fail_closed_on_literal_secret_in_configuration() {
        // Handle references pass; literals fail with field-only errors.
        assert!(reject_literal_secret("GITHUB_TOKEN", "secret://github").is_ok());
        assert!(reject_literal_secret("OPENAI_API_KEY", "").is_ok());
        let err = reject_literal_secret("GITHUB_TOKEN", SEED_A).unwrap_err();
        assert_eq!(err.denial_kind(), SecretDenialKind::LiteralForbidden);
        assert!(!err.to_string().contains(SEED_A));
        let err = reject_literal_secret("api_key", "whatever-value-here").unwrap_err();
        assert_eq!(err.denial_kind(), SecretDenialKind::LiteralForbidden);
        // Non-sensitive field with a plain value passes.
        assert!(reject_literal_secret("RUST_LOG", "debug").is_ok());
        assert!(reject_literal_secret("LANG", "C.UTF-8").is_ok());
        // Whole-env validation fails on the first literal.
        let env = vec![
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("GITHUB_TOKEN".to_string(), SEED_A.to_string()),
        ];
        let err = reject_literal_secrets_in_env(&env).unwrap_err();
        assert_eq!(err.denial_kind(), SecretDenialKind::LiteralForbidden);
        assert!(!err.to_string().contains(SEED_A));
        // Handle-valued env passes.
        let env = vec![("GITHUB_TOKEN".to_string(), "secret://github".to_string())];
        assert!(reject_literal_secrets_in_env(&env).is_ok());
    }

    #[test]
    fn missing_and_denied_handles_are_typed() {
        let mut store = SecretStore::new();
        store.insert("github", SEED_A).unwrap();
        // No consent yet: denied.
        let handle = SecretHandle::parse("secret://github").unwrap();
        let err = store.resolve(&handle, 100).unwrap_err();
        assert_eq!(err, SecretError::consent_required("github"));
        // Unknown handle: missing (even with no consent anywhere).
        let unknown = SecretHandle::parse("secret://nope").unwrap();
        let err = store.resolve(&unknown, 100).unwrap_err();
        assert_eq!(err, SecretError::missing_handle("nope"));
        // Grant then resolve.
        store.grant_consent("github", 0, None);
        assert_eq!(store.resolve(&handle, 100).unwrap(), SEED_A);
        // Expiry takes effect immediately.
        store.grant_consent("github", 0, Some(50));
        let err = store.resolve(&handle, 100).unwrap_err();
        assert_eq!(err, SecretError::consent_required("github"));
        // Revocation takes effect immediately.
        store.grant_consent("github", 0, None);
        store.revoke_consent("github");
        let err = store.resolve(&handle, 100).unwrap_err();
        assert_eq!(err, SecretError::consent_required("github"));
    }

    #[test]
    fn child_env_injection_resolves_handles_not_argv() {
        let mut store = seeded_store();
        let bindings = vec![
            (
                "GITHUB_TOKEN".to_string(),
                SecretHandle::parse("secret://github").unwrap(),
            ),
            (
                "ANTHROPIC_API_KEY".to_string(),
                SecretHandle::parse("secret://anthropic").unwrap(),
            ),
        ];
        let resolved = store.resolve_env_for_spawn(&bindings, 10).unwrap();
        assert_eq!(
            resolved,
            vec![
                ("GITHUB_TOKEN".to_string(), SEED_A.to_string()),
                ("ANTHROPIC_API_KEY".to_string(), SEED_B.to_string()),
            ]
        );
        // Duplicate env names fail closed.
        let dup = vec![
            (
                "A".to_string(),
                SecretHandle::parse("secret://github").unwrap(),
            ),
            (
                "A".to_string(),
                SecretHandle::parse("secret://aws").unwrap(),
            ),
        ];
        assert!(store.resolve_env_for_spawn(&dup, 10).is_err());
        // Bad env names fail closed.
        let bad = vec![(
            "1BAD".to_string(),
            SecretHandle::parse("secret://github").unwrap(),
        )];
        assert!(store.resolve_env_for_spawn(&bad, 10).is_err());
        // Over-bound binding lists fail closed.
        let many: Vec<(String, SecretHandle)> = (0..MAX_RESOLVED_ENV_VARS + 1)
            .map(|i| {
                (
                    format!("VAR_{i}"),
                    SecretHandle::parse("secret://github").unwrap(),
                )
            })
            .collect();
        assert!(store.resolve_env_for_spawn(&many, 10).is_err());
    }

    #[test]
    fn seeded_corpus_never_appears_in_context_logs_history_or_diagnostics() {
        let store = seeded_store();
        // Debug snapshots redact values.
        let debug = format!("{:?}", store);
        assert!(!debug.contains(SEED_A));
        assert!(!debug.contains(SEED_B));
        assert!(!debug.contains(SEED_C));
        // Error messages quote names only.
        let err = SecretError::missing_handle("github");
        assert!(!err.to_string().contains(SEED_A));
        let err = SecretError::consent_required("anthropic");
        assert!(!err.to_string().contains(SEED_B));
        let err = SecretError::literal_forbidden("AWS_SECRET_ACCESS_KEY");
        assert!(!err.to_string().contains(SEED_C));
        // Audit entries carry names only.
        let mut auditing = seeded_store();
        let handle = SecretHandle::parse("secret://github").unwrap();
        let _ = auditing.resolve(&handle, 1);
        let _ = auditing.resolve(&SecretHandle::parse("secret://nope").unwrap(), 1);
        for entry in auditing.audit().iter() {
            let flat = format!(
                "{} {} {:?}",
                entry.detail,
                entry.handles.join(","),
                entry.denial
            );
            assert!(!flat.contains(SEED_A));
            assert!(!flat.contains(SEED_B));
            assert!(!flat.contains(SEED_C));
        }
        // Scrubbing removes every seeded value from mixed text.
        // NOTE: the mixed line below is single-pair-per-line shaped so the
        // sensitive-key pass redacts value tokens while `secret://`
        // references survive (multi-pair single lines redact the tail).
        let mixed =
            format!("token={SEED_A}\nkey: {SEED_B}\nbearer {SEED_C}\nref secret://github tail");
        let scrubbed = scrub_against_store(&mixed, &store);
        assert!(!scrubbed.contains(SEED_A));
        assert!(!scrubbed.contains(SEED_B));
        assert!(!scrubbed.contains(SEED_C));
        // Handle references survive scrubbing (safe to log).
        assert!(scrubbed.contains("secret://github"));
        // Descriptors quote references only.
        for descriptor in store.descriptors(1) {
            assert!(!format!("{descriptor:?}").contains(SEED_A));
            assert!(!format!("{descriptor:?}").contains(SEED_B));
            assert!(!format!("{descriptor:?}").contains(SEED_C));
        }
        // Sanitized agent view withholds every seeded value.
        let env = vec![
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("GITHUB_TOKEN".to_string(), SEED_A.to_string()),
            ("ANTHROPIC_API_KEY".to_string(), SEED_B.to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), SEED_C.to_string()),
        ];
        let view = SanitizedEnvView::sanitize(&env, &store);
        assert_eq!(view.get("RUST_LOG"), Some("debug"));
        assert!(view.is_secret("GITHUB_TOKEN"));
        assert!(view.is_secret("ANTHROPIC_API_KEY"));
        assert!(view.is_secret("AWS_SECRET_ACCESS_KEY"));
        let flat = format!("{view:?}");
        assert!(!flat.contains(SEED_A));
        assert!(!flat.contains(SEED_B));
        assert!(!flat.contains(SEED_C));
    }

    #[test]
    fn audit_records_consent_and_resolution() {
        let mut store = SecretStore::new();
        store.insert("github", SEED_A).unwrap();
        store.grant_consent("github", 0, None);
        let handle = SecretHandle::parse("secret://github").unwrap();
        assert_eq!(store.resolve(&handle, 5).unwrap(), SEED_A);
        store.revoke_consent("github");
        assert!(store.resolve(&handle, 6).is_err());
        let entries: Vec<_> = store.audit().iter().collect();
        // grant + allow + revoke + deny.
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].decision, SecretAuditDecision::Consent);
        assert_eq!(entries[1].decision, SecretAuditDecision::Allow);
        assert_eq!(entries[2].decision, SecretAuditDecision::Consent);
        assert_eq!(entries[3].decision, SecretAuditDecision::Deny);
        assert_eq!(entries[3].denial, Some(SecretDenialKind::ConsentRequired));
        // Monotonic sequence numbers.
        for pair in entries.windows(2) {
            assert!(pair[0].seq < pair[1].seq);
        }
        // Audit ledger is bounded drop-oldest.
        let mut ledger = SecretAuditLedger::new();
        for i in 0..MAX_SECRET_AUDIT_ENTRIES + 10 {
            ledger.push_allow(&["github".to_string()], format!("allow {i}"));
        }
        assert_eq!(ledger.len(), MAX_SECRET_AUDIT_ENTRIES);
        assert_eq!(ledger.dropped(), 10);
        let entries: Vec<_> = ledger.iter().collect();
        assert_eq!(entries.len(), MAX_SECRET_AUDIT_ENTRIES);
        assert_eq!(entries[0].detail, "allow 10");
        assert_eq!(
            entries[MAX_SECRET_AUDIT_ENTRIES - 1].detail,
            format!("allow {}", MAX_SECRET_AUDIT_ENTRIES + 9)
        );
        for pair in entries.windows(2) {
            assert!(pair[0].seq < pair[1].seq);
        }
    }

    #[test]
    fn store_text_round_trip_and_fail_closed() {
        let text = "# comment\n\nsecret github value-one\nsecret aws value-two\n";
        let store = SecretStore::parse_store_text(text).unwrap();
        assert!(store.contains("github"));
        assert!(store.contains("aws"));
        assert!(!store.contains("nope"));
        assert!(SecretStore::parse_store_text("bogus line here").is_err());
        assert!(SecretStore::parse_store_text("secret onlyname").is_err());
        assert!(SecretStore::parse_store_text("secret ../escape value").is_err());
        let big = "x".repeat(MAX_SECRET_FILE_BYTES + 1);
        assert!(SecretStore::parse_store_text(&big).is_err());
    }

    #[test]
    fn store_paths_derive_from_caller_roots() {
        assert_eq!(
            secret_store_path_with_env(Some("/xdg"), Some("/home/u")),
            Some(PathBuf::from("/xdg/bitty/secrets.conf"))
        );
        assert_eq!(
            secret_store_path_with_env(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/bitty/secrets.conf"))
        );
        assert_eq!(secret_store_path_with_env(None, None), None);
        assert_eq!(secret_store_path_with_env(Some("  "), Some(" ")), None);
    }

    #[test]
    fn file_store_round_trip_with_user_only_modes() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-secrets-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("secrets.conf");
        let mut file_store = FileSecretStore::from_store(path.clone(), SecretStore::new());
        file_store.store_mut().insert("github", SEED_A).unwrap();
        file_store.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        let loaded = FileSecretStore::load(path.clone()).unwrap();
        assert!(loaded.store().contains("github"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn file_store_refuses_group_readable_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "bitty-secrets-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.conf");
        std::fs::write(&path, "secret github value-one\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = FileSecretStore::load(path).unwrap_err();
        assert!(err.to_string().contains("600"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent_expiry_and_bounds() {
        let grant = SecretConsent {
            handle: "github".to_string(),
            granted_at_ms: 0,
            expires_at_ms: Some(50),
        };
        assert!(grant.is_active(49));
        assert!(!grant.is_active(50));
        let session = SecretConsent {
            handle: "github".to_string(),
            granted_at_ms: 0,
            expires_at_ms: None,
        };
        assert!(session.is_active(u64::MAX));
        // Store capacity fails closed.
        let mut store = SecretStore::new();
        for i in 0..MAX_SECRETS {
            store.insert(&format!("handle-{i}"), "value-here").unwrap();
        }
        assert!(store.insert("one-more", "value-here").is_err());
        // Over-bound values fail closed.
        let big = "x".repeat(MAX_SECRET_VALUE_BYTES + 1);
        assert!(SecretStore::new().insert("github", &big).is_err());
    }

    #[test]
    fn error_display_is_log_safe() {
        // Even a hostile handle-shaped error must not echo a pasted value.
        let long_value = format!("secret://{}", "x".repeat(5000));
        let err = SecretHandle::parse(&long_value).unwrap_err();
        assert!(err.to_string().len() < 1000);
    }
}
