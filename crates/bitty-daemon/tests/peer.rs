//! bitty#2604 / CTX-1620: peer capture and re-check across a real process
//! boundary.
//!
//! The child is this same test binary re-executed with `--exact
//! peer_helper_child` and a socket path in `BITTYD_TEST_PEER_SOCKET`; that
//! keeps the credential path a real cross-process `SO_PEERCRED` /
//! `LOCAL_PEERPID` read without a `fork` or any `unsafe` code. The helper
//! connects, signals readiness, and waits for the parent to release it.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bitty_daemon::peer::{DispatchBoundary, PeerCause, PeerError, PeerGuard};

/// The child-side socket path handoff.
const HELPER_ENV: &str = "BITTYD_TEST_PEER_SOCKET";

/// Child-side half of the test. Runs as a no-op in the parent test process.
#[test]
fn peer_helper_child() {
    let Ok(path) = std::env::var(HELPER_ENV) else {
        return;
    };
    let mut stream = UnixStream::connect(path).expect("the helper connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("the helper sets a read timeout");
    stream
        .write_all(b"r")
        .expect("the helper reports readiness");
    let mut byte = [0u8; 1];
    let _ = stream.read(&mut byte);
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bittyd-peer-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("create the 0700 test root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn accept_with_timeout(listener: &UnixListener, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    listener
        .set_nonblocking(true)
        .expect("set the listener non-blocking");
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("the accepted stream is blocking");
                return Some(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn accept_with_deadline(listener: &UnixListener) -> UnixStream {
    accept_with_timeout(listener, Duration::from_secs(15))
        .expect("the peer helper did not connect in time")
}

#[test]
fn captures_and_rechecks_a_child_process_peer() {
    let temp = TempDir::new("child");
    let socket = temp.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind the peer socket");
    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .arg("--exact")
        .arg("peer_helper_child")
        .arg("--nocapture")
        .env(HELPER_ENV, &socket)
        .spawn()
        .expect("spawn the peer helper");

    let mut stream = accept_with_deadline(&listener);
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("set the read timeout");
    let mut ready = [0u8; 1];
    stream
        .read_exact(&mut ready)
        .expect("the helper reports readiness");

    let uid = fs::metadata(temp.path()).expect("temp metadata").uid();
    let guard = PeerGuard::capture(&stream).expect("capture the child peer");
    assert_eq!(guard.identity().pid(), child.id());
    assert_eq!(guard.identity().uid(), uid);
    #[cfg(target_os = "linux")]
    assert!(
        guard.identity().start_time().is_some(),
        "Linux exposes the peer start time"
    );
    for boundary in DispatchBoundary::ALL {
        guard
            .recheck(boundary)
            .expect("the helper is still the captured process");
    }

    stream.write_all(b"x").expect("release the helper");
    let status = child.wait().expect("wait for the helper");
    assert!(status.success(), "the helper exits cleanly");

    let error = guard
        .recheck(DispatchBoundary::Abort)
        .expect_err("a dead peer must be refused");
    assert!(
        matches!(
            error,
            PeerError::RecheckFailed {
                boundary: DispatchBoundary::Abort,
                cause: PeerCause::Exited | PeerCause::Changed { .. },
            }
        ),
        "unexpected re-check failure: {error}"
    );
}

/// The uid is mapped to the same value through an unprivileged user
/// namespace, so `SO_PEERCRED` alone cannot distinguish the child. Only the
/// user-namespace comparison refuses it (`D-15`).
#[test]
fn refuses_a_peer_from_a_different_user_namespace() {
    let temp = TempDir::new("userns");
    let socket = temp.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind the peer socket");
    let mut child = match Command::new("unshare")
        .arg("-Ur")
        .arg(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "peer_helper_child", "--nocapture"])
        .env(HELPER_ENV, &socket)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "skip: cannot spawn `unshare` for the user-namespace test: {error};                  the check stays unpinned on this host"
            );
            return;
        }
    };

    let Some(mut stream) = accept_with_timeout(&listener, Duration::from_secs(15)) else {
        match child.try_wait() {
            Ok(Some(status)) => eprintln!(
                "skip: `unshare -Ur` did not start the helper ({status}); the kernel or a                  security policy denied an unprivileged user namespace"
            ),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the user-namespace helper never connected");
            }
            Err(error) => panic!("waiting for the unshare helper failed: {error}"),
        }
        return;
    };

    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("set the read timeout");
    let mut ready = [0u8; 1];
    stream
        .read_exact(&mut ready)
        .expect("the helper reports readiness");

    let error = PeerGuard::capture(&stream)
        .expect_err("a uid mapped through a user namespace must not be same-user success");
    assert!(
        matches!(
            error,
            PeerError::CaptureFailed {
                cause: PeerCause::Unauthenticated { .. }
            }
        ),
        "unexpected capture result: {error}"
    );

    stream.write_all(b"x").expect("release the helper");
    let _ = child.wait();
}
