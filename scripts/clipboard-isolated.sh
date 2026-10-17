#!/usr/bin/env bash
# Destructive clipboard tests run only on a fresh Xvfb in private namespaces.
set -euo pipefail

readonly BLOCKED=77
readonly RUN_TIMEOUT=60
readonly KILL_GRACE=5
if [[ $# != 1 || ! -x $1 ]]; then
  echo 'usage: just --justfile scripts/clipboard.justfile isolated PATH_TO_CLIPBOARD_LIVE_TEST_BINARY' >&2
  exit 2
fi
for tool in bwrap Xvfb timeout; do
  if ! command -v "$tool" >/dev/null; then
    echo "clipboard-isolated: NOT EXECUTED (missing $tool)" >&2
    exit "$BLOCKED"
  fi
done

binary=$(realpath "$1")
mounts=()
for path in /usr /bin /lib /lib64; do
  if [[ -e $path ]]; then
    mounts+=(--ro-bind "$path" "$path")
  fi
done

# No host /tmp, runtime sockets, home, Xauthority, environment or network.
# bwrap destroys the namespaces on exit, including assertion/timeout failure.
timeout --foreground --kill-after="$KILL_GRACE" "$RUN_TIMEOUT" bwrap --unshare-all --die-with-parent --new-session \
  "${mounts[@]}" --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run --chdir /tmp \
  --ro-bind "$binary" /clipboard-test --clearenv --setenv PATH /usr/bin:/bin \
  /bin/bash -euo pipefail -c '
    readonly READY_TIMEOUT=10
    readonly EXPECTED_SCENARIOS=5
    server_pid=
    cleanup() {
      if [[ -n $server_pid ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
      fi
    }
    trap cleanup EXIT
    trap "exit 77" HUP INT TERM
    mkdir bitty
    cd bitty
    mkfifo display-ready
    exec 3<>display-ready
    Xvfb -displayfd 3 -nolisten tcp >xvfb.log 2>&1 &
    server_pid=$!
    if ! read -r -t "$READY_TIMEOUT" display_number <&3; then
      echo "clipboard-isolated: NOT EXECUTED (Xvfb startup failed)" >&2
      exit 77
    fi
    [[ $display_number =~ ^[0-9]+$ ]] || exit 77
    exec 3>&-
    export DISPLAY=:$display_number BITTY_CLIPBOARD_XVFB_PID=$server_pid
    # The outer timeout preserves sibling ownership evidence in /proc.
    status=0
    /clipboard-test --nocapture --test-threads=1 live_clipboard_ >results 2>&1 || status=$?
    cat results
    [[ $status == 0 ]] || exit "$status"
    verified=$(grep -c "\"status\": \"verified-local\"" results || true)
    if [[ $verified != "$EXPECTED_SCENARIOS" ]]; then
      echo "clipboard-isolated: NOT FULLY EXECUTED (native scenario skipped)" >&2
      exit 77
    fi
    echo "clipboard-isolated: verified all $EXPECTED_SCENARIOS native scenarios"
  '
