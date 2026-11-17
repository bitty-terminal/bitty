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
//! | private | a fresh unprivileged user + network namespace | `/proc/<pid>/net/tcp{,6}` of the daemon's own namespace | every TCP socket object created by any process in that namespace — the daemon and the client that attaches to it | a connect attempt that a route lookup rejects before a socket row exists (see below) |
//!
//! The private arm is the one that can speak for the daemon process itself: it
//! reads the kernel's TCP table for the namespace the daemon lives in, so no
//! PID attribution is needed and `PR_SET_DUMPABLE=0` is irrelevant. It needs
//! unprivileged user namespaces; on a host that refuses them the arm prints
//! `NOTCP private-arm: skipped` with the reason rather than reporting a pass it
//! did not observe (the repo's typed-skip convention, `tests/harness/real.rs`).
//! On this cell's Linux CI the arm runs.
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
//! Three independent checks, so an empty filter cannot pass:
//!
//! 1. [`tcp_absence::self_check`] binds a loopback listener in this test process
//!    and requires the same `ss` filter to attribute it (>= 1 `LISTEN` row);
//! 2. the private arm runs a control child in its *own* fresh namespace that
//!    binds one loopback listener, and requires `/proc/<pid>/net/tcp` to report
//!    it before the daemon's zero is trusted;
//! 3. every live phase asserts the daemon's PID set is non-empty and contains
//!    the PID this cell recorded at spawn — never a name or pattern match.
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

/// Absolute candidates for `unshare` (util-linux), then `PATH`.
const UNSHARE_CANDIDATES: [&str; 4] = [
    "/usr/bin/unshare",
    "/bin/unshare",
    "/usr/sbin/unshare",
    "/sbin/unshare",
];

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

/// Resolve the `unshare` binary, or `None` when the host has none.
fn unshare_binary() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(UNSHARE_ENV) {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "{UNSHARE_ENV}={} is not a file: the private-namespace arm cannot run",
            path.display()
        );
        return Some(path);
    }
    for candidate in UNSHARE_CANDIDATES {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("unshare");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
         see remains, so none of them holds a TCP socket"
    );

    // ── abort ────────────────────────────────────────────────────────────
    let mut aborted = Daemon::start("host-abort");
    let aborted_pid = aborted.pid();
    tcp_absence::scan_pids(&aborted.tree_pids()).assert_no_tcp("host-abort-startup");
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
/// `unshare --user --map-root-user --net` gives an unprivileged user + network
/// namespace; `/proc/<pid>/net/tcp{,6}` then reports only the sockets created
/// inside it, which is exactly the daemon's tree — no PID attribution needed,
/// so `PR_SET_DUMPABLE=0` cannot hide anything. The client has to live in the
/// same namespace as the daemon (`src/peer.rs` refuses a peer from another user
/// namespace: "a mapped uid is not same-user success"), so the driver is a
/// re-executed copy of this test binary running inside that namespace and
/// signalling each phase through marker files.
fn private_namespace_arm() {
    let Some(unshare) = unshare_binary() else {
        println!(
            "NOTCP private-arm: skipped — no `unshare` binary in {UNSHARE_CANDIDATES:?} or on PATH, \
             so the daemon's own socket table cannot be read from outside this process"
        );
        return;
    };

    // Unprivileged user namespaces are not available everywhere (some
    // distributions restrict them); a host that refuses them records the typed
    // skip instead of reporting a pass it did not observe.
    let probe = Command::new(&unshare)
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(BITTYD)
        .arg("--version")
        .output();
    match probe {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            println!(
                "NOTCP private-arm: skipped — `{} --user --map-root-user --net` exited {}: {}",
                unshare.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return;
        }
        Err(error) => {
            println!(
                "NOTCP private-arm: skipped — cannot run `{}`: {error}",
                unshare.display()
            );
            return;
        }
    }

    // ── control: the reader must see a socket in a private namespace ─────
    let control_root = Root::new("control");
    let mut control = spawn_helper(
        &unshare,
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
    let _ = control.kill();
    let _ = control.wait();

    // ── the daemon itself ────────────────────────────────────────────────
    let root = Root::new("private");
    root.prepare();
    let mut driver = spawn_helper(
        &unshare,
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
    // Isolation is asserted, not assumed: the daemon and this cell must be in
    // different network namespaces, or the table read would be the host's.
    let daemon_namespace = tcp_absence::namespace_identity(daemon_pid)
        .unwrap_or_else(|| panic!("no `/proc/{daemon_pid}/ns/net` entry for the daemon"));
    let host_namespace = tcp_absence::namespace_identity(std::process::id())
        .expect("this test process has a network namespace");
    assert_ne!(
        daemon_namespace, host_namespace,
        "the daemon must run in its own network namespace ({daemon_namespace} == {host_namespace})"
    );
    let pid_set = tcp_absence::session_pids(daemon_pid);
    assert!(
        pid_set.contains(&daemon_pid),
        "the daemon's PID set must include the PID the driver recorded ({daemon_pid}): {pid_set:?}"
    );
    println!(
        "NOTCP private-arm daemon pid={daemon_pid} netns={daemon_namespace} (host netns \
         {host_namespace})"
    );
    tcp_absence::scan_namespace(daemon_pid).assert_no_tcp("private-startup");

    wait_for_marker(&root, "attached", &mut driver, "the attached client");
    let attached = tcp_absence::scan_namespace(daemon_pid);
    attached.assert_no_tcp("private-attached");

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
    unshare: &Path,
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
    let mut command = Command::new(unshare);
    command
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(&executable)
        .args(args)
        .env(env_key, env_value)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.spawn().unwrap_or_else(|error| {
        panic!(
            "cannot spawn the helper `{} {:?}`: {error}",
            executable.display(),
            args
        )
    })
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
/// parent starts it with `--ignored --exact` inside
/// `unshare --user --map-root-user --net`. Nothing here asserts; every
/// assertion belongs to the parent, which is the external observer.
#[test]
#[ignore = "helper: re-executed inside a private network namespace by the no-TCP cell"]
fn private_namespace_driver() {
    let Some(root) = std::env::var_os(DRIVER_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    let socket = root.join("runtime").join("bitty").join("bittyd.sock");
    let capability = root.join("runtime").join("bitty").join("bittyd.operator");
    let stderr = root.join("daemon.stderr");
    let mut daemon = spawn_daemon(&root, &stderr);
    let pid = daemon.id();
    std::fs::write(root.join("daemon.pid"), pid.to_string()).expect("record the daemon PID");
    wait_ready(&socket, &capability, &mut daemon, &stderr);
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
    let status = daemon.wait().expect("reap the killed daemon");
    std::fs::write(root.join("stopped"), format!("{status}")).expect("mark the abort");
}

/// Helper: bind one loopback listener inside the namespace this process was
/// started in, and hold it until the observing parent kills it.
///
/// The parent's control row: the same `/proc/<pid>/net/tcp` reader must report
/// this listener, or the daemon's zero rows prove nothing.
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
    std::thread::sleep(Duration::from_secs(20));
    drop(listener);
}
