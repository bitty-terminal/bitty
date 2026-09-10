//! `terminal.shell` end-to-end spawn proof (CTX-0298, issue #495).
//!
//! The CTX-0295 configuration verification matrix found `terminal.shell`
//! parsed/validated/merged but *dead*: startup spawned only the CLI program
//! or `$SHELL`. These tests drive the built `bitty` binary via
//! `CARGO_BIN_EXE_bitty` with an isolated `XDG_CONFIG_HOME`, a configured
//! `terminal.shell` pointing at a marker-writing fake shell, and prove the
//! configured program is what actually gets spawned — through the real
//! config load -> effective config -> spawn path. `--headless` exits after
//! the bounded synthetic smoke, so every case is display-free.
//!
//! POSIX fake shells keep this `#[cfg(unix)]`; `require_pty!()` additionally
//! honors the force-no-PTY simulation path.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Hard timeout per invocation: headless startup probes GPU briefly; 30 s is
/// ample and a hung probe must never stall the suite.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Fresh isolated scratch root (config + HOME + fake shells + markers).
fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx0298-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes an executable fake shell that records its execution in `marker`.
fn write_marker_shell(dir: &Path, name: &str, marker: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let body = format!(
        "#!/bin/sh\nprintf 'configured-shell-ran' > '{}'\nexit 0\n",
        marker.display()
    );
    std::fs::write(&path, body).expect("write fake shell");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake shell")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake shell");
    path
}

/// Writes `$XDG_CONFIG_HOME/bitty/init.lua` with the given body.
fn write_config(xdg: &Path, body: &str) {
    let dir = xdg.join("bitty");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("init.lua"), body).expect("write init.lua");
}

/// Runs `bitty --headless` under the isolated root with the injected `$SHELL`
/// and a hard timeout (spawn + poll + kill by PID, no shell, no pipes).
fn run_headless(root: &Path, shell_env: &Path) -> Output {
    let mut child = Command::new(BITTY_BIN)
        .args(["--headless"])
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("SHELL", shell_env)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bitty --headless");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("poll bitty") {
            Some(_) => return child.wait_with_output().expect("collect bitty output"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bitty --headless exceeded {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

/// Bounded wait for the fake shell to write its marker.
fn wait_for_marker(marker: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    marker.exists()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn configured_terminal_shell_is_spawned_and_beats_env_shell() {
    bitty_test_support::require_pty!();
    let root = scratch_root("e2e");
    let marker = root.join("ran.marker");
    let fake = write_marker_shell(&root, "configured-shell", &marker);
    write_config(
        &root,
        &format!(
            "return {{\n    terminal = {{ scrollback = 10000, shell = \"{}\" }},\n}}\n",
            fake.display()
        ),
    );

    // Hostile `$SHELL`: if config did not win, no shell could be spawned.
    let output = run_headless(&root, Path::new("/nonexistent-ctx0298-env-shell"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "headless run must exit 0, stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    let fake_str = fake.to_str().expect("utf8 fake path");
    // Wording pinned: config attribution + the exact program handed to the
    // spawn layer (a spawn failure keeps the "effective" line but drops the
    // "spawned default shell" line, so both must be present).
    assert!(
        err.contains(&format!(
            "bitty: effective program \"{fake_str}\" (explicit=false, configured_shell=true)"
        )),
        "configured shell must be the effective program, stderr={err:?}"
    );
    assert!(
        err.contains(&format!("bitty: spawned default shell \"{fake_str}\"")),
        "configured shell must reach a successful spawn, stderr={err:?}"
    );
    assert!(
        err.contains("has_pty=true"),
        "PTY must be owned by the configured shell, stderr={err:?}"
    );
    assert!(
        wait_for_marker(&marker),
        "configured shell must execute as argv[0] (marker missing)"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unconfigured_shell_keeps_env_fallback() {
    bitty_test_support::require_pty!();
    let root = scratch_root("env");
    let marker = root.join("env.marker");
    let env_shell = write_marker_shell(&root, "env-shell", &marker);
    // No terminal table at all: `$SHELL` remains the fallback.
    write_config(&root, "return {}\n");

    let output = run_headless(&root, &env_shell);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("configured_shell=false"),
        "no config shell means configured_shell=false, stderr={:?}",
        stderr(&output)
    );
    assert!(
        wait_for_marker(&marker),
        "unconfigured run keeps spawning `$SHELL` (marker missing)"
    );
    let _ = std::fs::remove_dir_all(&root);
}
