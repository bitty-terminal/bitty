//! `bitty inspect` end-to-end dispatch proofs (CTX-0173).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`inspect` introspection) as
//! refined by `cli-contract-rfc.md` (mixed class, envelope v1, exit codes).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `inspect` dispatches before config load and GUI startup, so every case here
//! is headless-safe: no display, no instance, no plugin VM, and no user
//! config file is read (built-in defaults and static manifests only).

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env_remove("BITTY_SOCKET")
        .env_remove("BITTY_INSTANCE_ID")
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
fn inspect_help_exits_zero() {
    let output = run_bitty(&["inspect", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "inspect --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty inspect") && text.contains("command|key|plugin|config|protocol"),
        "inspect --help must describe targets, got {text:?}"
    );
}

#[test]
fn inspect_missing_target_is_usage_error() {
    let output = run_bitty(&["inspect"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty inspect` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("missing <target>"),
        "diagnostic must name the missing target, got {:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "usage errors must not emit stdout, got {:?}",
        stdout(&output)
    );
}

#[test]
fn inspect_unknown_target_is_usage_error() {
    let output = run_bitty(&["inspect", "fonts", "x"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown target must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("unknown target"),
        "diagnostic must name the bad target, got {:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_missing_value_is_usage_error() {
    let output = run_bitty(&["inspect", "command"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "missing value must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("missing <value>"),
        "diagnostic must name the missing value, got {:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_stray_separator_is_usage_error() {
    let output = run_bitty(&["inspect", "command", "core.view.list", "--"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_bad_format_is_usage_error() {
    let output = run_bitty(&["inspect", "command", "core.view.list", "--format", "yaml"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bad --format must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_socket_flag_is_usage_error() {
    // `inspect` is local-only: targeting flags fail closed, never ignored.
    let output = run_bitty(&[
        "inspect",
        "command",
        "core.view.list",
        "--socket",
        "/tmp/x.sock",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--socket with inspect must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_command_table_names_owner_and_scopes() {
    let output = run_bitty(&["inspect", "command", "core.terminal.text"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "known command must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("core.terminal.text") && text.contains("terminal.inspect"),
        "table must name id and scopes, got {text:?}"
    );
    assert!(
        text.contains("core"),
        "table must name the owner, got {text:?}"
    );
}

#[test]
fn inspect_command_json_envelope_shape() {
    let output = run_bitty(&["inspect", "command", "core.view.split", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "json inspect must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    let trimmed = text.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout must be one JSON object, got {text:?}"
    );
    for needle in [
        "\"v\":1",
        "\"command\":\"inspect\"",
        "\"ok\":true",
        "\"target\":\"command\"",
        "core.view.split",
        "view.manage",
    ] {
        assert!(text.contains(needle), "missing {needle}, got {text:?}");
    }
    // Machine output is never corrupted by diagnostics on stdout.
    assert_eq!(text.trim().lines().count(), 1);
}

#[test]
fn inspect_command_unknown_value_is_not_found() {
    let output = run_bitty(&["inspect", "command", "core.nope.nope", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unknown command must be NotFound (exit 1), stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"ok\":false") && text.contains("CommandNotFound"),
        "json must carry ok:false + code, got {text:?}"
    );
    // Table form reports the same miss on stderr with no stdout.
    let table = run_bitty(&["inspect", "command", "core.nope.nope"]);
    assert_eq!(table.status.code(), Some(1));
    assert!(stdout(&table).is_empty());
    assert!(stderr(&table).contains("unknown command"));
}

#[test]
fn inspect_key_bound_names_action() {
    let output = run_bitty(&["inspect", "key", "alt+h", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "bound chord must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"target\":\"key\"") && text.contains("\"bound\":true"),
        "json must mark bound, got {text:?}"
    );
    assert!(
        text.contains("\"action\":"),
        "json must name action, got {text:?}"
    );
}

#[test]
fn inspect_key_unbound_reaches_shell() {
    // Well-formed but unbound chords succeed with bound:false (single-owner
    // rule: unbound keys reach the PTY/shell, never chrome).
    let output = run_bitty(&["inspect", "key", "ctrl+shift+f9"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "unbound chord must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bound: no") && text.contains("pty"),
        "table must explain the PTY fallback, got {text:?}"
    );
}

#[test]
fn inspect_key_malformed_is_usage_error() {
    let output = run_bitty(&["inspect", "key", "ctrl++"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "malformed chord must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn inspect_plugin_table_names_owner() {
    let output = run_bitty(&["inspect", "plugin", "bitty-terminal.workspace"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "known plugin must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty-terminal.workspace") && text.contains("bitty-terminal"),
        "table must name id and owner publisher, got {text:?}"
    );
    // Deprecated alias still resolves with a removal note.
    let old = run_bitty(&["inspect", "plugin", "bitty-terminal.tabs"]);
    assert_eq!(
        old.status.code(),
        Some(0),
        "deprecated alias must still exit 0, stderr={:?}",
        stderr(&old)
    );
    let old_text = stdout(&old);
    assert!(
        old_text.contains("bitty-terminal.tabs") && old_text.contains("deprecated alias"),
        "alias table must name old id + deprecation, got {old_text:?}"
    );
}

#[test]
fn inspect_plugin_unknown_is_not_found() {
    let output = run_bitty(&["inspect", "plugin", "nope.nope", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unknown plugin must be NotFound (exit 1), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("\"ok\":false") && stdout(&output).contains("PluginNotFound"),
        "json must carry ok:false + code, got {:?}",
        stdout(&output)
    );
}

#[test]
fn inspect_config_default_value() {
    let output = run_bitty(&["inspect", "config", "font.size", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "known config key must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"target\":\"config\"")
            && text.contains("\"key\":\"font.size\"")
            && text.contains("\"source\":\"default\""),
        "json must name key and owning layer, got {text:?}"
    );
}

#[test]
fn inspect_config_unknown_is_not_found() {
    let output = run_bitty(&["inspect", "config", "font.nope"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unknown config key must be NotFound (exit 1), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("unknown config key"));
}

#[test]
fn inspect_protocol_stub_state() {
    let output = run_bitty(&["inspect", "protocol", "kitty-graphics", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "known protocol must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"target\":\"protocol\"")
            && text.contains("kitty-graphics")
            && text.contains("\"status\":\"stub\""),
        "json must name support state, got {text:?}"
    );
}

#[test]
fn inspect_protocol_unknown_is_not_found() {
    let output = run_bitty(&["inspect", "protocol", "nope", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unknown protocol must be NotFound (exit 1), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("\"ok\":false"));
}

#[test]
fn inspect_jsonl_matches_json_envelope() {
    let output = run_bitty(&["inspect", "config", "font.size", "--format", "jsonl"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "jsonl inspect must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"v\":1") && text.contains("\"command\":\"inspect\""),
        "jsonl must use the v1 envelope, got {text:?}"
    );
    assert_eq!(text.trim().lines().count(), 1);
}

#[test]
fn inspect_top_help_names_inspect() {
    // The top-level help advertises `inspect` alongside the other
    // subcommands (never requires an instance to print).
    let output = run_bitty(&["--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "top --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("inspect <target>"),
        "top help must advertise inspect, got {:?}",
        stdout(&output)
    );
}
