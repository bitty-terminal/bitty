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

## Graphics/scene retention decision (0.0.25, #2163)

The accepted graphics extraction contract (`W-133`, ADR 0016 Boundary 2)
assigns **decode and processing** to the graphics extension and keeps the
trust decision in Core: bounded protocol intake, `ImageStore` /
`ImagePlacement`, the aggregate image budget, pre-allocation and pre-upload
validation, and Terminal Truth. The extension's own scope record keeps the
whole placement-policy half and config-file background decode in Core.

Consequently this crate's graphics/scene-shaped modules are **Core-retained
implementation, not extraction candidates**:

- `image.rs` — Core-owned `ImageStore` / `ImagePlacement` and the IMG-1..IMG-8
  budget;
- `scene.rs`, `presentation.rs`, `projection.rs` — scene composition,
  presentation assembly, and fragment projection owned by the rich
  presentation contract;
- `kitty_place.rs`, `kitty_unicode.rs` — placement/admission/eviction policy,
  origin confinement, scroll anchoring, alternate-screen suppression, and
  placeholder geometry;
- `background.rs` — config-file background decode, explicitly outside the
  extension scope;
- `wallpaper_theme.rs` — appearance and palette policy, not graphics decode;
- `geometry.rs` — shared pixel-rect carriers.

The only extension-bound mechanics are `kitty_decode.rs` and
`kitty_raster.rs`. They already sit behind the Core-owned `kitty_seam.rs`
traits (`CoreKittyBackend` is the Core-local backend). Removing the
Core-local copies needs the composition-root binding and the graphics worker
protocol, parked in the extension's `CTX-0003` and Core `W-141`; no wire
contract is invented here. `kitty_seam.rs` is the single module that may name
the graphics extension crate, and the W-147 source fence enforces it.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-term-state`,
  `bitty-vt`, and `bitty-platform`.
- Third-party dependencies, per `Cargo.toml`: `getrandom` and a
  narrowed `image` facade (PNG, JPEG, WebP only); no network-facing
  dependency is declared. The graphics extension-side seam contract is
  published (`bitty-graphics#15`); Core keeps its bounded decode/raster
  backend behind `kitty_seam.rs` until the composition-root binding lands
  (W-141, worker protocol parked).
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
- `src/kitty_place.rs` — Kitty placement policy (declared-size pre-check plus
  admission/eviction/rect geometry) and the kitty ledger bounds
  (`KITTY_PLACE_MAX_IMAGES`, `KITTY_LEDGER_MAX_BYTES`). Bounded decode and
  raster mechanics moved to the `bitty-graphics` extension behind
  `src/kitty_seam.rs` (W-141 extraction); bounded `APC G` intake lives in
  `bitty-vt/src/kitty_apc.rs`.
- `src/kitty_seam.rs` — Core-owned decode/raster traits, seam caps, and the
  Core-local backend; the only module allowed to name the graphics extension
  crate.
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
`kitty_unicode`, `background`, and `geometry` modules are therefore
Core-retained implementation, not extraction material; only
`kitty_decode`/`kitty_raster` are extension-bound, and their Core copies stay
behind `kitty_seam.rs` until the composition-root binding and worker protocol
land (W-141).

Unwired helpers were removed in CTX-1412 (bitty#2117) and CTX-1415
(bitty#2358 / bitty#2359) because no Core path consumed them:
`src/presentation.rs` (`RichPresentation` snapshot aggregation),
`src/projection.rs` (fragment-to-`RichBlock` projection), `src/kitty.rs`
(legacy Kitty placeholder stub — real intake is `bitty-vt/src/kitty_apc.rs`),
and `src/wallpaper_theme.rs` (wallpaper-derived colorscheme; theme derivation
is plugin presentation policy, and the wired path is the plugin
`bitty.image.read` bridge). This is not a boundary change — `scene.rs`
remains Core-retained with the SCN-1..5 limits — and the projection was the
only reason this crate carried a production edge to the extracted `bitty-ipc`
repository. `crates/bitty-rich/tests/retired_surface_guard.rs` keeps all four
surfaces retired.
