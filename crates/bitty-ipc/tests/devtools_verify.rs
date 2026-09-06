//! Programmatic GUI-verification harness over the devtools socket (CTX-0183).
//!
//! Owner direction: replace ydotool/screenshot/polling live proofs with
//! programmatic input + state assertions. This file proves the assertion half
//! headlessly: it publishes known runtime snapshots into the introspection
//! live stores (the same `publish_*` entry points `bitty-runtime` drives),
//! serves them over a real Unix socket with `Dispatcher::with_defaults`
//! (the same code path the `bitty-app` servo drives), and asserts grid,
//! focus, modifier, and input-ring state from the client side — no seat,
//! no screenshots, no pixel polling.
//!
//! Covered with TODAY's surface (CTX-0188/0189 are NOT done):
//! - grid-text assert incl. generation-change wait (`getGridText`)
//! - focus-state assert (`getFocus`)
//! - modifier + last-input observability (`getModifiers`, `getInputRing`)
//! - fail-fast auth proof: unscoped `sendInput` yields machine-checkable
//!   `ScopeDenied` (driving real input needs scopes plus a draining main
//!   thread; key/mouse synthesis and clipboard read are CTX-0188's scope,
//!   process profiling is CTX-0189's).
//!
//! Headless and Unix-only. The live introspection stores are process-global,
//! so every test holds the file-local serial guard for its whole
//! publish-serve-assert sequence (CTX-0179 pattern; std-only, no extra
//! dev-dependency).

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitty_ipc::devtools::{
    Dispatcher, FocusPublish, InputEventPublish, ModifiersPublish, ServeContext, ServerInfo,
    clear_introspection_for_tests, prepare_socket_dir, publish_focus, publish_grid_text,
    publish_input_ring, publish_modifiers, serve_connection, transport_attested_peer,
};
use bitty_ipc::frame::{MAX_FRAME_BYTES, encode_frame};
use bitty_ipc::limits::RateLimiter;
use bitty_ipc::scope::ScopeSet;

/// Serial guard for the process-global live introspection stores.
///
/// Same `OnceLock<Mutex<()>>` idiom as the stores themselves. Poison-safe so
/// a panicking holder cannot cascade-fail the rest of the suite.
fn verify_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn hold_verify_lock() -> std::sync::MutexGuard<'static, ()> {
    verify_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn temp_socket_path(tag: &str) -> String {
    // Portable AF_UNIX paths: macOS SUN_LEN is 104 incl. NUL, so keep the
    // payload short with a short /tmp leaf plus short file name.
    let pid = std::process::id();
    let path = format!("/tmp/btv{pid}{tag}/s.sock");
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

/// Minimal verify client: sends one JSON-RPC envelope per call and returns
/// the raw framed response for machine-checkable substring asserts.
struct VerifyClient {
    stream: UnixStream,
    next_id: u64,
}

impl VerifyClient {
    fn connect(socket_path: &str) -> Self {
        let stream = UnixStream::connect(socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self { stream, next_id: 1 }
    }

    fn call(&mut self, method: &str, params: Option<&str>) -> String {
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

    /// Poll `getGridText` until `generation` reaches `want` (the state-based
    /// replacement for screenshot polling). Fails closed on deadline.
    fn wait_for_generation(&mut self, want: u64, deadline: Duration) -> String {
        let start = Instant::now();
        let marker = format!("\"generation\":{want}");
        loop {
            let grid = self.call("bitty.debug/getGridText", None);
            if grid.contains(&marker) {
                return grid;
            }
            assert!(
                start.elapsed() < deadline,
                "timed out waiting for grid generation {want}: {grid}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn spawn_verify_server(socket_path: String, granted: ScopeSet) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let verified = transport_attested_peer(unit_owner_uid(&socket_path));
        let dispatcher = Dispatcher::with_defaults();
        let server = ServerInfo::new("verify".to_string(), socket_path.clone(), 80, 24);
        let context = ServeContext::with_granted(&server, granted);
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

fn serve_with_cli_default(tag: &str) -> (String, std::thread::JoinHandle<()>) {
    let socket_path = temp_socket_path(tag);
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_verify_server(socket_path.clone(), ScopeSet::cli_default());
    // Give the listener a moment to bind (bounded, local-only).
    std::thread::sleep(Duration::from_millis(100));
    (socket_path, server)
}

#[test]
fn verify_grid_text_assert_replaces_screenshot() {
    let _guard = hold_verify_lock();
    clear_introspection_for_tests();
    // Frame 1: shell prompt plus command output, cursor on the output row.
    publish_grid_text(
        vec!["$ echo hello".to_string(), "hello".to_string()],
        1,
        5,
        true,
        7,
        80,
        24,
    );

    let (socket_path, server) = serve_with_cli_default("vg");
    let mut client = VerifyClient::connect(&socket_path);

    // State assert: exact lines, cursor, and damage generation — the
    // machine-checkable equivalent of "screenshot shows hello".
    let grid = client.call("bitty.debug/getGridText", None);
    assert!(
        grid.contains(r#""snapshot":"grid-text""#),
        "unexpected grid: {grid}"
    );
    assert!(
        grid.contains(r#""lines":["$ echo hello","hello"]"#),
        "unexpected grid lines: {grid}"
    );
    assert!(
        grid.contains(r#""row":1"#) && grid.contains(r#""col":5"#),
        "unexpected cursor: {grid}"
    );
    assert!(
        grid.contains(r#""generation":7"#),
        "unexpected generation: {grid}"
    );

    // Generation-change wait: a republish mid-poll must be observed without
    // pixel comparison. The publisher thread stands in for the runtime tick.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        publish_grid_text(
            vec![
                "$ echo hello".to_string(),
                "hello".to_string(),
                "$ ".to_string(),
            ],
            2,
            2,
            true,
            8,
            80,
            24,
        );
    });
    let next = client.wait_for_generation(8, Duration::from_secs(5));
    assert!(
        next.contains(r#""row":2"#),
        "cursor did not advance with generation: {next}"
    );

    drop(client);
    server.join().unwrap();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn verify_focus_state_assert_over_socket() {
    let _guard = hold_verify_lock();
    clear_introspection_for_tests();
    publish_focus(FocusPublish {
        focused: true,
        focused_view: Some(3),
        mouse_capture: false,
        alt_screen: false,
        bracketed_paste: true,
        focus_events: true,
    });

    let (socket_path, server) = serve_with_cli_default("vf");
    let mut client = VerifyClient::connect(&socket_path);

    let focus = client.call("bitty.debug/getFocus", None);
    assert!(
        focus.contains(r#""snapshot":"focus""#),
        "unexpected focus: {focus}"
    );
    assert!(
        focus.contains(r#""focused":true"#),
        "unexpected focused flag: {focus}"
    );
    assert!(
        focus.contains(r#""focused_view":3"#),
        "unexpected focused view: {focus}"
    );
    assert!(
        focus.contains(r#""bracketed_paste":true"#),
        "unexpected bracketed paste: {focus}"
    );
    assert!(
        focus.contains(r#""alt_screen":false"#),
        "unexpected alt screen: {focus}"
    );

    drop(client);
    server.join().unwrap();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn verify_modifiers_and_input_ring_observability() {
    let _guard = hold_verify_lock();
    clear_introspection_for_tests();
    publish_modifiers(ModifiersPublish {
        shift: false,
        control: true,
        alt: false,
        kitty_flags: 0,
    });
    publish_input_ring(vec![
        InputEventPublish {
            seq: 41,
            kind: "key".to_string(),
            label: "key:c pressed".to_string(),
            shift: false,
            control: true,
            alt: false,
            button: None,
            col: None,
            row: None,
            pressed: Some(true),
        },
        InputEventPublish {
            seq: 42,
            kind: "mouse".to_string(),
            label: "mouse:Left pressed col=10 row=5".to_string(),
            shift: false,
            control: false,
            alt: false,
            button: Some("Left".to_string()),
            col: Some(10),
            row: Some(5),
            pressed: Some(true),
        },
    ]);

    let (socket_path, server) = serve_with_cli_default("vm");
    let mut client = VerifyClient::connect(&socket_path);

    let mods = client.call("bitty.debug/getModifiers", None);
    assert!(
        mods.contains(r#""control":true"#) && mods.contains(r#""shift":false"#),
        "unexpected modifiers: {mods}"
    );

    let ring = client.call("bitty.debug/getInputRing", Some(r#"{"limit":10}"#));
    assert!(
        ring.contains(r#""snapshot":"input-ring""#),
        "unexpected ring: {ring}"
    );
    assert!(
        ring.contains("key:c pressed") && ring.contains("mouse:Left pressed col=10 row=5"),
        "unexpected ring events: {ring}"
    );
    assert!(
        ring.contains(r#""count":2"#),
        "unexpected ring count: {ring}"
    );

    drop(client);
    server.join().unwrap();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}

#[test]
fn verify_unscoped_send_input_is_denied_machine_checkably() {
    // Driving real input requires authority: with an empty scope set the
    // well-formed `sendInput` must fail fast with `ScopeDenied` (no 5 s
    // queue timeout, no partial state). Key/mouse synthesis beyond text
    // injection is CTX-0188's scope; this test pins the harness contract
    // that auth failures are machine-checkable, not hangs.
    let _guard = hold_verify_lock();
    clear_introspection_for_tests();

    let socket_path = temp_socket_path("vd");
    prepare_socket_dir(&socket_path).unwrap();
    let server = spawn_verify_server(socket_path.clone(), ScopeSet::new());
    std::thread::sleep(Duration::from_millis(100));
    let mut client = VerifyClient::connect(&socket_path);

    let denied = client.call(
        "bitty.debug/sendInput",
        Some(r#"{"terminal_id":"t:1","text":"echo hi"}"#),
    );
    assert!(
        denied.contains("ScopeDenied"),
        "expected ScopeDenied, got: {denied}"
    );

    drop(client);
    server.join().unwrap();
    clear_introspection_for_tests();
    std::fs::remove_file(&socket_path).ok();
}
