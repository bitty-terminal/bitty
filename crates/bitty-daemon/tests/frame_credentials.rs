//! bitty#2607 / CTX-1531: per-frame `SCM_CREDENTIALS` across real processes.
//!
//! The positive case is the same test binary re-executed with
//! `--exact frame_helper_single`: it connects, writes one frame, and waits. The
//! daemon-side reader must accept it with the captured peer's credentials.
//!
//! The negative case re-executes `frame_helper_fd_pass`, which connects and
//! hands its connected socket to a **grandchild** as stderr; the grandchild
//! writes the frame. The daemon captured the child's pid, so the frame's
//! `SCM_CREDENTIALS` pid is the grandchild's and the reader must refuse it with
//! `Unauthenticated`. That is the live-descriptor-handoff residual from the
//! security review, tested rather than qualified away.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bitty_daemon::peer::PeerGuard;
use bitty_daemon::service::frame::{FrameError, FramePeer, PeerFrameReader, enable_credentials};

const CHILD_SOCKET_ENV: &str = "BITTYD_TEST_FRAME_SOCKET";
const GRANDCHILD_PAYLOAD_ENV: &str = "BITTYD_TEST_FRAME_PAYLOAD";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bittyd-frame-{label}-{}-{}",
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

fn payload() -> Vec<u8> {
    br#"{"v":1,"id":"1","method":"daemon.negotiate","params":{"client_version":{"major":1,"minor":0}}}"#
        .to_vec()
}

fn frame_of(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn from_hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let high = (chunk[0] as char).to_digit(16).expect("hex") as u8;
            let low = (chunk[1] as char).to_digit(16).expect("hex") as u8;
            (high << 4) | low
        })
        .collect()
}

/// Grandchild half: write one frame to stderr, which the child mapped to the
/// inherited connection.
#[test]
fn frame_helper_grandchild() {
    let Ok(payload_hex) = std::env::var(GRANDCHILD_PAYLOAD_ENV) else {
        return;
    };
    let frame = frame_of(&from_hex(&payload_hex));
    enable_credentials(&std::io::stderr()).expect("the grandchild opts into credentials");
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(&frame).expect("write the frame");
    stderr.flush().expect("flush the frame");
}

/// Child half of the fd-passing case: connect, then hand the descriptor to a
/// grandchild and stay alive so the captured pid remains resolvable.
#[test]
fn frame_helper_fd_pass() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let stream = UnixStream::connect(socket).expect("child connects");
    let owned: OwnedFd = stream.into();
    let mut grandchild = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("frame_helper_grandchild")
        .arg("--nocapture")
        .env(GRANDCHILD_PAYLOAD_ENV, to_hex(&payload()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(owned))
        .spawn()
        .expect("spawn the grandchild");
    grandchild.wait().expect("wait for the grandchild");
    std::thread::sleep(Duration::from_millis(500));
}

/// Positive-control child: send the frame itself.
#[test]
fn frame_helper_single() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let mut stream = UnixStream::connect(socket).expect("child connects");
    enable_credentials(&stream).expect("sender credentials");
    stream
        .write_all(&frame_of(&payload()))
        .expect("write the frame");
    stream.flush().expect("flush");
    std::thread::sleep(Duration::from_millis(500));
}

fn accept_with_deadline(listener: &UnixListener) -> UnixStream {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "helper did not connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn read_until_error_or_frame(
    reader: &mut PeerFrameReader,
    stream: &UnixStream,
    expected: FramePeer,
) -> Result<Option<Vec<u8>>, FrameError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match reader.read(stream, expected) {
            Ok(None) => {
                assert!(Instant::now() < deadline, "no frame arrived");
                std::thread::sleep(Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

fn capture(stream: &UnixStream) -> FramePeer {
    let guard = PeerGuard::capture(stream).expect("capture the child");
    FramePeer {
        uid: guard.identity().uid(),
        pid: guard.identity().pid(),
    }
}

#[test]
fn a_frame_from_the_captured_process_is_accepted() {
    let temp = TempDir::new("single");
    let socket = temp.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("frame_helper_single")
        .arg("--nocapture")
        .env(CHILD_SOCKET_ENV, &socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the helper");
    let stream = accept_with_deadline(&listener);
    let expected = capture(&stream);
    stream.set_nonblocking(true).expect("nonblocking");
    let mut reader = PeerFrameReader::new();
    let received = read_until_error_or_frame(&mut reader, &stream, expected)
        .expect("the captured peer's frame is accepted")
        .expect("a complete frame");
    assert_eq!(received, payload());
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_frame_from_a_different_process_is_unauthenticated() {
    let temp = TempDir::new("fdpass");
    let socket = temp.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("frame_helper_fd_pass")
        .arg("--nocapture")
        .env(CHILD_SOCKET_ENV, &socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the child");
    let stream = accept_with_deadline(&listener);
    let expected = capture(&stream);
    stream.set_nonblocking(true).expect("nonblocking");
    let mut reader = PeerFrameReader::new();
    let error = read_until_error_or_frame(&mut reader, &stream, expected)
        .expect_err("a frame from the grandchild must be refused");
    assert_eq!(error, FrameError::Unauthenticated);
    let _ = child.kill();
    let _ = child.wait();
}
