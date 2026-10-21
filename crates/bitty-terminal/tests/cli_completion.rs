//! `bitty completion` end-to-end dispatch proofs (CTX-0763, issue #1375).
//!
//! Canonical: `docs/specifications/cli-contract-rfc.md` (`bitty completion`,
//! local class, Bash/Zsh/Fish/PowerShell/Nushell) with the stable `comp`
//! alias.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `completion` dispatches before config load and GUI startup, so every case
//! here is headless-safe: no display, no instance, no plugin VM.

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
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
fn every_shell_emits_a_script() {
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        let out = run_bitty(&["completion", shell]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "shell {shell}: {}",
            stderr(&out)
        );
        let script = stdout(&out);
        assert!(script.contains("bitty"), "shell {shell} script names bitty");
        assert!(
            script.contains("version"),
            "shell {shell} completes version"
        );
        assert!(
            script.contains("completion"),
            "shell {shell} completes itself"
        );
        // The hidden `component` alias (issue #1959) must not appear as a
        // standalone token; only the canonical `extension` spelling is
        // offered. Token-boundary match so `--with-components` does not
        // false-positive.
        assert!(
            !script
                .split(|c: char| !c.is_alphanumeric())
                .any(|tok| tok == "component"),
            "shell {shell} leaks hidden `component` alias"
        );
    }
}

#[test]
fn comp_alias_behaves_identically() {
    let canonical = run_bitty(&["completion", "bash"]);
    let alias = run_bitty(&["comp", "bash"]);
    assert_eq!(alias.status.code(), Some(0));
    assert_eq!(stdout(&alias), stdout(&canonical));
    let help = run_bitty(&["comp", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("bitty comp"));
}

#[test]
fn completion_help_names_shells() {
    let out = run_bitty(&["completion", "--help"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    for shell in ["bash", "zsh", "fish", "powershell", "nushell"] {
        assert!(text.contains(shell), "help names {shell}");
    }
    assert!(text.contains("grammar"), "help names the shared grammar");
}

const SHELLS: [&str; 5] = ["bash", "zsh", "fish", "powershell", "nushell"];

fn golden_path(shell: &str) -> String {
    format!(
        "{}/tests/fixtures/completion/{shell}.txt",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn scripts_match_golden_fixtures() {
    // Per-shell golden files pin the exact emitted script. Regenerate with
    // `bitty completion <shell> > crates/bitty-terminal/tests/fixtures/completion/<shell>.txt`
    // whenever the CLI grammar intentionally changes.
    for shell in SHELLS {
        let expected = std::fs::read_to_string(golden_path(shell))
            .unwrap_or_else(|err| panic!("read golden for {shell}: {err}"));
        let out = run_bitty(&["completion", shell]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "shell {shell}: {}",
            stderr(&out)
        );
        assert_eq!(
            stdout(&out),
            expected,
            "shell {shell} completion drifted from its golden fixture"
        );
    }
}

fn script_is_available(program: &str, version_arg: &str) -> bool {
    Command::new(program)
        .arg(version_arg)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write_script(tag: &str, shell: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "bitty-completion-parse-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("completion.{shell}"));
    let out = run_bitty(&["completion", shell]);
    assert_eq!(out.status.code(), Some(0));
    std::fs::write(&path, stdout(&out)).expect("write completion script");
    (dir, path)
}

#[test]
fn scripts_parse_in_native_shells() {
    // Parse-only: never executes the completion bodies.
    let cases: [(&str, &str, &[&str]); 4] = [
        ("bash", "bash", &["-n"]),
        ("zsh", "zsh", &["-n"]),
        ("fish", "fish", &["--no-execute"]),
        ("nushell", "nu", &["--no-config-file", "--commands"]),
    ];
    for (shell, program, args) in cases {
        if !script_is_available(program, "--version") {
            eprintln!("skipping {shell} parse: {program} unavailable");
            continue;
        }
        let (dir, path) = write_script("parse", shell);
        let mut command = Command::new(program);
        command.args(args);
        if shell == "nushell" {
            command.arg(format!("source {}", path.display()));
        } else {
            command.arg(&path);
        }
        let check = command.output().expect("spawn native parser");
        assert!(
            check.status.success(),
            "{program} rejects {shell} completion: stdout={} stderr={}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn powershell_script_parses_when_pwsh_exists() {
    if !script_is_available("pwsh", "-Version") {
        eprintln!("skipping powershell parse: pwsh unavailable");
        return;
    }
    let (dir, path) = write_script("parse", "powershell");
    let literal = path.to_string_lossy().replace('\'', "''");
    let check = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[void][scriptblock]::Create((Get-Content -Raw -LiteralPath '{literal}'))"),
        ])
        .output()
        .expect("spawn pwsh");
    assert!(
        check.status.success(),
        "pwsh rejects completion script: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn every_subcommand_is_completed_per_shell() {
    // End-to-end guard for #2372: every top-level word and every nested
    // verb from the parser-owned grammar tables appears in all five scripts.
    let required: &[&str] = &[
        "config",
        "path",
        "check",
        "edit",
        "profile",
        "plugin",
        "install",
        "extension",
        "show-source",
        "workspace",
        "move-panel",
        "shell-init",
        "completion",
        "doctor",
        "inspect",
        "dev",
        "trace",
        "latency",
        "dump",
        "grid",
        "overlay",
    ];
    for shell in SHELLS {
        let script = stdout(&run_bitty(&["completion", shell]));
        for word in required {
            assert!(
                script.contains(word),
                "completion {shell} must offer {word:?}"
            );
        }
    }
}

#[test]
fn completion_rejects_usage_errors() {
    for args in [
        vec!["completion"],
        vec!["completion", "tcl"],
        vec!["completion", "bash", "zsh"],
        vec!["completion", "bash", "--bogus"],
        vec!["completion", "--"],
        vec!["--socket", "/tmp/x.sock", "completion", "bash"],
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
