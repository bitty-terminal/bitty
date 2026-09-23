---
title: Throughput-Floor PB-6 Evidence (CTX-0676)
description: Bounded fixed-corpus sustained parse-and-render evidence for the PB-6 floor with the committed baseline artifact
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 55
---

<!-- markdownlint-disable MD025 -->

# Throughput-Floor PB-6 Evidence (CTX-0676)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the PB-6
  parse-and-render throughput harness and its captured baseline. It does not
  close PB-6 and does not claim Verified; the accepted budgets remain arch
  constraints until reference hardware and corpora are pinned (PERF-01,
  OQ-100).
- Ownership: bitty **CTX-0676** — _PERF evidence batch (PB-3 + PB-6)_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1061 (PERF-07, PB-6)
  - RFC: `performance-budget-rfc.md` PB-6 | Task: CTX-0676
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: a bounded, reproducible headless harness that measures sustained
  single-core VT parse → state-apply → render throughput over a fixed
  synthetic corpus (median of 3 rounds, 1 MiB/round); a committed evidence
  artifact with numbers plus host context; and bounded CI contract tests
  (pure math + provenance + a 64 KiB headless smoke). It changes no accepted
  budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); the product-level
  `docs/product/perf-evidence.md` and `docs/product/perf-baseline.md` rows are
  reconciled through this repository-local record, because the submodule is
  external content owned by another repository.

## What this wave adds

Before this task the repository had:

- `parser_throughput.rs` + `parser-throughput.json` (M1-11, CTX-0576) — the
  **parser stage only** (`Parser::advance`, 108–177 MiB/s on this host
  class), explicitly not a PB-6 compliance claim.
- `benches/terminal_state.rs` — an "8 KiB-equivalent apply throughput" proxy
  (~10.8 MiB/s on this host) printed next to the 40 floor, but committed
  nowhere.
- No committed parse-**and**-render number: PB-6's actual budgeted path had
  no anchor.

After this task:

- `crates/bitty-perf/src/throughput_floor.rs` — fixed-corpus
  parse→apply→render harness (`corpus_segment`, `measure`, median math,
  `baseline_json`).
- `benches/throughput_floor.rs` — human-facing `harness = false` bench that
  prints per-round/median MiB/s plus the floor verdict and exits 0 either
  way; `--write-baseline` commits the artifact and exits 2 only when the
  measurement itself failed (no fabricated numbers).
- `crates/bitty-perf/baselines/pb-throughput-floor.json` — committed artifact.
- `crates/bitty-perf/tests/throughput_floor_evidence.rs` — CI contract
  (determinism, math, headless smoke, artifact provenance).

## Measurement contract

- **Fixed synthetic corpus.** `corpus_segment` builds a deterministic 64 KiB
  segment in code (plain runs + SGR color + cursor motion + erase + OSC
  title); the measured buffer repeats it to 1 MiB. No file discovery, no
  network, no RNG. Final-round action density: 748 800 actions/MB
  (~0.71 actions/byte) — escape-leaning, i.e. the favorable side for an
  apply-bound path (fewer actions per byte than plain text).
- **The full budgeted path, per batch.** Every 8 KiB chunk runs parse →
  `State::apply` per action → `snapshot` + per-batch-delta `damage_since` →
  `GridRenderer::render` (fake rasterizer, no GPU), exactly like a terminal
  presenting each PTY batch. Single core, median of 3 measured rounds with a
  fresh `State`/`Parser`/renderer per round.
- **Honest floor.** The verdict compares the median against the accepted
  40 MiB/s floor and prints `PASS`/`ABOVE_BUDGET`; the bench never fails on
  the budget (Tier 1 reference hardware gates, not this host).
- **Host context from the environment.** OS, arch, toolchain, CPU count, and
  total memory come from `std::env::consts`, `available_parallelism`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
- **`#![forbid(unsafe_code)]`**, headless (no `winit::Window`, no
  `wgpu::Surface`).

## Harness and commands

```text
# Fast path (measure + verdict, no artifact):
cargo bench -p bitty-perf --bench throughput_floor -- --nocapture

# Regenerate the committed artifact (provenance from the environment):
BITTY_PERF_TASK=CTX-0676 BITTY_PERF_DATE=... BITTY_PERF_REVISION=... \
  BITTY_PERF_TOOLCHAIN=... BITTY_PERF_COMMAND=... BITTY_PERF_PROFILE=... \
  cargo bench -p bitty-perf --bench throughput_floor -- --nocapture \
    --write-baseline crates/bitty-perf/baselines/pb-throughput-floor.json
```

## Captured baseline

Command (exact): the `--write-baseline` line above with
`BITTY_PERF_COMMAND="cargo bench -p bitty-perf --bench throughput_floor --
--nocapture (sample_bytes=1048576 rounds=3)"`.

Environment for this capture (host context is recorded inside the artifact):

| Field         | Value                                                                          |
| ------------- | ------------------------------------------------------------------------------ |
| Revision      | worktree `c70b9f2b00facc4d848ce3f38df1f47a95a423fe`                            |
| Profile       | cargo `bench` release                                                          |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                                         |
| OS            | CachyOS Linux (Arch derivative), x86_64                                        |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe                                       |
| Sample        | 1 MiB/round × 3 rounds, median reported; 128 renders/round (1 per 8 KiB chunk) |

| Metric                     | Measured              | Budget     | Verdict        |
| -------------------------- | --------------------- | ---------- | -------------- |
| Sustained parse-and-render | **6.06 MiB/s** median | ≥ 40 MiB/s | `ABOVE_BUDGET` |
| Rounds                     | 5.94, 6.06, 6.20      | —          | tight (±2 %)   |

Stage breakdown on the same host (release, 1 MiB): parser alone ~71 MiB/s
(14 ms), parse+apply ~6.5 MiB/s (155 ms) — `State::apply` dominates and
render-per-batch-delta is negligible by comparison. The independent
`terminal_state` bench agrees (~10.8 MiB/s apply proxy, ~9.5 µs per Print).
The limiter is per-action batch finalization: `State::apply` runs
dispatch + `finalize_batch` (damage coalescing) once per action, and the
runtime hot path has no batch-apply API to amortize it (the only callers
apply one action at a time).

Interpretation, stated honestly:

- The median is **~6.6× below the 40 MiB/s floor on this workstation-class
  host**. The parser is not the problem (70+ MiB/s alone on this corpus);
  the budgeted parse-and-render path is apply-bound.
- This is a **finding, not a gate failure**: PB-6 budgets the slowest Tier 1
  reference machine and PERF-01/OQ-100 has not pinned it; a workstation-class
  miss this large indicates the floor as specified needs either a batch-apply
  fast path (amortize `finalize_batch` across a PTY chunk) or a re-scoped
  floor — a PERF-01 decision, recorded here as evidence.
- The numbers are a **regression anchor on one machine**, not a normalized
  score and not a cross-platform claim. Earlier spot rounds read 6.5–6.9
  MiB/s under lighter load; expect ±10 % run variance on shared hosts.

## Verification

```text
cargo test -p bitty-perf --lib -- throughput_floor          # unit math + headless smoke
cargo test -p bitty-perf --test throughput_floor_evidence   # CI contract
cargo bench -p bitty-perf --bench throughput_floor -- --nocapture
```

Expected: unit + contract tests pass with no display; the bench prints the
per-round/median line and the floor verdict.

## Limitations and confidence

- Single-host capture; no multi-machine distribution and no variance study
  (spot re-runs noted above are stability color, not statistics).
- Fake rasterizer, no GPU: render is not the limiter here, so GPU-backed
  numbers would move the total only slightly — but that claim needs a
  windowed measurement to confirm.
- One fixed corpus: escape-leaning density is favorable to an apply-bound
  path; plain-text (1 action/byte) would read lower per byte. Corpus breadth
  (seed/compat reuse like the parser baseline) is future work, owned by
  PERF-01 corpus pinning.
- Cross-platform claims require CI or reference-hardware evidence on the named
  platform; this host proves nothing for other platforms.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule
(`docs/product/perf-evidence.md` and `docs/product/perf-baseline.md`). This
repository cannot edit that external content in this change; the
reconciliation is: this document plus
`crates/bitty-perf/baselines/pb-throughput-floor.json` are the repository-local
PB-6 evidence, and a follow-up in the docs repository should link the new
artifact and record the below-floor finding. No row is claimed Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. The PB-6 accepted
value in `performance-budget-rfc.md` is untouched (PERF-01/OQ-100 owns the
floor's fate: batch-apply fast path or re-scope).

## References

- `docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`
- `crates/bitty-perf/baselines/pb-throughput-floor.json` (committed artifact)
- `crates/bitty-perf/baselines/parser-throughput.json` (parser-stage only, M1-11)
- `crates/bitty-perf/src/throughput_floor.rs` (harness)
- `benches/throughput_floor.rs` (human-facing bench)
- `benches/terminal_state.rs` (independent apply proxy, ~10.8 MiB/s)
- Issue #1061 (PERF-07)
