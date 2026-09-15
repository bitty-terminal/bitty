# `bitty-ipc`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-ipc` owns the generic out-of-process IPC bridge boundary for Bitty:
bounded framing and channels, the versioned wire envelope, scope families
with per-method authorization and a consent ledger, peer authentication,
rate limiting, an in-memory transport stub, the DevTools socket contract, an
MCP client stub, and the published bridge client that composes them in
dispatch order. Wire and transport details live behind owned types in
`src/frame.rs`, `src/wire.rs`, `src/transport.rs`, and `src/bridge.rs`.

## Status

Accepted contract, per `src/lib.rs`: the crate implements the accepted IPC
and Agent RFC, which closed `OQ-018` (see the bitty-docs open-questions
register). It introduces no new trust boundary and weakens no P0 gate.

## Boundaries

- `Cargo.toml` declares no dependencies: the crate is network-free by
  construction and mechanism-only.
- The bridge client carries no AI vocabulary and adds no network dependency
  (see `src/bridge.rs`).
- Opens no socket itself: the listener lifecycle belongs to the serving
  binary, while this crate parses, dispatches, and attests (see
  `src/devtools.rs`).
- Overflow sheds newest on streams and fails closed at capacity; exact caps
  are named constants in their modules, not repeated here.

## Layout

- `Cargo.toml` — package metadata; no dependency section.
- `src/lib.rs` — crate docs with acceptance and the module contract list.
- `src/frame.rs` — length-prefixed framing and the incremental framer.
- `src/channel.rs` — bounded request and response queues.
- `src/wire.rs` — versioned JSON envelope and method grammar.
- `src/scope.rs` — scope families, defaults, authorization, consent ledger.
- `src/auth.rs` — peer-credential verification and child tokens.
- `src/limits.rs` — rate limiting and payload and connection checks.
- `src/transport.rs` — in-memory transport stub pair.
- `src/bridge.rs` — published out-of-process bridge client.
- `src/devtools.rs` and `src/devtools/` — debugsocket contract.
- `src/mcp.rs` — bounded MCP client stub.
- `src/execution.rs` — generic execution backend and structured results.
- `src/tool_dispatch.rs` — host-side tool dispatch with consent.
- `src/snapshot.rs` — terminal snapshot read service.
- `src/rich_fragment.rs` — scene-fragment ingestion transport.
- `src/frame_digest.rs`, `src/ctl.rs`, `src/error.rs` — digests, control
  surface, and owned errors.
- `examples/` and `tests/` — usage examples and boundary tests.
