---
title: Performance Baseline Harness (Phase F)
description: Headless bounded baseline harness for PB-1..PB-7 — benches and tools/perf scripts referencing performance-budget-rfc, cargo bench plumbing without window/GPU
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Performance Baseline Harness (Phase F)

## Status and provenance

- Status: **draft**. Scaffold for M1 hardening Phase F, branch `ctx-0076/chore-perf-baseline`
  (CTX-0076), worktree `.worktrees/ctx-0076-chore-perf-baseline`, agent `opencode-commander`.
- Ownership: bitty **CTX-0076** — _Establish performance baseline harness (Phase F)_.
  - Priority: P1 | Area: perf | Labels: chore,area:runtime,P1 | Milestone: v0.1.0
  - RFC: `performance-budget-rfc` (`bitty-docs/docs/specifications/performance-budget-rfc.md`)
  - Task: CTX-0076
- Scope: scaffold `benches/{vt_throughput,terminal_state,reflow,search,render_prepare}.rs`
  and `tools/perf/{startup,rss,latency,idle}` plus this doc and `crates/bitty-perf`
  owner crate, all headless, bounded, `forbid(unsafe)`, no window/GPU leak.
- Authority: `performance-budget-rfc` is **accepted** (2026-08-26, closes OQ-001);
  budgets PB-1..PB-7 are accepted targets (none measured against a real build yet).
  Enforcement mechanisms require this implementing task; until reference hardware,
  corpora, and measurement harnesses are defined, budgets are arch constraints,
  not hard CI gates (see RFC Cross-cutting rules). This scaffold does not close
  that open item, does not claim budget compliance, and does not weaken normative
  security controls in `bitty-docs/docs/security/`.

## Budgets (accepted targets, from `performance-budget-rfc.md#budgets`)

| ID   | Metric           | Budget                                                       | Condition                                                       | Rationale hint                                         |
| ---- | ---------------- | ------------------------------------------------------------ | --------------------------------------------------------------- | ------------------------------------------------------ |
| PB-1 | Cold startup     | ≤ 100 ms p50 / ≤ 200 ms p99 first prompt frame               | default config, warm file cache, local shell                    | Ghostty sub-100 ms class on Apple Silicon              |
| PB-2 | Idle memory      | ≤ 80 MB RSS p50                                              | one window, default scrollback, 60 s idle, bundled plugins only | between Alacritty 14–75 MB and Ghostty 28–174 MB       |
| PB-3 | Typical-session  | ≤ 250 MB RSS 8 tabs 4 h + reclaim within 15 % after close+GC | mixed session (DevToolReviews scenario)                         | encode growth-over-time lesson                         |
| PB-4 | Input latency    | ≤ 8 ms p50 / ≤ 15 ms p99 key-to-screen                       | 60 Hz minimum, Wayland/frame-presented                          | leaders 2–5 ms p50; plugin pipeline stays off hot path |
| PB-5 | Package size     | binary ≤ 25 MB, dist ≤ 40 MB                                 | stripped release per Tier 1                                     | inference, lower confidence                            |
| PB-6 | Throughput floor | ≥ 40 MB/s sustained VT parse-and-render (single core)        | fixed synthetic corpus                                          | peers 48–54 MB/s (ettayeb), kitty 134 MB/s             |
| PB-7 | Idle resource    | ≤ 1 % avg CPU 10 min, zero wakeups when idle                 | no PTY output/animation/plugin timer                            | frame-on-demand; continuous loops forbidden            |

All budgets apply to default config with the safe startup path (no third-party
plugins); plugin budgets belong to the future isolation/resource RFC (OQ-014).
Measurement harnesses, corpora, and reference machines must be defined in the
owning repo before any budget becomes a hard gate.

## Harness — `benches/*.rs`

- `#![forbid(unsafe_code)]` — header asserts no `unsafe` in harness.
- Headless — only `bitty-vt::Parser → TerminalAction → bitty-term-state::State → Snapshot/Damage → bitty-render::GridRenderer(fake)`; no `winit::Window`, no `wgpu::Surface`, no `SurfaceTarget`, no display server.
  Grep `benches/` for `winit`/`wgpu`/`Window`/`Surface` must be 0 except in this forbid list.
- Bounded — `MAX_CORPUS_BYTES = 8 KiB` (`bitty-pty::READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 1024`, `SCROLLBACK_MAX_LINES = 10 000`,
  `REPLY_CAP_BYTES = 4096`, `DAMAGE_MAX_REGIONS_PER_BATCH = 256`, `MAX_FRAME_REGIONS = 256`. Each bench asserts its bound before measuring.
- Deterministic — synthetic corpora are repeatable patterns; byte-by-byte re-parse identity is asserted (`parse_bounded` pattern from `tests/compat/harness.rs`); `State::state_hash` reuse is available.
- `harness = false` — each bench is a `fn main()` binary run via `cargo bench --bench <name> -- --nocapture`; `cargo bench --no-run` must compile (checked in `tools/perf/latency`).

Usage headlessly:

```text
cargo bench --no-run                          # must compile, bounded
cargo bench -p bitty-perf --bench vt_throughput -- --nocapture
cargo bench -p bitty-perf --bench terminal_state -- --nocapture
cargo bench -p bitty-perf --bench reflow -- --nocapture
cargo bench -p bitty-perf --bench search -- --nocapture
cargo bench -p bitty-perf --bench render_prepare -- --nocapture
```

### `benches/vt_throughput.rs` — PB-6

Isolates `Parser::advance` on synthetic corpus (8 KiB slices, 32 KiB mid) covering printable, CSI SGR, DECSET, OSC, DCS; reports MB/s vs PB-6 floor 40 MB/s with soft gate (warn, not hard-fail).

### `benches/terminal_state.rs` — PB-4 + PB-6

Isolates `State::apply` (plus `Snapshot`) on synthetic byte → actions; reports mean µs per batch and 8 KiB-equivalent MB/s; asserts `SCROLLBACK_MAX_LINES` bound.

### `benches/reflow.rs` — PB-3 + PB-4

Measures `State::resize` across bounded dimensions (clamped 1..1000) for expand/shrink/no-op/large cases; asserts `check_invariants()` and scrollback cap after 200 resizes; warns when > 8 ms.

### `benches/search.rs` — search bounds

Covers `State::search` (`SEARCH_MAX_PATTERN_LEN` 256, `SEARCH_MAX_RESULTS` 1000, scrollback 10 k); benchmarks small/large/wide corpora, casefold, miss, worst single-char; asserts truncation and hard-cap.

### `benches/render_prepare.rs` — PB-4 + PB-7

Headless `GridRenderer` with `FakeRasterizer` (same fake `src/cache.rs`/`src/gpu.rs` use). Exercises `GridRenderer::render` ( `Snapshot`/`Damage` → `DrawList`/`Atlas`) for small full, big full, clean (`FrameMode::Clean`), and partial dirty; asserts idle `!needs_draw()` for PB-7 zero-wakeup.

## Tools — `tools/perf/{startup,rss,latency,idle}`

- `startup` — PB-1 `hyperfine` 50-run p50/p99 when `hyperfine` present, else `date +%s%N` fallback (5 samples), `cargo run --release -p bitty-app -- --help` headless proxy, `timeout` bounded.
- `rss` — PB-2/PB-3 via `ps -o rss=` / `/proc/<pid>/status VmRSS` (KiB → MB), transient `cargo run --release -p bitty-app -- --help` child, plus self-shell baseline and 5× reclaim proxy.
- `latency` — PB-4 via `cargo bench --no-run` compile plus bench bins (or `cargo bench --bench terminal_state/render_prepare` fallback) plus `python3` deterministic loop proxy; true keystroke→photon needs Wayland + frame-presented timestamp.
- `idle` — PB-7 via `GridRenderer` clean-frame assert + `ps -o %cpu` 10 s sample (sleep child ≈ 0 %); real 10 min ≤ 1 % measured on Tier 1 ref machine.

Each tool prints the PB budget line, the headless proxy note, and the `timeout` bound. They are executable `chmod +x`, `shellcheck -S warning` clean, and never open a window.

## Crate owner — `crates/bitty-perf`

- `crates/bitty-perf` exists only to own the workspace-root `benches/` targets (workspace `Cargo.toml` is virtual, so `[[bench]]` cannot live at the root).
  Each bench is declared as `[[bench]] path = "../../benches/<name>.rs" harness = false` with deps `bitty-vt`, `bitty-term-state`, `bitty-render`, `bitty-platform`, `bitty-pty`.
- `src/lib.rs` re-exports PB constants (`PB1_STARTUP_MS_P50`, `PB2_IDLE_RSS_MB`, etc.) and `MAX_CORPUS_BYTES`/`MAX_ACTIONS` so benches and `tools/perf/*` share one budget source. No runtime API; `is_headless_witness()` is the grep guard witness.

## Bounds and determinism

- Every bench asserts its corpus ≤ `MAX_CORPUS_BYTES` (or segment limit) and actions ≤ `MAX_ACTIONS`; `State` and `GridRenderer` paths assert `check_invariants()` and damage ≤ 256.
- No `unsafe`, no window, no GPU: CI runs `cargo check --workspace --all-targets --locked`, `cargo bench --no-run`, and `just check` headlessly.
- Determinism via byte-by-byte re-parse identity and `State` pure transitions.
- `cargo bench` improvement cannot weaken correctness/security/compat/accessibility/portability/fallback/recovery (RFC cross-cutting rule).

## Relationship to `release-ladder.md`

- `v0.3` row (GPU rendering, fonts, performance, graphics protocols) already lists perf budgets PB-1/PB-2 but had no harness. This file provides the Phase F baseline sketch; `release-ladder.md` retains the ladder and crate focus, this file owns the budgets/runbook.

## Next

- Wire `benches/*` into CI `cargo bench --no-run` gate (supplement to `just check`) without requiring a display.
- Pin reference hardware + OS (`specifications/performance-budget-rfc.md` Open items) and define the fixed synthetic corpus revision in `tmp/references/` before tightening to hard gates.
- Replace `tools/perf/*` `--help` proxies with real `bitty-app` cold-to-first-frame and Wayland photon timings on a Tier 1 box (requires display server, not CI).
- Land real `hyperfine` + `ps` + compositor traces on the reference machine and record p50/p99 + variance in this doc.
- Coordinate OQ-014 isolation so plugin VM creation cost is charged against plugin budgets, not PB-2/PB-3.

## Verification

```text
cargo check --workspace --all-targets --locked
cargo bench --no-run
cargo test --workspace --all-targets --locked
just check   # fmt-check + clippy -D warnings + test + actionlint + markdownlint
act -n       # workflow syntax (ci.yml, codeql.yml)
shellcheck -S warning tools/perf/startup tools/perf/rss tools/perf/latency tools/perf/idle
```

All must be 0 issues before commit; leave worktree dirty, record `carryctx` checkpoint per task (CTX-0076).
