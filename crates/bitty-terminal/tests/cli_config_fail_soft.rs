//! Binary-level fail-soft configuration startup proof (CTX-1490, issue #2580).
//!
//! Pre-fix, a present-but-invalid user file or profile aborted startup with
//! `validation error at 'window'`, so `bitty` and `bitty config check` both
//! exited 2 for the owner's partial `window = { opacity = ... }` profile.
//! These tests drive the built `bitty` binary with an isolated
//! `XDG_CONFIG_HOME`: startup must reach a tick and keep every valid part,
//! while `config check` stays strict (non-zero, exact field path).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated scratch root (config + HOME + profiles).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1490-{tag}-{}-{}",
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

/// Runs `bitty <args>` under the isolated root.
fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_LOG")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts a headless run started and reached a presented tick.
fn assert_reached_a_tick(output: &Output, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{context} must exit 0, stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stdout(output).contains("tick presented"),
        "{context} must reach a presented tick, stdout={:?}",
        stdout(output)
    );
}

#[test]
fn owner_partial_window_profile_starts_headless_and_ticks() {
    // Exact owner shape from the report: profile `default.lua` with
    // `window = { opacity = ... }` and no `padding`.
    let root = scratch_root("owner-window");
    write_profile(&root, "default", "return { window = { opacity = 0.9 } }\n");

    let startup = run_bitty(&root, &["--headless", "--profile", "default"]);
    assert_reached_a_tick(&startup, "partial-window profile startup");
    let startup_stderr = stderr(&startup);
    assert!(
        !startup_stderr.contains("validation error"),
        "a valid partial window must not report a validation error: {startup_stderr}"
    );

    // The strict diagnostic command accepts the same partial table now.
    let check = run_bitty(&root, &["config", "check", "--profile", "default"]);
    assert_eq!(
        check.status.code(),
        Some(0),
        "config check must accept a partial window, stdout={:?} stderr={:?}",
        stdout(&check),
        stderr(&check)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_user_table_is_ignored_at_startup_but_strict_check_fails() {
    let root = scratch_root("bad-table");
    // `secret-marker` is config content: diagnostics must never echo it.
    write_init(
        &root,
        concat!(
            "-- secret-marker: never print file contents\n",
            "return { font = { family = \"Mono\", size = 13.0 },\n",
            "  window = { opacity = 2.0, padding = 8 } }\n"
        ),
    );

    let startup = run_bitty(&root, &["--headless"]);
    assert_reached_a_tick(&startup, "startup with one bad table");
    let text = stderr(&startup);
    assert!(
        text.contains("ignoring 'window'"),
        "startup must name the ignored table: {text}"
    );
    assert!(
        text.contains("init.lua"),
        "startup must name the config path: {text}"
    );
    assert!(
        text.contains("window.opacity"),
        "startup must name the field path: {text}"
    );
    assert!(
        text.contains("ignored; using defaults and remaining layers"),
        "startup must state the fallback: {text}"
    );
    assert!(
        !text.contains("secret-marker"),
        "diagnostics must never echo file contents: {text}"
    );

    // Strict `config check` keeps the exact failure and non-zero exit.
    let check = run_bitty(&root, &["config", "check"]);
    assert_eq!(
        check.status.code(),
        Some(2),
        "config check must stay strict, stdout={:?} stderr={:?}",
        stdout(&check),
        stderr(&check)
    );
    let check_stderr = stderr(&check);
    assert!(
        check_stderr.contains("window"),
        "config check must report the field path: {check_stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_syntax_starts_with_remaining_layers() {
    let root = scratch_root("syntax");
    write_init(&root, "return { window = }\n");

    let startup = run_bitty(&root, &["--headless"]);
    assert_reached_a_tick(&startup, "startup with invalid syntax");
    let text = stderr(&startup);
    assert!(
        text.contains("ignoring invalid config file"),
        "startup must report the dropped file: {text}"
    );
    assert!(
        text.contains("ignored; using defaults and remaining layers"),
        "startup must state the fallback: {text}"
    );

    let check = run_bitty(&root, &["config", "check"]);
    assert_eq!(
        check.status.code(),
        Some(2),
        "config check must stay strict, stdout={:?} stderr={:?}",
        stdout(&check),
        stderr(&check)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_explicit_profile_stays_a_clear_fatal_error() {
    let root = scratch_root("missing-profile");
    let output = run_bitty(&root, &["--headless", "--profile", "ghost"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a requested-but-missing profile must stay fatal, stdout={:?} stderr={:?}",
        stdout(&output),
        stderr(&output)
    );
    let text = stderr(&output);
    assert!(text.contains("not found"), "stderr: {text}");
    assert!(text.contains("ghost"), "stderr: {text}");
    assert!(
        !stdout(&output).contains("tick presented"),
        "a fatal profile selection must not start"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_security_sensitive_value_never_aborts_or_escalates() {
    let root = scratch_root("security");
    write_init(
        &root,
        concat!(
            "return { decoration = { background_image_roots = { \"relative/one\" } },\n",
            "  font = { family = \"Mono\", size = 12.0 } }\n"
        ),
    );

    let startup = run_bitty(&root, &["--headless"]);
    assert_reached_a_tick(&startup, "startup with an invalid allowlist");
    let text = stderr(&startup);
    assert!(
        text.contains("ignoring 'decoration'"),
        "the invalid allowlist table must be dropped: {text}"
    );
    assert!(
        text.contains("ignored; using defaults and remaining layers"),
        "startup must state the fallback: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn whole_layer_drop_denies_profile_history_capture() {
    // CTX-1490 review finding 1 probe: a broken user layer used to let the
    // profile re-enable history capture. The whole-layer drop now denies every
    // security-sensitive element for the launch, so the profile cannot
    // resurrect transcript/command capture.
    let root = scratch_root("deny-history");
    write_init(&root, "return { font = }\n");
    write_profile(
        &root,
        "work",
        concat!(
            "return { history = { capture = { transcript = true, commands = true } },\n",
            "  font = { family = \"Mono\", size = 13.0 } }\n"
        ),
    );

    let startup = run_bitty(&root, &["--headless", "--profile", "work"]);
    assert_reached_a_tick(&startup, "startup with a whole-layer drop");
    let text = stderr(&startup);
    assert!(
        text.contains("ignoring invalid config file"),
        "the dropped user layer must be reported: {text}"
    );
    assert!(
        text.contains("profile 'work' from"),
        "the valid profile still loads: {text}"
    );
    assert!(
        text.contains("history capture off"),
        "the deny overlay must cover history: {text}"
    );
    assert!(
        text.contains("lower layers cannot re-enable them"),
        "the deny summary must explain the contract: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn denied_roots_clear_lower_layer_images_and_startup_ticks() {
    // CTX-1490 review blocker repro: the profile supplies a valid image plus
    // its approved root, while the user file has an unrelated invalid
    // geometric decoration. Pre-fix, roots were denied but the image survived,
    // so Runtime::new rejected the config and startup exited 1.
    let root = scratch_root("deny-images");
    let approved = root.join("approved");
    std::fs::create_dir_all(&approved).expect("approved root");
    let wall = approved.join("wall.png");
    // Content is irrelevant post-fix (no image is loaded); keep a PNG header
    // so the fixture is a plausible file rather than an empty path.
    std::fs::write(&wall, b"\x89PNG\r\n\x1a\n").expect("write image fixture");

    write_init(
        &root,
        "return { font = { family = \"Mono\", size = 13.0 },\n  decoration = { gaps_in = 99 } }\n",
    );
    write_profile(
        &root,
        "work",
        &format!(
            concat!(
                "return {{ decoration = {{ gaps_in = 2,\n",
                "  background_image = \"{wall}\",\n",
                "  background_image_roots = {{ \"{root}\" }} }},\n",
                " window = {{ background_image = \"{wall}\" }},\n",
                " views = {{ [\"*\"] = {{ background_image = \"{wall}\" }} }},\n",
                " font = {{ family = \"Mono\", size = 14.0 }} }}\n"
            ),
            wall = wall.display(),
            root = approved.display()
        ),
    );

    let startup = run_bitty(&root, &["--headless", "--profile", "work"]);
    assert_reached_a_tick(&startup, "startup with denied lower-layer images");
    let text = stderr(&startup);
    assert!(
        text.contains("ignoring 'decoration'"),
        "the invalid geometric decoration must be salvaged: {text}"
    );
    assert!(
        text.contains("background images disabled"),
        "the deny summary must cover images: {text}"
    );
    assert!(
        !text.contains("background image path denied"),
        "startup must not fail on a denied lower-layer image: {text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
