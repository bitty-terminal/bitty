//! `bitty reload` end-to-end proofs (bitty#2927).
//!
//! Canonical: `docs/interfaces/cli.md` (`ctl`/`reload` runtime section) as
//! refined by `docs/specifications/cli-contract-rfc.md`. These tests drive the
//! built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//!
//! Headless-safe cases (`--help`, usage errors, no live instance) need no
//! display, instance, or plugin VM. The live case spawns an isolated
//! `--test-mode` instance (own PID, own socket, own XDG tree) and never
//! touches a developer's running terminal: it kills only the child it
//! spawned.

use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Runs `bitty` with `args`, capturing output.
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

/// Extract a top-level `"key": <digits>` value from a flat JSON object.
#[cfg(unix)]
fn number_field(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let pos = text.find(&needle)?;
    let rest = text[pos + needle.len()..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Whether a top-level `"key": [...]` array contains `item` as a string.
#[cfg(unix)]
fn array_contains(text: &str, key: &str, item: &str) -> bool {
    let needle = format!("\"{key}\":[");
    let Some(pos) = text.find(&needle) else {
        return false;
    };
    let rest = &text[pos + needle.len()..];
    let end = rest.find(']').unwrap_or(rest.len());
    rest[..end].contains(&format!("\"{item}\""))
}

#[test]
fn reload_help_exits_zero_without_instance() {
    let output = run_bitty(&["reload", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "reload --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("bitty reload"), "{text:?}");
    for expected in [
        "--dry-run",
        "theme/palette",
        "keymaps",
        "scrollback",
        "restart-required",
        "plan version",
        "exit 6",
    ] {
        assert!(
            text.contains(expected),
            "reload --help must document {expected:?}, got {text:?}"
        );
    }
}

#[test]
fn reload_is_listed_in_help_and_completion() {
    let output = run_bitty(&["help", "reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "help reload must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("bitty reload"));
    let output = run_bitty(&["--help"]);
    assert!(
        stdout(&output).contains("reload [--dry-run]"),
        "top-level help must list the reload word"
    );
}

#[test]
fn reload_with_missing_socket_is_unavailable() {
    // A stale/absent socket is the "no live instance" contract: exit 6 with
    // the existing unavailable code plus a start-a-terminal hint. The test
    // never probes discovery, so it can never target a developer instance.
    let output = run_bitty(&[
        "reload",
        "--socket",
        "/tmp/bitty-reload-test-nonexistent.sock",
    ]);
    let err = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={err:?}"
    );
    assert!(
        err.contains("no live instance") && err.contains("start bitty"),
        "the diagnostic must carry the no-live-instance code and a hint: {err:?}"
    );
}

#[test]
fn reload_usage_errors_exit_two() {
    for args in [
        &["reload", "extra"][..],
        &["reload", "--bogus"],
        &["reload", "--format", "bogus"],
        &["reload", "--socket"],
        &["reload", "--instance", "Bad/Id"],
        &["reload", "--config", "/tmp/init.lua"],
        &["reload", "--profile", "work"],
        &["--config", "/tmp/init.lua", "reload"],
        &["--profile", "work", "reload"],
        &["reload", "--"],
    ] {
        let output = run_bitty(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be UsageError (exit 2), stderr={:?}",
            stderr(&output)
        );
    }
}

#[test]
fn reload_word_is_subcommand_not_program() {
    // A program literally named `reload` needs `bitty -- reload ...`; the
    // bare word never spawns anything.
    let output = run_bitty(&["reload", "/bin/sh"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray positional must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty(), "must not execute anything");
}

#[cfg(unix)]
mod live {
    use super::{BITTY_BIN, array_contains, number_field, stderr, stdout};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Output, Stdio};
    use std::time::{Duration, Instant};

    /// A 0700 scratch tree: socket dir, XDG config/data/runtime, and the
    /// instance's stderr log.
    struct LiveDir(PathBuf);

    impl LiveDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir()
                .join(format!("bitty-reload-live-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create live dir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("live dir mode 0700");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for LiveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The isolated test-mode instance under test.
    struct LiveInstance {
        child: Child,
        socket: String,
        config: PathBuf,
        stderr_path: PathBuf,
        _dir: LiveDir,
    }

    impl LiveInstance {
        /// Spawn with the given starting `init.lua`. `config.modify` is
        /// granted through the explicit server-side allowlist (the same
        /// elevation `bitty ctl config reload` needs); the client never
        /// carries it.
        fn spawn(config_lua: &str) -> Self {
            let dir = LiveDir::new();
            let bitty_dir = dir.path().join("bitty");
            std::fs::create_dir_all(&bitty_dir).expect("config dir");
            let config = bitty_dir.join("init.lua");
            std::fs::write(&config, config_lua).expect("write config");
            let socket = dir.path().join("s.sock").to_string_lossy().into_owned();
            assert!(socket.len() < 100, "socket path must fit AF_UNIX: {socket}");
            let stderr_path = dir.path().join("stderr.log");
            let stderr = std::fs::File::create(&stderr_path).expect("live stderr log");
            let mut cmd = Command::new(BITTY_BIN);
            cmd.arg("--test-mode")
                .env("BITTY_SOCKET", &socket)
                .env("BITTY_INSTANCE_ID", "e2e-ctl-reload")
                .env("BITTY_CTL_ELEVATE", "config.modify")
                .env("SHELL", "/bin/sh")
                .env("XDG_CONFIG_HOME", dir.path())
                .env("XDG_DATA_HOME", dir.path())
                .env("XDG_RUNTIME_DIR", dir.path())
                .env("HOME", dir.path())
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr));
            let child = cmd
                .spawn()
                .unwrap_or_else(|err| panic!("spawn {BITTY_BIN}: {err}"));
            Self {
                child,
                socket,
                config,
                stderr_path,
                _dir: dir,
            }
        }

        fn stderr_tail(&self) -> String {
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        }

        fn write_config(&self, lua: &str) {
            std::fs::write(&self.config, lua).expect("rewrite config");
        }

        fn wait_for_socket(&mut self) {
            let start = Instant::now();
            let deadline = Duration::from_secs(20);
            loop {
                if Path::new(&self.socket).exists() {
                    return;
                }
                if let Some(status) = self.child.try_wait().expect("try_wait") {
                    panic!(
                        "test-mode exited early ({status}) before serving {}; stderr:\n{}",
                        self.socket,
                        self.stderr_tail()
                    );
                }
                assert!(
                    start.elapsed() < deadline,
                    "timed out waiting for {}; stderr:\n{}",
                    self.socket,
                    self.stderr_tail()
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        /// Run the built binary against this instance with inherited endpoint
        /// env cleared (explicit `--socket` only).
        fn run(&self, args: &[&str]) -> Output {
            let mut cmd = Command::new(BITTY_BIN);
            cmd.args(args)
                .env_remove("BITTY_SOCKET")
                .env_remove("BITTY_INSTANCE_ID")
                .env_remove("BITTY_CTL_ELEVATE");
            cmd.output()
                .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
        }

        /// `bitty reload --socket <s> ...`.
        fn reload(&self, extra: &[&str]) -> Output {
            let mut args = vec!["reload", "--socket", self.socket.as_str()];
            args.extend_from_slice(extra);
            self.run(&args)
        }

        /// `bitty ctl --socket <s> <args...>`.
        fn ctl(&self, args: &[&str]) -> Output {
            let mut full = vec!["ctl", "--socket", self.socket.as_str()];
            full.extend_from_slice(args);
            self.run(&full)
        }

        /// Poll `terminal list --format json` until the drain loop serves it.
        fn wait_for_terminal_list(&self) -> String {
            let start = Instant::now();
            let deadline = Duration::from_secs(20);
            loop {
                let output = self.ctl(&["terminal", "list", "--format", "json"]);
                let text = stdout(&output);
                if output.status.code() == Some(0) && text.contains("\"ok\":true") {
                    return text;
                }
                assert!(
                    start.elapsed() < deadline,
                    "terminal list never succeeded; exit={:?} stdout={text:?} stderr={:?} server:\n{}",
                    output.status.code(),
                    stderr(&output),
                    self.stderr_tail()
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        /// Poll `terminal text t:1` until `marker` appears (the shell echoed
        /// it), then return the captured text.
        fn wait_for_marker(&self, marker: &str) -> String {
            let start = Instant::now();
            let deadline = Duration::from_secs(20);
            loop {
                let output = self.ctl(&["terminal", "text", "t:1", "--format", "json"]);
                let text = stdout(&output);
                if output.status.code() == Some(0) && text.contains(marker) {
                    return text;
                }
                assert!(
                    start.elapsed() < deadline,
                    "marker {marker:?} never reached the pane; exit={:?} stdout={text:?} server:\n{}",
                    output.status.code(),
                    self.stderr_tail()
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    impl Drop for LiveInstance {
        fn drop(&mut self) {
            // Own PID only: never pattern-kill, never touch other instances.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn terminal_count(terminal_list: &str) -> u64 {
        terminal_list.matches("\"terminal_id\":").count() as u64
    }

    #[test]
    fn reload_applies_a_theme_change_and_preserves_panes_pty_and_scrollback() {
        let mut instance = LiveInstance::spawn("return { appearance = { theme = \"dracula\" } }\n");
        instance.wait_for_socket();

        // Seed the pane with a marker so scrollback preservation is provable.
        let before_list = instance.wait_for_terminal_list();
        let pid_before = number_field(&before_list, "child_pid").expect("child_pid");
        let scrollback_before =
            number_field(&before_list, "scrollback_lines").expect("scrollback_lines");
        let terminals_before = terminal_count(&before_list);
        assert_eq!(terminals_before, 1, "one startup pane: {before_list}");
        let send = instance.ctl(&["terminal", "send", "t:1", "echo RELOAD_MARKER_OK\n"]);
        assert_eq!(
            send.status.code(),
            Some(0),
            "seed send failed: {:?} {}",
            send.status.code(),
            stderr(&send)
        );
        instance.wait_for_marker("RELOAD_MARKER_OK");

        // Apply a live theme change without restarting.
        instance.write_config("return { appearance = { theme = \"tokyo-night\" } }\n");
        let reload = instance.reload(&["--format", "json"]);
        let text = stdout(&reload);
        assert_eq!(
            reload.status.code(),
            Some(0),
            "reload must exit 0, stdout={text:?} stderr={:?} server:\n{}",
            stderr(&reload),
            instance.stderr_tail()
        );
        assert_eq!(
            text.trim().lines().count(),
            1,
            "json stdout must be one value: {text:?}"
        );
        for expected in [
            "\"v\":1",
            "\"command\":\"core.config.reload\"",
            "\"ok\":true",
            "\"applied\":true",
            "\"kind\":\"applied\"",
            "\"plan_version\":2",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text:?}");
        }
        assert!(
            array_contains(&text, "changed", "appearance.theme"),
            "the applied change must name appearance.theme: {text:?}"
        );

        // Panes, PTYs, and scrollback survive the reload.
        let after_list = instance.wait_for_terminal_list();
        assert_eq!(
            number_field(&after_list, "child_pid"),
            Some(pid_before),
            "the pane PTY must survive a reload: {after_list}"
        );
        assert_eq!(
            terminal_count(&after_list),
            terminals_before,
            "the pane count must survive a reload: {after_list}"
        );
        let after_scrollback =
            number_field(&after_list, "scrollback_lines").expect("scrollback_lines");
        assert!(
            after_scrollback >= scrollback_before,
            "a reload must never reset scrollback ({scrollback_before} -> {after_scrollback}): {after_list}"
        );
        instance.wait_for_marker("RELOAD_MARKER_OK");

        // --dry-run reports the same list and applies nothing.
        instance.write_config("return { appearance = { theme = \"catppuccin\" } }\n");
        let preview = instance.reload(&["--dry-run", "--format", "json"]);
        let preview_text = stdout(&preview);
        assert_eq!(
            preview.status.code(),
            Some(0),
            "dry run must exit 0, stdout={preview_text:?} stderr={:?}",
            stderr(&preview)
        );
        for expected in [
            "\"applied\":false",
            "\"dry_run\":true",
            "\"kind\":\"would-apply\"",
            "\"plan_version\":2",
        ] {
            assert!(
                preview_text.contains(expected),
                "dry run missing {expected}: {preview_text:?}"
            );
        }
        assert!(
            array_contains(&preview_text, "changed", "appearance.theme"),
            "dry run must report the same change: {preview_text:?}"
        );
        // The human table is not a contract, but it must never read as an
        // applied change.
        let preview_table = instance.reload(&["--dry-run"]);
        let table = stdout(&preview_table);
        assert_eq!(
            preview_table.status.code(),
            Some(0),
            "table dry run must exit 0: {table:?}"
        );
        assert!(
            table.contains("reload dry run on") && table.contains("would change: appearance.theme"),
            "table dry run must report the change list: {table:?}"
        );
        // The real reload after the dry run still sees the change and
        // applies it: the dry run neither applied nor advanced the version.
        let applied = instance.reload(&["--format", "json"]);
        let applied_text = stdout(&applied);
        assert_eq!(applied.status.code(), Some(0), "{applied_text:?}");
        assert!(
            applied_text.contains("\"applied\":true")
                && applied_text.contains("\"plan_version\":3"),
            "the real reload must apply and advance the version: {applied_text:?}"
        );
        assert!(
            array_contains(&applied_text, "changed", "appearance.theme"),
            "the dry run must not have applied the change: {applied_text:?}"
        );

        // A restart-required change is reported and never applied.
        instance.write_config(
            "return { appearance = { theme = \"catppuccin\" }, terminal = { shell = \"/bin/dash\", scrollback = 10000 } }\n",
        );
        let restart = instance.reload(&["--format", "json"]);
        let restart_text = stdout(&restart);
        assert_eq!(
            restart.status.code(),
            Some(0),
            "restart-required is not an error, stdout={restart_text:?} stderr={:?}",
            stderr(&restart)
        );
        for expected in [
            "\"applied\":false",
            "\"kind\":\"restart-required\"",
            "\"plan_version\":3",
        ] {
            assert!(
                restart_text.contains(expected),
                "restart-required missing {expected}: {restart_text:?}"
            );
        }
        assert!(
            array_contains(&restart_text, "restart_required", "terminal.shell"),
            "the restart list must name terminal.shell: {restart_text:?}"
        );
        let after_restart = instance.wait_for_terminal_list();
        assert_eq!(
            number_field(&after_restart, "child_pid"),
            Some(pid_before),
            "a restart-required change must not respawn the pane: {after_restart}"
        );

        // A rejected edit exits 3, names the dotted path, and keeps the plan.
        instance.write_config("return { window = { opacity = 2.0 } }\n");
        let rejected = instance.reload(&["--format", "json"]);
        let rejected_text = stdout(&rejected);
        assert_eq!(
            rejected.status.code(),
            Some(3),
            "a rejected edit must exit 3, stdout={rejected_text:?} stderr={:?}",
            stderr(&rejected)
        );
        assert!(
            rejected_text.contains("\"ok\":false")
                && rejected_text.contains("ConfigError")
                && rejected_text.contains("window.opacity"),
            "the typed error must name the dotted field path: {rejected_text:?}"
        );
        // The running plan is untouched: the pane still answers and the next
        // good reload starts from plan version 3.
        instance.wait_for_marker("RELOAD_MARKER_OK");
        instance.write_config("return { appearance = { theme = \"dracula\" } }\n");
        let recovered = instance.reload(&["--format", "json"]);
        let recovered_text = stdout(&recovered);
        assert_eq!(recovered.status.code(), Some(0), "{recovered_text:?}");
        assert!(
            recovered_text.contains("\"applied\":true")
                && recovered_text.contains("\"plan_version\":4"),
            "a rejected edit must not advance the plan version: {recovered_text:?}"
        );
    }
}
