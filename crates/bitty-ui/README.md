# `bitty-ui`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-ui` owns the View, layout tree, focus, and selection primitives:
viewports over snapshots, the deterministic layout solver over split, stack,
and overlay nodes, focus traversal, wide-character-aware selection anchoring,
headless search state, and the geometry, decoration, presentation-mode, and
scrollbar helpers around them. Everything is pure layout algebra, headless
testable, as documented in `src/lib.rs`.

## Status

Role per the accepted crate graph (ADR-0003), as stated in `src/lib.rs`:
view, layout node, split and stack and overlay, focus and resize, and
selection primitives above `bitty-term-state` alone.

## Boundaries

- Sole production dependency, per `Cargo.toml`: `bitty-term-state`, read
  only through its public snapshot surface; no network-facing dependency is
  declared.
- No render, platform, or PTY coupling: the crate compiles without any
  display server, and damage and present integration belongs to
  `bitty-runtime`.
- Split ratios, focus tie-breaking, and selection snapping are total and
  deterministic pure functions (see `src/lib.rs`).

## Layout

- `Cargo.toml` — package metadata and the `bitty-term-state` dependency.
- `src/lib.rs` — crate docs with the role, slice contents, and determinism.
- `src/view.rs` — viewport over a snapshot with scroll and reflow helpers.
- `src/layout.rs` — owned layout tree and deterministic solver.
- `src/focus.rs` — leaf focus traversal.
- `src/selection.rs` — ranges, anchoring, and selected text.
- `src/search.rs` — headless search UI state.
- `src/geometry.rs` — integer rect, point, size, and axis types.
- `src/presentation.rs` — per-leaf display mode.
- `src/decoration.rs`, `src/panel.rs`, `src/scrollbar.rs` — chrome helpers.
