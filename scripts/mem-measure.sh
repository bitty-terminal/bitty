#!/usr/bin/env bash
# mem-measure.sh — Headed idle-memory probe with interim gate (CTX-1036, issue #1809).
#
# Method (matches the #1809 comments): Hyprland-headed single idle shell
# panel, 12pt, empty scrollback; launch on workspace 5 silent; settle memory
# for 10 s (ONE sleep, no memory poll loop); read /proc/PID/status
# (VmRSS/VmSize/RssAnon/
# RssFile/RssShmem/Threads) + smaps PSS/USS + pmap -x; focus the measure
# workspace for one grim shot, wait for the compositor to settle with a
# bounded identity/geometry poll, then hand focus back; kill only the recorded
# bounded process tree with descendants first — no pkill; restore the
# previously focused workspace.
#
# Evidence stays local and uncommitted under --out-dir (default
# recording/mem, gitignored): status.txt, pss.txt, uss.txt, pmap.txt,
# topmappings.txt, smaps.txt, driver.txt, clients-before-shot.json,
# monitors-before-shot.json, activeworkspace-before-shot.json,
# clients-after-shot.json, monitors-after-shot.json,
# activeworkspace-after-shot.json, shot.png, summary.txt. The detached
# launch uses a per-run BITTY_SOCKET/BITTY_INSTANCE_ID pair so ctl discovery
# cannot select another live Bitty instance.
#
# Comparability contract (CTX-1216, issue #2087): headed RSS mixes
# bitty-owned memory with NVIDIA/Vulkan driver-resident pages that bitty
# cannot control. A driver upgrade alone (615.71.09 -> 615.78.08) moves
# ~+35 MB of driver-resident RSS for the same binary, and the driver's
# post-device faulting is nondeterministic run to run (~50 MB spread for
# identical launches), so two runs compare ONLY when the driver version
# (driver.txt) and the settle seconds match. summary.txt therefore splits
# pmap RSS into DRIVER_RSS_KB (named NVIDIA/Vulkan/GL/VkLayer mappings), ANON_RSS_KB
# (all anonymous mappings: heap plus driver arenas, inseparable by name),
# and OWNED_OTHER_KB (binary, libc, fonts — the bitty-linked set); a
# genuine bitty regression moves OWNED_OTHER_KB or the heap slice of
# ANON_RSS_KB, never the driver line.
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
VISUAL_SETTLE_POLLS=25
VISUAL_STABLE_POLLS=3
VISUAL_SETTLE_INTERVAL_SECS="0.2"
# Keep the probe below bitty-ipc's portable AF_UNIX contract, which is
# intentionally stricter than Linux's native sockaddr_un capacity.
SOCKET_PATH_LIMIT=100
PROCESS_TREE_MAX_DEPTH=8
PROCESS_TREE_MAX_MEMBERS=64

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
    sed -n '2,41p' "$0"
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
if ! [ "$SETTLE_SECS" -ge 5 ] || ! [ "$SETTLE_SECS" -le 60 ]; then
  fail_usage "--settle-secs must be 5..60"
fi
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
RUN_ID="$STAMP-$$"
RUN_DIR="$RUN_OUT/run-$RUN_ID"
mkdir -p "$RUN_DIR"
INSTANCE_ID="mem-measure-$RUN_ID"

# Unix-domain socket paths are bounded by sockaddr_un and the evidence path can
# be arbitrarily deep. Keep the socket in a private short-lived runtime
# directory instead of under RUN_DIR, preferring the environment-provided
# runtime locations and refusing to guess a host path.
RUNTIME_ROOT="${XDG_RUNTIME_DIR:-${TMPDIR:-}}"
[ -n "$RUNTIME_ROOT" ] || {
  echo "mem-measure: XDG_RUNTIME_DIR or TMPDIR is required for the runtime socket" >&2
  exit 2
}
case "$RUNTIME_ROOT" in
/*) ;;
*)
  echo "mem-measure: runtime directory must be absolute: $RUNTIME_ROOT" >&2
  exit 2
  ;;
esac
[ -d "$RUNTIME_ROOT" ] || {
  echo "mem-measure: runtime directory does not exist: $RUNTIME_ROOT" >&2
  exit 2
}
SOCKET_DIR="$(mktemp -d -- "$RUNTIME_ROOT/bm-XXXXXX")" || {
  echo "mem-measure: failed to create a private runtime directory under $RUNTIME_ROOT" >&2
  exit 2
}
SOCK="$SOCKET_DIR/s"
SOCKET_PATH_BYTES="$(LC_ALL=C printf '%s' "$SOCK" | wc -c)"
if [ "$SOCKET_PATH_BYTES" -ge "$SOCKET_PATH_LIMIT" ]; then
  rmdir -- "$SOCKET_DIR" 2>/dev/null || true
  echo "mem-measure: runtime socket path is too long (${SOCKET_PATH_BYTES} bytes; maximum $((SOCKET_PATH_LIMIT - 1)))" >&2
  exit 2
fi

TARGET_PID=""
TARGET_ADDR=""
TARGET_START_TIME=""
TARGET_EXE=""
TARGET_INSTANCE_ID=""
TARGET_IDENTITY_PROVEN=0
PREV_WS=""

cleanup_socket_dir() {
  [ -n "${SOCKET_DIR:-}" ] || return 0
  rm -f -- "${SOCK:-$SOCKET_DIR/s}" 2>/dev/null || true
  if ! rmdir -- "$SOCKET_DIR" 2>/dev/null; then
    echo "mem-measure: failed to remove runtime directory $SOCKET_DIR" >&2
  fi
}
trap cleanup_socket_dir EXIT

WS_COUNT="$(hyprctl clients -j | jq --argjson ws "$MEM_WS" '[.[] | select(.workspace.id == $ws)] | length')"
[ "$WS_COUNT" = "0" ] || {
  echo "mem-measure: workspace $MEM_WS is not empty ($WS_COUNT windows); refusing" >&2
  exit 2
}

PREV_WS="$(hyprctl activeworkspace -j | jq -r .id)"
echo "mem-measure: prev_ws=$PREV_WS measure_ws=$MEM_WS bin=$BIN run_dir=$RUN_DIR"

process_start_time() {
  pid="$1"
  stat_data="$(cat "/proc/$pid/stat" 2>/dev/null || true)"
  [ -n "$stat_data" ] || return 1
  # Remove pid and comm, whose parentheses may contain spaces, before reading
  # field 20 of the remaining stat fields (the original field 22).
  stat_data="${stat_data##*) }"
  start_time="$(printf '%s\n' "$stat_data" | awk '{print $20}')"
  case "$start_time" in
  '' | *[!0-9]*) return 1 ;;
  esac
  printf '%s\n' "$start_time"
}

process_executable() {
  readlink -f "/proc/$1/exe" 2>/dev/null || true
}

process_instance_matches() {
  pid="$1"
  expected_instance_id="$2"
  tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null |
    grep -Fqx "BITTY_INSTANCE_ID=$expected_instance_id"
}

process_is_exited() {
  local pid="$1"
  local stat_data
  local state
  [ ! -d "/proc/$pid" ] && return 0
  stat_data="$(cat "/proc/$pid/stat" 2>/dev/null || true)"
  [ -n "$stat_data" ] || return 1
  stat_data="${stat_data##*) }"
  state="$(printf '%s\n' "$stat_data" | awk '{print $1}')"
  [ "$state" = "Z" ]
}

capture_process_identity() {
  local pid="$1"
  local executable
  local start_time
  executable="$(process_executable "$pid")"
  start_time="$(process_start_time "$pid")"
  process_instance_matches "$pid" "$INSTANCE_ID" || return 1
  [ -n "$executable" ] && [ -n "$start_time" ] || return 1
  printf '%s\t%s\t%s\t%s\n' "$pid" "$start_time" "$executable" "$INSTANCE_ID"
}

process_identity_matches() {
  local pid="$1"
  local expected_executable="$2"
  local expected_start_time="$3"
  local expected_instance_id="$4"
  local actual_executable
  local actual_start_time
  actual_executable="$(process_executable "$pid")"
  actual_start_time="$(process_start_time "$pid")"
  [ "$actual_executable" = "$expected_executable" ] &&
    [ "$actual_start_time" = "$expected_start_time" ] &&
    [ "$expected_instance_id" = "$INSTANCE_ID" ] &&
    process_instance_matches "$pid" "$expected_instance_id"
}

signal_process_if_identity_matches() {
  local pid="$1"
  local expected_executable="$2"
  local expected_start_time="$3"
  local signal="$4"
  local expected_instance_id="$5"
  if process_is_exited "$pid"; then
    echo "mem-measure: skipping exited process-tree member PID $pid" >&2
    return 0
  fi
  if [ ! -d "/proc/$pid" ]; then
    echo "mem-measure: skipping exited process-tree member PID $pid" >&2
    return 0
  fi
  if ! process_identity_matches "$pid" "$expected_executable" "$expected_start_time" "$expected_instance_id"; then
    if process_is_exited "$pid"; then
      echo "mem-measure: skipping exited process-tree member PID $pid" >&2
      return 0
    fi
    echo "mem-measure: refusing to signal PID $pid: executable/start/instance identity could not be proven" >&2
    return 1
  fi
  kill -s "$signal" "$pid" 2>/dev/null || true
}

process_tree_snapshot() {
  local root_pid="$1"
  local tree_file="${2:-$RUN_DIR/process-tree.txt}"
  local frontier="$root_pid:0"
  local next_frontier
  local entry pid depth child child_depth identity
  local member_count=0
  local seen=" $root_pid "

  : >"$tree_file"
  if ! identity="$(capture_process_identity "$root_pid")"; then
    echo "mem-measure: refusing process-tree cleanup: root PID $root_pid identity could not be proven" >&2
    return 1
  fi
  IFS=$'\t' read -r pid start_time executable instance_id <<<"$identity"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$pid" "0" "0" "$start_time" "$executable" "$instance_id" >>"$tree_file"
  member_count=1

  while [ -n "$frontier" ]; do
    next_frontier=""
    for entry in $frontier; do
      pid="${entry%%:*}"
      depth="${entry##*:}"
      if [ "$depth" -ge "$PROCESS_TREE_MAX_DEPTH" ]; then
        continue
      fi
      child_depth=$((depth + 1))
      # The process table is read-only discovery. Every discovered member is
      # still identity-captured and revalidated before any signal is sent.
      CHILDREN="$(ps -eo pid=,ppid= 2>/dev/null | awk -v parent="$pid" '$2 == parent {print $1}' || true)"
      # shellcheck disable=SC2086
      for child in $CHILDREN; do
        case "$child" in
        '' | *[!0-9]*) continue ;;
        esac
        case "$seen" in
        *" $child "*) continue ;;
        esac
        member_count=$((member_count + 1))
        if [ "$member_count" -gt "$PROCESS_TREE_MAX_MEMBERS" ]; then
          echo "mem-measure: refusing process-tree cleanup: more than $PROCESS_TREE_MAX_MEMBERS members" >&2
          return 1
        fi
        seen="$seen$child "
        if ! identity="$(capture_process_identity "$child")"; then
          echo "mem-measure: refusing process-tree cleanup: PID $child identity could not be proven" >&2
          return 1
        fi
        IFS=$'\t' read -r child_pid child_start_time child_executable child_instance_id <<<"$identity"
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
          "$child_pid" "$pid" "$child_depth" "$child_start_time" \
          "$child_executable" "$child_instance_id" >>"$tree_file"
        next_frontier="$next_frontier $child:$child_depth"
      done
    done
    frontier="${next_frontier# }"
  done
}

signal_process_tree() {
  local signal="$1"
  local tree_file="$2"
  local line_index
  local member_pid _member_ppid _member_depth member_start_time member_executable member_instance_id
  [ -s "$tree_file" ] || return 1
  mapfile -t tree_lines <"$tree_file"
  for ((line_index = ${#tree_lines[@]} - 1; line_index >= 0; line_index--)); do
    IFS=$'\t' read -r member_pid _member_ppid _member_depth member_start_time member_executable member_instance_id <<<"${tree_lines[line_index]}"
    [ -n "$member_pid" ] || continue
    signal_process_if_identity_matches "$member_pid" "$member_executable" "$member_start_time" "$signal" "$member_instance_id" || true
  done
}

verify_target_identity() {
  local phase="$1"
  if [ "$TARGET_IDENTITY_PROVEN" != "1" ] || [ -z "$TARGET_PID" ]; then
    echo "mem-measure: refusing target window use $phase: identity was not proven" >&2
    return 1
  fi
  if ! process_identity_matches "$TARGET_PID" "$TARGET_EXE" "$TARGET_START_TIME" "$TARGET_INSTANCE_ID"; then
    echo "mem-measure: target PID $TARGET_PID identity mismatch $phase" >&2
    return 1
  fi
}

target_window_matches_identity() {
  verify_target_identity "window operation" || return 1
  hyprctl clients -j | jq -e \
    --arg address "$TARGET_ADDR" \
    --argjson pid "$TARGET_PID" \
    --argjson workspace "$MEM_WS" \
    '[.[] | select(.address == $address and .pid == $pid and .workspace.id == $workspace)] | length == 1' \
    >/dev/null
}

cleanup() {
  if [ -n "$TARGET_ADDR" ] && [ "$TARGET_IDENTITY_PROVEN" = "1" ] && target_window_matches_identity; then
    hyprctl dispatch "hl.dsp.window.close({ window = \"address:$TARGET_ADDR\" })" >/dev/null 2>&1 || true
  elif [ -n "$TARGET_ADDR" ]; then
    echo "mem-measure: refusing to close target window $TARGET_ADDR: identity could not be proven" >&2
  fi
  # The launch-time tree is the only cleanup target. Do not discover a new PID
  # or resnapshot after the root may have exited: those operations could miss
  # reparented descendants or select an unrelated process.
  TREE_FILE="$RUN_DIR/process-tree.txt"
  if [ -s "$TREE_FILE" ]; then
    # Revalidate each recorded member immediately before signaling. A member
    # that exited after the snapshot is skipped; a surviving member must still
    # match its executable, start time, and launch instance identity. This
    # deliberately uses the recorded tree even when its root has exited.
    signal_process_tree TERM "$TREE_FILE"
    sleep 2
    signal_process_tree KILL "$TREE_FILE"
  else
    echo "mem-measure: refusing process cleanup: bounded process tree was not proven" >&2
  fi
  if [ -n "${PREV_WS:-}" ] && [ "$PREV_WS" != "null" ]; then
    hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null 2>&1 || true
  fi
  cleanup_socket_dir
}
trap cleanup EXIT

focus_measure_workspace() {
  if ! hyprctl dispatch "hl.dsp.focus({ workspace = \"$MEM_WS\" })" >/dev/null; then
    echo "mem-measure: failed to dispatch focus to workspace $MEM_WS" >&2
    return 1
  fi
}

snapshot_visual_state() {
  suffix="$1"
  if ! hyprctl activeworkspace -j >"$RUN_DIR/activeworkspace-$suffix.json"; then
    echo "mem-measure: failed to record active workspace ($suffix)" >&2
    return 1
  fi
  if ! hyprctl clients -j >"$RUN_DIR/clients-$suffix.json"; then
    echo "mem-measure: failed to record clients ($suffix)" >&2
    return 1
  fi
  if ! hyprctl monitors -j >"$RUN_DIR/monitors-$suffix.json"; then
    echo "mem-measure: failed to record monitors ($suffix)" >&2
    return 1
  fi
  if ! jq -e 'type == "object"' "$RUN_DIR/activeworkspace-$suffix.json" >/dev/null ||
    ! jq -e 'type == "array"' "$RUN_DIR/clients-$suffix.json" >/dev/null ||
    ! jq -e 'type == "array"' "$RUN_DIR/monitors-$suffix.json" >/dev/null; then
    echo "mem-measure: invalid compositor state evidence ($suffix)" >&2
    return 1
  fi
}

visual_state_matches_target() {
  suffix="$1"
  active_ws="$(jq -r '.id' "$RUN_DIR/activeworkspace-$suffix.json")"
  monitor_ws="$(jq -r --argjson monitor "$TARGET_MONITOR" --argjson ws "$MEM_WS" \
    '.[] | select(.id == $monitor) | .activeWorkspace.id' \
    "$RUN_DIR/monitors-$suffix.json")"
  window_count="$(jq --arg address "$TARGET_ADDR" --argjson pid "$TARGET_PID" \
    --argjson monitor "$TARGET_MONITOR" --argjson ws "$MEM_WS" \
    '[.[] | select(.address == $address and .pid == $pid and .monitor == $monitor and .workspace.id == $ws)] | length' \
    "$RUN_DIR/clients-$suffix.json")"
  [ "$active_ws" = "$MEM_WS" ] &&
    [ "$monitor_ws" = "$MEM_WS" ] &&
    [ "$window_count" = "1" ]
}

visual_state_signature() {
  suffix="$1"
  jq -c -n \
    --arg address "$TARGET_ADDR" \
    --argjson pid "$TARGET_PID" \
    --argjson monitor "$TARGET_MONITOR" \
    --argjson ws "$MEM_WS" \
    --slurpfile clients "$RUN_DIR/clients-$suffix.json" \
    --slurpfile monitors "$RUN_DIR/monitors-$suffix.json" \
    '{
      window: ($clients[0][] | select(.address == $address and .pid == $pid and .monitor == $monitor and .workspace.id == $ws) | {
        address, pid, monitor, workspace: .workspace.id, at, size, class, title
      }),
      output: ($monitors[0][] | select(.id == $monitor) | {
        id, name, x, y, width, height, scale, activeWorkspace: .activeWorkspace.id
      })
    }'
}

verify_target_executable() {
  phase="$1"
  verify_target_identity "$phase"
}

wait_for_visual_settle() {
  stable_polls=0
  previous_signature=""
  for ((poll = 1; poll <= VISUAL_SETTLE_POLLS; poll++)); do
    verify_target_identity "visual settle" || return 1
    snapshot_visual_state "settle-$poll"
    if visual_state_matches_target "settle-$poll"; then
      current_signature="$(visual_state_signature "settle-$poll")"
      if [ "$current_signature" = "$previous_signature" ]; then
        stable_polls=$((stable_polls + 1))
      else
        stable_polls=1
      fi
      previous_signature="$current_signature"
      if [ "$stable_polls" -ge "$VISUAL_STABLE_POLLS" ]; then
        return 0
      fi
    else
      stable_polls=0
    fi
    sleep "$VISUAL_SETTLE_INTERVAL_SECS"
  done
  echo "mem-measure: workspace $MEM_WS did not settle for target window $TARGET_ADDR within ${VISUAL_SETTLE_POLLS} polls" >&2
  return 1
}

# Shell-quote the binary and per-run socket for `sh -c` (Hyprland runs
# exec_cmd through a shell), then present it as a Lua double-quoted string
# (embedded single quotes need no Lua escape; backslashes and double quotes
# are escaped).
# FONT_SIZE is charset/range-validated above and MEM_WS is 1..10, so neither
# can break out of the Lua string.
SHELL_QBIN="'${BIN//\'/\'\\\'\'}'"
SHELL_QSOCK="'${SOCK//\'/\'\\\'\'}'"
LUA_CMD="$(printf '%s' "BITTY_SOCKET=$SHELL_QSOCK BITTY_INSTANCE_ID=$INSTANCE_ID $SHELL_QBIN --font-size $FONT_SIZE" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
if ! hyprctl dispatch "hl.dsp.exec_cmd(\"$LUA_CMD\", {workspace='$MEM_WS silent'})" >/dev/null; then
  echo "mem-measure: failed to dispatch Bitty launch on workspace $MEM_WS" >&2
  exit 1
fi

# Single settle sleep: the method. No poll loop.
sleep "$SETTLE_SECS"

MATCHES="$(hyprctl clients -j | jq --argjson ws "$MEM_WS" '[.[] | select(.workspace.id == $ws and (.class == "bitty" or .class == "run.bitty.Bitty"))]')"
[ "$(printf '%s' "$MATCHES" | jq 'length')" = "1" ] || {
  echo "mem-measure: expected 1 bitty window on workspace $MEM_WS, found $(printf '%s' "$MATCHES" | jq 'length')" >&2
  exit 1
}
SELECTED_PID="$(printf '%s' "$MATCHES" | jq -r '.[0].pid')"
SELECTED_ADDR="$(printf '%s' "$MATCHES" | jq -r '.[0].address')"
SELECTED_MONITOR="$(printf '%s' "$MATCHES" | jq -r '.[0].monitor')"
SELECTED_START_TIME="$(process_start_time "$SELECTED_PID")" || {
  echo "mem-measure: failed to record process-start identity for PID $SELECTED_PID" >&2
  exit 1
}
if ! process_identity_matches "$SELECTED_PID" "$BIN" "$SELECTED_START_TIME" "$INSTANCE_ID"; then
  echo "mem-measure: refusing target window $SELECTED_ADDR: executable/start/instance identity could not be proven" >&2
  exit 1
fi
TARGET_PID="$SELECTED_PID"
TARGET_ADDR="$SELECTED_ADDR"
TARGET_MONITOR="$SELECTED_MONITOR"
TARGET_START_TIME="$SELECTED_START_TIME"
TARGET_EXE="$BIN"
TARGET_INSTANCE_ID="$INSTANCE_ID"
TARGET_IDENTITY_PROVEN=1
echo "mem-measure: pid=$TARGET_PID addr=$TARGET_ADDR"
process_tree_snapshot "$TARGET_PID" || exit 1

grep -E "VmRSS|VmSize|RssAnon|RssFile|RssShmem|Threads" "/proc/$TARGET_PID/status" | tee "$RUN_DIR/status.txt"
awk '/^Pss:/{p+=$2} END{print "PSS_kB="p}' "/proc/$TARGET_PID/smaps" | tee "$RUN_DIR/pss.txt"
awk '/^Private_Clean:/{c+=$2} /^Private_Dirty:/{d+=$2} END{print "USS_kB="c+d}' "/proc/$TARGET_PID/smaps" | tee "$RUN_DIR/uss.txt"
pmap -x "$TARGET_PID" >"$RUN_DIR/pmap.txt" 2>&1 || true
sort -k3 -n -r "$RUN_DIR/pmap.txt" 2>/dev/null | head -n 25 | tee "$RUN_DIR/topmappings.txt" || true
cp "/proc/$TARGET_PID/smaps" "$RUN_DIR/smaps.txt"
# CTX-1216: record the GPU driver version with the sample. Driver-resident
# pages dominate headed RSS and move under bitty's feet across upgrades, so
# a sample without its driver version cannot bisect a regression.
DRIVER_VERSION="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -n 1 | tr -d '[:space:]' || true)"
if [ -z "$DRIVER_VERSION" ]; then
  DRIVER_VERSION="$(awk '/NVRM version:/{print $4}' /proc/driver/nvidia/version 2>/dev/null || true)"
fi
if [ -z "$DRIVER_VERSION" ]; then
  DRIVER_VERSION="unknown"
fi
printf '%s\n' "$DRIVER_VERSION" | tee "$RUN_DIR/driver.txt"
# Evidence must show the measured window: the launch used a silent workspace,
# so a bare grim would capture whatever workspace the operator sits on.
# Memory was sampled above; focus and presentation settling deliberately happen
# afterward. Require the active workspace, its output, and the exact target
# window identity to remain stable across several compositor observations.
focus_measure_workspace
if ! FOCUSED_WS="$(hyprctl activeworkspace -j | jq -r '.id')"; then
  echo "mem-measure: failed to read active workspace after focus" >&2
  exit 1
fi
[ "$FOCUSED_WS" = "$MEM_WS" ] || {
  echo "mem-measure: failed to focus workspace $MEM_WS before visual settle (active=$FOCUSED_WS)" >&2
  exit 1
}
if ! wait_for_visual_settle; then
  exit 1
fi
snapshot_visual_state before-shot
if ! visual_state_matches_target before-shot; then
  echo "mem-measure: target workspace/window state changed before screenshot" >&2
  exit 1
fi
verify_target_executable before-shot
printf '%s\n' \
  "PID=$TARGET_PID" \
  "ADDRESS=$TARGET_ADDR" \
  "MONITOR=$TARGET_MONITOR" \
  "EXE=$TARGET_EXE" \
  "START_TIME=$TARGET_START_TIME" \
  "BITTY_INSTANCE_ID=$TARGET_INSTANCE_ID" >"$RUN_DIR/target-identity.txt"
if ! grim "$RUN_DIR/shot.png"; then
  echo "mem-measure: grim failed to capture $RUN_DIR/shot.png" >&2
  exit 1
fi
[ -s "$RUN_DIR/shot.png" ] || {
  echo "mem-measure: grim produced an empty screenshot at $RUN_DIR/shot.png" >&2
  exit 1
}
snapshot_visual_state after-shot
if ! visual_state_matches_target after-shot; then
  echo "mem-measure: target workspace/window state changed after screenshot" >&2
  exit 1
fi
verify_target_executable after-shot
if [ "$(visual_state_signature before-shot)" != "$(visual_state_signature after-shot)" ]; then
  echo "mem-measure: target window/output geometry changed across screenshot" >&2
  exit 1
fi
if ! hyprctl dispatch "hl.dsp.focus({ workspace = \"$PREV_WS\" })" >/dev/null; then
  echo "mem-measure: failed to restore workspace $PREV_WS after screenshot" >&2
  exit 1
fi

RSS_KB="$(awk '/VmRSS:/{print $2}' "$RUN_DIR/status.txt")"
VSZ_KB="$(awk '/VmSize:/{print $2}' "$RUN_DIR/status.txt")"
PSS_KB="$(awk -F= '/PSS_kB/{print $2}' "$RUN_DIR/pss.txt")"
USS_KB="$(awk -F= '/USS_kB/{print $2}' "$RUN_DIR/uss.txt")"
# CTX-1216 three-way pmap split (columns: Address Kbytes RSS Dirty Mode
# Mapping; the total line carries no mapping and never matches either
# class). DRIVER_RSS_KB is NVIDIA/Vulkan/GL-resident pages bitty cannot
# control; ANON_RSS_KB is every anonymous mapping (heap plus driver arenas,
# inseparable by name); OWNED_OTHER_KB is the named bitty-linked set
# (binary, libc, fonts) where a genuine regression must appear.
PMAP_RSS_KB="$(awk '$1 == "total" {print $4}' "$RUN_DIR/pmap.txt")"
PMAP_RSS_KB="${PMAP_RSS_KB:-0}"
DRIVER_RSS_KB="$(awk '$6 ~ /nvidia|libvulkan|libGL|libEGL|libOpenGL|libglvnd|glvkspirv|VkLayer/ {d+=$3} END{print d+0}' "$RUN_DIR/pmap.txt")"
ANON_RSS_KB="$(awk '/\[ anon \]/ {a+=$3} END{print a+0}' "$RUN_DIR/pmap.txt")"
OWNED_OTHER_KB="$((PMAP_RSS_KB - DRIVER_RSS_KB - ANON_RSS_KB))"
# Interim gate: 250 MB RSS (toward the 150 MB budget).
if [ "$RSS_KB" -le 256000 ]; then
  GATE="PASS"
else
  GATE="FAIL"
fi
{
  echo "BIN=$BIN"
  echo "DRIVER=$DRIVER_VERSION"
  echo "SETTLE_SECS=$SETTLE_SECS"
  echo "RSS_KB=$RSS_KB"
  echo "VSZ_KB=$VSZ_KB"
  echo "PSS_KB=$PSS_KB"
  echo "USS_KB=$USS_KB"
  echo "PMAP_RSS_KB=$PMAP_RSS_KB"
  echo "DRIVER_RSS_KB=$DRIVER_RSS_KB"
  echo "ANON_RSS_KB=$ANON_RSS_KB"
  echo "OWNED_OTHER_KB=$OWNED_OTHER_KB"
  echo "COMPARE=only same DRIVER plus same SETTLE_SECS runs compare (see header)"
  echo "GATE=$GATE (interim: RSS <= 250MB toward 150MB budget)"
} | tee "$RUN_DIR/summary.txt"

if [ "$GATE" = "FAIL" ]; then
  echo "mem-measure: GATE=FAIL (RSS ${RSS_KB}kB over 250MB interim budget)" >&2
  exit 1
fi
echo "mem-measure: GATE=PASS (RSS ${RSS_KB}kB within 250MB interim budget)"
