# `bitty-rich`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-rich` owns rich presentation at the headless, bounded layer: the image
store and scene composition behind the image and scene contracts, Kitty
decode and placement, hyperlink and shell-zone tracking, clipboard helpers,
command blocks, hint targets, and the external-editor composer round-trip.
Every collection is bounded with a stated overflow policy; the bound table
lives in `src/lib.rs` and is not repeated here.

## Status

Accepted contracts, per `src/lib.rs`: the rich-presentation RFC is accepted
and closed `OQ-008`, `OQ-015`, and `OQ-016` at design level.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-term-state`,
  `bitty-vt`, and `bitty-platform`.
- Third-party dependencies, per `Cargo.toml`: `getrandom` and a
  narrowed `image` facade (PNG, JPEG, WebP only); no network-facing
  dependency is declared. The Kitty PNG codec moved to the
  `bitty-graphics` extension crate (W-141 extraction).
- No GPU, no window system, no filesystem, and no `unsafe`, except the
  `host` module: the external-editor round-trip writes a restricted temp
  file and spawns the configured editor with a bounded timeout plus kill.
- The legacy terminal-truth image seam stays in `bitty-term-state`; new code
  uses the `image` module here.

## Layout

- `Cargo.toml` — package metadata and decoder dependencies.
- `src/lib.rs` — crate docs with the accepted contracts and the bound table.
- `src/image.rs` — RFC-compliant presentation image store.
- `src/scene.rs` — versioned blocks and declarative scene composition.
- `src/kitty.rs`, `src/kitty_place.rs` — Kitty placeholder stub and
  placement policy (declared-size pre-check plus admission/eviction/rect
  geometry). Bounded decode and raster mechanics moved to the
  `bitty-graphics` extension crate (W-141 extraction).
- `src/hyperlink.rs`, `src/shell.rs`, `src/clipboard.rs` — link table, shell
  zones plus the single OSC 7/133 read view (`ShellIntegration`: cwd, zones,
  command regions, last exit code), and clipboard helpers.
- `src/blocks.rs`, `src/hints.rs`, `src/background.rs` — command blocks, hint
  targets, and background images.
- `src/host.rs` — external-editor round-trip seam (retained host mechanism).
- `src/geometry.rs`, `src/loader.rs` — rect helpers and resource-root
  loading.
- `tests/` — headless presentation tests.

## Graphics/scene retention and retired surfaces

The accepted graphics contract (`W-133`, ADR 0016 Boundary 2) keeps the
graphics trust decision and scene composition in Core: bounded intake,
`ImageStore`/`ImagePlacement`, the aggregate budget, pre-upload validation,
and the rich scene model. This crate's `image`, `scene`, `kitty_place`,
`kitty_unicode`, `background`, `wallpaper_theme`, and `geometry` modules are
therefore Core-retained implementation, not extraction material; only
`kitty_decode`/`kitty_raster` are extension-bound, and their Core copies stay
behind `kitty_seam.rs` until the composition-root binding and worker protocol
land (W-141).

Two _unwired helpers_ were removed in CTX-1412 (bitty#2117) because no Core
path consumed them: `src/presentation.rs` (`RichPresentation` snapshot
aggregation) and `src/projection.rs` (fragment-to-`RichBlock` projection).
This is not a boundary change — `scene.rs` remains Core-retained with the
SCN-1..5 limits — and the projection was the only reason this crate carried
a production edge to the extracted `bitty-ipc` repository.
`crates/bitty-rich/tests/retired_surface_guard.rs` keeps them retired.
