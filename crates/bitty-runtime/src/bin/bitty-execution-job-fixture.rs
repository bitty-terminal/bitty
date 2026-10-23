//! Hermetic child process for execution parity tests (bitty#2494).
//!
//! The component and the linked supervisor both spawn this binary, so parity
//! compares the two supervision mechanisms instead of two different children.
//! Modes are selected through `BITTY_EXECUTION_JOB_MODE` (explicit env is
//! part of what the tests exercise); no shell is involved and the program
//! never reads files, the network, or ambient environment.
//!
//! Test-only: the release bundles ship only the `bitty` binary.

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let mode = std::env::var("BITTY_EXECUTION_JOB_MODE").unwrap_or_default();
    match mode.as_str() {
        "stdout-stderr" => {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"out-line\n");
            let _ = out.flush();
            let mut err = std::io::stderr();
            let _ = err.write_all(b"err-line\n");
            let _ = err.flush();
            ExitCode::SUCCESS
        }
        "exit-7" => {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"before-exit\n");
            let _ = out.flush();
            ExitCode::from(7)
        }
        "sleep" => {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"started\n");
            let _ = out.flush();
            let millis = std::env::var("BITTY_EXECUTION_JOB_SLEEP_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30_000);
            std::thread::sleep(Duration::from_millis(millis));
            ExitCode::SUCCESS
        }
        "newest" => {
            // Distinguishable bytes: the retained newest-wins store keeps the
            // tail. 300 KiB of A followed by 20 KiB of B means a 256 KiB tail
            // is all B for either supervisor.
            let mut out = std::io::stdout();
            let _ = out.write_all(&vec![b'A'; 300_000]);
            let _ = out.write_all(&vec![b'B'; 20_000]);
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        "newline-tail" => {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"alpha\nbeta\n");
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("bitty-execution-job-fixture: unknown mode '{mode}'");
            ExitCode::from(2)
        }
    }
}
