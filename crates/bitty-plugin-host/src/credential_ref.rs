//! `api_key_env` vs `api_key_cmd` credential semantics (OQ-054).
//!
//! `OQ-054` is adopted (MPC-1..MPC-4): `api_key_env` versus
//! `api_key_cmd` credential references, their exclusive-or resolution
//! order, and the project-level override boundary that cannot widen
//! credentials. This module carries the adopted reference data as pure,
//! bounded, fail-closed types, enforces the exclusive-or order at the
//! call boundary ([`resolve_choice`]) plus the narrow-only project
//! boundary ([`check_project_override`]), and ships the provider-schema
//! surface ([`ProviderCredentialConfig`]) with actual resolution
//! ([`resolve_provider_credential`]).
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

/// Maximum bytes of captured credential-command output accepted as a value.
///
/// Mirrors the secret value bound (`4096`) so a command source can never
/// smuggle a larger credential than the host store admits.
pub const MAX_CREDENTIAL_CMD_OUTPUT_BYTES: usize = 4096;

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
/// runs here; the winner's resolution is [`resolve_provider_credential`]
/// on the provider-schema surface ([`ProviderCredentialConfig`]).
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
