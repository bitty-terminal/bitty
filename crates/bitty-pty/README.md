# `bitty-pty`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-pty` is the owned PTY and process-lifecycle crate: spawning and
shutdown, resize, and I/O with explicit end-to-end backpressure from the
kernel through a bounded channel to the consumer. Unix and Windows (ConPTY)
are the Tier-1 backends; other platforms fail to compile rather than
misbehave, as documented in `src/lib.rs`.

## Status

PTY row of the Core Workspace Topology (ADR-0003), wrapping `portable-pty`
per the accepted upstream decision of ADR-0004, as stated in `src/lib.rs`.

## Boundaries

- One workspace-crate dependency, Windows only: `bitty-winjob`, the
  reviewed Job Object adapter behind the owned-tree backend; this crate
  itself stays free of `unsafe` (see `src/lib.rs`).
- Sole third-party PTY dependency, per `Cargo.toml`: `portable-pty`, which
  is wrapped and never adopted: its types never appear in the public API
  and every upstream failure flattens into the owned error type. The
  owned-tree module adds the platform system-call wrappers `rustix`
  (Linux) and `nix` (macOS).
- No shell interpolation: programs spawn from a direct argv vector.
- Children inherit the session environment with documented overrides, and
  output buffering is bounded (named bounds live in `src/lib.rs`, not here).

## Layout

- `Cargo.toml` — package metadata and dependencies.
- `src/lib.rs` — crate docs with the upstream boundary and security defaults.
- `src/builder.rs` — `PtyBuilder` for spawn configuration.
- `src/pty.rs` — spawned PTY handle and lifecycle.
- `src/reader.rs` and `src/writer.rs` — bounded channel I/O ends.
- `src/platform/` — Unix and ConPTY backend implementations.
- `src/error.rs` — owned error types.
- `src/tree.rs` and `src/tree/` — owned process trees: process groups on
  Linux/macOS, Job Objects on Windows.
- `tests/` — backend and backpressure tests.
