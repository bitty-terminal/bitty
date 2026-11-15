//! `bitty ctl` end-to-end dispatch proofs (CTX-0171).
//!
//! Canonical: `docs/interfaces/cli.md` (`ctl` section) as refined by
//! `docs/specifications/cli-contract-rfc.md` (`bitty ctl`, runtime class).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `ctl --help`, usage errors, and `instance list` are headless-safe (no
//! display, instance, or plugin VM). Verbs needing a live instance fail
//! closed with exit 6 (`Unavailable`) when none exists — never a silent
//! pick and never a hang (5 s IPC timeouts).

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

#[test]
fn ctl_help_exits_zero_without_instance() {
    let output = run_bitty(&["ctl", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "ctl --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty ctl") && text.contains("terminal send"),
        "ctl --help must describe control verbs, got {text:?}"
    );
}

#[test]
fn ctl_bare_with_no_args_is_usage_error() {
    let output = run_bitty(&["ctl"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty ctl` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_unknown_verb_is_usage_error() {
    let output = run_bitty(&["ctl", "frobnicate", "list"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown resource must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("unknown"),
        "diagnostic must name the problem, got {:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_stray_separator_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "list", "--"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_bad_format_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "list", "--format", "bogus"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bad --format must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_send_missing_text_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "send", "t:1"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "send without TEXT must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_close_invalid_id_is_usage_error() {
    let output = run_bitty(&["ctl", "terminal", "close", "nope"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bad terminal id must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_split_two_directions_is_usage_error() {
    let output = run_bitty(&["ctl", "view", "split", "--left", "--right"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "two split dirs must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_instance_list_exits_zero_without_instance() {
    // Local discovery: no IPC, no scope, never requires a live peer.
    let output = run_bitty(&["ctl", "instance", "list"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "instance list must exit 0, stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_instance_list_json_is_versioned_envelope() {
    let output = run_bitty(&["ctl", "instance", "list", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "instance list --format json must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("\"v\":1")
            && text.contains("\"command\":\"core.instance.list\"")
            && text.contains("\"ok\":true"),
        "json must be the v1 envelope, got {text:?}"
    );
    // Stdout must be exactly one JSON value (no interleaved logs).
    assert_eq!(
        text.trim().lines().count(),
        1,
        "json stdout must be one value, got {text:?}"
    );
}

#[test]
fn ctl_runtime_verb_without_instance_is_unavailable() {
    // No live instance in test env (isolated XDG_RUNTIME_DIR): must fail
    // closed with exit 6, never hang, never pick an unrelated instance.
    let output = run_bitty(&[
        "ctl",
        "--socket",
        "/tmp/bitty-ctl-test-nonexistent.sock",
        "terminal",
        "list",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_word_is_subcommand_not_program() {
    // `bitty ctl` never spawns a program named `ctl`; unknown verbs are
    // usage errors, not shell executions.
    let output = run_bitty(&["ctl", "terminal", "dance"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown verb must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        !stdout(&output).contains("dance"),
        "must not execute anything, got {:?}",
        stdout(&output)
    );
}

#[test]
fn ctl_help_names_workspace_verbs() {
    // CTX-0257 entry: help lists the workspace verbs with their id shape.
    let output = run_bitty(&["ctl", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        text.contains("workspace list")
            && text.contains("workspace new")
            && text.contains("workspace close ws:N")
            && text.contains("workspace focus ws:N"),
        "help must name workspace verbs, got {text:?}"
    );
}

#[test]
fn ctl_help_names_panel_list_and_panel_needs_live_instance() {
    // CTX-1117 (issue #1900): `panel list` is a first-class ctl verb with
    // the same needs-live-instance semantics as other runtime verbs.
    // CTX-1158 (issue #1967) wired it end to end via the `bitty-ipc`
    // listPanels scope, so help and usage advertise it again.
    let output = run_bitty(&["ctl", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        text.contains("panel list") && text.contains("core.panel.list"),
        "help must name panel list, got {text:?}"
    );
    // Usage names it too.
    let output = run_bitty(&["ctl", "panel"]);
    assert_eq!(output.status.code(), Some(2));
    // No live instance fails closed with exit 6 (same as workspace list).
    let output = run_bitty(&[
        "ctl",
        "--socket",
        "/tmp/bitty-ctl-test-nonexistent.sock",
        "panel",
        "list",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={:?}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn ctl_panel_list_live_roundtrip_via_test_mode_instance() {
    // CTX-1158 (issue #1967): `panel list` must round-trip live through the
    // enqueue path (dispatcher scope plus control queue plus drain), not just
    // parse or apply headless. Spawns an isolated `--test-mode` instance (own
    // PID, own socket, no desktop), runs the built binary's `ctl panel list`
    // against it, and asserts exit 0 plus panel data in JSON and table
    // shapes. No elevation: `view.inspect` rides the CLI default.
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct LiveDir(std::path::PathBuf);
    impl LiveDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::path::PathBuf::from(format!(
                "/tmp/bitty-ctl-panel-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create live dir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("live dir mode 0700");
            Self(path)
        }
    }
    impl Drop for LiveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct LiveInstance {
        child: Child,
        socket: String,
        stderr_path: std::path::PathBuf,
        _dir: LiveDir,
    }
    impl LiveInstance {
        fn spawn() -> Self {
            let dir = LiveDir::new();
            let socket = dir.0.join("s.sock").to_string_lossy().into_owned();
            assert!(socket.len() < 100, "socket path must fit AF_UNIX: {socket}");
            let stderr_path = dir.0.join("stderr.log");
            let stderr = std::fs::File::create(&stderr_path).expect("live stderr log");
            let mut cmd = Command::new(BITTY_BIN);
            cmd.arg("--test-mode")
                .arg("--safe")
                .env("BITTY_SOCKET", &socket)
                .env("BITTY_INSTANCE_ID", "e2e-ctl-panel")
                .env("SHELL", "/bin/sh")
                .env("XDG_CONFIG_HOME", dir.0.as_os_str())
                .env("XDG_DATA_HOME", dir.0.as_os_str())
                .env("XDG_RUNTIME_DIR", dir.0.as_os_str())
                .env("HOME", dir.0.as_os_str())
                .env_remove("BITTY_CTL_ELEVATE")
                .current_dir(dir.0.as_path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr));
            let child = cmd
                .spawn()
                .unwrap_or_else(|err| panic!("spawn {BITTY_BIN}: {err}"));
            Self {
                child,
                socket,
                stderr_path,
                _dir: dir,
            }
        }

        fn stderr_tail(&self) -> String {
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        }

        fn wait_for_socket(&mut self) {
            let start = Instant::now();
            let deadline = Duration::from_secs(20);
            loop {
                if std::path::Path::new(&self.socket).exists() {
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

        fn run_ctl(&self, args: &[&str]) -> Output {
            // Explicit `--socket` bypasses discovery; clear inherited
            // endpoint env so the test never picks a developer instance.
            let mut cmd = Command::new(BITTY_BIN);
            cmd.args(args)
                .env_remove("BITTY_SOCKET")
                .env_remove("BITTY_INSTANCE_ID");
            cmd.output()
                .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
        }
    }
    impl Drop for LiveInstance {
        fn drop(&mut self) {
            // Own PID only: never pattern-kill, never touch other instances.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let mut instance = LiveInstance::spawn();
    instance.wait_for_socket();

    // JSON is the machine contract: single value, v1 envelope, ok true,
    // tiled panel with bounds and focus marker. Poll briefly: the socket
    // file can exist just before the drain loop starts serving.
    let start = Instant::now();
    let deadline = Duration::from_secs(20);
    let json_text = loop {
        let output = instance.run_ctl(&[
            "ctl",
            "--socket",
            instance.socket.as_str(),
            "panel",
            "list",
            "--format",
            "json",
        ]);
        let text = stdout(&output);
        if output.status.code() == Some(0)
            && text.contains("\"ok\":true")
            && text.contains("\"command\":\"core.panel.list\"")
        {
            break text;
        }
        assert!(
            start.elapsed() < deadline,
            "panel list never succeeded; exit={:?} stdout={text:?} stderr={:?} server:\n{}",
            output.status.code(),
            stderr(&output),
            instance.stderr_tail()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(
        json_text.contains("\"v\":1")
            && json_text.contains("\"id\":\"p:1\"")
            && json_text.contains("\"view\":\"v:1\"")
            && json_text.contains("\"bounds\"")
            && json_text.contains("\"role\":\"tiled\"")
            && json_text.contains("\"focused\":true"),
        "live panel JSON must carry the tiled panel, got {json_text:?}"
    );
    assert_eq!(
        json_text.trim().lines().count(),
        1,
        "json stdout must be one value, got {json_text:?}"
    );

    // Table is the human shape: header plus the focused marker row.
    let output = instance.run_ctl(&["ctl", "--socket", instance.socket.as_str(), "panel", "list"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "panel list table must exit 0, stderr={:?}",
        stderr(&output)
    );
    let table = stdout(&output);
    assert!(
        table.contains("PANEL VIEW BOUNDS ROLE FOCUSED") && table.contains("p:1"),
        "live panel table must list p:1, got {table:?}"
    );
}

#[test]
fn ctl_workspace_bad_id_is_usage_error() {
    for args in [
        &["ctl", "workspace", "close", "v:2"] as &[&str],
        &["ctl", "workspace", "focus", "t:1"],
        &["ctl", "workspace", "close", "ws:007"],
        &["ctl", "workspace", "focus"] as &[&str],
        &["ctl", "workspace", "close", "ws:2", "extra"] as &[&str],
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
fn ctl_workspace_verb_without_instance_is_unavailable() {
    // Like other runtime verbs: no live instance fails closed with exit 6.
    let output = run_bitty(&[
        "ctl",
        "--socket",
        "/tmp/bitty-ctl-test-nonexistent.sock",
        "workspace",
        "list",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn ctl_consent_help_names_grant_and_revoke() {
    let output = run_bitty(&["ctl", "--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "ctl --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("consent grant") && text.contains("consent revoke-session"),
        "help must name consent verbs, got {text:?}"
    );
}

#[test]
fn ctl_consent_parse_errors_are_usage_errors() {
    // Issue #1520: malformed consent requests fail closed before any IPC.
    for args in [
        &["ctl", "consent", "grant"] as &[&str],
        &["ctl", "consent", "grant", "--scope", "view.manage"] as &[&str],
        &[
            "ctl",
            "consent",
            "grant",
            "--scope",
            "debug.control",
            "--terminal",
            "t:1",
        ] as &[&str],
        &["ctl", "consent", "revoke"] as &[&str],
        &[
            "ctl",
            "consent",
            "revoke-session",
            "--scope",
            "debug.control",
        ] as &[&str],
        &["ctl", "consent", "frobnicate"] as &[&str],
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
fn ctl_consent_grant_without_instance_is_unavailable() {
    // The grant resolves its target before prompting: with no live
    // instance it fails closed with exit 6 and never prompts.
    let output = run_bitty(&[
        "ctl",
        "--socket",
        "/tmp/bitty-ctl-test-nonexistent.sock",
        "consent",
        "revoke-session",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing socket must be Unavailable (exit 6), stderr={:?}",
        stderr(&output)
    );
}

// ── P0-AC-022 live elevation matrix (CTX-1842, bitty#2921) ───────────────
//
// The criterion's "granted only inspect/read scope" arm, on the terminal
// instance rather than the daemon: a real `bitty --test-mode` process serves
// its own `0600` Unix socket and the built `bitty ctl` client drives it. Each
// elevated verb is asked once with the shipped CLI default and once with a
// one-scope `BITTY_CTL_ELEVATE` allowlist, so the proof is per scope: an
// allowlist for `config.modify` must not also move `terminal.manage`.

/// A live `--test-mode` instance on a private socket (CTX-1842).
///
/// Runs the real `Runtime` plus the real servo with no display, GPU, or event
/// loop, exactly as `crate::test_mode` describes. The root is `0700`, the
/// socket lives inside it, and the child is killed by its own PID on drop: no
/// inherited `BITTY_SOCKET`, no developer instance, no host service.
#[cfg(unix)]
struct LiveCtl {
    child: std::process::Child,
    socket: String,
    root: std::path::PathBuf,
    stderr_log: std::path::PathBuf,
}

#[cfg(unix)]
impl LiveCtl {
    fn spawn(label: &str, elevate: Option<&str>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::path::PathBuf::from(format!(
            "/tmp/bitty-ctl-elev-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create live root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("live root mode 0700");
        let socket = root.join("s.sock").to_string_lossy().into_owned();
        assert!(socket.len() < 100, "socket path must fit AF_UNIX: {socket}");
        let stderr_log = root.join("stderr.log");
        let stderr = std::fs::File::create(&stderr_log).expect("live stderr log");
        let mut command = Command::new(BITTY_BIN);
        command
            .arg("--test-mode")
            .env_remove("BITTY_CTL_ELEVATE")
            .env("BITTY_SOCKET", &socket)
            .env("BITTY_INSTANCE_ID", label)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("XDG_CONFIG_HOME", root.as_os_str())
            .env("XDG_DATA_HOME", root.as_os_str())
            .env("XDG_RUNTIME_DIR", root.as_os_str())
            .env("HOME", root.as_os_str())
            .current_dir(root.as_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        if let Some(scopes) = elevate {
            command.env("BITTY_CTL_ELEVATE", scopes);
        }
        let child = command
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {BITTY_BIN}: {err}"));
        let mut live = Self {
            child,
            socket,
            root,
            stderr_log,
        };
        live.wait_for_socket();
        live
    }

    fn stderr_tail(&self) -> String {
        std::fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }

    fn wait_for_socket(&mut self) {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let deadline = Duration::from_secs(20);
        loop {
            if std::path::Path::new(&self.socket).exists() {
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

    fn ctl(&self, args: &[&str]) -> Output {
        let mut command = Command::new(BITTY_BIN);
        command
            .args(["ctl", "--socket", self.socket.as_str()])
            .args(args)
            .args(["--format", "json"])
            .env_remove("BITTY_SOCKET")
            .env_remove("BITTY_INSTANCE_ID");
        command
            .output()
            .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
    }
}

#[cfg(unix)]
impl Drop for LiveCtl {
    fn drop(&mut self) {
        // Own PID only: never a pattern kill, never another instance.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Assert one `bitty ctl` cell is the CLI's permission refusal for `scope`.
#[cfg(unix)]
fn assert_scope_denied(live: &LiveCtl, args: &[&str], scope: &str) {
    let output = live.ctl(args);
    let text = stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(7),
        "{args:?} must exit 7 (permission), stdout={text:?} stderr={:?}",
        stderr(&output)
    );
    assert!(
        text.contains("\"class\":\"Denied\"") && text.contains("\"code\":\"ScopeDenied\""),
        "{args:?} must carry class Denied and code ScopeDenied, got {text:?}"
    );
    assert!(
        text.contains(&format!("scope '{scope}'")),
        "{args:?} must name the missing scope {scope}, got {text:?}"
    );
}

/// Assert one `bitty ctl` cell succeeded, retrying until the test-mode drain
/// loop services the request (it starts just after the socket appears).
#[cfg(unix)]
fn assert_allowed(live: &LiveCtl, args: &[&str]) -> String {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let deadline = Duration::from_secs(20);
    loop {
        let output = live.ctl(args);
        let text = stdout(&output);
        if output.status.code() == Some(0) && text.contains("\"ok\":true") {
            return text;
        }
        assert!(
            start.elapsed() < deadline,
            "{args:?} never succeeded; exit={:?} stdout={text:?} stderr={:?} server:\n{}",
            output.status.code(),
            stderr(&output),
            live.stderr_tail()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn ctl_elevation_is_per_scope_on_a_live_instance() {
    // The shipped default: `terminal.inspect/input`, `view.inspect/manage`,
    // `config.inspect`, `plugin.inspect` and nothing else.
    let plain = LiveCtl::spawn("default", None);

    // Read and view-manage verbs ride the default and must succeed.
    let listed = assert_allowed(&plain, &["terminal", "list"]);
    assert!(
        listed.contains("\"total_lower_bound\":1"),
        "the default instance starts with one terminal, got {listed:?}"
    );
    assert_allowed(&plain, &["view", "list"]);
    assert_allowed(&plain, &["panel", "list"]);

    // The four elevated verbs are refused, each naming its own scope.
    assert_scope_denied(&plain, &["terminal", "spawn"], "terminal.manage");
    assert_scope_denied(&plain, &["terminal", "close", "t:1"], "terminal.manage");
    assert_scope_denied(&plain, &["workspace", "close", "ws:1"], "terminal.manage");
    assert_scope_denied(&plain, &["config", "reload"], "config.modify");

    // No partial state: the refused spawn created nothing.
    let after = assert_allowed(&plain, &["terminal", "list"]);
    assert!(
        after.contains("\"total_lower_bound\":1"),
        "a refused spawn must not create a terminal, got {after:?}"
    );

    // A one-scope allowlist elevates that scope only: `config.modify` moves the
    // reload verb and leaves `terminal.manage` exactly where it was.
    let config_only = LiveCtl::spawn("config-only", Some("config.modify"));
    let reloaded = assert_allowed(&config_only, &["config", "reload"]);
    assert!(
        reloaded.contains("\"reloaded\":true"),
        "an elevated reload must report its effect, got {reloaded:?}"
    );
    assert_scope_denied(&config_only, &["terminal", "spawn"], "terminal.manage");

    // The mirror arm: `terminal.manage` moves the spawn and leaves
    // `config.modify` denied, so neither allowlist is a blanket grant.
    let manage_only = LiveCtl::spawn("manage-only", Some("terminal.manage"));
    let spawned = assert_allowed(&manage_only, &["terminal", "spawn"]);
    assert!(
        spawned.contains("\"spawned\":true") && spawned.contains("\"terminal_id\":\"t:2\""),
        "an elevated spawn must report the new terminal, got {spawned:?}"
    );
    assert_scope_denied(&manage_only, &["config", "reload"], "config.modify");
    let two = assert_allowed(&manage_only, &["terminal", "list"]);
    assert!(
        two.contains("\"total_lower_bound\":2"),
        "the elevated spawn must be visible, got {two:?}"
    );
}
