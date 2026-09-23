//! CTX-0656: `VerifiedPeer` attested-constructor hardening (SEC-08 follow-up).
//!
//! The attested constructor is crate-private and fallible: the only public
//! paths to a `VerifiedPeer` run UID-equality verification first
//! ([`verify_peer_for_connection`] for peer-credential triples,
//! [`transport_attested_peer`] for the kernel-gated owner-only transport).
//! These tests prove, through the public API only, that forged attestation
//! mints no marker and missing attestation denies fail-closed.
//!
//! Headless and network-free: no sockets are bound except temporary Unix
//! endpoints under the process temp dir, no extra threads, no wall-clock.

use bitty_ipc::auth::{PeerCredentials, verify_peer_for_connection, verify_peer_uid};
use bitty_ipc::devtools::{
    Dispatcher, METHOD_FRAME_HASH, ServeContext, ServerInfo, handle_envelope,
};
#[cfg(unix)]
use bitty_ipc::devtools::{prepare_socket_dir, transport_attested_peer};
use bitty_ipc::error::{ErrorClass, IpcError};
use bitty_ipc::scope::ScopeSet;

const RUNTIME_UID: u32 = 1000;
const FOREIGN_UID: u32 = 2000;

fn expect_unauthenticated(result: Result<(), IpcError>, what: &str) {
    let err = result.unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "{what}: expected Unauthenticated, got {err:?}"
    );
    assert_eq!(
        err.error_class(),
        ErrorClass::Unauthenticated,
        "{what}: wrong error class"
    );
}

// ── forged attestation ──────────────────────────────────────────────────────

#[test]
fn forged_uid_mints_no_marker_at_headless_gate() {
    // A foreign UID mints no marker: rejected before any byte is read.
    let foreign = PeerCredentials::new(FOREIGN_UID, FOREIGN_UID, 99);
    expect_unauthenticated(
        verify_peer_for_connection(foreign, RUNTIME_UID).map(|_| ()),
        "foreign uid",
    );
    expect_unauthenticated(verify_peer_uid(foreign, RUNTIME_UID), "foreign uid");
    // Off-by-one, zero, and sentinel UIDs are forgeries too.
    for tampered in [RUNTIME_UID - 1, RUNTIME_UID + 1, 0, u32::MAX] {
        let peer = PeerCredentials::new(tampered, RUNTIME_UID, 42);
        assert!(
            verify_peer_for_connection(peer, RUNTIME_UID).is_err(),
            "tampered uid {tampered} must mint no marker"
        );
    }
    // Sanity: the true UID still passes and the marker binds that UID.
    let good = PeerCredentials::new(RUNTIME_UID, RUNTIME_UID, 42);
    let marker = verify_peer_for_connection(good, RUNTIME_UID).unwrap();
    assert_eq!(marker.peer_uid(), RUNTIME_UID);
}

#[test]
fn markers_bind_uid_across_identities() {
    // Markers minted for different UIDs never compare equal, so an
    // unverified peer can never be mistaken for an attested one.
    let first = verify_peer_for_connection(
        PeerCredentials::new(RUNTIME_UID, RUNTIME_UID, 1),
        RUNTIME_UID,
    )
    .unwrap();
    let second = verify_peer_for_connection(
        PeerCredentials::new(FOREIGN_UID, FOREIGN_UID, 1),
        FOREIGN_UID,
    )
    .unwrap();
    assert_ne!(first, second);
    assert_eq!(first.peer_uid(), RUNTIME_UID);
    assert_eq!(second.peer_uid(), FOREIGN_UID);
    // Same UID verifies to the same marker.
    let repeat = verify_peer_for_connection(
        PeerCredentials::new(RUNTIME_UID, 9999, i32::MAX),
        RUNTIME_UID,
    )
    .unwrap();
    assert_eq!(first, repeat);
}

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
    assert!(!ServeContext::new(&server).local_attested);
    assert!(!ServeContext::with_granted(&server, ScopeSet::all()).local_attested);
    assert!(!ServeContext::with_granted_session(&server, ScopeSet::all(), "m").local_attested);
    // Attestation requires a verified marker — obtainable only through
    // verification — and flips the flag explicitly.
    let peer = verify_peer_for_connection(
        PeerCredentials::new(RUNTIME_UID, RUNTIME_UID, 1),
        RUNTIME_UID,
    )
    .unwrap();
    let mut ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    ctx.attest_local_peer(&peer);
    assert!(ctx.local_attested);
}

#[test]
fn frame_hash_denies_without_attestation() {
    let server = test_server();
    let dispatcher = Dispatcher::with_defaults();
    let params = r#"{"terminalId":"t:1","bearer":"ctx-0656-never-issued-bearer"}"#;
    let envelope = format!(
        "{{\"id\":1,\"method\":\"{METHOD_FRAME_HASH}\",\"version\":\"1.0\",\"params\":{params}}}"
    );
    // Missing attestation denies even with every scope granted: the
    // transport gate runs before scope/bearer checks.
    let ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    assert!(!ctx.local_attested);
    let outcome = handle_envelope(envelope.as_bytes(), &dispatcher, &ctx);
    assert!(outcome.was_error, "unattested frameHash must fail");
    let text = String::from_utf8(outcome.response).unwrap();
    assert!(
        text.contains("ScopeDenied") && text.contains("attested transport"),
        "unattested denial must name the transport gate, got: {text}"
    );
    // With attestation the transport gate passes and failure moves
    // downstream (forged bearer), proving the gates are ordered.
    let peer = verify_peer_for_connection(
        PeerCredentials::new(RUNTIME_UID, RUNTIME_UID, 1),
        RUNTIME_UID,
    )
    .unwrap();
    let mut attested_ctx = ServeContext::with_granted_session(&server, ScopeSet::all(), "m");
    attested_ctx.attest_local_peer(&peer);
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
    let marker = transport_attested_peer(&socket_path, euid).unwrap();
    assert_eq!(marker.peer_uid(), euid);
    // The attested marker equals the headless-verified marker for the same
    // UID: two verification paths, one identity.
    let headless = verify_peer_for_connection(PeerCredentials::new(euid, euid, 1), euid).unwrap();
    assert_eq!(headless, marker);
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}ok", std::process::id()));
}

#[cfg(unix)]
#[test]
fn transport_rejects_missing_endpoint() {
    // Missing attestation material fails closed with no marker: empty and
    // NUL paths are rejected as malformed, absent files as unavailable.
    let missing = temp_socket_path("no");
    assert!(transport_attested_peer(&missing, 1000).is_err());
    assert!(transport_attested_peer("", 1000).is_err());
    assert!(transport_attested_peer("bad\0path", 1000).is_err());
    let err = transport_attested_peer(&missing, 1000).unwrap_err();
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
    let err = transport_attested_peer(&link_path, euid).unwrap_err();
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
    let err = transport_attested_peer(&socket_path, euid).unwrap_err();
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
    let err = transport_attested_peer(&socket_path, euid).unwrap_err();
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
    let err = transport_attested_peer(&socket_path, foreign).unwrap_err();
    assert!(
        matches!(err, IpcError::Unauthenticated { .. }),
        "foreign runtime uid must fail closed, got {err:?}"
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(format!("/tmp/bt{}fu", std::process::id()));
}
