//! `bitty plugin` end-to-end dispatch proofs (CTX-0150, issue #244).
//!
//! Canonical: `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/product/plugin-roadmap.md` owner direction
//! 2026-09-03 (DEC-0007), `https://github.com/bitty-terminal/bitty-plugins-docs/blob/main/extensibility/package-management.md`
//! ("Managed manifest"), and `docs/specifications/cli-contract-rfc.md`.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`
//! with an isolated `XDG_CONFIG_HOME`, so the developer's real
//! `bitty-plugins.lua` is never touched. `plugin` dispatches before config
//! load and GUI startup: no display, no instance, no plugin VM, and no
//! plugin code is ever executed. Consent input is piped so the fail-closed
//! paths are proven, not implied.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bitty_plugin_host::manifest::PluginId;
use bitty_runtime::plugin_runtime::{
    LuaValue, PluginRecord, PluginRuntime, PluginRuntimeConfig, SourceClass, load_index,
    write_index,
};

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

/// Cold ticks one legitimate RC-1 park may need to complete (bitty #2409).
///
/// Derived from the production constants: each cold tick opens one wall
/// window and a callback is bounded by the cumulative ceiling. The bound only
/// turns a stuck park into a test failure instead of a hang.
const PARK_RESUME_TICKS: usize =
    (bitty_lua::RC1_CALLBACK_CEILING_MS / bitty_lua::RC1_WALL_CLOCK_BUDGET_MS) as usize + 1;

/// Dispatch one command and settle an RC-1 park on the cold ticks (#2409).
///
/// bitty #2314 makes app-dispatched commands resumable at the 50 ms charged
/// wall budget, so a loaded runner can park even a trivial `greet` instead of
/// returning its value inline. This drives the documented recovery (the same
/// pump the app cold phase runs) and never re-dispatches, so the body still
/// executes exactly once.
fn dispatch_settled(rt: &mut PluginRuntime, id: &PluginId, command: &str) -> LuaValue {
    match rt.dispatch_command(id, command, &[]) {
        Ok(value) => value,
        Err(error) if error.is_parked() => {
            for _ in 0..PARK_RESUME_TICKS {
                let completions = rt.pump_parked_commands();
                if completions.is_empty() {
                    // Still parked: the next cold tick opens the next window.
                    continue;
                }
                assert_eq!(
                    completions.len(),
                    1,
                    "{command}: one parked invocation completes per cold tick"
                );
                let completion = completions
                    .into_iter()
                    .next()
                    .expect("checked exactly one completion");
                assert_eq!(completion.plugin.as_str(), id.as_str());
                assert_eq!(completion.command, command);
                assert_eq!(
                    rt.parked_command_count(),
                    0,
                    "{command}: a completed park must be dropped"
                );
                assert!(
                    rt.pump_parked_commands().is_empty(),
                    "{command}: a completed command must never replay"
                );
                return completion.outcome.expect("resumed dispatch");
            }
            panic!("{command}: the parked invocation did not complete");
        }
        Err(error) => panic!("{command}: dispatch failed: {error:?}"),
    }
}

/// Run the binary with an isolated config root and optional piped consent.
///
/// Both XDG roots are pinned into the scratch home and the development plugin
/// override is cleared, so a test can never read or write the developer's real
/// config or plugin store.
fn run_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    run_in_with_env(home, args, stdin, &[])
}

/// [`run_in`] with extra environment variables (test transports, proxies).
fn run_in_with_env(
    home: &Path,
    args: &[&str],
    stdin: Option<&str>,
    envs: &[(&str, String)],
) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .current_dir(home)
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
        .env_remove("BITTY_PLUGIN_DIR");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    home.join("bitty").join("bitty-plugins.lua")
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
    assert!(state.contains(&format!("[\"{PLUGIN}\"] = {{")), "{state}");
    assert!(
        state.contains("granted = { \"terminal.semantic-read\" }"),
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
    let backup = home.join("bitty").join("bitty-plugins.lua.bak");
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
fn plugin_explain_reports_allow_and_deny_live() {
    let home = scratch_dir("explain");
    let output = run_in(&home, &["plugin", "install", PLUGIN, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // Allow path: the mutual-grant chain names all six layers, the trust
    // level, enforcement, and the remediation/inspection pointer.
    let before = std::fs::read(state_file(&home)).expect("state bytes");
    let output = run_in(
        &home,
        &["plugin", "explain", PLUGIN, "terminal.semantic-read"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("decision:     allow"), "{text}");
    assert!(text.contains("first denying layer: none"), "{text}");
    assert!(
        text.contains("trust:        bundled-lua (level 1)"),
        "{text}"
    );
    for layer in [
        "host-ceiling",
        "user-policy",
        "project-policy",
        "parent-delegation",
        "task-grant",
        "agent-request",
    ] {
        assert!(text.contains(layer), "missing {layer}: {text}");
    }
    assert!(text.contains("budget:"), "{text}");
    assert!(text.contains("remediation:"), "{text}");
    // Safe mode is parity: the diagnostic never loads a VM, config, or store
    // state beyond the same read-only paths, so the report is byte-identical.
    let safe = run_in(
        &home,
        &[
            "--safe",
            "plugin",
            "explain",
            PLUGIN,
            "terminal.semantic-read",
        ],
        None,
    );
    assert_eq!(safe.status.code(), Some(0), "{}", stderr(&safe));
    assert_eq!(stdout(&safe), text, "--safe must not change the report");
    let after = std::fs::read(state_file(&home)).expect("state bytes");
    assert_eq!(before, after, "explain must never mutate the ledger");

    // Revoke the only grant: task-grant becomes the first denying layer and
    // the command exits 4 with the reason chain on stdout.
    let output = run_in(
        &home,
        &[
            "plugin",
            "revoke",
            PLUGIN,
            "--cap",
            "terminal.semantic-read",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run_in(
        &home,
        &["plugin", "explain", PLUGIN, "terminal.semantic-read"],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("decision:     deny"), "{text}");
    assert!(text.contains("first denying layer: task-grant"), "{text}");
    assert!(text.contains("denial kind:  exceeds-policy"), "{text}");
    assert!(text.contains("reason chain:"), "{text}");
    assert!(text.contains("bitty plugin install"), "{text}");
    assert!(stderr(&output).contains("denied"), "{}", stderr(&output));

    // Unknown capability: actionable stderr, exit 4, no panic.
    let output = run_in(&home, &["plugin", "explain", PLUGIN, "ui.rich"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("does not declare capability 'ui.rich'"),
        "{}",
        stderr(&output)
    );

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
fn plugin_profile_install_and_grant_flow() {
    let home = scratch_dir("profile-flow");

    // minimal: review screen, no selected grants, plugin stays disabled.
    let output = run_in(
        &home,
        &["plugin", "install", PLUGIN, "--profile", "minimal", "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("capability profile 'minimal' v1"), "{text}");
    assert!(text.contains("-- review before apply --"), "{text}");
    assert!(text.contains("+ will grant: none"), "{text}");
    let state = std::fs::read_to_string(state_file(&home)).expect("state written");
    assert!(state.contains("granted = {}"), "{state}");
    assert!(state.contains("enabled = false"), "{state}");

    // grant --profile editor-like adds exactly the one preselected
    // capability; the plugin remains disabled until `enable`.
    let output = run_in(
        &home,
        &[
            "plugin",
            "grant",
            PLUGIN,
            "--profile",
            "editor-like",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("applied profile 'editor-like' v1"), "{text}");
    let state = std::fs::read_to_string(state_file(&home)).expect("state rewritten");
    assert_eq!(
        state
            .matches("granted = { \"terminal.semantic-read\" }")
            .count(),
        1,
        "{state}"
    );
    assert_eq!(
        state.matches("terminal.semantic-read").count(),
        1,
        "no duplicate or additional grant: {state}"
    );
    assert!(state.contains("enabled = false"), "{state}");

    // Explicit enable is still the only activation transition.
    let output = run_in(&home, &["plugin", "enable", PLUGIN], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // A profile cannot grant a manifest-unrequested capability by name.
    let output = run_in(
        &home,
        &[
            "plugin",
            "grant",
            PLUGIN,
            "--cap",
            "network.connect:example.com:443",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_profile_rejects_store_sources_before_touching_state() {
    let home = scratch_dir("profile-store");

    let output = run_in(
        &home,
        &[
            "plugin",
            "install",
            "./my-plugin",
            "--profile",
            "editor-like",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(!state_file(&home).exists(), "nothing is written");

    let output = run_in(
        &home,
        &[
            "plugin",
            "grant",
            "xuepoo.hello",
            "--profile",
            "minimal",
            "--yes",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(!state_file(&home).exists(), "nothing is written");
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
        dispatch_settled(&mut runtime, &id, "greet"),
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
        dispatch_settled(&mut rt, &id, "greet"),
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
        dispatch_settled(&mut updated_rt, &id, "greet"),
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
        "lua-canonical",
        "selected:       ",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in {text}");
    }
    // The install auto-generated the commented defaults file, so the
    // canonical path is selected while every value still serves the manifest
    // default (the file itself returns an empty table).
    assert!(
        text.contains("xuepoo.cfg.lua (present)"),
        "generated defaults present in {text}"
    );

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
    assert!(json.contains("\"selected\":{\"path\":"), "{json}");
    assert!(json.contains("\"kind\":\"lua-canonical\""), "{json}");

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

    // Install auto-generated the commented defaults file. Scaffold is the
    // explicit overwrite path: without --force it refuses and leaves the
    // generated bytes (or user edits) untouched.
    assert!(target.is_file(), "install must generate the canonical path");
    let generated = std::fs::read_to_string(&target).expect("generated body");
    assert!(
        generated.contains("-- bitty-plugin-defaults: xuepoo.cfg "),
        "{generated}"
    );
    let output = run_in(&home, &["plugin", "config", "scaffold", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("AlreadyExists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("unchanged"),
        generated
    );

    let output = run_in(
        &home,
        &["plugin", "config", "scaffold", "xuepoo.cfg", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
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
    // Install generated the defaults file first; replace it with the symlink
    // under test.
    std::fs::remove_file(&target).expect("remove generated defaults");
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

/// Legacy TOML ledger path (migration source only).
fn legacy_state_file(home: &Path) -> PathBuf {
    home.join("bitty").join("bitty-plugins.toml")
}

#[test]
fn plugin_ledger_migrates_legacy_toml_once() {
    let home = scratch_dir("ledger-migrate");
    let ledger = state_file(&home);
    let legacy = legacy_state_file(&home);
    let hash = bitty_plugin_host::bundled::bundled_manifest_for(PLUGIN)
        .expect("bundled manifest")
        .manifest_hash();
    let body = format!(
        "state_version = 1\n\n[plugins.\"{PLUGIN}\"]\n\
         source = \"bundled\"\nmanifest_hash = \"{hash}\"\nenabled = true\n\
         granted = [\"terminal.semantic-read\"]\ndenied = [\"ui.rich\"]\n"
    );
    std::fs::create_dir_all(home.join("bitty")).expect("config dir");
    std::fs::write(&legacy, &body).expect("legacy ledger");

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(ledger.is_file(), "Lua ledger written by migration");
    assert!(!legacy.exists(), "legacy TOML moved aside");
    let backup = home.join("bitty").join("bitty-plugins.toml.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup"),
        body,
        "legacy bytes retained verbatim"
    );
    let migrated = std::fs::read_to_string(&ledger).expect("ledger");
    assert!(migrated.contains("[\"bitty-terminal.shell-integration\"] = {"));
    assert!(migrated.contains("granted = { \"terminal.semantic-read\" }"));
    assert!(migrated.contains("denied = { \"ui.rich\" }"));
    assert!(migrated.contains("enabled = true"));
    let text = stdout(&output);
    assert!(text.contains("enabled"), "{text}");
    assert!(text.contains("1/1"), "{text}");

    // Second run reads the Lua ledger and never rewrites it.
    let before = std::fs::read(&ledger).expect("bytes");
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read(&ledger).expect("bytes"), before);

    // Doctor sees a healthy, already-migrated ledger.
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ledger: 1 record(s)"),
        "{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_ledger_migrates_long_capabilities_without_corruption() {
    let home = scratch_dir("ledger-migrate-long-caps");
    let ledger = state_file(&home);
    let legacy = legacy_state_file(&home);
    let long_a = format!("fs.read:{}", "a".repeat(500));
    let long_b = format!("fs.read:{}", "b".repeat(500));
    let hash = "0".repeat(64);
    let body = format!(
        "state_version = 1\n\n[plugins.\"xuepoo.longcaps\"]\n\
         source = \"bundled\"\nmanifest_hash = \"{hash}\"\nenabled = true\n\
         granted = [\"{long_a}\", \"{long_b}\"]\ndenied = []\n"
    );
    std::fs::create_dir_all(home.join("bitty")).expect("config dir");
    std::fs::write(&legacy, &body).expect("legacy ledger");

    // The old inline renderer produced a >1024-byte capability line here;
    // migration then committed a ledger the parser rejected and moved the
    // TOML aside. The first post-migration command must now stay healthy.
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(ledger.is_file(), "Lua ledger written");
    assert!(!legacy.exists(), "legacy TOML moved aside");
    let backup = home.join("bitty").join("bitty-plugins.toml.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup"),
        body,
        "legacy bytes retained verbatim"
    );
    let migrated = std::fs::read_to_string(&ledger).expect("ledger");
    for line in migrated.lines() {
        assert!(
            line.len() <= 1024,
            "rendered line exceeds the parser bound: {} bytes",
            line.len()
        );
    }
    assert!(migrated.contains(&long_a), "{migrated}");
    assert!(migrated.contains(&long_b), "{migrated}");

    // Parsing is stable on the next command. Doctor reports the unknown
    // record as a warning (exit 1) but must not quarantine a valid ledger.
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("no bundled manifest"),
        "{}",
        stdout(&output)
    );
    let corrupt = std::fs::read_dir(home.join("bitty"))
        .expect("config dir")
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("bitty-plugins.lua.corrupt-")
        });
    assert!(!corrupt, "a valid migrated ledger must not be quarantined");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_migrates_a_legacy_ledger_on_repair() {
    let home = scratch_dir("doctor-legacy-ledger");
    let ledger = state_file(&home);
    let legacy = legacy_state_file(&home);
    let hash = bitty_plugin_host::bundled::bundled_manifest_for(PLUGIN)
        .expect("bundled manifest")
        .manifest_hash();
    let body = format!(
        "state_version = 1\n\n[plugins.\"{PLUGIN}\"]\n\
         source = \"bundled\"\nmanifest_hash = \"{hash}\"\nenabled = false\n\
         granted = []\ndenied = []\n"
    );
    std::fs::create_dir_all(home.join("bitty")).expect("config dir");
    std::fs::write(&legacy, &body).expect("legacy ledger");

    // Read-only doctor reports the pending legacy ledger without writing.
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("legacy bitty-plugins.toml"),
        "{}",
        stdout(&output)
    );
    assert!(!ledger.exists(), "doctor without --repair writes nothing");
    assert!(legacy.exists());

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(ledger.is_file(), "repair writes the Lua ledger");
    assert!(!legacy.exists(), "legacy moved aside");
    let backup = home.join("bitty").join("bitty-plugins.toml.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup"),
        body,
        "legacy bytes retained verbatim"
    );
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains(PLUGIN), "{}", stdout(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_repair_is_a_noop_for_a_current_ledger() {
    let home = scratch_dir("doctor-noop-ledger");
    let output = run_in(&home, &["plugin", "install", PLUGIN, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let ledger = state_file(&home);
    let before = std::fs::read(&ledger).expect("ledger bytes");

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ledger already consistent"),
        "{}",
        stdout(&output)
    );
    assert_eq!(std::fs::read(&ledger).expect("ledger bytes"), before);
    assert!(
        !home.join("bitty").join("bitty-plugins.lua.bak").exists(),
        "a healthy ledger must not churn its backup every repair"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_ledger_hand_edit_cannot_escalate_authority() {
    let home = scratch_dir("ledger-escalation");
    let output = run_in(&home, &["plugin", "install", PLUGIN, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let ledger = state_file(&home);
    let text = std::fs::read_to_string(&ledger).expect("ledger");
    let edited = text.replace(
        "granted = { \"terminal.semantic-read\" }",
        "granted = { \"terminal.semantic-read\", \"ui.rich\" }",
    );
    assert!(edited.contains("ui.rich"), "{edited}");
    std::fs::write(&ledger, edited).expect("hand edit");

    let output = run_in(&home, &["plugin", "info", PLUGIN], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("[granted] terminal.semantic-read"),
        "declared grant survives: {text}"
    );
    assert!(
        !text.contains("[granted] ui.rich"),
        "undeclared grant must not be granted: {text}"
    );
    assert!(
        stderr(&output).contains("does not declare"),
        "the dropped grant is recorded: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_install_generates_commented_defaults_once() {
    let home = scratch_dir("generated-defaults");
    let source = write_config_fixture(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("created default settings"),
        "{}",
        stdout(&output)
    );
    let target = config_dir(&home).join("xuepoo.cfg.lua");
    let body = std::fs::read_to_string(&target).expect("generated defaults");
    assert!(
        body.contains("-- bitty-plugin-defaults: xuepoo.cfg "),
        "{body}"
    );
    assert!(body.contains("-- show = true,"), "{body}");
    assert!(
        body.contains("--   modules: string-array = { \"workspaces\", \"clock\" }"),
        "{body}"
    );
    // The generated template is data only and parses as an empty settings
    // table: every value still resolves to the manifest default.
    assert!(body.contains("return {\n"), "{body}");
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("lua-canonical"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("manifest-default"),
        "{}",
        stdout(&output)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "generated defaults must be 0600");
    }

    // `info` points at the generated file.
    let output = run_in(&home, &["plugin", "info", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("settings:"), "{text}");
    assert!(
        text.contains(&target.display().to_string()),
        "info must point at the generated file: {text}"
    );
    assert!(text.contains("(present)"), "{text}");
    let output = run_in(
        &home,
        &["plugin", "info", "xuepoo.cfg", "--format", "json"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json = stdout(&output);
    assert!(json.contains("\"settings_file\":\""), "{json}");
    assert!(json.contains("\"settings_file_exists\":true"), "{json}");

    // A user edit is never overwritten by installing again.
    std::fs::write(&target, "return { show = false }\n").expect("user edit");
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        std::fs::read_to_string(&target).expect("unchanged"),
        "return { show = false }\n"
    );

    // A settings-less plugin gets no file at all.
    let output = run_in(&home, &["plugin", "install", PLUGIN, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!config_dir(&home).join(format!("{PLUGIN}.lua")).exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_install_skips_generation_beside_a_short_alias() {
    let home = scratch_dir("generated-short-alias");
    let target = config_dir(&home).join("xuepoo.cfg.lua");
    let alias = config_dir(&home).join("cfg.lua");
    std::fs::create_dir_all(config_dir(&home)).expect("plugins dir");
    std::fs::write(&alias, "return { show = false }\n").expect("alias settings");
    install_config_fixture(&home);
    assert!(
        !target.exists(),
        "the generated canonical file would shadow the alias"
    );
    assert_eq!(
        std::fs::read_to_string(&alias).expect("alias unchanged"),
        "return { show = false }\n"
    );
    let output = run_in(&home, &["plugin", "config", "show", "xuepoo.cfg"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("lua-alias"), "{text}");
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("show") && line.contains("false")),
        "alias value must remain effective: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_install_skips_generation_beside_a_legacy_toml() {
    let home = scratch_dir("generated-legacy-toml");
    let target = config_dir(&home).join("xuepoo.cfg.lua");
    let legacy = config_dir(&home).join("xuepoo.cfg.toml");
    std::fs::create_dir_all(config_dir(&home)).expect("plugins dir");
    std::fs::write(&legacy, "show = false\n").expect("legacy settings");
    install_config_fixture(&home);
    assert!(!target.exists(), "a Lua file would shadow the legacy TOML");
    assert_eq!(
        std::fs::read_to_string(&legacy).expect("legacy unchanged"),
        "show = false\n"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_install_never_writes_through_a_symlinked_defaults_file() {
    let home = scratch_dir("generated-symlink");
    let target = config_dir(&home).join("xuepoo.cfg.lua");
    let outside = home.join("outside.lua");
    std::fs::create_dir_all(config_dir(&home)).expect("plugins dir");
    std::fs::write(&outside, "return { show = false }\n").expect("outside");
    std::os::unix::fs::symlink(&outside, &target).expect("symlink target");
    install_config_fixture(&home);
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside unchanged"),
        "return { show = false }\n"
    );
    // The symlink is still a symlink; install must not replace it.
    let metadata = std::fs::symlink_metadata(&target).expect("metadata");
    assert!(metadata.file_type().is_symlink(), "symlink preserved");
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// Official catalog install-by-id (bitty-plugins#108): embedded catalog,
// pinned commit, raw manifest hash, no runtime metadata fetch.
// ---------------------------------------------------------------------------

#[cfg(unix)]
struct FakeGitTransport {
    envs: Vec<(&'static str, String)>,
    log: PathBuf,
}

#[cfg(unix)]
impl FakeGitTransport {
    /// Fake `git`/`curl`/`wget` on PATH. `git` records every invocation,
    /// serves `fixture` on checkout/clone, and answers `rev-parse` with the
    /// pin it was asked to fetch. `curl`/`wget` only record (and fail), so a
    /// runtime metadata fetch cannot pass silently.
    fn new(home: &Path, fixture: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = home.join("fakebin");
        std::fs::create_dir_all(&bin).expect("fake bin");
        let log = home.join("fake-transport.log");
        let pin_file = home.join("fake-transport.pin");
        let git = bin.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\n\
             set -eu\n\
             printf '%s\\n' \"git $*\" >> \"$FAKE_TRANSPORT_LOG\"\n\
             if [ \"$1\" = \"init\" ]; then\n\
               for last in \"$@\"; do :; done\n\
               mkdir -p \"$last\"\n\
               exit 0\n\
             fi\n\
             repo=\"\"\n\
             if [ \"$1\" = \"-C\" ]; then repo=\"$2\"; fi\n\
             case \"$*\" in\n\
               *\" rev-parse \"*) cat \"$FAKE_TRANSPORT_PIN\" ;;\n\
               *\" fetch \"*)\n\
                 for last in \"$@\"; do :; done\n\
                 printf '%s\\n' \"$last\" > \"$FAKE_TRANSPORT_PIN\" ;;\n\
               *\" checkout \"*) cp -R \"$FAKE_TRANSPORT_FIXTURE\"/. \"$repo\"/ ;;\n\
               *\" clone \"*)\n\
                 for last in \"$@\"; do :; done\n\
                 mkdir -p \"$last\"\n\
                 cp -R \"$FAKE_TRANSPORT_FIXTURE\"/. \"$last\"/ ;;\n\
               *) exit 0 ;;\n\
             esac\n",
        )
        .expect("fake git");
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755))
            .expect("fake git mode");
        for tool in ["curl", "wget"] {
            let path = bin.join(tool);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"{tool} $*\" >> \"$FAKE_TRANSPORT_LOG\"\nexit 1\n"
                ),
            )
            .expect("fake fetcher");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("fake fetcher mode");
        }
        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            envs: vec![
                ("PATH", path_env),
                ("FAKE_TRANSPORT_LOG", log.display().to_string()),
                ("FAKE_TRANSPORT_PIN", pin_file.display().to_string()),
                ("FAKE_TRANSPORT_FIXTURE", fixture.display().to_string()),
            ],
            log,
        }
    }

    fn envs(&self) -> &[(&'static str, String)] {
        &self.envs
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(unix)]
fn official_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/official/bar")
}

#[test]
fn plugin_info_resolves_every_official_id_offline() {
    use bitty_plugin_host::official::official_plugins;

    let home = scratch_dir("official-info");
    for entry in official_plugins() {
        let output = run_in(
            &home,
            &["plugin", "info", entry.id, "--format", "json"],
            None,
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}: {}",
            entry.id,
            stderr(&output)
        );
        let text = stdout(&output);
        assert!(
            text.contains("\"source\":\"registry\""),
            "{}: {text}",
            entry.id
        );
        assert!(
            text.contains(&format!("\"manifest_hash\":\"{}\"", entry.manifest_hash)),
            "{}: {text}",
            entry.id
        );
    }
    // Resolution is read-only and never creates a store or a ledger.
    assert!(!state_file(&home).exists());
    assert!(!data_store(&home).exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_install_resolves_every_installable_official_id_through_pinned_transport() {
    use bitty_plugin_host::official::official_plugins;

    // The fixture path is deliberately absent: the fake transport records the
    // repository/pin it was asked for, then fails the checkout. That proves
    // per-id resolution reaches the transport with the catalog's exact pin
    // without needing a full manifest fixture for all eleven entries.
    for entry in official_plugins() {
        let home = scratch_dir("official-resolve");
        let missing = home.join("missing-fixture");
        let transport = FakeGitTransport::new(&home, &missing);
        let output = run_in_with_env(
            &home,
            &["plugin", "install", entry.id, "--yes"],
            None,
            transport.envs(),
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}: {}",
            entry.id,
            stderr(&output)
        );
        let calls = transport.calls();
        assert!(
            calls.contains(&format!("remote add origin {}", entry.repository)),
            "{}: {calls}",
            entry.id
        );
        assert!(
            calls.contains(&format!(
                "fetch --quiet --depth=1 --no-tags origin {}",
                entry.pin
            )),
            "{}: {calls}",
            entry.id
        );
        assert!(
            !calls.contains("curl ") && !calls.contains("wget "),
            "{}: no runtime metadata fetch: {calls}",
            entry.id
        );
        assert!(
            !calls.contains("bitty-plugins"),
            "{}: the registry repository is never contacted at runtime: {calls}",
            entry.id
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(unix)]
#[test]
fn plugin_install_unknown_id_lists_bundled_and_official_ids_without_transport() {
    let home = scratch_dir("official-unknown");
    let transport = FakeGitTransport::new(&home, &official_fixture());
    let output = run_in_with_env(
        &home,
        &["plugin", "install", "xuepoo.markdown", "--yes"],
        None,
        transport.envs(),
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(
        text.contains("bundled ids: bitty-terminal.shell-integration"),
        "{text}"
    );
    assert!(text.contains("bitty-featured.activity"), "{text}");
    assert!(text.contains("bitty-terminal.bar"), "{text}");
    assert!(text.contains("bitty-terminal.dashboard"), "{text}");
    assert_eq!(transport.calls(), "", "resolution must not fetch");
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_install_by_official_id_fetches_pin_and_records_registry_source() {
    use bitty_runtime::plugin_runtime::SourceClass;

    let home = scratch_dir("official-bar");
    let transport = FakeGitTransport::new(&home, &official_fixture());
    let output = run_in_with_env(
        &home,
        &["plugin", "install", "bitty-terminal.bar", "--yes"],
        None,
        transport.envs(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("official 'bitty-terminal.bar'"), "{text}");
    assert!(text.contains("verified"), "{text}");
    assert!(text.contains("installed"), "{text}");

    let calls = transport.calls();
    assert!(calls.contains("fetch"), "{calls}");
    assert!(
        calls.contains("6af2be11ee3df9f2c4fecbd73408e2c2f70a6b8c"),
        "{calls}"
    );
    assert!(
        !calls.contains("curl "),
        "no runtime metadata fetch: {calls}"
    );
    assert!(
        !calls.contains("wget "),
        "no runtime metadata fetch: {calls}"
    );
    assert!(
        !calls.contains("bitty-plugins"),
        "the registry repository is never contacted at runtime: {calls}"
    );

    let store = data_store(&home);
    let records = load_index(&store).expect("store index");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].plugin_id, "bitty-terminal.bar");
    assert_eq!(records[0].source_class, SourceClass::Registry);
    assert!(
        !records[0].enabled,
        "fresh installs stay staged and disabled"
    );
    assert!(
        store
            .join("packages/bitty-terminal.bar/0.0.1/lua/bar/init.lua")
            .is_file(),
        "the pinned package tree must be staged"
    );

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("registry"), "{text}");
    assert!(text.contains("disabled"), "{text}");

    let output = run_in(&home, &["plugin", "enable", "bitty-terminal.bar"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("store index")[0].enabled);

    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_install_by_official_id_rejects_manifest_hash_mismatch_before_store_write() {
    let home = scratch_dir("official-hash-mismatch");
    let tampered = home.join("tampered-bar");
    std::fs::create_dir_all(&tampered).expect("tampered root");
    copy_dir(&official_fixture(), &tampered);
    let manifest_path = tampered.join("bitty-plugin.toml");
    let manifest = std::fs::read_to_string(&manifest_path).expect("manifest");
    let tampered_manifest = manifest.replacen("version = \"0.0.1\"", "version = \"0.0.2\"", 1);
    assert_ne!(tampered_manifest, manifest);
    std::fs::write(&manifest_path, tampered_manifest).expect("tampered manifest");

    let transport = FakeGitTransport::new(&home, &tampered);
    let output = run_in_with_env(
        &home,
        &["plugin", "install", "bitty-terminal.bar", "--yes"],
        None,
        transport.envs(),
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("manifest hash mismatch"), "{text}");
    assert!(text.contains("nothing installed"), "{text}");
    assert!(!data_store(&home).join("current.json").exists());
    assert!(
        !data_store(&home)
            .join("packages/bitty-terminal.bar")
            .exists()
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn plugin_install_by_official_id_shows_capabilities_and_honors_decline() {
    let home = scratch_dir("official-consent");
    let transport = FakeGitTransport::new(&home, &official_fixture());
    let output = run_in_with_env(
        &home,
        &["plugin", "install", "bitty-terminal.bar"],
        Some("n\n"),
        transport.envs(),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("workspace.control"), "{text}");
    assert!(text.contains("ui.rich"), "{text}");
    assert!(text.contains("[y/N]"), "{text}");
    assert!(!data_store(&home).join("current.json").exists());

    let output = run_in_with_env(
        &home,
        &["plugin", "install", "bitty-terminal.bar"],
        Some("y\n"),
        transport.envs(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(data_store(&home).join("current.json").is_file());
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            std::fs::create_dir_all(&target).expect("fixture dir");
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("fixture copy");
        }
    }
}

// ---------------------------------------------------------------------------
// Issue #2439: transactional store installs, orphan recovery, doctor --repair.
// ---------------------------------------------------------------------------

/// Backdate a staging directory past the doctor's age gate.
fn backdate_staging(path: &Path) {
    use std::time::{Duration, SystemTime};
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        // A directory handle needs FILE_FLAG_BACKUP_SEMANTICS (custom flags,
        // not the access mask) plus FILE_WRITE_ATTRIBUTES for `set_times`.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .expect("open staging dir")
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path).expect("open staging dir");
    let when = SystemTime::now() - Duration::from_secs(3600);
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("backdate staging");
}

#[test]
fn plugin_install_recovers_owner_orphan_repro() {
    // Exact owner repro: manifest-only version dir + empty lua/, no
    // current.json. list surfaces it; install --yes repairs it.
    let home = scratch_dir("orphan-repro");
    let source = write_external_fixture(&home, "source", "0.0.1", "hello");
    let source_arg = source.display().to_string();
    let store = data_store(&home);
    let version = store.join("packages/xuepoo.external/0.0.1");

    let output = run_in(&home, &["plugin", "install", &source_arg, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(version.join("lua/init.lua").is_file());

    // Interrupted/partial install: manifest stays, lua body is gone, index gone.
    std::fs::remove_file(version.join("lua/init.lua")).expect("truncate lua");
    std::fs::remove_file(store.join("current.json")).expect("drop index");

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("xuepoo.external"), "{text}");
    assert!(text.contains("orphaned"), "{text}");
    assert!(text.contains("bitty plugin doctor --repair"), "{text}");
    assert!(
        !text.contains("bitty plugin remove xuepoo.external --force"),
        "repair hints stay non-destructive: {text}"
    );

    // --yes approves the repair prompt (non-interactive).
    let output = run_in(&home, &["plugin", "install", &source_arg, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("replaced the existing package content"),
        "{}",
        stdout(&output)
    );
    assert!(
        version.join("lua/init.lua").is_file(),
        "complete tree restored"
    );
    let records = load_index(&store).expect("index");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].version, "0.0.1");
    let output = run_in(&home, &["plugin", "list"], None);
    assert!(!stdout(&output).contains("orphaned"), "{}", stdout(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_same_version_content_change_prompts_and_honors_force() {
    let home = scratch_dir("same-version");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello v1");
    let source_arg = source.display().to_string();
    let store = data_store(&home);
    let index = store.join("current.json");

    let output = run_in(&home, &["plugin", "install", &source_arg, "--yes"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let before = std::fs::read_to_string(&index).expect("index");
    let digest_before = load_index(&store).expect("index")[0].content_digest.clone();

    // Same manifest version, new Lua body.
    std::fs::write(
        source.join("lua/init.lua"),
        "bitty.commands.register({ id = \"greet\", title = \"Greet\", run = function() return \"hello v2\" end })\nreturn {}\n",
    )
    .expect("rewrite init");

    // Decline the repair prompt: nothing changes.
    let output = run_in(&home, &["plugin", "install", &source_arg], Some("n\n"));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("install was not approved"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read_to_string(&index).expect("index"), before);

    // Approve interactively.
    let output = run_in(&home, &["plugin", "install", &source_arg], Some("y\n"));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let records = load_index(&store).expect("index");
    assert_ne!(records[0].content_digest, digest_before);

    // `--force` replaces without a prompt.
    std::fs::write(
        source.join("lua/init.lua"),
        "bitty.commands.register({ id = \"greet\", title = \"Greet\", run = function() return \"hello v3\" end })\nreturn {}\n",
    )
    .expect("rewrite init again");
    let output = run_in(&home, &["plugin", "install", &source_arg, "--force"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("replaced the existing package content"),
        "{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_rebuilds_corrupt_index_and_never_lists_it_as_empty() {
    let home = scratch_dir("doctor-corrupt");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    std::fs::write(store.join("current.json"), b"{truncated").expect("corrupt index");

    // list fails closed with the repair command, never an empty store.
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("bitty plugin doctor --repair"),
        "{}",
        stderr(&output)
    );

    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("corrupt-index"), "{text}");
    assert!(text.contains("bitty plugin doctor --repair"), "{text}");

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("quarantined current.json"), "{text}");
    assert!(text.contains("wrote current.json"), "{text}");
    let records = load_index(&store).expect("rebuilt index");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].plugin_id, "xuepoo.external");
    assert!(records[0].granted.is_empty(), "repair never invents grants");
    assert!(
        store.join("quarantine").is_dir(),
        "the broken index is preserved for inspection"
    );
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("xuepoo.external"));
    assert!(stdout(&output).contains("broken") || stdout(&output).contains("disabled"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_repairs_orphans_and_stale_staging() {
    let home = scratch_dir("doctor-orphan");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let version = store.join("packages/xuepoo.external/1.0.0");
    std::fs::remove_file(version.join("lua/init.lua")).expect("truncate");
    std::fs::remove_file(store.join("current.json")).expect("drop index");
    let staging = store.join("packages/xuepoo.external/.tmp-crashed");
    std::fs::create_dir_all(&staging).expect("staging");
    std::fs::write(staging.join("bitty-plugin.toml"), b"partial").expect("partial");
    backdate_staging(&staging);

    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("incomplete-package"), "{text}");
    assert!(text.contains("stale-staging"), "{text}");

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("quarantined packages/xuepoo.external/1.0.0"),
        "{text}"
    );
    assert!(text.contains("removed staging"), "{text}");
    assert!(
        !version.exists(),
        "incomplete tree is quarantined, not listed"
    );
    assert!(!staging.exists());
    assert!(load_index(&store).expect("index").is_empty());
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !stdout(&output).contains("xuepoo.external"),
        "{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_quarantines_a_malformed_ledger_and_removes_unknown_records() {
    let home = scratch_dir("doctor-ledger");
    let state = state_file(&home);

    // Unknown but well-formed record: warn, then repair removes only it.
    std::fs::create_dir_all(home.join("bitty")).expect("config dir");
    std::fs::write(
        &state,
        "return {\n\
         \x20   state_version = 1,\n\
         \x20   plugins = {\n\
         \x20       [\"xuepoo.ghost\"] = {\n\
         \x20           source = \"bundled\",\n\
         \x20           manifest_hash = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\n\
         \x20           enabled = true,\n\
         \x20           granted = {},\n\
         \x20           denied = {},\n\
         \x20       },\n\
         \x20   },\n\
         }\n",
    )
    .expect("ledger");
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("xuepoo.ghost"),
        "{}",
        stdout(&output)
    );
    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = std::fs::read_to_string(&state).expect("ledger rewritten");
    assert!(!text.contains("xuepoo.ghost"), "{text}");
    assert!(text.contains("state_version = 1"), "{text}");

    // Malformed ledger: doctor still diagnoses, repair quarantines + resets.
    std::fs::write(
        &state,
        "return { state_version = 1, plugins = { [\"broken\" = {\n",
    )
    .expect("break ledger");
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ledger unreadable"),
        "{}",
        stdout(&output)
    );
    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("quarantined ledger"),
        "{}",
        stdout(&output)
    );
    let text = std::fs::read_to_string(&state).expect("fresh ledger");
    assert!(text.contains("state_version = 1"), "{text}");
    let corrupt = std::fs::read_dir(home.join("bitty"))
        .expect("config dir")
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("bitty-plugins.lua.corrupt-")
        });
    assert!(
        corrupt,
        "the broken ledger is preserved, not silently dropped"
    );
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_list_shows_partial_indexed_package_as_broken() {
    let home = scratch_dir("broken-row");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    std::fs::remove_dir_all(store.join("packages/xuepoo.external/1.0.0/lua"))
        .expect("truncate tree");

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("xuepoo.external"), "{text}");
    assert!(text.contains("broken"), "{text}");
    assert!(text.contains("bitty plugin doctor --repair"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_remove_force_cleans_orphan_and_allows_reinstall() {
    let home = scratch_dir("remove-orphan");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    std::fs::remove_file(store.join("current.json")).expect("drop index");

    let output = run_in(
        &home,
        &["plugin", "remove", "xuepoo.external", "--force"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("orphaned package directory"),
        "{}",
        stdout(&output)
    );
    assert!(!store.join("packages/xuepoo.external").exists());

    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(load_index(&store).expect("index").len(), 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_healthy_store_and_missing_ledger_exit_zero() {
    let home = scratch_dir("doctor-healthy");
    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("absent (empty store"), "{text}");
    assert!(
        text.contains("ledger:") && text.contains("absent"),
        "{text}"
    );
    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!state_file(&home).exists(), "no ledger is invented");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_detects_and_drops_a_dangling_record() {
    let home = scratch_dir("doctor-dangling");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    std::fs::remove_dir_all(store.join("packages/xuepoo.external")).expect("drop tree");

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("xuepoo.external"), "{text}");
    assert!(text.contains("broken"), "{text}");

    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("dangling-record"),
        "{}",
        stdout(&output)
    );
    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(load_index(&store).expect("index").is_empty());
    let output = run_in(&home, &["plugin", "list"], None);
    assert!(
        !stdout(&output).contains("xuepoo.external"),
        "{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// Review regressions: authoritative repair, crash-window asides, hints.
// ---------------------------------------------------------------------------

#[test]
fn plugin_doctor_repair_restores_a_crash_window_replaced_aside() {
    // Crash after the active version was renamed to `.tmp-replaced-*` and
    // before the index write: `current.json` still points at it, so repair
    // must restore the tree instead of deleting it as stale staging.
    let home = scratch_dir("aside-restore");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run_in(&home, &["plugin", "enable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let package_dir = store.join("packages/xuepoo.external");
    let target = package_dir.join("1.0.0");
    let aside = package_dir.join(format!(".tmp-replaced-{}-0", std::process::id()));
    std::fs::rename(&target, &aside).expect("simulate crash window");
    backdate_staging(&aside);
    let index_before = std::fs::read(store.join("current.json")).expect("index bytes");

    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("stale-staging"),
        "{}",
        stdout(&output)
    );

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("restored packages/xuepoo.external/1.0.0"),
        "the restore is reported: {text}"
    );
    assert!(target.is_dir(), "the only copy is restored, not deleted");
    assert!(!aside.exists(), "{text}");
    assert_eq!(
        std::fs::read(store.join("current.json")).expect("index bytes"),
        index_before,
        "the active record is unchanged"
    );
    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("enabled"), "{}", stdout(&output));
    assert!(!stdout(&output).contains("orphaned"), "{}", stdout(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_repair_quarantines_a_bundled_shadow_tree() {
    // A store record for a bundled id is invalid provenance. Repair drops the
    // record but must preserve the tree in quarantine, never delete it.
    let home = scratch_dir("bundled-shadow");
    let store = data_store(&home);
    let version_dir = store.join("packages").join(PLUGIN).join("1.0.0");
    std::fs::create_dir_all(version_dir.join("lua")).expect("shadow tree");
    std::fs::write(version_dir.join("lua/init.lua"), b"return {}\n").expect("body");
    write_index(
        &store,
        &[PluginRecord {
            source_class: SourceClass::LocalPath,
            plugin_id: PLUGIN.to_string(),
            version: "1.0.0".to_string(),
            root: format!("packages/{PLUGIN}/1.0.0"),
            manifest_hash: "a".repeat(64),
            content_digest: "b".repeat(64),
            enabled: true,
            granted: Vec::new(),
        }],
    )
    .expect("shadowing index");

    let output = run_in(&home, &["plugin", "doctor"], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("store/bundled mismatch"),
        "{}",
        stdout(&output)
    );

    let output = run_in(&home, &["plugin", "doctor", "--repair"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("quarantined packages/{PLUGIN}/1.0.0")),
        "{text}"
    );
    assert!(
        !version_dir.exists(),
        "the shadow tree is moved, not deleted"
    );
    let preserved = std::fs::read_dir(store.join("quarantine").join(PLUGIN))
        .expect("quarantine entry")
        .flatten()
        .any(|entry| entry.path().join("lua/init.lua").is_file());
    assert!(preserved, "the shadow tree is preserved for inspection");
    assert!(load_index(&store).expect("index").is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_list_keeps_a_healthy_row_state_with_extra_version_dirs() {
    // An extra unindexed version dir must not flip an enabled row to
    // `orphaned`, and the hint must not suggest the destructive remove.
    let home = scratch_dir("healthy-extra");
    let source = write_external_fixture(&home, "source", "1.0.0", "hello");
    let store = data_store(&home);
    let output = run_in(
        &home,
        &["plugin", "install", &source.display().to_string(), "--yes"],
        None,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = run_in(&home, &["plugin", "enable", "xuepoo.external"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    std::fs::create_dir_all(store.join("packages/xuepoo.external/9.9.9")).expect("extra dir");

    let output = run_in(&home, &["plugin", "list", "--format", "json"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"state\":\"enabled\""), "{text}");
    assert!(text.contains("\"enabled\":true"), "{text}");
    assert!(
        text.contains("bitty plugin doctor --repair"),
        "the repair hint stays available: {text}"
    );
    assert!(
        !text.contains("bitty plugin remove xuepoo.external --force"),
        "no destructive hint: {text}"
    );

    let output = run_in(&home, &["plugin", "list"], None);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("enabled"), "{text}");
    assert!(!text.contains("orphaned"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn plugin_doctor_accepts_no_color() {
    let home = scratch_dir("doctor-no-color");
    let output = run_in(&home, &["plugin", "doctor", "--no-color"], None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--no-color must be accepted: {}",
        stderr(&output)
    );
    assert!(!stdout(&output).contains('\u{1b}'), "{}", stdout(&output));
    let output = run_in(&home, &["plugin", "doctor", "--repair", "--no-color"], None);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--repair --no-color must be accepted: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}
