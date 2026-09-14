//! `bitty plugin` end-to-end dispatch proofs (CTX-0150, issue #244).
//!
//! Canonical: `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/product/plugin-roadmap.md` owner direction
//! 2026-09-03 (DEC-0007), `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/extensibility/package-management.md`
//! ("Managed manifest"), and `docs/specifications/cli-contract-rfc.md`.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`
//! with an isolated `XDG_CONFIG_HOME`, so the developer's real
//! `bitty-plugins.toml` is never touched. `plugin` dispatches before config
//! load and GUI startup: no display, no instance, no plugin VM, and no
//! plugin code is ever executed. Consent input is piped so the fail-closed
//! paths are proven, not implied.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{LuaValue, PluginRuntime, PluginRuntimeConfig, load_index};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

const PLUGIN: &str = "bitty-terminal.shell-integration";

/// Fresh isolated config root for one test.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-plugin-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run the binary with an isolated config root and optional piped consent.
///
/// Both XDG roots are pinned into the scratch home and the development plugin
/// override is cleared, so a test can never read or write the developer's real
/// config or plugin store.
fn run_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PLUGIN_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        None => command.stdin(Stdio::null()).output(),
        Some(answer) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn().expect("spawn bitty");
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(answer.as_bytes())
                .expect("write stdin");
            child.wait_with_output()
        }
    }
    .unwrap_or_else(|err| panic!("run {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn state_file(home: &Path) -> PathBuf {
    home.join("bitty").join("bitty-plugins.toml")
}

#[test]
fn plugin_list_is_local_and_creates_no_state() {
    let home = scratch_dir("list");
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(PLUGIN), "{text}");
    assert!(text.contains("available"), "{text}");
    assert!(!state_file(&home).exists(), "list must not write state");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_list_json_is_a_versioned_envelope() {
    let home = scratch_dir("list-json");
    let output = run_in(&home, &["plugin", "list", "--format", "json"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"v\":1"), "{text}");
    assert!(text.contains("\"command\":\"plugin\""), "{text}");
    assert!(text.contains("\"verb\":\"list\""), "{text}");
    assert!(text.contains("\"state\":\"available\""), "{text}");
    assert!(text.contains("\"pin_ok\":null"), "{text}");
    assert!(!state_file(&home).exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_full_lifecycle_live() {
    let home = scratch_dir("lifecycle");

    // install --yes: pin + consent non-interactively.
    let output = run_in(&home, &["plugin", "install", PLUGIN, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    let state = std::fs::read_to_string(state_file(&home)).expect("state written");
    assert!(
        state.contains(&format!("[plugins.\"{PLUGIN}\"]")),
        "{state}"
    );
    assert!(
        state.contains("granted = [\"terminal.semantic-read\"]"),
        "{state}"
    );
    assert!(state.contains("enabled = true"), "{state}");
    assert_eq!(state.matches("manifest_hash = ").count(), 1, "{state}");

    // list: enabled + pin ok + 1/1 granted.
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("enabled"), "{text}");
    assert!(text.contains("ok"), "{text}");
    assert!(text.contains("1/1"), "{text}");

    // info --format json exposes the hash and the granted capability.
    let output = run_in(&home, &["plugin", "info", PLUGIN, "--format", "json"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"manifest_hash\":\""), "{text}");
    assert!(text.contains("\"granted\":true"), "{text}");
    assert!(text.contains("Read structured terminal content"), "{text}");

    // disable then enable (idempotent toggles, grant kept).
    let output = run_in(&home, &["plugin", "disable", PLUGIN], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("disabled"), "{}", stdout(&output));
    let output = run_in(&home, &["plugin", "enable", PLUGIN], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("enabled"), "{}", stdout(&output));

    // remove is destructive: --force required, previous state backed up.
    let output = run_in(&home, &["plugin", "remove", PLUGIN], None);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(state_file(&home).exists());
    let output = run_in(&home, &["plugin", "remove", PLUGIN, "--force"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("removed"), "{}", stdout(&output));
    let state = std::fs::read_to_string(state_file(&home)).expect("state rewritten");
    assert!(!state.contains(PLUGIN), "{state}");
    let backup = home.join("bitty").join("bitty-plugins.toml.bak");
    assert!(backup.exists(), "backup must be kept");

    // CTX-0293: the capability-grant ledger, its backup, and the managed
    // directory are owner-only on Unix regardless of the process umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&state_file(&home)), 0o600, "manifest must be 0600");
        assert_eq!(mode(&backup), 0o600, "backup must be 0600");
        assert_eq!(mode(&home.join("bitty")), 0o700, "managed dir must be 0700");
    }

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_install_consent_decline_and_eof_fail_closed() {
    // Explicit decline: exit 1, prompt lists the capability, no state.
    let home = scratch_dir("decline");
    let output = run_in(&home, &["plugin", "install", PLUGIN], Some("n\n"));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("terminal.semantic-read"), "{text}");
    assert!(text.contains("[y/N]"), "{text}");
    assert!(!state_file(&home).exists(), "decline must not write state");

    // EOF: exit 1, no state.
    let output = run_in(&home, &["plugin", "install", PLUGIN], Some(""));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!state_file(&home).exists(), "EOF must not write state");

    // Approval via the prompt (no --yes) writes the record.
    let output = run_in(&home, &["plugin", "install", PLUGIN], Some("y\n"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(state_file(&home).exists(), "approval writes state");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_usage_and_plugin_errors_have_stable_exit_codes() {
    let home = scratch_dir("errors");

    // Unknown verb: usage error (2), usage text on stderr.
    let output = run_in(&home, &["plugin", "frobnicate"], None);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("usage: bitty plugin"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());

    // Well-formed but non-bundled id: plugin error (4).
    let output = run_in(
        &home,
        &["plugin", "install", "xuepoo.markdown", "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));

    // Malformed id: usage error (2).
    let output = run_in(&home, &["plugin", "install", "not_an_id", "--yes"], None);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // info for an unknown plugin in json mode emits a failure envelope.
    let output = run_in(
        &home,
        &["plugin", "info", "xuepoo.markdown", "--format", "json"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"ok\":false"), "{text}");
    assert!(text.contains("PluginNotFound"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_help_exits_zero_without_config_or_state() {
    let home = scratch_dir("help");
    let output = run_in(&home, &["plugin", "--help"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("usage: bitty plugin"), "{text}");
    assert!(text.contains("exit codes"), "{text}");
    assert!(!state_file(&home).exists());
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// External (non-bundled) package install: CTX-0406 end-to-end proof.
// ---------------------------------------------------------------------------

/// Isolated XDG data store root for one scratch home (`XDG_DATA_HOME=home`).
fn data_store(home: &Path) -> PathBuf {
    home.join("bitty").join("plugins")
}

/// Write an externally authored plugin source directory (the "third party"
/// package): manifest plus a `lua/init.lua` that registers one command.
fn write_external_fixture(home: &Path, dir_name: &str, version: &str, greeting: &str) -> PathBuf {
    write_external_fixture_with_id(home, dir_name, "xuepoo.external", version, greeting)
}

/// As [`write_external_fixture`], with an explicit plugin id.
fn write_external_fixture_with_id(
    home: &Path,
    dir_name: &str,
    id: &str,
    version: &str,
    greeting: &str,
) -> PathBuf {
    let dir = home.join(dir_name);
    std::fs::create_dir_all(dir.join("lua")).expect("fixture lua dir");
    let manifest = format!(
        "[plugin]\n\
         id = \"{id}\"\n\
         name = \"External CLI Fixture\"\n\
         version = \"{version}\"\n\
         description = \"externally authored CLI fixture\"\n\n\
         [compat]\n\
         bitty = \">=0.0.1,<1.0\"\n\
         plugin-api = \"^1.0\"\n\n\
         [capabilities]\n\
         platform.notify = true\n\n\
         [lazy]\n\
         commands = [\"{id}:greet\"]\n\
         events = []\n"
    );
    std::fs::write(dir.join("bitty-plugin.toml"), manifest).expect("fixture manifest");
    std::fs::write(
        dir.join("lua/init.lua"),
        format!(
            "bitty.commands.register({{\n  id = \"greet\",\n  title = \"Greet\",\n  \
             run = function() return \"{greeting}\" end,\n}})\nreturn {{}}\n"
        ),
    )
    .expect("fixture init");
    dir
}

#[test]
fn plugin_external_install_load_run_update_remove_live() {
    let home = scratch_dir("external");
    let source_v1 = write_external_fixture(&home, "source-v1", "1.0.0", "hello v1");
    let source_v1 = source_v1.display().to_string();
    let store = data_store(&home);

    // Install from the local directory with non-interactive consent.
    let output = run_in(&home, &["plugin", "install", &source_v1, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    assert!(
        store
            .join("packages/xuepoo.external/1.0.0/lua/init.lua")
            .is_file(),
        "staged package tree must exist"
    );
    let index = std::fs::read_to_string(store.join("current.json")).expect("store index");
    assert!(index.contains("xuepoo.external"), "{index}");
    assert!(index.contains("local-path"), "{index}");
    assert!(index.contains("1.0.0"), "{index}");
    assert!(index.contains("platform.notify"), "{index}");

    // list/info surface the installed package and its provenance.
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("xuepoo.external"), "{text}");
    assert!(text.contains("local-path"), "{text}");
    assert!(text.contains("enabled"), "{text}");
    let output = run_in(
        &home,
        &["plugin", "info", "xuepoo.external", "--format", "json"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"source\":\"local-path\""), "{text}");
    assert!(text.contains("\"granted\":true"), "{text}");

    // The real runtime the application uses loads the CLI-written store and
    // runs the externally authored command.
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store.clone()),
        ..PluginRuntimeConfig::default()
    });
    let discovered = rt.discover();
    assert_eq!(discovered.len(), 1, "{discovered:?}");
    let id = PluginId::new("xuepoo.external").expect("id");
    rt.activate(&id).expect("activate");
    assert_eq!(
        rt.dispatch_command(&id, "greet", &[]).expect("dispatch"),
        LuaValue::String("hello v1".to_string())
    );

    // Disable/enable flip the desired state atomically in the store index.
    let output = run_in(&home, &["plugin", "disable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!load_index(&store).expect("index")[0].enabled);
    let output = run_in(&home, &["plugin", "enable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("index")[0].enabled);

    // Update: a second local source stages the new version and retains the old.
    let source_v2 = write_external_fixture(&home, "source-v2", "2.0.0", "hello v2");
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            &source_v2.display().to_string(),
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("updated"), "{}", stdout(&output));
    assert!(store.join("packages/xuepoo.external/2.0.0").is_dir());
    assert!(store.join("packages/xuepoo.external/1.0.0").is_dir());
    let mut updated_rt = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store.clone()),
        ..PluginRuntimeConfig::default()
    });
    updated_rt.discover();
    updated_rt.activate(&id).expect("activate v2");
    assert_eq!(
        updated_rt
            .dispatch_command(&id, "greet", &[])
            .expect("dispatch"),
        LuaValue::String("hello v2".to_string())
    );

    // Remove requires --force and then deletes record plus tree.
    let output = run_in(&home, &["plugin", "remove", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let output = run_in(
        &home,
        &["plugin", "remove", "xuepoo.external", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("removed"), "{}", stdout(&output));
    assert!(!store.join("packages/xuepoo.external").exists());
    let output = run_in(&home, &["plugin", "list"], None);
    assert!(
        !stdout(&output).contains("xuepoo.external"),
        "{}",
        stdout(&output)
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_install_requires_consent() {
    let home = scratch_dir("external-consent");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let source = source.display().to_string();
    let store = data_store(&home);

    // Decline: nothing is staged.
    let output = run_in(&home, &["plugin", "install", &source], Some("n\n"));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("platform.notify"),
        "{}",
        stdout(&output)
    );
    assert!(!store.join("current.json").exists());

    // EOF: fails closed with nothing staged.
    let output = run_in(&home, &["plugin", "install", &source], Some(""));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(!store.join("current.json").exists());

    // Prompt approval stages the package.
    let output = run_in(&home, &["plugin", "install", &source], Some("y\n"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(store.join("current.json").is_file());

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_remote_source_fails_closed() {
    let home = scratch_dir("external-remote");
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            "https://example.com/plugin.git",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("remote sources are not implemented"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("current.json").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_bundled_id_is_rejected() {
    let home = scratch_dir("external-bundled");
    let source =
        write_external_fixture_with_id(&home, "source", "bitty-terminal.tabs", "1.0.0", "hello");
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("bundled catalog"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("packages").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_store_display_rejects_escaped_record_root() {
    let home = scratch_dir("store-escape");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let store = data_store(&home);

    // Plant a manifest outside the store and craft `current.json` to point at
    // it via `../`; a read-only command must never load it.
    let outside = home.join("bitty").join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::write(
        outside.join("bitty-plugin.toml"),
        "[plugin]\nid = \"xuepoo.external\"\nname = \"SHOULD NOT LOAD\"\nversion = \"9.9.9\"\n\
         description = \"escaped record root\"\n\n[compat]\nplugin-api = \"^1.0\"\n",
    )
    .expect("outside manifest");
    let index_path = store.join("current.json");
    let index = std::fs::read_to_string(&index_path).expect("index");
    let crafted = index.replace(
        "\"root\":\"packages/xuepoo.external/1.0.0\"",
        "\"root\":\"../outside\"",
    );
    assert_ne!(crafted, index, "the record root must be rewritten");
    std::fs::write(&index_path, crafted).expect("crafted index");

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        !text.contains("SHOULD NOT LOAD"),
        "an escaped record root must not be read: {text}"
    );
    assert!(text.contains("xuepoo.external"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}
