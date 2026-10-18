//! Binary-level first-start config fallback proof (CTX-1339, issue #2225).
//!
//! R-009 contract: on first start with missing or broken configuration Bitty
//! proceeds on the minimal built-in (`CoreDefaults`) configuration and reports
//! diagnostics; later failures retain the last good plan. These tests drive
//! the built `bitty` binary with an isolated `XDG_CONFIG_HOME` and the
//! deterministic `--headless` path (no display, instance, or plugin VM).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (config + HOME + state + data).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1339-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty").join("profiles")).expect("config dir");
    dir
}

/// Writes `$XDG_CONFIG_HOME/bitty/init.lua` with the given body.
fn write_init(root: &Path, body: &str) {
    std::fs::write(root.join("bitty").join("init.lua"), body).expect("write init.lua");
}

/// Writes `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` with the given body.
fn write_profile(root: &Path, name: &str, body: &str) {
    std::fs::write(
        root.join("bitty")
            .join("profiles")
            .join(format!("{name}.lua")),
        body,
    )
    .expect("write profile");
}

/// Runs `bitty <args>` under the isolated root.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    run_bitty_with_env(root, args, &[])
}

/// Runs `bitty <args>` under the isolated root with extra environment values.
fn run_bitty_with_env(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE");
    for (key, value) in envs {
        command.env(key, value);
    }
    command
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
fn first_start_with_broken_config_proceeds_with_builtin_defaults() {
    let root = scratch_root("broken");
    write_init(&root, "return { font = }\n");

    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "first start must proceed on built-ins, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("invalid config file"),
        "the load diagnostic must be reported, stderr={err:?}"
    );
    assert!(
        err.contains("first start") && err.contains("built-in defaults"),
        "the fallback must be reported, stderr={err:?}"
    );
    let text = stdout(&output);
    assert!(
        text.contains("source=default") && text.contains("file=(none)"),
        "built-in defaults must be in effect, stdout={text:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn valid_config_startup_is_unchanged() {
    let root = scratch_root("valid");
    write_init(&root, "return { appearance = { theme = \"dracula\" } }\n");

    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "valid config must start, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("theme 'dracula'") && text.contains("source=file"),
        "the file layer must still win, stdout={text:?}"
    );
    assert!(
        !stderr(&output).contains("built-in defaults"),
        "a valid config must not report the fallback"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_explicit_config_still_fails_closed() {
    let root = scratch_root("explicit-missing");
    let missing = root.join("missing.lua");
    let missing = missing.to_str().expect("utf-8 scratch path");

    let output = run_bitty(&root, &["--config", missing, "--headless"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an explicitly missing --config must stay fail-closed, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("not found"), "stderr={err:?}");
    assert!(
        !err.contains("built-in defaults"),
        "an explicit missing path is not a first-start fallback, stderr={err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_bitty_config_env_still_fails_closed() {
    let root = scratch_root("env-missing");
    let missing = root.join("missing.lua");
    let missing = missing.to_str().expect("utf-8 scratch path");

    let output = run_bitty_with_env(&root, &["--headless"], &[("BITTY_CONFIG", missing)]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an explicitly missing BITTY_CONFIG must stay fail-closed, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("BITTY_CONFIG"), "stderr={err:?}");
    assert!(err.contains("not found"), "stderr={err:?}");
    assert!(
        !err.contains("built-in defaults"),
        "an explicit missing path is not a first-start fallback, stderr={err:?}"
    );
    assert!(
        !stdout(&output).contains("bitty headless:"),
        "startup must not proceed with built-ins"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_theme_with_valid_file_stays_fail_closed() {
    let root = scratch_root("invalid-theme");
    write_init(&root, "return { appearance = { theme = \"dracula\" } }\n");

    let output = run_bitty(&root, &["--theme", "not-a-theme", "--headless"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown --theme is a CLI usage error, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("invalid --theme"), "stderr={err:?}");
    assert!(err.contains("appearance.theme"), "stderr={err:?}");
    assert!(
        !err.contains("built-in defaults"),
        "a valid user file must not absorb a CLI error, stderr={err:?}"
    );
    assert!(
        !stdout(&output).contains("bitty headless:"),
        "startup must not proceed with built-ins"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_profile_with_valid_file_stays_fail_closed() {
    let root = scratch_root("invalid-profile");
    write_init(&root, "return { appearance = { theme = \"dracula\" } }\n");
    // Passes profile-layer parsing/validation but fails the merged outline
    // contrast contract (AC-1) on the default dark background.
    write_profile(
        &root,
        "bad",
        "return { decoration = { border_color_focused = \"#000000\" } }\n",
    );

    let output = run_bitty(&root, &["--profile", "bad", "--headless"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an invalid profile must stay fail-closed, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("invalid profile"), "stderr={err:?}");
    assert!(err.contains("bad.lua"), "stderr={err:?}");
    assert!(
        !err.contains("init.lua"),
        "the valid user file must not be blamed, stderr={err:?}"
    );
    assert!(
        !err.contains("built-in defaults"),
        "a valid user file must not absorb a profile error, stderr={err:?}"
    );
    assert!(
        !stdout(&output).contains("bitty headless:"),
        "startup must not proceed with built-ins"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_config_startup_is_unchanged() {
    let root = scratch_root("missing");

    let output = run_bitty(&root, &["--headless"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a missing default-path config already yields defaults, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("source=default") && text.contains("file=(none)"),
        "missing config must keep the default path, stdout={text:?}"
    );
    assert!(
        !stderr(&output).contains("built-in defaults"),
        "a missing default config is not a fallback"
    );

    let _ = std::fs::remove_dir_all(&root);
}
