//! Socket-level `frameHash` proof over the devtools socket (CTX-0244).
//!
//! Same harness pattern as `devtools_automation.rs` (CTX-0188): publish a
//! known synthetic RGBA frame plus grid text into the live stores, serve
//! over a real Unix socket with `Dispatcher::with_defaults` through the
//! production-equivalent accept boundary (`transport_attested_peer` +
//! `attest_local_peer`), and assert from the client side.
//!
//! Covered here:
//! - `frameHash` digest over the socket equals the local
//!   `frame_digest_hex` computation (lossless-equality proof, no pixels).
//! - Unscoped calls denied machine-checkably (`ScopeDenied`).
//! - The response carries zero pixel bytes (marker spot-check).
//! - Live-grant ceremony stays local-manual: the ignored test below never
//!   runs in CI (no human consent available there).
//!
//! Headless and Unix-only. All frame content is synthetic fixture bytes —
//! never secrets (P0-AC-026 harness rule). Every test holds the file-local
//! serial guard (CTX-0179 pattern) and clears automation plus
//! introspection (incl. the RGBA store) before and after.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitty_ipc::devtools::{
    AutomationFamily, Dispatcher, ServeContext, ServerInfo, clear_automation_for_tests,
    clear_introspection_for_tests, frame_audit_len_for_tests, issue_automation_bearer_with_ttl,
    prepare_socket_dir, publish_frame_rgba, publish_grid_text, serve_connection,
    transport_attested_peer,
};
use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
use bitty_ipc::frame_digest::{FRAME_DIGEST_ALGO, frame_digest_hex};
use bitty_ipc::limits::RateLimiter;
use bitty_ipc::scope::{Scope, ScopeSet};

/// Serial guard for the process-global automation + introspection stores.
fn digest_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn hold_digest_lock() -> std::sync::MutexGuard<'static, ()> {
    digest_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn temp_socket_path(tag: &str) -> String {
    let pid = std::process::id();
    let path = format!("/tmp/btd{pid}{tag}/s.sock");
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

/// Minimal digest client: one JSON-RPC envelope per call.
struct DigestClient {
    stream: UnixStream,
    next_id: u64,
}

impl DigestClient {
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

fn digest_scopes() -> ScopeSet {
    let mut set = ScopeSet::new();
    set.insert(Scope::DebugTrace);
    set.insert(Scope::TerminalInspect);
    set
}

/// Deterministic synthetic RGBA fixture with an ASCII marker row (never
/// secrets): the marker must never appear in any response.
fn fixture_rgba(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let len = width as usize * height as usize * 4;
    let mut rgba = Vec::with_capacity(len);
    for i in 0..len {
        rgba.push(
            (i as u8)
                .wrapping_add(seed)
                .wrapping_mul(31)
                .wrapping_add(7),
        );
    }
    let marker = b"SOCKET-MARKER-NEVER-ON-WIRE";
    let at = len.min(128);
    for (i, b) in marker.iter().enumerate() {
        if at + i < len {
            rgba[at + i] = *b;
        }
    }
    rgba
}

fn spawn_digest_server(
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
        let _verified = transport_attested_peer(unit_owner_uid(&socket_path));
        let dispatcher = Dispatcher::with_defaults();
        let server = ServerInfo::new("digest-proof".to_string(), socket_path.clone(), 80, 24);
        // Production-equivalent: the accept boundary verified a local peer,
        // so the context carries the attestation `frameHash` requires.
        let mut context = ServeContext::with_granted_session(&server, granted, &session);
        context.attest_local_peer();
        let mut limiter = RateLimiter::rc9_default();
        let clock = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        };
        let stats = serve_connection(
            &mut stream,
            _verified,
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
fn frame_hash_digest_matches_local_computation_over_socket() {
    let _guard = hold_digest_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();

    let (width, height, seq) = (96u32, 64u32, 12u64);
    let rgba = fixture_rgba(width, height, 11);
    publish_frame_rgba(width, height, seq, rgba.clone());
    publish_grid_text(
        vec!["synthetic harness line".to_string()],
        0,
        0,
        true,
        seq,
        80,
        24,
    );
    // Digest grants need an explicit short TTL (the default minter refuses
    // the `FrameDigest` family fail-closed).
    let bearer = issue_automation_bearer_with_ttl(
        "proof-digest",
        "t:1",
        AutomationFamily::FrameDigest,
        0,
        60_000,
    )
    .unwrap();
    let socket_path = temp_socket_path("dh");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_digest_server(socket_path.clone(), digest_scopes(), "proof-digest");
    std::thread::sleep(Duration::from_millis(100));
    let mut client = DigestClient::connect(&socket_path);

    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{bearer}\"}}");
    let proof = client.call("bitty.debug/frameHash", &params);
    let expect = frame_digest_hex(width, height, seq, &rgba);
    assert!(
        proof.contains("\"snapshot\":\"frameHash\""),
        "unexpected proof: {proof}"
    );
    assert!(
        proof.contains(&format!("\"algo\":\"{FRAME_DIGEST_ALGO}\"")),
        "unexpected proof: {proof}"
    );
    assert!(
        proof.contains(&format!("\"digest\":\"{expect}\"")),
        "digest mismatch over socket: {proof}"
    );
    assert!(
        proof.contains("\"trust\":\"untrusted-observation\""),
        "unexpected proof: {proof}"
    );
    assert!(
        !proof.contains("SOCKET-MARKER-NEVER-ON-WIRE"),
        "pixel bytes reached the wire: {proof}"
    );
    assert!(proof.len() < 512, "digest response must stay tiny");

    drop(client);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn frame_hash_denied_without_grant_over_socket_and_audited() {
    let _guard = hold_digest_lock();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_frame_rgba(16, 16, 1, fixture_rgba(16, 16, 1));

    let socket_path = temp_socket_path("dd");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_digest_server(socket_path.clone(), digest_scopes(), "proof-deny");
    std::thread::sleep(Duration::from_millis(100));
    let mut client = DigestClient::connect(&socket_path);

    let audit_before = frame_audit_len_for_tests();
    let denied = client.call(
        "bitty.debug/frameHash",
        r#"{"terminalId":"t:1","bearer":"bogus"}"#,
    );
    assert!(
        denied.contains("ScopeDenied"),
        "expected ScopeDenied, got: {denied}"
    );
    // Denied digest calls are audited too (attributable terminal).
    assert!(
        frame_audit_len_for_tests() > audit_before,
        "denied digest call left no audit trace"
    );

    drop(client);
    server.join().unwrap();
    clear_automation_for_tests();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

/// Local-manual only: the live-grant ceremony (a human confirming
/// synthetic-only content at the DevTools/`bitty dev` consent prompt)
/// cannot run in CI. This test never runs by default (`#[ignore]`); run it
/// explicitly against a live instance with `BITTY_LIVE_GRANT_TESTS=1`.
#[test]
#[ignore = "manual-only: needs a live instance plus a human consent ceremony; CI must never mint real grants"]
fn frame_hash_live_grant_manual_only() {
    if std::env::var("BITTY_LIVE_GRANT_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set BITTY_LIVE_GRANT_TESTS=1 to run against a live instance");
        return;
    }
    // Manual procedure (no automation possible by design):
    // 1. Start bitty with a synthetic-only fixture on the observed
    //    terminal (never passwords, tokens, clipboard, or env bytes).
    // 2. Confirm the consent prompt for a `frame-digest` grant.
    // 3. Call `bitty.debug/frameHash` over the Unix socket and compare
    //    with the harness-local `frame_digest_hex(headless_rgba)`.
    // 4. Confirm `ScopeDenied` after grant expiry (<= 120 s) and a
    //    `digest` audit trail on the serving side.
    panic!("manual-only: point this at a live BITTY_SOCKET before enabling");
}
