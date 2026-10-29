//! The real-service driver: starts the built `bittyd` binary and speaks the
//! real Unix endpoint.
//!
//! Linux is the only servable cell in this slice. Other cells record a typed
//! `cell-not-servable` skip instead of compiling a silent pass.

#![cfg(target_os = "linux")]

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::harness::corpus::{DriverKind, Vector};
use crate::harness::evidence;
use crate::harness::runner::{
    Driver, ErrorReply, ExitObservation, Outcome, Reply, RequestRecord, ServiceFacts, Vars,
    cell_vars,
};

const BITTYD: &str = env!("CARGO_BIN_EXE_bittyd");
const START_BUDGET: Duration = Duration::from_secs(10);
const STOP_BUDGET: Duration = Duration::from_secs(10);
const EOF_BUDGET: Duration = Duration::from_secs(2);
const IO_BUDGET: Duration = Duration::from_secs(10);

static NEXT_ROOT: AtomicU32 = AtomicU32::new(0);

/// The real-service driver.
pub struct RealDriver {
    root: Option<Root>,
    daemon: Option<Daemon>,
    clients: std::collections::BTreeMap<String, Client>,
    facts: ServiceFacts,
    requests: Vec<RequestRecord>,
    operator_capability: String,
    host_grant: Option<String>,
    needs_host_grant: bool,
    next_id: u64,
}

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
        let path = std::env::temp_dir().join(format!(
            "bittyd-conformance-{label}-{}-{suffix}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create conformance root");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod conformance root");
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

    fn audit_dir(&self) -> PathBuf {
        self.state().join("bitty").join("audit")
    }

    /// The owner-private orphan journal directory the daemon opens.
    fn orphans(&self) -> PathBuf {
        self.runtime().join("bitty").join("orphans")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    fn start(root: &Root) -> Result<Self, String> {
        std::fs::create_dir_all(root.runtime()).map_err(|_| "runtime dir".to_owned())?;
        std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "runtime mode".to_owned())?;
        std::fs::create_dir_all(root.state()).map_err(|_| "state dir".to_owned())?;
        let child = Command::new(BITTYD)
            .arg("start")
            .env("XDG_RUNTIME_DIR", root.runtime())
            .env("XDG_STATE_HOME", root.state())
            .env_remove("BITTYD_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| "cannot spawn bittyd".to_owned())?;
        let mut daemon = Self { child };
        let deadline = Instant::now() + START_BUDGET;
        while Instant::now() < deadline {
            // Readiness is an accepted connection plus the operator
            // capability: the socket is bound before the capability is
            // minted, and the driver reads it immediately after `start`.
            if root.capability().exists() && UnixStream::connect(root.endpoint()).is_ok() {
                return Ok(daemon);
            }
            if daemon.child.try_wait().ok().flatten().is_some() {
                return Err("bittyd exited before binding".to_owned());
            }
            // Bounded backoff for process startup; the deadline is the bound.
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon.kill();
        Err("bittyd did not bind within the startup budget".to_owned())
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_reaped(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => {
                    // Bounded backoff for a short-lived CLI; the deadline is
                    // the hard bound and the child is killed past it.
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    }

    fn wait_within(&mut self, budget: Duration) -> Option<i32> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    // Bounded backoff for a short-lived CLI; the deadline is
                    // the hard bound and the child is killed past it.
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return None,
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill();
        }
    }
}

struct Client {
    stream: UnixStream,
}

impl Client {
    fn connect(endpoint: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(endpoint).map_err(|_| "cannot connect".to_owned())?;
        bitty_daemon::service::frame::enable_credentials(&stream)
            .map_err(|_| "per-frame credentials".to_owned())?;
        stream
            .set_read_timeout(Some(IO_BUDGET))
            .map_err(|_| "read timeout".to_owned())?;
        stream
            .set_write_timeout(Some(IO_BUDGET))
            .map_err(|_| "write timeout".to_owned())?;
        Ok(Self { stream })
    }

    fn send(&mut self, id: &str, method: &str, params: &Value) -> Result<(), String> {
        let request = json!({"v": 1, "id": id, "method": method, "params": params});
        write_frame(&mut self.stream, &request).map_err(|_| "write frame".to_owned())
    }

    fn send_raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .map_err(|_| "write raw frame".to_owned())
    }

    fn recv(&mut self) -> Result<Outcome, String> {
        let mut header = [0u8; 4];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if is_eof(&error) => return Ok(Outcome::Closed),
            Err(_) => return Err("read response header".to_owned()),
        }
        let length = u32::from_be_bytes(header) as usize;
        let mut payload = vec![0u8; length];
        match self.stream.read_exact(&mut payload) {
            Ok(()) => {}
            Err(error) if is_eof(&error) => return Ok(Outcome::Closed),
            Err(_) => return Err("read response body".to_owned()),
        }
        let value: Value =
            serde_json::from_slice(&payload).map_err(|_| "parse response".to_owned())?;
        Ok(Outcome::Reply(reply_from(value)))
    }

    fn eof(&mut self, budget: Duration) -> bool {
        let _ = self.stream.set_read_timeout(Some(budget));
        let mut byte = [0u8; 1];
        match self.stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => is_eof(&error),
            Ok(_) => false,
        }
    }

    fn drop_without_reading(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn signal(pid: u32, name: &str) -> Result<(), String> {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .map_err(|_| "cannot signal the daemon".to_owned())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "signal refused".to_owned())
}

fn is_eof(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
    )
}

fn write_frame(stream: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    stream.write_all(&frame_bytes(value)?)
}

fn frame_bytes(value: &Value) -> std::io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(value)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn reply_from(value: Value) -> Reply {
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    let error = value.get("error").map(|error| ErrorReply {
        class: error
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("Internal")
            .to_owned(),
        code: error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("Internal")
            .to_owned(),
        counts: error.get("counts").and_then(|counts| {
            Some((
                counts.get("actual")?.as_u64()?,
                counts.get("limit")?.as_u64()?,
            ))
        }),
    });
    Reply { ok, result, error }
}

fn transport_failure(code: &str) -> Outcome {
    Outcome::Reply(Reply {
        ok: false,
        result: Value::Null,
        error: Some(ErrorReply {
            class: "Harness".to_owned(),
            code: code.to_owned(),
            counts: None,
        }),
    })
}

impl RealDriver {
    pub fn new() -> Self {
        Self {
            root: None,
            daemon: None,
            clients: std::collections::BTreeMap::new(),
            facts: ServiceFacts::default(),
            requests: Vec::new(),
            operator_capability: String::new(),
            host_grant: None,
            needs_host_grant: false,
            next_id: 1,
        }
    }

    /// Reads the per-boot operator capability once the daemon has written it.
    ///
    /// The endpoint socket appears before the capability file, and the file is
    /// created empty before the 64-hex token is written, so neither the socket
    /// nor a first successful read is a readiness signal. Retry until the exact
    /// canonical value is present, with a bounded startup deadline; anything
    /// else after the deadline stays a setup error.
    fn read_capability(root: &Root) -> Result<String, String> {
        const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const READY_STEP: std::time::Duration = std::time::Duration::from_millis(10);
        fn canonical(text: &str) -> bool {
            let trimmed = text.trim_end_matches('\n');
            trimmed.len() == 64 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
        }
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        loop {
            match std::fs::read_to_string(root.capability()) {
                Ok(text) if canonical(&text) => {
                    return Ok(text.trim_end_matches('\n').to_owned());
                }
                Ok(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(READY_STEP);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(READY_STEP);
                }
                _ => return Err("cannot read the operator capability".to_owned()),
            }
        }
    }

    fn start(
        &mut self,
        label: &str,
        host_grant: bool,
        orphan_record: Option<&str>,
    ) -> Result<(), String> {
        self.stop();
        let root = Root::new(label);
        seed_orphan_record(&root, orphan_record)?;
        let daemon = Daemon::start(&root)?;
        self.operator_capability = Self::read_capability(&root)?;
        self.needs_host_grant = host_grant;
        self.host_grant = None;
        self.root = Some(root);
        self.daemon = Some(daemon);
        self.clients.clear();
        self.requests.clear();
        self.next_id = 1;
        self.facts = self.probe()?;
        if self.needs_host_grant {
            self.host_grant = Some(self.mint_host_grant()?);
        }
        Ok(())
    }

    fn probe(&mut self) -> Result<ServiceFacts, String> {
        let endpoint = self
            .root
            .as_ref()
            .map(Root::endpoint)
            .ok_or_else(|| "no root".to_owned())?;
        let mut client = Client::connect(&endpoint)?;
        let id = self.next_request_id();
        client.send(
            &id,
            "daemon.negotiate",
            &json!({"client_version": {"major": 1, "minor": 0}}),
        )?;
        let outcome = client.recv()?;
        let Outcome::Reply(reply) = outcome else {
            return Err("probe handshake closed".to_owned());
        };
        if !reply.ok {
            return Err("probe handshake refused".to_owned());
        }
        let instance_id = reply
            .result
            .get("daemon_instance_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let capabilities = reply
            .result
            .get("capabilities")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(name, value)| Some((name.clone(), value.as_bool()?)))
                    .collect()
            })
            .unwrap_or_default();
        let mut limits = std::collections::BTreeMap::new();
        if let Some(items) = reply.result.get("limits").and_then(Value::as_array) {
            for item in items {
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(effective) = item.get("effective").and_then(Value::as_u64) else {
                    continue;
                };
                limits.insert(name.to_owned(), effective);
            }
        }
        Ok(ServiceFacts {
            instance_id,
            cell: evidence::cell(),
            version: bitty_daemon::CRATE_VERSION.to_owned(),
            capabilities,
            limits,
        })
    }

    fn mint_host_grant(&mut self) -> Result<String, String> {
        let endpoint = self
            .root
            .as_ref()
            .map(Root::endpoint)
            .ok_or_else(|| "no root".to_owned())?;
        let mut client = Client::connect(&endpoint)?;
        let id = self.next_request_id();
        client.send(
            &id,
            "daemon.negotiate",
            &json!({
                "client_version": {"major": 1, "minor": 0},
                "operator_capability": self.operator_capability,
            }),
        )?;
        let Outcome::Reply(reply) = client.recv()? else {
            return Err("operator handshake closed".to_owned());
        };
        if !reply.ok {
            return Err("operator handshake refused".to_owned());
        }
        let id = self.next_request_id();
        client.send(&id, "daemon.host_grant", &json!({"action": "mint"}))?;
        let Outcome::Reply(reply) = client.recv()? else {
            return Err("host grant closed".to_owned());
        };
        reply
            .result
            .get("grant")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "host grant refused".to_owned())
    }

    fn next_request_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    fn endpoint(&self) -> Result<PathBuf, String> {
        self.root
            .as_ref()
            .map(Root::endpoint)
            .ok_or_else(|| "no root".to_owned())
    }

    fn stop(&mut self) {
        if let (Some(root), Some(daemon)) = (self.root.as_ref(), self.daemon.as_mut()) {
            let capability = std::fs::read_to_string(root.capability()).ok();
            if let Some(capability) = capability {
                if let Ok(mut client) = Client::connect(&root.endpoint()) {
                    let request = json!({
                        "v": 1,
                        "id": "stop",
                        "method": "daemon.negotiate",
                        "params": {
                            "client_version": {"major": 1, "minor": 0},
                            "operator_capability": capability.trim_end_matches('\n'),
                        },
                    });
                    if write_frame(&mut client.stream, &request).is_ok()
                        && matches!(client.recv(), Ok(Outcome::Reply(reply)) if reply.ok)
                    {
                        let stop = json!({
                            "v": 1,
                            "id": "stop-2",
                            "method": "daemon.stop",
                            "params": {"mode": "force"},
                        });
                        let _ = write_frame(&mut client.stream, &stop);
                        let _ = client.recv();
                    }
                }
            }
            if daemon.wait_within(STOP_BUDGET).is_none() {
                daemon.kill();
            }
        }
        self.daemon = None;
        self.root = None;
        self.clients.clear();
    }

    fn client(&mut self, name: &str) -> &mut Client {
        if !self.clients.contains_key(name) {
            let endpoint = self.endpoint().expect("root exists");
            let client = Client::connect(&endpoint).expect("connect");
            self.clients.insert(name.to_owned(), client);
        }
        self.clients.get_mut(name).expect("just inserted")
    }
}

impl Driver for RealDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Real
    }

    fn prepare(&mut self, vector: &Vector) -> Result<Vars, String> {
        self.start(
            &vector.id.to_lowercase(),
            vector.host_grant,
            vector.orphan_record.as_deref(),
        )?;
        let mut vars = Vars::new();
        vars.insert(
            "operator_capability".to_owned(),
            Value::String(self.operator_capability.clone()),
        );
        if let Some(grant) = &self.host_grant {
            vars.insert("host_grant".to_owned(), Value::String(grant.clone()));
        }
        cell_vars(&mut vars, bitty_daemon::worker::WORKER_SESSIONS_SERVED);
        Ok(vars)
    }

    fn call(&mut self, connection: &str, method: &str, params: Value) -> Outcome {
        let id = self.next_request_id();
        self.requests.push(RequestRecord {
            method: method.to_owned(),
            params: params.clone(),
        });
        let client = self.client(connection);
        if client.send(&id, method, &params).is_err() {
            return transport_failure("WriteFailed");
        }
        match client.recv() {
            Ok(outcome) => outcome,
            Err(_) => transport_failure("ReadFailed"),
        }
    }

    fn call_dropping(&mut self, connection: &str, method: &str, params: Value) -> Outcome {
        let id = self.next_request_id();
        self.requests.push(RequestRecord {
            method: method.to_owned(),
            params: params.clone(),
        });
        let client = self.client(connection);
        let _ = client.send(&id, method, &params);
        client.drop_without_reading();
        self.clients.remove(connection);
        Outcome::Closed
    }

    fn expect_eof(&mut self, connection: &str) -> bool {
        self.clients
            .get_mut(connection)
            .is_some_and(|client| client.eof(EOF_BUDGET))
    }

    fn peer_exit(&mut self, _connection: &str, method: &str, params: &Value) -> Result<(), String> {
        let endpoint = self.endpoint()?;
        let capability = self.operator_capability.clone();
        let pid = self
            .daemon
            .as_ref()
            .map(Daemon::pid)
            .ok_or_else(|| "no daemon".to_owned())?;
        let helper = std::env::current_exe().map_err(|_| "no test binary".to_owned())?;
        let mut child = Command::new(helper)
            .args(["--exact", "peer_exit_helper", "--ignored", "--nocapture"])
            .env("BITTY_CONFORMANCE_HELPER", "peer-exit")
            .env("BITTY_CONFORMANCE_SOCKET", &endpoint)
            .env("BITTY_CONFORMANCE_CAPABILITY", &capability)
            .env("BITTY_CONFORMANCE_METHOD", method)
            .env(
                "BITTY_CONFORMANCE_PARAMS",
                serde_json::to_string(params).map_err(|_| "bad params".to_owned())?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "cannot spawn the peer helper".to_owned())?;
        {
            use std::io::BufRead;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| "no helper stdout".to_owned())?;
            let mut reader = std::io::BufReader::new(stdout);
            let mut ready = false;
            for _ in 0..32 {
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .map_err(|_| "helper did not become ready".to_owned())?
                    == 0
                {
                    break;
                }
                if line.contains("ready") {
                    ready = true;
                    break;
                }
            }
            if !ready {
                let _ = child.kill();
                return Err("helper did not become ready".to_owned());
            }
        }
        signal(pid, "STOP")?;
        let sent = (|| -> Result<(), String> {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| "no helper stdin".to_owned())?;
            stdin
                .write_all(b"go\n")
                .map_err(|_| "cannot release the helper".to_owned())?;
            let status = child.wait().map_err(|_| "helper did not exit".to_owned())?;
            if !status.success() {
                return Err("helper failed".to_owned());
            }
            Ok(())
        })();
        let resumed = signal(pid, "CONT");
        sent?;
        resumed?;
        // The service applies the refusal on its next loop iteration; wait for
        // the attributable record before the corpus asserts it.
        let deadline = Instant::now() + EOF_BUDGET;
        while Instant::now() < deadline {
            if self.audit_text().contains("unauthenticated") {
                return Ok(());
            }
            // The refusal is applied on the service loop's next iteration;
            // the deadline bounds the settlement, there is no other signal.
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("no peer-exit refusal record".to_owned())
    }

    fn queue_pressure(
        &mut self,
        _connection: &str,
        _method: &str,
        _count: usize,
        _params: &Value,
    ) -> Outcome {
        transport_failure("RequiresInjectedOutboundCap")
    }

    fn refusal_burst(
        &mut self,
        connection: &str,
        method: &str,
        count: usize,
        code: &str,
    ) -> Result<(), String> {
        for _ in 0..count {
            let outcome = self.call(connection, method, Value::Null);
            let Outcome::Reply(reply) = outcome else {
                return Err("the connection closed during the burst".to_owned());
            };
            let Some(error) = reply.error else {
                return Err("the burst produced a success".to_owned());
            };
            if error.code != code {
                return Err(format!(
                    "the burst returned {}, expected {code}",
                    error.code
                ));
            }
        }
        Ok(())
    }

    fn oversized_frame(&mut self, connection: &str, declared: u32) -> Outcome {
        let id = self.next_request_id();
        self.requests.push(RequestRecord {
            method: "frame".to_owned(),
            params: json!({"declared_len": declared, "id": id}),
        });
        let client = self.client(connection);
        if client.send_raw(&declared.to_be_bytes()).is_err() {
            return transport_failure("WriteFailed");
        }
        if client.eof(EOF_BUDGET) {
            Outcome::Closed
        } else {
            transport_failure("NotClosed")
        }
    }

    fn partial_frame(&mut self, connection: &str, declared: u32, body: usize) -> Outcome {
        self.requests.push(RequestRecord {
            method: "partial_frame".to_owned(),
            params: json!({"declared_len": declared, "body_bytes": body}),
        });
        let client = self.client(connection);
        let mut bytes = Vec::with_capacity(4 + body);
        bytes.extend_from_slice(&declared.to_be_bytes());
        bytes.extend(std::iter::repeat_n(b'x', body));
        if client.send_raw(&bytes).is_err() {
            return transport_failure("WriteFailed");
        }
        if client.eof(EOF_BUDGET) {
            Outcome::Closed
        } else {
            transport_failure("NotClosed")
        }
    }

    fn raw_request(&mut self, connection: &str, request: Value, expect_eof: bool) -> Outcome {
        self.requests.push(RequestRecord {
            method: request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("raw")
                .to_owned(),
            params: request.get("params").cloned().unwrap_or(Value::Null),
        });
        let client = self.client(connection);
        let Ok(bytes) = frame_bytes(&request) else {
            return transport_failure("EncodeFailed");
        };
        if client.send_raw(&bytes).is_err() {
            return transport_failure("WriteFailed");
        }
        let outcome = match client.recv() {
            Ok(outcome) => outcome,
            Err(_) => return transport_failure("ReadFailed"),
        };
        if expect_eof && !client.eof(EOF_BUDGET) {
            return transport_failure("NotClosed");
        }
        outcome
    }

    fn pipelined_negotiate(
        &mut self,
        connection: &str,
        second: &Value,
        partial_body: Option<usize>,
    ) -> Outcome {
        let negotiate_id = self.next_request_id();
        let second_id = self.next_request_id();
        let negotiation = json!({
            "v": 1,
            "id": negotiate_id,
            "method": "daemon.negotiate",
            "params": {"client_version": {"major": 1, "minor": 0}},
        });
        let Value::Object(mut request) = second.clone() else {
            return transport_failure("BadRequest");
        };
        request.insert("v".to_owned(), json!(1));
        request.insert("id".to_owned(), json!(second_id));
        let request = Value::Object(request);
        self.requests.push(RequestRecord {
            method: "daemon.negotiate".to_owned(),
            params: json!({"client_version": {"major": 1, "minor": 0}}),
        });
        self.requests.push(RequestRecord {
            method: second
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("pipelined")
                .to_owned(),
            params: second.get("params").cloned().unwrap_or(Value::Null),
        });
        let (Ok(head), Ok(tail)) = (frame_bytes(&negotiation), frame_bytes(&request)) else {
            return transport_failure("EncodeFailed");
        };
        let split = partial_body
            .map_or(tail.len(), |body| 4 + body)
            .min(tail.len());
        let mut batch = head;
        batch.extend_from_slice(&tail[..split]);
        let client = self.client(connection);
        if client.send_raw(&batch).is_err() {
            return transport_failure("WriteFailed");
        }
        // A partial tail is only completed after the first write, reproducing
        // the wire evidence: a frame split across two writes must not resume
        // as a fresh request. A server that already refused and closed rejects
        // the tail; that is the expected outcome, not a harness failure.
        if split < tail.len() {
            let _ = client.send_raw(&tail[split..]);
        }
        let handshake = match client.recv() {
            Ok(outcome) => outcome,
            Err(_) => return transport_failure("ReadFailed"),
        };
        if !matches!(&handshake, Outcome::Reply(reply) if reply.ok) {
            return transport_failure("HandshakeFailed");
        }
        let refusal = match client.recv() {
            Ok(outcome) => outcome,
            Err(_) => return transport_failure("ReadFailed"),
        };
        if !client.eof(EOF_BUDGET) {
            return transport_failure("NotClosed");
        }
        refusal
    }

    fn restart(&mut self) -> Result<Vars, String> {
        let needs_host_grant = self.needs_host_grant;
        self.stop();
        self.start("restart", needs_host_grant, None)?;
        let mut vars = Vars::new();
        vars.insert(
            "operator_capability".to_owned(),
            Value::String(self.operator_capability.clone()),
        );
        if let Some(grant) = &self.host_grant {
            vars.insert("host_grant".to_owned(), Value::String(grant.clone()));
        }
        cell_vars(&mut vars, bitty_daemon::worker::WORKER_SESSIONS_SERVED);
        Ok(vars)
    }

    fn crash(&mut self) -> Result<(), String> {
        let mut daemon = self
            .daemon
            .take()
            .ok_or_else(|| "no daemon to crash".to_owned())?;
        let pid = daemon.pid();
        signal(pid, "KILL")?;
        if !daemon.wait_reaped(STOP_BUDGET) {
            return Err("the daemon did not exit after SIGKILL".to_owned());
        }
        self.clients.clear();
        self.root = None;
        Ok(())
    }

    fn advance_clock(&mut self, _millis: u64) {
        // The served loop uses a process monotonic clock; no injection point
        // exists. Vectors that need one are fake-only and record a typed skip.
    }

    fn audit_text(&self) -> String {
        let Some(root) = &self.root else {
            return String::new();
        };
        let mut text = String::new();
        let Ok(entries) = std::fs::read_dir(root.audit_dir()) else {
            return text;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        for path in paths {
            if let Ok(chunk) = std::fs::read_to_string(path) {
                text.push_str(&chunk);
            }
        }
        text
    }

    fn file_mode(&self, role: &str) -> Option<u32> {
        let root = self.root.as_ref()?;
        let path = match role {
            "endpoint" => root.endpoint(),
            "audit_log" => root.audit_dir().join("audit.log"),
            "audit_dir" => root.audit_dir(),
            _ => return None,
        };
        std::fs::symlink_metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
    }

    fn no_tcp_listener(&self) -> Result<(), String> {
        let pid = self
            .daemon
            .as_ref()
            .map(Daemon::pid)
            .ok_or_else(|| "no daemon".to_owned())?;
        let listeners = tcp_listeners(pid);
        if listeners.is_empty() {
            Ok(())
        } else {
            Err("a TCP listener is open".to_owned())
        }
    }

    fn daemon_absent(&mut self) -> Result<ExitObservation, String> {
        let root = Root::new("absent");
        std::fs::create_dir_all(root.runtime()).map_err(|_| "runtime dir".to_owned())?;
        std::fs::set_permissions(root.runtime(), std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "runtime mode".to_owned())?;
        std::fs::create_dir_all(root.state()).map_err(|_| "state dir".to_owned())?;
        let mut child = Command::new(BITTYD)
            .env("XDG_RUNTIME_DIR", root.runtime())
            .env("XDG_STATE_HOME", root.state())
            .env_remove("BITTYD_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| "cannot run bittyd".to_owned())?;
        let deadline = Instant::now() + START_BUDGET;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    // Bounded backoff for a short-lived CLI; the deadline is
                    // the hard bound and the child is killed past it.
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("plain bittyd exceeded the invocation budget".to_owned());
                }
                Err(_) => return Err("cannot observe plain bittyd".to_owned()),
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|_| "cannot read plain bittyd output".to_owned())?;
        if root.endpoint().exists() || root.capability().exists() {
            return Err("plain bittyd created a runtime file".to_owned());
        }
        Ok(ExitObservation {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.requests.clone()
    }

    fn facts(&self) -> ServiceFacts {
        self.facts.clone()
    }

    fn finish(&mut self) {
        self.stop();
    }
}

impl Drop for RealDriver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Session id the seeded quarantined record uses.
///
/// The vector names the same literal in `record_id`; the value is a canonical
/// 32-hex id so it round-trips through the journal record path.
const SEEDED_RECORD_ID: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

/// Seed one crash-left journal record before the daemon starts.
///
/// The record is a same-boot, reserved, subreaper record: the daemon cannot
/// prove a pre-spawn identity for the fallback, so the startup scan quarantines
/// it as `unverified` (contract `D-06`), retains it, and never signals it. A
/// malformed placeholder would expire immediately once the recorded origin is
/// older than the retention window, so the seed is a real encoded record. The
/// directory and file use the owner-private modes the journal enforces, so the
/// seed survives the startup scan rather than failing the open.
fn seed_orphan_record(root: &Root, seed: Option<&str>) -> Result<(), String> {
    let Some("quarantined") = seed else {
        return Ok(());
    };
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| "cannot read the boot id".to_owned())?
        .trim()
        .to_owned();
    let directory = root.orphans();
    std::fs::create_dir_all(&directory)
        .map_err(|_| "cannot create the orphan journal".to_owned())?;
    // The daemon refuses a runtime directory that is not owner-private, so
    // every directory this seed creates before startup gets 0700.
    for path in [
        root.runtime(),
        root.runtime().join("bitty"),
        directory.clone(),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "cannot chmod the orphan journal".to_owned())?;
    }
    let record = format!(
        "bittyd-orphan 2\nsession_id {SEEDED_RECORD_ID}\ngeneration 0\nclass reserved\nboot_id {boot_id}\nreserved_at_ms 1\nvalidated_at_ms -\ncontainment subreaper\npid -\nstart_time -\nprocess_group -\nuid -\n"
    );
    let path = directory.join(format!("{SEEDED_RECORD_ID}.orphan"));
    std::fs::write(&path, record).map_err(|_| "cannot seed the quarantined record".to_owned())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot chmod the seeded record".to_owned())?;
    Ok(())
}

/// Socket inodes the process holds, plus any of them in a TCP LISTEN row.
fn tcp_listeners(pid: u32) -> Vec<String> {
    let mut owned = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
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
