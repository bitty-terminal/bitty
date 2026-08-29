<!-- markdownlint-disable MD025 -->

# Resize Compatibility (`tests/compat/resize`)

Resize and reflow corpus — deterministic grid reflow, scroll region, alt-screen, and `vttest` resize menus.

## Source

- Resize events: `PhysicalSize` to logical grid via `RuntimeConfig::grid_from_pixels` (8×16 cell), `State::resize` (bounded `[1,1000]` per dimension, scrollback reflow), `map_resize_to_surface_extent` (zero-size skip). Captured from `vttest` menu 2 (screen resize) and from `bitty-runtime` headless resize fixtures.
- Ghostty / kitty / WezTerm differential — feed same resize + byte burst and compare `Snapshot` dimensions / `Damage` regions to reference dumped grid; zero-size minimized/occluded contract must be honest (skip) per `bitty_platform::map_resize_to_surface_extent`.
- Existing baseline — `crates/bitty-runtime/tests/v01_minimal_terminal.rs::v01_resize_headless_reconfigures_surface_and_reflows_layout_deterministically` (800×600 → 100×37, horizontal split 100 → 50+50, zero skip), `crates/bitty-term-state/tests/resize_scrollback.rs`, `crates/bitty-runtime/tests/resize_scrollback.rs`.

## Bounds

- `#![forbid(unsafe_code)]`, headless, `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`, resize clamped `[1,1000]`.
- No window/GPU — `State::resize` is in-memory, no `winit`/`wgpu` in this lab.

## Layout

```text
resize/
  README.md
  corpus/
    01-resize-reflow.bin      # bytes before/after resize marker
    placeholder.bin
```
