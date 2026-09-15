# `bitty-agent`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-agent` owns the generic, host-neutral Agent protocol vocabulary for
Bitty: owner-qualified identity, bounded messages, a stub tool vocabulary
with deterministic results and no LLM I/O, read-only observations, and the
bounded side queue plus session that carry them. It is the small,
headless-testable layer beneath the IPC and Agent contracts; wire framing,
transport, authentication, scopes, and rate limits live in `bitty-ipc`.

## Status

Accepted contract, implementation not yet verified, per `src/lib.rs`: the IPC
and Agent RFC closed `OQ-018` and the DevTools RFC closed `OQ-019` (see the
bitty-docs open-questions register). The implementation here is `Implemented`,
not yet `Verified`; do not describe its behavior as shipped until a release
ships it.

## Boundaries

- `Cargo.toml` declares no dependencies: the crate is network-free by
  construction.
- Depends on nothing inside or outside the workspace.
- Model selection, authentication, consent, streaming, real tool dispatch,
  compaction, and MCP transport live outside this crate (see `src/lib.rs`).
- Observations are read-only snapshots and terminal output is labeled
  untrusted; the crate never weakens read-only-by-default, least-privilege
  scopes, untrusted-observation labeling, or per-client consent.

## Layout

- `Cargo.toml` — package metadata; no dependency section.
- `src/lib.rs` — crate docs with status and the security-alignment section.
- `src/id.rs` — owner-qualified `AgentId` identity type.
- `src/message.rs` — bounded `AgentMessage` vocabulary.
- `src/tool.rs` — `ToolSpec`, `ToolCall`, `ToolResult`, and `ToolRegistry`.
- `src/observation.rs` — bounded read-only `AgentObservation` snapshots.
- `src/queue.rs` — bounded side queue shared by sessions and observations.
- `src/session.rs` — `AgentSession` owning identity, registry, and history.
- `src/error.rs` — owned error types.
