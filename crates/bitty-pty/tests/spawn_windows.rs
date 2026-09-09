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
//! - **Answer conhost's DSR or the child freezes (win469d lesson).**
//!   ConPTY owns no cursor grid: when the console client queries cursor
//!   state (which `cmd.exe` does during startup), conhost emits `ESC[6n`
//!   (DSR, device-status-report request) into the output pipe and blocks
//!   the client until the terminal answers `ESC[{row};{col}R` (CPR,
//!   cursor-position report) on the input pipe. A byte-collecting harness
//!   that never answers leaves the child frozen pre-prompt: the only bytes
//!   that ever arrive are the 4 DSR bytes, `set`/`echo` input sits
//!   unprocessed, and even one-shot `cmd /C exit N` never exits. Every
//!   read/wait loop below therefore replies `ESC[1;1R` once per observed
//!   DSR via [`reply_to_dsrs`]. A real terminal answers with its actual
//!   cursor position; `1;1` is a stub that only unblocks client progress,
//!   which is all these Tier-1 tests assert.
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

/// ConPTY cursor-query request: conhost emits this when the console client
/// needs cursor state (see module docs). Each occurrence must be answered
/// once or the child stays frozen.
const DSR_REQUEST: &[u8] = b"\x1b[6n";
/// Stub cursor-position report answering [`DSR_REQUEST`]: row 1, column 1.
/// A real terminal reports its live cursor; these Tier-1 tests only need
/// client progress, so the origin stub suffices.
const CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// Answers every not-yet-answered [`DSR_REQUEST`] occurrence in `buf` by
/// writing [`CPR_REPLY`] once per occurrence. `answered` counts replies
/// already sent for this buffer, so split or repeated requests each get
/// exactly one reply. Write failures are ignored: the child may have
/// exited (one-shot tests) or been killed, in which case there is nobody
/// left to unblock and the caller asserts on bytes already collected.
fn reply_to_dsrs(writer: &mut bitty_pty::PtyWriter, buf: &[u8], answered: &mut usize) {
    let mut occurrences = 0;
    for window in buf.windows(DSR_REQUEST.len()) {
        if window == DSR_REQUEST {
            occurrences += 1;
        }
    }
    while *answered < occurrences {
        let _ = writer.write_all(CPR_REPLY);
        let _ = writer.flush();
        *answered += 1;
    }
}

/// Reads until `needle` is observed, EOF, or `deadline` — whichever comes
/// first — and returns whatever arrived. Never blocks past the deadline.
/// Callers assert on the returned bytes (with byte count and lossy text in
/// the message) so a missing marker fails with evidence, not a hang.
///
/// Replies to conhost DSR requests along the way (see [`reply_to_dsrs`]):
/// without the CPR answers the child never produces the marker at all.
fn read_until(
    reader: &bitty_pty::PtyReader,
    writer: &mut bitty_pty::PtyWriter,
    needle: &[u8],
    deadline: Instant,
) -> Vec<u8> {
    debug_assert!(!needle.is_empty(), "read_until needs a non-empty marker");
    let mut out = Vec::new();
    let mut dsrs_answered = 0;
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
                reply_to_dsrs(writer, &out, &mut dsrs_answered);
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

/// Bounded reap for one-shot children that must exit on their own while
/// answering conhost DSR requests.
///
/// Even `cmd /C exit N` initializes the console and can stall behind an
/// unanswered DSR (win469d: `cmd /C exit 0` never exited within 30s with
/// no harness replies). So this polls `wait_timeout` in [`RECV_TICK`]
/// slices while draining whatever the pump delivered and answering DSRs;
/// output bytes are discarded (this path asserts exit codes, not output).
/// On timeout kills the child, reaps it, and panics with the final status.
/// The overall bound is [`WAIT_TIMEOUT`]; no path blocks past it.
fn reap_one_shot_with_dsr_pump(
    pty: &mut bitty_pty::Pty,
    reader: &bitty_pty::PtyReader,
    writer: &mut bitty_pty::PtyWriter,
    what: &str,
) -> bitty_pty::ExitStatus {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let mut seen = Vec::new();
    let mut dsrs_answered = 0;
    loop {
        match pty.wait_timeout(RECV_TICK).expect("wait_timeout slice") {
            Some(status) => return status,
            None => {
                if Instant::now() >= deadline {
                    let _ = pty.kill();
                    let after = pty
                        .wait_timeout(KILL_REAP_TIMEOUT)
                        .expect("reap after kill");
                    panic!("{what} did not exit within {WAIT_TIMEOUT:?}; killed, reap: {after:?}");
                }
                // Child still alive: drain any DSR request and answer it so
                // console init can proceed; discard the bytes themselves.
                while let Ok(Some(chunk)) = reader.recv_timeout(RECV_TICK) {
                    seen.extend_from_slice(&chunk);
                    reply_to_dsrs(writer, &seen, &mut dsrs_answered);
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
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
    // One-shot success: `cmd /C exit 0` must reap cleanly. The DSR pump
    // is required even though no output is asserted: console init stalls
    // behind an unanswered DSR (win469d).
    let mut pty = PtyBuilder::new("cmd.exe")
        .arg("/C")
        .arg("exit 0")
        .spawn()
        .expect("spawn cmd /C exit 0");
    let reader = pty.take_reader().expect("reader half");
    let mut writer = pty.take_writer().expect("writer half");
    let status = reap_one_shot_with_dsr_pump(&mut pty, &reader, &mut writer, "cmd /C exit 0");
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
    let reader = pty.take_reader().expect("reader half");
    let mut writer = pty.take_writer().expect("writer half");
    let status = reap_one_shot_with_dsr_pump(&mut pty, &reader, &mut writer, "cmd /C exit 3");
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

    // `& echo` appends a sentinel AFTER `set` finishes (`&` runs it
    // unconditionally), so waiting for the sentinel proves the whole
    // environment listing arrived. `set` output sorts alphabetically:
    // stopping at the first marker (`BITTY_PROBE=1`) returns mid-stream
    // (win469e: 1290 bytes ending mid-CARGO) while `TERM=*` sorts later.
    // The leading `\r\n` keeps the input echo (`...echo SENTINEL`) from
    // matching early: only the command-output line is newline-delimited
    // on both sides.
    writer
        .write_all(b"set & echo BITTY_SET_DONE\r\n")
        .expect("write set to pty");
    writer.flush().expect("flush");

    // `set` separates with `\r\n`; byte-substring search is agnostic.
    // Bounded: on timeout the partial bytes below show what arrived.
    let deadline = Instant::now() + CMD_TIMEOUT;
    let output = read_until(&reader, &mut writer, b"\r\nBITTY_SET_DONE\r\n", deadline);
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
    let output = read_until(&reader, &mut writer, b"TERM_PROGRAM=custom-term", deadline);
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
    let echoed = read_until(&reader, &mut writer, b"hello-bitty-pty", deadline);
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
