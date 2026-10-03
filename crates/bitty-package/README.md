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

- `Cargo.toml` depends only on `ed25519-dalek` 2.x for `V-C` verification:
  pure Rust, no file I/O, no network, no process spawning — the crate stays
  network-free by construction.
- No file I/O, no network, no process spawning, and no plugin VM contact, per
  `src/lib.rs`.
- No package code is ever executed: installation spans discovery through
  staging while activation is a separate transaction it never performs.
- No runtime or platform coupling, and no registry or revocation
  infrastructure beyond in-memory stub stores.
- `V-C` (`TrustMode::Signed`) signature verification is Ed25519 (bitty#767,
  OQ-029): `verify_signature` checks the record against the enrolled key
  directory fail-closed. `V-A`/`V-B` are unaffected.

## Package-manager boundary (W-101 / CTX-0927)

`src/boundary.rs` records the accepted `package-manager-boundary.md` audit as
code: every public package operation and every `bitty-package` consumer is
classified as Core-retained (bounded manifest/lock parsing, integrity
primitives, startup re-verification, runtime loading) or external-manager-owned
(resolution, source fetch, install, activation, rollback, update, uninstall,
list). `src/startup.rs` exposes `validate_installed_generation`, the pure
read-only entry point Core uses to re-derive integrity, manifest binding, and
the grant snapshot of an already-installed generation without fetching,
installing, or mutating the store. The install-time resolver, activation, and
trust modules stay in place until `bitty-plugin-manager` replaces them; the
boundary doc string records the remaining `W-101` wiring blocker.

## Layout

- `Cargo.toml` — package metadata; sole dependency is `ed25519-dalek` for V-C.
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
- `src/boundary.rs` — W-101 package-manager boundary audit (operation/consumer
  classification, Core-never-network).
- `src/startup.rs` — Core read-only installed-generation validation.
- `tests/` — lifecycle and verification tests.
