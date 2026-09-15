# `bitty-core`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-core` is the bootstrap compilation seed for the pre-implementation
Bitty workspace, per `src/lib.rs`. It exists so the workspace has a compiling
member while product behavior lands in the crates that own their boundaries,
and its `Cargo.toml` description marks it to be retired.

## Boundaries

- `Cargo.toml` declares no dependencies: the crate is network-free by
  construction.
- A workspace member with a single module and no public items: it owns no
  product behavior, no public API surface, and no runtime semantics.
- Edition, version, and lints follow the workspace root, like every other
  member.
- Must not gain logic that belongs to an owning crate; new behavior goes to
  the crate that owns its boundary.

## Layout

- `Cargo.toml` — package metadata with the to-be-retired description.
- `src/lib.rs` — single-line crate documentation.
- Listed under `members` in the workspace root `Cargo.toml`.
