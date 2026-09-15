# `bitty-platform`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-platform` is the window and event-loop adapter that wraps `winit`
behind a strictly Bitty-owned API: a single owned event vocabulary, validated
DPI-aware size types, the owned surface-target seam consumed by the renderer,
and clipboard and notification primitives. On headless machines the event loop
entry degrades gracefully to an owned display-unavailable error instead of
panicking, as documented in `src/lib.rs`.

## Status

Role per the accepted crate graph (ADR-0003) and the `winit` adoption row of
ADR-0004, as stated in `src/lib.rs`.

## Boundaries

- Third-party dependencies, per `Cargo.toml`: `winit`, `arboard` for
  clipboard, and a pinned `raw-window-handle` re-export; no workspace-crate
  dependency.
- No `winit` type escapes this crate except the single
  `raw-window-handle` exception at the GPU surface boundary (see
  `src/surface.rs`).
- No business semantics: no grid, render, terminal-state, or plugin coupling,
  and input encoding policy deliberately lives elsewhere.
- Display-dependent integration tests sit behind the default-off `gui-tests`
  feature and never run in CI.

## Layout

- `Cargo.toml` — package metadata, features, and upstream dependencies.
- `src/lib.rs` — crate docs with ownership rules and the headless test seam.
- `src/app.rs` — event-loop driver over an owned handler.
- `src/event.rs` — owned platform event vocabulary.
- `src/keyboard.rs` — logical key subset carried by keyboard events.
- `src/dpi.rs` — validated DPI-aware size types.
- `src/surface.rs` — owned surface-target GPU attachment seam.
- `src/clipboard.rs` — clipboard primitives.
- `src/url.rs` — URL helpers.
- `src/error.rs` — owned error types.
- `tests/` — headless unit coverage plus gated display tests.
