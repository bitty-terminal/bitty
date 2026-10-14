//! Binary-level `--mascot` / `--no-splash` proof (issue #1318, CTX-1217).
//!
//! `--mascot` prints the vendored Bittie pixel art (half-block cells, 32
//! columns by 16 rows) to stdout and exits 0 without touching config,
//! instances, or the plugin VM; it records the first-run splash marker
//! best-effort. `--no-splash` parses and is advertised in `--help`. These
//! tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty` with an
//! isolated `XDG_DATA_HOME`/`HOME` so the host marker state is never read
//! or written.
//!
//! The subprocess stdout is piped (never a TTY), so the unified CLI color
//! gate pins the escape-free monochrome render here; the truecolor render
//! is pinned by unit test (`mascot_render_...` in `src/tests.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (data home + HOME).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1217-{tag}-{}-{}",
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
    run_bitty_with_env(root, args, &[])
}

/// [`run_bitty`] plus extra `KEY=value` environment entries.
fn run_bitty_with_env(root: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_DATA_HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env_remove("COLUMNS")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("NO_COLOR")
        .env("TERM", "xterm-256color");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command
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
    // Piped stdout pins the monochrome half-block render: 16 rows of 32
    // cells, escape-free, with the sprite's block glyphs present.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        16,
        "mascot prints 16 splash rows, got {}",
        lines.len()
    );
    for line in &lines {
        assert_eq!(line.chars().count(), 32, "splash row is 32 cells: {line:?}");
    }
    assert!(!text.contains('\u{1b}'), "piped mascot is escape-free");
    assert!(text.contains('\u{2580}'), "half-block art present");
    let marker = root.join("bitty").join("splash-shown");
    assert!(
        marker.exists(),
        "--mascot records the splash marker best-effort"
    );
}

#[test]
fn mascot_flag_narrow_window_prints_one_line_fallback() {
    let root = scratch_root("narrow");
    let output = run_bitty_with_env(&root, &["--mascot"], &[("COLUMNS", "20")]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("too narrow"));
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
    // splash rows even with no marker present (deterministic CI).
    let root = scratch_root("headless");
    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "headless must exit 0, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout(&output).contains('\u{2580}'),
        "headless never splashes"
    );
    assert!(
        !stdout(&output).contains("too narrow"),
        "headless never splashes"
    );
}
