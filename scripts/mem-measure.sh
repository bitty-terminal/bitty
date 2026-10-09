#!/usr/bin/env bash
# mem-measure.sh — Headed idle-memory probe with interim gate (CTX-1036, issue #1809).
#
# Method (matches the #1809 comments): Hyprland-headed single idle shell
# panel, 12pt, empty scrollback; launch on workspace 5 silent; settle 10 s
# (ONE sleep, no poll loop); read /proc/PID/status (VmRSS/VmSize/RssAnon/
# RssFile/RssShmem/Threads) + smaps PSS/USS + pmap -x; one grim shot; kill
# only the recorded PID (+ recorded direct children, no pkill); restore the
# previously focused workspace.
#
# Evidence stays local and uncommitted under --out-dir (default
# recording/mem, gitignored): status.txt, pss.txt, uss.txt, pmap.txt,
# topmappings.txt, smaps.txt, shot.png, summary.txt.
#
# Interim gate: idle single-panel main-process RSS at or below 250 MB
# (ghostty parity, ~40% cut from the ~421 MB baseline) toward the 150 MB
# budget. Exits 1 with GATE=FAIL when over budget, 0 with GATE=PASS inside.
# The headless-CI twin of this gate is the policy pin in
# crates/bitty-render/tests/memory_budget.rs (a default flipped back fails
# CI); this script is the headed numeric leg for local runs.
#
# Usage:
#   bash scripts/mem-measure.sh [--binary PATH] [--build] [--workspace ID]
#       [--settle-secs N] [--font-size PTS] [--out-dir DIR]
#   bash scripts/mem-measure.sh --help
#   bash scripts/mem-measure.sh --dry-run [--binary PATH] [--workspace ID] ...

set -euo pipefail

OUT_DIR="recording/mem"
MEM_WS="5"
SETTLE_SECS=10
FONT_SIZE="12"
BIN_OVERRIDE=""
BUILD=0
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
  --out-dir)
    OUT_DIR="${2:-}"
    shift 2
    ;;
  --workspace)
    MEM_WS="${2:-}"
    shift 2
    ;;
  --settle-secs)
    SETTLE_SECS="${2:-10}"
    shift 2
    ;;
  --font-size)
    FONT_SIZE="${2:-12}"
    shift 2
    ;;
  --binary)
    BIN_OVERRIDE="${2:-}"
    shift 2
    ;;
  --build)
    BUILD=1
    shift
    ;;
  --dry-run)
    DRY_RUN=1
    shift
    ;;
  --help | -h)
    sed -n '2,24p' "$0"
    echo "  --binary PATH   explicit bitty binary (default: target/release/bitty)"
    echo "  --build         cargo build -p bitty-terminal --release first"
    echo "  --workspace ID  Hyprland workspace 1..10 (default 5)"
    echo "  --settle-secs N idle settle 5..60, single sleep (default 10)"
    echo "  --font-size PTS terminal font size (default 12)"
    echo "  --out-dir DIR   evidence directory (default recording/mem)"
    echo "  --dry-run       print the measure plan; launch nothing"
    exit 0
    ;;
  *)
    echo "unknown flag $1" >&2
    exit 2
    ;;
  esac
done

is_uint() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

fail_usage() {
  echo "mem-measure: $1" >&2
  exit 2
}

is_uint "$SETTLE_SECS" || fail_usage "--settle-secs must be a positive integer"
[ "$SETTLE_SECS" -ge 5 ] && [ "$SETTLE_SECS" -le 60 ] || fail_usage "--settle-secs must be 5..60"
case "$MEM_WS" in
1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10) ;;
*) fail_usage "--workspace must be 1..10" ;;
esac
[ -n "$OUT_DIR" ] || fail_usage "--out-dir must not be empty"
command -v jq >/dev/null 2>&1 || {
  echo "mem-measure: jq is required" >&2
  exit 2
}
command -v grim >/dev/null 2>&1 || {
  echo "mem-measure: grim is required" >&2
  exit 2
}

ROOT="$(git rev-parse --show-toplevel)"
case "$OUT_DIR" in
/*) RUN_OUT="$OUT_DIR" ;;
*) RUN_OUT="$ROOT/$OUT_DIR" ;;
esac

if [ "$BUILD" = "1" ]; then
  echo "mem-measure: building release binary"
  (cd "$ROOT" && cargo build -p bitty-terminal --release --locked --quiet)
fi
if [ -n "$BIN_OVERRIDE" ]; then
  BIN="$BIN_OVERRIDE"
else
  BIN="$ROOT/target/release/bitty"
fi
[ -x "$BIN" ] || {
  echo "mem-measure: no executable binary at $BIN (pass --binary or --build)" >&2
  exit 2
}
# Hyprland launches detached (its own cwd): a relative --binary would never
# resolve there. Canonicalize to absolute before dispatch.
BIN="$(readlink -f "$BIN")"
[ -x "$BIN" ] || {
  echo "mem-measure: no executable binary at $BIN (pass --binary or --build)" >&2
  exit 2
}

if [ "$DRY_RUN" = "1" ]; then
  echo "mem-measure: plan bin=$BIN workspace=$MEM_WS settle=${SETTLE_SECS}s font=${FONT_SIZE}pt out=$RUN_OUT gate_rss_kb=256000"
  exit 0
fi
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="$RUN_OUT/run-$STAMP"
mkdir -p "$RUN_DIR"

WS_COUNT="$(hyprctl clients -j | jq --argjson ws "$MEM_WS" '[.[] | select(.workspace.id == $ws)] | length')"
[ "$WS_COUNT" = "0" ] || {
  echo "mem-measure: workspace $MEM_WS is not empty ($WS_COUNT windows); refusing" >&2
  exit 2
}

TARGET_PID=""
TARGET_ADDR=""
PREV_WS="$(hyprctl activeworkspace -j | jq -r .id)"
echo "mem-measure: prev_ws=$PREV_WS measure_ws=$MEM_WS bin=$BIN run_dir=$RUN_DIR"

cleanup() {
  if [ -n "$TARGET_ADDR" ]; then
    if hyprctl clients -j | jq -e --arg a "$TARGET_ADDR" 'any(.[]; .address == $a)' >/dev/null 2>&1; then
      hyprctl dispatch "hl.dsp.window.close({ window = \"address:$TARGET_ADDR\" })" >/dev/null 2>&1 || true
    fi
  fi
  if [ -n "$TARGET_PID" ] && kill -0 "$TARGET_PID" 2>/dev/null; then
    kill "$TARGET_PID" 2>/dev/null || true
    sleep 2
    if kill -0 "$TARGET_PID" 2>/dev/null; then
      kill -9 "$TARGET_PID" 2>/dev/null || true
    fi
  fi
  if [ -n "$TARGET_PID" ]; then
    for orphan in $(ps --ppid "$TARGET_PID" -o pid= 2>/dev/null); do
      kill "$orphan" 2>/dev/null || true
    done
  fi
  if [ -n "$PREV_WS" ] && [ "$PREV_WS" != "null" ]; then
    hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

hyprctl dispatch "hl.dsp.exec_cmd('$BIN --font-size $FONT_SIZE', {workspace='$MEM_WS silent'})" >/dev/null

# Single settle sleep: the method. No poll loop.
sleep "$SETTLE_SECS"

MATCHES="$(hyprctl clients -j | jq --argjson ws "$MEM_WS" '[.[] | select(.workspace.id == $ws and (.class == "bitty" or .class == "run.bitty.Bitty"))]')"
[ "$(printf '%s' "$MATCHES" | jq 'length')" = "1" ] || {
  echo "mem-measure: expected 1 bitty window on workspace $MEM_WS, found $(printf '%s' "$MATCHES" | jq 'length')" >&2
  exit 1
}
TARGET_PID="$(printf '%s' "$MATCHES" | jq -r '.[0].pid')"
TARGET_ADDR="$(printf '%s' "$MATCHES" | jq -r '.[0].address')"
echo "mem-measure: pid=$TARGET_PID addr=$TARGET_ADDR"
# shellcheck disable=SC2009
ps --ppid "$TARGET_PID" -o pid= >"$RUN_DIR/children.txt" 2>/dev/null || true

grep -E "VmRSS|VmSize|RssAnon|RssFile|RssShmem|Threads" "/proc/$TARGET_PID/status" | tee "$RUN_DIR/status.txt"
awk '/^Pss:/{p+=$2} END{print "PSS_kB="p}' "/proc/$TARGET_PID/smaps" | tee "$RUN_DIR/pss.txt"
awk '/^Private_Clean:/{c+=$2} /^Private_Dirty:/{d+=$2} END{print "USS_kB="c+d}' "/proc/$TARGET_PID/smaps" | tee "$RUN_DIR/uss.txt"
pmap -x "$TARGET_PID" >"$RUN_DIR/pmap.txt" 2>&1 || true
sort -k3 -n -r "$RUN_DIR/pmap.txt" 2>/dev/null | head -n 25 | tee "$RUN_DIR/topmappings.txt" || true
cp "/proc/$TARGET_PID/smaps" "$RUN_DIR/smaps.txt"
grim "$RUN_DIR/shot.png"

RSS_KB="$(awk '/VmRSS:/{print $2}' "$RUN_DIR/status.txt")"
VSZ_KB="$(awk '/VmSize:/{print $2}' "$RUN_DIR/status.txt")"
PSS_KB="$(awk -F= '/PSS_kB/{print $2}' "$RUN_DIR/pss.txt")"
USS_KB="$(awk -F= '/USS_kB/{print $2}' "$RUN_DIR/uss.txt")"
# Interim gate: 250 MB RSS (toward the 150 MB budget).
if [ "$RSS_KB" -le 256000 ]; then
  GATE="PASS"
else
  GATE="FAIL"
fi
{
  echo "BIN=$BIN"
  echo "RSS_KB=$RSS_KB"
  echo "VSZ_KB=$VSZ_KB"
  echo "PSS_KB=$PSS_KB"
  echo "USS_KB=$USS_KB"
  echo "GATE=$GATE (interim: RSS <= 250MB toward 150MB budget)"
} | tee "$RUN_DIR/summary.txt"

if [ "$GATE" = "FAIL" ]; then
  echo "mem-measure: GATE=FAIL (RSS ${RSS_KB}kB over 250MB interim budget)" >&2
  exit 1
fi
echo "mem-measure: GATE=PASS (RSS ${RSS_KB}kB within 250MB interim budget)"
