//! Hermetic storage-component fixture for the Core host client (bitty#2495).
//!
//! With no mode the fixture serves the real protocol-v1 component server from
//! the pinned `bitty-storage` crate, so parity tests drive the same server the
//! installable `bitty-storage` binary ships. A `fixture-mode` file next to the
//! executable selects a fault-injection server (the host clears the child
//! environment, so the mode cannot travel through env):
//!
//! - `silent-handshake`: read `Hello` and never answer.
//! - `wrong-protocol` / `wrong-component` / `wrong-version`: answer `Hello`
//!   with one incorrect `HelloAck` field.
//! - `malformed-frame`: correct handshake, then an unknown tag.
//! - `oversized-frame`: correct handshake, then a declared frame over the cap.
//! - `truncated-frame`: correct handshake, then a short payload and EOF.
//! - `crash-after-ack`: correct handshake, then exit.
//! - `stderr-noise`: correct handshake, write more than the host ring, then
//!   serve only `Shutdown`.
//! - `empty-page-loop`: answer every snapshot read with an empty non-final
//!   page so the host page bound must fail closed.
//! - `error-stale`: answer `CommitBatch` with `StaleRevision`.
//! - `error-uncertain`: answer `CommitBatch` with `Uncertain`.
//! - `unknown-error`: answer the first request with an unknown error code.
//!
//! Every mode is test-only: release bundles ship only the `bitty` binary.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use bitty_storage_component::component::COMPONENT_VERSION;
use bitty_storage_component::component::protocol::{
    ErrorKind, FrameReader, MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, write_frame,
};
use bitty_storage_component::component::server::serve_stdio;
use bitty_storage_component::component::store::StorageRoot;

const EXIT_REFUSED: u8 = 2;
const EXIT_PROTOCOL: u8 = 3;

/// Runs the selected fixture mode and returns the process exit code.
pub fn run(root: PathBuf) -> u8 {
    let mode = fixture_mode();
    match mode.as_str() {
        "" | "serve" => serve(root),
        "silent-handshake" => silent_handshake(),
        "wrong-protocol" => wrong_ack(2, "storage", COMPONENT_VERSION),
        "wrong-component" => wrong_ack(PROTOCOL_VERSION, "other", COMPONENT_VERSION),
        "wrong-version" => wrong_ack(PROTOCOL_VERSION, "storage", "9.9.9"),
        "malformed-frame" => after_handshake_linger(|out| {
            out.write_all(&[0, 0, 0, 1, 0x7f])?;
            out.flush()
        }),
        "oversized-frame" => after_handshake_linger(|out| {
            let declared = u32::try_from(MAX_FRAME_BYTES + 1).expect("fits u32");
            out.write_all(&declared.to_be_bytes())?;
            out.flush()
        }),
        "truncated-frame" => after_handshake(|out| {
            out.write_all(&[0, 0, 0, 10, 0x21, 0, 0])?;
            out.flush()
        }),
        "crash-after-ack" => after_handshake(|_| Ok(())),
        "unsolicited-frame" => after_handshake(|out| {
            for request in 1..=4 {
                frame(
                    out,
                    &Message::NamespaceOpened {
                        request,
                        namespace: "unexpected".to_owned(),
                        revision: 0,
                        rows: 0,
                        bytes: 0,
                    },
                )?;
            }
            out.flush()?;
            // Keep the process alive so the host observes the unsolicited
            // frames instead of a write failure from an already-exited child.
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }),
        "stderr-noise" => stderr_noise(),
        "empty-page-loop" => custom_loop("empty-page-loop"),
        "error-stale" => custom_loop("error-stale"),
        "error-uncertain" => custom_loop("error-uncertain"),
        "unknown-error" => custom_loop("unknown-error"),
        other => {
            eprintln!("bitty-storage-component-fixture: unknown mode '{other}'");
            EXIT_REFUSED
        }
    }
}

fn fixture_mode() -> String {
    let Ok(executable) = std::env::current_exe() else {
        return String::new();
    };
    let Some(parent) = executable.parent() else {
        return String::new();
    };
    std::fs::read_to_string(parent.join("fixture-mode"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

/// Parses the host's fixed `--root PATH` argument.
pub fn root_argument() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    let flag = args.next()?;
    if flag == "--root" {
        return args.next().map(PathBuf::from);
    }
    let flag = flag.to_string_lossy();
    flag.strip_prefix("--root=")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn serve(root: PathBuf) -> u8 {
    match StorageRoot::new(root) {
        Ok(root) => serve_stdio(root),
        Err(_) => EXIT_PROTOCOL,
    }
}

fn read_hello<R: Read>(reader: &mut FrameReader<R>) -> bool {
    matches!(reader.read_message(), Ok(Some(Message::Hello { .. })))
}

fn acknowledge<W: Write>(out: &mut W, protocol: u16, component: &str, version: &str) -> bool {
    write_frame(
        out,
        &Message::HelloAck {
            protocol,
            component: component.to_owned(),
            version: version.to_owned(),
        },
    )
    .is_ok()
        && out.flush().is_ok()
}

fn silent_handshake() -> u8 {
    let mut reader = FrameReader::new(io::stdin());
    if !read_hello(&mut reader) {
        return EXIT_PROTOCOL;
    }
    let mut sink = Vec::new();
    let _ = io::stdin().read_to_end(&mut sink);
    0
}

fn wrong_ack(protocol: u16, component: &str, version: &str) -> u8 {
    let mut reader = FrameReader::new(io::stdin());
    if !read_hello(&mut reader) {
        return EXIT_PROTOCOL;
    }
    let mut out = io::stdout();
    if !acknowledge(&mut out, protocol, component, version) {
        return EXIT_PROTOCOL;
    }
    EXIT_PROTOCOL
}

fn after_handshake(write: impl FnOnce(&mut io::Stdout) -> io::Result<()>) -> u8 {
    after_handshake_mode(write, false)
}

/// Writes a malformed/oversized frame and stays alive so the host reads it
/// instead of racing the child exit into a write failure.
fn after_handshake_linger(write: impl FnOnce(&mut io::Stdout) -> io::Result<()>) -> u8 {
    after_handshake_mode(write, true)
}

fn after_handshake_mode(write: impl FnOnce(&mut io::Stdout) -> io::Result<()>, linger: bool) -> u8 {
    let mut reader = FrameReader::new(io::stdin());
    if !read_hello(&mut reader) {
        return EXIT_PROTOCOL;
    }
    let mut out = io::stdout();
    if !acknowledge(&mut out, PROTOCOL_VERSION, "storage", COMPONENT_VERSION) {
        return EXIT_PROTOCOL;
    }
    if write(&mut out).is_err() {
        return EXIT_PROTOCOL;
    }
    if linger {
        std::thread::sleep(Duration::from_secs(5));
    }
    EXIT_PROTOCOL
}

fn stderr_noise() -> u8 {
    let mut reader = FrameReader::new(io::stdin());
    if !read_hello(&mut reader) {
        return EXIT_PROTOCOL;
    }
    let mut out = io::stdout();
    if !acknowledge(&mut out, PROTOCOL_VERSION, "storage", COMPONENT_VERSION) {
        return EXIT_PROTOCOL;
    }
    let noise = vec![b'A'; 96 * 1024];
    let mut stderr = io::stderr();
    if stderr.write_all(&noise).is_err() || stderr.write_all(b"TAIL").is_err() {
        return EXIT_PROTOCOL;
    }
    let _ = stderr.flush();
    serve_until_shutdown(&mut reader, &mut out)
}

fn serve_until_shutdown<R: Read, W: Write>(reader: &mut FrameReader<R>, out: &mut W) -> u8 {
    match reader.read_message() {
        Ok(Some(Message::Shutdown)) => {
            let _ = write_frame(out, &Message::ShutdownAck);
            let _ = out.flush();
            0
        }
        Ok(Some(_)) | Err(_) => EXIT_PROTOCOL,
        Ok(None) => 0,
    }
}

fn custom_loop(mode: &str) -> u8 {
    let mut reader = FrameReader::new(io::stdin());
    if !read_hello(&mut reader) {
        return EXIT_PROTOCOL;
    }
    let mut out = io::stdout();
    if !acknowledge(&mut out, PROTOCOL_VERSION, "storage", COMPONENT_VERSION) {
        return EXIT_PROTOCOL;
    }
    loop {
        let message = match reader.read_message() {
            Ok(Some(message)) => message,
            Ok(None) => return 0,
            Err(_) => return EXIT_PROTOCOL,
        };
        let result = match (mode, message) {
            ("unknown-error", Message::OpenNamespace { .. })
            | ("unknown-error", Message::ReadSnapshot { .. })
            | ("unknown-error", Message::CommitBatch { .. })
            | ("unknown-error", Message::Rollback { .. }) => write_unknown_error(&mut out),
            (_, Message::OpenNamespace { request, namespace }) => frame(
                &mut out,
                &Message::NamespaceOpened {
                    request,
                    namespace,
                    revision: 0,
                    rows: 0,
                    bytes: 0,
                },
            ),
            (
                "empty-page-loop",
                Message::ReadSnapshot {
                    request, namespace, ..
                },
            ) => frame(
                &mut out,
                &Message::SnapshotChunk {
                    request,
                    namespace,
                    revision: 0,
                    start_row: 0,
                    rows: Vec::new(),
                    total_rows: 1,
                    total_bytes: 0,
                    done: false,
                },
            ),
            ("error-stale", Message::CommitBatch { request, .. }) => frame(
                &mut out,
                &Message::Error {
                    request,
                    kind: ErrorKind::StaleRevision,
                    message: "revision conflict".to_owned(),
                },
            ),
            ("error-uncertain", Message::CommitBatch { request, .. }) => frame(
                &mut out,
                &Message::Error {
                    request,
                    kind: ErrorKind::Uncertain,
                    message: "storage outcome uncertain; reopen namespace".to_owned(),
                },
            ),
            (_, Message::Shutdown) => {
                let _ = frame(&mut out, &Message::ShutdownAck);
                let _ = out.flush();
                return 0;
            }
            _ => return EXIT_PROTOCOL,
        };
        if result.is_err() || out.flush().is_err() {
            return EXIT_PROTOCOL;
        }
    }
}

/// Writes one protocol frame, flattening the codec error for fixture control
/// flow (the real server keeps the typed diagnostic).
fn frame(out: &mut impl Write, message: &Message) -> io::Result<()> {
    write_frame(out, message).map_err(|_| io::Error::other("frame write failed"))
}

/// Writes a syntactically valid `Error` frame with an unknown kind code.
fn write_unknown_error(out: &mut impl Write) -> io::Result<()> {
    let mut payload = Vec::with_capacity(14);
    payload.push(0x2f);
    payload.extend_from_slice(&0u64.to_be_bytes());
    payload.push(255);
    payload.extend_from_slice(&0u32.to_be_bytes());
    out.write_all(
        &u32::try_from(payload.len())
            .expect("fits u32")
            .to_be_bytes(),
    )?;
    out.write_all(&payload)
}
