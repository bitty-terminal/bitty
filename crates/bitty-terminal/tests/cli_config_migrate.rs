//! Binary-level `bitty config migrate` proofs (bitty#2923).
//!
//! Every case uses an isolated `XDG_CONFIG_HOME`; the owner's real config is
//! never touched. `config migrate`, `config validate`, and `config check` are
//! pre-runtime operations, so no display, instance, IPC peer, or plugin VM is
//! required.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// The pre-version (version 0) fixture shape: no `config_version` key.
const VERSION_ZERO: &str = "return {\n    font = { family = \"Mono\", size = 13.0 },\n}\n";

/// Fresh isolated scratch root with a `bitty/` config directory.
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1720-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("bitty").join("profiles")).expect("config dir");
    dir
}

fn init_path(root: &Path) -> PathBuf {
    root.join("bitty").join("init.lua")
}

fn write_init(root: &Path, body: &str) {
    std::fs::write(init_path(root), body).expect("write init.lua");
}

/// Runs `bitty <args>` under the isolated root.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
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

fn assert_ok(output: &Output, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{label} must succeed, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

fn backup_entries(root: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root.join("bitty"))
        .expect("config dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains(".bak-"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn version_zero_migrates_with_a_backup_then_second_run_is_a_no_op() {
    let root = scratch_root("migrate");
    write_init(&root, VERSION_ZERO);
    let path = init_path(&root);

    let output = run_bitty(&root, &["config", "migrate"]);
    assert_ok(&output, "config migrate");
    let text = stdout(&output);
    assert!(
        text.contains("config_version = (absent) -> 1"),
        "the change list must name the version change: {text:?}"
    );
    assert!(
        text.contains("migrated") && text.contains("from config_version 0 to 1"),
        "the success line must name both versions: {text:?}"
    );

    let migrated = std::fs::read_to_string(&path).expect("migrated file");
    assert!(
        migrated.contains("config_version = 1,"),
        "the file must be stamped at version 1: {migrated:?}"
    );
    assert!(
        migrated.contains("font = { family = \"Mono\", size = 13.0 }"),
        "every other byte of the user shape must survive: {migrated:?}"
    );

    let backups = backup_entries(&root);
    assert_eq!(backups.len(), 1, "one backup expected: {backups:?}");
    assert_eq!(
        backups[0],
        path.with_file_name("init.lua.bak-0"),
        "the backup name carries the source version"
    );
    assert_eq!(
        std::fs::read_to_string(&backups[0]).expect("backup bytes"),
        VERSION_ZERO,
        "the backup must hold the original bytes"
    );

    let before_second = std::fs::read(&path).expect("file bytes before second run");
    let second = run_bitty(&root, &["config", "migrate"]);
    assert_ok(&second, "second config migrate");
    assert!(
        stdout(&second).contains("nothing to migrate"),
        "a current file must be a no-op: {:?}",
        stdout(&second)
    );
    assert_eq!(
        std::fs::read(&path).expect("file bytes after second run"),
        before_second,
        "the no-op must not touch the file"
    );
    assert_eq!(
        backup_entries(&root).len(),
        1,
        "the no-op must not write another backup"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dry_run_leaves_the_file_byte_identical() {
    let root = scratch_root("dry-run");
    write_init(&root, VERSION_ZERO);
    let path = init_path(&root);
    let before = std::fs::read(&path).expect("original bytes");

    let output = run_bitty(&root, &["config", "migrate", "--dry-run"]);
    assert_ok(&output, "config migrate --dry-run");
    assert!(
        stdout(&output).contains("dry run") && stdout(&output).contains("not written"),
        "the dry run must say nothing was written: {:?}",
        stdout(&output)
    );
    assert_eq!(
        std::fs::read(&path).expect("bytes after dry run"),
        before,
        "the dry run must leave the file byte-identical"
    );
    assert!(
        backup_entries(&root).is_empty(),
        "the dry run must not create a backup"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_mode_refuses_and_never_writes() {
    let root = scratch_root("safe");
    write_init(&root, VERSION_ZERO);
    let path = init_path(&root);
    let before = std::fs::read(&path).expect("original bytes");

    let output = run_bitty(&root, &["--safe", "config", "migrate"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "safe mode must refuse: stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("--safe is not supported"),
        "the refusal must be explicit: {:?}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read(&path).expect("bytes after safe refusal"),
        before
    );
    assert!(
        backup_entries(&root).is_empty(),
        "safe mode must not create a backup"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_newer_version_is_refused_with_the_version_named() {
    let root = scratch_root("newer");
    let body = "return { config_version = 2 }\n";
    write_init(&root, body);
    let path = init_path(&root);

    let output = run_bitty(&root, &["config", "migrate"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a newer version must fail closed: stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("config_version 2"),
        "the refusal must name the version: {:?}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("file bytes"),
        body,
        "a refused file must stay untouched"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validate_and_check_report_the_version_and_migration_hint() {
    let root = scratch_root("validate");
    write_init(&root, VERSION_ZERO);

    let validate = run_bitty(&root, &["config", "validate"]);
    assert_ok(&validate, "config validate on version 0");
    let text = stdout(&validate);
    assert!(
        text.contains("config_version 0") && text.contains("migration available"),
        "validate must report version 0 and the migration hint: {text:?}"
    );

    let check = run_bitty(&root, &["config", "check"]);
    assert_ok(&check, "config check on version 0");
    let text = stdout(&check);
    assert!(
        text.contains("config_version = 0 (migration available: bitty config migrate)"),
        "check must report the version row: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn version_one_validates_without_a_migration_hint() {
    let root = scratch_root("current");
    write_init(&root, "return { config_version = 1 }\n");

    let output = run_bitty(&root, &["config", "validate"]);
    assert_ok(&output, "config validate on version 1");
    let text = stdout(&output);
    assert!(
        text.contains("config_version 1 (current)"),
        "a current file must say so: {text:?}"
    );
    assert!(
        !text.contains("migration available"),
        "a current file has nothing to migrate: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn no_backup_skips_the_backup_and_collisions_number_the_suffix() {
    let root = scratch_root("backup");
    write_init(&root, VERSION_ZERO);
    let path = init_path(&root);

    let no_backup = run_bitty(&root, &["config", "migrate", "--no-backup"]);
    assert_ok(&no_backup, "config migrate --no-backup");
    assert!(
        backup_entries(&root).is_empty(),
        "--no-backup must not create a backup"
    );

    // Reset to the version-0 shape and pre-create the default backup name.
    write_init(&root, VERSION_ZERO);
    let taken = path.with_file_name("init.lua.bak-0");
    std::fs::write(&taken, "older backup\n").expect("pre-create backup");

    let output = run_bitty(&root, &["config", "migrate"]);
    assert_ok(&output, "config migrate with a taken backup name");
    assert_eq!(
        std::fs::read_to_string(&taken).expect("older backup bytes"),
        "older backup\n",
        "an existing backup must never be overwritten"
    );
    let backups = backup_entries(&root);
    assert!(
        backups.contains(&path.with_file_name("init.lua.bak-0.1")),
        "a collision must pick a numbered suffix: {backups:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profile_flag_migrates_the_named_profile_file() {
    let root = scratch_root("profile");
    let profile = root.join("bitty").join("profiles").join("work.lua");
    std::fs::write(&profile, VERSION_ZERO).expect("write profile");

    let output = run_bitty(&root, &["config", "migrate", "--profile", "work"]);
    assert_ok(&output, "config migrate --profile work");
    let migrated = std::fs::read_to_string(&profile).expect("migrated profile");
    assert!(
        migrated.contains("config_version = 1,"),
        "the named profile must be stamped: {migrated:?}"
    );
    assert!(
        profile.with_file_name("work.lua.bak-0").exists(),
        "the profile backup must be written beside it"
    );
    assert!(
        !init_path(&root).exists(),
        "the user init.lua must not be created or touched"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn json_format_reports_the_change_list_and_backup() {
    let root = scratch_root("json");
    write_init(&root, VERSION_ZERO);
    let path = init_path(&root);

    let output = run_bitty(&root, &["config", "migrate", "--format", "json"]);
    assert_ok(&output, "config migrate --format json");
    let text = stdout(&output);
    assert!(
        text.contains("\"kind\":\"config-migration\""),
        "the envelope kind must be typed: {text:?}"
    );
    assert!(
        text.contains("\"from\":0") && text.contains("\"to\":1"),
        "the envelope must name both versions: {text:?}"
    );
    assert!(
        text.contains("\"key\":\"config_version\",\"from\":null,\"to\":\"1\""),
        "the change list must name the absent old value: {text:?}"
    );
    assert!(
        text.contains(&format!(
            "\"backup\":\"{}\"",
            path.with_file_name("init.lua.bak-0").display()
        )),
        "the envelope must report the backup path: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
