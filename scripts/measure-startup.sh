#!/usr/bin/env bash
# measure-startup.sh — windowed PB-1 launch samples (p50/p99 over N runs).
#
# PB-1 is the real-window launch-to-first-frame budget in
# docs/specifications/performance-budget-rfc.md#pb-1: <= 100 ms p50 and
# <= 200 ms p99 on the reference desktop. A shell script cannot observe the
# first presented frame, so this harness times `bitty --headless` (one
# headless tick, then exit) as a *launch proxy*: process start, argument and
# config parsing, font discovery + index reconciliation, runtime and grid
# renderer construction, and the first tick are inside the number; GPU/window
# creation and presentation are not. Use `tools/perf/startup` for the
# instrumented real-window phase timeline; this harness exists to compare
# warm-index launch behaviour across builds on one machine.
#
# Warm-index condition: every launch shares one isolated XDG cache tree. The
# first launch builds `font-index-v2` (reported separately as `cold`), and
# the sampled window starts at launch 2, so `p50`/`p99` measure the
# warm-index path. `--cold` deletes the index before every launch instead and
# reports a cold window.
#
# Sampling: N launches (default 30), nearest-rank percentiles
# (p50 = ceil(0.50 * N), p99 = ceil(0.99 * N)), wall-clock milliseconds
# around the child process. This is developer evidence, not a CI gate:
# nothing in `just check` or the release workflow calls it.
#
# Isolation and process safety: HOME, XDG_CONFIG_HOME, XDG_DATA_HOME,
# XDG_STATE_HOME, XDG_CACHE_HOME, and XDG_RUNTIME_DIR all point into one
# `mktemp -d` tree; the child is only waited on (never signalled), so the
# operator's desktop, config, and cache are untouched.
#
# Usage:
#   scripts/measure-startup.sh [--binary PATH] [--runs N] [--cold]
#   scripts/measure-startup.sh --help
#
# scratch-paths-exempt: mktemp owns the isolated tree; no host path literal.

set -euo pipefail
export LC_ALL=C

RUNS=30
BIN=""
COLD=0

usage() {
  sed -n '2,35p' "$0"
  cat <<'EOF'

Options:
  --binary PATH  bitty executable (default: <repo>/target/release/bitty)
  --runs N       sampled launches 1..10000 (default: 30)
  --cold         delete font-index-v2 before every launch
  -h, --help     show this help
EOF
}

fail() {
  echo "measure-startup: $*" >&2
  exit 2
}

while (($# > 0)); do
  case "$1" in
  --binary)
    (($# >= 2)) || fail "--binary needs a path"
    BIN="$2"
    shift 2
    ;;
  --runs)
    (($# >= 2)) || fail "--runs needs a count"
    RUNS="$2"
    shift 2
    ;;
  --cold)
    COLD=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    fail "unknown argument: $1 (use --help)"
    ;;
  esac
done

[[ "$RUNS" =~ ^[0-9]+$ ]] || fail "--runs must be a number"
((RUNS >= 1 && RUNS <= 10000)) || fail "--runs must be 1..10000"

if [[ -z "${EPOCHREALTIME:-}" ]]; then
  fail "bash 5+ is required (EPOCHREALTIME)"
fi

if [[ "${BITTY_FONT_INDEX:-}" == "0" ]]; then
  fail "BITTY_FONT_INDEX=0 disables the persistent index, so the warm-index condition cannot hold"
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
if [[ -z "$BIN" ]]; then
  BIN="${repo_root}/target/release/bitty"
fi
[[ -x "$BIN" ]] || fail "binary not executable: $BIN (build it with: cargo build --release -p bitty-terminal)"

work="$(mktemp -d)"
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT

export HOME="${work}/home"
export XDG_CONFIG_HOME="${work}/config"
export XDG_DATA_HOME="${work}/data"
export XDG_STATE_HOME="${work}/state"
export XDG_CACHE_HOME="${work}/cache"
export XDG_RUNTIME_DIR="${work}/run"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

index_path="${XDG_CACHE_HOME}/bitty/font-index-v2"

now_ms() {
  local stamp=$EPOCHREALTIME
  local secs=${stamp%%.*}
  local frac=${stamp#*.}
  printf '%s' "$((secs * 1000 + 10#$frac / 1000))"
}

time_launch() {
  local start end
  start="$(now_ms)"
  if ! "$BIN" --headless >/dev/null 2>&1; then
    fail "launch failed: $BIN --headless"
  fi
  end="$(now_ms)"
  printf '%s' "$((end - start))"
}

echo "measure-startup: windowed PB-1 launch proxy (bitty --headless), not the real-window gate"
echo "binary: $BIN"
echo "budget: p50 <= 100 ms / p99 <= 200 ms (real window, performance-budget-rfc.md#pb-1)"

cold_ms=""
if ((COLD)); then
  echo "warm-index condition: OFF (--cold removes font-index-v2 before every launch)"
else
  rm -f "$index_path"
  cold_ms="$(time_launch)"
  echo "warm-index condition: ON (launch 1 builds the index; samples are warm)"
  echo "cold launch: ${cold_ms}ms (index build, excluded from the window)"
fi

samples=()
for ((run = 0; run < RUNS; run++)); do
  if ((COLD)); then
    rm -f "$index_path"
  fi
  samples+=("$(time_launch)")
done

printf '%s\n' "${samples[@]}" | sort -n | awk '
  { values[NR] = $1; total += $1 }
  END {
    n = NR
    p50 = int((n * 50 + 99) / 100); if (p50 < 1) p50 = 1
    p99 = int((n * 99 + 99) / 100); if (p99 < 1) p99 = 1
    printf "samples=%d p50=%dms p99=%dms mean=%.1fms max=%dms\n", n, values[p50], values[p99], total / n, values[n]
    printf "budget (proxy only, not the real-window gate): %s\n", (values[p50] <= 100 && values[p99] <= 200) ? "within p50/p99 budget" : "above p50/p99 budget"
  }'
echo "note: a headless launch is a proxy; rerun tools/perf/startup for real-window PB-1 phases."
