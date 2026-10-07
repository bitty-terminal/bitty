//! Explicit local-user consent for debug/automation scopes plus a session
//! revocation surface (bitty issue #1520, follow-up from #1515 / CTX-0792).
//!
//! The accepted DevTools contract (`bitty-terminal-docs`
//! `specifications/devtools-rfc.md`, "Automation bearer scoping") requires:
//!
//! - item 1: automation bearer issuance only through an explicit local-user
//!   consent gesture on the owning authenticated session, never from flags,
//!   environment variables, configuration files, or child-process
//!   inheritance;
//! - item 4: revocation through the session-consent lifecycle (`revoke_scope`
//!   / `revoke_session`) with host-side detachment and an auditable receipt.
//!
//! #1515 built the server-owned connection authority (`ControlAuthority`
//! with consent generations, `grant_scope`, `revoke_scope`,
//! `revoke_session`, and session/principal/generation-bound automation
//! bearers) but left both halves unreachable: the connection-bound issuer
//! stays test-only and no user-facing action calls the revoke half.
//!
//! This module is the bitty-owned wiring between a live session and that
//! authority:
//!
//! - [`grant_consent_scope`] / [`grant_automation_consent`] are the only
//!   non-test callers of [`ControlAuthority::grant_scope`] in this
//!   repository. Both require an [`ExplicitConsent`] token, which production
//!   code can only obtain from [`ExplicitConsent::confirm_interactive`] (a
//!   local-TTY prompt; never flags, environment, configuration, or an
//!   inherited descriptor). Tests use [`ExplicitConsent::for_tests`].
//! - [`revoke_consent_scope`] / [`revoke_consent_session`] call
//!   [`ControlAuthority::revoke_scope`] / [`ControlAuthority::revoke_session`]
//!   (plus per-token [`revoke_automation_bearer`](bitty_ipc::devtools::revoke_automation_bearer)
//!   for ledger-tracked bearers) and return an auditable [`RevokeReceipt`].
//!   Revocation advances the consent generation (scope revoke) or ends the
//!   session, so queued controls deny at the next drain recheck
//!   (`authorize_at_drain`) and future requests deny, both with no side
//!   effect.
//! - The [`METHOD_GRANT_CONSENT_SCOPE`], [`METHOD_REVOKE_CONSENT_SCOPE`],
//!   and [`METHOD_REVOKE_CONSENT_SESSION`] IPC methods (registered by
//!   [`register_consent_methods`] on the bitty-owned [`Dispatcher`](bitty_ipc::devtools::Dispatcher))
//!   expose the same gesture to DevTools UI clients: the grant method
//!   requires the exact `ALLOW <what>` confirmation phrase for the requested
//!   grant, so the UI dialog is the gesture and the phrase is its evidence.
//!   `bitty ctl consent` speaks these methods (see `ctl::request`).
//!
//! Bearer binding note: the pinned `bitty-ipc` revision exposes the
//! session-bound minter
//! ([`issue_automation_bearer_with_ttl`](bitty_ipc::devtools::issue_automation_bearer_with_ttl))
//! and keeps its connection-bound (principal plus consent-generation)
//! issuer test-only. Bearers minted here are therefore bound to one session,
//! one terminal, and one method family with a capped TTL; principal and
//! consent-generation binding is enforced fail-closed at authorize time by
//! `bitty-ipc` (a bearer whose binding no longer matches the live session
//! denies with `ScopeDenied`). Granting the scopes through the authority is
//! what makes the session eligible; the receipt carries the post-grant
//! consent generation so a future connection-bound issuer can bind to it.
//! No bearer token is ever logged: receipts returned over IPC carry the
//! token once for the consenting caller, while [`ConsentReceipt::to_log_json`]
//! omits it.

use std::str::FromStr as _;

use bitty_ipc::ctl as ipc_ctl;
use bitty_ipc::devtools::{
    DevtoolsRequest, Dispatcher, HandlerError, ServeContext, issue_automation_bearer_with_ttl,
    revoke_automation_bearer,
};

/// Grant one consentable scope to the calling session (explicit gesture).
pub const METHOD_GRANT_CONSENT_SCOPE: &str = "bitty.debug/grantConsentScope";
/// Revoke one scope from the calling session (explicit action, receipt).
pub const METHOD_REVOKE_CONSENT_SCOPE: &str = "bitty.debug/revokeConsentScope";
/// End the calling session and revoke its tracked bearers (explicit action).
pub const METHOD_REVOKE_CONSENT_SESSION: &str = "bitty.debug/revokeConsentSession";

/// Prefix of every confirmation phrase (`ALLOW <what>`).
pub const CONSENT_CONFIRM_PREFIX: &str = "ALLOW ";

/// Maximum bytes of a `scope` / `terminalId` / `family` / `confirm` param.
///
/// Matches the IPC echo bounds: phrases stay short, bounded, and log-safe.
pub const MAX_CONSENT_PARAM_BYTES: usize = 128;

/// Scopes the consent gesture may grant (debug plus terminal automation
/// halves). Everything else (view, config, plugin, process, terminal.manage)
/// stays outside consent: granting those would widen the connection beyond
/// the automation/debug lane this gesture owns.
const CONSENTABLE_SCOPES: [bitty_ipc::Scope; 5] = [
    bitty_ipc::Scope::DebugInspect,
    bitty_ipc::Scope::DebugTrace,
    bitty_ipc::Scope::DebugControl,
    bitty_ipc::Scope::TerminalInspect,
    bitty_ipc::Scope::TerminalInput,
];

/// Failure of a consent or revoke operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentError {
    /// Malformed request shape (bad params, bad ids, oversize input).
    Usage(String),
    /// No live session for the gesture (unknown or revoked session).
    /// Servo-side only (the unix IPC servo): allowed dead elsewhere.
    #[cfg_attr(not(unix), allow(dead_code))]
    Unauthenticated(String),
    /// The gesture was not given (wrong confirmation phrase, no TTY) or the
    /// session is not eligible (missing family scopes, unknown family).
    Denied(String),
    /// The authority or bearer store is unavailable (poisoned lock, entropy
    /// failure, store exhaustion).
    Unavailable(String),
}

impl ConsentError {
    fn usage(reason: impl Into<String>) -> Self {
        Self::Usage(reason.into())
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn unauthenticated(reason: impl Into<String>) -> Self {
        Self::Unauthenticated(reason.into())
    }

    fn denied(reason: impl Into<String>) -> Self {
        Self::Denied(reason.into())
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }

    /// Short code for receipts and the wire (`InvalidParams`,
    /// `Unauthenticated`, `ScopeDenied`, `Unavailable`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "InvalidParams",
            Self::Unauthenticated(_) => "Unauthenticated",
            Self::Denied(_) => "ScopeDenied",
            Self::Unavailable(_) => "Unavailable",
        }
    }

    /// Human message (never carries a bearer token or secret).
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(reason)
            | Self::Unauthenticated(reason)
            | Self::Denied(reason)
            | Self::Unavailable(reason) => reason,
        }
    }

    /// Wire category for [`HandlerError`] (`usage` / `scope` / `transport`).
    /// Servo-side only (unix IPC handlers).
    #[cfg_attr(not(unix), allow(dead_code))]
    fn category(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Unauthenticated(_) | Self::Denied(_) => "scope",
            Self::Unavailable(_) => "transport",
        }
    }

    /// Servo-side only (unix IPC handlers).
    #[cfg_attr(not(unix), allow(dead_code))]
    fn to_handler_error(&self) -> HandlerError {
        HandlerError::new(self.category(), self.code(), self.message().to_string())
    }
}

/// Proof that the local user explicitly consented (opaque by construction).
///
/// Production code obtains this only from [`ExplicitConsent::confirm_interactive`],
/// which prompts on the local controlling terminal and requires the exact
/// `ALLOW <what>` phrase. There is deliberately no constructor from flags,
/// environment variables, configuration values, or inherited descriptors:
/// adding one would reopen the issuance path the accepted contract forbids.
/// Hermetic tests use [`ExplicitConsent::for_tests`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplicitConsent {
    _sealed: (),
}

impl ExplicitConsent {
    /// Prompt the local user and require the exact `ALLOW <what>` phrase.
    ///
    /// Fails closed when stdin is not a terminal (no piped/scripted consent),
    /// on EOF, or on any phrase mismatch. Reads nothing but the one typed
    /// line; no environment, flag, configuration, or inherited state is
    /// consulted.
    pub fn confirm_interactive(what: &str) -> Result<Self, ConsentError> {
        use std::io::{BufRead as _, IsTerminal as _};

        if what.is_empty() || what.len() > MAX_CONSENT_PARAM_BYTES {
            return Err(ConsentError::usage(
                "consent prompt needs a bounded scope description",
            ));
        }
        if !std::io::stdin().is_terminal() {
            return Err(ConsentError::denied(
                "consent needs a local terminal (no piped input)",
            ));
        }
        eprintln!("bitty: automation consent requested for {what}");
        eprintln!("bitty: type `ALLOW {what}` to grant, anything else aborts");
        let mut line = String::new();
        std::io::BufReader::new(std::io::stdin())
            .read_line(&mut line)
            .map_err(|err| ConsentError::unavailable(format!("consent prompt failed: {err}")))?;
        let typed = line.trim();
        if typed == expected_confirm_phrase(what) {
            Ok(Self { _sealed: () })
        } else {
            Err(ConsentError::denied("consent phrase mismatch"))
        }
    }

    /// Consent token for hermetic tests (no prompt, no environment).
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self { _sealed: () }
    }
}

/// Expected confirmation phrase for a grant description (`ALLOW <what>`).
#[must_use]
pub fn expected_confirm_phrase(what: &str) -> String {
    format!("{CONSENT_CONFIRM_PREFIX}{what}")
}

/// Parse a scope name and restrict it to the consentable lane.
///
/// Accepts the five debug/terminal-automation scopes; rejects everything
/// else (including `terminal.manage`) so the gesture cannot widen the
/// connection beyond its lane.
pub fn parse_consent_scope(raw: &str) -> Result<bitty_ipc::Scope, ConsentError> {
    if raw.is_empty() || raw.len() > MAX_CONSENT_PARAM_BYTES {
        return Err(ConsentError::usage("consent scope must be 1..=128 bytes"));
    }
    let scope = bitty_ipc::Scope::from_str(raw)
        .map_err(|_| ConsentError::usage(format!("unknown scope {raw:?}")))?;
    if CONSENTABLE_SCOPES.contains(&scope) {
        Ok(scope)
    } else {
        Err(ConsentError::denied(format!(
            "scope {raw:?} is not consentable (debug/terminal-automation lane only)"
        )))
    }
}

/// Automation method family a consent grant addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentFamily {
    /// `synthesizeInput` (`debug.control` plus `terminal.input`).
    Synthesize,
    /// `captureFrame` (`debug.trace` plus `terminal.inspect`).
    Capture,
}

impl ConsentFamily {
    /// Parse `synthesize` / `capture` (the [`AutomationFamily`](bitty_ipc::devtools::AutomationFamily)
    /// canonical tokens).
    pub fn parse(raw: &str) -> Result<Self, ConsentError> {
        match raw {
            "synthesize" => Ok(Self::Synthesize),
            "capture" => Ok(Self::Capture),
            _ => Err(ConsentError::usage(format!(
                "unknown automation family {raw:?} (want synthesize|capture)"
            ))),
        }
    }

    /// Canonical family token.
    /// Servo-side only (receipts served by the unix IPC servo).
    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synthesize => "synthesize",
            Self::Capture => "capture",
        }
    }

    /// The two scopes the family needs (debug scope plus terminal half).
    #[must_use]
    pub fn required_scopes(self) -> [bitty_ipc::Scope; 2] {
        match self {
            Self::Synthesize => [
                bitty_ipc::Scope::DebugControl,
                bitty_ipc::Scope::TerminalInput,
            ],
            Self::Capture => [
                bitty_ipc::Scope::DebugTrace,
                bitty_ipc::Scope::TerminalInspect,
            ],
        }
    }

    /// Grant description bound into the confirmation phrase
    /// (`synthesize t:1`, `capture t:2`). Servo-side only.
    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn grant_description(self, terminal_id: &str) -> String {
        format!("{} {terminal_id}", self.as_str())
    }

    /// Bearer TTL for the family (the accepted 10-minute automation cap).
    /// Servo-side only.
    #[cfg_attr(not(unix), allow(dead_code))]
    #[must_use]
    pub fn ttl_ms(self) -> u64 {
        bitty_ipc::devtools::AUTOMATION_BEARER_TTL_MS
    }

    /// Servo-side only.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn automation_family(self) -> bitty_ipc::devtools::AutomationFamily {
        match self {
            Self::Synthesize => bitty_ipc::devtools::AutomationFamily::Synthesize,
            Self::Capture => bitty_ipc::devtools::AutomationFamily::Capture,
        }
    }
}

/// Auditable receipt for a consent grant.
///
/// Returned to the consenting caller (over IPC: exactly once, in the method
/// result). The `bearer` field carries the freshly minted automation token
/// when a family was granted; it is never logged (see [`ConsentReceipt::to_log_json`]).
///
/// Servo-side receipt (returned by the unix IPC servo); allowed dead elsewhere.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentReceipt {
    /// Session the grant landed on.
    pub session_id: String,
    /// Scope granted by this gesture.
    pub scope: bitty_ipc::Scope,
    /// Terminal the automation bearer is bound to (family grants only).
    pub terminal_id: Option<String>,
    /// Family the automation bearer is bound to (family grants only).
    pub family: Option<ConsentFamily>,
    /// Freshly minted bearer token (family grants only; shown once).
    pub bearer: Option<String>,
    /// Consent generation after the grant (voids older snapshots/bearers).
    pub generation: u64,
    /// Grant time (caller-supplied clock, ms).
    pub at_ms: u64,
    /// True when the session already held the scope (no generation change).
    pub already_held: bool,
}

/// Servo-side only (the unix IPC servo renders it).
#[cfg_attr(not(unix), allow(dead_code))]
impl ConsentReceipt {
    /// Full receipt JSON for the consenting caller (includes `bearer` once).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = format!(
            "{{\"sessionId\":\"{}\",\"scope\":\"{}\",\"generation\":{},\"atMs\":{},\"alreadyHeld\":{}",
            json_escape(&self.session_id),
            self.scope.as_str(),
            self.generation,
            self.at_ms,
            self.already_held,
        );
        if let Some(terminal) = &self.terminal_id {
            out.push_str(&format!(",\"terminalId\":\"{}\"", json_escape(terminal)));
        }
        if let Some(family) = &self.family {
            out.push_str(&format!(",\"family\":\"{}\"", family.as_str()));
        }
        if let Some(bearer) = &self.bearer {
            out.push_str(&format!(",\"bearer\":\"{}\"", json_escape(bearer)));
        }
        out.push('}');
        out
    }

    /// Log-safe receipt JSON (never carries the bearer token).
    #[must_use]
    pub fn to_log_json(&self) -> String {
        format!(
            "{{\"sessionId\":\"{}\",\"scope\":\"{}\",\"generation\":{},\"atMs\":{},\"alreadyHeld\":{},\"bearerIssued\":{}}}",
            json_escape(&self.session_id),
            self.scope.as_str(),
            self.generation,
            self.at_ms,
            self.already_held,
            self.bearer.is_some(),
        )
    }
}

/// Auditable receipt for a revocation (scope or session).
///
/// Servo-side receipt (returned by the unix IPC servo); allowed dead elsewhere.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeReceipt {
    /// Session the revocation ran against.
    pub session_id: String,
    /// Scope revoked (`None` for a full session revoke).
    pub scope: Option<bitty_ipc::Scope>,
    /// True when the session (and scope, for scope revokes) existed.
    pub revoked: bool,
    /// Tracked bearer tokens revoked alongside (session revoke only).
    pub bearers_revoked: usize,
    /// Consent generation after a scope revoke (`None` when the session is gone).
    pub generation: Option<u64>,
    /// Revoke time (caller-supplied clock, ms).
    pub at_ms: u64,
}

/// Servo-side only (the unix IPC servo renders it).
#[cfg_attr(not(unix), allow(dead_code))]
impl RevokeReceipt {
    /// Receipt JSON (no secrets: revocations carry no tokens).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = format!(
            "{{\"sessionId\":\"{}\",\"revoked\":{},\"bearersRevoked\":{},\"atMs\":{}",
            json_escape(&self.session_id),
            self.revoked,
            self.bearers_revoked,
            self.at_ms,
        );
        if let Some(scope) = &self.scope {
            out.push_str(&format!(",\"scope\":\"{}\"", scope.as_str()));
        }
        if let Some(generation) = self.generation {
            out.push_str(&format!(",\"generation\":{generation}"));
        }
        out.push('}');
        out
    }
}

/// Build `grantConsentScope` params JSON for a validated grant.
///
/// `confirm` is the exact `ALLOW <what>` phrase the local user typed at the
/// interactive prompt; callers must never source it from flags, environment,
/// configuration, or inherited state. An empty `confirm` always denies
/// server-side (fail closed), which is what [`CtlRequest::wire_params`](crate::ctl::request::CtlRequest::wire_params)
/// emits before the execute path splices the prompted phrase in.
#[must_use]
pub fn consent_grant_params(
    scope: &str,
    terminal_id: Option<&str>,
    family: Option<&str>,
    confirm: &str,
) -> String {
    let mut out = format!(
        "{{\"scope\":\"{}\",\"confirm\":\"{}\"",
        json_escape(scope),
        json_escape(confirm)
    );
    if let Some(terminal) = terminal_id {
        out.push_str(&format!(",\"terminalId\":\"{}\"", json_escape(terminal)));
    }
    if let Some(family) = family {
        out.push_str(&format!(",\"family\":\"{}\"", json_escape(family)));
    }
    out.push('}');
    out
}

/// Grant one consentable scope to a live session (explicit gesture only).
///
/// Requires `consent` (see [`ExplicitConsent`]); reads no environment, flag,
/// configuration, or inherited state. Fails closed for unknown sessions.
/// Returns `already_held: true` without touching the generation when the
/// session already holds the scope.
///
/// Servo-side only (the unix IPC servo and its tests call it).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn grant_consent_scope(
    authority: &ipc_ctl::ControlAuthority,
    session_id: &str,
    scope: bitty_ipc::Scope,
    consent: &ExplicitConsent,
    now_ms: u64,
) -> Result<ConsentReceipt, ConsentError> {
    let _ = consent;
    if !CONSENTABLE_SCOPES.contains(&scope) {
        return Err(ConsentError::denied(format!(
            "scope {:?} is not consentable",
            scope.as_str()
        )));
    }
    let before = authority
        .snapshot(session_id)
        .map_err(|_| ConsentError::unauthenticated("connection authority is no longer active"))?;
    if before.scopes.contains(scope) {
        return Ok(ConsentReceipt {
            session_id: session_id.to_string(),
            scope,
            terminal_id: None,
            family: None,
            bearer: None,
            generation: before.identity.consent_generation,
            at_ms: now_ms,
            already_held: true,
        });
    }
    if !authority.grant_scope(session_id, scope) {
        return Err(ConsentError::unauthenticated(
            "connection authority is no longer active",
        ));
    }
    let after = authority
        .snapshot(session_id)
        .map_err(|_| ConsentError::unavailable("connection authority vanished after grant"))?;
    let receipt = ConsentReceipt {
        session_id: session_id.to_string(),
        scope,
        terminal_id: None,
        family: None,
        bearer: None,
        generation: after.identity.consent_generation,
        at_ms: now_ms,
        already_held: false,
    };
    crate::logging::warn(|| format!("bitty: consent granted {}", receipt.to_log_json()));
    Ok(receipt)
}

/// Grant an automation family to one terminal of a live session and mint its
/// bearer through the connection authority (explicit gesture only).
///
/// Grants the family's debug scope plus its terminal half, then mints a
/// bearer bound to (session, terminal, family) with the family TTL. The
/// bearer lives in memory only, expires with the TTL or the session, and is
/// returned exactly once in [`ConsentReceipt::bearer`]. See the module docs
/// for the binding limits against the pinned `bitty-ipc` revision.
///
/// Servo-side only (the unix IPC servo and its tests call it).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn grant_automation_consent(
    authority: &ipc_ctl::ControlAuthority,
    session_id: &str,
    terminal_id: &str,
    family: ConsentFamily,
    consent: &ExplicitConsent,
    now_ms: u64,
) -> Result<ConsentReceipt, ConsentError> {
    let _ = consent;
    ipc_ctl::parse_terminal_id(terminal_id)
        .map_err(|err| ConsentError::usage(format!("invalid terminal id: {err}")))?;
    authority
        .snapshot(session_id)
        .map_err(|_| ConsentError::unauthenticated("connection authority is no longer active"))?;
    let mut already_held = true;
    for scope in family.required_scopes() {
        let receipt = grant_consent_scope(authority, session_id, scope, consent, now_ms)?;
        already_held = already_held && receipt.already_held;
    }
    let bearer = issue_automation_bearer_with_ttl(
        session_id,
        terminal_id,
        family.automation_family(),
        now_ms,
        family.ttl_ms(),
    )
    .map_err(|err| {
        let (_, _, message) = ipc_error_triple(&err);
        ConsentError::unavailable(format!("bearer issuance failed: {message}"))
    })?;
    let after = authority
        .snapshot(session_id)
        .map_err(|_| ConsentError::unavailable("connection authority vanished after grant"))?;
    let receipt = ConsentReceipt {
        session_id: session_id.to_string(),
        scope: family.required_scopes()[0],
        terminal_id: Some(terminal_id.to_string()),
        family: Some(family),
        bearer: Some(bearer),
        generation: after.identity.consent_generation,
        at_ms: now_ms,
        already_held,
    };
    crate::logging::warn(|| format!("bitty: consent granted {}", receipt.to_log_json()));
    Ok(receipt)
}

/// Revoke one scope from a live session (explicit action).
///
/// Strips the scope from the session and every terminal capability entry and
/// advances the consent generation, so queued controls and bearers issued
/// under the old consent deny at the next dispatch boundary with no side
/// effect. Infallible receipt: `revoked: false` when the session is gone or
/// never held the scope.
///
/// Servo-side only (the unix IPC servo and its tests call it).
#[cfg_attr(not(unix), allow(dead_code))]
#[must_use]
pub fn revoke_consent_scope(
    authority: &ipc_ctl::ControlAuthority,
    session_id: &str,
    scope: bitty_ipc::Scope,
    now_ms: u64,
) -> RevokeReceipt {
    let revoked = authority.revoke_scope(session_id, scope);
    let generation = authority
        .snapshot(session_id)
        .ok()
        .map(|snapshot| snapshot.identity.consent_generation);
    let receipt = RevokeReceipt {
        session_id: session_id.to_string(),
        scope: Some(scope),
        revoked,
        bearers_revoked: 0,
        generation,
        at_ms: now_ms,
    };
    crate::logging::warn(|| format!("bitty: consent revoked {}", receipt.to_json()));
    receipt
}

/// End a session and revoke its tracked bearers (explicit action).
///
/// Queued and future requests of the session deny from the next dispatch
/// boundary on (the live snapshot is gone); every tracked bearer token is
/// revoked alongside so nothing minted under the session outlives it.
///
/// Servo-side only (the unix IPC servo and its tests call it).
#[cfg_attr(not(unix), allow(dead_code))]
#[must_use]
pub fn revoke_consent_session(
    authority: &ipc_ctl::ControlAuthority,
    session_id: &str,
    tracked_bearers: &[String],
    now_ms: u64,
) -> RevokeReceipt {
    let mut bearers_revoked = 0usize;
    for token in tracked_bearers {
        if revoke_automation_bearer(token) {
            bearers_revoked += 1;
        }
    }
    let revoked = authority.revoke_session(session_id);
    let receipt = RevokeReceipt {
        session_id: session_id.to_string(),
        scope: None,
        revoked,
        bearers_revoked,
        generation: None,
        at_ms: now_ms,
    };
    crate::logging::warn(|| format!("bitty: session revoked {}", receipt.to_json()));
    receipt
}

/// Split an [`IpcError`](bitty_ipc::IpcError) into (category, code, message).
/// Servo-side only (unix IPC handlers).
#[cfg_attr(not(unix), allow(dead_code))]
fn ipc_error_triple(error: &bitty_ipc::IpcError) -> (&'static str, String, String) {
    match error {
        bitty_ipc::IpcError::FrameTooLarge { actual, limit } => (
            "transport",
            "FrameTooLarge".into(),
            format!("frame {actual} exceeds {limit}"),
        ),
        bitty_ipc::IpcError::PayloadTooLarge {
            field,
            limit,
            actual,
        } => (
            "transport",
            "PayloadTooLarge".into(),
            format!("{field} exceeds {limit} (got {actual})"),
        ),
        bitty_ipc::IpcError::FrameTruncated { expected, actual } => (
            "transport",
            "Transport".into(),
            format!("frame truncated (expected {expected}, got {actual})"),
        ),
        bitty_ipc::IpcError::InvalidFrame { reason }
        | bitty_ipc::IpcError::InvalidRequest { reason } => {
            ("usage", "InvalidRequest".into(), reason.clone())
        }
        bitty_ipc::IpcError::ChannelFull { capacity }
        | bitty_ipc::IpcError::TransportFull { capacity } => (
            "budget",
            "RateLimited".into(),
            format!("channel at capacity ({capacity})"),
        ),
        bitty_ipc::IpcError::ChannelClosed { reason }
        | bitty_ipc::IpcError::TransportClosed { reason }
        | bitty_ipc::IpcError::Transport { reason } => {
            ("transport", "Transport".into(), reason.clone())
        }
        bitty_ipc::IpcError::Timeout {
            request_id,
            timeout_ms,
        } => (
            "transport",
            "Timeout".into(),
            format!("request {request_id} timed out after {timeout_ms}ms"),
        ),
        bitty_ipc::IpcError::PendingLimitExceeded { limit, actual } => (
            "budget",
            "LimitExceeded".into(),
            format!("pending {actual} exceeds {limit}"),
        ),
        bitty_ipc::IpcError::InvalidMethod { method, reason } => (
            "usage",
            "InvalidMethod".into(),
            format!("invalid method {method}: {reason}"),
        ),
        bitty_ipc::IpcError::ScopeDenied { scope, action } => (
            "scope",
            "ScopeDenied".into(),
            format!("scope {scope:?} denied for {action}"),
        ),
        bitty_ipc::IpcError::Denied { code, reason } => ("scope", code.clone(), reason.clone()),
        bitty_ipc::IpcError::Unauthenticated { reason } => {
            ("scope", "Unauthenticated".into(), reason.clone())
        }
        bitty_ipc::IpcError::LimitExceeded {
            field,
            limit,
            actual,
        } => (
            "budget",
            "LimitExceeded".into(),
            format!("{field} exceeds {limit} (got {actual})"),
        ),
        bitty_ipc::IpcError::VersionMismatch { expected, actual } => (
            "transport",
            "VersionMismatch".into(),
            format!("version mismatch (expected {expected}, got {actual})"),
        ),
        bitty_ipc::IpcError::NotFound { reason } => ("usage", "NotFound".into(), reason.clone()),
        bitty_ipc::IpcError::Unavailable { reason } => {
            ("transport", "Unavailable".into(), reason.clone())
        }
        bitty_ipc::IpcError::Internal { reason } => {
            ("transport", "Unavailable".into(), reason.clone())
        }
    }
}

/// Escape a string for embedding in receipt JSON (control bytes safe).
fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out
}

/// Current time in milliseconds since the Unix epoch (server clock for receipts).
/// Servo-side only (unix IPC handlers).
#[cfg_attr(not(unix), allow(dead_code))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Extract a flat `"key": "string"` field from a params object (bounded).
/// Servo-side only (unix IPC handlers).
#[cfg_attr(not(unix), allow(dead_code))]
fn extract_string_param(params_raw: Option<&str>, key: &str) -> Option<String> {
    let raw = params_raw?;
    let needle = format!("\"{key}\"");
    let pos = raw.find(&needle)?;
    let after = &raw[pos + needle.len()..];
    let colon = after.find(':')?;
    let mut rest = after[colon + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    rest = &rest[1..];
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        let ch = chars.next()?;
        match ch {
            '"' => break,
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                _ => return None,
            },
            ch if (ch as u32) < 0x20 => return None,
            ch => out.push(ch),
        }
        if out.len() > MAX_CONSENT_PARAM_BYTES {
            return None;
        }
    }
    Some(out)
}

/// Live connection authority behind a request, or fail closed.
///
/// Consent needs a real authority: hermetic authority-less contexts (explicit
/// test constructors) answer `Unauthenticated` instead of widening anything.
///
/// Servo-side only (unix IPC handlers).
#[cfg_attr(not(unix), allow(dead_code))]
fn live_authority(
    context: &ServeContext,
) -> Result<(ipc_ctl::ControlAuthority, String), HandlerError> {
    if !context.authority_required() {
        return Err(HandlerError::new(
            "scope",
            "Unauthenticated",
            "consent needs a live connection authority".to_string(),
        ));
    }
    let grant = context.connection_grant().ok_or_else(|| {
        HandlerError::new(
            "scope",
            "Unauthenticated",
            "connection authority is unavailable".to_string(),
        )
    })?;
    Ok((grant.authority().clone(), grant.session_id().to_string()))
}

/// `bitty.debug/grantConsentScope`: explicit-gesture scope grant.
///
/// Params: `{"scope":"debug.control","confirm":"ALLOW debug.control"}` for a
/// plain scope, or `{"scope":"debug.control","terminalId":"t:1",
/// "family":"synthesize","confirm":"ALLOW synthesize t:1"}` for an
/// automation family (grants both family scopes and mints the bearer).
/// The grant lands on the calling session only: there is no cross-session
/// grant, so one connection can never widen another.
///
/// Servo-side only (registered on the unix IPC servo dispatcher).
#[cfg_attr(not(unix), allow(dead_code))]
fn handle_grant_consent_scope(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let params = request.params_raw.as_deref();
    let scope_raw = extract_string_param(params, "scope").ok_or_else(|| {
        HandlerError::new(
            "usage",
            "InvalidParams",
            "grantConsentScope needs a string \"scope\"".to_string(),
        )
    })?;
    let scope = parse_consent_scope(&scope_raw).map_err(|err| err.to_handler_error())?;
    let (authority, session_id) = live_authority(context)?;
    let at = now_ms();

    if let Some(family_raw) = extract_string_param(params, "family") {
        let family = ConsentFamily::parse(&family_raw).map_err(|err| err.to_handler_error())?;
        let terminal_id = extract_string_param(params, "terminalId").ok_or_else(|| {
            HandlerError::new(
                "usage",
                "InvalidParams",
                "grantConsentScope with \"family\" needs a string \"terminalId\"".to_string(),
            )
        })?;
        let expected = expected_confirm_phrase(&family.grant_description(&terminal_id));
        let confirm = extract_string_param(params, "confirm").unwrap_or_default();
        if confirm != expected {
            return Err(HandlerError::new(
                "scope",
                "ScopeDenied",
                "consent phrase mismatch (explicit ALLOW required)".to_string(),
            ));
        }
        if family.required_scopes()[0] != scope {
            return Err(HandlerError::new(
                "usage",
                "InvalidParams",
                "family scope must match \"scope\"".to_string(),
            ));
        }
        // The gesture token is proven by the exact phrase above; the sealed
        // test token stands in for hermetic tests (same code path).
        let receipt = grant_automation_consent(
            &authority,
            &session_id,
            &terminal_id,
            family,
            &ExplicitConsent { _sealed: () },
            at,
        )
        .map_err(|err| err.to_handler_error())?;
        return Ok(receipt.to_json());
    }

    if extract_string_param(params, "terminalId").is_some() {
        return Err(HandlerError::new(
            "usage",
            "InvalidParams",
            "\"terminalId\" needs a \"family\"".to_string(),
        ));
    }
    let expected = expected_confirm_phrase(scope.as_str());
    let confirm = extract_string_param(params, "confirm").unwrap_or_default();
    if confirm != expected {
        return Err(HandlerError::new(
            "scope",
            "ScopeDenied",
            "consent phrase mismatch (explicit ALLOW required)".to_string(),
        ));
    }
    let receipt = grant_consent_scope(
        &authority,
        &session_id,
        scope,
        &ExplicitConsent { _sealed: () },
        at,
    )
    .map_err(|err| err.to_handler_error())?;
    Ok(receipt.to_json())
}

/// `bitty.debug/revokeConsentScope`: explicit scope revoke with receipt.
///
/// Params: `{"scope":"debug.control"}`. Acts on the calling session only.
///
/// Servo-side only (registered on the unix IPC servo dispatcher).
#[cfg_attr(not(unix), allow(dead_code))]
fn handle_revoke_consent_scope(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let scope_raw =
        extract_string_param(request.params_raw.as_deref(), "scope").ok_or_else(|| {
            HandlerError::new(
                "usage",
                "InvalidParams",
                "revokeConsentScope needs a string \"scope\"".to_string(),
            )
        })?;
    let scope = parse_consent_scope(&scope_raw).map_err(|err| err.to_handler_error())?;
    let (authority, session_id) = live_authority(context)?;
    Ok(revoke_consent_scope(&authority, &session_id, scope, now_ms()).to_json())
}

/// `bitty.debug/revokeConsentSession`: end the calling session with receipt.
///
/// Params: `{}` (acts on the calling session) or `{"bearer":"<token>"}` to
/// revoke one tracked bearer alongside. Queued and future requests of the
/// session deny from the next dispatch boundary on.
///
/// Servo-side only (registered on the unix IPC servo dispatcher).
#[cfg_attr(not(unix), allow(dead_code))]
fn handle_revoke_consent_session(
    context: &ServeContext,
    request: &DevtoolsRequest,
) -> Result<String, HandlerError> {
    let (authority, session_id) = live_authority(context)?;
    let tracked: Vec<String> = extract_string_param(request.params_raw.as_deref(), "bearer")
        .into_iter()
        .collect();
    Ok(revoke_consent_session(&authority, &session_id, &tracked, now_ms()).to_json())
}

/// Register the consent/revoke methods on a bitty-owned [`Dispatcher`].
///
/// Additive-only over `with_defaults` / `with_test_mode`: no existing
/// method, scope, bound, or error shape changes. Method names are
/// statically valid, so a registration failure is a programming error.
///
/// Servo-side only (called by the unix IPC servo setup).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn register_consent_methods(dispatcher: &mut Dispatcher) -> Result<(), bitty_ipc::IpcError> {
    dispatcher.register(METHOD_GRANT_CONSENT_SCOPE, handle_grant_consent_scope)?;
    dispatcher.register(METHOD_REVOKE_CONSENT_SCOPE, handle_revoke_consent_scope)?;
    dispatcher.register(METHOD_REVOKE_CONSENT_SESSION, handle_revoke_consent_session)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_authority() -> ipc_ctl::ControlAuthority {
        ipc_ctl::ControlAuthority::new()
    }

    fn open_test_session(authority: &ipc_ctl::ControlAuthority) -> ipc_ctl::ConnectionGrant {
        authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test connection")
    }

    #[test]
    fn consent_scope_allowlist_covers_debug_and_terminal_halves() {
        for raw in [
            "debug.inspect",
            "debug.trace",
            "debug.control",
            "terminal.inspect",
            "terminal.input",
        ] {
            assert!(parse_consent_scope(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn consent_scope_rejects_widening_scopes() {
        for raw in [
            "terminal.manage",
            "view.inspect",
            "view.manage",
            "config.modify",
            "plugin.manage",
            "process.spawn",
            "debug.admin",
            "",
        ] {
            assert!(parse_consent_scope(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn consent_scope_rejects_oversize_and_unknown() {
        assert!(parse_consent_scope(&"x".repeat(MAX_CONSENT_PARAM_BYTES + 1)).is_err());
        assert!(parse_consent_scope("debug.inspect\0").is_err());
    }

    #[test]
    fn confirm_phrase_is_exact_and_scope_bound() {
        assert_eq!(
            expected_confirm_phrase("debug.control"),
            "ALLOW debug.control"
        );
        assert_ne!(
            expected_confirm_phrase("debug.control"),
            "ALLOW debug.trace"
        );
        assert_ne!(
            expected_confirm_phrase("debug.control"),
            "allow debug.control"
        );
    }

    #[test]
    fn consent_family_parses_and_maps_scopes() {
        let synth = ConsentFamily::parse("synthesize").expect("synthesize");
        assert_eq!(
            synth.required_scopes(),
            [
                bitty_ipc::Scope::DebugControl,
                bitty_ipc::Scope::TerminalInput
            ]
        );
        let cap = ConsentFamily::parse("capture").expect("capture");
        assert_eq!(
            cap.required_scopes(),
            [
                bitty_ipc::Scope::DebugTrace,
                bitty_ipc::Scope::TerminalInspect
            ]
        );
        assert!(ConsentFamily::parse("frame-digest").is_err());
        assert!(ConsentFamily::parse("").is_err());
    }

    #[test]
    fn grant_scope_lands_and_reports_generation() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let receipt = grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugControl,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("grant");
        assert!(!receipt.already_held);
        assert_eq!(receipt.session_id, grant.session_id());
        assert_eq!(receipt.scope, bitty_ipc::Scope::DebugControl);
        assert_eq!(receipt.at_ms, 1_000);
        let snapshot = authority.snapshot(grant.session_id()).expect("snapshot");
        assert!(snapshot.scopes.contains(bitty_ipc::Scope::DebugControl));
        assert_eq!(snapshot.identity.consent_generation, receipt.generation);
    }

    #[test]
    fn grant_scope_is_idempotent_without_generation_change() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let first = grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugTrace,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("first grant");
        assert!(!first.already_held);
        let second = grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugTrace,
            &ExplicitConsent::for_tests(),
            2_000,
        )
        .expect("second grant");
        assert!(second.already_held);
        assert_eq!(second.generation, first.generation);
    }

    #[test]
    fn grant_scope_fails_closed_for_unknown_session() {
        let authority = test_authority();
        let err = grant_consent_scope(
            &authority,
            "session-404",
            bitty_ipc::Scope::DebugControl,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect_err("unknown session must fail");
        assert_eq!(err.code(), "Unauthenticated");
    }

    #[test]
    fn grant_scope_never_reads_operator_ceiling() {
        // The gesture outcome is independent of BITTY_CTL_ELEVATE: the
        // operator ceiling seeds open-time scopes only, never consent. The
        // grant functions take no environment input by construction (this
        // module contains no `std::env` read; `rg 'std::env|BITTY_'`
        // over `consent.rs` must stay empty outside this comment), so a
        // session opened with empty scopes is granted purely by gesture.
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let before = authority.snapshot(grant.session_id()).expect("snapshot");
        assert!(!before.scopes.contains(bitty_ipc::Scope::DebugControl));
        let receipt = grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugControl,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("grant ignores env");
        assert!(!receipt.already_held);
    }

    #[test]
    fn automation_grant_mints_session_bound_bearer() {
        bitty_ipc::devtools::clear_automation_for_tests();
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let before_count = bitty_ipc::devtools::automation_bearer_count_for_tests();
        let receipt = grant_automation_consent(
            &authority,
            grant.session_id(),
            "t:1",
            ConsentFamily::Synthesize,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("automation grant");
        assert_eq!(receipt.terminal_id.as_deref(), Some("t:1"));
        assert_eq!(receipt.family, Some(ConsentFamily::Synthesize));
        let bearer = receipt.bearer.clone().expect("bearer issued");
        assert!(!bearer.is_empty());
        assert_eq!(
            bitty_ipc::devtools::automation_bearer_count_for_tests(),
            before_count + 1
        );
        // The log form never carries the token.
        assert!(!receipt.to_log_json().contains(&bearer));
        assert!(receipt.to_log_json().contains("bearerIssued\":true"));
        // Full receipt carries it exactly once for the consenting caller.
        assert_eq!(receipt.to_json().matches(&bearer).count(), 1);
        bitty_ipc::devtools::clear_automation_for_tests();
    }

    #[test]
    fn automation_grant_rejects_bad_terminal() {
        bitty_ipc::devtools::clear_automation_for_tests();
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let err = grant_automation_consent(
            &authority,
            grant.session_id(),
            "nope",
            ConsentFamily::Capture,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect_err("bad terminal must fail");
        assert_eq!(err.code(), "InvalidParams");
        bitty_ipc::devtools::clear_automation_for_tests();
    }

    #[test]
    fn revoke_scope_denies_future_snapshot_scopes() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugControl,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("grant");
        let receipt = revoke_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugControl,
            2_000,
        );
        assert!(receipt.revoked);
        assert_eq!(receipt.scope, Some(bitty_ipc::Scope::DebugControl));
        let snapshot = authority
            .snapshot(grant.session_id())
            .expect("session lives");
        assert!(!snapshot.scopes.contains(bitty_ipc::Scope::DebugControl));
    }

    #[test]
    fn granted_terminal_scope_authorizes_addressed_read() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let params = ipc_ctl::params_terminal_id("t:1");
        let before = authority.snapshot(grant.session_id()).expect("snapshot");
        assert!(
            ipc_ctl::authorize_ctl_action(
                ipc_ctl::METHOD_GET_TERMINAL_TEXT,
                Some(&params),
                &before
            )
            .is_err()
        );
        grant_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::TerminalInspect,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("grant");
        let after = authority.snapshot(grant.session_id()).expect("snapshot");
        assert!(
            ipc_ctl::authorize_ctl_action(ipc_ctl::METHOD_GET_TERMINAL_TEXT, Some(&params), &after)
                .is_ok()
        );
    }

    #[test]
    fn revoke_scope_is_stable_for_unknown_session_or_scope() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let missing_scope = revoke_consent_scope(
            &authority,
            grant.session_id(),
            bitty_ipc::Scope::DebugControl,
            1_000,
        );
        assert!(!missing_scope.revoked);
        let missing_session = revoke_consent_scope(
            &authority,
            "session-404",
            bitty_ipc::Scope::DebugControl,
            1_000,
        );
        assert!(!missing_session.revoked);
        assert_eq!(missing_session.generation, None);
    }

    #[test]
    fn revoke_session_ends_authorization_and_tracked_bearers() {
        bitty_ipc::devtools::clear_automation_for_tests();
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let session = grant.session_id().to_string();
        let consent_receipt = grant_automation_consent(
            &authority,
            &session,
            "t:2",
            ConsentFamily::Capture,
            &ExplicitConsent::for_tests(),
            1_000,
        )
        .expect("automation grant");
        let bearer = consent_receipt.bearer.expect("bearer");
        let before_count = bitty_ipc::devtools::automation_bearer_count_for_tests();
        assert!(before_count >= 1);
        let receipt = revoke_consent_session(&authority, &session, &[bearer], 2_000);
        assert!(receipt.revoked);
        assert_eq!(
            bitty_ipc::devtools::automation_bearer_count_for_tests(),
            before_count - 1
        );
        assert!(authority.snapshot(&session).is_err());
        bitty_ipc::devtools::clear_automation_for_tests();
    }

    #[test]
    fn revoke_session_is_idempotent() {
        let authority = test_authority();
        let grant = open_test_session(&authority);
        let session = grant.session_id().to_string();
        let first = revoke_consent_session(&authority, &session, &[], 1_000);
        assert!(first.revoked);
        let second = revoke_consent_session(&authority, &session, &[], 2_000);
        assert!(!second.revoked);
    }

    fn test_server() -> bitty_ipc::devtools::ServerInfo {
        bitty_ipc::devtools::ServerInfo::new(
            String::from("consent-test"),
            String::from("/tmp/bitty-consent-test.sock"),
            80,
            24,
        )
    }

    fn authority_context(
        server: &bitty_ipc::devtools::ServerInfo,
        authority: &ipc_ctl::ControlAuthority,
    ) -> ServeContext {
        let grant = authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test grant");
        ServeContext::with_connection_grant(server, grant)
    }

    fn devtools_request(method: &str, params: &str) -> DevtoolsRequest {
        DevtoolsRequest {
            id_raw: String::from("1"),
            method: method.to_string(),
            has_jsonrpc: true,
            params_raw: Some(params.to_string()),
        }
    }

    #[test]
    fn grant_handler_requires_exact_phrase() {
        let server = test_server();
        let authority = test_authority();
        let grant = authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test grant");
        let context = ServeContext::with_connection_grant(&server, grant);
        let session = context.session_id().to_string();

        // Wrong phrase denies.
        let err = handle_grant_consent_scope(
            &context,
            &devtools_request(
                METHOD_GRANT_CONSENT_SCOPE,
                "{\"scope\":\"debug.control\",\"confirm\":\"ALLOW debug.trace\"}",
            ),
        )
        .expect_err("phrase mismatch must deny");
        assert_eq!(err.code, "ScopeDenied");

        // Exact phrase grants.
        let ok = handle_grant_consent_scope(
            &context,
            &devtools_request(
                METHOD_GRANT_CONSENT_SCOPE,
                "{\"scope\":\"debug.control\",\"confirm\":\"ALLOW debug.control\"}",
            ),
        )
        .expect("exact phrase grants");
        assert!(ok.contains(&session));
        assert!(ok.contains("debug.control"));
        let snapshot = authority.snapshot(&session).expect("snapshot");
        assert!(snapshot.scopes.contains(bitty_ipc::Scope::DebugControl));
    }

    #[test]
    fn grant_handler_rejects_cross_family_mismatch() {
        let server = test_server();
        let authority = test_authority();
        let grant = authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test grant");
        let context = ServeContext::with_connection_grant(&server, grant);
        let err = handle_grant_consent_scope(
            &context,
            &devtools_request(
                METHOD_GRANT_CONSENT_SCOPE,
                "{\"scope\":\"debug.trace\",\"terminalId\":\"t:1\",\"family\":\"synthesize\",\"confirm\":\"ALLOW synthesize t:1\"}",
            ),
        )
        .expect_err("family/scope mismatch must fail");
        assert_eq!(err.code, "InvalidParams");
    }

    #[test]
    fn grant_handler_mints_family_bearer_on_exact_phrase() {
        bitty_ipc::devtools::clear_automation_for_tests();
        let server = test_server();
        let authority = test_authority();
        let grant = authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test grant");
        let context = ServeContext::with_connection_grant(&server, grant);
        let session = context.session_id().to_string();
        let ok = handle_grant_consent_scope(
            &context,
            &devtools_request(
                METHOD_GRANT_CONSENT_SCOPE,
                "{\"scope\":\"debug.control\",\"terminalId\":\"t:1\",\"family\":\"synthesize\",\"confirm\":\"ALLOW synthesize t:1\"}",
            ),
        )
        .expect("family grant");
        assert!(ok.contains("\"bearer\":\""));
        assert!(ok.contains("\"family\":\"synthesize\""));
        let snapshot = authority.snapshot(&session).expect("snapshot");
        assert!(snapshot.scopes.contains(bitty_ipc::Scope::DebugControl));
        assert!(snapshot.scopes.contains(bitty_ipc::Scope::TerminalInput));
        bitty_ipc::devtools::clear_automation_for_tests();
    }

    #[test]
    fn revoke_handlers_act_on_calling_session_only() {
        let server = test_server();
        let authority = test_authority();
        let grant = authority
            .open_connection(
                bitty_ipc::ScopeSet::new(),
                ipc_ctl::TerminalCapabilities::from_scopes(&bitty_ipc::ScopeSet::new()),
            )
            .expect("test grant");
        let context = ServeContext::with_connection_grant(&server, grant);
        let session = context.session_id().to_string();

        // A "sessionId" param cannot steer the revoke elsewhere: the handler
        // ignores unknown fields and always acts on the caller.
        let ok = handle_revoke_consent_scope(
            &context,
            &devtools_request(
                METHOD_REVOKE_CONSENT_SCOPE,
                "{\"scope\":\"debug.control\",\"sessionId\":\"session-999\"}",
            ),
        )
        .expect("revoke parses");
        assert!(ok.contains(&session));
        assert!(!ok.contains("session-999"));

        let ok = handle_revoke_consent_session(
            &context,
            &devtools_request(METHOD_REVOKE_CONSENT_SESSION, "{}"),
        )
        .expect("session revoke");
        assert!(ok.contains(&session));
        assert!(authority.snapshot(&session).is_err());
    }

    #[test]
    fn consent_handlers_reject_authority_less_contexts() {
        let server = test_server();
        let context = ServeContext::with_granted(&server, bitty_ipc::ScopeSet::all());
        let err = handle_grant_consent_scope(
            &context,
            &devtools_request(
                METHOD_GRANT_CONSENT_SCOPE,
                "{\"scope\":\"debug.control\",\"confirm\":\"ALLOW debug.control\"}",
            ),
        )
        .expect_err("authority-less grant must fail");
        assert_eq!(err.code, "Unauthenticated");
        let err = handle_revoke_consent_session(
            &context,
            &devtools_request(METHOD_REVOKE_CONSENT_SESSION, "{}"),
        )
        .expect_err("authority-less revoke must fail");
        assert_eq!(err.code, "Unauthenticated");
    }

    #[test]
    fn consent_methods_register_additively() {
        let mut dispatcher = Dispatcher::with_defaults();
        assert!(!dispatcher.contains(METHOD_GRANT_CONSENT_SCOPE));
        register_consent_methods(&mut dispatcher).expect("register");
        assert!(dispatcher.contains(METHOD_GRANT_CONSENT_SCOPE));
        assert!(dispatcher.contains(METHOD_REVOKE_CONSENT_SCOPE));
        assert!(dispatcher.contains(METHOD_REVOKE_CONSENT_SESSION));
        // Existing methods survive registration.
        assert!(dispatcher.contains("bitty.debug/ping"));
    }

    #[test]
    fn authority_context_helper_binds_a_live_session() {
        let server = test_server();
        let authority = test_authority();
        let context = authority_context(&server, &authority);
        // The helper binds whatever session the authority minted; the point
        // is the context carries a live, snapshottable session.
        let snapshot = authority
            .snapshot(context.session_id())
            .expect("live session");
        assert_eq!(snapshot.identity.session_id, context.session_id());
    }
}
