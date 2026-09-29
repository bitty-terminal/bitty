//! CR-APP-01: unknown CLI flags are usage errors (CTX-0207).
//!
//! A typo'd flag must never be spawned as a program: `bitty --bogus`
//! rejects with usage on stderr and exit code 2. Only post-`--` tokens
//! may name a program. These tests drive the built `bitty` binary via
//! `CARGO_BIN_EXE_bitty`; the rejection dispatches before config load,
//! runtime creation, and GUI startup, so every case here is headless-safe.

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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unknown_long_flag_is_usage_error_with_exit_2() {
    let output = run_bitty(&["--bogus"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown flag must be a usage error (exit 2), stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("unknown flag") && err.contains("--bogus"),
        "stderr must name the rejected flag, got {err:?}"
    );
    assert!(
        err.contains("Usage:"),
        "stderr must print usage, got {err:?}"
    );
}

#[test]
fn unknown_short_flag_is_usage_error_with_exit_2() {
    let output = run_bitty(&["-x"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown short flag must be a usage error (exit 2), stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("unknown flag") && err.contains("-x"),
        "stderr must name the rejected flag, got {err:?}"
    );
    assert!(
        err.contains("Usage:"),
        "stderr must print usage, got {err:?}"
    );
}

#[test]
fn unknown_flag_after_known_flags_still_rejects() {
    // Known flags parse first, but the typo still fails closed (exit 2)
    // instead of spawning `--bogus` as a program.
    let output = run_bitty(&["--headless", "--bogus"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown flag must be a usage error (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("Usage:"),
        "stderr must print usage, got {:?}",
        stderr(&output)
    );
}
