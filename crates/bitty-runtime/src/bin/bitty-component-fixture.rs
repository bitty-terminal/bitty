//! Hermetic native-component test fixture (CTX-0906).
//!
//! Speaks wire protocol v1 on stdin/stdout like `bitty-net`, but performs no
//! network I/O: it answers every `HttpRequest` from local data so the broker
//! integration tests can spawn a real coprocess on every platform. Behavior
//! is selected by a `fixture-mode` file in the working directory (the
//! component version directory), because the broker clears the environment
//! and passes no arguments:
//!
//! - `echo` (default): `ResponseHead` 200 with `x-plugin-id`, `x-grant`,
//!   `x-env`, and `x-cwd-mode` headers, then the URL as a one-chunk body.
//! - `hold`: never answers requests (in-flight and idle tests).
//! - `crash`: exits with status 3 on the first request.
//! - `ignore-eof`: answers like `echo`, but keeps running after stdin EOF
//!   until killed (grace-then-kill test).
//! - `bad-ack`: answers `Hello` with an unsupported protocol version.
//! - `mute`: never answers `Hello` (handshake timeout test).
//! - `flood`: answers with a body larger than the request budget.
//!
//! Not a product binary: it is never installed or packaged.

#![forbid(unsafe_code)]

use std::io::{BufReader, BufWriter, Write, stdin, stdout};
use std::process::ExitCode;
use std::time::Duration;

use bitty_network_wire::{FrameReader, Message, PROTOCOL_VERSION, write_frame};

/// File in the working directory selecting the fixture behavior.
const MODE_FILE: &str = "fixture-mode";
/// Exit status of the `crash` mode.
const CRASH_EXIT: u8 = 3;
/// Exit status on a protocol or I/O failure.
const FAILURE_EXIT: u8 = 2;
/// Sleep slice while ignoring EOF (the broker kills the process).
const IGNORE_EOF_SLICE: Duration = Duration::from_millis(50);
/// Version announced in `HelloAck`.
const FIXTURE_VERSION: &str = "0.0.1";

fn main() -> ExitCode {
    let mode = std::fs::read_to_string(MODE_FILE)
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "echo".to_owned());
    let mut reader = FrameReader::new(BufReader::new(stdin()));
    let mut out = BufWriter::new(stdout());
    loop {
        let message = match reader.read_message() {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(_) => return ExitCode::from(FAILURE_EXIT),
        };
        let replies = match message {
            Message::Hello { .. } if mode == "mute" => Vec::new(),
            Message::Hello { component, .. } => {
                let protocol = if mode == "bad-ack" {
                    PROTOCOL_VERSION + 1
                } else {
                    PROTOCOL_VERSION
                };
                vec![Message::HelloAck {
                    protocol,
                    component,
                    version: FIXTURE_VERSION.to_owned(),
                }]
            }
            Message::HttpRequest {
                id,
                plugin_id,
                grant,
                url,
                max_body_bytes,
                ..
            } => match mode.as_str() {
                "hold" => Vec::new(),
                "crash" => return ExitCode::from(CRASH_EXIT),
                "flood" => {
                    let size = usize::try_from(max_body_bytes)
                        .unwrap_or(usize::MAX)
                        .saturating_add(1);
                    vec![
                        Message::ResponseHead {
                            id,
                            status: 200,
                            headers: Vec::new(),
                        },
                        Message::ResponseBody {
                            id,
                            data: vec![b'x'; size.min(bitty_network_wire::MAX_BODY_CHUNK_BYTES)],
                            last: true,
                        },
                    ]
                }
                _ => {
                    let grant_text = grant
                        .hosts
                        .iter()
                        .map(|host| {
                            let ports: Vec<String> =
                                host.ports.iter().map(u16::to_string).collect();
                            format!("{}:{}", host.host, ports.join("+"))
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let mut env_names: Vec<String> = std::env::vars_os()
                        .map(|(name, _)| name.to_string_lossy().into_owned())
                        .collect();
                    env_names.sort();
                    let cwd_mode = if std::path::Path::new(MODE_FILE).exists() {
                        "version-dir"
                    } else {
                        "unknown"
                    };
                    vec![
                        Message::ResponseHead {
                            id,
                            status: 200,
                            headers: vec![
                                ("x-plugin-id".to_owned(), plugin_id),
                                ("x-grant".to_owned(), grant_text),
                                ("x-env".to_owned(), env_names.join(",")),
                                ("x-cwd-mode".to_owned(), cwd_mode.to_owned()),
                            ],
                        },
                        Message::ResponseBody {
                            id,
                            data: url.into_bytes(),
                            last: true,
                        },
                    ]
                }
            },
            Message::Shutdown => break,
            // RequestBody / Cancel need no answer in the fixture.
            _ => Vec::new(),
        };
        for reply in &replies {
            if write_frame(&mut out, reply).is_err() {
                return ExitCode::from(FAILURE_EXIT);
            }
        }
        if out.flush().is_err() {
            return ExitCode::from(FAILURE_EXIT);
        }
    }
    if mode == "ignore-eof" {
        loop {
            std::thread::sleep(IGNORE_EOF_SLICE);
        }
    }
    ExitCode::SUCCESS
}
