//! Integration smoke tests driving real child processes through the owned
//! PTY API.
//!
//! Entirely gated to `cfg(unix)`: Windows runners compile the crate but skip
//! these tests until the ConPTY backend slice lands.

#![cfg(unix)]

use std::io::Write;
use std::time::Duration;

use bitty_pty::PtyBuilder;
use bitty_pty::PtyError;

const ECHO_TIMEOUT: Duration = Duration::from_secs(10);

/// Collects output until EOF, asserting every chunk arrives through the
/// bounded channel. Returns the concatenation.
fn drain(reader: &bitty_pty::PtyReader, deadline: std::time::Instant) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = reader.recv() {
        assert!(
            chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
            "chunk exceeded declared read size"
        );
        out.extend_from_slice(&chunk);
        assert!(std::time::Instant::now() < deadline, "test timed out");
    }
    out
}

#[test]
fn cat_echo_resize_and_graceful_shutdown() {
    let mut pty = PtyBuilder::new("/bin/cat")
        .size(80, 24)
        .spawn()
        .expect("spawn /bin/cat");

    assert_eq!(pty.size().expect("initial size"), (80, 24));
    pty.resize(120, 40).expect("resize up");
    assert_eq!(pty.size().expect("resized size"), (120, 40));
    pty.resize(10, 5).expect("resize down");
    assert_eq!(pty.size().expect("shrunk size"), (10, 5));

    let mut writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    writer
        .write_all(b"bitty-pty-smoke\n")
        .expect("write to pty");
    writer.flush().expect("flush");

    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut echoed = Vec::new();
    while !contains(&echoed, b"bitty-pty-smoke") {
        match reader.recv_timeout(ECHO_TIMEOUT).expect("recv_timeout") {
            Some(chunk) => echoed.extend_from_slice(&chunk),
            None => break,
        }
        assert!(std::time::Instant::now() < deadline, "echo timed out");
    }
    assert!(
        contains(&echoed, b"bitty-pty-smoke"),
        "expected echo, got {echoed:?}"
    );

    // Graceful shutdown: dropping the writer sends EOF; `cat` exits 0.
    drop(writer);
    let status = pty.wait().expect("reap after EOF");
    assert!(status.is_success(), "cat should exit cleanly: {status:?}");
    assert_eq!(status.code(), 0);

    // Reader must reach EOF now that the child is gone.
    let _rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    reader.join().expect("pump ended cleanly at EOF");
}

#[test]
fn kill_path_reports_unsuccessful_status() {
    let mut pty = PtyBuilder::new("/bin/cat").spawn().expect("spawn cat");

    // Take the halves so Drop-time kill is exercised with resources live.
    let _writer = pty.take_writer().expect("writer half");
    let _reader = pty.take_reader().expect("reader half");

    pty.kill().expect("kill");
    let status = pty.wait().expect("wait after kill");
    assert!(!status.is_success(), "killed child cannot report success");
}

#[test]
fn shutdown_kills_and_reaps_in_one_step() {
    let mut pty = PtyBuilder::new("/bin/cat").spawn().expect("spawn cat");
    let status = pty.shutdown().expect("shutdown");
    assert!(!status.is_success());
    // Double reap is refused rather than misreported.
    assert!(matches!(pty.try_wait(), Err(PtyError::ChildAlreadyReaped)));
    assert!(matches!(pty.wait(), Err(PtyError::ChildAlreadyReaped)));
}

#[test]
fn child_environment_is_minimal_allowlist_only() {
    let mut pty = PtyBuilder::new("/usr/bin/env")
        .env("BITTY_PROBE", "1")
        .spawn()
        .expect("spawn env");

    let reader = pty.take_reader().expect("reader half");
    let writer = pty.take_writer().expect("writer half");
    drop(writer); // env prints its environment then exits on stdin EOF

    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let output = drain(&reader, deadline);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&output);
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.contains(&"TERM=xterm-256color"),
        "default TERM missing from {text:?}"
    );
    assert!(
        lines.contains(&"BITTY_PROBE=1"),
        "allowlisted entry missing from {text:?}"
    );
    for line in &lines {
        let key = line.split('=').next().unwrap_or("");
        assert!(
            matches!(key, "TERM" | "BITTY_PROBE" | "SHELL"),
            "child leaked environment entry {line:?}; allowlist violated"
        );
    }
}

#[test]
fn cwd_is_applied_to_child() {
    let mut pty = PtyBuilder::new("/bin/pwd")
        .cwd("/tmp")
        .spawn()
        .expect("spawn pwd in /tmp");

    let reader = pty.take_reader().expect("reader half");
    let status = pty.wait().expect("pwd exits immediately");
    assert!(status.is_success());

    let output = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    reader.join().expect("pump clean");
    let text = String::from_utf8_lossy(&output);
    assert_eq!(text.trim_end(), "/tmp", "pwd reported {text:?}");
}

#[test]
fn halves_can_only_be_taken_once() {
    let mut pty = PtyBuilder::new("/bin/cat").spawn().expect("spawn cat");
    let _ = pty.take_writer().expect("first writer");
    let _ = pty.take_reader().expect("first reader");
    assert!(matches!(
        pty.take_writer(),
        Err(PtyError::HalfAlreadyTaken("writer"))
    ));
    assert!(matches!(
        pty.take_reader(),
        Err(PtyError::HalfAlreadyTaken("reader"))
    ));
}

#[test]
fn invalid_spawn_requests_are_rejected_without_spawning() {
    assert!(matches!(
        PtyBuilder::new("").spawn(),
        Err(PtyError::EmptyProgram)
    ));
    assert!(matches!(
        PtyBuilder::new("/bin/cat").size(0, 0).spawn(),
        Err(PtyError::InvalidSize { .. })
    ));
    assert!(matches!(
        PtyBuilder::new("/nonexistent-bitty-pty-binary-xyz").spawn(),
        Err(PtyError::Upstream(_) | PtyError::Io(_))
    ));
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
