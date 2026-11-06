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

## Config version and migration

`init.lua` returns a plain-data table with an optional top-level
`config_version` integer. `CURRENT_CONFIG_VERSION` (currently `1`, in
`src/migration.rs`) names the shape this crate understands; a file that omits
the key is version `0`, the pre-version shape, and still loads unchanged.
A declared version newer than `CURRENT_CONFIG_VERSION` fails closed and names
the version (`ConfigError::ConfigVersionUnsupported`); it is never guessed.

`bitty config migrate` loads the file, prints the per-key change list
(`config_version = (absent) -> 1`), and rewrites it atomically (unique sibling,
sync, mode preserved, rename as the commit point). `--dry-run` computes and
prints the list without writing. `--backup` is the default and copies the
original to `<file>.bak-<from-version>` (a numbered suffix when that name is
taken, never overwriting an older backup); `--no-backup` skips it. Safe mode
(`--safe`) refuses with a typed error because it never writes. A file already
at the current version prints `nothing to migrate` and exits `0` without
touching the filesystem.

`config validate` and `config check` report the declared version and, when it
is older, the one-line hint that a migration is available.

Transforms live in the `CONFIG_TRANSFORMS` table keyed by **source** version.
Adding version 2 is one change with three parts:

1. Add one named transform `fn migrate_config_v1_to_v2(ConfigPlan) ->
Result<ConfigPlan, ConfigError>` and register it in `CONFIG_TRANSFORMS`.
2. Add one test that migrates a version-1 fixture and asserts the renamed or
   reshaped key (and append its `ConfigChange` entries in `config_changes`).
3. Add one entry to the config guide describing the change.

Bump `CURRENT_CONFIG_VERSION` in the same change. Version 1 renames no keys,
so `config migrate` only stamps the version. A transform that renames or
reshapes keys also needs a Lua serializer for the transformed plan; until that
exists, `config migrate` refuses with `E_CONFIG_MIGRATION` instead of writing a
version stamp that hides the unapplied change.

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
- `src/migration.rs` — `config_version` constants, transforms, change list,
  and the source rewriter behind `bitty config migrate`.
- `src/merge.rs` — precedence-ordered merge with conflict attribution.
- `src/reload.rs` — reload classification and live reconciliation.
- `src/trust.rs` — hash-bound project consent lifecycle.
- `src/keymap.rs` and `src/theme.rs` — keymap and theme plan fragments.
- `src/error.rs` — owned error types.
