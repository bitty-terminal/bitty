# `bitty-term-state`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-term-state` is the Terminal Truth core: the sole interpreter of the
typed action stream produced by `bitty-vt`. It owns grid, cursor, modes,
scrollback, damage tracking, replies, and semantic zones, and performs no
I/O: device-status replies are queued and returned to the caller. Transitions
are a pure function of initial state plus action sequence, with canonical
hashing for deterministic replay, as documented in `src/lib.rs`.

## Status

Accepted contracts, per `src/lib.rs`: the terminal-state RFC sections on grid
and state invariants, the damage tracking model, and deterministic replay
guarantees, plus the ADR-0003 dependency row placing this crate above
`bitty-vt` alone.

## Boundaries

- Sole production dependency, per `Cargo.toml`: `bitty-vt`; no
  network-facing dependency is declared.
- Performs no I/O of any kind; reads happen through versioned snapshots plus
  damage, never through mutable interior access.
- Bound retention and reply behavior are named constants citing their RFC
  clauses; see the bound register in `src/lib.rs`, not repeated here.

## Layout

- `Cargo.toml` — package metadata and the `bitty-vt` dependency.
- `src/lib.rs` — crate docs with the contract list and bound register.
- `src/state.rs` and `src/state/` — state machine and zone records.
- `src/grid.rs` — grid storage and reflow.
- `src/damage.rs` — damage tracking and bounded history.
- `src/scrollback.rs` — bounded scrollback retention.
- `src/replies.rs` — queued device-status replies.
- `src/search.rs` — scrollback search over state.
- `src/modes.rs`, `src/cursor.rs`, `src/tabs.rs`, `src/charsets.rs`,
  `src/cell.rs` — modes, cursor, tab stops, charsets, and cells.
- `src/image.rs` — legacy terminal-truth image placeholder seam.
- `src/canonical.rs` — canonical serialization behind the public hash module.
- `tests/` — invariant and replay tests.
