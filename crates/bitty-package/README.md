# `bitty-package`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-package` owns the package lifecycle and integrity verification for
Bitty: owned manifest and lockfile types with digest binding, the staged
lifecycle from discovery through activation to retention, the integrity
verification chain applied identically to every source type, publisher trust
options, atomic activation with rollback, and the closed constraint grammar
with its deterministic resolver. The pipeline shape is documented in
`src/lib.rs`.

## Status

Draft, per `src/lib.rs`: the crate implements the proposed
package-lifecycle RFC (`OQ-021`, `OQ-022`) and its contract may change without
a semver major bump until the RFC is accepted. Nothing here claims normative
behavior, stable formats, or settled publisher-trust policy.

## Boundaries

- `Cargo.toml` declares no dependencies: the crate is network-free by
  construction.
- No file I/O, no network, no process spawning, and no plugin VM contact, per
  `src/lib.rs`.
- No package code is ever executed: installation spans discovery through
  staging while activation is a separate transaction it never performs.
- No runtime or platform coupling, and no registry or revocation
  infrastructure beyond in-memory stub stores.

## Layout

- `Cargo.toml` — package metadata; no dependency section.
- `src/lib.rs` — crate docs with draft status and the pipeline description.
- `src/manifest.rs` and `src/lockfile.rs` — manifest and lockfile types.
- `src/lifecycle.rs` — staged lifecycle with fail-closed transitions.
- `src/integrity.rs` — integrity verification chain.
- `src/trust.rs` — publisher trust options and re-approval.
- `src/activation.rs` — atomic activation, retention, and rollback.
- `src/version.rs`, `src/requirement.rs`, `src/resolver.rs` — constraint
  grammar and deterministic resolution.
- `src/source.rs` — source declarations.
- `src/error.rs` — owned error types.
- `tests/` — lifecycle and verification tests.
