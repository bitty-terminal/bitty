//! Secret-storage tiers with per-tier policy (OQ-055, Accepted: confirm).
//!
//! `OQ-055` is adopted (four tiers confirmed): host-consumed environment,
//! `0600` secrets file, OS keyring, and command references
//! (`pass`/1Password style), each with required consent plus names-only
//! audit on top of ADR 0006. This module records the tier descriptors as
//! pure, bounded, fail-closed types plus the tier mechanisms: keyring
//! backend selection ([`KeyringBackend`]), bounded command execution
//! ([`execute_command_ref`]), and rotation policy ([`RotationPolicy`]).
//!
//! The accepted host store ([`crate::secrets`], opaque `secret://`
//! handles, redacting `Display`/`Debug`, `[redacted]` marker) stays
//! authoritative; this module classifies *where* a secret lives, what
//! policy that tier requires, and how the two executable tiers resolve
//! without ever letting values cross a log boundary.
//!
//! # Non-goals
//!
//! No OS keyring dependency is introduced (`std` only): keyring reads
//! stay fail-closed until a backend crate is accepted. There is no
//! `unsafe` and no new dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of a tier label accepted by [`SecretTier::parse`].
pub const MAX_TIER_LABEL_BYTES: usize = 32;

/// Maximum bytes of a command-reference program or argument.
pub const MAX_COMMAND_REF_PART_BYTES: usize = 256;

/// Maximum arguments in one [`CommandRef`].
pub const MAX_COMMAND_REF_ARGS: usize = 16;

/// Maximum bytes of captured command output accepted as a secret value.
///
/// Mirrors [`crate::secrets::MAX_SECRET_VALUE_BYTES`] (`4096`) so a
/// command tier can never smuggle a larger value than the host store
/// admits.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 4096;

/// Maximum bytes of a keyring backend label.
pub const MAX_KEYRING_BACKEND_LABEL_BYTES: usize = 32;

/// Maximum bytes of a keyring service name.
pub const MAX_KEYRING_SERVICE_BYTES: usize = 128;

/// Maximum bytes of a keyring account name.
pub const MAX_KEYRING_ACCOUNT_BYTES: usize = 128;

/// Default rotation age: 90 days in seconds.
pub const DEFAULT_ROTATION_MAX_AGE_SECS: u64 = 90 * 24 * 60 * 60;

/// Maximum rotation age accepted: 365 days in seconds.
pub const MAX_ROTATION_MAX_AGE_SECS: u64 = 365 * 24 * 60 * 60;

// ── tiers ─────────────────────────────────────────────────────────────────

/// Candidate secret-storage tiers (OQ-055).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretTier {
    /// Host-consumed environment (already accepted via ADR-0006 allowlist).
    HostEnv,
    /// `$XDG_CONFIG_HOME/bitty/secrets.env`, mode `0600` (candidate).
    ConfigFile,
    /// OS keyring (candidate; backend undecided).
    OsKeyring,
    /// External command reference, `pass`/1Password style (candidate;
    /// naming only — never executed by this kernel).
    CommandRef,
}

impl SecretTier {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostEnv => "host-env",
            Self::ConfigFile => "config-file",
            Self::OsKeyring => "os-keyring",
            Self::CommandRef => "command-ref",
        }
    }

    /// Parse a tier label; unknown tiers fail closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_TIER_LABEL_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "secret_tier".to_string(),
                limit: MAX_TIER_LABEL_BYTES,
                actual: s.len(),
            });
        }
        match s {
            "host-env" => Ok(Self::HostEnv),
            "config-file" => Ok(Self::ConfigFile),
            "os-keyring" => Ok(Self::OsKeyring),
            "command-ref" => Ok(Self::CommandRef),
            _ => Err(PluginError::registry(format!(
                "unknown secret tier '{s}' (OQ-055 candidate; deny by default)"
            ))),
        }
    }

    /// Per-tier policy: consent, audit, and redaction requirements.
    ///
    /// Policy only tightens with tier sensitivity: every tier requires
    /// explicit consent, an audit entry, and redaction. Higher tiers add
    /// process-isolation (`CommandRef` output must never cross a log
    /// boundary still undecided by the ruling).
    #[must_use]
    pub const fn policy(self) -> TierPolicy {
        match self {
            Self::HostEnv => TierPolicy {
                consent: ConsentRule::AllowlistedRead,
                audit: true,
                redact_everywhere: true,
                isolate_subprocess_output: false,
            },
            Self::ConfigFile => TierPolicy {
                consent: ConsentRule::ExplicitGrant,
                audit: true,
                redact_everywhere: true,
                isolate_subprocess_output: false,
            },
            Self::OsKeyring => TierPolicy {
                consent: ConsentRule::ExplicitGrant,
                audit: true,
                redact_everywhere: true,
                isolate_subprocess_output: false,
            },
            Self::CommandRef => TierPolicy {
                consent: ConsentRule::ExplicitGrant,
                audit: true,
                redact_everywhere: true,
                isolate_subprocess_output: true,
            },
        }
    }
}

impl fmt::Display for SecretTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── per-tier policy ───────────────────────────────────────────────────────

/// How consent is obtained before a tier is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsentRule {
    /// Read only through the host allowlist (`bitty.env.get` precedent).
    AllowlistedRead,
    /// Read only with an explicit per-handle consent grant.
    ExplicitGrant,
}

impl fmt::Display for ConsentRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::AllowlistedRead => "allowlisted-read",
            Self::ExplicitGrant => "explicit-grant",
        };
        f.write_str(label)
    }
}

/// Candidate per-tier consent/audit/redaction policy (OQ-055).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TierPolicy {
    consent: ConsentRule,
    audit: bool,
    redact_everywhere: bool,
    isolate_subprocess_output: bool,
}

impl TierPolicy {
    /// Consent rule for this tier.
    #[must_use]
    pub const fn consent(self) -> ConsentRule {
        self.consent
    }

    /// Whether every read appends an audit entry.
    #[must_use]
    pub const fn audit(self) -> bool {
        self.audit
    }

    /// Whether values from this tier are redacted in every diagnostic.
    #[must_use]
    pub const fn redact_everywhere(self) -> bool {
        self.redact_everywhere
    }

    /// Whether subprocess output carrying this tier must stay isolated
    /// from logs (command-reference tiers only, mechanism undecided).
    #[must_use]
    pub const fn isolate_subprocess_output(self) -> bool {
        self.isolate_subprocess_output
    }
}

// ── command reference ───────────────────────────────────────────────────

/// Command reference naming a secret source (`pass`/1Password style).
///
/// Holds the path to a value, never the value itself. Resolve it with
/// [`execute_command_ref`]: no shell is ever invoked, output is bounded
/// to [`MAX_COMMAND_OUTPUT_BYTES`], and diagnostics quote the program
/// name only.
///
/// `Display`/`Debug` quote the program and argument *shapes* only in
/// that they never carry secret values — a reference holds no value at
/// all, only the path to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRef {
    program: String,
    args: Vec<String>,
}

impl CommandRef {
    /// Build a reference; empty programs, oversize parts, overlong
    /// argument lists, and NUL bytes fail closed.
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Result<Self, PluginError> {
        let program = program.into();
        validate_command_part("command_ref.program", &program)?;
        if args.len() > MAX_COMMAND_REF_ARGS {
            return Err(PluginError::LimitExceeded {
                field: "command_ref.args".to_string(),
                limit: MAX_COMMAND_REF_ARGS,
                actual: args.len(),
            });
        }
        for arg in &args {
            validate_command_part("command_ref.arg", arg)?;
        }
        Ok(Self { program, args })
    }

    /// Program name or path (executed only by [`execute_command_ref`],
    /// never through a shell).
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Arguments (naming only; carry no secret values).
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Validate one command-reference part (shared by program and args).
fn validate_command_part(field: &str, part: &str) -> Result<(), PluginError> {
    if part.is_empty() {
        return Err(PluginError::registry(format!("{field} must not be empty")));
    }
    if part.len() > MAX_COMMAND_REF_PART_BYTES {
        return Err(PluginError::LimitExceeded {
            field: field.to_string(),
            limit: MAX_COMMAND_REF_PART_BYTES,
            actual: part.len(),
        });
    }
    if part.bytes().any(|b| b == 0) {
        return Err(PluginError::registry(format!(
            "{field} must not contain NUL bytes"
        )));
    }
    Ok(())
}

/// Execute a [`CommandRef`] and return its secret value (OQ-055 command tier).
///
/// Fail-closed and log-safe: no shell is invoked
/// (`Command::new(program).args(args)` directly), stdin is null,
/// stderr is discarded, and only stdout — stripped of one trailing
/// newline — is returned. Diagnostics quote the program name only;
/// captured output never enters an error, log, or audit detail.
/// Empty output, NUL bytes, non-UTF-8 output, oversize output, spawn
/// failures, and non-zero exits all deny.
pub fn execute_command_ref(cmd: &CommandRef) -> Result<String, PluginError> {
    run_command_output(cmd.program(), cmd.args())
}

/// Shared bounded command-output runner (no shell, names-only errors).
fn run_command_output(program: &str, args: &[String]) -> Result<String, PluginError> {
    use std::process::{Command, Stdio};
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            PluginError::registry(format!("command-ref '{program}' failed to spawn: {err}"))
        })?;
    if !output.status.success() {
        return Err(PluginError::registry(format!(
            "command-ref '{program}' exited with {status}",
            status = output.status
        )));
    }
    let stdout = output.stdout;
    if stdout.len() > MAX_COMMAND_OUTPUT_BYTES {
        return Err(PluginError::LimitExceeded {
            field: "command_ref.output".to_string(),
            limit: MAX_COMMAND_OUTPUT_BYTES,
            actual: stdout.len(),
        });
    }
    if stdout.contains(&0) {
        return Err(PluginError::registry(format!(
            "command-ref '{program}' output must not contain NUL bytes"
        )));
    }
    let mut text = String::from_utf8(stdout).map_err(|_| {
        PluginError::registry(format!("command-ref '{program}' output is not UTF-8"))
    })?;
    // Strip one trailing newline (tolerate CRLF); interior whitespace is
    // preserved so secret bytes are never altered beyond the line ending
    // that helpers like `pass`/`op` append.
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    if text.is_empty() {
        return Err(PluginError::registry(format!(
            "command-ref '{program}' produced empty output"
        )));
    }
    Ok(text)
}

// ── keyring backend selection ────────────────────────────────────────────

/// OS keyring backend choice (OQ-055 keyring tier).
///
/// `std`-only selection: this enum fixes *which* backend a future
/// keyring read would use. Reads stay fail-closed (see
/// [`read_keyring_ref`]) until an OS keyring dependency is accepted —
/// selection without ambient reads is the mechanism this slice ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyringBackend {
    /// Platform default (`SecretService` on Linux, `Keychain` on macOS,
    /// Windows credential vault on Windows).
    Auto,
    /// Linux Secret Service / `libsecret` shape.
    SecretService,
    /// macOS Keychain shape.
    Keychain,
    /// Windows credential-vault shape.
    WindowsCredential,
    /// Keyring tier disabled: every read denies fail-closed.
    Disabled,
}

impl KeyringBackend {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SecretService => "secret-service",
            Self::Keychain => "keychain",
            Self::WindowsCredential => "windows-credential",
            Self::Disabled => "disabled",
        }
    }

    /// Parse a backend label; unknown labels fail closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_KEYRING_BACKEND_LABEL_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "keyring_backend".to_string(),
                limit: MAX_KEYRING_BACKEND_LABEL_BYTES,
                actual: s.len(),
            });
        }
        match s {
            "auto" => Ok(Self::Auto),
            "secret-service" => Ok(Self::SecretService),
            "keychain" => Ok(Self::Keychain),
            "windows-credential" => Ok(Self::WindowsCredential),
            "disabled" => Ok(Self::Disabled),
            _ => Err(PluginError::registry(format!(
                "unknown keyring backend '{s}' (deny by default)"
            ))),
        }
    }

    /// Platform default for an injected `std::env::consts::OS` value.
    ///
    /// Pure and testable: `"linux"` maps to Secret Service,
    /// `"macos"` to Keychain, `"windows"` to the credential vault, and
    /// anything else to `Disabled` (deny by default).
    #[must_use]
    pub const fn default_for_os(os: &str) -> Self {
        // `str` equality in `const fn` is byte comparison; match on the
        // short OS labels `std::env::consts::OS` produces.
        if os.len() == 5 && matches!(os.as_bytes(), b"linux") {
            Self::SecretService
        } else if os.len() == 5 && matches!(os.as_bytes(), b"macos") {
            Self::Keychain
        } else if os.len() == 7 && matches!(os.as_bytes(), b"windows") {
            Self::WindowsCredential
        } else {
            Self::Disabled
        }
    }

    /// Whether this backend permits a read attempt.
    #[must_use]
    pub const fn is_available(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

impl fmt::Display for KeyringBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Select the keyring backend from an injected OS label plus an optional
/// `BITTY_KEYRING_BACKEND` override.
///
/// Pure and fail-closed: an explicit override that parses wins; an
/// override that fails to parse resolves to `Disabled` (never to a
/// permissive default); otherwise the platform default applies and
/// `Auto` resolves to that default.
pub fn select_keyring_backend_with(os: &str, backend_override: Option<&str>) -> KeyringBackend {
    if let Some(label) = backend_override {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            return KeyringBackend::default_for_os(os);
        }
        match KeyringBackend::parse(trimmed) {
            Ok(KeyringBackend::Auto) => KeyringBackend::default_for_os(os),
            Ok(backend) => backend,
            Err(_) => KeyringBackend::Disabled,
        }
    } else {
        KeyringBackend::default_for_os(os)
    }
}

/// Live backend selection (`std::env::consts::OS` plus the live
/// `BITTY_KEYRING_BACKEND` override).
///
/// Thin wrapper so unit tests stay hermetic via
/// [`select_keyring_backend_with`].
#[must_use]
pub fn select_keyring_backend() -> KeyringBackend {
    let backend_override = std::env::var("BITTY_KEYRING_BACKEND").ok();
    select_keyring_backend_with(std::env::consts::OS, backend_override.as_deref())
}

/// Keyring entry reference: service plus account names only.
///
/// Names a keyring entry without carrying or fetching a value, mirroring
/// [`CommandRef`] discipline: diagnostics quote names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringRef {
    service: String,
    account: String,
}

impl KeyringRef {
    /// Build a reference; empty, oversize, or NUL/control-bearing names
    /// fail closed.
    pub fn new(
        service: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, PluginError> {
        let service = service.into();
        let account = account.into();
        validate_keyring_part("keyring.service", &service, MAX_KEYRING_SERVICE_BYTES)?;
        validate_keyring_part("keyring.account", &account, MAX_KEYRING_ACCOUNT_BYTES)?;
        Ok(Self { service, account })
    }

    /// Service name (never a secret value).
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Account name (never a secret value).
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }
}

/// Validate one keyring name part.
fn validate_keyring_part(field: &str, part: &str, limit: usize) -> Result<(), PluginError> {
    if part.is_empty() {
        return Err(PluginError::registry(format!("{field} must not be empty")));
    }
    if part.len() > limit {
        return Err(PluginError::LimitExceeded {
            field: field.to_string(),
            limit,
            actual: part.len(),
        });
    }
    if part.bytes().any(|b| b == 0) {
        return Err(PluginError::registry(format!(
            "{field} must not contain NUL bytes"
        )));
    }
    if part.chars().any(|c| c.is_control()) {
        return Err(PluginError::registry(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

/// Attempt a keyring read (fail-closed until a backend crate is accepted).
///
/// Selection ([`select_keyring_backend`]) decides *which* backend would
/// serve the read; this function enforces the consequence: `Disabled`
/// denies, and every other backend denies with a names-only
/// not-wired error. Values never enter this function, so there is
/// nothing to redact. The error quotes backend, service, and account
/// names only.
pub fn read_keyring_ref(
    backend: KeyringBackend,
    entry: &KeyringRef,
) -> Result<String, PluginError> {
    if !backend.is_available() {
        return Err(PluginError::grant(format!(
            "keyring backend '{backend}' is disabled (deny by default)"
        )));
    }
    Err(PluginError::registry(format!(
        "keyring backend '{backend}' is selected but OS keyring reads are not wired (service '{service}', account '{account}'; deny by default)",
        service = entry.service(),
        account = entry.account(),
    )))
}

// ── rotation ─────────────────────────────────────────────────────────────

/// Rotation policy for file/keyring/command tiers (OQ-055).
///
/// Pure due-check only: [`RotationPolicy::is_due`] compares injected
/// millisecond timestamps with saturating arithmetic, so clock skew
/// (`now_ms < last_rotated_ms`) never forces rotation. Actual rotation
/// re-provisions through the accepted store path
/// ([`crate::secrets::SecretStore::insert`] plus
/// [`crate::secrets::FileSecretStore::save`]); this type only decides
/// *when* that path is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RotationPolicy {
    max_age_secs: u64,
}

impl RotationPolicy {
    /// Build a policy; zero ages and ages over one year fail closed.
    pub fn new(max_age_secs: u64) -> Result<Self, PluginError> {
        if max_age_secs == 0 {
            return Err(PluginError::registry(
                "rotation max_age_secs must not be zero".to_string(),
            ));
        }
        if max_age_secs > MAX_ROTATION_MAX_AGE_SECS {
            return Err(PluginError::LimitExceeded {
                field: "rotation.max_age_secs".to_string(),
                limit: MAX_ROTATION_MAX_AGE_SECS as usize,
                actual: max_age_secs as usize,
            });
        }
        Ok(Self { max_age_secs })
    }

    /// Default policy (90 days).
    #[must_use]
    pub const fn default_policy() -> Self {
        Self {
            max_age_secs: DEFAULT_ROTATION_MAX_AGE_SECS,
        }
    }

    /// Maximum age in seconds.
    #[must_use]
    pub const fn max_age_secs(self) -> u64 {
        self.max_age_secs
    }

    /// Whether a secret last rotated at `last_rotated_ms` is due at
    /// `now_ms` (saturating, fail-open toward *not* due on skew).
    #[must_use]
    pub const fn is_due(self, last_rotated_ms: u64, now_ms: u64) -> bool {
        if now_ms < last_rotated_ms {
            return false;
        }
        now_ms.saturating_sub(last_rotated_ms) >= self.max_age_secs.saturating_mul(1000)
    }
}

/// Rotate one secret value through the accepted store path.
///
/// Fails closed when `name` is unknown (never creates on rotate);
/// otherwise validates the replacement through
/// [`crate::secrets::SecretStore::insert`]. Consent and audit stay with
/// the store's resolve path; rotation only re-provisions.
pub fn rotate_secret_value(
    store: &mut crate::secrets::SecretStore,
    name: &str,
    new_value: &str,
) -> Result<(), crate::secrets::SecretError> {
    use crate::secrets::SecretError;
    if !store.contains(name) {
        return Err(SecretError::missing_handle(name));
    }
    store.insert(name, new_value)
}

// ── call-boundary gate (CTX-0330) ──────────────────────────────────────────

/// Tier access context for the call-boundary gate (CTX-0330).
///
/// Bundles the tier being read with whether the caller holds an explicit
/// grant for it, so the resolve boundary takes one argument instead of a
/// bare boolean that is easy to misread at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TierAccess {
    /// Tier holding the secret.
    pub tier: SecretTier,
    /// Whether the caller holds an explicit grant for this tier.
    pub consent: bool,
}

impl TierAccess {
    /// Check this access against the tier policy (fail-closed).
    pub fn check(self) -> Result<(), PluginError> {
        check_tier_access(self.tier, self.consent)
    }
}

/// Check tier access at the secret call boundary (CTX-0330).
///
/// Pure, fail-closed, names-only: `tier` names *where* a secret would live
/// and `tier_consent` records whether the caller holds an explicit grant
/// for that tier. Tiers whose policy is [`ConsentRule::AllowlistedRead`]
/// (`HostEnv`) pass here — the per-key allowlist is enforced separately at
/// the `bitty.env` boundary — while [`ConsentRule::ExplicitGrant`] tiers
/// fail closed with a grant error until consent is shown. Values never
/// enter this function, so there is nothing to redact.
///
/// This is the consent half of the per-tier policy; the audit half runs at
/// the resolve boundary (`PluginHost::resolve_secret_with_tier`), which
/// records the allow/deny outcome in the secret audit ledger.
pub fn check_tier_access(tier: SecretTier, tier_consent: bool) -> Result<(), PluginError> {
    match tier.policy().consent() {
        ConsentRule::AllowlistedRead => Ok(()),
        ConsentRule::ExplicitGrant => {
            if tier_consent {
                Ok(())
            } else {
                Err(PluginError::grant(format!(
                    "secret tier '{}' requires explicit consent (deny by default)",
                    tier.as_str()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_round_trip() {
        for tier in [
            SecretTier::HostEnv,
            SecretTier::ConfigFile,
            SecretTier::OsKeyring,
            SecretTier::CommandRef,
        ] {
            assert_eq!(SecretTier::parse(tier.as_str()), Ok(tier));
            assert_eq!(tier.to_string(), tier.as_str());
        }
    }

    #[test]
    fn unknown_tier_denies() {
        assert!(SecretTier::parse("vault").is_err());
        assert!(SecretTier::parse("").is_err());
        assert!(SecretTier::parse("HOST-ENV").is_err());
    }

    #[test]
    fn every_tier_audits_and_redacts() {
        for tier in [
            SecretTier::HostEnv,
            SecretTier::ConfigFile,
            SecretTier::OsKeyring,
            SecretTier::CommandRef,
        ] {
            let policy = tier.policy();
            assert!(policy.audit(), "{tier} must audit");
            assert!(policy.redact_everywhere(), "{tier} must redact");
        }
    }

    #[test]
    fn only_command_ref_isolates_output() {
        assert!(!SecretTier::HostEnv.policy().isolate_subprocess_output());
        assert!(!SecretTier::ConfigFile.policy().isolate_subprocess_output());
        assert!(!SecretTier::OsKeyring.policy().isolate_subprocess_output());
        assert!(SecretTier::CommandRef.policy().isolate_subprocess_output());
    }

    #[test]
    fn host_env_uses_allowlist_but_config_file_needs_grant() {
        assert_eq!(
            SecretTier::HostEnv.policy().consent(),
            ConsentRule::AllowlistedRead
        );
        assert_eq!(
            SecretTier::ConfigFile.policy().consent(),
            ConsentRule::ExplicitGrant
        );
    }

    #[test]
    fn command_ref_naming_validates() {
        let reference = CommandRef::new("pass", vec!["show".to_string(), "bitty/api".to_string()])
            .expect("valid reference");
        assert_eq!(reference.program(), "pass");
        assert_eq!(reference.args().len(), 2);
    }

    #[test]
    fn command_ref_malformed_denies() {
        assert!(CommandRef::new("", Vec::new()).is_err());
        assert!(CommandRef::new("pass", vec!["ok".to_string(), "has\0nul".to_string()]).is_err());
        let many = vec!["a".to_string(); MAX_COMMAND_REF_ARGS + 1];
        assert!(CommandRef::new("pass", many).is_err());
        let long = "x".repeat(MAX_COMMAND_REF_PART_BYTES + 1);
        assert!(CommandRef::new(long, Vec::new()).is_err());
    }

    #[test]
    fn consent_labels_stable() {
        assert_eq!(ConsentRule::AllowlistedRead.to_string(), "allowlisted-read");
        assert_eq!(ConsentRule::ExplicitGrant.to_string(), "explicit-grant");
    }

    #[test]
    fn tier_gate_allows_host_env_without_consent() {
        assert!(check_tier_access(SecretTier::HostEnv, false).is_ok());
        assert!(check_tier_access(SecretTier::HostEnv, true).is_ok());
    }

    #[test]
    fn tier_gate_denies_grant_tiers_without_consent() {
        for tier in [
            SecretTier::ConfigFile,
            SecretTier::OsKeyring,
            SecretTier::CommandRef,
        ] {
            let error =
                check_tier_access(tier, false).expect_err("grant tier without consent must deny");
            assert!(
                matches!(error, PluginError::Grant { .. }),
                "tier {tier} must fail with a grant error"
            );
            assert!(check_tier_access(tier, true).is_ok());
        }
    }

    #[test]
    fn keyring_backend_labels_stable() {
        for (backend, label) in [
            (KeyringBackend::Auto, "auto"),
            (KeyringBackend::SecretService, "secret-service"),
            (KeyringBackend::Keychain, "keychain"),
            (KeyringBackend::WindowsCredential, "windows-credential"),
            (KeyringBackend::Disabled, "disabled"),
        ] {
            assert_eq!(KeyringBackend::parse(label), Ok(backend));
            assert_eq!(backend.to_string(), label);
        }
        assert!(KeyringBackend::parse("vault").is_err());
        assert!(KeyringBackend::parse("").is_err());
    }

    #[test]
    fn keyring_backend_defaults_per_os() {
        assert_eq!(
            KeyringBackend::default_for_os("linux"),
            KeyringBackend::SecretService
        );
        assert_eq!(
            KeyringBackend::default_for_os("macos"),
            KeyringBackend::Keychain
        );
        assert_eq!(
            KeyringBackend::default_for_os("windows"),
            KeyringBackend::WindowsCredential
        );
        assert_eq!(
            KeyringBackend::default_for_os("plan9"),
            KeyringBackend::Disabled
        );
    }

    #[test]
    fn keyring_backend_selection_is_fail_closed() {
        assert_eq!(
            select_keyring_backend_with("linux", None),
            KeyringBackend::SecretService
        );
        assert_eq!(
            select_keyring_backend_with("linux", Some("keychain")),
            KeyringBackend::Keychain
        );
        assert_eq!(
            select_keyring_backend_with("linux", Some("auto")),
            KeyringBackend::SecretService
        );
        // Unparsable override never falls back to a permissive backend.
        assert_eq!(
            select_keyring_backend_with("linux", Some("vault")),
            KeyringBackend::Disabled
        );
        assert_eq!(
            select_keyring_backend_with("macos", Some("")),
            KeyringBackend::Keychain
        );
    }

    #[test]
    fn keyring_ref_validates() {
        let entry = KeyringRef::new("bitty", "api-key").expect("valid ref");
        assert_eq!(entry.service(), "bitty");
        assert_eq!(entry.account(), "api-key");
        assert!(KeyringRef::new("", "a").is_err());
        assert!(KeyringRef::new("s", "").is_err());
        assert!(KeyringRef::new("s", "has\nnewline").is_err());
    }

    #[test]
    fn keyring_read_denies_fail_closed_without_values() {
        let entry = KeyringRef::new("bitty", "api-key").expect("valid ref");
        let disabled = read_keyring_ref(KeyringBackend::Disabled, &entry)
            .expect_err("disabled backend must deny");
        assert!(matches!(disabled, PluginError::Grant { .. }));
        // Wired backends deny too until a backend crate lands; the
        // names (not values) stay visible for audit.
        let pending = read_keyring_ref(KeyringBackend::SecretService, &entry)
            .expect_err("unwired backend must deny");
        let text = pending.to_string();
        assert!(text.contains("bitty"), "{text}");
        assert!(text.contains("api-key"), "{text}");
    }

    #[test]
    fn command_ref_executes_without_shell() {
        let echo = CommandRef::new("echo", vec!["hello".to_string()]).expect("valid ref");
        assert_eq!(execute_command_ref(&echo), Ok("hello".to_string()));
        let missing =
            CommandRef::new("bitty-definitely-missing-xyz", Vec::new()).expect("valid ref");
        let error = execute_command_ref(&missing).expect_err("missing program must deny");
        let text = error.to_string();
        assert!(text.contains("bitty-definitely-missing-xyz"), "{text}");
        let failing = CommandRef::new("false", Vec::new()).expect("valid ref");
        assert!(execute_command_ref(&failing).is_err());
    }

    #[test]
    fn command_ref_empty_output_denies() {
        let empty = CommandRef::new("true", Vec::new()).expect("valid ref");
        assert!(execute_command_ref(&empty).is_err());
    }

    #[test]
    fn rotation_policy_validates_and_checks_due() {
        assert!(RotationPolicy::new(0).is_err());
        assert!(RotationPolicy::new(MAX_ROTATION_MAX_AGE_SECS + 1).is_err());
        let policy = RotationPolicy::new(3_600).expect("valid policy");
        assert!(!policy.is_due(1_000, 1_000));
        assert!(!policy.is_due(1_000, 1_000 + 3_599_999));
        assert!(policy.is_due(1_000, 1_000 + 3_600_000));
        // Clock skew never forces rotation.
        assert!(!policy.is_due(2_000, 1_000));
        assert_eq!(
            RotationPolicy::default_policy().max_age_secs(),
            DEFAULT_ROTATION_MAX_AGE_SECS
        );
    }

    #[test]
    fn rotate_secret_value_reprovisions_without_creating() {
        use crate::secrets::SecretStore;
        let mut store = SecretStore::new();
        assert!(rotate_secret_value(&mut store, "api", "v2").is_err());
        store.insert("api", "v1").expect("seed");
        assert!(rotate_secret_value(&mut store, "api", "v2").is_ok());
        assert!(rotate_secret_value(&mut store, "api", "").is_ok());
    }
}
