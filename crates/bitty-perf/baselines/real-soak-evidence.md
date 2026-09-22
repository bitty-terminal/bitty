---
title: Real-Render Soak Automation (CTX-0642)
description: Automated and schedulable long-duration real-render evidence chain for bitty on Tier 1 Hyprland hosts, pairing the hyprctl+grim pixel leg with the DevTools-preferred grid-text leg plus per-capture RSS for the PB-3 anchor
category: product
audience: maintainer
document_type: research
status: draft
website_publish: false
sidebar_order: 53
---

<!-- markdownlint-disable MD025 -->

# Real-Render Soak Automation (CTX-0642)

## Status and provenance

- Status: **draft evidence**. Repository-owned automation plus an honest
  `unavailable` baseline for the long-duration real-render soak. It does not
  close PB-3/PB-7 and does not claim Verified; the accepted budgets remain
  arch constraints until reference hardware and corpora are pinned (PERF-01,
  OQ-100).
- Ownership: bitty **CTX-0642** — _Real-render soak automation_.
  - Priority: P1 | Area: perf | Labels: chore,P1,area:perf | Milestone: v0.1.0
  - Issue: #1063 (PERF-09) | Sub-issue of: #974 (performance epic)
  - RFC: `performance-budget-rfc.md` PB-3/PB-7 | Task: CTX-0642
  - Depends on: none
- Scope: an opt-in, bounded, schedulable chain that soaks a real `bitty`
  window for hours and collects periodic evidence through the pixel leg
  (`hyprctl` + `grim`, previously manual) and the DevTools-preferred
  introspection leg (`bitty ctl terminal text`, backed by the
  `bitty.debug/*` socket contract), plus per-capture RSS samples; a
  headless-safe planner bench; a committed evidence shape with an honest
  initial `unavailable` record; and bounded CI contract tests. It changes no
  accepted budget or gate threshold.
- Authority: canonical budgets live in the `bitty-terminal-docs` submodule
  (`docs/specifications/performance-budget-rfc.md`); this file is
  repository-local evidence and automation, in the same posture as
  `real-window-evidence.md` (CTX-0592).

## What this wave adds

Before this task the repository had:

- `crates/bitty-perf/src/real_window.rs` — opt-in PB-1/PB-2 measurement over
  seconds, but no long-duration leg.
- `crates/bitty-runtime/tests/soak.rs` — headless 1000-tick soak proving the
  bytes-to-present plumbing, with the `hyprctl` + `grim` capture leg
  explicitly unautomated (live Hyprland session required).
- `scripts/visual-smoke.sh` — the single-shot Hyprland capture workflow
  (build, launch on workspace 4, wait for first frame, one `grim`
  screenshot, close, restore focus), but no duration loop, no scheduler,
  and no evidence artifact.

After this task:

- `crates/bitty-perf/src/real_soak.rs` — soak config, bounded capture plan,
  capture-leg probing, RSS trend helpers, and schedule/evidence JSON
  builders. `#![forbid(unsafe_code)]`, Unix-gated where it touches
  processes or the display.
- `benches/real_soak.rs` — human-facing `harness = false` planner bench
  (prints the plan and leg status; `UNMEASURED` on headless CI).
- `crates/bitty-perf/tests/real_soak_evidence.rs` — bounded CI contract
  test (gate/leg discipline, plan math, artifact provenance).
- `crates/bitty-perf/baselines/pb-real-soak.json` — committed artifact,
  currently `unavailable` with the reason recorded (no fabricated numbers).
- `scripts/real-render-soak.sh` — the automated capture chain plus
  `--dry-run` planning and `--print-systemd` / `--print-cron` scheduling
  output.
- `scripts/tests/real-render-soak.test.sh` — headless contract test for
  the script (plan math, rejections, schedule rendering, `UNAVAILABLE`
  refusal).

## Measurement contract

- **Opt-in.** A live run requires `BITTY_PERF_REAL_SOAK=1`, a resolvable
  `bitty` binary (`--binary`, else the `target/<profile>` build the script
  produces), and a live Hyprland capture leg (`hyprctl`, `grim`, `jq` on
  `PATH` plus `HYPRLAND_INSTANCE_SIGNATURE`). Without all three the script
  exits 2 with `UNAVAILABLE` and writes nothing; headless CI reports
  `Unavailable` by design and remains green.
- **No fabricated numbers.** The committed artifact carries either measured
  captures or an explicit `unavailable` status with a reason. `--dry-run`
  writes schedules (`"status": "scheduled"`), never measurements.
- **Two legs per capture.** The pixel leg screenshots the compositor output
  with `grim`; the introspection leg snapshots grid text through
  `bitty ctl terminal text` (DevTools-preferred: no screenshot parsing, no
  new authority — the socket contract and scopes are owned by
  `bitty-ipc/src/devtools.rs`). Each capture additionally samples child
  RSS (`/proc/<pid>/status`, falling back to `ps -o rss=`).
- **Bounded and deterministic.** Duration (60..86400 s, default 14400 s =
  the PB-3 4 h window) and interval (30..3600 s, default 300 s) are clamped;
  the capture count never exceeds 512 (the interval widens instead, and the
  widening is reported). Every child is killed and reaped on every path;
  the window is closed and the previously focused workspace restored by an
  `EXIT` trap, so a window is never stranded.
- **Host context from the environment.** OS, arch, toolchain, CPU count,
  and total memory come from `uname`, `rustc --version`, `nproc`, and
  `/proc/meminfo`; no checkout path, username, or hostname is embedded.
  Evidence records screenshot file names only.
- **Local-only evidence.** Screenshots and grid snapshots may show shell
  output, so the script drives a fixed synthetic workload (`idle`, `mixed`,
  or `input-spam`) and writes everything under the caller-chosen output
  directory (default `recording/real-soak`, gitignored). Nothing is
  committed by a run; promoting an artifact to `baselines/` is an explicit,
  reviewed copy.
- **`#![forbid(unsafe_code)]`** for the Rust half; the script targets only
  its own child PID and window address (no pattern kills).

## Harness and commands

```text
# Plan only (headless-safe, launches nothing):
bash scripts/real-render-soak.sh --dry-run
bash scripts/real-render-soak.sh --dry-run --duration-secs 600 --interval-secs 300
cargo bench -p bitty-perf --bench real_soak -- --nocapture
just perf-real-soak

# Full Tier 1 run (Hyprland session + built binary required):
BITTY_PERF_REAL_SOAK=1 bash scripts/real-render-soak.sh --out-dir recording/real-soak
BITTY_PERF_REAL_SOAK=1 bash scripts/real-render-soak.sh --out-dir recording/real-soak \
  --duration-secs 14400 --interval-secs 300 --workload mixed --release
just perf-real-soak-run recording/real-soak

# Emit a reviewable schedule document from a dry run:
bash scripts/real-render-soak.sh --dry-run --write-schedule recording/real-soak/schedule.json
```

Environment knobs (all optional, clamped):

| Variable                        | Default              | Bound      | Purpose                                  |
| ------------------------------- | -------------------- | ---------- | ---------------------------------------- |
| `BITTY_PERF_REAL_SOAK`          | unset                | —          | `1` opts into live soak measurement      |
| `BITTY_PERF_SOAK_DURATION_SECS` | 14400 (4 h)          | 60..86400  | Soak wall time (bench planner + script)  |
| `BITTY_PERF_SOAK_INTERVAL_SECS` | 300 (5 min)          | 30..3600   | Capture cadence (bench planner + script) |
| `BITTY_PERF_SOAK_WORKSPACE`     | 4                    | 1..10      | Hyprland workspace hosting the window    |
| `BITTY_PERF_SOAK_WORKLOAD`      | mixed                | allowlist  | `idle`, `mixed`, or `input-spam`         |
| `BITTY_PERF_SOAK_OUT_DIR`       | recording/real-soak  | —          | Evidence directory (planner resolution)  |
| `BITTY_PERF_BIN`                | unset                | —          | Explicit binary path (`--binary`)        |

## Scheduling

The script renders scheduler definitions from caller-supplied directories
only — no checkout path is baked into the repository:

```text
# systemd user timer (daily): print, review, then install under
# ~/.config/systemd/user/ as bitty-real-soak.service/.timer.
bash scripts/real-render-soak.sh --print-systemd \
  --repo-dir <repo-checkout> --out-dir <evidence-dir> \
  --duration-secs 14400 --interval-secs 300

# cron alternative (daily 02:00): print, review, then crontab -e.
bash scripts/real-render-soak.sh --print-cron \
  --repo-dir <repo-checkout> --out-dir <evidence-dir>
```

Each run writes a timestamped `run-<UTC-timestamp>/` subdirectory, so
scheduled runs never clobber each other. Rotation and retention of old runs
is the operator's policy (evidence is gitignored scratch, not history).

## Evidence artifact

A completed run directory holds:

- `soak-evidence.json` — same shape as
  `bitty_perf::real_soak::evidence_json`: schema version, task/issues,
  capture date, revision, command (knobs only, no paths), profile, budget
  reference, host context, and the `soak` block with status, duration,
  effective interval, workspace, workload, completed captures, RSS
  first/last/max plus growth percent against the 250 MB PB-3 budget, and
  the per-capture rows (index, offset, screenshot file name, RSS, grid
  snapshot bytes, driver flag).
- `captures.csv` — the same rows in tabular form for quick plotting.
- `capture-NNNN.png` — pixel-leg screenshots (local only, never committed).
- `capture-NNNN.grid.json` — introspection-leg snapshots (local only).
- `bitty.log` — child stdout for first-frame and failure diagnosis.

The committed `baselines/pb-real-soak.json` is the last reviewed record;
it is `unavailable` until a Tier 1 run completes and a reviewer promotes a
run's `soak-evidence.json` with refreshed provenance.

## Verification

```text
cargo test -p bitty-perf --test real_soak_evidence --locked   # gate/plan/provenance contract
cargo test -p bitty-perf --lib --locked                        # plan math + trend helpers
cargo bench -p bitty-perf --bench real_soak --no-run --locked  # planner compiles headlessly
bash scripts/tests/real-render-soak.test.sh                    # script contract (headless)
bash scripts/real-render-soak.sh --dry-run                     # plan without a display
just check                                                     # fmt + clippy -D warnings + test + lint
BITTY_PERF_REAL_SOAK=1 just perf-real-soak-run recording/real-soak  # Tier 1 only
```

Expected: on headless CI the bench prints `UNMEASURED` and exits 0; both
contract tests pass with no display.

## Limitations and confidence

- The committed artifact is `unavailable`: the automation is landed and
  tested, but no Tier 1 long-duration capture has completed on this branch
  yet. The first scheduled run plus a reviewed promotion closes that gap.
- One window, one compositor (Hyprland), synthetic workload: this anchors
  the PB-3 typical-session trend and the PB-7 idle discipline on one
  machine; it is not a multi-host study and not a real-user session.
- The workload driver (`bitty ctl terminal send`) is best-effort per
  capture: a denied scope or missing socket records `driver_ok=false` on
  that row instead of aborting the soak, so pixel and RSS evidence survive
  a driver outage. Rows with `driver_ok=false` are excluded from
  input-responsiveness claims.
- `grim` captures the compositor output, not the `bitty` surface alone;
  pixel captures prove rendering happened but are not cropped or
  interpreted — the grid-text leg is the machine-checkable leg.

## Reconciliation with the product docs

The canonical product rows live in the `bitty-terminal-docs` submodule.
This repository cannot edit that external content here; the reconciliation
is: this document plus `crates/bitty-perf/baselines/pb-real-soak.json` are
the repository-local PERF-09 automation record, and a follow-up in the docs
repository should link the chain once the first Tier 1 run is promoted. No
row is claimed Verified.

## Affected contracts

No budget, gate threshold, or normative control changes. The accepted
PB-3/PB-7 values in `performance-budget-rfc.md` are untouched (PERF-01 /
OQ-100 owns the decision to turn them into hard gates). The headless soak
in `crates/bitty-runtime/tests/soak.rs` keeps its CI role; its header now
points at this chain as the automated real-render half.

## References

- `docs/specifications/performance-budget-rfc.md#pb-3-typical-session-memory`
  and `#pb-7-idle-cpu`
- `crates/bitty-perf/baselines/real-window-evidence.md` (CTX-0592 posture
  this chain reuses: opt-in, `Unavailable` on CI, no fabricated numbers)
- `scripts/visual-smoke.sh` (single-shot capture workflow this chain loops)
- `crates/bitty-ipc/src/devtools.rs` (socket contract behind the
  introspection leg)
