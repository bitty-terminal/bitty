//! Binary-level `--fail-loud` startup proof (CTX-0481, issue #762).
//!
//! Fail-soft startup masks broken shells: a failed PTY spawn only warns and
//! the headless smoke still exits 0, so CI stays green while the shell is
//! broken. `--fail-loud` is the opt-in fail-loud startup option: a requested
//! startup step (primary shell, pane shells, IPC servo) that fails aborts
//! with a non-zero exit code. The default stays fail-soft.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`
//! with an isolated `XDG_CONFIG_HOME`/`HOME`; `--headless` dispatches
//! without a display, instance, or plugin VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Exit code `--fail-loud` uses for a failed startup step.
const EXIT_STARTUP: i32 = 1;

/// Fresh isolated scratch root.
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0481-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs `bitty <args>` under the isolated root.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_SOCKET")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A program that can never spawn: the path does not exist.
const BROKEN_PROGRAM: &str = "/nonexistent-ctx0481/bitty-broken-shell";

#[test]
fn fail_loud_headless_broken_shell_exits_non_zero() {
    // The regression: pre-fix the spawn failure only warned and the smoke
    // still exited 0 (green CI masking a broken shell).
    let root = scratch_root("broken-loud");
    let output = run_bitty(&root, &["--headless", "--fail-loud", "--", BROKEN_PROGRAM]);
    assert_eq!(
        output.status.code(),
        Some(EXIT_STARTUP),
        "--fail-loud must abort on a failed shell spawn (stderr={:?})",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("spawn failed") && err.contains(BROKEN_PROGRAM),
        "stderr must name the failed startup step, got {err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_headless_broken_shell_stays_fail_soft() {
    // Negative control: without `--fail-loud` the documented fail-soft
    // behavior is preserved (headless smoke still proves the tick path).
    let root = scratch_root("broken-soft");
    let output = run_bitty(&root, &["--headless", "--", BROKEN_PROGRAM]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "default startup must stay fail-soft (stderr={:?})",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("PTY spawn failed"),
        "fail-soft path must still warn, got {:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Unix-only positive control: the default shell chain falls back to
/// `/bin/sh`, which does not exist on Windows (the Windows default-shell
/// gap is tracked separately), so a healthy bare startup cannot be assumed
/// cross-platform.
#[cfg(unix)]
#[test]
fn fail_loud_headless_good_shell_exits_zero() {
    // Positive control: `--fail-loud` must not turn a healthy startup into
    // a failure.
    let root = scratch_root("good-loud");
    let output = run_bitty(&root, &["--headless", "--fail-loud"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "healthy --fail-loud startup must exit 0 (stderr={:?})",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}
