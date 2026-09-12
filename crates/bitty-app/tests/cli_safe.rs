//! Binary-level `--safe` app-wiring proof (CTX-0346, issue #575).
//!
//! Pre-fix, the app-level `--safe` flag only gated the plugin VM and was
//! never consulted by `load_merged_config`, so a user config file still won:
//! `bitty config check --safe --config <hostile.lua>` reported the file's
//! decoration geometry and outline pair. `--safe` must instead select the
//! built-in safe effective config (`0/0/1/0/0`, opaque `#FFFFFF`/`#808080`)
//! regardless of any external layer, per spec rule 5
//! (`bitty-docs/docs/specifications/workspace-compositor.md`) and
//! RFC-0001/OQ-039, and must never abort on a hostile/invalid user config
//! (R-009 / P0-AC-019).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`
//! with an isolated `XDG_CONFIG_HOME`; `config check` and `--headless`
//! dispatch without a display, instance, or plugin VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Hostile user file: non-default decoration geometry and a valid-but-wrong
/// outline pair the safe path must ignore.
const HOSTILE_LUA: &str = r##"return {
  decoration = {
    gaps_in = 5,
    gaps_out = 7,
    border = 3,
    radius = 4,
    content_inset = 2,
    border_color_focused = "#33CCFF",
    border_color_idle = "#595959AA",
  },
}
"##;

/// Fresh isolated scratch root (config + HOME + hostile file).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0346-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes `$XDG_CONFIG_HOME/bitty/init.lua` with the given body.
fn write_config(xdg: &Path, body: &str) {
    let dir = xdg.join("bitty");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("init.lua"), body).expect("write init.lua");
}

/// Runs `bitty <args>` under the isolated root, removing config env so the
/// explicit `--config` (or defaults) is the only layer in play.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// One `config check` row value for `key`, e.g. `decoration.gaps_in`.
fn row(text: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn safe_config_check_ignores_hostile_file_decoration() {
    let root = scratch_root("check");
    let hostile = root.join("hostile.lua");
    std::fs::write(&hostile, HOSTILE_LUA).expect("write hostile.lua");
    let hostile_str = hostile.to_str().expect("utf8 hostile path");

    let output = run_bitty(
        &root,
        &["config", "check", "--safe", "--config", hostile_str],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe config check must succeed, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    // Safe geometry `0/0/1/0/0` and opaque pair, sourced from defaults.
    for (key, value) in [
        ("decoration.gaps_in", "0 (default)"),
        ("decoration.gaps_out", "0 (default)"),
        ("decoration.border", "1 (default)"),
        ("decoration.radius", "0 (default)"),
        ("decoration.content_inset", "0 (default)"),
        ("decoration.border_color_focused", "#FFFFFF (default)"),
        ("decoration.border_color_idle", "#808080 (default)"),
    ] {
        assert_eq!(
            row(&text, key),
            value,
            "--safe must force {key}; output:\n{text}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn non_safe_config_check_keeps_hostile_file_values() {
    // Negative control: without `--safe` the same file still wins, so the
    // fix cannot degrade normal user-config behavior.
    let root = scratch_root("nonsafe");
    let hostile = root.join("hostile.lua");
    std::fs::write(&hostile, HOSTILE_LUA).expect("write hostile.lua");
    let hostile_str = hostile.to_str().expect("utf8 hostile path");

    let output = run_bitty(&root, &["config", "check", "--config", hostile_str]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    // The non-safe path reports the file's values with a `file: <path>`
    // source; only the leading value is asserted (the scratch path varies).
    assert!(
        row(&text, "decoration.gaps_in").starts_with("5 (file: "),
        "non-safe must keep the file value, output:\n{text}"
    );
    assert!(
        row(&text, "decoration.border_color_focused").starts_with("#33CCFF (file: "),
        "non-safe must keep the file outline color, output:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_config_check_startup_ignores_invalid_explicit_config() {
    // R-009/P0-AC-019: safe startup must never abort on a hostile user
    // config, including an out-of-range value and a missing explicit file.
    let root = scratch_root("invalid");
    let bad = root.join("bad.lua");
    std::fs::write(&bad, "return { decoration = { gaps_in = 999 } }\n").expect("write bad.lua");
    let bad_str = bad.to_str().expect("utf8 bad path");

    let output = run_bitty(&root, &["config", "check", "--safe", "--config", bad_str]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe mode must not evaluate the invalid file, stderr={:?}",
        stderr(&output)
    );
    assert_eq!(row(&stdout(&output), "decoration.gaps_in"), "0 (default)");

    // A missing `--config` is also an external layer safe mode never reads.
    let missing = root.join("does-not-exist.lua");
    let missing_str = missing.to_str().expect("utf8 missing path");
    let missing_output = run_bitty(
        &root,
        &["config", "check", "--safe", "--config", missing_str],
    );
    assert_eq!(
        missing_output.status.code(),
        Some(0),
        "safe mode must not fail on a missing explicit config, stderr={:?}",
        stderr(&missing_output)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_headless_startup_succeeds_with_hostile_config() {
    // End-to-end binary wiring: `--safe --headless` with a hostile config
    // must reach the headless smoke with the safe config selected.
    let root = scratch_root("headless");
    write_config(&root, HOSTILE_LUA);
    let output = run_bitty(
        &root,
        &[
            "--safe",
            "--headless",
            "--config",
            root.join("bitty/init.lua")
                .to_str()
                .expect("utf8 init path"),
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless startup must exit 0, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}
