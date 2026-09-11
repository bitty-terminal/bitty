# Bitty

Bitty is a pre-implementation terminal workspace. This repository
currently contains a 16-crate Cargo workspace (see Current scaffold) plus the
quality gates that validate it. Draft crates implement the Minimal Correct
Terminal headless slice (`vt` + `pty` + `term-state` + `platform` + `config` +
`render` + `ui` + `runtime` + `app`; `package`/`lua` leaves ready) under
`publish = false` where RFCs are still draft — `0.0.1` has published 9 leaf
crates to crates.io (`vt`, `pty`, `platform`, `config`, `package`, `lua`,
`term-state`, `ui`, `render`) and a Linux x64 binary preview on GitHub
Releases (`bitty-app` remains `publish = false`, preview only); it does not yet
provide a stable public Rust API, and the detailed publish verification remains
recorded in `docs/product/release-ladder.md` and
`docs/product/formal-release-0.0.1.md`.

The accepted bootstrap boundary is recorded in
[ADR 0001](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0001-repository-bootstrap-baseline.md)
and the
[repository bootstrap guide](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/development/repository-bootstrap.md).
Canonical product, architecture, security, and project documentation belongs in
the [bitty-docs repository](https://github.com/bitty-terminal/bitty-docs).

## See the project workflow (CarryCtx)

CarryCtx engineering state (tasks, sessions, checkpoints) is not cloned. A
fresh clone restores it from the in-repo `refs/heads/carryctx-snapshots`
branch:

```sh
just workflow-import-dry   # fetch + validate the snapshot; no DB writes
just workflow-import       # initialize CarryCtx state if needed, then import
```

Then `carryctx stats` reports the restored tasks, sessions, and checkpoints.
Provenance, redaction, and `--force` behavior are covered under the
repository snapshot documentation below.

## Current scaffold

- The virtual Cargo workspace has 16 members (`vt`, `pty`, `platform`,
  `config`, `package`, `lua`, `term-state`, `ui`, `render`, `plugin-host`,
  `rich`, `ipc`, `agent`, `runtime`, `app`, `core`) with a `publish = false`
  workspace root; nine leaves/branch crates are `publish = true` at
  `0.0.1` and seven tail crates remain `publish = false` until their RFCs are
  accepted (see `docs/product/release-ladder.md` for the DAG and publish
  order).
- All crates use Rust edition 2024, `resolver = "3"`, MSRV `1.85`, and the
  pinned toolchain `1.97.1` with `rustfmt` and Clippy (`rust-toolchain.toml`,
  `clippy.toml`). Dependencies are pinned (`wgpu 26.0`, `crossfont 0.9`,
  `piccolo 0.3.3`, `portable-pty 0.9`, `winit 0.30`, `vte 0.15`) and workspace
  lints enforce `unsafe_code = deny`.
- The pinned stable toolchain includes `rustfmt` and Clippy; CI also runs a
  `x86_64-pc-windows-gnu` check and headless tests for the `v0.1` slice.
- `just check` runs formatting, Clippy, tests, and workflow linting without
  rewriting source files (`fmt-check + clippy + test + actionlint +
markdownlint`).

## Workflow snapshot restore

CarryCtx runtime state (`.git/carryctx/state.sqlite`) is never cloned. The
redacted engineering snapshot lives in this repository on the branch
`refs/heads/carryctx-snapshots`, one commit per publication. The commander's
merge closeout publishes it with `just workflow-publish`; a fresh clone
restores its local CarryCtx DB from that branch:

```sh
just workflow-import-dry   # fetch + validate the snapshot; no DB writes
just workflow-import       # initialize CarryCtx state if needed, then import
```

The import fetches `refs/heads/carryctx-snapshots`, refuses to replace a
non-empty local DB without `--force` (`just workflow-import --force`), and
prints provenance (snapshot commit + source). Snapshots are redacted
publication artifacts produced by `carryctx export --publication`: CarryCtx
refuses them as merge sources, so restore always uses replace mode, and a
secret that leaked before rotation must still be rotated at the source.

## Status and deferred decisions

This workspace is foundation evidence, not a product release. The `v0.1`
headless slice (shell echo, resize, backpressure — 708 tests in `ctx-0050`)
is draft evidence awaiting independent review; the remaining crate graph
slices, license, release profiles, release automation, publication policy,
platform tiers, and product behavior remain deferred to separate reviewed
decisions and tasks. See `docs/product/release-ladder.md` for the
`0.0.1`-to-`1.0` ladder and `docs/product/g1-publish-*.md` for publish
readiness.

No commit, branch, pull request, package publication, or release is implied by
the presence of these files.
