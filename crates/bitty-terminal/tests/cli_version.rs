//! `bitty version` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty version`,
//! local class, stable table form plus `--format json`).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `version` dispatches before config load and GUI startup, so every case
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
fn version_table_form_is_rfc_shape() {
    let out = run_bitty(&["version"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out).trim().to_string();
    assert!(text.starts_with("bitty "), "table form: {text}");
    assert!(
        text.contains('(') && text.ends_with(')'),
        "channel/commit: {text}"
    );
    assert!(!text.contains('\n'), "single line: {text}");
}

#[test]
fn version_flag_aliases_match_subcommand() {
    let via_word = stdout(&run_bitty(&["version"]));
    for flag in ["--version", "-V"] {
        let out = run_bitty(&[flag]);
        assert_eq!(out.status.code(), Some(0), "flag {flag}: {}", stderr(&out));
        assert_eq!(stdout(&out), via_word, "alias {flag} matches");
    }
}

#[test]
fn version_json_envelope_is_versioned() {
    for args in [
        &["version", "--format", "json"][..],
        &["--format", "json", "version"][..],
    ] {
        let out = run_bitty(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "args {args:?}: {}",
            stderr(&out)
        );
        let text = stdout(&out);
        assert!(text.contains("\"v\":1"), "envelope: {text}");
        assert!(text.contains("\"command\":\"version\""), "envelope: {text}");
        assert!(text.contains("\"ok\":true"), "envelope: {text}");
        assert!(text.contains("\"version\":\""), "semver field: {text}");
        assert!(text.contains("\"channel\":\""), "channel field: {text}");
        assert!(text.contains("\"commit\":\""), "commit field: {text}");
        assert!(stderr(&out).is_empty(), "stdout JSON never corrupted");
    }
}

#[test]
fn version_jsonl_matches_json() {
    let json = stdout(&run_bitty(&["version", "--format", "json"]));
    let jsonl = run_bitty(&["version", "--format", "jsonl"]);
    assert_eq!(jsonl.status.code(), Some(0));
    assert_eq!(stdout(&jsonl), json);
}

#[test]
fn version_help_needs_no_instance() {
    let out = run_bitty(&["version", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("bitty version"));
}

#[test]
fn version_rejects_usage_errors() {
    for args in [
        vec!["version", "extra"],
        vec!["version", "--format", "yaml"],
        vec!["version", "--"],
        vec!["version", "--bogus"],
        vec!["--socket", "/tmp/x.sock", "version"],
        vec!["version", "--socket=/tmp/x.sock"],
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
