#!/usr/bin/env bash
# Helpers shared by the headless memory probe and its focused contract tests.

MEM_PROBE_MARKER_WAIT_ATTEMPTS=150
MEM_PROBE_MARKER_WAIT_INTERVAL=0.2

require_pidfd_support() {
  python3 - <<'PY'
import os
import signal
import sys

if sys.platform != "linux" or not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
    raise SystemExit(1)
PY
}

wait_for_terminal_marker() {
  local terminal_id="$1" marker="$2" attempts="${3:-$MEM_PROBE_MARKER_WAIT_ATTEMPTS}"
  local reply=""

  for _ in $(seq 1 "$attempts"); do
    if reply="$("$BIN" ctl --format json --socket "$SOCK" terminal text "$terminal_id" 2>/dev/null)" &&
      jq -e --arg marker "$marker" '[.. | objects | .text? // empty] | any(gsub("[[:space:]]"; "") | contains($marker))' \
        <<<"$reply" >/dev/null 2>&1; then
      return 0
    fi
    sleep "$MEM_PROBE_MARKER_WAIT_INTERVAL"
  done
  printf 'marker timeout terminal=%s marker=%s\n' \
    "$terminal_id" "$marker" >&2
  return 1
}

process_starttime() {
  python3 - "$1" "$2" <<'PY'
import pathlib
import sys

pid, instance = sys.argv[1:]
proc = pathlib.Path("/proc") / pid
try:
    environ = (proc / "environ").read_bytes().split(b"\0")
    if f"BITTY_INSTANCE_ID={instance}".encode() not in environ:
        raise SystemExit(1)
    stat = (proc / "stat").read_text()
    fields_after_comm = stat[stat.rfind(")") + 2 :].split()
    print(fields_after_comm[19])
except (OSError, IndexError):
    raise SystemExit(1)
PY
}

signal_owned_process() {
  python3 - "$1" "$2" "$3" "$4" <<'PY'
import os
import pathlib
import signal
import sys

pid_text, instance, expected_starttime, requested_signal = sys.argv[1:]
try:
    pid = int(pid_text)
    number = getattr(signal, requested_signal)
    pidfd_open = getattr(os, "pidfd_open")
    pidfd_send_signal = getattr(signal, "pidfd_send_signal")
except (AttributeError, ValueError):
    raise SystemExit("pidfd signaling is unavailable; refusing numeric-PID signal")

try:
    pidfd = pidfd_open(pid)
except OSError:
    raise SystemExit(1)

proc = pathlib.Path("/proc") / str(pid)
try:
    environ = (proc / "environ").read_bytes().split(b"\0")
    if f"BITTY_INSTANCE_ID={instance}".encode() not in environ:
        raise SystemExit("process identity changed; refusing signal")
    stat = (proc / "stat").read_text()
    fields_after_comm = stat[stat.rfind(")") + 2 :].split()
    if fields_after_comm[19] != expected_starttime:
        raise SystemExit("process starttime changed; refusing signal")
    pidfd_send_signal(pidfd, number)
except (OSError, IndexError):
    raise SystemExit(1)
finally:
    os.close(pidfd)
PY
}

wait_for_owned_process_exit() {
  local pid="$1" instance="$2" starttime="$3" attempts="${4:-30}"
  for _ in $(seq 1 "$attempts"); do
    local current_starttime
    current_starttime="$(process_starttime "$pid" "$instance" 2>/dev/null || true)"
    if [ -z "$current_starttime" ]; then
      return 0
    fi
    [ "$current_starttime" = "$starttime" ] || return 2
    sleep 0.1
  done
  return 1
}
