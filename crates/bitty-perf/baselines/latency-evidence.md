---
title: Input-Latency PB-4 Evidence (CTX-0686)
description: Bounded headless key-to-screen p50/p99 evidence for PB-4 with the committed baseline artifact
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 56
---

<!-- markdownlint-disable MD025 -->

# Input-Latency PB-4 Evidence (CTX-0686)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the PB-4
  key-to-screen latency harness and its captured baseline. It does not close
  PB-4 and does not claim Verified; the accepted budgets remain arch
  constraints until reference hardware and corpora are pinned (PERF-01,
  OQ-100).
- Ownership: bitty **CTX-0686** — _PERF leftover batch (PB-4 + PB-01)_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1059 (PERF-05, PB-4)
  - RFC: `performance-budget-rfc.md` PB-4 | Task: CTX-0686
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: a bounded, reproducible headless harness that measures the full
  `keydown → PTY → parser → state → render → present` path with per-stage
  tracing and p50/p99/mean/max over 1,000 samples; a committed evidence
  artifact with numbers plus host context; an explicit refusal to write when
  no sample presents (exit 2, no fabricated numbers); and bounded CI contract
  tests (exact boundary math + provenance only). It changes no accepted
  budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); the product-level
  `docs/product/perf-evidence.md` and `docs/product/perf-baseline.md` rows are
  reconciled through this repository-local record, because the submodule is
  external content owned by another repository.

## What this wave adds

Before this task the repository had:

- `crates/bitty-perf/src/latency.rs` — the CTX-0100 tracer (`measure_latency`,
  `measure_latency_with_pty_echo`, work-floor vs wall-clock split, CTX-0494 /
  CTX-0500 shared-runner probes) with unit tests but no committed artifact.
- `benches/latency_real.rs` — printed p50/p99 plus the bench-gated sanity
  asserts but committed no numbers anywhere.

After this task:

- `crates/bitty-perf/src/latency.rs` — `baseline_json` (schema 1, refuses
  zero-presented reports) plus `committed_latency_json` and the
  `LATENCY_BASELINE_REL_PATH` / `LATENCY_BASELINE_SAMPLES` constants.
- `benches/latency_real.rs` — human-facing `harness = false` bench that
  prints both variants plus the sanity verdicts and exits 0 either way;
  `--write-baseline` commits the 1,000-sample headless report and refuses to
  write when unmeasured (exit 2, no fabricated numbers).
- `crates/bitty-perf/baselines/pb-latency.json` — committed artifact.
- `crates/bitty-perf/tests/latency_evidence.rs` — CI contract (exact boundary
  math, refusal path, artifact provenance; live leg is 8 headless samples).

## Measurement contract

- **Full hot path, stage-traced.** Every sample timestamps encode (≤64 B),
  `handle_key_event`, PTY→parser→state, and render→present; the sum is the
  pipeline *work*, the `Instant` span is wall clock. Statistics are
  p50/p99/mean/max over presented samples only — never a single point.
- **Two paths, honestly labeled.** The committed capture is the deterministic
  injected-echo model (`mode=injected-echo`, 1,000/1,000 synthetic); the bench
  also drives the real-`cat` PTY variant, which labels itself `real-pty-echo`
  only when real echo bytes actually arrived (32/200 synthetic fell back on
  this capture). A silent fallback is never read as real-PTY evidence.
- **Headless seam, stated.** Present is `Surface::headless_present`; no
  display server or compositor timestamp participates. The 60 Hz
  frame-presented budget text is gated on Tier 1, not on this host.
- **No fabricated numbers.** Zero presented samples → `Err` → bench exit 2.
- **Bounded.** 1,000-sample primary, ≤64 B per key; no display, no network.
- **Host context from the environment.** OS, arch, toolchain, CPU count, and
  total memory come from `std::env::consts`, `available_parallelism`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
- **`#![forbid(unsafe_code)]`** throughout.

## Harness and commands

```text
# Fast path (measure + verdicts, no artifact):
cargo bench -p bitty-perf --bench latency_real -- --nocapture

# Regenerate the committed artifact (provenance from the environment):
BITTY_PERF_TASK=CTX-0686 BITTY_PERF_DATE=... BITTY_PERF_REVISION=... \
  BITTY_PERF_TOOLCHAIN=... BITTY_PERF_COMMAND=... BITTY_PERF_PROFILE=... \
  cargo bench -p bitty-perf --bench latency_real -- --nocapture \
    --write-baseline crates/bitty-perf/baselines/pb-latency.json
```

## Captured baseline

Command (exact): the `--write-baseline` line above with
`BITTY_PERF_COMMAND="cargo bench -p bitty-perf --bench latency_real --
--nocapture --write-baseline
crates/bitty-perf/baselines/pb-latency.json"`.

Environment for this capture (host context is recorded inside the artifact):

| Field         | Value                                                                                |
| ------------- | ------------------------------------------------------------------------------------ |
| Revision      | worktree `13d555649b47e07bf56a0d467ffb5332b3f86396`                                   |
| Profile       | cargo `bench` release                                                                |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                                               |
| OS            | CachyOS Linux (Arch derivative), x86_64                                              |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe                                             |
| Sample        | 1,000 headless injected-echo samples, all presented                                  |

| Metric                          | Measured       | Budget                 | Verdict  |
| ------------------------------- | -------------- | ---------------------- | -------- |
| Wall p50 (key-to-screen)        | 1.654 ms       | ≤ 8 ms                 | `PASS`   |
| Wall p99                        | 3.833 ms       | ≤ 15 ms                | `PASS`   |
| Wall mean / max                 | 1.657 / 9.845 ms | —                    | anchor   |
| Work p50 / p99 / min            | 1.653 / 3.833 / 0.414 ms | same budgets   | `PASS`   |
| Real-PTY variant (secondary)    | p50 0.446 / p99 0.654 ms (`real-pty-echo`) | same budgets | `PASS` |

Interpretation, stated honestly:

- The headless pipeline **passes with ~79 % p50 and ~74 % p99 headroom on
  this host** — but this is the software present seam, not a 60 Hz
  frame-presented measurement; compositor-vs-headless delta is unmeasured
  here. It is a regression anchor, not a compliance claim.
- Wall and work percentiles coincide (1.654 vs 1.653 ms p50): on this
  unloaded host scheduler gaps contribute ~nothing; on a loaded shared runner
  they dominate, which is why the artifact carries both series and the exact
  budget verdicts stay pinned by `pb4_work_budget_classification_is_exact`.
- The single 9.845 ms max is one slow first-present, inside the 1 %
  allowance the n=200 p99 estimator was sized for (#659) — at n=1,000 the p99
  excludes the 10 worst samples.
- The real-PTY secondary leg confirms the injected-echo model is not
  optimistic: real `cat` echo through `poll_pty` reads *faster* (0.446 ms
  p50) than the synthetic path on this host.

## Verification

```text
cargo test -p bitty-perf --lib -- latency               # unit math + probes
cargo test -p bitty-perf --test latency_evidence         # CI contract
cargo bench -p bitty-perf --bench latency_real -- --nocapture
```

Expected: unit + contract tests pass with no display; the bench prints
`PASS p50+p99` on a quiet workstation and the sanity line on CI.

## Limitations and confidence

- Single-host capture; no multi-machine distribution and no variance study
  (one 1,000-sample run shown above).
- Headless software present: no GPU, no compositor, no 60 Hz frame pacing —
  the budgeted frame-presented number needs a windowed Tier 1 run.
- Synthetic key mix (printable + Enter/Backspace/ArrowRight), no real shell
  workload behind the PTY in the committed leg.
- Cross-platform claims require CI or reference-hardware evidence on the named
  platform; this host proves nothing for other platforms.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule
(`docs/product/perf-evidence.md` and `docs/product/perf-baseline.md`). This
repository cannot edit that external content in this change; the
reconciliation is: this document plus
`crates/bitty-perf/baselines/pb-latency.json` are the repository-local PB-4
evidence, and a follow-up in the docs repository should link the new artifact
and record the headless-PASS / Tier-1-pending split. No row is claimed
Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. The PB-4 accepted
values in `performance-budget-rfc.md` are untouched (PERF-01/OQ-100 owns the
decision to turn them into hard gates, including the frame-presented Tier 1
protocol this finding calls for).

## References

- `docs/specifications/performance-budget-rfc.md#pb-4-input-latency`
- `crates/bitty-perf/baselines/pb-latency.json` (committed artifact)
- `crates/bitty-perf/src/latency.rs` (harness)
- `benches/latency_real.rs` (human-facing bench)
- Issue #1059 (PERF-05)
