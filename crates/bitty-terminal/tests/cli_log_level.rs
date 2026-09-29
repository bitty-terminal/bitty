//! Binary-level stderr log-level discipline proof (CTX-0482, issue #763).
//!
//! Startup diagnostics used to print unconditionally (`eprintln!`), so
//! `--log-level` only gated per-frame tick lines. The discipline now is:
//! `Error < Warn` always emit at their level, info-class startup lines
//! (theme/keymaps/layout/spawn/IPC/plugin summaries) require `info` or
//! above, and the default (`warn`) stays quiet.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`
//! with an isolated `XDG_CONFIG_HOME`/`HOME`; `--headless` dispatches
//! without a display, instance, or plugin VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// A program that can never spawn: the path does not exist.
const BROKEN_PROGRAM: &str = "/nonexistent-ctx0482/bitty-broken-shell";

/// Fresh isolated scratch root.
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0482-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs `bitty <args>` under the isolated root.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_LOG")
        .env_remove("RUST_LOG")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn startup_info_lines_require_info_level() {
    // Platform-independent info markers: the spawn-adjacent lines stay in a
    // unix-only block below because the default shell chain falls back to
    // `/bin/sh`, which does not exist on Windows.
    let markers = [
        "bitty: theme '",
        "bitty: keymaps resolved",
        "bitty: layout installed",
        "bitty: effective program",
    ];
    let root = scratch_root("quiet-default");
    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(output.status.code(), Some(0));
    let err = stderr(&output);
    for marker in markers {
        assert!(
            !err.contains(marker),
            "default (warn) must stay quiet, found {marker:?} in {err:?}"
        );
    }

    let output = run_bitty(&root, &["--headless", "--log-level", "info"]);
    assert_eq!(output.status.code(), Some(0));
    let err = stderr(&output);
    for marker in markers {
        assert!(
            err.contains(marker),
            "--log-level info must print {marker:?}, stderr={err:?}"
        );
    }
    #[cfg(unix)]
    assert!(
        err.contains("bitty: PTY shell spawned"),
        "--log-level info must print the spawn summary, stderr={err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn log_level_error_suppresses_warnings_but_not_the_run() {
    // The fail-soft spawn path still ticks (exit 0) at `--log-level error`;
    // only its warning line is gated away (the operator asked for errors).
    let root = scratch_root("error");
    let output = run_bitty(
        &root,
        &["--headless", "--log-level", "error", "--", BROKEN_PROGRAM],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "fail-soft default must still exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("PTY spawn failed"),
        "warnings are below error level, stderr={:?}",
        stderr(&output)
    );

    // Positive control: the default warn level keeps the same warning.
    let output = run_bitty(&root, &["--headless", "--", BROKEN_PROGRAM]);
    assert!(
        stderr(&output).contains("PTY spawn failed"),
        "default warn must surface the spawn failure, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}
