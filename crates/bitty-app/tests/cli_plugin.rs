//! `bitty plugin` end-to-end dispatch proofs (CTX-0150, issue #244).
//!
//! Canonical: `bitty-docs/docs/product/plugin-roadmap.md` owner direction
//! 2026-09-03 (DEC-0007), `docs/extensibility/package-management.md`
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
fn run_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
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
