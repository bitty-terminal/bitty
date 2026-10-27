//! bitty#2604 / CTX-1620: real Unix endpoint behavior.
//!
//! These tests create a throwaway runtime root per case, so they exercise the
//! real filesystem: mode and ownership attestation, the single-instance lock,
//! stale-socket reclamation, live-endpoint refusal, symlink and non-socket
//! refusal, the path-length bound, and owner-checked removal. Nothing here
//! accepts a connection in the daemon role: the listener is only probed.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use bitty_daemon::endpoint::{
    EndpointEnv, EndpointError, EndpointPaths, LOCK_FILE_MODE, MAX_SOCKET_PATH_BYTES,
    RUNTIME_DIR_MODE, RUNTIME_SUBDIR, SOCKET_MODE, TamperKind, open_endpoint,
};

/// Minimal temporary directory guard (no external dev-dependency).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bittyd-endpoint-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::DirBuilder::new()
            .mode(RUNTIME_DIR_MODE)
            .create(&path)
            .expect("create the 0700 test root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The derived runtime directory inside the root.
    fn runtime_dir(&self) -> PathBuf {
        self.path.join(RUNTIME_SUBDIR)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn env_for(root: &Path) -> EndpointEnv {
    EndpointEnv::new(Some(root.as_os_str().to_owned()), None)
}

fn env_for_override(socket: &Path) -> EndpointEnv {
    EndpointEnv::new(None, Some(socket.as_os_str().to_owned()))
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).expect("metadata").mode() & 0o777
}

fn uid_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).expect("metadata").uid()
}

fn inode_of(path: &Path) -> u64 {
    fs::symlink_metadata(path).expect("metadata").ino()
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .expect("metadata")
        .file_type()
        .is_socket()
}

fn create_runtime_dir(temp: &TempDir) {
    fs::DirBuilder::new()
        .mode(RUNTIME_DIR_MODE)
        .create(temp.runtime_dir())
        .expect("create the derived runtime directory");
}

#[test]
fn binds_the_endpoint_with_owner_private_modes() {
    let temp = TempDir::new("bind");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");

    assert_eq!(endpoint.runtime_dir(), temp.runtime_dir());
    assert_eq!(mode_of(endpoint.runtime_dir()), RUNTIME_DIR_MODE);
    assert_eq!(uid_of(endpoint.runtime_dir()), uid_of(temp.path()));
    assert!(is_socket(endpoint.socket_path()));
    assert_eq!(mode_of(endpoint.socket_path()), SOCKET_MODE);
    assert_eq!(uid_of(endpoint.socket_path()), uid_of(temp.path()));

    let paths = EndpointPaths::resolve(&env_for(temp.path())).expect("resolve");
    assert_eq!(mode_of(paths.lock()), LOCK_FILE_MODE);
    assert_eq!(uid_of(paths.lock()), uid_of(temp.path()));

    UnixStream::connect(endpoint.socket_path()).expect("the listener is bound");
    let socket = endpoint.socket_path().to_path_buf();
    endpoint.close().expect("clean shutdown removes the socket");
    assert!(!socket.exists(), "the socket is removed on clean shutdown");
}

#[test]
fn a_second_instance_is_refused_and_the_first_keeps_serving() {
    let temp = TempDir::new("second");
    let first = open_endpoint(&env_for(temp.path())).expect("first instance");
    let socket = first.socket_path().to_path_buf();

    let error = open_endpoint(&env_for(temp.path())).expect_err("second instance must refuse");
    assert!(
        matches!(error, EndpointError::AlreadyRunning { .. }),
        "{error}"
    );
    assert!(is_socket(&socket), "the live endpoint is untouched");
    UnixStream::connect(&socket).expect("the first instance still serves");

    first.close().expect("clean shutdown");
    let second = open_endpoint(&env_for(temp.path())).expect("the lock is released");
    second.close().expect("clean shutdown");
}

#[test]
fn a_live_endpoint_is_never_unlinked() {
    let temp = TempDir::new("live");
    create_runtime_dir(&temp);
    let socket = temp.runtime_dir().join("bittyd.sock");
    let live = UnixListener::bind(&socket).expect("bind a live endpoint");
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).expect("socket mode");
    let inode = inode_of(&socket);

    let error = open_endpoint(&env_for(temp.path())).expect_err("a live endpoint must refuse");
    assert!(
        matches!(error, EndpointError::AlreadyRunning { .. }),
        "{error}"
    );
    assert_eq!(inode_of(&socket), inode, "the live socket is not replaced");
    UnixStream::connect(&socket).expect("the live endpoint still accepts");

    drop(live);
    let endpoint =
        open_endpoint(&env_for(temp.path())).expect("the failed start released the lock");
    endpoint.close().expect("clean shutdown");
}

#[test]
fn a_stale_socket_is_reclaimed() {
    let temp = TempDir::new("stale");
    create_runtime_dir(&temp);
    let socket = temp.runtime_dir().join("bittyd.sock");
    let stale = UnixListener::bind(&socket).expect("bind the socket to go stale");
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).expect("socket mode");
    let stale_inode = inode_of(&socket);
    drop(stale);
    assert!(
        UnixStream::connect(&socket).is_err(),
        "the stale endpoint refuses connections"
    );

    let endpoint = open_endpoint(&env_for(temp.path())).expect("a stale socket is reclaimed");
    assert_ne!(inode_of(endpoint.socket_path()), stale_inode);
    UnixStream::connect(endpoint.socket_path()).expect("the reclaimed endpoint accepts");
    endpoint.close().expect("clean shutdown");
}

#[test]
fn a_stale_socket_with_a_drifted_mode_is_still_reclaimed() {
    let temp = TempDir::new("stale-mode");
    create_runtime_dir(&temp);
    let socket = temp.runtime_dir().join("bittyd.sock");
    let stale = UnixListener::bind(&socket).expect("bind the socket to go stale");
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o755)).expect("drift the mode");
    drop(stale);

    // A dead socket owned by this uid is reclaimable: the mode check belongs
    // to the bound endpoint, and a crash between bind and chmod must not brick
    // the next start. A *live* endpoint is never unlinked. The inode is not
    // asserted to change: a freshly created socket can reuse the inode of the
    // object removed moments earlier (observed on CI), so the mode and a real
    // connect are the load-bearing evidence.
    let endpoint = open_endpoint(&env_for(temp.path())).expect("a dead socket is reclaimed");
    assert_eq!(mode_of(endpoint.socket_path()), SOCKET_MODE);
    std::os::unix::net::UnixStream::connect(endpoint.socket_path())
        .expect("the reclaimed endpoint accepts connections");
    let path = endpoint.socket_path().to_path_buf();
    endpoint.close().expect("clean shutdown");
    assert!(!path.exists(), "clean shutdown removes the socket");
}

#[test]
fn a_symlinked_endpoint_is_refused_and_the_target_survives() {
    let temp = TempDir::new("symlink");
    create_runtime_dir(&temp);
    let target = temp.path().join("target");
    fs::write(&target, b"keep").expect("write the symlink target");
    let socket = temp.runtime_dir().join("bittyd.sock");
    std::os::unix::fs::symlink(&target, &socket).expect("create the symlink");

    let error = open_endpoint(&env_for(temp.path())).expect_err("a symlink must refuse");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Symlink,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        fs::symlink_metadata(&socket)
            .expect("the symlink is untouched")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).expect("target intact"), b"keep");
}

#[test]
fn a_regular_file_at_the_endpoint_is_refused_and_preserved() {
    let temp = TempDir::new("regular");
    create_runtime_dir(&temp);
    let socket = temp.runtime_dir().join("bittyd.sock");
    fs::write(&socket, b"keep").expect("write the file");

    let error = open_endpoint(&env_for(temp.path())).expect_err("a file must refuse");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::NotASocket,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(fs::read(&socket).expect("file intact"), b"keep");
}

#[test]
fn a_missing_runtime_root_is_a_typed_no_endpoint() {
    let temp = TempDir::new("missing-root");
    let missing = temp.path().join("does-not-exist");
    let error = open_endpoint(&EndpointEnv::new(
        Some(missing.as_os_str().to_owned()),
        None,
    ))
    .expect_err("an unusable runtime root is a typed no-endpoint");
    assert!(matches!(error, EndpointError::NoEndpoint { .. }), "{error}");
    assert!(!missing.join(RUNTIME_SUBDIR).exists());
}

#[test]
fn an_over_long_endpoint_is_refused_before_creating_anything() {
    let temp = TempDir::new("long");
    let name = "s".repeat(MAX_SOCKET_PATH_BYTES);
    let socket = temp.path().join(name);
    let error = open_endpoint(&env_for_override(&socket)).expect_err("over-long path refuses");
    match error {
        EndpointError::PathTooLong { bytes, max, .. } => {
            assert!(bytes > max);
            assert_eq!(max, MAX_SOCKET_PATH_BYTES);
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(
        fs::read_dir(temp.path()).expect("read root").count(),
        0,
        "no directory or socket is created for a refused path"
    );
}

#[test]
fn an_override_endpoint_is_bound_inside_the_override_parent() {
    let temp = TempDir::new("override");
    let socket = temp.path().join("custom.sock");
    let endpoint =
        open_endpoint(&env_for_override(&socket)).expect("an override socket is supported");
    assert_eq!(endpoint.socket_path(), socket);
    assert_eq!(mode_of(endpoint.socket_path()), SOCKET_MODE);
    assert_eq!(mode_of(endpoint.runtime_dir()), RUNTIME_DIR_MODE);
    endpoint.close().expect("clean shutdown");
}

#[test]
fn closing_does_not_unlink_a_replacement() {
    let temp = TempDir::new("replacement");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let socket = endpoint.socket_path().to_path_buf();
    fs::remove_file(&socket).expect("remove the bound socket");
    let replacement = UnixListener::bind(&socket).expect("bind a replacement");
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).expect("socket mode");

    let error = endpoint
        .close()
        .expect_err("a replacement is not ours to remove");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Replaced,
                ..
            }
        ),
        "{error}"
    );
    assert!(is_socket(&socket), "the replacement is preserved");
    UnixStream::connect(&socket).expect("the replacement still accepts");
    drop(replacement);
}

#[test]
fn reattest_reports_directory_and_socket_drift() {
    let temp = TempDir::new("reattest");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let runtime_dir = endpoint.runtime_dir().to_path_buf();
    let socket = endpoint.socket_path().to_path_buf();

    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o755)).expect("drift the dir");
    let error = endpoint
        .reattest()
        .expect_err("a 0755 runtime dir is tampered");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::WrongMode { .. },
                ..
            }
        ),
        "{error}"
    );
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(RUNTIME_DIR_MODE))
        .expect("restore the dir");

    fs::set_permissions(&socket, fs::Permissions::from_mode(0o644)).expect("drift the socket");
    let error = endpoint.reattest().expect_err("a 0644 socket is tampered");
    assert!(error.is_tamper());
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE))
        .expect("restore the socket");

    endpoint.reattest().expect("the restored endpoint attests");
    endpoint.close().expect("clean shutdown");
}

#[test]
fn reattest_reports_a_missing_socket_and_close_is_ok() {
    let temp = TempDir::new("missing-socket");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let socket = endpoint.socket_path().to_path_buf();
    fs::remove_file(&socket).expect("delete the socket");

    let error = endpoint.reattest().expect_err("a deleted socket is tamper");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Missing,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.is_tamper());
    assert_eq!(error.result_code(), Some("Unauthenticated"));

    // A clean shutdown after the object is already gone is not an error.
    endpoint.close().expect("close after deletion is Ok");
}

#[test]
fn reattest_reports_a_missing_runtime_directory() {
    let temp = TempDir::new("missing-dir");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let runtime_dir = endpoint.runtime_dir().to_path_buf();
    fs::remove_dir_all(&runtime_dir).expect("delete the runtime directory");

    let error = endpoint
        .reattest()
        .expect_err("a deleted runtime directory is tamper");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Missing,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.is_tamper());
    assert!(!runtime_dir.exists());
}

#[test]
fn reattest_reports_a_replaced_socket() {
    let temp = TempDir::new("replaced-socket");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let socket = endpoint.socket_path().to_path_buf();
    fs::remove_file(&socket).expect("remove the bound socket");
    let replacement = UnixListener::bind(&socket).expect("bind a replacement");
    fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).expect("socket mode");

    let error = endpoint.reattest().expect_err("a replacement is tamper");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Replaced,
                ..
            }
        ),
        "{error}"
    );
    assert!(is_socket(&socket));
    drop(replacement);
    drop(endpoint);
}

#[test]
fn close_leaves_the_lock_file_in_place() {
    let temp = TempDir::new("lock-kept");
    let endpoint = open_endpoint(&env_for(temp.path())).expect("bind the endpoint");
    let paths = EndpointPaths::resolve(&env_for(temp.path())).expect("resolve");
    let inode = inode_of(paths.lock());
    endpoint.close().expect("clean shutdown");
    assert!(
        fs::symlink_metadata(paths.lock())
            .expect("the lock file survives a clean shutdown")
            .file_type()
            .is_file()
    );
    assert_eq!(inode_of(paths.lock()), inode);
    assert_eq!(mode_of(paths.lock()), LOCK_FILE_MODE);
}

#[test]
fn a_symlinked_lock_path_is_refused_at_startup() {
    let temp = TempDir::new("lock-symlink");
    create_runtime_dir(&temp);
    let paths = EndpointPaths::resolve(&env_for(temp.path())).expect("resolve");
    let target = temp.path().join("lock-target");
    fs::write(&target, b"keep").expect("write the target");
    std::os::unix::fs::symlink(&target, paths.lock()).expect("create the symlink");

    let error = open_endpoint(&env_for(temp.path())).expect_err("a symlink must refuse");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::Symlink,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        fs::symlink_metadata(paths.lock())
            .expect("the symlink is untouched")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target).expect("target intact"), b"keep");
}

#[test]
fn a_directory_at_the_lock_path_is_refused_at_startup() {
    let temp = TempDir::new("lock-dir");
    create_runtime_dir(&temp);
    let paths = EndpointPaths::resolve(&env_for(temp.path())).expect("resolve");
    fs::create_dir(paths.lock()).expect("create the directory");

    let error = open_endpoint(&env_for(temp.path())).expect_err("a directory must refuse");
    assert!(
        matches!(
            error,
            EndpointError::Tampered {
                kind: TamperKind::NotARegularFile,
                ..
            }
        ),
        "{error}"
    );
    assert!(paths.lock().is_dir());
}

#[test]
fn a_shared_runtime_root_is_refused() {
    for mode in [0o770, 0o707, 0o777] {
        let temp = TempDir::new("shared-root");
        let root = temp.path().to_path_buf();
        fs::set_permissions(&root, fs::Permissions::from_mode(mode)).expect("share the root");
        let error = open_endpoint(&env_for(&root)).expect_err("a shared root must refuse");
        match error {
            EndpointError::Tampered {
                kind: TamperKind::SharedRuntimeRoot { mode: observed },
                ..
            } => assert_eq!(observed, mode),
            other => panic!("unexpected error for mode {mode:o}: {other}"),
        }
        assert!(
            !root.join(RUNTIME_SUBDIR).exists(),
            "nothing is created under a shared runtime root"
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(RUNTIME_DIR_MODE))
            .expect("restore the root for cleanup");
    }
}
