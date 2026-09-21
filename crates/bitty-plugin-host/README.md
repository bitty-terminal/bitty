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
- `src/effective.rs` — effective-capability intersection engine (CTX-0524,
  OQ-057): six-layer intersection, self-grant prohibition,
  typed denial with reason chain, XDG/`.bitty` policy loading, audit ledger,
  and the Hard Safety/Policy/Strategy enforcement map.
- `src/grant.rs` — hash-bound grant records and the grant store.
- `src/registry.rs` — registry, lifecycle states, and generations.
- `src/lifecycle.rs` — host-owned lifecycle enforcement (FS-2 degradation
  ladder, FS-4 enforcement records, FS-6 reload ordering; pinned by
  `tests/lifecycle.rs`).
- `src/event.rs` — event classes, bounded queues, and delivery policy.
- `src/host.rs` — host owning registry, grants, pipeline, side queue, the
  effective-capability audit ledger (`authorize_effective` /
  `delegate_effective` seam on top of the unchanged grant gate), the
  filesystem-authorization seam (`authorize_fs` / `grant_fs_consent` /
  `revoke_fs_consent` / `scrub_fs_content` with `fs_policy` /
  `fs_audit`), and the
  host secret store (`secrets` field with `resolve_secret_for_spawn` /
  `sanitized_env_view` / `scrub_against_secrets`).
- `src/secrets.rs` — host secret store and opaque credential handles
  (CTX-0521): `secret://` parsing, `SecretStore` with
  per-handle consent and audit ledger, child-env-only resolution,
  fail-closed literal detection (MPC-2), `SanitizedEnvView` agent view,
  P0-AC-026 scrubbing, and the XDG file store with user-only modes.
- `src/fs_authz.rs` — filesystem authorization: sensitive-path policy plus
  secret detection (CTX-0523): `FilesystemScope` (granted
  path set, deny-by-default, hostile patterns fail closed at construction
  via the CTX-0465/0489/0495 wave predicate without duplicating it),
  `SensitivePathPolicy` (default-deny `.env`/`.env.*`, `~/.ssh/**`,
  `~/.gnupg/**`, `~/.aws/credentials`, token stores, browser credential
  stores, with explicit per-path user consent as the `secret://`-style
  escape hatch), content-based secret detection (`content_looks_secret`,
  fail-closed over-reject consistent with the CTX-0521 heuristics, so
  renamed secrets are still caught), typed `FsDecision`/`FsError`
  (path-only diagnostics, never values, per CTX-0521/P0-AC-026), and the
  bounded `FsAuditLedger`. The host seam
  (`PluginHost::authorize_fs`/`grant_fs_consent`/`revoke_fs_consent`/
  `scrub_fs_content`) authorizes through the CTX-0524 six-layer
  intersection first, then the scope + policy + content layers, auditing
  every decision; Lua, agent-tool, and execution-request surfaces share
  this single seam with no bypass.
- `src/install.rs` — install-path verification seam.
- `src/origin.rs` — unknown-origin restrictive policy (R-020, P0-AC-032):
  advisory origin classification fail-closed to `Unknown`, restrictive
  policy for `Unknown`/`Remote`, relaxation only via explicit user
  override.
- `src/tools.rs` — tool surface helpers.
- `src/bundled.rs` — bundled-plugin declarations.
- `src/error.rs` — owned error types.
- `tests/` — headless host and pipeline tests.
