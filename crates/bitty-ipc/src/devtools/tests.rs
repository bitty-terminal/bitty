use super::handlers::{live_input_store, parse_optional_uint_param};
use super::*;
#[cfg(unix)]
use crate::auth::DIR_MODE;
use crate::error::IpcError;
use crate::frame::{MAX_FRAME_BYTES, encode_frame};
use crate::limits::RateLimiter;
use crate::wire::MAX_JSON_DEPTH;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};

fn test_server_info() -> ServerInfo {
    ServerInfo::new(
        "test-inst".to_string(),
        "/run/user/1000/bitty/test-inst.sock".to_string(),
        80,
        24,
    )
}

fn test_context() -> ServeContext {
    ServeContext::new(&test_server_info())
}

// ── socket path ─────────────────────────────────────────────────────

#[test]
fn socket_path_bitty_socket_wins_verbatim() {
    let path =
        resolve_socket_path(1000, Some("/run/user/1000"), Some("/tmp/custom.sock"), None).unwrap();
    assert_eq!(path, "/tmp/custom.sock");
}

#[test]
fn socket_path_xdg_plus_instance() {
    let path = resolve_socket_path(1000, Some("/run/user/1000"), None, Some("my-inst_1")).unwrap();
    assert_eq!(path, "/run/user/1000/bitty/my-inst_1.sock");
}

#[test]
fn socket_path_defaults() {
    let path = resolve_socket_path(1000, None, None, None).unwrap();
    assert_eq!(path, "/run/user/1000/bitty/default.sock");
}

#[test]
fn socket_path_empty_socket_falls_through() {
    let path = resolve_socket_path(1000, Some("/run/user/1000"), Some(""), None).unwrap();
    assert_eq!(path, "/run/user/1000/bitty/default.sock");
}

#[test]
fn socket_path_rejects_long_and_nul() {
    let long = "a".repeat(MAX_SOCKET_PATH_BYTES + 1);
    assert!(resolve_socket_path(1000, None, Some(&long), None).is_err());
    assert!(resolve_socket_path(1000, None, Some("/tmp/a\0b.sock"), None).is_err());
}

#[test]
fn socket_path_portable_bound_is_pinned() {
    // Portable AF_UNIX ceiling: 100 payload bytes fits Linux 108 and
    // macOS/BSD 104 incl. NUL with margin (historical floor 92).
    const { assert!(MAX_SOCKET_PATH_BYTES <= 100) };
    const { assert!(SUN_LEN_LINUX == 108) };
    const { assert!(SUN_LEN_MACOS == 104) };
    const { assert!(MAX_SOCKET_PATH_BYTES < SUN_LEN_MACOS) };
    // Every resolved path fits the portable bound incl. NUL.
    let path = resolve_socket_path(1000, Some("/run/user/1000"), None, None).unwrap();
    assert!(path.len() <= MAX_SOCKET_PATH_BYTES);
    assert!(path.len() < SUN_LEN_MACOS);
}

#[test]
fn socket_path_hashes_long_instance_to_fit() {
    // A 64-char instance with a medium base overflows direct form but
    // fits via deterministic hash clamping.
    let base = format!("/tmp/{}", "b".repeat(50));
    let long_instance = "c".repeat(MAX_INSTANCE_ID_LEN);
    let direct_len =
        base.len() + 1 + SOCKET_LEAF_DIR.len() + 1 + long_instance.len() + ".sock".len();
    assert!(direct_len > MAX_SOCKET_PATH_BYTES);
    let path = resolve_socket_path(1000, Some(&base), None, Some(&long_instance)).unwrap();
    assert!(path.len() <= MAX_SOCKET_PATH_BYTES);
    assert!(!path.contains(&long_instance));
    assert!(path.ends_with(".sock"));
    // Deterministic: same instance hashes identically.
    let again = resolve_socket_path(1000, Some(&base), None, Some(&long_instance)).unwrap();
    assert_eq!(path, again);
}

#[test]
fn socket_path_rejects_long_base_fail_closed() {
    // Even the hashed leaf cannot save a base dir that is itself too long.
    let base = format!("/tmp/{}", "d".repeat(120));
    let err = resolve_socket_path(1000, Some(&base), None, None).unwrap_err();
    let reason = format!("{err}");
    assert!(reason.contains("AF_UNIX") || reason.contains("too long"));
    let long_socket = format!("/tmp/{}.sock", "e".repeat(120));
    assert!(resolve_socket_path(1000, None, Some(&long_socket), None).is_err());
}

#[test]
fn socket_path_rejects_bad_instance() {
    assert!(resolve_socket_path(1000, None, None, Some("bad/id")).is_err());
    assert!(resolve_socket_path(1000, None, None, Some("")).is_err());
    let long = "a".repeat(MAX_INSTANCE_ID_LEN + 1);
    assert!(resolve_socket_path(1000, None, None, Some(&long)).is_err());
    assert!(resolve_socket_path(1000, None, None, Some("has space")).is_err());
}

#[test]
fn socket_path_from_env_needs_base_without_uid() {
    let env = SocketEnv::default();
    assert!(resolve_socket_path_from_env(&env, None).is_err());
    let (path, instance) = resolve_socket_path_from_env(&env, Some(1000)).unwrap();
    assert_eq!(path, "/run/user/1000/bitty/default.sock");
    assert_eq!(instance, "default");
}

#[test]
fn socket_path_from_env_socket_override() {
    let env = SocketEnv {
        bitty_socket: Some("/tmp/x.sock".to_string()),
        xdg_runtime_dir: None,
        instance_id: Some("ignored".to_string()),
    };
    let (path, instance) = resolve_socket_path_from_env(&env, None).unwrap();
    assert_eq!(path, "/tmp/x.sock");
    assert_eq!(instance, "ignored");
}

// ── windows pipe naming (CTX-0196) ──────────────────────────────────

#[test]
fn windows_pipe_name_maps_instance_verbatim() {
    assert_eq!(
        windows_pipe_name("default"),
        r"\\.\pipe\bitty-default".to_string()
    );
    assert_eq!(
        windows_pipe_name("my-inst_1"),
        r"\\.\pipe\bitty-my-inst_1".to_string()
    );
}

#[test]
fn windows_pipe_name_roundtrips_through_parser() {
    for id in ["default", "a", "my-inst_1", "ABC-9_z"] {
        let pipe = windows_pipe_name(id);
        let file = pipe.rsplit('\\').next().unwrap();
        assert_eq!(windows_instance_from_pipe_name(file).as_deref(), Some(id));
    }
}

#[test]
fn windows_pipe_parser_skips_foreign_and_malformed() {
    assert_eq!(windows_instance_from_pipe_name("bitty-"), None);
    assert_eq!(windows_instance_from_pipe_name("other-pipe"), None);
    assert_eq!(windows_instance_from_pipe_name(""), None);
    assert_eq!(windows_instance_from_pipe_name("bitty-has space"), None);
    assert_eq!(windows_instance_from_pipe_name("bitty-bad/id"), None);
    assert_eq!(windows_instance_from_pipe_name("BITTY-default"), None);
    let long = format!("bitty-{}", "a".repeat(MAX_INSTANCE_ID_LEN + 1));
    assert_eq!(windows_instance_from_pipe_name(&long), None);
    // Pipe names never carry the socket suffix.
    assert_eq!(windows_instance_from_pipe_name("bitty-default.sock"), None);
}

// ── parsing ─────────────────────────────────────────────────────────

#[test]
fn parse_transport_shape_without_jsonrpc() {
    let payload = br#"{"id":1,"method":"bitty.debug/ping","params":{},"version":"1.0"}"#;
    let request = parse_request(payload).unwrap();
    assert_eq!(request.id_raw, "1");
    assert_eq!(request.method, "bitty.debug/ping");
    assert!(!request.has_jsonrpc);
}

#[test]
fn parse_protocol_shape_with_jsonrpc() {
    let payload =
        br#"{"jsonrpc":"2.0","id":42,"method":"bitty.debug/getSnapshot","version":"1.0"}"#;
    let request = parse_request(payload).unwrap();
    assert_eq!(request.id_raw, "42");
    assert_eq!(request.method, "bitty.debug/getSnapshot");
    assert!(request.has_jsonrpc);
}

#[test]
fn parse_rejects_wrong_version_with_id() {
    let payload = br#"{"id":7,"method":"bitty.debug/ping","version":"2.0"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "UnsupportedVersion");
    assert_eq!(fault.id_raw.as_deref(), Some("7"));
}

#[test]
fn parse_rejects_missing_version() {
    let payload = br#"{"id":7,"method":"bitty.debug/ping"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "MissingVersion");
}

#[test]
fn parse_rejects_unprefixed_method() {
    let payload = br#"{"id":1,"method":"terminal.text","version":"1.0"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "InvalidMethod");
}

#[test]
fn parse_rejects_string_id() {
    let payload = br#"{"id":"1","method":"bitty.debug/ping","version":"1.0"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "MissingId");
}

#[test]
fn parse_rejects_ambient_authority() {
    let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0","scope":"admin"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "ForbiddenField");
    assert_eq!(fault.id_raw.as_deref(), Some("1"));
}

#[test]
fn parse_allows_nested_scope_in_params() {
    let payload =
        br#"{"id":1,"method":"bitty.debug/ping","version":"1.0","params":{"scope":"value"}}"#;
    assert!(parse_request(payload).is_ok());
}

#[test]
fn parse_rejects_non_object_and_garbage() {
    assert!(parse_request(br#"[1,2]"#).is_err());
    assert!(parse_request(b"not json").is_err());
    assert!(parse_request(b"").is_err());
    assert!(parse_request(&[0xFF, 0xFE]).is_err());
}

#[test]
fn parse_rejects_bad_jsonrpc() {
    let payload = br#"{"jsonrpc":"1.0","id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
    let fault = parse_request(payload).unwrap_err();
    assert_eq!(fault.code, "InvalidJsonRpc");
}

#[test]
fn parse_accepts_escaped_method() {
    let payload = br#"{"id":1,"method":"bitty.debug\u002fping","version":"1.0"}"#;
    let request = parse_request(payload).unwrap();
    assert_eq!(request.method, "bitty.debug/ping");
}

#[test]
fn parse_rejects_deep_nesting() {
    let nested = "[".repeat(MAX_JSON_DEPTH + 1) + &"]".repeat(MAX_JSON_DEPTH + 1);
    assert!(parse_request(nested.as_bytes()).is_err());
}

// ── dispatch ────────────────────────────────────────────────────────

#[test]
fn dispatch_ping_round_trip() {
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":3,"method":"bitty.debug/ping","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"id\":3"));
    assert!(text.contains("\"ok\":true"));
    assert!(text.contains("\"version\":\"1.0\""));
    assert!(text.contains("\"jsonrpc\":\"2.0\""));
}

#[test]
fn dispatch_snapshot_carries_stats() {
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"jsonrpc":"2.0","id":9,"method":"bitty.debug/getSnapshot","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"id\":9"));
    assert!(text.contains("\"snapshot\":\"runtime-stats\""));
    assert!(text.contains("\"instance\":\"test-inst\""));
    assert!(text.contains("\"cols\":80"));
    assert!(text.contains("\"rows\":24"));
}

#[test]
fn dispatch_unknown_method_is_error() {
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":1,"method":"bitty.debug/nope","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("UnknownMethod"));
    assert!(text.contains("\"id\":1"));
}

#[test]
fn version_mismatch_is_correlated_error() {
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":11,"method":"bitty.debug/ping","version":"9.9"}"#,
        &dispatcher,
        &context,
    );
    assert!(outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("UnsupportedVersion"));
    assert!(text.contains("\"id\":11"));
}

#[test]
fn dispatcher_registers_new_methods_for_follow_up() {
    fn custom(context: &ServeContext, _request: &DevtoolsRequest) -> Result<String, HandlerError> {
        Ok(format!("{{\"uptime_ms\":{}}}", context.uptime_ms))
    }
    let mut dispatcher = Dispatcher::with_defaults();
    // CTX-0144 (ping, getSnapshot) plus CTX-0159 introspection
    // (getGridText, getInputRing, getModifiers, getFocus) plus CTX-0171
    // control (listWindows, listViews, listTerminals, spawnTerminal,
    // closeTerminal, sendInput, getTerminalText, splitView, focusView,
    // reloadConfig) plus CTX-0257 workspace entry (listWorkspaces,
    // createWorkspace, closeWorkspace, focusWorkspace) plus CTX-0259 move
    // (moveWorkspace) plus CTX-0188
    // automation (synthesizeInput,
    // captureFrame) plus CTX-0244 digest (frameHash) plus CTX-0189
    // profiling (getProcessStats, getFrameStats, streamProcessStats,
    // streamFrameStats).
    assert_eq!(dispatcher.method_count(), 28);
    assert!(dispatcher.contains("bitty.debug/getGridText"));
    assert!(dispatcher.contains("bitty.debug/getInputRing"));
    assert!(dispatcher.contains("bitty.debug/getModifiers"));
    assert!(dispatcher.contains("bitty.debug/getFocus"));
    assert!(dispatcher.contains(METHOD_SYNTHESIZE_INPUT));
    assert!(dispatcher.contains(METHOD_CAPTURE_FRAME));
    assert!(dispatcher.contains(METHOD_FRAME_HASH));
    assert!(dispatcher.contains(METHOD_GET_PROCESS_STATS));
    assert!(dispatcher.contains(METHOD_GET_FRAME_STATS));
    assert!(dispatcher.contains(METHOD_STREAM_PROCESS_STATS));
    assert!(dispatcher.contains(METHOD_STREAM_FRAME_STATS));
    for method in crate::ctl::all_control_methods() {
        assert!(
            dispatcher.contains(method),
            "control method {method} must be registered"
        );
    }
    dispatcher
        .register("bitty.debug/customProbe", custom)
        .unwrap();
    assert!(dispatcher.contains("bitty.debug/customProbe"));
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":1,"method":"bitty.debug/customProbe","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    assert!(dispatcher.register("terminal.text", custom).is_err());
}

#[test]
fn error_message_truncated_to_bound() {
    let long = "x".repeat(MAX_ERROR_MESSAGE_CHARS + 100);
    let bytes = encode_error("1", "usage", "InvalidRequest", &long);
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.len() < long.len() + 200);
    assert!(text.contains("..."));
}

#[test]
fn id_zero_error_shape() {
    let bytes = id_zero_error("transport", "FrameTooLarge", "too big");
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("\"id\":0"));
    assert!(text.contains("FrameTooLarge"));
}

// ── connection serving (unix socketpair, no listener) ───────────────

#[cfg(unix)]
#[test]
fn serve_connection_ping_pong_over_socketpair() {
    use std::os::unix::net::UnixStream;

    let (mut client, mut server) = UnixStream::pair().unwrap();
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let peer = transport_attested_peer(1000);
    let mut limiter = RateLimiter::rc9_default();
    let clock = || 0u64;

    let handle = std::thread::spawn(move || {
        serve_connection(
            &mut server,
            peer,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
    });

    let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
    let wire = encode_frame(payload).unwrap();
    client.write_all(&wire).unwrap();

    let mut header = [0u8; 4];
    client.read_exact(&mut header).unwrap();
    let len = u32::from_be_bytes(header) as usize;
    assert!(len <= MAX_FRAME_BYTES);
    let mut body = vec![0u8; len];
    client.read_exact(&mut body).unwrap();
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("\"ok\":true"));

    drop(client);
    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.responses, 1);
    assert_eq!(stats.denied, 0);
}

#[cfg(unix)]
#[test]
fn serve_path_takes_verified_marker_only() {
    use crate::auth::{PeerCredentials, verify_peer_for_connection};
    // Regression for CodeQL HIGH `cleartext logging of sensitive
    // information`: `serve_connection` takes only the pre-verified
    // `VerifiedPeer` marker, so no `PeerCredentials`-typed value flows
    // into the serving path. Accept-boundary verification is fail-closed.
    let good = PeerCredentials::new(1000, 1000, 1);
    let verified = verify_peer_for_connection(good, 1000).unwrap();
    let attested = transport_attested_peer(1000);
    assert_eq!(verified, attested);

    // Foreign UID cannot produce a marker: rejected before any byte read.
    let foreign = PeerCredentials::new(2000, 2000, 99);
    let err = verify_peer_for_connection(foreign, 1000).unwrap_err();
    assert!(matches!(err, IpcError::Unauthenticated { .. }));

    // Verified marker serves correctly over a socketpair.
    use std::os::unix::net::UnixStream;

    let (mut client, mut server) = UnixStream::pair().unwrap();
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let mut limiter = RateLimiter::rc9_default();
    let clock = || 0u64;
    let handle = std::thread::spawn(move || {
        serve_connection(
            &mut server,
            verified,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
    });
    let payload = br#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#;
    let wire = encode_frame(payload).unwrap();
    client.write_all(&wire).unwrap();
    let mut header = [0u8; 4];
    client.read_exact(&mut header).unwrap();
    let len = u32::from_be_bytes(header) as usize;
    let mut body = vec![0u8; len];
    client.read_exact(&mut body).unwrap();
    assert!(String::from_utf8(body).unwrap().contains("\"ok\":true"));
    drop(client);
    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.requests, 1);
}

#[cfg(unix)]
#[test]
fn serve_connection_rate_limits_with_error_response() {
    use std::os::unix::net::UnixStream;

    let (mut client, mut server) = UnixStream::pair().unwrap();
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let peer = transport_attested_peer(1000);
    let mut limiter = RateLimiter::new(100, 1);
    let clock = || 0u64;

    let handle = std::thread::spawn(move || {
        serve_connection(
            &mut server,
            peer,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
    });

    for id in 1..=2u64 {
        let payload =
            format!("{{\"id\":{id},\"method\":\"bitty.debug/ping\",\"version\":\"1.0\"}}");
        let wire = encode_frame(payload.as_bytes()).unwrap();
        client.write_all(&wire).unwrap();
    }
    let mut texts = Vec::new();
    for _ in 0..2 {
        let mut header = [0u8; 4];
        client.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; len];
        client.read_exact(&mut body).unwrap();
        texts.push(String::from_utf8(body).unwrap());
    }
    assert!(texts[0].contains("\"ok\":true"));
    assert!(texts[1].contains("RateLimited"));
    drop(client);
    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.requests, 2);
    assert_eq!(stats.denied, 1);
}

#[cfg(unix)]
#[test]
fn serve_connection_oversize_frame_closes() {
    use std::os::unix::net::UnixStream;

    let (mut client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let peer = transport_attested_peer(1000);
    let mut limiter = RateLimiter::rc9_default();
    let clock = || 0u64;

    let handle = std::thread::spawn(move || {
        serve_connection(
            &mut server,
            peer,
            &dispatcher,
            &context,
            &mut limiter,
            &clock,
        )
    });

    let huge = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
    client.write_all(&huge).unwrap();
    let mut header = [0u8; 4];
    client.read_exact(&mut header).unwrap();
    let len = u32::from_be_bytes(header) as usize;
    let mut body = vec![0u8; len];
    client.read_exact(&mut body).unwrap();
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("FrameTooLarge"));
    let stats = handle.join().unwrap().unwrap();
    assert_eq!(stats.framing_errors, 1);
}

// ── directory attestation (unix, temp dirs) ─────────────────────────

#[cfg(unix)]
#[test]
fn prepare_socket_dir_enforces_0700() {
    let base = std::env::temp_dir().join(format!(
        "bitty-ctx0144-{}-{}",
        std::process::id(),
        "prepare"
    ));
    let socket_path = base.join("bitty/t.sock");
    let socket_str = socket_path.to_str().unwrap();
    let attestation = prepare_socket_dir(socket_str).unwrap();
    assert_eq!(attestation.dir_mode, DIR_MODE);
    // Second call on the existing good leaf succeeds.
    let again = prepare_socket_dir(socket_str).unwrap();
    assert_eq!(again, attestation);
    std::fs::remove_dir_all(&base).ok();
}

#[cfg(unix)]
#[test]
fn prepare_socket_dir_rejects_bad_mode() {
    use std::os::unix::fs::PermissionsExt;

    let base = std::env::temp_dir().join(format!(
        "bitty-ctx0144-{}-{}",
        std::process::id(),
        "badmode"
    ));
    let leaf = base.join("bitty");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket_str = leaf.join("t.sock").to_str().unwrap().to_string();
    let err = prepare_socket_dir(&socket_str).unwrap_err();
    assert!(matches!(err, IpcError::Unauthenticated { .. }));
    std::fs::remove_dir_all(&base).ok();
}

/// CR-IPC-01: a symlinked socket directory must fail closed even when the
/// link target is a well-formed `0700` directory owned by us.
#[cfg(unix)]
#[test]
fn prepare_socket_dir_rejects_symlinked_leaf() {
    use std::os::unix::fs::PermissionsExt;

    let base = std::env::temp_dir().join(format!(
        "bitty-ctx0203-{}-{}",
        std::process::id(),
        "symlink-leaf"
    ));
    let target = base.join("real");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = base.join("bitty");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let socket_str = link.join("t.sock").to_str().unwrap().to_string();
    let err = prepare_socket_dir(&socket_str).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "symlinked leaf must fail closed, got: {err:?}"
    );
    std::fs::remove_dir_all(&base).ok();
}

/// CR-IPC-01: a symlinked socket path must fail closed before any chmod
/// is applied to its target.
#[cfg(unix)]
#[test]
fn attest_bound_socket_rejects_symlink() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let base = std::env::temp_dir().join(format!(
        "bitty-ctx0203-{}-{}",
        std::process::id(),
        "symlink-sock"
    ));
    let dir_path = base.join("bitty");
    std::fs::create_dir_all(&dir_path).unwrap();
    std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket_str = dir_path.join("t.sock").to_str().unwrap().to_string();
    let attestation = prepare_socket_dir(&socket_str).unwrap();

    let real = dir_path.join("real.sock");
    std::fs::write(&real, b"x").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
    let link_path = dir_path.join("link.sock");
    std::os::unix::fs::symlink(&real, &link_path).unwrap();
    let link_str = link_path.to_str().unwrap();

    let err = attest_bound_socket(link_str, &attestation).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "symlinked socket must fail closed, got: {err:?}"
    );
    // Fail-closed before chmod: the link target keeps its pre-existing mode.
    let target_mode = std::fs::metadata(&real).unwrap().mode() & 0o777;
    assert_eq!(target_mode, 0o644);
    std::fs::remove_dir_all(&base).ok();
}

// ── introspection (CTX-0159, read-only, bounded) ───────────────────────
//
// The global live stores (`live_grid_store`, `live_input_store`, ...) are
// shared across tests in this binary. Rust runs tests in parallel
// threads, so one test's `clear_introspection_for_tests` can wipe another
// test's published snapshot mid-sequence (CTX-0179 CI flake: 134 passed /
// 1 failed on `introspection_round_trip_all_methods_sequential`). Every
// test below that touches the globals holds
// `lock_introspection_for_test` for its whole publish→assert sequence;
// tests that only exercise pure parsers need no guard. Test-only:
// production paths never take this lock (lock order is always
// serial-guard → store locks, never the reverse, so no deadlock).

/// Serial guard for the process-global live introspection stores.
///
/// std-only on purpose (`serial_test` would add a dev-dependency for
/// what is ten lines): same `OnceLock<Mutex<...>>` idiom as the stores
/// themselves. Poison-safe so a panicking holder cannot cascade-fail the
/// rest of the suite.
fn introspection_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Hold for the whole body of any test that publishes, reads, or clears
/// the global introspection stores.
fn lock_introspection_for_test() -> std::sync::MutexGuard<'static, ()> {
    introspection_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn introspection_params_are_per_method_bounded() {
    // Absent params mean defaults.
    assert_eq!(
        parse_optional_uint_param(None, "rows", MAX_INSPECT_ROWS, MAX_INSPECT_ROWS).unwrap(),
        MAX_INSPECT_ROWS
    );
    // Present and valid.
    assert_eq!(
        parse_optional_uint_param(
            Some(r#"{"rows":10,"cols":40}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .unwrap(),
        10
    );
    assert_eq!(
        parse_optional_uint_param(
            Some(r#"{"limit":5}"#),
            "limit",
            MAX_INPUT_RING,
            MAX_INPUT_RING
        )
        .unwrap(),
        5
    );
    // Unknown keys are ignored (forward compatible).
    assert_eq!(
        parse_optional_uint_param(
            Some(r#"{"other":99}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .unwrap(),
        MAX_INSPECT_ROWS
    );
    // Oversize, zero, non-numeric, and signed values fail closed.
    assert!(
        parse_optional_uint_param(
            Some(r#"{"rows":999}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .is_err()
    );
    assert!(
        parse_optional_uint_param(
            Some(r#"{"rows":0}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .is_err()
    );
    assert!(
        parse_optional_uint_param(
            Some(r#"{"rows":"10"}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .is_err()
    );
    assert!(
        parse_optional_uint_param(
            Some(r#"{"rows":-3}"#),
            "rows",
            MAX_INSPECT_ROWS,
            MAX_INSPECT_ROWS
        )
        .is_err()
    );
}

#[test]
fn introspection_envelope_params_shape() {
    // Object params are captured verbatim for handlers.
    let request = parse_request(
        br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":10}}"#,
    )
    .unwrap();
    assert_eq!(request.params_raw.as_deref(), Some(r#"{"rows":10}"#));
    // Absent params yield None (defaults apply).
    let request =
        parse_request(br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#).unwrap();
    assert_eq!(request.params_raw, None);
    // Array params fail closed.
    let fault = parse_request(
        br#"{"id":2,"method":"bitty.debug/getGridText","version":"1.0","params":[1]}"#,
    )
    .unwrap_err();
    assert_eq!(fault.code, "InvalidParams");
    // Oversize params fail closed before dispatch.
    let big = format!(
        "{{\"id\":3,\"method\":\"bitty.debug/getGridText\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{}\"}}}}",
        "p".repeat(MAX_PARAMS_BYTES)
    );
    let fault = parse_request(big.as_bytes()).unwrap_err();
    assert_eq!(fault.code, "PayloadTooLarge");
}

#[test]
fn introspection_round_trip_all_methods_sequential() {
    let _introspection_guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    publish_grid_text(
        vec!["hello introspect".to_string(), "second row".to_string()],
        0,
        16,
        true,
        7,
        80,
        24,
    );
    publish_input_ring(vec![
        InputEventPublish {
            seq: 1,
            kind: "key".to_string(),
            label: "key:a".to_string(),
            shift: false,
            control: false,
            alt: false,
            button: None,
            col: None,
            row: None,
            pressed: Some(true),
        },
        InputEventPublish {
            seq: 2,
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
    publish_modifiers(ModifiersPublish {
        shift: true,
        control: false,
        alt: false,
        kitty_flags: 0,
    });
    publish_focus(FocusPublish {
        focused: true,
        focused_view: Some(1),
        mouse_capture: false,
        alt_screen: false,
        bracketed_paste: false,
        focus_events: false,
    });

    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();

    let outcome = handle_envelope(
        br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"snapshot\":\"grid-text\""));
    assert!(text.contains("hello introspect"));
    assert!(text.contains("\"row\":0"));

    // Bounded slice via params.
    let outcome = handle_envelope(
        br#"{"id":2,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":1,"cols":5}}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("hello"));

    let outcome = handle_envelope(
        br#"{"id":3,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"snapshot\":\"input-ring\""));
    assert!(text.contains("key:a"));
    assert!(text.contains("mouse:Left"));

    let outcome = handle_envelope(
        br#"{"id":4,"method":"bitty.debug/getInputRing","version":"1.0","params":{"limit":1}}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("mouse:Left"));
    assert!(!text.contains("key:a"));

    let outcome = handle_envelope(
        br#"{"id":5,"method":"bitty.debug/getModifiers","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"snapshot\":\"modifiers\""));
    assert!(text.contains("\"shift\":true"));

    let outcome = handle_envelope(
        br#"{"id":6,"method":"bitty.debug/getFocus","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"snapshot\":\"focus\""));
    assert!(text.contains("\"focused\":true"));

    // Oversize params fail closed with correlated errors.
    let outcome = handle_envelope(
        br#"{"id":7,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":999}}"#,
        &dispatcher,
        &context,
    );
    assert!(outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("InvalidParams"));
    assert!(text.contains("\"id\":7"));

    clear_introspection_for_tests();
}

#[test]
fn introspection_empty_store_is_not_an_error() {
    let _introspection_guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"lines\":[]"));
    let outcome = handle_envelope(
        br#"{"id":2,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("\"events\":[]")
    );
    clear_introspection_for_tests();
}

#[test]
fn introspection_publish_is_bounded() {
    let _introspection_guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    // Overlong grid input is truncated deterministically at publish time.
    let long_line = "x".repeat(MAX_INSPECT_COLS + 50);
    let many: Vec<String> = (0..(MAX_INSPECT_ROWS + 10))
        .map(|i| format!("{long_line}-{i}"))
        .collect();
    publish_grid_text(many, 0, 0, true, 1, 80, 24);
    let dispatcher = Dispatcher::with_defaults();
    let context = test_context();
    let outcome = handle_envelope(
        br#"{"id":1,"method":"bitty.debug/getGridText","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.len() <= MAX_INSPECT_JSON_BYTES + 512);
    // Overlong labels are truncated at publish time.
    publish_input_ring(vec![InputEventPublish {
        seq: 1,
        kind: "key".to_string(),
        label: "y".repeat(MAX_INPUT_LABEL_CHARS + 100),
        shift: false,
        control: false,
        alt: false,
        button: None,
        col: None,
        row: None,
        pressed: Some(true),
    }]);
    let outcome = handle_envelope(
        br#"{"id":2,"method":"bitty.debug/getInputRing","version":"1.0"}"#,
        &dispatcher,
        &context,
    );
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.len() <= MAX_INSPECT_JSON_BYTES + 512);
    clear_introspection_for_tests();
}

// ── test automation (CTX-0188, Amendment A1 candidate) ──────────────────
//
// Bearer matrix, scope intersection, bounds, redaction, and rate shedding
// for `synthesizeInput` + `captureFrame`. Every test holds the serial
// guard (automation shares the input/grid stores) and clears automation
// plus introspection before and after.

fn automation_scopes_synthesize() -> crate::scope::ScopeSet {
    let mut set = crate::scope::ScopeSet::new();
    set.insert(crate::scope::Scope::DebugControl);
    set.insert(crate::scope::Scope::TerminalInput);
    set
}

fn automation_scopes_capture() -> crate::scope::ScopeSet {
    let mut set = crate::scope::ScopeSet::new();
    set.insert(crate::scope::Scope::DebugTrace);
    set.insert(crate::scope::Scope::TerminalInspect);
    set
}

fn automation_context(
    server: &ServerInfo,
    granted: crate::scope::ScopeSet,
    session: &str,
    now_ms: u64,
) -> ServeContext {
    let mut ctx = ServeContext::with_granted_session(server, granted, session);
    ctx.uptime_ms = now_ms;
    ctx
}

fn synth_envelope(id: u64, params: &str) -> Vec<u8> {
    format!(
        "{{\"id\":{id},\"method\":\"{METHOD_SYNTHESIZE_INPUT}\",\"version\":\"1.0\",\"params\":{params}}}"
    )
    .into_bytes()
}

fn capture_envelope(id: u64, params: &str) -> Vec<u8> {
    format!(
        "{{\"id\":{id},\"method\":\"{METHOD_CAPTURE_FRAME}\",\"version\":\"1.0\",\"params\":{params}}}"
    )
    .into_bytes()
}

// ── frameHash digest (CTX-0244) ─────────────────────────────────────────
//
// Digest equality vs local computation, full denial matrix, TTL cap,
// 2/s rate ceiling, fail-closed publish validation, bounded audit with
// served-digest content, and no-bypass isolation in both family
// directions. Every test holds the serial guard and clears automation
// plus introspection (incl. the RGBA store) before and after. All
// content is synthetic fixture bytes — never secrets (P0-AC-026
// harness rule).

fn digest_envelope(id: u64, params: &str) -> Vec<u8> {
    format!(
        "{{\"id\":{id},\"method\":\"{METHOD_FRAME_HASH}\",\"version\":\"1.0\",\"params\":{params}}}"
    )
    .into_bytes()
}

fn digest_context(
    server: &ServerInfo,
    granted: crate::scope::ScopeSet,
    session: &str,
    now_ms: u64,
) -> ServeContext {
    // Attested like the production accept boundary
    // (`transport_attested_peer` + `attest_local_peer`): same-process
    // in-process dispatch is local by construction.
    let mut ctx = automation_context(server, granted, session, now_ms);
    ctx.attest_local_peer();
    ctx
}

/// Deterministic synthetic RGBA fixture (never secrets): a gradient
/// over `w*h*4` bytes with an ASCII marker row to prove no pixel bytes
/// reach the response.
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
    // ASCII marker the response must never contain (uninvertibility
    // spot-check, not a proof — the proof is the 32-byte digest).
    let marker = b"FRAMEHASH-MARKER-NEVER-ON-WIRE";
    let at = len.min(256);
    for (i, b) in marker.iter().enumerate() {
        if at + i < len {
            rgba[at + i] = *b;
        }
    }
    rgba
}

fn response_text(outcome: &HandleOutcome) -> String {
    String::from_utf8(outcome.response.clone()).unwrap()
}

#[test]
fn frame_hash_digest_equals_local_computation_across_geometries() {
    use crate::frame_digest::{FRAME_DIGEST_ALGO, frame_digest_hex};
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    // Three geometries (tiny, odd-sized, and multi-kilopixel) with
    // distinct seeds and frame sequences.
    for (id, (w, h, seq, seed)) in [(80u32, 60u32, 9u64, 1u8), (17, 5, 41, 2), (320, 200, 7, 3)]
        .into_iter()
        .enumerate()
    {
        let rgba = fixture_rgba(w, h, seed);
        publish_frame_rgba(w, h, seq, rgba.clone());
        publish_grid_text(vec!["synthetic".to_string()], 0, 0, true, seq, 80, 24);
        let tok = issue_automation_bearer_with_ttl(
            "digest-eq",
            "t:1",
            AutomationFamily::FrameDigest,
            0,
            60_000,
        )
        .unwrap();
        let ctx = digest_context(&server, automation_scopes_capture(), "digest-eq", 0);
        let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(id as u64 + 1, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "geometry {w}x{h} must verify");
        let text = response_text(&outcome);
        let expect = frame_digest_hex(w, h, seq, &rgba);
        assert!(text.contains("\"snapshot\":\"frameHash\""), "got: {text}");
        assert!(
            text.contains(&format!("\"algo\":\"{FRAME_DIGEST_ALGO}\"")),
            "got: {text}"
        );
        assert!(
            text.contains(&format!("\"digest\":\"{expect}\"")),
            "digest mismatch at {w}x{h}: {text}"
        );
        assert!(text.contains(&format!("\"frameSeq\":{seq}")), "got: {text}");
        assert!(
            text.contains("\"trust\":\"untrusted-observation\""),
            "got: {text}"
        );
        assert!(text.len() < 512, "digest response must stay tiny: {text}");
        assert!(
            !text.contains("FRAMEHASH-MARKER-NEVER-ON-WIRE"),
            "pixel bytes reached the wire: {text}"
        );
        clear_automation_for_tests();
    }
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_auth_matrix_denies_everything_unauthorized() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    publish_frame_rgba(8, 8, 3, fixture_rgba(8, 8, 9));
    let digest_tok =
        issue_automation_bearer_with_ttl("m", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
            .unwrap();
    let capture_tok = issue_automation_bearer("m", "t:1", AutomationFamily::Capture, 0).unwrap();

    // (label, scopes, session, terminal-in-params, token, now, attest)
    let full = automation_scopes_capture();
    let mut no_trace = automation_scopes_capture();
    no_trace.remove(crate::scope::Scope::DebugTrace);
    let mut no_inspect = automation_scopes_capture();
    no_inspect.remove(crate::scope::Scope::TerminalInspect);
    let cases: Vec<(&str, crate::scope::ScopeSet, &str, &str, String, u64, bool)> = vec![
        (
            "no-bearer",
            full.clone(),
            "m",
            "t:1",
            String::new(),
            0,
            true,
        ),
        (
            "wrong-family-capture-token",
            full.clone(),
            "m",
            "t:1",
            capture_tok.clone(),
            0,
            true,
        ),
        (
            "wrong-terminal",
            full.clone(),
            "m",
            "t:2",
            digest_tok.clone(),
            0,
            true,
        ),
        (
            "wrong-session",
            full.clone(),
            "other",
            "t:1",
            digest_tok.clone(),
            0,
            true,
        ),
        (
            "missing-debug-trace",
            no_trace,
            "m",
            "t:1",
            digest_tok.clone(),
            0,
            true,
        ),
        (
            "missing-terminal-inspect",
            no_inspect,
            "m",
            "t:1",
            digest_tok.clone(),
            0,
            true,
        ),
        (
            "unattested-transport",
            full.clone(),
            "m",
            "t:1",
            digest_tok.clone(),
            0,
            false,
        ),
        (
            "forged-token-full-scopes",
            crate::scope::ScopeSet::all(),
            "m",
            "t:1",
            "forged-token".to_string(),
            0,
            true,
        ),
    ];
    for (label, scopes, session, term, token, now, attest) in cases {
        let mut ctx = automation_context(&server, scopes, session, now);
        if attest {
            ctx.attest_local_peer();
        }
        let params = if token.is_empty() {
            format!("{{\"terminalId\":\"{term}\"}}")
        } else {
            format!("{{\"terminalId\":\"{term}\",\"bearer\":\"{token}\"}}")
        };
        let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
        assert!(outcome.was_error, "{label} must fail");
        assert!(
            response_text(&outcome).contains("ScopeDenied"),
            "{label}: bearer-vs-scope failures must share the ScopeDenied shape, got: {}",
            response_text(&outcome)
        );
    }
    // Malformed shape fails closed without attribution (no audit needed).
    let ctx = digest_context(&server, automation_scopes_capture(), "m", 0);
    let outcome = handle_envelope(&digest_envelope(2, "{}"), &dispatcher, &ctx);
    assert!(response_text(&outcome).contains("InvalidParams"));
    let outcome = handle_envelope(
        &digest_envelope(3, r#"{"terminalId":"t:*"}"#),
        &dispatcher,
        &ctx,
    );
    assert!(response_text(&outcome).contains("InvalidParams"));
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_ttl_capped_at_two_minutes_and_expiry_revokes() {
    use crate::frame_digest::FRAME_DIGEST_TTL_MS;
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    assert_eq!(FRAME_DIGEST_TTL_MS, 120_000);
    // The default 10-minute minter refuses digest grants fail-closed:
    // a digest TTL must be explicit.
    assert!(issue_automation_bearer("ttl", "t:1", AutomationFamily::FrameDigest, 0).is_err());
    // Zero and over-cap TTLs fail closed.
    assert!(
        issue_automation_bearer_with_ttl("ttl", "t:1", AutomationFamily::FrameDigest, 0, 0)
            .is_err()
    );
    assert!(
        issue_automation_bearer_with_ttl(
            "ttl",
            "t:1",
            AutomationFamily::FrameDigest,
            0,
            FRAME_DIGEST_TTL_MS + 1
        )
        .is_err()
    );
    // Cap edge issues; sibling families still enjoy the 10-minute cap.
    let tok = issue_automation_bearer_with_ttl(
        "ttl",
        "t:1",
        AutomationFamily::FrameDigest,
        500,
        FRAME_DIGEST_TTL_MS,
    )
    .unwrap();
    assert!(
        issue_automation_bearer_with_ttl(
            "ttl",
            "t:1",
            AutomationFamily::Synthesize,
            0,
            AUTOMATION_BEARER_TTL_MS
        )
        .is_ok()
    );
    // Grant valid at issuance, denied exactly at issue+TTL
    // (virtual clock: no sleeps, no wall-clock).
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    publish_frame_rgba(4, 4, 1, fixture_rgba(4, 4, 5));
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
    let ctx = digest_context(&server, automation_scopes_capture(), "ttl", 500);
    let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error, "grant must verify at issuance");
    let ctx = digest_context(
        &server,
        automation_scopes_capture(),
        "ttl",
        500 + FRAME_DIGEST_TTL_MS,
    );
    let outcome = handle_envelope(&digest_envelope(2, &params), &dispatcher, &ctx);
    assert!(outcome.was_error);
    assert!(
        response_text(&outcome).contains("ScopeDenied"),
        "expired digest grant must deny: {}",
        response_text(&outcome)
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_rate_sheds_third_digest_per_second() {
    use crate::frame_digest::MAX_FRAME_DIGEST_PER_SEC;
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    assert_eq!(MAX_FRAME_DIGEST_PER_SEC, 2);
    publish_frame_rgba(4, 4, 1, fixture_rgba(4, 4, 5));
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok =
        issue_automation_bearer_with_ttl("rl", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
            .unwrap();
    let ctx = digest_context(&server, automation_scopes_capture(), "rl", 0);
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
    for id in 1..=MAX_FRAME_DIGEST_PER_SEC as u64 {
        let outcome = handle_envelope(&digest_envelope(id, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "digest {id} must pass under ceiling");
    }
    let outcome = handle_envelope(
        &digest_envelope(MAX_FRAME_DIGEST_PER_SEC as u64 + 1, &params),
        &dispatcher,
        &ctx,
    );
    assert!(outcome.was_error);
    assert!(
        response_text(&outcome).contains("RateLimited"),
        "third digest in one window must shed: {}",
        response_text(&outcome)
    );
    // Window slides: one second later the ceiling admits again.
    let ctx = digest_context(&server, automation_scopes_capture(), "rl", 1_000);
    let outcome = handle_envelope(&digest_envelope(9, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error, "window must slide after 1 s");
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_unavailable_without_presented_frame_and_rejects_bad_publish() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok =
        issue_automation_bearer_with_ttl("np", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
            .unwrap();
    // Authorized-but-unavailable calls still consume the rate budget
    // (authorize runs first), so each probe advances the virtual clock.
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
    // Never a hash of nothing: empty store reads as indeterminate.
    let ctx = digest_context(&server, automation_scopes_capture(), "np", 0);
    let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
    assert!(outcome.was_error);
    assert!(
        response_text(&outcome).contains("Unavailable"),
        "got: {}",
        response_text(&outcome)
    );
    // Fail-closed publish validation: zero extents, length mismatch,
    // and over-cap frames never become digestable.
    publish_frame_rgba(0, 8, 1, vec![0u8; 32]);
    publish_frame_rgba(8, 8, 1, vec![0u8; 8 * 8 * 4 - 1]);
    publish_frame_rgba(8, 8, 1, vec![0u8; 8 * 8 * 4 + 1]);
    publish_frame_rgba(u32::MAX, u32::MAX, 1, vec![0u8; 16]);
    let ctx = digest_context(&server, automation_scopes_capture(), "np", 1_000);
    let outcome = handle_envelope(&digest_envelope(2, &params), &dispatcher, &ctx);
    assert!(
        response_text(&outcome).contains("Unavailable"),
        "bad publishes must not become digestable: {}",
        response_text(&outcome)
    );
    // A valid publish after bad ones still verifies (drop, not poison).
    publish_frame_rgba(8, 8, 2, fixture_rgba(8, 8, 1));
    let ctx = digest_context(&server, automation_scopes_capture(), "np", 2_000);
    let outcome = handle_envelope(&digest_envelope(3, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error, "valid publish must verify");
    assert!(
        response_text(&outcome).contains("\"frameSeq\":2"),
        "got: {}",
        response_text(&outcome)
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_audit_covers_granted_and_denied_and_stays_bounded() {
    use crate::frame_digest::frame_digest_hex;
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let rgba = fixture_rgba(8, 8, 4);
    publish_frame_rgba(8, 8, 55, rgba.clone());
    let tok =
        issue_automation_bearer_with_ttl("au", "t:1", AutomationFamily::FrameDigest, 0, 120_000)
            .unwrap();
    // Granted call appends a digest entry carrying the served digest.
    let before = frame_audit_len_for_tests();
    let ctx = digest_context(&server, automation_scopes_capture(), "au", 0);
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\"}}");
    let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error);
    let text = response_text(&outcome);
    let served = frame_digest_hex(8, 8, 55, &rgba);
    assert!(text.contains(&served));
    assert_eq!(frame_audit_len_for_tests(), before + 1);
    let snap = frame_audit_snapshot_for_tests();
    let entry = snap.last().unwrap();
    assert_eq!(entry.format, "digest");
    assert_eq!(entry.session_id, "au");
    assert_eq!(entry.terminal_id, "t:1");
    assert_eq!(entry.frame_seq, 55);
    assert_eq!(entry.digest_hex, served);
    // Denied calls append too (attributable terminal): wrong-terminal
    // denials are unbounded by rate (auth fails first), so 65 of them
    // prove the 64-entry drop-oldest bound deterministically.
    for id in 2..=66u64 {
        let bad = format!("{{\"terminalId\":\"t:9\",\"bearer\":\"{tok}\"}}");
        let outcome = handle_envelope(&digest_envelope(id, &bad), &dispatcher, &ctx);
        assert!(outcome.was_error);
    }
    assert_eq!(frame_audit_len_for_tests(), MAX_AUTOMATION_BEARERS);
    let snap = frame_audit_snapshot_for_tests();
    assert!(snap.iter().all(|e| e.format == "digest"));
    assert!(
        snap.iter().all(|e| e.digest_hex.is_empty()),
        "denied entries carry no digest"
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_hash_family_isolation_holds_in_both_directions() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    publish_frame_rgba(8, 8, 1, fixture_rgba(8, 8, 6));
    publish_grid_text(vec!["ok".to_string()], 0, 0, true, 1, 80, 24);
    let digest_tok =
        issue_automation_bearer_with_ttl("iso", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
            .unwrap();
    let capture_tok = issue_automation_bearer("iso", "t:1", AutomationFamily::Capture, 0).unwrap();
    let ctx = digest_context(&server, automation_scopes_capture(), "iso", 0);
    // Capture bearer on frameHash: denied (never widened).
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{capture_tok}\"}}");
    let outcome = handle_envelope(&digest_envelope(1, &params), &dispatcher, &ctx);
    assert!(response_text(&outcome).contains("ScopeDenied"));
    // Digest bearer on captureFrame: denied (never widened).
    let params =
        format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\",\"format\":\"semantic\"}}");
    let outcome = handle_envelope(&capture_envelope(2, &params), &dispatcher, &ctx);
    assert!(
        response_text(&outcome).contains("ScopeDenied"),
        "digest bearer must not capture: {}",
        response_text(&outcome)
    );
    // Digest bearer on frameHash: verifies (control case).
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\"}}");
    let outcome = handle_envelope(&digest_envelope(3, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error);
    // No-bypass: even every scope granted, a forged digest token denies,
    // and revocation takes effect immediately.
    let all = digest_context(&server, crate::scope::ScopeSet::all(), "iso", 0);
    let params = r#"{"terminalId":"t:1","bearer":"forged-digest-token"}"#;
    let outcome = handle_envelope(&digest_envelope(4, params), &dispatcher, &all);
    assert!(response_text(&outcome).contains("ScopeDenied"));
    assert!(revoke_automation_bearer(&digest_tok));
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{digest_tok}\"}}");
    let outcome = handle_envelope(&digest_envelope(5, &params), &dispatcher, &ctx);
    assert!(response_text(&outcome).contains("ScopeDenied"));
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn frame_digest_publish_gate_arms_only_with_live_grant() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    assert!(!frame_digest_publish_wanted());
    let synth = issue_automation_bearer("gate", "t:1", AutomationFamily::Synthesize, 0).unwrap();
    let cap = issue_automation_bearer("gate", "t:1", AutomationFamily::Capture, 0).unwrap();
    // Other families never arm the RGBA publish path.
    assert!(!frame_digest_publish_wanted());
    let digest =
        issue_automation_bearer_with_ttl("gate", "t:1", AutomationFamily::FrameDigest, 0, 60_000)
            .unwrap();
    assert!(frame_digest_publish_wanted());
    assert!(revoke_automation_bearer(&digest));
    assert!(!frame_digest_publish_wanted());
    assert!(revoke_automation_bearer(&synth));
    assert!(revoke_automation_bearer(&cap));
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_unscoped_synthesize_is_scope_denied_no_partial_state() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let seq_before = synthetic_seq_for_tests();
    // Empty scopes + no bearer: fail fast ScopeDenied.
    let ctx = automation_context(&server, crate::scope::ScopeSet::new(), "s1", 1000);
    let outcome = handle_envelope(
        &synth_envelope(
            1,
            r#"{"terminalId":"t:1","bearer":"nope","originLabel":"harness","events":[{"type":"key","key":"a"}]}"#,
        ),
        &dispatcher,
        &ctx,
    );
    assert!(outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("ScopeDenied"), "got: {text}");
    assert_eq!(synthetic_seq_for_tests(), seq_before);
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_bearer_matrix_fails_closed() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let synth_scopes = automation_scopes_synthesize();
    let capture_scopes = automation_scopes_capture();
    // Issue valid bearers at t=1000.
    let synth_tok =
        issue_automation_bearer("sess-a", "t:1", AutomationFamily::Synthesize, 1000).unwrap();
    let capture_tok =
        issue_automation_bearer("sess-a", "t:1", AutomationFamily::Capture, 1000).unwrap();
    // Absent bearer.
    let ctx = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
    let outcome = handle_envelope(
        &synth_envelope(
            1,
            r#"{"terminalId":"t:1","originLabel":"h","events":[{"type":"key","key":"a"}]}"#,
        ),
        &dispatcher,
        &ctx,
    );
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    assert!(outcome.was_error);
    // Wrong-session bearer.
    let ctx_other = automation_context(&server, synth_scopes.clone(), "sess-b", 1000);
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx_other);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // Wrong-terminal bearer.
    let ctx = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
    let params = format!(
        "{{\"terminalId\":\"t:2\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(3, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // Wrong-family bearer (capture token on synthesize).
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{capture_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(4, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // Expired bearer.
    let mut ctx_expired = automation_context(&server, synth_scopes.clone(), "sess-a", 1000);
    ctx_expired.uptime_ms = 1000 + AUTOMATION_BEARER_TTL_MS;
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{synth_tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(5, &params), &dispatcher, &ctx_expired);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("ScopeDenied"), "expired must deny: {text}");
    // Capture wrong-family (synthesize token on capture): re-issue a live
    // synthesize token to prove family mismatch distinctly (the earlier
    // synth token was consumed by the expiry check above).
    publish_grid_text(vec!["hello".to_string()], 0, 5, true, 3, 80, 24);
    let ctx_cap = automation_context(&server, capture_scopes, "sess-a", 1000);
    let live_synth =
        issue_automation_bearer("sess-a", "t:1", AutomationFamily::Synthesize, 1000).unwrap();
    let params2 =
        format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{live_synth}\",\"format\":\"semantic\"}}");
    let outcome = handle_envelope(&capture_envelope(6, &params2), &dispatcher, &ctx_cap);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    let _ = capture_tok;
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_scope_intersection_requires_debug_plus_terminal() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 500).unwrap();
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    // DebugControl alone (missing TerminalInput) denies.
    let mut only_debug = crate::scope::ScopeSet::new();
    only_debug.insert(crate::scope::Scope::DebugControl);
    let ctx = automation_context(&server, only_debug, "s1", 500);
    let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // TerminalInput alone (missing DebugControl) denies.
    let mut only_term = crate::scope::ScopeSet::new();
    only_term.insert(crate::scope::Scope::TerminalInput);
    let ctx = automation_context(&server, only_term, "s1", 500);
    let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // Capture: DebugTrace alone denies, TerminalInspect alone denies.
    let cap_tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 500).unwrap();
    publish_grid_text(vec!["x".to_string()], 0, 1, true, 1, 80, 24);
    let cap_params =
        format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{cap_tok}\",\"format\":\"semantic\"}}");
    let mut only_trace = crate::scope::ScopeSet::new();
    only_trace.insert(crate::scope::Scope::DebugTrace);
    let ctx = automation_context(&server, only_trace, "s1", 500);
    let outcome = handle_envelope(&capture_envelope(3, &cap_params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_synthesize_bounds_fail_closed() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
    let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
    // Zero events.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[]}}"
    );
    let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // 65 events exceeds the 64/call ceiling.
    let many: Vec<String> = (0..65)
        .map(|_| "{\"type\":\"key\",\"key\":\"a\"}".to_string())
        .collect();
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{}]}}",
        many.join(",")
    );
    let outcome = handle_envelope(&synth_envelope(2, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // Wildcard terminal fails closed (exactly one t:N per call).
    let params = format!(
        "{{\"terminalId\":\"*\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(3, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // Unknown event type.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"teleport\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(4, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // Bad mouse button.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"mouse\",\"button\":\"Side\",\"action\":\"click\",\"col\":1,\"row\":1}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(5, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // Zero wheel delta.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"wheel\",\"deltaRows\":0}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(6, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // Paste with NUL.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"paste\",\"text\":\"a\\u0000b\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(7, &params), &dispatcher, &ctx);
    assert!(outcome.was_error);
    // Missing originLabel.
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(8, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_synthesize_success_marks_synthetic_origin() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok =
        issue_automation_bearer("harness", "t:1", AutomationFamily::Synthesize, 2000).unwrap();
    let ctx = automation_context(&server, automation_scopes_synthesize(), "harness", 2000);
    let seq_before = synthetic_seq_for_tests();
    let params = format!(
        concat!(
            "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"e2e-harness\",",
            "\"events\":[{{\"type\":\"key\",\"key\":\"Enter\",\"pressed\":true}},",
            "{{\"type\":\"mouse\",\"button\":\"Left\",\"action\":\"click\",\"col\":10,\"row\":5}},",
            "{{\"type\":\"wheel\",\"deltaRows\":-3}},",
            "{{\"type\":\"paste\",\"text\":\"echo hi\"}}]}}"
        ),
        tok = tok
    );
    let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"accepted\":4"), "got: {text}");
    assert!(text.contains("\"rejected\":0"), "got: {text}");
    assert!(text.contains("\"synthetic\":true"), "got: {text}");
    assert!(synthetic_seq_for_tests() > seq_before);
    // Input-ring observability carries the indelible synthetic marker.
    let ring_ctx = automation_context(
        &server,
        crate::scope::ScopeSet::cli_default(),
        "harness",
        2000,
    );
    let _ = ring_ctx;
    let guard = live_input_store().lock().unwrap();
    assert!(guard.len() >= 4);
    assert!(
        guard
            .iter()
            .all(|e| e.label.contains("[synthetic:e2e-harness]"))
    );
    drop(guard);
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_synthesize_rate_sheds_with_budget_error() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
    let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"load\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    for id in 1..=MAX_SYNTH_CALLS_PER_SEC as u64 {
        let outcome = handle_envelope(&synth_envelope(id, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "call {id} must pass under ceiling");
    }
    let outcome = handle_envelope(
        &synth_envelope(MAX_SYNTH_CALLS_PER_SEC as u64 + 1, &params),
        &dispatcher,
        &ctx,
    );
    assert!(outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("RateLimited"), "got: {text}");
    // A different bearer (benign concurrent session) is unaffected.
    let tok2 = issue_automation_bearer("s2", "t:2", AutomationFamily::Synthesize, 0).unwrap();
    let ctx2 = automation_context(&server, automation_scopes_synthesize(), "s2", 0);
    let params2 = format!(
        "{{\"terminalId\":\"t:2\",\"bearer\":\"{tok2}\",\"originLabel\":\"load\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(100, &params2), &dispatcher, &ctx2);
    assert!(!outcome.was_error);
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_capture_semantic_redacts_and_labels_untrusted() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_grid_text(
        vec![
            "$ echo hello".to_string(),
            "hello".to_string(),
            "DB_PASSWORD=hunter2".to_string(),
            "clipboard bytes leak".to_string(),
        ],
        1,
        5,
        true,
        9,
        80,
        24,
    );
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 7000).unwrap();
    let ctx = automation_context(&server, automation_scopes_capture(), "s1", 7000);
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"semantic\"}}");
    let outcome = handle_envelope(&capture_envelope(1, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"snapshot\":\"frame\""), "got: {text}");
    assert!(
        text.contains("\"trust\":\"untrusted-observation\""),
        "got: {text}"
    );
    assert!(text.contains("hello"), "got: {text}");
    assert!(!text.contains("hunter2"), "secret leaked: {text}");
    assert!(
        !text.contains("clipboard bytes leak"),
        "clipboard leaked: {text}"
    );
    assert!(text.contains(REDACTED_MARKER), "got: {text}");
    assert!(text.contains("\"frameSeq\":9"), "got: {text}");
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_capture_pixels_requires_opt_in_masks_and_audits() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_grid_text(vec!["SECRET=topsecret".to_string()], 0, 1, true, 11, 80, 24);
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 8000).unwrap();
    let ctx = automation_context(&server, automation_scopes_capture(), "s1", 8000);
    // Without explicit opt-in: fail closed.
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"pixels\"}}");
    let outcome = handle_envelope(&capture_envelope(1, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("InvalidParams")
    );
    // With opt-in: masked record, zero text, audited caller.
    let audit_before = frame_audit_len_for_tests();
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"pixels\",\"explicitOptIn\":true}}"
    );
    let outcome = handle_envelope(&capture_envelope(2, &params), &dispatcher, &ctx);
    assert!(!outcome.was_error);
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(text.contains("\"format\":\"pixels\""), "got: {text}");
    assert!(text.contains("\"masked\":true"), "got: {text}");
    assert!(text.contains("\"audited\":true"), "got: {text}");
    assert!(text.contains("\"caller\":\"s1\""), "got: {text}");
    assert!(!text.contains("topsecret"), "pixels leaked text: {text}");
    assert!(
        !text.contains("\"lines\""),
        "pixels must carry zero text: {text}"
    );
    assert!(frame_audit_len_for_tests() > audit_before);
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_capture_rate_sheds_at_fps_ceiling() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    publish_grid_text(vec!["f".to_string()], 0, 1, true, 1, 80, 24);
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Capture, 0).unwrap();
    let ctx = automation_context(&server, automation_scopes_capture(), "s1", 0);
    let params = format!("{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"format\":\"semantic\"}}");
    for id in 1..=MAX_CAPTURE_FPS as u64 {
        let outcome = handle_envelope(&capture_envelope(id, &params), &dispatcher, &ctx);
        assert!(!outcome.was_error, "frame {id} must pass under ceiling");
    }
    let outcome = handle_envelope(
        &capture_envelope(MAX_CAPTURE_FPS as u64 + 1, &params),
        &dispatcher,
        &ctx,
    );
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("RateLimited")
    );
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_bearer_lifecycle_revoke_ttl_cap_and_no_env_issuance() {
    let _guard = lock_introspection_for_test();
    clear_introspection_for_tests();
    clear_automation_for_tests();
    // TTL bounds hold fail-closed.
    assert!(
        issue_automation_bearer_with_ttl("s", "t:1", AutomationFamily::Synthesize, 0, 0).is_err()
    );
    assert!(
        issue_automation_bearer_with_ttl(
            "s",
            "t:1",
            AutomationFamily::Synthesize,
            0,
            AUTOMATION_BEARER_TTL_MS + 1
        )
        .is_err()
    );
    // Revocation takes effect immediately.
    let server = test_server_info();
    let dispatcher = Dispatcher::with_defaults();
    let tok = issue_automation_bearer("s1", "t:1", AutomationFamily::Synthesize, 0).unwrap();
    assert!(revoke_automation_bearer(&tok));
    let ctx = automation_context(&server, automation_scopes_synthesize(), "s1", 0);
    let params = format!(
        "{{\"terminalId\":\"t:1\",\"bearer\":\"{tok}\",\"originLabel\":\"h\",\"events\":[{{\"type\":\"key\",\"key\":\"a\"}}]}}"
    );
    let outcome = handle_envelope(&synth_envelope(1, &params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // No-bypass: elevation alone never substitutes for a bearer. Even
    // with every scope granted, a forged token still fails closed, and
    // issuance has no env/config/flag path (code inspection: only
    // `issue_automation_bearer` inserts into the memory-only store).
    let ctx = automation_context(&server, crate::scope::ScopeSet::all(), "s1", 0);
    let params = r#"{"terminalId":"t:1","bearer":"forged-token","originLabel":"h","events":[{"type":"key","key":"a"}]}"#;
    let outcome = handle_envelope(&synth_envelope(2, params), &dispatcher, &ctx);
    assert!(
        String::from_utf8(outcome.response)
            .unwrap()
            .contains("ScopeDenied")
    );
    // Bearers are never persisted: the store is memory-only.
    assert_eq!(automation_bearer_count_for_tests(), 0);
    clear_automation_for_tests();
    clear_introspection_for_tests();
}

#[test]
fn automation_params_two_tier_envelope_bound() {
    // Automation methods admit larger params than the 4 KiB
    // introspection bound (up to 32 KiB); other methods stay capped.
    let big_pad = "p".repeat(MAX_PARAMS_BYTES);
    let auto_payload = format!(
        "{{\"id\":1,\"method\":\"{METHOD_SYNTHESIZE_INPUT}\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{big_pad}\"}}}}"
    );
    assert!(parse_request(auto_payload.as_bytes()).is_ok());
    let plain_payload = format!(
        "{{\"id\":1,\"method\":\"bitty.debug/getGridText\",\"version\":\"1.0\",\"params\":{{\"pad\":\"{big_pad}\"}}}}"
    );
    assert!(parse_request(plain_payload.as_bytes()).is_err());
}
