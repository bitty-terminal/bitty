# `bitty-plugin-host`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-plugin-host` is the plugin-platform host: it owns the plugin registry,
manifest validation, the closed capability grammar, the hash-bound grant
lifecycle, and the bounded event pipeline plus the bounded side queue that
observes terminal events without blocking the producer. Its `install` module
wires package-pipeline verification into the install path before any staging.
The RFC section mapping lives in `src/lib.rs`.

## Status

Accepted contracts, implementation not yet verified, per `src/lib.rs`: the
plugin-platform RFC is accepted and closed `OQ-011`, `OQ-012`, and `OQ-013`.
The implementation is `Implemented`, not yet `Verified`; the install wiring
to the still-proposed package-lifecycle RFC is a draft seam.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-term-state`,
  `bitty-config`, and `bitty-package`; no network-facing dependency is
  declared.
- Pure data plus validation on the host side: no Lua VM coupling, no file
  I/O, no platform window or GPU coupling, and no `unsafe`.
- The capability grammar is deny-by-default and closed: unknown identifiers
  fail validation instead of being ignored.
- Presentation never rewrites terminal truth and safe mode skips third-party
  plugins.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs with status and the RFC section mapping.
- `src/manifest.rs` — manifest discovery and validation.
- `src/capability.rs` — closed capability grammar.
- `src/grant.rs` — hash-bound grant records and the grant store.
- `src/registry.rs` — registry, lifecycle states, and generations.
- `src/event.rs` — event classes, bounded queues, and delivery policy.
- `src/host.rs` — host owning registry, grants, pipeline, and side queue.
- `src/install.rs` — install-path verification seam.
- `src/tools.rs` — tool surface helpers.
- `src/bundled.rs` — bundled-plugin declarations.
- `src/error.rs` — owned error types.
- `tests/` — headless host and pipeline tests.
