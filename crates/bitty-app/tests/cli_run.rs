//! `bitty run -- COMMAND...` end-to-end dispatch proofs (CTX-0170).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`run` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty run`, local class).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `run` dispatches before config load and GUI startup, so every case here is
//! headless-safe: no display, instance, IPC, or plugin VM is touched. The
//! child inherits stdio pipes captured here, runs directly (no shell), and
//! its numeric exit code becomes `bitty`'s exit code.

use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Runs `bitty` with `args`, capturing output. Callers assert on status and
/// streams; every child here is a fast POSIX utility (`true`, `echo`, ...).
fn run_bitty(args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn run_help_exits_zero_with_usage() {
    let output = run_bitty(&["run", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "run --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty run") && text.contains("-- COMMAND"),
        "run --help must describe the separator contract, got {text:?}"
    );
}

#[test]
fn run_missing_separator_is_usage_error() {
    // `bitty run echo hi` must not run echo: `--` is required (exit 2).
    let output = run_bitty(&["run", "echo", "hi"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare token before `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("--"),
        "diagnostic must name the `--` separator, got {:?}",
        stderr(&output)
    );
}

#[test]
fn run_bare_with_no_args_is_usage_error() {
    let output = run_bitty(&["run"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty run` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_missing_command_after_separator_is_usage_error() {
    let output = run_bitty(&["run", "--"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "`run --` with no COMMAND must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_minimal_command_passes_through_stdout() {
    let output = run_bitty(&["run", "--", "echo", "hello-run"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "echo child must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("hello-run"),
        "child stdout must pass through, got {:?}",
        stdout(&output)
    );
}

#[test]
fn run_exit_code_passthrough_true_false() {
    let ok = run_bitty(&["run", "--", "true"]);
    assert_eq!(ok.status.code(), Some(0), "true must map to 0");
    let fail = run_bitty(&["run", "--", "false"]);
    assert_ne!(
        fail.status.code(),
        Some(0),
        "false must not map to 0 (passthrough)"
    );
}

#[test]
fn run_exit_code_passthrough_arbitrary() {
    // `sh -c 'exit 42'` proves numeric passthrough beyond 0/1.
    let output = run_bitty(&["run", "--", "sh", "-c", "exit 42"]);
    assert_eq!(
        output.status.code(),
        Some(42),
        "child exit 42 must become bitty exit 42, stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_env_option_reaches_child() {
    let output = run_bitty(&[
        "run",
        "--env",
        "BITTY_RUN_PROBE=bar",
        "--",
        "printenv",
        "BITTY_RUN_PROBE",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--env child must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert_eq!(
        stdout(&output).trim(),
        "bar",
        "--env KEY=VALUE must reach the child, got {:?}",
        stdout(&output)
    );
}

#[test]
fn run_child_gets_stable_indicators() {
    // RFC-stable indicators: TERM=bitty, BITTY=1, BITTY_VERSION=<semver>.
    let term = run_bitty(&["run", "--", "printenv", "TERM"]);
    assert_eq!(term.status.code(), Some(0));
    assert_eq!(stdout(&term).trim(), "bitty");
    let flag = run_bitty(&["run", "--", "printenv", "BITTY"]);
    assert_eq!(flag.status.code(), Some(0));
    assert_eq!(stdout(&flag).trim(), "1");
    let version = run_bitty(&["run", "--", "printenv", "BITTY_VERSION"]);
    assert_eq!(version.status.code(), Some(0));
    assert!(
        !stdout(&version).trim().is_empty(),
        "BITTY_VERSION must be non-empty"
    );
}

#[test]
fn run_title_option_exposed_as_env() {
    let output = run_bitty(&[
        "run",
        "--title",
        "demo-title",
        "--",
        "printenv",
        "BITTY_TITLE",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--title child must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert_eq!(stdout(&output).trim(), "demo-title");
}

/// Normalizes a reported child dir for cross-platform comparison: folds
/// `\` to `/`, maps MSYS2/Git-Bash `/c/...` to `c:/...`, strips trailing
/// `/`, and lowercases (Windows is case-insensitive; Unix unaffected).
fn normalize_cwd_for_assert(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if normalized.len() >= 3
        && bytes[0] == b'/'
        && bytes[2] == b'/'
        && bytes[1].is_ascii_alphabetic()
    {
        normalized = format!("{}:/{}", bytes[1] as char, &normalized[3..]);
    }
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized.to_lowercase()
}

#[test]
fn run_cwd_option_changes_child_dir() {
    // Platform-correct temp dir: hardcoded `/tmp` does not exist on Windows
    // (os error 267). `temp_dir()` is `/tmp` on Unix, `C:\...\Temp` on Windows.
    let expected = std::env::temp_dir();
    let expected_str = expected.to_string_lossy().into_owned();
    let output = run_bitty(&["run", "--cwd", expected_str.as_str(), "--", "pwd"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--cwd temp_dir child must exit 0, stderr={:?}",
        stderr(&output)
    );
    let actual = stdout(&output);
    let actual_norm = normalize_cwd_for_assert(actual.trim());
    let expected_norm = normalize_cwd_for_assert(expected_str.trim());
    let leaf = expected
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    assert!(
        actual_norm == expected_norm || actual_norm.ends_with(&format!("/{leaf}")),
        "--cwd must change the child dir to {expected_str:?}, got {actual:?}",
    );
}

#[test]
fn run_unknown_flag_before_separator_is_usage_error() {
    let output = run_bitty(&["run", "--headless", "--", "echo"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown flag before `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_flag_after_separator_is_verbatim_command() {
    // `run -- --help` runs a program literally named `--help`: spawn fails
    // with generic error (exit 1), it is not help (exit 0) nor usage (exit 2).
    let output = run_bitty(&["run", "--", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "program named `--help` must fail as spawn error (exit 1), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_conflicting_names_after_separator_are_commands() {
    // Bare-PROGRAM conflict: `run -- config` runs a program named
    // `config`; it must not enter `bitty config` (which would exit 2 with
    // config usage). A missing `config` program is a spawn failure (exit 1).
    let output = run_bitty(&["run", "--", "config"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "`run -- config` must attempt the child (exit 1 when missing), stderr={:?}",
        stderr(&output)
    );
    // Same for the word `run` itself: `bitty run -- run ...` addresses a
    // program literally named `run`, never this subcommand recursively.
    let again = run_bitty(&["run", "--", "run"]);
    assert_eq!(
        again.status.code(),
        Some(1),
        "`run -- run` must attempt the child (exit 1 when missing), stderr={:?}",
        stderr(&again)
    );
}

#[test]
fn run_missing_program_is_generic_error_not_usage() {
    let output = run_bitty(&["run", "--", "bitty-run-definitely-missing-program-xyz"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "missing program must be generic error (exit 1), not usage (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn run_missing_cwd_is_generic_error() {
    // Join onto the platform temp dir so the parent is valid on Windows too;
    // the leaf itself must not exist (spawn failure, exit 1, on every OS).
    let missing = std::env::temp_dir().join("bitty-run-definitely-missing-dir-xyz");
    let _ = std::fs::remove_dir_all(&missing);
    let missing_str = missing.to_string_lossy().into_owned();
    let output = run_bitty(&["run", "--cwd", missing_str.as_str(), "--", "true"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "missing --cwd must be spawn failure (exit 1), stderr={:?}",
        stderr(&output)
    );
}
