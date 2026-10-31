//! Real report-helper path for the stall watchdog (#2497, #2560).
//!
//! The monitor never writes to its own stderr. It spawns the built `bitty`
//! binary in a private argv mode that atomically replaces the XDG state slot
//! and then writes the report to its inherited stderr; the monitor kills the
//! helper at the delivery deadline, which is what keeps a blocked stderr pipe
//! bounded without changing the application's file-descriptor flags.
//!
//! These tests drive that exact production entrypoint through
//! `CARGO_BIN_EXE_bitty`. The helper dispatches before config load and GUI
//! startup, so every case is headless-safe and display-free. State writes also
//! exercise the confined path and generation contract: a redirected or
//! symlinked slot, a missing generation, an empty report, and a stale
//! generation all fail closed without touching the previous evidence.

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
/// Evidence generation env variable (mirrors `ATTEMPT_ENV`).
const ATTEMPT_ENV: &str = "BITTY_WATCHDOG_ATTEMPT";
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

/// XDG-derived evidence slot under a test directory.
fn slot_path(xdg: &Path) -> PathBuf {
    xdg.join("bitty").join("stall-watchdog.log")
}

/// XDG-derived generation lock file.
fn lock_path(xdg: &Path) -> PathBuf {
    xdg.join("bitty").join(".stall-watchdog.log.lock")
}

/// One helper invocation's environment.
struct HelperInvocation<'a> {
    xdg: &'a Path,
    report: &'a [u8],
    attempt: Option<u64>,
    /// Explicit `BITTY_WATCHDOG_STATE_PATH` value (hostile redirect tests).
    state_override: Option<&'a Path>,
    /// Whether to pass the state path at all.
    request_state: bool,
}

impl HelperInvocation<'_> {
    fn new<'a>(xdg: &'a Path, report: &'a [u8]) -> HelperInvocation<'a> {
        HelperInvocation {
            xdg,
            report,
            attempt: Some(1),
            state_override: None,
            request_state: true,
        }
    }
}

/// Spawn the real helper, feed `report` on stdin, and wait with a hard bound.
///
/// The stdin write is best-effort: an over-cap helper exits early after
/// reading its bounded prefix, and the resulting EPIPE is expected.
fn run_helper(invocation: HelperInvocation<'_>) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .arg(STATE_WRITER_ARG)
        .env("XDG_STATE_HOME", invocation.xdg)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if invocation.request_state {
        let path = invocation
            .state_override
            .map_or_else(|| slot_path(invocation.xdg), Path::to_path_buf);
        command.env(STATE_WRITER_PATH_ENV, path);
    } else {
        command.env_remove(STATE_WRITER_PATH_ENV);
    }
    match invocation.attempt {
        Some(attempt) => {
            command.env(ATTEMPT_ENV, attempt.to_string());
        }
        None => {
            command.env_remove(ATTEMPT_ENV);
        }
    }
    let mut child = command.spawn().expect("spawn watchdog helper");
    let mut stdin = child.stdin.take().expect("helper stdin");
    let _ = stdin.write_all(invocation.report);
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

fn assert_stderr_carries(output: &Output, report: &[u8]) {
    let expected = format!("{}\n", String::from_utf8_lossy(report));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        expected,
        "the diagnostic of last resort must still carry the report"
    );
}

#[test]
fn helper_replaces_the_state_slot_then_writes_stderr() {
    let dir = temp_dir("state");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    // Seed a previous report: the helper must replace it, not append.
    std::fs::write(&path, "previous report\n").expect("seed slot");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n  thread-state snapshot\n");
    let output = run_helper(HelperInvocation::new(&dir, report.as_bytes()));
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
    assert_stderr_carries(&output, report.as_bytes());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_fails_closed_on_oversized_input_and_keeps_the_previous_slot() {
    let dir = temp_dir("oversized");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    std::fs::write(&path, "previous report\n").expect("seed slot");
    // The helper caps stdin at the report bound (16 KiB since bitty#2830);
    // 32 KiB is over-cap.
    let report = vec![b'x'; 32 * 1024];
    let output = run_helper(HelperInvocation::new(&dir, &report));
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
    // The derived state directory is a regular file, so creation fails; the
    // helper must report the failure through its exit status while the report
    // itself still reaches the diagnostic of last resort (stderr).
    std::fs::write(dir.join("bitty"), "not a directory").expect("blocker file");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let output = run_helper(HelperInvocation::new(&dir, report.as_bytes()));
    assert!(
        !output.status.success(),
        "a failed state write must exit non-zero"
    );
    assert_stderr_carries(&output, report.as_bytes());
    assert!(
        !slot_path(&dir).exists(),
        "no slot may be created through a blocked parent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_without_a_state_path_writes_only_stderr() {
    let dir = temp_dir("stderr-only");
    let report = b"bitty: main-loop stall recovered after 2000 ms";
    let mut invocation = HelperInvocation::new(&dir, report);
    invocation.request_state = false;
    invocation.attempt = None;
    let output = run_helper(invocation);
    assert!(
        output.status.success(),
        "stderr-only helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_stderr_carries(&output, report);
    assert!(!slot_path(&dir).exists(), "no slot may be created");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_rejects_an_empty_report_and_keeps_the_previous_slot() {
    let dir = temp_dir("empty");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    std::fs::write(&path, "previous report\n").expect("seed slot");
    let output = run_helper(HelperInvocation::new(&dir, b""));
    assert!(
        !output.status.success(),
        "an empty report must not be published"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("state slot"),
        "previous report\n",
        "an empty report must never replace evidence"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_requires_a_generation_for_state_writes() {
    let dir = temp_dir("no-attempt");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    std::fs::write(&path, "previous report\n").expect("seed slot");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let mut invocation = HelperInvocation::new(&dir, report.as_bytes());
    invocation.attempt = None;
    let output = run_helper(invocation);
    assert!(
        !output.status.success(),
        "a state write without a generation must fail closed"
    );
    assert_stderr_carries(&output, report.as_bytes());
    assert_eq!(
        std::fs::read_to_string(&path).expect("state slot"),
        "previous report\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_rejects_a_redirected_state_path() {
    let dir = temp_dir("redirect");
    let victim_dir = temp_dir("redirect-victim");
    let victim = victim_dir.join("stall-watchdog.log");
    std::fs::write(&victim, "untouched\n").expect("victim");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let mut invocation = HelperInvocation::new(&dir, report.as_bytes());
    invocation.state_override = Some(&victim);
    let output = run_helper(invocation);
    assert!(
        !output.status.success(),
        "a path outside the derived slot must be rejected"
    );
    assert_stderr_carries(&output, report.as_bytes());
    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim"),
        "untouched\n",
        "the redirected path must never be written"
    );
    assert!(!slot_path(&dir).exists(), "no fallback slot may be written");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&victim_dir);
}

#[cfg(unix)]
#[test]
fn helper_rejects_a_symlinked_state_slot() {
    let dir = temp_dir("symlink");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, "untouched\n").expect("victim");
    std::os::unix::fs::symlink(&victim, &path).expect("symlink slot");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let output = run_helper(HelperInvocation::new(&dir, report.as_bytes()));
    assert!(
        !output.status.success(),
        "a symlinked slot must fail closed"
    );
    assert_stderr_carries(&output, report.as_bytes());
    assert_eq!(
        std::fs::read_to_string(&victim).expect("victim"),
        "untouched\n",
        "no evidence may be written through the symlink"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn helper_rejects_a_stale_generation() {
    let dir = temp_dir("stale");
    let path = slot_path(&dir);
    std::fs::create_dir_all(path.parent().expect("slot parent")).expect("state dir");
    std::fs::write(&path, "newer evidence\n").expect("seed slot");
    // A newer attempt already holds generation 5; this helper carries 4.
    std::fs::write(lock_path(&dir), "5\n").expect("seed generation");
    let report = format!("{STALL_PREFIX}: no progress for 2000 ms\n");
    let mut invocation = HelperInvocation::new(&dir, report.as_bytes());
    invocation.attempt = Some(4);
    let output = run_helper(invocation);
    assert!(
        output.status.success(),
        "a stale attempt is discarded, not an error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("state slot"),
        "newer evidence\n",
        "a stale attempt must never replace newer evidence"
    );
    assert_stderr_carries(&output, report.as_bytes());
    let _ = std::fs::remove_dir_all(&dir);
}
