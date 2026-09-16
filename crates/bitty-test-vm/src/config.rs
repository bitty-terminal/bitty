//! Environment configuration for the VM tier.
//!
//! Every host-specific value is derived from the environment; nothing is
//! hardcoded. The pure `*_from` functions take the raw variable values so
//! tests can cover every branch without mutating process-global state
//! (`std::env::set_var` is `unsafe` in edition 2024 and this crate forbids
//! unsafe code).

use std::ffi::OsStr;
use std::path::PathBuf;

/// VM root: holds `images/base` and `runs` (required for plan/smoke).
pub const VM_ROOT_ENV: &str = "BITTY_VM_ROOT";
/// Any value forces the smoke path to report every live stage as gated.
/// Mirrors the `BITTY_TEST_FORCE_NO_PTY` convention of `bitty-test-support`.
pub const FORCE_SKIP_ENV: &str = "BITTY_VM_FORCE_SKIP";
/// Any value opts into live execution (same as `--execute`), so CI can
/// enable a KVM-capable runner without changing the command line.
pub const LIVE_ENV: &str = "BITTY_VM_LIVE";
/// Documentation-only: guest installation media for the manual, far-future
/// base-image creation step. The controller never reads or mounts it.
pub const ISO_PATH_ENV: &str = "ISO_PATH";

/// Non-empty value of a single environment variable.
pub fn vm_root_from(var: Option<&OsStr>) -> Option<PathBuf> {
    var.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Any value at all (including empty) counts as "set", mirroring the
/// `BITTY_TEST_FORCE_NO_PTY` truth table.
pub fn flag_set_from(var: Option<&OsStr>) -> bool {
    var.is_some()
}

/// Resolved configuration for one controller invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    /// Configured VM root, when set and non-empty.
    pub vm_root: Option<PathBuf>,
    /// Live execution is refused and reported as gated.
    pub force_skip: bool,
    /// Live execution (`--execute` or `BITTY_VM_LIVE`) was requested.
    pub live: bool,
    /// `ISO_PATH` is set (reported by `doctor`; never used to run a VM).
    pub iso_path_set: bool,
}

impl VmConfig {
    /// Read the process environment.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var_os(VM_ROOT_ENV).as_deref(),
            std::env::var_os(FORCE_SKIP_ENV).as_deref(),
            std::env::var_os(LIVE_ENV).as_deref(),
            std::env::var_os(ISO_PATH_ENV).as_deref(),
        )
    }

    /// Pure construction from raw values (testable without env mutation).
    pub fn from_values(
        vm_root: Option<&OsStr>,
        force_skip: Option<&OsStr>,
        live: Option<&OsStr>,
        iso_path: Option<&OsStr>,
    ) -> Self {
        Self {
            vm_root: vm_root_from(vm_root),
            force_skip: flag_set_from(force_skip),
            live: flag_set_from(live),
            iso_path_set: flag_set_from(iso_path),
        }
    }

    /// Apply a `--root` override on top of the environment.
    pub fn with_root_override(mut self, root: Option<PathBuf>) -> Self {
        if root.is_some() {
            self.vm_root = root;
        }
        self
    }

    /// Apply a `--execute` override on top of the environment.
    pub fn with_execute(mut self, execute: bool) -> Self {
        if execute {
            self.live = true;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_root_requires_a_non_empty_value() {
        assert_eq!(vm_root_from(None), None);
        assert_eq!(vm_root_from(Some(OsStr::new(""))), None);
        assert_eq!(
            vm_root_from(Some(OsStr::new("/srv/bitty-vm"))),
            Some(PathBuf::from("/srv/bitty-vm"))
        );
    }

    #[test]
    fn flags_trigger_on_any_value() {
        assert!(!flag_set_from(None));
        assert!(flag_set_from(Some(OsStr::new(""))));
        assert!(flag_set_from(Some(OsStr::new("0"))));
        assert!(flag_set_from(Some(OsStr::new("1"))));
    }

    #[test]
    fn config_combines_environment_and_overrides() {
        let base = VmConfig::from_values(
            Some(OsStr::new("/env-root")),
            None,
            None,
            Some(OsStr::new("/media/iso")),
        );
        assert_eq!(base.vm_root, Some(PathBuf::from("/env-root")));
        assert!(!base.force_skip);
        assert!(!base.live);
        assert!(base.iso_path_set);

        let cli = base
            .clone()
            .with_root_override(Some(PathBuf::from("/cli-root")))
            .with_execute(true);
        assert_eq!(cli.vm_root, Some(PathBuf::from("/cli-root")));
        assert!(cli.live);

        let no_override = base.clone().with_root_override(None);
        assert_eq!(no_override.vm_root, Some(PathBuf::from("/env-root")));

        let forced = VmConfig::from_values(None, Some(OsStr::new("1")), None, None);
        assert!(forced.force_skip);
        assert_eq!(forced.vm_root, None);
        assert!(!forced.iso_path_set);
    }
}
