//! Cleared-environment allowlist for component processes.

use std::ffi::{OsStr, OsString};

use super::{COMPONENT_ENV_ALLOWLIST, COMPONENT_ENV_WINDOWS_ALLOWLIST};

/// Names forwarded on a platform: [`COMPONENT_ENV_ALLOWLIST`], plus
/// [`COMPONENT_ENV_WINDOWS_ALLOWLIST`] when `windows` (DIR-030 D1).
fn forwarded_names_for(windows: bool) -> impl Iterator<Item = &'static str> {
    let extra: &'static [&'static str] = if windows {
        &COMPONENT_ENV_WINDOWS_ALLOWLIST
    } else {
        &[]
    };
    COMPONENT_ENV_ALLOWLIST.iter().chain(extra).copied()
}

/// Names forwarded on the build platform.
fn forwarded_names() -> impl Iterator<Item = &'static str> {
    forwarded_names_for(cfg!(windows))
}

/// Whether `name` may be forwarded to a component process.
///
/// Matching is exact and case-sensitive on every platform: the allowlist
/// names both spellings of the proxy variables explicitly. The Windows-only
/// names match only on Windows.
#[must_use]
pub fn is_allowlisted_env(name: &str) -> bool {
    forwarded_names().any(|allowed| allowed == name)
}

/// The environment a component process starts with.
///
/// The child's environment is cleared and only these pairs are set; the
/// list can only ever hold [`COMPONENT_ENV_ALLOWLIST`] names (plus
/// [`COMPONENT_ENV_WINDOWS_ALLOWLIST`] on Windows).
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
    pub fn capture(lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        Self::capture_for(cfg!(windows), lookup)
    }

    fn capture_for(windows: bool, mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let vars = forwarded_names_for(windows)
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

    fn windows_host() -> BTreeMap<&'static str, &'static str> {
        [
            ("SystemRoot", "fixture-system-root"),
            ("windir", "fixture-windir"),
            ("PATH", "fixture-path"),
            ("LANG", "C.UTF-8"),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn system_root_is_forwarded_only_for_windows() {
        assert_eq!(COMPONENT_ENV_WINDOWS_ALLOWLIST, ["SystemRoot"]);
        let host = windows_host();
        let windows = ComponentEnv::capture_for(true, |name| host.get(name).map(OsString::from));
        let names: Vec<&OsStr> = windows.vars().map(|(name, _)| name).collect();
        assert_eq!(names, ["LANG", "SystemRoot"]);
        let other = ComponentEnv::capture_for(false, |name| host.get(name).map(OsString::from));
        let names: Vec<&OsStr> = other.vars().map(|(name, _)| name).collect();
        assert_eq!(names, ["LANG"]);
        for windows in [true, false] {
            let names: Vec<&str> = forwarded_names_for(windows).collect();
            assert!(!names.contains(&"windir"));
            assert!(!names.iter().any(|name| name.eq_ignore_ascii_case("PATH")));
        }
    }

    #[test]
    fn build_platform_decides_system_root() {
        let host = windows_host();
        let env = ComponentEnv::capture(|name| host.get(name).map(OsString::from));
        let forwarded = env.vars().any(|(name, _)| name == "SystemRoot");
        assert_eq!(forwarded, cfg!(windows));
        assert_eq!(is_allowlisted_env("SystemRoot"), cfg!(windows));
        assert!(!is_allowlisted_env("windir"));
    }
}
