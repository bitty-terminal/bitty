//! `api_key_env` vs `api_key_cmd` credential references (OQ-054).
//!
//! `OQ-054` is adopted (MPC-1..MPC-4): `api_key_env` versus
//! `api_key_cmd` credential references, their exclusive-or resolution
//! order, and the project-level override boundary that cannot widen
//! credentials. This module carries the adopted reference mechanism as
//! pure, bounded, fail-closed types, enforces the exclusive-or order at the
//! call boundary ([`resolve_choice`]) plus the narrow-only project
//! boundary ([`check_project_override`]).
//!
//! The provider-schema surface built on these references
//! ([`crate::provider_credential::ProviderCredentialConfig`] with actual
//! resolution) moved to [`crate::provider_credential`] in CTX-0916 S5; this
//! module keeps a deprecated re-export shim for one release (removed in
//! S7). New callers import from there (or the crate root), never from here.
//!
//! A [`CredentialRef`] names *where* a credential comes from without
//! carrying a value, and [`resolve_precedence`] decides *which*
//! reference wins without reading anything. Resolution reads through an
//! injected environment lookup and one bounded, shell-free command
//! runner; diagnostics quote variable and program names only — values
//! never enter an error, log, or audit detail. Composes with ADR-0006
//! and MP-10 per the OQ-054 state.
//!
//! # Non-goals
//!
//! Provider adapters stay in `bitty-ai`. There is no `unsafe`, no I/O
//! beyond the single bounded credential-command spawn, and no new
//! dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of an environment variable name.
pub const MAX_CREDENTIAL_ENV_NAME_BYTES: usize = 128;

/// Maximum bytes of a command program or argument.
pub const MAX_CREDENTIAL_CMD_PART_BYTES: usize = 256;

/// Maximum arguments in one [`CredentialRef::Cmd`].
pub const MAX_CREDENTIAL_CMD_ARGS: usize = 16;

// ── deprecated S5 shim (CTX-0916, removed in S7) ────────────────────────────
//
// The provider-schema surface moved to [`crate::provider_credential`] in S5.
// These re-exports keep the previous import path working for one release;
// new callers import from there (or the crate root) instead.

/// Deprecated shim: import [`crate::provider_credential::ProviderCredentialConfig`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::ProviderCredentialConfig`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::ProviderCredentialConfig;

/// Deprecated shim: import [`crate::provider_credential::check_provider_override`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::check_provider_override`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::check_provider_override;

/// Deprecated shim: import [`crate::provider_credential::resolve_provider_credential`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::resolve_provider_credential`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::resolve_provider_credential;

/// Deprecated shim: import [`crate::provider_credential::resolve_provider_credential_live`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::resolve_provider_credential_live`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::resolve_provider_credential_live;

/// Deprecated shim: import [`crate::provider_credential::execute_credential_cmd`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::execute_credential_cmd`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::execute_credential_cmd;

/// Deprecated shim: import [`crate::provider_credential::MAX_CREDENTIAL_CMD_OUTPUT_BYTES`] instead.
///
/// Removed in CTX-0916 S7.
#[deprecated(
    note = "moved to `crate::provider_credential::MAX_CREDENTIAL_CMD_OUTPUT_BYTES`; the shim is removed in CTX-0916 S7"
)]
pub use crate::provider_credential::MAX_CREDENTIAL_CMD_OUTPUT_BYTES;

// ── credential reference ──────────────────────────────────────────────────

/// Credential reference: `api_key_env` vs `api_key_cmd` (OQ-054 adopted).
///
/// Names a source only — never a value. `Env` names one environment
/// variable; `Cmd` names a program plus arguments without executing
/// them. Both forms validate fail-closed at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRef {
    /// `api_key_env`: resolve from the named environment variable.
    Env {
        /// Variable name (`[A-Z0-9_]` with a leading letter or `_`).
        var: String,
    },
    /// `api_key_cmd`: resolve from the named command's output.
    Cmd {
        /// Program name or path (never executed by this kernel).
        program: String,
        /// Arguments (naming only).
        args: Vec<String>,
    },
}

impl CredentialRef {
    /// Build an `api_key_env` reference; invalid names fail closed.
    pub fn from_env(var: impl Into<String>) -> Result<Self, PluginError> {
        let var = var.into();
        validate_env_name(&var)?;
        Ok(Self::Env { var })
    }

    /// Build an `api_key_cmd` reference; malformed parts fail closed.
    pub fn from_cmd(program: impl Into<String>, args: Vec<String>) -> Result<Self, PluginError> {
        let program = program.into();
        validate_cmd_part("credential_cmd.program", &program)?;
        if args.len() > MAX_CREDENTIAL_CMD_ARGS {
            return Err(PluginError::LimitExceeded {
                field: "credential_cmd.args".to_string(),
                limit: MAX_CREDENTIAL_CMD_ARGS,
                actual: args.len(),
            });
        }
        for arg in &args {
            validate_cmd_part("credential_cmd.arg", arg)?;
        }
        Ok(Self::Cmd { program, args })
    }

    /// Which source kind this reference names.
    #[must_use]
    pub fn source_kind(&self) -> CredentialSource {
        match self {
            Self::Env { .. } => CredentialSource::Env,
            Self::Cmd { .. } => CredentialSource::Cmd,
        }
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Names only — no value exists in this module to leak.
            Self::Env { var } => write!(f, "api_key_env:{var}"),
            Self::Cmd { program, args } => {
                write!(f, "api_key_cmd:{program}")?;
                for arg in args {
                    write!(f, " {arg}")?;
                }
                Ok(())
            }
        }
    }
}

/// Which credential source a reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialSource {
    Env,
    Cmd,
}

impl fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Env => "api_key_env",
            Self::Cmd => "api_key_cmd",
        };
        f.write_str(label)
    }
}

/// Validate an environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn validate_env_name(var: &str) -> Result<(), PluginError> {
    if var.is_empty() {
        return Err(PluginError::registry(
            "api_key_env name must not be empty".to_string(),
        ));
    }
    if var.len() > MAX_CREDENTIAL_ENV_NAME_BYTES {
        return Err(PluginError::LimitExceeded {
            field: "api_key_env".to_string(),
            limit: MAX_CREDENTIAL_ENV_NAME_BYTES,
            actual: var.len(),
        });
    }
    let mut bytes = var.bytes();
    let first = bytes.next().unwrap_or(b'_');
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(PluginError::registry(format!(
            "api_key_env name '{var}' must start with a letter or '_'"
        )));
    }
    if !var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(PluginError::registry(format!(
            "api_key_env name '{var}' uses characters outside [A-Za-z0-9_]"
        )));
    }
    Ok(())
}

/// Validate one command part (program or argument): non-empty, bounded,
/// no NUL, no shell metacharacters (this kernel never invokes a shell).
fn validate_cmd_part(field: &str, part: &str) -> Result<(), PluginError> {
    if part.is_empty() {
        return Err(PluginError::registry(format!("{field} must not be empty")));
    }
    if part.len() > MAX_CREDENTIAL_CMD_PART_BYTES {
        return Err(PluginError::LimitExceeded {
            field: field.to_string(),
            limit: MAX_CREDENTIAL_CMD_PART_BYTES,
            actual: part.len(),
        });
    }
    if part.bytes().any(|b| {
        b == 0 || b == b'\n' || b == b';' || b == b'|' || b == b'&' || b == b'$' || b == b'`'
    }) {
        return Err(PluginError::registry(format!(
            "{field} must not contain shell metacharacters"
        )));
    }
    Ok(())
}

// ── resolution order (pure, reads nothing) ────────────────────────────────

/// Candidate resolution outcome: which reference wins, if any.
///
/// Presence flags only — the caller reports whether each key is *set*;
/// this function never reads the environment or runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialPrecedence {
    /// Neither key is set: fail closed, no credential.
    Unset,
    /// Only `api_key_env` is set.
    Env,
    /// Only `api_key_cmd` is set.
    Cmd,
    /// Both keys are set: ambiguous, fail closed (deny, surface conflict).
    Conflict,
}

/// Resolution order (OQ-054 adopted): at most one of the two keys may
/// be set; both set is a conflict that denies rather than prefers.
#[must_use]
pub const fn resolve_precedence(env_set: bool, cmd_set: bool) -> CredentialPrecedence {
    match (env_set, cmd_set) {
        (false, false) => CredentialPrecedence::Unset,
        (true, false) => CredentialPrecedence::Env,
        (false, true) => CredentialPrecedence::Cmd,
        (true, true) => CredentialPrecedence::Conflict,
    }
}

// ── call-boundary choice (pure, reads nothing) ────────────────────────────

/// Call-boundary choice (OQ-054 adopted): enforce the exclusive-or order
/// where two optional references meet.
///
/// At most one reference may be present: both present is a conflict that
/// denies fail-closed with a grant error naming both references (names only —
/// references carry no values, so there is nothing to redact), and neither
/// present resolves to no credential. No environment is read and no command
/// runs here; the winner's resolution is
/// [`crate::provider_credential::resolve_provider_credential`]
/// on the provider-schema surface
/// ([`crate::provider_credential::ProviderCredentialConfig`]).
pub fn resolve_choice(
    env: Option<&CredentialRef>,
    cmd: Option<&CredentialRef>,
) -> Result<Option<CredentialSource>, PluginError> {
    match (env, cmd) {
        (None, None) => Ok(None),
        (Some(_), None) => Ok(Some(CredentialSource::Env)),
        (None, Some(_)) => Ok(Some(CredentialSource::Cmd)),
        (Some(env_ref), Some(cmd_ref)) => Err(PluginError::grant(format!(
            "credential conflict: '{env_ref}' and '{cmd_ref}' are both set; \
             set at most one (OQ-054 adopted)"
        ))),
    }
}

// ── project override boundary (pure, never widens) ────────────────────────

/// Project-override boundary (OQ-054 adopted): a project layer may
/// only narrow credentials — keep the identical reference or remove it
/// — never widen (change source, rename the variable/program, or add a
/// reference where the base has none).
///
/// Returns `Ok` for narrow-or-equal overlays and a typed error for any
/// widening. `None` on either side means "no reference at that layer".
pub fn check_project_override(
    base: Option<&CredentialRef>,
    overlay: Option<&CredentialRef>,
) -> Result<(), PluginError> {
    match (base, overlay) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(PluginError::grant(
            "project layer must not add a credential reference where the base has none \
             (OQ-054 adopted: project overrides narrow only)"
                .to_string(),
        )),
        (_, None) => Ok(()),
        (Some(base_ref), Some(overlay_ref)) => {
            if base_ref == overlay_ref {
                Ok(())
            } else {
                Err(PluginError::grant(format!(
                    "project layer must not widen credentials: base '{base_ref}' \
                     vs overlay '{overlay_ref}' (OQ-054 adopted)"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_validate() {
        assert!(CredentialRef::from_env("OPENAI_API_KEY").is_ok());
        assert!(CredentialRef::from_env("_PRIVATE").is_ok());
        assert!(CredentialRef::from_env("").is_err());
        assert!(CredentialRef::from_env("9LIVES").is_err());
        assert!(CredentialRef::from_env("has-dash").is_err());
        assert!(CredentialRef::from_env("has space").is_err());
        assert!(CredentialRef::from_env("lower.dot").is_err());
    }

    #[test]
    fn cmd_parts_validate() {
        assert!(
            CredentialRef::from_cmd("pass", vec!["show".to_string(), "api".to_string()]).is_ok()
        );
        assert!(CredentialRef::from_cmd("", Vec::new()).is_err());
        assert!(CredentialRef::from_cmd("op", vec!["read;evil".to_string()]).is_err());
        assert!(CredentialRef::from_cmd("op", vec!["$(evil)".to_string()]).is_err());
        assert!(CredentialRef::from_cmd("op", vec!["a|b".to_string()]).is_err());
        let many = vec!["a".to_string(); MAX_CREDENTIAL_CMD_ARGS + 1];
        assert!(CredentialRef::from_cmd("op", many).is_err());
    }

    #[test]
    fn display_names_sources_only() {
        let env = CredentialRef::from_env("MY_KEY").expect("valid env");
        assert_eq!(env.to_string(), "api_key_env:MY_KEY");
        assert_eq!(env.source_kind(), CredentialSource::Env);
        let cmd = CredentialRef::from_cmd("pass", vec!["show".to_string()]).expect("valid cmd");
        assert_eq!(cmd.to_string(), "api_key_cmd:pass show");
        assert_eq!(cmd.source_kind(), CredentialSource::Cmd);
    }

    #[test]
    fn precedence_is_exclusive() {
        assert_eq!(
            resolve_precedence(false, false),
            CredentialPrecedence::Unset
        );
        assert_eq!(resolve_precedence(true, false), CredentialPrecedence::Env);
        assert_eq!(resolve_precedence(false, true), CredentialPrecedence::Cmd);
        assert_eq!(
            resolve_precedence(true, true),
            CredentialPrecedence::Conflict
        );
    }

    #[test]
    fn project_overlay_narrows_only() {
        let base = CredentialRef::from_env("BASE_KEY").expect("valid env");
        let same = CredentialRef::from_env("BASE_KEY").expect("valid env");
        let wider = CredentialRef::from_env("OTHER_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("pass", vec!["x".to_string()]).expect("valid cmd");

        assert!(check_project_override(None, None).is_ok());
        assert!(check_project_override(Some(&base), None).is_ok());
        assert!(check_project_override(Some(&base), Some(&same)).is_ok());
        // Adding where the base has none widens: deny.
        assert!(check_project_override(None, Some(&base)).is_err());
        // Renaming widens: deny.
        assert!(check_project_override(Some(&base), Some(&wider)).is_err());
        // Switching source widens: deny.
        assert!(check_project_override(Some(&base), Some(&cmd)).is_err());
    }

    #[test]
    fn source_labels_stable() {
        assert_eq!(CredentialSource::Env.to_string(), "api_key_env");
        assert_eq!(CredentialSource::Cmd.to_string(), "api_key_cmd");
    }

    #[test]
    fn choice_enforces_exclusive_or() {
        let env = CredentialRef::from_env("MY_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("pass", vec!["show".to_string()]).expect("valid cmd");
        assert_eq!(resolve_choice(None, None), Ok(None));
        assert_eq!(
            resolve_choice(Some(&env), None),
            Ok(Some(CredentialSource::Env))
        );
        assert_eq!(
            resolve_choice(None, Some(&cmd)),
            Ok(Some(CredentialSource::Cmd))
        );
        // Both present denies, naming both references (names only).
        let error = resolve_choice(Some(&env), Some(&cmd)).expect_err("both set must deny");
        let text = error.to_string();
        assert!(text.contains("api_key_env:MY_KEY"), "{text}");
        assert!(text.contains("api_key_cmd:pass show"), "{text}");
    }

    #[test]
    fn choice_agrees_with_precedence() {
        let env = CredentialRef::from_env("MY_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("pass", vec!["show".to_string()]).expect("valid cmd");
        for (env_set, cmd_set, precedence) in [
            (false, false, CredentialPrecedence::Unset),
            (true, false, CredentialPrecedence::Env),
            (false, true, CredentialPrecedence::Cmd),
            (true, true, CredentialPrecedence::Conflict),
        ] {
            assert_eq!(resolve_precedence(env_set, cmd_set), precedence);
            let env_ref = if env_set { Some(&env) } else { None };
            let cmd_ref = if cmd_set { Some(&cmd) } else { None };
            match resolve_choice(env_ref, cmd_ref) {
                Ok(choice) => assert!(
                    precedence != CredentialPrecedence::Conflict,
                    "conflict must deny, got {choice:?}"
                ),
                Err(_) => assert_eq!(precedence, CredentialPrecedence::Conflict),
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_shim_matches_canonical_path() {
        // CTX-0916 S5 (DEC-0100): the deprecated shim names exactly the
        // canonical moved type, so old-path construction, display, and
        // resolution behave identically until S7 removes the shim.
        use crate::provider_credential::{
            ProviderCredentialConfig as Canonical, resolve_provider_credential as canonical_resolve,
        };
        use std::collections::BTreeMap;

        let env = CredentialRef::from_env("SHIM_KEY").expect("valid env");
        let via_shim = ProviderCredentialConfig::new(Some(env.clone()), None).expect("valid");
        let via_canonical = Canonical::new(Some(env), None).expect("valid");
        // Same type: a shim value binds directly to a canonical binding.
        let canonical: Canonical = via_shim.clone();
        assert_eq!(canonical, via_canonical);
        assert_eq!(via_shim.to_string(), via_canonical.to_string());
        assert_eq!(via_shim.source(), via_canonical.source());

        let mut vars = BTreeMap::new();
        vars.insert("SHIM_KEY".to_string(), "shim-value".to_string());
        assert_eq!(
            super::resolve_provider_credential(&via_shim, |v| vars.get(v).cloned()),
            canonical_resolve(&via_canonical, |v| vars.get(v).cloned()),
        );
        assert_eq!(
            super::MAX_CREDENTIAL_CMD_OUTPUT_BYTES,
            crate::provider_credential::MAX_CREDENTIAL_CMD_OUTPUT_BYTES
        );
    }
}
