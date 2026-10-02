//! Cleared-environment allowlist for component processes.

use std::ffi::{OsStr, OsString};

use super::COMPONENT_ENV_ALLOWLIST;

/// Whether `name` may be forwarded to a component process.
///
/// Matching is exact and case-sensitive on every platform: the allowlist
/// names both spellings of the proxy variables explicitly.
#[must_use]
pub fn is_allowlisted_env(name: &str) -> bool {
    COMPONENT_ENV_ALLOWLIST.contains(&name)
}

/// The environment a component process starts with.
///
/// The child's environment is cleared and only these pairs are set; the
/// list can only ever hold [`COMPONENT_ENV_ALLOWLIST`] names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentEnv {
    vars: Vec<(OsString, OsString)>,
}

impl ComponentEnv {
    /// Empty environment (nothing forwarded).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Capture the allowlisted variables from the host process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::capture(|name| std::env::var_os(name))
    }

    /// Capture the allowlisted variables through `lookup` (hermetic tests
    /// pass a map). Names outside the allowlist are never queried.
    #[must_use]
    pub fn capture(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let vars = COMPONENT_ENV_ALLOWLIST
            .iter()
            .filter_map(|name| lookup(name).map(|value| (OsString::from(name), value)))
            .collect();
        Self { vars }
    }

    /// Forwarded `(name, value)` pairs, in allowlist order.
    pub fn vars(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
    }

    /// Number of forwarded variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether nothing is forwarded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn capture_forwards_only_allowlisted_names() {
        let host: BTreeMap<&str, &str> = [
            ("PATH", "fixture-path"),
            ("HOME", "fixture-home"),
            ("GITHUB_TOKEN", "secret"),
            ("HTTPS_PROXY", "http://proxy.invalid:3128"),
            ("no_proxy", "localhost"),
            ("LANG", "C.UTF-8"),
        ]
        .into_iter()
        .collect();
        let mut queried = Vec::new();
        let env = ComponentEnv::capture(|name| {
            queried.push(name.to_owned());
            host.get(name).map(OsString::from)
        });
        let names: Vec<&OsStr> = env.vars().map(|(name, _)| name).collect();
        assert_eq!(names, ["HTTPS_PROXY", "no_proxy", "LANG"]);
        assert!(queried.iter().all(|name| is_allowlisted_env(name)));
        assert!(!queried.iter().any(|name| name == "PATH"));
    }

    #[test]
    fn allowlist_excludes_path_and_secrets() {
        for name in [
            "PATH",
            "HOME",
            "USER",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!is_allowlisted_env(name), "{name}");
        }
        for name in COMPONENT_ENV_ALLOWLIST {
            assert!(is_allowlisted_env(name));
        }
        assert!(!is_allowlisted_env("lang"));
    }

    #[test]
    fn empty_forwards_nothing() {
        assert!(ComponentEnv::empty().is_empty());
        assert_eq!(ComponentEnv::empty().len(), 0);
    }
}
