//! Test-only I/O denial harness for the configuration subsystem.
//!
//! `bitty#2869` requires that `--safe` performs **no external configuration
//! read**, proved by a harness that denies I/O instead of trusting prose or a
//! parked FIFO. [`deny`] arms a thread-local gate; while it is armed every
//! configuration filesystem door is recorded and refused:
//!
//! - `read_config_file` (the one choke point behind every entry-file, profile,
//!   and `extends` read);
//! - the local-theme loader (the one external read `appearance.theme` can
//!   trigger); and
//! - the configuration module-loader install (the only door through which the
//!   configuration VM can reach the filesystem, at `<config-dir>/lua`).
//!
//! The gate is deliberately narrow and explicit rather than a syscall filter:
//! the configuration VM has no `io`, `os`, `loadfile`, or `dofile`, so the
//! three doors above are the complete external-read surface of a
//! configuration generation. A test arms the gate, runs the same resolution
//! the binary runs, and asserts [`attempts`] is empty. The negative control in
//! the same suite runs an equivalent non-safe load and asserts it is non-empty,
//! so the harness can fail.
//!
//! # Trusted bootstrap
//!
//! [`TRUSTED_BOOTSTRAP`] is the explicitly enumerated set of sources a safe
//! generation may draw from. Both are compiled-in data, not paths: the default
//! plan and the built-in safe overlay. Nothing on disk, in `$XDG_CONFIG_HOME`,
//! `$APPDATA`, `--config`, `BITTY_CONFIG`, a profile, or the local-theme
//! directory belongs to the bootstrap set.
//!
//! Compiled only under the `io-denied-harness` feature, which test builds
//! enable. A shipped build has no gate, no record, and no denial branch.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// A source a safe-mode generation is allowed to read.
///
/// These are compiled-in, path-free identities: no filesystem read, no root,
/// no entry file. A new bootstrap source must be added here deliberately.
///
/// The set is one entry because `--safe` resolves exactly
/// `bitty_config::reload::fallback_builtin()` (compiled defaults plus the safe
/// decoration) through `safe_merged`; nothing is layered on top. Spelling it
/// as a list keeps the rule explicit if a future safe path gains a second
/// compiled-in source.
pub const TRUSTED_BOOTSTRAP: &[&str] = &["builtin:safe-plan"];

/// One attempted external read, recorded while the gate was armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Stable door name: `config-file`, `local-theme`, or `module-loader`.
    pub kind: &'static str,
    /// The path the attempt named (test evidence only; never rendered).
    pub path: PathBuf,
}

struct State {
    armed: bool,
    attempts: Vec<Attempt>,
}

impl State {
    const fn new() -> Self {
        Self {
            armed: false,
            attempts: Vec::new(),
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = const { RefCell::new(State::new()) };
}

/// An armed I/O denial gate. Dropping it disarms the gate and clears the
/// record, so a failing assertion still leaves the thread usable.
pub struct Guard {
    _private: (),
}

impl Guard {
    /// The attempts recorded since this guard armed the gate.
    #[must_use]
    pub fn attempts(&self) -> Vec<Attempt> {
        attempts()
    }

    /// Whether any external read was attempted.
    #[must_use]
    pub fn saw_attempt(&self) -> bool {
        !attempts().is_empty()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.armed = false;
            state.attempts.clear();
        });
    }
}

/// Arm the gate. Every recorded attempt is refused with `PermissionDenied`.
#[must_use]
pub fn deny() -> Guard {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.armed = true;
        state.attempts.clear();
    });
    Guard { _private: () }
}

/// The attempts recorded while the gate is (or was last) armed.
#[must_use]
pub fn attempts() -> Vec<Attempt> {
    STATE.with(|state| state.borrow().attempts.clone())
}

/// The paths recorded while the gate is (or was last) armed.
#[must_use]
pub fn attempted_paths() -> Vec<PathBuf> {
    attempts().into_iter().map(|attempt| attempt.path).collect()
}

/// The explicitly enumerated safe-mode bootstrap set.
#[must_use]
pub fn trusted_bootstrap() -> &'static [&'static str] {
    TRUSTED_BOOTSTRAP
}

fn record(kind: &'static str, path: &Path) -> bool {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if !state.armed {
            return false;
        }
        state.attempts.push(Attempt {
            kind,
            path: path.to_path_buf(),
        });
        true
    })
}

/// Deny and record one configuration file read.
pub(crate) fn deny_config_read(path: &Path) -> Result<(), crate::ConfigError> {
    if record("config-file", path) {
        return Err(crate::ConfigError::InvalidInput {
            message: String::from("external configuration read denied by the I/O harness"),
        });
    }
    Ok(())
}

/// Deny and record one local-theme file read.
pub(crate) fn deny_local_theme_read(path: &Path) -> Result<(), crate::ConfigError> {
    if record("local-theme", path) {
        return Err(crate::ConfigError::InvalidInput {
            message: String::from("local-theme read denied by the I/O harness"),
        });
    }
    Ok(())
}

/// Deny and record one configuration module-loader install.
///
/// The loader is the configuration VM's only filesystem door: without an
/// install the VM cannot open, stat, or read a module file.
pub(crate) fn deny_loader_install(root: &Path) -> Result<(), crate::ConfigError> {
    if record("module-loader", root) {
        return Err(crate::ConfigError::InvalidInput {
            message: String::from("configuration module loader denied by the I/O harness"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_records_and_refuses_then_clears() {
        {
            let gate = deny();
            assert!(deny_config_read(Path::new("/tmp/init.lua")).is_err());
            assert!(deny_local_theme_read(Path::new("/tmp/theme.toml")).is_err());
            assert!(deny_loader_install(Path::new("/tmp/lua")).is_err());
            assert_eq!(gate.attempts().len(), 3);
            assert_eq!(
                gate.attempts()
                    .iter()
                    .map(|attempt| attempt.kind)
                    .collect::<Vec<_>>(),
                vec!["config-file", "local-theme", "module-loader"]
            );
        }
        // Disarmed: the same doors are inert and nothing is recorded.
        assert!(deny_config_read(Path::new("/tmp/init.lua")).is_ok());
        assert!(attempts().is_empty());
    }

    #[test]
    fn bootstrap_set_is_embedded_and_path_free() {
        assert_eq!(trusted_bootstrap(), TRUSTED_BOOTSTRAP);
        for source in trusted_bootstrap() {
            assert!(source.starts_with("builtin:"), "{source}");
            assert!(!source.contains('/'), "{source}");
            assert!(!source.contains('\\'), "{source}");
        }
    }
}
