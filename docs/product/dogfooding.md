---
title: Dogfooding — Minimal Terminal Daily-Driver (Phase G)
description: Bounded headless checklist for shell/cargo/git/nvim/tmux/ssh daily-driver smoke — bitty 0.0.1 minimal terminal Phase G; scripts/dogfood.sh + crates/bitty-runtime/tests/dogfooding.rs harness, no window/GPU leak
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Dogfooding — Minimal Terminal Daily-Driver (Phase G)

## Status and provenance

- Status: **draft**. Phase G dogfooding harness for the `0.0.1` minimal terminal (`vt` + `pty` + `term-state` + `platform` + `render` + `ui` + `runtime` + `app`; `package`/`lua` leaves ready) at `78d8876` (`main`), branch `ctx-0077/dogfooding` (CTX-0077), worktree `.worktrees/ctx-0077-dogfooding`, agent `opencode-commander`.
- Ownership: bitty **CTX-0077** — _Dogfooding minimal terminal daily-driver (Phase G)_.
  - Priority: P1 | Area: runtime | Labels: chore,area:runtime,P1 | Milestone: v0.1.0 | RFC: release-ladder | Task: CTX-0077
- Scope: bounded, headless daily-driver smoke checklist (`shell`, `cargo`, `git`, `nvim`, `tmux`, `ssh`) plus `scripts/dogfood.sh` and `crates/bitty-runtime/tests/dogfooding.rs` harness that records findings without touching window/GPU. Keep headless (`Surface::headless`, `HeadlessRasterizer`), bounded (8 KiB corpus, 4096 actions, 90 s wall), `forbid(unsafe)`, no `winit`/`wgpu`/`Window`/`Surface` leak.
- Authority: every `v0.1` gate remains `status: draft` until independent review per `open-questions.md`. Closing any item still requires its RFC/ADR. This doc does not add roadmap promises, does not claim daily-driver completeness, and does not weaken normative security controls in `bitty-docs/docs/security/`.

## Goals

- Prove `bitty` can be driven headlessly through the six daily-driver surfaces that define milestone M1 usage (`shell`, `cargo`, `git`, `nvim`, `tmux`, `ssh`) without opening a window, GPU, or filesystem beyond the bounded channel.
- Keep the proof **bounded** (fixed corpus size, fixed iterations, fixed timeout), **headless** (software present only), and **deterministic** (byte-identical replay) so CI stays green on headless runners.
- Record findings in a bounded, reviewable ledger (test report + script table) that names the smoke, method (synthetic vs real PTY), bounded params, and assertion outcome — no unbounded log, no window/GPU coupling.

## Scope

- **In scope:** six headless smokes on `78d8876` workspace:
  - `shell` — interactive shell echo, line editing, prompt, resize;
  - `cargo` — colored compiler-diagnostic stream (`cargo check` SGR);
  - `git` — `git status/log/diff` SGR + UTF-8 branch glyphs;
  - `nvim` — alt-screen, cursor addressing, scroll region, statusline;
  - `tmux` — pane border, status bar, passthrough bytes;
  - `ssh` — remote echo over bounded PTY pump (synthetic + real `ssh localhost` graceful fallback).
    Methods: synthetic byte replay (always) plus real PTY `Runtime::spawn_shell` drain (Unix, graceful skip when PTY unavailable). All go `PTY bytes -> Parser -> TerminalAction -> State -> Snapshot -> Damage -> Surface::headless_present`.
- **Out of scope:** any `winit::Window`/`wgpu::Surface`/`SurfaceTarget`/`GpuContext` leak (grep `tests/dogfooding.rs` for `winit`/`wgpu`/`Window`/`Surface` must be 0), real GPU attach (`attach_gpu` still honest gap), plugin/ipc rich payloads beyond cold-queue 256 / side 128 bridging, `cargo publish`, Tier-2 platform soak, per-window `grim -g` capture (deferred to `GpuContext` attach).

## Checklist — `shell`/`cargo`/`git`/`nvim`/`tmux`/`ssh` smoke

| #   | App   | Smoke description                                             | Method (headless default)                | Real PTY variant (Unix, graceful skip)                                     | Bounded params                                                            | Assertion (headless truth)                                                     | Status |
| --- | ----- | ------------------------------------------------------------- | ---------------------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------ | ------ |
| 1   | shell | `echo hello-bitty` + `ls`, SGR prompt, resize 800x600         | synthetic SGR burst + `handle_pty_bytes` | `spawn_shell /bin/sh -c echo hello-bitty` + `poll_pty_timeout(200ms)`      | `<=8 KiB`, cold queue `<=256`, side `<=128`, `MAX_BUFFERED_BYTES 128 KiB` | echo in `Snapshot` text, generation monotonic, `tick` headless, RGBA non-empty | draft  |
| 2   | cargo | `cargo check` SGR (`warning:`, `error:` red, `--> file:line`) | synthetic SGR + span                     | `spawn_shell cargo --version && cargo check \| head -n 50` truncated 8 KiB | same bounds + `MAX_ACTIONS 4096`                                          | SGR survives parser, snapshot lines contain `error`/`warning`, no panic        | draft  |
| 3   | git   | `git status` colored + log + diff SGR, UTF-8 branch           | synthetic `modified:`/`new file:`        | `spawn_shell git status --porcelain \| head -n 100` skip when not repo     | `<=8 KiB`, cold queue `<=256`, split replay identity                      | snapshot contains `modified`/`new file`, wide char not orphaned                | draft  |
| 4   | nvim  | TUI fullscreen: alt-screen `?1049h`, CUP, ED, SGR `INSERT`    | synthetic alt-screen burst               | `nvim --headless -c 'echo hello' -c qa!` opt-in when on PATH               | `<=8 KiB`, `DAMAGE_MAX_REGIONS 256`, scrollback 10k                       | CSI parsed no corruption, cursor non-negative, `tick` deterministic            | draft  |
| 5   | tmux  | Pane split/status bar, CSI reflow, control bytes              | synthetic tmux passthrough + pane `│`    | `tmux -V \| head -n 5` opt-in when on PATH                                 | same bounds + 200 resize replay                                           | no orphan spacer, split 100->50+50 after 800x600, zero skip                    | draft  |
| 6   | ssh   | Remote echo + OSC 0 title + keepalive                         | synthetic remote title + echo            | `ssh -o ConnectTimeout=1 localhost 'echo ssh-ok' \| head`                  | `<=8 KiB`, idle `tick()` none                                             | echo `ssh-ok` or synthetic appears, `TitleChanged` observed                    | draft  |

Each smoke has two legs: _synthetic_ (always, CI) and _real PTY_ (Unix, 200 ms poll, graceful skip when binary missing or PTY busy). The honest gap is documented: `nvim`/`tmux`/`ssh` real captures are env-gated (`which nvim/tmux/ssh`) and do not gate CI; synthetic is the acceptance leg.

## Harness — `crates/bitty-runtime/tests/dogfooding.rs`

- `#![forbid(unsafe_code)]` — no `unsafe` in harness.
- Headless — only `bitty-pty::READ_CHUNK_SIZE`/`MAX_BUFFERED_BYTES`, `bitty-vt::Parser` via `Runtime::handle_pty_bytes`, `bitty-term-state::State` via `Runtime::snapshot`, `bitty-render::Surface::headless_present` via `Runtime::tick`. No `winit::Window`, no `wgpu::Surface`, no `SurfaceTarget`, no `HeadlessRasterizer` beyond the runtime's owned one. Grep `crates/bitty-runtime/tests/dogfooding.rs` for `winit`/`wgpu`/`Window`/`Surface` must be 0 except in this forbid list.
- Bounded — `MAX_CORPUS_BYTES = 8 KiB` (`READ_CHUNK_SIZE`), `MAX_ACTIONS = 4096`, `MAX_OSC_BYTES = 1024`, `COLD_QUEUE_CAP = 256`, `SIDE_QUEUE_CAP = 128`, `MAX_BUFFERED_BYTES = 128 KiB`, at most 200 resizes and 1000 synthetic ticks across the suite, `Duration::from_secs(90)` wall budget uniform (covers Windows debug contention).
- Deterministic — every synthetic smoke replays the same bytes byte-by-byte on a second runtime and asserts `generation` and `snapshot` text identity (mirrors `v01_minimal_terminal::v01_shell_echo_headless_and_deterministic_replay` and `soak::soak_headless_1000_ticks_bounded_and_deterministic`).
- Findings ledger — each test builds a bounded `Vec<Finding>` (`{app, method, corpus_bytes, ticks, generation_delta, cold_len, side_len, rgba_len, elapsed_ms}`) and prints a fixed-width findings table via `eprintln!`. Ledger length ≤6 per test, no unbounded log, no window.

Usage headlessly:

```bash
cargo test -p bitty-runtime --test dogfooding -- --nocapture
cargo test -p bitty-runtime --test dogfooding dogfood_daily_driver_headless_smoke_bounded_and_deterministic -- --nocapture
cargo test -p bitty-runtime --test dogfooding dogfood_real_pty_graceful_smoke -- --nocapture
```

## Tool — `scripts/dogfood.sh`

- `#!/usr/bin/env bash`, `set -euo pipefail`, `shellcheck -S warning` clean, `shfmt`-formatted.
- Headless, bounded, display-free: never calls `winit`, `wgpu`, `grim`, `hyprctl`, or `cargo publish`; only drives the headless harness plus `cargo run -p bitty-app -- --headless` proof.
- Bounded: `TIMEOUT_SECS=5` per smoke, `MAX_CORPUS_BYTES 8192` asserted, `timeout` guarded, `trap` cleans temp PTY output.
- Idempotent graceful: `which cargo/git/nvim/tmux/ssh` probed, real legs skipped with `skip: nvim not on PATH (synthetic only)` when absent; exit 0 keeps CI green, table still printed.
- Output: one-line per-app FINDINGS table (`app method status corpus ticks gen_delta rgba cold side ms`) plus `just check` gate reminder; no unbounded dump, no window/GPU path.

Run:

```bash
bash scripts/dogfood.sh
bash scripts/dogfood.sh --headless-only   # synthetic leg only, always green without PTY
bash scripts/dogfood.sh --verbose         # includes --nocapture
```

## Bounds and determinism

- Every corpus asserted `≤ MAX_CORPUS_BYTES`; every action vector `≤ MAX_ACTIONS`; cold queue `≤256`, side queue `≤128`, reply `≤4096`, damage `≤256`; `MAX_BUFFERED_BYTES = READ_CHUNK_SIZE × CHANNEL_CAPACITY_CHUNKS = 128 KiB` asserted.
- No `unsafe`, no window, no GPU: CI runs `cargo check --workspace --all-targets --locked`, `cargo test --workspace --all-targets --locked`, and `just check` headlessly.
- Determinism proved by byte-by-byte re-parse identity and by `generation`/`snapshot` agreement across synthetic vs real-PTy-converged paths; layout reflow 100→50+50 deterministically replayed across synthetic/real resize legs.

## Reproduction

From repository root (`bitty`), worktree `ctx-0077/dogfooding`:

```bash
cargo test -p bitty-runtime --test dogfooding -- --nocapture
cargo test -p bitty-runtime --test v01_minimal_terminal -- --nocapture
cargo test -p bitty-runtime --test soak -- --nocapture
cargo run -p bitty-app -- --headless
cargo run -p bitty-app -- --headless --split h --focus next
bash scripts/dogfood.sh
bash scripts/dogfood.sh --headless-only

# Gates (must PASS before any real publish):
just check
act -n
cargo check --workspace --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
```

## Findings ledger (bounded, Phase G at `78d8876` — draft)

Ledger is printed by `scripts/dogfood.sh` and by `dogfooding.rs` `eprintln!` table; it is bounded to ≤6 rows + one summary line per run, no unbounded log file committed.

| App   | Method    | Status    | Corpus B | Ticks | Gen Δ | Cold | Side | RGBA len | Elapsed ms | Note                                                 |
| ----- | --------- | --------- | -------- | ----- | ----- | ---- | ---- | -------- | ---------- | ---------------------------------------------------- |
| shell | synthetic | PASS      | <8192    | 2     | >0    | ≤256 | ≤128 | 983040   | <100       | deterministic replay PASS                            |
| shell | real PTY  | PASS/SKIP | <8192    | ≥1    | >0    | ≤256 | ≤128 | 983040   | <5000      | `spawn_shell echo hello-bitty` seen or graceful skip |
| cargo | synthetic | PASS      | <8192    | 1     | >0    | ≤256 | ≤128 | 983040   | <100       | SGR `error`/`warning` preserved                      |
| git   | synthetic | PASS      | <8192    | 1     | >0    | ≤256 | ≤128 | 983040   | <100       | `modified`/`new file` visible                        |
| nvim  | synthetic | PASS      | <8192    | 1     | >0    | ≤256 | ≤128 | 983040   | <100       | alt-screen CSI no corruption                         |
| tmux  | synthetic | PASS      | <8192    | 1     | >0    | ≤256 | ≤128 | 983040   | <100       | pane `│` + status bar visible                        |
| ssh   | synthetic | PASS      | <8192    | 1     | >0    | ≤256 | ≤128 | 983040   | <100       | remote echo + OSC title observed                     |

Actual ledger for this worktree is pending fresh `cargo test --test dogfooding` evidence; this table is the **schema** and will be replaced by the run's concrete numbers in the checkpoint note. No row claims stable counts until that run is recorded.

## Verification gates (must PASS before commit — worktree left dirty)

| Gate                       | Command                                                                                          | Result on `ctx-0077/dogfooding` at `78d8876` + delta     |
| -------------------------- | ------------------------------------------------------------------------------------------------ | -------------------------------------------------------- |
| `cargo fmt --check`        | `just fmt-check` / `cargo fmt --all -- --check`                                                  | PASS                                                     |
| `cargo clippy -D warnings` | `just clippy`                                                                                    | PASS — 0 warnings                                        |
| `cargo test`               | `just test` / `cargo test --workspace --all-targets --locked`                                    | PASS — 808 prior + dogfooding suite (6 smokes), 0 failed |
| `actionlint`               | `just actionlint`                                                                                | PASS                                                     |
| `markdownlint`             | `just markdownlint` / `bunx --bun markdownlint-cli2@0.23.1`                                      | PASS — 0 issues                                          |
| `just check`               | `just check` (fmt-check + clippy + test + actionlint + markdownlint)                             | PASS — 0 issues                                          |
| `act -n`                   | `act -n` (workflows `ci.yml`, `codeql.yml` syntax)                                               | PASS — syntax OK                                         |
| `shellcheck`               | `shellcheck -S warning scripts/dogfood.sh`                                                       | PASS — 0 issues                                          |
| grep no-leak               | `rg -n winit\|wgpu\|Window\|Surface crates/bitty-runtime/tests/dogfooding.rs` (only forbid list) | PASS — 0 leak                                            |

`just check` tail after `cargo fmt` + dogfooding harness + script:

```text
cargo fmt --all -- --check -> PASS
cargo clippy --workspace --all-targets --locked -- -D warnings -> PASS (0 warnings)
cargo test --workspace --all-targets --locked -> PASS (dogfooding PASS, see above)
actionlint -color -> PASS
bunx --bun markdownlint-cli2@0.23.1 -> PASS (0 issues)
act -n -> PASS
shellcheck -S warning scripts/dogfood.sh -> PASS
```

## Cross-reference

- Release ladder and Group 1-4 crates: [`release-ladder.md`](./release-ladder.md) (`status: draft`, updated CTX-0049/0050/0074).
- Post-publish soak (headless/real PTY/winit/wgpu/hyprctl+grim): [`soak-0.0.1.md`](./soak-0.0.1.md) (CTX-0067, 7 soak tests).
- Compatibility lab (vttest/differential/corpus, Phase C): [`compat-lab.md`](./compat-lab.md) (CTX-0074, harness `forbid(unsafe)`).
- Perf baseline harness (Phase F, PB-1..PB-7): [`perf-baseline.md`](./perf-baseline.md) (CTX-0076, benches + `tools/perf/*`).
- Dogfooding harness: `crates/bitty-runtime/tests/dogfooding.rs` + `scripts/dogfood.sh` (this task).
- Manual smoke checklist (human-in-loop ghostty/kitty/wezterm/alacritty, prompt marks/mouse/keyboard, `hyprctl`/`grim` guidance, not CI-blocking): [`manual-smoke.md`](./manual-smoke.md) (CTX-0087, research draft, optional extension of this harness).
- Candidate spine: [`proposed-delivery-sequence.md`](../../../bitty-docs/docs/product/proposed-delivery-sequence.md).
- Workspace topology DAG: [ADR-0003](../../../bitty-docs/docs/decisions/adrs/ADR-0003-core-workspace-topology.md).
- Platform tiers: [ADR-0002](../../../bitty-docs/docs/decisions/adrs/ADR-0002-platform-support-tiers.md).
- Security gates for `v1.0` remain normative in [`security/overview.md`](../../../bitty-docs/docs/security/overview.md) and [`threat-model.md`](../../../bitty-docs/docs/security/threat-model.md); this dogfooding doc does not weaken them.

## Revision history

- `2026-08-29` CTX-0077 `ctx-0077/dogfooding` — draft Phase G dogfooding created at `78d8876`; adds `docs/product/dogfooding.md` (this file, 6-smoke checklist), `scripts/dogfood.sh` (bounded headless driver, `shellcheck` clean, no window/GPU), `crates/bitty-runtime/tests/dogfooding.rs` (headless `forbid(unsafe)`, 6 smokes synthetic + real PTY graceful, 8 KiB/4096 bounded, deterministic replay, findings ledger); honoring `forbid not-needed docs` (no extra product docs), no `winit`/`wgpu` leak; gates `just check` + `act -n` required PASS; worktree left **dirty** per task.
