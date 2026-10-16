//! Binary-level `--no-splash` proof plus the #2135 `--mascot` removal proof.
//!
//! `--no-splash` parses and is advertised in `--help`; the first-run splash
//! itself is untouched. `--mascot` was removed in 0.0.24: the flag is now an
//! unknown pre-`--` flag (rejected fail-closed, exit 2) and no help surface
//! advertises it. These tests drive the built `bitty` binary via
//! `CARGO_BIN_EXE_bitty` with an isolated `XDG_DATA_HOME`/`HOME` so the
//! host marker state is never read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (data home + HOME).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1265-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs `bitty <args>` with isolated data/config roots and no `COLUMNS`.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_DATA_HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env_remove("COLUMNS")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn mascot_flag_removed_rejected_as_unknown() {
    // #2135: `--mascot` no longer exists; unknown pre-`--` flags fail
    // closed with exit 2 and name the flag.
    let root = scratch_root("mascot-removed");
    let output = run_bitty(&root, &["--mascot"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--mascot must exit 2, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--mascot"),
        "rejection names the flag"
    );
}

#[test]
fn help_advertises_no_splash_without_mascot() {
    let root = scratch_root("help");
    let output = run_bitty(&root, &["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        !text.contains("--mascot"),
        "help must not list --mascot, got:\n{text}"
    );
    assert!(text.contains("--no-splash"), "help lists --no-splash");
}

#[test]
fn headless_with_fresh_data_root_stays_splash_free() {
    // Machine flows skip the splash: `--headless` stdout must not contain
    // the fallback line even with no marker present (deterministic CI).
    let root = scratch_root("headless");
    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "headless must exit 0, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout(&output).contains("too narrow"),
        "headless never splashes"
    );
}
