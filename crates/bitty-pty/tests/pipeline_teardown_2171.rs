//! Regression test for issue #2171: the shell pipeline `yes X | head -n 5`
//! typed into a bitty terminal panel must tear itself down.
//!
//! Reported symptom: `head` exits after five lines, but `yes` survives as an
//! `S+` process and the shell never returns a prompt (the pipeline wedges),
//! while the same pipeline in a direct guest SSH shell completes in ~2 ms.
//! The suspected defect is pty/process-group SIGPIPE delivery or child
//! reaping in the pty layer.
//!
//! This drives the exact pipeline through the owned PTY API on `/bin/sh`
//! (the same spawn path bitty uses) and asserts two properties the reported
//! symptom violates:
//!
//!   1. The direct child (`sh -c 'yes X | head -n 5'`, which waits for every
//!      pipeline member) is reaped within the bound — a surviving `yes` keeps
//!      it alive.
//!   2. No process carrying this run's unique marker survives the reap.
//!
//! Unix-only: the Windows ConPTY backend is a separate slice.
#![cfg(unix)]

use std::time::Duration;
use std::time::Instant;

use bitty_pty::PtyBuilder;

/// Bound for every wait: a wedged pipeline must fail loudly here instead of
/// hanging the suite (the crate's fail-closed test posture).
const BOUND: Duration = Duration::from_secs(10);

/// Returns the live processes whose command line carries `marker`, excluding
/// this test binary itself. Reads `/proc` directly — no new dependency, and
/// it sees a straggler the instant it exists rather than waiting for `ps`.
fn survivors(marker: &str) -> Vec<String> {
    let me = std::process::id();
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if !cmdline.contains(marker) {
            continue;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Field 3 of /proc/<pid>/stat is the state character (`S`, `R`, …).
        let state = stat.split_whitespace().nth(2).unwrap_or("?");
        found.push(format!("pid={pid} state={state} cmdline={cmdline:?}"));
    }
    found
}

#[test]
fn yes_head_pipeline_tears_down_without_stragglers() {
    let marker = format!("ACCT2_2171_{}", std::process::id());
    let script = format!("yes {marker} | head -n 5");
    let mut pty = PtyBuilder::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .spawn()
        .expect("spawn sh pipeline");
    let reader = pty.take_reader().expect("reader half");

    let start = Instant::now();
    let reaped = pty.wait_timeout(BOUND).expect("wait_timeout");
    let elapsed = start.elapsed();
    let status = reaped.unwrap_or_else(|| {
        panic!(
            "WEDGE: `{script}` not reaped within {BOUND:?} ({elapsed:?}); \
             stragglers: {:?}",
            survivors(&marker),
        )
    });

    // Let an orphaned member be reparented to init before scanning, so the
    // straggler check is not a race against the reap itself.
    std::thread::sleep(Duration::from_millis(250));
    let stragglers = survivors(&marker);
    assert!(
        stragglers.is_empty(),
        "`{script}` left survivors after the child ({:?}) was reaped in \
         {elapsed:?}: {stragglers:?}",
        status.code(),
    );

    // The master read must reach EOF once every slave fd holder is gone.
    let deadline = Instant::now() + BOUND;
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "drain stalled: {} bytes, pump_error={:?}, stragglers={:?}",
                out.len(),
                reader.pump_error(),
                survivors(&marker),
            );
        }
        match reader.recv_timeout(remaining) {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(err) => panic!("drain failed ({err:?}): {} bytes", out.len()),
        }
    }
    let marker_lines = String::from_utf8_lossy(&out)
        .lines()
        .filter(|line| line.contains(&marker))
        .count();
    assert_eq!(
        marker_lines, 5,
        "head -n 5 must pass exactly five marker lines, got {marker_lines}"
    );
    reader.join().expect("pump clean after pipeline");
}

/// The product scenario: the pipeline is typed into an *interactive* shell
/// (job control enabled), not run via `sh -c`. A surviving `yes` keeps the
/// shell from returning to its prompt; a follow-up `echo` only runs once the
/// pipeline has fully torn down. Fails if the prompt never returns.
#[test]
fn interactive_shell_returns_to_prompt_after_yes_head_pipeline() {
    use std::io::Write as _;

    let marker = format!("AFTER_2171_{}", std::process::id());
    let yes_word = format!("YESWORD_2171_{}", std::process::id());
    let mut pty = PtyBuilder::new("/bin/sh")
        .arg("-i")
        .env("PS1", "READY2171> ")
        .env("PS2", "> ")
        .spawn()
        .expect("spawn interactive sh");
    let mut writer = pty.take_writer().expect("writer half");
    let reader = pty.take_reader().expect("reader half");

    // Type the reported pipeline, then a follow-up command whose output only
    // appears once the shell is responsive again.
    writer
        .write_all(format!("yes {yes_word} | head -n 5\n").as_bytes())
        .expect("write pipeline");
    writer
        .write_all(format!("echo {marker}\n").as_bytes())
        .expect("write follow-up");
    writer.flush().expect("flush");

    // Drain until the follow-up marker is seen, or the bound elapses.
    let deadline = Instant::now() + BOUND;
    let mut out = Vec::new();
    let mut seen = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match reader.recv_timeout(remaining) {
            Ok(Some(chunk)) => {
                out.extend_from_slice(&chunk);
                if String::from_utf8_lossy(&out).contains(&marker) {
                    seen = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let stragglers = survivors(&yes_word);
    assert!(
        seen,
        "WEDGE: interactive shell did not return to a prompt after \
         `yes {yes_word} | head -n 5` within {BOUND:?}; \
         yes stragglers: {stragglers:?}; output tail: {:?}",
        String::from_utf8_lossy(&out[out.len().saturating_sub(400)..]),
    );
    assert!(
        stragglers.is_empty(),
        "`yes {yes_word} | head -n 5` left survivors in the interactive shell: {stragglers:?}",
    );

    pty.kill().ok();
    let _ = pty.wait_timeout(BOUND);
    let _ = reader.join();
}
