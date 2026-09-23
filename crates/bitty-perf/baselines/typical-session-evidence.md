---
title: Typical-Session PB-3 Evidence (CTX-0676)
description: Bounded synthetic 8-tab session memory + growth evidence for PB-3 with the committed baseline artifact
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 54
---

<!-- markdownlint-disable MD025 -->

# Typical-Session PB-3 Evidence (CTX-0676)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the PB-3
  typical-session memory harness and its captured baseline. It does not close
  PB-3 and does not claim Verified; the accepted budgets remain arch
  constraints until reference hardware and corpora are pinned (PERF-01,
  OQ-100).
- Ownership: bitty **CTX-0676** — _PERF evidence batch (PB-3 + PB-6)_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1058 (PERF-04, PB-3)
  - RFC: `performance-budget-rfc.md` PB-3 | Task: CTX-0676
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: a bounded, reproducible headless harness that opens 8 real `State`
  tabs, feeds each a fixed 64 KiB mixed workload, samples self RSS
  before/open/close, and checks the 250 MB open budget plus the 15 %
  reclaim budget; a committed evidence artifact with numbers plus host
  context; an explicit `Unavailable` result where `/proc/self` RSS is
  unreadable; and bounded CI contract tests (pure math + provenance only).
  It changes no accepted budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); the product-level
  `docs/product/perf-evidence.md` and `docs/product/perf-baseline.md` rows are
  reconciled through this repository-local record, because the submodule is
  external content owned by another repository.

## What this wave adds

Before this task the repository had:

- `real_soak.rs` / `dogfood_session.rs` — opt-in Tier 1 real-window soak and
  daily-driver automation whose committed artifacts (`pb-real-soak.json`,
  `pb-dogfood-session.json`) both report `unavailable`: no Tier 1 session
  has completed, so no PB-3 number existed anywhere.
- No headless PB-3 anchor at all: CI and workstations had no memory/growth
  signal.

After this task:

- `crates/bitty-perf/src/typical_session.rs` — bounded synthetic 8-tab
  harness (`workload_bytes`, `measure_typical_session`, pure
  growth/reclaim math, `baseline_json` that refuses unmeasured reports).
- `benches/typical_session.rs` — human-facing `harness = false` bench that
  prints the open/reclaim verdicts and exits 0 either way; `--write-baseline`
  commits the artifact and refuses to write when unmeasured (exit 2, no
  fabricated numbers).
- `crates/bitty-perf/baselines/pb-typical-session.json` — committed artifact.
- `crates/bitty-perf/tests/typical_session_evidence.rs` — CI contract
  (determinism, math, artifact provenance; no display).

## Measurement contract

- **Synthetic proxy, bounded.** 8 tabs × 64 KiB fixed workload (shell/SGR/
  cursor/OSC/plain mix, line-repeated, deterministic), fed in 8 KiB chunks
  through a real `Parser` + `State::apply` per tab; close step truncates to
  1 tab. 512 KiB total, ~0.1 s wall. No display, no PTY, no network.
- **Real RSS, not a model.** All three samples are `/proc/self/status`
  `VmRSS` taken in-process; unreadable counters yield `Unavailable`, never a
  number.
- **Reclaim definition.** Post-close RSS within 15 % above the pre-open
  baseline (`reclaim_within_pct <= 15`), matching the "reclaim within 15 %
  after close+GC" budget text as closely as a GC-less language allows (Rust
  has no GC; freed pages stay mapped until the allocator purges — see the
  finding below).
- **Host context from the environment.** OS, arch, toolchain, CPU count, and
  total memory come from `std::env::consts`, `available_parallelism`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
- **`#![forbid(unsafe_code)]`**, Linux-gated for the RSS probe.

## Harness and commands

```text
# Fast path (measure + verdicts, no artifact):
cargo bench -p bitty-perf --bench typical_session -- --nocapture

# Regenerate the committed artifact (provenance from the environment):
BITTY_PERF_TASK=CTX-0676 BITTY_PERF_DATE=... BITTY_PERF_REVISION=... \
  BITTY_PERF_TOOLCHAIN=... BITTY_PERF_COMMAND=... BITTY_PERF_PROFILE=... \
  cargo bench -p bitty-perf --bench typical_session -- --nocapture \
    --write-baseline crates/bitty-perf/baselines/pb-typical-session.json
```

## Captured baseline

Command (exact): the `--write-baseline` line above with
`BITTY_PERF_COMMAND="cargo bench -p bitty-perf --bench typical_session --
--nocapture --write-baseline
crates/bitty-perf/baselines/pb-typical-session.json"`.

Environment for this capture (host context is recorded inside the artifact):

| Field         | Value                                                                      |
| ------------- | -------------------------------------------------------------------------- |
| Revision      | worktree `c70b9f2b00facc4d848ce3f38df1f47a95a423fe`                        |
| Profile       | cargo `bench` release                                                      |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                                     |
| OS            | CachyOS Linux (Arch derivative), x86_64                                    |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe                                   |
| Sample        | 8 tabs × 64 KiB fixed workload (512 KiB total), self-RSS before/open/close |

| Metric                          | Measured   | Budget                  | Verdict        |
| ------------------------------- | ---------- | ----------------------- | -------------- |
| 8-tab open RSS                  | 69.039 MB  | ≤ 250 MB                | `PASS`         |
| Growth over baseline (2.754 MB) | +66.285 MB | — (anchor only)         | —              |
| Post-close RSS (1 tab)          | 54.184 MB  | within 15 % of baseline | `ABOVE_BUDGET` |
| Reclaim distance                | +1867.5 %  | ≤ +15 %                 | `ABOVE_BUDGET` |

Repeat runs are stable (open 68.98–69.04 MB, close 54.13–54.18 MB across 4
runs); the growth is dominated by per-tab `State` footprint (~8.3 MB/tab of
grid/scrollback structures for a 64 KiB workload), not by the workload bytes.

Interpretation, stated honestly:

- Open **passes with ~72 % headroom** on this host — but this is a 64 KiB/tab
  synthetic, not a 4 h mixed session; real scrollback accumulation, images,
  and plugin state are outside this window. It is a regression anchor, not a
  compliance claim.
- Reclaim **fails under the strict RSS reading, by allocator design**: Rust
  has no GC and the allocator retains freed pages in-process, so dropping 7
  of 8 tabs moves VmRSS only 69 → 54 MB instead of back toward the 2.8 MB
  baseline. The 15 % reclaim budget as literally specified (VmRSS after
  close+GC) is not achievable without an explicit purge or a logical-heap
  (rather than RSS) definition. This is a finding for PERF-01, not a product
  defect: the budget needs an allocator-aware reclaim definition
  (e.g. `mallinfo`/`malloc_info` logical heap, or an explicit
  purge-then-sample protocol) before it can gate.
- The 8-tab real-session measurement stays deferred per #1058; the Tier 1
  soak/dogfood automation (`pb-real-soak.json`, `pb-dogfood-session.json`)
  remains the vehicle for it.

## Verification

```text
cargo test -p bitty-perf --lib -- typical_session           # unit math
cargo test -p bitty-perf --test typical_session_evidence    # CI contract
cargo bench -p bitty-perf --bench typical_session -- --nocapture
```

Expected: unit + contract tests pass with no display; the bench prints
`PASS` open and the reclaim verdict on Linux, `UNMEASURED` with a reason
where `/proc/self` RSS is unreadable.

## Limitations and confidence

- Single-host capture; no multi-machine distribution and no variance study
  (4 repeat runs shown above are stability color, not statistics).
- Synthetic 64 KiB/tab workload: exercises parser + state + allocation, not
  a 4 h session's scrollback growth, images, or plugins.
- Self-RSS of the bench process includes harness overhead; the baseline is
  tiny (2.8 MB), so relative percents are dramatic — read absolutes.
- Cross-platform claims require CI or reference-hardware evidence on the named
  platform; this host proves nothing for other platforms.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule
(`docs/product/perf-evidence.md` and `docs/product/perf-baseline.md`). This
repository cannot edit that external content in this change; the
reconciliation is: this document plus
`crates/bitty-perf/baselines/pb-typical-session.json` are the repository-local
PB-3 evidence, and a follow-up in the docs repository should link the new
artifact and record the open-PASS / reclaim-finding split. No row is claimed
Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. The PB-3 accepted
values in `performance-budget-rfc.md` are untouched (PERF-01/OQ-100 owns the
decision to turn them into hard gates, including the allocator-aware reclaim
definition this finding calls for).

## References

- `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`
- `crates/bitty-perf/baselines/pb-typical-session.json` (committed artifact)
- `crates/bitty-perf/src/typical_session.rs` (harness)
- `benches/typical_session.rs` (human-facing bench)
- Issue #1058 (PERF-04)
