//! `bitty completion` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty completion`,
//! local class, Bash/Zsh/Fish/PowerShell/Nushell) with the stable `comp`
//! alias.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `completion` dispatches before config load and GUI startup, so every case
//! here is headless-safe: no display, no instance, no plugin VM.

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
fn every_shell_emits_a_script() {
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        let out = run_bitty(&["completion", shell]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "shell {shell}: {}",
            stderr(&out)
        );
        let script = stdout(&out);
        assert!(script.contains("bitty"), "shell {shell} script names bitty");
        assert!(
            script.contains("version"),
            "shell {shell} completes version"
        );
        assert!(
            script.contains("completion"),
            "shell {shell} completes itself"
        );
    }
}

#[test]
fn comp_alias_behaves_identically() {
    let canonical = run_bitty(&["completion", "bash"]);
    let alias = run_bitty(&["comp", "bash"]);
    assert_eq!(alias.status.code(), Some(0));
    assert_eq!(stdout(&alias), stdout(&canonical));
    let help = run_bitty(&["comp", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("bitty comp"));
}

#[test]
fn completion_help_names_shells() {
    let out = run_bitty(&["completion", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        assert!(text.contains(shell), "help names {shell}");
    }
}

#[test]
fn completion_rejects_usage_errors() {
    for args in [
        vec!["completion"],
        vec!["completion", "tcl"],
        vec!["completion", "bash", "zsh"],
        vec!["completion", "bash", "--bogus"],
        vec!["completion", "--"],
        vec!["--socket", "/tmp/x.sock", "completion", "bash"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must fail closed: {}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "no stdout script on usage error");
    }
}
