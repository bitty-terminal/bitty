#!/usr/bin/env bash
# mem-probe-headless.sh — headless RSS/PSS/thread/fd probe for Bitty panels.
#
# Companion to mem-measure.sh (issue #1809 / CTX-1036). That script measures a
# headed single-panel session on a live compositor; this one measures the real
# runtime through `--test-mode` with no display, GPU, or window manager, so a
# regression run is reproducible on a headless host and does not touch the
# operator's desktop (issue #2524).
#
# What it does: creates an isolated XDG tree, writes a throwaway `init.lua`
# with the requested scrollback, starts `bitty --test-mode` on a private
# socket, splits to the requested panel count through `bitty ctl`, runs a
# scrollback-filling command in every live pane, optionally feeds one
# Kitty-protocol image payload into pane 1, then samples
# /proc/PID/status + smaps_rollup + fd/task counts on a fixed interval.
#
# CSV columns: t_s,rss_kb,vsize_kb,pss_kb,anon_kb,file_kb,shmem_kb,threads,fds.
# `rss_kb` (VmRSS) and `pss_kb` are resident; `vsize_kb` (VmSize) is virtual
# address space and is reported separately so a VSZ claim can be reproduced
# without being conflated with memory pressure. Optional RSS/PSS gates read
# their respective resident peaks only, never VmSize.
#
# Isolation and process safety:
# - HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME, XDG_CACHE_HOME, and
#   XDG_RUNTIME_DIR all point into one `mktemp -d` tree; the parent process's
#   desktop session and config are never read or written.
# - Only the PID started by this run is signalled, and only after its
#   environment still carries this run's BITTY_INSTANCE_ID. No pkill/killall.
#
# Evidence stays local under the out dir: `samples.csv`, `session.log`,
# `status.txt`, `summary.txt`. Use `memory-summary.sh`-style diffing (or any
# CSV reader) to compare two runs; comparability requires the same panel
# count, configured scrollback, generated lines per pane, image payload,
# sample interval, and binary build flags.
#
# Gates (optional): `--max-rss-kb N` and `--max-pss-kb N` independently exit 1
# when their respective resident-memory peaks exceed the caller-owned budget.
# VmSize is reported for analysis but never used as a resident-memory gate.
#
# Usage:
#   scripts/mem-probe-headless.sh --binary target/release/bitty [options]
#   scripts/mem-probe-headless.sh --help
#
# scratch-paths-exempt: mktemp owns the isolated tree; no host path literal.

set -euo pipefail

BIN=""
PANELS=1
SCROLLBACK=5000
SECONDS_TO_RUN=60
SAMPLE_INTERVAL=10
IMAGE=""
OUT_DIR=""
MAX_RSS_KB=0
MAX_PSS_KB=0
DRY_RUN=0
KEEP=0

usage() {
  sed -n '2,40p' "$0"
  cat <<'EOF'

Options:
  --binary PATH        bitty executable (default: <repo>/target/release/bitty)
  --panels N           pane count 1..8 (default: 1)
  --scrollback N       terminal.scrollback lines 0..100000 (default: 5000)
  --image PATH         optional Kitty-protocol payload file fed into pane 1
  --seconds N          observation window 0..3600 (default: 60)
  --interval N         sample interval 1..60 seconds (default: 10)
  --max-rss-kb N       optional gate: exit 1 when final/peak VmRSS exceeds N
  --max-pss-kb N       optional resident gate: exit 1 when peak PSS exceeds N
  --out-dir DIR        evidence directory (default: <tmp>/bitty-mem-probe-XXXX)
  --keep               keep the isolated tree (default: removed on exit)
  --dry-run            print the plan; launch nothing
  --help               this text
EOF
}

die() {
  echo "mem-probe-headless: $1" >&2
  exit 2
}

is_uint() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
  --binary)
    BIN="${2:-}"
    shift 2
    ;;
  --panels)
    PANELS="${2:-}"
    shift 2
    ;;
  --scrollback)
    SCROLLBACK="${2:-}"
    shift 2
    ;;
  --image)
    IMAGE="${2:-}"
    shift 2
    ;;
  --seconds)
    SECONDS_TO_RUN="${2:-}"
    shift 2
    ;;
  --interval)
    SAMPLE_INTERVAL="${2:-}"
    shift 2
    ;;
  --max-rss-kb)
    MAX_RSS_KB="${2:-}"
    shift 2
    ;;
  --max-pss-kb)
    MAX_PSS_KB="${2:-}"
    shift 2
    ;;
  --out-dir)
    OUT_DIR="${2:-}"
    shift 2
    ;;
  --keep)
    KEEP=1
    shift
    ;;
  --dry-run)
    DRY_RUN=1
    shift
    ;;
  --help | -h)
    usage
    exit 0
    ;;
  *)
    die "unknown flag $1"
    ;;
  esac
done

is_uint "$PANELS" || die "--panels must be a positive integer"
[ "$PANELS" -ge 1 ] && [ "$PANELS" -le 8 ] || die "--panels must be 1..8"
is_uint "$SCROLLBACK" || die "--scrollback must be a non-negative integer"
[ "$SCROLLBACK" -le 100000 ] || die "--scrollback must be <= 100000"
is_uint "$SECONDS_TO_RUN" || die "--seconds must be a non-negative integer"
[ "$SECONDS_TO_RUN" -le 3600 ] || die "--seconds must be <= 3600"
is_uint "$SAMPLE_INTERVAL" || die "--interval must be a positive integer"
[ "$SAMPLE_INTERVAL" -ge 1 ] && [ "$SAMPLE_INTERVAL" -le 60 ] ||
  die "--interval must be 1..60"
is_uint "$MAX_RSS_KB" || die "--max-rss-kb must be a non-negative integer"
is_uint "$MAX_PSS_KB" || die "--max-pss-kb must be a non-negative integer"
[ -z "$IMAGE" ] || [ -f "$IMAGE" ] || die "no such image payload: $IMAGE"

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
if [ -z "$BIN" ]; then
  BIN="$ROOT/target/release/bitty"
fi
[ -x "$BIN" ] || die "no executable binary at $BIN (pass --binary or build release)"
BIN="$(readlink -f "$BIN")"

if [ "$DRY_RUN" = "1" ]; then
  echo "mem-probe-headless: plan bin=$BIN panels=$PANELS scrollback=$SCROLLBACK"
  echo "  seconds=$SECONDS_TO_RUN interval=$SAMPLE_INTERVAL image=${IMAGE:-none}"
  echo "  max_rss_kb=$MAX_RSS_KB max_pss_kb=$MAX_PSS_KB out=${OUT_DIR:-<mktemp -d>}"
  exit 0
fi

if [ -z "$OUT_DIR" ]; then
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bitty-mem-probe-XXXXXX")"
else
  mkdir -p "$OUT_DIR"
  RUN_DIR="$OUT_DIR"
fi
ISOLATED="$(mktemp -d "${TMPDIR:-/tmp}/bitty-mem-iso-XXXXXX")"
CONFIG_DIR="$ISOLATED/config/bitty"
mkdir -p "$CONFIG_DIR" "$ISOLATED/data" "$ISOLATED/state" "$ISOLATED/cache" \
  "$ISOLATED/run" "$ISOLATED/home"
chmod 700 "$ISOLATED/run"
printf 'return { terminal = { scrollback = %s } }\n' "$SCROLLBACK" \
  >"$CONFIG_DIR/init.lua"

SOCK="$ISOLATED/run/sock"
INSTANCE="mem-probe-$$"
APP_PID=""
PSS_SAMPLE_FAILED=0
SAMPLES="$RUN_DIR/samples.csv"
LOG="$RUN_DIR/session.log"

cleanup() {
  if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
    # Only signal the process this run started and can still identify.
    if tr '\0' '\n' <"/proc/$APP_PID/environ" 2>/dev/null |
      grep -Fqx "BITTY_INSTANCE_ID=$INSTANCE"; then
      kill -TERM "$APP_PID" 2>/dev/null || true
      for _ in $(seq 1 30); do
        kill -0 "$APP_PID" 2>/dev/null || break
        sleep 0.1
      done
      kill -KILL "$APP_PID" 2>/dev/null || true
    fi
  fi
  if [ "$KEEP" = "1" ]; then
    echo "mem-probe-headless: kept $ISOLATED"
  else
    rm -rf "$ISOLATED"
  fi
}
trap cleanup EXIT

export BITTY_SOCKET="$SOCK" BITTY_INSTANCE_ID="$INSTANCE"
export HOME="$ISOLATED/home"
export XDG_CONFIG_HOME="$ISOLATED/config" XDG_DATA_HOME="$ISOLATED/data"
export XDG_STATE_HOME="$ISOLATED/state" XDG_CACHE_HOME="$ISOLATED/cache"
export XDG_RUNTIME_DIR="$ISOLATED/run"
export TERM=xterm-256color
unset WAYLAND_DISPLAY DISPLAY SHELL

"$BIN" --test-mode --log-level info --config "$CONFIG_DIR/init.lua" \
  >"$LOG" 2>&1 &
APP_PID=$!

for _ in $(seq 1 300); do
  [ -S "$SOCK" ] && break
  kill -0 "$APP_PID" 2>/dev/null || die "bitty exited during startup (see $LOG)"
  sleep 0.05
done
[ -S "$SOCK" ] || die "test-mode socket did not appear (see $LOG)"

ctl() { "$BIN" ctl --socket "$SOCK" "$@" >/dev/null; }

echo "t_s,rss_kb,vsize_kb,pss_kb,anon_kb,file_kb,shmem_kb,threads,fds" >"$SAMPLES"
sample() {
  local t="$1" rss vsize pss anon file shmem threads fds
  rss="$(awk '/^VmRSS:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  vsize="$(awk '/^VmSize:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  anon="$(awk '/^RssAnon:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  file="$(awk '/^RssFile:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  shmem="$(awk '/^RssShmem:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  threads="$(awk '/^Threads:/{print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
  pss="$(awk '/^Pss:/{print $2}' "/proc/$APP_PID/smaps_rollup" 2>/dev/null || true)"
  if [ -z "$pss" ]; then
    if [ "$MAX_PSS_KB" -gt 0 ]; then PSS_SAMPLE_FAILED=1; fi
    pss=0
  fi
  fds="$(ls "/proc/$APP_PID/fd" 2>/dev/null | wc -l)"
  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$t" "$rss" "$vsize" "$pss" "$anon" "$file" "$shmem" "$threads" "$fds" >>"$SAMPLES"
}

# A short settle after startup, then the requested panel count.
sleep 1
i=1
while [ "$i" -lt "$PANELS" ]; do
  ctl view split --right
  i=$((i + 1))
  sleep 0.2
done
sleep 1

# Fail closed if any split did not create all requested pane sessions. A
# successful `view split` can leave an empty leaf when shell spawn fails.
TERMINALS_JSON="$RUN_DIR/terminals.json"
"$BIN" ctl --socket "$SOCK" terminal list >"$TERMINALS_JSON"
ACTUAL_PANELS="$(jq '.terminals | length' "$TERMINALS_JSON")"
LIVE_PANELS="$(jq '[.diagnostics[] | select(.has_session)] | length' "$TERMINALS_JSON")"
[ "$ACTUAL_PANELS" -eq "$PANELS" ] || die "expected $PANELS panes, got $ACTUAL_PANELS (see $TERMINALS_JSON)"
for _ in $(seq 1 30); do
  [ "$LIVE_PANELS" -eq "$PANELS" ] && break
  sleep 0.2
  "$BIN" ctl --socket "$SOCK" terminal list >"$TERMINALS_JSON"
  LIVE_PANELS="$(jq '[.diagnostics[] | select(.has_session)] | length' "$TERMINALS_JSON")"
done
[ "$LIVE_PANELS" -eq "$PANELS" ] || die "expected $PANELS live pane sessions, got $LIVE_PANELS (see $TERMINALS_JSON)"

# A layout split creates a distinct live shell per pane. Fill every one rather
# than claiming the configured scrollback limit is populated merely because
# the panes exist. Generate beyond the history cap so the visible rows do not
# prevent all panes from reaching the configured scrollback limit.
PANE_IDS="$(jq -r '.diagnostics[] | select(.has_session) | .terminal_id' "$TERMINALS_JSON")"
[ "$SCROLLBACK" -eq 0 ] && GENERATED_LINES=1 || GENERATED_LINES="$((SCROLLBACK + 100))"
while IFS= read -r pane_id; do
  pane_num="${pane_id#t:}"
  ctl view focus "v:$pane_num"
  ctl terminal send "$pane_id" "seq 1 $GENERATED_LINES"$'\r'
  sleep 0.2
done <<<"$PANE_IDS"
sleep 2

# Wait for each shell's final marker, then confirm the scrollback reached the
# configured cap in every live pane before taking memory samples.
while IFS= read -r pane_id; do
  pane_num="${pane_id#t:}"
  ctl view focus "v:$pane_num"
  ctl terminal send "$pane_id" "echo BITTY_MEM_PROBE_DONE_$pane_num"$'\r'
done <<<"$PANE_IDS"
sleep 1
"$BIN" ctl --socket "$SOCK" terminal list >"$TERMINALS_JSON"
[ "$SCROLLBACK" -eq 0 ] && REQUIRED_LINES=0 || REQUIRED_LINES="$SCROLLBACK"
for _ in $(seq 1 30); do
  SHORT_PANELS="$(jq --argjson required "$REQUIRED_LINES" '[.diagnostics[] | select(.has_session and .scrollback_lines < $required)] | length' "$TERMINALS_JSON")"
  [ "$SHORT_PANELS" -eq 0 ] && break
  sleep 0.2
  "$BIN" ctl --socket "$SOCK" terminal list >"$TERMINALS_JSON"
done
[ "$SHORT_PANELS" -eq 0 ] || die "scrollback was not populated to $REQUIRED_LINES lines in every live pane (see $TERMINALS_JSON)"

if [ -n "$IMAGE" ]; then
  # Feed the payload through the pane's shell so it crosses the PTY parser
  # (the same path a previewer uses), then let the decoder settle.
  ctl terminal send "t:1" "cat '$IMAGE'; echo IMAGE_DONE"$'\r'
  sleep 2
fi

START="$(date +%s)"
sample 0
while [ $(($(date +%s) - START)) -lt "$SECONDS_TO_RUN" ]; do
  sleep "$SAMPLE_INTERVAL"
  kill -0 "$APP_PID" 2>/dev/null || die "bitty exited during observation (see $LOG)"
  sample $(($(date +%s) - START))
done

grep -E 'VmRSS|VmHWM|VmSize|RssAnon|RssFile|RssShmem|Threads' \
  "/proc/$APP_PID/status" >"$RUN_DIR/status.txt" 2>/dev/null || true
if [ "$MAX_PSS_KB" -gt 0 ] && [ "$PSS_SAMPLE_FAILED" -eq 1 ]; then
  die "PSS gate enabled but one or more smaps_rollup samples could not be read"
fi
FINAL_RSS="$(awk -F, 'END{print $2}' "$SAMPLES")"
FINAL_VSIZE="$(awk -F, 'END{print $3}' "$SAMPLES")"
FINAL_PSS="$(awk -F, 'END{print $4}' "$SAMPLES")"
PEAK_RSS="$(awk -F, 'NR > 1 && $2 > max {max = $2} END{print max + 0}' "$SAMPLES")"
PEAK_VSIZE="$(awk -F, 'NR > 1 && $3 > max {max = $3} END{print max + 0}' "$SAMPLES")"
PEAK_PSS="$(awk -F, 'NR > 1 && $4 > max {max = $4} END{print max + 0}' "$SAMPLES")"
FINAL_THREADS="$(awk -F, 'END{print $8}' "$SAMPLES")"
FINAL_FDS="$(awk -F, 'END{print $9}' "$SAMPLES")"
{
  echo "BIN=$BIN"
  echo "PANELS=$PANELS"
  echo "SCROLLBACK=$SCROLLBACK"
  echo "IMAGE=${IMAGE:-none}"
  echo "SECONDS=$SECONDS_TO_RUN"
  echo "SAMPLE_INTERVAL=$SAMPLE_INTERVAL"
  echo "FINAL_RSS_KB=$FINAL_RSS"
  echo "PEAK_RSS_KB=$PEAK_RSS"
  echo "FINAL_VSIZE_KB=$FINAL_VSIZE"
  echo "PEAK_VSIZE_KB=$PEAK_VSIZE"
  echo "FINAL_PSS_KB=$FINAL_PSS"
  echo "PEAK_PSS_KB=$PEAK_PSS"
  echo "FINAL_THREADS=$FINAL_THREADS"
  echo "FINAL_FDS=$FINAL_FDS"
  echo "SAMPLES=$SAMPLES"
  if [ "$MAX_RSS_KB" -gt 0 ]; then
    if [ "$PEAK_RSS" -le "$MAX_RSS_KB" ]; then
      echo "GATE=PASS (peak ${PEAK_RSS}kB <= ${MAX_RSS_KB}kB)"
    else
      echo "GATE=FAIL (peak ${PEAK_RSS}kB > ${MAX_RSS_KB}kB)"
    fi
  fi
  if [ "$MAX_PSS_KB" -gt 0 ]; then
    if [ "$PEAK_PSS" -le "$MAX_PSS_KB" ]; then
      echo "PSS_GATE=PASS (peak ${PEAK_PSS}kB <= ${MAX_PSS_KB}kB)"
    else
      echo "PSS_GATE=FAIL (peak ${PEAK_PSS}kB > ${MAX_PSS_KB}kB)"
    fi
  fi
} | tee "$RUN_DIR/summary.txt"

if { [ "$MAX_RSS_KB" -gt 0 ] && [ "$PEAK_RSS" -gt "$MAX_RSS_KB" ]; } ||
  { [ "$MAX_PSS_KB" -gt 0 ] && [ "$PEAK_PSS" -gt "$MAX_PSS_KB" ]; }; then
  exit 1
fi
