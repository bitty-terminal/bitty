# Bitty

Bitty is a pre-implementation terminal workspace. This repository
currently contains a 16-crate Cargo workspace (see Current scaffold) plus the
quality gates that validate it. Draft crates implement the Minimal Correct
Terminal headless slice (`vt` + `pty` + `term-state` + `platform` + `config` +
`render` + `ui` + `runtime` + `app`; `package`/`lua` leaves ready) under
`publish = false` where RFCs are still draft — it does not yet provide a
released terminal binary, stable public Rust API, or published crates.io
artifacts beyond the `cargo publish --dry-run` verification recorded in
`docs/product/release-ladder.md`.

The accepted bootstrap boundary is recorded in
[ADR 0001](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0001-repository-bootstrap-baseline.md)
and the
[repository bootstrap guide](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/development/repository-bootstrap.md).
Canonical product, architecture, security, and project documentation belongs in
the [bitty-docs repository](https://github.com/bitty-terminal/bitty-docs).

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
