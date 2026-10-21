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
fn migrate_rejects_value_flags_instead_of_discarding_them() {
    let root = scratch_root("migrate-flags");
    let legacy = "return { theme = \"dark\", window = { opacity = 0.9, padding = 9 } }\n";
    std::fs::write(init_path(&root), legacy).expect("legacy config");

    // Migration copies the legacy bytes verbatim, so a wizard value flag can
    // never apply; each one is a usage error instead of a silent discard.
    for flags in [
        &["init", "--migrate", "--theme", "dark"][..],
        &["init", "--migrate", "--font-family", "Fira Code"][..],
        &["init", "--migrate", "--font-size", "13"][..],
        &["init", "--migrate", "--scrollback", "2000"][..],
        &["init", "--migrate", "--close-confirm", "never"][..],
        &["init", "--migrate", "--gaps-in", "2"][..],
        &["init", "--migrate", "--gaps-out", "4"][..],
        &["init", "--migrate", "--border", "1"][..],
        &["init", "--migrate", "--radius", "3"][..],
    ] {
        let refused = run_bitty(&root, flags);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{flags:?} must be a usage error, stderr={:?}",
            stderr(&refused)
        );
        let err = stderr(&refused);
        assert!(
            err.contains("--migrate") && err.contains(flags[2]),
            "{flags:?} names the rejected flag, stderr:\n{err}"
        );
        assert_eq!(
            std::fs::read_to_string(init_path(&root)).expect("legacy intact"),
            legacy,
            "{flags:?} writes nothing"
        );
        assert!(
            !config_dir(&root).join("profiles").exists(),
            "{flags:?} writes no profile"
        );
    }

    // A flag-free migration still works after the refusals.
    assert_ok(
        &run_bitty(&root, &["init", "--migrate"]),
        "flag-free migrate",
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default")).expect("migrated profile"),
        legacy
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn migrate_bounds_the_legacy_file_before_reading() {
    let root = scratch_root("migrate-bound");
    let path = init_path(&root);
    // 11 bytes per line * 8192 lines = 88 KiB, above the 64 KiB config bound.
    let oversized = "-- padding\n".repeat(8 * 1024);
    std::fs::write(&path, &oversized).expect("oversized legacy config");

    let refused = run_bitty(&root, &["init", "--migrate"]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "oversized legacy config must fail closed, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("exceeds"),
        "diagnostic names the size bound, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        !config_dir(&root).join("profiles").exists(),
        "size refusal writes no profile"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("legacy intact"),
        oversized
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn migrate_refuses_symlinked_legacy_file() {
    let root = scratch_root("migrate-symlink");
    let real = root.join("real.lua");
    let legacy = "return { theme = \"dark\" }\n";
    std::fs::write(&real, legacy).expect("real legacy config");
    std::os::unix::fs::symlink(&real, init_path(&root)).expect("symlink");

    let refused = run_bitty(&root, &["init", "--migrate"]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "symlinked legacy config must fail closed, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("symlink"),
        "diagnostic names the symlink, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        !config_dir(&root).join("profiles").exists(),
        "symlink refusal writes no profile"
    );
    assert_eq!(
        std::fs::read_to_string(&real).expect("link target intact"),
        legacy
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn migrate_after_default_profile_seed_has_a_safe_path() {
    let root = scratch_root("migrate-after-seed");
    let legacy = "return { theme = \"dark\", font = { family = \"Maple Mono\", size = 13 } }\n";
    std::fs::write(init_path(&root), legacy).expect("legacy config");

    // `init --profile default` on a legacy layout seeds the destination
    // without touching the active legacy file.
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "default"]),
        "seed default profile",
    );
    let seeded = std::fs::read_to_string(profile_path(&root, "default")).expect("seeded profile");
    assert!(seeded.contains("theme = \"dark\""));
    assert_ne!(seeded, legacy);

    // Without --force the migration refuses, but the refusal names the
    // explicit safe path instead of dead-ending.
    let refused = run_bitty(&root, &["init", "--migrate"]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "different destination content needs --force, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("--migrate --force"),
        "refusal names the safe path, stderr={:?}",
        stderr(&refused)
    );
    assert_eq!(
        std::fs::read_to_string(init_path(&root)).expect("legacy intact"),
        legacy
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default")).expect("seeded intact"),
        seeded
    );

    // --force replaces the seeded profile, keeps its bytes in a collision-free
    // backup, and completes the migration.
    assert_ok(
        &run_bitty(&root, &["init", "--migrate", "--force"]),
        "forced migrate",
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default")).expect("migrated profile"),
        legacy
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default").with_extension("lua.bak"))
            .expect("seeded profile backup"),
        seeded
    );
    let selector = std::fs::read_to_string(init_path(&root)).expect("selector");
    assert!(selector.contains("format = \"bitty-profile-v1\""));
    assert!(selector.contains("profile = \"default\""));

    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "post-migration check");
    assert!(
        row(&stdout(&check), "font.family").starts_with("\"Maple Mono\""),
        "migrated values resolve, output:\n{}",
        stdout(&check)
    );

    // Re-running is not a new dead end: the selector short-circuits first.
    let again = run_bitty(&root, &["init", "--migrate"]);
    assert_eq!(again.status.code(), Some(2));
    assert!(
        stderr(&again).contains("already a profile selector"),
        "stderr={:?}",
        stderr(&again)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn migrate_reuses_an_identical_destination_profile() {
    let root = scratch_root("migrate-identical");
    let legacy = "return { theme = \"dark\", window = { opacity = 0.9, padding = 6 } }\n";
    std::fs::write(init_path(&root), legacy).expect("legacy config");
    std::fs::create_dir_all(config_dir(&root).join("profiles")).expect("profiles dir");
    // A previous partial migration may already have copied the exact bytes.
    std::fs::write(profile_path(&root, "default"), legacy).expect("identical destination");

    assert_ok(
        &run_bitty(&root, &["init", "--migrate"]),
        "identical destination needs no force",
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "default")).expect("destination"),
        legacy
    );
    assert!(
        !profile_path(&root, "default")
            .with_extension("lua.bak")
            .exists(),
        "an identical destination is reused, not rewritten"
    );
    assert!(
        std::fs::read_to_string(init_path(&root))
            .expect("selector")
            .contains("profile = \"default\"")
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn migrated_config_lua_alias_is_switchable_by_profile_use() {
    let root = scratch_root("alias-use");
    let alias = config_dir(&root).join("config.lua");
    let legacy = "return { theme = \"dark\", window = { opacity = 0.9, padding = 5 } }\n";
    std::fs::write(&alias, legacy).expect("legacy alias");
    assert!(!init_path(&root).exists(), "only the alias exists");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "seed work profile",
    );

    // An unmigrated legacy alias still fails closed with the migration hint.
    let legacy_refusal = run_bitty(&root, &["config", "profile", "use", "work"]);
    assert_eq!(legacy_refusal.status.code(), Some(2));
    assert!(
        stderr(&legacy_refusal).contains("--migrate"),
        "stderr={:?}",
        stderr(&legacy_refusal)
    );
    assert_eq!(
        std::fs::read_to_string(&alias).expect("alias intact"),
        legacy
    );

    assert_ok(&run_bitty(&root, &["init", "--migrate"]), "migrate alias");
    let migrated = std::fs::read_to_string(&alias).expect("alias selector");
    assert!(migrated.contains("format = \"bitty-profile-v1\""));
    assert!(migrated.contains("profile = \"default\""));
    assert!(!init_path(&root).exists(), "migration keeps the alias name");

    // The migrated alias is a real selector: explicit activation rewrites it.
    assert_ok(
        &run_bitty(&root, &["config", "profile", "use", "work"]),
        "use on migrated alias",
    );
    let rewritten = std::fs::read_to_string(&alias).expect("rewritten alias");
    assert!(rewritten.contains("profile = \"work\""));
    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "alias check");
    assert!(
        row(&stdout(&check), "profile").starts_with("\"work\" (profile: "),
        "output:\n{}",
        stdout(&check)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_show_attributes_shipped_defaults() {
    let root = scratch_root("show-attribution");
    assert_ok(
        &run_bitty(&root, &["init", "--profile", "minimal"]),
        "minimal profile",
    );

    let show = run_bitty(&root, &["config", "profile", "show", "minimal"]);
    assert_ok(&show, "show minimal");
    let text = stdout(&show);
    let theme = row(&text, "appearance.theme");
    assert!(
        theme.contains("(profile: ") && theme.contains("minimal.lua"),
        "a profile-set value names its file:\n{text}"
    );
    for key in [
        "font.family",
        "font.size",
        "terminal.scrollback",
        "window.opacity",
        "window.padding",
        "decoration.gaps_in",
        "close_confirm",
    ] {
        assert!(
            row(&text, key).ends_with("(default)"),
            "{key} is a shipped default, never profile-owned:\n{text}"
        );
    }

    // A preset that writes the value attributes it to its own layer.
    assert_ok(
        &run_bitty(&root, &["init", "--profile", "dark"]),
        "dark profile",
    );
    let show = run_bitty(&root, &["config", "profile", "show", "dark"]);
    assert_ok(&show, "show dark");
    let text = stdout(&show);
    assert!(
        row(&text, "font.family").contains("(profile: ")
            && row(&text, "font.family").contains("dark.lua"),
        "a generated value names its profile file:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn preset_profiles_seed_without_prompts_or_activation() {
    let mut lengths = Vec::new();
    for (name, want_theme) in [
        ("minimal", "dark"),
        ("dark", "dark"),
        ("light", "github-light"),
    ] {
        let root = scratch_root(&format!("preset-{name}"));
        // No --yes and no TTY: presets are deterministic seeds, never prompts.
        let output = run_bitty(&root, &["init", "--profile", name]);
        assert_ok(&output, name);
        assert!(
            !stdout(&output).contains("Shell:"),
            "preset {name} must not prompt, stdout={:?}",
            stdout(&output)
        );
        assert!(
            !init_path(&root).exists(),
            "preset {name} never writes a selector"
        );
        assert!(
            !profile_path(&root, "default").exists(),
            "preset {name} never touches the default profile"
        );

        let body = std::fs::read_to_string(profile_path(&root, name)).expect("preset profile body");
        assert!(
            body.contains(&format!("theme = \"{want_theme}\"")),
            "preset {name} seeds theme {want_theme}:\n{body}"
        );
        if name == "minimal" {
            assert!(!body.contains("font ="), "minimal omits font:\n{body}");
            assert!(
                !body.contains("decoration ="),
                "minimal omits decoration:\n{body}"
            );
            assert!(
                !body.contains("terminal ="),
                "minimal omits terminal:\n{body}"
            );
        } else {
            assert!(body.contains("font = { family ="), "{body}");
            assert!(
                body.contains(&format!("theme = \"{want_theme}\"")),
                "{body}"
            );
        }
        lengths.push(body.len());

        let check = run_bitty(&root, &["config", "check", "--profile", name]);
        assert_ok(&check, &format!("{name} check"));
        let text = stdout(&check);
        assert!(
            row(&text, "profile").starts_with(&format!("\"{name}\" (profile: ")),
            "preset profile resolves:\n{text}"
        );
        assert!(
            row(&text, "appearance.theme").starts_with(&format!("\"{want_theme}\"")),
            "preset theme resolves:\n{text}"
        );

        let show = run_bitty(&root, &["config", "profile", "show", name]);
        assert_ok(&show, &format!("{name} show"));
        assert!(
            row(&stdout(&show), "profile").starts_with(&format!("\"{name}\"")),
            "profile show names the preset:\n{}",
            stdout(&show)
        );

        let _ = std::fs::remove_dir_all(&root);
    }
    assert!(
        lengths[0] < lengths[1] && lengths[0] < lengths[2],
        "minimal stays the smallest seed (lengths: {lengths:?})"
    );
}

#[test]
fn preset_reinit_requires_force_and_backs_up_atomically() {
    let root = scratch_root("preset-force");
    assert_ok(
        &run_bitty(&root, &["init", "--profile", "dark"]),
        "seed dark",
    );
    let target = profile_path(&root, "dark");
    let original = std::fs::read_to_string(&target).expect("dark profile");

    let refused = run_bitty(&root, &["init", "--profile", "dark"]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "re-init without --force must fail closed, stderr={:?}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("already exists"),
        "refusal names the conflict, stderr={:?}",
        stderr(&refused)
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("profile intact"),
        original
    );
    assert!(
        !target.with_extension("lua.bak").exists(),
        "a refusal creates no backup"
    );

    // A user edit survives in the recoverable backup, and the replacement is
    // atomic (no temporary siblings survive).
    let edited = format!("{original}-- local edit\n");
    std::fs::write(&target, &edited).expect("user edit");
    assert_ok(
        &run_bitty(&root, &["init", "--profile", "dark", "--force"]),
        "force seed dark",
    );
    let backup = target.with_extension("lua.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup body"),
        edited,
        "backup holds the exact previous bytes"
    );
    let profiles_dir = config_dir(&root).join("profiles");
    let leftovers: Vec<String> = std::fs::read_dir(&profiles_dir)
        .expect("profiles dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");

    // The forced replacement re-validates through the normal load path.
    let check = run_bitty(&root, &["config", "check", "--profile", "dark"]);
    assert_ok(&check, "forced profile re-validates");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn named_profile_reinit_backups_are_collision_free() {
    let root = scratch_root("named-force");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "work profile",
    );
    let target = profile_path(&root, "work");
    let first = std::fs::read_to_string(&target).expect("work body");
    assert_ok(
        &run_bitty(
            &root,
            &[
                "init",
                "--yes",
                "--profile",
                "work",
                "--font-size",
                "15",
                "--force",
            ],
        ),
        "force work",
    );
    let second = std::fs::read_to_string(&target).expect("work body 2");
    assert_ne!(first, second, "forced re-init applies the new value flags");
    assert_eq!(
        std::fs::read_to_string(target.with_extension("lua.bak")).expect("first backup"),
        first
    );
    assert_ok(
        &run_bitty(
            &root,
            &[
                "init",
                "--yes",
                "--profile",
                "work",
                "--font-size",
                "16",
                "--force",
            ],
        ),
        "force work again",
    );
    let second_backup = config_dir(&root).join("profiles").join("work.lua.bak.1");
    assert_eq!(
        std::fs::read_to_string(&second_backup).expect("second backup"),
        second,
        "a second forced write never overwrites the sole recovery copy"
    );
    assert_eq!(
        std::fs::read_to_string(target.with_extension("lua.bak")).expect("first backup intact"),
        first
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn selection_precedence_cli_over_env_over_selector() {
    let root = scratch_root("precedence");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "work profile",
    );
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "envp"]),
        "env profile",
    );
    std::fs::write(
        profile_path(&root, "work"),
        "return { font = { family = \"Fira Code\", size = 14 } }\n",
    )
    .expect("work body");
    std::fs::write(
        profile_path(&root, "envp"),
        "return { font = { family = \"Iosevka\", size = 13 } }\n",
    )
    .expect("env body");

    // Persisted selector (bare launch).
    let bare = run_bitty(&root, &["config", "check"]);
    assert_ok(&bare, "selector check");
    assert!(
        row(&stdout(&bare), "profile").starts_with("\"default\" (profile: "),
        "{}",
        stdout(&bare)
    );

    // BITTY_PROFILE wins over the persisted selector.
    let env_selected = Command::new(BITTY_BIN)
        .args(["config", "check"])
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_PROFILE", "envp")
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .output()
        .expect("spawn env check");
    assert_ok(&env_selected, "env check");
    let env_text = stdout(&env_selected);
    assert!(
        row(&env_text, "profile").starts_with("\"envp\" (profile: "),
        "{env_text}"
    );
    assert!(
        row(&env_text, "font.family").starts_with("\"Iosevka\""),
        "{env_text}"
    );

    // --profile wins over BITTY_PROFILE for that one launch.
    let cli_selected = Command::new(BITTY_BIN)
        .args(["config", "check", "--profile", "work"])
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_PROFILE", "envp")
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .output()
        .expect("spawn cli check");
    assert_ok(&cli_selected, "cli check");
    let cli_text = stdout(&cli_selected);
    assert!(
        row(&cli_text, "profile").starts_with("\"work\" (profile: "),
        "{cli_text}"
    );
    assert!(
        row(&cli_text, "font.family").starts_with("\"Fira Code\""),
        "{cli_text}"
    );

    // The persisted selector is never rewritten by a one-launch selection.
    assert!(
        std::fs::read_to_string(init_path(&root))
            .expect("selector")
            .contains("profile = \"default\"")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_profile_name_and_bitty_profile_never_target_output() {
    let root = scratch_root("names");
    let bad = run_bitty(&root, &["init", "--yes", "--profile", "../escape"]);
    assert_eq!(bad.status.code(), Some(2), "traversal name refused");
    assert!(!config_dir(&root).join("profiles").exists());

    // An explicitly supplied blank name is a usage error, never a silent
    // fallback that overwrites the default profile.
    for blank in [
        &["init", "--yes", "--profile", ""][..],
        &["init", "--yes", "--profile", "   "][..],
        &["init", "--yes", "--profile="][..],
    ] {
        let refused = run_bitty(&root, blank);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "blank profile name must fail closed for {blank:?}, stderr={:?}",
            stderr(&refused)
        );
        assert!(!init_path(&root).exists(), "blank name writes no selector");
        assert!(
            !config_dir(&root).join("profiles").exists(),
            "blank name writes no profile"
        );
    }

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
fn env_profile_overrides_selector_and_invalid_env_fails_closed() {
    let root = scratch_root("env-override");
    assert_ok(&run_bitty(&root, &["init", "--yes"]), "fresh init");
    assert_ok(
        &run_bitty(&root, &["init", "--yes", "--profile", "work"]),
        "work profile",
    );
    std::fs::write(
        profile_path(&root, "work"),
        "return { font = { family = \"Fira Code\", size = 14 } }\n",
    )
    .expect("work body");

    let env_check = |profile: &str| {
        Command::new(BITTY_BIN)
            .args(["config", "check"])
            .env("XDG_CONFIG_HOME", &root)
            .env("HOME", &root)
            .env("BITTY_PROFILE", profile)
            .env("NO_COLOR", "1")
            .env_remove("BITTY_CONFIG")
            .output()
            .expect("spawn config check")
    };

    let selected = env_check("work");
    assert_ok(&selected, "BITTY_PROFILE override");
    let text = stdout(&selected);
    assert!(
        row(&text, "profile").starts_with("\"work\" (profile: "),
        "environment selection wins over the selector, output:\n{text}"
    );
    assert!(
        row(&text, "font.family").starts_with("\"Fira Code\""),
        "{text}"
    );

    let invalid = env_check("../escape");
    assert_eq!(
        invalid.status.code(),
        Some(2),
        "invalid BITTY_PROFILE fails closed rather than falling back to the selector"
    );
    assert!(
        stderr(&invalid).contains("BITTY_PROFILE"),
        "diagnostic names the environment source, stderr={:?}",
        stderr(&invalid)
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

#[test]
fn init_shell_name_positional_hints_shell_init_and_writes_nothing() {
    // `bitty init fish` is a common shell-init typo (#2367): a targeted hint
    // precedes the usage and nothing is written. The generic unexpected-
    // argument path keeps its existing shape.
    let root = scratch_root("shell-hint");
    let output = run_bitty(&root, &["init", "fish"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "shell-name positional is a usage error, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("bitty init: `fish` is a shell name; did you mean `bitty shell-init fish`?"),
        "targeted hint, stderr:\n{err}"
    );
    let hint_at = err.find("is a shell name").expect("hint present");
    let usage_at = err.find("usage: bitty init").expect("usage present");
    assert!(
        hint_at < usage_at,
        "hint must precede the usage, stderr:\n{err}"
    );
    assert!(!init_path(&root).exists(), "hint writes no config");
    assert!(
        !profile_path(&root, "default").exists(),
        "hint writes no profile"
    );

    let generic = run_bitty(&root, &["init", "bogus"]);
    assert_eq!(generic.status.code(), Some(2));
    let generic_err = stderr(&generic);
    assert!(
        generic_err.contains("bitty init: unexpected argument 'bogus'"),
        "generic path unchanged, stderr:\n{generic_err}"
    );
    assert!(
        !generic_err.contains("is a shell name"),
        "generic path gets no shell hint, stderr:\n{generic_err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
