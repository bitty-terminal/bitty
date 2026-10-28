//! `bitty x` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty x`,
//! extension class, collision-free qualified route). This slice resolves the
//! route from static manifests and fails closed without executing plugin
//! code: unknown plugins and execution attempts are plugin errors (exit 4),
//! parse failures are usage errors (exit 2).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `x` dispatches before config load and GUI startup from static manifests
//! only, so every case here is headless-safe: no display, no instance, and
//! no plugin VM is ever loaded.

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str]) -> Output {
    // Isolate store discovery from the developer's installed plugins: the
    // default `bitty x --help` view now reads the same store as
    // `bitty plugin list` (bitty#2774).
    let data_home = std::env::temp_dir().join(format!("bitty-x-empty-data-{}", std::process::id()));
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_DATA_HOME", data_home)
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
fn x_help_lists_installed_plugins() {
    let out = run_bitty(&["x", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Installed plugins"), "help: {text}");
    assert!(text.contains("bitty-terminal."), "bundled set: {text}");
}

#[test]
fn x_help_and_alias_read_installed_store_plugins() {
    use bitty_runtime::plugin_runtime::SourceClass;
    use bitty_runtime::plugin_runtime::resolution::{PluginRecord, write_index};

    // Issue #2774: the discovery view must see the same store as
    // `bitty plugin list`, not only the bundled catalog.
    let data_home = std::env::temp_dir().join(format!(
        "bitty-x-store-{}-{}",
        std::process::id(),
        std::line!()
    ));
    let _ = std::fs::remove_dir_all(&data_home);
    let store = data_home.join("bitty").join("plugins");
    let package = store.join("packages/xuepoo.store/1.0.0");
    std::fs::create_dir_all(&package).expect("package dirs");
    std::fs::write(
        package.join("bitty-plugin.toml"),
        "[plugin]\nid = \"xuepoo.store\"\nname = \"Store plugin\"\nversion = \"1.0.0\"\n\
         description = \"installed from the store\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n\
         [capabilities]\n\n[lazy]\ncommands = [\"xuepoo.store:run\"]\nevents = []\n",
    )
    .expect("manifest");
    write_index(
        &store,
        &[PluginRecord {
            source_class: SourceClass::Registry,
            plugin_id: "xuepoo.store".to_string(),
            version: "1.0.0".to_string(),
            root: "packages/xuepoo.store/1.0.0".to_string(),
            manifest_hash: "a".repeat(64),
            content_digest: "b".repeat(64),
            enabled: true,
            granted: Vec::new(),
        }],
    )
    .expect("index");

    let run = |args: &[&str]| {
        Command::new(BITTY_BIN)
            .args(args)
            .env("XDG_DATA_HOME", &data_home)
            .output()
            .unwrap_or_else(|err| panic!("spawn {args:?}: {err}"))
    };
    let help = run(&["x", "--help"]);
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
    let text = stdout(&help);
    assert!(text.contains("xuepoo.store (1.0.0)"), "{text}");
    assert!(text.contains("xuepoo.store:run"), "{text}");

    let plugin_help = run(&["x", "xuepoo.store", "--help"]);
    assert_eq!(
        plugin_help.status.code(),
        Some(0),
        "{}",
        stderr(&plugin_help)
    );
    assert!(stdout(&plugin_help).contains("xuepoo.store:run"));

    // The short alias resolves to the store row (extension execution is
    // still a follow-up, so the route fails closed without running code).
    let route = run(&["x", "store", "run"]);
    assert_eq!(route.status.code(), Some(4), "{}", stderr(&route));
    let message = stderr(&route);
    assert!(message.contains("xuepoo.store"), "{message}");
    assert!(
        message.contains("extension execution is a follow-up"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(data_home);
}

#[test]
fn x_plugin_help_shows_static_commands() {
    // #1572 / CTX-0994: shell-integration only; workspace manifest retired.
    let out = run_bitty(&["x", "bitty-terminal.shell-integration", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("bitty-terminal.shell-integration"));
    // Retired workspace id is now unknown (plugin error, exit 4).
    let retired = run_bitty(&["x", "bitty-terminal.workspace", "--help"]);
    assert_eq!(
        retired.status.code(),
        Some(4),
        "stderr: {}",
        stderr(&retired)
    );
}

#[test]
fn x_unknown_plugin_is_plugin_error() {
    let out = run_bitty(&["x", "nope.missing", "render"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("nope.missing"));
    let out = run_bitty(&["x", "nope.missing", "render", "--format", "json"]);
    assert_eq!(out.status.code(), Some(4));
    let text = stdout(&out);
    assert!(text.contains("\"v\":1"), "envelope: {text}");
    assert!(text.contains("\"command\":\"x\""), "envelope: {text}");
    assert!(text.contains("\"ok\":false"), "envelope: {text}");
}

#[test]
fn x_execution_attempt_is_plugin_error_without_vm() {
    // Shell-integration declares no commands, so any command is a plugin error.
    let out = run_bitty(&["x", "bitty-terminal.shell-integration", "open"]);
    assert_eq!(out.status.code(), Some(4), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("bitty-terminal.shell-integration"));
}

#[test]
fn x_rejects_usage_errors() {
    for args in [
        vec!["x"],
        vec!["x", "bitty-terminal.shell-integration"],
        vec!["x", "bitty-terminal.shell-integration", "open", "--"],
        vec![
            "x",
            "bitty-terminal.shell-integration",
            "open",
            "--format",
            "yaml",
        ],
        vec!["--socket", "/tmp/x.sock", "x", "--help"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must fail closed: {}",
            stderr(&out)
        );
    }
}
