#!/usr/bin/env bash
# mem-measure.test.sh — CTX-1036 contract test for scripts/mem-measure.sh.
# shellcheck disable=SC2016
#
# Headless-safe: every assertion runs through --help, --dry-run, or argument
# refusal, so no display, Hyprland session, grim capture, or launched window
# is required. The live headed path is exercised by hand on ws5 per the #1809
# method, never here.
set -euo pipefail

cd "$(dirname "$0")/../.."

SESSION=./scripts/mem-measure.sh
FAIL=0

expect_exit() {
  want="$1"
  shift
  got=0
  "$@" >/dev/null 2>&1 || got=$?
  if [ "$got" != "$want" ]; then
    echo "FAIL: expected exit $want, got $got: $*" >&2
    FAIL=1
  fi
}

expect_stdout() {
  # Capture first, then grep: piping straight into `grep -qF` lets the
  # early grep exit SIGPIPE the producer, which fails under the
  # caller's `pipefail`. Small outputs only; that is all we assert.
  pattern="$1"
  shift
  out="$("$@" 2>/dev/null)" || true
  if ! printf '%s\n' "$out" | grep -qF -- "$pattern"; then
    echo "FAIL: stdout missing [$pattern]: $*" >&2
    FAIL=1
  fi
}

expect_source() {
  pattern="$1"
  if ! grep -qF -- "$pattern" "$SESSION"; then
    echo "FAIL: script missing [$pattern]" >&2
    FAIL=1
  fi
}

expect_absent() {
  pattern="$1"
  if grep -qF -- "$pattern" "$SESSION"; then
    echo "FAIL: script unexpectedly contains [$pattern]" >&2
    FAIL=1
  fi
}

expect_source_order() {
  first="$1"
  second="$2"
  first_line="$(grep -nF -- "$first" "$SESSION" | head -n 1 | cut -d: -f1)"
  second_line="$(grep -nF -- "$second" "$SESSION" | head -n 1 | cut -d: -f1)"
  if [ -z "$first_line" ] || [ -z "$second_line" ] || [ "$first_line" -ge "$second_line" ]; then
    echo "FAIL: source order is not [$first] before [$second]" >&2
    FAIL=1
  fi
}

# Source-contract patterns intentionally contain literal shell expansions.

# --help lists the contract surface.
expect_exit 0 "$SESSION" --help
expect_stdout "--dry-run" "$SESSION" --help
expect_stdout "--workspace ID" "$SESSION" --help
expect_stdout "--settle-secs N" "$SESSION" --help
expect_stdout "250" "$SESSION" --help

# --dry-run prints the plan and launches nothing.
expect_exit 0 "$SESSION" --dry-run --binary /bin/true
expect_stdout "mem-measure: plan" "$SESSION" --dry-run --binary /bin/true
expect_stdout "workspace=5" "$SESSION" --dry-run --binary /bin/true
expect_exit 0 "$SESSION" --dry-run --binary /bin/true --font-size 12.5

# CTX-1216 (issue #2087): the comparability contract stays documented —
# same-driver plus same-settle comparisons, with the three-way RSS split.
expect_stdout "driver" "$SESSION" --help
expect_stdout "DRIVER_RSS_KB" "$SESSION" --help
expect_stdout "OWNED_OTHER_KB" "$SESSION" --help
expect_stdout "driver.txt" "$SESSION" --help

# CTX-1269: the headed capture must isolate the launched instance, wait for
# compositor state to settle after focus, and retain identity evidence around
# the screenshot. These are source-contract assertions because this test is
# intentionally headless; headed validation is restricted to workspace 5.
expect_source 'BITTY_SOCKET='
expect_source 'BITTY_INSTANCE_ID='
expect_absent 'SOCK="$RUN_DIR/'
expect_source 'XDG_RUNTIME_DIR:-${TMPDIR:-}'
expect_source 'SOCKET_PATH_LIMIT=100'
expect_source 'runtime socket path is too long'
expect_source 'cleanup_socket_dir'
expect_source 'process_executable'
expect_source 'process_start_time'
expect_source 'process_identity_matches'
expect_source 'kill -s "$signal" "$pid"'
expect_source 'refusing to signal PID'
expect_absent 'refusing fallback PID'
expect_source 'PROCESS_TREE_MAX_DEPTH'
expect_source 'PROCESS_TREE_MAX_MEMBERS'
expect_source 'process_tree_snapshot'
expect_source 'TREE_FILE="$RUN_DIR/process-tree.txt"'
expect_source 'signal_process_tree'
expect_source 'signal_process_tree TERM "$TREE_FILE"'
expect_source 'signal_process_tree KILL "$TREE_FILE"'
expect_source 'deliberately uses the recorded tree even when its root has exited'
expect_source 'skipping exited process-tree member PID'
expect_source 'if [ ! -d "/proc/$pid" ]; then'
expect_source 'more than $PROCESS_TREE_MAX_MEMBERS members'
expect_absent 'validate_process_tree "$TREE_FILE"'
expect_absent 'process_tree_snapshot "$KILL_PID"'
expect_absent 'CUR_PIDS='
expect_absent 'PRE_PIDS='
expect_absent 'BIN_BASE='
expect_absent 'ps -eo pid=,comm='
expect_absent 'verify_target_identity "cleanup"'
expect_absent 'if [ -n "$KILL_PID" ]; then'
expect_source 'The launch-time tree is the only cleanup target'
expect_source_order 'TREE_FILE="$RUN_DIR/process-tree.txt"' 'signal_process_tree TERM "$TREE_FILE"'
expect_source 'Revalidate each recorded member immediately before signaling'
expect_source 'wait_for_visual_settle'
expect_source 'FOCUSED_WS'
expect_source 'activeworkspace-before-shot.json'
expect_source 'monitors-before-shot.json'
expect_source 'clients-after-shot.json'
expect_source 'activeworkspace-after-shot.json'
expect_source 'target-identity.txt'
expect_source 'readlink -f "/proc/$1/exe"'
expect_source 'BITTY_INSTANCE_ID=$INSTANCE_ID'
expect_source 'process_instance_matches'
expect_source 'process_is_exited'
expect_source '[ "$state" = "Z" ]'
expect_source 'if process_is_exited "$pid"; then'
expect_source 'executable/start/instance identity'
expect_source 'TARGET_IDENTITY_PROVEN=1'
expect_source 'target_window_matches_identity'
expect_source 'START_TIME=$TARGET_START_TIME'
expect_source 'if ! grim "$RUN_DIR/shot.png"; then'
expect_source 'failed to dispatch focus to workspace'
expect_absent 'kill "$child"'
expect_absent 'kill -9 "$child"'

# Refusals: unknown flag, bad workspace, bad settle, bad font size, missing binary.
expect_exit 2 "$SESSION" --bogus-flag
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --workspace 11
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --settle-secs 3
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --settle-secs 61
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size abc
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size 0
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size 129
expect_exit 2 "$SESSION" --dry-run --binary /bin/true --font-size "12; echo pwned"
expect_exit 2 "$SESSION" --dry-run --binary /nonexistent-bitty-mem-probe

if [ "$FAIL" != "0" ]; then
  echo "mem-measure contract: FAIL" >&2
  exit 1
fi
echo "mem-measure contract: PASS"
