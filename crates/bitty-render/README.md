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
the `harfrust`/`swash`/`fontdb` shaped stack wrapped as the production default
plus `crossfont` wrapped as the explicit fallback, per the accepted rows of
ADR-0004 and CTX-1607, as stated in `src/lib.rs`.

## Backend selection (CTX-1607)

Production default is the `harfrust`/`swash`/`fontdb` stack in
`src/shaped.rs` (programming ligatures, `font.features`, and
`font.disable_ligatures` are live). `crossfont` is the explicit fallback:
the runtime selects it when the shaped stack cannot initialize or when
`BITTY_TEXT_STACK=classic` is set. Non-shaping rasterizers are served by the
per-cell path through `ShapedRunSource::shaping_supported`. The fallback is
observable in startup logs, the verbose tick line, and `bitty doctor`'s
`text-stack` row. See the `Backend selection` section in `src/lib.rs`.

## Font discovery (CTX-1607)

The shaped stack keeps a versioned face index at
`$XDG_CACHE_HOME/bitty/font-index-v1`. A cold process discovers the font
files once and parses only each face's `name`/`OS/2`/`post` tables in
bounded parallel workers, then writes the index atomically; a warm process
reads the cached metadata, stats each path, and reparses only changed files.
Faces load lazily, so startup reads no font outlines. The index is ignored
(and the upstream full scan runs) when `BITTY_FONT_INDEX=0` is set, when the
cache root is unavailable, or when a corrupt/version-mismatched index must be
rebuilt.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-term-state`,
  `bitty-platform`, and `bitty-config`.
- Third-party dependencies, per `Cargo.toml`: `wgpu`, the shaped
  `harfrust`/`swash`/`fontdb` stack, the `crossfont` fallback, and
  `ttf-parser`/`fontconfig-parser` for the persistent discovery index, plus
  a dev-dependency on `bitty-vt` for tests; no network-facing dependency is
  declared.
- No upstream type appears in the public API; every upstream failure flattens
  into the owned error type.
- `skia-safe` is rejected and must not be introduced; cursor visuals,
  scrollback viewport rendering, shaping, and subpixel policy remain deferred
  (see `src/lib.rs` and `src/grid.rs`).

## Chrome tokens (CTX-1477)

`src/chrome.rs` is the single Core-owned chrome language for floating
surfaces: a one-physical-pixel border, six-pixel radius, eight-pixel padding,
theme-owned muted/focused outlines, and raised/overlay surface elevations.
Help/which-key, command-palette/presentation overlays, plugin overlay
surfaces, panel rings, and band chrome resolve through this module; widgets do
not choose raw border widths or ad-hoc accents. Pixel surfaces use the
`RoundedFill` SDF ring plus the inner-arc glyph clip as the corner mask, and
overlay cards paint in three ordered sublayers (card surface, hosted row
backgrounds, border ring) so plugin row colours stay visible while the ring
remains above content; one-row bands use rounded box-drawing corners when the
corner cell is blank, with the documented light-box/square fallback when glyph
coverage is absent.

## Layout

- `Cargo.toml` — package metadata, the `sw-fallback` feature, dependencies.
- `src/lib.rs` — crate docs with the upstream boundary and scope limits.
- `src/frame.rs` — frame planning from pixel-domain damage.
- `src/grid.rs` and `src/grid/` — snapshot and damage to draw-record pipeline.
- `src/atlas.rs` and `src/cache.rs` — atlas layout math and glyph cache.
- `src/font_index.rs` — persistent versioned discovery index (cold parallel
  metadata scan, lazy per-path rebuild, atomic write).
- `src/glyph.rs`, `src/fallback.rs`, `src/crossfont_backend.rs` — rasterizer
  contract, font-chain fallback, and the crossfont wrapper (explicit
  fallback).
- `src/shaped.rs` — production shaped backend (`fontdb` + `harfrust` +
  `swash`) plus the run-shaping surface.
- `src/gpu.rs`, `src/pipeline.rs`, `src/batch.rs` — GPU context, WGSL
  pipelines, and CPU batch translation.
- `src/software.rs` — opt-in CPU compositor behind `sw-fallback`.
- `src/geometry.rs`, `src/hidpi.rs`, `src/window.rs` — rect algebra, scaling,
  and window attachment helpers.
- `src/chrome.rs` — shared chrome tokens, the rounded frame paint path, and
  the grid-corner glyph fallback.
- `src/error.rs` — owned error types.
- `tests/` — headless pipeline and seam tests.
