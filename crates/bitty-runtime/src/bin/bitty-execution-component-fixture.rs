//! Hermetic execution-component fixture for the Core host client (bitty#2494).
//!
//! With no mode the fixture serves the real execution component protocol by
//! delegating to [`bitty_execution::component::serve_stdio`], so parity tests
//! drive the same server code the installable `bitty-execution` binary ships.
//!
//! `BITTY_EXECUTION_COMPONENT_FIXTURE_MODE` selects a fault-injection server
//! for host-client error paths:
//!
//! - `oversized-frame`: complete the handshake, then declare a frame larger
//!   than the 256 KiB wire cap.
//! - `malformed-frame`: complete the handshake, then send an unknown tag.
//! - `zero-frame`: complete the handshake, then send a zero-length frame.
//! - `truncated-frame`: complete the handshake, then truncate a payload.
//! - `crash-on-spawn`: complete the handshake, then exit as soon as a
//!   `Spawn` arrives.
//! - `silent-handshake`: never answer `Hello`.
//! - `v1-ack`: answer the v2 `Hello` with a v1 `HelloAck`.
//! - `job-error-on-spawn`: return a high-bit job-correlated error.
//! - `read-error`: complete one job, then return a request-correlated read
//!   error.
//! - `connection-error`: return a connection-level error after the handshake.
//!
//! Every mode is test-only: the release bundles ship only the `bitty` binary.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use bitty_execution::component::COMPONENT_VERSION;
use bitty_execution::component::protocol::{
    ErrorKind, ExitKind, FrameReader, MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, WireCancelEffect,
    WireDeadlineClock, WireKillScope, WireOomGap, WireOomVerdict, job_error_correlation,
    write_frame,
};

fn main() -> ExitCode {
    let mode = std::env::var("BITTY_EXECUTION_COMPONENT_FIXTURE_MODE")
        .ok()
        .or_else(fixture_mode_file)
        .unwrap_or_default();
    let code = match mode.as_str() {
        "" | "serve" => bitty_execution::component::serve_stdio(),
        "oversized-frame" => {
            let (_reader, mut output) = handshake();
            let declared = u32::try_from(MAX_FRAME_BYTES + 1).expect("fits u32");
            let _ = output.write_all(&declared.to_be_bytes());
            let _ = output.flush();
            wait_for_host_close();
            3
        }
        "malformed-frame" => {
            let (_reader, mut output) = handshake();
            // Length 1, unknown tag 0x7f. The host must reject the tag.
            let _ = output.write_all(&[0, 0, 0, 1, 0x7f]);
            let _ = output.flush();
            wait_for_host_close();
            3
        }
        "zero-frame" => {
            let (_reader, mut output) = handshake();
            let _ = output.write_all(&[0, 0, 0, 0]);
            let _ = output.flush();
            wait_for_host_close();
            3
        }
        "truncated-frame" => {
            let (_reader, mut output) = handshake();
            // Declared payload of ten bytes; only three follow. Exit so the
            // host observes a short payload as EOF, not a stalled read.
            let _ = output.write_all(&[0, 0, 0, 10, 0x21, 0, 0]);
            let _ = output.flush();
            3
        }
        "crash-on-spawn" => {
            let (mut reader, _output) = handshake();
            loop {
                match reader.read_message() {
                    Ok(Some(Message::Spawn { .. })) => return ExitCode::from(1),
                    Ok(Some(Message::Shutdown)) | Ok(None) => return ExitCode::SUCCESS,
                    Ok(Some(_)) => {}
                    Err(_) => return ExitCode::from(3),
                }
            }
        }
        "silent-handshake" => {
            std::thread::sleep(Duration::from_secs(30));
            3
        }
        "v1-ack" => {
            let (_reader, _output) = handshake_with_protocol(1);
            wait_for_host_close();
            0
        }
        "job-error-on-spawn" => job_error_on_spawn(),
        "read-error" => read_error(),
        "connection-error" => {
            let (_reader, mut output) = handshake_with_protocol(PROTOCOL_VERSION);
            write_frame(
                &mut output,
                &Message::Error {
                    request: 0,
                    kind: ErrorKind::ComponentLost,
                    message: "fixture connection lost".to_owned(),
                },
            )
            .ok();
            output.flush().ok();
            wait_for_host_close();
            0
        }
        _ => {
            eprintln!("bitty-execution-component-fixture: unknown mode '{mode}'");
            2
        }
    };
    ExitCode::from(code)
}

/// Reads an optional `fixture-mode` file from the component's working
/// directory (the version directory), so integration tests can select a fault
/// mode without widening the production environment allowlist.
fn fixture_mode_file() -> Option<String> {
    let text = std::fs::read_to_string("fixture-mode").ok()?;
    let mode = text.trim();
    (!mode.is_empty()).then(|| mode.to_owned())
}

/// Reads `Hello` and answers `HelloAck`; returns the reader for follow-ups.
fn handshake() -> (FrameReader<std::io::Stdin>, std::io::Stdout) {
    handshake_with_protocol(PROTOCOL_VERSION)
}

/// Reads `Hello` and answers with a selected protocol version.
fn handshake_with_protocol(protocol: u16) -> (FrameReader<std::io::Stdin>, std::io::Stdout) {
    let mut reader = FrameReader::new(std::io::stdin());
    let hello = reader.read_message();
    if !matches!(hello, Ok(Some(Message::Hello { .. }))) {
        std::process::exit(3);
    }
    let mut output = std::io::stdout();
    let ack = Message::HelloAck {
        protocol,
        component: "execution".to_owned(),
        version: COMPONENT_VERSION.to_owned(),
    };
    let _ = write_frame(&mut output, &ack);
    let _ = output.flush();
    (reader, output)
}

/// Sends a high-bit job-correlated error for the first submitted spawn.
fn job_error_on_spawn() -> u8 {
    let (mut reader, mut output) = handshake();
    loop {
        match reader.read_message() {
            Ok(Some(Message::Spawn { job, .. })) => {
                write_frame(
                    &mut output,
                    &Message::Error {
                        request: job_error_correlation(job),
                        kind: ErrorKind::InvalidRequest,
                        message: "fixture rejected job".to_owned(),
                    },
                )
                .ok();
                output.flush().ok();
            }
            Ok(Some(Message::Shutdown)) => {
                write_frame(&mut output, &Message::ShutdownAck).ok();
                output.flush().ok();
                return 0;
            }
            Ok(None) => return 0,
            Ok(Some(_)) => {}
            Err(_) => return 3,
        }
    }
}

/// Completes one synthetic job, then returns a low-bit read-request error.
fn read_error() -> u8 {
    let (mut reader, mut output) = handshake();
    loop {
        match reader.read_message() {
            Ok(Some(Message::Spawn { job, .. })) => {
                write_frame(&mut output, &Message::Spawned { job, generation: 1 }).ok();
                write_frame(&mut output, &Message::Started { job }).ok();
                write_frame(
                    &mut output,
                    &Message::Exit {
                        job,
                        outcome: ExitKind::Success,
                        detail: 0,
                        cancel: WireCancelEffect::None,
                        clock: WireDeadlineClock::None,
                        kill_scope: WireKillScope::OwnedTree,
                        oom: WireOomVerdict::NotOom,
                        oom_gap: WireOomGap::None,
                        stdout_total: 0,
                        stdout_evicted: 0,
                        stderr_total: 0,
                        stderr_evicted: 0,
                    },
                )
                .ok();
                output.flush().ok();
            }
            Ok(Some(Message::ReadOutput { request, .. })) => {
                write_frame(
                    &mut output,
                    &Message::Error {
                        request,
                        kind: ErrorKind::InvalidRequest,
                        message: "fixture rejected read".to_owned(),
                    },
                )
                .ok();
                output.flush().ok();
            }
            Ok(Some(Message::Shutdown)) => {
                write_frame(&mut output, &Message::ShutdownAck).ok();
                output.flush().ok();
                return 0;
            }
            Ok(Some(_)) => {}
            Ok(None) => return 0,
            Err(_) => return 3,
        }
    }
}

/// Keeps the fixture alive until the host closes stdin, so the host side of
/// the fault (not a fixture exit) is what the test observes.
fn wait_for_host_close() {
    let mut sink = [0u8; 256];
    let mut input = std::io::stdin();
    while matches!(input.read(&mut sink), Ok(count) if count > 0) {}
    // The host is expected to kill the process; do not linger forever if a
    // buggy path leaves the pipe open.
    std::thread::sleep(Duration::from_secs(2));
}
