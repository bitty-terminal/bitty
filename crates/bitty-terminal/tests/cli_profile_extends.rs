//! Binary-level profile `extends` chain proof (CTX-0759, issue #1366).
//!
//! Pre-fix, `extends` was rejected as undeclared in every layer (exit 2)
//! and `resolve_profile_chain` was dead code, so single-parent profile
//! chains could not be declared. These tests drive the built `bitty`
//! binary via `CARGO_BIN_EXE_bitty` with an isolated `XDG_CONFIG_HOME`;
//! `config check` and `--headless` dispatch without a display, instance,
//! or plugin VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (config + HOME + profiles).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0759-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty").join("profiles")).expect("profiles dir");
    dir
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

/// Writes `$XDG_CONFIG_HOME/bitty/init.lua` with the given body.
fn write_init(root: &Path, body: &str) {
    std::fs::write(root.join("bitty").join("init.lua"), body).expect("write init.lua");
}

/// Runs `bitty <args>` under the isolated root; profiles resolve under it.
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

/// One `config check` row value for `key`, e.g. `window.padding`.
fn row(text: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn profile_extends_chain_loads_base_as_lower_layer() {
    let root = scratch_root("chain");
    write_profile(
        &root,
        "base",
        "return { font = { family = \"Maple Mono\", size = 14 }, window = { opacity = 1.0, padding = 4 } }\n",
    );
    write_profile(
        &root,
        "child",
        "return { extends = \"base\", window = { opacity = 0.9, padding = 8 } }\n",
    );

    let output = run_bitty(&root, &["config", "check", "--profile", "child"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "extends chain must load, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    // Base values survive as the lower layer, attributed to the profile.
    assert!(
        row(&text, "font.family").starts_with("\"Maple Mono\" (profile: "),
        "base font must load with profile source, output:\n{text}"
    );
    // The child overrides the base per field.
    assert!(
        row(&text, "window.padding").starts_with("8 (profile: "),
        "child padding must win, output:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_extends_cycle_exits_2_with_extends_path() {
    let root = scratch_root("cycle");
    write_profile(&root, "a", "return { extends = \"b\" }\n");
    write_profile(&root, "b", "return { extends = \"a\" }\n");

    let output = run_bitty(&root, &["config", "check", "--profile", "a"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "extends cycle must fail closed, stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("extends"),
        "cycle error must name the extends path, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_extends_missing_parent_exits_2_with_extends_path() {
    let root = scratch_root("missing");
    write_profile(&root, "child", "return { extends = \"ghost\" }\n");

    let output = run_bitty(&root, &["config", "check", "--profile", "child"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "missing parent must fail closed, stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("extends"),
        "missing-parent error must name the extends path, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_extends_in_user_file_stays_undeclared() {
    // `extends` in `init.lua` keeps the pre-fix fail-closed behavior: it
    // is a profile-layer-only key.
    let root = scratch_root("userextends");
    write_init(&root, "return { extends = \"base\" }\n");
    write_profile(&root, "base", "return { theme = \"dark\" }\n");

    let output = run_bitty(&root, &["config", "check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "user-layer extends must fail closed, stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("extends"),
        "user-layer error must name extends, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_extends_headless_startup_succeeds() {
    // Issue #1366 repro shape: `--headless --profile child` with
    // `extends = "base"` must start (exit 0), not exit 2.
    let root = scratch_root("headless");
    write_profile(
        &root,
        "base",
        "return { font = { family = \"Maple Mono\", size = 14 } }\n",
    );
    write_profile(
        &root,
        "child",
        "return { extends = \"base\", window = { opacity = 0.9, padding = 8 } }\n",
    );

    let output = run_bitty(&root, &["--headless", "--profile", "child"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "headless startup with extends chain must exit 0, stderr={:?}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}
