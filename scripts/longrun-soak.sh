#!/usr/bin/env bash
# longrun-soak.sh — bounded long-run soak + stall capture (issue #2483, CTX-2466).
#
# Reproduces the owner's "long-running Bitty becomes unresponsive" shape in an
# isolated display, with a liveness probe on every sample and the opt-in
# main-loop stall watchdog enabled. Nothing here touches the owner's desktop:
# the harness starts its own Xvfb display, its own XDG roots, and records the
# PIDs it launches. Cleanup kills only those recorded process groups.
#
# Stages exercised per round:
#   output   scrollback growth through real PTY input (bounded payloads)
#   images   Kitty graphics transmit/place/delete through the PTY stream
#   input    short input bursts and bracketed-paste-ish text
#   resize   window resizes through xdotool (skipped when unavailable)
#   idle     the loop parks in ControlFlow::Wait; a quiet window is not a stall
#
# Every sample measures `bitty ctl view list` latency, RSS, thread count, and
# fd count, and counts watchdog stall reports. Evidence stays under --out-dir
# (gitignored scratch) and carries counts, never host paths.
#
# `--stall-probe` additionally reproduces the blocking-PTY-write class: it
# starts a child that stops reading stdin, then sends bounded input bursts.
# Pre-fix (issue #2457 unmerged) the event loop parks in write(2) and the
# probe reports BLOCKED plus the watchdog report; post-fix it reports PASS.
# By default the probe expects PASS (regression mode); add `--expect-stall`
# to capture today's defect as evidence and still exit 0.
#
# Bounds: durations 1..=86400 s, intervals 1..=3600 s, watchdog threshold
# 100..=120000 ms, payloads below the 4096-byte ctl parameter cap, at most
# LONG_RUN_MAX_SAMPLES samples. Unknown flags and invalid knobs exit 2.
#
# Usage:
#   bash scripts/longrun-soak.sh --duration-secs 600
#   bash scripts/longrun-soak.sh --duration-secs 900 --interval-secs 10 \
#       --workload mixed --out-dir recording/longrun-soak
#   bash scripts/longrun-soak.sh --duration-secs 120 --stall-probe --expect-stall
#   bash scripts/longrun-soak.sh --dry-run
#
# CI runs only --help/--dry-run/argument validation through
# scripts/tests/longrun-soak.test.sh; the live path needs Xvfb.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="recording/longrun-soak"
DURATION_SECS=600
INTERVAL_SECS=10
WORKLOAD="mixed"
BIN_OVERRIDE=""
XVFB_BIN="${XVFB_BIN:-Xvfb}"
DISPLAY_START=91
DISPLAY_END=99
STALL_MS=2000
PLUGIN_DIR=""
SHELL_BIN=""
DRY_RUN=0
STALL_PROBE=0
EXPECT_STALL=0
PROBE_AFTER_SECS=20
PROBE_WRITES=8
PROBE_BYTES=3000
CTL_TIMEOUT_SECS=3
STARTUP_TIMEOUT_SECS=25
RESIZE=1

# Hard caps that keep the harness itself bounded.
LONG_RUN_MAX_SAMPLES=512
LONG_RUN_MAX_DURATION=86400
LONG_RUN_MAX_INTERVAL=3600
LONG_RUN_MIN_STALL_MS=100
LONG_RUN_MAX_STALL_MS=120000
LONG_RUN_MAX_PROBE_WRITES=16
LONG_RUN_MAX_PROBE_BYTES=3800
LONG_RUN_CTL_PARAM_CAP=4096

ROOT=""
XVFB_PID=""
APP_PID=""
APP_PGID=""
SOCK=""
TERMINAL_ID=""
SHELL_PID=""
SAMPLES=0
STALL_REPORTS=0
LIVENESS_TIMEOUTS=0
PROBE_RESULT="skip"
APP_EXITED=0

usage() {
  awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0"
}

die() {
  echo "longrun-soak: $*" >&2
  exit 2
}

# Seconds since the Unix epoch in milliseconds.
now_ms() {
  date +%s%3N
}

is_uint() {
  case "$1" in
  '' | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
    --out-dir)
      OUT_DIR="${2:-}"
      shift 2
      ;;
    --duration-secs)
      DURATION_SECS="${2:-}"
      shift 2
      ;;
    --interval-secs)
      INTERVAL_SECS="${2:-}"
      shift 2
      ;;
    --workload)
      WORKLOAD="${2:-}"
      shift 2
      ;;
    --binary)
      BIN_OVERRIDE="${2:-}"
      shift 2
      ;;
    --xvfb)
      XVFB_BIN="${2:-}"
      shift 2
      ;;
    --stall-ms)
      STALL_MS="${2:-}"
      shift 2
      ;;
    --plugin-dir)
      PLUGIN_DIR="${2:-}"
      shift 2
      ;;
    --shell)
      SHELL_BIN="${2:-}"
      shift 2
      ;;
    --probe-after-secs)
      PROBE_AFTER_SECS="${2:-}"
      shift 2
      ;;
    --probe-writes)
      PROBE_WRITES="${2:-}"
      shift 2
      ;;
    --stall-probe)
      STALL_PROBE=1
      shift
      ;;
    --expect-stall)
      EXPECT_STALL=1
      shift
      ;;
    --no-resize)
      RESIZE=0
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown flag: $1"
      ;;
    esac
  done

  is_uint "$DURATION_SECS" || die "--duration-secs must be an integer"
  is_uint "$INTERVAL_SECS" || die "--interval-secs must be an integer"
  is_uint "$STALL_MS" || die "--stall-ms must be an integer"
  is_uint "$PROBE_AFTER_SECS" || die "--probe-after-secs must be an integer"
  is_uint "$PROBE_WRITES" || die "--probe-writes must be an integer"
  [ "$DURATION_SECS" -ge 1 ] && [ "$DURATION_SECS" -le "$LONG_RUN_MAX_DURATION" ] ||
    die "--duration-secs must be 1..$LONG_RUN_MAX_DURATION"
  [ "$INTERVAL_SECS" -ge 1 ] && [ "$INTERVAL_SECS" -le "$LONG_RUN_MAX_INTERVAL" ] ||
    die "--interval-secs must be 1..$LONG_RUN_MAX_INTERVAL"
  [ "$STALL_MS" -ge "$LONG_RUN_MIN_STALL_MS" ] && [ "$STALL_MS" -le "$LONG_RUN_MAX_STALL_MS" ] ||
    die "--stall-ms must be $LONG_RUN_MIN_STALL_MS..$LONG_RUN_MAX_STALL_MS"
  [ "$PROBE_WRITES" -ge 1 ] && [ "$PROBE_WRITES" -le "$LONG_RUN_MAX_PROBE_WRITES" ] ||
    die "--probe-writes must be 1..$LONG_RUN_MAX_PROBE_WRITES"
  case "$WORKLOAD" in
  idle | mixed | stress) ;;
  *) die "--workload must be idle|mixed|stress" ;;
  esac
  [ "$PROBE_BYTES" -le "$LONG_RUN_MAX_PROBE_BYTES" ] || die "probe payload too large"
  [ "$PROBE_BYTES" -lt "$LONG_RUN_CTL_PARAM_CAP" ] || die "probe payload exceeds the ctl parameter cap"

  local planned=$(((DURATION_SECS + INTERVAL_SECS - 1) / INTERVAL_SECS + 1))
  [ "$planned" -le "$LONG_RUN_MAX_SAMPLES" ] ||
    die "plan needs $planned samples, cap is $LONG_RUN_MAX_SAMPLES"
  PLANNED_SAMPLES=$planned
}

resolve_binary() {
  if [ -n "$BIN_OVERRIDE" ]; then
    BIN="$BIN_OVERRIDE"
  else
    BIN="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/debug/bitty"
  fi
  [ -x "$BIN" ] || die "bitty binary not executable: $BIN (build it or pass --binary)"
}

resolve_shell() {
  if [ -n "$SHELL_BIN" ]; then
    [ -x "$SHELL_BIN" ] || die "shell not executable: $SHELL_BIN"
    return
  fi
  if [ -x /usr/bin/fish ]; then
    SHELL_BIN=/usr/bin/fish
  elif [ -n "${SHELL:-}" ] && [ -x "$SHELL" ]; then
    SHELL_BIN="$SHELL"
  else
    SHELL_BIN=/bin/sh
  fi
}

print_plan() {
  echo "longrun-soak plan"
  echo "  binary=$BIN"
  echo "  shell=$SHELL_BIN"
  echo "  out-dir=$OUT_DIR"
  echo "  duration=${DURATION_SECS}s interval=${INTERVAL_SECS}s samples=$PLANNED_SAMPLES"
  echo "  workload=$WORKLOAD resize=$RESIZE"
  echo "  watchdog=on threshold=${STALL_MS}ms"
  echo "  stall-probe=$STALL_PROBE expect-stall=$EXPECT_STALL probe-after=${PROBE_AFTER_SECS}s"
  echo "  isolation=private Xvfb + private XDG roots; evidence keeps counts only"
  echo "  note=live run needs Xvfb; CI asserts --help/--dry-run only"
}

prepare_probe_payload() {
  # Keep the payload construction boxed so `set -e` never trips on a
  # partial write and the shellcheck SC2016 reading stays honest.
  PROBE_PAYLOAD="$(head -c "$PROBE_BYTES" /dev/zero | tr '\0' 'x')"
}

pick_display() {
  local d
  # scratch-paths-exempt: X11 protocol socket/lock locations, not evidence
  for d in $(seq "$DISPLAY_START" "$DISPLAY_END"); do
    [ -e "/tmp/.X11-unix/X$d" ] && continue
    [ -e "/tmp/.X$d-lock" ] && continue
    DISPLAY_NUM="$d"
    return 0
  done
  die "no free X display in ${DISPLAY_START}..${DISPLAY_END}"
}

start_xvfb() {
  local log="$OUT_DIR/xvfb.log"
  local out
  out="$(command -v "$XVFB_BIN" 2>/dev/null || true)"
  [ -n "$out" ] || die "Xvfb not found ('$XVFB_BIN'); install xorg-server-xvfb or pass --xvfb PATH"
  setsid "$out" ":$DISPLAY_NUM" -screen 0 1280x800x24 -ac +extension GLX +extension RANDR +render -noreset \
    >"$log" 2>&1 &
  XVFB_PID=$!
  local deadline=$((SECONDS + 10))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if ! kill -0 "$XVFB_PID" 2>/dev/null; then
      XVFB_PID=""
      return 1
    fi
    # scratch-paths-exempt: X11 protocol socket location, not evidence
    [ -e "/tmp/.X11-unix/X$DISPLAY_NUM" ] && return 0
    sleep 0.1
  done
  return 1
}

cleanup() {
  local status=$?
  set +e
  if [ -n "$APP_PGID" ]; then
    kill -TERM -- "-$APP_PGID" 2>/dev/null || kill -TERM "$APP_PID" 2>/dev/null
    local deadline=$((SECONDS + 5))
    while [ "$SECONDS" -lt "$deadline" ] && kill -0 "$APP_PID" 2>/dev/null; do
      sleep 0.1
    done
    kill -KILL -- "-$APP_PGID" 2>/dev/null || kill -KILL "$APP_PID" 2>/dev/null
  fi
  if [ -n "$XVFB_PID" ]; then
    kill -TERM "$XVFB_PID" 2>/dev/null
    wait "$XVFB_PID" 2>/dev/null
  fi
  if [ -n "$ROOT" ] && [ -d "$ROOT" ]; then
    rm -rf "$ROOT"
  fi
  exit "$status"
}

trap cleanup EXIT INT TERM

# Launch the app in its own session so cleanup can target exactly this tree.
start_app() {
  local rt="$ROOT/runtime"
  mkdir -p "$ROOT/home" "$ROOT/config" "$ROOT/data" "$ROOT/state" "$ROOT/cache" "$ROOT/plugins" "$rt"
  chmod 700 "$rt"
  export HOME="$ROOT/home"
  export XDG_RUNTIME_DIR="$rt"
  export XDG_CONFIG_HOME="$ROOT/config"
  export XDG_DATA_HOME="$ROOT/data"
  export XDG_STATE_HOME="$ROOT/state"
  export XDG_CACHE_HOME="$ROOT/cache"
  export DISPLAY=":$DISPLAY_NUM"
  unset WAYLAND_DISPLAY WAYLAND_SOCKET HYPRLAND_INSTANCE_SIGNATURE
  export SHELL="$SHELL_BIN"
  export BITTY_INSTANCE_ID="soak$DISPLAY_NUM"
  export BITTY_STALL_WATCHDOG=1
  export BITTY_STALL_WATCHDOG_MS="$STALL_MS"
  if [ -n "$PLUGIN_DIR" ]; then
    export BITTY_PLUGIN_DIR="$PLUGIN_DIR"
  fi
  SOCK="$rt/bitty/soak$DISPLAY_NUM.sock"
  setsid "$BIN" --log-level info -- "$SHELL_BIN" -i >"$OUT_DIR/bitty.log" 2>&1 &
  APP_PID=$!
  APP_PGID=$APP_PID
}

ctl() {
  timeout "$CTL_TIMEOUT_SECS" "$BIN" ctl --socket "$SOCK" "$@"
}

wait_ready() {
  local deadline=$((SECONDS + STARTUP_TIMEOUT_SECS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if ! kill -0 "$APP_PID" 2>/dev/null; then
      echo "longrun-soak: bitty exited during startup (see $OUT_DIR/bitty.log)" >&2
      exit 1
    fi
    if ctl view list >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "longrun-soak: bitty never answered ctl within ${STARTUP_TIMEOUT_SECS}s" >&2
  exit 1
}

# Focus the primary leaf once so `terminal send` targets a live session.
focus_primary() {
  local views terminals view
  views="$(ctl --format json view list 2>&1 || true)"
  view="$(printf '%s' "$views" | jq -r '(.result // .) | .views[0].id' 2>/dev/null || true)"
  if [ -z "$view" ]; then
    printf 'views=%s\n' "$views" >"$OUT_DIR/focus-debug.txt"
    return 1
  fi
  if ! ctl view focus "$view" >/dev/null 2>&1; then
    printf 'focus failed for %s\nviews=%s\n' "$view" "$views" >"$OUT_DIR/focus-debug.txt"
    return 1
  fi
  terminals="$(ctl --format json terminal list 2>&1 || true)"
  TERMINAL_ID="$(printf '%s' "$terminals" |
    jq -r --arg v "$view" '(.result // .) | .diagnostics[]? | select(.view_id == $v) | .terminal_id' 2>/dev/null |
    head -n1 || true)"
  SHELL_PID="$(printf '%s' "$terminals" |
    jq -r --arg v "$view" '(.result // .) | .diagnostics[]? | select(.view_id == $v) | .child_pid' 2>/dev/null |
    head -n1 || true)"
  if [ -z "$TERMINAL_ID" ]; then
    printf 'view=%s\nterminals=%s\n' "$view" "$terminals" >"$OUT_DIR/focus-debug.txt"
    return 1
  fi
}

send_text() {
  [ -n "$TERMINAL_ID" ] || return 1
  local out
  if ! out="$(ctl terminal send "$TERMINAL_ID" "$1" 2>&1)"; then
    printf 'send failed term=%s: %s\n' "$TERMINAL_ID" "$out" >>"$OUT_DIR/send-errors.txt"
    return 1
  fi
}

burst_output() {
  # ~40 lines per send, well below the 4096-byte ctl parameter cap.
  local cmd
  cmd="sh -c 'i=0; while [ \$i -lt 40 ]; do printf \"soak %s %s\\n\" \"\$i\" \"................................................................\"; i=\$((i+1)); done'"
  cmd+=$'\r'
  send_text "$cmd" || return 1
}

burst_input() {
  local text
  text="abc"
  send_text "$text" || return 1
  send_text $'\r' || return 1
}

burst_images() {
  # 1x1 transparent PNG (67 bytes), transmit + place + delete. The APC is
  # parsed by the PTY stream regardless of the host terminal's graphics.
  local png="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
  send_text $'\x1b_Ga=T,f=100,i=1,s=1,v=1;'"$png"$'\x1b\\' || return 1
  send_text $'\x1b_Ga=p,i=1,p=1;\x1b\\' || return 1
  send_text $'\x1b_Ga=d,d=I,i=1;\x1b\\' || return 1
}

burst_resize() {
  [ "$RESIZE" -eq 1 ] || return 0
  command -v xdotool >/dev/null 2>&1 || return 0
  local wid
  wid="$(xdotool search --pid "$APP_PID" 2>/dev/null | head -n1 || true)"
  [ -n "$wid" ] || wid="$(xdotool search --name 'Bitty' 2>/dev/null | head -n1 || true)"
  [ -n "$wid" ] || return 0
  xdotool windowsize "$wid" 120 40 2>/dev/null || return 0
  xdotool windowsize "$wid" 80 24 2>/dev/null || return 0
}

sample() {
  local elapsed="$1"
  local start_ms end_ms ctl_ms="timeout" rss=0 threads=0 fds=0 reports=0
  reports="$(grep -c 'main-loop stall detected' "$OUT_DIR/bitty.log" 2>/dev/null || true)"
  reports="${reports:-0}"
  STALL_REPORTS="$reports"
  start_ms="$(now_ms)"
  if ctl view list >/dev/null 2>&1; then
    end_ms="$(now_ms)"
    ctl_ms=$((end_ms - start_ms))
  else
    LIVENESS_TIMEOUTS=$((LIVENESS_TIMEOUTS + 1))
  fi
  if [ -r "/proc/$APP_PID/status" ]; then
    rss="$(awk '/^VmRSS:/ {print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
    threads="$(awk '/^Threads:/ {print $2}' "/proc/$APP_PID/status" 2>/dev/null || echo 0)"
    fds="$(ls "/proc/$APP_PID/fd" 2>/dev/null | wc -l | tr -d ' ')"
  elif ! kill -0 "$APP_PID" 2>/dev/null; then
    APP_EXITED=1
  fi
  printf '%s,%s,%s,%s,%s,%s,%s\n' \
    "$elapsed" "$ctl_ms" "$rss" "$threads" "$fds" "$reports" "$WORKLOAD" >>"$OUT_DIR/samples.csv"
  SAMPLES=$((SAMPLES + 1))
  printf 'sample %s: ctl=%sms rss=%sKiB threads=%s fds=%s stalls=%s\n' \
    "$elapsed" "$ctl_ms" "$rss" "$threads" "$fds" "$reports"
}

capture_stall_evidence() {
  {
    echo "=== watchdog log lines ==="
    grep -n 'main-loop stall' "$OUT_DIR/bitty.log" || true
    echo "=== single-slot watchdog report ==="
    cat "$XDG_STATE_HOME/bitty/stall-watchdog.log" 2>/dev/null || echo "(none)"
    echo "=== kernel thread snapshot (bounded) ==="
    ps -L -o pid,tid,stat,wchan:32,comm -p "$APP_PID" 2>/dev/null | head -n 80 || true
    echo "=== process totals ==="
    ps -o pid,etime,nlwp,rss,vsz,stat -p "$APP_PID" 2>/dev/null || true
  } >"$OUT_DIR/stall-evidence.txt"
  if command -v eu-stack >/dev/null 2>&1; then
    timeout 10 eu-stack -p "$APP_PID" >"$OUT_DIR/stall-stacks.txt" 2>&1 || true
  fi
}

stall_probe() {
  echo "stall-probe: starting a raw-mode child that stops reading stdin"
  {
    echo "view=$(ctl --format json view list 2>&1 || true)"
    echo "term=$(ctl --format json terminal list 2>&1 || true)"
    echo "text_before=$(ctl terminal text "$TERMINAL_ID" 2>&1 | tail -3 || true)"
  } >"$OUT_DIR/probe-debug.txt"
  # Settle any pending prompt input, then run the child with a raw line
  # discipline: canonical mode buffers far past the probe payload on this
  # kernel, raw mode is the bounded queue the owner's freeze reports.
  send_text $'\x03' || true
  send_text $'\r' || true
  sleep 1
  send_text "sh -c 'stty raw -echo; exec sleep 900'"$'\r' || true
  # Do not burst until the child proves it is running with a raw line
  # discipline; a burst into a still-cooked prompt would silently pass.
  # The process table is the reliable signal here: grid text extraction is
  # presentation-dependent, and a no-WM Xvfb session may not present.
  local ready=0 attempt=0
  while [ "$attempt" -lt 40 ]; do
    if [ -n "$SHELL_PID" ] && ps --ppid "$SHELL_PID" -o args= 2>/dev/null | grep -qF 'sleep 900'; then
      ready=1
      break
    fi
    attempt=$((attempt + 1))
    sleep 0.25
  done
  if [ "$ready" -ne 1 ]; then
    PROBE_RESULT="inconclusive"
    {
      echo "term=$TERMINAL_ID shell=$SHELL_PID"
      echo "--- terminal text tail ---"
      ctl terminal text "$TERMINAL_ID" 2>&1 | tail -20 || true
      echo "--- bitty log tail ---"
      tail -12 "$OUT_DIR/bitty.log"
    } >>"$OUT_DIR/probe-debug.txt"
    echo "stall-probe: INCONCLUSIVE (non-reading child never started)"
    return
  fi
  local i rc start_ms end_ms blocked=0
  for i in $(seq 1 "$PROBE_WRITES"); do
    start_ms="$(now_ms)"
    if timeout "$CTL_TIMEOUT_SECS" "$BIN" ctl --socket "$SOCK" terminal send "$TERMINAL_ID" "$PROBE_PAYLOAD" >/dev/null 2>&1; then
      end_ms="$(now_ms)"
      echo "stall-probe: send $i $((end_ms - start_ms)) ms"
    else
      rc=$?
      end_ms="$(now_ms)"
      blocked=1
      echo "stall-probe: send $i BLOCKED rc=$rc after $((end_ms - start_ms)) ms"
      break
    fi
  done
  if [ "$blocked" -eq 0 ] && ! ctl view list >/dev/null 2>&1; then
    blocked=1
  fi
  # The watchdog needs one threshold window plus a sample to report.
  sleep $(((STALL_MS + 999) / 1000 + 1))
  if [ "$blocked" -eq 1 ] || grep -q 'main-loop stall detected' "$OUT_DIR/bitty.log" 2>/dev/null; then
    capture_stall_evidence
    PROBE_RESULT="blocked"
    echo "stall-probe: BLOCKED (event loop parked; evidence in $OUT_DIR/stall-evidence.txt)"
  else
    PROBE_RESULT="pass"
    echo "stall-probe: PASS (bounded writes, runtime responsive)"
  fi
}

run_live() {
  mkdir -p "$OUT_DIR"
  : >"$OUT_DIR/samples.csv"
  echo "elapsed_s,ctl_ms,rss_kib,threads,fds,stall_reports,workload" >"$OUT_DIR/samples.csv.tmp"
  mv "$OUT_DIR/samples.csv.tmp" "$OUT_DIR/samples.csv"
  ROOT="$(mktemp -d "${TMPDIR:-/tmp}/bitty-longrun-soak.XXXXXX")"
  mkdir -p "$ROOT/home" "$ROOT/config" "$ROOT/data" "$ROOT/state" "$ROOT/cache" "$ROOT/plugins"
  pick_display
  if ! start_xvfb; then
    echo "longrun-soak: Xvfb failed on :$DISPLAY_NUM (see $OUT_DIR/xvfb.log)" >&2
    exit 1
  fi
  echo "longrun-soak: display :$DISPLAY_NUM xvfb_pid=$XVFB_PID"
  start_app
  wait_ready
  focus_primary || die "could not resolve a focused terminal (see $OUT_DIR/focus-debug.txt)"
  echo "longrun-soak: app pid=$APP_PID terminal=${TERMINAL_ID:-none} socket=$SOCK"
  prepare_probe_payload

  local start_s=$SECONDS
  local probe_done=0
  while :; do
    local elapsed=$((SECONDS - start_s))
    [ "$elapsed" -ge "$DURATION_SECS" ] && break
    if ! kill -0 "$APP_PID" 2>/dev/null; then
      APP_EXITED=1
      break
    fi
    case "$WORKLOAD" in
    idle) ;;
    stress)
      burst_output || true
      burst_images || true
      burst_input || true
      burst_resize || true
      ;;
    mixed)
      burst_output || true
      burst_images || true
      burst_input || true
      burst_resize || true
      ;;
    esac
    if [ "$STALL_PROBE" -eq 1 ] && [ "$probe_done" -eq 0 ] && [ "$elapsed" -ge "$PROBE_AFTER_SECS" ]; then
      stall_probe
      probe_done=1
      if [ "$PROBE_RESULT" = "blocked" ]; then
        break
      fi
    fi
    sample "$elapsed"
    local remaining=$((DURATION_SECS - (SECONDS - start_s)))
    [ "$remaining" -gt 0 ] || break
    local nap="$INTERVAL_SECS"
    [ "$nap" -gt "$remaining" ] && nap="$remaining"
    sleep "$nap"
  done

  STALL_REPORTS="$(grep -c 'main-loop stall detected' "$OUT_DIR/bitty.log" 2>/dev/null || true)"
  STALL_REPORTS="${STALL_REPORTS:-0}"
  local status=0
  if [ "$APP_EXITED" -eq 1 ]; then
    echo "longrun-soak: FAIL app exited before the window closed" >&2
    status=1
  fi
  if [ "$LIVENESS_TIMEOUTS" -gt 0 ] && [ "$PROBE_RESULT" != "blocked" ]; then
    echo "longrun-soak: FAIL $LIVENESS_TIMEOUTS liveness probe(s) timed out" >&2
    status=1
  fi
  if [ "$STALL_REPORTS" -gt 0 ] && [ "$PROBE_RESULT" != "blocked" ]; then
    echo "longrun-soak: FAIL watchdog recorded $STALL_REPORTS stall(s)" >&2
    status=1
  fi
  if [ "$STALL_PROBE" -eq 1 ]; then
    case "$PROBE_RESULT" in
    blocked)
      if [ "$EXPECT_STALL" -ne 1 ]; then
        echo "longrun-soak: FAIL stall probe blocked (issue #2457 class)" >&2
        status=1
      fi
      ;;
    pass)
      if [ "$EXPECT_STALL" -eq 1 ]; then
        echo "longrun-soak: FAIL expected a captured stall, probe=pass" >&2
        status=1
      fi
      ;;
    *)
      echo "longrun-soak: FAIL stall probe inconclusive (child never started)" >&2
      status=1
      ;;
    esac
  fi
  {
    echo "samples=$SAMPLES"
    echo "liveness_timeouts=$LIVENESS_TIMEOUTS"
    echo "stall_reports=$STALL_REPORTS"
    echo "stall_probe=$PROBE_RESULT"
    echo "app_exited=$APP_EXITED"
    echo "status=$status"
  } >"$OUT_DIR/summary.txt"
  echo "longrun-soak: samples=$SAMPLES liveness_timeouts=$LIVENESS_TIMEOUTS stalls=$STALL_REPORTS probe=$PROBE_RESULT"
  echo "longrun-soak: evidence $OUT_DIR"
  exit "$status"
}

parse_args "$@"
resolve_binary
resolve_shell
print_plan
if [ "$DRY_RUN" -eq 1 ]; then
  exit 0
fi
run_live
