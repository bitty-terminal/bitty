//! Candidate secret-storage tiers with per-tier policy (OQ-055).
//!
//! `OQ-055` is still open: no ruling fixes which storage tiers are in
//! scope beyond host-consumed environment plus `secrets.conf`, nor how
//! consent, audit, and redaction apply per tier. This module records the
//! candidate direction only — host environment, `0600` config file, OS
//! keyring, command references (`pass`/1Password style) — as pure,
//! bounded, fail-closed descriptors.
//!
//! Nothing here stores, loads, or resolves a secret: there is no file
//! I/O, no keyring call, and no subprocess. [`CommandRef`] names a
//! command without ever running it. The accepted host store
//! ([`crate::secrets`], opaque `secret://` handles, redacting
//! `Display`/`Debug`, `[redacted]` marker) stays authoritative; this
//! kernel only classifies *where* a secret would live and what policy
//! that tier would require, so a future ruling has a tested shape to
//! accept or replace.
//!
//! # Non-goals
//!
//! Tier admission, keyring backend choice, command execution and output
//! handling, and rotation stay undecided until the OQ-055 ruling.
//! There is no `unsafe` and no new dependency (`std` only).

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

// ── command reference (naming only) ───────────────────────────────────────

/// Candidate command reference naming a secret source (`pass`/1Password
/// style). Naming only: this kernel never spawns a process, and no
/// resolution method exists until the OQ-055 ruling fixes execution,
/// output handling, and audit shape.
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

    /// Program name or path (never executed by this kernel).
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
}
