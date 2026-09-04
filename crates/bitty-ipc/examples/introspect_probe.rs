//! Tiny `BITTY_SOCKET` introspection probe (CTX-0159, Issue #258).
//!
//! CLI-first demonstration that grid/input/modifier/focus snapshots round-trip
//! over the served socket without screenshots. This is an example (not a
//! release binary): it is never installed, never spawned by `bitty-app`, and
//! performs only the four read-only `bitty.debug/*` introspection queries.
//!
//! Wire behavior mirrors the sibling `bitty-devtools` repository (read-only):
//! length-prefixed `u32` big-endian framing plus `version: "1.0"` JSON
//! envelopes, per `bitty-devtools/src/transport.ts` and
//! `bitty-devtools/src/protocol.ts`.
//!
//! Usage:
//!
//! ```sh
//! BITTY_SOCKET=/run/user/1000/bitty/default.sock \
//!   cargo run -p bitty-ipc --example introspect_probe
//! # or with an explicit path:
//! cargo run -p bitty-ipc --example introspect_probe -- /tmp/bitty.sock
//! ```
//!
//! Each query prints one pretty line to stdout; socket or protocol failures
//! exit non-zero with a single stderr line (fail-closed, no panic payload).

#![forbid(unsafe_code)]

use std::io::{Read, Write};

#[cfg(unix)]
fn main() {
    if let Err(reason) = run() {
        eprintln!("introspect_probe: {reason}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("introspect_probe: unix sockets required (unavailable on this platform)");
    std::process::exit(2);
}

/// Maximum response frame accepted (mirrors `bitty-ipc` 256 KiB).
#[cfg(unix)]
const MAX_RESPONSE: usize = 256 * 1024;

#[cfg(unix)]
fn run() -> Result<(), String> {
    use std::os::unix::net::UnixStream;

    let socket_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("BITTY_SOCKET").ok())
        .ok_or_else(|| "usage: introspect_probe [SOCKET_PATH] (or set BITTY_SOCKET)".to_string())?;
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|err| format!("connect {socket_path} failed: {err}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .map_err(|err| format!("timeout setup failed: {err}"))?;

    let queries: &[(&str, &str)] = &[
        (
            "ping",
            r#"{"id":1,"method":"bitty.debug/ping","version":"1.0"}"#,
        ),
        (
            "grid",
            r#"{"id":2,"method":"bitty.debug/getGridText","version":"1.0","params":{"rows":24,"cols":80}}"#,
        ),
        (
            "input",
            r#"{"id":3,"method":"bitty.debug/getInputRing","version":"1.0","params":{"limit":16}}"#,
        ),
        (
            "modifiers",
            r#"{"id":4,"method":"bitty.debug/getModifiers","version":"1.0"}"#,
        ),
        (
            "focus",
            r#"{"id":5,"method":"bitty.debug/getFocus","version":"1.0"}"#,
        ),
    ];
    for (name, payload) in queries {
        let wire = encode_frame(payload.as_bytes())?;
        stream
            .write_all(&wire)
            .map_err(|err| format!("write {name} failed: {err}"))?;
        stream
            .flush()
            .map_err(|err| format!("flush {name} failed: {err}"))?;
        let body = read_framed(&mut stream, name)?;
        println!("== {name} ==\n{body}");
    }
    Ok(())
}

#[cfg(unix)]
fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_RESPONSE {
        return Err(format!(
            "request {} exceeds limit {MAX_RESPONSE}",
            payload.len()
        ));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(unix)]
fn read_framed(stream: &mut std::os::unix::net::UnixStream, name: &str) -> Result<String, String> {
    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|err| format!("read {name} header failed: {err}"))?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_RESPONSE {
        return Err(format!("response {name} frame {len} exceeds limit"));
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .map_err(|err| format!("read {name} body failed: {err}"))?;
    String::from_utf8(body).map_err(|_| format!("response {name} is not utf-8"))
}
