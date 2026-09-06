//! `bitty list` end-to-end dispatch proofs (CTX-0172).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`list` introspection) as
//! refined by `docs/specifications/cli-contract-rfc.md` (mixed class, envelope
//! v1, exit codes, `ls` alias).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `list` dispatches before config load and GUI startup, so every case here
//! is headless-safe: no display, no plugin VM, and (for themes/plugins) no
//! instance is touched. `instances` cases use isolated `XDG_RUNTIME_DIR` and
//! `BITTY_SOCKET` fixtures, never the developer's live sockets.

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BITTY_BIN);
    cmd.args(args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn run_bitty_isolated(args: &[&str]) -> Output {
    // Isolate instance discovery from the developer's live sockets:
    // empty temp runtime dir + no advisory socket/instance ids.
    let dir = std::env::temp_dir().join(format!(
        "bitty-list-isolated-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_string_lossy().to_string();
    let out = Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_RUNTIME_DIR", &dir_str)
        .env_remove("BITTY_SOCKET")
        .env_remove("BITTY_INSTANCE_ID")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"));
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse_stdout_json(text: &str) -> serde_json_like::Doc {
    serde_json_like::parse(text)
}

/// Minimal JSON field extractor without a new dependency: finds
/// `"key":value` shapes emitted by the envelope (whitespace-free compact
/// form). Panics with the full text on mismatch so failures are actionable.
mod serde_json_like {
    #[derive(Debug)]
    pub struct Doc(String);
    pub fn parse(text: &str) -> Doc {
        let trimmed = text.trim();
        assert!(
            trimmed.starts_with('{') && trimmed.ends_with('}'),
            "stdout must be one JSON object, got {text:?}"
        );
        Doc(trimmed.to_string())
    }
    impl Doc {
        pub fn contains(&self, needle: &str) -> bool {
            self.0.contains(needle)
        }
        pub fn text(&self) -> &str {
            &self.0
        }
    }
}

#[test]
fn list_help_exits_zero() {
    let output = run_bitty(&["list", "--help"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty list") && text.contains("themes|plugins|instances"),
        "list --help must describe kinds, got {text:?}"
    );
}

#[test]
fn list_missing_kind_is_usage_error() {
    let output = run_bitty(&["list"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty list` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("missing <kind>"),
        "diagnostic must name the missing kind, got {:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "usage errors must not emit stdout JSON, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_unknown_kind_is_usage_error() {
    let output = run_bitty(&["list", "fonts"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown kind must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("unknown kind"),
        "diagnostic must name the kind error, got {:?}",
        stderr(&output)
    );
}

#[test]
fn list_themes_table_contains_builtin_dark() {
    let output = run_bitty(&["list", "themes"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list themes must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty-dark") && text.contains("#1e1e2e"),
        "themes table must carry the built-in dark preset, got {text:?}"
    );
}

#[test]
fn list_themes_json_envelope_shape() {
    let output = run_bitty(&["list", "themes", "--format", "json"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list themes --format json must exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"v\":1"),
        "envelope version: {:?}",
        doc.text()
    );
    assert!(
        doc.contains("\"command\":\"list\""),
        "command field: {:?}",
        doc.text()
    );
    assert!(doc.contains("\"ok\":true"), "ok field: {:?}", doc.text());
    assert!(
        doc.contains("\"kind\":\"themes\""),
        "kind field: {:?}",
        doc.text()
    );
    assert!(doc.contains("bitty-dark"), "theme name: {:?}", doc.text());
    // Stderr must not corrupt stdout JSON: stdout is exactly one line object.
    assert_eq!(
        stdout(&output).trim().lines().count(),
        1,
        "json must be one value, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_themes_jsonl_is_single_line_envelope() {
    let output = run_bitty(&["list", "themes", "--format", "jsonl"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"v\":1"));
    assert!(doc.contains("\"kind\":\"themes\""));
}

#[test]
fn list_plugins_table_contains_bundled() {
    let output = run_bitty(&["list", "plugins"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list plugins must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty-terminal.tabs"),
        "plugins table must carry bundled ids, got {text:?}"
    );
}

#[test]
fn list_plugins_json_count_is_ten() {
    let output = run_bitty(&["list", "plugins", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"kind\":\"plugins\""));
    assert!(doc.contains("\"count\":10"), "count: {:?}", doc.text());
    assert!(doc.contains("bitty-terminal.palette"));
}

#[test]
fn list_instances_empty_dir_is_success_with_count_zero() {
    let output = run_bitty_isolated(&["list", "instances"]);
    if cfg!(windows) {
        // Instance discovery is Unix-only; on Windows the binary reports
        // Unavailable (exit 6) by design (see `list.rs` non-Unix stub).
        assert_eq!(
            output.status.code(),
            Some(6),
            "windows empty instances must exit 6, stderr={:?}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("instance discovery is unavailable on this platform"),
            "diagnostic must name the platform gate, got {:?}",
            stderr(&output)
        );
        let json = run_bitty_isolated(&["list", "instances", "--format", "json"]);
        assert_eq!(
            json.status.code(),
            Some(6),
            "windows empty instances json must exit 6, stderr={:?}",
            stderr(&json)
        );
        let doc = parse_stdout_json(&stdout(&json));
        assert!(doc.contains("\"ok\":false"), "ok field: {:?}", doc.text());
        assert!(
            doc.contains("\"class\":\"Unavailable\""),
            "class: {:?}",
            doc.text()
        );
        assert!(
            doc.contains("unavailable on this platform"),
            "message: {:?}",
            doc.text()
        );
        return;
    }
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty instances must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("(no instances"),
        "empty table must hint, got {:?}",
        stdout(&output)
    );
    let json = run_bitty_isolated(&["list", "instances", "--format", "json"]);
    assert_eq!(json.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&json));
    assert!(doc.contains("\"kind\":\"instances\""));
    assert!(doc.contains("\"count\":0"), "count: {:?}", doc.text());
}

#[test]
fn list_alias_ls_matches_list_for_themes() {
    let canonical = run_bitty(&["list", "themes", "--format", "json"], &[]);
    let alias = run_bitty(&["ls", "themes", "--format", "json"], &[]);
    assert_eq!(canonical.status.code(), Some(0));
    assert_eq!(alias.status.code(), Some(0));
    let canonical_doc = parse_stdout_json(&stdout(&canonical));
    let alias_doc = parse_stdout_json(&stdout(&alias));
    assert!(canonical_doc.contains("\"command\":\"list\""));
    assert!(alias_doc.contains("\"command\":\"ls\""));
    // Same payload apart from the spelling field.
    let strip = |s: &str| {
        s.replace("\"command\":\"list\"", "\"command\":\"X\"")
            .replace("\"command\":\"ls\"", "\"command\":\"X\"")
    };
    assert_eq!(
        strip(&stdout(&canonical)),
        strip(&stdout(&alias)),
        "alias must produce identical payload apart from command spelling"
    );
}

#[test]
fn list_stray_separator_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn list_unknown_format_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--format", "yaml"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown format must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "usage errors must not emit stdout, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_extra_positional_is_usage_error() {
    let output = run_bitty(&["list", "themes", "extra"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "extra positional must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn list_socket_for_non_instances_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--socket", "/tmp/x.sock"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--socket with themes must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
// Portable: Unix probes the missing path, Windows hits the platform stub;
// both report Unavailable (exit 6) with an ok:false envelope.
fn list_explicit_missing_socket_is_runtime_unavailable() {
    let output = run_bitty_isolated(&[
        "list",
        "instances",
        "--socket",
        "/tmp/bitty-list-definitely-missing-9f8e7d.sock",
        "--format",
        "json",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing explicit socket must be exit 6, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"ok\":false"), "ok field: {:?}", doc.text());
    assert!(
        doc.contains("\"class\":\"Unavailable\""),
        "class: {:?}",
        doc.text()
    );
}
