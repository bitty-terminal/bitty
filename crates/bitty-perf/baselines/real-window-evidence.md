---
title: Real-Window PB-1/PB-2 Evidence (CTX-0592)
description: Opt-in Tier 1 real-window harness and captured baseline for PB-1 cold startup (launch-to-first-frame p50/p99) and PB-2 idle RSS, with headless-CI Unavailable behavior
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 52
---

<!-- markdownlint-disable MD025 -->

# Real-Window PB-1/PB-2 Evidence (CTX-0592)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the real-window
  harness and its captured baseline. It does not close PB-1/PB-2 and does not
  claim Verified; the accepted budgets remain arch constraints until
  reference hardware and corpora are pinned (PERF-01, OQ-100).
- Ownership: bitty **CTX-0592** — _PB-1 startup + PB-2 idle-memory evidence
  harness_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issues: #1056 (PERF-02, PB-1), #1057 (PERF-03, PB-2)
  - RFC: `performance-budget-rfc.md` PB-1/PB-2 | Task: CTX-0592
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: an opt-in, bounded, reproducible harness that measures PB-1 and PB-2
  on a Tier 1 host; a committed evidence artifact with numbers plus host
  context; an explicit `Unavailable` result on headless CI; and bounded CI
  contract tests. It changes no accepted budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); the product-level
  `docs/product/perf-evidence.md` and `docs/product/perf-baseline.md` rows are
  reconciled through this repository-local record plus the PR notes below,
  because the submodule is external content owned by another repository.

## What this wave adds

Before this task the repository had:

- `crates/bitty-perf/src/startup.rs` — headless phase tracing plus a
  `StartupDistribution`, but `measure_real_window_startup` only validated
  `WindowConfig`/`EventLoop` and never measured a real first frame, never
  aggregated p50/p99 over launches, and committed no PB-1 baseline.
- `tools/perf/rss` — a bash proxy that sampled `cargo run --help` and the
  shell, with soft verdicts; no in-crate PB-2 measurement.

After this task:

- `crates/bitty-perf/src/real_window.rs` — real-window PB-1/PB-2 measurement.
- `benches/real_window.rs` — human-facing `harness = false` bench.
- `crates/bitty-perf/tests/real_window_evidence.rs` — bounded CI contract test.
- `crates/bitty-perf/baselines/pb-real-window.json` — committed artifact.
- `crates/bitty-app` emits an opt-in `bitty perf: first-frame` stdout marker
  (gated on `BITTY_PERF_STARTUP_MARKER`) after the first frame presents.

## Measurement contract

- **Opt-in.** A run requires `BITTY_PERF_REAL_WINDOW=1` and a resolvable
  `bitty` binary (`BITTY_PERF_BIN`, else the discovered `target/release`,
  then `target/debug`). Without either the harness returns `Unavailable` with
  a reason and writes nothing; headless CI reports `Unavailable` by design and
  remains green.
- **No fabricated numbers.** `--write-baseline` refuses to write when either
  measurement is unavailable (exit 2); the committed artifact carries either
  measured positive numbers or an explicit `unavailable` status.
- **Real binary, not a proxy.** PB-1 is the elapsed time from spawning the
  real `bitty` process to the `bitty perf: first-frame` marker. PB-2 is the
  child's `VmRSS` (or `ps -o rss=`) sampled across the idle window.
- **Bounded and deterministic.** Sample count, idle window, and per-launch
  timeout are clamped constants (`MAX_STARTUP_SAMPLES` 50, `MAX_IDLE_SECS`
  300, `MAX_STARTUP_TIMEOUT_SECS` 120); every child is killed and reaped on
  every path; the stdout reader is a bounded line pump behind a timeout.
- **Host context from the environment.** OS, arch, toolchain, CPU count, and
  total memory come from `std::env::consts`, `available_parallelism`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
- **`#![forbid(unsafe_code)]`**, Unix-gated (other platforms report
  `Unavailable`).

## Harness and commands

```text
# On a Tier 1 host with a display and a built binary:
BITTY_PERF_REAL_WINDOW=1 cargo bench -p bitty-perf --bench real_window -- --nocapture
# or:
just perf-real-window

# Regenerate the committed artifact (provenance from the environment):
BITTY_PERF_DATE=... BITTY_PERF_REVISION=... BITTY_PERF_TOOLCHAIN=... \
  just perf-real-window-baseline
```

Environment knobs (all optional, clamped):

| Variable                          | Default | Bound | Purpose                               |
| --------------------------------- | ------- | ----- | ------------------------------------- |
| `BITTY_PERF_REAL_WINDOW`          | unset   | —     | `1` opts into real-window measurement |
| `BITTY_PERF_BIN`                  | unset   | —     | Explicit binary path                  |
| `BITTY_PERF_STARTUP_SAMPLES`      | 5       | ≤ 50  | PB-1 launches sampled                 |
| `BITTY_PERF_IDLE_SECS`            | 5       | ≤ 300 | PB-2 idle window                      |
| `BITTY_PERF_STARTUP_TIMEOUT_SECS` | 20      | ≤ 120 | Per-launch first-frame timeout        |

## Captured baseline

Command (exact): `just perf-real-window-baseline` (equivalently the
`BITTY_PERF_REAL_WINDOW=1 cargo bench ... --write-baseline` line above).

Environment for this capture (host context is recorded inside the artifact):

| Field         | Value                                                               |
| ------------- | ------------------------------------------------------------------- |
| Revision      | base `main` `6fd9d4d1f399565cc07ea228e5f43335d0cb52f5` (pre-change) |
| Profile       | cargo `bench` release                                               |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                              |
| OS            | CachyOS Linux (Arch derivative), Wayland (Hyprland), x86_64         |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe                            |
| Sample        | 10 launches (PB-1); one session, 60 s idle, 3 RSS samples (PB-2)    |

| Metric            | Measured | Budget   | Verdict        |
| ----------------- | -------- | -------- | -------------- |
| PB-1 p50          | 227.8 ms | ≤ 100 ms | `ABOVE_BUDGET` |
| PB-1 p99          | 257.0 ms | ≤ 200 ms | `ABOVE_BUDGET` |
| PB-2 idle RSS p50 | 426.3 MB | ≤ 80 MB  | `ABOVE_BUDGET` |

A second 10-launch run on the same host (recorded under
`recording/ctx-0592/evidence-capture.md`) measured p50 254.2 ms / p99
410.7 ms and RSS p50 431.3 MB, consistent with the committed artifact; the
spread (`~210-410 ms`) shows why the opt-in regression factor is deliberately
loose on this shared workstation.

Interpretation, stated honestly:

- These are **measurements of the current vertical slice**, not a
  budget-compliance claim. Both metrics are above the accepted budgets on this
  host. The PB-1 dominant cost is cold GPU/wgpu initialization on a real
  surface (consistent with the CTX-0100 headless finding that
  `wgpu_init_probe` dominates); the PB-2 reading reflects the current
  single-process slice's resident set after a real window is up.
- The numbers are a **regression anchor on one machine**, not a normalized
  score, and not a cross-platform claim. PB-1/PB-2 compliance is decided on
  pinned Tier 1 reference hardware once PERF-01/OQ-100 lands.
- The harness is the deliverable; the above-budget verdicts are honest
  findings that should feed the PERF-01 budget/harness decision.

## Regression threshold

The opt-in bench compares against the committed baseline with a generous
`REGRESSION_FACTOR` of 3× (p50 / RSS) so a noisy shared workstation does not
flake. It is informational on the bench path; the CI contract test only
asserts the `Unavailable`/provenance discipline and the pure metric math, so
CI never depends on a real display or a loaded GPU.

## Verification

```text
cargo test -p bitty-perf --test real_window_evidence --locked   # Unavailable/provenance contract
cargo test -p bitty-perf --lib --locked                        # metric math + baseline provenance
cargo bench --no-run                                           # real_window bench compiles headlessly
just check                                                     # fmt + clippy -D warnings + test + lint
BITTY_PERF_REAL_WINDOW=1 just perf-real-window                 # Tier 1 only (Unmeasured on CI)
```

Expected: on headless CI the bench prints `UNMEASURED` and exits 0; the
contract test passes with no display.

## Limitations and confidence

- Single-host capture; no multi-machine p50/p99 and no variance study.
- PB-2 samples one window after a bounded idle interval (60 s in the capture),
  not a 4 h soak; PB-3 typical-session memory remains separate.
- The harness launches the process and reads a first-frame marker; it does not
  attribute startup time to individual phases (that is what
  `src/startup.rs`/`benches/startup_real.rs` do headlessly).
- Cross-platform claims require CI or reference-hardware evidence on the named
  platform; this host proves nothing for other platforms.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule
(`docs/product/perf-evidence.md` CTX-0100 and `docs/product/perf-baseline.md`
CTX-0076). This repository cannot edit that external content in this PR; the
reconciliation is: this document plus
`crates/bitty-perf/baselines/pb-real-window.json` are the repository-local
PB-1/PB-2 real-window evidence, and a follow-up in the docs repository should
link the new artifact and record the above-budget finding. No row is claimed
Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. PB-1/PB-2 accepted
values in `performance-budget-rfc.md` are untouched (PERF-01/OQ-100 owns the
decision to turn them into hard gates).

## References

- `docs/specifications/performance-budget-rfc.md#pb-1-cold-startup-time`
- `docs/specifications/performance-budget-rfc.md#pb-2-idle-memory`
- `crates/bitty-perf/baselines/pb-real-window.json` (committed artifact)
- `crates/bitty-perf/baselines/README.md` (parser-throughput runbook sibling)
- `crates/bitty-perf/src/real_window.rs` (harness)
- `benches/real_window.rs` (human-facing bench)
