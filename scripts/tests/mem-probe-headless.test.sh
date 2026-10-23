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

# A current shell cannot impersonate the launched instance just by supplying
# its PID. starttime/instance checks and pidfd signaling must fail closed.
if signal_owned_process "$$" "not-the-current-instance" 1 SIGTERM 2>/dev/null; then
  echo "FAIL: mismatched process identity was signaled" >&2
  exit 1
fi
kill -0 "$$"

BITTY_INSTANCE_ID="mem-probe-test-owned" sleep 30 &
OWNED_PID=$!
OWNED_STARTTIME=""
for _ in $(seq 1 50); do
  OWNED_STARTTIME="$(process_starttime "$OWNED_PID" mem-probe-test-owned 2>/dev/null || true)"
  [ -n "$OWNED_STARTTIME" ] && break
  sleep 0.02
done
[ -n "$OWNED_STARTTIME" ] || {
  kill "$OWNED_PID" 2>/dev/null || true
  echo "FAIL: could not identify owned child" >&2
  exit 1
}
signal_owned_process "$OWNED_PID" mem-probe-test-owned "$OWNED_STARTTIME" SIGTERM
wait "$OWNED_PID" 2>/dev/null || true
if kill -0 "$OWNED_PID" 2>/dev/null; then
  echo "FAIL: pidfd did not signal the identified child" >&2
  exit 1
fi

echo "mem-probe-headless contract: PASS"
