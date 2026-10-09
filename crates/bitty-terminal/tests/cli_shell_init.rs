//! `bitty shell-init` end-to-end dispatch proofs (CTX-1054, issue #1813).
//!
//! `bitty completion <shell>` already emits static completion scripts, but
//! nothing installs or wires them. `bitty shell-init <shell>` emits the
//! shell-integration script (prompt hooks for OSC 7 cwd plus OSC 133
//! prompt-start/exit-status marks, then one eval line wiring the matching
//! `bitty completion <shell>` output), and `bitty init` points at it, so a
//! fresh fish/zsh/bash gets working Tab completion plus cwd/status marks.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `shell-init` dispatches before config load and GUI startup, so every case
//! here is headless-safe: no display, no instance, no plugin VM.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn run_bitty_isolated(home: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("HOME", home)
        .env("SHELL", "/bin/bash")
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PLUGIN_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Fresh isolated scratch root for one test.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-shell-init-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn every_shell_emits_hooks_and_completion_wiring() {
    let wiring = [
        ("bash", "bitty completion bash"),
        ("zsh", "bitty completion zsh"),
        ("fish", "bitty completion fish"),
        ("powershell", "bitty completion powershell"),
        ("nushell", "bitty completion nushell"),
    ];
    for (shell, wire) in wiring {
        let out = run_bitty(&["shell-init", shell]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "shell {shell}: {}",
            stderr(&out)
        );
        let script = stdout(&out);
        assert!(script.contains("bitty"), "shell {shell} names bitty");
        assert!(
            script.contains("]133;"),
            "shell {shell} marks OSC 133 prompt/status"
        );
        assert!(script.contains("]7;"), "shell {shell} reports OSC 7 cwd");
        assert!(
            script.contains(wire),
            "shell {shell} wires Tab completion via {wire:?}"
        );
    }
}

#[test]
fn shell_aliases_match_completion() {
    for (alias, canonical) in [("pwsh", "powershell"), ("nu", "nushell")] {
        let via_alias = run_bitty(&["shell-init", alias]);
        let via_canonical = run_bitty(&["shell-init", canonical]);
        assert_eq!(via_alias.status.code(), Some(0));
        assert_eq!(stdout(&via_alias), stdout(&via_canonical));
    }
}

#[test]
fn shell_init_help_names_shells_and_wiring() {
    let out = run_bitty(&["shell-init", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        assert!(text.contains(shell), "help names {shell}");
    }
    assert!(text.contains("bitty completion"), "help names wiring");
}

#[test]
fn shell_init_rejects_usage_errors() {
    for args in [
        vec!["shell-init"],
        vec!["shell-init", "tcl"],
        vec!["shell-init", "bash", "zsh"],
        vec!["shell-init", "bash", "--bogus"],
        vec!["shell-init", "--"],
        vec!["--socket", "/tmp/x.sock", "shell-init", "bash"],
        vec!["--instance", "i:1", "shell-init", "fish"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "args {args:?} must fail closed: {}",
            stderr(&out)
        );
        assert!(stdout(&out).is_empty(), "no stdout script on usage error");
    }
}

#[test]
fn shell_init_ignores_envelope_flags() {
    for args in [
        vec!["shell-init", "--format", "json", "bash"],
        vec!["shell-init", "fish", "--no-color"],
        vec!["--format=json", "shell-init", "zsh"],
    ] {
        let out = run_bitty(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "args {args:?}: {}",
            stderr(&out)
        );
        assert!(
            stdout(&out).contains("bitty"),
            "args {args:?} still emit the script"
        );
    }
}

#[test]
fn bash_script_parses_cleanly() {
    let out = run_bitty(&["shell-init", "bash"]);
    assert_eq!(out.status.code(), Some(0));
    let dir = scratch_dir("bash-parse");
    let script = dir.join("bitty-shell-init.bash");
    std::fs::write(&script, stdout(&out)).expect("write bash script");
    // Git Bash on Windows mangles backslashes in native paths (C:\...),
    // so pass the path with forward slashes (C:/...), which it accepts.
    let script_arg = script.to_string_lossy().replace('\\', "/");
    let check = Command::new("bash")
        .arg("-n")
        .arg(&script_arg)
        .output()
        .expect("spawn bash -n");
    assert!(
        check.status.success(),
        "bash -n rejects emitted script {}: exit={} stdout={} stderr={}",
        script_arg,
        check.status,
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn fish_script_parses_cleanly_when_fish_exists() {
    let fish = Command::new("fish")
        .args(["--version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !fish {
        return;
    }
    let out = run_bitty(&["shell-init", "fish"]);
    assert_eq!(out.status.code(), Some(0));
    let dir = scratch_dir("fish-parse");
    let script = dir.join("bitty-shell-init.fish");
    std::fs::write(&script, stdout(&out)).expect("write fish script");
    let check = Command::new("fish")
        .arg("--no-execute")
        .arg(script.as_os_str())
        .output()
        .expect("spawn fish --no-execute");
    assert!(
        check.status.success(),
        "fish --no-execute rejects emitted script: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn completion_scripts_complete_shell_init() {
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        let out = run_bitty(&["completion", shell]);
        assert_eq!(out.status.code(), Some(0));
        assert!(
            stdout(&out).contains("shell-init"),
            "completion {shell} completes the new shell-init word"
        );
    }
}

#[test]
fn init_points_at_shell_init() {
    let home = scratch_dir("init-hint");
    let out = run_bitty_isolated(&home, &["init", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "init --yes: {}", stderr(&out));
    assert!(
        stdout(&out).contains("shell-init"),
        "init success points at shell integration"
    );
    let target = home.join("bitty").join("init.lua");
    assert!(target.exists(), "init --yes writes init.lua");
}
