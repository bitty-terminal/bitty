# Dogfood session evidence (CTX-0643, PERF-10)

Continuous daily-driver session automation: a bounded run of a real `bitty`
window on a Tier 1 Hyprland host that cycles through all six daily-driver
surfaces (shell, cargo, git, nvim, tmux, ssh) every cycle and collects
per-cycle evidence through the pixel leg (`hyprctl` + `grim`), the
DevTools-preferred introspection leg (`bitty ctl terminal text`), and RSS
sampling for the PB-3 typical-session anchor (250 MB) and PB-7 idle CPU.

## Tenets (shared with the PERF-09 soak chain)

- **Opt-in only.** Live runs require `BITTY_PERF_DOGFOOD_SESSION=1`, a
  resolvable `bitty` binary, and a Hyprland session with
  `hyprctl`/`grim`/`jq`. Without all three every entry point refuses
  honestly (`UNAVAILABLE`, exit 2 for the script) and CI stays green.
- **Bounded.** Duration 60..86400 s (default 14400 = 4 h), cycle cadence
  60..3600 s (default 600 = 10 min), at most 256 cycles per run — the
  cadence widens instead of overflowing.
- **Local-only evidence.** Cycle captures may show shell output. Runs write
  timestamped run dirs under a caller-chosen directory (default
  `recording/dogfood-session`, gitignored): `cycles.csv`,
  `session-evidence.json`, per-cycle PNG + grid JSON, and the bitty log.
  Screenshots are file names only inside the JSON; no absolute host path is
  recorded. Promoting an artifact to this directory is an explicit,
  reviewed copy.

## Usage

```text
# Headless-safe plan (always green, no display):
cargo bench -p bitty-perf --bench dogfood_session -- --nocapture
cargo bench -p bitty-perf --bench dogfood_session -- --nocapture --write-schedule /tmp/session-plan.json

# Contract tests (headless CI):
cargo test -p bitty-perf --test dogfood_session_evidence

# Full chain (Tier 1 host only):
BITTY_PERF_DOGFOOD_SESSION=1 just perf-dogfood-session-run recording/dogfood-session

# Script contract (headless-safe):
just dogfood-session-test

# Continuous headless session proof (no display):
cargo test -p bitty-runtime --test dogfooding dogfood_daily_driver_session_continuous_bounded
```

## Per-cycle driver

Each cycle drives every selected app once through `bitty ctl terminal
send` with a fixed synthetic command (versions and short status probes —
no network, no side effects beyond the scratch shell), then records one
pixel capture, one RSS sample, and one grid-text snapshot. A cycle with a
missed leg records `driver_ok: false` instead of failing the run; the
window is never stranded (EXIT trap closes it and restores the previously
focused workspace). Scheduling uses the script's `--print-systemd` /
`--print-cron` emitters, same as the soak chain.

## Evidence shape

`session-evidence.json` carries a `session` section: `status`
(`measured` | `unavailable`), the clamped plan, the driven `apps`,
`completed_cycles`, RSS first/last/max/growth, `budget_mb: 250`, and the
per-cycle ledger (`index`, `at_secs`, `screenshot` basename, `rss_mb`,
`grid_text_bytes`, `apps_driven`, `driver_ok`).

## Status

`pb-dogfood-session.json` is `unavailable`: CTX-0643 landed the
automation; no Tier 1 daily-driver session has completed on this branch
yet. Replacing it with a `measured` artifact requires a completed live
run on a Tier 1 host plus independent review.
