//! Socket-level automation proof over the devtools socket (CTX-0188).
//!
//! CTX-0183 harness pattern: publish known runtime snapshots into the live
//! stores, serve them over a real Unix socket with `Dispatcher::with_defaults`
//! (the same code path the `bitty-app` servo drives), and assert from the
//! client side — no seat, no screenshots, no pixel polling.
//!
//! Covered here (Amendment A1 candidate):
//! - `synthesizeInput` receipt over the socket plus `[synthetic]` markers
//!   visible to `getInputRing` (harness/user distinguishable).
//! - `captureFrame/semantic` redaction over the socket (seeded secret and
//!   clipboard bytes never appear; `untrusted-observation` label present).
//! - Unscoped automation denied machine-checkably (`ScopeDenied`, no hang,
//!   no partial state).
//!
//! Headless and Unix-only. Automation shares the process-global
//! introspection stores, so every test holds the file-local serial guard
//! (CTX-0179 pattern; std-only, no extra dev-dependency) and clears
//! automation plus introspection before and after.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitty_ipc::devtools::{
    AutomationFamily, Dispatcher, ServeContext, ServerInfo, clear_automation_for_tests,
    clear_introspection_for_tests, issue_automation_bearer, prepare_socket_dir, publish_grid_text,
    serve_connection, transport_attested_peer,
};
use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
use bitty_ipc::limits::RateLimiter;
use bitty_ipc::scope::{Scope, ScopeSet};

/// Serial guard for the process-global automation + introspection stores.
fn automation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn hold_automation_lock() -> std::sync::MutexGuard<'static, ()> {
    automation_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn temp_socket_path(tag: &str) -> String {
    let pid = std::process::id();
    let path = format!("/tmp/bta{pid}{tag}/s.sock");
    assert!(
        path.len() < 100,
        "socket path must fit macOS SUN_LEN: {path} ({} bytes)",
        path.len()
    );
    path
}

fn read_framed(stream: &mut UnixStream) -> String {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let len = u32::from_be_bytes(header) as usize;
    assert!(len <= MAX_FRAME_BYTES, "response exceeds frame bound");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

/// Minimal automation client: one JSON-RPC envelope per call.
struct AutomationClient {
    stream: UnixStream,
    next_id: u64,
}

impl AutomationClient {
    fn connect(socket_path: &str) -> Self {
        let stream = UnixStream::connect(socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self { stream, next_id: 1 }
    }

    fn call(&mut self, method: &str, params: &str) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params},\"version\":\"1.0\"}}"
        );
        let wire = encode_frame(envelope.as_bytes()).unwrap();
        self.stream.write_all(&wire).unwrap();
        self.stream.flush().unwrap();
        let response = read_framed(&mut self.stream);
        assert!(
            response.contains(&format!("\"id\":{id}")),
            "response lost correlation id {id}: {response}"
        );
        response
    }
}

fn synth_scopes() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::DebugControl);
    set.insert(Scope::TerminalInput);
    set
}

fn capture_scopes() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::DebugTrace);
    set.insert(Scope::TerminalInspect);
    set
}

fn spawn_automation_server(
    socket_path: String,
    granted: ScopeSet,
    session: &str,
) -> std::thread::JoinHandle<()> {
    let session = session.to_string();
    std::thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let verified = transport_attested_peer(unit_owner_uid(&socket_path));
        let dispatcher = Dispatcher::with_defaults();
        let server = ServerInfo::new("automation-proof".to_string(), socket_path.clone(), 80, 24);
        let context = ServeContext::with_granted_session(&server, granted, &session);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        };
        let stats = serve_connection(
            &mut stream,
            verified,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
        .unwrap();
        assert!(stats.requests >= 1, "expected test requests");
        assert_eq!(stats.responses, stats.requests);
    })
}

#[cfg(unix)]
fn unit_owner_uid(path: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
}

#[test]
fn automation_synthesize_receipt_and_markers_over_socket() {
    let _guard = hold_automation_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();

    // The servo stamps uptime near zero at spawn; issue the bearer at zero
    // so the request-time clock (small monotonic delta) stays within TTL.
    let bearer =
        issue_automation_bearer("proof-synth", "t:1", AutomationFamily::Synthesize, 0).unwrap();
    let socket_path = temp_socket_path("as");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_automation_server(socket_path.clone(), synth_scopes(), "proof-synth");
    std::thread::sleep(Duration::from_millis(100));
    let mut client = AutomationClient::connect(&socket_path);

    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{bearer}\",\"originLabel\":\"proof-harness\",\"events\":[{{\"type\":\"key\",\"key\":\"Enter\"}},{{\"type\":\"mouse\",\"button\":\"Left\",\"action\":\"click\",\"col\":10,\"row\":5}}]}}"
    );
    let receipt = client.call("bitty.debug/synthesizeInput", &params);
    assert!(
        receipt.contains("\"accepted\":2"),
        "unexpected receipt: {receipt}"
    );
    assert!(
        receipt.contains("\"synthetic\":true"),
        "unexpected receipt: {receipt}"
    );

    // Markers are observable to the read-only ring (no extra scope needed
    // for the assert path; the ring itself is the trace surface).
    let ring = client.call("bitty.debug/getInputRing", "{\"limit\":10}");
    assert!(
        ring.contains("[synthetic:proof-harness]"),
        "missing synthetic marker: {ring}"
    );

    drop(client);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn automation_capture_semantic_redacts_over_socket() {
    let _guard = hold_automation_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_grid_text(
        vec![
            "$ echo hello".to_string(),
            "hello".to_string(),
            "DB_PASSWORD=hunter2".to_string(),
        ],
        1,
        5,
        true,
        21,
        80,
        24,
    );

    let bearer = issue_automation_bearer("proof-cap", "t:1", AutomationFamily::Capture, 0).unwrap();
    let socket_path = temp_socket_path("ac");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_automation_server(socket_path.clone(), capture_scopes(), "proof-cap");
    std::thread::sleep(Duration::from_millis(100));
    let mut client = AutomationClient::connect(&socket_path);

    let params =
        format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{bearer}\",\"format\":\"semantic\"}}");
    let frame = client.call("bitty.debug/captureFrame", &params);
    assert!(
        frame.contains("\"snapshot\":\"frame\""),
        "unexpected frame: {frame}"
    );
    assert!(
        frame.contains("\"trust\":\"untrusted-observation\""),
        "unexpected frame: {frame}"
    );
    assert!(frame.contains("hello"), "unexpected frame: {frame}");
    assert!(
        !frame.contains("hunter2"),
        "secret leaked over socket: {frame}"
    );
    assert!(frame.contains("[redacted]"), "unexpected frame: {frame}");

    drop(client);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn automation_unscoped_calls_denied_machine_checkably_over_socket() {
    let _guard = hold_automation_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();

    let socket_path = temp_socket_path("ad");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_automation_server(socket_path.clone(), ScopeSet::new(), "proof-deny");
    std::thread::sleep(Duration::from_millis(100));
    let mut client = AutomationClient::connect(&socket_path);

    let denied = client.call(
        "bitty.debug/synthesizeInput",
        r#"{"terminalId":"t:1","bearer":"bogus","originLabel":"h","events":[{"type":"key","key":"a"}]}"#,
    );
    assert!(
        denied.contains("ScopeDenied"),
        "expected ScopeDenied, got: {denied}"
    );

    let denied = client.call(
        "bitty.debug/captureFrame",
        r#"{"terminalId":"t:1","bearer":"bogus","format":"semantic"}"#,
    );
    assert!(
        denied.contains("ScopeDenied"),
        "expected ScopeDenied, got: {denied}"
    );

    drop(client);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}
