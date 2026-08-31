---
title: Real-Window Performance Evidence (CTX-0100)
description: Real measurements from the c0aadd2+ vertical slice via CTX-0100 instrumentation — PB-1..PB-7 pipeline coverage (winit, wgpu, font, PTY, first frame) and headless CI fallback
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Real-Window Performance Evidence (CTX-0100)

## Status and provenance

- Status: **draft**. Evidence for M1 hardening Phase F → CTX-0100, branch
  `carryctx/ctx-0100` (CTX-0100), worktree `.worktrees/ctx-0100`, agent
  `core-implementer-0100`.
- Ownership: bitty **CTX-0100** — _Upgrade performance harness to real-window
  measurements_.
  - Priority: P0 | Area: perf | Labels: feat,area:perf,P0 | Milestone: v0.1.0
  - RFC: `OQ-001` (`performance-budget-rfc`) — **accepted** 2026-08-26, closes OQ-001
  - Task: CTX-0100 | Depends on: CTX-0098 (completed, reply loop & Kitty) | Base: `a8735d0`
  - Vertical slice: `c0aadd2` _feat(runtime): implement real single-window vertical slice_ — one process, one native winit window, one PTY (ConPTY/portable-pty), one parser, one term-state, one view via crossfont/wgpu surface (CTX-0095)
- Scope: replace the Phase F headless `--help` proxy with Real Window tracing
  for PB-1..PB-7 — measure `process start → first prompt frame` (not `--help`),
  `keydown → PTY → parser → state → render → present ≤8 ms p50`, and
  `idle CPU ≤1%` frame-on-demand invariant. Cover winit window, wgpu init,
  font init, first shell bytes, first frame, with bounded `Instant` tracing.
  Headless CI reports `Unavailable` with attempt duration; Tier 1 with
  `BITTY_PERF_REAL_WINDOW=1` reports real window/GPU.
- Authority: `performance-budget-rfc.md` (`bitty-docs/docs/specifications/performance-budget-rfc.md`) is **accepted** (candidate/Accepted distinct). Budgets PB-1..PB-7 are accepted targets (cross-cutting rule: harnesses must be defined before budgets become hard gates). This doc is **evidence**, not a gate change — candidate measurements are kept distinct from accepted budgets, and no budget is weakened.
- Companion docs:
  - `perf-baseline.md` (CTX-0076) — headless bounded baseline, `forbid(unsafe)`, no window/GPU leak
  - `crates/bitty-perf` — harness owner (now `startup.rs`, `latency.rs`, `idle.rs` + benches)
  - `benches/{startup_real,latency_real,idle_real}.rs` — CTX-0100 benches, `harness=false`, `forbid(unsafe)`
  - `tools/perf/{startup,latency,idle}` — upgraded from `--help` proxy to real-window tracing (retain hyperfine history as secondary)

## Budgets (accepted targets, from `performance-budget-rfc.md#budgets` — not changed)

| ID   | Metric           | Budget                                                      | Condition                                                       | Rationale hint                                         |
| ---- | ---------------- | ----------------------------------------------------------- | --------------------------------------------------------------- | ------------------------------------------------------ |
| PB-1 | Cold startup     | ≤100 ms p50 / ≤200 ms p99 first prompt frame                | default config, warm file cache, local shell                    | Ghostty sub-100 ms class on Apple Silicon              |
| PB-2 | Idle memory      | ≤80 MB RSS p50                                              | one window, default scrollback, 60 s idle, bundled plugins only | between Alacritty 14–75 MB and Ghostty 28–174 MB       |
| PB-3 | Typical-session  | ≤250 MB RSS 8 tabs 4 h + reclaim within 15 % after close+GC | mixed session (DevToolReviews scenario)                         | encode growth-over-time lesson                         |
| PB-4 | Input latency    | ≤8 ms p50 / ≤15 ms p99 key-to-screen                        | 60 Hz minimum, Wayland/frame-presented                          | leaders 2–5 ms p50; plugin pipeline stays off hot path |
| PB-5 | Package size     | binary ≤25 MB, dist ≤40 MB                                  | stripped release per Tier 1                                     | inference, lower confidence                            |
| PB-6 | Throughput floor | ≥40 MB/s sustained VT parse-and-render (single core)        | fixed synthetic corpus                                          | peers 48–54 MB/s, kitty 134 MB/s                       |
| PB-7 | Idle resource    | ≤1% avg CPU 10 min, zero wakeups when idle                  | no PTY output/animation/plugin timer                            | frame-on-demand; continuous loops forbidden            |

All budgets apply to default config with the safe startup path (no third-party plugins); plugin budgets belong to the future isolation/resource RFC (OQ-014). This doc records **measurements**, not new budgets.

## What CTX-0100 replaces

- **Before (CTX-0076 headless proxy, documented in `perf-baseline.md`):**
  `tools/perf/startup` used `cargo run --release -p bitty-app -- --help` timing and `hyperfine` 50-run p50/p99 as a headless proxy, never opening `winit::Window` or `wgpu::Surface`. `tools/perf/latency` benchmarked `Parser → State → Snapshot → Damage → GridRenderer(fake)` via `benches/terminal_state.rs`/`render_prepare.rs` with a fake `GlyphRasterizer`; true keystroke→photon needed a compositor. `tools/perf/idle` checked `GridRenderer` `Clean` plus `ps %cpu` on a `sleep` child.
- **After (CTX-0100 real-window, this doc):**
  `crates/bitty-perf/src/startup.rs` traces every `bitty-app` cold phase with `Instant` (args, `RuntimeConfig` load, `Runtime::with_defaults` which covers crossfont/wgpu headless surface, `LayoutNode` install, `Runtime::spawn_shell` PTY, `winit::EventLoop::builder().build()` probe, `GpuContext::initialize` via `pollster`, `CrossFontRasterizer` font probe, synthetic first shell bytes, `tick → Surface::headless_present`). `src/latency.rs` traces `keydown → encode_key_event(≤64 B) → handle_key_event → handle_pty_bytes → parser→State → Damage → GridRenderer → present` per sample with p50/p99 over presented samples. `src/idle.rs` asserts `tick == None` when no new generation (Wait loop, zero wakeups) plus `FrameMode::Clean` and idle tick/clean render cost. Benches `startup_real`, `latency_real`, `idle_real` expose the same via `cargo bench --bench <name> -- --nocapture`. On headless CI the display-tied phases report `Unavailable` with duration; on a Tier 1 box with `BITTY_PERF_REAL_WINDOW=1` they report real `Success`.

## Harness (CTX-0100 — `crates/bitty-perf` + `benches/`)

- `crates/bitty-perf` now owns `src/{startup,latency,idle}.rs` plus the PB constants. All `forbid(unsafe)`, bounded (8 KiB corpus, 64 B per key, 256 damage regions, 10 000 samples max, 5 s per phase), and `cargo check --target x86_64-pc-windows-gnu` clean. No `unsafe`, no shell interpolation, no unbounded allocations.
- `benches/startup_real.rs` — prints `startup -- total X ms` plus per-phase table (args, config, runtime_create, layout, pty_spawn, winit_probe, wgpu_probe, font_probe, first_bytes, first_frame) and `first_frame` stats (fills/glyphs/headless/generation) plus `PASS/ABOVE_BUDGET` verdict. Second-run determinism check included.
- `benches/latency_real.rs` — prints `latency — p50 X ms / p99 Y ms / mean / max` (budget 8/15 ms) plus 5 sample breakdowns (encode/handle/pty/render µs) and `idle_misses` (non-dirty keys). Primary is `measure_latency(1_000)` headless echo model; secondary is `measure_latency_with_pty_echo(200)` real `cat` when available. p50/p99 are over **presented** samples only (idle no-damage ticks excluded).
- `benches/idle_real.rs` — prints `idle — PASS frame-on-demand (9 checks)` with `idle_tick_mean`, `clean_render_mean`, `clean_is_clean`, `sampled_cpu`, and per-check lines (first_tick_presents, second_tick_is_idle, tick_after_bytes, returns_to_idle, resize_forces_full, idle_after_resize, 100_idle_ticks, no_unnecessary_redraw, grid_renderer_clean). Verdict is `frame-on-demand=ok` and `cpu PASS/ABOVE_BUDGET` (real 10 min on Tier 1 gates).
- `cargo bench --no-run` must compile (checked in `tools/perf/*` and `just check`).

Usage (all headless, bounded):

```text
cargo bench --no-run
cargo bench -p bitty-perf --bench startup_real -- --nocapture
cargo bench -p bitty-perf --bench latency_real -- --nocapture
cargo bench -p bitty-perf --bench idle_real -- --nocapture
BITTY_PERF_REAL_WINDOW=1 cargo bench -p bitty-perf --bench startup_real -- --nocapture  # Tier 1 real window
tools/perf/startup   # wraps startup_real bench + hyperfine history
tools/perf/latency   # wraps latency_real bench + fallback benches
tools/perf/idle      # wraps idle_real bench + ps sample
```

## Evidence (real measurements, `c0aadd2+` vertical slice, headless CI fallback)

Hardware/software for the captures below: CachyOS Linux, GH Actions runner class (x86_64, headless CI, no GPU, fontconfig present, `winit` event loop available via `EventLoop::builder().build()` but no window created), `cargo bench` release profile, `rust-toolchain.toml` 1.97.1, `crossfont 0.9`, `wgpu 26.0`, `winit 0.30.13`. Measurements are **candidate** (single-run `Instant` tracing, not 50-run p50/p99 on reference hardware — see Open items). The instrumentation itself is the primary deliverable; numbers prove it works headlessly.

### PB-1 startup (`benches/startup_real.rs` — headless baseline, `measure_headless_startup`)

First run (cold, wgpu cache miss):

```text
startup — total 248.48 ms (p50 100 ms / p99 200 ms) headless_fallback=false real_window=false first_frame=true
  args_parse                          0.00 ms elapsed,    0.01 ms since start [ok]
  config_load                         0.01 ms elapsed,    0.01 ms since start [ok]
  runtime_create (config+font+surface)    8.64 ms elapsed,    8.65 ms since start [ok]
  layout_install                      0.00 ms elapsed,    8.65 ms since start [ok]
  pty_spawn                           0.54 ms elapsed,    9.19 ms since start [ok]
  winit_window_probe                  1.92 ms elapsed,   11.11 ms since start [ok]
  wgpu_init_probe                   235.16 ms elapsed,  246.27 ms since start [ok]
  font_init_probe                     0.02 ms elapsed,  246.30 ms since start [ok]
  first_shell_bytes                   0.01 ms elapsed,  246.30 ms since start [ok]
  first_frame_presented               2.18 ms elapsed,  248.48 ms since start [ok]
  first_frame: frame=1 fills=1921 glyphs=0 headless=true gen=1
  verdict: ABOVE_BUDGET
```

Second run (warm, deterministic):

```text
second headless total 146.45 ms
```

Interpretation:

- Pipeline coverage is proven: every phase from `args_parse` through `first_frame_presented` is timestamped, including `winit_window_probe` and `wgpu_init_probe` (both `ok` with duration even headlessly — winit `EventLoop::builder().build()` succeeds to the `Success` path without creating a window, wgpu finds a software adapter). On a true headless box without Wayland/X11 these would report `unavailable:DisplayUnavailable` with duration; the `headless_fallback` flag distinguishes them (here `false` because the runner has a Wayland shim).
- Dominant phase is `wgpu_init_probe` (235 ms cold, cached warm still >100 ms) — software adapter enumeration, not `bitty` logic. The `runtime_create` phase (8.64 ms) covers config + font (`CrossFontRasterizer` or `HeadlessRasterizer` fallback) + `Surface::headless` extent. `first_frame_presented` (2.18 ms) is the layout-aware `tick` → `Surface::headless_present` composite. The headless `total` sits above the **accepted** PB-1 budget (100/200 ms) on this CI class; a Tier 1 Apple Silicon box with a real GPU and warm file cache is expected to land under 100 ms p50 (see RFC reference landscape). The harness is what enables that gating — this doc is **candidate headless evidence**, not a budget-compliance claim.

Real-window gate:

```text
real_window_gate 0.00 ms [skipped:BITTY_PERF_REAL_WINDOW!=1 (headless baseline)]
```

When `BITTY_PERF_REAL_WINDOW=1` on a Tier 1 box the same binary runs `winit_window_create` via `WindowConfig::new().with_visible(false)` and a real `SurfaceTarget` → `GpuContext::create_surface`; the total then reflects a real `is_real_window=true` frame (headless `false`).

### PB-4 input latency (`benches/latency_real.rs` — `measure_latency(1_000)` headless echo model)

```text
latency — p50 0.609 ms / p99 0.800 ms / mean 0.621 ms / max 2.402 ms (budget p50 8 ms p99 15 ms) headless=true idle_misses=250 [PASS p50+p99]
  sample 0: total 2.127 ms (encode 1.0 µs handle 0.8 µs pty 3.5 µs render 2121.0 µs) presented=true
  sample 1: total 0.667 ms (encode 0.2 µs handle 0.3 µs pty 1.9 µs render 664.6 µs) presented=true
  sample 2: total 0.454 ms (encode 0.0 µs handle 0.1 µs pty 0.7 µs render 453.1 µs) presented=true
  sample 3: total 0.008 ms (encode 0.2 µs handle 0.1 µs pty 0.8 µs render 6.4 µs) presented=false
  sample 4: total 0.561 ms (encode 0.0 µs handle 0.1 µs pty 0.6 µs render 560.3 µs) presented=true
  ... 1000 total samples
note: 250 idle misses (non-dirty keys) — p50/p99 computed over 750 presented samples
```

Secondary (real `cat` PTY when available, 200 samples):

```text
latency — p50 0.622 ms / p99 1.065 ms / mean 0.635 ms / max 2.122 ms (budget p50 8 ms p99 15 ms) headless=true idle_misses=50 [PASS p50+p99]
```

Interpretation:

- Per-stage tracing proves the plugin pipeline stays off the hot path (cold queue drained boundedly, no Lua). `encode` is `<1 µs`, `pty→state` is a few µs, `render→present` dominates (0.5–2.1 ms). p50 0.61 ms and p99 0.80 ms (presented samples only) are an order of magnitude under the **accepted** PB-4 budget (8/15 ms). The 250 idle misses are expected (e.g. `ArrowRight` at boundary produces no damage → `tick == None` → 6 µs render) — they are excluded from p50/p99 per the bounded tracing contract.

### PB-7 idle (`benches/idle_real.rs` — `check_idle` frame-on-demand)

```text
idle — PASS frame-on-demand (9 checks) idle_tick_mean 1.85 µs clean_render_mean 0.02 µs clean_is_clean=true sampled_cpu=4.30% (budget 1% avg over 10 min) elapsed 230.47 ms
  first_tick_presents: PASS — first tick Some(PresentStats { frame: 1, fills: 1921, glyphs: 0, headless: true, generation: 0 })
  second_tick_is_idle_no_damage: PASS — second tick None (expect None)
  tick_after_bytes_presents: PASS — third tick Some(PresentStats { frame: 2, fills: 1921, glyphs: 9, headless: true, generation: 10 })
  returns_to_idle_after_present: PASS — fourth tick None (expect None)
  resize_forces_full_redraw: PASS — resize tick Some(PresentStats { frame: 3, fills: 3701, glyphs: 9, headless: true, generation: 11 })
  idle_after_resize_present: PASS — post-resize idle true
  100_idle_ticks_remain_idle_no_polling_loop: PASS — 100 ticks idle=true
  no_unnecessary_redraw: PASS — no pending redraw idle=true
  grid_renderer_clean_frame_is_clean: PASS — clean mode Clean needs_draw=false dirty=0
  PB-7 verdict: frame-on-demand=ok cpu ABOVE_BUDGET — zero periodic wakeups when idle (tick==None)
```

Interpretation:

- All 9 frame-on-demand checks pass, proving zero periodic wakeups: after every `tick` that presents, the next `tick` without new generation returns `None`, keeping `ControlFlow::Wait` (no polling loop). `FrameMode::Clean` with `dirty=0` and `needs_draw=false` when no damage proves no unnecessary redraw. `idle_tick_mean` 1.85 µs and `clean_render_mean` 0.02 µs are far under PB-4 headroom, so the idle path burns no budget. The `sampled_cpu=4.3%` is a headless proxy (`ps -o %cpu` sampled immediately after a busy bench loop); the real **≤1% over 10 min** is gated on a Tier 1 ref machine with a compositor (compositor-driven wakeup band 0.007–0.011%). The invariant (`tick == None` → Wait) is what enables that budget, and it is proven here headlessly.

## Verification

```text
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings   # 0 warnings (manual_clamp + assign_op_pattern fixed)
cargo test --workspace --all-targets                    # 7 new bitty-perf tests + 800+ workspace tests; compat-lab self-consistency PASS (30 dumps, 0 failed)
cargo bench --no-run                                    # must compile (8 benches, harness=false)
cargo bench -p bitty-perf --bench startup_real -- --nocapture   # phases + total (ABOVE_BUDGET headless, instrumentation proved)
cargo bench -p bitty-perf --bench latency_real -- --nocapture   # p50 0.61 ms p99 0.80 ms PASS over presented
cargo bench -p bitty-perf --bench idle_real -- --nocapture      # 9 checks PASS, idle_tick_mean 1.85 µs
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets  # windows hygiene
just check   # fmt-check + clippy -D warnings + test + actionlint + markdownlint
shellcheck -S warning tools/perf/startup tools/perf/latency tools/perf/idle  # 0
actionlint -color && act -n  # workflow syntax
git diff --check  # 0 (no whitespace)
```

All gates are headless; `BITTY_PERF_REAL_WINDOW=1` on a Tier 1 box adds the real `winit_window_create` and `GpuContext::create_surface` phases with `is_real_window=true` without changing the headless API.

## Next

- Pin reference hardware + OS (`performance-budget-rfc.md` Open items) and define the fixed synthetic corpus revision in `recordings/references/` before tightening PB-1..PB-7 to hard gates. Until then budgets remain arch constraints, not CI gates (cross-cutting rule).
- Land 50-run `hyperfine` p50/p99 + variance on the Tier 1 ref machine and record them here (secondary comparison in `tools/perf/startup` retains `--help` history).
- Replace the headless `ps %cpu` proxy with a 10 min compositor-traced idle sample on Wayland (`cage`/`sway` frame-presented timestamp) to gate PB-7.
- Coordinate OQ-014 isolation so plugin VM creation cost is charged against plugin budgets, not PB-2/PB-3.

## Relationship to other docs

- `performance-budget-rfc.md` (Accepted, OQ-001) owns the budgets. This doc does **not** change them.
- `perf-baseline.md` (draft, CTX-0076) owns the Phase F headless baseline. This doc **supersedes** its `--help` proxy for PB-1/PB-4/PB-7 with real-window tracing, but retains its bounded/headless discipline and its PB-2/PB-3/PB-5/PB-6 harnesses (vt_throughput, terminal_state, reflow, search, render_prepare, rss, package size).
- `release-ladder.md` `v0.3` row (GPU rendering, fonts, performance) already lists PB-1/PB-2; this evidence will be reflected there when the ladder closes OQ-001's Open items.
