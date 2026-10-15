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
    while let Ok(Some(chunk)) = reader.recv() {
        assert!(
            chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
            "chunk exceeded declared read size"
        );
        out.extend_from_slice(&chunk);
        assert!(std::time::Instant::now() < deadline, "test timed out");
    }
    out
}

/// Reads until `marker` is observed, returning everything so far.
///
/// Never panics: EOF, a dead pump, or a quiet stream past the bound all end
/// the wait and return short; the caller owns what a missing marker means
/// (the `spawn_gated` respawn, or content assertions). The total wait stays
/// within `ECHO_TIMEOUT` via a shrinking per-recv bound.
fn observe_until(reader: &bitty_pty::PtyReader, marker: &[u8]) -> Vec<u8> {
    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut out = Vec::new();
    while !contains(&out, marker) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match reader.recv_timeout(remaining) {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk),
            _ => break,
        }
    }
    out
}

/// Spawns a `read`-gated shell, observes `marker`, releases the gate.
///
/// `spawn` builds the PTY (typically `sh -c '<stmt>; read dummy'` plus any
/// builder config) and reports the library-level result instead of panicking:
/// the crate already retries the transient FreeBSD `TIOCSCTTY` race (EPERM /
/// ENOTTY, CTX-1020) five times internally, so a surviving `Err` is either a
/// genuine misconfiguration or residual emulated-VM load. The gate holds the
/// slave open — the hold-open half of the FreeBSD exit-vs-drain fix
/// (CTX-1019/CTX-1020 family): the child cannot exit, and the kernel cannot
/// discard undrained output on slave close, before the expected bytes are
/// seen. Returns the live PTY, its reader, and the bytes observed so far; the
/// caller drains the rest and joins.
///
/// Resilience: a spawn `Err` or a missing marker drops the dud PTY (its
/// `Drop` kills and reaps, so a stuck shell cannot linger) and retries once
/// with a fresh device. Only those two cases trigger the single respawn —
/// partial output that ends at EOF is returned as-is, so genuinely wrong
/// content still fails in the caller's own assertions. A second miss panics
/// with the terminal state attached. The retry covers the residual FreeBSD
/// transients under parallel-spawn load in the emulated VM: a stale terminal
/// state on a recycled PTS (shell never runs, nothing observable
/// distinguishes it from a hung child) or an `EPERM`/`ENOTTY` that survived
/// the library-level retries. A fresh spawn recovers; a genuine failure fails
/// twice and still panics with diagnosis.
fn spawn_gated(
    marker: &[u8],
    spawn: impl Fn() -> Result<bitty_pty::Pty, bitty_pty::PtyError>,
) -> (bitty_pty::Pty, bitty_pty::PtyReader, Vec<u8>) {
    let mut diagnosis = String::from("no attempts ran");
    for _ in 0..2 {
        let mut pty = match spawn() {
            Ok(pty) => pty,
            Err(err) => {
                diagnosis = format!("spawn failed: {err:?}");
                continue;
            }
        };
        let reader = pty.take_reader().expect("reader half");
        let mut writer = pty.take_writer().expect("writer half");
        let observed = observe_until(&reader, marker);
        if contains(&observed, marker) {
            writer.write_all(b"\n").expect("release read gate");
            writer.flush().expect("flush read gate");
            drop(writer);
            return (pty, reader, observed);
        }
        // Marker missing: record the terminal state for the panic below,
        // then drop the dud (its `Drop` kills and reaps, so a stuck shell
        // cannot linger) and try once more with a fresh spawn.
        diagnosis = format!(
            "child={:?} fg={:?} observed={:?}",
            pty.pid(),
            pty.foreground_pgid(),
            String::from_utf8_lossy(&observed),
        );
        drop(pty);
    }
    panic!("gated child never produced the marker after a fresh respawn: {diagnosis}");
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
fn child_environment_inherits_session_with_overrides() {
    let (_pty, reader, mut out) = spawn_gated(b"__BITTY_ENV_DONE__", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/env; echo __BITTY_ENV_DONE__; read dummy")
            .env("BITTY_PROBE", "1")
            .spawn()
    });

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&out);
    // PTY output on macOS includes CRLF, line-discipline control chars and
    // caret echo (e.g. "\r\n^D\x08\x08BITTY_PROBE=1\r\n" where ^D is 0x04
    // echoed as "^D" or raw). Normalize by stripping caret notation then
    // extracting KEY=VALUE with alphanumeric scan to tolerate leading garbage.
    let mut normalized: Vec<String> = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // Remove all control bytes first (including \x04, \x08, \r etc)
        let cleaned0: String = line.chars().filter(|c| !c.is_control()).collect();
        // Strip caret echo sequences: "^@".. "^Z", "^[", "^\", "^]", "^^", "^_", "^?"
        // as emitted by the PTY line discipline for control bytes.
        let mut cleaned = String::with_capacity(cleaned0.len());
        let mut chars = cleaned0.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '^' {
                if let Some(&next) = chars.peek() {
                    let nb = next as u8;
                    if (0x40..=0x5F).contains(&nb) || nb == b'?' {
                        chars.next();
                        continue;
                    }
                }
            }
            cleaned.push(c);
        }
        let cleaned = cleaned.trim();
        if cleaned.is_empty() || !cleaned.contains('=') {
            continue;
        }
        // Extract key as contiguous [A-Za-z0-9_] immediately before '='
        if let Some(eq_pos) = cleaned.find('=') {
            let bytes = cleaned.as_bytes();
            let mut start = eq_pos;
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
            {
                start -= 1;
            }
            let key = &cleaned[start..eq_pos];
            let value = cleaned[eq_pos + 1..].trim();
            if !key.is_empty() {
                normalized.push(format!("{}={}", key, value));
            } else {
                normalized.push(cleaned.to_string());
            }
        }
    }
    // Use substring-tolerant checks for presence; exact line match fails on
    // macOS due to control/caret prefix, so check normalized entries.
    assert!(
        normalized.iter().any(|l| l == "TERM=xterm-256color")
            || text.contains("TERM=xterm-256color"),
        "default TERM missing from {text:?} normalized {normalized:?}"
    );
    assert!(
        normalized.iter().any(|l| l == "COLORTERM=truecolor")
            || text.contains("COLORTERM=truecolor"),
        "default COLORTERM missing from {text:?} normalized {normalized:?}"
    );
    assert!(
        normalized.iter().any(|l| l == "BITTY_PROBE=1") || text.contains("BITTY_PROBE=1"),
        "allowlisted entry missing from {text:?} normalized {normalized:?}"
    );
    // Verify child inherits environment entries from parent (e.g. PATH is always present)
    assert!(
        normalized.iter().any(|l| l.starts_with("PATH=")) || text.contains("PATH="),
        "inherited PATH missing from child environment: {text:?} normalized {normalized:?}"
    );
}

#[test]
fn child_environment_builder_overrides_defaults() {
    let (_pty, reader, mut out) = spawn_gated(b"__BITTY_ENV_DONE__", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/env; echo __BITTY_ENV_DONE__; read dummy")
            .env("TERM", "custom-256color")
            .env("COLORTERM", "custom-color")
            .spawn()
    });

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("TERM=custom-256color"),
        "expected custom TERM override in {text:?}"
    );
    assert!(
        text.contains("COLORTERM=custom-color"),
        "expected custom COLORTERM override in {text:?}"
    );
    assert!(
        !text.contains("TERM=xterm-256color"),
        "default TERM should have been overridden in {text:?}"
    );
    assert!(
        !text.contains("COLORTERM=truecolor"),
        "default COLORTERM should have been overridden in {text:?}"
    );
}

#[test]
fn child_has_term_program_bitty_by_default() {
    // CTX-0194: TERM_PROGRAM must read `bitty` so term-DB probes fall back
    // to symbols instead of Kitty-graphics APC. No chafa dependency: assert
    // the sanitized child environment directly via headless PTY byte capture.
    let (_pty, reader, mut out) = spawn_gated(b"__BITTY_ENV_DONE__", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/env; echo __BITTY_ENV_DONE__; read dummy")
            .spawn()
    });

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("TERM_PROGRAM=bitty"),
        "expected TERM_PROGRAM=bitty in {text:?}"
    );
}

#[test]
fn child_explicit_term_program_override_wins() {
    let (_pty, reader, mut out) = spawn_gated(b"__BITTY_ENV_DONE__", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/env; echo __BITTY_ENV_DONE__; read dummy")
            .env("TERM_PROGRAM", "custom-term")
            .spawn()
    });

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("TERM_PROGRAM=custom-term"),
        "explicit TERM_PROGRAM should win in {text:?}"
    );
    assert!(
        !text.contains("TERM_PROGRAM=bitty"),
        "default TERM_PROGRAM must be overridden in {text:?}"
    );
}

#[test]
#[allow(unsafe_code)]
fn child_graphics_fingerprint_is_sanitized() {
    // CTX-0194 regression: simulate a bitty launched from ghostty/kitty/
    // wezterm/iTerm (post-CTX-0165 inherit-then-override) by poisoning the
    // parent environment, then assert the PTY child is sanitized. Headless
    // PTY byte capture; chafa is not required as a dependency.
    let poison: &[(&str, &str)] = &[
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.3.1-poison"),
        ("GHOSTTY_BIN_DIR", "/tmp/bitty-poison"),
        ("GHOSTTY_RESOURCES_DIR", "/tmp/bitty-poison-res"),
        ("WEZTERM_PANE", "9-poison"),
        ("WEZTERM_EXECUTABLE", "/tmp/bitty-poison-wezterm"),
        ("KITTY_PID", "99999"),
        ("KITTY_WINDOW_ID", "7-poison"),
        ("KITTY_LISTEN_ON", "/tmp/bitty-poison-kitty.sock"),
        ("VTE_VERSION", "7500"),
        ("ITERM_SESSION_ID", "w0t0p0:POISON"),
        ("ITERM_PROFILE", "Poison"),
        ("LC_TERMINAL", "iTerm2"),
        ("LC_TERMINAL_VERSION", "3.5.0-poison"),
    ];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = poison
        .iter()
        .map(|(k, _)| (*k, std::env::var_os(k)))
        .collect();

    let (_pty, reader, mut out) = spawn_gated(b"__BITTY_ENV_DONE__", || {
        for (k, v) in poison {
            // `std::env::set_var` is (correctly) flagged unsafe in Rust 2024
            // because it races with `getenv` in other threads; the poison
            // window here is narrowed to spawn-only and restored immediately
            // after. Re-applied on every attempt: a respawned PTY must see
            // the same poisoned parent.
            unsafe {
                std::env::set_var(k, v);
            }
        }
        let spawned = PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("/usr/bin/env; echo __BITTY_ENV_DONE__; read dummy")
            .spawn();

        // Restore the parent environment immediately: the child snapshot is
        // taken at spawn time, so later assertions cannot be affected.
        for (k, prev) in &saved {
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        spawned
    });

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&out);
    // Sanitized: parent fingerprints must not reach the child; TERM_PROGRAM
    // is overridden to bitty rather than removed.
    assert!(
        text.contains("TERM_PROGRAM=bitty"),
        "TERM_PROGRAM must be overridden to bitty in {text:?}"
    );
    for marker in [
        "TERM_PROGRAM=ghostty",
        "TERM_PROGRAM_VERSION=",
        "GHOSTTY_BIN_DIR=",
        "GHOSTTY_RESOURCES_DIR=",
        "WEZTERM_PANE=",
        "WEZTERM_EXECUTABLE=",
        "KITTY_PID=",
        "KITTY_WINDOW_ID=",
        "KITTY_LISTEN_ON=",
        "VTE_VERSION=",
        "ITERM_SESSION_ID=",
        "ITERM_PROFILE=",
        "LC_TERMINAL=",
        "LC_TERMINAL_VERSION=",
    ] {
        assert!(
            !text.contains(marker),
            "graphics fingerprint {marker:?} must be stripped, got {text:?}"
        );
    }
    // Functional environment still inherits (CTX-0165 posture preserved).
    assert!(
        text.contains("PATH="),
        "inherited PATH must survive sanitization in {text:?}"
    );
    assert!(
        text.contains("TERM=xterm-256color"),
        "default TERM must survive sanitization in {text:?}"
    );
    assert!(
        text.contains("COLORTERM=truecolor"),
        "default COLORTERM must survive sanitization in {text:?}"
    );
}

#[test]
fn cwd_is_applied_to_child() {
    // `sh -c 'pwd; …'` instead of a bare `/bin/pwd` (same family as the
    // `shell_echo` fix, CTX-1019): `pwd` exits microseconds after writing,
    // so the slave can close before the pump's first master read and the
    // tiny output is lost. The trailing `read` gate holds the shell open
    // until output is observed, without changing what the test proves
    // (the `-c` string is still a single argv element, and `pwd` still runs
    // in the builder-supplied cwd). Gating waits for a distinct DONE marker
    // (CodeRabbit on #1830): coupling the gate to the asserted `/tmp` path
    // would retry a genuinely wrong cwd instead of failing the assertion,
    // and keeps the gate consistent with the env tests.
    let (mut pty, reader, mut out) = spawn_gated(b"__BITTY_CWD_DONE__", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("pwd; printf '__BITTY_CWD_DONE__\\n'; read dummy")
            .cwd("/tmp")
            .spawn()
    });

    let status = pty.wait().expect("sh exits after read gate");
    assert!(status.is_success());

    let rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    out.extend_from_slice(&rest);
    reader.join().expect("pump clean");
    let text = String::from_utf8_lossy(&out);
    // /tmp is a symlink to /private/tmp on macOS; canonicalize both sides.
    // Only the first line carries `pwd` output: releasing the `read` gate
    // and dropping the writer echo extra newlines and caret sequences
    // (notably macOS `^D` EOF echo), which must not pollute the comparison.
    let raw = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let cleaned: String = raw
        .trim_start_matches(|c: char| c.is_control())
        .trim_end_matches(|c: char| c.is_control())
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let reported = cleaned.trim();
    let expected = std::path::Path::new("/tmp");
    let canonical_expected =
        std::fs::canonicalize(expected).unwrap_or_else(|_| expected.to_path_buf());
    let reported_path = std::path::Path::new(reported);
    let canonical_reported =
        std::fs::canonicalize(reported_path).unwrap_or_else(|_| reported_path.to_path_buf());
    assert!(
        reported == "/tmp"
            || reported == "/private/tmp"
            || canonical_reported == canonical_expected
            || reported_path == canonical_expected,
        "pwd reported {text:?} (normalized {reported:?}) canonical {canonical_reported:?} expected /tmp canonical {canonical_expected:?}"
    );
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

#[test]
fn shell_echo_via_sh_with_bounded_backpressure() {
    // Real shell echo dogfood for 0.0.1: `sh -c '…'` proves direct argv
    // exec (no shell interpolation inside bitty-pty), the bounded channel, and
    // clean exit. Works headlessly — no window or GPU required.
    //
    // FreeBSD robustness (0.0.22 release leg): the shell must stay alive until
    // the reader has observed the echo. A bare `sh -c 'echo …'` exits
    // immediately after writing, so on FreeBSD 15.1 the slave can close
    // before the pump's first master read and the tiny output is lost
    // (`expected shell echo, got []` while Linux/macOS/Windows still deliver
    // the queued bytes). The trailing `read` gate holds the slave open until
    // the test releases it with a newline, removing the exit-vs-drain race on
    // every platform without changing what the test proves (the `-c` string
    // is still a single argv element: any interpolation inside bitty-pty
    // would break the `;` sequencing). Spawned through `spawn_gated`
    // (respawn-on-silence) like the other gated tests below.
    let (mut pty, reader, mut out) = spawn_gated(b"hello-bitty-pty", || {
        PtyBuilder::new("/bin/sh")
            .arg("-c")
            .arg("echo hello-bitty-pty; read dummy")
            .spawn()
    });

    // PTY size is still kernel-queryable even for a shell child.
    let (cols, rows) = pty.size().expect("size after shell spawn");
    assert!(
        cols >= 10 && rows >= 5,
        "unexpected initial size {cols}x{rows}"
    );
    assert!(
        contains(&out, b"hello-bitty-pty"),
        "expected shell echo, got {out:?} as {}",
        String::from_utf8_lossy(&out)
    );

    let status = pty.wait().expect("reap sh");
    assert!(
        status.is_success(),
        "shell echo should exit 0, got {status:?}"
    );

    // Drain remaining bytes (e.g. trailing newline, shell prompt if any),
    // proving the try_recv path does not break backpressure: a non-blocking
    // poll while draining must not panic and must respect the same bound
    // if it yields data. Assert pump ended cleanly with bounded semantics.
    let rest_deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    while let Ok(Some(chunk)) = reader.recv() {
        assert!(
            chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
            "shell echo chunk {} exceeds READ_CHUNK_SIZE {}",
            chunk.len(),
            bitty_pty::READ_CHUNK_SIZE
        );
        out.extend_from_slice(&chunk);
        if let bitty_pty::PtyRecv::Chunk(extra) = reader.try_recv() {
            assert!(extra.len() <= bitty_pty::READ_CHUNK_SIZE);
            out.extend_from_slice(&extra);
        }
        assert!(
            std::time::Instant::now() < rest_deadline,
            "shell echo drain timed out"
        );
    }
    reader.join().expect("pump ended cleanly after shell");
}

/// Outcome of a single flood-drain attempt against one PTY.
struct FloodAttempt {
    chunks: usize,
    total: usize,
    max_chunk: usize,
    /// True when the drain saw clean EOF; false when the overall deadline
    /// expired or the pump died mid-drain.
    finished: bool,
}

/// Maximum flood attempts: the first spawn plus bounded fresh-PTY respawns.
///
/// CTX-1255 (re-release run 38809771936): the SIGHUP fast-exit transient is
/// bursty — consecutive spawns can go silent back-to-back, burning the old
/// budget of 2 in 0.14s without consuming any deadline. Five matches the
/// library's own `MAX_SPAWN_ATTEMPTS` bound for the same FreeBSD spawn-race
/// family (`platform/unix.rs`); worst-case cost stays inside the shared
/// `ECHO_TIMEOUT` (silent stalls are time-shared by
/// `silent_deadline_for_attempt`, instant failures cost only a
/// `FLOOD_RETRY_PAUSE` settle pause each).
const MAX_FLOOD_ATTEMPTS: usize = 5;

/// Settle pause between flood attempts: lets a SIGHUP burst clear instead of
/// hot-spinning spawns inside the same burst window. Bounded (four 100 ms
/// pauses worst case, ~4% of the shared deadline) and paid only on the
/// already-silent path; a healthy first attempt never sleeps.
const FLOOD_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Whether a flood attempt's outcome earns a fresh-PTY respawn (CTX-1251).
///
/// Only a silent startup is retryable: the attempt delivered nothing, so it
/// carries no signal about the bound under test. That covers both FreeBSD
/// recycled-PTS transient signatures — a stalled drain (the child took a
/// stale hangup SIGHUP at startup while something still holds the slave, so
/// the pump never sees EOF) and a clean EOF with zero chunks (a fast `seq`
/// exit discarded before the pump's first read, the CTX-1019 exit-vs-drain
/// race, or an instant SIGHUP fast-exit, CTX-1255). Any attempt that
/// delivered output is accepted (clean EOF) or a genuine failure (mid-stream
/// stall), never a retry; the bound asserts in the caller still run on every
/// accepted attempt.
fn flood_attempt_needs_retry(attempt: &FloodAttempt, attempts_used: usize) -> bool {
    if attempts_used >= MAX_FLOOD_ATTEMPTS {
        return false;
    }
    attempt.chunks == 0
}

/// Silent-phase deadline for a flood attempt: how long the drain waits for
/// the *first* chunk before giving up the attempt.
///
/// Every non-final attempt must reserve time for the respawns it may earn: a
/// silent startup stall (the CTX-1251 SIGHUP signature) would otherwise burn
/// the whole shared deadline and leave the retries nothing to run on. So each
/// non-final attempt waits only an equal share of the remaining shared budget
/// for its first chunk (attempt 1 of N gets ~1/N); once any chunk arrives the
/// attempt is healthy and drains until the shared deadline, and the final
/// attempt always gets the full remainder (a last silence is genuine and
/// panics loud). With two attempts this is exactly the CTX-1254 half-split
/// (attempt 1: half, attempt 2: full remainder).
fn silent_deadline_for_attempt(
    deadline: std::time::Instant,
    attempts_used: usize,
) -> Option<std::time::Instant> {
    let remaining_attempts = MAX_FLOOD_ATTEMPTS.saturating_sub(attempts_used);
    if remaining_attempts == 0 {
        return None;
    }
    let now = std::time::Instant::now();
    let remaining = deadline.saturating_duration_since(now);
    let shares = remaining_attempts.saturating_add(1);
    Some(now + remaining / (shares as u32))
}

/// Drains one flood PTY until clean EOF or the shared deadline, asserting
/// the per-chunk and total bounds on the way.
///
/// Returns the attempt outcome instead of panicking: the caller owns the
/// retry-or-fail-loud decision (`flood_attempt_needs_retry`). `finished` is
/// true only on clean EOF; a stall (deadlines expired or pump failure) sets
/// it false and the caller panics with the child state attached.
///
/// `silent_deadline` caps only the wait for the *first* chunk (the attempt-1
/// reservation from `silent_deadline_for_attempt`): while still silent the
/// drain waits on it, so a startup stall leaves time for the fresh-PTY
/// respawn. After the first chunk — or when `None` (final attempt) — the
/// full shared `deadline` applies, so a healthy but slow drain is never cut
/// short for retry time it does not need.
fn drain_flood_attempt(
    reader: &bitty_pty::PtyReader,
    deadline: std::time::Instant,
    silent_deadline: Option<std::time::Instant>,
) -> FloodAttempt {
    let mut total = 0usize;
    let mut chunks = 0usize;
    let mut max_chunk = 0usize;
    loop {
        let wait_deadline = if chunks == 0 {
            silent_deadline.unwrap_or(deadline)
        } else {
            deadline
        };
        let remaining = wait_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return FloodAttempt {
                chunks,
                total,
                max_chunk,
                finished: false,
            };
        }
        match reader.recv_timeout(remaining) {
            Ok(Some(chunk)) => {
                assert!(
                    chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
                    "flood chunk {} exceeds READ_CHUNK_SIZE {}",
                    chunk.len(),
                    bitty_pty::READ_CHUNK_SIZE
                );
                max_chunk = max_chunk.max(chunk.len());
                total += chunk.len();
                chunks += 1;
                // The channel itself is bounded to 16 chunks; even if we drained
                // slowly, total buffered inside the crate at any instant could never
                // exceed MAX_BUFFERED_BYTES. We prove the weaker invariant that no
                // single chunk exceeds READ_CHUNK_SIZE and that the pump completes
                // without unbounded growth (total > MAX shunts would have hung or
                // panicked if backpressure were broken).
                //
                // `seq 1 5000` emits 23,893 bytes raw (numbers 1..5000 plus
                // newlines); PTY ONLCR expands each LF to CRLF, so at most
                // 28,893 bytes reach the master. The bound below (48,192)
                // leaves ~19 KiB of headroom for line-discipline echo;
                // anything beyond means backpressure duplicated or leaked.
                assert!(
                    total <= 5000 * 8 + 8192,
                    "unreasonable total {total}, backpressure may have duplicated or leaked"
                );
            }
            // Clean EOF once the queue drained: `seq` exited on its own.
            Ok(None) => {
                return FloodAttempt {
                    chunks,
                    total,
                    max_chunk,
                    finished: true,
                };
            }
            // Stall or dead pump: report it, never hang. The caller turns
            // this into a retry (silent startup transient) or a loud panic
            // with the child state attached.
            Err(_) => {
                return FloodAttempt {
                    chunks,
                    total,
                    max_chunk,
                    finished: false,
                };
            }
        }
    }
}

#[test]
fn flood_retry_covers_only_silent_startup_transients() {
    // CTX-1251: a flood attempt that delivered nothing is a startup
    // transient (stale-PTS SIGHUP stall, a fast-`seq` exit discarded
    // before the pump's first read, or an instant SIGHUP fast-exit burst,
    // CTX-1255) and earns a fresh-PTY respawn while budget remains; an
    // exhausted budget is genuine and must panic loud.
    let silent_eof = FloodAttempt {
        chunks: 0,
        total: 0,
        max_chunk: 0,
        finished: true,
    };
    assert!(flood_attempt_needs_retry(&silent_eof, 1));
    assert!(flood_attempt_needs_retry(
        &silent_eof,
        MAX_FLOOD_ATTEMPTS - 1
    ));
    assert!(!flood_attempt_needs_retry(&silent_eof, MAX_FLOOD_ATTEMPTS));
    let stalled_silent = FloodAttempt {
        chunks: 0,
        total: 0,
        max_chunk: 0,
        finished: false,
    };
    assert!(flood_attempt_needs_retry(&stalled_silent, 1));
    assert!(!flood_attempt_needs_retry(
        &stalled_silent,
        MAX_FLOOD_ATTEMPTS
    ));
    // Any attempt that delivered output says nothing about startup: a
    // clean EOF is accepted (the bound asserts below still run on it) and
    // a mid-stream stall is a genuine failure, never a retry.
    let partial_eof = FloodAttempt {
        chunks: 3,
        total: 100,
        max_chunk: 64,
        finished: true,
    };
    assert!(!flood_attempt_needs_retry(&partial_eof, 1));
    let partial_stall = FloodAttempt {
        chunks: 3,
        total: 100,
        max_chunk: 64,
        finished: false,
    };
    assert!(!flood_attempt_needs_retry(&partial_stall, 1));
}

#[test]
fn flood_silent_deadline_reserves_retry_time_for_non_final_attempts() {
    // CTX-1251 (CodeRabbit on #2125), generalized by CTX-1255: every
    // non-final attempt must reserve time for the respawns it may earn, or a
    // silent stall burns the whole shared deadline and the retries have
    // nothing to run on. Only the final attempt waits on the full remainder.
    let far = std::time::Instant::now() + Duration::from_secs(10);
    let first = silent_deadline_for_attempt(far, 1).expect("attempt 1 caps silence");
    let now = std::time::Instant::now();
    assert!(first > now, "silent deadline must be in the future");
    assert!(
        first <= far,
        "silent deadline must not exceed the shared one"
    );
    // Attempt 1 of MAX_FLOOD_ATTEMPTS takes one equal share, leaving most
    // of the budget for the respawns.
    assert!(
        first < now + Duration::from_secs(4),
        "attempt 1 must leave budget for the respawns, got {first:?}"
    );
    assert!(
        silent_deadline_for_attempt(far, MAX_FLOOD_ATTEMPTS - 1).is_some(),
        "penultimate attempt still reserves retry time"
    );
    assert!(
        silent_deadline_for_attempt(far, MAX_FLOOD_ATTEMPTS).is_none(),
        "final attempt waits on the full remainder"
    );
}

#[test]
fn flood_drain_attempt_reports_clean_eof_with_output() {
    // Small live probe of the shared drain helper: direct `seq` resolves
    // through PATH on every runner and the helper reports clean EOF with
    // output. Bounded fresh-PTY respawns with a settle pause absorb the
    // FreeBSD burst-silence transient (CTX-1255, re-release run 38809771936:
    // back-to-back spawns died instantly with SIGHUP and zero chunks, so the
    // old budget of 2 burned in 0.14s without consuming any deadline). Every
    // bound assert below still runs unchanged on the accepted attempt.
    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut attempts_used = 0usize;
    let mut accepted: Option<FloodAttempt> = None;
    let mut diagnosis = String::from("no attempts ran");
    let mut pty = None;
    let mut reader = None;
    while attempts_used < MAX_FLOOD_ATTEMPTS {
        attempts_used += 1;
        let mut spawned = PtyBuilder::new("seq")
            .arg("1")
            .arg("10")
            .spawn()
            .expect("spawn seq");
        let spawned_reader = spawned.take_reader().expect("reader half");
        let attempt = drain_flood_attempt(
            &spawned_reader,
            deadline,
            silent_deadline_for_attempt(deadline, attempts_used),
        );
        if attempt.finished && attempt.chunks > 0 {
            accepted = Some(attempt);
            pty = Some(spawned);
            reader = Some(spawned_reader);
            break;
        }
        if flood_attempt_needs_retry(&attempt, attempts_used) {
            // Silent startup transient: record the terminal state, drop the
            // dud (its `Drop` kills and reaps within a bound, so a stuck
            // holder of the slave cannot linger), and pause so a SIGHUP
            // burst can clear before the next fresh device.
            diagnosis = format!(
                "attempt {attempts_used}: {} chunks={} total={} \
                 child pid={:?} fg={:?} exit={:?} pump_error={:?}",
                if attempt.finished {
                    "silent EOF"
                } else {
                    "stalled drain"
                },
                attempt.chunks,
                attempt.total,
                spawned.pid(),
                spawned.foreground_pgid(),
                spawned.try_wait().ok().flatten(),
                spawned_reader.pump_error(),
            );
            drop(spawned);
            std::thread::sleep(FLOOD_RETRY_PAUSE);
            continue;
        }
        // Output started but the drain stalled, or the final attempt stayed
        // silent: genuine failure, loud with the child state attached.
        diagnosis = format!(
            "attempt {attempts_used}: {} chunks={} total={}",
            if attempt.finished {
                "silent EOF"
            } else {
                "stalled drain"
            },
            attempt.chunks,
            attempt.total,
        );
        break;
    }
    let attempt = accepted.unwrap_or_else(|| panic!("seq 1 10 must deliver output: {diagnosis}"));
    assert!(attempt.total > 0);
    assert!(attempt.max_chunk > 0 && attempt.max_chunk <= bitty_pty::READ_CHUNK_SIZE);
    let mut pty = pty.expect("pty half");
    let status = pty.wait().expect("reap seq");
    assert!(status.is_success(), "seq should exit 0, got {status:?}");
    reader.expect("reader half").join().expect("pump clean");
}

#[test]
fn backpressure_bound_holds_under_flood() {
    // Flood the bounded channel with a self-terminating single process:
    // `seq 1 5000` exits on its own after bounded output, with no
    // pipeline-death dependence. The previous `yes | head -n 5000` source
    // hung forever on FreeBSD 15.1 (CTX-1249 / #2119): when the pipeline
    // never dies no EOF arrives, and the old `while let Ok(Some) =
    // reader.recv()` drain blocked forever because its deadline assert only
    // ran after a chunk arrived (zero chunks = infinite block, stuck
    // release).
    //
    // CTX-1251 (reopened #2119, re-release run 38781204894): the
    // self-terminating source plus the fail-closed drain worked as designed
    // — the 60-minute silent hang became a loud 10-second timeout — but the
    // child died at startup with SIGHUP and zero chunks ever arrived
    // (`flood drain stalled (Timeout): chunks=0 total=0 ... fg=None
    // exit=Hangup pump_error=None`). Root-cause analysis:
    //
    // - The spawn path has no Linux/FreeBSD divergence to fix: `portable-pty`
    //   resets the inherited signal dispositions (including SIGHUP) to
    //   `SIG_DFL`, unblocks the mask, and runs `setsid` + `TIOCSCTTY`
    //   identically on every Unix. A SIGHUP that lands after a successful
    //   spawn is a kernel-delivered hangup on a recycled PTS (the CTX-1020
    //   family already documents stale terminal state on recycled devices
    //   under parallel-spawn load in the emulated FreeBSD VM), outside
    //   anything the library controls — so, as with that family, the right
    //   layer is a bounded fresh-PTY retry in the test, not a spawn change.
    // - The old `sh -c` middleman is gone: `seq` is spawned directly as an
    //   argv vector (no shell fork/orphan/job-control chain left to
    //   diagnose, and direct argv exec matches the crate's security
    //   posture). The bare name resolves through PATH on every runner
    //   (FreeBSD base, macOS BSD userland, GNU coreutils on Linux), so no
    //   absolute-path portability claim is needed.
    // - A silent attempt (stalled drain or clean EOF, both with zero chunks)
    //   is a startup transient and earns a fresh-PTY respawn (bounded by
    //   `MAX_FLOOD_ATTEMPTS`, with a `FLOOD_RETRY_PAUSE` settle pause between
    //   attempts so a bursty run of consecutive silent spawns can clear —
    //   CTX-1255 saw two instant silents burn the old budget of 2 in 0.14s);
    //   the shared `ECHO_TIMEOUT` deadline still bounds all attempts together
    //   (each non-final attempt waits only an equal share of the remaining
    //   budget for its first chunk, so a silent stall always leaves time for
    //   the respawns; after the first chunk the full deadline applies), and
    //   every bound assert below still runs unchanged on the accepted attempt.
    //   An exhausted budget, or a mid-stream stall after output started,
    //   panics loud with the child state attached instead of hanging the
    //   release.
    //
    // The kernel PTY buffer + channel backpressure blocks the child instead
    // of growing the heap; the in-crate buffer never exceeds
    // MAX_BUFFERED_BYTES.
    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut diagnosis = String::from("no attempts ran");
    let mut attempts_used = 0usize;
    let mut accepted: Option<(FloodAttempt, bitty_pty::Pty, bitty_pty::PtyReader)> = None;
    while attempts_used < MAX_FLOOD_ATTEMPTS {
        attempts_used += 1;
        let mut pty = match PtyBuilder::new("seq").arg("1").arg("5000").spawn() {
            Ok(pty) => pty,
            Err(err) => {
                diagnosis = format!("attempt {attempts_used}: spawn failed: {err:?}");
                continue;
            }
        };
        let reader = pty.take_reader().expect("reader half");
        let attempt = drain_flood_attempt(
            &reader,
            deadline,
            silent_deadline_for_attempt(deadline, attempts_used),
        );
        if attempt.finished && attempt.chunks > 0 {
            accepted = Some((attempt, pty, reader));
            break;
        }
        if flood_attempt_needs_retry(&attempt, attempts_used) {
            // Silent startup transient: record the terminal state, then drop
            // the dud (its `Drop` kills and reaps within a bound, so a
            // stuck or stopped holder of the slave cannot linger), pause so
            // a SIGHUP burst can clear (CTX-1255), and try once more with a
            // fresh device. `try_wait` here only marks the
            // dud reaped when it is already dead; a live dud stays unreaped
            // so `Drop` still kills it — no leak either way.
            diagnosis = format!(
                "attempt {attempts_used}: {} chunks={} total={} \
                 child pid={:?} fg={:?} exit={:?} pump_error={:?}",
                if attempt.finished {
                    "silent EOF"
                } else {
                    "stalled drain"
                },
                attempt.chunks,
                attempt.total,
                pty.pid(),
                pty.foreground_pgid(),
                pty.try_wait().ok().flatten(),
                reader.pump_error(),
            );
            drop(pty);
            std::thread::sleep(FLOOD_RETRY_PAUSE);
            continue;
        }
        // Mid-stream stall after output started, or a silent final attempt:
        // genuine failure, loud with the child state attached.
        panic!(
            "flood drain failed ({}) after {attempts_used} attempt(s): chunks={} total={} \
             child pid={:?} fg={:?} exit={:?} pump_error={:?}",
            if attempt.finished {
                "silent EOF on the final attempt"
            } else {
                "stalled drain"
            },
            attempt.chunks,
            attempt.total,
            pty.pid(),
            pty.foreground_pgid(),
            pty.try_wait().ok().flatten(),
            reader.pump_error(),
        );
    }
    let (attempt, mut pty, reader) = accepted.unwrap_or_else(|| {
        panic!("flood never produced output after bounded respawns: {diagnosis}");
    });
    let chunks = attempt.chunks;
    let max_chunk = attempt.max_chunk;
    assert!(chunks > 0, "flood should produce at least one chunk");
    assert!(
        max_chunk > 0 && max_chunk <= bitty_pty::READ_CHUNK_SIZE,
        "max chunk sanity"
    );
    // Verify the semantic bound documented in lib.rs: channel holds at most
    // CHANNEL_CAPACITY_CHUNKS chunks, so hard buffer bound is 128 KiB.
    assert_eq!(
        bitty_pty::MAX_BUFFERED_BYTES,
        bitty_pty::READ_CHUNK_SIZE * bitty_pty::CHANNEL_CAPACITY_CHUNKS
    );

    let status = pty.wait().expect("reap flood");
    // `seq` exits 0 after bounded output. We only assert the child was
    // reaped, not success, matching the previous posture.
    let _ = status.code();
    reader.join().expect("pump clean after flood");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
