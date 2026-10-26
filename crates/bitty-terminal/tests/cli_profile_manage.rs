//! Binary-level named-profile management proof (CTX-1589 / bitty#2449).
//!
//! Drives the built `bitty` binary with an isolated `XDG_CONFIG_HOME`:
//! create/clone/rename/delete/use/get/set/list/show, conflict semantics,
//! traversal refusal, schema validation, atomic writes, and the `--json`
//! envelope. `bitty config` dispatch is local, so no display, instance, or
//! plugin VM is created.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (config + HOME + profiles).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1589-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty").join("profiles")).expect("profiles dir");
    dir
}

fn profile_path(root: &Path, name: &str) -> PathBuf {
    root.join("bitty")
        .join("profiles")
        .join(format!("{name}.lua"))
}

fn init_path(root: &Path) -> PathBuf {
    root.join("bitty").join("init.lua")
}

/// Writes `$XDG_CONFIG_HOME/bitty/profiles/<name>.lua` with the given body.
fn write_profile(root: &Path, name: &str, body: &str) {
    std::fs::write(profile_path(root, name), body).expect("write profile");
}

/// Writes the versioned selector naming `profile`.
fn write_selector(root: &Path, profile: &str) {
    std::fs::write(
        init_path(root),
        format!(
            "return {{\n    format = \"bitty-profile-v1\",\n    profile = \"{profile}\",\n}}\n"
        ),
    )
    .expect("write selector");
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

fn assert_ok(output: &Output, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{what} must succeed, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

fn assert_exit(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what} must exit {code}, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

/// One `config check` row value for `key`, e.g. `font.size`.
fn row(text: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"))
        .trim()
        .to_string()
}

/// Leaves no `.*.tmp-*` staging files behind.
fn assert_no_temp_files(root: &Path, what: &str) {
    let leftovers: Vec<String> = std::fs::read_dir(root.join("bitty").join("profiles"))
        .expect("profiles dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "{what} left temp files: {leftovers:?}"
    );
}

#[test]
fn create_writes_minimal_profile_and_existing_name_conflicts() {
    let root = scratch_root("create");
    let created = run_bitty(&root, &["config", "profile", "create", "work"]);
    assert_ok(&created, "create");
    let path = profile_path(&root, "work");
    let first = std::fs::read_to_string(&path).expect("created profile");
    assert!(first.contains("return {"), "{first}");
    assert_ok(
        &run_bitty(&root, &["config", "profile", "show", "work"]),
        "show created profile",
    );

    let again = run_bitty(&root, &["config", "profile", "create", "work"]);
    assert_exit(&again, 2, "existing profile create");
    let err = stderr(&again);
    assert!(
        err.contains("already exists") && err.contains("refusing to overwrite"),
        "conflict message: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("profile intact"),
        first,
        "a conflict never rewrites the profile"
    );
    assert_no_temp_files(&root, "create conflict");

    let reserved = run_bitty(&root, &["config", "profile", "create", "default"]);
    assert_exit(&reserved, 2, "reserved default");
    assert!(
        stderr(&reserved).contains("reserved for `bitty init`"),
        "reserved message: {}",
        stderr(&reserved)
    );
    assert!(!profile_path(&root, "default").exists());
    // `default` is reserved for `bitty init` on the destructive verbs too,
    // but cloning it out is the supported way to customize a copy.
    write_profile(
        &root,
        "default",
        "return {\n    appearance = { theme = \"dark\" },\n}\n",
    );
    let delete_default = run_bitty(&root, &["config", "profile", "delete", "default"]);
    assert_exit(&delete_default, 2, "delete default");
    assert!(profile_path(&root, "default").exists());
    let rename_default = run_bitty(&root, &["config", "profile", "rename", "work", "default"]);
    assert_exit(&rename_default, 2, "rename to default");
    assert_ok(
        &run_bitty(&root, &["config", "profile", "clone", "default", "custom"]),
        "clone default",
    );
    assert!(profile_path(&root, "custom").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clone_copies_bytes_verbatim_and_refuses_existing_destination() {
    let root = scratch_root("clone");
    let body =
        "-- user comment\nreturn {\n    font = { family = \"Mono\", size = 13 }, -- inline\n}\n";
    write_profile(&root, "base", body);
    assert_ok(
        &run_bitty(&root, &["config", "profile", "clone", "base", "copy"]),
        "clone",
    );
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "copy")).expect("clone"),
        body,
        "clone copies the source bytes (comments preserved)"
    );
    assert_exit(
        &run_bitty(&root, &["config", "profile", "clone", "base", "copy"]),
        2,
        "clone onto existing",
    );
    assert_exit(
        &run_bitty(&root, &["config", "profile", "clone", "missing", "copy2"]),
        2,
        "clone from missing",
    );
    assert!(!profile_path(&root, "copy2").exists());
    assert_no_temp_files(&root, "clone");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rename_moves_in_place_and_refuses_existing_destination() {
    let root = scratch_root("rename");
    write_profile(
        &root,
        "old",
        "return { font = { family = \"Mono\", size = 12 } }\n",
    );
    write_profile(&root, "taken", "return {}\n");
    assert_ok(
        &run_bitty(&root, &["config", "profile", "rename", "old", "new"]),
        "rename",
    );
    assert!(!profile_path(&root, "old").exists());
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "new")).expect("renamed"),
        "return { font = { family = \"Mono\", size = 12 } }\n"
    );
    assert_exit(
        &run_bitty(&root, &["config", "profile", "rename", "new", "taken"]),
        2,
        "rename onto existing",
    );
    assert!(profile_path(&root, "new").exists(), "source preserved");
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "taken")).expect("taken intact"),
        "return {}\n"
    );
    assert_exit(
        &run_bitty(&root, &["config", "profile", "rename", "new", "new"]),
        2,
        "rename onto itself",
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_removes_an_inactive_profile_only() {
    let root = scratch_root("delete");
    write_profile(&root, "work", "return {}\n");
    write_profile(&root, "other", "return {}\n");
    write_selector(&root, "work");
    assert_exit(
        &run_bitty(&root, &["config", "profile", "delete", "work"]),
        2,
        "delete active (selector)",
    );
    assert!(
        stderr(&run_bitty(&root, &["config", "profile", "delete", "work"]))
            .contains("is active (--profile/BITTY_PROFILE or the init.lua selector)"),
        "active message"
    );
    assert!(profile_path(&root, "work").exists());
    // A one-launch `--profile` value never excuses deleting the persisted
    // selector's target (it would break the next launch).
    assert_exit(
        &run_bitty(
            &root,
            &["config", "profile", "delete", "work", "--profile", "other"],
        ),
        2,
        "delete selector target with another --profile",
    );
    assert!(profile_path(&root, "work").exists());
    assert_exit(
        &run_bitty(
            &root,
            &["config", "profile", "delete", "other", "--profile", "other"],
        ),
        2,
        "delete active (--profile)",
    );
    assert!(profile_path(&root, "other").exists());
    assert_ok(
        &run_bitty(&root, &["config", "profile", "delete", "other"]),
        "delete inactive",
    );
    assert!(!profile_path(&root, "other").exists());
    assert_no_temp_files(&root, "delete");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_and_rename_refuse_a_profile_another_profile_extends() {
    let root = scratch_root("extends-ref");
    write_profile(
        &root,
        "base",
        "return { font = { family = \"Mono\", size = 12 } }\n",
    );
    write_profile(&root, "child", "return { extends = \"base\" }\n");
    let delete = run_bitty(&root, &["config", "profile", "delete", "base"]);
    assert_exit(&delete, 2, "delete extended base");
    assert!(
        stderr(&delete).contains("extended by 'child'"),
        "dependent message: {}",
        stderr(&delete)
    );
    assert!(profile_path(&root, "base").exists());
    let rename = run_bitty(&root, &["config", "profile", "rename", "base", "base2"]);
    assert_exit(&rename, 2, "rename extended base");
    assert!(profile_path(&root, "base").exists());
    assert!(!profile_path(&root, "base2").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_names_and_traversal_are_rejected_without_writes() {
    let root = scratch_root("traversal");
    write_profile(&root, "work", "return {}\n");
    for bad in [
        "../evil",
        "../../etc/passwd",
        "a/b",
        "..",
        ".",
        "x.lua",
        " ",
    ] {
        let output = run_bitty(&root, &["config", "profile", "create", bad]);
        assert_exit(&output, 2, &format!("create {bad:?}"));
        assert!(
            stderr(&output).contains("bitty config profile create:"),
            "create {bad:?} names the command: {}",
            stderr(&output)
        );
    }
    assert!(!root.join("evil.lua").exists(), "no file escaped the root");
    assert!(!root.join("bitty").join("evil.lua").exists());

    let set = run_bitty(
        &root,
        &["config", "profile", "set", "../work", "font.size", "12"],
    );
    assert_exit(&set, 2, "set traversal name");
    let get = run_bitty(
        &root,
        &["config", "profile", "get", "work", "../../etc/passwd"],
    );
    assert_exit(&get, 2, "get traversal path");
    assert!(
        stderr(&get).contains("invalid config path"),
        "path message: {}",
        stderr(&get)
    );
    assert_no_temp_files(&root, "traversal");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn use_switches_the_selector_and_is_idempotent() {
    let root = scratch_root("use");
    write_profile(&root, "alpha", "return {}\n");
    write_profile(&root, "beta", "return {}\n");
    write_selector(&root, "alpha");
    assert_ok(
        &run_bitty(&root, &["config", "profile", "use", "beta"]),
        "switch to beta",
    );
    let selector = std::fs::read_to_string(init_path(&root)).expect("selector");
    assert!(selector.contains("profile = \"beta\""), "{selector}");
    let backups_before = std::fs::read_dir(root.join("bitty"))
        .expect("bitty dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak"))
        .count();

    let again = run_bitty(&root, &["config", "profile", "use", "beta"]);
    assert_ok(&again, "use already active");
    assert!(
        stdout(&again).contains("already active"),
        "no-op message: {}",
        stdout(&again)
    );
    let backups_after = std::fs::read_dir(root.join("bitty"))
        .expect("bitty dir")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak"))
        .count();
    assert_eq!(
        backups_before, backups_after,
        "an already-active use writes nothing (no new .bak)"
    );
    assert_exit(
        &run_bitty(&root, &["config", "profile", "use", "missing"]),
        2,
        "use missing",
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_writes_a_schema_value_get_reads_it_and_config_check_passes() {
    let root = scratch_root("set-get");
    write_profile(
        &root,
        "work",
        "-- keep this comment\nreturn {\n    appearance = { theme = \"dark\" }, -- trailing\n}\n",
    );
    let set = run_bitty(
        &root,
        &["config", "profile", "set", "work", "font.size", "13"],
    );
    assert_ok(&set, "set font.size");
    let content = std::fs::read_to_string(profile_path(&root, "work")).expect("edited profile");
    assert!(content.contains("-- keep this comment"), "{content}");
    assert!(content.contains("-- trailing"), "{content}");
    assert!(content.contains("font = { family = "), "{content}");
    assert!(content.contains("size = 13"), "{content}");

    let get = run_bitty(&root, &["config", "profile", "get", "work", "font.size"]);
    assert_ok(&get, "get font.size");
    let text = stdout(&get);
    assert!(text.starts_with("font.size = 13 (profile: "), "{text}");

    let check = run_bitty(&root, &["config", "check", "--profile", "work"]);
    assert_ok(&check, "config check");
    assert!(
        row(&stdout(&check), "font.size").starts_with("13 (profile: "),
        "config check sees the new value: {}",
        stdout(&check)
    );

    // A profile chain validates the whole chain after the edit.
    write_profile(&root, "child", "return { extends = \"work\" }\n");
    assert_ok(
        &run_bitty(
            &root,
            &["config", "profile", "set", "child", "font.size", "14"],
        ),
        "set in child",
    );
    let check = run_bitty(&root, &["config", "check", "--profile", "child"]);
    assert_ok(&check, "check child");
    assert!(row(&stdout(&check), "font.size").starts_with("14 (profile: "));
    assert_no_temp_files(&root, "set");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_rejects_schema_invalid_values_and_keeps_the_previous_bytes() {
    let root = scratch_root("invalid-value");
    let original = "return {\n    font = { family = \"Mono\", size = 12 },\n}\n";
    write_profile(&root, "work", original);
    for (path, value) in [
        ("font.size", "0"),
        ("font.size", "9999"),
        ("window.opacity", "2.0"),
        ("terminal.bell", "loud"),
        ("selection.auto_copy", "yes"),
        ("font.family", ""),
    ] {
        let output = run_bitty(&root, &["config", "profile", "set", "work", path, value]);
        assert_exit(&output, 2, &format!("set {path} {value:?}"));
        let err = stderr(&output);
        assert!(
            err.contains("invalid value for") || err.contains("validation error at"),
            "schema message for {path}={value:?}: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(profile_path(&root, "work")).expect("profile"),
            original,
            "a rejected value never rewrites {path}"
        );
    }
    // Unknown paths are refused before any read/write.
    let unknown = run_bitty(
        &root,
        &["config", "profile", "set", "work", "font.nope", "1"],
    );
    assert_exit(&unknown, 2, "unknown path");
    assert!(stderr(&unknown).contains("unknown config path"));
    // A syntactically invalid profile is not edited.
    write_profile(&root, "broken", "return { font = ");
    let broken = run_bitty(
        &root,
        &["config", "profile", "set", "broken", "font.size", "12"],
    );
    assert_exit(&broken, 2, "broken profile");
    assert_eq!(
        std::fs::read_to_string(profile_path(&root, "broken")).expect("broken"),
        "return { font = "
    );
    assert_no_temp_files(&root, "invalid value");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_creates_missing_tables_and_replaces_existing_values() {
    let root = scratch_root("set-shape");
    write_profile(&root, "work", "return {\n}\n");
    assert_ok(
        &run_bitty(
            &root,
            &[
                "config",
                "profile",
                "set",
                "work",
                "decoration.gaps_in",
                "4",
            ],
        ),
        "set nested missing",
    );
    assert_ok(
        &run_bitty(
            &root,
            &[
                "config",
                "profile",
                "set",
                "work",
                "decoration.gaps_in",
                "6",
            ],
        ),
        "replace nested",
    );
    let content = std::fs::read_to_string(profile_path(&root, "work")).expect("profile");
    assert_eq!(
        content.matches("decoration").count(),
        1,
        "no duplicate table: {content}"
    );
    assert!(content.contains("gaps_in = 6"), "{content}");
    assert_ok(
        &run_bitty(
            &root,
            &[
                "config",
                "profile",
                "set",
                "work",
                "font.fallback",
                "Mono, Sans",
            ],
        ),
        "set list",
    );
    assert_ok(
        &run_bitty(&root, &["config", "check", "--profile", "work"]),
        "check after list",
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn json_envelopes_have_a_stable_shape() {
    let root = scratch_root("json");
    let created = run_bitty(&root, &["config", "profile", "create", "work", "--json"]);
    assert_ok(&created, "create --json");
    let text = stdout(&created);
    assert_eq!(text.lines().count(), 1, "one envelope line: {text}");
    assert!(
        text.starts_with(
            "{\"v\":1,\"command\":\"config profile create\",\"ok\":true,\"result\":{\"kind\":\"profile\",\"action\":\"create\",\"name\":\"work\",\"path\":"
        ),
        "create envelope: {text}"
    );
    assert!(text.contains("\"changed\":true"), "{text}");

    write_profile(
        &root,
        "other",
        "return {\n    font = { family = \"Mono\", size = 13 },\n}\n",
    );
    let listed = run_bitty(&root, &["config", "profile", "list", "--json"]);
    assert_ok(&listed, "list --json");
    let text = stdout(&listed);
    assert!(text.contains("\"profiles\":["), "{text}");
    assert!(text.contains("\"name\":\"other\""), "{text}");
    assert!(text.contains("\"name\":\"work\""), "{text}");
    assert!(text.contains("\"count\":2"), "{text}");
    assert!(
        text.find("\"other\"").expect("other") < text.find("\"work\"").expect("work"),
        "names stay sorted: {text}"
    );

    let shown = run_bitty(&root, &["config", "profile", "show", "other", "--json"]);
    assert_ok(&shown, "show --json");
    let text = stdout(&shown);
    assert!(text.contains("\"layers\":["), "{text}");
    assert!(text.contains("\"keys\":["), "{text}");
    assert!(
        text.contains("\"key\":\"font.size\",\"value\":13,\"type\":\"number\""),
        "{text}"
    );

    let got = run_bitty(
        &root,
        &["config", "profile", "get", "other", "font.size", "--json"],
    );
    assert_ok(&got, "get --json");
    let text = stdout(&got);
    assert!(
        text.contains("\"key\":\"font.size\",\"value\":13,\"type\":\"number\""),
        "{text}"
    );
    assert!(text.contains("\"layer\":\"profile\""), "{text}");

    let missing = run_bitty(&root, &["config", "profile", "delete", "nope", "--json"]);
    assert_exit(&missing, 2, "missing --json");
    let text = stdout(&missing);
    assert!(text.contains("\"ok\":false"), "{text}");
    assert!(text.contains("\"code\":\"E_PROFILE_NOT_FOUND\""), "{text}");

    // `--json` composes before the `config` word too.
    let before = run_bitty(&root, &["--json", "config", "profile", "list"]);
    assert_ok(&before, "--json before config");
    assert!(stdout(&before).contains("\"ok\":true"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn outside_config_json_stays_an_unknown_flag() {
    let root = scratch_root("json-scope");
    let output = run_bitty(&root, &["--json", "list", "plugins"]);
    assert_exit(&output, 2, "bare --json");
    assert!(
        stderr(&output).contains("unknown flag '--json'"),
        "json stays scoped to config: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&root);
}
