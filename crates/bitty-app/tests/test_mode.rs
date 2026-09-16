//! `bitty --test-mode` end-to-end protocol proofs (CTX-0506, research 043).
//!
//! Drives the built `bitty` binary as an external E2E target: spawns the
//! binary with `--test-mode`, connects to the `BITTY_SOCKET` it serves, and
//! exercises the stable E2E surface over the existing `bitty.debug/*` framing
//! — no display, no GPU, no VM.
//!
//! Surface asserted here (flag-gated, default-deny without the flag):
//! - `bitty.debug/testInfo` handshake (test-mode only)
//! - live panel state: `listViews` / `listTerminals`
//! - panel control: `splitView` (fresh pane + shell), `focusView`, `sendInput`
//! - state assertions: `getTerminalText`, `getGridText` (grid + cursor)
//! - deterministic teardown: `testExit` (elevated `debug.control`)
//!
//! Headless and Unix-only: the servo is a same-UID `0600` Unix socket and
//! test mode runs the tick loop without a display server.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");
/// Bounded startup wait: socket bind happens after config + primary shell.
const STARTUP_DEADLINE: Duration = Duration::from_secs(20);
/// Bounded state-wait: shell output round-trips through the tick loop.
const STATE_DEADLINE: Duration = Duration::from_secs(20);
/// Poll interval for state waits.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Test-owned temp directory (removed on drop).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        // Short `/tmp` leaf: AF_UNIX `SUN_LEN` is 104 on macOS including
        // NUL, and `std::env::temp_dir()` can be long on macOS.
        let path = PathBuf::from(format!("/tmp/bitty-e2e-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create test dir");
        // The servo refuses a socket directory that is not owner-only
        // (0700), exactly like the production `prepare_socket_dir` policy.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("socket dir mode 0700");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawned `bitty` instance plus its PID guard.
///
/// Drop kills the process (the test recorded the PID at spawn; no
/// name/pattern kills are used).
struct TestInstance {
    child: Child,
    socket: String,
    stderr_path: PathBuf,
    _dir: TestDir,
}

impl TestInstance {
    fn spawn_test_mode(tag: &str, elevate_debug_control: bool) -> Self {
        let dir = TestDir::new(tag);
        let socket_path = dir.path().join("s.sock");
        let socket = socket_path.to_string_lossy().into_owned();
        assert!(
            socket.len() < 100,
            "socket path must fit macOS SUN_LEN: {socket}"
        );
        let stderr_path = dir.path().join("stderr.log");
        let stderr = std::fs::File::create(&stderr_path).expect("stderr log");
        let mut cmd = Command::new(BITTY_BIN);
        cmd.arg("--test-mode")
            .arg("--safe")
            .env("BITTY_SOCKET", &socket)
            .env("BITTY_INSTANCE_ID", format!("e2e-{tag}"))
            .env("SHELL", "/bin/sh")
            .env("XDG_CONFIG_HOME", dir.path())
            .env("XDG_DATA_HOME", dir.path())
            .env("XDG_RUNTIME_DIR", dir.path())
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        if elevate_debug_control {
            cmd.env("BITTY_CTL_ELEVATE", "debug.control");
        } else {
            cmd.env_remove("BITTY_CTL_ELEVATE");
        }
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

    /// Connect to the served socket within the startup deadline.
    fn connect(&mut self) -> E2eClient {
        let start = Instant::now();
        loop {
            if let Ok(stream) = UnixStream::connect(&self.socket) {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .expect("write timeout");
                return E2eClient { stream, next_id: 1 };
            }
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "bitty --test-mode exited early ({status}) before serving {}; stderr:\n{}",
                    self.socket,
                    self.stderr_tail()
                );
            }
            assert!(
                start.elapsed() < STARTUP_DEADLINE,
                "timed out waiting for {}; stderr:\n{}",
                self.socket,
                self.stderr_tail()
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Wait for a clean exit after `testExit`.
    fn wait_exit(&mut self, deadline: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                start.elapsed() < deadline,
                "bitty --test-mode did not exit after testExit; stderr:\n{}",
                self.stderr_tail()
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for TestInstance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal raw-wire client (same shape as `bitty-ipc`'s verify harness).
struct E2eClient {
    stream: UnixStream,
    next_id: u64,
}

impl E2eClient {
    fn call(&mut self, method: &str, params: Option<&str>) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = match params {
            Some(p) => format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{p},\"version\":\"1.0\"}}"
            ),
            None => format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"version\":\"1.0\"}}"
            ),
        };
        let wire = encode_frame(envelope.as_bytes()).expect("encode frame");
        self.stream.write_all(&wire).expect("write request");
        self.stream.flush().expect("flush request");
        let response = self.read_framed();
        assert!(
            response.contains(&format!("\"id\":{id}")),
            "response lost correlation id {id}: {response}"
        );
        response
    }

    fn call_ok(&mut self, method: &str, params: Option<&str>) -> String {
        let response = self.call(method, params);
        assert!(
            !response.contains("\"error\""),
            "{method} failed: {response}"
        );
        response
    }

    fn read_framed(&mut self) -> String {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).expect("read header");
        let len = u32::from_be_bytes(header) as usize;
        assert!(len <= MAX_FRAME_BYTES, "frame exceeds bound: {len}");
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).expect("read body");
        String::from_utf8(body).expect("utf8 response")
    }

    /// Poll `method` until the response contains `needle`.
    fn wait_for(&mut self, method: &str, params: Option<&str>, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let response = self.call_ok(method, params);
            if response.contains(needle) {
                return response;
            }
            assert!(
                start.elapsed() < STATE_DEADLINE,
                "{method} never reported {needle:?}: {response}"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

#[test]
fn test_mode_serves_stable_e2e_surface_end_to_end() {
    let mut instance = TestInstance::spawn_test_mode("e2e", true);
    let mut client = instance.connect();

    // 1. Flag-gated handshake: only a `--test-mode` instance answers.
    let info = client.call_ok("bitty.debug/testInfo", None);
    assert!(
        info.contains("\"test_mode\":true")
            && info.contains("\"surface\":\"e2e\"")
            && info.contains("\"protocol\":\"1.0\""),
        "testInfo must identify the E2E surface: {info}"
    );

    // 2. Live panel state: the primary leaf exists and owns the primary
    //    PTY session (the pane registry tracks split-pane sessions only, so
    //    `has_pane_session` is asserted on the split leaf in step 5).
    let views = client.call_ok("bitty.debug/listViews", None);
    assert!(views.contains("\"v:1\""), "primary view missing: {views}");
    let terminals = client.call_ok("bitty.debug/listTerminals", None);
    assert!(
        terminals.contains("\"id\":\"t:1\""),
        "primary terminal missing: {terminals}"
    );

    // 3. Screen + cursor assertion on the primary grid.
    let grid = client.call_ok("bitty.debug/getGridText", None);
    assert!(
        grid.contains("\"cursor\":{") && grid.contains("\"visible\":"),
        "getGridText must expose the cursor: {grid}"
    );

    // 4. Drive the primary pane: send keys, wait for the shell output.
    client.call_ok(
        "bitty.debug/sendInput",
        Some("{\"terminal_id\":\"t:1\",\"text\":\"echo BITTY_E2E_PRIMARY\\n\"}"),
    );
    client.wait_for(
        "bitty.debug/getTerminalText",
        Some("{\"terminal_id\":\"t:1\"}"),
        "BITTY_E2E_PRIMARY",
    );

    // 5. Panel creation: split the focused pane; the new leaf gets its own
    //    shell and focus follows it (CTX-0364/CTX-0387 parity).
    let split = client.call_ok("bitty.debug/splitView", Some("{\"direction\":\"right\"}"));
    assert!(
        split.contains("\"new_view\":\"v:2\""),
        "split must name the fresh view: {split}"
    );
    let terminals = client.call_ok("bitty.debug/listTerminals", None);
    assert!(
        terminals.contains("\"id\":\"t:2\"") && terminals.contains("\"has_pane_session\":true"),
        "split pane must own a live pane session: {terminals}"
    );

    // 6. Drive the fresh pane: send keys, wait for its independent output.
    client.call_ok(
        "bitty.debug/sendInput",
        Some("{\"terminal_id\":\"t:2\",\"text\":\"echo BITTY_E2E_SPLIT\\n\"}"),
    );
    client.wait_for(
        "bitty.debug/getTerminalText",
        Some("{\"terminal_id\":\"t:2\"}"),
        "BITTY_E2E_SPLIT",
    );

    // 7. Focus control stays observable.
    let focus = client.call_ok("bitty.debug/getFocus", None);
    assert!(
        focus.contains("\"focused_view\":2"),
        "split must move focus to the fresh view: {focus}"
    );

    // 8. Deterministic teardown through the existing control queue
    //    (`debug.control` is elevated explicitly; no ambient authority).
    let exit = client.call_ok("bitty.debug/testExit", None);
    assert!(exit.contains("\"exiting\":true"), "testExit: {exit}");
    let status = instance.wait_exit(Duration::from_secs(10));
    assert!(
        status.success(),
        "test-mode must exit 0 after testExit, got {status}; stderr:\n{}",
        instance.stderr_tail()
    );
}

#[test]
fn test_mode_exit_is_not_ambient_authority() {
    // Test mode registers `testExit`, but registration grants nothing: the
    // verb still needs the elevated `debug.control` scope, so a caller
    // without the explicit `BITTY_CTL_ELEVATE` allowlist is denied and the
    // instance keeps serving (no partial state, no shutdown).
    let mut instance = TestInstance::spawn_test_mode("noelev", false);
    let mut client = instance.connect();
    let denied = client.call("bitty.debug/testExit", None);
    assert!(
        denied.contains("\"error\"") && denied.contains("ScopeDenied"),
        "testExit without elevation must be ScopeDenied: {denied}"
    );
    let info = client.call_ok("bitty.debug/testInfo", None);
    assert!(
        info.contains("\"test_mode\":true"),
        "instance must keep serving after a denied testExit: {info}"
    );
}
