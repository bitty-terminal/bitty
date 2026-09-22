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
- `src/real_window.rs` — real-window PB-1 startup (launch-to-first-frame
  p50/p99) and PB-2 idle-RSS measurement (CTX-0592); opt-in, bounded,
  `Unavailable` without `BITTY_PERF_REAL_WINDOW=1`.
- `src/real_soak.rs` — long-duration real-render soak automation (CTX-0642,
  PERF-09): clamped soak config, bounded capture plan, `hyprctl` + `grim`
  leg probing, RSS trend helpers, and schedule/evidence JSON builders;
  opt-in, bounded, `Unavailable` without `BITTY_PERF_REAL_SOAK=1`.
- `baselines/parser-throughput.json` — committed baseline artifact (numbers
  plus provenance); `baselines/README.md` records the runbook, exact command,
  environment, and limitations.
- `baselines/pb-real-window.json` — committed real-window evidence artifact
  (PB-1/PB-2 numbers plus host context and provenance);
  `baselines/real-window-evidence.md` records the runbook and limitations.
- `baselines/pb-real-soak.json` — committed long-duration soak artifact
  (PB-3/PB-7 numbers plus host context and provenance, `unavailable` until
  the first Tier 1 run is promoted);
  `baselines/real-soak-evidence.md` records the automation runbook,
  scheduling, and limitations.
- `tests/parser_throughput_regression.rs` — bounded CI regression gate run by
  plain `cargo test` (also on the optimized `bench` profile via
  `just perf-parser`).
- `tests/real_window_evidence.rs` — bounded CI contract test for the
  real-window harness (asserts `Unavailable` without opt-in and baseline
  provenance).
- `tests/real_soak_evidence.rs` — bounded CI contract test for the soak
  planner (asserts gate/leg discipline, plan math, and artifact
  provenance); the script chain is covered headlessly by
  `scripts/tests/real-render-soak.test.sh` (`just real-render-soak-test`).

## Parser throughput baseline (CTX-0576, M1-11)

`src/parser_throughput.rs` measures `bitty_vt::Parser::advance` in isolation
over reused deterministic corpora (`bitty-vt` seeds, `tests/compat/*/corpus`,
a synthetic escape storm) and compares the escape/plain throughput ratios
against `baselines/parser-throughput.json`. The gate is generous
(4× ratio collapse) so shared runners and debug `cargo test` builds do not
flake; it catches pathological regressions only. Run `just perf-parser` for
the optimized verdict and `just perf-parser-baseline` to regenerate the
artifact after a recorded environment change.

## Real-window PB-1/PB-2 evidence (CTX-0592)

`src/real_window.rs` launches the real `bitty` binary and measures the accepted
budgets end-to-end: PB-1 cold startup (process launch to the first presented
frame, p50/p99) and PB-2 idle RSS (one window, bounded idle interval). It is
opt-in — `BITTY_PERF_REAL_WINDOW=1` plus a built binary — and reports
`Unavailable` with a reason otherwise, so headless CI never fabricates numbers.
Run `just perf-real-window` on a Tier 1 host and `just
perf-real-window-baseline` to regenerate the artifact; see
`baselines/real-window-evidence.md` for the runbook and limitations.

## Long-duration real-render soak (CTX-0642, PERF-09)

`src/real_soak.rs` plus `scripts/real-render-soak.sh` automate the
previously manual `hyprctl` + `grim` capture leg as a schedulable soak: a
real window runs for hours while each capture pairs a `grim` screenshot
with a DevTools-preferred `bitty ctl terminal text` snapshot and an RSS
sample for the PB-3 anchor. Planning (`just perf-real-soak`,
`--dry-run`) is headless-safe; live runs need `BITTY_PERF_REAL_SOAK=1`
and a Hyprland session, otherwise they report `Unavailable` and exit 2.
See `baselines/real-soak-evidence.md` for the runbook, scheduling, and
the committed `baselines/pb-real-soak.json` shape.
