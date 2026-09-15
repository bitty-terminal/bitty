# `bitty-vt`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-vt` is the byte-stream VT parser that produces semantic
`TerminalAction` values for the downstream state machine. It wraps the `vte`
state machine as an implementation detail, pre-scans Kitty `APC G` sequences
the underlying machine leaves inert, and decodes invalid bytes to the
replacement character identically offline and live. Parser obligations follow
the typed action interface of the terminal-state RFC (see `src/lib.rs`).

## Boundaries

- Sole production dependency, per `Cargo.toml`: `vte`, which stays behind
  the public API: no `vte` type appears in the public surface.
- No terminal state and no I/O: the parser is a pure function from byte
  stream to action stream.
- Bounded, panic-free parsing: over-limit parameters, sequences, and payloads
  yield truncated or inert actions, never unbounded growth (see
  `src/bounded.rs`).
- Zero `unsafe`, per the workspace lint and `src/lib.rs`.

## Layout

- `Cargo.toml` — package metadata and the `vte` dependency.
- `src/lib.rs` — crate docs with the topology rules and usage example.
- `src/action.rs` — `TerminalAction` and its value vocabulary.
- `src/parser.rs` and `src/parser/` — byte driver plus dispatch and SGR.
- `src/bounded.rs` — bounded parameter and payload types.
- `src/kitty_apc.rs` — Kitty APC chunk reassembly under a ledger cap.
- `seeds/` — parser corpora seeds.
- `tests/` — parser conformance tests.
