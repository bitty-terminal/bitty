//! Binary-level `--mascot` / `--no-splash` proof (issue #1318, CTX-0729).
//!
//! `--mascot` prints the vendored Bittie art to stdout and exits 0 without
//! touching config, instances, or the plugin VM; it records the first-run
//! splash marker best-effort. `--no-splash` parses and is advertised in
//! `--help`. These tests drive the built `bitty` binary via
//! `CARGO_BIN_EXE_bitty` with an isolated `XDG_DATA_HOME`/`HOME` so the
//! host marker state is never read or written.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (data home + HOME).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0729-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs `bitty <args>` with isolated data/config roots and no `COLUMNS`,
/// so the full art (not the narrow fallback) is expected.
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
fn mascot_flag_prints_art_exits_zero_and_records_marker() {
    let root = scratch_root("mascot");
    let output = run_bitty(&root, &["--mascot"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "mascot must exit 0, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout(&output);
    assert!(
        text.lines().count() > 1 && text.lines().count() <= 32,
        "mascot prints the multi-line art, got {} lines",
        text.lines().count()
    );
    assert!(text.is_ascii(), "mascot art is pure ASCII for pipe-safety");
    let marker = root.join("bitty").join("splash-shown");
    assert!(
        marker.exists(),
        "--mascot records the splash marker best-effort"
    );
}

#[test]
fn help_advertises_mascot_and_no_splash() {
    let root = scratch_root("help");
    let output = run_bitty(&root, &["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("--mascot"), "help lists --mascot");
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
