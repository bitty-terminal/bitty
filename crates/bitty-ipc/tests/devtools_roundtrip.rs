//! End-to-end round-trip over a real Unix socket (CTX-0144, Issue #236).
//!
//! Headless and Unix-only: binds a temporary socket, serves it with
//! `bitty_ipc::devtools` (the same code path the `bitty-app` servo drives),
//! and speaks the `bitty-devtools` wire from the client side: `u32`
//! big-endian length-prefixed JSON (`bitty-devtools/src/transport.ts`
//! framing) carrying `protocol.ts`-shaped envelopes. Proves handshake
//! (`bitty.debug/ping`) plus one snapshot query (`bitty.debug/getSnapshot`)
//! round-trip, plus fail-closed oversize rejection.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitty_ipc::PeerCredentials;
use bitty_ipc::devtools::{
    Dispatcher, ServeContext, ServerInfo, prepare_socket_dir, serve_connection,
};
use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
use bitty_ipc::limits::RateLimiter;

fn temp_socket_path(tag: &str) -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("bitty-ctx0144-e2e-{pid}-{nanos}-{tag}"));
    dir.join("bitty")
        .join(format!("{tag}.sock"))
        .to_str()
        .unwrap()
        .to_string()
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

fn send_request(stream: &mut UnixStream, json: &str) {
    let wire = encode_frame(json.as_bytes()).unwrap();
    stream.write_all(&wire).unwrap();
    stream.flush().unwrap();
}

fn spawn_server(socket_path: String, min_requests: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let peer = PeerCredentials::new(
            unit_owner_uid(&socket_path),
            unit_owner_uid(&socket_path),
            0,
        );
        let dispatcher = Dispatcher::with_defaults();
        let server = ServerInfo::new("e2e".to_string(), socket_path.clone(), 80, 24);
        let context = ServeContext::new(&server);
        let mut limiter = RateLimiter::rc9_default();
        let clock = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0)
        };
        let stats = serve_connection(
            &mut stream,
            peer,
            unit_owner_uid(&socket_path),
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
        .unwrap();
        assert!(stats.requests >= min_requests, "expected test requests");
        assert_eq!(stats.responses, stats.requests);
    })
}

#[cfg(unix)]
fn unit_owner_uid(path: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).map(|m| m.uid()).unwrap_or(0)
}

#[test]
fn ipc_round_trip_ping_and_snapshot_over_unix_socket() {
    let socket_path = temp_socket_path("roundtrip");
    let dir = prepare_socket_dir(&socket_path).unwrap();
    assert_eq!(dir.dir_mode, 0o700);

    let server = spawn_server(socket_path.clone(), 3);
    // Give the listener a moment to bind (bounded, local-only).
    std::thread::sleep(Duration::from_millis(100));
    let mut client = UnixStream::connect(&socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // Handshake: transport shape without jsonrpc (transport.ts IpcRequest).
    send_request(
        &mut client,
        r#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#,
    );
    let pong = read_framed(&mut client);
    assert!(
        pong.contains(r#""jsonrpc":"2.0""#),
        "unexpected pong: {pong}"
    );
    assert!(pong.contains(r#""id":1"#), "unexpected pong: {pong}");
    assert!(pong.contains(r#""ok":true"#), "unexpected pong: {pong}");

    // Snapshot: protocol shape with jsonrpc (protocol.ts RequestFrame).
    send_request(
        &mut client,
        r#"{"jsonrpc":"2.0","id":2,"method":"bitty.debug/getSnapshot","version":"1.0"}"#,
    );
    let snapshot = read_framed(&mut client);
    assert!(
        snapshot.contains(r#""snapshot":"runtime-stats""#),
        "unexpected snapshot: {snapshot}"
    );
    assert!(
        snapshot.contains(r#""cols":80"#),
        "unexpected snapshot: {snapshot}"
    );
    assert!(
        snapshot.contains(r#""rows":24"#),
        "unexpected snapshot: {snapshot}"
    );

    // Unknown method: correlated usage error, connection stays open.
    send_request(
        &mut client,
        r#"{"id":3,"method":"bitty.debug/doesNotExist","version":"1.0"}"#,
    );
    let unknown = read_framed(&mut client);
    assert!(unknown.contains("UnknownMethod"), "unexpected: {unknown}");
    assert!(unknown.contains(r#""id":3"#), "unexpected: {unknown}");

    drop(client);
    server.join().unwrap();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn ipc_oversize_frame_is_rejected_fail_closed() {
    let socket_path = temp_socket_path("oversize");
    prepare_socket_dir(&socket_path).unwrap();

    let server = spawn_server(socket_path.clone(), 0);
    std::thread::sleep(Duration::from_millis(100));
    let mut client = UnixStream::connect(&socket_path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let huge_len = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
    client.write_all(&huge_len).unwrap();
    client.flush().unwrap();
    let rejection = read_framed(&mut client);
    assert!(
        rejection.contains("FrameTooLarge"),
        "unexpected rejection: {rejection}"
    );
    assert!(
        rejection.contains(r#""id":0"#),
        "unexpected rejection: {rejection}"
    );

    drop(client);
    server.join().unwrap();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn ipc_stale_socket_file_is_rebindable() {
    // A leftover socket file from a dead instance must not block rebinding:
    // remove + bind succeeds (the servo's stale-cleanup path at OS level).
    let socket_path = temp_socket_path("stale");
    prepare_socket_dir(&socket_path).unwrap();
    {
        let _first = UnixListener::bind(&socket_path).unwrap();
        // `_first` drops here without accepting: stale file remains.
    }
    assert!(std::path::Path::new(&socket_path).exists());
    std::fs::remove_file(&socket_path).unwrap();
    let _second = UnixListener::bind(&socket_path).unwrap();
    std::fs::remove_file(&socket_path).ok();
}
