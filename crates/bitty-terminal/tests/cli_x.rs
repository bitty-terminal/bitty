//! `bitty x` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty x`,
//! extension class, collision-free qualified route). This slice resolves the
//! route from static manifests and fails closed without executing plugin
//! code: unknown plugins and execution attempts are plugin errors (exit 4),
//! parse failures are usage errors (exit 2).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `x` dispatches before config load and GUI startup from static manifests
//! only, so every case here is headless-safe: no display, no instance, and
//! no plugin VM is ever loaded.

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

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
fn x_help_lists_installed_plugins() {
    let out = run_bitty(&["x", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Installed plugins"), "help: {text}");
    assert!(text.contains("bitty-terminal."), "bundled set: {text}");
}

#[test]
fn x_plugin_help_shows_static_commands() {
    let out = run_bitty(&["x", "bitty-terminal.workspace", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("bitty-terminal.workspace"));
}

#[test]
fn x_unknown_plugin_is_plugin_error() {
    let out = run_bitty(&["x", "nope.missing", "render"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("nope.missing"));
    let out = run_bitty(&["x", "nope.missing", "render", "--format", "json"]);
    assert_eq!(out.status.code(), Some(4));
    let text = stdout(&out);
    assert!(text.contains("\"v\":1"), "envelope: {text}");
    assert!(text.contains("\"command\":\"x\""), "envelope: {text}");
    assert!(text.contains("\"ok\":false"), "envelope: {text}");
}

#[test]
fn x_execution_attempt_is_plugin_error_without_vm() {
    let out = run_bitty(&["x", "bitty-terminal.workspace", "open"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("bitty-terminal.workspace"));
}

#[test]
fn x_rejects_usage_errors() {
    for args in [
        vec!["x"],
        vec!["x", "bitty-terminal.workspace"],
        vec!["x", "bitty-terminal.workspace", "open", "--"],
        vec!["x", "bitty-terminal.workspace", "open", "--format", "yaml"],
        vec!["--socket", "/tmp/x.sock", "x", "--help"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must fail closed: {}",
            stderr(&out)
        );
    }
}
