//! Real report-helper path for the stall watchdog (#2497).
//!
//! The monitor never writes to its own stderr. It spawns the built `bitty`
//! binary in a private argv mode that atomically replaces the XDG state slot
//! and then writes the report to its inherited stderr; the monitor kills the
//! helper at the delivery deadline, which is what keeps a blocked stderr pipe
//! bounded without changing the application's file-descriptor flags.
//!
//! These tests drive that exact production entrypoint through
//! `CARGO_BIN_EXE_bitty`. The helper dispatches before config load and GUI
//! startup, so every case is headless-safe and display-free.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");
/// Hard timeout per helper invocation.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
/// Private helper argv (mirrors `STATE_WRITER_ARG`).
const STATE_WRITER_ARG: &str = "--internal-stall-watchdog-state";
/// State-slot env variable (mirrors `STATE_WRITER_PATH_ENV`).
const STATE_WRITER_PATH_ENV: &str = "BITTY_WATCHDOG_STATE_PATH";
/// A stall report prefix the helper recognizes (mirrors `STALL_REPORT_PREFIX`).
const STALL_PREFIX: &str = "bitty: main-loop stall detected";

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bitty-watchdog-helper-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Spawn the real helper, feed `report` on stdin, and wait with a hard bound.
///
/// The stdin write is best-effort: an over-cap helper exits early after
/// reading its bounded prefix, and the resulting EPIPE is expected.
fn run_helper(report: &[u8], state: Option<&Path>) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .arg(STATE_WRITER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match state {
        Some(path) => {
            command.env(STATE_WRITER_PATH_ENV, path);
        }
        None => {
            command.env_remove(STATE_WRITER_PATH_ENV);
        }
    }
    let mut child = command.spawn().expect("spawn watchdog helper");
    let mut stdin = child.stdin.take().expect("helper stdin");
    let _ = stdin.write_all(report);
    drop(stdin);
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        if child.try_wait().expect("poll helper").is_some() {
            return child.wait_with_output().expect("collect helper output");
        }
        assert!(
            Instant::now() < deadline,
            "watchdog helper exceeded {HELPER_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn helper_replaces_the_state_slot_then_writes_stderr() {
    let dir = temp_dir("state");
    let path = dir.join("stall-watchdog.log");
    // Seed a previous report: the helper must replace it, not append.
    std::fs::write(&path, "previous report\n").expect("seed slot");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n  thread-state snapshot\n");
    let output = run_helper(report.as_bytes(), Some(&path));
    assert!(
        output.status.success(),
        "helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("state slot"),
        report,
        "the helper must replace the single-slot state file with the report"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{report}\n"),
        "the helper writes the report to its inherited stderr after the state slot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_fails_closed_on_oversized_input_and_keeps_the_previous_slot() {
    let dir = temp_dir("oversized");
    let path = dir.join("stall-watchdog.log");
    std::fs::write(&path, "previous report\n").expect("seed slot");
    // The helper caps stdin at the 8 KiB report bound; 16 KiB is over-cap.
    let report = vec![b'x'; 16 * 1024];
    let output = run_helper(&report, Some(&path));
    assert!(
        !output.status.success(),
        "an over-cap helper input must fail closed"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("state slot"),
        "previous report\n",
        "an over-cap report must never touch the previous evidence"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_state_failure_is_nonzero_and_still_reaches_stderr() {
    let dir = temp_dir("state-failure");
    // The state parent is a regular file, so the atomic write fails; the
    // helper must report the failure through its exit status while the report
    // itself still reaches the diagnostic of last resort (stderr).
    let blocker = dir.join("blocked");
    std::fs::write(&blocker, "not a directory").expect("blocker file");
    let path = blocker.join("stall-watchdog.log");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let output = run_helper(report.as_bytes(), Some(&path));
    assert!(
        !output.status.success(),
        "a failed state write must exit non-zero"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{report}\n"),
        "stderr must still carry the report after a state-write failure"
    );
    assert!(
        !path.exists(),
        "no slot may be created through a blocked parent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_without_a_state_path_writes_only_stderr() {
    let output = run_helper(b"bitty: main-loop stall recovered after 2000 ms", None);
    assert!(
        output.status.success(),
        "stderr-only helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "bitty: main-loop stall recovered after 2000 ms\n"
    );
}
