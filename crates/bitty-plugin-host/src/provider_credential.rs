//! AI-side provider credential schema surface (CTX-0916 S5, DEC-0100).
//!
//! Staged in Core pending the S6 `bitty-ai` registration handoff: the
//! provider-schema surface ([`ProviderCredentialConfig`], its narrow-only
//! override check, and credential resolution) is AI policy, while the
//! reference mechanism it builds on ([`crate::credential_ref`]) stays Core.
//! The canonical home moves to `bitty-ai` in S6; Core keeps a deprecated
//! re-export shim in [`crate::credential_ref`] for one release (removed in
//! S7). New callers import from here (or the crate root), never from the
//! shim.
//!
//! A [`ProviderCredentialConfig`] names *where* a provider credential comes
//! from without carrying a value, and resolution reads through an injected
//! environment lookup and one bounded, shell-free command runner;
//! diagnostics quote variable and program names only — values never enter
//! an error, log, or audit detail. Composes with ADR-0006 and MP-10 per the
//! OQ-054 state.
//!
//! # Non-goals
//!
//! Provider adapters stay in `bitty-ai`. There is no `unsafe`, no I/O
//! beyond the single bounded credential-command spawn, and no new
//! dependency (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::credential_ref::{
    CredentialRef, CredentialSource, check_project_override, resolve_choice,
};
use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of captured credential-command output accepted as a value.
///
/// Mirrors the secret value bound (`4096`) so a command source can never
/// smuggle a larger credential than the host store admits.
pub const MAX_CREDENTIAL_CMD_OUTPUT_BYTES: usize = 4096;

// ── provider-schema surface (OQ-054 implementation) ───────────────────────

/// Provider credential config: the accepted `api_key_env` /
/// `api_key_cmd` surface (OQ-054 MPC-1..MPC-4).
///
/// At most one field may resolve: both set denies as conflict at
/// resolution ([`resolve_choice`]), neither set resolves to no
/// credential. Construction preserves both fields so the conflict stays
/// observable (and auditable) instead of being rejected silently at
/// parse time. `Display`/`Debug` quote reference names only — values
/// never enter this type.
///
/// CTX-0916 S5 (DEC-0100): AI-side schema staged in Core; the canonical
/// home moves to `bitty-ai` in S6. Import from here (or the crate root),
/// not from the deprecated [`crate::credential_ref`] shim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderCredentialConfig {
    /// `api_key_env` reference, if configured.
    pub api_key_env: Option<CredentialRef>,
    /// `api_key_cmd` reference, if configured.
    pub api_key_cmd: Option<CredentialRef>,
}

impl ProviderCredentialConfig {
    /// Build a config from already-validated references.
    ///
    /// Both-present is preserved (resolution denies as conflict); each
    /// reference must still be the matching source kind (`Env` for
    /// `api_key_env`, `Cmd` for `api_key_cmd`), otherwise construction
    /// denies fail-closed.
    pub fn new(
        api_key_env: Option<CredentialRef>,
        api_key_cmd: Option<CredentialRef>,
    ) -> Result<Self, PluginError> {
        if let Some(ref reference) = api_key_env {
            if reference.source_kind() != CredentialSource::Env {
                return Err(PluginError::registry(format!(
                    "api_key_env must name an env reference, got '{reference}'"
                )));
            }
        }
        if let Some(ref reference) = api_key_cmd {
            if reference.source_kind() != CredentialSource::Cmd {
                return Err(PluginError::registry(format!(
                    "api_key_cmd must name a command reference, got '{reference}'"
                )));
            }
        }
        Ok(Self {
            api_key_env,
            api_key_cmd,
        })
    }

    /// Empty config (no credential).
    #[must_use]
    pub fn unset() -> Self {
        Self {
            api_key_env: None,
            api_key_cmd: None,
        }
    }

    /// Whether neither reference is configured.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.api_key_env.is_none() && self.api_key_cmd.is_none()
    }

    /// Which source wins under the exclusive-or order, if any.
    ///
    /// Both set denies fail-closed with a names-only grant error.
    pub fn source(&self) -> Result<Option<CredentialSource>, PluginError> {
        resolve_choice(self.api_key_env.as_ref(), self.api_key_cmd.as_ref())
    }

    /// Narrow-only project check for a whole provider config.
    ///
    /// Each field is checked with [`check_project_override`]: the
    /// overlay may keep each reference identical or remove it, never
    /// add, rename, or switch sources.
    pub fn check_overlay(&self, overlay: &Self) -> Result<(), PluginError> {
        check_provider_override(self, overlay)
    }
}

impl fmt::Display for ProviderCredentialConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.api_key_env, &self.api_key_cmd) {
            (None, None) => f.write_str("credential:unset"),
            (Some(env), None) => write!(f, "{env}"),
            (None, Some(cmd)) => write!(f, "{cmd}"),
            (Some(env), Some(cmd)) => write!(f, "conflict:{env}+{cmd}"),
        }
    }
}

/// Provider-level project override (OQ-054 adopted narrow-only).
///
/// Checks the `api_key_env` and `api_key_cmd` fields independently with
/// [`check_project_override`]; any widening on either field denies.
pub fn check_provider_override(
    base: &ProviderCredentialConfig,
    overlay: &ProviderCredentialConfig,
) -> Result<(), PluginError> {
    check_project_override(base.api_key_env.as_ref(), overlay.api_key_env.as_ref())?;
    check_project_override(base.api_key_cmd.as_ref(), overlay.api_key_cmd.as_ref())
}

/// Resolve a provider credential to its value, if configured.
///
/// Exclusive-or first ([`ProviderCredentialConfig::source`]): conflict
/// denies before anything is read. `Env` resolves through `env_lookup`
/// (injected so tests stay hermetic; the live wrapper below supplies
/// `std::env`); missing or empty variables deny with a names-only
/// error. `Cmd` resolves through the bounded shell-free runner
/// ([`execute_credential_cmd`]). Values are returned, never logged —
/// callers must inject them into child environments only, per ADR-0006.
pub fn resolve_provider_credential(
    config: &ProviderCredentialConfig,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, PluginError> {
    match config.source()? {
        None => Ok(None),
        Some(CredentialSource::Env) => {
            let var = match config.api_key_env.as_ref() {
                Some(CredentialRef::Env { var }) => var.as_str(),
                _ => {
                    return Err(PluginError::registry(
                        "api_key_env misconfigured (deny by default)".to_string(),
                    ));
                }
            };
            match env_lookup(var) {
                Some(value) if !value.is_empty() => Ok(Some(value)),
                _ => Err(PluginError::grant(format!(
                    "credential env '{var}' is missing or empty (deny by default)"
                ))),
            }
        }
        Some(CredentialSource::Cmd) => {
            let (program, args) = match config.api_key_cmd.as_ref() {
                Some(CredentialRef::Cmd { program, args }) => (program.as_str(), args.as_slice()),
                _ => {
                    return Err(PluginError::registry(
                        "api_key_cmd misconfigured (deny by default)".to_string(),
                    ));
                }
            };
            execute_credential_cmd(program, args).map(Some)
        }
    }
}

/// Live-environment resolution (`std::env` lookup).
///
/// Thin wrapper so the core ([`resolve_provider_credential`]) stays
/// hermetic in tests.
pub fn resolve_provider_credential_live(
    config: &ProviderCredentialConfig,
) -> Result<Option<String>, PluginError> {
    resolve_provider_credential(config, |var| std::env::var(var).ok())
}

/// Execute a credential command and return its output value.
///
/// Same shell-free, bounded, names-only discipline as the secret-tier
/// runner: direct spawn, null stdin, discarded stderr, one trailing
/// newline stripped, empty/NUL/non-UTF-8/oversize/non-zero Spawn
/// results deny. Errors quote `program` only.
pub fn execute_credential_cmd(program: &str, args: &[String]) -> Result<String, PluginError> {
    use std::process::{Command, Stdio};
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            PluginError::registry(format!("api_key_cmd '{program}' failed to spawn: {err}"))
        })?;
    if !output.status.success() {
        return Err(PluginError::registry(format!(
            "api_key_cmd '{program}' exited with {status}",
            status = output.status
        )));
    }
    let stdout = output.stdout;
    if stdout.len() > MAX_CREDENTIAL_CMD_OUTPUT_BYTES {
        return Err(PluginError::LimitExceeded {
            field: "api_key_cmd.output".to_string(),
            limit: MAX_CREDENTIAL_CMD_OUTPUT_BYTES,
            actual: stdout.len(),
        });
    }
    if stdout.contains(&0) {
        return Err(PluginError::registry(format!(
            "api_key_cmd '{program}' output must not contain NUL bytes"
        )));
    }
    let mut text = String::from_utf8(stdout).map_err(|_| {
        PluginError::registry(format!("api_key_cmd '{program}' output is not UTF-8"))
    })?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    if text.is_empty() {
        return Err(PluginError::registry(format!(
            "api_key_cmd '{program}' produced empty output"
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_ref::CredentialRef;

    #[test]
    fn provider_config_enforces_source_kinds() {
        let env = CredentialRef::from_env("MY_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("pass", vec!["show".to_string()]).expect("valid cmd");
        // Swapped kinds deny at construction.
        assert!(ProviderCredentialConfig::new(Some(cmd.clone()), None).is_err());
        assert!(ProviderCredentialConfig::new(None, Some(env.clone())).is_err());
        let unset = ProviderCredentialConfig::unset();
        assert!(unset.is_unset());
        assert_eq!(unset.source(), Ok(None));
        assert_eq!(unset.to_string(), "credential:unset");
    }

    #[test]
    fn provider_config_resolves_unset_env_cmd_conflict() {
        use std::collections::BTreeMap;
        let env = CredentialRef::from_env("MY_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("echo", vec!["from-cmd".to_string()]).expect("valid cmd");
        let mut vars = BTreeMap::new();
        vars.insert("MY_KEY".to_string(), "from-env".to_string());

        let unset = ProviderCredentialConfig::unset();
        assert_eq!(
            resolve_provider_credential(&unset, |v| vars.get(v).cloned()),
            Ok(None)
        );

        let env_only = ProviderCredentialConfig::new(Some(env.clone()), None).expect("valid");
        assert_eq!(env_only.source(), Ok(Some(CredentialSource::Env)));
        assert_eq!(
            resolve_provider_credential(&env_only, |v| vars.get(v).cloned()),
            Ok(Some("from-env".to_string()))
        );
        // Missing variable denies with the name (never a value).
        let missing =
            resolve_provider_credential(&env_only, |_| None).expect_err("missing env must deny");
        assert!(missing.to_string().contains("MY_KEY"));

        let cmd_only = ProviderCredentialConfig::new(None, Some(cmd)).expect("valid");
        assert_eq!(cmd_only.source(), Ok(Some(CredentialSource::Cmd)));
        assert_eq!(
            resolve_provider_credential(&cmd_only, |v| vars.get(v).cloned()),
            Ok(Some("from-cmd".to_string()))
        );

        // Both set denies before anything is read.
        let both = ProviderCredentialConfig::new(Some(env), cmd_only.api_key_cmd.clone())
            .expect("conflict preserved");
        let error = resolve_provider_credential(&both, |v| vars.get(v).cloned())
            .expect_err("conflict must deny");
        let text = error.to_string();
        assert!(text.contains("api_key_env:MY_KEY"), "{text}");
        assert!(text.contains("api_key_cmd:echo from-cmd"), "{text}");
    }

    #[test]
    fn provider_overlay_narrows_only() {
        let env = CredentialRef::from_env("BASE_KEY").expect("valid env");
        let same = CredentialRef::from_env("BASE_KEY").expect("valid env");
        let wider = CredentialRef::from_env("OTHER_KEY").expect("valid env");
        let cmd = CredentialRef::from_cmd("pass", vec!["x".to_string()]).expect("valid cmd");
        let base = ProviderCredentialConfig::new(Some(env), None).expect("valid");
        let narrow_same = ProviderCredentialConfig::new(Some(same), None).expect("valid");
        let narrow_none = ProviderCredentialConfig::unset();
        let widen_rename = ProviderCredentialConfig::new(Some(wider), None).expect("valid");
        let widen_add = ProviderCredentialConfig::new(None, Some(cmd)).expect("valid");

        assert!(base.check_overlay(&narrow_same).is_ok());
        assert!(base.check_overlay(&narrow_none).is_ok());
        assert!(base.check_overlay(&widen_rename).is_err());
        // Removing env while adding cmd still widens (source switch).
        assert!(base.check_overlay(&widen_add).is_err());
        // Empty base gains nothing.
        assert!(narrow_none.check_overlay(&base).is_err());
        assert!(narrow_none.check_overlay(&narrow_none).is_ok());
    }

    #[test]
    fn credential_cmd_empty_output_denies() {
        assert!(execute_credential_cmd("true", &[]).is_err());
        assert!(execute_credential_cmd("false", &[]).is_err());
        let missing = execute_credential_cmd("bitty-definitely-missing-xyz", &[]);
        let text = missing.expect_err("missing program must deny").to_string();
        assert!(text.contains("bitty-definitely-missing-xyz"), "{text}");
    }
}
