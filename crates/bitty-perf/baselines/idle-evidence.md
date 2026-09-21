---
title: Idle PB-7 Evidence (CTX-0636)
description: Bounded PB-7 idle CPU/wakeup evidence for the frame-on-demand invariant plus a parked-Runtime child window, with the committed baseline artifact
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 53
---

<!-- markdownlint-disable MD025 -->

# Idle PB-7 Evidence (CTX-0636)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the PB-7 idle
  CPU/wakeup harness and its captured baseline. It does not close PB-7 and
  does not claim Verified; the accepted budgets remain arch constraints until
  reference hardware and corpora are pinned (PERF-01, OQ-100).
- Ownership: bitty **CTX-0636** — _PB-7 idle CPU/wakeup evidence_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1062 (PERF-08, PB-7)
  - RFC: `performance-budget-rfc.md` PB-7 | Task: CTX-0636
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: a bounded, reproducible harness that measures idle CPU and wakeups of
  a proven-idle `Runtime` on a Linux host; a committed evidence artifact with
  numbers plus host context; an explicit `Unavailable` result where `/proc`
  counters are absent; and bounded CI contract tests (pure parser/serializer
  math only — CI never parks a 60 s window). It changes no accepted budget or
  gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); the product-level
  `docs/product/perf-evidence.md` and `docs/product/perf-baseline.md` rows are
  reconciled through this repository-local record, because the submodule is
  external content owned by another repository.

## What this wave adds

Before this task the repository had:

- `crates/bitty-perf/src/idle.rs` — the CTX-0100 frame-on-demand checks
  (`tick == None` when idle, `FrameMode::Clean` on clean damage) with a
  bounded `ps %cpu` self-sample that reports the bench process's lifetime
  average, not an idle window (it read 16–18 % on a shared workstation right
  after the busy bench loop — sampling noise, not product CPU).
- `benches/idle_real.rs` — asserted the invariant and cost headroom but
  committed no idle-CPU/wakeup numbers.
- `tools/perf/idle` — a bash proxy that sampled an unrelated `sleep` child.

After this task:

- `crates/bitty-perf/src/idle.rs` — `IdleCpuEvidence`: a parked-`Runtime`
  child measurement (CPU ticks plus voluntary/involuntary context switches
  from `/proc`, Linux-only), `run_idle_child`, `baseline_json`, and the
  `pb-idle.json` schema.
- `benches/idle_real.rs` — `--idle-window <secs>` runs the extended sample;
  `--write-baseline <path>` commits the artifact and refuses to write when the
  invariant fails or the window is unmeasured (exit 2, no fabricated numbers).
- `crates/bitty-perf/baselines/pb-idle.json` — committed artifact.
- `just perf-idle` / `just perf-idle-baseline` — human-facing recipes.

## Measurement contract

- **Proven-idle subject.** The child constructs a default `Runtime`, presents
  once, asserts the next `tick` is `None`, then blocks on a condvar with a
  deadline — the headless equivalent of the platform `ControlFlow::Wait` loop
  (`terminal_app.rs`: `AboutToWait` arms `set_wait()` when no hover,
  animation, or bell deadline is pending). After the park it re-verifies
  `tick` is still `None`; a pending present exits nonzero, failing the window.
- **No background threads in the subject.** `Runtime::with_defaults` spawns no
  worker or forwarder threads (PTY and panel workers start only on use), so
  the child's counters reflect a truly quiescent runtime.
- **Real counters, not a proxy.** CPU is `100 * Δ(utime+stime) / (CLK_TCK *
wall)` from `/proc/<pid>/stat` (comm parsed after the last `)`, safe Rust);
  wakeups are `Δvoluntary + Δinvoluntary` from `/proc/<pid>/status`. `CLK_TCK`
  comes from `getconf`. Every spawned child is reaped on every path (bounded
  grace, then kill + wait).
- **No fabricated numbers.** `--write-baseline` exits 2 when the invariant
  fails or the window is unavailable; non-Linux hosts report `Unavailable`
  with a reason and exit 0.
- **Bounded.** Window clamped to 1–300 s (`BITTY_PERF_IDLE_SECS`, default 60);
  tick/render cost samples stay at 3000/2000 iterations.
- **Host context from the environment.** OS, arch, toolchain, CPU count, and
  total memory come from `std::env::consts`, `available_parallelism`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
- **`#![forbid(unsafe_code)]`**, Linux-gated for the extended sample.

## Harness and commands

```text
# Fast path (invariant + cost means, no extended window):
just perf-idle

# Extended sample with an explicit window (Linux):
cargo bench -p bitty-perf --bench idle_real -- --nocapture --idle-window 60

# Regenerate the committed artifact (provenance from the environment):
BITTY_PERF_TASK=CTX-0636 BITTY_PERF_DATE=... BITTY_PERF_REVISION=... \
  BITTY_PERF_TOOLCHAIN=... BITTY_PERF_COMMAND=... BITTY_PERF_PROFILE=... \
  just perf-idle-baseline
```

Environment knobs (all optional, clamped):

| Variable               | Default | Bound | Purpose                        |
| ---------------------- | ------- | ----- | ------------------------------ |
| `BITTY_PERF_IDLE_SECS` | 60      | 1–300 | Extended idle window (seconds) |

## Captured baseline

Command (exact): `just perf-idle-baseline` with the provenance environment
below (equivalently the `BITTY_PERF_*=... cargo bench ... --write-baseline`
line above).

Environment for this capture (host context is recorded inside the artifact):

| Field         | Value                                                                                              |
| ------------- | -------------------------------------------------------------------------------------------------- |
| Revision      | worktree `b0b1155aec54b127503d049a5df61f84cd24d9b8`                                                |
| Profile       | cargo `bench` release                                                                              |
| Toolchain     | `rustc 1.98.1` (`rust-toolchain.toml`)                                                             |
| OS            | CachyOS Linux (Arch derivative), x86_64                                                            |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe                                                           |
| Sample        | 9 invariant checks + 3000 idle ticks + 2000 clean renders; one parked-`Runtime` child, 60 s window |

| Metric                         | Measured    | Budget                      | Verdict |
| ------------------------------ | ----------- | --------------------------- | ------- |
| Frame-on-demand (9 checks)     | 9/9 PASS    | zero wakeups when idle      | `PASS`  |
| Parked-`Runtime` avg CPU, 60 s | 0.000 %     | ≤ 1 % avg over 10 min       | `PASS`  |
| Parked-`Runtime` wakeups, 60 s | 2 (1v + 1i) | zero periodic wakeups       | `PASS`  |
| Idle tick mean                 | 1.864 µs    | << 8 ms (PB-4 p50 headroom) | `PASS`  |
| Clean render mean              | 0.020 µs    | << 8 ms                     | `PASS`  |

The 2 wakeups are spawn/settle edge effects (child start plus the single
condvar-deadline fire and exit); the 60 s steady state shows zero periodic
wakeups and zero CPU ticks, consistent with the `tick == None → Wait`
mechanism. The legacy `ps` self-sample in the same run still reads high
(16–18 %) because `ps %cpu` averages over the bench process lifetime
including the busy measurement loop — that proxy is retained for continuity
but is not the evidence; the parked-child window is.

Interpretation, stated honestly:

- These are **measurements of the current headless slice**, not a
  budget-compliance claim. The subject is a default `Runtime` parked on a
  condvar, not a real windowed session with a compositor, GPU, PTY, or
  plugins; compositor-driven wakeups and real-driver costs are outside this
  window. The accepted 10-minute average on pinned Tier 1 reference hardware
  is decided once PERF-01/OQ-100 lands.
- The numbers are a **regression anchor on one machine**, not a normalized
  score and not a cross-platform claim.
- The harness is the deliverable; the within-budget reading on this host
  should feed the PERF-01 budget/harness decision, not pre-empt it.

## Verification

```text
cargo test -p bitty-perf --lib idle --locked        # invariant + parser/serializer math
cargo bench -p bitty-perf --bench idle_real -- --nocapture --idle-window 5   # 5 s smoke
just check                                          # fmt + clippy -D warnings + test + lint
just perf-idle                                      # fast path (Tier 1 or workstation)
```

Expected: unit tests pass with no display; the 5 s smoke prints
`idle-cpu — PASS` on Linux and `UNMEASURED` with a reason elsewhere.

## Limitations and confidence

- Single-host capture; no multi-machine distribution and no variance study.
- The extended window (60 s, max 300 s) is a bounded proxy for the accepted
  10-minute average; the real 10-minute measurement stays gated on Tier 1.
- Headless subject only: no winit event loop, no wgpu surface, no PTY bytes,
  no plugin timers. Panel-worker poll cadences (`panels_async.rs`) and plugin
  timers are out of this window by construction (not started in the subject);
  their idle contribution needs a windowed session measurement.
- The legacy `ps` self-sample remains noisy by design (lifetime average);
  reviewers should read the `idle-cpu` line, not the `sampled_cpu` field.
- Cross-platform claims require CI or reference-hardware evidence on the named
  platform; this host proves nothing for other platforms.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule
(`docs/product/perf-evidence.md` and `docs/product/perf-baseline.md`). This
repository cannot edit that external content in this change; the
reconciliation is: this document plus
`crates/bitty-perf/baselines/pb-idle.json` are the repository-local PB-7
evidence, and a follow-up in the docs repository should link the new artifact
and record the within-budget-on-this-host finding. No row is claimed Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. The PB-7 accepted
values in `performance-budget-rfc.md` are untouched (PERF-01/OQ-100 owns the
decision to turn them into hard gates).

## References

- `docs/specifications/performance-budget-rfc.md#pb-7-idle-resource-usage`
- `crates/bitty-perf/baselines/pb-idle.json` (committed artifact)
- `crates/bitty-perf/src/idle.rs` (harness)
- `benches/idle_real.rs` (human-facing bench)
- `crates/bitty-app/src/terminal_app.rs` (`AboutToWait` → `set_wait()`)
