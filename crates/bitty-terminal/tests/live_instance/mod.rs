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
//! - [`TestInstance::spawn_graphical`] runs the windowed app under a
//!   compositor *the test owns* (a private `Xvfb` it spawns, or a socket named
//!   by `BITTY_TEST_COMPOSITOR` inside its own temp root), so the verbs that
//!   need an app thread (the `new-window` lane) are actually serviced. It
//!   returns `None` when no test-owned compositor can be obtained, which is how
//!   a headless CI runner reports the cell unrun without pretending it ran.
//!
//! # Safety: never the host's compositor
//!
//! The workspace bans host-compositor GUI testing. On 2026-09-24 a test driving
//! the owner's live Hyprland session segfaulted Hyprland and logged the owner
//! out; spawning probe windows on that session is part of the ban. This fixture
//! therefore performs **no ambient discovery**: it never reads the caller's
//! `WAYLAND_DISPLAY` / `DISPLAY`, and never globs `/run/user/<uid>` for a
//! `wayland-*` socket. [`resolve_test_compositor`] panics — not skips — when
//! `HYPRLAND_INSTANCE_SIGNATURE` is set or a requested socket resolves into the
//! ambient session, and the windowed cells are `#[ignore]`d so a plain
//! `cargo test` cannot open a window at all.
//!
//! Every instance owns a `0700` root, its socket inside it, and its own PID:
//! Drop kills that PID only, never a pattern kill, and never an instance it
//! did not start. Nothing is inherited from the host's `BITTY_SOCKET` /
//! `BITTY_INSTANCE_ID` / `BITTY_CTL_ELEVATE`.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
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
        //
        // `BITTY_TEST_ROOT` names the parent directory instead, because the
        // windowed cell needs an operator-readable path for its explicit
        // `BITTY_TEST_COMPOSITOR` socket and the PID-derived leaf is not
        // knowable in advance. The tag stays the leaf either way, so concurrent
        // cells never share a root. Unset (the default, and every headless
        // cell) keeps the exact path this fixture always used.
        let path = match std::env::var_os("BITTY_TEST_ROOT") {
            Some(root) if !root.is_empty() => PathBuf::from(root).join(tag),
            _ => PathBuf::from(format!("/tmp/bitty-e2e-{}-{tag}", std::process::id())),
        };
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
    /// Present only for a windowed instance: the test-owned compositor whose
    /// lifetime this instance pins. Declared last so it outlives the app that
    /// is attached to it.
    _compositor: Option<TestCompositor>,
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
            _compositor: None,
        }
    }

    /// Spawn the windowed app under a compositor this test owns.
    ///
    /// Returns `None` when no test-owned compositor can be obtained (no `Xvfb`
    /// and no `BITTY_TEST_COMPOSITOR`): the new-window lane needs a real event
    /// loop that only the windowed app drains, so such a host reports the cell
    /// unrun instead of failing. There is no ambient fallback — see
    /// [`TestCompositor`].
    pub(crate) fn spawn_graphical(tag: &str) -> Option<Self> {
        let dir = TestDir::new(tag);
        let compositor = TestCompositor::acquire(&dir, tag)?;
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
            // The display connection installed below is the ONLY one this app
            // may see: the ambient session is scrubbed, never inherited.
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env("BITTY_SOCKET", &socket)
            .env("BITTY_INSTANCE_ID", format!("e2e-{tag}"))
            // The window-registration record the cells cite is an info line.
            .env("BITTY_LOG", "info")
            .env("SHELL", "/bin/sh")
            .env("XDG_CONFIG_HOME", dir.path())
            .env("XDG_DATA_HOME", dir.path())
            .env("XDG_RUNTIME_DIR", compositor.runtime_dir())
            .env("HOME", dir.path())
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        compositor.apply(&mut cmd);
        let child = cmd
            .spawn()
            .unwrap_or_else(|err| panic!("spawn {BITTY_BIN}: {err}"));
        let mut instance = Self {
            child,
            socket,
            stderr_path,
            _dir: dir,
            _compositor: Some(compositor),
        };
        // A windowed start that dies before serving is the test-owned
        // compositor's refusal to give the app a surface, not a scope answer:
        // report it as unrun.
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

/// The only display connection a windowed test instance may use: one the test
/// itself owns.
///
/// The workspace bans host-compositor GUI testing. On 2026-09-24 a test driving
/// the owner's live Hyprland session segfaulted Hyprland and logged the owner
/// out; spawning probe windows on that session is part of the ban. Ambient
/// discovery (`WAYLAND_DISPLAY`, `DISPLAY`, or a `wayland-*` glob of
/// `/run/user/<uid>`) is therefore not merely fragile here — on the owner's
/// machine it attaches a real windowed `bitty` to the owner's desktop.
///
/// A windowed instance is served by exactly one of:
///
/// - a private `Xvfb` this test spawns on a display number it picks and records
///   inside its own temp root, killed on drop (the `Xvfb` binary comes from
///   `PATH` or an explicit `BITTY_TEST_XVFB`);
/// - a socket named by `BITTY_TEST_COMPOSITOR`, which
///   [`resolve_test_compositor`] refuses unless it resolves inside this test's
///   private temp root.
///
/// Anything else is unrun. Two inputs panic instead of skipping — the owner's
/// session and the ambient runtime directory — because a skip there would hide
/// an attempt to touch the owner's desktop.
struct TestCompositor {
    kind: CompositorKind,
    /// The directory the app gets as `XDG_RUNTIME_DIR`: always under the test's
    /// own root, never the host's.
    runtime_dir: PathBuf,
}

enum CompositorKind {
    /// A socket the caller named with `BITTY_TEST_COMPOSITOR`, already proven
    /// to resolve inside this test's private temp root.
    Explicit { socket: PathBuf },
    /// A private `Xvfb` this test spawned and therefore owns.
    PrivateXvfb {
        display: String,
        child: Child,
        lock: PathBuf,
    },
}

/// How long a freshly spawned `Xvfb` gets to publish its display socket.
const XVFB_READY_DEADLINE: Duration = Duration::from_secs(10);

impl TestCompositor {
    /// Acquire a test-owned compositor, or `None` for the `AC022_UNRUN` path.
    fn acquire(dir: &TestDir, tag: &str) -> Option<Self> {
        let env = CompositorEnv::from_process(dir.path());
        let xvfb = find_xvfb();
        match resolve_test_compositor(&env, xvfb.as_deref())? {
            CompositorSource::Explicit(socket) => {
                let runtime_dir = socket.parent()?.to_path_buf();
                Some(Self {
                    kind: CompositorKind::Explicit { socket },
                    runtime_dir,
                })
            }
            CompositorSource::PrivateXvfb => Self::spawn_xvfb(&xvfb?, dir, tag),
        }
    }

    fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Install the display connection for this compositor, and nothing else.
    fn apply(&self, cmd: &mut Command) {
        match &self.kind {
            CompositorKind::Explicit { socket } => {
                let name = socket
                    .file_name()
                    .expect("a socket inside the temp root has a file name");
                cmd.env("WAYLAND_DISPLAY", name);
            }
            CompositorKind::PrivateXvfb { display, .. } => {
                cmd.env("DISPLAY", display);
            }
        }
    }

    /// Spawn a private `Xvfb` and record the display number in `dir`.
    ///
    /// The number is probed from a PID-derived offset and recorded in the
    /// test's own root, so two concurrent cells never pick the same one and the
    /// choice is attributable in an artifact.
    fn spawn_xvfb(xvfb: &Path, dir: &TestDir, tag: &str) -> Option<Self> {
        let start = 90 + (std::process::id() % 60);
        for number in start..=start + 80 {
            let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
            if lock.exists() || socket.exists() {
                continue;
            }
            let log = match std::fs::File::create(dir.path().join(format!("xvfb-{tag}.log"))) {
                Ok(log) => log,
                Err(err) => {
                    eprintln!("live_instance: cannot create the Xvfb log: {err}");
                    return None;
                }
            };
            let mut child = match Command::new(xvfb)
                .arg(format!(":{number}"))
                .args(["-screen", "0", "800x600x24", "-nolisten", "tcp"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    eprintln!("live_instance: cannot spawn {}: {err}", xvfb.display());
                    return None;
                }
            };
            let _ = std::fs::write(dir.path().join("display"), number.to_string());
            let deadline = Instant::now() + XVFB_READY_DEADLINE;
            loop {
                if socket.exists() {
                    return Some(Self {
                        kind: CompositorKind::PrivateXvfb {
                            display: format!(":{number}"),
                            child,
                            lock,
                        },
                        runtime_dir: dir.path().to_path_buf(),
                    });
                }
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        eprintln!("live_instance: no free X display number for a private Xvfb");
        None
    }
}

impl Drop for TestCompositor {
    fn drop(&mut self) {
        if let CompositorKind::PrivateXvfb { child, lock, .. } = &mut self.kind {
            let _ = child.kill();
            let _ = child.wait();
            // Only ever ours: this variant exists because this process spawned
            // the server that created the lock.
            let _ = std::fs::remove_file(&*lock);
        }
    }
}

/// The environment the compositor resolver may consult.
///
/// Captured as data rather than read inside the resolver, so the refusal rules
/// are provable without mutating this process's environment (`set_var` is
/// `unsafe` in edition 2024 and racy under the parallel test harness).
struct CompositorEnv {
    /// `HYPRLAND_INSTANCE_SIGNATURE`: present means this process is inside the
    /// owner's live session. Checked first, so nothing can override it.
    hyprland_signature: Option<String>,
    /// `BITTY_TEST_COMPOSITOR`: an explicit, test-owned display socket.
    test_compositor: Option<PathBuf>,
    /// This test's private temp root (an existing, `0700` directory).
    temp_root: PathBuf,
    /// Current uid, for the `/run/user/<uid>` ambient-session boundary.
    uid: u32,
    /// Base for a relative `BITTY_TEST_COMPOSITOR`.
    cwd: PathBuf,
}

impl CompositorEnv {
    fn from_process(temp_root: &Path) -> Self {
        Self {
            hyprland_signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                .ok()
                .filter(|value| !value.is_empty()),
            test_compositor: std::env::var_os("BITTY_TEST_COMPOSITOR")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty()),
            temp_root: temp_root.to_path_buf(),
            uid: nix::unistd::Uid::current().as_raw(),
            cwd: std::env::current_dir().unwrap_or_else(|_| temp_root.to_path_buf()),
        }
    }
}

/// What a windowed instance was allowed to attach to.
enum CompositorSource {
    /// The caller-named socket, proven to live inside the test's temp root.
    Explicit(PathBuf),
    /// A private `Xvfb` the test will spawn itself.
    PrivateXvfb,
}

/// Decide what a windowed instance may attach to, or refuse loudly.
///
/// Returns `None` only when nothing test-owned is available; that is the
/// `AC022_UNRUN` path, never a fallback to the ambient session. Two inputs are
/// hard refusals (`panic!`, not `None`):
///
/// - `HYPRLAND_INSTANCE_SIGNATURE` present: the caller is inside the owner's
///   live session, and any window spawned here lands on it;
/// - an explicit socket that resolves into `/run/user/<uid>` (the ambient
///   session's runtime directory) or outside this test's private temp root.
fn resolve_test_compositor(env: &CompositorEnv, xvfb: Option<&Path>) -> Option<CompositorSource> {
    if env.hyprland_signature.is_some() {
        panic!(
            "AC022 compositor refusal: HYPRLAND_INSTANCE_SIGNATURE is set, so this process is \
             inside the owner's live Hyprland session. The workspace bans host-compositor GUI \
             testing: on 2026-09-24 a test driving that session segfaulted Hyprland and logged \
             the owner out, and spawning probe windows on it is part of that ban. A windowed cell \
             runs only under a compositor the test itself owns (a private Xvfb, or \
             BITTY_TEST_COMPOSITOR inside the test's own temp root); run it outside the owner's \
             session, never against it."
        );
    }
    if let Some(candidate) = &env.test_compositor {
        let socket = normalize_path(&env.cwd, candidate);
        refuse_ambient_socket(env.uid, &socket);
        if !socket.starts_with(&env.temp_root) {
            panic!(
                "AC022 compositor refusal: BITTY_TEST_COMPOSITOR={} resolves outside this test's \
                 private temp root ({}). The workspace bans host-compositor GUI testing \
                 (2026-09-24: driving the owner's live Hyprland session segfaulted it and logged \
                 the owner out), so a window may only attach to a compositor this test created.",
                socket.display(),
                env.temp_root.display()
            );
        }
        // A symlink inside the temp root must not reach out of it either.
        if let Ok(real) = std::fs::canonicalize(&socket) {
            refuse_ambient_socket(env.uid, &real);
            if !real.starts_with(&env.temp_root) {
                panic!(
                    "AC022 compositor refusal: BITTY_TEST_COMPOSITOR={} resolves (after symlinks) \
                     to {}, outside this test's private temp root ({}). The workspace bans \
                     host-compositor GUI testing (2026-09-24: driving the owner's live Hyprland \
                     session segfaulted it and logged the owner out).",
                    socket.display(),
                    real.display(),
                    env.temp_root.display()
                );
            }
        }
        return Some(CompositorSource::Explicit(socket));
    }
    if xvfb.is_some() {
        return Some(CompositorSource::PrivateXvfb);
    }
    None
}

/// Refuse a socket that resolves into the owner's ambient session.
fn refuse_ambient_socket(uid: u32, socket: &Path) {
    let ambient_root = PathBuf::from(format!("/run/user/{uid}"));
    if socket.starts_with(&ambient_root) {
        panic!(
            "AC022 compositor refusal: {} lives under /run/user/{uid}/, the owner's ambient \
             session runtime directory. The workspace bans host-compositor GUI testing \
             (2026-09-24: driving the owner's live Hyprland session segfaulted it and logged the \
             owner out, and spawning probe windows there is part of the ban). Point \
             BITTY_TEST_COMPOSITOR at a socket inside the test's own temp root, or let the test \
             spawn a private Xvfb.",
            socket.display()
        );
    }
}

/// Make `path` absolute against `base` and resolve `.`/`..` lexically.
fn normalize_path(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// `Xvfb` binary: an explicit `BITTY_TEST_XVFB`, else the first one on `PATH`.
fn find_xvfb() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("BITTY_TEST_XVFB") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        eprintln!(
            "live_instance: BITTY_TEST_XVFB={} is not a file; treating this host as having no \
             test-owned compositor",
            path.display()
        );
        return None;
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join("Xvfb"))
        .find(|candidate| candidate.is_file())
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

#[cfg(test)]
mod compositor_refusal {
    //! Proof that the ambient-session escape hatch is closed.
    //!
    //! These are ordinary unit tests: they build a [`CompositorEnv`] by hand, so
    //! no environment mutation, no display server, and no window is involved.
    //! Each refusal assertion fails — not compiles away — if the guard it covers
    //! is removed, which is what makes them a proof rather than a comment.

    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Mutex;

    use super::*;

    /// The panic hook is process-global; two of these tests running at once must
    /// not leave the silent hook installed for the rest of the run.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    fn env_for(temp_root: &Path) -> CompositorEnv {
        CompositorEnv {
            hyprland_signature: None,
            test_compositor: None,
            temp_root: temp_root.to_path_buf(),
            uid: 4242,
            cwd: temp_root.to_path_buf(),
        }
    }

    /// The message from the panic `body` raised, or a failure if it did not
    /// panic at all: a refusal that stops refusing has to go red here.
    fn refusal(body: impl FnOnce()) -> String {
        let _hook = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = catch_unwind(AssertUnwindSafe(body));
        std::panic::set_hook(previous);
        match result {
            Ok(()) => panic!("expected a compositor refusal, but the resolver accepted the input"),
            Err(payload) => {
                if let Some(message) = payload.downcast_ref::<String>() {
                    message.clone()
                } else if let Some(message) = payload.downcast_ref::<&'static str>() {
                    (*message).to_owned()
                } else {
                    String::from("<non-string panic payload>")
                }
            }
        }
    }

    #[test]
    fn refuses_the_owners_live_hyprland_session() {
        let dir = TestDir::new("ac022-refusal-hyprland");
        let mut env = env_for(dir.path());
        env.hyprland_signature = Some(String::from("0123456789abcdef"));
        // A private Xvfb is available and still is not used: the refusal is a
        // stop, not a fall-through to the next source.
        let message = refusal(|| {
            let _ = resolve_test_compositor(&env, Some(Path::new("/usr/bin/Xvfb")));
        });
        assert!(
            message.contains("HYPRLAND_INSTANCE_SIGNATURE"),
            "the refusal must name the ambient-session variable: {message}"
        );
        assert!(
            message.contains("bans host-compositor GUI testing"),
            "the refusal must name the workspace ban: {message}"
        );
    }

    #[test]
    fn refuses_a_display_socket_outside_the_test_root() {
        let dir = TestDir::new("ac022-refusal-socket");

        // The ambient session's own runtime directory.
        let mut env = env_for(dir.path());
        env.test_compositor = Some(PathBuf::from("/run/user/4242/wayland-0"));
        let message = refusal(|| {
            let _ = resolve_test_compositor(&env, None);
        });
        assert!(
            message.contains("/run/user/4242/wayland-0"),
            "the refusal must name the ambient socket: {message}"
        );
        assert!(
            message.contains("bans host-compositor GUI testing"),
            "the refusal must name the workspace ban: {message}"
        );

        // A socket that resolves outside the temp root belongs to someone else.
        let mut env = env_for(dir.path());
        env.test_compositor = Some(dir.path().join("..").join("ac022-foreign-wayland-0"));
        let message = refusal(|| {
            let _ = resolve_test_compositor(&env, None);
        });
        assert!(
            message.contains("outside this test's private temp root"),
            "the refusal must name the containment rule: {message}"
        );

        // Control: the same resolver accepts a socket inside the root, so the
        // two refusals above are not vacuous.
        let owned = dir.path().join("wayland-7");
        let mut env = env_for(dir.path());
        env.test_compositor = Some(owned.clone());
        match resolve_test_compositor(&env, None) {
            Some(CompositorSource::Explicit(socket)) => assert_eq!(socket, owned),
            _ => panic!("a socket inside the test root must resolve to Explicit"),
        }

        // And a host with no test-owned compositor is unrun: `None`, neither a
        // refusal nor an attachment.
        let env = env_for(dir.path());
        assert!(resolve_test_compositor(&env, None).is_none());
    }
}
