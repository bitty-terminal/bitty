---
title: PB-2 Idle RSS Evidence (CTX-0694)
description: Tier 1 real-window re-measurement of PB-2 idle RSS for issue #1190, with a /proc contributor breakdown and a reduction path proposal
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 53
---

<!-- markdownlint-disable MD025 -->

# PB-2 Idle RSS Evidence (CTX-0694)

## Status and provenance

- Status: **draft evidence**. Repository-owned record for the PB-2 idle-RSS
  re-measurement and contributor analysis. It does not close PB-2 and does not
  claim Verified; the accepted budget remains an arch constraint until
  reference hardware and corpora are pinned (PERF-01, OQ-100).
- Ownership: bitty **CTX-0694** — _PB-2 idle RSS real-window measurement +
  analysis_.
  - Priority: P1 | Area: perf | Labels: chore,area:perf,P1 | Milestone: v0.1.0
  - Issue: #1190 (PERF: PB-2 idle RSS ~426 MB vs ≤ 80 MB budget)
  - RFC: `performance-budget-rfc.md` PB-2 | Task: CTX-0694
  - Depends on: PERF-01 (#1055, blocked on OQ-100 for budget **gating** only —
    this wave adds evidence, not gates)
- Scope: a Tier 1 real-window re-measurement of idle RSS on the current
  vertical slice using the CTX-0592 harness unchanged; a committed
  `pb-rss.json` artifact; a `/proc` contributor breakdown of one idle
  session; and a reduction path proposal. It changes no accepted budget or
  gate threshold.
- Verdict: `ABOVE_BUDGET`. This wave closes as **EVIDENCE_ONLY** (Relates
  #1190, not Closes).

## Measurement contract

The run reuses the CTX-0592 real-window harness (`src/real_window.rs`,
`benches/real_window.rs`) without modification, so its contract still holds:

- **Opt-in.** `BITTY_PERF_REAL_WINDOW=1` plus a resolvable `bitty` binary
  (`BITTY_PERF_BIN`); otherwise `Unavailable`, nothing fabricated.
- **Real binary, real window.** Release `bitty` on Hyprland (Wayland), one
  window, default scrollback, bundled plugins only, 60 s idle, 3 RSS samples.
- **Bounded.** 10 startup launches, 60 s idle, 30 s per-launch timeout; every
  child killed and reaped on every path.
- **Host context from the environment.** No checkout path, username, or
  hostname is embedded; provenance comes from `BITTY_PERF_*` variables.

## Captured baseline

Command (exact):

```text
BITTY_PERF_REAL_WINDOW=1 BITTY_PERF_STARTUP_SAMPLES=10 BITTY_PERF_IDLE_SECS=60 \
  cargo bench -p bitty-perf --bench real_window -- --nocapture
```

Environment for this capture (host context is recorded inside the artifact):

| Field | Value |
| ----- | ----- |
| Revision | `c9680cb` (CTX-0694 base, post CTX-0592) |
| Profile | cargo `bench` release |
| Toolchain | `rustc 1.98.1` (`rust-toolchain.toml`) |
| OS | CachyOS Linux (Arch derivative), Wayland (Hyprland), x86_64 |
| Machine class | desktop 24-core x86_64, 31 GiB RAM, NVMe, NVIDIA RTX 4060 Laptop |
| Sample | 10 launches (PB-1); one session, 60 s idle, 3 RSS samples (PB-2) |

| Metric | Measured | Budget | Verdict |
| ------ | -------- | ------ | ------- |
| PB-2 idle RSS p50 | 426.7 MB | ≤ 80 MB | `ABOVE_BUDGET` |
| PB-1 p50 (context) | 218.1 ms | ≤ 100 ms | `ABOVE_BUDGET` |
| PB-1 p99 (context) | 245.9 ms | ≤ 200 ms | `ABOVE_BUDGET` |

Cross-checks on the same host agree: 425.8 MB (10 s idle pilot, same day),
426.3 MB (CTX-0592 committed artifact), 431.3 MB (CTX-0592 second capture).
The gap is stable and reproducible, not a one-run outlier.

Artifact: `crates/bitty-perf/baselines/pb-rss.json` (same shape as
`pb-real-window.json`; task `CTX-0694`, issues `[1190]`).

## Contributor breakdown

A sibling idle session (same binary, first-frame marker seen, ~15 s idle,
VmRSS 436124 kB ≈ 426 MB) was inspected via `/proc/<pid>/status` and
`/proc/<pid>/smaps` before being killed. Summary:

| Component | RSS | Share of VmRSS | Notes |
| --------- | --- | -------------- | ----- |
| `RssFile` total | 240 MB | ~55% | dominated by the rows below |
| `RssShmem` total | 145 MB | ~33% | 87 shared-permission mappings (GPU/Wayland buffer pool) |
| `RssAnon` total | 50 MB | ~12% | includes `[heap]` 30 MB (system allocator, no custom global allocator) |
| `/dev/nvidiactl` | 141 MB | ~33% | NVIDIA driver mapping attributed to the process |
| NVIDIA userspace libs | ~57 MB | ~13% | `libnvidia-gpucomp` 27 MB, `libnvidia-glcore` 13 MB, `libnvidia-eglcore` 9 MB, remainder |
| `libLLVM` + gallium | ~18 MB | ~4% | Mesa/GL stack residency |
| `bitty` binary itself | 10 MB | ~2% | 18.3 MB on disk; text 13 MB |

Not resident at idle: scrollback (default 10000 lines, window freshly
opened, buffer near-empty) and bundled plugins (six `v1` staged entries,
disabled by default per `bitty-plugin-host/src/bundled.rs`).

Interpretation, stated honestly:

- App-attributable anonymous memory is roughly **50 MB** — already inside the
  80 MB budget taken on its own.
- About **85% of VmRSS** is GPU/driver/file-mapped residency
  (`nvidiactl` + NVIDIA libs + the 145 MB shared buffer pool + Mesa/LLVM),
  which the application cannot free and which scales with the driver stack,
  not with terminal state.
- As currently specified — whole-process VmRSS on a real window — PB-2 is
  unreachable on this proprietary-driver host: the driver stack alone is more
  than twice the budget before the first line of terminal state.

## Reduction path proposal

No code in this wave; the proposal below is input to PERF-01 (#1055) and
future tasks, ordered by leverage:

1. **Split the metric (spec, PERF-01).** Gate the part the product controls
   — app-owned anonymous memory (`RssAnon` / private-dirty, ~50 MB today,
   already within budget) — and keep whole-process VmRSS as informational on
   proprietary-driver hosts. Alternatively pin the gate to reference hardware
   without a proprietary driver stack (Intel iGPU on Mesa, or Lavapipe
   software rendering for the gate with real-GPU runs informational).
2. **Shrink the shared buffer pool (MB-scale).** Audit why 87
   shared-permission mappings totaling 145 MB are retained for one idle
   window: Wayland `wl_shm` buffer retention, EGL surface/swapchain depth,
   and font-atlas sizing in `bitty-render` are the first places to look.
3. **Defer and narrow GPU residency (MB-scale).** Prefer the low-power
   adapter where the platform offers one, lazily create secondary surfaces,
   and compare wgpu backends (Vulkan vs GL) on NVIDIA for residency, not just
   frame time.
4. **Trim the heap tail (MB-scale).** The 30 MB `[heap]` under the system
   allocator is already small; `malloc_trim` tuning or an allocator with
   background purging only matters after items 1–3.
5. **Do not** renegotiate the 80 MB number here, and do not turn PB-2 into a
   hard gate until PERF-01/OQ-100 lands. The harness keeps reporting
   `ABOVE_BUDGET` honestly.

## Verification

```text
cargo test -p bitty-perf --test real_window_evidence --locked   # Unavailable/provenance contract
cargo test -p bitty-perf --lib --locked                        # metric math + baseline provenance
cargo bench -p bitty-perf --bench real_window --no-run          # bench compiles headlessly
just fmt-check                                                  # formatting gate
RUSTFLAGS="-D warnings" cargo clippy --workspace               # lint gate
cargo +1.85 check -p bitty-perf                                # MSRV floor (no let-chains)
BITTY_PERF_REAL_WINDOW=1 just perf-real-window                 # Tier 1 only (Unmeasured on CI)
```

Expected: on headless CI the bench prints `UNMEASURED` and exits 0; the
contract tests pass with no display.

## Limitations and confidence

- Single-host capture on a proprietary NVIDIA stack; the contributor split
  (especially the 141 MB `nvidiactl` mapping and the 145 MB shared pool) is
  driver- and host-specific and must not be read as a cross-platform claim.
- The `/proc` breakdown comes from a sibling session at the same RSS level,
  not from inside the harness sampling window; it attributes the steady
  state, not the sampling instant.
- PB-2 samples one window after a bounded idle interval (60 s), not a 4 h
  soak; PB-3 typical-session memory remains separate.
- Cross-platform claims require CI or reference-hardware evidence on the
  named platform; this host proves nothing for other platforms.

## Affected contracts

No budget, gate threshold, or normative control changes. The accepted PB-2
value in `performance-budget-rfc.md` is untouched (PERF-01/OQ-100 owns the
decision to turn it into a hard gate). The CTX-0592 harness, bench, and
`pb-real-window.json` artifact are unmodified; `pb-rss.json` is additive.

## References

- `docs/specifications/performance-budget-rfc.md#pb-2-idle-memory`
- `crates/bitty-perf/baselines/pb-rss.json` (committed artifact, this wave)
- `crates/bitty-perf/baselines/real-window-evidence.md` (CTX-0592 harness)
- `crates/bitty-perf/baselines/pb-real-window.json` (CTX-0592 artifact)
- Issue #1190 (this wave); PERF-01 #1055 (budget gating, blocked on OQ-100)
