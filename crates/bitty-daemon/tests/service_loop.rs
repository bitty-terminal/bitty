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
//! - the daemon never opens a TCP listener (checked through `/proc/<pid>/fd`
//!   against `/proc/net/tcp` and `/proc/net/tcp6`);
//! - `daemon.negotiate` authenticates the operator capability and
//!   `daemon.stop force` shuts the loop down cleanly, removing the socket.
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
        std::fs::create_dir_all(root.runtime()).expect("runtime dir");
        std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod runtime");
        std::fs::create_dir_all(root.state()).expect("state dir");
        let child = Command::new(BITTYD)
            .arg("start")
            .env("XDG_RUNTIME_DIR", root.runtime())
            .env("XDG_STATE_HOME", root.state())
            .env_remove("BITTYD_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bittyd start");
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
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
        let request = json!({"v": 1, "id": id, "method": method, "params": params});
        let payload = serde_json::to_vec(&request).expect("encode request");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        self.stream.write_all(&frame).expect("write frame");
        self.recv()
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
        let mut params = json!({"client_version": {"major": 1, "minor": 0}});
        if let Some(capability) = operator_capability {
            params["operator_capability"] = Value::String(capability.to_owned());
        }
        self.call("daemon.negotiate", params)
    }
}

fn read_capability(root: &TempRoot) -> String {
    let raw = std::fs::read_to_string(root.capability()).expect("capability file");
    raw.trim_end_matches('\n').to_owned()
}

/// Socket inodes the process holds, plus any of them in a TCP LISTEN row.
fn tcp_listeners_of_process(pid: u32) -> Vec<String> {
    let mut owned = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).expect("process fds") {
        let entry = entry.expect("fd entry");
        let target = std::fs::read_link(entry.path()).expect("fd target");
        if let Some(text) = target.to_str() {
            if let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                owned.push(inode.to_owned());
            }
        }
    }
    let mut listeners = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            if fields[3] == "0A" && owned.iter().any(|inode| inode == fields[9]) {
                listeners.push(format!("{table}: {}", fields[1]));
            }
        }
    }
    listeners
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
    assert!(
        tcp_listeners_of_process(daemon.pid()).is_empty(),
        "bittyd must not open a TCP listener: {:?}",
        tcp_listeners_of_process(daemon.pid())
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
    assert!(scopes.iter().any(|scope| scope == "session.inspect"));
    assert!(!scopes.iter().any(|scope| scope == "session.host"));
    assert!(!scopes.iter().any(|scope| scope == "daemon.control"));
    assert_eq!(result["capabilities"]["worker_sessions"], false);
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
    // The connection closed; the next read sees EOF.
    let mut header = [0u8; 4];
    assert_eq!(client.stream.read(&mut header).unwrap_or(0), 0);

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
