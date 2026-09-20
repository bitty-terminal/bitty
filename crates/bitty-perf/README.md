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
- `src/parser_throughput.rs` — parser-throughput baseline measurement and
  ratio gate (CTX-0576, M1-11); corpora loading, median-of-rounds
  measurement, baseline parsing, and the regression check.
- `baselines/parser-throughput.json` — committed baseline artifact (numbers
  plus provenance); `baselines/README.md` records the runbook, exact command,
  environment, and limitations.
- `tests/parser_throughput_regression.rs` — bounded CI regression gate run by
  plain `cargo test` (also on the optimized `bench` profile via
  `just perf-parser`).

## Parser throughput baseline (CTX-0576, M1-11)

`src/parser_throughput.rs` measures `bitty_vt::Parser::advance` in isolation
over reused deterministic corpora (`bitty-vt` seeds, `tests/compat/*/corpus`,
a synthetic escape storm) and compares the escape/plain throughput ratios
against `baselines/parser-throughput.json`. The gate is generous
(4× ratio collapse) so shared runners and debug `cargo test` builds do not
flake; it catches pathological regressions only. Run `just perf-parser` for
the optimized verdict and `just perf-parser-baseline` to regenerate the
artifact after a recorded environment change.
