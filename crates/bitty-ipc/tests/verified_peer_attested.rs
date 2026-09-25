#![cfg(unix)]

//! CTX-0656: `VerifiedPeer` attested-constructor hardening (SEC-08 follow-up).
//!
//! The real platform adapter obtains peer credentials from the connected Unix
//! stream and the serving context binds that stream before dispatch. These
//! tests also keep endpoint-only checks as negative fixtures; they never grant
//! dispatch authority.
//!
//! Headless and network-free: no sockets are bound except temporary Unix
//! endpoints under the process temp dir, no extra threads, no wall-clock.

use bitty_ipc::devtools::{
    Dispatcher, METHOD_FRAME_HASH, ServeContext, ServerInfo, handle_envelope,
};
#[cfg(unix)]
use bitty_ipc::devtools::{prepare_socket_dir, verify_socket_endpoint_for_connect};
use bitty_ipc::error::IpcError;
use bitty_ipc::scope::ScopeSet;

// ── missing attestation ─────────────────────────────────────────────────────

fn test_server() -> ServerInfo {
    ServerInfo::new(
        "attested-tests".to_string(),
        "attested.sock".to_string(),
        80,
        24,
    )
}

#[test]
fn contexts_default_unattested_and_require_proof() {
    let server = test_server();
    // Every constructor defaults to unattested (fail-closed, no default-allow).
    assert!(!ServeContext::new(&server).is_local_attested());
    assert!(!ServeContext::with_granted(&server, ScopeSet::all()).is_local_attested());
    assert!(!ServeContext::with_granted_session(&server, ScopeSet::all(), "m").is_local_attested());
    let (_client, stream) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    let proof = ctx.bind_connected_stream_current(&stream).unwrap();
    ctx.attest_local_peer(&proof.identity());
    assert!(ctx.is_local_attested());
}

#[test]
fn frame_hash_denies_without_attestation() {
    let server = test_server();
    let dispatcher = Dispatcher::with_defaults();
    let params = r#"{"terminalId":"t:1","bearer":"ctx-0656-never-issued-bearer"}"#;
    let envelope = format!(
        "{{\"id\":1,\"method\":\"{METHOD_FRAME_HASH}\",\"version\":\"1.0\",\"params\":{params}}}"
    );
    // Missing connected proof denies before the handler runs.
    let ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    assert!(!ctx.is_local_attested());
    let outcome = handle_envelope(envelope.as_bytes(), &dispatcher, &ctx);
    assert!(outcome.was_error, "unbound frameHash must fail");
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(
        text.contains("Unauthenticated") && text.contains("peer recheck"),
        "unbound denial must name the peer gate, got: {text}"
    );
    // With a real bound peer the transport gate passes and failure moves
    // downstream to the bearer check.
    let (_client, stream) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut attested_ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    let proof = attested_ctx.bind_connected_stream_current(&stream).unwrap();
    attested_ctx.attest_local_peer(&proof.identity());
    let outcome = handle_envelope(envelope.as_bytes(), &dispatcher, &attested_ctx);
    assert!(outcome.was_error, "forged bearer must still fail");
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(
        text.contains("ScopeDenied") && !text.contains("attested transport"),
        "attested denial must pass the transport gate, got: {text}"
    );
}

// ── forged / missing transport attestation (Unix) ───────────────────────────

#[cfg(unix)]
fn temp_socket_path(tag: &str) -> String {
    let pid = std::process::id();
    let path = format!("/tmp/bt{pid}{tag}/s.sock");
    assert!(path.len() < 100, "socket path must fit SUN_LEN: {path}");
    path
}

#[cfg(unix)]
fn endpoint_owner_uid(socket_path: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(socket_path).unwrap().uid()
}

#[cfg(unix)]
fn bind_owned_endpoint(tag: &str) -> (String, std::os::unix::net::UnixListener) {
    use std::os::unix::fs::PermissionsExt;
    let socket_path = temp_socket_path(tag);
    let _dir = prepare_socket_dir(&socket_path).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (socket_path, listener)
}

#[cfg(unix)]
#[test]
fn transport_attests_owned_endpoint_and_binds_uid() {
    let (socket_path, listener) = bind_owned_endpoint("ok");
    let euid = endpoint_owner_uid(&socket_path);
    let endpoint = verify_socket_endpoint_for_connect(&socket_path, euid).unwrap();
    assert_eq!(endpoint.uid, euid);
    let client = std::os::unix::net::UnixStream::connect(&socket_path).unwrap();
    let (server, _) = listener.accept().unwrap();
    let mut context = ServeContext::new(&test_server());
    let proof = context.bind_connected_stream(&server, euid).unwrap();
    assert_eq!(proof.identity().peer_uid(), euid);
    drop(client);
    drop(server);
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}ok", std::process::id()));
}

#[cfg(unix)]
#[test]
fn transport_rejects_missing_endpoint() {
    // Missing attestation material fails closed with no marker: empty and
    // NUL paths are rejected as malformed, absent files as unavailable.
    let missing = temp_socket_path("no");
    assert!(verify_socket_endpoint_for_connect(&missing, 1000).is_err());
    assert!(verify_socket_endpoint_for_connect("", 1000).is_err());
    assert!(verify_socket_endpoint_for_connect("bad\0path", 1000).is_err());
    let err = verify_socket_endpoint_for_connect(&missing, 1000).unwrap_err();
    assert!(
        matches!(
            err,
            IpcError::Unavailable { .. } | IpcError::InvalidRequest { .. }
        ),
        "missing endpoint must fail closed, got {err:?}"
    );
}

#[cfg(unix)]
#[test]
fn transport_rejects_forged_endpoint() {
    use std::os::unix::fs::PermissionsExt;
    // Symlinked socket: forged endpoint, refused before any byte is read.
    let (target_path, target_listener) = bind_owned_endpoint("sl");
    let link_path = temp_socket_path("sl-link");
    std::fs::create_dir_all(std::path::Path::new(&link_path).parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target_path, &link_path).unwrap();
    let euid = endpoint_owner_uid(&target_path);
    let err = verify_socket_endpoint_for_connect(&link_path, euid).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "symlinked endpoint must fail closed, got {err:?}"
    );
    drop(target_listener);
    let _ = std::fs::remove_file(&link_path);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}sl", std::process::id()));
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}sl-link", std::process::id()));

    // Group/world-readable directory: forged endpoint, refused.
    let (socket_path, listener) = bind_owned_endpoint("dm");
    let parent = std::path::Path::new(&socket_path)
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let euid = endpoint_owner_uid(&socket_path);
    let err = verify_socket_endpoint_for_connect(&socket_path, euid).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "readable dir must fail closed, got {err:?}"
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}dm", std::process::id()));

    // Group-readable socket: forged endpoint, refused.
    let (socket_path, listener) = bind_owned_endpoint("sm");
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let euid = endpoint_owner_uid(&socket_path);
    let err = verify_socket_endpoint_for_connect(&socket_path, euid).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "readable socket must fail closed, got {err:?}"
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}sm", std::process::id()));

    // Foreign runtime UID against a well-formed endpoint: the UID check
    // inside the attested constructor refuses, so no marker is minted.
    let (socket_path, listener) = bind_owned_endpoint("fu");
    let euid = endpoint_owner_uid(&socket_path);
    let foreign = euid.wrapping_add(1);
    let err = verify_socket_endpoint_for_connect(&socket_path, foreign).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "foreign runtime uid must fail closed, got {err:?}"
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}fu", std::process::id()));
}
