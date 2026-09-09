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
//!
//! # ConPTY timing rules (win469c lessons)
//!
//! - **Never block unbounded.** `Pty::wait`, `PtyReader::recv`, and
//!   `PtyReader::join` (while the pump thread is inside a kernel read) can
//!   all block forever when a child never signals exit, so every test below
//!   uses [`Pty::wait_timeout`] plus `recv_timeout` ticks and fails loudly
//!   with partial output instead of hanging the test binary.
//! - **Drain before reap, never wait-then-drain for one-shot children.**
//!   Process exit does not mean output delivery: conhost renders the child's
//!   console writes into the output pipe asynchronously, so a `cmd /C ...`
//!   child can exit (and its session tear down) before the last bytes reach
//!   our reader. Environment assertions therefore run against an interactive
//!   `cmd.exe` that stays alive until the markers are observed; only the
//!   exit-code test (which needs no output) uses one-shot `cmd /C exit N`.
//! - **Drop the `Pty` (ClosePseudoConsole) before `join`.** The pump thread
//!   sits in a kernel read on a pipe conhost owns; tearing the console down
//!   first guarantees the read terminates and `join` returns promptly. After
//!   a kill-driven shutdown the pump outcome (clean EOF vs broken pipe) is
//!   conhost-racy, so it is intentionally not asserted here: all product
//!   assertions happen on bytes already collected.

#![cfg(windows)]

use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use bitty_pty::PtyBuilder;
use bitty_pty::PtyError;
use bitty_test_support::require_pty;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for a child that must exit on its own (one-shot `cmd /C ...`).
/// Generous on purpose: a healthy child exits in milliseconds; the bound
/// only fires when something is genuinely stuck, and then it must fail
/// loudly instead of holding the CI job until the 30-minute ceiling.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for reaping after `kill`: termination is near-instant, so a short
/// bound that still tolerates a loaded runner.
const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(15);
/// Single `recv_timeout` tick inside the marker loops below. Short ticks let
/// the loop re-check the overall deadline instead of sleeping past it.
const RECV_TICK: Duration = Duration::from_millis(500);

/// Reads until `needle` is observed, EOF, or `deadline` — whichever comes
/// first — and returns whatever arrived. Never blocks past the deadline.
/// Callers assert on the returned bytes (with byte count and lossy text in
/// the message) so a missing marker fails with evidence, not a hang.
fn read_until(reader: &bitty_pty::PtyReader, needle: &[u8], deadline: Instant) -> Vec<u8> {
    debug_assert!(!needle.is_empty(), "read_until needs a non-empty marker");
    let mut out = Vec::new();
    while !contains(&out, needle) {
        if Instant::now() >= deadline {
            break;
        }
        match reader.recv_timeout(RECV_TICK) {
            Ok(Some(chunk)) => {
                assert!(
                    chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
                    "chunk {} exceeded READ_CHUNK_SIZE {}",
                    chunk.len(),
                    bitty_pty::READ_CHUNK_SIZE
                );
                out.extend_from_slice(&chunk);
            }
            // EOF (child gone and pump drained) or pump ended: whatever we
            // have is all there is; the caller asserts on it.
            Ok(None) => break,
            // Tick elapsed with no data: loop around and re-check deadline.
            Err(_) => continue,
        }
    }
    out
}

/// Bounded reap for children that must exit on their own.
///
/// Returns the status on success. On timeout kills the child, reaps it, and
/// panics with the final status: a stuck child must fail loudly (win469c
/// held the whole test binary ~23 min in an unbounded `wait()`).
fn reap_bounded(pty: &mut bitty_pty::Pty, what: &str) -> bitty_pty::ExitStatus {
    match pty.wait_timeout(WAIT_TIMEOUT).expect("wait_timeout") {
        Some(status) => status,
        None => {
            let _ = pty.kill();
            let after = pty
                .wait_timeout(KILL_REAP_TIMEOUT)
                .expect("reap after kill");
            panic!("{what} did not exit within {WAIT_TIMEOUT:?}; killed, reap: {after:?}");
        }
    }
}

/// Kill, bounded reap, console teardown, then pump join.
///
/// Takes ownership so the `Pty` (and its ConPTY handles) can be dropped
/// before joining: dropping closes the pseudo-console, which unblocks the
/// pump's kernel read and makes `join` prompt. The join outcome itself is
/// intentionally unchecked — after a kill-driven teardown conhost may
/// deliver clean EOF or a broken pipe, and that race carries no product
/// signal (all assertions below run on bytes collected beforehand).
fn shutdown_and_join(mut pty: bitty_pty::Pty, reader: bitty_pty::PtyReader, what: &str) {
    let _ = pty.kill();
    match pty.wait_timeout(WAIT_TIMEOUT).expect("reap after kill") {
        Some(status) => assert!(
            !status.is_success(),
            "{what}: killed child must not report success: {status:?}"
        ),
        None => panic!("{what}: child survived kill past {WAIT_TIMEOUT:?}"),
    }
    drop(pty);
    let _ = reader.join();
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
    let status = reap_bounded(&mut pty, "cmd /C exit 0");
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
    let status = reap_bounded(&mut pty, "cmd /C exit 3");
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

    let reader = pty.take_reader().expect("reader half");
    shutdown_and_join(pty, reader, "interactive cmd after resize");
}

#[test]
fn kill_path_reports_unsuccessful_status() {
    require_pty!();
    let mut pty = PtyBuilder::new("cmd.exe").spawn().expect("spawn cmd");

    // Take the halves so Drop-time kill is exercised with resources live.
    let _writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    pty.kill().expect("kill");
    let status = reap_bounded(&mut pty, "cmd after kill");
    assert!(!status.is_success(), "killed child cannot report success");
    drop(pty);
    let _ = reader.join();
}

#[test]
fn child_environment_inherits_session_with_overrides() {
    require_pty!();
    // Interactive `cmd.exe` stays alive until the markers are observed, so
    // no exit/teardown race can truncate the output (see module docs): ask
    // the live shell to print its environment with `set`.
    let mut pty = PtyBuilder::new("cmd.exe")
        .env("BITTY_PROBE", "1")
        .spawn()
        .expect("spawn interactive cmd");

    let mut writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    writer.write_all(b"set\r\n").expect("write set to pty");
    writer.flush().expect("flush");

    // `set` separates with `\r\n`; byte-substring search is agnostic.
    // Bounded: on timeout the partial bytes below show what arrived.
    let deadline = Instant::now() + CMD_TIMEOUT;
    let output = read_until(&reader, b"BITTY_PROBE=1", deadline);
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("BITTY_PROBE=1"),
        "allowlisted entry missing after {CMD_TIMEOUT:?} ({} bytes): {text:?}",
        output.len()
    );
    assert!(
        text.contains("TERM=xterm-256color"),
        "default TERM missing ({} bytes): {text:?}",
        output.len()
    );
    assert!(
        text.contains("COLORTERM=truecolor"),
        "default COLORTERM missing ({} bytes): {text:?}",
        output.len()
    );
    assert!(
        text.contains("TERM_PROGRAM=bitty"),
        "default TERM_PROGRAM missing ({} bytes): {text:?}",
        output.len()
    );

    drop(writer);
    shutdown_and_join(pty, reader, "interactive cmd after set");
}

#[test]
fn child_explicit_term_program_override_wins() {
    require_pty!();
    let mut pty = PtyBuilder::new("cmd.exe")
        .env("TERM_PROGRAM", "custom-term")
        .spawn()
        .expect("spawn interactive cmd");

    let mut writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    writer.write_all(b"set\r\n").expect("write set to pty");
    writer.flush().expect("flush");

    let deadline = Instant::now() + CMD_TIMEOUT;
    let output = read_until(&reader, b"TERM_PROGRAM=custom-term", deadline);
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("TERM_PROGRAM=custom-term"),
        "explicit TERM_PROGRAM should win after {CMD_TIMEOUT:?} ({} bytes): {text:?}",
        output.len()
    );
    assert!(
        !text.contains("TERM_PROGRAM=bitty"),
        "default TERM_PROGRAM must be overridden ({} bytes): {text:?}",
        output.len()
    );

    drop(writer);
    shutdown_and_join(pty, reader, "interactive cmd after TERM_PROGRAM set");
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

    let deadline = Instant::now() + CMD_TIMEOUT;
    let echoed = read_until(&reader, b"hello-bitty-pty", deadline);
    assert!(
        contains(&echoed, b"hello-bitty-pty"),
        "expected echo after {CMD_TIMEOUT:?} ({} bytes): {:?}",
        echoed.len(),
        String::from_utf8_lossy(&echoed)
    );

    drop(writer);
    shutdown_and_join(pty, reader, "interactive cmd after echo");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
