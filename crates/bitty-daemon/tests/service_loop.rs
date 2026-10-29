#![cfg(target_os = "linux")]

//! bitty#2607 / CTX-1531: process-level service-loop behavior.
//!
//! These tests start the real `bittyd start` binary against a throwaway
//! `XDG_RUNTIME_DIR` and `XDG_STATE_HOME`, speak the bitty-ipc v1 envelope over
//! the real Unix socket, and assert:
//!
//! - the endpoint is a `0600` socket inside a `0700` directory and a plain
//!   `bittyd` invocation binds nothing;
//! - a second start returns the typed already-running result and touches
//!   nothing;
//! - the daemon never opens a TCP listener (`daemon.status` self-reports
//!   `tcp_listeners: 0` from the daemon's own `/proc/self` fd table; the
//!   daemon is non-dumpable, so the test cannot inspect `/proc/<pid>/fd`);
//! - `daemon.negotiate` authenticates the operator capability and
//!   `daemon.stop force` shuts the loop down cleanly, removing the socket;
//! - closing refusals (`VersionMismatch`, `Unauthenticated`,
//!   `Denied/AlreadyNegotiated`) flush their refusal frame and then really
//!   close, checked as an EOF inside the negotiate deadline;
//! - the handshake advertises only the three baseline `session.*` scopes plus
//!   the explicit `terminal.manage` and `process.spawn` transport grants, never
//!   an ambient `view.*` content scope;
//! - a frame written after the handshake is read is served, while a frame
//!   queued before the response is refused and closed;
//! - a stopping daemon refuses a pipelined mutation on the same drain;
//! - the pre-authentication byte budget closes a dribbled frame and writes one
//!   aggregated audit record;
//! - `SIGTERM`/`SIGINT` drain to exit `0`, and endpoint tamper exits `6`.
//!
//! Linux is the only servable cell in this slice; the test is gated on it.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BITTYD: &str = env!("CARGO_BIN_EXE_bittyd");

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bittyd-loop-{label}-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp root");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        Self { path }
    }

    fn runtime(&self) -> PathBuf {
        self.path.join("runtime")
    }

    fn state(&self) -> PathBuf {
        self.path.join("state")
    }

    fn endpoint(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.sock")
    }

    fn capability(&self) -> PathBuf {
        self.runtime().join("bitty").join("bittyd.operator")
    }

    fn audit_log(&self) -> PathBuf {
        self.state().join("bitty").join("audit").join("audit.log")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(root: &TempRoot) -> Self {
        Self::start_with(root, &[])
    }

    fn start_with(root: &TempRoot, extra_env: &[(&str, &str)]) -> Self {
        std::fs::create_dir_all(root.runtime()).expect("runtime dir");
        std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod runtime");
        std::fs::create_dir_all(root.state()).expect("state dir");
        let mut command = Command::new(BITTYD);
        command
            .arg("start")
            .env("XDG_RUNTIME_DIR", root.runtime())
            .env("XDG_STATE_HOME", root.state())
            .env_remove("BITTYD_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn bittyd start");
        let daemon = Self { child };
        daemon.wait_for_socket(&root.endpoint());
        daemon
    }

    fn wait_for_socket(&self, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("bittyd did not bind {} in time", socket.display());
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait(mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait for bittyd")
    }

    /// Wait up to `budget` for the daemon to exit, then kill it.
    fn wait_within(mut self, budget: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("bittyd did not exit within {budget:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
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

/// Send a signal to the daemon this test started (a recorded pid, never a
/// name- or pattern-based kill).
fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill the recorded daemon pid");
    assert!(status.success(), "kill -{signal} {pid} failed");
}

struct Client {
    stream: UnixStream,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        // Linux attaches SCM_CREDENTIALS on send when the sending socket has
        // SO_PASSCRED; a client that skips this is refused as Unauthenticated.
        bitty_daemon::service::frame::enable_credentials(&stream).expect("per-frame credentials");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        Self { stream, next_id: 1 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id.to_string();
        self.next_id += 1;
        self.stream
            .write_all(&encode_request(&id, method, params))
            .expect("write frame");
        self.recv()
    }

    /// Queue several requests in one write, so a mutation cannot be processed
    /// before the stop that precedes it in the same drain.
    fn send_batch(&mut self, requests: &[(&str, Value)]) -> Vec<String> {
        let mut frames = Vec::new();
        let mut ids = Vec::new();
        for (method, params) in requests {
            let id = self.next_id.to_string();
            self.next_id += 1;
            frames.extend_from_slice(&encode_request(&id, method, params.clone()));
            ids.push(id);
        }
        self.stream.write_all(&frames).expect("write frames");
        ids
    }

    fn recv(&mut self) -> Value {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header).expect("read header");
        let len = u32::from_be_bytes(header) as usize;
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).expect("read payload");
        serde_json::from_slice(&payload).expect("response json")
    }

    fn negotiate(&mut self, operator_capability: Option<&str>) -> Value {
        self.negotiate_with(operator_capability, None)
    }

    fn negotiate_with(
        &mut self,
        operator_capability: Option<&str>,
        host_grant: Option<&str>,
    ) -> Value {
        self.negotiate_full(operator_capability, host_grant, None)
    }

    fn negotiate_full(
        &mut self,
        operator_capability: Option<&str>,
        host_grant: Option<&str>,
        client_correlation: Option<&str>,
    ) -> Value {
        let mut params = json!({"client_version": {"major": 1, "minor": 0}});
        if let Some(capability) = operator_capability {
            params["operator_capability"] = Value::String(capability.to_owned());
        }
        if let Some(grant) = host_grant {
            params["host_grant"] = Value::String(grant.to_owned());
        }
        if let Some(correlation) = client_correlation {
            params["client_correlation"] = Value::String(correlation.to_owned());
        }
        self.call("daemon.negotiate", params)
    }

    /// Write one raw frame (used for a wire version outside the contract).
    fn send_raw_frame(&mut self, request: Value) {
        let payload = serde_json::to_vec(&request).expect("encode request");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        self.stream.write_all(&frame).expect("write frame");
    }
}

/// Reads the per-boot operator capability once the daemon has written it.
///
/// The endpoint socket appears before the capability file, so a client that can
/// connect is not yet guaranteed a readable capability. Wait for it with a
/// bounded startup deadline instead of racing the writer; a missing file after
/// the deadline is a real failure.
fn read_capability(root: &TempRoot) -> String {
    const CAPABILITY_READY_TIMEOUT: Duration = Duration::from_secs(5);
    const CAPABILITY_READY_STEP: Duration = Duration::from_millis(10);
    let deadline = Instant::now() + CAPABILITY_READY_TIMEOUT;
    loop {
        match std::fs::read_to_string(root.capability()) {
            Ok(raw) => return raw.trim_end_matches('\n').to_owned(),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline =>
            {
                std::thread::sleep(CAPABILITY_READY_STEP);
            }
            Err(error) => panic!("capability file: {error:?}"),
        }
    }
}

/// One session created with host consent and claimed by a controller.
struct ControlledSession {
    creator: Client,
    session_id: String,
    create_token: String,
    controller_token: String,
    controller_attach: String,
}

/// Create a session with host consent and claim control with the creator.
fn controlled_session(root: &TempRoot, operator: &mut Client, label: &str) -> ControlledSession {
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();

    let mut creator = Client::connect(&root.endpoint());
    assert_eq!(creator.negotiate_with(None, Some(&host_grant))["ok"], true);
    let created = creator.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": format!("{label}-create")}),
    );
    assert_eq!(created["ok"], true, "{created}");
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let create_token = created["result"]["control_token"]
        .as_str()
        .expect("create token")
        .to_owned();

    let claimed = creator.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": create_token,
            "idempotency_key": format!("{label}-claim"),
        }),
    );
    assert_eq!(claimed["ok"], true, "{claimed}");
    let controller_attach = claimed["result"]["attach_id"]
        .as_str()
        .expect("controller attach")
        .to_owned();
    let controller_token = claimed["result"]["control_token"]
        .as_str()
        .expect("rotated token")
        .to_owned();

    ControlledSession {
        creator,
        session_id,
        create_token,
        controller_token,
        controller_attach,
    }
}

/// Detach the controller's own lease so the control relation is vacant.
fn vacate_control(creator: &mut Client, controller_attach: &str) {
    let detached = creator.call("session.detach", json!({"attach_id": controller_attach}));
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(detached["result"]["was_attached"], true);
}

/// Assert the closed vacant-control refusal and its no-token-echo shape.
fn assert_invalid_control_token(value: &Value) {
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["class"], "Denied", "{value}");
    assert_eq!(value["error"]["code"], "InvalidControlToken", "{value}");
}

/// Read one session's `state_revision` through the operator view.
///
/// A consumer that just detached its last lease is no longer a live visibility
/// relation, so the operator (which holds `daemon.inspect`) is the stable
/// reader for a counter assertion.
fn operator_revision(operator: &mut Client, session_id: &str) -> u64 {
    let status = operator.call("session.status", json!({"session_id": session_id}));
    assert_eq!(status["ok"], true, "{status}");
    status["result"]["session"]["state_revision"]
        .as_u64()
        .expect("state revision")
}

/// Mint an observer grant as the controller and attach a fresh observer.
fn attach_observer(
    root: &TempRoot,
    creator: &mut Client,
    session_id: &str,
    label: &str,
) -> (Client, String) {
    let minted = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": format!("{label}-grant")}),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let grant = minted["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    let attach_id = attached["result"]["attach_id"]
        .as_str()
        .expect("observer attach")
        .to_owned();
    (observer, attach_id)
}

/// Encode one v1 request as a length-prefixed frame.
fn encode_request(id: &str, method: &str, params: Value) -> Vec<u8> {
    let request = json!({"v": 1, "id": id, "method": method, "params": params});
    let payload = serde_json::to_vec(&request).expect("encode request");
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Assert a real EOF arrives within `budget`.
///
/// A read timeout is *not* EOF: `WouldBlock`/`TimedOut` panics, so a daemon
/// that keeps the connection open fails the test instead of passing on the
/// deadline.
fn assert_eof(stream: &mut UnixStream, budget: Duration) {
    stream.set_read_timeout(Some(budget)).expect("read timeout");
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Ok(read) => panic!("expected EOF within {budget:?}, read {read} bytes"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
            ) => {}
        Err(error) => panic!("expected EOF within {budget:?}, got {error:?}"),
    }
}

/// Count audit lines carrying `needle`.
fn audit_lines_with(root: &TempRoot, needle: &str) -> usize {
    let log = std::fs::read_to_string(root.audit_log()).unwrap_or_default();
    log.lines().filter(|line| line.contains(needle)).count()
}

/// Individual wire refusals, suppressed refusals, and aggregate records.
fn wire_audit_totals(root: &TempRoot) -> (usize, u64, usize) {
    let log = std::fs::read_to_string(root.audit_log()).unwrap_or_default();
    let mut individual = 0;
    let mut suppressed = 0;
    let mut aggregates = 0;
    for line in log.lines().filter(|line| line.contains("a=wire")) {
        if let Some(value) = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("sup="))
        {
            aggregates += 1;
            suppressed += value.parse::<u64>().expect("suppressed count");
        } else if line.contains("r=refused c=not_found") {
            individual += 1;
        }
    }
    (individual, suppressed, aggregates)
}

#[test]
fn start_serves_the_endpoint_and_never_opens_a_tcp_listener() {
    let root = TempRoot::new("serve");
    let daemon = Daemon::start(&root);
    let socket = root.endpoint();
    let metadata = std::fs::symlink_metadata(&socket).expect("socket");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::symlink_metadata(socket.parent().expect("dir"))
            .expect("dir")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let mut client = Client::connect(&socket);
    let response = client.negotiate(None);
    assert_eq!(response["ok"], true, "{response}");
    let result = &response["result"];
    assert_eq!(result["wire_version"], 1);
    assert_eq!(result["service_api_version"]["major"], 1);
    assert_eq!(result["daemon_state"], "running");
    assert!(result["client_id"].as_str().expect("client id").len() == 32);
    let scopes = result["scopes"].as_array().expect("scopes");
    let advertised: Vec<&str> = scopes
        .iter()
        .map(|scope| scope.as_str().expect("scope"))
        .collect();
    assert_eq!(
        advertised,
        [
            "session.inspect",
            "session.attach",
            "session.control",
            "terminal.manage",
            "process.spawn",
        ],
        "the baseline is the three session scopes plus terminal.manage and process.spawn"
    );
    assert!(
        !scopes
            .iter()
            .any(|scope| scope.as_str() == Some("session.host"))
    );
    assert!(!scopes.iter().any(|scope| scope == "daemon.control"));
    assert!(
        advertised.contains(&"terminal.manage"),
        "the content-plane scope is granted at negotiate"
    );
    assert_eq!(
        result["capabilities"]["worker_sessions"],
        bitty_daemon::worker::WORKER_SESSIONS_SERVED
    );
    assert_eq!(
        result["limits"].as_array().expect("limits").len(),
        bitty_daemon::limits::LIMIT_COUNT
    );

    // A consumer may not read daemon status.
    let denied = client.call("daemon.status", Value::Null);
    assert_eq!(denied["ok"], false);
    assert_eq!(denied["error"]["code"], "ScopeViolation");
    assert_eq!(denied["error"]["class"], "Denied");

    // The operator capability opens the operator surface.
    let capability = read_capability(&root);
    let mut operator = Client::connect(&socket);
    let response = operator.negotiate(Some(&capability));
    assert_eq!(response["ok"], true, "{response}");
    let scopes = response["result"]["scopes"].as_array().expect("scopes");
    assert!(scopes.iter().any(|scope| scope == "daemon.control"));
    let status = operator.call("daemon.status", Value::Null);
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result"]["audit_degraded"], false);
    assert_eq!(
        status["result"]["tcp_listeners"], 0,
        "the daemon reports no TCP listener on its own fd table"
    );

    let stopped = operator.call("daemon.stop", json!({"mode": "force"}));
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert_eq!(stopped["result"]["accepted"], true);
    let status = daemon.wait();
    assert_eq!(status.code(), Some(0));
    assert!(!socket.exists(), "the socket is removed on clean shutdown");
}

#[test]
fn a_second_start_is_a_typed_already_running_result() {
    let root = TempRoot::new("second");
    let daemon = Daemon::start(&root);
    let output = Command::new(BITTYD)
        .arg("start")
        .env("XDG_RUNTIME_DIR", root.runtime())
        .env("XDG_STATE_HOME", root.state())
        .env_remove("BITTYD_ENDPOINT")
        .output()
        .expect("second start");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already running"), "stderr: {stderr}");

    // The first instance still serves.
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true);

    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait().code(), Some(0));
}

/// Pin `RLIMIT_CORE=0` and the cleared dumpable flag on the real process.
#[test]
fn start_disables_core_dumps_and_is_not_dumpable() {
    let root = TempRoot::new("core-dump");
    let daemon = Daemon::start(&root);

    // D-16: RLIMIT_CORE is zero for both the soft and hard limit at startup.
    let limits =
        std::fs::read_to_string(format!("/proc/{}/limits", daemon.pid())).expect("process limits");
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max core file size"))
        .expect("Max core file size row");
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(fields[4], "0", "soft core limit: {line}");
    assert_eq!(fields[5], "0", "hard core limit: {line}");

    // PR_SET_DUMPABLE=0: a same-uid process (this test is the parent) cannot
    // open the daemon's memory. Without the control this open succeeds, so the
    // probe discriminates.
    match std::fs::File::open(format!("/proc/{}/mem", daemon.pid())) {
        Ok(_) => panic!("a same-uid process must not open a non-dumpable daemon's memory"),
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "expected EACCES from the non-dumpable daemon: {error}"
        ),
    }

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_failed_operator_capability_is_one_static_refusal_and_closes() {
    let root = TempRoot::new("auth");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());
    let wrong = "0".repeat(64);
    let response = client.negotiate(Some(&wrong));
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["error"]["code"], "Unauthenticated");
    assert_eq!(response["error"]["class"], "Unauthenticated");
    assert_eq!(response["error"]["message"], "authentication failed");
    assert!(
        !response["error"]["message"]
            .as_str()
            .expect("message")
            .contains("bittyd.operator"),
        "the refusal must not reveal the capability file"
    );
    // The connection closed *after* flushing the refusal: a real EOF arrives
    // well inside the 5 s negotiate deadline (a timeout is a failure).
    assert_eof(&mut client.stream, Duration::from_secs(2));

    // The daemon is still healthy for a different connection.
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait().code(), Some(0));
}

/// Stop the daemon through the operator capability and wait for exit.
fn stop_cleanly(root: &TempRoot, daemon: Daemon) -> std::process::ExitStatus {
    let capability = read_capability(root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    daemon.wait_within(Duration::from_secs(10))
}

#[test]
fn closing_refusals_flush_the_frame_and_then_close() {
    let root = TempRoot::new("closing");
    let daemon = Daemon::start(&root);

    // A second negotiate: the refusal frame is delivered, then EOF.
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true);
    let second = client.call(
        "daemon.negotiate",
        json!({"client_version": {"major": 1, "minor": 0}}),
    );
    assert_eq!(second["ok"], false);
    assert_eq!(second["error"]["code"], "AlreadyNegotiated");
    assert_eq!(second["error"]["class"], "Denied");
    assert_eof(&mut client.stream, Duration::from_secs(2));

    // A frame before the negotiate response: refused and closed.
    let mut before = Client::connect(&root.endpoint());
    let refused = before.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "before"}),
    );
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"]["code"], "AlreadyNegotiated");
    assert_eof(&mut before.stream, Duration::from_secs(2));

    // A service version mismatch: refused and closed.
    let mut version = Client::connect(&root.endpoint());
    let mismatch = version.call(
        "daemon.negotiate",
        json!({"client_version": {"major": 2, "minor": 0}}),
    );
    assert_eq!(mismatch["ok"], false);
    assert_eq!(mismatch["error"]["code"], "VersionMismatch");
    assert_eq!(mismatch["error"]["class"], "Validation");
    assert_eof(&mut version.stream, Duration::from_secs(2));

    // A wire version mismatch: refused and closed.
    let mut wire = Client::connect(&root.endpoint());
    wire.send_raw_frame(json!({
        "v": 99,
        "id": "wire-1",
        "method": "daemon.negotiate",
        "params": {"client_version": {"major": 1, "minor": 0}},
    }));
    let refused = wire.recv();
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"]["code"], "VersionMismatch");
    assert_eof(&mut wire.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

fn correlation_digest(correlation: &str) -> String {
    bitty_daemon::handshake::hex(
        &bitty_daemon::handshake::ClientCorrelation::parse(correlation)
            .expect("correlation")
            .digest(),
    )
}

#[test]
fn a_pipelined_request_behind_negotiate_is_refused() {
    let root = TempRoot::new("pipelined-handshake");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);

    let mut client = Client::connect(&root.endpoint());
    let ids = client.send_batch(&[
        (
            "daemon.negotiate",
            json!({
                "client_version": {"major": 1, "minor": 0},
                "operator_capability": capability,
            }),
        ),
        ("daemon.status", json!({})),
    ]);
    let handshake = client.recv();
    assert_eq!(handshake["id"], ids[0]);
    assert_eq!(handshake["ok"], true, "{handshake}");

    // A frame sent before the handshake response is never served and never
    // reordered behind the handshake: the tail is refused with a typed frame
    // whose id is omitted because the queued bytes are not parsed.
    let refused = client.recv();
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Denied");
    assert_eq!(refused["error"]["code"], "AlreadyNegotiated");
    assert!(
        refused.get("id").is_none(),
        "a pipelined tail is never parsed for an id: {refused}"
    );
    assert_eof(&mut client.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_partial_pipelined_tail_cannot_resume_as_a_request() {
    let root = TempRoot::new("pipelined-partial");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());

    let negotiate = encode_request(
        "1",
        "daemon.negotiate",
        json!({"client_version": {"major": 1, "minor": 0}}),
    );
    let status = encode_request("2", "daemon.status", json!({}));
    // The wire evidence: negotiate, then the next frame's header and 10 body
    // bytes, with the remaining 90 bytes sent later.
    let split = 4 + 10;
    let mut batch = negotiate;
    batch.extend_from_slice(&status[..split]);
    client.stream.write_all(&batch).expect("batch");
    let _ = client.stream.write_all(&status[split..]);

    let handshake = client.recv();
    assert_eq!(handshake["id"], "1");
    assert_eq!(handshake["ok"], true, "{handshake}");
    let refused = client.recv();
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Denied");
    assert_eq!(refused["error"]["code"], "AlreadyNegotiated");
    assert!(refused.get("id").is_none(), "{refused}");
    assert_eof(&mut client.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_frame_written_after_the_handshake_is_read_is_served() {
    let root = TempRoot::new("post-handshake-frame");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);

    let mut client = Client::connect(&root.endpoint());
    let handshake = client.negotiate(Some(&capability));
    assert_eq!(handshake["ok"], true, "{handshake}");

    // The conformance case: the client has read the full handshake and writes
    // its next request immediately. On localhost the byte is often already
    // queued when the response finishes draining; it is post-handshake by
    // construction and must be served, not refused as a raced tail.
    let status = client.call("daemon.status", json!({}));
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result"]["daemon_state"], "running", "{status}");

    // The connection stays usable after that first post-handshake frame.
    let second = client.call("daemon.status", json!({}));
    assert_eq!(second["ok"], true, "{second}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_failed_handshake_is_final() {
    let root = TempRoot::new("failed-handshake");
    let daemon = Daemon::start(&root);

    // A malformed negotiate is `InvalidRequest`, then the connection closes;
    // a following valid negotiate is never served.
    let mut malformed = Client::connect(&root.endpoint());
    let refused = malformed.call("daemon.negotiate", json!({}));
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Validation");
    assert_eq!(refused["error"]["code"], "InvalidRequest");
    let _ = malformed.stream.write_all(&encode_request(
        "2",
        "daemon.negotiate",
        json!({"client_version": {"major": 1, "minor": 0}}),
    ));
    assert_eof(&mut malformed.stream, Duration::from_secs(2));

    // A refused operator capability also closes; there is no second try.
    let mut capability = Client::connect(&root.endpoint());
    let refused = capability.call(
        "daemon.negotiate",
        json!({
            "client_version": {"major": 1, "minor": 0},
            "operator_capability": "ff".repeat(32),
        }),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Unauthenticated");
    assert_eq!(refused["error"]["code"], "Unauthenticated");
    let _ = capability.stream.write_all(&encode_request(
        "2",
        "daemon.negotiate",
        json!({"client_version": {"major": 1, "minor": 0}}),
    ));
    assert_eof(&mut capability.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn an_over_long_correlation_id_gets_a_typed_refusal_and_closes() {
    let root = TempRoot::new("overlong-id");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());

    client.send_raw_frame(json!({
        "v": 1,
        "id": "x".repeat(65),
        "method": "daemon.negotiate",
        "params": {"client_version": {"major": 1, "minor": 0}},
    }));
    let refused = client.recv();
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Validation");
    assert_eq!(refused["error"]["code"], "InvalidRequest");
    assert!(
        refused.get("id").is_none(),
        "an over-long id is never echoed: {refused}"
    );
    assert_eof(&mut client.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_frame_without_a_wire_version_is_final() {
    let root = TempRoot::new("missing-v");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());

    client.send_raw_frame(json!({
        "id": "no-version",
        "method": "session.create",
        "params": {"kind": "terminal", "idempotency_key": "missing-v"},
    }));
    let refused = client.recv();
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Validation");
    assert_eq!(refused["error"]["code"], "InvalidRequest");

    // The transport refusal is final: a later negotiate on the same
    // connection is never served.
    let _ = client.stream.write_all(&encode_request(
        "2",
        "daemon.negotiate",
        json!({"client_version": {"major": 1, "minor": 0}}),
    ));
    assert_eof(&mut client.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn grant_ttls_are_positive_and_bounded_by_the_compiled_limits() {
    let root = TempRoot::new("grant-ttl");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let mut session = controlled_session(&root, &mut operator, "ttl");

    for (ttl, limit) in [(0u64, 60_000u64), (60_001, 60_000)] {
        let refused = session.creator.call(
            "session.grant",
            json!({"session_id": session.session_id.clone(), "ttl_ms": ttl, "idempotency_key": format!("ttl-{ttl}")}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["class"], "Validation");
        assert_eq!(refused["error"]["code"], "InvalidRequest");
        assert_eq!(refused["error"]["counts"]["actual"], ttl);
        assert_eq!(refused["error"]["counts"]["limit"], limit);
    }
    let accepted = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "ttl_ms": 1_000,
            "idempotency_key": "ttl-accepted",
        }),
    );
    assert_eq!(accepted["ok"], true, "{accepted}");
    assert!(accepted["result"]["expires_at_ms"].as_u64().is_some());

    for (ttl, limit) in [(0u64, 300_000u64), (300_001, 300_000)] {
        let refused = operator.call(
            "daemon.host_grant",
            json!({"action": "mint", "ttl_ms": ttl}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["code"], "InvalidRequest");
        assert_eq!(refused["error"]["counts"]["actual"], ttl);
        assert_eq!(refused["error"]["counts"]["limit"], limit);
    }
    let accepted = operator.call(
        "daemon.host_grant",
        json!({"action": "mint", "ttl_ms": 1_000}),
    );
    assert_eq!(accepted["ok"], true, "{accepted}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn grant_correlation_digests_are_exact_and_method_scoped() {
    let root = TempRoot::new("grant-digest-shape");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let mut session = controlled_session(&root, &mut operator, "digest-shape");

    // Exactly 32 lowercase hex is accepted; every other width or case is
    // `InvalidRequest`, because it could never equal a 128-bit connection
    // digest and would mint a grant that is dead on arrival.
    for width in [0usize, 31, 33, 64] {
        let refused = session.creator.call(
            "session.grant",
            json!({
                "session_id": session.session_id.clone(),
                "observer_correlation_digest": "a".repeat(width),
                "idempotency_key": format!("digest-{width}"),
            }),
        );
        assert_eq!(refused["ok"], false, "{width}: {refused}");
        assert_eq!(refused["error"]["code"], "InvalidRequest", "{width}");
    }
    let uppercase = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "observer_correlation_digest": "AB".repeat(16),
            "idempotency_key": "digest-uppercase",
        }),
    );
    assert_eq!(uppercase["error"]["code"], "InvalidRequest", "{uppercase}");
    // The host-grant mint takes the client digest, not the observer one.
    let cross_host = operator.call(
        "daemon.host_grant",
        json!({"action": "mint", "observer_correlation_digest": "ab".repeat(16)}),
    );
    assert_eq!(cross_host["ok"], false, "{cross_host}");
    assert_eq!(cross_host["error"]["code"], "InvalidRequest");
    // The observer mint takes the observer digest, not the client one.
    let cross_observer = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "client_correlation_digest": "ab".repeat(16),
            "idempotency_key": "cross-observer",
        }),
    );
    assert_eq!(cross_observer["ok"], false, "{cross_observer}");
    assert_eq!(cross_observer["error"]["code"], "InvalidRequest");

    // A digest of the right width binds and is accepted.
    let accepted = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "observer_correlation_digest": correlation_digest("observer-a"),
            "idempotency_key": "digest-exact",
        }),
    );
    assert_eq!(accepted["ok"], true, "{accepted}");

    // A revoke is keyless by `grant_id`; mint-only fields are refused there.
    let minted = session.creator.call(
        "session.grant",
        json!({"session_id": session.session_id.clone(), "idempotency_key": "digest-revoke"}),
    );
    let grant_id = minted["result"]["grant_id"].as_str().expect("grant id");
    for params in [
        json!({"grant_id": grant_id, "ttl_ms": 1_000}),
        json!({"grant_id": grant_id, "observer_correlation_digest": "ab".repeat(16)}),
        json!({"grant_id": grant_id, "client_correlation_digest": "ab".repeat(16)}),
    ] {
        let refused = session.creator.call("session.grant.revoke", params);
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["code"], "InvalidRequest", "{refused}");
    }

    let over_wide = operator.call(
        "daemon.host_grant",
        json!({"action": "mint", "client_correlation_digest": "ab".repeat(32)}),
    );
    assert_eq!(over_wide["ok"], false, "{over_wide}");
    assert_eq!(over_wide["error"]["code"], "InvalidRequest");
    let host = operator.call("daemon.host_grant", json!({"action": "mint"}));
    let host_id = host["result"]["grant_id"].as_str().expect("host grant id");
    for params in [
        json!({"action": "revoke", "grant_id": host_id, "ttl_ms": 1_000}),
        json!({"action": "revoke", "grant_id": host_id, "client_correlation_digest": "ab".repeat(16)}),
        json!({"action": "revoke", "grant_id": host_id, "observer_correlation_digest": "ab".repeat(16)}),
    ] {
        let refused = operator.call("daemon.host_grant", params);
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["code"], "InvalidRequest", "{refused}");
    }
    let revoked = operator.call(
        "daemon.host_grant",
        json!({"action": "revoke", "grant_id": host_id}),
    );
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn grant_correlation_bindings_gate_consume_attach_and_revoke() {
    let root = TempRoot::new("grant-correlation");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let mut session = controlled_session(&root, &mut operator, "correlation");
    let observer_a = correlation_digest("observer-a");

    // A host grant bound to observer-a cannot be consumed by observer-b.
    let host = operator.call(
        "daemon.host_grant",
        json!({"action": "mint", "client_correlation_digest": observer_a.clone()}),
    );
    assert_eq!(host["ok"], true, "{host}");
    let host_bearer = host["result"]["grant"].as_str().expect("grant");
    let mut wrong_host = Client::connect(&root.endpoint());
    let handshake = wrong_host.negotiate_full(None, Some(host_bearer), Some("observer-b"));
    assert_eq!(handshake["ok"], true, "{handshake}");
    assert!(
        !handshake["result"]["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .any(|scope| scope.as_str() == Some("session.host")),
        "a correlation mismatch must not consume host consent: {handshake}"
    );
    let mut right_host = Client::connect(&root.endpoint());
    let handshake = right_host.negotiate_full(None, Some(host_bearer), Some("observer-a"));
    assert_eq!(handshake["ok"], true, "{handshake}");
    assert!(
        handshake["result"]["scopes"]
            .as_array()
            .expect("scopes")
            .iter()
            .any(|scope| scope.as_str() == Some("session.host")),
        "the bound connection consumes host consent: {handshake}"
    );

    // An observer grant bound to observer-a is refused to observer-b and
    // consumed by observer-a.
    let minted = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "observer_correlation_digest": observer_a.clone(),
            "idempotency_key": "bound-observer",
        }),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let bearer = minted["result"]["grant"].as_str().expect("grant");
    let mut wrong = Client::connect(&root.endpoint());
    assert_eq!(
        wrong.negotiate_full(None, None, Some("observer-b"))["ok"],
        true
    );
    let refused = wrong.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session.session_id.clone(),
            "observer_grant": bearer,
        }),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Denied");
    assert_eq!(refused["error"]["code"], "ObserverGrantRequired");

    let mut right = Client::connect(&root.endpoint());
    assert_eq!(
        right.negotiate_full(None, None, Some("observer-a"))["ok"],
        true
    );
    let attached = right.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session.session_id.clone(),
            "observer_grant": bearer,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");

    // A non-operator revoke from another correlation is indistinguishable from
    // an invisible grant; the bound connection can revoke.
    let minted = session.creator.call(
        "session.grant",
        json!({
            "session_id": session.session_id.clone(),
            "observer_correlation_digest": observer_a.clone(),
            "idempotency_key": "bound-revoke",
        }),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let grant_id = minted["result"]["grant_id"]
        .as_str()
        .expect("grant id")
        .to_owned();
    let detached = session.creator.call(
        "session.detach",
        json!({"attach_id": session.controller_attach.clone()}),
    );
    assert_eq!(detached["ok"], true, "{detached}");

    let mut stranger = Client::connect(&root.endpoint());
    assert_eq!(
        stranger.negotiate_full(None, None, Some("observer-b"))["ok"],
        true
    );
    let denied = stranger.call(
        "session.grant.revoke",
        json!({"grant_id": grant_id, "control_token": session.controller_token.clone()}),
    );
    // A correlation mismatch is byte-identical to the unknown-grant answer:
    // the mismatch must not be an existence oracle, and nothing is revoked.
    assert_eq!(denied["ok"], true, "{denied}");
    assert_eq!(denied["result"]["grant_id"], grant_id, "{denied}");
    assert_eq!(denied["result"]["revoked"], false, "{denied}");
    assert_eq!(denied["result"]["not_active"], true, "{denied}");

    // The observer lease itself cannot hold the control relation; a fresh
    // connection with the matching correlation and the vacant-control token
    // can revoke. It succeeds, proving the stranger's call revoked nothing.
    let mut right_revoke = Client::connect(&root.endpoint());
    assert_eq!(
        right_revoke.negotiate_full(None, None, Some("observer-a"))["ok"],
        true
    );
    let revoked = right_revoke.call(
        "session.grant.revoke",
        json!({"grant_id": grant_id, "control_token": session.controller_token.clone()}),
    );
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true);

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_stopping_daemon_refuses_a_pipelined_create() {
    let root = TempRoot::new("stopping");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();

    // One connection carries both the operator capability (to stop) and the
    // consumed host grant (to create), so the stop and the create are drained
    // in one read on one connection.
    let mut client = Client::connect(&root.endpoint());
    let response = client.negotiate_with(Some(&capability), Some(&grant));
    assert_eq!(response["ok"], true, "{response}");
    let scopes: Vec<&str> = response["result"]["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .map(|scope| scope.as_str().expect("scope"))
        .collect();
    assert!(scopes.contains(&"session.host"), "{scopes:?}");
    assert!(scopes.contains(&"daemon.control"), "{scopes:?}");

    let ids = client.send_batch(&[
        ("daemon.stop", json!({"mode": "force"})),
        (
            "session.create",
            json!({"kind": "terminal", "idempotency_key": "pipelined"}),
        ),
    ]);
    let stopped = client.recv();
    let created = client.recv();
    assert_eq!(stopped["id"], ids[0]);
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert_eq!(stopped["result"]["accepted"], true);
    assert_eq!(stopped["result"]["terminated"], 0);
    assert_eq!(created["id"], ids[1]);
    assert_eq!(created["ok"], false, "a stopping daemon creates nothing");
    assert_eq!(created["error"]["code"], "DaemonStopping");
    assert_eq!(created["error"]["class"], "Denied");

    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
    assert_eq!(
        audit_lines_with(&root, "a=session.create r=refused c=daemon_stopping"),
        1,
        "the stopped create is attributed once"
    );
}

#[test]
fn a_pre_authentication_budget_closes_a_dribbled_frame() {
    let root = TempRoot::new("budget");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());

    // Declare a `max_frame_bytes` body and send 20 KiB without negotiating.
    // The pre-auth budget is 8 KiB, so the daemon refuses mid-assembly.
    let mut frame = Vec::new();
    frame.extend_from_slice(&200_000u32.to_be_bytes());
    frame.extend(std::iter::repeat_n(b'x', 20_000));
    client.stream.write_all(&frame).expect("dribbled frame");
    assert_eof(&mut client.stream, Duration::from_secs(2));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    assert_eq!(
        audit_lines_with(&root, "a=wire r=refused c=limit_exceeded"),
        1,
        "one aggregated record for the budget closure"
    );
}

#[test]
fn an_unnegotiated_connection_past_the_deadline_is_closed_with_one_record() {
    let root = TempRoot::new("deadline");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());

    // `negotiate_timeout_ms` is 5 000 ms at the compiled defaults.
    std::thread::sleep(Duration::from_millis(5_500));
    assert_eof(&mut client.stream, Duration::from_secs(3));

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    assert_eq!(
        audit_lines_with(&root, "a=wire r=refused c=timeout"),
        1,
        "one aggregated record for the deadline closure"
    );
}

#[test]
fn sigterm_and_sigint_drain_to_exit_zero() {
    for signal in ["TERM", "INT"] {
        let root = TempRoot::new(&format!("signal-{}", signal.to_lowercase()));
        let daemon = Daemon::start(&root);
        let mut client = Client::connect(&root.endpoint());
        assert_eq!(client.negotiate(None)["ok"], true);
        send_signal(daemon.pid(), signal);
        let status = daemon.wait_within(Duration::from_secs(10));
        assert_eq!(status.code(), Some(0), "SIG{signal} drains cleanly");
        assert!(
            !root.endpoint().exists(),
            "the socket is removed after SIG{signal}"
        );
    }
}

#[test]
fn endpoint_tamper_exits_six() {
    let root = TempRoot::new("tamper");
    let daemon = Daemon::start(&root);
    let socket = root.endpoint();

    // Replace the bound socket object with a regular file. The bounded tamper
    // interval re-attests the path and must take the exit-6 path.
    std::fs::remove_file(&socket).expect("remove the bound socket");
    std::fs::write(&socket, b"not a socket").expect("plant a foreign object");

    let status = daemon.wait_within(Duration::from_secs(15));
    assert_eq!(
        status.code(),
        Some(6),
        "endpoint tamper is the contract's exit code 6"
    );
}

/// A tamper exit keeps the suppressed tail as one aggregate record.
#[test]
fn an_endpoint_tamper_flushes_the_pending_refusal_aggregate() {
    let root = TempRoot::new("tamper-audit");
    // The test seam shortens the re-attestation interval so the tamper check
    // lands inside the one-second refusal window; a test-start sleep would
    // not control the daemon clock and could pass without the flush.
    let daemon = Daemon::start_with(&root, &[("BITTYD_TEST_TAMPER_INTERVAL_MS", "200")]);
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true);

    // 100 wire refusals against the 20-record budget: 80 are suppressed and
    // must survive the tamper exit as one aggregate.
    let requests: Vec<(&str, Value)> = (0..100).map(|_| ("session.unknown", Value::Null)).collect();
    client.send_batch(&requests);
    for _ in 0..100 {
        let response = client.recv();
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "NotFound");
    }

    let socket = root.endpoint();
    std::fs::remove_file(&socket).expect("remove the bound socket");
    std::fs::write(&socket, b"not a socket").expect("plant a foreign object");
    let status = daemon.wait_within(Duration::from_secs(15));
    assert_eq!(status.code(), Some(6));

    let (individual, suppressed, aggregates) = wire_audit_totals(&root);
    assert_eq!(individual, 20, "the budget bounds the individual records");
    assert_eq!(
        suppressed, 80,
        "the tamper exit flushes the suppressed tail"
    );
    assert_eq!(aggregates, 1, "exactly one aggregate survives the exit");
    assert_eq!(
        audit_lines_with(&root, "h=not_found:80"),
        1,
        "the aggregate carries the bounded histogram"
    );
}

#[test]
fn the_keyed_attach_and_control_flow_is_reachable_over_the_wire() {
    let root = TempRoot::new("attach-flow");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();

    let mut creator = Client::connect(&root.endpoint());
    let handshake = creator.negotiate_with(None, Some(&host_grant));
    assert_eq!(handshake["ok"], true, "{handshake}");

    // session.create
    let created = creator.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "flow-create"}),
    );
    assert_eq!(created["ok"], true, "{created}");
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let create_token = created["result"]["control_token"]
        .as_str()
        .expect("create token")
        .to_owned();

    // (a) controller claim through the real envelope.
    let claimed = creator.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": create_token,
            "idempotency_key": "flow-claim",
        }),
    );
    assert_eq!(claimed["ok"], true, "{claimed}");
    assert_eq!(claimed["result"]["role"], "controller");
    let controller_attach = claimed["result"]["attach_id"]
        .as_str()
        .expect("controller attach")
        .to_owned();
    let controller_token = claimed["result"]["control_token"]
        .as_str()
        .expect("rotated token")
        .to_owned();

    // Consumer mint of an observer grant as the controller.
    let grant = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "flow-grant"}),
    );
    assert_eq!(grant["ok"], true, "{grant}");
    let observer_grant = grant["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    let observer_grant_id = grant["result"]["grant_id"].as_str().expect("grant id");

    // (b) observer attach through the real envelope.
    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": observer_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    assert_eq!(attached["result"]["role"], "observer");
    let observer_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("observer attach")
        .to_owned();

    // session.control.transfer reaches its handler.
    let transferred = creator.call(
        "session.control.transfer",
        json!({
            "session_id": session_id,
            "attach_id": observer_attach,
            "expected_generation": 0,
            "idempotency_key": "flow-transfer",
        }),
    );
    assert_eq!(transferred["ok"], true, "{transferred}");
    assert_eq!(transferred["result"]["controller"], observer_attach);
    assert!(
        transferred["result"].get("control_token").is_none(),
        "the requester never receives the target's rotated token"
    );

    // lease.heartbeat reaches its handler; the rotated token is delivered on
    // the target connection's next response after routing.
    let first = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
    assert_eq!(first["ok"], true, "{first}");
    let second = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
    assert_eq!(second["ok"], true, "{second}");
    let delivered = second["result"]["control_token"]
        .as_str()
        .expect("control-token delivery");
    assert_ne!(delivered, controller_token);

    // Own-lease session.detach reaches its handler.
    let detached = observer.call("session.detach", json!({"attach_id": observer_attach}));
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(detached["result"]["was_attached"], true);
    let detached = creator.call("session.detach", json!({"attach_id": controller_attach}));
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(observer_grant_id.len(), 32);

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// A repeat detach of the caller's own handle is the idempotent
/// `not_attached` acknowledgement (bitty#2768, corpus `V-07`): no revision
/// bump, no lease resurrection, and no `session.control` requirement.
#[test]
fn a_repeat_own_detach_is_the_idempotent_not_attached_acknowledgement() {
    let root = TempRoot::new("repeat-detach");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        controller_attach,
        ..
    } = controlled_session(&root, &mut operator, "repeat");
    assert_eq!(operator_revision(&mut operator, &session_id), 1);

    let detached = creator.call("session.detach", json!({"attach_id": controller_attach}));
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(detached["result"]["was_attached"], true, "{detached}");
    assert_eq!(detached["result"]["not_attached"], false, "{detached}");
    assert_eq!(
        detached["result"]["session"]["state_revision"], 2,
        "{detached}"
    );
    assert_eq!(operator_revision(&mut operator, &session_id), 2);

    // The repeat is idempotent for as long as the connection remembers the
    // handle: the same acknowledgement, the same revision, and no lease.
    for _ in 0..2 {
        let repeat = creator.call("session.detach", json!({"attach_id": controller_attach}));
        assert_eq!(repeat["ok"], true, "{repeat}");
        assert_eq!(repeat["result"]["was_attached"], false, "{repeat}");
        assert_eq!(repeat["result"]["not_attached"], true, "{repeat}");
        assert_eq!(repeat["result"]["revoked"], false, "{repeat}");
        assert_eq!(repeat["result"]["attach_id"], controller_attach.as_str());
        assert_eq!(repeat["result"]["session"]["attach_count"], 0, "{repeat}");
        assert_eq!(repeat["result"]["session"]["state_revision"], 2, "{repeat}");
        assert_eq!(operator_revision(&mut operator, &session_id), 2);
    }

    // No resurrection: the handle names no live lease, so a heartbeat is the
    // uniform `NotFound` and control stays vacant.
    let heartbeat = creator.call("lease.heartbeat", json!({"attach_id": controller_attach}));
    assert_eq!(heartbeat["ok"], false, "{heartbeat}");
    assert_eq!(heartbeat["error"]["code"], "NotFound", "{heartbeat}");

    // The history is per connection: another client cannot turn a handle it
    // never held into an acknowledgement.
    let mut stranger = Client::connect(&root.endpoint());
    assert_eq!(stranger.negotiate(None)["ok"], true);
    let foreign = stranger.call("session.detach", json!({"attach_id": controller_attach}));
    assert_eq!(foreign["ok"], false, "{foreign}");
    assert_eq!(foreign["error"]["class"], "NotFound", "{foreign}");
    assert_eq!(foreign["error"]["code"], "NotFound", "{foreign}");

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// A controller that detaches another connection's lease records itself for
/// the repeat: the second call is the contract's `not_attached`
/// acknowledgement, not `NotFound` (bitty#2768 review).
#[test]
fn a_controller_repeat_detach_of_a_foreign_handle_is_not_attached() {
    let root = TempRoot::new("controller-repeat-detach");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        ..
    } = controlled_session(&root, &mut operator, "controller-repeat");
    let (mut observer, observer_attach) =
        attach_observer(&root, &mut creator, &session_id, "controller-repeat");
    let revision = operator_revision(&mut operator, &session_id);

    let revoked = creator.call("session.detach", json!({"attach_id": observer_attach}));
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["was_attached"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");
    assert_eq!(revoked["result"]["attach_id"], observer_attach.as_str());
    assert_eq!(operator_revision(&mut operator, &session_id), revision + 1);

    for _ in 0..2 {
        let repeat = creator.call("session.detach", json!({"attach_id": observer_attach}));
        assert_eq!(repeat["ok"], true, "{repeat}");
        assert_eq!(repeat["result"]["was_attached"], false, "{repeat}");
        assert_eq!(repeat["result"]["not_attached"], true, "{repeat}");
        assert_eq!(repeat["result"]["revoked"], false, "{repeat}");
        assert_eq!(repeat["result"]["attach_id"], observer_attach.as_str());
        assert_eq!(repeat["result"]["session"]["attach_count"], 1, "{repeat}");
        assert_eq!(
            repeat["result"]["session"]["state_revision"],
            revision + 1,
            "{repeat}"
        );
        assert_eq!(operator_revision(&mut operator, &session_id), revision + 1);
    }

    // The revocation removed the target lease; the handle is not revived.
    let heartbeat = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
    assert_eq!(heartbeat["ok"], false, "{heartbeat}");
    assert_eq!(heartbeat["error"]["code"], "NotFound", "{heartbeat}");

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// An operator that revokes another connection's lease records itself for the
/// repeat exactly as a controller does.
#[test]
fn an_operator_repeat_detach_of_a_foreign_handle_is_not_attached() {
    let root = TempRoot::new("operator-repeat-detach");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        ..
    } = controlled_session(&root, &mut operator, "operator-repeat");
    let (mut observer, observer_attach) =
        attach_observer(&root, &mut creator, &session_id, "operator-repeat");
    let revision = operator_revision(&mut operator, &session_id);

    let revoked = operator.call("session.detach", json!({"attach_id": observer_attach}));
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["was_attached"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");
    assert_eq!(operator_revision(&mut operator, &session_id), revision + 1);

    for _ in 0..2 {
        let repeat = operator.call("session.detach", json!({"attach_id": observer_attach}));
        assert_eq!(repeat["ok"], true, "{repeat}");
        assert_eq!(repeat["result"]["was_attached"], false, "{repeat}");
        assert_eq!(repeat["result"]["not_attached"], true, "{repeat}");
        assert_eq!(repeat["result"]["revoked"], false, "{repeat}");
        assert_eq!(
            repeat["result"]["session"]["state_revision"],
            revision + 1,
            "{repeat}"
        );
        assert_eq!(operator_revision(&mut operator, &session_id), revision + 1);
    }

    let heartbeat = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
    assert_eq!(heartbeat["ok"], false, "{heartbeat}");
    assert_eq!(heartbeat["error"]["code"], "NotFound", "{heartbeat}");

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// Grant mint, observer-grant consume, and revoke each bump `state_revision`
/// exactly once (bitty#2767, corpus `C-04`); heartbeats and keyed replays never
/// do.
#[test]
fn grant_mint_consume_and_revoke_each_bump_the_state_revision() {
    let root = TempRoot::new("grant-revision");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        ..
    } = controlled_session(&root, &mut operator, "grant-revision");
    // create -> 0, controller attach -> 1.
    assert_eq!(operator_revision(&mut operator, &session_id), 1);

    let minted = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "gr-mint-1"}),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let observer_grant = minted["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    // The mint is its own snapshot-visible mutation.
    assert_eq!(operator_revision(&mut operator, &session_id), 2);

    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": observer_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    // The consume and the attach each bump once, and the reply carries the
    // post-mutation snapshot.
    assert_eq!(
        attached["result"]["session"]["state_revision"], 4,
        "{attached}"
    );
    assert_eq!(operator_revision(&mut operator, &session_id), 4);
    let observer_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("observer attach")
        .to_owned();

    // A heartbeat is never a mutation.
    let heartbeat = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
    assert_eq!(heartbeat["ok"], true, "{heartbeat}");
    assert_eq!(operator_revision(&mut operator, &session_id), 4);

    // A keyed replay of the consumed mint changes nothing either.
    let replay = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "gr-mint-1"}),
    );
    assert_eq!(replay["ok"], true, "{replay}");
    assert_eq!(replay["result"]["replayed"], true, "{replay}");
    assert_eq!(operator_revision(&mut operator, &session_id), 4);

    // Mint -> 5, revoke -> 6, and a second revoke is the idempotent no-op.
    let pending = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "gr-mint-2"}),
    );
    assert_eq!(pending["ok"], true, "{pending}");
    let pending_id = pending["result"]["grant_id"]
        .as_str()
        .expect("grant id")
        .to_owned();
    assert_eq!(operator_revision(&mut operator, &session_id), 5);

    let revoked = creator.call("session.grant.revoke", json!({"grant_id": pending_id}));
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");
    assert_eq!(revoked["result"]["not_active"], false, "{revoked}");
    assert_eq!(operator_revision(&mut operator, &session_id), 6);

    let again = creator.call("session.grant.revoke", json!({"grant_id": pending_id}));
    assert_eq!(again["ok"], true, "{again}");
    assert_eq!(again["result"]["revoked"], false, "{again}");
    assert_eq!(again["result"]["not_active"], true, "{again}");
    assert_eq!(operator_revision(&mut operator, &session_id), 6);

    // The detach still lands as one ordered bump after the grant mutations.
    let detached = observer.call("session.detach", json!({"attach_id": observer_attach}));
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(
        detached["result"]["session"]["state_revision"], 7,
        "{detached}"
    );
    assert_eq!(operator_revision(&mut operator, &session_id), 7);

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// A terminal session refuses an observer-grant mint — including a keyed
/// repeat — with `NotRunning`, and refuses a revoke the same way instead of
/// answering the idempotent `not_active` no-op.
#[test]
fn a_grant_on_a_terminal_session_is_not_running_and_revokes_nothing() {
    let root = TempRoot::new("terminal-grant");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        controller_token,
        ..
    } = controlled_session(&root, &mut operator, "terminal-grant");

    let minted = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "term-mint-1"}),
    );
    assert_eq!(minted["ok"], true, "{minted}");
    let pending_id = minted["result"]["grant_id"]
        .as_str()
        .expect("grant id")
        .to_owned();

    let aborted = creator.call(
        "session.abort",
        json!({
            "session_id": session_id,
            "expected_generation": 0,
            "reason_class": "cancel",
            "control_token": controller_token,
        }),
    );
    assert_eq!(aborted["ok"], true, "{aborted}");
    let terminal_revision = aborted["result"]["session"]["state_revision"]
        .as_u64()
        .expect("terminal revision");
    assert_eq!(
        operator_revision(&mut operator, &session_id),
        terminal_revision
    );

    // The terminal transition retires the session's key store, so the same
    // key is a new request on a frozen record, not a `D-18` replay.
    for key in ["term-mint-2", "term-mint-1"] {
        let mint = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": key}),
        );
        assert_eq!(mint["ok"], false, "{mint}");
        assert_eq!(mint["error"]["class"], "Denied", "{mint}");
        assert_eq!(mint["error"]["code"], "NotRunning", "{mint}");
        assert_eq!(
            operator_revision(&mut operator, &session_id),
            terminal_revision
        );
    }

    let revoked = creator.call("session.grant.revoke", json!({"grant_id": pending_id}));
    assert_eq!(revoked["ok"], false, "{revoked}");
    assert_eq!(revoked["error"]["class"], "Denied", "{revoked}");
    assert_eq!(revoked["error"]["code"], "NotRunning", "{revoked}");
    assert_eq!(
        operator_revision(&mut operator, &session_id),
        terminal_revision
    );

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// A former observer cannot revoke another client's pending grant once the
/// session is terminal: the mutation is `Denied/NotRunning` before the
/// authority relation, and the frozen revision does not change.
#[test]
fn a_former_observer_cannot_revoke_a_grant_on_a_terminal_session() {
    let root = TempRoot::new("terminal-observer-revoke");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let ControlledSession {
        mut creator,
        session_id,
        controller_token,
        ..
    } = controlled_session(&root, &mut operator, "terminal-observer-revoke");

    let observer_grant = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "term-observer-mint"}),
    );
    assert_eq!(observer_grant["ok"], true, "{observer_grant}");
    let observer_grant = observer_grant["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": observer_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");

    // A grant minted for another client stays pending across the transition.
    let pending = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "term-observer-pending"}),
    );
    assert_eq!(pending["ok"], true, "{pending}");
    let pending_id = pending["result"]["grant_id"]
        .as_str()
        .expect("pending grant id")
        .to_owned();

    let aborted = creator.call(
        "session.abort",
        json!({
            "session_id": session_id,
            "expected_generation": 0,
            "reason_class": "cancel",
            "control_token": controller_token,
        }),
    );
    assert_eq!(aborted["ok"], true, "{aborted}");
    let terminal_revision = aborted["result"]["session"]["state_revision"]
        .as_u64()
        .expect("terminal revision");

    let revoked = observer.call("session.grant.revoke", json!({"grant_id": pending_id}));
    assert_eq!(revoked["ok"], false, "{revoked}");
    assert_eq!(revoked["error"]["class"], "Denied", "{revoked}");
    assert_eq!(revoked["error"]["code"], "NotRunning", "{revoked}");
    assert_eq!(
        operator_revision(&mut operator, &session_id),
        terminal_revision
    );

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// A consumed host grant revoke refuses fresh and replayed creates at once.
#[test]
fn a_consumed_host_grant_revoke_withdraws_create_before_the_ttl() {
    let root = TempRoot::new("host-grant-revoke");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let grant_id = minted["result"]["grant_id"]
        .as_str()
        .expect("grant id")
        .to_owned();
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();

    let mut creator = Client::connect(&root.endpoint());
    let handshake = creator.negotiate_with(None, Some(&host_grant));
    assert_eq!(handshake["ok"], true, "{handshake}");
    let created = creator.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "revoke-k1"}),
    );
    assert_eq!(created["ok"], true, "{created}");

    let revoked = operator.call(
        "daemon.host_grant",
        json!({"action": "revoke", "grant_id": grant_id}),
    );
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");
    assert_eq!(revoked["result"]["not_active"], false, "{revoked}");

    // The fresh key and the key already applied on this connection are both
    // refused: the revoke withdraws the create authority before the keyed
    // lookup, so the 300 s TTL is not the effective bound and a replay cannot
    // resurrect the session.
    for key in ["revoke-k1", "revoke-k2"] {
        let refused = creator.call(
            "session.create",
            json!({"kind": "terminal", "idempotency_key": key}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["class"], "Denied", "{refused}");
        assert_eq!(refused["error"]["code"], "ConsentRequired", "{refused}");
    }

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// Pin the vacant-control `InvalidControlToken` and its adjacent boundaries.
#[test]
fn a_stale_control_token_at_a_vacant_control_is_invalid_control_token() {
    let root = TempRoot::new("control-token");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();

    let mut creator = Client::connect(&root.endpoint());
    assert_eq!(creator.negotiate_with(None, Some(&host_grant))["ok"], true);
    let created = creator.call(
        "session.create",
        json!({"kind": "terminal", "idempotency_key": "ct-create"}),
    );
    assert_eq!(created["ok"], true, "{created}");
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let create_token = created["result"]["control_token"]
        .as_str()
        .expect("create token")
        .to_owned();

    let claimed = creator.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": create_token,
            "idempotency_key": "ct-claim",
        }),
    );
    assert_eq!(claimed["ok"], true, "{claimed}");
    assert!(claimed["result"]["attach_id"].is_string(), "{claimed}");
    let controller_token = claimed["result"]["control_token"]
        .as_str()
        .expect("rotated token")
        .to_owned();

    let grant = creator.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "ct-grant"}),
    );
    assert_eq!(grant["ok"], true, "{grant}");
    let observer_grant = grant["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();

    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": observer_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    let observer_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("observer attach")
        .to_owned();

    let transferred = creator.call(
        "session.control.transfer",
        json!({
            "session_id": session_id,
            "attach_id": observer_attach,
            "expected_generation": 0,
            "idempotency_key": "ct-transfer",
        }),
    );
    assert_eq!(transferred["ok"], true, "{transferred}");

    // The rotated token is delivered only on the target connection's next
    // response, so read it through the target's own heartbeat.
    let mut rotated = None;
    for _ in 0..2 {
        let heartbeat = observer.call("lease.heartbeat", json!({"attach_id": observer_attach}));
        if let Some(token) = heartbeat["result"]["control_token"].as_str() {
            rotated = Some(token.to_owned());
        }
    }
    let rotated = rotated.expect("target-addressed rotated token");

    let mut claimant = Client::connect(&root.endpoint());
    assert_eq!(claimant.negotiate(None)["ok"], true);

    // Wrong token while another lease holds control: the stale token is not a
    // visibility relation, so the target stays uniformly `NotFound`.
    let occupied = claimant.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": controller_token,
            "idempotency_key": "ct-occupied",
        }),
    );
    assert_eq!(occupied["ok"], false, "{occupied}");
    assert_eq!(occupied["error"]["class"], "NotFound", "{occupied}");
    assert_eq!(occupied["error"]["code"], "NotFound", "{occupied}");

    // The transfer target detaches its own lease: control is vacant and the
    // rotated token is still current.
    let detached = observer.call("session.detach", json!({"attach_id": observer_attach}));
    assert_eq!(detached["ok"], true, "{detached}");

    // The same stale token at the vacant control is the closed contract code,
    // not the uniform `NotFound`.
    let stale = claimant.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": controller_token,
            "idempotency_key": "ct-stale",
        }),
    );
    assert_eq!(stale["ok"], false, "{stale}");
    assert_eq!(stale["error"]["class"], "Denied", "{stale}");
    assert_eq!(stale["error"]["code"], "InvalidControlToken", "{stale}");
    assert!(
        !stale.to_string().contains(&controller_token),
        "the refusal echoes no token"
    );

    // A truly invisible target is still `NotFound`, never the token code.
    let unknown_session = "0".repeat(32);
    let invisible = claimant.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": unknown_session,
            "expected_generation": 0,
            "control_token": "1".repeat(64),
            "idempotency_key": "ct-unknown",
        }),
    );
    assert_eq!(invisible["ok"], false, "{invisible}");
    assert_eq!(invisible["error"]["class"], "NotFound", "{invisible}");
    assert_eq!(invisible["error"]["code"], "NotFound", "{invisible}");

    // The valid rotated token claims the vacant control.
    let reclaimed = claimant.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "control_token": rotated,
            "idempotency_key": "ct-reclaim",
        }),
    );
    assert_eq!(reclaimed["ok"], true, "{reclaimed}");
    assert_eq!(reclaimed["result"]["role"], "controller", "{reclaimed}");
    assert!(reclaimed["result"]["attach_id"].is_string(), "{reclaimed}");

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

/// Every control method maps a missing/stale token at a vacant control and
/// keeps the occupied/invisible/post-transfer boundaries.
#[test]
fn every_control_method_maps_a_stale_token_at_a_vacant_control() {
    let root = TempRoot::new("control-methods");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let ControlledSession {
        mut creator,
        session_id,
        create_token,
        controller_token,
        controller_attach,
    } = controlled_session(&root, &mut operator, "cm");
    vacate_control(&mut creator, &controller_attach);

    let mut claimant = Client::connect(&root.endpoint());
    assert_eq!(claimant.negotiate(None)["ok"], true);

    // Missing tokens are the contract's closed code at a vacant control, for
    // both controller attach and the token-optional control methods.
    let missing_attach = claimant.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 0,
            "idempotency_key": "cm-attach-missing",
        }),
    );
    assert_invalid_control_token(&missing_attach);
    let missing_grant = claimant.call(
        "session.grant",
        json!({"session_id": session_id, "idempotency_key": "cm-grant-missing"}),
    );
    assert_invalid_control_token(&missing_grant);

    // session.grant
    let stale = claimant.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": create_token,
            "idempotency_key": "cm-grant-stale",
        }),
    );
    assert_invalid_control_token(&stale);
    let granted = claimant.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": controller_token,
            "idempotency_key": "cm-grant-ok",
        }),
    );
    assert_eq!(granted["ok"], true, "{granted}");
    let grant_id = granted["result"]["grant_id"]
        .as_str()
        .expect("grant id")
        .to_owned();

    // session.grant.revoke
    let stale = claimant.call(
        "session.grant.revoke",
        json!({"grant_id": grant_id, "control_token": create_token}),
    );
    assert_invalid_control_token(&stale);
    let revoked = claimant.call(
        "session.grant.revoke",
        json!({"grant_id": grant_id, "control_token": controller_token}),
    );
    assert_eq!(revoked["ok"], true, "{revoked}");
    assert_eq!(revoked["result"]["revoked"], true, "{revoked}");

    // session.detach (another lease)
    let observer_grant = claimant.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": controller_token,
            "idempotency_key": "cm-detach-grant",
        }),
    )["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    let mut observer = Client::connect(&root.endpoint());
    assert_eq!(observer.negotiate(None)["ok"], true);
    let attached = observer.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": observer_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    let observer_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("observer attach")
        .to_owned();
    let stale = claimant.call(
        "session.detach",
        json!({"attach_id": observer_attach, "control_token": create_token}),
    );
    assert_invalid_control_token(&stale);
    let detached = claimant.call(
        "session.detach",
        json!({"attach_id": observer_attach, "control_token": controller_token}),
    );
    assert_eq!(detached["ok"], true, "{detached}");
    assert_eq!(detached["result"]["was_attached"], true, "{detached}");

    // session.control.transfer
    let target_grant = claimant.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": controller_token,
            "idempotency_key": "cm-transfer-grant",
        }),
    )["result"]["grant"]
        .as_str()
        .expect("observer grant")
        .to_owned();
    let mut target = Client::connect(&root.endpoint());
    assert_eq!(target.negotiate(None)["ok"], true);
    let attached = target.call(
        "session.attach",
        json!({
            "attach_role": "observer",
            "session_id": session_id,
            "observer_grant": target_grant,
        }),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    let target_attach = attached["result"]["attach_id"]
        .as_str()
        .expect("target attach")
        .to_owned();
    let stale = claimant.call(
        "session.control.transfer",
        json!({
            "session_id": session_id,
            "attach_id": target_attach,
            "expected_generation": 0,
            "control_token": create_token,
            "idempotency_key": "cm-transfer-stale",
        }),
    );
    assert_invalid_control_token(&stale);
    let transferred = claimant.call(
        "session.control.transfer",
        json!({
            "session_id": session_id,
            "attach_id": target_attach,
            "expected_generation": 0,
            "control_token": controller_token,
            "idempotency_key": "cm-transfer-ok",
        }),
    );
    assert_eq!(transferred["ok"], true, "{transferred}");
    assert_eq!(
        transferred["result"]["controller"], target_attach,
        "{transferred}"
    );

    // Read the token rotated onto the transfer target, then detach it to make
    // control vacant again.
    let mut rotated = None;
    for _ in 0..2 {
        let heartbeat = target.call("lease.heartbeat", json!({"attach_id": target_attach}));
        if let Some(token) = heartbeat["result"]["control_token"].as_str() {
            rotated = Some(token.to_owned());
        }
    }
    let rotated = rotated.expect("target-addressed rotated token");
    let mut post = Client::connect(&root.endpoint());
    assert_eq!(post.negotiate(None)["ok"], true);

    // While the transfer target still held control, the pre-rotation token was
    // not a visibility relation: the target stays the uniform NotFound.
    let occupied = post.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": controller_token,
            "idempotency_key": "cm-occupied",
        }),
    );
    assert_eq!(occupied["ok"], false, "{occupied}");
    assert_eq!(occupied["error"]["class"], "NotFound", "{occupied}");
    assert_eq!(occupied["error"]["code"], "NotFound", "{occupied}");

    let detached = target.call("session.detach", json!({"attach_id": target_attach}));
    assert_eq!(detached["ok"], true, "{detached}");

    // After the target detaches, the pre-transfer token is refused and the
    // rotated token is accepted.
    let old = post.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": controller_token,
            "idempotency_key": "cm-post-old",
        }),
    );
    assert_invalid_control_token(&old);
    let new = post.call(
        "session.grant",
        json!({
            "session_id": session_id,
            "control_token": rotated,
            "idempotency_key": "cm-post-new",
        }),
    );
    assert_eq!(new["ok"], true, "{new}");

    // session.abort on a second session: a stale token is the same code and
    // the current token commits the pre-bind cancel.
    let ControlledSession {
        creator: mut second_creator,
        session_id: second_session,
        create_token: second_create_token,
        controller_token: second_controller_token,
        controller_attach: second_attach,
    } = controlled_session(&root, &mut operator, "cm-abort");
    vacate_control(&mut second_creator, &second_attach);
    let mut abort_claimant = Client::connect(&root.endpoint());
    assert_eq!(abort_claimant.negotiate(None)["ok"], true);
    let stale = abort_claimant.call(
        "session.abort",
        json!({
            "session_id": second_session,
            "expected_generation": 0,
            "reason_class": "cancel",
            "control_token": second_create_token,
        }),
    );
    assert_invalid_control_token(&stale);
    let aborted = abort_claimant.call(
        "session.abort",
        json!({
            "session_id": second_session,
            "expected_generation": 0,
            "reason_class": "cancel",
            "control_token": second_controller_token,
        }),
    );
    assert_eq!(aborted["ok"], true, "{aborted}");

    assert_eq!(
        operator.call("daemon.stop", json!({"mode": "force"}))["ok"],
        true
    );
    assert_eq!(daemon.wait_within(Duration::from_secs(10)).code(), Some(0));
}

#[test]
fn an_unnegotiated_connection_cannot_probe_the_method_catalog() {
    let root = TempRoot::new("catalog-gate");
    let daemon = Daemon::start(&root);

    // A well-formed but unknown method is AlreadyNegotiated, not NotFound: the
    // stage-2 gate runs before method resolution.
    let mut unknown = Client::connect(&root.endpoint());
    let response = unknown.call("session.unknown", Value::Null);
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["error"]["code"], "AlreadyNegotiated");
    assert_eof(&mut unknown.stream, Duration::from_secs(2));

    // A grammar-invalid name is also AlreadyNegotiated, not InvalidMethod.
    let mut malformed = Client::connect(&root.endpoint());
    malformed.send_raw_frame(json!({
        "v": 1,
        "id": "malformed-1",
        "method": "Session.Create",
        "params": Value::Null,
    }));
    let response = malformed.recv();
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["error"]["code"], "AlreadyNegotiated");
    assert_eof(&mut malformed.stream, Duration::from_secs(2));

    // Stage 1 stays ahead of the gate: an unsupported wire version is
    // VersionMismatch even when the method is not the handshake.
    let mut wrong_version = Client::connect(&root.endpoint());
    wrong_version.send_raw_frame(json!({
        "v": 99,
        "id": "version-1",
        "method": "session.unknown",
        "params": Value::Null,
    }));
    let response = wrong_version.recv();
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["error"]["code"], "VersionMismatch");
    assert_eof(&mut wrong_version.stream, Duration::from_secs(2));

    // After a successful negotiate the same unknown method is NotFound.
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true);
    let response = client.call("session.unknown", Value::Null);
    assert_eq!(response["error"]["code"], "NotFound");

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_wire_refusal_burst_is_budgeted_and_aggregated() {
    let root = TempRoot::new("wire-budget");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true);

    // `refusal_audit_budget_per_sec` is 20 at the compiled defaults. A burst of
    // non-closing wire refusals must not write one record per frame, and the
    // frames past the budget must not be invisible: the window ends with one
    // aggregate carrying the suppressed count and the code histogram.
    let requests: Vec<(&str, Value)> = (0..60).map(|_| ("session.unknown", Value::Null)).collect();
    client.send_batch(&requests);
    for _ in 0..60 {
        let response = client.recv();
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "NotFound");
    }

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    let (individual, suppressed, aggregates) = wire_audit_totals(&root);
    assert_eq!(individual, 20, "the budget bounds the individual records");
    assert_eq!(suppressed, 40, "every suppressed refusal is counted");
    assert_eq!(aggregates, 1, "one aggregate per window with suppression");
    assert_eq!(
        individual as u64 + suppressed,
        60,
        "no refusal is invisible"
    );
    assert_eq!(
        audit_lines_with(&root, "a=wire r=refused c=rate_limited"),
        1,
        "the aggregate is the wire record with the budget refusal"
    );
    assert_eq!(
        audit_lines_with(&root, "h=not_found:40"),
        1,
        "the bounded histogram carries the suppressed code"
    );
}

#[test]
fn plain_status_never_binds_and_creates_no_runtime_file() {
    let root = TempRoot::new("status");
    std::fs::create_dir_all(root.runtime()).expect("runtime dir");
    std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod runtime");
    std::fs::create_dir_all(root.state()).expect("state dir");
    let output = Command::new(BITTYD)
        .env("XDG_RUNTIME_DIR", root.runtime())
        .env("XDG_STATE_HOME", root.state())
        .env_remove("BITTYD_ENDPOINT")
        .output()
        .expect("status");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ipc service: wired"), "{stdout}");
    assert!(stdout.contains("endpoint: none bound"), "{stdout}");
    assert!(!root.endpoint().exists());
    assert!(!root.capability().exists());
}

// ── worker spawn/bind, generation 1, and `session.resize` (bitty#2790) ──────

/// Create a worker with host consent on a fresh connection.
///
/// The spawn specification is the bounded six-field subset; the helper keeps
/// one successful shape so a test only varies what it asserts.
fn create_worker(
    root: &TempRoot,
    operator: &mut Client,
    label: &str,
    argv: Value,
) -> (Client, Value) {
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();
    let mut creator = Client::connect(&root.endpoint());
    assert_eq!(creator.negotiate_with(None, Some(&host_grant))["ok"], true);
    let created = creator.call(
        "session.create",
        json!({
            "kind": "worker",
            "idempotency_key": format!("{label}-create"),
            "argv": argv,
            "cwd": "/",
            "cols": 100,
            "rows": 30,
            "env": ["BITTYD_WORKER_TEST=1"],
            "term": "xterm-256color",
        }),
    );
    assert_eq!(created["ok"], true, "{created}");
    (creator, created)
}

/// Read one session through the operator view.
fn operator_session(operator: &mut Client, session_id: &str) -> Value {
    let status = operator.call("session.status", json!({"session_id": session_id}));
    assert_eq!(status["ok"], true, "{status}");
    status["result"]["session"].clone()
}

/// Poll the operator view until the session reaches one of `states`.
fn wait_for_worker_state(operator: &mut Client, session_id: &str, states: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let session = operator_session(operator, session_id);
        let state = session["state"].as_str().unwrap_or("");
        if states.contains(&state) {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "worker never reached {states:?}: {session}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_worker_create_binds_generation_one_and_keeps_its_journal_record_while_live() {
    let root = TempRoot::new("worker-bind");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let (mut creator, created) =
        create_worker(&root, &mut operator, "bind", json!(["/bin/sleep", "60"]));
    let session = &created["result"]["session"];
    assert_eq!(session["kind"], "worker");
    assert_eq!(session["state"], "running");
    assert_eq!(session["generation"], 1, "the worker binds generation one");
    assert_eq!(session["state_revision"], 1);
    assert_eq!(session["attach_count"], 0);
    assert_eq!(created["result"]["replayed"], false);
    let token = created["result"]["control_token"]
        .as_str()
        .expect("create token");
    assert_eq!(token.len(), 64);
    let session_id = session["session_id"].as_str().expect("session id");

    // The same key and the same specification replay the bound record without
    // spawning a second worker.
    let replay = creator.call(
        "session.create",
        json!({
            "kind": "worker",
            "idempotency_key": "bind-create",
            "argv": ["/bin/sleep", "60"],
            "cwd": "/",
            "cols": 100,
            "rows": 30,
            "env": ["BITTYD_WORKER_TEST=1"],
            "term": "xterm-256color",
        }),
    );
    assert_eq!(replay["ok"], true, "{replay}");
    assert_eq!(replay["result"]["replayed"], true);
    assert_eq!(replay["result"]["session"]["session_id"], session_id);
    assert_eq!(replay["result"]["session"]["generation"], 1);

    // A different spawn specification under the same key is a conflict, never
    // a descriptor replay.
    let conflict = creator.call(
        "session.create",
        json!({
            "kind": "worker",
            "idempotency_key": "bind-create",
            "argv": ["/bin/sleep", "61"],
            "cwd": "/",
        }),
    );
    assert_eq!(conflict["ok"], false, "{conflict}");
    assert_eq!(conflict["error"]["code"], "IdempotencyConflict");

    // The bound session still owns its committed journal record. A reconcile
    // read must skip it: the live worker is not an orphan.
    let verified = operator.call("daemon.orphans.verify", json!({"action": "verify"}));
    assert_eq!(verified["ok"], true, "{verified}");
    assert_eq!(verified["result"]["committed"], 1);
    assert_eq!(verified["result"]["reserved"], 0);
    let after = operator_session(&mut operator, session_id);
    assert_eq!(after["state"], "running", "{after}");
    assert_eq!(
        after["state_revision"], 1,
        "the reconcile read changes nothing"
    );

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn worker_resize_requires_the_controller_lease_and_bumps_the_revision() {
    let root = TempRoot::new("worker-resize");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let (mut creator, created) =
        create_worker(&root, &mut operator, "resize", json!(["/bin/sleep", "60"]));
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // The controller lease is the authority for the content plane.
    let claimed = creator.call(
        "session.attach",
        json!({
            "attach_role": "controller",
            "session_id": session_id,
            "expected_generation": 1,
            "control_token": created["result"]["control_token"],
            "idempotency_key": "resize-claim",
        }),
    );
    assert_eq!(claimed["ok"], true, "{claimed}");
    assert_eq!(claimed["result"]["session"]["generation"], 1);

    let resized = creator.call(
        "session.resize",
        json!({"session_id": session_id, "generation": 1, "cols": 120, "rows": 40}),
    );
    assert_eq!(resized["ok"], true, "{resized}");
    assert_eq!(resized["result"]["session"]["state"], "running");
    assert_eq!(resized["result"]["session"]["generation"], 1);
    assert_eq!(
        resized["result"]["session"]["state_revision"], 3,
        "attach then resize bump the revision exactly once each"
    );

    // A stale generation is refused before the content plane is touched.
    let stale = creator.call(
        "session.resize",
        json!({"session_id": session_id, "generation": 0, "cols": 80, "rows": 24}),
    );
    assert_eq!(stale["ok"], false, "{stale}");
    assert_eq!(stale["error"]["class"], "Denied");
    assert_eq!(stale["error"]["code"], "StaleGeneration");
    assert_eq!(
        operator_session(&mut operator, &session_id)["state_revision"],
        3,
        "a stale resize changes nothing"
    );

    // A zero or over-bound dimension is a malformed request.
    for (cols, rows) in [(0, 24), (80, 0), (1001, 24), (80, 1001)] {
        let invalid = creator.call(
            "session.resize",
            json!({"session_id": session_id, "generation": 1, "cols": cols, "rows": rows}),
        );
        assert_eq!(invalid["ok"], false, "{invalid}");
        assert_eq!(invalid["error"]["code"], "InvalidRequest");
    }
    let unknown = creator.call(
        "session.resize",
        json!({
            "session_id": "33333333333333333333333333333333",
            "generation": 1,
            "cols": 80,
            "rows": 24,
        }),
    );
    assert_eq!(unknown["ok"], false, "{unknown}");
    assert_eq!(unknown["error"]["code"], "NotFound");

    // An observer holds a lease but never control; resize is the typed
    // controller-only refusal and changes no revision.
    let (mut observer, _attach) = attach_observer(&root, &mut creator, &session_id, "resize");
    let before = operator_session(&mut operator, &session_id)["state_revision"]
        .as_u64()
        .expect("revision");
    let refused = observer.call(
        "session.resize",
        json!({"session_id": session_id, "generation": 1, "cols": 90, "rows": 30}),
    );
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["class"], "Denied");
    assert_eq!(refused["error"]["code"], "ControllerRequired");
    assert_eq!(
        operator_session(&mut operator, &session_id)["state_revision"],
        before,
        "an observer resize changes nothing"
    );

    // A same-user connection with neither the operator view nor a live lease
    // gets the uniform NotFound: resize is not an existence or revision
    // oracle.
    let mut stranger = Client::connect(&root.endpoint());
    assert_eq!(stranger.negotiate(None)["ok"], true);
    let hidden = stranger.call(
        "session.resize",
        json!({"session_id": session_id, "generation": 1, "cols": 80, "rows": 24}),
    );
    assert_eq!(hidden["ok"], false, "{hidden}");
    assert_eq!(hidden["error"]["class"], "NotFound");
    assert_eq!(hidden["error"]["code"], "NotFound");
    assert_eq!(
        operator_session(&mut operator, &session_id)["state_revision"],
        before,
        "a hidden resize changes nothing"
    );

    // Control is still held by the original controller after the refusal.
    let still = creator.call(
        "session.resize",
        json!({"session_id": session_id, "generation": 1, "cols": 100, "rows": 30}),
    );
    assert_eq!(still["ok"], true, "{still}");
    assert_eq!(still["result"]["session"]["state_revision"], before + 1);

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_failed_worker_spawn_leaves_no_journal_record_or_bound_session() {
    let root = TempRoot::new("worker-spawn-fail");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let (_creator, created) = create_worker(
        &root,
        &mut operator,
        "spawn-fail",
        json!(["/nonexistent/bittyd-worker-probe"]),
    );
    let session = &created["result"]["session"];
    assert_eq!(session["kind"], "worker");
    assert_eq!(session["state"], "failed");
    assert_eq!(session["terminal_reason"], "spawn_failed");
    assert_eq!(session["cleanup"], "not_started");
    assert_eq!(session["generation"], 0, "a failed spawn never binds");
    assert_eq!(
        created["result"]["control_token"],
        Value::Null,
        "a failed session has no claimable control"
    );
    let session_id = session["session_id"].as_str().expect("session id");

    // No reserved or committed record survives the failure.
    let verified = operator.call("daemon.orphans.verify", json!({"action": "verify"}));
    assert_eq!(verified["ok"], true, "{verified}");
    assert_eq!(verified["result"]["reserved"], 0);
    assert_eq!(verified["result"]["committed"], 0);

    // The failed record is visible as a terminal worker, not as a running one.
    let failed = operator_session(&mut operator, session_id);
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["terminal_reason"], "spawn_failed");
    let status = operator.call("daemon.status", Value::Null);
    assert_eq!(status["ok"], true, "{status}");
    assert_eq!(status["result"]["active_sessions"], 0);
    assert_eq!(status["result"]["sessions"]["running"], 0);
    assert_eq!(status["result"]["sessions"]["terminal"], 1);

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_worker_exit_is_recorded_once_with_its_containment_cleanup() {
    let root = TempRoot::new("worker-exit");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    let (mut creator, created) = create_worker(
        &root,
        &mut operator,
        "exit",
        json!(["/bin/sh", "-c", "exit 7"]),
    );
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let exited = wait_for_worker_state(&mut operator, &session_id, &["exited", "failed"]);
    assert_eq!(exited["terminal_reason"], "worker_exit");
    assert_eq!(exited["generation"], 1);
    assert_eq!(exited["kind"], "worker");

    // The cleanup class is the mechanism's truthful claim: a cgroup subtree is
    // swept and its journal record removed; the subreaper fallback can never
    // prove a whole tree and quarantines the record instead.
    let status = operator.call("daemon.status", Value::Null);
    let mechanism = status["result"]["orphans"]["mechanism"]
        .as_str()
        .expect("mechanism");
    let verified = operator.call("daemon.orphans.verify", json!({"action": "verify"}));
    assert_eq!(verified["ok"], true, "{verified}");
    match mechanism {
        "cgroup_v2" => {
            assert_eq!(exited["cleanup"], "swept");
            assert_eq!(verified["result"]["committed"], 0);
            assert_eq!(verified["result"]["reserved"], 0);
            assert_eq!(verified["result"]["unverified"], 0);
        }
        "subreaper_fallback" => {
            assert_eq!(exited["cleanup"], "unverified");
            assert_eq!(verified["result"]["committed"], 0);
            assert_eq!(verified["result"]["reserved"], 0);
            assert_eq!(
                verified["result"]["unverified"], 1,
                "an unprovable tree stays quarantined, never silently forgotten"
            );
        }
        other => panic!("unexpected mechanism {other}"),
    }

    // A second read is stable: the terminal transition is recorded once.
    let again = operator_session(&mut operator, &session_id);
    assert_eq!(again["state"], exited["state"]);
    assert_eq!(again["state_revision"], exited["state_revision"]);

    // The terminal transition retired the create token, so a keyed replay of
    // the create returns the same terminal record without a claimable token.
    let replay = creator.call(
        "session.create",
        json!({
            "kind": "worker",
            "idempotency_key": "exit-create",
            "argv": ["/bin/sh", "-c", "exit 7"],
            "cwd": "/",
            "cols": 100,
            "rows": 30,
            "env": ["BITTYD_WORKER_TEST=1"],
            "term": "xterm-256color",
        }),
    );
    assert_eq!(replay["ok"], true, "{replay}");
    assert!(
        replay["result"]["control_token"].is_null(),
        "a terminal replay exposes no claimable token"
    );
    assert_eq!(replay["result"]["session"]["state"], exited["state"]);

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn a_worker_exit_kills_the_recorded_group_before_the_reap() {
    let root = TempRoot::new("worker-descendant");
    let daemon = Daemon::start(&root);
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

    // The shell leaves a background descendant in its process group and then
    // exits. A reap-before-signal path would drop the group id before the kill
    // and leak the descendant.
    let pid_file = root.path.join("descendant.pid");
    let (_creator, created) = create_worker(
        &root,
        &mut operator,
        "descendant",
        json!([
            "/bin/sh",
            "-c",
            "sleep 60 & echo $! > \"$1\"; exit 3",
            "bittyd-worker",
            pid_file.to_str().expect("a UTF-8 temp path"),
        ]),
    );
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let exited = wait_for_worker_state(&mut operator, &session_id, &["exited", "failed"]);
    assert_eq!(exited["terminal_reason"], "worker_exit");

    let descendant: u32 = std::fs::read_to_string(&pid_file)
        .expect("the shell wrote the descendant pid")
        .trim()
        .parse()
        .expect("a pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{descendant}")).exists() {
        assert!(
            Instant::now() < deadline,
            "descendant {descendant} survived the worker exit"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

#[test]
fn worker_create_refuses_without_consent_and_before_an_invalid_kind() {
    let root = TempRoot::new("worker-consent");
    let daemon = Daemon::start(&root);
    let mut client = Client::connect(&root.endpoint());
    assert_eq!(client.negotiate(None)["ok"], true, "no host grant");

    // The connection holds `process.spawn` but no `session.host`: consent is
    // the refusal, not a capability one.
    let consent = client.call(
        "session.create",
        json!({
            "kind": "worker",
            "idempotency_key": "consent-worker",
            "argv": ["/bin/sleep", "60"],
            "cwd": "/",
        }),
    );
    assert_eq!(consent["ok"], false, "{consent}");
    assert_eq!(consent["error"]["class"], "Denied");
    assert_eq!(consent["error"]["code"], "ConsentRequired");

    // An unknown kind is gated the same way: create scope first.
    let unknown = client.call(
        "session.create",
        json!({"kind": "daemon", "idempotency_key": "unknown-kind"}),
    );
    assert_eq!(unknown["ok"], false, "{unknown}");
    assert_eq!(unknown["error"]["code"], "ConsentRequired");

    // With create authority, the unknown kind itself is malformed.
    let capability = read_capability(&root);
    let mut operator = Client::connect(&root.endpoint());
    assert_eq!(operator.negotiate(Some(&capability))["ok"], true);
    let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
    assert_eq!(minted["ok"], true, "{minted}");
    let host_grant = minted["result"]["grant"]
        .as_str()
        .expect("host grant bearer")
        .to_owned();
    let mut creator = Client::connect(&root.endpoint());
    assert_eq!(creator.negotiate_with(None, Some(&host_grant))["ok"], true);
    let malformed = creator.call(
        "session.create",
        json!({"kind": "daemon", "idempotency_key": "unknown-kind"}),
    );
    assert_eq!(malformed["ok"], false, "{malformed}");
    assert_eq!(malformed["error"]["code"], "InvalidRequest");

    let status = operator.call("daemon.status", Value::Null);
    assert_eq!(status["result"]["active_sessions"], 0);
    assert_eq!(status["result"]["sessions"]["created"], 0);

    assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
}

/// bitty#2792: pushed session events on the negotiated connection.
///
/// Every test here opts in with `events: true` at `daemon.negotiate` unless it
/// is asserting the `events: false` behavior, so the opt-in is what turns the
/// wire on (`D-10`).
mod event_wire {
    use super::*;

    /// Negotiate with the event opt-in, optionally presenting a host grant.
    fn negotiate_events(client: &mut Client, host_grant: Option<&str>) -> Value {
        let mut params = json!({
            "client_version": {"major": 1, "minor": 0},
            "events": true,
        });
        if let Some(grant) = host_grant {
            params["host_grant"] = Value::String(grant.to_owned());
        }
        client.call("daemon.negotiate", params)
    }

    /// Read the next frame and assert it is a pushed event, not a response.
    fn recv_event(client: &mut Client) -> Value {
        let frame = client.recv();
        assert!(
            frame.get("event").is_some(),
            "expected an event frame, got {frame}"
        );
        assert!(
            frame.get("ok").is_none(),
            "an event frame is not a response envelope: {frame}"
        );
        assert_eq!(frame["v"], 1, "the event rides the v1 wire: {frame}");
        frame["event"].clone()
    }

    /// A host-consent mint plus an event-opted consumer connection.
    fn event_consumer(root: &TempRoot, operator: &mut Client, label: &str) -> (Client, Value) {
        let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
        assert_eq!(minted["ok"], true, "{minted}");
        let host_grant = minted["result"]["grant"]
            .as_str()
            .expect("host grant bearer")
            .to_owned();
        let mut client = Client::connect(&root.endpoint());
        let handshake = negotiate_events(&mut client, Some(&host_grant));
        assert_eq!(handshake["ok"], true, "{handshake}");
        assert_eq!(
            handshake["result"]["capabilities"]["events"], true,
            "{label}: the served cell advertises delivery"
        );
        (client, handshake)
    }

    fn create_and_claim(client: &mut Client, label: &str) -> (String, String, String, Value) {
        let created = client.call(
            "session.create",
            json!({"kind": "terminal", "idempotency_key": format!("{label}-create")}),
        );
        assert_eq!(created["ok"], true, "{created}");
        let session_id = created["result"]["session"]["session_id"]
            .as_str()
            .expect("session id")
            .to_owned();
        let create_token = created["result"]["control_token"]
            .as_str()
            .expect("create token")
            .to_owned();
        let claimed = client.call(
            "session.attach",
            json!({
                "attach_role": "controller",
                "session_id": session_id,
                "expected_generation": 0,
                "control_token": create_token,
                "idempotency_key": format!("{label}-claim"),
            }),
        );
        assert_eq!(claimed["ok"], true, "{claimed}");
        let attach_id = claimed["result"]["attach_id"]
            .as_str()
            .expect("controller attach")
            .to_owned();
        let controller_token = claimed["result"]["control_token"]
            .as_str()
            .expect("rotated token")
            .to_owned();
        (session_id, attach_id, controller_token, created)
    }

    #[test]
    fn events_follow_the_response_in_revision_order() {
        let root = TempRoot::new("events-order");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let (mut creator, handshake) = event_consumer(&root, &mut operator, "order");
        let instance = handshake["result"]["daemon_instance_id"]
            .as_str()
            .expect("instance id")
            .to_owned();
        let (session_id, attach_id, controller_token, _created) =
            create_and_claim(&mut creator, "order");

        // The attach response is the first frame; the new lease's single copy
        // of its own attach event is the replay, not a queued frame.
        let minted = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": "order-grant"}),
        );
        assert_eq!(minted["ok"], true, "{minted}");
        let event = recv_event(&mut creator);
        assert_eq!(event["kind"], "session_state", "{event}");
        assert_eq!(event["session_id"], session_id, "{event}");
        assert_eq!(event["attach_id"], attach_id, "{event}");
        assert_eq!(event["instance_id"], instance, "{event}");
        assert_eq!(event["terminal"], false, "{event}");
        let mint_revision = event["revision"].as_u64().expect("revision");

        let grant_id = minted["result"]["grant_id"]
            .as_str()
            .expect("grant id")
            .to_owned();
        let revoked = creator.call(
            "session.grant.revoke",
            json!({"grant_id": grant_id, "control_token": controller_token}),
        );
        assert_eq!(revoked["ok"], true, "{revoked}");
        let event = recv_event(&mut creator);
        assert_eq!(event["kind"], "session_state", "{event}");
        let revoke_revision = event["revision"].as_u64().expect("revision");
        assert!(
            revoke_revision > mint_revision,
            "the stream is strictly revision-ordered"
        );
        let status = operator.call("session.status", json!({"session_id": session_id}));
        assert_eq!(
            status["result"]["session"]["state_revision"], revoke_revision,
            "the last delivered event is the current revision: {status}"
        );

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    #[test]
    fn an_observer_receives_only_its_own_lease_copy() {
        let root = TempRoot::new("events-routing");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let (mut creator, _) = event_consumer(&root, &mut operator, "routing");
        let (session_id, controller_attach, _, _) = create_and_claim(&mut creator, "routing");

        let minted = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": "routing-grant"}),
        );
        assert_eq!(minted["ok"], true, "{minted}");
        let grant = minted["result"]["grant"]
            .as_str()
            .expect("observer grant")
            .to_owned();
        let _ = recv_event(&mut creator);

        let mut observer = Client::connect(&root.endpoint());
        assert_eq!(negotiate_events(&mut observer, None)["ok"], true);
        let attached = observer.call(
            "session.attach",
            json!({
                "attach_role": "observer",
                "session_id": session_id,
                "observer_grant": grant,
            }),
        );
        assert_eq!(attached["ok"], true, "{attached}");
        let observer_attach = attached["result"]["attach_id"]
            .as_str()
            .expect("observer attach")
            .to_owned();
        assert_ne!(observer_attach, controller_attach);

        // The controller revokes the observer's lease: the target receives the
        // target-only `lease_revoked` frame and the survivor receives the
        // canonical `session_state`, each naming its own lease.
        let detached = creator.call(
            "session.detach",
            json!({"attach_id": observer_attach, "control_token": null}),
        );
        assert_eq!(detached["ok"], true, "{detached}");

        let observer_event = recv_event(&mut observer);
        assert_eq!(observer_event["kind"], "lease_revoked", "{observer_event}");
        assert_eq!(
            observer_event["attach_id"], observer_attach,
            "{observer_event}"
        );
        assert_ne!(
            observer_event["attach_id"], controller_attach,
            "a lease never receives another lease's copy"
        );
        // The revoked target keeps its connection: the final frame is
        // delivered and the client reconciles with `session.status`.
        let status = observer.call("session.status", json!({"session_id": session_id}));
        assert_eq!(status["ok"], false, "{status}");
        assert_eq!(status["error"]["code"], "NotFound", "{status}");

        let creator_event = recv_event(&mut creator);
        assert_eq!(creator_event["kind"], "session_state", "{creator_event}");
        assert_eq!(
            creator_event["attach_id"], controller_attach,
            "{creator_event}"
        );
        assert_ne!(creator_event["attach_id"], observer_attach);

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    #[test]
    fn the_terminal_event_is_delivered_with_its_marker() {
        let root = TempRoot::new("events-terminal");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let (mut creator, _) = event_consumer(&root, &mut operator, "terminal");
        let (session_id, attach_id, controller_token, _) =
            create_and_claim(&mut creator, "terminal");

        let aborted = creator.call(
            "session.abort",
            json!({
                "session_id": session_id,
                "reason_class": "cancel",
                "generation": 0,
                "control_token": controller_token,
            }),
        );
        assert_eq!(aborted["ok"], true, "{aborted}");
        let terminal_revision = aborted["result"]["session"]["state_revision"]
            .as_u64()
            .expect("revision");

        // Every revision the abort advanced is delivered in order; the terminal
        // one carries the marker and is never shed.
        let mut event = recv_event(&mut creator);
        while event["kind"] != "session_terminal" {
            assert_eq!(event["kind"], "session_state", "{event}");
            event = recv_event(&mut creator);
        }
        assert_eq!(event["terminal"], true, "{event}");
        assert_eq!(event["revision"], terminal_revision, "{event}");
        assert_eq!(event["attach_id"], attach_id, "{event}");
        assert_eq!(
            event["terminal_marker"]["reason"], "consumer_cancel",
            "{event}"
        );

        // The terminal record is visible to the reconciliation read, and the
        // client that received the frame never has to guess.
        let status = operator.call("session.status", json!({"session_id": session_id}));
        assert_eq!(status["ok"], true, "{status}");
        assert_eq!(status["result"]["session"]["state"], "terminated");
        assert_eq!(
            status["result"]["session"]["state_revision"], terminal_revision,
            "{status}"
        );

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    #[test]
    fn a_disconnect_releases_the_lease_and_notifies_the_survivor() {
        let root = TempRoot::new("events-disconnect");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let (mut creator, _) = event_consumer(&root, &mut operator, "disconnect");
        let (session_id, _controller_attach, _, _) = create_and_claim(&mut creator, "disconnect");

        let minted = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": "disconnect-grant"}),
        );
        assert_eq!(minted["ok"], true, "{minted}");
        let grant = minted["result"]["grant"]
            .as_str()
            .expect("observer grant")
            .to_owned();
        let _ = recv_event(&mut creator);

        let mut observer = Client::connect(&root.endpoint());
        assert_eq!(negotiate_events(&mut observer, None)["ok"], true);
        let attached = observer.call(
            "session.attach",
            json!({
                "attach_role": "observer",
                "session_id": session_id,
                "observer_grant": grant,
            }),
        );
        assert_eq!(attached["ok"], true, "{attached}");
        let observer_attach = attached["result"]["attach_id"]
            .as_str()
            .expect("observer attach")
            .to_owned();

        // The controller disappears without a detach. The server releases its
        // lease and the surviving lease observes the detach revision.
        drop(creator);
        let event = recv_event(&mut observer);
        assert_eq!(event["kind"], "session_state", "{event}");
        assert_eq!(event["attach_id"], observer_attach, "{event}");
        let detached_revision = event["revision"].as_u64().expect("revision");

        let status = operator.call("session.status", json!({"session_id": session_id}));
        assert_eq!(status["ok"], true, "{status}");
        assert_eq!(status["result"]["session"]["attach_count"], 1, "{status}");
        assert_eq!(
            status["result"]["session"]["state_revision"], detached_revision,
            "the disconnect detach is the event's revision"
        );

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    #[test]
    fn a_connection_that_did_not_opt_in_receives_no_event_frame() {
        let root = TempRoot::new("events-optout");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let minted = operator.call("daemon.host_grant", json!({"action": "mint"}));
        let host_grant = minted["result"]["grant"]
            .as_str()
            .expect("host grant bearer")
            .to_owned();
        let mut creator = Client::connect(&root.endpoint());
        let handshake = creator.negotiate_with(None, Some(&host_grant));
        assert_eq!(handshake["ok"], true, "{handshake}");
        assert_eq!(
            handshake["result"]["capabilities"]["events"], true,
            "the cell serves delivery even for a client that does not opt in"
        );

        let created = creator.call(
            "session.create",
            json!({"kind": "terminal", "idempotency_key": "optout-create"}),
        );
        let session_id = created["result"]["session"]["session_id"]
            .as_str()
            .expect("session id")
            .to_owned();
        let claimed = creator.call(
            "session.attach",
            json!({
                "attach_role": "controller",
                "session_id": session_id,
                "expected_generation": 0,
                "control_token": created["result"]["control_token"],
                "idempotency_key": "optout-claim",
            }),
        );
        assert_eq!(claimed["ok"], true, "{claimed}");
        assert!(
            claimed["result"].get("backlog").is_none(),
            "no replay keys without the opt-in: {claimed}"
        );

        let _ = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": "optout-grant"}),
        );
        // The next frame must be the response to a normal request, not a
        // pushed event.
        let status = creator.call("session.status", json!({"session_id": session_id}));
        assert_eq!(status["ok"], true, "{status}");
        assert!(status.get("event").is_none(), "{status}");

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    #[test]
    fn an_event_opted_attach_replays_the_bounded_backlog() {
        let root = TempRoot::new("events-replay");
        let daemon = Daemon::start(&root);
        let capability = read_capability(&root);
        let mut operator = Client::connect(&root.endpoint());
        assert_eq!(operator.negotiate(Some(&capability))["ok"], true);

        let (mut creator, _) = event_consumer(&root, &mut operator, "replay");
        let (session_id, _, _, _) = create_and_claim(&mut creator, "replay");
        for index in 0..3 {
            let _ = creator.call(
                "session.grant",
                json!({
                    "session_id": session_id,
                    "idempotency_key": format!("replay-grant-{index}"),
                }),
            );
            let _ = recv_event(&mut creator);
        }

        let minted = creator.call(
            "session.grant",
            json!({"session_id": session_id, "idempotency_key": "replay-grant-observer"}),
        );
        let grant = minted["result"]["grant"]
            .as_str()
            .expect("observer grant")
            .to_owned();
        let _ = recv_event(&mut creator);

        let mut observer = Client::connect(&root.endpoint());
        assert_eq!(negotiate_events(&mut observer, None)["ok"], true);
        let attached = observer.call(
            "session.attach",
            json!({
                "attach_role": "observer",
                "session_id": session_id,
                "observer_grant": grant,
            }),
        );
        assert_eq!(attached["ok"], true, "{attached}");
        let backlog = attached["result"]["backlog"]
            .as_array()
            .expect("the replay is served");
        assert!(!backlog.is_empty(), "the retained ring is replayed");
        assert_eq!(attached["result"]["backlog_truncated"], false);
        assert_eq!(attached["result"]["earliest_revision"], 1);
        let revisions: Vec<u64> = backlog
            .iter()
            .map(|event| event["revision"].as_u64().expect("revision"))
            .collect();
        assert!(
            revisions.windows(2).all(|pair| pair[0] < pair[1]),
            "the replay is revision-ordered: {revisions:?}"
        );
        assert!(
            backlog
                .iter()
                .all(|event| event["attach_id"] == observer_attach(&attached)),
            "every replayed copy is routed to the new lease"
        );

        assert_eq!(stop_cleanly(&root, daemon).code(), Some(0));
    }

    fn observer_attach(attached: &Value) -> String {
        attached["result"]["attach_id"]
            .as_str()
            .expect("observer attach")
            .to_owned()
    }
}
