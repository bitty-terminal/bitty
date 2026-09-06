//! `bitty doctor` headless integration proof (CTX-0175).
//!
//! Local-class subcommand: requires no running instance and never loads a
//! plugin VM. Every invocation here runs the built binary via
//! `CARGO_BIN_EXE_bitty` with a hard timeout (spawn + poll + kill by PID,
//! no shell, no pipes) so a hung probe can never stall the suite. No GUI is
//! needed: `doctor` never creates a window.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Binary under test (compile-time proof the artifact is named `bitty`).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Hard timeout per invocation (doctor probes are 3s each; 20s is ample).
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(20);

/// Spawns `BITTY_BIN args...` and waits up to [`DOCTOR_TIMEOUT`], killing by
/// PID on deadline. No shell, no pipes beyond captured stdio.
fn spawn_doctor(args: &[&str], env_extra: &[(&str, &str)]) -> Output {
    use std::process::Stdio;
    let mut cmd = Command::new(BITTY_BIN);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_extra {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn bitty doctor");
    let deadline = Instant::now() + DOCTOR_TIMEOUT;
    loop {
        match child.try_wait().expect("poll doctor") {
            Some(_) => return child.wait_with_output().expect("collect doctor output"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bitty doctor timed out after {DOCTOR_TIMEOUT:?} (args={args:?})");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Unique scratch file per test (parallel-safe: pid + atomic counter).
fn scratch_file(tag: &str, contents: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path =
        std::env::temp_dir().join(format!("bitty-doctor-{tag}-{}-{n}.lua", std::process::id()));
    std::fs::write(&path, contents).expect("write scratch config");
    path
}

/// Stdout as UTF-8 (doctor output is ASCII + `—`/`…`; lossy is fine).
fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn table_output_lists_every_check_and_summary() {
    let output = spawn_doctor(&["doctor", "--no-color"], &[]);
    let stdout = stdout_text(&output);
    assert!(stdout.starts_with("bitty doctor — "), "header: {stdout}");
    for id in [
        "binary",
        "config",
        "keymaps",
        "fonts",
        "display",
        "gpu",
        "clipboard",
        "pty",
        "terminfo",
        "shell",
        "images",
        "plugins",
    ] {
        assert!(stdout.contains(id), "missing {id}: {stdout}");
    }
    assert!(stdout.contains("summary:"), "summary: {stdout}");
    // No-color run carries no ANSI escapes on stdout.
    assert!(!stdout.contains('\u{1b}'), "no-color violated: {stdout:?}");
    // Valid invocation never fails closed with usage.
    assert_ne!(
        output.status.code(),
        Some(2),
        "unexpected usage error: {stdout}"
    );
}

#[test]
fn json_envelope_is_versioned_and_never_corrupted() {
    let output = spawn_doctor(&["doctor", "--format", "json"], &[]);
    let stdout = stdout_text(&output);
    let trimmed = stdout.trim();
    assert!(trimmed.starts_with('{'), "envelope: {stdout}");
    assert!(trimmed.ends_with('}'), "envelope: {stdout}");
    assert!(trimmed.contains("\"v\":1"), "version: {stdout}");
    assert!(
        trimmed.contains("\"command\":\"doctor\""),
        "command: {stdout}"
    );
    assert!(trimmed.contains("\"result\":"), "result: {stdout}");
    assert!(trimmed.contains("\"checks\":"), "checks: {stdout}");
    // Exactly one JSON value on stdout (single line, no interleaved logs).
    assert_eq!(
        trimmed.lines().count(),
        1,
        "stdout must be one JSON value: {stdout}"
    );
    // Balanced braces as a cheap well-formedness probe.
    assert_eq!(
        trimmed.chars().filter(|c| *c == '{').count(),
        trimmed.chars().filter(|c| *c == '}').count(),
        "unbalanced: {stdout}"
    );
    // Exit code is one of the stable doctor codes (never usage here).
    let code = output.status.code().unwrap_or(-1);
    assert!(
        matches!(code, 0 | 1 | 3 | 5 | 8),
        "unstable exit {code}: {stdout}"
    );
}

#[test]
fn jsonl_matches_json_single_line_shape() {
    let output = spawn_doctor(&["doctor", "--format", "jsonl"], &[]);
    let stdout = stdout_text(&output);
    let trimmed = stdout.trim();
    assert!(trimmed.contains("\"v\":1"), "version: {stdout}");
    assert!(
        trimmed.contains("\"command\":\"doctor\""),
        "command: {stdout}"
    );
    assert_eq!(
        trimmed.lines().count(),
        1,
        "jsonl must be one value per invocation: {stdout}"
    );
}

#[test]
fn unknown_format_fails_closed_with_usage() {
    let output = spawn_doctor(&["doctor", "--format", "yaml"], &[]);
    assert_eq!(output.status.code(), Some(2), "want exit 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown --format"),
        "diagnostic on stderr: {stderr}"
    );
    // Stdout stays empty so no partial envelope escapes.
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on usage error"
    );
}

#[test]
fn extra_positional_fails_closed_with_usage() {
    let output = spawn_doctor(&["doctor", "extra"], &[]);
    assert_eq!(output.status.code(), Some(2), "want exit 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument"),
        "diagnostic on stderr: {stderr}"
    );
}

#[test]
fn format_equals_shape_composes() {
    let output = spawn_doctor(&["doctor", "--format=json"], &[]);
    let stdout = stdout_text(&output);
    assert!(stdout.contains("\"v\":1"), "envelope: {stdout}");
    assert_ne!(output.status.code(), Some(2), "valid shape: {stdout}");
}

#[test]
fn valid_config_file_keeps_doctor_recoverable() {
    let path = scratch_file("valid", "return {}\n");
    let arg = format!("--config={}", path.display());
    let output = spawn_doctor(&["doctor", "--format", "json", &arg], &[]);
    let stdout = stdout_text(&output);
    assert!(stdout.contains("\"id\":\"config\""), "config row: {stdout}");
    // A valid file never trips the config gate (exit 3).
    assert_ne!(
        output.status.code(),
        Some(3),
        "valid config must not fail gate: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn invalid_config_file_trips_config_gate() {
    let path = scratch_file("invalid", "return { font = }\n");
    let arg = format!("--config={}", path.display());
    let output = spawn_doctor(&["doctor", "--format", "json", &arg], &[]);
    let stdout = stdout_text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "invalid config must trip exit 3: {stdout}"
    );
    assert!(stdout.contains("\"ok\":false"), "ok flag: {stdout}");
    assert!(
        stdout.contains("\"class\":\"ConfigError\""),
        "error class: {stdout}"
    );
    assert!(stdout.contains("\"id\":\"config\""), "config row: {stdout}");
    assert!(
        stdout.contains("\"status\":\"fail\""),
        "config fail: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}
