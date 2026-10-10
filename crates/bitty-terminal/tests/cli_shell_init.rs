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
fn bash_script_parses_cleanly_when_bash_exists() {
    // Windows runners expose a WSL stub as bash that exits non-zero with
    // no installed distributions, so probe for a working bash first.
    let bash = Command::new("bash")
        .args(["--version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !bash {
        return;
    }
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
fn bash_hook_runs_first_and_captures_real_status() {
    let bash = Command::new("bash")
        .args(["--version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !bash {
        return;
    }
    let out = run_bitty(&["shell-init", "bash"]);
    assert_eq!(out.status.code(), Some(0));
    let dir = scratch_dir("bash-order");
    let script = dir.join("bitty-shell-init.bash");
    std::fs::write(&script, stdout(&out)).expect("write bash script");
    let script_arg = script.to_string_lossy().replace('\\', "/");
    // A stub `bitty` keeps the trailing completion eval a no-op.
    let probe = dir.join("probe.bash");
    std::fs::write(
        &probe,
        format!(
            "bitty() {{ :; }}\nPROMPT_COMMAND=\"true\"\nsource \"{script_arg}\"\n\
             printf 'ORDER=<%s>\\n' \"$PROMPT_COMMAND\"\nfalse\neval \"$PROMPT_COMMAND\"\n"
        ),
    )
    .expect("write probe");
    let probe_arg = probe.to_string_lossy().replace('\\', "/");
    let check = Command::new("bash")
        .arg(&probe_arg)
        .output()
        .expect("spawn bash probe");
    assert!(check.status.success(), "probe failed: {:?}", check.status);
    let text = String::from_utf8_lossy(&check.stdout).into_owned();
    assert!(
        text.contains("ORDER=<_bitty_prompt_hook;"),
        "hook precedes existing PROMPT_COMMAND entries: {text:?}"
    );
    assert!(
        text.contains("]133;D;1"),
        "hook reports the failing command status, not the prior entry: {text:?}"
    );
    // Array-valued PROMPT_COMMAND keeps every entry with the hook first.
    let array_probe = dir.join("array-probe.bash");
    std::fs::write(
        &array_probe,
        format!(
            "bitty() {{ :; }}\nPROMPT_COMMAND=(true)\nsource \"{script_arg}\"\ndeclare -p PROMPT_COMMAND\n"
        ),
    )
    .expect("write array probe");
    let array_arg = array_probe.to_string_lossy().replace('\\', "/");
    let array_check = Command::new("bash")
        .arg(&array_arg)
        .output()
        .expect("spawn bash array probe");
    let array_text = String::from_utf8_lossy(&array_check.stdout).into_owned();
    assert!(
        array_text.contains("[0]=\"_bitty_prompt_hook\""),
        "hook leads array-valued PROMPT_COMMAND: {array_text:?}"
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
fn osc7_scripts_encode_cwd() {
    // CTX-1074: all five scripts must encode cwd before OSC 7 insertion.
    let bash = stdout(&run_bitty(&["shell-init", "bash"]));
    assert!(bash.contains("_bitty_urlencode"), "bash defines encoder");
    assert!(
        bash.contains(r#"$(_bitty_urlencode "$PWD")"#),
        "bash encodes PWD"
    );
    let zsh = stdout(&run_bitty(&["shell-init", "zsh"]));
    assert!(zsh.contains("_bitty_urlencode"), "zsh defines encoder");
    let fish = stdout(&run_bitty(&["shell-init", "fish"]));
    assert!(
        fish.contains("string escape --style=url"),
        "fish encodes via string escape"
    );
    assert!(
        fish.contains("(_bitty_urlencode (pwd))"),
        "fish encodes pwd"
    );
    let pwsh = stdout(&run_bitty(&["shell-init", "powershell"]));
    assert!(
        pwsh.contains("_BittyUrlEncode"),
        "powershell defines encoder"
    );
    assert!(pwsh.contains("EscapeDataString"), "powershell pct-encodes");
    assert!(
        pwsh.contains(r#"-replace '\\', '/'"#),
        "powershell normalizes backslashes"
    );
    assert!(
        pwsh.contains("$i -eq 0 -and $parts[$i] -match '^[A-Za-z]:$'"),
        "powershell restricts drive colon to first segment (/tmp/C:/x keeps %3A)"
    );
    let nu = stdout(&run_bitty(&["shell-init", "nushell"]));
    assert!(nu.contains("url encode"), "nushell encodes via url encode");
    assert!(
        nu.contains("str replace --all ':' '%3A'"),
        "nushell encodes colons"
    );
}

#[test]
fn bash_osc7_encodes_spaces_and_percent() {
    let bash = Command::new("bash")
        .args(["--version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !bash {
        return;
    }
    let out = run_bitty(&["shell-init", "bash"]);
    assert_eq!(out.status.code(), Some(0));
    let dir = scratch_dir("bash-encode");
    let script = dir.join("bitty-shell-init.bash");
    std::fs::write(&script, stdout(&out)).expect("write bash script");
    let script_arg = script.to_string_lossy().replace('\\', "/");
    let probe = dir.join("encode-probe.bash");
    std::fs::write(
        &probe,
        format!(
            "bitty() {{ :; }}\nsource \"{script_arg}\"\n\
             _bitty_urlencode \"/tmp/foo bar\"\nprintf '\\n'\n\
             _bitty_urlencode \"/tmp/100% legit\"\nprintf '\\n'\n\
             _bitty_urlencode \"/tmp/a#b?c\"\nprintf '\\n'\n"
        ),
    )
    .expect("write probe");
    let probe_arg = probe.to_string_lossy().replace('\\', "/");
    let check = Command::new("bash")
        .arg(&probe_arg)
        .output()
        .expect("spawn bash encode probe");
    assert!(check.status.success(), "probe failed: {:?}", check.status);
    let text = String::from_utf8_lossy(&check.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        vec!["/tmp/foo%20bar", "/tmp/100%25%20legit", "/tmp/a%23b%3Fc"],
        "bash encoder pins spaces and percent signs: {lines:?}"
    );
}

#[test]
fn fish_osc7_encodes_spaces_and_percent() {
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
    let dir = scratch_dir("fish-encode");
    let script = dir.join("bitty-shell-init.fish");
    std::fs::write(&script, stdout(&out)).expect("write fish script");
    let probe = dir.join("encode-probe.fish");
    std::fs::write(
        &probe,
        format!(
            "function bitty; end\nsource {}\n\
             _bitty_urlencode \"/tmp/foo bar\"\n\
             _bitty_urlencode \"/tmp/100% legit\"\n\
             _bitty_urlencode \"/tmp/a#b?c\"\n",
            script.to_string_lossy()
        ),
    )
    .expect("write probe");
    let check = Command::new("fish")
        .arg(probe.as_os_str())
        .output()
        .expect("spawn fish encode probe");
    assert!(
        check.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let text = String::from_utf8_lossy(&check.stdout).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        vec!["/tmp/foo%20bar", "/tmp/100%25%20legit", "/tmp/a%23b%3Fc"],
        "fish encoder pins spaces and percent signs: {lines:?}"
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
