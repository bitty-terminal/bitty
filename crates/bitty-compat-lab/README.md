# `bitty-compat-lab`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-compat-lab` is the workspace-member integration point for the headless,
bounded terminal-compatibility lab: it lets `cargo test -p bitty-compat-lab`
and `cargo test --workspace` exercise the lab without relying on
workspace-root test discovery. It re-exports the canonical harness, carries
the release compatibility matrix plus compare and report helpers, and ships
binaries for collecting dumps and rendering reports.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-vt` and
  `bitty-term-state`, plus a dev-dependency on `bitty-pty`; no
  network-facing dependency is declared.
- Does not own the harness source of truth: the canonical harness stays at
  `tests/compat/harness.rs` in the repository root and is re-exported through
  a path module (see `src/lib.rs`).
- Matrix entries map to bounded corpora plus deterministic state hashes; see
  `src/matrix.rs` for the exact shape. No window system, GPU, network, or RNG
  participates.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs, workspace-root path helpers, harness re-export.
- `src/matrix.rs` — release compatibility matrix over surfaces and terminals.
- `src/compare.rs` — differential comparison helpers.
- `src/report.rs` — report rendering helpers.
- `src/bin/collect_dumps.rs` — dump-collection binary.
- `src/bin/compat_report.rs` — report binary.
- `tests/` — lab integration tests (`compat_matrix`, `compare`, `report`,
  `harness`, `live_compat`, `dogfooding_corpus`).
