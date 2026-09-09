//! Shared test-harness helpers for Bitty (CTX-0267).
//!
//! Live-spawn tests (real shells through a real PTY) cannot run on platforms
//! whose PTY backend is unimplemented. Per ADR-0002 Unix is the Tier-1 launch
//! platform and the Windows ConPTY backend reports [`PtyError::Unsupported`]
//! (see `bitty-pty/src/platform/windows.rs`), so spawning `/bin/sh` there
//! fails instead of skipping. Historically each such test grew its own
//! `#[cfg(unix)]` gate (CTX-0227 sh-fakes, CTX-0257 workspace live-close),
//! which hides the test on Windows entirely: compile breaks there go
//! unnoticed until CI fails.
//!
//! This crate provides one central gate instead:
//!
//! - [`pty_supported`] reports whether the live PTY backend exists here.
//! - [`require_pty`] (macro) is the first statement of every live-spawn
//!   test: on unsupported platforms the test returns early with a `SKIP`
//!   notice (pass, not fail); on Unix behavior is unchanged.
//!
//! The `BITTY_TEST_FORCE_NO_PTY` environment variable forces "unsupported"
//! for any value (including empty). It exists so the skip path is exercisable
//! on Linux/macOS: `BITTY_TEST_FORCE_NO_PTY=1 cargo test ...`.
//!
//! [`PtyError::Unsupported`]: https://github.com/bitty-terminal/bitty
use std::ffi::OsStr;

/// Environment variable that forces [`pty_supported`] to `false`.
///
/// Any value (including empty) counts as set; only fully unset means "no
/// override". Checked with `var_os` so non-UTF-8 values still force a skip.
pub const FORCE_NO_PTY_ENV: &str = "BITTY_TEST_FORCE_NO_PTY";

/// Pure detection core: `force_skip` mirrors [`FORCE_NO_PTY_ENV`] being set,
/// `platform_supported` mirrors the platform having a live PTY backend
/// (today: `cfg!(unix)`).
///
/// Truth table: skip wins over platform; both must agree for support.
pub fn pty_supported_impl(force_skip: bool, platform_supported: bool) -> bool {
    !force_skip && platform_supported
}

/// Pure override check: `Some(_)` (any value, even empty/non-UTF-8) forces a
/// skip; `None` (unset) defers to the platform.
pub fn env_forces_skip(var: Option<&OsStr>) -> bool {
    var.is_some()
}

/// Whether a live PTY spawn is expected to work on this machine.
///
/// `false` on platforms without a PTY backend (today: non-Unix, where the
/// ConPTY slice is unimplemented per ADR-0002) and whenever
/// [`FORCE_NO_PTY_ENV`] is set. Pure logic lives in [`pty_supported_impl`]
/// so the matrix is unit-testable without touching the process environment.
pub fn pty_supported() -> bool {
    pty_supported_impl(
        env_forces_skip(std::env::var_os(FORCE_NO_PTY_ENV).as_deref()),
        cfg!(unix),
    )
}

/// First statement of every live-spawn test.
///
/// Expands to an early `return` (test passes, reported as `SKIP`) when
/// [`pty_supported`] is `false`, so the test compiles on every platform but
/// only spawns where a PTY backend exists. Must be invoked inside a `#[test]`
/// function returning `()` (a plain early return, no panic, no exit).
///
/// ```ignore
/// #[test]
/// fn live_shell_echo() {
///     bitty_test_support::require_pty!();
///     // ... spawn /bin/sh here; reached only where supported.
/// }
/// ```
#[macro_export]
macro_rules! require_pty {
    () => {
        if !$crate::pty_supported() {
            eprintln!(
                "SKIP live-PTY test: no PTY backend on this platform (see ADR-0002); \
                 set {} only to force this path.",
                $crate::FORCE_NO_PTY_ENV
            );
            return;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_matrix_skip_wins_over_platform() {
        // Supported platform, no override: live spawns run.
        assert!(pty_supported_impl(false, true));
        // Forced skip wins even on a supported platform (the Linux
        // `BITTY_TEST_FORCE_NO_PTY=1` simulation path).
        assert!(!pty_supported_impl(true, true));
        // Unsupported platform stays unsupported with or without override.
        assert!(!pty_supported_impl(false, false));
        assert!(!pty_supported_impl(true, false));
    }

    #[test]
    fn env_override_triggers_on_any_value() {
        assert!(!env_forces_skip(None));
        assert!(env_forces_skip(Some(OsStr::new("1"))));
        assert!(env_forces_skip(Some(OsStr::new(""))));
        // Non-UTF-8 values still count (var_os, never from_utf8).
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert!(env_forces_skip(Some(OsStr::from_bytes(&[0x66, 0x80]))));
        }
    }

    #[test]
    fn public_entry_matches_platform_unless_overridden() {
        let expected = pty_supported_impl(
            env_forces_skip(std::env::var_os(FORCE_NO_PTY_ENV).as_deref()),
            cfg!(unix),
        );
        assert_eq!(pty_supported(), expected);
        // Sanity on the platform half: this crate's backend expectation is
        // Unix-only until the ConPTY slice lands (ADR-0002).
        assert_eq!(cfg!(unix), true_or_false_platform_probe());
    }

    /// Documents the platform assumption in one place: Unix has the live
    /// backend, other platforms do not (yet). If Windows gains ConPTY
    /// support, update this probe, [`pty_supported`], and the lint markers
    /// together.
    fn true_or_false_platform_probe() -> bool {
        cfg!(unix)
    }
}
