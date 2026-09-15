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
- Third-party dependencies, per `Cargo.toml`: `getrandom`, `png`, and a
  narrowed `image` facade (PNG, JPEG, WebP only); no network-facing
  dependency is declared.
- No GPU, no window system, no filesystem, and no `unsafe`, except the
  `composer` module: the external-editor round-trip writes a restricted temp
  file and spawns the configured editor with a bounded timeout plus kill.
- The legacy terminal-truth image seam stays in `bitty-term-state`; new code
  uses the `image` module here.

## Layout

- `Cargo.toml` — package metadata and decoder dependencies.
- `src/lib.rs` — crate docs with the accepted contracts and the bound table.
- `src/image.rs` — RFC-compliant presentation image store.
- `src/scene.rs` — versioned blocks and declarative scene composition.
- `src/kitty.rs`, `src/kitty_decode.rs`, `src/kitty_place.rs` — Kitty
  placeholder stub, bounded decode, and placement.
- `src/hyperlink.rs`, `src/shell.rs`, `src/clipboard.rs` — link table, shell
  zones, and clipboard helpers.
- `src/blocks.rs`, `src/hints.rs`, `src/background.rs` — command blocks, hint
  targets, and background images.
- `src/composer.rs` — external-editor round-trip seam.
- `src/geometry.rs`, `src/loader.rs`, `src/presentation.rs` — rect helpers,
  loading, and presentation assembly.
- `tests/` — headless presentation tests.
