//! Binary-level profile-backed `bitty init` proofs (CTX-1404, issue #2187).
//!
//! Every case uses an isolated `XDG_CONFIG_HOME`; the owner's real config,
//! plugin store, shell rc files, clipboard, and desktop are never touched.
//! `init` and `config check` are pre-runtime operations, so no display,
//! instance, IPC peer, or plugin VM is required.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root with a `bitty/` config directory.
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1404-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty")).expect("config dir");
    dir
}

fn config_dir(root: &Path) -> PathBuf {
    root.join("bitty")
}

fn init_path(root: &Path) -> PathBuf {
    config_dir(root).join("init.lua")
}

fn profile_path(root: &Path, name: &str) -> PathBuf {
    config_dir(root)
        .join("profiles")
        .join(format!("{name}.lua"))
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
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn row(text: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"))
        .trim()
        .to_string()
}

fn assert_ok(output: &Output, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{label} must succeed, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

#[test]
fn bare_init_creates_profile_and_thin_selector() {
    let root = scratch_root("fresh");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");

    let selector = std::fs::read_to_string(init_path(&root)).expect("selector");
    assert!(selector.contains("format = \"bitty-profile-v1\""));
    assert!(selector.contains("profile = \"default\""));
    assert!(
        !selector.contains("font ="),
        "selector stays thin: {selector}"
    );
    assert!(!selector.contains("decoration ="));

    let profile = std::fs::read_to_string(profile_path(&root, "default")).expect("profile");
    assert!(profile.contains("theme = \"dark\""));
    assert!(profile.contains("font = { family ="));
    assert!(profile.contains("scrollback ="));

    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "config check");
    let text = stdout(&check);
    assert!(
        row(&text, "profile").starts_with("\"default\" (profile: "),
        "selector must resolve default.lua, output:\n{text}"
    );
    assert!(
        row(&text, "font.family").starts_with('"')
            && row(&text, "font.family").contains("(profile: "),
        "generated values come from the profile, output:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn second_init_refuses_and_keeps_user_overrides() {
    let root = scratch_root("rerun");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    let selector_path = init_path(&root);
    let original = std::fs::read_to_string(&selector_path).expect("selector");
    let edited = original.replace(
        "    -- overrides go here",
        "    window = { opacity = 0.9, padding = 8 },",
    );
    std::fs::write(&selector_path, &edited).expect("user edit");

    let second = run_bitty(&root, &["init", "--yes"]);
    assert_eq!(
        second.status.code(),
        Some(2),
        "second init without --force must refuse, stderr={:?}",
        stderr(&second)
    );
    assert_eq!(
        std::fs::read_to_string(&selector_path).expect("selector intact"),
        edited,
        "refusal keeps the user edit"
    );

    // User overrides win over the generated profile (`file > profile`).
    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "config check with overrides");
    let text = stdout(&check);
    assert!(
        row(&text, "window.opacity").starts_with("0.9 (file: "),
        "{text}"
    );
    assert!(
        row(&text, "window.padding").starts_with("8 (file: "),
        "{text}"
    );
    assert!(
        row(&text, "appearance.theme").contains("(profile: "),
        "{text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_selected_profile_fails_closed() {
    let root = scratch_root("missing-profile");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    std::fs::remove_file(profile_path(&root, "default")).expect("remove profile");

    let check = run_bitty(&root, &["config", "check"]);
    assert_eq!(
        check.status.code(),
        Some(2),
        "missing selected profile must fail closed, stdout={:?}",
        stdout(&check)
    );
    assert!(
        stderr(&check).contains("selected profile 'default'"),
        "diagnostic names the selected profile, stderr={:?}",
        stderr(&check)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn named_init_writes_only_that_profile_until_explicit_use() {
    let root = scratch_root("named");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "minimal"]),
        "named init",
    );
    assert!(
        profile_path(&root, "minimal").exists(),
        "minimal profile written"
    );
    assert!(!init_path(&root).exists(), "named init creates no selector");
    assert!(
        !profile_path(&root, "default").exists(),
        "named init does not create default.lua"
    );

    // A bare launch stays unprofiled: creating a profile never activates it.
    let before = run_bitty(&root, &["config", "check"]);
    assert_ok(&before, "unprofiled check");
    assert!(
        row(&stdout(&before), "profile").starts_with("(none)"),
        "unselected profile must not be inferred, output:\n{}",
        stdout(&before)
    );

    assert_ok(
        &run_bitty(&root, &["config", "profile", "use", "minimal"]),
        "explicit activation",
    );
    let selector = std::fs::read_to_string(init_path(&root)).expect("selector");
    assert!(selector.contains("profile = \"minimal\""));
    let after = run_bitty(&root, &["config", "check"]);
    assert_ok(&after, "activated check");
    assert!(
        row(&stdout(&after), "profile").starts_with("\"minimal\" (profile: "),
        "activated profile resolves, output:\n{}",
        stdout(&after)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_profile_overrides_selector_for_one_launch() {
    let root = scratch_root("override");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "work profile",
    );
    // Rewrite the work profile so its values are distinguishable.
    std::fs::write(
        profile_path(&root, "work"),
        "return { font = { family = \"Fira Code\", size = 14 } }\n",
    )
    .expect("work profile body");

    let selected = run_bitty(&root, &["config", "check", "--profile", "work"]);
    assert_ok(&selected, "--profile override");
    let text = stdout(&selected);
    assert!(
        row(&text, "profile").starts_with("\"work\" (profile: "),
        "CLI selection wins, output:\n{text}"
    );
    assert!(
        row(&text, "font.family").starts_with("\"Fira Code\""),
        "CLI-selected profile values load, output:\n{text}"
    );

    // The persisted selector is untouched: a bare launch still uses default.
    let bare = run_bitty(&root, &["config", "check"]);
    assert_ok(&bare, "bare check");
    assert!(
        row(&stdout(&bare), "profile").starts_with("\"default\" (profile: "),
        "one-launch override must not rewrite the selector, output:\n{}",
        stdout(&bare)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn use_preserves_selector_overrides_and_backs_up() {
    let root = scratch_root("use-preserve");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "work profile",
    );
    let selector_path = init_path(&root);
    let original = std::fs::read_to_string(&selector_path).expect("selector");
    let edited = original.replace(
        "    -- overrides go here",
        "    window = { opacity = 0.85, padding = 6 },",
    );
    std::fs::write(&selector_path, &edited).expect("user edit");

    assert_ok(
        &run_bitty(&root, &["config", "profile", "use", "work"]),
        "use work",
    );
    let rewritten = std::fs::read_to_string(&selector_path).expect("rewritten selector");
    assert!(rewritten.contains("profile = \"work\""));
    assert!(
        rewritten.contains("window = { opacity = 0.85, padding = 6 }"),
        "activation preserves user overrides:\n{rewritten}"
    );
    assert!(
        selector_path.with_extension("lua.bak").exists(),
        "activation keeps a recoverable backup"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn legacy_init_is_never_replaced_even_with_force_and_can_migrate() {
    let root = scratch_root("legacy");
    let legacy = "return { theme = \"dark\", font = { family = \"Maple Mono\", size = 13 }, window = { opacity = 0.9, padding = 7 } }\n";
    std::fs::write(init_path(&root), legacy).expect("legacy config");

    for extra in [&["init", "--yes"][..], &["init", "--yes", "--force"][..]] {
        let refused = run_bitty(&root, extra);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "legacy init must refuse {extra:?}, stderr={:?}",
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("--migrate"),
            "refusal names the migration path, stderr={:?}",
            stderr(&refused)
        );
        assert_eq!(
            std::fs::read_to_string(init_path(&root)).expect("legacy intact"),
            legacy
        );
        assert!(!profile_path(&root, "default").exists());
    }

    let migrated = run_bitty(&root, &["init", "--migrate"]);
    assert_ok(&migrated, "migrate");
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default")).expect("migrated profile"),
        legacy,
        "migration copies complete legacy bytes"
    );
    let selector = std::fs::read_to_string(init_path(&root)).expect("selector");
    assert!(selector.contains("format = \"bitty-profile-v1\""));
    assert!(selector.contains("profile = \"default\""));
    assert!(
        init_path(&root).with_extension("lua.bak").exists(),
        "legacy bytes stay recoverable"
    );
    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "post-migration check");
    let text = stdout(&check);
    assert!(
        row(&text, "font.family").starts_with("\"Maple Mono\""),
        "{text}"
    );
    assert!(row(&text, "window.padding").starts_with("7"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_profile_name_and_bitty_profile_never_target_output() {
    let root = scratch_root("names");
    let bad = run_bitty(&root, &["init", "--yes", "--profile", "../escape"]);
    assert_eq!(bad.status.code(), Some(2), "traversal name refused");
    assert!(!config_dir(&root).join("profiles").exists());

    // A mutating command rejects an omitted target instead of silently
    // falling back to a bare init.
    let missing = run_bitty(&root, &["init", "--yes", "--profile"]);
    assert_eq!(
        missing.status.code(),
        Some(2),
        "missing --profile value is a usage error"
    );
    assert!(
        stderr(&missing).contains("--profile needs a profile name"),
        "diagnostic names the missing value, stderr={:?}",
        stderr(&missing)
    );
    assert!(!init_path(&root).exists(), "missing target writes nothing");

    // BITTY_PROFILE is a one-launch input, never an init output target.
    let output = Command::new(BITTY_BIN)
        .args(["init", "--yes"])
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_PROFILE", "work")
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .output()
        .expect("spawn init with BITTY_PROFILE");
    assert_eq!(output.status.code(), Some(0), "{:?}", stderr(&output));
    assert!(profile_path(&root, "default").exists());
    assert!(!profile_path(&root, "work").exists());
    assert!(
        std::fs::read_to_string(init_path(&root))
            .expect("selector")
            .contains("profile = \"default\"")
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn explicit_config_selector_resolves_sibling_profile() {
    let root = scratch_root("explicit");
    let custom_dir = root.join("custom-config");
    std::fs::create_dir_all(&custom_dir).expect("custom dir");
    let selector = custom_dir.join("bitty.lua");
    let selector_s = selector.display().to_string();
    let output = Command::new(BITTY_BIN)
        .args(["init", "--yes"])
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_CONFIG", &selector_s)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_PROFILE")
        .output()
        .expect("spawn init with BITTY_CONFIG");
    assert_eq!(output.status.code(), Some(0), "{:?}", stderr(&output));
    assert!(custom_dir.join("profiles").join("default.lua").exists());

    let check = Command::new(BITTY_BIN)
        .args(["config", "check"])
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_CONFIG", &selector_s)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_PROFILE")
        .output()
        .expect("spawn config check");
    assert_ok(&check, "explicit selector check");
    let text = stdout(&check);
    assert!(
        row(&text, "profile").starts_with("\"default\" (profile: "),
        "selector resolves profiles beside the explicit file, output:\n{text}"
    );
    assert!(
        row(&text, "font.family").contains("(profile: "),
        "explicit selector profile values load, output:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_list_and_path_do_not_create_or_evaluate() {
    let root = scratch_root("list");
    let empty = run_bitty(&root, &["config", "profile", "list"]);
    assert_ok(&empty, "empty list");
    assert!(stdout(&empty).trim().is_empty());

    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "beta"]),
        "beta profile",
    );
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "alpha"]),
        "alpha profile",
    );
    let listed = run_bitty(&root, &["config", "profile", "list"]);
    assert_ok(&listed, "list");
    assert_eq!(stdout(&listed), "alpha\nbeta\n");

    let path = run_bitty(&root, &["config", "profile", "path", "alpha"]);
    assert_ok(&path, "path");
    assert_eq!(
        stdout(&path).trim(),
        profile_path(&root, "alpha").display().to_string()
    );
    assert!(
        !profile_path(&root, "gamma").exists(),
        "path never creates a target"
    );

    let _ = std::fs::remove_dir_all(&root);
}
