//! `bitty dev` end-to-end dispatch proofs (CTX-0174).
//!
//! Canonical: `bitty-docs/docs/interfaces/cli.md` (`dev` diagnostics) as
//! refined by `docs/specifications/cli-contract-rfc.md` (`bitty dev` mixed
//! class, local-only slice, envelope v1, exit codes) with instrumentation
//! scopes owned by `docs/specifications/devtools-rfc.md`.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `dev` dispatches before config load and GUI startup, so every case here
//! is headless-safe: no display, no plugin VM, no instance is touched.
//! Each invocation spawns with a hard timeout (spawn + poll + kill by PID,
//! no shell, no pipes) so a hung probe can never stall the suite.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Hard timeout per invocation (startup trace probes GPU briefly; 30s ample).
const DEV_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawns `BITTY_BIN args...` and waits up to [`DEV_TIMEOUT`], killing by
/// PID on deadline. No shell, no pipes beyond captured stdio.
fn spawn_dev(args: &[&str], env_extra: &[(&str, &str)]) -> Output {
    use std::process::Stdio;
    let mut cmd = Command::new(BITTY_BIN);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_extra {
        cmd.env(key, value);
    }
    // Isolate instance discovery: dev is local-only and must never depend
    // on the developer's live sockets.
    cmd.env_remove("BITTY_SOCKET");
    cmd.env_remove("BITTY_INSTANCE_ID");
    let mut child = cmd.spawn().expect("spawn bitty dev");
    let deadline = Instant::now() + DEV_TIMEOUT;
    loop {
        match child.try_wait().expect("poll dev") {
            Some(_) => return child.wait_with_output().expect("collect dev output"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bitty dev timed out after {DEV_TIMEOUT:?} (args={args:?})");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Stdout as UTF-8 (dev output is ASCII; lossy is fine).
fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Stderr as UTF-8.
fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts a versioned `dev` success envelope on stdout (single JSON value).
fn assert_dev_envelope(stdout: &str, verb: &str) {
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout must be one JSON object, got {stdout:?}"
    );
    assert!(trimmed.contains("\"v\":1"), "version: {stdout:?}");
    assert!(
        trimmed.contains("\"command\":\"dev\""),
        "command: {stdout:?}"
    );
    assert!(trimmed.contains("\"ok\":true"), "ok flag: {stdout:?}");
    assert!(
        trimmed.contains(&format!("\"verb\":\"{verb}\"")),
        "verb {verb}: {stdout:?}"
    );
    assert_eq!(
        trimmed.lines().count(),
        1,
        "stdout must be one JSON value, got {stdout:?}"
    );
}

#[test]
fn dev_help_exits_zero_and_names_verbs() {
    let output = spawn_dev(&["dev", "--help"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "dev --help must exit 0, stderr={:?}",
        stderr_text(&output)
    );
    let text = stdout_text(&output);
    for token in ["trace", "capture", "dump", "overlay"] {
        assert!(text.contains(token), "help must name {token}: {text:?}");
    }
}

#[test]
fn dev_global_help_flag_routes_to_dev_help() {
    let output = spawn_dev(&["--help", "dev"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout_text(&output).contains("bitty dev"));
}

#[test]
fn bare_dev_is_usage_error() {
    let output = spawn_dev(&["dev"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty dev` must be UsageError (exit 2)"
    );
    assert!(
        stderr_text(&output).contains("missing <verb>"),
        "diagnostic must name the missing verb"
    );
    assert!(
        output.stdout.is_empty(),
        "usage errors must not emit stdout"
    );
}

#[test]
fn unknown_verb_names_valid_set() {
    let output = spawn_dev(&["dev", "frob"], &[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("unknown verb"), "stderr={stderr:?}");
    assert!(
        stderr.contains("trace|capture|dump|overlay"),
        "stderr={stderr:?}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn trace_startup_table_reports_phases() {
    let output = spawn_dev(&["dev", "trace", "startup"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "trace startup must exit 0, stderr={:?}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    for phase in ["args_parse", "runtime_create", "first_frame_presented"] {
        assert!(stdout.contains(phase), "missing {phase}: {stdout:?}");
    }
    assert!(stdout.contains("verdict:"), "verdict: {stdout:?}");
}

#[test]
fn trace_startup_json_envelope() {
    let output = spawn_dev(&["dev", "trace", "startup", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "trace");
    assert!(stdout.contains("\"phases\":"), "phases: {stdout:?}");
    assert!(stdout.contains("\"total_ms\":"), "total: {stdout:?}");
}

#[test]
fn trace_latency_table_and_json() {
    let output = spawn_dev(&["dev", "trace", "latency", "--iterations", "3"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "trace latency must exit 0, stderr={:?}",
        stderr_text(&output)
    );
    assert!(
        stdout_text(&output).contains("latency"),
        "summary: {:?}",
        stdout_text(&output)
    );
    let output = spawn_dev(
        &[
            "dev",
            "trace",
            "latency",
            "--iterations",
            "3",
            "--format",
            "json",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "trace");
    assert!(stdout.contains("\"p50_ms\":"), "p50: {stdout:?}");
}

#[test]
fn trace_latency_bounds_fail_closed() {
    for args in [
        vec!["dev", "trace", "latency", "--iterations", "0"],
        vec!["dev", "trace", "latency", "--iterations", "1001"],
        vec!["dev", "trace", "latency", "--iterations", "abc"],
        vec!["dev", "trace", "startup", "--iterations", "5"],
    ] {
        let output = spawn_dev(&args, &[]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "want exit 2 for {args:?}, stderr={:?}",
            stderr_text(&output)
        );
        assert!(output.stdout.is_empty(), "no stdout on usage error");
    }
}

#[test]
fn capture_table_reports_hash() {
    for layout in ["single", "split", "stack", "overlay"] {
        let output = spawn_dev(&["dev", "capture", "--layout", layout], &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "capture {layout} must exit 0, stderr={:?}",
            stderr_text(&output)
        );
        let stdout = stdout_text(&output);
        assert!(stdout.contains("rgba_hash="), "hash: {stdout:?}");
        assert!(stdout.contains("frame="), "frame: {stdout:?}");
    }
}

#[test]
fn capture_is_deterministic_for_same_layout() {
    let first = stdout_text(&spawn_dev(&["dev", "capture", "--layout", "split"], &[]));
    let second = stdout_text(&spawn_dev(&["dev", "capture", "--layout", "split"], &[]));
    let hash = |text: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("surface:"))
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(hash(&first), hash(&second), "same layout must hash equal");
}

#[test]
fn capture_json_envelope() {
    let output = spawn_dev(&["dev", "capture", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "capture");
    assert!(stdout.contains("\"rgba_hash\":"), "hash: {stdout:?}");
    assert!(
        stdout.contains("\"layout\":\"single\""),
        "layout: {stdout:?}"
    );
}

#[test]
fn capture_bad_layout_fails_closed() {
    let output = spawn_dev(&["dev", "capture", "--layout", "diagonal"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_text(&output).contains("unknown --layout"));
    assert!(output.stdout.is_empty());
}

#[test]
fn dump_grid_table_and_json() {
    let output = spawn_dev(&["dev", "dump", "grid"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "dump grid must exit 0, stderr={:?}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("cursor="), "cursor: {stdout:?}");
    assert!(
        stdout.contains("bitty headless smoke"),
        "corpus: {stdout:?}"
    );
    let output = spawn_dev(&["dev", "dump", "grid", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "dump");
    assert!(stdout.contains("\"lines\":"), "lines: {stdout:?}");
    assert!(stdout.contains("\"cursor_row\":"), "cursor: {stdout:?}");
}

#[test]
fn dump_grid_bounds_fail_closed() {
    for args in [
        vec!["dev", "dump", "grid", "--rows", "0"],
        vec!["dev", "dump", "grid", "--cols", "257"],
        vec!["dev", "dump", "scene", "--rows", "4"],
    ] {
        let output = spawn_dev(&args, &[]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "want exit 2 for {args:?}, stderr={:?}",
            stderr_text(&output)
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn dump_scene_table_and_json() {
    let output = spawn_dev(&["dev", "dump", "scene"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(stdout.contains("damage regions:"), "damage: {stdout:?}");
    assert!(stdout.contains("leaf"), "allocations: {stdout:?}");
    let output = spawn_dev(&["dev", "dump", "scene", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "dump");
    assert!(stdout.contains("\"damage\":"), "damage: {stdout:?}");
}

#[test]
fn dump_atlas_table_and_json() {
    let output = spawn_dev(&["dev", "dump", "atlas"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "dump atlas must exit 0, stderr={:?}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("placements="), "atlas: {stdout:?}");
    assert!(stdout.contains("texels_len="), "texels: {stdout:?}");
    let output = spawn_dev(&["dev", "dump", "atlas", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "dump");
    assert!(stdout.contains("\"placements\":"), "placements: {stdout:?}");
}

#[test]
fn overlay_list_names_catalog() {
    let output = spawn_dev(&["dev", "overlay", "list"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    for name in ["damage", "cells", "glyphs", "images", "layout", "banner"] {
        assert!(stdout.contains(name), "missing {name}: {stdout:?}");
    }
    assert!(stdout.contains("deferred"), "deferred: {stdout:?}");
    let output = spawn_dev(&["dev", "overlay", "list", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "overlay");
    assert!(stdout.contains("\"overlays\":"), "catalog: {stdout:?}");
}

#[test]
fn overlay_show_headless_proofs() {
    for name in ["damage", "banner"] {
        let output = spawn_dev(&["dev", "overlay", "show", name], &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "overlay show {name} must exit 0, stderr={:?}",
            stderr_text(&output)
        );
        let stdout = stdout_text(&output);
        assert!(
            stdout.contains("available-headless") || stdout.contains(name),
            "proof: {stdout:?}"
        );
    }
    let output = spawn_dev(
        &["dev", "overlay", "show", "banner", "--format", "json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "overlay");
    assert!(stdout.contains("\"banner\":"), "banner: {stdout:?}");
}

#[test]
fn overlay_show_deferred_is_informational() {
    let output = spawn_dev(&["dev", "overlay", "show", "glyphs"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "deferred overlay stays informational (exit 0)"
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("deferred"), "reason: {stdout:?}");
    let output = spawn_dev(
        &["dev", "overlay", "show", "cells", "--format", "json"],
        &[],
    );
    let stdout = stdout_text(&output);
    assert!(
        stdout.contains("\"status\":\"deferred\""),
        "status: {stdout:?}"
    );
}

#[test]
fn overlay_show_errors_fail_closed() {
    for args in [
        vec!["dev", "overlay", "show"],
        vec!["dev", "overlay", "show", "nope"],
        vec!["dev", "overlay"],
        vec!["dev", "overlay", "list", "damage"],
        vec!["dev", "dump"],
    ] {
        let output = spawn_dev(&args, &[]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "want exit 2 for {args:?}, stderr={:?}",
            stderr_text(&output)
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn unknown_format_and_stray_separator_fail_closed() {
    let output = spawn_dev(&["dev", "capture", "--format", "yaml"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_text(&output).contains("unknown --format"));
    assert!(output.stdout.is_empty());
    let output = spawn_dev(&["dev", "capture", "--"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn remote_target_flags_are_rejected_local_only() {
    for args in [
        vec!["dev", "capture", "--socket", "/tmp/a.sock"],
        vec!["dev", "capture", "--socket=/tmp/a.sock"],
        vec!["--socket", "/tmp/a.sock", "dev", "capture"],
        vec!["dev", "dump", "grid", "--instance", "i:1"],
        vec!["--instance", "i:1", "dev", "trace", "startup"],
    ] {
        let output = spawn_dev(&args, &[]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "want exit 2 for {args:?}, stderr={:?}",
            stderr_text(&output)
        );
        let stderr = stderr_text(&output);
        assert!(stderr.contains("local-only"), "stderr={stderr:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn global_format_before_word_composes() {
    let output = spawn_dev(&["--format", "json", "dev", "capture"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_dev_envelope(&stdout_text(&output), "capture");
}

#[test]
fn program_named_dev_needs_escape_hatch() {
    // `bitty -- dev` must not dispatch the subcommand: it attempts a program
    // named `dev` (which fails to spawn and falls back to headless smoke).
    // `BITTY_HEADLESS=1` keeps the proof off the GUI path (no display here).
    let output = spawn_dev(&["--", "dev"], &[("BITTY_HEADLESS", "1")]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "escape hatch must not be a usage error, stderr={:?}",
        stderr_text(&output)
    );
    assert!(
        !stdout_text(&output).contains("\"command\":\"dev\""),
        "escape hatch must not emit the dev envelope"
    );
}

#[test]
fn jsonl_matches_json_single_line_shape() {
    let output = spawn_dev(&["dev", "capture", "--format", "jsonl"], &[]);
    let stdout = stdout_text(&output);
    assert_dev_envelope(&stdout, "capture");
}

#[test]
fn capture_split_layout_differs_from_single_headless() {
    // CTX-0220: the `dev` surface distinguishes WM states without a seat —
    // split-layout capture must render a different surface than single, and
    // the JSON envelope must carry the requested layout name.
    let single = spawn_dev(&["dev", "capture", "--layout", "single"], &[]);
    assert_eq!(single.status.code(), Some(0));
    let split = spawn_dev(&["dev", "capture", "--layout", "split"], &[]);
    assert_eq!(split.status.code(), Some(0));
    let surface = |output: &std::process::Output| {
        stdout_text(output)
            .lines()
            .find_map(|line| line.trim().strip_prefix("surface:").map(str::to_string))
            .unwrap_or_default()
    };
    let single_surface = surface(&single);
    let split_surface = surface(&split);
    assert!(!single_surface.is_empty(), "single must report a surface");
    assert!(!split_surface.is_empty(), "split must report a surface");
    assert_ne!(
        single_surface, split_surface,
        "split must render differently from single"
    );

    let json = spawn_dev(
        &["dev", "capture", "--layout", "split", "--format", "json"],
        &[],
    );
    assert_eq!(json.status.code(), Some(0));
    let stdout = stdout_text(&json);
    assert_dev_envelope(&stdout, "capture");
    assert!(
        stdout.contains("\"layout\":\"split\""),
        "envelope names the layout: {stdout:?}"
    );
}
