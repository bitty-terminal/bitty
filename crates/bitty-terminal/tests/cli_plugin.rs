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
        // The scp-like `git@…` case must never block on a host-key or
        // password prompt against real SSH port 22: prompts are disabled and
        // SSH is forced non-interactive onto closed port 9 (refused fast).
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=2 -p 9",
        )
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

    // Explicit enable is the only activation transition (grant kept).
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
    // Fetch-adjacent posture: helpers resolve via PATH and inherit the
    // system proxy env plus the system CA store (no bundled certs).
    assert!(text.contains("resolve via PATH"), "{text}");
    assert!(text.contains("system proxy env"), "{text}");
    assert!(text.contains("system CA store"), "{text}");
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

/// Write a zero-capability external plugin for the fresh-install activation
/// boundary.  With no capability consent needed, `--yes` must still not turn
/// a newly installed package into an active package.
fn write_zero_capability_fixture(
    home: &Path,
    dir_name: &str,
    id: &str,
    version: &str,
    greeting: &str,
) -> PathBuf {
    let dir = home.join(dir_name);
    std::fs::create_dir_all(dir.join("lua")).expect("lua dir");
    let manifest = format!(
        "[plugin]\n\
         id = \"{id}\"\n\
         name = \"Zero Capability Fixture\"\n\
         version = \"{version}\"\n\
         description = \"zero-capability CLI fixture\"\n\n\
         [compat]\n\
         bitty = \">=0.0.1,<1.0\"\n\
         plugin-api = \"^1.0\"\n\n\
         [lazy]\n\
         commands = [\"{id}:greet\"]\n\
         events = []\n"
    );
    std::fs::write(dir.join("bitty-plugin.toml"), manifest).expect("manifest");
    std::fs::write(
        dir.join("lua/init.lua"),
        format!(
            "bitty.commands.register({{\n  id = \"greet\",\n  title = \"Greet\",\n  run = function() return \"{greeting}\" end,\n}})\nreturn {{}}\n"
        ),
    )
    .expect("init");
    dir
}

#[test]
fn plugin_fresh_zero_capability_install_is_staged_until_explicit_enable() {
    let home = scratch_dir("fresh-disabled");
    let source = write_zero_capability_fixture(
        &home,
        "source-zero-capability",
        "xuepoo.fresh",
        "1.0.0",
        "fresh plugin ran",
    );
    let source = source.display().to_string();
    let store = data_store(&home);

    // `--yes` has no capability prompt to gate, but it must not authorize
    // activation. The install only stages and records the package.
    let output = run_in(&home, &["plugin", "install", &source, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let records = load_index(&store).expect("store index");
    assert_eq!(records.len(), 1);
    assert!(!records[0].enabled, "fresh install must remain disabled");

    // A fresh runtime startup discovers no enabled package and therefore does
    // not execute the package's init.lua.
    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store.clone()),
        ..PluginRuntimeConfig::default()
    });
    assert!(runtime.discover().is_empty());
    assert!(runtime.discovered_ids().is_empty());

    // The separate explicit enable operation authorizes discovery/activation.
    let output = run_in(&home, &["plugin", "enable", "xuepoo.fresh"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("store index")[0].enabled);
    assert_eq!(runtime.discover().len(), 1);
    let id = PluginId::new("xuepoo.fresh").expect("id");
    runtime
        .activate(&id)
        .expect("explicitly enabled plugin activates");
    assert_eq!(
        runtime
            .dispatch_command(&id, "greet", &[])
            .expect("dispatch"),
        LuaValue::String("fresh plugin ran".to_string())
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_install_does_not_parse_or_execute_disabled_package_code() {
    let home = scratch_dir("install-no-exec");
    let source = write_zero_capability_fixture(
        &home,
        "source-invalid-lua",
        "xuepoo.invalid-lua",
        "1.0.0",
        "unused",
    );
    // The package body is intentionally invalid Lua. Installation must only
    // validate and stage untrusted bytes; parsing/execution belongs after the
    // explicit enable boundary.
    std::fs::write(source.join("lua/init.lua"), "this is not valid lua\n")
        .expect("invalid fixture");
    let source = source.display().to_string();
    let store = data_store(&home);

    let output = run_in(&home, &["plugin", "install", &source, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!load_index(&store).expect("store index")[0].enabled);

    let mut runtime = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store),
        ..PluginRuntimeConfig::default()
    });
    assert!(runtime.discover().is_empty());

    let _ = std::fs::remove_dir_all(&home);
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

/// Write an external fixture with an explicit capability set.
fn write_external_fixture_with_capabilities(
    home: &Path,
    dir_name: &str,
    id: &str,
    version: &str,
    greeting: &str,
    capabilities: &[&str],
) -> PathBuf {
    let dir = home.join(dir_name);
    std::fs::create_dir_all(dir.join("lua")).expect("fixture lua dir");
    let capability_lines = capabilities
        .iter()
        .map(|capability| format!("{capability} = true\n"))
        .collect::<String>();
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
         {capability_lines}\n\
         [lazy]\n\
         commands = [\"{id}:greet\"]\n\
         events = []\n"
    );
    std::fs::write(dir.join("bitty-plugin.toml"), manifest).expect("fixture manifest");
    std::fs::write(
        dir.join("lua/init.lua"),
        format!(
            "bitty.commands.register({{\n  id = \"greet\",\n  title = \"Greet\",\n  run = function() return \"{greeting}\" end,\n}})\nreturn {{}}\n"
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
    assert!(text.contains("disabled"), "{text}");
    let output = run_in(
        &home,
        &["plugin", "info", "xuepoo.external", "--format", "json"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"source\":\"local-path\""), "{text}");
    assert!(text.contains("\"granted\":true"), "{text}");

    // The real runtime loads the CLI-written store but skips the fresh
    // disabled package during startup.
    let mut rt = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store.clone()),
        ..PluginRuntimeConfig::default()
    });
    let discovered = rt.discover();
    assert!(discovered.is_empty(), "{discovered:?}");
    let id = PluginId::new("xuepoo.external").expect("id");
    assert!(rt.discovered_ids().is_empty());

    // Explicit enable authorizes the runtime to load and execute it.
    let output = run_in(&home, &["plugin", "enable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("index")[0].enabled);
    assert_eq!(rt.discover().len(), 1);
    rt.activate(&id).expect("activate");
    assert_eq!(
        rt.dispatch_command(&id, "greet", &[]).expect("dispatch"),
        LuaValue::String("hello v1".to_string())
    );

    // Update while enabled: a second local source stages the new version,
    // retains the old, and preserves enabled state without another enable.
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
    assert!(load_index(&store).expect("index")[0].enabled);

    // Disable/enable flip the desired state atomically in the store index.
    let output = run_in(&home, &["plugin", "disable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!load_index(&store).expect("index")[0].enabled);

    let source_v3 = write_external_fixture(&home, "source-v3", "3.0.0", "hello v3");
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            &source_v3.display().to_string(),
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("updated"), "{}", stdout(&output));
    assert!(!load_index(&store).expect("index")[0].enabled);
    assert!(store.join("packages/xuepoo.external/3.0.0").is_dir());

    let mut updated_rt = PluginRuntime::new(PluginRuntimeConfig {
        store_root: Some(store.clone()),
        ..PluginRuntimeConfig::default()
    });
    assert!(updated_rt.discover().is_empty());
    let output = run_in(&home, &["plugin", "enable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("index")[0].enabled);
    assert_eq!(updated_rt.discover().len(), 1);
    updated_rt.activate(&id).expect("activate v3");
    assert_eq!(
        updated_rt
            .dispatch_command(&id, "greet", &[])
            .expect("dispatch"),
        LuaValue::String("hello v3".to_string())
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
fn plugin_capability_update_decline_and_eof_leave_index_and_grants_unchanged() {
    let home = scratch_dir("external-capability-update");
    let source_v1 = write_external_fixture(&home, "source-v1", "1.0.0", "hello v1");
    let source_v1 = source_v1.display().to_string();
    let store = data_store(&home);

    let output = run_in(&home, &["plugin", "install", &source_v1, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let before_index = std::fs::read(store.join("current.json")).expect("index bytes");
    let before_records = load_index(&store).expect("records");
    let source_v2 = write_external_fixture_with_capabilities(
        &home,
        "source-v2",
        "xuepoo.external",
        "2.0.0",
        "hello v2",
        &["platform.notify", "terminal.semantic-read"],
    );
    let source_v2 = source_v2.display().to_string();

    // A capability increase requires consent and must leave every committed
    // index field and grant unchanged when declined.
    let output = run_in(&home, &["plugin", "install", &source_v2], Some("n\n"));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        std::fs::read(store.join("current.json")).expect("index bytes"),
        before_index
    );
    assert_eq!(load_index(&store).expect("records"), before_records);
    assert!(!store.join("packages/xuepoo.external/2.0.0").exists());

    // EOF is also fail-closed with the same complete state preservation.
    let output = run_in(&home, &["plugin", "install", &source_v2], Some(""));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(
        std::fs::read(store.join("current.json")).expect("index bytes"),
        before_index
    );
    assert_eq!(load_index(&store).expect("records"), before_records);
    assert!(!store.join("packages/xuepoo.external/2.0.0").exists());

    let _ = std::fs::remove_dir_all(&home);
}

/// Post-install next steps for store installs (CTX-1268, bitty#2144).
///
/// Golden: `install`/`enable` success prints the enable command, the store
/// package root, the canonical Lua settings path, the `config show` and
/// `config scaffold` pointers, the manifest's `[hints] next_steps`, and the
/// `info` pointer. The hint never renders a legacy TOML example, even when
/// the manifest declares `[settings]` defaults.
#[test]
fn plugin_store_install_and_enable_print_next_steps() {
    let home = scratch_dir("next-steps");
    let dir = home.join("source-onboard");
    std::fs::create_dir_all(dir.join("lua")).expect("fixture lua dir");
    let manifest = "[plugin]\n\
         id = \"xuepoo.onboard\"\n\
         name = \"Onboarding Fixture\"\n\
         version = \"1.0.0\"\n\
         description = \"next-steps fixture\"\n\n\
         [compat]\n\
         bitty = \">=0.0.1,<1.0\"\n\
         plugin-api = \"^1.0\"\n\n\
         [capabilities]\n\
         platform.notify = true\n\n\
         [lazy]\n\
         commands = [\"xuepoo.onboard:greet\"]\n\
         events = []\n\n\
         [settings]\n\
         show = { type = \"bool\", default = true }\n\
         edge = { type = \"string\", default = \"bottom\" }\n\n\
         [hints]\n\
         next_steps = [\"Pin the bar module in your config, then restart bitty.\"]\n";
    std::fs::write(dir.join("bitty-plugin.toml"), manifest).expect("fixture manifest");
    std::fs::write(
        dir.join("lua/init.lua"),
        "bitty.commands.register({\n  id = \"greet\",\n  title = \"Greet\",\n  \
         run = function() return \"hi\" end,\n})\nreturn {}\n",
    )
    .expect("fixture init");

    // Install success prints manifest-driven next steps.
    let output = run_in(
        &home,
        &["plugin", "install", &dir.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("installed"), "{text}");
    assert!(text.contains("Next steps for 'xuepoo.onboard':"), "{text}");
    assert!(
        text.contains("enable: bitty plugin enable xuepoo.onboard"),
        "{text}"
    );
    // Path separators are platform-native (`\` on Windows): normalize
    // before matching the store/config layout.
    let portable = text.replace('\\', "/");
    assert!(portable.contains("packages/xuepoo.onboard/1.0.0"), "{text}");
    assert!(portable.contains("plugins/xuepoo.onboard.lua"), "{text}");
    assert!(!portable.contains(".toml"), "{text}");
    assert!(
        text.contains("inspect: bitty plugin config show xuepoo.onboard"),
        "{text}"
    );
    assert!(
        text.contains("scaffold: bitty plugin config scaffold xuepoo.onboard"),
        "{text}"
    );
    // Declared settings no longer leak a legacy TOML snippet into the hint.
    assert!(!text.contains("show = true"), "{text}");
    assert!(!text.contains("edge = \"bottom\""), "{text}");
    assert!(!text.contains("minimal example"), "{text}");
    assert!(
        text.contains("- Pin the bar module in your config, then restart bitty."),
        "{text}"
    );
    assert!(
        text.contains("more: bitty plugin info xuepoo.onboard"),
        "{text}"
    );

    // Store enable success (after a disable) prints them again.
    let output = run_in(&home, &["plugin", "disable", "xuepoo.onboard"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run_in(&home, &["plugin", "enable", "xuepoo.onboard"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("enabled"), "{text}");
    assert!(text.contains("Next steps for 'xuepoo.onboard':"), "{text}");
    assert!(
        text.contains("enable: bitty plugin enable xuepoo.onboard"),
        "{text}"
    );
    let portable = text.replace('\\', "/");
    assert!(portable.contains("plugins/xuepoo.onboard.lua"), "{text}");
    assert!(!portable.contains(".toml"), "{text}");
    assert!(
        text.contains("inspect: bitty plugin config show xuepoo.onboard"),
        "{text}"
    );
    assert!(
        text.contains("scaffold: bitty plugin config scaffold xuepoo.onboard"),
        "{text}"
    );
    assert!(!text.contains("edge = \"bottom\""), "{text}");
    assert!(
        text.contains("more: bitty plugin info xuepoo.onboard"),
        "{text}"
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
    // CTX-1103 (#1901): allowed git spellings clone via fixed-argv system
    // git; rejected schemes fail closed with a naming diagnostic. An
    // unroutable https git URL fails at clone time (exit 1) with nothing
    // staged.
    let home = scratch_dir("external-remote");
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            "https://127.0.0.1:9/no-such-plugin.git",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("`git clone` failed"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("current.json").exists());

    // http downgrade fails closed with a naming diagnostic (exit 4).
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            "http://example.com/plugin.git",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("refusing http://"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("current.json").exists());

    // file escape fails closed with a naming diagnostic (exit 4).
    let output = run_in(
        &home,
        &["plugin", "install", "file:///tmp/plugin.git", "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("refusing file://"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("current.json").exists());

    // ssh without git@ fails closed with a naming diagnostic (exit 4).
    let output = run_in(
        &home,
        &["plugin", "install", "ssh://example.com/plugin.git", "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("refusing ssh://"),
        "{}",
        stderr(&output)
    );
    assert!(!data_store(&home).join("current.json").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_git_spellings_are_accepted_as_sources() {
    // The three allowed spellings reach the git fetch path (not the bundled
    // id path): unroutable hosts fail at clone (exit 1), never as unknown
    // plugin (exit 4 with UnknownPlugin) and never with usage (exit 2).
    let home = scratch_dir("external-git-spellings");
    for url in [
        "https://127.0.0.1:9/a/b.git",
        "git+https://127.0.0.1:9/a/b.git",
        "git@127.0.0.1:a/b.git",
    ] {
        let output = run_in(&home, &["plugin", "install", url, "--yes"], None);
        assert_eq!(output.status.code(), Some(1), "{url}: {}", stderr(&output));
        assert!(
            stderr(&output).contains("`git clone` failed"),
            "{url}: {}",
            stderr(&output)
        );
    }
    assert!(!data_store(&home).join("current.json").exists());
    let _ = std::fs::remove_dir_all(&home);
}

/// Pack a source dir into an archive via the system helper under test.
fn pack_archive(source: &Path, archive: &Path, kind: &str) {
    let status = match kind {
        "tar.gz" => std::process::Command::new("tar")
            .args([
                "-czf",
                &archive.display().to_string(),
                "-C",
                &source.display().to_string(),
                ".",
            ])
            .status()
            .expect("run tar"),
        "tar.zst" => std::process::Command::new("tar")
            .args([
                "--use-compress-program=unzstd",
                "-cf",
                &archive.display().to_string(),
                "-C",
                &source.display().to_string(),
                ".",
            ])
            .status()
            .expect("run tar"),
        "zip" => {
            let status = std::process::Command::new("zip")
                .args(["-r", &archive.display().to_string(), "."])
                .current_dir(source)
                .status();
            match status {
                Ok(status) => status,
                Err(_) => {
                    // `zip` may be absent where only `unzip` ships; fall back
                    // to python for packing (unpack still uses system unzip).
                    std::process::Command::new("python3")
                        .args([
                            "-c",
                            &format!(
                                "import zipfile,pathlib; z=zipfile.ZipFile({archive:?},'w',zipfile.ZIP_STORED); \
                                 [z.write(p,p.relative_to({source:?})) for p in pathlib.Path({source:?}).rglob('*') if p.is_file()]"
                            ),
                        ])
                        .status()
                        .expect("run python3")
                }
            }
        }
        _ => panic!("unknown archive kind {kind}"),
    };
    assert!(status.success(), "pack {kind} failed: {status}");
}

#[test]
fn plugin_external_archive_install_and_hostile_members_fail_closed() {
    let home = scratch_dir("external-archive");
    let store = data_store(&home);

    // Happy path: .tar.gz installs exactly like the unpacked directory.
    let source = write_external_fixture(&home, "arch-src", "1.0.0", "hello arc");
    let archive = home.join("pkg.tar.gz");
    pack_archive(&source, &archive, "tar.gz");
    let output = run_in(
        &home,
        &["plugin", "install", &archive.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    assert!(
        store
            .join("packages/xuepoo.external/1.0.0/lua/init.lua")
            .is_file()
    );
    // Remove before the hostile cases so they must prove nothing is staged.
    let output = run_in(
        &home,
        &["plugin", "remove", "xuepoo.external", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // Happy path: .zip installs as well (skip when unzip is absent, e.g.
    // Windows without unzip: the install must fail closed, tested below).
    if std::process::Command::new("unzip")
        .arg("-v")
        .output()
        .is_ok()
    {
        let zip_archive = home.join("pkg.zip");
        pack_archive(&source, &zip_archive, "zip");
        let output = run_in(
            &home,
            &[
                "plugin",
                "install",
                &zip_archive.display().to_string(),
                "--yes",
            ],
            None,
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let output = run_in(
            &home,
            &["plugin", "remove", "xuepoo.external", "--force"],
            None,
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }

    // Hostile: `../` member fails closed (exit 4), nothing staged.
    let hostile = home.join("hostile.tar.gz");
    std::process::Command::new("python3")
        .args([
            "-c",
            &format!(
                "import tarfile,io; tf=tarfile.open({archive:?},'w:gz'); \
                 ti=tarfile.TarInfo('../evil.txt'); d=b'x'; ti.size=len(d); tf.addfile(ti,io.BytesIO(d)); tf.close()",
                archive = hostile.display().to_string(),
            ),
        ])
        .status()
        .expect("craft hostile tar");
    let output = run_in(
        &home,
        &["plugin", "install", &hostile.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("refusing hostile archive"),
        "{}",
        stderr(&output)
    );
    // The happy-path package was removed above; a hostile archive must not
    // stage anything new (the index file itself persists as an empty list).
    assert!(!store.join("packages/xuepoo.external").exists());
    if store.join("current.json").is_file() {
        let index = std::fs::read_to_string(store.join("current.json")).expect("index");
        assert!(!index.contains("xuepoo.external"), "{index}");
    }

    // Hostile: absolute member fails closed (exit 4), nothing staged.
    let hostile_abs = home.join("hostile-abs.zip");
    std::process::Command::new("python3")
        .args([
            "-c",
            &format!(
                "import zipfile; z=zipfile.ZipFile({archive:?},'w'); z.writestr('/tmp/absolute.txt','x'); z.close()",
                archive = hostile_abs.display().to_string(),
            ),
        ])
        .status()
        .expect("craft hostile zip");
    // When unzip is absent the install fails closed as missing-helper (1);
    // otherwise it fails as hostile member (4). Either is fail-closed.
    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            &hostile_abs.display().to_string(),
            "--yes",
        ],
        None,
    );
    assert!(
        output.status.code() == Some(4) || output.status.code() == Some(1),
        "{}",
        stderr(&output)
    );
    assert!(!store.join("packages/xuepoo.external").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_archive_zip_bomb_fails_closed_pre_extraction() {
    // CTX-1103 CodeRabbit MAJOR: a highly compressible tar.gz whose member
    // names are all valid (so the traversal audit passes) but whose declared
    // unpacked bytes exceed the 64 MiB cap must fail closed at the size gate
    // before anything is extracted — exit 4 naming the unpacked total, with
    // no store record staged.
    let home = scratch_dir("external-zipbomb");
    let store = data_store(&home);
    let bomb = home.join("bomb.tar.gz");
    let status = std::process::Command::new("python3")
        .args([
            "-c",
            &format!(
                "import tarfile; tf=tarfile.open({archive:?},'w:gz'); \
                 import io; \
                 m='[plugin]\\nid = \"xuepoo.bomb\"\\nname = \"B\"\\nversion = \"1.0.0\"\\ndescription = \"b\"\\n'; \
                 ti=tarfile.TarInfo('bitty-plugin.toml'); ti.size=len(m); tf.addfile(ti,io.BytesIO(m.encode())); \
                 big=65*1024*1024; ti=tarfile.TarInfo('big.bin'); ti.size=big; tf.addfile(ti,io.BytesIO(bytes(big))); tf.close()",
                archive = bomb.display().to_string(),
            ),
        ])
        .status()
        .expect("craft bomb tar");
    assert!(status.success());
    let output = run_in(
        &home,
        &["plugin", "install", &bomb.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unpacked bytes")
            && stderr(&output).contains("refusing hostile archive"),
        "{}",
        stderr(&output)
    );
    assert!(!store.join("current.json").exists());
    assert!(!store.join("packages").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_external_bundled_id_is_rejected() {
    let home = scratch_dir("external-bundled");
    // #1572 / CTX-0994: shell-integration is the remaining bundled id.
    let source = write_external_fixture_with_id(
        &home,
        "source",
        "bitty-terminal.shell-integration",
        "1.0.0",
        "hello",
    );
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

// ---------------------------------------------------------------------------
// `bitty plugin config show|scaffold` (#2246)
// ---------------------------------------------------------------------------

/// External fixture with declared `[settings]` (issue #2246).
fn write_config_fixture(home: &Path) -> PathBuf {
    let dir = home.join("source-config");
    std::fs::create_dir_all(dir.join("lua")).expect("fixture lua dir");
    let manifest = "[plugin]\n\
         id = \"xuepoo.cfg\"\n\
         name = \"Config Fixture\"\n\
         version = \"1.0.0\"\n\
         description = \"per-plugin settings fixture\"\n\n\
         [compat]\n\
         bitty = \">=0.0.1,<1.0\"\n\
         plugin-api = \"^1.0\"\n\n\
         [capabilities]\n\
         platform.notify = true\n\n\
         [lazy]\n\
         commands = [\"xuepoo.cfg:greet\"]\n\
         events = []\n\n\
         [settings]\n\
         show = { type = \"bool\", default = true }\n\
         edge = { type = \"string\", default = \"bottom\" }\n\
         limit = { type = \"int\", default = 8 }\n\
         modules = { type = \"string-array\", default = [\"workspaces\", \"clock\"] }\n";
    std::fs::write(dir.join("bitty-plugin.toml"), manifest).expect("fixture manifest");
    std::fs::write(
        dir.join("lua/init.lua"),
        "bitty.commands.register({\n  id = \"greet\",\n  title = \"Greet\",\n  \
         run = function() return \"hi\" end,\n})\nreturn {}\n",
    )
    .expect("fixture init");
    dir
}

fn install_config_fixture(home: &Path) {
    let source = write_config_fixture(home);
    let output = run_in(
        home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

fn config_dir(home: &Path) -> PathBuf {
    home.join("bitty").join("plugins")
}

#[test]
fn plugin_config_show_reports_schema_and_source_layers() {
    let home = scratch_dir("config-show");
    install_config_fixture(&home);

    // Manifest defaults: every key resolves to the default layer.
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    for needle in [
        "show",
        "bool",
        "true",
        "manifest-default",
        "edge",
        "\"bottom\"",
        "limit",
        "int",
        "8",
        "modules",
        "string-array",
        "selected:       - (manifest defaults only)",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in {text}");
    }

    // JSON is a versioned envelope with the same attribution.
    let output = run_in(
        &home,
        &["plugin", "config", "show", "xuepoo.cfg", "--json"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json = stdout(&output);
    assert!(json.contains("\"v\":1"), "{json}");
    assert!(json.contains("\"verb\":\"config\""), "{json}");
    assert!(json.contains("\"action\":\"show\""), "{json}");
    assert!(json.contains("\"source\":\"manifest-default\""), "{json}");
    assert!(json.contains("\"description\":null"), "{json}");
    assert!(json.contains("\"selected\":null"), "{json}");

    // Canonical Lua wins and every overridden key attributes to user-file.
    std::fs::create_dir_all(config_dir(&home)).expect("plugins dir");
    std::fs::write(
        config_dir(&home).join("xuepoo.cfg.lua"),
        "return { show = false, modules = { \"clock\" } }\n",
    )
    .expect("canonical lua");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("lua-canonical"), "{text}");
    assert!(text.contains("user-file"), "{text}");
    assert!(text.contains("false"), "{text}");
    assert!(text.contains("[\"clock\"]"), "{text}");
    // `edge` and `limit` were untouched: the manifest default still serves.
    let edge_line = text
        .lines()
        .find(|line| line.trim_start().starts_with("edge"))
        .expect("edge row");
    assert!(edge_line.contains("manifest-default"), "{edge_line}");

    // Legacy TOML is disclosed as the active source when it is selected.
    std::fs::remove_file(config_dir(&home).join("xuepoo.cfg.lua")).expect("remove lua");
    std::fs::write(config_dir(&home).join("xuepoo.cfg.toml"), "show = false\n")
        .expect("legacy toml");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("toml-legacy"), "{text}");
    assert!(text.contains("legacy TOML is the active source"), "{text}");

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_config_show_fails_closed_on_unknown_and_undeclared_keys() {
    let home = scratch_dir("config-show-errors");
    install_config_fixture(&home);

    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.missing"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unknown plugin"),
        "{}",
        stderr(&output)
    );

    std::fs::create_dir_all(config_dir(&home)).expect("plugins dir");
    std::fs::write(
        config_dir(&home).join("xuepoo.cfg.lua"),
        "return { show = false, frobnicate = 7 }\n",
    )
    .expect("undeclared key");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    // The report still prints the effective values, then fails closed.
    assert!(
        stdout(&output).contains("frobnicate"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("UndeclaredKey"),
        "{}",
        stderr(&output)
    );

    // A malformed settings file is a typed rejection, never a silent default.
    std::fs::write(
        config_dir(&home).join("xuepoo.cfg.lua"),
        "return io.open('/etc/passwd')\n",
    )
    .expect("hostile settings");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ConfigRejected"),
        "{}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("manifest-default"),
        "{}",
        stdout(&output)
    );

    // An unreadable stored manifest is a schema error, not an unknown plugin.
    let manifest = data_store(&home).join("packages/xuepoo.cfg/1.0.0/bitty-plugin.toml");
    std::fs::write(&manifest, "not a manifest\n").expect("corrupt manifest");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("settings schema") || stderr(&output).contains("unreadable"),
        "{}",
        stderr(&output)
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_config_scaffold_round_trips_and_refuses_overwrite() {
    let home = scratch_dir("config-scaffold");
    install_config_fixture(&home);
    let target = config_dir(&home).join("xuepoo.cfg.lua");

    let output = run_in(&home, &["plugin", "config", "scaffold", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(target.is_file(), "scaffold must create the canonical path");
    let body = std::fs::read_to_string(&target).expect("scaffold body");
    assert!(body.contains("return {"), "{body}");
    assert!(body.contains("show = true,"), "{body}");
    assert!(
        body.contains("modules = { \"workspaces\", \"clock\" },"),
        "{body}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "scaffold must be owner-only");
    }
    // No temp file survives a successful commit.
    let leftovers: Vec<_> = std::fs::read_dir(config_dir(&home))
        .expect("plugins dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".bitty-plugin-config-")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Round-trip: scaffold -> show parses the generated file and attributes
    // every declared key to the user file.
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("lua-canonical"), "{text}");
    assert!(!text.contains("manifest-default"), "{text}");

    // Refuse to overwrite without --force, and leave the bytes untouched.
    std::fs::write(&target, "return { show = false }\n").expect("user edit");
    let output = run_in(&home, &["plugin", "config", "scaffold", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("AlreadyExists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("unchanged"),
        "return { show = false }\n"
    );

    // --force restores the template.
    let output = run_in(
        &home,
        &["plugin", "config", "scaffold", "xuepoo.cfg", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        std::fs::read_to_string(&target)
            .expect("rewritten")
            .contains("show = true,")
    );

    // The issue shape remains an alias: `config <id> --init`.
    let output = run_in(
        &home,
        &["plugin", "config", "xuepoo.cfg", "--init", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // A plugin without declarations has nothing to scaffold.
    let output = run_in(
        &home,
        &[
            "plugin",
            "config",
            "scaffold",
            "bitty-terminal.shell-integration",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("NoSettings"),
        "{}",
        stderr(&output)
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_config_scaffold_refuses_symlinked_target() {
    let home = scratch_dir("config-scaffold-symlink");
    install_config_fixture(&home);
    let target = config_dir(&home).join("xuepoo.cfg.lua");
    let outside = home.join("outside.lua");
    std::fs::write(&outside, "return { show = false }\n").expect("outside target");
    std::os::unix::fs::symlink(&outside, &target).expect("symlink target");

    let output = run_in(
        &home,
        &["plugin", "config", "scaffold", "xuepoo.cfg", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("SymlinkTarget"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside unchanged"),
        "return { show = false }\n"
    );
    // A symlinked target is also a hard error for inspection.
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ConfigRejected"),
        "{}",
        stderr(&output)
    );

    let _ = std::fs::remove_dir_all(&home);
}
