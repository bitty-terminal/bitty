---
title: PB Harness, Corpora, and Reference-Machine Pin (CTX-0686)
description: Pinned inventory of the PB-1..PB-7 evidence harnesses, fixed corpora, and capture machines for PERF-01; gating stays blocked on OQ-100
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 57
---

<!-- markdownlint-disable MD025 -->

# PB Harness, Corpora, and Reference-Machine Pin (CTX-0686)

## Status and provenance

- Status: **draft pin record**. Repository-owned inventory of which harness
  owns each PB, which corpora are fixed, and which machines have captured
  evidence. It pins the *means of measurement* so future captures are
  comparable. It does **not** turn any budget into a hard gate and does not
  claim Verified.
- Ownership: bitty **CTX-0686** — _PERF leftover batch (PB-4 + PB-01)_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1055 (PERF-01, pin PB harness, corpora, reference machines)
  - RFC: `performance-budget-rfc.md`; OQ-100 (open) | Task: CTX-0686
  - **Blocked:** the *gating* half of PERF-01 awaits an owner decision
    ([BLOCKED: OQ-100]); this wave pins the harness/corpora/machines record
    only and changes no gate.
- Scope: one inventory table (PB → module → bench → artifact → evidence doc
  → capture status), the fixed-corpora definitions, the capture-machine
  record, and a CI contract test that keeps the table honest (every listed
  artifact exists, every artifact is provenanced). It changes no accepted
  budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); this file is the
  repository-local pin record PERF-01 asks for.

## Harness inventory (pinned)

Every PB has exactly one owning module, one human-facing bench, one committed
artifact, and one evidence doc. All harnesses are headless, bounded, and
`#![forbid(unsafe_code)]`; every `--write-baseline` refuses unmeasured input
with exit 2 and every provenance block comes from the environment (no checkout
path, username, or hostname is embedded — enforced by contract tests).

| PB | Budget (accepted target) | Module | Bench | Artifact | Evidence doc | Capture status |
| -- | ------------------------ | ------ | ----- | -------- | ------------ | -------------- |
| PB-1 | cold startup p50 100 / p99 200 ms | `real_window.rs` | `real_window.rs` | `pb-real-window.json` | `real-window-evidence.md` | measured (Tier 1 opt-in) |
| PB-2 | idle RSS p50 80 MB | `real_window.rs` | `real_window.rs` | `pb-real-window.json` | `real-window-evidence.md` | measured (Tier 1 opt-in) |
| PB-3 | 8-tab 250 MB + 15 % reclaim | `typical_session.rs` | `typical_session.rs` | `pb-typical-session.json` | `typical-session-evidence.md` | measured headless anchor; reclaim `ABOVE_BUDGET` finding; real 4 h session deferred |
| PB-4 | key-to-screen p50 8 / p99 15 ms | `latency.rs` | `latency_real.rs` | `pb-latency.json` | `latency-evidence.md` | measured headless anchor (this task) |
| PB-5 | binary ≤ 25 MB, dist ≤ 40 MB | release-pipeline replication (no `bitty-perf` module) | — (recipe in evidence doc) | sizes recorded in-doc | `package-size-evidence.md` | measured first-link PASS |
| PB-6 | parse-and-render ≥ 40 MiB/s | `throughput_floor.rs` | `throughput_floor.rs` | `pb-throughput-floor.json` | `throughput-floor-evidence.md` | measured headless anchor; `ABOVE_BUDGET` finding (6.06 vs 40) |
| PB-7 | idle ≤ 1 % CPU, zero wakeups | `idle.rs` | `idle_real.rs` | `pb-idle.json` | `idle-evidence.md` | measured (60 s parked child, 0.0000 %) |
| — | parser stage only (M1-11, not a PB) | `parser_throughput.rs` | `parser_throughput.rs` | `parser-throughput.json` | `baselines/README.md` | measured; explicitly not PB-6 |
| — | PB-3 long-session vehicle | `real_soak.rs` / `dogfood_session.rs` | `real_soak.rs` / `dogfood_session.rs` | `pb-real-soak.json` / `pb-dogfood-session.json` | `real-soak-evidence.md` / `dogfood-session-evidence.md` | `unavailable` (no Tier 1 session yet) |

Budget constants live in exactly one place: `crates/bitty-perf/src/lib.rs`
(`PB1_STARTUP_MS_P50/P99`, `PB2_IDLE_RSS_MB`, `PB3_TYPICAL_RSS_MB` /
`PB3_RECLAIM_PCT`, `PB4_LATENCY_MS_P50/P99`, `PB5_BINARY_MB/DIST_MB`,
`PB6_THROUGHPUT_MB_S`, `PB7_IDLE_CPU_PCT`). Every harness reads them; none
redefines them.

## Corpora (pinned)

Fixed, deterministic, in-code inputs — reused across captures so numbers stay
comparable:

- **PB-3 synthetic session:** 8 tabs × 64 KiB line-repeated shell/SGR/cursor/
  OSC/plain mix (`WORKLOAD_LINE`), 8 KiB chunks through a real `Parser` +
  `State::apply`; 512 KiB total. Not a 4 h session; a regression anchor.
- **PB-4 key mix:** printable + Enter/Backspace/ArrowRight cycle, ≤64 B per
  key; injected-echo (deterministic) primary, real-`cat` PTY secondary with
  honest mode labeling.
- **PB-6 floor corpus:** fixed synthetic segment (`SEGMENT_BYTES`), 1 MiB per
  round × 3 rounds, median reported; escape-leaning density. Seed/compat
  reuse is deferred to the PERF-01 corpus decision.
- **Parser baseline (M1-11):** reused `crates/bitty-vt/seeds/*.bin` +
  `tests/compat/*/corpus/*.bin` + synthetic escape storm; the only
  file-backed corpus, and the files are committed inputs, not discoveries.

No harness globs the working tree for inputs at measure time.

## Reference machines (pinned record, Tier 1 pending)

Captures to date share one machine class (recorded as `host_context` inside
every artifact, plus the evidence-doc table):

- CachyOS Linux (Arch derivative), x86_64, `rustc 1.98.1`
  (`rust-toolchain.toml`), desktop 24-core x86_64, 31 GiB RAM, NVMe.

What is **not** pinned — the PERF-01 remainder, blocked on OQ-100:

- The slowest Tier 1 reference machine (budgets are specified against it;
  every evidence doc gates compliance on it).
- The 60 Hz frame-presented PB-4 protocol (windowed Tier 1 run).
- The 10-minute PB-7 average (bounded 60 s proxy committed instead).
- The allocator-aware PB-3 reclaim definition (VmRSS finding recorded).
- The PB-6 floor fate (batch-apply fast path or re-scope).
- Corpus breadth (seed/compat reuse for PB-6).

## Gating status (explicit)

No hard gates change in this wave. CI enforces the *harness contract*
(determinism, exact boundary math, artifact provenance, no host paths) via
`cargo test -p bitty-perf`; CI does **not** fail on budget verdicts, because
the budgets are accepted targets awaiting the OQ-100 reference-hardware
decision. Turning them into gates is the blocked remainder of #1055, not this
record.

## Verification

```text
cargo test -p bitty-perf --test harness_pin_evidence    # CI contract
```

Expected: the pin record names every PB artifact and every named file exists
with `schema_version` and provenance intact.

## Affected contracts

No budget, gate threshold, or normative control changes. The accepted values
in `performance-budget-rfc.md` are untouched; OQ-100 owns the gating decision.

## References

- `docs/specifications/performance-budget-rfc.md` (PB-1..PB-7)
- `crates/bitty-perf/src/lib.rs` (single budget-constant owner)
- `crates/bitty-perf/tests/harness_pin_evidence.rs` (pin contract)
- Issue #1055 (PERF-01)
