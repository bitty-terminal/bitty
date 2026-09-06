//! `bitty ctl` end-to-end dispatch proofs (CTX-0171).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`ctl` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty ctl`, runtime class).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `ctl --help`, usage errors, and `instance list` are headless-safe (no
//! display, instance, or plugin VM). Verbs needing a live instance fail
//! closed with exit 6 (`Unavailable`) when none exists — never a silent
//! pick and never a hang (5 s IPC timeouts).

use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Runs `bitty` with `args`, capturing output.
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
fn ctl_help_exits_zero_without_instance() {
    let output = run_bitty(&["ctl", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "ctl --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty ctl") && text.contains("terminal send"),
        "ctl --help must describe control verbs, got {text:?}"
    );
}

#[test]
fn ctl_bare_with_no_args_is_usage_error() {
    let output = run_bitty(&["ctl"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty ctl` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_unknown_verb_is_usage_error() {
    let output = run_bitty(&["ctl", "frobnicate", "list"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown resource must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("unknown"),
        "diagnostic must name the problem, got {:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_stray_separator_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "list", "--"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_bad_format_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "list", "--format", "bogus"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bad --format must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_send_missing_text_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "send", "t:1"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "send without TEXT must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_close_invalid_id_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "close", "nope"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bad terminal id must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_split_two_directions_is_usage_error() {
    let output = run_bitty(&["ctl", "view", "split", "--left", "--right"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "two split dirs must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_instance_list_exits_zero_without_instance() {
    // Local discovery: no IPC, no scope, never requires a live peer.
    let output = run_bitty(&["ctl", "instance", "list"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "instance list must exit 0, stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_instance_list_json_is_versioned_envelope() {
    let output = run_bitty(&["ctl", "instance", "list", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "instance list --format json must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"v\":1")
            && text.contains("\"command\":\"core.instance.list\"")
            && text.contains("\"ok\":true"),
        "json must be the v1 envelope, got {text:?}"
    );
    // Stdout must be exactly one JSON value (no interleaved logs).
    assert_eq!(
        text.trim().lines().count(),
        1,
        "json stdout must be one value, got {text:?}"
    );
}

#[test]
fn ctl_runtime_verb_without_instance_is_unavailable() {
    // No live instance in test env (isolated XDG_RUNTIME_DIR): must fail
    // closed with exit 6, never hang, never pick an unrelated instance.
    let output = run_bitty(&[
        "ctl",
        "--socket",
        "/tmp/bitty-ctl-test-nonexistent.sock",
        "terminal",
        "list",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_word_is_subcommand_not_program() {
    // `bitty ctl` never spawns a program named `ctl`; unknown verbs are
    // usage errors, not shell executions.
    let output = run_bitty(&["ctl", "terminal", "dance"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown verb must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        !stdout(&output).contains("dance"),
        "must not execute anything, got {:?}",
        stdout(&output)
    );
}
