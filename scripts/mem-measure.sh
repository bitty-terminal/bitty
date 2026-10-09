#!/usr/bin/env bash
# mem-measure.sh — Headed idle-memory probe with interim gate (CTX-1036, issue #1809).
#
# Method (matches the #1809 comments): Hyprland-headed single idle shell
# panel, 12pt, empty scrollback; launch on workspace 5 silent; settle 10 s
# (ONE sleep, no poll loop); read /proc/PID/status (VmRSS/VmSize/RssAnon/
# RssFile/RssShmem/Threads) + smaps PSS/USS + pmap -x; focus the measure
# workspace for one grim shot, then hand focus back; kill only the recorded
# PID (matched window PID, else the post-snapshot launch candidate) plus
# recorded direct children first — no pkill; restore the previously focused
# workspace.
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
# Point size is interpolated into the Hyprland launch command: restrict the
# charset first (so the awk range check below cannot break out), then the
# range to bitty's (0, 128] override domain.
case "$FONT_SIZE" in
'' | *[!0-9.]* | .* | *. | *.*.*) fail_usage "--font-size must be a number in (0, 128]" ;;
esac
awk "BEGIN{exit !(($FONT_SIZE > 0) && ($FONT_SIZE <= 128))}" || fail_usage "--font-size must be in (0, 128]"

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
# Headed prerequisites are live-path-only: --dry-run prints the plan on a
# headless runner without screenshot tooling (the contract test relies on
# this), while live measurement still refuses to run without them.
command -v jq >/dev/null 2>&1 || {
  echo "mem-measure: jq is required" >&2
  exit 2
}
command -v grim >/dev/null 2>&1 || {
  echo "mem-measure: grim is required" >&2
  exit 2
}
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

# Launch-identity snapshot for the orphan fallback in cleanup(): Hyprland
# launches detached (no PID back), so record same-basename PIDs before
# dispatch. Compared by process name (`comm`), never the full command line,
# so this script's own `--binary` argument cannot self-match. Best-effort
# for a dev-tool probe: a concurrent unrelated launch inside the settle
# window could be mistaken, but the empty-workspace refusal keeps that rare.
BIN_BASE="$(basename "$BIN")"
PRE_PIDS="$(ps -eo pid=,comm= 2>/dev/null | awk -v want="$BIN_BASE" '$2 == want {print $1}' || true)"

cleanup() {
  # Resolve the kill target: the matched window PID, else any same-basename
  # process that appeared after the pre-launch snapshot. A failed window
  # match leaves TARGET_PID empty; without this the detached launch leaks.
  KILL_PID="$TARGET_PID"
  if [ -z "$KILL_PID" ] && [ -n "$BIN_BASE" ]; then
    CUR_PIDS="$(ps -eo pid=,comm= 2>/dev/null | awk -v want="$BIN_BASE" '$2 == want {print $1}' || true)"
    # shellcheck disable=SC2086
    for pid in $CUR_PIDS; do
      case " $PRE_PIDS " in
      *" $pid "*) ;;
      *)
        KILL_PID="$pid"
        break
        ;;
      esac
    done
  fi
  if [ -n "$TARGET_ADDR" ]; then
    if hyprctl clients -j | jq -e --arg a "$TARGET_ADDR" 'any(.[]; .address == $a)' >/dev/null 2>&1; then
      hyprctl dispatch "hl.dsp.window.close({ window = \"address:$TARGET_ADDR\" })" >/dev/null 2>&1 || true
    fi
  fi
  if [ -n "$KILL_PID" ]; then
    # Children before the parent while PPIDs still resolve: prefer the
    # recorded list (PIDs stay valid after reparenting; a post-kill
    # `ps --ppid` query would miss survivors), else query live now.
    CHILDREN=""
    if [ -f "$RUN_DIR/children.txt" ]; then
      CHILDREN="$(cat "$RUN_DIR/children.txt" 2>/dev/null || true)"
    fi
    if [ -z "$CHILDREN" ]; then
      # shellcheck disable=SC2009
      CHILDREN="$(ps --ppid "$KILL_PID" -o pid= 2>/dev/null || true)"
    fi
    # shellcheck disable=SC2086
    for child in $CHILDREN; do
      if kill -0 "$child" 2>/dev/null; then
        kill "$child" 2>/dev/null || true
      fi
    done
    if kill -0 "$KILL_PID" 2>/dev/null; then
      kill "$KILL_PID" 2>/dev/null || true
      sleep 2
      if kill -0 "$KILL_PID" 2>/dev/null; then
        kill -9 "$KILL_PID" 2>/dev/null || true
      fi
    fi
    # Recorded children that survived TERM get KILL by identity (not PPID).
    # shellcheck disable=SC2086
    for child in $CHILDREN; do
      if kill -0 "$child" 2>/dev/null; then
        kill -9 "$child" 2>/dev/null || true
      fi
    done
  fi
  if [ -n "$PREV_WS" ] && [ "$PREV_WS" != "null" ]; then
    hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# Shell-quote the binary for `sh -c` (Hyprland runs exec_cmd through a
# shell), then present it as a Lua double-quoted string (embedded single
# quotes need no Lua escape; backslashes and double quotes are escaped).
# FONT_SIZE is charset/range-validated above and MEM_WS is 1..10, so neither
# can break out of the Lua string.
SHELL_QBIN="'${BIN//\'/\'\\\'\'}'"
LUA_CMD="$(printf '%s' "$SHELL_QBIN --font-size $FONT_SIZE" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
hyprctl dispatch "hl.dsp.exec_cmd(\"$LUA_CMD\", {workspace='$MEM_WS silent'})" >/dev/null

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
# Evidence must show the measured window: the launch used a silent workspace,
# so a bare grim would capture whatever workspace the operator sits on.
# Focus the measure workspace for the shot, then hand focus back (the EXIT
# trap restores PREV_WS again — idempotent).
hyprctl dispatch "hl.dsp.focus({ workspace = \"$MEM_WS\" })" >/dev/null 2>&1 || true
grim "$RUN_DIR/shot.png"
hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null 2>&1 || true

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
