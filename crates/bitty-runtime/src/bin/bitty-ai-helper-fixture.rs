//! Hermetic resident-helper test fixture (CTX-1253, bitty #2123, DIR-030).
//!
//! Speaks the accepted bitty-ipc length-prefixed framing on stdin/stdout
//! (4-byte big-endian length prefix, [`bitty_ipc::frame::MAX_FRAME_BYTES`]
//! payload bound) so the resident-singleton supervisor can be exercised
//! against a real coprocess without any AI build and without any network. It
//! is **not** a product binary: it is never installed or packaged.
//!
//! Behavior is selected by a `fixture-mode` file in the working directory
//! (the component version directory), because the supervisor clears the
//! environment and passes no arguments:
//!
//! - `echo` (default): answer each frame with `ai-fixture:` + the same bytes.
//! - `mute`: read frames and never answer (residency/liveness test).
//! - `crash`: exit with status 7 on the first frame (restart-bound test).
//! - `ignore-eof`: answer like `echo`, but keep running after stdin EOF until
//!   killed (shutdown grace-then-kill test).
//! - `bad-frame`: emit a frame header whose declared length exceeds the
//!   framing bound, then stay silent (framing-fault fails closed).

#![forbid(unsafe_code)]

use std::io::{Read, Write, stdin, stdout};
use std::process::ExitCode;
use std::time::Duration;

use bitty_ipc::frame::{Framer, MAX_FRAME_BYTES, encode_frame};

/// File in the working directory selecting the fixture behavior.
const MODE_FILE: &str = "fixture-mode";
/// Exit status of the `crash` mode.
const CRASH_EXIT: u8 = 7;
/// Exit status on a protocol or I/O failure.
const FAILURE_EXIT: u8 = 2;
/// Prefix `echo` prepends to every answered frame.
const ECHO_PREFIX: &[u8] = b"ai-fixture:";
/// Sleep slice while ignoring EOF (the supervisor kills the process).
const IGNORE_EOF_SLICE: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    let mode = std::fs::read_to_string(MODE_FILE)
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "echo".to_owned());

    if mode == "bad-frame" {
        // A declared length far past the bound: the supervisor's framer must
        // refuse it and fail closed rather than buffer unboundedly.
        let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap_or(u32::MAX);
        let mut out = stdout();
        if out.write_all(&declared.to_be_bytes()).is_err() || out.flush().is_err() {
            return ExitCode::from(FAILURE_EXIT);
        }
        return ExitCode::from(FAILURE_EXIT);
    }

    let mut input = stdin();
    let mut output = stdout();
    let mut framer = Framer::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => {
                if mode == "ignore-eof" {
                    // Keep the process alive past EOF so the supervisor must
                    // kill it after the shutdown grace.
                    loop {
                        std::thread::sleep(IGNORE_EOF_SLICE);
                    }
                }
                return ExitCode::SUCCESS;
            }
            Ok(n) => {
                let frames = match framer.push_bytes(&buf[..n]) {
                    Ok(frames) => frames,
                    Err(_) => return ExitCode::from(FAILURE_EXIT),
                };
                for frame in frames {
                    if mode == "crash" {
                        return ExitCode::from(CRASH_EXIT);
                    }
                    if mode == "mute" {
                        continue;
                    }
                    let mut reply = ECHO_PREFIX.to_vec();
                    reply.extend_from_slice(frame.payload());
                    let wire = match encode_frame(&reply) {
                        Ok(wire) => wire,
                        Err(_) => return ExitCode::from(FAILURE_EXIT),
                    };
                    if output.write_all(&wire).is_err() || output.flush().is_err() {
                        return ExitCode::from(FAILURE_EXIT);
                    }
                }
            }
        }
    }
}
