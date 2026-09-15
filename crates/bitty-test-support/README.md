# `bitty-test-support`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-test-support` provides the shared test-harness helpers for live-PTY
gating: `pty_supported` reports whether the live PTY backend exists on this
machine, and the `require_pty` macro opens every live-spawn test with an
early passing skip where it does not. A force-no-PTY environment override
exists so the skip path is exercisable everywhere. See `src/lib.rs` for the
gating contract and platform notes.

## Boundaries

- `Cargo.toml` declares no dependencies: the crate is network-free by
  construction.
- Detection and gating only: the crate never spawns processes itself.
- Unix and Windows (ConPTY) are the Tier-1 backends; POSIX-only programs
  still need their own platform gate on top.
- The override environment variable and truth table are defined in
  `src/lib.rs`; consult that file rather than copying names here.

## Layout

- `Cargo.toml` — package metadata; no dependency section.
- `src/lib.rs` — detection core, override check, gate macro, and docs.
