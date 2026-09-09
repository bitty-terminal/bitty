//! Windows ConPTY integration tests (CTX-0268 Tier-1 slice).
//!
//! Mirrors the Unix `spawn_smoke.rs` coverage with inbox Windows programs:
//! bare `cmd.exe` (resolved via the system directory, no PATH dependence)
//! is the interactive shell; `cmd /C ...` is the one-shot runner. Every test
//! calls `require_pty!()` first: it is the pty-gate lint marker and keeps
//! the `BITTY_TEST_FORCE_NO_PTY` simulation path exercisable.
//!
//! The whole file is `#![cfg(windows)]`: the spawned programs exist only
//! there. POSIX-spawning tests stay in `spawn_smoke.rs` (`#![cfg(unix)]`);
//! porting them to platform-neutral programs is deferred follow-up work.

#![cfg(windows)]

use std::io::Write;
use std::time::Duration;

use bitty_pty::PtyBuilder;
use bitty_pty::PtyError;
use bitty_test_support::require_pty;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);

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
fn cmd_exit_code_round_trips() {
    require_pty!();
    // One-shot success: `cmd /C exit 0` must reap cleanly.
    let mut pty = PtyBuilder::new("cmd.exe")
        .arg("/C")
        .arg("exit 0")
        .spawn()
        .expect("spawn cmd /C exit 0");
    let status = pty.wait().expect("reap cmd");
    assert!(status.is_success(), "exit 0 must succeed: {status:?}");
    assert_eq!(status.code(), 0);
    // ConPTY has no signals; the signal slot is always empty.
    assert_eq!(status.signal(), None);

    // One-shot failure: the raw exit code must survive ConPTY.
    let mut pty = PtyBuilder::new("cmd.exe")
        .arg("/C")
        .arg("exit 3")
        .spawn()
        .expect("spawn cmd /C exit 3");
    let status = pty.wait().expect("reap failing cmd");
    assert!(!status.is_success(), "exit 3 must fail: {status:?}");
    assert_eq!(status.code(), 3);
    assert_eq!(status.signal(), None);
}

#[test]
fn conpty_resize_round_trips_on_live_shell() {
    require_pty!();
    // Interactive `cmd.exe` blocks on stdin: resize while it is alive.
    let mut pty = PtyBuilder::new("cmd.exe")
        .size(80, 24)
        .spawn()
        .expect("spawn interactive cmd");

    assert_eq!(pty.size().expect("initial size"), (80, 24));
    pty.resize(120, 40).expect("resize up");
    assert_eq!(pty.size().expect("resized size"), (120, 40));
    pty.resize(10, 5).expect("resize down");
    assert_eq!(pty.size().expect("shrunk size"), (10, 5));

    // ConPTY exposes no terminal device path.
    assert_eq!(pty.tty_name(), None);
    assert!(pty.pid().is_some(), "child pid must be known");

    let status = pty.shutdown().expect("kill and reap interactive cmd");
    assert!(!status.is_success());
}

#[test]
fn kill_path_reports_unsuccessful_status() {
    require_pty!();
    let mut pty = PtyBuilder::new("cmd.exe").spawn().expect("spawn cmd");

    // Take the halves so Drop-time kill is exercised with resources live.
    let _writer = pty.take_writer().expect("writer half");
    let _reader = pty.take_reader().expect("reader half");

    pty.kill().expect("kill");
    let status = pty.wait().expect("wait after kill");
    assert!(!status.is_success(), "killed child cannot report success");
}

#[test]
fn child_environment_inherits_session_with_overrides() {
    require_pty!();
    // `cmd /C set` prints the whole child environment, then exits.
    let mut pty = PtyBuilder::new("cmd.exe")
        .arg("/C")
        .arg("set")
        .env("BITTY_PROBE", "1")
        .spawn()
        .expect("spawn cmd /C set");

    let reader = pty.take_reader().expect("reader half");
    let writer = pty.take_writer().expect("writer half");
    drop(writer);

    let status = pty.wait().expect("reap cmd /C set");
    assert!(status.is_success(), "cmd /C set must exit 0: {status:?}");

    let output = drain(&reader, std::time::Instant::now() + CMD_TIMEOUT);
    reader.join().expect("pump clean");

    // `set` separates with `\r\n`; byte-substring search is agnostic.
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("BITTY_PROBE=1"),
        "allowlisted entry missing from {text:?}"
    );
    assert!(
        text.contains("TERM=xterm-256color"),
        "default TERM missing from {text:?}"
    );
    assert!(
        text.contains("COLORTERM=truecolor"),
        "default COLORTERM missing from {text:?}"
    );
    assert!(
        text.contains("TERM_PROGRAM=bitty"),
        "default TERM_PROGRAM missing from {text:?}"
    );
}

#[test]
fn child_explicit_term_program_override_wins() {
    require_pty!();
    let mut pty = PtyBuilder::new("cmd.exe")
        .arg("/C")
        .arg("set")
        .env("TERM_PROGRAM", "custom-term")
        .spawn()
        .expect("spawn cmd /C set");

    let reader = pty.take_reader().expect("reader half");
    let writer = pty.take_writer().expect("writer half");
    drop(writer);

    let status = pty.wait().expect("reap cmd /C set");
    assert!(status.is_success());

    let output = drain(&reader, std::time::Instant::now() + CMD_TIMEOUT);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("TERM_PROGRAM=custom-term"),
        "explicit TERM_PROGRAM should win in {text:?}"
    );
}

#[test]
fn invalid_spawn_requests_are_rejected_without_spawning() {
    require_pty!();
    assert!(matches!(
        PtyBuilder::new("").spawn(),
        Err(PtyError::EmptyProgram)
    ));
    assert!(matches!(
        PtyBuilder::new("cmd.exe").size(0, 0).spawn(),
        Err(PtyError::InvalidSize { .. })
    ));
    assert!(matches!(
        PtyBuilder::new("nonexistent-bitty-pty-binary-xyz").spawn(),
        Err(PtyError::Upstream(_) | PtyError::Io(_))
    ));
}

#[test]
fn cmd_echo_flows_through_bounded_channel() {
    require_pty!();
    // Interactive echo dogfood: write a line, read it back through the
    // bounded channel. `cmd` echoes input plus the prompt; search for the
    // marker bytes rather than an exact line.
    let mut pty = PtyBuilder::new("cmd.exe").spawn().expect("spawn cmd");

    let mut writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    writer
        .write_all(b"echo hello-bitty-pty\r\n")
        .expect("write to pty");
    writer.flush().expect("flush");

    let deadline = std::time::Instant::now() + CMD_TIMEOUT;
    let mut echoed = Vec::new();
    while !contains(&echoed, b"hello-bitty-pty") {
        match reader.recv_timeout(CMD_TIMEOUT).expect("recv_timeout") {
            Some(chunk) => {
                assert!(chunk.len() <= bitty_pty::READ_CHUNK_SIZE);
                echoed.extend_from_slice(&chunk);
            }
            None => break,
        }
        assert!(std::time::Instant::now() < deadline, "echo timed out");
    }
    assert!(
        contains(&echoed, b"hello-bitty-pty"),
        "expected echo, got {:?}",
        String::from_utf8_lossy(&echoed)
    );

    let status = pty.shutdown().expect("kill and reap cmd");
    assert!(!status.is_success());
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
