# `bitty-lua`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-lua` wraps the pure-Rust `piccolo` VM to give Bitty deterministic,
bounded Lua execution for per-plugin isolation and, per DEC-0011, user
configuration evaluation. Each VM instance gets isolated globals with a
restricted standard library, host work happens only through
capability-checked callbacks, and instruction, wall-clock, and heap budgets
fail closed into suspension. Budget mechanics and the determinism contract
are documented in `src/lib.rs`.

## Boundaries

- Sole third-party dependency, per `Cargo.toml`: `piccolo`; no
  network-facing dependency is declared.
- Standard library only, with no `unsafe` and no ambient `io`, `os`, or
  `debug` authority (see `src/stdlib.rs`).
- Configuration chunks run under the same budgets as plugins and must return
  plain-data tables; the typed schema in `bitty-config` stays the validation
  authority (see `src/config.rs`).
- There is no `mlua` dependency anywhere, so no VM type conflict can arise.

## Layout

- `Cargo.toml` — package metadata and the `piccolo` dependency.
- `src/lib.rs` — crate docs with the role, budgets, and determinism sections.
- `src/host.rs` — VM lifecycle, budget enforcement, and host calls.
- `src/ui.rs` — Plugin API v1 declarative UI scenes (`bitty.ui.mount` /
  `bitty.ui.update`) and the bounded `UiNode` model.
- `src/config.rs` — configuration-chunk evaluation and table extraction.
- `src/stdlib.rs` — restricted standard-library construction.
- `tests/` — headless budget and isolation tests.
