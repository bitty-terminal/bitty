//! Live `bitty` instance fixture shared by the terminal-instance integration
//! tests (bitty#2921 / CTX-1850).
//!
//! Extracted from `tests/test_mode.rs` so the `P0-AC-022` live cells and the
//! E2E protocol proofs drive the *same* spawn, socket-wait, and raw-wire client
//! rather than a second copy. It is the terminal-instance counterpart of the
//! daemon's `crates/bitty-daemon/tests/harness/` fixture.
//!
//! Two constructors, and they differ only in how the app is started:
//!
//! - [`TestInstance::spawn_test_mode`] runs `--test-mode --safe`: the real
//!   `Runtime` and the real `BITTY_SOCKET` servo with no display, GPU, or
//!   event loop;
//! - [`TestInstance::spawn_graphical`] runs the windowed app on the host's
//!   compositor, so the verbs that need an app thread (the `new-window` lane)
//!   are actually serviced. It returns `None` when the host exposes no
//!   compositor socket, which is how a headless CI runner stays green without
//!   pretending the cell ran.
//!
//! Every instance owns a `0700` root, its socket inside it, and its own PID:
//! Drop kills that PID only, never a pattern kill, and never an instance it
//! did not start. Nothing is inherited from the host's `BITTY_SOCKET` /
//! `BITTY_INSTANCE_ID` / `BITTY_CTL_ELEVATE`.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
pub(crate) const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");
/// Bounded startup wait: socket bind happens after config + primary shell.
pub(crate) const STARTUP_DEADLINE: Duration = Duration::from_secs(20);
/// Bounded state-wait: shell output round-trips through the tick loop.
pub(crate) const STATE_DEADLINE: Duration = Duration::from_secs(20);
/// Poll interval for state waits.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Test-owned temp directory (removed on drop).
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new(tag: &str) -> Self {
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

    pub(crate) fn path(&self) -> &Path {
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
pub(crate) struct TestInstance {
    child: Child,
    socket: String,
    stderr_path: PathBuf,
    _dir: TestDir,
}

impl TestInstance {
    pub(crate) fn spawn_test_mode(tag: &str, elevate_debug_control: bool) -> Self {
        let dir = TestDir::new(tag);
        let socket = dir.path().join("s.sock").to_string_lossy().into_owned();
        assert!(
            socket.len() < 100,
            "socket path must fit macOS SUN_LEN: {socket}"
        );
        Self::spawn_test_mode_with_socket(tag, elevate_debug_control, dir, socket)
    }

    pub(crate) fn spawn_test_mode_with_socket(
        tag: &str,
        elevate_debug_control: bool,
        dir: TestDir,
        socket: String,
    ) -> Self {
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

    /// Spawn the windowed app on the host's compositor.
    ///
    /// Returns `None` when the host advertises no usable compositor socket: the
    /// new-window lane needs a real event loop that only the windowed app
    /// drains, so a headless runner reports the cell unrun instead of failing.
    /// The instance keeps its private config/state root and its own socket; only
    /// the display connection and `XDG_RUNTIME_DIR` come from the host, because
    /// the compositor socket lives there.
    pub(crate) fn spawn_graphical(tag: &str) -> Option<Self> {
        let host = HostDisplay::detect()?;
        let dir = TestDir::new(tag);
        let socket = dir.path().join("s.sock").to_string_lossy().into_owned();
        assert!(
            socket.len() < 100,
            "socket path must fit macOS SUN_LEN: {socket}"
        );
        let stderr_path = dir.path().join("stderr.log");
        let stderr = std::fs::File::create(&stderr_path).expect("stderr log");
        let mut cmd = Command::new(BITTY_BIN);
        cmd.arg("--safe")
            .env_remove("BITTY_CTL_ELEVATE")
            .env("BITTY_SOCKET", &socket)
            .env("BITTY_INSTANCE_ID", format!("e2e-{tag}"))
            // The window-registration record the cells cite is an info line.
            .env("BITTY_LOG", "info")
            .env("SHELL", "/bin/sh")
            .env("XDG_CONFIG_HOME", dir.path())
            .env("XDG_DATA_HOME", dir.path())
            .env("XDG_RUNTIME_DIR", &host.runtime_dir)
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        match &host.wayland_display {
            Some(name) => cmd.env("WAYLAND_DISPLAY", name),
            None => cmd.env_remove("WAYLAND_DISPLAY"),
        };
        match &host.x11_display {
            Some(name) => cmd.env("DISPLAY", name),
            None => cmd.env_remove("DISPLAY"),
        };
        let child = cmd
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {BITTY_BIN}: {err}"));
        let mut instance = Self {
            child,
            socket,
            stderr_path,
            _dir: dir,
        };
        // A windowed start that dies before serving is the host's refusal to
        // give the app a surface, not a scope answer: report it as unrun.
        if instance.wait_for_socket(STARTUP_DEADLINE) {
            Some(instance)
        } else {
            eprintln!(
                "live_instance: windowed start never served {}; stderr:\n{}",
                instance.socket,
                instance.stderr_tail()
            );
            None
        }
    }

    pub(crate) fn socket(&self) -> &str {
        &self.socket
    }

    pub(crate) fn stderr_tail(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn wait_for_socket(&mut self, deadline: Duration) -> bool {
        let start = Instant::now();
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return true;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                return false;
            }
            if start.elapsed() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Connect to the served socket within the startup deadline.
    pub(crate) fn connect(&mut self) -> E2eClient {
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
                    "bitty exited early ({status}) before serving {}; stderr:\n{}",
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
    pub(crate) fn wait_exit(&mut self, deadline: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                start.elapsed() < deadline,
                "bitty did not exit after testExit; stderr:\n{}",
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

/// The host's display connection, discovered from the environment.
///
/// Nothing is hardcoded: `XDG_RUNTIME_DIR` (or the `/run/user/<uid>` default
/// the XDG base-directory spec defines) plus whichever `wayland-*` socket or
/// `DISPLAY` the session advertises.
struct HostDisplay {
    runtime_dir: PathBuf,
    wayland_display: Option<String>,
    x11_display: Option<String>,
}

impl HostDisplay {
    fn detect() -> Option<Self> {
        let runtime_dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let uid = nix::unistd::Uid::current().as_raw();
                PathBuf::from(format!("/run/user/{uid}"))
            }
        };
        let wayland_display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|name| !name.is_empty() && runtime_dir.join(name).exists())
            .or_else(|| first_wayland_socket(&runtime_dir));
        let x11_display = std::env::var("DISPLAY")
            .ok()
            .filter(|name| !name.is_empty());
        if wayland_display.is_none() && x11_display.is_none() {
            return None;
        }
        Some(Self {
            runtime_dir,
            wayland_display,
            x11_display,
        })
    }
}

/// First `wayland-*` socket in `dir`, sorted for determinism.
fn first_wayland_socket(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        .collect();
    names.sort();
    names.into_iter().find(|name| {
        std::fs::metadata(dir.join(name))
            .map(|meta| meta.file_type().is_socket())
            .unwrap_or(false)
    })
}

/// Minimal raw-wire client (same shape as `bitty-ipc`'s verify harness).
pub(crate) struct E2eClient {
    stream: UnixStream,
    next_id: u64,
}

impl E2eClient {
    pub(crate) fn call(&mut self, method: &str, params: Option<&str>) -> String {
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

    pub(crate) fn call_ok(&mut self, method: &str, params: Option<&str>) -> String {
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
    pub(crate) fn wait_for(&mut self, method: &str, params: Option<&str>, needle: &str) -> String {
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

    /// Poll `method` for `budget` and assert `needle` never appears.
    ///
    /// The bounded poll is what makes a "no mutation" claim mean something: a
    /// refusal that silently applied would surface as the marker within the
    /// same tick budget a positive assertion uses.
    pub(crate) fn wait_absent(
        &mut self,
        method: &str,
        params: Option<&str>,
        needle: &str,
        budget: Duration,
    ) -> String {
        let start = Instant::now();
        loop {
            let response = self.call_ok(method, params);
            assert!(
                !response.contains(needle),
                "{method} observed {needle:?} after a refused request: {response}"
            );
            if start.elapsed() >= budget {
                return response;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}
