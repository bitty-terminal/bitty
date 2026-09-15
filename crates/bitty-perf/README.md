# `bitty-perf`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-perf` owns the performance baseline harness: it hosts the
workspace-root `benches/` targets so `cargo bench` compiles while the
workspace stays virtual, and it carries probe instrumentation covering the
`bitty-app` cold path plus input latency and idle behavior. Everything is
headless, bounded, and `forbid(unsafe_code)`; budget definitions live in the
referenced performance RFC and evidence notes, not here (see `src/lib.rs`).

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-vt`,
  `bitty-term-state`, `bitty-render`, `bitty-platform`, `bitty-pty`,
  `bitty-runtime`, `bitty-config`, and `bitty-ui`.
- Third-party dependencies, per `Cargo.toml`: `pollster` and `winit`, the
  latter only for bounded real-window probes; no network-facing dependency is
  declared.
- Bench targets live at `benches/*.rs` in the repository root, not in this
  crate directory.
- Display-tied phases report `Unavailable` with their attempt duration on
  headless CI rather than requiring a display (see `src/startup.rs`).

## Layout

- `Cargo.toml` — package metadata, dependencies, and bench target wiring.
- `src/lib.rs` — crate docs, budget pointers, and the headless witness.
- `src/startup.rs` — cold-path phase instrumentation.
- `src/latency.rs` — input-latency stage breakdown.
- `src/idle.rs` — frame-on-demand idle gating.
