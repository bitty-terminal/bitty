#!/usr/bin/env bash
# Contract tests for scripts/lib/mem-probe-headless.sh.
set -euo pipefail

cd "$(dirname "$0")/../.."
source scripts/lib/mem-probe-headless.sh

TMP_BASE="$(mktemp -d "${TMPDIR:-/tmp}/bitty-mem-probe-test.XXXXXX")"
trap 'rm -rf -- "$TMP_BASE"' EXIT

BIN="$TMP_BASE/fake-bitty"
cat >"$BIN" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *t:2) printf '{"v":1,"command":"core.terminal.text","ok":true,"result":{"terminal_id":"t:2","text":"...BITTY_MEM_PROBE_D\\nONE_89abcdef2"}}\n' ;;
  *) printf '{"v":1,"command":"core.terminal.text","ok":true,"result":{"terminal_id":"t:1","text":"still generating"}}\n' ;;
esac
EOF
chmod +x "$BIN"
SOCK="$TMP_BASE/socket"

if ! wait_for_terminal_marker t:2 BITTY_MEM_PROBE_DONE_89abcdef2 1; then
  echo "FAIL: expected pane marker to be observed" >&2
  exit 1
fi

if wait_for_terminal_marker t:1 BITTY_MEM_PROBE_DONE_89abcdef1 2; then
  echo "FAIL: missing pane marker unexpectedly passed the timeout" >&2
  exit 1
fi

is_live_child() {
  local pid="$1" state
  kill -0 "$pid" 2>/dev/null || return 1
  state="$(ps -o stat= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
  [ -n "$state" ] && [[ "$state" != Z* ]]
}

spawn_owned_child() {
  local instance="$1" pid_var="$2" starttime_var="$3"
  local pid starttime
  BITTY_INSTANCE_ID="$instance" sleep 30 &
  pid=$!
  starttime=""
  for _ in $(seq 1 50); do
    starttime="$(process_starttime "$pid" "$instance" 2>/dev/null || true)"
    [ -n "$starttime" ] && break
    sleep 0.02
  done
  [ -n "$starttime" ] || {
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "FAIL: could not identify owned child" >&2
    exit 1
  }
  printf -v "$pid_var" '%s' "$pid"
  printf -v "$starttime_var" '%s' "$starttime"
}

stop_owned_child() {
  local pid="$1"
  if is_live_child "$pid"; then
    kill "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

assert_refused_and_alive() {
  local label="$1" pid="$2" instance="$3" starttime="$4" expected="$5"
  local output
  if output="$(signal_owned_process "$pid" "$instance" "$starttime" SIGTERM 2>&1)"; then
    echo "FAIL [$label]: identity mutation was not refused" >&2
    stop_owned_child "$pid"
    exit 1
  fi
  if ! grep -Fq -- "$expected" <<<"$output"; then
    echo "FAIL [$label]: refusal did not name the rejected identity" >&2
    printf '%s\n' "$output" >&2
    stop_owned_child "$pid"
    exit 1
  fi
  if ! is_live_child "$pid"; then
    echo "FAIL [$label]: child was signaled despite identity mismatch" >&2
    stop_owned_child "$pid"
    exit 1
  fi
  stop_owned_child "$pid"
}

# Correct instance + wrong starttime: removing the starttime guard must signal
# this child, so the child-liveness assertion makes that mutation fail.
starttime_pid=""
starttime_value=""
spawn_owned_child mem-probe-test-starttime starttime_pid starttime_value
assert_refused_and_alive \
  wrong-starttime "$starttime_pid" mem-probe-test-starttime "${starttime_value}0" \
  "process starttime changed"

# Wrong instance + correct starttime: removing the instance guard must signal
# this child, so the child-liveness assertion makes that mutation fail.
instance_pid=""
instance_starttime=""
spawn_owned_child mem-probe-test-instance instance_pid instance_starttime
assert_refused_and_alive \
  wrong-instance "$instance_pid" not-the-owned-instance "$instance_starttime" \
  "process identity changed"

# Remove both pidfd APIs in a child Python startup via sitecustomize. The
# production helper must refuse rather than fall back to os.kill; the child
# must remain live, making an os.kill mutation fail.
NO_PIDFD_DIR="$TMP_BASE/no-pidfd"
mkdir -p "$NO_PIDFD_DIR"
cat >"$NO_PIDFD_DIR/sitecustomize.py" <<'PY'
import os
import signal

for module, name in ((os, "pidfd_open"), (signal, "pidfd_send_signal")):
    try:
        delattr(module, name)
    except AttributeError:
        pass
PY
spawn_owned_child mem-probe-test-pidfd pidfd_pid pidfd_starttime
pidfd_pid="${pidfd_pid:-}"
pidfd_starttime="${pidfd_starttime:-}"
pidfd_output=""
if pidfd_output="$(PYTHONPATH="$NO_PIDFD_DIR" signal_owned_process \
  "$pidfd_pid" mem-probe-test-pidfd "$pidfd_starttime" SIGTERM 2>&1)"; then
  echo "FAIL [pidfd-unavailable]: numeric-PID fallback was not refused" >&2
  stop_owned_child "$pidfd_pid"
  exit 1
fi
if ! grep -Fq 'refusing numeric-PID signal' <<<"$pidfd_output"; then
  echo "FAIL [pidfd-unavailable]: missing pidfd refusal" >&2
  printf '%s\n' "$pidfd_output" >&2
  stop_owned_child "$pidfd_pid"
  exit 1
fi
if ! is_live_child "$pidfd_pid"; then
  echo "FAIL [pidfd-unavailable]: child was signaled by fallback" >&2
  stop_owned_child "$pidfd_pid"
  exit 1
fi
stop_owned_child "$pidfd_pid"

# With pidfds available, replacing `pidfd_send_signal` with `os.kill` must be
# observable. Patch os.kill only inside the signaling Python child; pidfd
# signaling must still terminate this correctly identified child.
KILL_GUARD_DIR="$TMP_BASE/kill-guard"
mkdir -p "$KILL_GUARD_DIR"
cat >"$KILL_GUARD_DIR/sitecustomize.py" <<'PY'
import os


def forbidden_numeric_pid_signal(*_args, **_kwargs):
    raise RuntimeError("numeric os.kill forbidden")


os.kill = forbidden_numeric_pid_signal
PY
spawn_owned_child mem-probe-test-pidfd-send pidfd_send_pid pidfd_send_starttime
pidfd_send_pid="${pidfd_send_pid:-}"
pidfd_send_starttime="${pidfd_send_starttime:-}"
if ! PYTHONPATH="$KILL_GUARD_DIR" signal_owned_process \
  "$pidfd_send_pid" mem-probe-test-pidfd-send "$pidfd_send_starttime" SIGTERM; then
  echo "FAIL [pidfd-send]: pidfd signaling did not bypass numeric os.kill" >&2
  stop_owned_child "$pidfd_send_pid"
  exit 1
fi
for _ in $(seq 1 50); do
  is_live_child "$pidfd_send_pid" || break
  sleep 0.02
done
if is_live_child "$pidfd_send_pid"; then
  echo "FAIL [pidfd-send]: correctly identified child survived pidfd signal" >&2
  stop_owned_child "$pidfd_send_pid"
  exit 1
fi
wait "$pidfd_send_pid" 2>/dev/null || true

echo "mem-probe-headless contract: PASS"
