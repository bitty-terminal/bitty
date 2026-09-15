# `bitty-render`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-render` owns rendering for the microkernel core: frame planning from
damage descriptors, glyph atlas math with a bounded cache, upstream
rasterization behind a Bitty-owned trait, the grid pipeline from terminal
snapshots to owned draw records, CPU batch translation, GPU presentation
resources, and an opt-in CPU compositor for headless runs. The grid pipeline
reads only the public snapshot and damage surface of `bitty-term-state` and
never mutates terminal state, per `src/lib.rs`.

## Status

Render row of the Core Workspace Topology (ADR-0003), with `wgpu` adopted and
`crossfont` wrapped per the accepted rows of ADR-0004, as stated in
`src/lib.rs`.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-term-state`,
  `bitty-platform`, and `bitty-config`.
- Third-party dependencies, per `Cargo.toml`: `wgpu` and `crossfont`, plus a
  dev-dependency on `bitty-vt` for tests; no network-facing dependency is
  declared.
- No upstream type appears in the public API; every upstream failure flattens
  into the owned error type.
- `skia-safe` is rejected and must not be introduced; cursor visuals,
  scrollback viewport rendering, shaping, and subpixel policy remain deferred
  (see `src/lib.rs` and `src/grid.rs`).

## Layout

- `Cargo.toml` — package metadata, the `sw-fallback` feature, dependencies.
- `src/lib.rs` — crate docs with the upstream boundary and scope limits.
- `src/frame.rs` — frame planning from pixel-domain damage.
- `src/grid.rs` and `src/grid/` — snapshot and damage to draw-record pipeline.
- `src/atlas.rs` and `src/cache.rs` — atlas layout math and glyph cache.
- `src/glyph.rs`, `src/fallback.rs`, `src/crossfont_backend.rs` — rasterizer
  contract, font-chain fallback, and the crossfont wrapper.
- `src/gpu.rs`, `src/pipeline.rs`, `src/batch.rs` — GPU context, WGSL
  pipelines, and CPU batch translation.
- `src/software.rs` — opt-in CPU compositor behind `sw-fallback`.
- `src/geometry.rs`, `src/hidpi.rs`, `src/window.rs` — rect algebra, scaling,
  and window attachment helpers.
- `src/error.rs` — owned error types.
- `tests/` — headless pipeline and seam tests.
