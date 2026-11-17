//! No-TCP lifecycle cell for the `bittyd` daemon process (bitty#2921).
//!
//! The security corpus records that "no external no-TCP scan" exists for the
//! daemon itself: the claim that `bittyd` holds no listening TCP socket and no
//! outbound TCP connection across its lifecycle has never been observed from
//! outside the process. `crates/bitty-terminal/tests/no_tcp_lifecycle.rs`
//! (`CTX-1865`) is the terminal-instance half; this target is the daemon half.
//!
//! # No `AC-0xx` id is claimed
//!
//! The daemon corpus assigns the no-TCP control no acceptance-criterion id: the
//! property appears only as prose — "The daemon opens no TCP listener" at
//! `crates/bitty-daemon/README.md:89` — with no criterion attached. This target
//! therefore claims no AC id; which criterion (if any) the observation rolls up
//! into is the security reviewer's call.
//!
//! # Two channels, because one of them cannot speak for this process
//!
//! The terminal-side cell attributes `ss` rows to the instance's process tree by
//! PID (`users:(("name",pid=..))`). That works because the instance is
//! dumpable. `bittyd` is not: `src/service/server.rs` (`harden_core_dumps`) runs
//! `PR_SET_DUMPABLE=0` before it binds the endpoint, so an unprivileged same-uid
//! observer cannot read `/proc/<pid>/fd` and `ss -p` can never name the daemon
//! as a socket's owner. A PID-filtered `ss` scan of the daemon alone therefore
//! reports zero rows whether or not a socket exists — exactly the vacuous pass
//! this cell must not ship. (The repo already records the same limitation for
//! `daemon.status`'s self-count at `tests/service_loop.rs:14`.)
//!
//! So the cell observes through two independent channels and states what each
//! one covers:
//!
//! | Arm | Namespace | Channel | Covers | Blind to |
//! | --- | --- | --- | --- | --- |
//! | host | the host's | `ss -tanpH` filtered to the daemon's PID set | every *attributable* member: the daemon's children, and the attached client (this test process) | the daemon's own rows (`PR_SET_DUMPABLE=0`); the arm prints the measured refusal |
//! | private | a fresh network namespace of the daemon's own: an unprivileged user + network namespace, or `sudo -n`'s network namespace | `/proc/<pid>/net/tcp{,6}` of the daemon's own namespace | every TCP socket object created by any process in that namespace — the daemon and the client that attaches to it | a connect attempt that a route lookup rejects before a socket row exists (see below) |
//!
//! The private arm is the one that can speak for the daemon process itself: it
//! reads the kernel's TCP table for the namespace the daemon lives in, so no
//! PID attribution is needed and `PR_SET_DUMPABLE=0` is irrelevant.
//!
//! # The private arm is mandatory: it can fail, never skip
//!
//! This cell exists because the host arm is vacuous for the daemon: `ss -p`
//! cannot name a non-dumpable process, so the host arm reports zero rows
//! whether or not the daemon listens on TCP. A pass therefore has to mean the
//! daemon's own table was read, and the arm is the only thing that reads it.
//! It obtains a private network namespace from the first channel that works on
//! this host:
//!
//! 1. unprivileged `unshare --user --map-root-user --net …`, no privilege
//!    required;
//! 2. the runner's passwordless escalation, `sudo -n unshare --net …`.
//!
//! Both channels are probed with `true`, never with the product, so a `bittyd`
//! that fails inside a namespace cannot read as a host limitation. When neither
//! channel works the cell **fails** with both probe results and the remedy: at
//! that point nothing has observed the daemon's table, and a green run would be
//! the vacuous pass this cell exists to prevent. A host that forbids both may
//! set `BITTY_NOTCP_ALLOW_PRIVATE_SKIP=1` to record the gap instead; the cell
//! then prints `NOTCP private-arm: SKIPPED … UNPROVEN` and claims nothing about
//! the daemon. CI never sets that variable.
//!
//! # Phases
//!
//! | Phase | Host arm | Private arm |
//! | --- | --- | --- |
//! | startup | zero rows for the daemon's PID set | zero rows in the daemon's namespace |
//! | attached | zero rows while a negotiated client is connected, and zero rows for the client process itself | zero rows while a negotiated client is connected inside the same namespace |
//! | after detach | zero rows once the client is gone | zero rows once the client is gone |
//! | teardown | `SIGTERM` drains to exit `0`; no PID of the tree remains, so none holds a socket | the recorded PID is `SIGKILL`ed; `/proc/<pid>` is gone, and with its last member the namespace is destroyed |
//! | abort | a second daemon `SIGKILL`ed at its recorded PID: no PID of the tree remains | (covered by the teardown arm, which is the abort) |
//!
//! The attach is the daemon's real path: a `UnixStream` on the endpoint the
//! daemon bound, `SO_PASSCRED` enabled, and a framed `daemon.negotiate`. No
//! TCP is involved on either side, which is what the "attached" row asserts.
//!
//! # Non-vacuity
//!
//! Five independent checks, so an empty filter cannot pass:
//!
//! 1. [`tcp_absence::self_check`] binds a loopback listener in this test process
//!    and requires the same `ss` filter to attribute it (>= 1 `LISTEN` row);
//! 2. the private arm runs a control child in its *own* fresh namespace that
//!    binds one loopback listener, and requires `/proc/<pid>/net/tcp` to report
//!    it before the daemon's zero is trusted;
//! 3. every live phase asserts the daemon's PID set is non-empty and contains
//!    the PID this cell recorded at spawn — never a name or pattern match;
//! 4. a loopback listener this cell holds in *its own* namespace is required to
//!    appear in this cell's table and to be absent from the daemon's, so a read
//!    of the host's table cannot pass as the daemon's;
//! 5. the driver records `/proc/<pid>/ns/net` for itself and for the daemon, and
//!    the arm asserts those are equal and are not this cell's namespace: the
//!    daemon is a member of the namespace whose table was read.
//!
//! # What this cell does not prove
//!
//! - Inside a private network namespace an outbound `connect()` to a
//!   non-loopback address fails at route lookup without creating a TCP row, so
//!   the private arm proves **no TCP socket object was created**, not that no
//!   attempt was made. The host arm sees no attempt either, but only for the
//!   members it can attribute.
//! - No session is created, so terminal children are out of scope. A child
//!   inherits the daemon's namespace, so the private arm's table would show its
//!   rows; the host arm's `ss` filter would attribute them because children are
//!   dumpable.
//! - The daemon is observed in a namespace this cell creates. That changes
//!   nothing about the code path under test (the daemon binds a Unix socket and
//!   never consults network configuration), but the isolation is the cell's, not
//!   production's.
//! - Under the privileged channel the helper, and through it the daemon, runs as
//!   `root`; the unprivileged channel runs both as mapped root inside a user
//!   namespace. The claim is a socket table, which the uid does not change, and
//!   the host arm drives the same lifecycle as the invoking user.
//! - The parent can read the table of a root-owned daemon but not its
//!   `/proc/<pid>/fd`, its `/proc/<pid>/ns/net`, or signal it, so the namespace
//!   identity comes from the driver (the process that spawned the daemon) and
//!   the teardown is the driver's `SIGKILL`. A failed assertion in that channel
//!   is cleaned up by the driver's own kill-on-drop and, failing that, by the
//!   runner's teardown.
//!
//! Run with `cargo test -p bitty-daemon --test no_tcp_listener_absence --offline
//! -- --nocapture`.
//!
//! # Linux only
//!
//! The scan is `ss`/`/proc` and the second channel is `/proc/<pid>/net`, so the
//! whole target is `cfg(target_os = "linux")` — not `cfg(unix)`. macOS has
//! neither `ss` nor `/proc`, so a `unix` gate would admit a platform on which
//! the cell can only fail; on macOS and Windows this target compiles to
//! nothing, and reports no pass it did not observe.

#![cfg(target_os = "linux")]

#[path = "harness/tcp_absence.rs"]
mod tcp_absence;

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Built by Cargo for this integration test.
const BITTYD: &str = env!("CARGO_BIN_EXE_bittyd");

/// The method that proves a client is attached: the daemon answers it before
/// any scope is granted (`ScopeRequirement::None` for `daemon.negotiate`).
const NEGOTIATE: &str = "daemon.negotiate";

/// One frame exchange bound. The daemon's own request bound is shorter.
const IO_BUDGET: Duration = Duration::from_secs(10);

/// Bounds for a daemon that must bind, and for one that must exit.
const START_BUDGET: Duration = Duration::from_secs(15);
const STOP_BUDGET: Duration = Duration::from_secs(10);

/// Bound for a driver/control helper to reach its next marker file.
const MARKER_BUDGET: Duration = Duration::from_secs(30);

/// Process-poll interval. Bounded work, so a sleep is the honest wait.
const POLL: Duration = Duration::from_millis(10);

/// Environment override for the `unshare` binary under test.
const UNSHARE_ENV: &str = "BITTY_TEST_UNSHARE";
/// Environment override for the `sudo` binary of the privileged channel.
const SUDO_ENV: &str = "BITTY_TEST_SUDO";

/// Absolute candidates for `unshare` (util-linux), then `PATH`.
const UNSHARE_CANDIDATES: [&str; 4] = [
    "/usr/bin/unshare",
    "/bin/unshare",
    "/usr/sbin/unshare",
    "/sbin/unshare",
];

/// Absolute candidates for `sudo`, then `PATH`.
const SUDO_CANDIDATES: [&str; 4] = ["/usr/bin/sudo", "/bin/sudo", "/usr/sbin/sudo", "/sbin/sudo"];

/// Absolute candidates for `env`, which carries the helper's marker root past
/// `sudo`'s `env_reset`.
const ENV_CANDIDATES: [&str; 2] = ["/usr/bin/env", "/bin/env"];

/// Absolute candidates for `true`: the only payload of the namespace probes, so
/// no probe depends on the product.
const TRUE_CANDIDATES: [&str; 2] = ["/bin/true", "/usr/bin/true"];

/// The environment that downgrades "this host cannot create a network
/// namespace" from a failure into a recorded, explicitly-requested gap. CI
/// never sets it, so a green run there always means the daemon's own table was
/// read.
const ALLOW_SKIP_ENV: &str = "BITTY_NOTCP_ALLOW_PRIVATE_SKIP";

/// Marker-file environment for the private-namespace driver helper.
const DRIVER_ROOT_ENV: &str = "BITTY_NOTCP_DRIVER_ROOT";
/// Marker-file environment for the private-namespace listener control.
const CONTROL_ROOT_ENV: &str = "BITTY_NOTCP_CONTROL_ROOT";

/// Test names of the two helpers, as libtest reports them.
const DRIVER_HELPER: &str = "private_namespace_driver";
const CONTROL_HELPER: &str = "private_namespace_listener_control";

/// Distinguishes temp roots of this binary's successive daemons.
static NEXT_ROOT: AtomicU32 = AtomicU32::new(0);

// ── fixture ─────────────────────────────────────────────────────────────────

/// One throwaway runtime/state root per daemon, removed on drop.
struct Root {
    path: PathBuf,
}

impl Root {
    fn new(label: &str) -> Self {
        let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        // Short on purpose: the endpoint path is bounded to
        // `MAX_SOCKET_PATH_BYTES` (107 on Linux) and the temp root is part of it.
        let path = std::env::temp_dir().join(format!(
            "notcp-{label}-{}-{suffix}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test root");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod the test root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn runtime(&self) -> PathBuf {
        self.path.join("runtime")
    }

    fn state(&self) -> PathBuf {
        self.path.join("state")
    }

    fn socket(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.sock")
    }

    fn capability(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.operator")
    }

    fn stderr_log(&self) -> PathBuf {
        self.path.join("daemon.stderr")
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The 0700 runtime dir and the state dir the daemon will write into.
    fn prepare(&self) {
        std::fs::create_dir_all(self.runtime()).expect("create the runtime dir");
        std::fs::set_permissions(self.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod the runtime dir");
        std::fs::create_dir_all(self.state()).expect("create the state dir");
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A daemon this cell launched, and the root it writes into.
struct Daemon {
    child: Child,
    root: Root,
}

impl Daemon {
    fn start(label: &str) -> Self {
        let root = Root::new(label);
        root.prepare();
        let socket = root.socket();
        let capability = root.capability();
        let stderr = root.stderr_log();
        let mut child = spawn_daemon(root.path(), &stderr);
        wait_ready(&socket, &capability, &mut child, &stderr);
        Self { child, root }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn socket(&self) -> PathBuf {
        self.root.socket()
    }

    /// The PID set the host arm's `ss` filter is allowed to attribute to.
    fn tree_pids(&self) -> Vec<u32> {
        tcp_absence::session_pids(self.pid())
    }

    fn stderr_tail(&self) -> String {
        std::fs::read_to_string(self.root.stderr_log()).unwrap_or_default()
    }

    fn wait_exit(&mut self, budget: Duration) -> ExitStatus {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait the daemon") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "bittyd did not exit within {budget:?}; stderr:\n{}",
                self.stderr_tail()
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Launch `bittyd start` against `root`'s runtime and state dirs.
///
/// `root` is the temp root itself (the daemon derives `<root>/runtime` and
/// `<root>/state` from the environment, exactly as it does in production), and
/// the process is put in its own group so the host arm's PID set is the tree
/// this cell started and never a name match. A caller that wants the daemon in
/// a private namespace runs this code inside one — the private arm's driver
/// does — because namespaces are inherited and nothing here has to know.
fn spawn_daemon(root: &Path, stderr: &Path) -> Child {
    let mut command = Command::new(BITTYD);
    command.arg("start");
    command
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env_remove("BITTYD_ENDPOINT")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            File::create(stderr).expect("create the daemon stderr log"),
        ));
    command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn bittyd for {}: {error}", root.display()))
}

/// Wait until the daemon has bound its endpoint and minted its capability.
///
/// Readiness is both facts: the socket file appears before the capability is
/// written, and the capability file is created empty before the token is, so
/// neither alone is readiness. Mirrors `tests/harness/real.rs`.
fn wait_ready(socket: &Path, capability: &Path, child: &mut Child, stderr: &Path) {
    let deadline = Instant::now() + START_BUDGET;
    while Instant::now() < deadline {
        if socket.exists() && capability_ready(capability) {
            return;
        }
        if let Some(status) = child.try_wait().expect("try_wait the daemon") {
            panic!(
                "bittyd exited ({status}) before binding {}; stderr:\n{}",
                socket.display(),
                std::fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        std::thread::sleep(POLL);
    }
    panic!(
        "bittyd did not bind {} within {START_BUDGET:?}; stderr:\n{}",
        socket.display(),
        std::fs::read_to_string(stderr).unwrap_or_default()
    );
}

/// Whether the operator capability file holds a complete token.
fn capability_ready(capability: &Path) -> bool {
    match std::fs::read_to_string(capability) {
        Ok(text) => {
            let trimmed = text.trim_end_matches('\n');
            trimmed.len() == 64 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
        }
        Err(_) => false,
    }
}

/// A client on the daemon's real endpoint: Unix socket, credentials on, framed
/// JSON.
struct Client {
    stream: UnixStream,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let deadline = Instant::now() + START_BUDGET;
        loop {
            match UnixStream::connect(socket) {
                Ok(stream) => {
                    bitty_daemon::service::frame::enable_credentials(&stream)
                        .expect("enable per-frame credentials on the client");
                    stream
                        .set_read_timeout(Some(IO_BUDGET))
                        .expect("set the client read timeout");
                    stream
                        .set_write_timeout(Some(IO_BUDGET))
                        .expect("set the client write timeout");
                    return Self { stream, next_id: 1 };
                }
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "cannot connect to {}: {error}",
                        socket.display()
                    );
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({"v": 1, "id": id, "method": method, "params": params});
        let payload = serde_json::to_vec(&request).expect("encode the request");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        self.stream
            .write_all(&frame)
            .unwrap_or_else(|error| panic!("write the {method} frame: {error}"));
        let mut header = [0u8; 4];
        self.stream
            .read_exact(&mut header)
            .unwrap_or_else(|error| panic!("read the {method} reply header: {error}"));
        let length = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; length];
        self.stream
            .read_exact(&mut body)
            .unwrap_or_else(|error| panic!("read the {method} reply body: {error}"));
        serde_json::from_slice(&body)
            .unwrap_or_else(|error| panic!("parse the {method} reply ({error})"))
    }
}

/// Whether a reply frame reports success.
fn reply_ok(reply: &Value) -> bool {
    reply.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

/// Resolve a program by an optional environment override, then absolute
/// candidates, then `PATH`.
///
/// An override that is not a file is a hard failure: it names the binary this
/// cell must use, so silently falling through to another one would hide a
/// misconfiguration behind a different channel.
fn resolve_binary(env_key: Option<&str>, candidates: &[&str], program: &str) -> Option<PathBuf> {
    if let Some(env_key) = env_key {
        if let Some(explicit) = std::env::var_os(env_key) {
            let path = PathBuf::from(explicit);
            assert!(
                path.is_file(),
                "{env_key}={} is not a file: the private-namespace arm cannot run",
                path.display()
            );
            return Some(path);
        }
    }
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve the `unshare` binary, or `None` when the host has none.
fn unshare_binary() -> Option<PathBuf> {
    resolve_binary(Some(UNSHARE_ENV), &UNSHARE_CANDIDATES, "unshare")
}

/// The channel that gives this cell a private network namespace.
///
/// Both channels run the same driver and therefore the same daemon; they differ
/// only in who is allowed to create the namespace. The arm is mandatory:
/// [`resolve_netns`] fails the cell when neither channel works.
enum Netns {
    /// `unshare --user --map-root-user --net …`: a user + network namespace of
    /// this process's own, with no privilege required.
    UserNamespace { unshare: PathBuf },
    /// `sudo -n unshare --net …`: the runner's passwordless escalation for a
    /// network namespace only. The user namespace is unchanged, so the driver,
    /// the daemon and this cell stay in one.
    Privileged {
        sudo: PathBuf,
        unshare: PathBuf,
        env: PathBuf,
    },
}

impl Netns {
    /// The channel, spelled as the command it runs (without quoting).
    fn label(&self) -> String {
        match self {
            Self::UserNamespace { unshare } => {
                format!("{} --user --map-root-user --net", unshare.display())
            }
            Self::Privileged { sudo, unshare, .. } => {
                format!("{} -n {} --net", sudo.display(), unshare.display())
            }
        }
    }

    /// `<channel> <program> <args>`, with every `env` pair exported to
    /// `program`.
    ///
    /// `sudo` resets the environment, so the privileged channel passes each pair
    /// as an `env(1)` argv prefix instead of depending on `sudoers`' `env_keep`.
    fn command(&self, program: &Path, args: &[&str], env: &[(&str, &Path)]) -> Command {
        let mut command = match self {
            Self::UserNamespace { unshare } => {
                let mut command = Command::new(unshare);
                command
                    .args(["--user", "--map-root-user", "--net", "--"])
                    .arg(program);
                command
            }
            Self::Privileged {
                sudo,
                unshare,
                env: env_binary,
            } => {
                let mut command = Command::new(sudo);
                command.arg("-n").arg(env_binary);
                for (key, value) in env {
                    command.arg(format!("{key}={}", value.display()));
                }
                command.arg(unshare).args(["--net", "--"]).arg(program);
                command
            }
        };
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command
    }

    /// Whether this channel can create a namespace here, and why not when it
    /// cannot.
    ///
    /// The payload is `true`: a probe that ran the product would report a broken
    /// `bittyd` as a host limitation and silently disable the only channel that
    /// can speak for the daemon.
    fn ready(&self) -> Result<(), String> {
        let true_binary = resolve_binary(None, &TRUE_CANDIDATES, "true")
            .ok_or_else(|| format!("no `true` binary in {TRUE_CANDIDATES:?} or on PATH"))?;
        let line = format!("`{}` {}", self.label(), true_binary.display());
        let output = self
            .command(&true_binary, &[], &[])
            .output()
            .map_err(|error| format!("cannot run {line}: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        let code = output
            .status
            .code()
            .map_or_else(|| "a signal".to_owned(), |code| code.to_string());
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!("{line} exited {code}: {detail}"))
    }
}

/// Whether the host was explicitly allowed to record the private arm as an
/// unproven gap instead of failing.
fn private_arm_opt_out() -> bool {
    matches!(
        std::env::var(ALLOW_SKIP_ENV),
        Ok(value) if !value.is_empty() && value != "0"
    )
}

/// The channel this host can give the private arm, or `None` only when both
/// channels failed *and* [`ALLOW_SKIP_ENV`] asked for the gap.
///
/// A host that cannot create a namespace cannot observe the daemon's own socket
/// table, and this cell never reports a pass it did not observe: the default is
/// a typed failure that names both probes, their output, and the remedy.
fn resolve_netns() -> Option<Netns> {
    let unshare = unshare_binary();
    let mut failures: Vec<String> = Vec::new();

    if let Some(unshare) = unshare.clone() {
        let candidate = Netns::UserNamespace { unshare };
        match candidate.ready() {
            Ok(()) => {
                println!(
                    "NOTCP private-arm channel: `{}` created a namespace here without privilege",
                    candidate.label()
                );
                return Some(candidate);
            }
            Err(reason) => failures.push(format!("unprivileged: {reason}")),
        }
    } else {
        failures.push(format!(
            "unprivileged: no `unshare` binary in {UNSHARE_CANDIDATES:?} or on PATH"
        ));
    }

    if let Some(unshare) = unshare {
        let sudo = resolve_binary(Some(SUDO_ENV), &SUDO_CANDIDATES, "sudo");
        let env = resolve_binary(None, &ENV_CANDIDATES, "env");
        match (sudo, env) {
            (Some(sudo), Some(env)) => {
                let candidate = Netns::Privileged { sudo, unshare, env };
                match candidate.ready() {
                    Ok(()) => {
                        println!(
                            "NOTCP private-arm channel: `{}` created a namespace here, so the \
                             unprivileged channel is the one this host refuses",
                            candidate.label()
                        );
                        return Some(candidate);
                    }
                    Err(reason) => failures.push(format!("privileged: {reason}")),
                }
            }
            (None, _) => failures.push(format!(
                "privileged: no `sudo` binary in {SUDO_CANDIDATES:?} or on PATH"
            )),
            (Some(_), None) => failures.push(format!(
                "privileged: no `env` binary in {ENV_CANDIDATES:?} or on PATH to carry the \
                 helper's marker root across `sudo`"
            )),
        }
    }

    let detail = failures
        .iter()
        .map(|failure| format!("\n    {failure}"))
        .collect::<String>();
    if private_arm_opt_out() {
        println!(
            "NOTCP private-arm: SKIPPED — {ALLOW_SKIP_ENV} is set and neither namespace channel \
             works here, so the daemon's own TCP table is UNPROVEN: the host arm's `ss` filter \
             cannot attribute a non-dumpable daemon, and this run claims nothing about it. \
             Probes:{detail}"
        );
        return None;
    }
    panic!(
        "NOTCP private-arm cannot run: neither namespace channel works, and this cell refuses to \
         pass without the only channel that can read the daemon's own TCP table — the host arm's \
         `ss` filter cannot attribute a non-dumpable daemon, so a pass here would be vacuous. \
         Probes:{detail}\n  \
         Remedy: install util-linux's `unshare` and allow unprivileged user namespaces (`sysctl -w \
         kernel.unprivileged_userns_clone=1`; Ubuntu 24.04 also restricts them through AppArmor, \
         `sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`), or grant this user a \
         passwordless `sudo -n unshare --net` entry. A host that forbids both may set \
         {ALLOW_SKIP_ENV}=1 to record the gap instead: the cell then prints the evidence as \
         UNPROVEN instead of failing."
    )
}

fn kill_pid(pid: u32, signal: nix::sys::signal::Signal) {
    let raw = i32::try_from(pid).expect("a recorded PID fits in i32");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), signal).unwrap_or_else(|error| {
        panic!("cannot send {signal:?} to the recorded PID {pid}: {error}")
    });
}

fn proc_entry_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Kills a PID this cell recorded if the cell unwinds before its own teardown.
///
/// The private arm's daemon is a child of the namespace driver, not of this
/// process, so no `Child` handle of ours reaps it; this guard is what keeps a
/// failed assertion from leaving a daemon behind. It is disarmed once the
/// teardown is observed, so a reused PID can never be signalled.
struct PidGuard {
    pid: u32,
    armed: bool,
}

impl PidGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if !self.armed || !proc_entry_exists(self.pid) {
            return;
        }
        if let Ok(raw) = i32::try_from(self.pid) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(raw),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

// ── the cell ────────────────────────────────────────────────────────────────

#[test]
fn daemon_holds_no_tcp_sockets_across_its_lifecycle() {
    // A zero result only means something if the same scan can see a socket.
    let control = tcp_absence::self_check();
    println!(
        "NOTCP control: the ss filter attributed {control} LISTEN row(s) to this test process's \
         own loopback listener, so a zero daemon result is not vacuous"
    );

    host_namespace_arm();
    private_namespace_arm();
}

/// The host-namespace arm: the lifecycle the daemon is normally driven through.
///
/// `ss` attribution covers every member of the daemon's process tree the
/// observer may inspect, which includes the attached client (this test process)
/// and any child the daemon spawns. It cannot cover the daemon itself: the arm
/// measures that refusal and prints it, and the private arm answers for the
/// daemon's own socket table.
fn host_namespace_arm() {
    // ── startup ──────────────────────────────────────────────────────────
    let mut daemon = Daemon::start("host");
    let pid = daemon.pid();
    println!(
        "NOTCP host-arm daemon pid={pid} socket={}",
        daemon.socket().display()
    );
    let startup = tcp_absence::scan_pids(&daemon.tree_pids());
    assert!(
        startup.pids.contains(&pid),
        "the daemon's PID set must include the PID this cell recorded ({pid}): {:?}",
        startup.pids
    );
    startup.assert_no_tcp("host-startup");

    // Measure, rather than assume, what this channel can say about the daemon.
    match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(_) => println!(
            "NOTCP host-arm attribution: `/proc/{pid}/fd` is readable by this observer, so the ss \
             filter can attribute the daemon's own TCP rows here"
        ),
        Err(error) => println!(
            "NOTCP host-arm attribution: `/proc/{pid}/fd` is refused ({error}); the daemon serves \
             non-dumpable (`src/service/server.rs`, `harden_core_dumps`), so the pid filter above \
             speaks for the tree's attributable members only — the daemon's own socket table is \
             the private-namespace arm's claim"
        ),
    }

    // ── attached ─────────────────────────────────────────────────────────
    let mut client = Client::connect(&daemon.socket());
    let reply = client.call(
        NEGOTIATE,
        &json!({"client_version": {"major": 1, "minor": 0}}),
    );
    assert!(
        reply_ok(&reply),
        "the daemon must serve a client while attached: {reply}"
    );
    let attached = tcp_absence::scan_pids(&daemon.tree_pids());
    assert!(
        attached.pids.contains(&pid),
        "the attached scan must still cover the daemon PID: {:?}",
        attached.pids
    );
    attached.assert_no_tcp("host-attached");
    // The other end of the attach is this test process, which is dumpable: the
    // attach transport is observable here and must not be TCP.
    tcp_absence::scan_pids(&[std::process::id()]).assert_no_tcp("host-attached-client");

    // ── after detach ─────────────────────────────────────────────────────
    drop(client);
    // One poll interval is not enough for the servo to notice the close; give
    // it a bounded pause so "after detach" is observable rather than assumed.
    std::thread::sleep(POLL * 20);
    let detached = tcp_absence::scan_pids(&daemon.tree_pids());
    assert!(
        detached.pids.contains(&pid),
        "the daemon must still be alive after its client detached: {:?}",
        detached.pids
    );
    detached.assert_no_tcp("host-after-detach");

    // ── orderly stop ─────────────────────────────────────────────────────
    kill_pid(pid, nix::sys::signal::Signal::SIGTERM);
    let status = daemon.wait_exit(STOP_BUDGET);
    assert!(
        status.success(),
        "SIGTERM must drain and exit 0, got {status}; stderr:\n{}",
        daemon.stderr_tail()
    );
    let after_stop = daemon.tree_pids();
    assert!(
        after_stop.is_empty(),
        "no process may outlive an orderly stop, found {after_stop:?}"
    );
    tcp_absence::scan_pids(&after_stop).assert_no_tcp("host-after-stop");
    println!(
        "NOTCP host-after-stop: the recorded PID {pid} exited 0 and no PID the fixture can still \
         see remains, so the post-mortem scan has nothing left to attribute"
    );

    // ── abort ────────────────────────────────────────────────────────────
    let mut aborted = Daemon::start("host-abort");
    let aborted_pid = aborted.pid();
    let abort_startup = tcp_absence::scan_pids(&aborted.tree_pids());
    assert!(
        abort_startup.pids.contains(&aborted_pid),
        "the aborted daemon's PID set must include the PID this cell recorded ({aborted_pid}): {:?}",
        abort_startup.pids
    );
    abort_startup.assert_no_tcp("host-abort-startup");
    kill_pid(aborted_pid, nix::sys::signal::Signal::SIGKILL);
    let status = aborted.wait_exit(STOP_BUDGET);
    assert!(
        !status.success(),
        "an aborted daemon must not report a clean exit, got {status}"
    );
    let after_abort = aborted.tree_pids();
    assert!(
        after_abort.is_empty(),
        "no process may outlive the recorded PID being killed, found {after_abort:?}"
    );
    tcp_absence::scan_pids(&after_abort).assert_no_tcp("host-after-abort");
    println!(
        "NOTCP host-after-abort: pid {aborted_pid} was killed, no PID the fixture can still see \
         remains, and zero TCP rows were ever attributed to the tree"
    );
}

/// The private-namespace arm: the daemon's own socket table, observed from
/// outside the process.
///
/// The channel (`unshare --user --map-root-user --net`, or the runner's
/// passwordless `sudo -n unshare --net` when unprivileged user namespaces are
/// refused) gives the driver a network namespace of its own, which the daemon
/// inherits. `/proc/<pid>/net/tcp{,6}` then reports only the sockets created
/// inside it — no PID attribution needed, so `PR_SET_DUMPABLE=0` cannot hide
/// anything. The client has to live in the same namespace as the daemon
/// (`src/peer.rs` refuses a peer from another user namespace: "a mapped uid is
/// not same-user success"), so the driver is a re-executed copy of this test
/// binary running inside that namespace and signalling each phase through marker
/// files.
///
/// The channel comes from [`resolve_netns`], which fails the cell when no
/// namespace can be created: this arm is the only evidence that speaks for the
/// daemon, so it is never skipped by default.
fn private_namespace_arm() {
    let Some(netns) = resolve_netns() else {
        return;
    };

    // ── the reader, and not the host's table ─────────────────────────────
    // A loopback listener this cell holds in *its own* namespace. The same
    // reader must report it in this cell's table and must not report it in the
    // daemon's, which is what keeps "zero rows" from being a read of the host's
    // table by accident.
    let carrier = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind the carrier listener in this cell's namespace");
    let carrier_port = carrier
        .local_addr()
        .expect("read the carrier listener's address")
        .port();
    let carrier_row = format!(":{carrier_port:04X}");
    let own_namespace = tcp_absence::namespace_identity(std::process::id())
        .expect("this test process has a network namespace");

    // ── control: the reader must see a socket in a private namespace ─────
    let control_root = Root::new("control");
    let mut control = spawn_helper(
        &netns,
        &["--ignored", "--exact", CONTROL_HELPER, "--nocapture"],
        CONTROL_ROOT_ENV,
        control_root.path(),
        &control_root.path().join("helper"),
    );
    wait_for_marker(
        &control_root,
        "control.port",
        &mut control,
        "control listener",
    );
    let control_rows = tcp_absence::scan_namespace(control.id()).rows.len();
    assert!(
        control_rows >= 1,
        "NOTCP namespace control failed: `/proc/{}/net/tcp{{,6}}` reported no row for a private \
         namespace that holds one loopback listener, so a zero result for the daemon would be \
         vacuous",
        control.id()
    );
    println!(
        "NOTCP namespace-control: the same reader attributed {control_rows} LISTEN row(s) to a \
         private namespace holding one loopback listener (pid {}), so a zero daemon result is not \
         vacuous",
        control.id()
    );
    // Release the control through its marker: under the privileged channel its
    // uid is root, so this process may not be able to signal it.
    std::fs::write(control_root.marker("control.stop"), "1").expect("release the control listener");
    let _ = control.kill();
    let _ = control.wait();

    // ── the daemon itself ────────────────────────────────────────────────
    let root = Root::new("private");
    // The daemon attests that its runtime root is owned by its own uid, so the
    // driver — never this process, whose uid differs under the privileged
    // channel — creates the roots it will attest.
    let mut driver = spawn_helper(
        &netns,
        &["--ignored", "--exact", DRIVER_HELPER, "--nocapture"],
        DRIVER_ROOT_ENV,
        root.path(),
        &root.path().join("helper"),
    );

    wait_for_marker(&root, "startup", &mut driver, "the daemon's startup");
    let daemon_pid = read_pid(&root).unwrap_or_else(|| {
        panic!(
            "the driver did not record the daemon PID; helper stderr:\n{}",
            std::fs::read_to_string(root.path().join("helper.err")).unwrap_or_default()
        )
    });
    let mut daemon_guard = PidGuard::new(daemon_pid);
    // Membership is asserted, not assumed: the daemon must be a member of the
    // namespace the driver ran in, and that namespace must not be this cell's —
    // otherwise the table read would be the host's. The identities come from the
    // driver because a parent of another uid cannot resolve the namespace link
    // of a daemon the privileged channel started as root.
    let daemon_namespace = read_marker(&root, "daemon.netns");
    let driver_namespace = read_marker(&root, "driver.netns");
    assert_eq!(
        daemon_namespace, driver_namespace,
        "the daemon must run in the namespace its driver was started in (daemon \
         {daemon_namespace}, driver {driver_namespace})"
    );
    assert_ne!(
        daemon_namespace, own_namespace,
        "the daemon must run in its own network namespace (daemon {daemon_namespace} == this \
         cell's {own_namespace}), or the table read would be the host's"
    );
    let pid_set = tcp_absence::session_pids(daemon_pid);
    assert!(
        pid_set.contains(&daemon_pid),
        "the daemon's PID set must include the PID the driver recorded ({daemon_pid}): {pid_set:?}"
    );
    println!(
        "NOTCP private-arm daemon pid={daemon_pid} netns={daemon_namespace} (driver netns \
         {driver_namespace}, this cell's netns {own_namespace}) via `{}`",
        netns.label()
    );
    let startup = tcp_absence::scan_namespace(daemon_pid);
    startup.assert_no_tcp("private-startup");

    // ── the table that was read is the daemon's, not the host's ──────────
    let own = tcp_absence::scan_namespace(std::process::id());
    assert!(
        own.rows.iter().any(|row| row.contains(&carrier_row)),
        "the namespace reader must report this cell's own loopback listener (port {carrier_port}) \
         in this cell's table, or it cannot be trusted to report the daemon's"
    );
    assert!(
        !startup.rows.iter().any(|row| row.contains(&carrier_row)),
        "the daemon's table must not be this cell's: it reported the host listener on port \
         {carrier_port}: {:?}",
        startup.rows
    );
    println!(
        "NOTCP namespace-isolation: this cell's table reports its own loopback listener (port \
         {carrier_port}) and the daemon's table does not, so the daemon's table belongs to \
         another namespace"
    );

    wait_for_marker(&root, "attached", &mut driver, "the attached client");
    // The driver writes the daemon's reply into the marker: a phase called
    // "attached" is evidence of an attach only if the daemon actually served it,
    // so the reply is parsed here instead of trusting the marker's arrival.
    let attached_reply: Value = serde_json::from_str(&read_marker(&root, "attached"))
        .unwrap_or_else(|error| panic!("the driver's attach marker is not a reply: {error}"));
    assert!(
        reply_ok(&attached_reply),
        "the daemon must serve the client inside its namespace while attached: {attached_reply}"
    );
    let attached = tcp_absence::scan_namespace(daemon_pid);
    attached.assert_no_tcp("private-attached");
    assert!(
        !attached.rows.iter().any(|row| row.contains(&carrier_row)),
        "the attached scan must still read the daemon's namespace and not this cell's: it \
         reported the host listener on port {carrier_port}: {:?}",
        attached.rows
    );

    wait_for_marker(&root, "detached", &mut driver, "the client's detach");
    tcp_absence::scan_namespace(daemon_pid).assert_no_tcp("private-after-detach");

    wait_for_marker(&root, "stopped", &mut driver, "the daemon's abort");
    assert!(
        !proc_entry_exists(daemon_pid),
        "the recorded PID {daemon_pid} must be gone after the driver's SIGKILL"
    );
    // The teardown is observed: never signal this number again, in case a later
    // process is handed it.
    daemon_guard.disarm();
    println!(
        "NOTCP private-after-abort: pid {daemon_pid} was killed and `/proc/{daemon_pid}` is gone, \
         so the namespace its TCP table belonged to has no member left to hold a socket"
    );
    let status = driver.wait().expect("reap the private-namespace driver");
    assert!(
        status.success(),
        "the private-namespace driver must exit 0, got {status}; helper stderr:\n{}",
        std::fs::read_to_string(root.path().join("helper.err")).unwrap_or_default()
    );
}

/// Spawn one of this binary's `#[ignore]`d helpers inside a fresh private
/// namespace, with its stdout and stderr captured next to the marker files.
fn spawn_helper(
    netns: &Netns,
    args: &[&str],
    env_key: &str,
    env_value: &Path,
    log_prefix: &Path,
) -> Child {
    let executable = std::env::current_exe().expect("resolve this test binary");
    let stdout = File::create(format!("{}.out", log_prefix.display()))
        .expect("create the helper stdout log");
    let stderr = File::create(format!("{}.err", log_prefix.display()))
        .expect("create the helper stderr log");
    let mut command = netns.command(&executable, args, &[(env_key, env_value)]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.spawn().unwrap_or_else(|error| {
        panic!(
            "cannot spawn the helper through `{}`: `{}` {args:?}: {error}",
            netns.label(),
            executable.display()
        )
    })
}

/// The text a helper recorded in `root`/`name`, trimmed.
fn read_marker(root: &Root, name: &str) -> String {
    let path = root.marker(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read the helper's `{}`: {error}", path.display()))
        .trim()
        .to_owned()
}

/// Create the runtime and state roots the daemon attests, as this process's uid.
///
/// The endpoint refuses a runtime root owned by a different user
/// (`src/endpoint.rs`, `attest_runtime_root`), and under the privileged channel
/// the daemon runs as root: the driver, which shares the daemon's uid, is the
/// only process that can create them for it.
fn prepare_daemon_roots(root: &Path) {
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).expect("create the driver's runtime root");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("chmod the driver's runtime root");
    std::fs::create_dir_all(root.join("state")).expect("create the driver's state root");
}

/// Relax the ownership of a driver's tree so the invoking user can remove it.
///
/// A driver the privileged channel started as root owns every directory it
/// created, and the daemon's `0700` runtime directory would stop this cell's
/// `Root` drop from unlinking them. Called once the daemon is dead, so no tamper
/// interval can observe the relaxed mode, and only for a root fixture.
fn hand_back_tree(path: &Path) {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777));
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            hand_back_tree(&entry.path());
        }
    }
}

/// Kills the daemon if the driver unwinds before its own teardown.
///
/// The parent cannot signal a daemon the privileged channel started as root, so
/// the driver is the process that has to reap it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Block until `name` exists in `root`, failing with the helper's own output.
fn wait_for_marker(root: &Root, name: &str, helper: &mut Child, what: &str) {
    let marker = root.marker(name);
    let deadline = Instant::now() + MARKER_BUDGET;
    while Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        if let Some(status) = helper.try_wait().expect("try_wait the helper") {
            panic!(
                "the private-namespace helper exited ({status}) before {what}; helper stdout:\n\
                 {}\nhelper stderr:\n{}",
                std::fs::read_to_string(format!("{}.out", root.path().join("helper").display()))
                    .unwrap_or_default(),
                std::fs::read_to_string(format!("{}.err", root.path().join("helper").display()))
                    .unwrap_or_default()
            );
        }
        std::thread::sleep(POLL);
    }
    panic!(
        "the private-namespace helper did not reach {what} within {MARKER_BUDGET:?}; helper \
         stderr:\n{}",
        std::fs::read_to_string(format!("{}.err", root.path().join("helper").display()))
            .unwrap_or_default()
    );
}

/// The daemon PID the driver recorded, once it exists.
fn read_pid(root: &Root) -> Option<u32> {
    std::fs::read_to_string(root.marker("daemon.pid"))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

// ── helpers the arm re-executes inside the private namespace ────────────────

/// Helper: run one whole daemon lifecycle inside the namespace this process was
/// started in, and mark each phase for the observing parent.
///
/// It is `#[ignore]`d so a normal run of this target never executes it: the
/// parent starts it inside a private network namespace (`--user
/// --map-root-user --net`, or `sudo -n --net`). Nothing here asserts; every
/// assertion belongs to the parent, which is the external observer. What it does
/// provide is the two namespace identities the parent cannot resolve itself when
/// the channel made this process root.
#[test]
#[ignore = "helper: re-executed inside a private network namespace by the no-TCP cell"]
fn private_namespace_driver() {
    let Some(root) = std::env::var_os(DRIVER_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    prepare_daemon_roots(&root);
    let driver_namespace = tcp_absence::namespace_identity(std::process::id())
        .expect("the driver has a network namespace");
    std::fs::write(root.join("driver.netns"), &driver_namespace)
        .expect("record the driver's namespace");
    let socket = root.join("runtime").join("bitty").join("bittyd.sock");
    let capability = root.join("runtime").join("bitty").join("bittyd.operator");
    let stderr = root.join("daemon.stderr");
    let mut daemon = KillOnDrop(spawn_daemon(&root, &stderr));
    let pid = daemon.0.id();
    std::fs::write(root.join("daemon.pid"), pid.to_string()).expect("record the daemon PID");
    let daemon_namespace = tcp_absence::namespace_identity(pid)
        .unwrap_or_else(|| panic!("no `/proc/{pid}/ns/net` entry for the daemon"));
    std::fs::write(root.join("daemon.netns"), &daemon_namespace)
        .expect("record the daemon's namespace");
    wait_ready(&socket, &capability, &mut daemon.0, &stderr);
    std::fs::write(root.join("startup"), "1").expect("mark startup");
    std::thread::sleep(Duration::from_millis(300));

    // Attach inside the same namespace: the peer guard refuses a client from a
    // different user namespace, so this is the only attach that can exist here.
    let mut client = Client::connect(&socket);
    let reply = client.call(
        NEGOTIATE,
        &json!({"client_version": {"major": 1, "minor": 0}}),
    );
    std::fs::write(root.join("attached"), reply.to_string()).expect("mark the attach");
    std::thread::sleep(Duration::from_millis(600));

    drop(client);
    std::fs::write(root.join("detached"), "1").expect("mark the detach");
    std::thread::sleep(Duration::from_millis(300));

    kill_pid(pid, nix::sys::signal::Signal::SIGKILL);
    let status = daemon.0.wait().expect("reap the killed daemon");
    // The daemon is dead: hand the tree back so the parent can remove it even
    // when this helper (and therefore the daemon) ran as root.
    hand_back_tree(&root);
    std::fs::write(root.join("stopped"), format!("{status}")).expect("mark the abort");
}

/// Helper: bind one loopback listener inside the namespace this process was
/// started in, and hold it until the parent asks for it to stop.
///
/// The parent's control row: the same `/proc/<pid>/net/tcp` reader must report
/// this listener, or the daemon's zero rows prove nothing. The parent releases
/// it through a marker rather than a signal, because under the privileged channel
/// this process runs as root.
#[test]
#[ignore = "helper: re-executed inside a private network namespace by the no-TCP cell"]
fn private_namespace_listener_control() {
    let Some(root) = std::env::var_os(CONTROL_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind a loopback listener inside the private namespace");
    let port = listener
        .local_addr()
        .expect("read the control listener's address")
        .port();
    std::fs::write(root.join("control.port"), port.to_string()).expect("record the control port");
    let stop = root.join("control.stop");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if stop.exists() {
            break;
        }
        std::thread::sleep(POLL);
    }
    drop(listener);
}
