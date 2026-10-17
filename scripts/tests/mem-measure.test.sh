#!/usr/bin/env bash
# mem-measure.test.sh — CTX-1036 contract test for scripts/mem-measure.sh.
# shellcheck disable=SC1090,SC2016,SC2034,SC2317
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

# Extract and execute the production snapshot function with shell-level test
# doubles. This keeps the test headless and avoids touching the real process
# table or /proc while exercising the actual discovery/counting control flow.
TREE_FUNCTION_FILE="$(mktemp)"
trap 'rm -f -- "$TREE_FUNCTION_FILE"' EXIT
awk '
  /^process_tree_snapshot\(\) \{/ { in_function = 1 }
  /^signal_process_tree\(\) \{/ && in_function { exit }
  in_function { print }
' "$SESSION" >"$TREE_FUNCTION_FILE"

run_snapshot_case() (
  case_name="$1"
  process_table="$2"
  capture_failures="$3"
  exited_outcomes="$4"
  member_limit="$5"
  expected_status="$6"
  expected_members="$7"
  expected_pids="$8"
  expected_error="$9"

  case_dir="$(mktemp -d)"
  trap 'rm -rf -- "$case_dir"' EXIT
  RUN_DIR="$case_dir"
  PROCESS_TREE_MAX_DEPTH=8
  PROCESS_TREE_MAX_MEMBERS="$member_limit"
  MOCK_PROCESS_TABLE="$process_table"
  MOCK_CAPTURE_FAILURES="$capture_failures"
  MOCK_EXITED_OUTCOMES="$exited_outcomes"

  # Bash resolves functions before external commands in a pipeline, so this
  # safely replaces only the read-only process-table discovery command.
  ps() {
    printf '%s\n' "$MOCK_PROCESS_TABLE"
  }

  capture_process_identity() {
    case " $MOCK_CAPTURE_FAILURES " in
    *" $1 "*) return 1 ;;
    esac
    printf '%s\tstart-%s\t/bin/bitty\tinstance\n' "$1" "$1"
  }

  process_is_exited() {
    for outcome in $MOCK_EXITED_OUTCOMES; do
      case "$outcome" in
      "$1":absent | "$1":zombie) return 0 ;;
      "$1":indeterminate) return 2 ;;
      esac
    done
    # A non-zero result models a surviving process. The production branch must
    # refuse both this outcome and an indeterminate /proc read above.
    return 1
  }

  # shellcheck disable=SC1090
  source "$TREE_FUNCTION_FILE"
  tree_file="$case_dir/tree.txt"
  error_file="$case_dir/error.txt"
  status=0
  process_tree_snapshot 100 "$tree_file" 2>"$error_file" || status=$?

  if [ "$status" != "$expected_status" ]; then
    echo "FAIL [$case_name]: expected status $expected_status, got $status" >&2
    return 1
  fi
  member_count="$(wc -l <"$tree_file")"
  if [ "$member_count" != "$expected_members" ]; then
    echo "FAIL [$case_name]: expected $expected_members recorded members, got $member_count" >&2
    cat "$tree_file" >&2
    return 1
  fi
  if [ -n "$expected_pids" ]; then
    recorded_pids="$(cut -f1 "$tree_file" | tr '\n' ' ' | sed 's/ $//')"
    if [ "$recorded_pids" != "$expected_pids" ]; then
      echo "FAIL [$case_name]: expected recorded PIDs [$expected_pids], got [$recorded_pids]" >&2
      return 1
    fi
  fi
  if [ -n "$expected_error" ] && ! grep -qF -- "$expected_error" "$error_file"; then
    echo "FAIL [$case_name]: stderr missing [$expected_error]" >&2
    cat "$error_file" >&2
    return 1
  fi
)

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
expect_source 'if process_is_exited "$child"; then'
expect_source 'skipping exited process-tree member PID $child during discovery'
expect_source 'member_count=$((member_count + 1))'
expect_source 'member_count=$((member_count - 1))'
expect_source 'continue'
expect_source_order 'if ! identity="$(capture_process_identity "$child")"; then' 'if process_is_exited "$child"; then'
expect_source_order 'if process_is_exited "$child"; then' 'member_count=$((member_count - 1))'
expect_source_order 'member_count=$((member_count - 1))' 'skipping exited process-tree member PID $child during discovery'
expect_source_order 'if ! identity="$(capture_process_identity "$child")"; then' 'if [ "$member_count" -gt "$PROCESS_TREE_MAX_MEMBERS" ]; then'
expect_source_order 'if process_is_exited "$child"; then' 'echo "mem-measure: refusing process-tree cleanup: PID $child identity could not be proven"'
expect_source 'executable/start/instance identity'
expect_source 'TARGET_IDENTITY_PROVEN=1'
expect_source 'target_window_matches_identity'
expect_source 'START_TIME=$TARGET_START_TIME'
expect_source 'if ! grim "$RUN_DIR/shot.png"; then'
expect_source 'failed to dispatch focus to workspace'
expect_absent 'kill "$child"'
expect_absent 'kill -9 "$child"'

# CTX-1296: execute process_tree_snapshot with deterministic process-table and
# identity doubles. Absent/zombie children are omitted after identity capture
# fails, and omitted children do not consume the recorded-member bound.
run_snapshot_case \
  'absent-and-zombie-children-are-skipped' \
  $'101 100\n102 100\n103 100' \
  '101 102' \
  '101:absent 102:zombie' \
  2 0 2 '100 103' ''

# A failed identity capture for a child that is still present, or whose /proc
# state is indeterminate, must refuse the whole snapshot fail-closed.
run_snapshot_case \
  'surviving-child-identity-failure-refuses' \
  '201 100' \
  '201' \
  '' \
  64 1 1 '100' 'PID 201 identity could not be proven'
run_snapshot_case \
  'indeterminate-child-identity-failure-refuses' \
  '202 100' \
  '202' \
  '202:indeterminate' \
  64 1 1 '100' 'PID 202 identity could not be proven'

# A valid root plus two valid children exceeds a bound of two and must be
# refused even though every surviving member has a proven identity.
run_snapshot_case \
  'excessive-surviving-members-refuse' \
  $'401 100\n402 100' \
  '' \
  '' \
  2 1 2 '100 401' 'more than 2 members'

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
