//! `bitty-ipc`: IPC and MCP boundary for Bitty (accepted IPC-Agent RFC, OQ-018).
//!
//! This crate implements the **accepted** `IPC and Agent RFC`
//! (`bitty-docs/docs/specifications/ipc-agent-rfc.md`, frontmatter
//! `accepted` on 2026-08-29, closes `OQ-018`). The RFC's instance
//! selection, transport and framing, wire and auth, scope families,
//! rate limits `RC-9`/`RC-10`, Agent bounded messages, consent and
//! streaming, and verification plan are normative and now have
//! implementation evidence here. No new trust boundary is introduced
//! and no P0 gate is weakened.
//!
//! Acceptance was per independent category-owner, docs-curator, and
//! security-auditor review (CTX-0076) with P0 sign-off on 2026-08-29;
//! see the RFC's `P0 Review Sign-off` and `bitty-docs/docs/reviews/p0-review-checklist.md`.
//! The lifecycle is `Draft -> experimental review evidence -> Accepted (2026-08-29) -> normative`.
//!
//! # What this crate owns
//!
//! - Bounded framing: length-prefixed `u32` BE + payload `<= 256 KiB`
//!   (`frame::MAX_FRAME_BYTES`, `frame::encode_frame` / `decode_frame`,
//!   incremental `frame::Framer` with bounded internal buffer `512 KiB`
//!   max, shed newest on overflow).
//! - Bounded channels: request/response queue caps (`channel::DEFAULT_REQUEST_CAPACITY`
//!   / `DEFAULT_RESPONSE_CAPACITY`, `channel::MAX_CHANNEL_CAPACITY` 256,
//!   pending table `MAX_PENDING_REQUESTS` 64) and transport caps
//!   (`transport::DEFAULT_TRANSPORT_CAPACITY` 64) so a malicious peer
//!   cannot grow memory without limit (T-01, RC-9).
//! - Wire envelope v1: versioned JSON envelope `v`, `id` bounded 64 bytes,
//!   `method` RFC grammar, params bounded JSON depth `<= 32`, no ambient
//!   `auth`/`scope` field (wire::WIRE_VERSION, wire::validate_request_envelope).
//! - Scopes: families `terminal`/`view`/`config`/`plugin`/`process`/`debug`
//!   with 13 distinct scopes, `ScopeSet` defaults for CLI interactive vs
//!   MCP/Agent read-only, per-method required scope, server-side
//!   `authorize_method`, and per-client/per-scope `ConsentLedger` (scope).
//! - Auth: headless peer-credential verification via UID equality
//!   (`auth::verify_peer_uid`, `auth::verify_unix_endpoint` for `SO_PEERCRED`
//!   / `LOCAL_PEERCRED`, `auth::verify_windows_pipe` for named-pipe ACL),
//!   and short-lived child tokens over PTY fd never via environment
//!   (`auth::ChildToken`, `auth::ChildTokenStore`, 60 s TTL, bounded 64).
//! - Rate limits: `RC-9` (100 req/s, 2x burst, 1 MiB payload, 16 concurrent
//!   connections, shed newest) and `RC-10` (256 KiB stream chunk ceiling)
//!   with headless `limits::RateLimiter` and payload/connection checks.
//! - Transport stub: in-memory `transport::StdioTransportStub` pair with
//!   `forward_to` headless pipe simulation, no OS handle, fail-closed at
//!   capacity, `send_drop_oldest` only for observation streams.
//! - DevTools socket contract (`devtools::Dispatcher`,
//!   `devtools::resolve_socket_path`, `devtools::serve_connection`):
//!   headless `bitty.debug/*` request parsing, extensible method dispatch
//!   (`ping`, `getSnapshot`), framed serving over caller-provided streams,
//!   and socket-directory attestation (`0700`/`0600`). Opens no socket
//!   itself; the `bitty-app` servo owns the listener lifecycle.
//! - MCP stub: `mcp::McpClientStub` with bounded framing, correlation, and
//!   deterministic timeouts (`DEFAULT_MCP_TIMEOUT_MS` 10 s, ceiling 30 s).
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
//!   "IPC, CLI, and child processes" (`T-09`, `R-011`, `R-012`) and "MCP, Agents, and
//!   DevTools" (`T-10`, `R-013`).
//! - `bitty-docs/docs/security/risk-register.md` `R-011` / `R-012` / `R-013` / `R-014`.
//!
//! The crate therefore enforces at the data boundary:
//!
//! - Hard frame-payload bound: 256 KiB per message (T-01, invariant 7, RC-10).
//! - Bounded channels/transport and pending table (T-01, RC-9).
//! - Method-name RFC grammar validation before dispatch.
//! - Deterministic timeouts checked from caller-supplied `now_ms`.
//! - Peer-credential UID equality (`SO_PEERCRED` paradigm) before any
//!   request is parsed; directory/socket modes `0700`/`0600` and owner
//!   checks fail-closed (no unauthenticated fallback).
//! - Per-request scope evaluation server-side; client-sent `scope`/`auth`
//!   fields are ignored and rejected at the wire layer.
//! - Fail-closed overflow (`try_send` refuses at capacity) and countable
//!   shedding/denial (FS-IP4 attribution).
//! - Owned errors and no ambient authority; `MCP` responses remain
//!   untrusted observation data (T-10).
//!
//! # Architecture (headless-testable)
//!
//! ```text
//! peer bytes --Framer--> Frame (256 KiB) --decode--> WireEnvelope(v1) --authorize--> IpcRequest
//!                                |                         |                |          |
//!                                v                         v                v          v
//!                         StdioTransportStub          ScopeSet        ConsentLedger  BoundedChannel
//!                          (outgoing/incoming)         Auth (SO_PEERCRED)  RateLimiter
//!                                |                         |                |
//!                                +----> McpClientStub -----+                |
//!                                   (stdio + framing + timeouts)         IpcEndpoint
//! ```
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
//! - All structures are owned (`String`, `Vec`, `BTreeMap` …), never `&str`.
//! - The crate never spawns a child, never opens a socket, and never executes
//!   plugin code.

#![forbid(unsafe_code)]

pub mod auth;
pub mod channel;
pub mod devtools;
pub mod error;
pub mod frame;
pub mod limits;
pub mod mcp;
pub mod scope;
pub mod transport;
pub mod wire;

pub use auth::{
    CHILD_TOKEN_TTL_MS, ChildToken, ChildTokenStore, DIR_MODE, MAX_CHILD_TOKENS,
    MAX_SCOPED_ID_BYTES, MAX_TOKEN_TTL_MS, PeerCredentials, SOCKET_MODE, verify_peer_uid,
    verify_unix_endpoint, verify_windows_pipe,
};
pub use channel::{
    BoundedChannel, DEFAULT_REQUEST_CAPACITY, DEFAULT_RESPONSE_CAPACITY, IpcEndpoint, IpcRequest,
    IpcResponse, MAX_CHANNEL_CAPACITY, MAX_METHOD_BYTES, MAX_PENDING_REQUESTS, RequestId,
};
pub use error::{ErrorClass, IpcError};
pub use frame::{Frame, Framer, MAX_BUFFERED_BYTES, MAX_FRAME_BYTES, decode_frame, encode_frame};
pub use limits::{
    RC9_BURST_PER_SEC, RC9_MAX_CONNECTIONS, RC9_PAYLOAD_CAP_BYTES, RC9_REQ_PER_SEC, RC9_WINDOW_MS,
    RC10_CHUNK_CEILING, RateLimiter, check_connection_cap, check_payload_cap,
};
pub use mcp::{
    DEFAULT_MCP_TIMEOUT_MS, MAX_MCP_PENDING, McpClientConfig, McpClientStub, McpNotification,
    McpRequest as McpIpcRequest, McpResponse as McpIpcResponse,
};
pub use scope::{
    ConsentGrant, ConsentLedger, Scope, ScopeSet, authorize_method, required_scope_for_method,
    validate_method_name,
};
pub use transport::{DEFAULT_TRANSPORT_CAPACITY, MAX_TRANSPORT_CAPACITY, StdioTransportStub};
pub use wire::{
    CHUNK_CEILING, MAX_ID_BYTES, MAX_JSON_DEPTH, WIRE_VERSION, validate_chunk,
    validate_request_envelope, validate_response_envelope, validate_wire_version,
};

/// Crate-level re-exports that remain draft or stable per RFC.
///
/// Importing through `bitty_ipc::draft` keeps call sites that depend on the
/// provisional surface self-documenting. The `draft` path now mirrors the
/// accepted RFC's stable scopes/auth/wire; it remains for compatibility while
/// new imports should prefer the top-level `bitty_ipc::Scope` / `PeerCredentials`.
#[allow(clippy::mixed_attributes_style)]
pub mod draft {
    pub use crate::auth::{ChildToken, ChildTokenStore, PeerCredentials};
    pub use crate::channel::{
        BoundedChannel, DEFAULT_REQUEST_TIMEOUT_MS, IpcEndpoint, IpcRequest, IpcResponse,
        MAX_REQUEST_TIMEOUT_MS, RequestId,
    };
    pub use crate::error::{ErrorClass, IpcError};
    pub use crate::frame::{Frame, Framer, MAX_FRAME_BYTES, decode_frame, encode_frame};
    pub use crate::mcp::{McpClientConfig, McpClientStub, McpNotification};
    pub use crate::scope::{Scope, ScopeSet};
    pub use crate::transport::StdioTransportStub;
    pub use crate::wire::{WIRE_VERSION, validate_request_envelope};
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

    #[test]
    fn scopes_and_auth_headless() {
        // Scope: CLI default can inspect but not manage
        let cli = ScopeSet::cli_default();
        assert!(authorize_method("terminal.text", &cli).is_ok());
        assert!(authorize_method("terminal.close", &cli).is_err());

        // Auth: same UID passes, different fails
        let peer = PeerCredentials::new(1000, 1000, 42);
        assert!(verify_peer_uid(peer, 1000).is_ok());
        assert!(verify_peer_uid(peer, 1001).is_err());

        // Wire version
        assert!(validate_wire_version(1).is_ok());
        assert!(validate_wire_version(2).is_err());

        // Child token headless
        let tok = ChildToken::new(
            "tok-1".into(),
            Scope::TerminalInspect,
            "t:4".into(),
            0,
            1000,
        )
        .unwrap();
        let mut store = ChildTokenStore::new();
        store.insert(tok).unwrap();
        assert!(
            store
                .verify("tok-1", Scope::TerminalInspect, "t:4", 500)
                .is_ok()
        );
        assert!(
            store
                .verify("tok-1", Scope::TerminalInspect, "t:4", 1000)
                .is_err()
        );
    }

    #[test]
    fn rate_limits_headless() {
        let mut lim = RateLimiter::rc9_default();
        for _ in 0..RC9_BURST_PER_SEC {
            lim.check(0).unwrap();
        }
        assert!(lim.check(0).is_err());
        // Payload cap
        assert!(check_payload_cap(RC9_PAYLOAD_CAP_BYTES).is_ok());
        assert!(check_payload_cap(RC9_PAYLOAD_CAP_BYTES + 1).is_err());
        assert!(check_connection_cap(RC9_MAX_CONNECTIONS - 1).is_ok());
        assert!(check_connection_cap(RC9_MAX_CONNECTIONS).is_err());
    }
}
