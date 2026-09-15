# `bitty-config`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-config` owns the typed, validated configuration pipeline for Bitty:
the two-stage declarative plan from sandboxed Lua through `ConfigPlan` to
typed validation, migration, merge, diff, and reconcile, plus the project
trust lifecycle over declarative data. The user file is evaluated in the
`bitty-lua` sandbox and the typed schema remains the validation authority, as
documented in `src/lib.rs`.

## Status

Draft, per `src/lib.rs`: the crate implements the proposed
configuration-model RFC (`OQ-010`) as experimental review evidence and carries
no compatibility promise. Candidate A (two-stage declarative plan with Rust
reconciliation) is the accepted pipeline for v1; the imperative overlay is
deferred to a future RFC.

## Boundaries

- Sole workspace-internal dependency, per `Cargo.toml`: `bitty-lua`; no
  network-facing dependency is declared.
- Past Lua evaluation the pipeline stays pure data plus validation, with no
  `unsafe`.
- Must not execute project-scope Lua: trust operates over declarative data
  only (see `src/trust.rs`).
- Reload classes are declared by the schema, never inferred (see
  `src/reload.rs`).

## Layout

- `Cargo.toml` — package metadata and the `bitty-lua` dependency.
- `src/lib.rs` — crate docs with draft status and the RFC section mapping.
- `src/plan.rs` — `ConfigPlan` and layer precedence.
- `src/file.rs` — sandboxed `init.lua` / `config.lua` loading.
- `src/validation.rs` and `src/types.rs` — typed field validation.
- `src/migration.rs` — schema-version migration transforms.
- `src/merge.rs` — precedence-ordered merge with conflict attribution.
- `src/reload.rs` — reload classification and live reconciliation.
- `src/trust.rs` — hash-bound project consent lifecycle.
- `src/keymap.rs` and `src/theme.rs` — keymap and theme plan fragments.
- `src/error.rs` — owned error types.
