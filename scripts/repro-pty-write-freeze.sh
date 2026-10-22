#!/usr/bin/env bash
# Deterministic, bounded repro for bitty#2446: a PTY write must never park
# the terminal event loop when the child stops reading its stdin.
#
# A `--test-mode` instance runs a shell that switches its tty to raw mode and
# then ignores stdin (`exec sleep 60`). Eight 4 KiB input writes overflow the
# kernel input queue; on pre-fix builds the fourth call blocks the runtime
# owner thread in `write(2)` and the following `view list` probe times out.
# The script only uses PIDs it launched, bounds every probe with `timeout`,
# and exits non-zero with the observed timings on a regression.
#
# Usage: scripts/repro-pty-write-freeze.sh [PATH_TO_BITTY]
#   BITTY_BIN may also name the binary; default target/debug/bitty.
# Host requirements: Linux with GNU coreutils (`timeout`, `date +%s%3N`).

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly REPO_ROOT
readonly BIN="${1:-${BITTY_BIN:-$REPO_ROOT/target/debug/bitty}}"
readonly INSTANCE="ptiowrite"
readonly SEND_BYTES=4000
readonly SEND_COUNT=8
readonly PROBE_TIMEOUT=2

if [[ ! -x $BIN ]]; then
  echo "repro: not executable: $BIN (build with: cargo build -p bitty-terminal --bin bitty)" >&2
  exit 2
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/bitty-pty-write-repro.XXXXXX")"
readonly SCRATCH
readonly RUNTIME="$SCRATCH/run"
mkdir -p "$RUNTIME" "$SCRATCH/home" "$SCRATCH/config" "$SCRATCH/data" "$SCRATCH/state" "$SCRATCH/cache"
chmod 700 "$RUNTIME"

pid=""
child_pid=""

cleanup() {
  if [[ -n $child_pid ]] && kill -0 "$child_pid" 2>/dev/null; then
    kill "$child_pid" 2>/dev/null || true
  fi
  if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    for _ in {1..50}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -rf "$SCRATCH"
}
trap cleanup EXIT

export HOME="$SCRATCH/home"
export XDG_RUNTIME_DIR="$RUNTIME"
export XDG_CONFIG_HOME="$SCRATCH/config"
export XDG_DATA_HOME="$SCRATCH/data"
export XDG_STATE_HOME="$SCRATCH/state"
export XDG_CACHE_HOME="$SCRATCH/cache"
export BITTY_INSTANCE_ID="$INSTANCE"
export SHELL=/bin/sh

ctl() {
  timeout "$PROBE_TIMEOUT" "$BIN" ctl --instance "$INSTANCE" "$@"
}

"$BIN" --test-mode --log-level error >"$SCRATCH/stderr.log" 2>&1 &
pid=$!

ready=0
for _ in {1..50}; do
  if ctl view list >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.1
done
if [[ $ready != 1 ]]; then
  echo "repro: test-mode instance never became ready (pid $pid)" >&2
  exit 1
fi

# Record the shell pid so cleanup never leaves the sleeper behind.
if terminals="$(ctl terminal list 2>/dev/null)"; then
  child_pid="$(sed -n 's/.*"child_pid":\([0-9][0-9]*\).*/\1/p' <<<"$terminals" | head -n1)"
fi

# Raw mode removes the canonical line-discipline discard, so the kernel
# input queue really fills; `exec` keeps the recorded pid valid. Wait for the
# marker instead of sleeping: a slow shell must not make the burst land in
# canonical mode (where overflow is discarded and the repro would pass
# vacuously).
ctl terminal send t:1 'stty raw -echo || exit 1; printf __RAW_READY__; exec sleep 60' >/dev/null
ctl terminal send t:1 $'\r' >/dev/null
raw_ready=0
for _ in {1..50}; do
  if ctl terminal text t:1 2>/dev/null | grep -q __RAW_READY__; then
    raw_ready=1
    break
  fi
  sleep 0.1
done
if [[ $raw_ready != 1 ]]; then
  echo "repro: raw-mode sleeper never reported readiness" >&2
  exit 1
fi

payload="$(head -c "$SEND_BYTES" /dev/zero | tr '\0' x)"
failed=0
for i in $(seq 1 "$SEND_COUNT"); do
  started="$(date +%s%3N)"
  if ! ctl terminal send t:1 "$payload" >/dev/null 2>&1; then
    echo "FAIL: send $i/$SEND_COUNT timed out after ${PROBE_TIMEOUT}s (event loop blocked in the PTY write)"
    failed=1
    break
  fi
  finished="$(date +%s%3N)"
  echo "send $i/$SEND_COUNT: $((finished - started)) ms"
done

if [[ $failed == 0 ]]; then
  started="$(date +%s%3N)"
  if ! ctl view list >/dev/null 2>&1; then
    echo "FAIL: view list timed out after the write burst (runtime owner thread is not draining the loop)"
    failed=1
  else
    finished="$(date +%s%3N)"
    echo "view list after burst: $((finished - started)) ms"
  fi
fi

if [[ $failed != 0 ]]; then
  echo "repro: FAIL (bitty#2446 regression)" >&2
  exit 1
fi
echo "repro: PASS (bounded PTY writes, runtime responsive)"
