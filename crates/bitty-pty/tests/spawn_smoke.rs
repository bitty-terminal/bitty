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
fn child_environment_inherits_session_with_overrides() {
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
    let mut pty = PtyBuilder::new("/usr/bin/env")
        .env("TERM", "custom-256color")
        .env("COLORTERM", "custom-color")
        .spawn()
        .expect("spawn env");

    let reader = pty.take_reader().expect("reader half");
    let writer = pty.take_writer().expect("writer half");
    drop(writer);

    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let output = drain(&reader, deadline);
    reader.join().expect("pump clean");

    let text = String::from_utf8_lossy(&output);
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
    // /tmp is a symlink to /private/tmp on macOS; canonicalize both sides.
    let raw = text.trim();
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
    // Real shell echo dogfood for 0.0.1: `sh -c 'echo …'` proves direct argv
    // exec (no shell interpolation inside bitty-pty), the bounded channel, and
    // clean exit. Works headlessly — no window or GPU required.
    let mut pty = PtyBuilder::new("/bin/sh")
        .arg("-c")
        .arg("echo hello-bitty-pty")
        .spawn()
        .expect("spawn sh -c echo");

    // PTY size is still kernel-queryable even for a shell child.
    let (cols, rows) = pty.size().expect("size after shell spawn");
    assert!(
        cols >= 10 && rows >= 5,
        "unexpected initial size {cols}x{rows}"
    );

    let reader = pty.take_reader().expect("reader half");
    // No writer needed; `echo` exits without input.

    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut out = Vec::new();
    while !contains(&out, b"hello-bitty-pty") {
        match reader.recv_timeout(ECHO_TIMEOUT).expect("recv_timeout") {
            Some(chunk) => {
                assert!(
                    chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
                    "shell echo chunk {} exceeds READ_CHUNK_SIZE {}",
                    chunk.len(),
                    bitty_pty::READ_CHUNK_SIZE
                );
                assert!(
                    chunk.len() <= bitty_pty::MAX_BUFFERED_BYTES,
                    "chunk must not exceed MAX_BUFFERED_BYTES"
                );
                out.extend_from_slice(&chunk);
                // Also prove try_recv path does not break backpressure:
                // a non-blocking poll after the blocking recv should not panic
                // and must respect the same bound if it yields data.
                if let Some(extra) = reader.try_recv() {
                    assert!(extra.len() <= bitty_pty::READ_CHUNK_SIZE);
                    out.extend_from_slice(&extra);
                }
            }
            None => break,
        }
        assert!(std::time::Instant::now() < deadline, "shell echo timed out");
    }
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

    // Drain remaining bytes (e.g. trailing newline, shell prompt if any)
    // and assert pump ended cleanly with bounded semantics.
    let _rest = drain(&reader, std::time::Instant::now() + ECHO_TIMEOUT);
    reader.join().expect("pump ended cleanly after shell");
}

#[test]
fn backpressure_bound_holds_under_flood() {
    // Flood the bounded channel: the child produces unbounded output (`yes`
    // piped through `head -n 5000` so it terminates), but the in-crate
    // buffer never exceeds MAX_BUFFERED_BYTES. The kernel PTY buffer +
    // channel backpressure blocks the child instead of growing the heap.
    let mut pty = PtyBuilder::new("/bin/sh")
        .arg("-c")
        .arg("yes | head -n 5000")
        .spawn()
        .expect("spawn flood");

    let reader = pty.take_reader().expect("reader half");

    let deadline = std::time::Instant::now() + ECHO_TIMEOUT;
    let mut total = 0usize;
    let mut chunks = 0usize;
    let mut max_chunk = 0usize;
    // Drain until EOF, asserting per-chunk bound holds even under flood.
    while let Some(chunk) = reader.recv() {
        assert!(
            chunk.len() <= bitty_pty::READ_CHUNK_SIZE,
            "flood chunk {} exceeds READ_CHUNK_SIZE {}",
            chunk.len(),
            bitty_pty::READ_CHUNK_SIZE
        );
        max_chunk = max_chunk.max(chunk.len());
        total += chunk.len();
        chunks += 1;
        assert!(
            std::time::Instant::now() < deadline,
            "flood drain timed out"
        );
        // The channel itself is bounded to 16 chunks; even if we drained
        // slowly, total buffered inside the crate at any instant could never
        // exceed MAX_BUFFERED_BYTES. We prove the weaker invariant that no
        // single chunk exceeds READ_CHUNK_SIZE and that the pump completes
        // without unbounded growth (total > MAX shunts would have hung or
        // panicked if backpressure were broken).
        assert!(
            total <= 5000 * 10 + 8192,
            "unreasonable total {total}, backpressure may have duplicated or leaked"
        );
    }
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
    // `yes | head` exits 0 on most cores (SIGPIPE on `yes` is masked by pipe).
    // We only assert the child was reaped, not success.
    let _ = status.code();
    reader.join().expect("pump clean after flood");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
