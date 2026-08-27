//! `bitty-ipc`: draft IPC and MCP boundary for Bitty.
//!
//! # Draft status — not normative
//!
//! This crate implements the **proposed** spine phase "IPC -> Agent" from
//! `bitty-docs/docs/product/proposed-delivery-sequence.md` (candidate
//! build-order `PTY -> VT -> Grid -> Font -> GPU -> Correct Terminal ->
//! Config -> Command/Event -> Plugin Runtime -> Plugin Manager -> DevTools ->
//! Rich Presentation -> **IPC** -> Agent`). That sequence, the CLI/MCP detail
//! in `bitty-docs/docs/interfaces/cli.md`, and the resource ceilings in
//! `bitty-docs/docs/specifications/isolation-resource-rfc.md` (`RC-9` /
//! `RC-10`, `IR-D3`) are all **`draft` / `proposed`** research records,
//! closing `OQ-018` ("How are instances selected, authenticated, and
//! authorized for IPC/MCP clients?") only if a dedicated IPC/MCP protocol RFC
//! is adopted after independent review by the category owner, a docs curator,
//! and a security reviewer.
//!
//! Nothing here claims normative wire format, stable IPC identifiers, frozen
//! capability scopes, or a settled authentication/peer-credential mechanism.
//! The crate is intentionally `draft` / `proposed` and its contract **may
//! change** without a semver major bump until the RFC is accepted. Do not
//! describe its behavior as shipped until an ADR records acceptance and a
//! release ships it.
//!
//! The crate is **pure data + bounded queues** on the host side: it owns
//! bounded IPC request/response channels, length-prefixed message framing
//! bounded at 256 KiB, stdio transport stubs, and MCP client primitives
//! (request correlation plus deterministic timeouts). There is no process
//! spawn, no real `Unix socket`/`named pipe` endpoint, no peer-credential
//! check, and no `unsafe` — the crate is headlessly testable on both Linux
//! CI and the `windows-latest` job. A real transport with `XDG_RUNTIME_DIR`
//! socket modes and Windows ACLs belongs to a follow-up slice and must pass
//! a focused security review before any claim of peer authentication.
//!
//! # Trust boundary
//!
//! Every byte that crosses the IPC/MCP boundary is treated as originating
//! from an **untrusted client** per ADR-0003 and the normative security
//! corpus:
//!
//! - `bitty-docs/docs/security/overview.md` invariant 5: "IPC is local-
//!   user-only by default and every operation has an explicit scope."
//! - `bitty-docs/docs/security/threat-model.md` boundary map
//!   `PTY bytes | Lua plugin | IPC / MCP -> Bitty core` and sections
//!   "IPC, CLI, and child processes" (`T-09`, `R-011`) and "MCP, Agents, and
//!   DevTools" (`T-10`, `R-013`).
//! - `bitty-docs/docs/security/risk-register.md` `R-011` / `R-012` / `R-013`.
//!
//! The crate therefore enforces at the data boundary:
//!
//! - Hard frame-payload bound: 256 KiB per message (message framing bounded
//!   256 KiB, task requirement; `RC-10` snapshot chunking).
//! - Bounded channels: request/response queue caps (`DEFAULT_REQUEST_CAPACITY`
//!   / `DEFAULT_RESPONSE_CAPACITY`, `MAX_CHANNEL_CAPACITY` 256, pending table
//!   `MAX_PENDING_REQUESTS` 64) and transport caps (`DEFAULT_TRANSPORT_CA-
//!   PACITY` 64) so a malicious peer cannot grow memory without limit (`T-01`).
//! - Method-name validation: non-empty, `<= 128` bytes, no control bytes /
//!   interior whitespace — untrusted method strings never reach dispatch without
//!   checks.
//! - Deterministic timeouts: per-request deadlines (`DEFAULT_REQUEST_TIMEOUT_MS`
//!   candidate `5 s`, `DEFAULT_MCP_TIMEOUT_MS` `10 s`, hard ceiling
//!   `MAX_REQUEST_TIMEOUT_MS` `30 s`) checked from caller-supplied `now_ms`,
//!   never wall-clock time, so headless tests and replay stay deterministic.
//! - Fail-closed overflow: [`channel::BoundedChannel::try_send`] and
//!   [`transport::StdioTransportStub::try_send_frame`] refuse when at capacity;
//!   there is no silent loss for request/response acknowledgement. The
//!   loss-tolerant `send_drop_oldest` helper exists only for explicitly
//!   attributed observation streams.
//! - Owned errors and no ambient authority: every failure is an owned
//!   [`error::IpcError`] with a stable [`error::ErrorClass`]; the crate never
//!   hands out filesystem, process, clipboard, or window handles, and it never
//!   executes plugin code. MCP responses carry the "untrusted observation data"
//!   posture required by `T-10` — content must not be mixed into instruction
//!   channels.
//!
//! # Architecture (candidate, headless-testable)
//!
//! ```text
//! peer bytes --Framer--> Frame (256 KiB bound) --decode--> IpcRequest/IpcResponse
//!                                |                           |          |
//!                                v                           v          v
//!                         StdioTransportStub        BoundedChannel  IpcEndpoint
//!                          (outgoing/incoming)      (request/response caps)
//!                                |                           |
//!                                +----> McpClientStub -------+
//!                                   (stdio stub + bounded framing + timeouts)
//! ```
//!
//! - Framing: [`frame::encode_frame`] / [`frame::decode_frame`] plus
//!   incremental [`frame::Framer`] with a bounded internal buffer.
//! - Channel: [`channel::BoundedChannel<T>`], [`channel::IpcRequest`] /
//!   [`channel::IpcResponse`], [`channel::IpcEndpoint`] (pending table +
//!   deterministic `drain_expired`).
//! - Transport: [`transport::StdioTransportStub`] — in-memory `VecDeque<Frame>`
//!   pair, `forward_to` headless pipe simulation, no OS handle.
//! - MCP: [`mcp::McpClientStub`] — stdio stub + framed correlation + candidate
//!   `MCP_REQ`/`MCP_RESP` wire helpers (headless, not normative JSON-RPC).
//!
//! # Ownership rules (ADR-0003 / ADR-0004)
//!
//! - **Depends on:** no workspace crate (isolated boundary row; may be wired
//!   into `bitty-runtime` in a follow-up slice). No third-party dependencies
//!   — pure `std` only.
//! - **Never holds** GPU objects, window handles, PTY file descriptors, or
//!   internal hot-path objects. It observes nothing from the VT/grid hot path
//!   except bounded snapshots it is explicitly handed.
//! - **`#![forbid(unsafe_code)]`** at crate and workspace level; `MSRV 1.85`,
//!   `edition = "2024"`.
//! - All structures are owned (`String`, `Vec`, `BTreeMap` …), never `&str` —
//!   so requests, responses, and frames are cloneable, comparable, and sendable
//!   without lifetimes.
//! - The crate never spawns a child, never opens a socket, and never executes
//!   plugin code (task requirement "no plugin execution").
//!
//! # Open items remaining under OQ-018 and OQ-014
//!
//! - Real `XDG_RUNTIME_DIR` socket / Windows named-pipe endpoint, permission
//!   bits (`0600` / current-user ACL), peer-credential validation per action,
//!   and the exact `no default TCP` posture remain specified by the
//!   isolation RFC and threat model but unimplemented — the stub documents
//!   honestly that `forward_to` is a headless simulation.
//! - Exact numeric queue depths, per-connection request rates (`RC-9` candidate
//!   `100 req/s`, burst, 1 MiB payload ceiling, 16 concurrent connections),
//!   and per-client observability/billing remain `OQ-014` candidates.
//! - Full JSON-RPC 2.0 wire compatibility, MCP capability negotiation, and
//!   scope/elevation semantics (`inspect` vs `input` vs `manage` vs `debug`)
//!   are deferred to the IPC/MCP protocol RFC.
//! - Authentication token distribution to child scopes (`BITTY_SOCKET`, etc.)
//!   and remote/nested-terminal safety (spoofing of CLI env vars) remain
//!   open per `cli.md`.
//!
//! Until the IPC/MCP RFC is accepted and the security reviewer signs off, this
//! crate must not be described as a stable IPC endpoint.

#![forbid(unsafe_code)]

pub mod channel;
pub mod error;
pub mod frame;
pub mod mcp;
pub mod transport;

pub use channel::{
    BoundedChannel, DEFAULT_REQUEST_CAPACITY, DEFAULT_RESPONSE_CAPACITY, IpcEndpoint, IpcRequest,
    IpcResponse, MAX_CHANNEL_CAPACITY, MAX_METHOD_BYTES, MAX_PENDING_REQUESTS, RequestId,
};
pub use error::{ErrorClass, IpcError};
pub use frame::{Frame, Framer, MAX_BUFFERED_BYTES, MAX_FRAME_BYTES, decode_frame, encode_frame};
pub use mcp::{
    DEFAULT_MCP_TIMEOUT_MS, MAX_MCP_PENDING, McpClientConfig, McpClientStub, McpNotification,
    McpRequest as McpIpcRequest, McpResponse as McpIpcResponse,
};
pub use transport::{DEFAULT_TRANSPORT_CAPACITY, MAX_TRANSPORT_CAPACITY, StdioTransportStub};

/// Crate-level re-exports that remain draft.
///
/// Importing through `bitty_ipc::draft` makes call sites that depend on
/// the provisional IPC/MCP surface self-documenting: the `draft` path is
/// the reminder that every import here may change without a semver major
/// bump until `OQ-018` is closed by an accepted RFC.
#[allow(clippy::mixed_attributes_style)]
pub mod draft {

    pub use crate::channel::{
        BoundedChannel, DEFAULT_REQUEST_TIMEOUT_MS, IpcEndpoint, IpcRequest, IpcResponse,
        MAX_REQUEST_TIMEOUT_MS, RequestId,
    };
    pub use crate::error::{ErrorClass, IpcError};
    pub use crate::frame::{Frame, Framer, MAX_FRAME_BYTES, decode_frame, encode_frame};
    pub use crate::mcp::{McpClientConfig, McpClientStub, McpNotification};
    pub use crate::transport::StdioTransportStub;
}

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn crate_is_headless_and_bounded() {
        // Framing bound.
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(encode_frame(&big).is_err());

        // Channel caps.
        let mut ep = IpcEndpoint::with_capacity(2, 2);
        ep.create_request("method".into(), vec![], 0, 1000).unwrap();
        ep.create_request("other".into(), vec![], 0, 1000).unwrap();
        assert!(
            ep.create_request("overflow".into(), vec![], 0, 1000)
                .is_err()
        );

        // Transport stub headless roundtrip.
        let mut a = StdioTransportStub::new(4);
        let mut b = StdioTransportStub::new(4);
        a.try_send_payload(b"hello ipc").unwrap();
        let moved = a.forward_to(&mut b);
        assert_eq!(moved, 1);
        assert_eq!(b.recv_incoming().unwrap().payload(), b"hello ipc");
    }

    #[test]
    fn forbid_unsafe_is_enforced_at_compile_time() {
        // This test exists only to keep the crate's `#![forbid(unsafe_code)]`
        // honest: any `unsafe` block added anywhere in the crate would fail
        // to compile, not merely lint.
        let frame = Frame::new(b"safe".to_vec()).unwrap();
        assert_eq!(frame.payload(), b"safe");
    }

    #[test]
    fn draft_docs_are_headless_mcp_flow() {
        let mut client = McpClientStub::new(McpClientConfig::default()).unwrap();
        let id = client
            .send_request("initialize".into(), b"{}".to_vec(), 0)
            .unwrap();
        assert_eq!(client.pending_count(), 1);
        client
            .inject_response(McpIpcResponse::success(id, b"ok".to_vec()).unwrap())
            .unwrap();
        let out = client.poll_responses();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, id);
    }
}
