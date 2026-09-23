//! `bitty cmd` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty cmd`, mixed
//! class, escape hatch). This slice validates the id and fails closed without
//! live registry dispatch (exit 6); parse failures are usage errors (exit 2).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `cmd` dispatches before config load and GUI startup, so every case here
//! is headless-safe: no display, no instance, no plugin VM.

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
fn well_formed_id_fails_closed_unavailable() {
    let out = run_bitty(&["cmd", "core.terminal.text"]);
    assert_eq!(out.status.code(), Some(6), "stderr: {}", stderr(&out));
    assert!(stdout(&out).is_empty(), "table diagnostics go to stderr");
    assert!(stderr(&out).contains("core.terminal.text"));
}

#[test]
fn colon_form_with_blob_fails_closed_unavailable() {
    let out = run_bitty(&[
        "cmd",
        "example.markdown:render",
        "--format",
        "json",
        "--",
        "{\"terminal_id\":\"t:4\"}",
    ]);
    assert_eq!(out.status.code(), Some(6), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("\"v\":1"), "envelope: {text}");
    assert!(text.contains("\"command\":\"cmd\""), "envelope: {text}");
    assert!(text.contains("\"ok\":false"), "envelope: {text}");
    assert!(text.contains("Unavailable"), "envelope: {text}");
}

#[test]
fn cmd_help_needs_no_instance() {
    let out = run_bitty(&["cmd", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("bitty cmd"));
}

#[test]
fn cmd_rejects_usage_errors() {
    for args in [
        vec!["cmd"],
        vec!["cmd", "foo"],
        vec!["cmd", "A.b"],
        vec!["cmd", "core.terminal.text", "extra"],
        vec!["cmd", "core.view.split", "--", "{}", "{}"],
        vec!["cmd", "core.view.split", "--", "printenv FOO"],
        vec!["cmd", "core.view.split", "--format", "yaml"],
        vec!["cmd", "--format", "json"],
        vec!["--socket", "/tmp/x.sock", "cmd", "core.view.split"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must fail closed: {}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "no stdout on usage error");
    }
}
